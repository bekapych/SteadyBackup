//! Состояние загрузки обновлений: что уже скачано.
//!
//! Отдельный файл `<data>\state\<job_id>.download.json`: обычное состояние задания
//! (`JobState`, инкремент копий) к загрузке не относится, и писать в него чужие поля нельзя.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadState {
    /// Последняя скачанная версия платформы. Обновление ищется уже от неё.
    #[serde(default)]
    pub platform: Option<String>,
    /// Скачанные конфигурации: `programName` → версия, цепочка обновлений до которой
    /// скачана целиком. Запись одного звена цепочки сюда не попадает.
    #[serde(default)]
    pub configurations: BTreeMap<String, String>,
}

impl DownloadState {
    /// `<data>\state\<job_id>.download.json`.
    pub fn path(data_dir: &Path, job_id: &uuid::Uuid) -> PathBuf {
        data_dir.join("state").join(format!("{job_id}.download.json"))
    }

    /// Нет файла — пустое состояние (как у обычного состояния задания).
    pub fn load(data_dir: &Path, job_id: &uuid::Uuid) -> Result<Self> {
        let p = Self::path(data_dir, job_id);
        if !p.is_file() {
            return Ok(Self::default());
        }
        let data = std::fs::read(&p).with_context(|| format!("чтение {}", p.display()))?;
        serde_json::from_slice(&data).with_context(|| format!("формат {}", p.display()))
    }

    /// Атомарная запись: временный файл рядом и переименование (как в `state.rs`).
    pub fn save(&self, data_dir: &Path, job_id: &uuid::Uuid) -> Result<()> {
        let p = Self::path(data_dir, job_id);
        crate::util::write_atomic(&p, &serde_json::to_vec_pretty(self)?)
    }

    /// Версия, от которой ищется обновление: скачанная ранее, иначе начальная из задания.
    pub fn current<'a>(saved: Option<&'a String>, start: &'a str) -> &'a str {
        match saved.map(String::as_str).filter(|v| !v.trim().is_empty()) {
            Some(v) => v,
            None => start.trim(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_prefers_saved_version() {
        let saved = "8.3.25.1394".to_string();
        assert_eq!(DownloadState::current(Some(&saved), "8.3.24.1"), "8.3.25.1394");
        assert_eq!(DownloadState::current(None, " 8.3.24.1 "), "8.3.24.1");
        let blank = String::new();
        assert_eq!(DownloadState::current(Some(&blank), "8.3.24.1"), "8.3.24.1");
    }
}
