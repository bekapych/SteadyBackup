//! Обновление программы: раз в сутки проверка выпуска на GitHub, загрузка установщика
//! с проверкой SHA-256 и тихая установка `msiexec` от имени службы.
//!
//! Установщик останавливает и удаляет службу (`steady.exe uninstall`), ставит файлы и
//! регистрирует службу заново. Поэтому перед запуском msiexec пишется `updates\pending.json`,
//! а новая служба при старте сверяет версию и сохраняет итог. Окно в трее установщик закрывает;
//! после успешного обновления служба запускает его снова в активных сеансах пользователей.

use crate::engine::Engine;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use steady_core::update::{self, Pending, Release};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Как часто проверять выпуски.
const CHECK_EVERY_HOURS: i64 = 24;
/// Первая проверка — не сразу после старта: не задерживаем запуск компьютера.
const FIRST_DELAY: Duration = Duration::from_secs(120);
/// Пауза цикла: в том числе повтор автоустановки, отложенной из-за задания.
const LOOP_EVERY: Duration = Duration::from_secs(600);

/// Сохраняется в `updates\state.json`: последняя проверка и итог прошлой установки.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    checked: Option<DateTime<Local>>,
    #[serde(default)]
    latest: Option<Release>,
    /// Ошибка последней проверки, загрузки или установки.
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    last_result: Option<String>,
    #[serde(default)]
    last_result_ok: Option<bool>,
    /// Версия, установка которой не удалась: автоматически её больше не ставим.
    #[serde(default)]
    failed_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Idle,
    Checking,
    Downloading,
    Installing,
}

pub struct Updater {
    data_dir: PathBuf,
    /// `steady console`: проверять можно, устанавливать — нет (это делает только служба).
    console: bool,
    st: Mutex<(Persisted, Phase)>,
}

