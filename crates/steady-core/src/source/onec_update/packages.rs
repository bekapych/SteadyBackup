//! Поиск локальных пакетов 1С и построение цепочки обновления.

use super::cfu::{self, UpdateSource};
use super::util::{decode_text, ini_get, parse_ini_head, same_name, Version};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageKind {
    /// `.cfu` — только из перечисленных версий.
    Cfu,
    /// `.cf` — из любой более ранней версии.
    Cf,
}

#[derive(Clone, Debug)]
pub struct Package {
    pub file: PathBuf,
    pub kind: PackageKind,
    pub version: Version,
    pub from: Vec<Version>,
    pub vendor: String,
    pub name: String,
    /// Исходные конфигурации `.cfu`; пусто — известны только версии из UpdInfo.txt.
    pub sources: Vec<UpdateSource>,
}

impl Package {
    pub fn applies_to(&self, v: &Version) -> bool {
        match self.kind {
            PackageKind::Cfu => self.from.iter().any(|f| f == v),
            PackageKind::Cf => self.version > *v,
        }
    }
}

/// Найденные пакеты и предупреждения о пропущенных файлах.
pub struct Scan {
    pub packages: Vec<Package>,
    pub warnings: Vec<String>,
}

/// Сканирует корни до глубины 8. Нечитаемый корень — ошибка задания, а не «обновлений нет».
pub fn scan(roots: &[PathBuf]) -> anyhow::Result<Scan> {
    let mut s = Scan {
        packages: Vec::new(),
        warnings: Vec::new(),
    };
    for root in roots {
        walk(root, &mut s, 0)?;
    }
    s.packages
        .sort_by(|a, b| a.version.cmp(&b.version).then(a.file.cmp(&b.file)));
    Ok(s)
}

