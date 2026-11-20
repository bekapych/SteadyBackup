//! Файлы и папки (локальные, UNC, WebDAV): полная, инкрементная, дифференциальная копия.

use super::Prepared;
use crate::destination::webdav::WebDavClient;
use crate::job::{ArchiveMode, BackupKind};
use crate::state::{stamp_of, Catalog, FileStamp, JobState};
use anyhow::{bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub struct FilesParams<'a> {
    pub paths: &'a [String],
    pub user: Option<&'a str>,
    pub password: Option<&'a str>,
    pub include: &'a [String],
    pub exclude: &'a [String],
    pub requested: BackupKind,
    pub full_every: u32,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    kind: BackupKind,
    created: chrono::DateTime<chrono::Local>,
    sources: Vec<String>,
    copied: Vec<String>,
    /// Файлы, удалённые с момента базовой копии.
    deleted: Vec<String>,
    /// Файлы, которые не удалось прочитать (попадут в следующую копию).
    failed: Vec<String>,
}

pub(super) struct Filter {
    inc: Option<GlobSet>,
    exc: Option<GlobSet>,
}

impl Filter {
    pub(super) fn new(include: &[String], exclude: &[String]) -> Result<Self> {
        Ok(Filter { inc: build_set(include)?, exc: build_set(exclude)? })
    }

    pub(super) fn accepts(&self, rel: &str) -> bool {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        if let Some(s) = &self.inc {
            if !s.is_match(rel) && !s.is_match(name) {
                return false;
            }
        }
        if let Some(s) = &self.exc {
            if s.is_match(rel) || s.is_match(name) {
                return false;
            }
        }
        true
    }
}

pub(super) struct Run<'a> {
    base: Option<&'a Catalog>,
    pub(super) data_root: PathBuf,
    current: Catalog,
    copied: Vec<String>,
    failed: Vec<String>,
    pub(super) skipped_links: u32,
    pub(super) empty_dirs: u32,
    /// Ключи каталогов, которые не удалось обойти. Файлы под ними не видели и не считаем удалёнными.
    pub(super) unseen_prefixes: Vec<String>,
    bytes: u64,
}

impl Run<'_> {
    /// Нужно ли копировать файл; заодно учитывает его в текущем каталоге.
    pub(super) fn changed(&self, key: &str, stamp: &FileStamp) -> bool {
        self.base.map_or(true, |b| b.files.get(key) != Some(stamp))
    }

    pub(super) fn record(&mut self, key: String, stamp: FileStamp, copied: Option<u64>) {
        if let Some(n) = copied {
            self.bytes += n;
            self.copied.push(key.clone());
        }
        self.current.files.insert(key, stamp);
    }

    pub(super) fn fail(&mut self, key: String, err: impl std::fmt::Display) {
        tracing::warn!("{key}: {err}");
        // Не заносим в каталог: файл попадёт в следующую копию.
        self.failed.push(key);
    }

    pub(super) fn dst(&self, key: &str) -> Result<PathBuf> {
        let p = self.data_root.join(key.replace('/', "\\"));
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        Ok(p)
    }
}

pub fn prepare(p: &FilesParams, state: &JobState, staging: &Path) -> Result<Prepared> {
    if p.paths.is_empty() {
        bail!("не указаны исходные папки");
    }
    let filter = Filter::new(p.include, p.exclude)?;
    let kind = effective_kind(p.requested, p.full_every, state);
    let mut run = Run::start(kind, state, staging);

    for (idx, root) in p.paths.iter().enumerate() {
        let is_dav = root.starts_with("http://") || root.starts_with("https://");
        // Каждый источник в своей подпапке: "0_Docs", "1_Projects" ...
        let label = root_label(idx, root);
        if is_dav {
            scan_webdav(&mut run, &filter, root, &label, p.user, p.password)?;
        } else {
            scan_fs(&mut run, &filter, Path::new(root), &label)?;
        }
    }
    finish(run, kind, state, staging, p.paths)
}

/// Вид копии с учётом состояния: первая, принудительная и «полная каждые N» — полные.
pub(super) fn effective_kind(requested: BackupKind, full_every: u32, state: &JobState) -> BackupKind {
    match requested {
        BackupKind::Full => BackupKind::Full,
        _ if state.force_full || state.last_full.is_none() => BackupKind::Full,
        _ if full_every > 0 && state.since_full >= full_every => BackupKind::Full,
        k => k,
    }
}