impl Updater {
    pub fn new(data_dir: PathBuf, console: bool) -> Self {
        let p: Persisted = std::fs::read(data_dir.join("updates").join("state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { data_dir, console, st: Mutex::new((p, Phase::Idle)) }
    }

    fn dir(&self) -> PathBuf {
        self.data_dir.join("updates")
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (Persisted, Phase)> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, p: &Persisted) {
        let r = serde_json::to_vec_pretty(p)
            .map_err(anyhow::Error::from)
            .and_then(|b| steady_core::util::write_atomic(&self.dir().join("state.json"), &b));
        if let Err(e) = r {
            tracing::error!("обновление: запись состояния: {e:#}");
        }
    }

    fn update<R>(&self, f: impl FnOnce(&mut Persisted, &mut Phase) -> R) -> R {
        let mut g = self.lock();
        let (p, ph) = &mut *g;
        let r = f(p, ph);
        let snapshot = g.0.clone();
        drop(g);
        self.save(&snapshot);
        r
    }

    fn available(p: &Persisted) -> Option<&Release> {
        p.latest.as_ref().filter(|r| update::is_newer(&r.version, CURRENT))
    }

    /// Кратко для `status`: GUI опрашивает его каждые 2 с.
    pub fn brief(&self) -> Value {
        let g = self.lock();
        json!({ "available": Self::available(&g.0).map(|r| r.version.clone()), "phase": g.1 })
    }

    pub fn status(&self) -> Value {
        let s = steady_core::settings::ServiceSettings::load(&self.data_dir).update;
        let g = self.lock();
        let (p, phase) = (&g.0, g.1);
        json!({
            "current": CURRENT,
            "check": s.check,
            "auto_install": s.auto_install,
            "checked": p.checked,
            "latest": p.latest.as_ref().map(|r| json!({
                "version": r.version, "url": r.url, "notes": r.notes, "published": r.published, "size": r.size,
            })),
            "available": Self::available(p).is_some(),
            "phase": phase,
            "error": p.error,
            "last_result": p.last_result,
            "last_result_ok": p.last_result_ok,
            "can_install": !self.console,
        })
    }

    /// Запрос к GitHub. Ошибка сохраняется в состоянии и возвращается.
    pub fn check_now(&self) -> Result<Release> {
        {
            let mut g = self.lock();
            if g.1 != Phase::Idle {
                bail!("обновление уже проверяется или устанавливается");
            }
            g.1 = Phase::Checking;
        }
        let r = update::fetch_latest();
        self.update(|p, ph| {
            *ph = Phase::Idle;
            p.checked = Some(Local::now());
            match &r {
                Ok(rel) => {
                    if p.latest.as_ref().map(|l| &l.version) != Some(&rel.version) {
                        tracing::info!("обновление: последняя версия {} (установлена {CURRENT})", rel.version);
                    }
                    p.latest = Some(rel.clone());
                    p.error = None;
                }
                Err(e) => p.error = Some(format!("{e:#}")),
            }
        });
        r
    }

    /// Проверки перед установкой; возвращает выпуск.
    fn prepare_install(&self, engine: &Engine) -> Result<Release> {
        if self.console {
            bail!("установка обновлений выполняется только службой SteadyBackup, не в консольном режиме");
        }
        if engine.is_busy() {
            bail!("выполняется или ждёт очереди задание — дождитесь его завершения");
        }
        let mut g = self.lock();
        if g.1 != Phase::Idle {
            bail!("обновление уже проверяется или устанавливается");
        }
        let rel = Self::available(&g.0).cloned().context("новой версии нет — сначала проверьте обновления")?;
        g.1 = Phase::Downloading;
        Ok(rel)
    }

    /// Ручная установка из GUI: проверки сразу, загрузка и установка — в фоне.
    pub fn start_install(engine: &Arc<Engine>) -> Result<String> {
        let rel = engine.updater.prepare_install(engine)?;
        let e = engine.clone();
        let version = rel.version.clone();
        std::thread::Builder::new().name("update-install".into()).spawn(move || {
            if let Err(err) = e.updater.install(&e, rel) {
                tracing::error!("обновление: {err:#}");
            }
        })?;
        Ok(version)
    }

    /// Загрузка, проверка и запуск msiexec. Вызывается после `prepare_install`.
    fn install(&self, engine: &Arc<Engine>, rel: Release) -> Result<()> {
        let r = self.install_inner(engine, &rel);
        if let Err(e) = &r {
            let msg = format!("{e:#}");
            self.update(|p, ph| {
                *ph = Phase::Idle;
                p.error = Some(msg);
            });
        }
        r
    }

    fn install_inner(&self, engine: &Arc<Engine>, rel: &Release) -> Result<()> {
        tracing::info!("обновление: загрузка {} ({} байт)", rel.msi_name, rel.size);
        let msi = match update::download(rel, &self.dir()) {
            Ok(p) => p,
            Err(e) => {
                // Несовпадение суммы — не временная ошибка сети: эту версию сами больше не ставим.
                if format!("{e:#}").contains("не совпал") {
                    self.update(|p, _| p.failed_version = Some(rel.version.clone()));
                }
                return Err(e);
            }
        };
        update::clean_downloads(&self.dir(), Some(&msi));
        // Между проверкой и этим местом могло начаться задание.
        if !engine.begin_update() {
            bail!("началось задание — установка отложена");
        }
        let log = self.dir().join(format!("install-{}.log", rel.version));
        let pending = Pending { from: CURRENT.into(), to: rel.version.clone(), started: Local::now(), log: log.clone() };
        if let Err(e) = pending.save(&self.data_dir) {
            engine.end_update();
            return Err(e.context("запись pending.json"));
        }
        self.update(|p, ph| {
            *ph = Phase::Installing;
            p.error = None;
        });
        tracing::info!("обновление: установка {} → {}, журнал {}", CURRENT, rel.version, log.display());
        let msiexec = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("System32")
            .join("msiexec.exe");
        // Не через process::run: его Job Object завершил бы msiexec вместе со службой,
        // которую установщик как раз останавливает.
        let child = std::process::Command::new(&msiexec)
            .arg("/i")
            .arg(&msi)
            .args(["/qn", "/norestart", "/l*v"])
            .arg(&log)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let _ = Pending::take(&self.data_dir);
                engine.end_update();
                bail!("не удалось запустить msiexec: {e}");
            }
        };
        // При успехе установщик остановит службу раньше, чем msiexec завершится.
        // Дождались кода — значит, установка не состоялась.
        let status = child.wait();
        let _ = Pending::take(&self.data_dir);
        engine.end_update();
        let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
        let msg = match code {
            0 | 3010 => format!("установщик завершился (код {code}), но служба не перезапустилась; журнал: {}", log.display()),
            1618 => "уже выполняется другая установка Windows — повторите позже".to_string(),
            c => format!("установщик завершился с кодом {c}; журнал: {}", log.display()),
        };
        self.update(|p, ph| {
            *ph = Phase::Idle;
            p.last_result = Some(format!("Обновление до {} не установлено: {msg}", rel.version));
            p.last_result_ok = Some(false);
            if code != 1618 {
                p.failed_version = Some(rel.version.clone());
            }
        });
        bail!("{msg}")
    }

