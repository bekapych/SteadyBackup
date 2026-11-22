//! Конвейер: источник → staging → 7-Zip → назначения → ротация.

use crate::archive;
use crate::destination::{self, Target};
use crate::job::{BackupKind, Job, Source};
use crate::source;
use crate::state::StateStore;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

pub struct RunContext {
    /// Каталог данных программы (OAuth-токены и т.п.).
    pub data_dir: PathBuf,
    /// Временные каталоги заданий.
    pub work_dir: PathBuf,
    pub state: StateStore,
    pub seven_zip: Option<PathBuf>,
    /// Уведомление о текущем этапе (для GUI).
    pub on_phase: Option<Box<dyn Fn(&str) + Send + Sync>>,
    /// Остановка службы или отмена из GUI. Клоны смотрят на тот же флаг.
    pub cancel: crate::process::CancelFlag,
}

impl RunContext {
    fn phase(&self, s: &str) {
        tracing::info!("{s}");
        if let Some(f) = &self.on_phase {
            f(s);
        }
    }
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct RunReport {
    pub job: String,
    pub kind: BackupKind,
    pub set_name: String,
    pub items: u64,
    pub bytes: u64,
    pub destinations_ok: usize,
    /// Назначения, в которые копия не попала.
    pub errors: Vec<String>,
    /// Предупреждения источника (копия создана).
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Подходящей цепочки обновления не было: успешный no-op без копии и назначений.
    #[serde(default)]
    pub skipped: bool,
}

pub fn run_job(ctx: &RunContext, job: &Job) -> Result<RunReport> {
    // Дочерние процессы и обход файлов на этом потоке видят тот же флаг.
    let _cancel = crate::process::enter_cancel(ctx.cancel.clone());
    ctx.cancel.check()?;
    // Обновление 1С не является резервным набором: ветка строго до требования назначений,
    // загрузки состояния, staging и 7-Zip. Общие attempts к ней не применяются.
    if matches!(job.source, Source::OneCUpdate { .. }) {
        let src = crate::resources::Resources::new(&ctx.data_dir).resolve_source(&job.source)?;
        return source::onec_update::run(ctx, job, &src);
    }
    if job.destinations.is_empty() {
        bail!("в задании нет назначений");
    }
    let state = ctx.state.load(&job.id)?;
    let run_dir = ctx.work_dir.join(format!("{}_{}", job.id, uuid::Uuid::new_v4().simple()));
    let staging = run_dir.join("staging");
    std::fs::create_dir_all(&staging)?;
    let _cleanup = Cleanup(run_dir.clone());

    ctx.phase(&format!("«{}»: подготовка источника", job.name));
    let src = crate::resources::Resources::new(&ctx.data_dir).resolve_source(&job.source)?;
    let timeout = job.process_timeout();
    let pause = |i: u32| crate::util::sleep_cancellable(crate::util::backoff(i));
    let mut attempt = 0;
    // Источники с несколькими базами повторяют каждую базу сами.
    let outer_attempts = if src.retries_per_item() { 1 } else { job.step_attempts() };
    let prepared = crate::util::retry("подготовка источника", outer_attempts, pause, || {
        attempt += 1;
        if attempt > 1 {
            // Остатки прошлой попытки (недописанный .dt, часть файлов) не должны попасть в копию.
            reset_dir(&staging)?;
            ctx.phase(&format!("«{}»: подготовка источника, попытка {attempt}", job.name));
        }
        source::prepare(&src, &state, &staging, timeout, job.step_attempts())
    })?;

    let set_name = format!(
        "{}_{}_{}",
        safe_name(&job.name),
        chrono::Local::now().format("%Y%m%d_%H%M%S"),
        prepared.kind.suffix()
    );

    ctx.cancel.check()?;
    // Части копии: (каталог, имя объекта без .7z). Одна часть — весь staging под именем набора;
    // при отдельных архивах — по файлу, строке источника, подпапке или базе с именем
    // `<набор>.<метка>`. Такие имена разбираются `parse_set` как тот же набор, и ротация
    // удаляет их вместе.
    let units: Vec<(PathBuf, String)> = if job.archive.enabled && matches!(&src, Source::Files { .. } | Source::SshFiles { .. }) {
        source::files::split_archives(&staging, &run_dir, &set_name, job.archive.mode)?
    } else if src.separate_archives() && !prepared.parts.is_empty() {
        split_parts(&run_dir, &prepared.parts, &set_name)?
    } else {
        vec![(staging.clone(), set_name.clone())]
    };

    // Что отправляем: тома архивов или каталоги целиком.
    let mut pack_warnings = Vec::new();
    let mut payload: Vec<(PathBuf, String)> = Vec::new();
    if job.archive.enabled {
        let sz = archive::find_7zip(ctx.seven_zip.as_deref())?;
        // Архив пишется рядом со staging и в худшем случае сопоставим с ним по размеру.
        crate::settings::ensure_free(&ctx.work_dir, prepared.bytes.saturating_add(64 * 1024 * 1024))?;
        let mut opts = job.archive.clone();
        if prepared.packed {
            // Уже сжато SQL Server: 7-Zip только упаковывает (пароль, тома, проверка).
            opts.level = 0;
        }
        opts.password = crate::secret::reveal_opt(opts.password.as_deref())?;
        for (dir, name) in &units {
            ctx.cancel.check()?;
            let what = if units.len() > 1 { format!("упаковка 7-Zip {name}") } else { "упаковка 7-Zip".to_string() };
            ctx.phase(&format!("«{}»: {what}", job.name));
            let out = run_dir.join(format!("{name}.7z"));
            let mut attempt = 0;
            let (volumes, w) = crate::util::retry(&what, job.step_attempts(), pause, || {
                attempt += 1;
                // 7-Zip `a` дописывает в существующий архив: тома прошлой попытки удаляем.
                remove_archive(&run_dir, name)?;
                if attempt > 1 {
                    ctx.phase(&format!("«{}»: {what}, попытка {attempt}", job.name));
                }
                archive::pack(&sz, dir, &out, &opts, timeout)
            })?;
            pack_warnings.extend(w);
            payload.extend(volumes.into_iter().map(|p| {
                let n = p.file_name().unwrap().to_string_lossy().into_owned();
                (p, n)
            }));
        }
    } else {
        payload = units.clone();
    }

    let mut errors = Vec::new();
    let mut dest_warnings = Vec::new();
    let mut ok = 0;
    for dest in &job.destinations {
        ctx.cancel.check()?;
        let res = (|| -> Result<()> {
            // Обрыв соединения (SFTP, WebDAV) портит сеанс: после ошибки назначение открывается заново.
            let mut target: Option<Box<dyn Target>> = None;
            let mut first_open = true;
            for (local, name) in &payload {
                let mut attempt = 0;
                crate::util::retry(&format!("отправка {name}"), job.upload_attempts(), pause, || {
                    attempt += 1;
                    let t = match &mut target {
                        Some(t) => t,
                        None => {
                            let t = destination::open(dest, &ctx.data_dir)?;
                            if first_open {
                                dest_warnings.extend(t.warnings());
                                first_open = false;
                            }
                            target.insert(t)
                        }
                    };
                    let try_note = if attempt > 1 { format!(", попытка {attempt}") } else { String::new() };
                    ctx.phase(&format!("«{}»: отправка {name} → {}{try_note}", job.name, t.describe()));
                    let r = t.put(local, name).with_context(|| t.describe());
                    if r.is_err() {
                        target = None;
                    }
                    r
                })?;
            }
            let mut t = match target {
                Some(t) => t,
                None => destination::open(dest, &ctx.data_dir)?,
            };
            if job.retention.rotates() {
                rotate(t.as_mut(), &safe_name(&job.name), &job.retention, chrono::Local::now())?;
            }
            t.finish()
        })();
        match res {
            Ok(()) => ok += 1,
            Err(e) => {
                tracing::error!("назначение: {e:#}");
                errors.push(format!("{e:#}"));
            }
        }
    }

    // Состояние фиксируем, только если копия дошла хотя бы до одного назначения.
    let mut warnings = prepared.warnings;
    warnings.extend(pack_warnings);
    warnings.extend(dest_warnings);
    if ok > 0 {
        if let Some(st) = &prepared.new_state {
            let mut st = st.clone();
            if !errors.is_empty() {
                // Инкремент, дошедший не везде, сдвигает каталог. Без полной следующей копии
                // в отставшем назначении цепочка уже не соберётся.
                st.force_full = true;
                warnings.push(if prepared.kind == BackupKind::Full {
                    "полная копия дошла не до всех назначений: следующая тоже будет полной".into()
                } else {
                    "копия дошла не до всех назначений: следующая будет полной, иначе в отставшем назначении останется дыра в цепочке".into()
                });
            } else if prepared.kind == BackupKind::Full {
                st.force_full = false;
            }
            ctx.state.save(&job.id, &st).context("сохранение состояния задания")?;
        }
    } else {
        bail!("копия не сохранена ни в одно назначение: {}", errors.join("; "));
    }

    // Сохранённые базы уже отправлены; несохранённые делают запуск ошибочным
    // (с уведомлением и перезапусками).
    if !prepared.failures.is_empty() {
        let mut msg = format!(
            "не сохранено баз: {} (остальные отправлены в назначений: {ok}): {}",
            prepared.failures.len(),
            prepared.failures.join("; ")
        );
        if !errors.is_empty() {
            msg.push_str(&format!("; ошибки назначений: {}", errors.join("; ")));
        }
        bail!(msg);
    }

    Ok(RunReport {
        job: job.name.clone(),
        kind: prepared.kind,
        set_name,
        items: prepared.items,
        bytes: prepared.bytes,
        destinations_ok: ok,
        errors,
        warnings,
        skipped: false,
    })
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Раскладывает файлы частей по каталогам `run_dir\parts\<метка>` (перемещением, без копий).
/// Возвращает (каталог, `<набор>.<метка>`).
fn split_parts(
    run_dir: &std::path::Path,
    parts: &[(String, Vec<PathBuf>)],
    set_name: &str,
) -> Result<Vec<(PathBuf, String)>> {
    let root = run_dir.join("parts");
    let mut v = Vec::new();
    for (label, files) in parts {
        let dir = root.join(label);
        std::fs::create_dir_all(&dir).with_context(|| format!("создание {}", dir.display()))?;
        for f in files {
            let name = f.file_name().context("файл части без имени")?;
            std::fs::rename(f, dir.join(name)).with_context(|| format!("перенос {}", f.display()))?;
        }
        v.push((dir, format!("{set_name}.{label}")));
    }
    Ok(v)
}

/// Очищает каталог перед повторной подготовкой источника.
fn reset_dir(dir: &std::path::Path) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).with_context(|| format!("очистка {}", dir.display()))?;
    }
    std::fs::create_dir_all(dir).with_context(|| format!("создание {}", dir.display()))
}