/// Метка корня источника в каталоге и в архиве: "0_Docs", "1_Projects" …
pub(super) fn root_label(idx: usize, root: &str) -> String {
    let leaf = root.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or("root");
    let leaf = percent_encoding::percent_decode_str(leaf).decode_utf8_lossy();
    format!("{idx}_{}", crate::pipeline::safe_name(&leaf))
}

impl<'a> Run<'a> {
    pub(super) fn start(kind: BackupKind, state: &'a JobState, staging: &Path) -> Self {
        let base = match kind {
            BackupKind::Full => None,
            BackupKind::Differential => state.last_full.as_ref(),
            BackupKind::Incremental => state.last_any.as_ref(),
        };
        Run {
            base,
            data_root: staging.join("data"),
            current: Catalog::default(),
            copied: vec![],
            failed: vec![],
            skipped_links: 0,
            empty_dirs: 0,
            unseen_prefixes: vec![],
            bytes: 0,
        }
    }
}

/// Манифест, новое состояние и предупреждения по итогам обхода.
pub(super) fn finish(run: Run, kind: BackupKind, state: &JobState, staging: &Path, sources: &[String]) -> Result<Prepared> {
    // Удалён = был в базе и точно отсутствует сейчас. Непрочитанный файл и всё под каталогом,
    // который не удалось обойти, в базу не попали — это не удаление.
    let deleted: Vec<String> = run
        .base
        .map(|b| b.files.keys().filter(|k| !seen(&run, k)).cloned().collect())
        .unwrap_or_default();

    let manifest = Manifest {
        kind,
        created: chrono::Local::now(),
        sources: sources.to_vec(),
        copied: run.copied,
        deleted,
        failed: run.failed,
    };
    std::fs::create_dir_all(staging)?;
    std::fs::write(staging.join("manifest.json"), serde_json::to_vec_pretty(&manifest)?)
        .context("запись manifest.json")?;

    let current = run.current;
    let new_state = match kind {
        BackupKind::Full => {
            JobState { last_full: Some(current.clone()), last_any: Some(current), since_full: 0, hyperv_ref: None, force_full: false }
        }
        _ => JobState {
            last_full: state.last_full.clone(),
            last_any: Some(current),
            since_full: state.since_full + 1,
            hyperv_ref: None,
            force_full: false,
        },
    };
    let mut warnings = match manifest.failed.len() {
        0 => vec![],
        n => vec![format!("не удалось прочитать файлов: {n} (попадут в следующую копию), напр. {}", manifest.failed[0])],
    };
    if run.skipped_links > 0 {
        warnings.push(format!("пропущены символические ссылки: {}", run.skipped_links));
    }
    if run.empty_dirs > 0 {
        warnings.push(format!("пустые каталоги не копируются: {}", run.empty_dirs));
    }
    if !run.unseen_prefixes.is_empty() {
        warnings.push(format!(
            "не удалось обойти каталогов: {} (файлы в них не считаются удалёнными), напр. {}",
            run.unseen_prefixes.len(),
            run.unseen_prefixes[0]
        ));
    }
    Ok(Prepared { kind, new_state: Some(new_state), items: manifest.copied.len() as u64, bytes: run.bytes, warnings, packed: false, parts: vec![], failures: vec![] })
}

#[derive(Default)]
struct ArchiveUnit {
    copied: Vec<String>,
    deleted: Vec<String>,
    failed: Vec<String>,
}

/// Ключ архива для файла: весь путь, корень строки источника или его подпапка первого уровня.
fn archive_group(mode: ArchiveMode, key: &str) -> String {
    let (root, rel) = key.split_once('/').unwrap_or((key, ""));
    match mode {
        ArchiveMode::Single => "manifest".into(),
        ArchiveMode::PerFile => key.into(),
        ArchiveMode::PerSource => root.into(),
        ArchiveMode::PerDirectory => match rel.split_once('/') {
            Some((first, _)) => format!("{root}/{first}"),
            None => root.into(),
        },
    }
}