    /// Итог установки, начатой прошлым экземпляром службы.
    fn finish_pending(&self) {
        let Some(p) = Pending::take(&self.data_dir) else { return };
        let ok = p.to == CURRENT;
        let text = if ok {
            format!("Обновлено с версии {} до {} ({})", p.from, p.to, p.started.format("%d.%m.%Y %H:%M"))
        } else {
            format!("Обновление до {} не установлено (работает версия {CURRENT}); журнал: {}", p.to, p.log.display())
        };
        if ok {
            tracing::info!("{text}");
        } else {
            tracing::error!("{text}");
        }
        self.update(|st, _| {
            st.last_result = Some(text);
            st.last_result_ok = Some(ok);
            if ok {
                st.error = None;
            } else {
                st.failed_version = Some(p.to.clone());
            }
        });
        if ok {
            update::clean_downloads(&self.dir(), None);
            if !self.console {
                relaunch_gui();
            }
        }
    }

    /// Пора ли проверять по расписанию.
    fn check_due(&self) -> bool {
        let g = self.lock();
        g.0.checked.is_none_or(|t| Local::now() - t >= chrono::Duration::hours(CHECK_EVERY_HOURS))
    }

    /// Автоустановка: есть новая версия, она не падала раньше, служба свободна.
    fn auto_candidate(&self, engine: &Engine) -> bool {
        if self.console || engine.is_busy() {
            return false;
        }
        let g = self.lock();
        g.1 == Phase::Idle
            && Self::available(&g.0).is_some_and(|r| g.0.failed_version.as_deref() != Some(r.version.as_str()))
    }
}

/// Фоновый поток: итог прошлой установки, проверка раз в сутки, автоустановка.
pub fn spawn(engine: Arc<Engine>) {
    let r = std::thread::Builder::new().name("updater".into()).spawn(move || {
        engine.updater.finish_pending();
        if !engine.sleep_unless_stopped(FIRST_DELAY) {
            return;
        }
        loop {
            let s = steady_core::settings::ServiceSettings::load(&engine.data_dir).update;
            if s.check && engine.updater.check_due() {
                if let Err(e) = engine.updater.check_now() {
                    tracing::warn!("обновление: {e:#}");
                }
            }
            if s.check && s.auto_install && engine.updater.auto_candidate(&engine) {
                match engine.updater.prepare_install(&engine) {
                    Ok(rel) => {
                        if let Err(e) = engine.updater.install(&engine, rel) {
                            tracing::warn!("автообновление: {e:#}");
                        }
                    }
                    Err(e) => tracing::info!("автообновление отложено: {e:#}"),
                }
            }
            if !engine.sleep_unless_stopped(LOOP_EVERY) {
                return;
            }
        }
    });
    if let Err(e) = r {
        tracing::error!("обновление: поток не запущен: {e}");
    }
}

/// Запускает окно в трее в активных сеансах пользователей: установщик его закрыл,
/// а автозапуск сработает только при следующем входе в Windows.
#[cfg(windows)]
fn relaunch_gui() {
    let Some(exe) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("steady-gui.exe"))) else {
        return;
    };
    if !exe.is_file() {
        return;
    }
    for (session, r) in launch_in_sessions(&exe) {
        match r {
            Ok(()) => tracing::info!("обновление: окно SteadyBackup запущено в сеансе {session}"),
            Err(e) => tracing::warn!("обновление: окно в сеансе {session} не запущено: {e}"),
        }
    }
}

#[cfg(not(windows))]
fn relaunch_gui() {}

