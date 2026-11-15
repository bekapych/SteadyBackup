//! Глобальные настройки службы: `<data>\settings.json`.
//!
//! Рабочий каталог — место staging и архива. По умолчанию `<data>\work` на системном диске;
//! для больших копий его переносят на другой том. При старте служба удаляет оттуда только
//! недоделанные каталоги своих заданий (авария или отключение питания), чужие файлы не трогает.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceSettings {
    /// Абсолютный путь. Пусто — `<data>\work`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_dir: Option<String>,
    /// Проверка и установка новых версий.
    #[serde(default)]
    pub update: crate::update::UpdateSettings,
}

impl ServiceSettings {
    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("settings.json")
    }

    pub fn load(data_dir: &Path) -> Self {
        std::fs::read(Self::path(data_dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let p = Self::path(data_dir);
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &p).with_context(|| format!("запись {}", p.display()))?;
        Ok(())
    }
}

/// Рабочий каталог задания. Ошибка — путь задан, но им нельзя пользоваться.
pub fn work_dir(data_dir: &Path) -> Result<PathBuf> {
    resolve_work_dir(data_dir, &ServiceSettings::load(data_dir))
}

pub fn resolve_work_dir(data_dir: &Path, settings: &ServiceSettings) -> Result<PathBuf> {
    let Some(raw) = settings.work_dir.as_deref().filter(|s| !s.trim().is_empty()) else {
        return Ok(data_dir.join("work"));
    };
    let p = PathBuf::from(raw.trim());
    if !p.is_absolute() {
        bail!("рабочий каталог должен быть абсолютным: {}", p.display());
    }
    if unsafe_work_dir(data_dir, &p) {
        bail!(
            "рабочий каталог не может быть корнем диска, каталогом данных или папкой над ним: {}",
            p.display()
        );
    }
    Ok(p)
}

/// Корень диска, сам каталог данных или его предок. Очистка такого пути затронула бы чужие данные.
fn unsafe_work_dir(data_dir: &Path, work: &Path) -> bool {
    let work = norm(work);
    if is_drive_root(&work) {
        return true;
    }
    let data = norm(data_dir);
    data == work || data.starts_with(&(work.clone() + "\\"))
}

fn norm(p: &Path) -> String {
    let mut s = p.to_string_lossy().replace('/', "\\").to_lowercase();
    // `C:\` оставляем с завершающим слэшем, иначе корень не отличить от `C:`.
    if !is_drive_root(&s) {
        while s.ends_with('\\') {
            s.pop();
        }
    }
    s
}

fn is_drive_root(s: &str) -> bool {
    let b = s.as_bytes();
    (b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
        || (b.len() == 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\')
}

/// Имя каталога одного запуска: `<uuid задания>_<32 hex>`.
pub fn is_run_leftover(name: &std::ffi::OsStr) -> bool {
    let Some(s) = name.to_str() else { return false };
    let Some((id, nonce)) = s.rsplit_once('_') else { return false };
    uuid::Uuid::parse_str(id).is_ok() && nonce.len() == 32 && nonce.chars().all(|c| c.is_ascii_hexdigit())
}

/// Удаляет недоделанные каталоги заданий. Каталог сам остаётся; чужие имена не трогает.
/// Возвращает число удалённых объектов.
pub fn clean_work_dir(dir: &Path) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut n = 0;
    for entry in std::fs::read_dir(dir).with_context(|| format!("чтение {}", dir.display()))? {
        let entry = entry?;
        if !is_run_leftover(&entry.file_name()) {
            continue;
        }
        let p = entry.path();
        if p.is_dir() {
            std::fs::remove_dir_all(&p).with_context(|| format!("удаление {}", p.display()))?;
        } else {
            std::fs::remove_file(&p).with_context(|| format!("удаление {}", p.display()))?;
        }
        n += 1;
    }
    Ok(n)
}

/// Хватает ли места под ещё `need` байт. Неизвестный объём (не Windows, ошибка API) не блокирует запуск.
pub fn ensure_free(dir: &Path, need: u64) -> Result<()> {
    space_error(free_bytes(dir), need, dir)
}

pub fn space_error(free: Option<u64>, need: u64, dir: &Path) -> Result<()> {
    if let Some(free) = free {
        if free < need {
            bail!(
                "недостаточно места в рабочем каталоге {}: нужно {}, свободно {}",
                dir.display(),
                fmt_bytes(need),
                fmt_bytes(free)
            );
        }
    }
    Ok(())
}

pub fn fmt_bytes(b: u64) -> String {
    let units = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} Б") } else { format!("{v:.1} {}", units[i]) }
}

