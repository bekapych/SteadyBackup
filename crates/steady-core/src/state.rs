//! Каталог состояния файлов для инкрементных и дифференциальных копий.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub size: u64,
    /// mtime, наносекунды от UNIX-эпохи.
    pub mtime: i128,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub files: BTreeMap<String, FileStamp>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobState {
    /// Снимок на момент последней полной копии.
    pub last_full: Option<Catalog>,
    /// Снимок на момент последней любой копии.
    pub last_any: Option<Catalog>,
    /// Сколько копий сделано после последней полной.
    pub since_full: u32,
    /// Hyper-V: InstanceID точки отслеживания изменений (RCT) после последней копии.
    #[serde(default)]
    pub hyperv_ref: Option<String>,
    /// Следующая копия должна быть полной: прошлая инкрементная или дифференциальная дошла
    /// не до всех назначений, и в отставшем останется дыра в цепочке.
    #[serde(default)]
    pub force_full: bool,
}

pub struct StateStore {
    dir: PathBuf,
}

impl StateStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path(&self, job_id: &uuid::Uuid) -> PathBuf {
        self.dir.join(format!("{job_id}.json"))
    }

    pub fn load(&self, job_id: &uuid::Uuid) -> Result<JobState> {
        let p = self.path(job_id);
        if !p.exists() {
            return Ok(JobState::default());
        }
        let data = std::fs::read(&p).with_context(|| format!("чтение {}", p.display()))?;
        Ok(serde_json::from_slice(&data)?)
    }

    pub fn save(&self, job_id: &uuid::Uuid, state: &JobState) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let p = self.path(job_id);
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(state)?)?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }
}

fn unix_nanos(t: std::time::SystemTime) -> i128 {
    t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i128).unwrap_or(0)
}

/// Штамп для каталога изменений и необязательная дата создания для фильтра.
/// Дату создания не сохраняем в `FileStamp`: иначе старый каталог задания счёл бы все файлы изменёнными.
pub fn metadata_of(path: &Path) -> Result<(FileStamp, Option<i128>)> {
    let md = std::fs::metadata(path)?;
    let mtime = md.modified().map(unix_nanos)?;
    let created = md.created().ok().map(unix_nanos);
    Ok((FileStamp { size: md.len(), mtime }, created))
}

pub fn stamp_of(path: &Path) -> Result<FileStamp> {
    metadata_of(path).map(|v| v.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_state_without_force_full_loads() {
        let st: JobState = serde_json::from_str(r#"{"since_full":2}"#).unwrap();
        assert!(!st.force_full);
        assert_eq!(st.since_full, 2);
    }
}