fn walk(dir: &Path, s: &mut Scan, depth: usize) -> anyhow::Result<()> {
    if depth > 8 {
        return Ok(());
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if depth == 0 => {
            return Err(anyhow::Error::new(e)
                .context(format!("каталог обновлений {} не читается", dir.display())));
        }
        Err(e) => {
            s.warnings
                .push(format!("{}: каталог не читается: {e}", dir.display()));
            return Ok(());
        }
    };
    let mut files = Vec::new();
    for entry in rd {
        match entry {
            Ok(e) => {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, s, depth + 1)?;
                } else {
                    files.push(p);
                }
            }
            Err(e) => s
                .warnings
                .push(format!("{}: ошибка чтения каталога: {e}", dir.display())),
        }
    }
    let ext = |p: &Path, e: &str| {
        p.extension()
            .is_some_and(|x| x.to_string_lossy().eq_ignore_ascii_case(e))
    };
    let named = |name: &str| {
        files.iter().find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name))
        })
    };
    let read_meta = |p: Option<&PathBuf>, s: &mut Scan| match p {
        Some(p) => match read_kv(p) {
            Ok(kv) => kv,
            Err(e) => {
                s.warnings
                    .push(format!("{}: метаданные не прочитаны: {e:#}", p.display()));
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    let mft = read_meta(files.iter().find(|p| ext(p, "mft")), s);
    let upd = read_meta(named("UpdInfo.txt"), s);
    let vendor = ini_get(&mft, "Vendor").unwrap_or("").to_string();
    let name = ini_get(&mft, "Name").unwrap_or("").to_string();
    // Версия: UpdInfo.txt, манифест, затем имя каталога 3_0_150_25.
    let version = ini_get(&upd, "Version")
        .and_then(Version::parse)
        .or_else(|| ini_get(&mft, "Version").and_then(Version::parse))
        .or_else(|| {
            dir.file_name()
                .and_then(|n| Version::parse(&n.to_string_lossy()))
        });

    for f in &files {
        let kind = if ext(f, "cfu") {
            PackageKind::Cfu
        } else if ext(f, "cf") {
            PackageKind::Cf
        } else {
            continue;
        };
        let Some(version) = version.clone() else {
            if kind == PackageKind::Cf && name.is_empty() {
                continue;
            }
            s.warnings.push(format!(
                "{}: версия неизвестна (нет UpdInfo.txt, манифеста .mft и номера в имени каталога) — пропущен",
                f.display()
            ));
            continue;
        };
        let from = if kind == PackageKind::Cfu {
            ini_get(&upd, "FromVersions")
                .unwrap_or("")
                .split([';', ','])
                .filter_map(Version::parse)
                .collect()
        } else {
            Vec::new()
        };
        s.packages.push(Package {
            file: f.clone(),
            kind,
            version,
            from,
            vendor: vendor.clone(),
            name: name.clone(),
            sources: Vec::new(),
        });
    }
    Ok(())
}

fn read_kv(p: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let raw = std::fs::read(p)?;
    Ok(parse_ini_head(&decode_text(&raw)))
}

/// Пакет, если он подходит к конфигурации (`vendor`, одно из имён `names`).
pub fn select(p: &Package, vendor: &str, names: &[&str]) -> Result<Option<Package>, String> {
    let vendor_ok = |v: &str| v.is_empty() || vendor.is_empty() || same_name(v, vendor);
    // Полный .cf без имени нельзя применять к произвольной конфигурации.
    if p.kind == PackageKind::Cf && p.name.is_empty() {
        return Ok(None);
    }
    let name_ok = p.name.is_empty() || names.iter().any(|n| same_name(&p.name, n));
    if !vendor_ok(&p.vendor) || !name_ok {
        return Ok(None);
    }
    let sources = if p.kind == PackageKind::Cfu && p.sources.is_empty() {
        match cfu::read_sources(&p.file) {
            Ok(s) => s,
            Err(e) if p.from.is_empty() => {
                return Err(format!(
                    "{}: неизвестно, из каких версий он обновляет ({e:#}; UpdInfo.txt нет) — пропущен",
                    p.file.display()
                ));
            }
            Err(_) => Vec::new(),
        }
    } else {
        p.sources.clone()
    };
    if sources.is_empty() {
        return Ok(Some(p.clone()));
    }
    let own: Vec<UpdateSource> = sources
        .into_iter()
        .filter(|s| vendor_ok(&s.vendor) && names.iter().any(|n| same_name(&s.name, n)))
        .collect();
    if own.is_empty() {
        return Ok(None);
    }
    let mut q = p.clone();
    q.from = own.iter().map(|s| s.version.clone()).collect();
    q.sources = own;
    Ok(Some(q))
}

/// Новейшая версия, для которой при пустой цепочке не хватает промежуточных пакетов.
/// Это ошибочная конфигурация каталога обновлений, а не успешный no-op.
pub fn missing_intermediate(
    packages: &[Package],
    current: &Version,
    chain: &[Package],
) -> Option<Version> {
    if !chain.is_empty() {
        return None;
    }
    packages
        .iter()
        .map(|p| &p.version)
        .filter(|v| *v > current)
        .max()
        .cloned()
}

/// Цепочка от `current` до самой новой достижимой версии (или до `target`).
/// Стоимость пути: сначала число полных `.cf`, затем общее число шагов.
pub fn plan(packages: &[Package], current: &Version, target: Option<&Version>) -> Vec<Package> {
    let mut nodes: Vec<Version> = packages
        .iter()
        .map(|p| p.version.clone())
        .filter(|v| v > current && target.is_none_or(|t| v <= t))
        .collect();
    nodes.push(current.clone());
    nodes.sort();
    nodes.dedup();
    let mut best: HashMap<Version, (usize, usize)> = HashMap::from([(current.clone(), (0, 0))]);
    let mut prev: HashMap<Version, (Version, usize)> = HashMap::new();
    for v in &nodes {
        let Some(&(cf, steps)) = best.get(v) else {
            continue;
        };
        for (i, p) in packages.iter().enumerate() {
            if p.version <= *v || !p.applies_to(v) || target.is_some_and(|t| p.version > *t) {
                continue;
            }
            let cost = (cf + usize::from(p.kind == PackageKind::Cf), steps + 1);
            if best.get(&p.version).is_none_or(|b| cost < *b) {
                best.insert(p.version.clone(), cost);
                prev.insert(p.version.clone(), (v.clone(), i));
            }
        }
    }
    let goal = match target {
        Some(t) if prev.contains_key(t) => t.clone(),
        Some(_) => return Vec::new(),
        None => match prev.keys().max() {
            Some(v) => v.clone(),
            None => return Vec::new(),
        },
    };
    let mut chain = Vec::new();
    let mut v = goal;
    while let Some((from, i)) = prev.get(&v) {
        chain.push(packages[*i].clone());
        v = from.clone();
    }
    chain.reverse();
    chain
}
