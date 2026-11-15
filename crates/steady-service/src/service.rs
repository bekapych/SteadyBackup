//! Служба Windows: установка, удаление, запуск под SCM, а также консольный режим.

use crate::engine::Engine;
use anyhow::Result;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept, ServiceErrorControl,
    ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState,
    ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

pub const SERVICE_NAME: &str = "SteadyBackup";
const DISPLAY_NAME: &str = "SteadyBackup — резервное копирование";

/// Общая часть консольного режима и службы: запускает движок и IPC, ждёт сигнала остановки.
pub fn run_until(
    data_dir: PathBuf,
    seven_zip: Option<PathBuf>,
    stop: std::sync::mpsc::Receiver<()>,
) -> Result<()> {
    let engine = Engine::new(data_dir, seven_zip);
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let (tx, rx) = tokio::sync::watch::channel(false);
    let e = engine.clone();
    let ipc = rt.spawn(async move {
        let r = crate::ipc::serve(e, rx).await;
        if let Err(err) = &r {
            tracing::error!("IPC: {err:#}");
        }
        r
    });
    engine.start();
    let _ = stop.recv();
    tracing::info!("остановка службы");
    let _ = tx.send(true);
    engine.stop();
    // wait_hint SCM — 30 с. Дольше ждать нельзя: система всё равно завершит процесс.
    if !engine.wait_worker(Duration::from_secs(30)) {
        tracing::warn!("исполнитель не завершился за 30 с");
    }
    let _ = rt.block_on(ipc);
    Ok(())
}

pub fn run_console(data_dir: PathBuf, seven_zip: Option<PathBuf>) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let tx2 = tx.clone();
    std::thread::spawn(move || {
        let mut s = String::new();
        match std::io::stdin().read_line(&mut s) {
            // stdin закрыт (запуск без консоли) — работаем, пока процесс не завершат.
            Ok(0) | Err(_) => loop {
                std::thread::park();
            },
            Ok(_) => {
                let _ = tx2.send(());
            }
        }
    });
    println!("Служба запущена в консоли. Enter — остановить.");
    let _ = tx;
    run_until(data_dir, seven_zip, rx)
}

// ---- SCM ----

static SERVICE_ARGS: std::sync::OnceLock<(PathBuf, Option<PathBuf>)> = std::sync::OnceLock::new();

pub fn run_service(data_dir: PathBuf, seven_zip: Option<PathBuf>) -> Result<()> {
    let _ = SERVICE_ARGS.set((data_dir, seven_zip));
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_args: Vec<OsString>) {
    let (data_dir, seven_zip) = SERVICE_ARGS.get().cloned().unwrap_or_else(|| (crate::default_data_dir(), None));
    if let Err(e) = service_body(data_dir, seven_zip) {
        tracing::error!("служба: {e:#}");
    }
}

fn service_body(data_dir: PathBuf, seven_zip: Option<PathBuf>) -> Result<()> {
    // Права на каталог данных выставлены в main() до запуска диспетчера службы.
    let (tx, rx) = std::sync::mpsc::channel();
    // Дескриптор статуса появляется только после register, а Stop приходит в handler.
    let status_slot: std::sync::Arc<std::sync::Mutex<Option<service_control_handler::ServiceStatusHandle>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let slot = status_slot.clone();
    let handler = move |ev| match ev {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            if let Some(h) = slot.lock().unwrap().as_ref() {
                let _ = h.set_service_status(ServiceStatus {
                    service_type: ServiceType::OWN_PROCESS,
                    current_state: ServiceState::StopPending,
                    controls_accepted: ServiceControlAccept::empty(),
                    exit_code: ServiceExitCode::Win32(0),
                    checkpoint: 1,
                    wait_hint: Duration::from_secs(30),
                    process_id: None,
                });
            }
            let _ = tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(SERVICE_NAME, handler)?;
    *status_slot.lock().unwrap() = Some(status);
    let set = |state, accept| {
        status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(30),
            process_id: None,
        })
    };
    set(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN)?;
    tracing::info!("служба запущена");
    let r = run_until(data_dir, seven_zip, rx);
    set(ServiceState::Stopped, ServiceControlAccept::empty())?;
    r
}

pub fn install(data_dir: Option<PathBuf>) -> Result<()> {
    let dir = data_dir.clone().unwrap_or_else(crate::default_data_dir);
    steady_core::acl::secure_data_dir(&dir)?;
    println!("Права на {} — только SYSTEM и Администраторы.", dir.display());
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    let mut args = vec![];
    if let Some(d) = data_dir {
        args.push(OsString::from("--data-dir"));
        args.push(d.into_os_string());
    }
    args.push(OsString::from("service"));
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: args,
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let svc = mgr.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)?;
    svc.set_description("Резервное копирование файлов, баз 1С, MS SQL и виртуальных машин Hyper-V")?;
    let restart = ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(60) };
    svc.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86400)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![restart.clone(), restart.clone(), restart]),
    })?;
    svc.start::<&str>(&[])?;
    println!("Служба {SERVICE_NAME} установлена и запущена.");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let svc = mgr.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE)?;
    if svc.query_status()?.current_state != ServiceState::Stopped {
        let _ = svc.stop();
        for _ in 0..60 {
            if svc.query_status()?.current_state == ServiceState::Stopped {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    svc.delete()?;
    println!("Служба {SERVICE_NAME} удалена.");
    Ok(())
}
