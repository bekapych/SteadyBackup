mod engine;
mod ipc;
#[cfg(windows)]
mod service;
mod updater;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use steady_core::destination::gdrive;
use steady_core::state::StateStore;
use steady_core::store::{load_job_file, Store};
use steady_core::{run_job, RunContext};

#[derive(Parser)]
#[command(name = "steady", version, about = "SteadyBackup — резервное копирование")]
struct Cli {
    /// Каталог данных (задания, состояние, токены, журналы). По умолчанию %ProgramData%\SteadyBackup.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Путь к 7z.exe / 7za.exe.
    #[arg(long, global = true)]
    seven_zip: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Выполнить задание из JSON-файла (без службы).
    Run { job: PathBuf },
    /// Проверить назначения задания: подключение и список файлов.
    TestDest { job: PathBuf },
    /// Добавить/обновить задание в каталоге данных службы.
    Import { job: PathBuf },
    /// Экспорт заданий, хранилищ, подключений и настроек. Пароли в файле — открытым текстом.
    Export { file: PathBuf },
    /// Импорт файла, созданного `steady export`.
    ImportSettings {
        file: PathBuf,
        /// Удалить задания, хранилища и подключения, которых нет в файле.
        #[arg(long)]
        replace: bool,
    },
    /// Список заданий в каталоге данных.
    Jobs,
    /// Зашифровать пароль (DPAPI) для вставки в задание.
    Protect { secret: String },
    /// Авторизовать доступ к Google Drive.
    GdriveAuth {
        /// Имя учётной записи (используется в заданиях).
        account: String,
        /// Свой OAuth-клиент (по умолчанию — встроенный клиент SteadyBackup).
        #[arg(long, requires = "client_secret")]
        client_id: Option<String>,
        #[arg(long)]
        client_secret: Option<String>,
    },
    /// Показать найденную платформу 1С.
    Find1c,
    /// Восстановить файлы из цепочки наборов: полная, затем инкрементные по порядку.
    /// Аргументы — архивы .7z или уже распакованные каталоги с manifest.json.
    Restore {
        #[arg(required = true)]
        sets: Vec<PathBuf>,
        /// Каталог, куда развернуть цепочку. Должен быть пустым или новым.
        #[arg(long)]
        target: PathBuf,
        #[arg(long)]
        password: Option<String>,
    },
    /// Восстановить ВМ Hyper-V из цепочки архивов: полная, затем инкрементные по порядку.
    RestoreVm {
        /// Архивы .7z (для многотомных — первый том .7z.001).
        #[arg(required = true)]
        archives: Vec<PathBuf>,
        /// Каталог для восстановленной ВМ.
        #[arg(long)]
        target: PathBuf,
        /// Пароль архивов (или dpapi:...).
        #[arg(long)]
        password: Option<String>,
        /// Имя ВМ после импорта.
        #[arg(long)]
        name: Option<String>,
        /// Только собрать диски и конфигурацию, не регистрировать ВМ в Hyper-V.
        #[arg(long)]
        no_register: bool,
    },
    /// Отправить тестовое уведомление: email | telegram.
    TestNotify { channel: String },
    /// Запустить планировщик и IPC в консоли (для отладки).
    Console,
    /// Точка входа службы (вызывается SCM).
    #[command(hide = true)]
    Service,
    /// Проверить, можно ли запускать программу от имени службы (права на файл и каталоги).
    #[command(hide = true)]
    CheckExe { path: PathBuf },
    /// Отправить JSON-команду работающей службе, напр. '{"cmd":"list_jobs"}' или @файл.json.
    Ipc { json: String },
    /// Установить и запустить службу Windows.
    Install,
    /// Остановить и удалить службу Windows.
    Uninstall,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let data_dir = cli.data_dir.clone().unwrap_or_else(default_data_dir);
    // Служба: права на каталог данных выставляются до создания журнала и любых файлов.
    // Ошибку пишем после инициализации журнала, но службу без защищённого каталога не запускаем.
    let secured = if matches!(cli.cmd, Cmd::Service) { Some(steady_core::acl::secure_data_dir(&data_dir)) } else { None };
    let _guard = init_logging(&data_dir, matches!(cli.cmd, Cmd::Service | Cmd::Console));
    if let Some(Err(e)) = secured {
        tracing::error!("каталог данных {}: {e:#}", data_dir.display());
        return Err(e);
    }
    // Свой рабочий каталог (не <data>\work) тоже только для SYSTEM и Администраторов.
    if matches!(cli.cmd, Cmd::Service) {
        if let Ok(work) = steady_core::settings::work_dir(&data_dir) {
            if work != data_dir.join("work") {
                if let Err(e) = steady_core::acl::secure_data_dir(&work) {
                    tracing::error!("рабочий каталог {}: {e:#}", work.display());
                    return Err(e);
                }
            }
        }
    }