/// Удаляет архив `<set>.7z` и его тома `<set>.7z.NNN` в `dir` (только их).
fn remove_archive(dir: &std::path::Path, set_name: &str) -> Result<()> {
    let base = format!("{set_name}.7z");
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let n = e.file_name().to_string_lossy().into_owned();
        let volume = n
            .strip_prefix(&base)
            .is_some_and(|rest| rest.is_empty() || (rest.len() == 4 && rest.starts_with('.') && rest[1..].chars().all(|c| c.is_ascii_digit())));
        if volume && e.file_type()?.is_file() {
            std::fs::remove_file(e.path()).with_context(|| format!("удаление {}", e.path().display()))?;
        }
    }
    Ok(())
}

pub fn safe_name(s: &str) -> String {
    let r: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
        .collect();
    if r.is_empty() { "job".into() } else { r }
}

/// Разбирает имя объекта: `<job>_<YYYYmmdd>_<HHMMSS>_<kind>[.7z[.001]]` → (ключ набора, вид).
fn parse_set(job: &str, name: &str) -> Option<(String, BackupKind)> {
    let rest = name.strip_prefix(job)?.strip_prefix('_')?;
    if rest.len() < 15 {
        return None;
    }
    let (stamp, tail) = rest.split_at(15);
    if !stamp.chars().enumerate().all(|(i, c)| if i == 8 { c == '_' } else { c.is_ascii_digit() }) {
        return None;
    }
    let tail = tail.strip_prefix('_')?;
    let kind_str = tail.split('.').next()?;
    let kind = match kind_str {
        "full" => BackupKind::Full,
        "inc" => BackupKind::Incremental,
        "diff" => BackupKind::Differential,
        _ => return None,
    };
    Some((format!("{stamp}_{kind_str}"), kind))
}

