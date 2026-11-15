//! Запуск внешних программ с ограничением по времени.
//!
//! Исполнитель заданий один, поэтому зависшая программа (например, 1С с невидимым диалогом
//! в сеансе 0) останавливает все задания. Процесс помещается в Job Object с
//! `KILL_ON_JOB_CLOSE`: по тайм-ауту завершается всё дерево процессов, а если служба сама
//! аварийно завершится, система закроет Job Object и тоже убьёт потомков.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Отмена текущего задания. Клон указывает на тот же флаг: служба и GUI выставляют его,
/// конвейер и `process::run` на потоке исполнителя его читают.
#[derive(Clone, Debug)]
pub struct CancelFlag {
    inner: Arc<CancelInner>,
}

#[derive(Debug)]
struct CancelInner {
    set: AtomicBool,
    reason: Mutex<String>,
}

impl Default for CancelFlag {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelFlag {
    pub fn new() -> Self {
        Self { inner: Arc::new(CancelInner { set: AtomicBool::new(false), reason: Mutex::new(String::new()) }) }
    }

    pub fn request(&self, reason: impl Into<String>) {
        *self.inner.reason.lock().unwrap() = reason.into();
        self.inner.set.store(true, Ordering::SeqCst);
    }

    pub fn is_set(&self) -> bool {
        self.inner.set.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> Result<()> {
        if self.is_set() {
            bail!("{}", self.inner.reason.lock().unwrap());
        }
        Ok(())
    }
}

thread_local! {
    static ACTIVE_CANCEL: std::cell::RefCell<Option<CancelFlag>> = const { std::cell::RefCell::new(None) };
}

/// Пока guard жив, `run` и `check_cancel` на этом потоке видят флаг задания.
pub struct CancelGuard;

pub fn enter_cancel(flag: CancelFlag) -> CancelGuard {
    ACTIVE_CANCEL.with(|c| *c.borrow_mut() = Some(flag));
    CancelGuard
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        ACTIVE_CANCEL.with(|c| *c.borrow_mut() = None);
    }
}

/// Ошибка, если текущее задание отменено. Вне задания — всегда успех.
pub fn check_cancel() -> Result<()> {
    ACTIVE_CANCEL.with(|c| c.borrow().as_ref().map(|f| f.check()).unwrap_or(Ok(())))
}

/// Результат выполнения: код возврата и собранный вывод.
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Запускает команду, ждёт завершения не дольше `timeout`. Вывод собирается всегда.
/// По тайм-ауту дерево процессов завершается и возвращается ошибка с названием этапа `what`.
pub fn run(mut cmd: Command, timeout: Duration, what: &str) -> Result<Output> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().with_context(|| format!("{what}: не удалось запустить"))?;
    let job = KillOnClose::attach(&child);
    if let Err(e) = &job {
        // Без Job Object по тайм-ауту завершится только сам процесс, без потомков.
        tracing::warn!("{what}: Job Object недоступен: {e:#}");
    }

    // Читаем вывод в отдельных потоках: иначе процесс, заполнивший буфер канала, повиснет.
    let out_h = child.stdout.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = s.read_to_end(&mut b);
            b
        })
    });
    let err_h = child.stderr.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = s.read_to_end(&mut b);
            b
        })
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(st) = child.try_wait().with_context(|| format!("{what}: ожидание"))? {
            break Some(st);
        }
        if let Err(e) = check_cancel() {
            kill_tree(&job, &mut child);
            let _ = child.wait();
            drop(job);
            if let Some(h) = out_h {
                let _ = h.join();
            }
            if let Some(h) = err_h {
                let _ = h.join();
            }
            return Err(e);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    let Some(status) = status else {
        kill_tree(&job, &mut child);
        let _ = child.wait();
        // Потоки чтения завершатся, когда закроются каналы всех процессов дерева.
        drop(job);
        if let Some(h) = out_h {
            let _ = h.join();
        }
        if let Some(h) = err_h {
            let _ = h.join();
        }
        bail!("{what}: превышено время ожидания ({}), процесс завершён принудительно", fmt_duration(timeout));
    };

    let stdout = out_h.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = err_h.and_then(|h| h.join().ok()).unwrap_or_default();
    Ok(Output { status, stdout, stderr })
}