#[cfg(windows)]
fn launch_in_sessions(exe: &std::path::Path) -> Vec<(u32, std::result::Result<(), String>)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows_sys::Win32::System::RemoteDesktop::{
        WTSActive, WTSEnumerateSessionsW, WTSFreeMemory, WTSQueryUserToken, WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessAsUserW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
    };

    let mut out = Vec::new();
    let mut cmd: Vec<u16> = format!("\"{}\" --tray", exe.display()).encode_utf16().chain([0]).collect();
    let dir: Vec<u16> = exe.parent().unwrap_or(exe).as_os_str().encode_wide().chain([0]).collect();
    let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain([0]).collect();
    // SAFETY: буферы WTS освобождаются WTSFreeMemory, дескрипторы — CloseHandle в этой же функции.
    unsafe {
        let mut info: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
        let mut count = 0u32;
        if WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut info, &mut count) == 0 {
            return out;
        }
        let ids: Vec<u32> = std::slice::from_raw_parts(info, count as usize)
            .iter()
            .filter(|s| s.State == WTSActive)
            .map(|s| s.SessionId)
            .collect();
        WTSFreeMemory(info as _);
        for id in ids {
            let mut token: HANDLE = std::ptr::null_mut();
            if WTSQueryUserToken(id, &mut token) == 0 {
                out.push((id, Err(format!("нет пользователя (код {})", std::io::Error::last_os_error()))));
                continue;
            }
            let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
            let has_env = CreateEnvironmentBlock(&mut env, token, 0) != 0;
            let mut si: STARTUPINFOW = std::mem::zeroed();
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            si.lpDesktop = desktop.as_mut_ptr();
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let ok = CreateProcessAsUserW(
                token,
                std::ptr::null(),
                cmd.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                if has_env { CREATE_UNICODE_ENVIRONMENT } else { 0 },
                if has_env { env } else { std::ptr::null() },
                dir.as_ptr(),
                &si,
                &mut pi,
            );
            let err = std::io::Error::last_os_error();
            if has_env {
                DestroyEnvironmentBlock(env);
            }
            CloseHandle(token);
            if ok != 0 {
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
                out.push((id, Ok(())));
            } else {
                out.push((id, Err(err.to_string())));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(v: &str) -> Release {
        Release {
            version: v.into(),
            url: String::new(),
            notes: String::new(),
            published: None,
            msi_name: format!("SteadyBackup-{v}.msi"),
            msi_url: String::new(),
            size: 1,
            sha256: "0".repeat(64),
        }
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("steady-updater-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn only_newer_release_is_available() {
        let mut p = Persisted { latest: Some(rel("0.0.1")), ..Default::default() };
        assert!(Updater::available(&p).is_none());
        p.latest = Some(rel("999.0.0"));
        assert!(Updater::available(&p).is_some());
    }

    /// Итог установки записывает новая служба: совпала версия — успех, иначе ошибка и запрет автоповтора.
    #[test]
    fn pending_result_is_recorded() {
        let d = tmp();
        let p = Pending { from: "0.0.1".into(), to: CURRENT.into(), started: Local::now(), log: d.join("l.log") };
        p.save(&d).unwrap();
        let u = Updater::new(d.clone(), true);
        u.finish_pending();
        let s = u.status();
        assert_eq!(s["last_result_ok"], json!(true), "{s}");
        assert!(!Pending::path(&d).exists());

        let p = Pending { from: CURRENT.into(), to: "999.0.0".into(), started: Local::now(), log: d.join("l.log") };
        p.save(&d).unwrap();
        let u = Updater::new(d.clone(), true);
        u.finish_pending();
        assert_eq!(u.status()["last_result_ok"], json!(false));
        assert_eq!(u.lock().0.failed_version.as_deref(), Some("999.0.0"));
        // Состояние переживает перезапуск службы.
        let again = Updater::new(d.clone(), true);
        assert_eq!(again.status()["last_result_ok"], json!(false));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn console_mode_never_installs() {
        let d = tmp();
        let u = Updater::new(d.clone(), true);
        assert_eq!(u.status()["can_install"], json!(false));
        let _ = std::fs::remove_dir_all(&d);
    }
}