/// Оставляет последние полные цепочки (`keep_full`) и удаляет наборы без такой цепочки
/// по сроку (`keep_days`) и числу (`keep_sets`). Чужие имена и `.partial` не трогает.
pub fn rotate(
    t: &mut dyn Target,
    job: &str,
    retention: &crate::job::Retention,
    now: chrono::DateTime<chrono::Local>,
) -> Result<()> {
    let mut sets: BTreeMap<String, (BackupKind, Vec<String>)> = BTreeMap::new();
    for name in t.list()? {
        if name.ends_with(".partial") {
            continue;
        }
        if let Some((key, kind)) = parse_set(job, &name) {
            sets.entry(key).or_insert((kind, vec![])).1.push(name);
        }
    }
    let fulls: Vec<String> = sets.iter().filter(|(_, v)| v.0 == BackupKind::Full).map(|(k, _)| k.clone()).collect();

    // Ключи строго старше этой границы не входят в сохранённую полную цепочку.
    // None — полных нет или `keep_full` выключен: срок и число применяются ко всем наборам задания.
    let chain_from = (retention.keep_full > 0 && !fulls.is_empty()).then(|| {
        if fulls.len() > retention.keep_full as usize {
            fulls[fulls.len() - retention.keep_full as usize].clone()
        } else {
            fulls[0].clone()
        }
    });

    let mut doomed: BTreeSet<String> = BTreeSet::new();
    // Прежнее правило: лишние полные и всё, что старше самой старой из сохранённых.
    if let Some(from) = &chain_from {
        if fulls.len() > retention.keep_full as usize {
            doomed.extend(sets.keys().filter(|k| *k < from).cloned());
        }
    }
    // Наборы вне сохранённой цепочки (журнал/дифф. без своих полных, хвост до первой полной).
    let eligible: Vec<&String> = sets
        .keys()
        .filter(|k| match &chain_from {
            Some(from) => *k < from,
            None => true,
        })
        .collect();
    let n = eligible.len();
    for (i, key) in eligible.iter().enumerate() {
        let from_newest = n - 1 - i;
        let by_count = retention.keep_sets > 0 && from_newest >= retention.keep_sets as usize;
        let by_age = retention.keep_days > 0 && older_than(key, retention.keep_days, now);
        if by_count || by_age {
            doomed.insert((*key).clone());
        }
    }
    for key in doomed {
        for name in &sets[&key].1 {
            tracing::info!("ротация: удаление {name}");
            t.delete(name)?;
        }
    }
    Ok(())
}

