use anyhow::{Context, Result};
use std::path::Path;

pub fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else {
            if let Some(p) = target.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Запись через временный файл и `rename`: обрыв не оставляет обрезанный JSON.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).with_context(|| format!("запись {}", path.display()))?;
    Ok(())
}

/// Пауза перед повтором номер `i` (с нуля): 1, 2, 4 … с, не дольше минуты.
pub fn backoff(i: u32) -> std::time::Duration {
    std::time::Duration::from_secs(1u64 << i.min(6)).min(std::time::Duration::from_secs(60))
}

/// Выполняет `op` до `attempts` раз (не меньше одного). Между попытками — `pause(i)`.
/// Отмену задания не повторяет: если флаг текущего задания выставлен, сразу возвращает ошибку.
pub fn retry<T>(
    what: &str,
    attempts: u32,
    mut pause: impl FnMut(u32),
    mut op: impl FnMut() -> Result<T>,
) -> Result<T> {
    let attempts = attempts.max(1);
    let mut i = 0;
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) if crate::process::check_cancel().is_err() => return Err(e),
            Err(e) if i + 1 >= attempts => {
                return Err(if attempts > 1 { e.context(format!("{what}: не удалось за {attempts} попыток")) } else { e });
            }
            Err(e) => {
                tracing::warn!("{what}: {e:#}; повтор {} из {attempts} через {} с", i + 2, backoff(i).as_secs());
                pause(i);
                i += 1;
            }
        }
    }
}

/// Пауза, которую прерывает отмена задания.
pub fn sleep_cancellable(d: std::time::Duration) {
    let end = std::time::Instant::now() + d;
    while std::time::Instant::now() < end {
        if crate::process::check_cancel().is_err() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250).min(end - std::time::Instant::now()));
    }
}

/// Полный путь к Windows PowerShell. Служба не ищет программы по PATH: в PATH могут оказаться
/// каталоги, куда пользователь может положить свой powershell.exe.
pub fn powershell_exe() -> std::path::PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    std::path::Path::new(&root).join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe")
}

/// Однопоточный tokio-runtime для синхронного кода.
pub fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    #[test]
    fn retry_then_succeeds_without_extra_attempts() {
        let mut n = 0;
        let mut pauses = Vec::new();
        let v = retry(
            "назначение",
            5,
            |i| pauses.push(i),
            || {
                n += 1;
                if n < 3 {
                    bail!("обрыв");
                }
                Ok(n)
            },
        )
        .unwrap();
        assert_eq!(v, 3);
        assert_eq!(pauses, vec![0, 1]);
    }

    #[test]
    fn retry_gives_up_and_keeps_the_last_error() {
        let mut n = 0;
        let err = format!(
            "{:#}",
            retry("SFTP", 3, |_| {}, || -> Result<()> {
                n += 1;
                bail!("сеть");
            })
            .unwrap_err()
        );
        assert_eq!(n, 3);
        assert!(err.contains("не удалось за 3 попыток") && err.contains("сеть"), "{err}");
    }

    #[test]
    fn single_attempt_keeps_error_text() {
        let err = format!("{:#}", retry("x", 0, |_| {}, || -> Result<()> { bail!("сеть") }).unwrap_err());
        assert_eq!(err, "сеть");
    }

    /// Отменённое задание не повторяется.
    #[test]
    fn retry_stops_on_cancel() {
        let flag = crate::process::CancelFlag::new();
        let _g = crate::process::enter_cancel(flag.clone());
        let mut n = 0;
        let r = retry("x", 5, |_| {}, || -> Result<()> {
            n += 1;
            flag.request("задание отменено");
            bail!("прервано");
        });
        assert!(r.is_err());
        assert_eq!(n, 1);
    }

    #[test]
    fn backoff_is_capped() {
        assert_eq!(backoff(0).as_secs(), 1);
        assert_eq!(backoff(3).as_secs(), 8);
        assert_eq!(backoff(30).as_secs(), 60);
    }
}