/// Свободно байт на томе `path` (ближайший существующий каталог). `None` — узнать не удалось.
pub fn free_bytes(path: &Path) -> Option<u64> {
    let existing = nearest_existing(path)?;
    free_bytes_of(&existing)
}

fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut p = path.to_path_buf();
    loop {
        if p.exists() {
            return Some(p);
        }
        if !p.pop() {
            return None;
        }
    }
}

#[cfg(windows)]
fn free_bytes_of(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut avail = 0u64;
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(avail)
}

#[cfg(not(windows))]
fn free_bytes_of(_: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("steady-work-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn default_work_dir_is_under_data() {
        let data = Path::new(r"C:\ProgramData\SteadyBackup");
        assert_eq!(resolve_work_dir(data, &ServiceSettings::default()).unwrap(), data.join("work"));
    }

    #[test]
    fn rejects_root_data_dir_and_its_parent() {
        let data = Path::new(r"C:\ProgramData\SteadyBackup");
        assert!(resolve_work_dir(data, &ServiceSettings { work_dir: Some(r"C:\".into()), ..Default::default() }).is_err());
        assert!(resolve_work_dir(data, &ServiceSettings { work_dir: Some(r"C:\ProgramData".into()), ..Default::default() }).is_err());
        assert!(resolve_work_dir(data, &ServiceSettings { work_dir: Some(data.display().to_string()), ..Default::default() }).is_err());
        assert!(resolve_work_dir(data, &ServiceSettings { work_dir: Some(r"work".into()), ..Default::default() }).unwrap_err().to_string().contains("абсолютным"));
        assert!(resolve_work_dir(data, &ServiceSettings { work_dir: Some(r"D:\Backup\work".into()), ..Default::default() }).is_ok());
    }

    #[test]
    fn clean_removes_only_run_leftovers() {
        let d = tmp();
        let id = uuid::Uuid::new_v4();
        let run = d.join(format!("{id}_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(run.join("staging")).unwrap();
        std::fs::write(run.join("staging").join("a.txt"), b"x").unwrap();
        std::fs::write(d.join("keep.txt"), b"y").unwrap();
        std::fs::create_dir_all(d.join("not-a-run")).unwrap();
        assert_eq!(clean_work_dir(&d).unwrap(), 1);
        assert!(!run.exists());
        assert!(d.join("keep.txt").is_file());
        assert!(d.join("not-a-run").is_dir());
        assert_eq!(clean_work_dir(&d).unwrap(), 0);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn missing_work_dir_is_already_clean() {
        let d = tmp().join("absent");
        assert_eq!(clean_work_dir(&d).unwrap(), 0);
    }

    #[test]
    fn space_error_names_the_shortage() {
        let dir = Path::new(r"E:\work");
        let err = space_error(Some(100), 200, dir).unwrap_err().to_string();
        assert!(err.contains("недостаточно места в рабочем каталоге"), "{err}");
        assert!(err.contains(r"E:\work"), "{err}");
        assert!(space_error(Some(200), 200, dir).is_ok());
        assert!(space_error(None, 200, dir).is_ok());
    }

    #[test]
    fn old_settings_file_has_empty_work_dir() {
        let s: ServiceSettings = serde_json::from_str("{}").unwrap();
        assert!(s.work_dir.is_none());
    }
}