/// Раскладывает файловый staging по архивам и даёт каждому собственный манифест.
/// Без этого общий манифест требовал бы при восстановлении файлы из других архивов.
pub(crate) fn split_archives(
    staging: &Path,
    run_dir: &Path,
    set_name: &str,
    mode: ArchiveMode,
) -> Result<Vec<(PathBuf, String)>> {
    if mode == ArchiveMode::Single {
        return Ok(vec![(staging.to_path_buf(), set_name.into())]);
    }
    let manifest_path = staging.join("manifest.json");
    let manifest: Manifest = serde_json::from_slice(
        &std::fs::read(&manifest_path).with_context(|| format!("чтение {}", manifest_path.display()))?,
    )
    .with_context(|| format!("разбор {}", manifest_path.display()))?;

    let mut groups: std::collections::BTreeMap<String, ArchiveUnit> = std::collections::BTreeMap::new();
    if mode == ArchiveMode::PerSource {
        // Даже строка без изменений получает архив с пустым манифестом: это её копия в наборе.
        for (idx, source) in manifest.sources.iter().enumerate() {
            groups.entry(root_label(idx, source)).or_default();
        }
    }
    for key in &manifest.copied {
        groups.entry(archive_group(mode, key)).or_default().copied.push(key.clone());
    }
    for key in &manifest.deleted {
        groups.entry(archive_group(mode, key)).or_default().deleted.push(key.clone());
    }
    for key in &manifest.failed {
        // Непрочитанный файл сам по себе не требует нового архива; если архив его группы есть,
        // сведения об ошибке остаются и в его манифесте.
        if let Some(unit) = groups.get_mut(&archive_group(mode, key)) {
            unit.failed.push(key.clone());
        }
    }
    if groups.is_empty() {
        // Пустой инкремент всё равно должен оставить набор в назначении и участвовать в ротации.
        groups.insert(
            "manifest".into(),
            ArchiveUnit { copied: vec![], deleted: vec![], failed: manifest.failed.clone() },
        );
    }

    let label_bases: Vec<String> = groups.keys().map(|key| key.replace('/', ".")).collect();
    let labels = super::unique_names(&label_bases, &[]);
    let mut units = Vec::with_capacity(groups.len());
    for (idx, (_, unit)) in groups.into_iter().enumerate() {
        // Каталоги короткие и уникальные по номеру, даже если исходные имена совпали после очистки.
        let dir = run_dir.join("file-parts").join(format!("{idx:04}"));
        std::fs::create_dir_all(&dir).with_context(|| format!("создание {}", dir.display()))?;
        for key in &unit.copied {
            let rel = key.replace('/', "\\");
            let src = staging.join("data").join(&rel);
            let dst = dir.join("data").join(&rel);
            let parent = dst.parent().context("файл архива без родительского каталога")?;
            std::fs::create_dir_all(parent).with_context(|| format!("создание {}", parent.display()))?;
            std::fs::rename(&src, &dst).with_context(|| format!("перенос {}", src.display()))?;
        }
        let part_manifest = Manifest {
            kind: manifest.kind,
            created: manifest.created,
            sources: manifest.sources.clone(),
            copied: unit.copied,
            deleted: unit.deleted,
            failed: unit.failed,
        };
        std::fs::write(dir.join("manifest.json"), serde_json::to_vec_pretty(&part_manifest)?)
            .with_context(|| format!("запись манифеста в {}", dir.display()))?;
        units.push((dir, format!("{set_name}.{}", labels[idx])));
    }
    Ok(units)
}

