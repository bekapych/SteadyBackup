//! Хранилище заданий и журнал выполнения в каталоге данных.
//!
//! ```text
//! <data>\jobs\<id>.json      задания
//! <data>\state\<id>.json     каталоги файлов для инкр./дифф. копий
//! <data>\history.jsonl       журнал запусков
//! <data>\scheduler.json      время последней проверки расписания по заданиям
//! <data>\groups\<id>.json    группы заданий с общим расписанием
//! <data>\group-marks.json    то же для групп
//! ```

use crate::group::Group;
use crate::job::Job;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub struct Store {
    pub dir: PathBuf,
    /// Загружать только задания, владелец файла которых — SYSTEM или «Администраторы».
    /// Включается для службы: защита на случай, если права на каталог данных снова ослабнут.
    pub require_admin_owner: bool,
    /// Последняя запись журнала по заданию. Заполняется хвостом файла, не всем журналом.
    last: std::sync::Mutex<HistoryCache>,
}

struct HistoryCache {
    ready: bool,
    by_job: HashMap<Uuid, HistoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub job_id: Uuid,
    pub job: String,
    pub started: chrono::DateTime<chrono::Local>,
    pub finished: chrono::DateTime<chrono::Local>,
    pub ok: bool,
    /// Копия создана, но часть назначений не удалась.
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub report: Option<crate::RunReport>,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), require_admin_owner: false, last: std::sync::Mutex::new(HistoryCache { ready: false, by_job: HashMap::new() }) }
    }

    /// Хранилище службы: задания чужих владельцев не загружаются.
    pub fn for_service(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), require_admin_owner: true, last: std::sync::Mutex::new(HistoryCache { ready: false, by_job: HashMap::new() }) }
    }

    fn jobs_dir(&self) -> PathBuf {
        self.dir.join("jobs")
    }

    pub fn load_jobs(&self) -> Result<Vec<Job>> {
        let mut v = Vec::new();
        let dir = self.jobs_dir();
        if !dir.exists() {
            return Ok(v);
        }
        for e in std::fs::read_dir(&dir)? {
            let p = e?.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            if self.require_admin_owner {
                match crate::acl::owned_by_admins(&p) {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::error!(
                            "{}: задание пропущено — владелец файла не SYSTEM и не «Администраторы»",
                            p.display()
                        );
                        continue;
                    }
                    Err(e) => {
                        tracing::error!("{}: задание пропущено: {e:#}", p.display());
                        continue;
                    }
                }
            }
            match load_job_file(&p) {
                Ok(j) => v.push(j),
                Err(e) => tracing::error!("{}: {e:#}", p.display()),
            }
        }
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }

    pub fn save_job(&self, job: &Job) -> Result<PathBuf> {
        std::fs::create_dir_all(self.jobs_dir())?;
        let p = self.jobs_dir().join(format!("{}.json", job.id));
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(job)?)?;
        std::fs::rename(&tmp, &p)?;
        Ok(p)
    }

    pub fn delete_job(&self, id: &Uuid) -> Result<()> {
        let p = self.jobs_dir().join(format!("{id}.json"));
        if p.exists() {
            std::fs::remove_file(p)?;
        }
        // Порядок группы с удалённым заданием не ломает: неизвестные id при запуске пропускаются.
        let _ = std::fs::remove_file(self.dir.join("state").join(format!("{id}.json")));
        // Состояние загрузки обновлений 1С живёт отдельным файлом.
        let _ = std::fs::remove_file(self.dir.join("state").join(format!("{id}.download.json")));
        Ok(())
    }

    pub fn append_history(&self, h: &HistoryEntry) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let p = self.dir.join("history.jsonl");
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p)?;
        writeln!(f, "{}", serde_json::to_string(h)?)?;
        drop(f);
        self.remember(h);
        // 8 МиБ: хвост переписывается, файл не растёт без предела.
        if p.metadata().map(|m| m.len()).unwrap_or(0) > 8 * 1024 * 1024 {
            let _ = rotate_history(&p, 2000);
        }
        Ok(())
    }

    /// Последняя запись по заданию, без чтения всего журнала.
    pub fn last(&self, id: &Uuid) -> Option<HistoryEntry> {
        self.ensure_cache();
        self.last.lock().unwrap().by_job.get(id).cloned()
    }

    fn remember(&self, h: &HistoryEntry) {
        self.ensure_cache();
        self.last.lock().unwrap().by_job.insert(h.job_id, h.clone());
    }

    fn ensure_cache(&self) {
        let mut g = self.last.lock().unwrap();
        if g.ready {
            return;
        }
        if let Ok(entries) = read_history_tail(&self.dir.join("history.jsonl"), 5000) {
            for e in entries {
                g.by_job.insert(e.job_id, e);
            }
        }
        g.ready = true;
    }

    /// Последние `limit` записей журнала (новые в конце). Читается хвост файла, не весь журнал.
    pub fn history(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        read_history_tail(&self.dir.join("history.jsonl"), limit)
    }

    pub fn load_marks(&self) -> HashMap<Uuid, chrono::NaiveDateTime> {
        std::fs::read(self.dir.join("scheduler.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save_marks(&self, m: &HashMap<Uuid, chrono::NaiveDateTime>) -> Result<()> {
        crate::util::write_atomic(&self.dir.join("scheduler.json"), &serde_json::to_vec(m)?)
    }

    // ---- группы ----

    fn groups_dir(&self) -> PathBuf {
        self.dir.join("groups")
    }

    /// Группы по имени. Повреждённый файл пропускается с записью в журнал, как у заданий.
    pub fn load_groups(&self) -> Result<Vec<Group>> {
        let mut v = Vec::new();
        let dir = self.groups_dir();
        let Ok(rd) = std::fs::read_dir(&dir) else { return Ok(v) };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            if self.require_admin_owner && !crate::acl::owned_by_admins(&p).unwrap_or(false) {
                tracing::error!("{}: группа пропущена — владелец файла не SYSTEM и не «Администраторы»", p.display());
                continue;
            }
            let parsed = std::fs::read_to_string(&p)
                .map_err(anyhow::Error::from)
                .and_then(|t| Ok(serde_json::from_str::<Group>(t.trim_start_matches('\u{feff}'))?));
            match parsed {
                Ok(g) => v.push(g),
                Err(err) => tracing::error!("{}: {err:#}", p.display()),
            }
        }
        v.sort_by_key(|g| g.name.to_lowercase());
        Ok(v)
    }

    pub fn save_group(&self, g: &Group) -> Result<()> {
        crate::util::write_atomic(&self.groups_dir().join(format!("{}.json", g.id)), &serde_json::to_vec_pretty(g)?)
    }

    /// Удаляет группу. Задания остаются, ссылка на группу у них снимается.
    pub fn delete_group(&self, id: &Uuid) -> Result<()> {
        for mut j in self.load_jobs()? {
            if j.group == Some(*id) {
                j.group = None;
                self.save_job(&j)?;
            }
        }
        let p = self.groups_dir().join(format!("{id}.json"));
        if p.exists() {
            std::fs::remove_file(p)?;
        }
        let mut marks = self.load_group_marks();
        if marks.remove(id).is_some() {
            self.save_group_marks(&marks)?;
        }
        Ok(())
    }

    pub fn load_group_marks(&self) -> HashMap<Uuid, chrono::NaiveDateTime> {
        std::fs::read(self.dir.join("group-marks.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save_group_marks(&self, m: &HashMap<Uuid, chrono::NaiveDateTime>) -> Result<()> {
        crate::util::write_atomic(&self.dir.join("group-marks.json"), &serde_json::to_vec(m)?)
    }
}

fn read_history_tail(path: &Path, limit: usize) -> Result<Vec<HistoryEntry>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let len = file.metadata()?.len();
    let window = (limit as u64).saturating_mul(2048).clamp(64 * 1024, 2 * 1024 * 1024);
    let start = len.saturating_sub(window);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    let mut lines = buf.lines();
    if start > 0 {
        lines.next();
    }
    let parsed: Vec<HistoryEntry> = lines.filter_map(|l| serde_json::from_str(l).ok()).collect();
    let skip = parsed.len().saturating_sub(limit);
    Ok(parsed.into_iter().skip(skip).collect())
}

fn rotate_history(path: &Path, keep: usize) -> Result<()> {
    let cutoff = chrono::Local::now() - chrono::Duration::days(180);
    let entries: Vec<HistoryEntry> = read_history_tail(path, keep)?.into_iter().filter(|e| e.finished >= cutoff).collect();
    let mut body = String::new();
    for e in &entries {
        body.push_str(&serde_json::to_string(e)?);
        body.push('\n');
    }
    crate::util::write_atomic(path, body.as_bytes())
}

pub fn load_job_file(path: &Path) -> Result<Job> {
    let text = std::fs::read_to_string(path).with_context(|| format!("чтение {}", path.display()))?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).with_context(|| format!("разбор {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job_json(id: &str, name: &str) -> String {
        format!(
            r#"{{"id":"{id}","name":"{name}","source":{{"type":"files","paths":["C:\\x"],"kind":"full"}},"destinations":[]}}"#
        )
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("steady-store-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(d.join("jobs")).unwrap();
        d
    }

    #[test]
    fn plain_store_loads_any_owner() {
        let d = tmp();
        std::fs::write(d.join("jobs").join("a.json"), job_json("11111111-1111-1111-1111-111111111111", "A")).unwrap();
        assert_eq!(Store::new(&d).load_jobs().unwrap().len(), 1);
        std::fs::remove_dir_all(&d).ok();
    }

    /// Файл, созданный обычным пользователем (тесты запускаются без повышения прав), служба не загружает.
    #[cfg(windows)]
    #[test]
    fn service_store_skips_user_owned_jobs() {
        let d = tmp();
        std::fs::write(d.join("jobs").join("a.json"), job_json("11111111-1111-1111-1111-111111111111", "A")).unwrap();
        if crate::acl::owned_by_admins(&d.join("jobs").join("a.json")).unwrap() {
            // Тест запущен от имени администратора с повышением — владелец будет «Администраторы».
            std::fs::remove_dir_all(&d).ok();
            return;
        }
        assert!(Store::for_service(&d).load_jobs().unwrap().is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn deleting_group_keeps_jobs_and_clears_reference() {
        let d = tmp();
        let store = Store::new(&d);
        let gid = Uuid::from_u128(7);
        let g = Group {
            id: gid,
            name: "Ночь".into(),
            enabled: true,
            schedule: crate::schedule::Schedule::Manual,
            run_missed: true,
            order: vec![],
            restarts: 0,
            restart_delay_minutes: 0,
            attempts: 0,
        };
        store.save_group(&g).unwrap();
        let mut j: Job = serde_json::from_str(&job_json("11111111-1111-1111-1111-111111111111", "A")).unwrap();
        j.group = Some(gid);
        store.save_job(&j).unwrap();
        assert_eq!(store.load_groups().unwrap().len(), 1);

        store.delete_group(&gid).unwrap();
        assert!(store.load_groups().unwrap().is_empty());
        let jobs = store.load_jobs().unwrap();
        assert_eq!(jobs.len(), 1, "задание не удаляется вместе с группой");
        assert!(jobs[0].group.is_none());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn history_tail_and_last_do_not_need_the_whole_file() {
        let d = tmp();
        let p = d.join("history.jsonl");
        let id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        let line = |n: i32| {
            format!(
                r#"{{"job_id":"{id}","job":"t","started":"2026-11-10T10:00:00+03:00","finished":"2026-11-10T10:00:0{n}+03:00","ok":true}}"#
            )
        };
        std::fs::write(&p, format!("{}\n{}\n{}\n", line(1), line(2), line(3))).unwrap();
        let tail = read_history_tail(&p, 2).unwrap();
        assert_eq!(tail.len(), 2);
        assert!(tail[1].finished > tail[0].finished);
        let store = Store::new(&d);
        assert_eq!(store.last(&id).unwrap().finished, tail[1].finished);
        std::fs::remove_dir_all(&d).ok();
    }
}
