//! Восстановление файловой цепочки: полная копия, затем инкрементные и дифференциальные по порядку.
//!
//! Каждый набор — каталог с `manifest.json` и `data\` (как в архиве задания). `deleted` из манифеста
//! удаляет файлы, которых уже нет в источнике. Результат повторяет дерево `data\`.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct Manifest {
    kind: crate::job::BackupKind,
    #[serde(default)]
    copied: Vec<String>,
    #[serde(default)]
    deleted: Vec<String>,
}

#[derive(Debug)]
pub struct RestoreReport {
    pub files: u64,
    pub deleted: u64,
    pub sets: usize,
}

/// Архивы .7z распаковываются во временный каталог; каталог с `manifest.json` берётся как есть.
pub fn restore(seven_zip: &Path, sets: &[PathBuf], password: Option<&str>, target: &Path) -> Result<RestoreReport> {
    let tmp = std::env::temp_dir().join(format!("steady-restore-{}", uuid::Uuid::new_v4().simple()));
    let mut extracted = Vec::new();
    for (i, set) in sets.iter().enumerate() {
        if set.join("manifest.json").is_file() {
            extracted.push(set.clone());
            continue;
        }
        let dir = tmp.join(format!("{i:03}"));
        std::fs::create_dir_all(&dir)?;
        extract_7z(seven_zip, set, password, &dir)?;
        extracted.push(dir);
    }
    let report = apply_sets(&extracted, target);
    let _ = std::fs::remove_dir_all(&tmp);
    report
}

fn extract_7z(seven_zip: &Path, archive: &Path, password: Option<&str>, dir: &Path) -> Result<()> {
    let mut cmd = std::process::Command::new(seven_zip);
    cmd.arg("x").arg("-y").arg("-bd").arg(format!("-o{}", dir.display()));
    cmd.arg(match password {
        Some(p) if !p.is_empty() => format!("-p{p}"),
        _ => "-p".into(),
    });
    cmd.arg(archive);
    // Пароль в аргументе — ограничение 7-Zip: stdin для -p эта сборка не принимает.
    let out = crate::process::run(cmd, std::time::Duration::from_secs(12 * 3600), "распаковка 7-Zip")?;
    if !out.status.success() {
        bail!("распаковка {}: {}", archive.display(), String::from_utf8_lossy(&out.stdout).lines().last().unwrap_or(""));
    }
    Ok(())
}

/// Применяет уже распакованные наборы к пустому или существующему каталогу.
/// Первый набор должен быть полной копией. Порядок — как передан (от старого к новому).
pub fn apply_sets(sets: &[PathBuf], target: &Path) -> Result<RestoreReport> {
    if sets.is_empty() {
        bail!("не указаны наборы для восстановления");
    }
    std::fs::create_dir_all(target)?;
    let mut files = 0u64;
    let mut deleted = 0u64;
    for (i, set) in sets.iter().enumerate() {
        let manifest: Manifest = read_manifest(set)?;
        if i == 0 && manifest.kind != crate::job::BackupKind::Full {
            bail!("цепочка должна начинаться с полной копии, а не с {}", manifest.kind.suffix());
        }
        let data = set.join("data");
        for rel in &manifest.copied {
            let src = data.join(rel.replace('/', "\\"));
            if !src.is_file() {
                bail!("{}: в наборе нет файла {}", set.display(), rel);
            }
            let dst = target.join(rel.replace('/', "\\"));
            if let Some(p) = dst.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::copy(&src, &dst).with_context(|| format!("копирование {}", rel))?;
            files += 1;
        }
        for rel in &manifest.deleted {
            let dst = target.join(rel.replace('/', "\\"));
            if dst.is_file() {
                std::fs::remove_file(&dst)?;
                deleted += 1;
            }
        }
    }
    Ok(RestoreReport { files, deleted, sets: sets.len() })
}

fn read_manifest(set: &Path) -> Result<Manifest> {
    let p = set.join("manifest.json");
    let text = std::fs::read_to_string(&p).with_context(|| format!("нет {}", p.display()))?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).with_context(|| format!("разбор {}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_dir(root: &Path, name: &str, manifest: &str, files: &[(&str, &str)]) -> PathBuf {
        let d = root.join(name);
        std::fs::create_dir_all(d.join("data")).unwrap();
        std::fs::write(d.join("manifest.json"), manifest).unwrap();
        for (rel, body) in files {
            let p = d.join("data").join(rel.replace('/', "\\"));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        d
    }

    #[test]
    fn full_then_incremental_applies_deletes() {
        let root = std::env::temp_dir().join(format!("steady-restore-{}", uuid::Uuid::new_v4().simple()));
        let full = set_dir(
            &root,
            "full",
            r#"{"kind":"full","copied":["0_src/a.txt","0_src/b.txt"],"deleted":[]}"#,
            &[("0_src/a.txt", "old"), ("0_src/b.txt", "gone")],
        );
        let inc = set_dir(
            &root,
            "inc",
            r#"{"kind":"incremental","copied":["0_src/a.txt"],"deleted":["0_src/b.txt"]}"#,
            &[("0_src/a.txt", "new")],
        );
        let target = root.join("out");
        let report = apply_sets(&[full, inc], &target).unwrap();
        assert_eq!(std::fs::read(target.join("0_src").join("a.txt")).unwrap(), b"new");
        assert!(!target.join("0_src").join("b.txt").exists());
        assert_eq!(report.deleted, 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn chain_must_start_with_full() {
        let root = std::env::temp_dir().join(format!("steady-restore-{}", uuid::Uuid::new_v4().simple()));
        let inc = set_dir(&root, "inc", r#"{"kind":"incremental","copied":[],"deleted":[]}"#, &[]);
        let err = apply_sets(&[inc], &root.join("out")).unwrap_err().to_string();
        assert!(err.contains("полной"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }
}