/// Набор старше `days` суток относительно локального времени имени (`YYYYmmdd_HHMMSS`).
/// Неразобранную дату не удаляем.
fn older_than(key: &str, days: u32, now: chrono::DateTime<chrono::Local>) -> bool {
    let Some(stamp) = key.get(..15) else { return false };
    let Ok(t) = chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%d_%H%M%S") else { return false };
    let Some(cutoff) = now.naive_local().checked_sub_signed(chrono::Duration::days(i64::from(days))) else {
        return false;
    };
    t < cutoff
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::ArchiveMode;
    use chrono::TimeZone;

    fn file_staging(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("steady-file-units-{name}-{}", uuid::Uuid::new_v4().simple()));
        let staging = root.join("staging");
        let copied = [
            "0_Docs/a.txt",
            "0_Docs/Sub/b.txt",
            "0_Docs/Sub/deep/c.txt",
            "0_Docs/Other/d.txt",
            "1_Projects/root.txt",
            "1_Projects/Sub/e.txt",
        ];
        for key in copied {
            let path = staging.join("data").join(key.replace('/', "\\"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, key).unwrap();
        }
        let manifest = serde_json::json!({
            "kind": "incremental",
            "created": chrono::Local::now(),
            "sources": ["D:\\Docs", "D:\\Projects"],
            "copied": copied,
            "deleted": ["0_Docs/Sub/old.txt", "1_Projects/gone.txt"],
            "failed": []
        });
        std::fs::write(staging.join("manifest.json"), serde_json::to_vec(&manifest).unwrap()).unwrap();
        (root, staging)
    }

    fn manifest(dir: &std::path::Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap()
    }

    fn copied(dir: &std::path::Path) -> Vec<String> {
        serde_json::from_value(manifest(dir)["copied"].clone()).unwrap()
    }

    #[test]
    fn single_file_archive_keeps_common_staging() {
        let (root, staging) = file_staging("single");
        let units = source::files::split_archives(&staging, &root, "Docs_20261120_120000_inc", ArchiveMode::Single).unwrap();
        assert_eq!(units, vec![(staging.clone(), "Docs_20261120_120000_inc".into())]);
        assert_eq!(copied(&staging).len(), 6);
        assert!(staging.join("data/0_Docs/Sub/deep/c.txt").is_file());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn every_file_gets_archive_with_own_manifest() {
        let (root, staging) = file_staging("files");
        let units = source::files::split_archives(&staging, &root, "Docs_20261120_120000_inc", ArchiveMode::PerFile).unwrap();
        // Шесть изменённых и два удалённых: удаление тоже должно попасть в цепочку восстановления.
        assert_eq!(units.len(), 8);
        for (dir, _) in &units {
            let m = manifest(dir);
            assert_eq!(m["copied"].as_array().unwrap().len() + m["deleted"].as_array().unwrap().len(), 1);
        }
        assert!(units.iter().any(|(dir, _)| dir.join("data/0_Docs/Sub/deep/c.txt").is_file()));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn every_source_line_gets_archive_with_own_changes_and_deletes() {
        let (root, staging) = file_staging("sources");
        let units = source::files::split_archives(&staging, &root, "Docs_20261120_120000_inc", ArchiveMode::PerSource).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(copied(&units[0].0), vec!["0_Docs/a.txt", "0_Docs/Sub/b.txt", "0_Docs/Sub/deep/c.txt", "0_Docs/Other/d.txt"]);
        assert_eq!(copied(&units[1].0), vec!["1_Projects/root.txt", "1_Projects/Sub/e.txt"]);
        assert_eq!(manifest(&units[0].0)["deleted"], serde_json::json!(["0_Docs/Sub/old.txt"]));
        assert_eq!(manifest(&units[1].0)["deleted"], serde_json::json!(["1_Projects/gone.txt"]));
        assert!(!staging.join("data/0_Docs/a.txt").exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn every_first_level_directory_gets_archive_and_root_files_stay_together() {
        let (root, staging) = file_staging("dirs");
        let units = source::files::split_archives(&staging, &root, "Docs_20261120_120000_inc", ArchiveMode::PerDirectory).unwrap();
        assert_eq!(units.len(), 5);
        let by_suffix = |suffix: &str| &units.iter().find(|(_, name)| name.ends_with(suffix)).unwrap().0;
        assert_eq!(copied(by_suffix(".0_Docs")), vec!["0_Docs/a.txt"]);
        assert_eq!(copied(by_suffix(".0_Docs.Sub")), vec!["0_Docs/Sub/b.txt", "0_Docs/Sub/deep/c.txt"]);
        assert_eq!(copied(by_suffix(".0_Docs.Other")), vec!["0_Docs/Other/d.txt"]);
        assert_eq!(copied(by_suffix(".1_Projects")), vec!["1_Projects/root.txt"]);
        assert_eq!(copied(by_suffix(".1_Projects.Sub")), vec!["1_Projects/Sub/e.txt"]);
        assert_eq!(manifest(by_suffix(".0_Docs.Sub"))["deleted"], serde_json::json!(["0_Docs/Sub/old.txt"]));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn empty_per_file_run_creates_manifest_archive() {
        let root = std::env::temp_dir().join(format!("steady-file-units-empty-{}", uuid::Uuid::new_v4().simple()));
        let staging = root.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(
            staging.join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "kind": "incremental", "created": chrono::Local::now(), "sources": ["D:\\Docs"],
                "copied": [], "deleted": [], "failed": []
            }))
            .unwrap(),
        )
        .unwrap();
        let units = source::files::split_archives(&staging, &root, "Docs_20261120_120000_inc", ArchiveMode::PerFile).unwrap();
        assert_eq!(units.len(), 1);
        assert!(units[0].1.ends_with(".manifest"));
        assert!(copied(&units[0].0).is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn parse() {
        let (k, kind) = parse_set("Docs", "Docs_20261109_120000_inc.7z.002").unwrap();
        assert_eq!(k, "20261109_120000_inc");
        assert_eq!(kind, BackupKind::Incremental);
        assert!(parse_set("Docs", "Docs2_20261109_120000_inc.7z").is_none());
        assert!(parse_set("Docs", "Docs_2026_inc.7z").is_none());
    }

    /// Архивы отдельных баз — один набор с общим ключом: ротация удаляет их вместе.
    #[test]
    fn per_database_archives_share_set_key() {
        let a = parse_set("Sql", "Sql_20261109_120000_full.ERP.7z").unwrap();
        let b = parse_set("Sql", "Sql_20261109_120000_full.Учёт_2.7z.001").unwrap();
        let c = parse_set("Sql", "Sql_20261109_120000_full.7z").unwrap();
        assert_eq!(a, (c.0.clone(), BackupKind::Full));
        assert_eq!(b.0, c.0);
        let mut t = Mem(vec![
            "Sql_20261101_000000_full.A.7z".into(),
            "Sql_20261101_000000_full.B.7z".into(),
            "Sql_20261102_000000_full.A.7z".into(),
            "Sql_20261102_000000_full.B.7z".into(),
        ]);
        let r = crate::job::Retention { keep_full: 1, ..Default::default() };
        rotate(&mut t, "Sql", &r, at(2026, 11, 10)).unwrap();
        assert_eq!(t.0, vec!["Sql_20261102_000000_full.A.7z", "Sql_20261102_000000_full.B.7z"]);
    }

    #[test]
    fn split_parts_moves_files_per_label() {
        let d = std::env::temp_dir().join(format!("steady-parts-{}", uuid::Uuid::new_v4().simple()));
        let st = d.join("staging");
        std::fs::create_dir_all(&st).unwrap();
        std::fs::write(st.join("ERP.dump"), b"1").unwrap();
        std::fs::write(st.join("globals.sql"), b"2").unwrap();
        let parts = vec![
            ("ERP".to_string(), vec![st.join("ERP.dump")]),
            ("globals".to_string(), vec![st.join("globals.sql")]),
        ];
        let units = split_parts(&d, &parts, "Pg_20261109_120000_full").unwrap();
        assert_eq!(units[0].1, "Pg_20261109_120000_full.ERP");
        assert!(units[0].0.join("ERP.dump").is_file() && units[1].0.join("globals.sql").is_file());
        assert!(!st.join("ERP.dump").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    struct Mem(Vec<String>);

    impl Target for Mem {
        fn describe(&self) -> String {
            "mem".into()
        }
        fn put(&mut self, _: &std::path::Path, _: &str) -> Result<()> {
            Ok(())
        }
        fn list(&mut self) -> Result<Vec<String>> {
            Ok(self.0.clone())
        }
        fn delete(&mut self, name: &str) -> Result<()> {
            self.0.retain(|n| n != name);
            Ok(())
        }
    }

    fn at(y: i32, m: u32, d: u32) -> chrono::DateTime<chrono::Local> {
        chrono::Local.with_ymd_and_hms(y, m, d, 12, 0, 0).single().expect("дата")
    }

    fn has(t: &Mem, part: &str) -> bool {
        t.0.iter().any(|n| n.contains(part))
    }

    /// Журнальные наборы без полных: старые по сроку удаляются, чужие и `.partial` остаются.
    #[test]
    fn keep_days_drops_old_sets_without_fulls() {
        let mut t = Mem(vec![
            "Log_20261001_120000_inc.7z".into(),
            "Log_20261001_120000_inc.7z.001".into(),
            "Log_20261101_120000_inc.7z".into(),
            "Log_20261109_120000_inc.7z".into(),
            "Log_20261001_120000_inc.7z.partial".into(),
            "other.txt".into(),
            "OtherJob_20261001_120000_inc.7z".into(),
        ]);
        let r = crate::job::Retention { keep_full: 0, keep_days: 30, keep_sets: 0 };
        rotate(&mut t, "Log", &r, at(2026, 11, 10)).unwrap();
        assert!(!t.0.iter().any(|n| n == "Log_20261001_120000_inc.7z" || n == "Log_20261001_120000_inc.7z.001"));
        assert!(t.0.iter().any(|n| n == "Log_20261001_120000_inc.7z.partial"));
        assert!(has(&t, "Log_20261101"));
        assert!(has(&t, "Log_20261109"));
        assert!(t.0.iter().any(|n| n == "other.txt"));
        assert!(has(&t, "OtherJob_"));
    }

    /// Ровно на границе срока набор остаётся (удаляется только строго старше).
    #[test]
    fn keep_days_keeps_set_on_cutoff() {
        let mut t = Mem(vec!["Log_20261011_120000_inc.7z".into(), "Log_20261011_115959_inc.7z".into()]);
        let r = crate::job::Retention { keep_days: 30, ..Default::default() };
        rotate(&mut t, "Log", &r, at(2026, 11, 10)).unwrap();
        assert!(has(&t, "20261011_120000"));
        assert!(!has(&t, "20261011_115959"));
    }

    #[test]
    fn keep_sets_drops_oldest_without_fulls() {
        let mut t = Mem(vec![
            "Log_20261101_000000_inc.7z".into(),
            "Log_20261102_000000_inc.7z".into(),
            "Log_20261103_000000_inc.7z".into(),
            "Log_20261104_000000_diff.7z".into(),
            "Log_20261105_000000_inc.7z".into(),
        ]);
        let r = crate::job::Retention { keep_sets: 2, ..Default::default() };
        rotate(&mut t, "Log", &r, at(2026, 11, 10)).unwrap();
        assert_eq!(t.0.len(), 2);
        assert!(has(&t, "20261104"));
        assert!(has(&t, "20261105"));
    }

    /// Прежнее правило: две последние полные и всё не старше самой старой из них. Чужое не тронуто.
    #[test]
    fn keep_full_unchanged() {
        let mut t = Mem(vec![
            "J_20261101_000000_full.7z".into(),
            "J_20261102_000000_inc.7z".into(),
            "J_20261103_000000_full.7z".into(),
            "J_20261104_000000_inc.7z".into(),
            "J_20261105_000000_full.7z".into(),
            "stranger.bak".into(),
        ]);
        let r = crate::job::Retention { keep_full: 2, ..Default::default() };
        rotate(&mut t, "J", &r, at(2026, 11, 10)).unwrap();
        assert!(!has(&t, "20261101") && !has(&t, "20261102"));
        assert!(has(&t, "20261103") && has(&t, "20261104") && has(&t, "20261105"));
        assert!(t.0.iter().any(|n| n == "stranger.bak"));
    }

    /// Мало полных — `keep_full` ничего не удаляет, даже старое. Срок при этом цепочку не ломает.
    #[test]
    fn keep_full_under_limit_keeps_chain_despite_age() {
        let mut t = Mem(vec![
            "J_20260901_000000_inc.7z".into(),
            "J_20260915_000000_full.7z".into(),
            "J_20260916_000000_inc.7z".into(),
        ]);
        let r = crate::job::Retention { keep_full: 2, keep_days: 7, keep_sets: 0 };
        rotate(&mut t, "J", &r, at(2026, 11, 10)).unwrap();
        assert!(!has(&t, "20260901"), "хвост до полной удаляется по сроку");
        assert!(has(&t, "20260915") && has(&t, "20260916"), "цепочка сохранённой полной остаётся");
    }

    /// Два назначения, второе падает на инкременте: следующая копия полная в оба.
    #[test]
    fn partial_increment_forces_next_full() {
        let root = std::env::temp_dir().join(format!("steady-chain-{}", uuid::Uuid::new_v4().simple()));
        let src = root.join("src");
        let d1 = root.join("dest1");
        let d2 = root.join("dest2");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::create_dir_all(&d2).unwrap();
        std::fs::write(src.join("a.txt"), b"one").unwrap();

        let id = uuid::Uuid::new_v4();
        let job = |second: &std::path::Path| -> Job {
            serde_json::from_str(&format!(
                r#"{{"id":"{id}","name":"Docs","source":{{"type":"files","paths":["{src}"],"kind":"incremental","vss":false}},"destinations":[{{"type":"local","path":"{d1}"}},{{"type":"local","path":"{second}"}}],"archive":{{"enabled":false}}}}"#,
                src = src.display().to_string().replace('\\', "\\\\"),
                d1 = d1.display().to_string().replace('\\', "\\\\"),
                second = second.display().to_string().replace('\\', "\\\\"),
            ))
            .unwrap()
        };
        let ctx = RunContext {
            data_dir: root.join("data"),
            work_dir: root.join("work"),
            state: crate::state::StateStore::new(root.join("state")),
            seven_zip: None,
            on_phase: None,
            cancel: crate::process::CancelFlag::new(),
        };
        let first = run_job(&ctx, &job(&d2)).unwrap();
        assert_eq!(first.kind, BackupKind::Full, "без базы первая копия полная");
        assert!(first.errors.is_empty());

        std::fs::write(src.join("a.txt"), b"two").unwrap();
        let blocked = root.join("not-a-dir");
        std::fs::write(&blocked, b"x").unwrap();
        let second = run_job(&ctx, &job(&blocked)).unwrap();
        assert_eq!(second.kind, BackupKind::Incremental);
        assert_eq!(second.destinations_ok, 1);
        assert_eq!(second.errors.len(), 1);
        assert!(
            second.warnings.iter().any(|w| w.contains("следующая будет полной")),
            "{:?}",
            second.warnings
        );
        assert!(ctx.state.load(&id).unwrap().force_full);

        let third = run_job(&ctx, &job(&d2)).unwrap();
        assert_eq!(third.kind, BackupKind::Full);
        assert!(third.errors.is_empty(), "{:?}", third.errors);
        let full_in = |dir: &std::path::Path| {
            std::fs::read_dir(dir).unwrap().flatten().any(|e| e.file_name().to_string_lossy().ends_with("_full"))
        };
        assert!(full_in(&d1) && full_in(&d2), "полная копия должна попасть в оба назначения");
        assert!(!ctx.state.load(&id).unwrap().force_full);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Перед повтором упаковки удаляются только тома этого набора.
    #[test]
    fn remove_archive_drops_only_own_volumes() {
        let d = std::env::temp_dir().join(format!("steady-vol-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(d.join("staging")).unwrap();
        for n in ["S_1.7z", "S_1.7z.001", "S_1.7z.002", "S_1.7z.tmp", "S_10.7z", "other.txt"] {
            std::fs::write(d.join(n), b"x").unwrap();
        }
        remove_archive(&d, "S_1").unwrap();
        let mut left: Vec<String> =
            std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, vec!["S_1.7z.tmp", "S_10.7z", "other.txt", "staging"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn old_retention_json_has_new_limits_off() {
        let r: crate::job::Retention = serde_json::from_str(r#"{"keep_full":4}"#).unwrap();
        assert_eq!((r.keep_full, r.keep_days, r.keep_sets), (4, 0, 0));
        assert!(r.rotates());
        assert!(!crate::job::Retention::default().rotates());
    }
}