fn scan_fs(run: &mut Run, filter: &Filter, root: &Path, label: &str) -> Result<()> {
    if !root.exists() {
        bail!("источник не найден: {}", root.display());
    }
    let mut dirs = std::collections::HashSet::new();
    let mut nonempty = std::collections::HashSet::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("пропуск: {e}");
                // Ошибка обхода — обычно недоступный каталог. Без префикса все файлы под ним
                // выпали бы из каталога и попали в deleted.
                if let Some(path) = e.path() {
                    if let Ok(rel) = path.strip_prefix(root) {
                        let rel = rel.to_string_lossy().replace('\\', "/");
                        let prefix = if rel.is_empty() { label.to_string() } else { format!("{label}/{rel}") };
                        run.unseen_prefixes.push(prefix);
                    }
                }
                continue;
            }
        };
        if entry.file_type().is_symlink() || entry.path_is_symlink() {
            run.skipped_links += 1;
            continue;
        }
        if entry.file_type().is_dir() {
            dirs.insert(entry.path().to_path_buf());
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let mut parent = entry.path().parent();
        while let Some(p) = parent {
            if !nonempty.insert(p.to_path_buf()) {
                break;
            }
            parent = p.parent();
        }
        let rel = entry.path().strip_prefix(root)?.to_string_lossy().replace('\\', "/");
        if !filter.accepts(&rel) {
            continue;
        }
        crate::process::check_cancel()?;
        let key = format!("{label}/{rel}");
        let stamp = match stamp_of(entry.path()) {
            Ok(s) => s,
            Err(e) => {
                run.fail(key, e);
                continue;
            }
        };
        if run.changed(&key, &stamp) {
            crate::settings::ensure_free(&run.data_root, stamp.size)?;
            let dst = run.dst(&key)?;
            match std::fs::copy(entry.path(), &dst) {
                Ok(n) => run.record(key, stamp, Some(n)),
                Err(e) => run.fail(key, e),
            }
        } else {
            run.record(key, stamp, None);
        }
    }
    run.empty_dirs += dirs.iter().filter(|d| !nonempty.contains(*d)).count() as u32;
    Ok(())
}

fn scan_webdav(run: &mut Run, filter: &Filter, url: &str, label: &str, user: Option<&str>, pw: Option<&str>) -> Result<()> {
    let client = WebDavClient::new(url, user, pw)?;
    let files = client.walk().with_context(|| format!("WebDAV-источник {url}"))?;
    for f in files {
        if !filter.accepts(&f.rel) {
            continue;
        }
        crate::process::check_cancel()?;
        let key = format!("{label}/{}", f.rel);
        let stamp = FileStamp { size: f.size, mtime: f.mtime };
        if run.changed(&key, &stamp) {
            crate::settings::ensure_free(&run.data_root, stamp.size)?;
            let dst = run.dst(&key)?;
            match client.get_file(&f.rel, &dst) {
                Ok(n) => run.record(key, stamp, Some(n)),
                Err(e) => run.fail(key, e),
            }
        } else {
            run.record(key, stamp, None);
        }
    }
    Ok(())
}

/// Файл из базы ещё существует (или мы не смогли это проверить).
fn seen(run: &Run, key: &str) -> bool {
    run.current.files.contains_key(key)
        || run.failed.iter().any(|f| f == key)
        || run.unseen_prefixes.iter().any(|p| key == p || key.starts_with(p) && key[p.len()..].starts_with('/'))
}