fn kill_tree(job: &Result<KillOnClose>, child: &mut std::process::Child) {
    match job {
        Ok(j) => j.terminate(),
        Err(_) => {
            let _ = child.kill();
        }
    }
}

pub fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    match (s / 3600, s / 60 % 60, s % 60) {
        (0, 0, s) => format!("{s} с"),
        (0, m, 0) => format!("{m} мин"),
        (0, m, s) => format!("{m} мин {s} с"),
        (h, 0, _) => format!("{h} ч"),
        (h, m, _) => format!("{h} ч {m} мин"),
    }
}

#[cfg(windows)]
struct KillOnClose(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for KillOnClose {}

#[cfg(windows)]
impl KillOnClose {
    fn attach(child: &std::process::Child) -> Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                bail!("CreateJobObject: {}", std::io::Error::last_os_error());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) != 0
                && AssignProcessToJobObject(h, child.as_raw_handle() as _) != 0;
            if !ok {
                let e = std::io::Error::last_os_error();
                CloseHandle(h);
                bail!("Job Object: {e}");
            }
            Ok(Self(h))
        }
    }

    fn terminate(&self) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for KillOnClose {
    fn drop(&mut self) {
        // KILL_ON_JOB_CLOSE: закрытие последнего дескриптора завершает оставшиеся процессы дерева.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(not(windows))]
struct KillOnClose;

#[cfg(not(windows))]
impl KillOnClose {
    fn attach(_: &std::process::Child) -> Result<Self> {
        bail!("Job Object поддерживается только в Windows")
    }
    fn terminate(&self) {}
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn ps(script: &str) -> Command {
        let mut c = Command::new(crate::util::powershell_exe());
        c.args(["-NoProfile", "-NonInteractive", "-Command", script]);
        c
    }

    #[test]
    fn collects_output_and_exit_code() {
        let out = run(ps("Write-Output hello; [Console]::Error.WriteLine('oops'); exit 3"), Duration::from_secs(60), "тест")
            .unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&out.stdout).contains("hello"));
        assert!(String::from_utf8_lossy(&out.stderr).contains("oops"));
    }

    /// Процесс и его потомок спят дольше тайм-аута: ошибка, оба процесса завершены.
    #[test]
    fn timeout_kills_process_tree() {
        let marker = format!("steady-timeout-{}", uuid::Uuid::new_v4().simple());
        // Потомок — отдельный powershell с маркером в командной строке, чтобы его можно было найти.
        let script = format!(
            "Start-Process -NoNewWindow -FilePath powershell -ArgumentList '-NoProfile','-Command','Start-Sleep 120 # {marker}'; Start-Sleep 120"
        );
        let started = Instant::now();
        let err = run(ps(&script), Duration::from_secs(4), "1С: выгрузка").err().unwrap().to_string();
        assert!(err.contains("1С: выгрузка: превышено время ожидания (4 с)"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(30), "ожидание не прервано");

        std::thread::sleep(Duration::from_secs(1));
        let check = Command::new(crate::util::powershell_exe())
            .args([
                "-NoProfile",
                "-Command",
                &format!("@(Get-CimInstance Win32_Process | Where-Object {{ $_.CommandLine -like '*{marker}*' -and $_.ProcessId -ne $PID }}).Count"),
            ])
            .output()
            .unwrap();
        let left = String::from_utf8_lossy(&check.stdout).trim().to_string();
        assert_eq!(left, "0", "потомок не завершён");
    }

    /// Отмена завершает процесс, не дожидаясь тайм-аута.
    #[test]
    fn cancel_kills_process() {
        let flag = CancelFlag::new();
        let _guard = enter_cancel(flag.clone());
        let f = flag.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            f.request("прервано остановкой службы");
        });
        let started = Instant::now();
        let err = run(ps("Start-Sleep 120"), Duration::from_secs(60), "тест").err().unwrap().to_string();
        assert!(err.contains("прервано остановкой службы"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(20), "ожидание не прервано");
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(Duration::from_secs(4)), "4 с");
        assert_eq!(fmt_duration(Duration::from_secs(1800)), "30 мин");
        assert_eq!(fmt_duration(Duration::from_secs(5400)), "1 ч 30 мин");
        assert_eq!(fmt_duration(Duration::from_secs(7200)), "2 ч");
    }
}