    match cli.cmd {
        Cmd::Run { job } => {
            let job = load_job_file(&job)?;
            let ctx = RunContext {
                data_dir: data_dir.clone(),
                work_dir: steady_core::settings::work_dir(&data_dir)?,
                state: StateStore::new(data_dir.join("state")),
                seven_zip: cli.seven_zip,
                on_phase: None,
                cancel: steady_core::process::CancelFlag::new(),
            };
            let report = run_job(&ctx, &job)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Cmd::TestDest { job } => {
            let job = load_job_file(&job)?;
            for d in &job.destinations {
                match steady_core::destination::open(d, &data_dir).and_then(|mut t| {
                    let n = t.list()?.len();
                    Ok((t.describe(), n))
                }) {
                    Ok((name, n)) => println!("OK     {name}: объектов {n}"),
                    Err(e) => println!("ОШИБКА {e:#}"),
                }
            }
        }
        Cmd::Export { file } => {
            let (bundle, warnings) = steady_core::bundle::export_dir(&data_dir)?;
            let text = serde_json::to_string_pretty(&bundle)?;
            steady_core::util::write_atomic(&file, text.as_bytes())?;
            for w in &warnings {
                println!("предупреждение: {w}");
            }
            println!(
                "Записано: {} (заданий: {}, хранилищ: {}, подключений: {})",
                file.display(),
                bundle.jobs.len(),
                bundle.storages.len(),
                bundle.connections.len()
            );
            println!("В файле пароли открытым текстом. Удалите его после переноса.");
        }
        Cmd::ImportSettings { file, replace } => {
            let text = std::fs::read_to_string(&file).with_context(|| format!("чтение {}", file.display()))?;
            let bundle = steady_core::bundle::parse(&text)?;
            let report = steady_core::bundle::import_dir(&data_dir, &bundle, replace)?;
            if bundle.settings.as_ref().is_some_and(|s| s.work_dir.is_some()) {
                if let Ok(path) = steady_core::settings::work_dir(&data_dir) {
                    if path != data_dir.join("work") {
                        if let Err(e) = steady_core::acl::secure_data_dir(&path) {
                            println!("предупреждение: права на рабочий каталог не выставлены: {e:#}");
                        }
                    }
                }
            }
            println!(
                "Импортировано: заданий {}, хранилищ {}, подключений {}",
                report.jobs, report.storages, report.connections
            );
            if replace {
                println!(
                    "Удалено: заданий {}, хранилищ {}, подключений {}",
                    report.deleted_jobs, report.deleted_storages, report.deleted_connections
                );
            }
            for w in &report.warnings {
                println!("предупреждение: {w}");
            }
        }
        Cmd::Import { job } => {
            let job = load_job_file(&job)?;
            // Если служба запущена (и каталог данных не переопределён) — через неё, иначе напрямую в файлы.
            #[cfg(windows)]
            if cli.data_dir.is_none() {
                if let Ok(v) = ipc_call(&serde_json::json!({ "cmd": "save_job", "job": job })) {
                    println!("{} → служба: {}", job.name, v);
                    return Ok(());
                }
            }
            let p = Store::new(&data_dir).save_job(&job)?;
            println!("{} → {}", job.name, p.display());
        }
        Cmd::Jobs => {
            for j in Store::new(&data_dir).load_jobs()? {
                let next = j.schedule.next_after(chrono::Local::now().naive_local());
                println!(
                    "{}  {:<30} {}  след.: {}",
                    j.id,
                    j.name,
                    if j.enabled { "вкл " } else { "выкл" },
                    next.map(|d| d.format("%d.%m.%Y %H:%M").to_string()).unwrap_or_else(|| "—".into())
                );
            }
        }
        Cmd::Protect { secret } => println!("{}", steady_core::secret::protect_str(&secret)?),
        Cmd::GdriveAuth { account, client_id, client_secret } => {
            let id = client_id.unwrap_or_else(|| gdrive::BUILTIN_CLIENT_ID.into());
            let secret = client_secret.unwrap_or_else(|| gdrive::BUILTIN_CLIENT_SECRET.into());
            let p = gdrive::authorize(&gdrive::TokenStore::new(&data_dir), &account, &id, &secret)?;
            println!("Сохранено: {}", p.display());
        }
        Cmd::Restore { sets, target, password } => {
            let sz = steady_core::archive::find_7zip(cli.seven_zip.as_deref())?;
            let pw = steady_core::secret::reveal_opt(password.as_deref())?;
            let report = steady_core::restore::restore(&sz, &sets, pw.as_deref(), &target)?;
            println!("Восстановлено файлов: {}, удалено: {}, наборов: {}", report.files, report.deleted, report.sets);
        }
        Cmd::RestoreVm { archives, target, password, name, no_register } => {
            let sz = steady_core::archive::find_7zip(cli.seven_zip.as_deref())?;
            let pw = steady_core::secret::reveal_opt(password.as_deref())?;
            let r = steady_core::source::hyperv::restore(&sz, &archives, pw.as_deref(), &target, name.as_deref(), !no_register)?;
            println!("{r}");
        }
        Cmd::CheckExe { path } => match steady_core::acl::check_trusted_exe(&path) {
            Ok(()) => println!("OK     {}", path.display()),
            Err(e) => println!("ОТКАЗ  {e:#}"),
        },
        Cmd::TestNotify { channel } => {
            let s = steady_core::notify::NotifySettings::load(&data_dir);
            steady_core::notify::send_test(&s, &channel)?;
            println!("отправлено");
        }
        Cmd::Find1c => match steady_core::source::onec::find_platform() {
            Some(p) => println!("{}", p.display()),
            None => println!("платформа 1С не найдена"),
        },
        #[cfg(windows)]
        Cmd::Console => service::run_console(data_dir, cli.seven_zip)?,
        #[cfg(windows)]
        Cmd::Service => service::run_service(data_dir, cli.seven_zip)?,
        #[cfg(windows)]
        Cmd::Ipc { json } => {
            let text = match json.strip_prefix('@') {
                Some(path) => std::fs::read_to_string(path)?,
                None => json,
            };
            let req: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))?;
            let v = ipc_call(&req)?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        #[cfg(windows)]
        Cmd::Install => service::install(cli.data_dir)?,
        #[cfg(windows)]
        Cmd::Uninstall => service::uninstall()?,
        #[cfg(not(windows))]
        _ => anyhow::bail!("служба поддерживается только в Windows"),
    }
    Ok(())
}