fn build_set(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p).with_context(|| format!("маска {p}"))?);
    }
    Ok(Some(b.build()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{FileDateField, FileDateFilter, FileDateMode};
    use chrono::TimeZone;
    use serde::Deserialize;

    fn at(y: i32, m: u32, d: u32, h: u32) -> chrono::DateTime<chrono::Local> {
        chrono::Local.with_ymd_and_hms(y, m, d, h, 0, 0).single().expect("дата")
    }

    fn ns(t: chrono::DateTime<chrono::Local>) -> i128 {
        t.timestamp_nanos_opt().expect("наносекунды") as i128
    }

    fn date_filter(field: FileDateField, mode: FileDateMode) -> FileDateFilter {
        FileDateFilter { field, mode, within_hours: 24, since: None }
    }

    #[test]
    fn relative_date_filter_accepts_only_last_24_hours() {
        let now = at(2026, 11, 20, 12);
        let filter = Filter::new(&[], &[], &date_filter(FileDateField::Modified, FileDateMode::Within), now).unwrap();
        let recent = FileStamp { size: 1, mtime: ns(at(2026, 11, 20, 11)) };
        let boundary = FileStamp { size: 1, mtime: ns(at(2026, 11, 19, 12)) };
        let old = FileStamp { size: 1, mtime: ns(at(2026, 11, 18, 13)) };
        assert!(filter.accepts_date(&recent, None).unwrap());
        assert!(filter.accepts_date(&boundary, None).unwrap(), "не старше суток — включая границу");
        assert!(!filter.accepts_date(&old, None).unwrap());
    }

    #[test]
    fn date_filter_can_use_creation_instead_of_modification() {
        let now = at(2026, 11, 20, 12);
        let filter = Filter::new(&[], &[], &date_filter(FileDateField::Created, FileDateMode::Within), now).unwrap();
        let stamp = FileStamp { size: 1, mtime: ns(at(2026, 11, 1, 12)) };
        assert!(filter.accepts_date(&stamp, Some(ns(at(2026, 11, 20, 11)))).unwrap());
        assert!(!filter.accepts_date(&stamp, Some(ns(at(2026, 11, 1, 12)))).unwrap());
        assert!(filter.accepts_date(&stamp, None).is_err(), "нет даты создания — явная ошибка, а не неполная копия");
    }

    #[test]
    fn fixed_date_filter_accepts_files_since_selected_moment() {
        let mut options = date_filter(FileDateField::Modified, FileDateMode::Since);
        options.since = Some("2026-11-19T12:00:00".into());
        let filter = Filter::new(&[], &[], &options, at(2026, 11, 20, 12)).unwrap();
        let before = FileStamp { size: 1, mtime: ns(at(2026, 11, 19, 11)) };
        let boundary = FileStamp { size: 1, mtime: ns(at(2026, 11, 19, 12)) };
        let after = FileStamp { size: 1, mtime: ns(at(2026, 11, 19, 13)) };
        assert!(!filter.accepts_date(&before, None).unwrap());
        assert!(filter.accepts_date(&boundary, None).unwrap());
        assert!(filter.accepts_date(&after, None).unwrap());
    }

    #[test]
    fn date_filtered_file_is_not_reported_as_deleted() {
        let mut run = run_stub(&[], &[]);
        run.date_filtered("0_src/old.txt".into());
        assert!(seen(&run, "0_src/old.txt"));
        assert!(!seen(&run, "0_src/other.txt"));
    }

    #[test]
    fn prepare_copies_only_files_matching_modified_date() {
        let src = tmp("date-filter");
        let recent = src.join("recent.txt");
        let old = src.join("old.txt");
        std::fs::write(&recent, b"recent").unwrap();
        std::fs::write(&old, b"old").unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&old).unwrap();
        file.set_times(
            std::fs::FileTimes::new().set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 3600)),
        )
        .unwrap();
        let paths = vec![src.to_string_lossy().into_owned()];
        let options = date_filter(FileDateField::Modified, FileDateMode::Within);
        let p = FilesParams {
            paths: &paths,
            user: None,
            password: None,
            include: &[],
            exclude: &[],
            date_filter: &options,
            requested: BackupKind::Full,
            full_every: 0,
        };
        let staging = tmp("date-filter-stage");
        let prepared = prepare(&p, &JobState::default(), &staging).unwrap();
        let man: Man = serde_json::from_slice(&std::fs::read(staging.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(prepared.items, 1);
        assert!(has_suffix(&man.copied, "/recent.txt"));
        assert!(!has_suffix(&man.copied, "/old.txt"));
        std::fs::remove_dir_all(src).ok();
        std::fs::remove_dir_all(staging).ok();
    }

    fn run_stub(failed: &[&str], prefixes: &[&str]) -> Run<'static> {
        Run {
            base: None,
            data_root: PathBuf::new(),
            current: Catalog::default(),
            copied: vec![],
            failed: failed.iter().map(|s| (*s).to_string()).collect(),
            unseen_prefixes: prefixes.iter().map(|s| (*s).to_string()).collect(),
            skipped_links: 0,
            empty_dirs: 0,
            bytes: 0,
        }
    }

    #[test]
    fn seen_keeps_failed_and_unreadable_prefixes() {
        let run = run_stub(&["0_src/busy.txt"], &["0_src/locked"]);
        assert!(seen(&run, "0_src/busy.txt"));
        assert!(seen(&run, "0_src/locked"));
        assert!(seen(&run, "0_src/locked/a.txt"));
        // Совпадение префикса без границы каталога — другой каталог.
        assert!(!seen(&run, "0_src/locked-other/a.txt"));
        assert!(!seen(&run, "0_src/gone.txt"));
    }

    #[derive(Deserialize)]
    struct Man {
        copied: Vec<String>,
        deleted: Vec<String>,
        failed: Vec<String>,
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("steady-files-{name}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn prepare_at(root: &Path, state: &JobState, kind: BackupKind) -> (PathBuf, Prepared, Man) {
        let staging = tmp("stage");
        let path = root.to_string_lossy().into_owned();
        let paths = vec![path];
        let p = FilesParams {
            paths: &paths,
            user: None,
            password: None,
            include: &[],
            exclude: &[],
            requested: kind,
            full_every: 0,
        };
        let prepared = prepare(&p, state, &staging).unwrap();
        let man: Man = serde_json::from_slice(&std::fs::read(staging.join("manifest.json")).unwrap()).unwrap();
        (staging, prepared, man)
    }

    fn has_suffix(keys: &[String], suffix: &str) -> bool {
        keys.iter().any(|k| k.ends_with(suffix))
    }

    /// (а) занятый файл из базы — в failed, не в deleted; (б) удалённый — в deleted;
    /// (в) после разблокировки файл попадает в следующую копию.
    #[cfg(windows)]
    #[test]
    fn locked_file_is_not_deleted_and_copied_later() {
        use std::os::windows::fs::OpenOptionsExt;
        let src = tmp("src");
        let busy = src.join("busy.txt");
        let gone = src.join("gone.txt");
        std::fs::write(&busy, b"busy").unwrap();
        std::fs::write(&gone, b"gone").unwrap();

        let (_, first, _) = prepare_at(&src, &JobState::default(), BackupKind::Full);
        let state = first.new_state.expect("состояние");

        std::fs::remove_file(&gone).unwrap();
        // Штамп должен отличаться от базы, иначе файл не читают и ошибка не возникает.
        std::fs::write(&busy, b"busy-changed").unwrap();
        // share_mode(0): чужое чтение (std::fs::copy) получает ERROR_SHARING_VIOLATION.
        let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&busy).unwrap();
        let (_, second, man) = prepare_at(&src, &state, BackupKind::Incremental);
        assert!(has_suffix(&man.failed, "/busy.txt"), "failed: {:?}", man.failed);
        assert!(!has_suffix(&man.deleted, "/busy.txt"), "deleted: {:?}", man.deleted);
        assert!(has_suffix(&man.deleted, "/gone.txt"), "deleted: {:?}", man.deleted);
        drop(lock);

        let state = second.new_state.expect("состояние");
        let (_, _, man) = prepare_at(&src, &state, BackupKind::Incremental);
        assert!(has_suffix(&man.copied, "/busy.txt"), "copied: {:?}", man.copied);
        assert!(!has_suffix(&man.deleted, "/busy.txt"), "deleted: {:?}", man.deleted);
        let _ = std::fs::remove_dir_all(&src);
    }

    /// Каталог, который не удалось прочитать, не превращает свои файлы в deleted.
    #[cfg(windows)]
    #[test]
    fn unreadable_dir_is_not_deleted() {
        let src = tmp("src");
        let sub = src.join("locked");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("secret.txt"), b"s").unwrap();
        std::fs::write(src.join("keep.txt"), b"k").unwrap();

        let (_, first, _) = prepare_at(&src, &JobState::default(), BackupKind::Full);
        let state = first.new_state.expect("состояние");

        let user = std::env::var("USERNAME").unwrap();
        let deny = std::process::Command::new("icacls")
            .arg(&sub)
            .arg("/deny")
            .arg(format!("{user}:(OI)(CI)(RX)"))
            .output()
            .unwrap();
        assert!(deny.status.success(), "icacls: {}", String::from_utf8_lossy(&deny.stderr));

        let (_, _, man) = prepare_at(&src, &state, BackupKind::Incremental);
        let _ = std::process::Command::new("icacls").arg(&sub).arg("/reset").output();
        assert!(
            !has_suffix(&man.deleted, "/secret.txt"),
            "deleted: {:?}, warnings would miss a prefix",
            man.deleted
        );
        assert!(!has_suffix(&man.deleted, "/keep.txt"), "deleted: {:?}", man.deleted);
        let _ = std::fs::remove_dir_all(&src);
    }
}