/// Запрос к работающей службе. Возвращает `result` или ошибку из ответа.
#[cfg(windows)]
fn ipc_call(req: &serde_json::Value) -> Result<serde_json::Value> {
    use std::io::{BufRead, BufReader, Write};
    let mut pipe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(ipc::pipe_name())
        .map_err(|e| anyhow::anyhow!("служба недоступна ({}): {e}", ipc::pipe_name()))?;
    writeln!(pipe, "{}", serde_json::to_string(req)?)?;
    let mut line = String::new();
    BufReader::new(pipe).read_line(&mut line)?;
    let mut v: serde_json::Value = serde_json::from_str(&line)?;
    if v["ok"].as_bool() == Some(true) {
        Ok(v["result"].take())
    } else {
        anyhow::bail!("{}", v["error"].as_str().unwrap_or("ошибка службы"))
    }
}

/// Консоль — всегда; для службы/консольного режима — ещё и ежедневный файл в <data>\logs.
fn init_logging(data_dir: &std::path::Path, to_file: bool) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;
    let filter = || tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let console = tracing_subscriber::fmt::layer().with_filter(filter());
    if to_file {
        let appender = tracing_appender::rolling::daily(data_dir.join("logs"), "steady.log");
        let (nb, guard) = tracing_appender::non_blocking(appender);
        let file = tracing_subscriber::fmt::layer().with_ansi(false).with_writer(nb).with_filter(filter());
        tracing_subscriber::registry().with(console).with(file).init();
        Some(guard)
    } else {
        tracing_subscriber::registry().with(console).init();
        None
    }
}

pub fn default_data_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\ProgramData"))
        .join("SteadyBackup")
}
