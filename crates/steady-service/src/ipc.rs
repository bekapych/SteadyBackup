//! IPC: именованный канал \\.\pipe\SteadyBackup, построчный JSON.
//!
//! Запрос:  {"id": 1, "cmd": "list_jobs", ...параметры}
//! Ответ:   {"id": 1, "ok": true, "result": ...} | {"id": 1, "ok": false, "error": "..."}
//!
//! `id` в запросе — номер вызова (целое). Если команде нужен свой строковый `id`
//! (хранилище, подключение), номер передаётся в `req_id`, иначе число затирает параметр.
//!
//! Подключиться может любой вошедший пользователь; команды, меняющие настройки или
//! раскрывающие задания, доступны только процессам с правами администратора (с повышением UAC)
//! и SYSTEM. Без повышения доступны только команды из списка `read_only`.

use crate::engine::Engine;
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use steady_core::destination::gdrive;
use steady_core::resources::{Connection, Resources, Storage};
use steady_core::Job;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use uuid::Uuid;

pub const PIPE_NAME: &str = r"\\.\pipe\SteadyBackup";

/// Имя канала. `STEADY_PIPE` позволяет запустить вторую копию (`steady console`) для проверок,
/// не мешая установленной службе.
pub fn pipe_name() -> String {
    match std::env::var("STEADY_PIPE") {
        Ok(n) if !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') => {
            format!(r"\\.\pipe\{n}")
        }
        _ => PIPE_NAME.to_string(),
    }
}
/// SYSTEM, Администраторы — полный доступ; прошедшие проверку — чтение/запись в канал.
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)";

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Request {
    Status,
    ListJobs,
    GetJob { job_id: Uuid },
    /// Задание в Box: Source с настройками загрузки обновлений 1С заметно увеличил вариант,
    /// а запросов этого вида за сеанс единицы.
    SaveJob { job: Box<Job> },
    DeleteJob { job_id: Uuid },
    RunJob { job_id: Uuid },
    /// Снять выполняющееся задание или убрать его из очереди.
    CancelJob { job_id: Uuid },
    History {
        #[serde(default = "default_limit")]
        limit: usize,
    },
    Protect { secret: String },
    TestDestination { destination: steady_core::Destination },
    /// Список учётных записей Google Drive, авторизованных на этом компьютере.
    GdriveAccounts,
    /// Путь к найденной платформе 1С.
    Find1c,
    GetNotify,
    GetSettings,
    /// Пустой work_dir — каталог по умолчанию `<data>\work`.
    SaveSettings {
        #[serde(default)]
        work_dir: Option<String>,
    },
    SaveNotify { settings: steady_core::notify::NotifySettings },
    /// Тестовое сообщение: channel = "email" | "telegram". Настройки берутся из запроса (ещё не сохранённые).
    TestNotify { channel: String, settings: steady_core::notify::NotifySettings },
    /// Чаты, недавно писавшие боту (для выбора chat_id).
    TelegramChats { bot_token: String },
    /// Виртуальные машины Hyper-V на этом компьютере.
    ListVms,
    /// Базы сервера MS SQL или PostgreSQL для источника из формы задания (подключение или
    /// встроенные поля). Заодно проверяет подключение. Только администраторам: использует учётные данные.
    ListDatabases { source: steady_core::Source },
    /// Источник «Файлы по SSH» из формы: вход на сервер и наличие каждого пути.
    /// Возвращает отпечаток сервера, если он не закреплён. Только администраторам.
    CheckSshPaths { source: steady_core::Source },
    /// Конфигурации 1С, доступные учётной записи ИТС, — для выбора в форме задания.
    /// Только администраторам: запрос несёт пароль ИТС. Пароль в ответ не возвращается.
    ListItsConfigurations {
        username: String,
        password: String,
        #[serde(default)]
        proxy: Option<steady_core::DownloadProxy>,
        #[serde(default)]
        insecure_tls: bool,
    },

    // ---- хранилища и подключения ----
    ListStorages,
    SaveStorage { storage: Storage },
    DeleteStorage { id: String },
    /// Проверка: подключение, число объектов, место. Либо сохранённое (id), либо из формы (storage).
    TestStorage {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        storage: Option<Storage>,
        #[serde(default)]
        folder: Option<String>,
    },
    ListConnections,
    SaveConnection { connection: Connection },
    DeleteConnection { id: String },
    /// Перевести назначения всех заданий на хранилища.
    MigrateJobs,
    /// Вход в Google: служба слушает 127.0.0.1 и возвращает URL; GUI открывает его в браузере.
    /// Без client_id — встроенный OAuth-клиент SteadyBackup.
    GdriveAuthStart {
        #[serde(default)]
        client_id: Option<String>,
    },
    /// Ожидание завершения входа; при успехе возвращает refresh-токен (DPAPI), email и квоту.
    GdriveAuthFinish {
        flow: String,
        /// Только для своего OAuth-клиента: открытый текст или "dpapi:stored" (взять из хранилища storage_id).
        #[serde(default)]
        client_secret: Option<String>,
        #[serde(default)]
        storage_id: Option<String>,
    },
    /// Импорт учётной записи Google (refresh-токен, полученный на другом компьютере).
    GdriveImport {
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        client_secret: Option<String>,
        refresh_token: String,
    },
    // ---- группы заданий ----
    ListGroups,
    /// Нулевой id — новая группа. `jobs` — состав и порядок: эти задания переводятся в группу,
    /// остальные её задания из неё выводятся.
    SaveGroup {
        group: steady_core::group::Group,
        jobs: Vec<Uuid>,
    },
    /// Группа удаляется, задания остаются (без группы).
    DeleteGroup { group_id: Uuid },
    /// Поставить включённые задания группы в очередь по порядку.
    RunGroup { group_id: Uuid },
    /// Копия задания: новый id, имя «(копия)», те же источник, назначения и расписание.
    CopyJob { job_id: Uuid },
    /// Состояние обновления: текущая и последняя версии, настройки, итог прошлой установки.
    UpdateStatus,
    /// Проверить выпуски на GitHub сейчас.
    CheckUpdate,
    /// Скачать и установить новую версию (в фоне; служба при этом перезапускается).
    InstallUpdate,
    /// Настройки обновления.
    SaveUpdateSettings { settings: steady_core::update::UpdateSettings },
    /// Все задания и настройки. Пароли в ответе открытым текстом — только для файла переноса.
    ExportSettings,
    /// `text` — содержимое файла, созданного export_settings.
    ImportSettings {
        text: String,
        #[serde(default)]
        replace: bool,
    },
}

fn default_limit() -> usize {
    200
}

/// `allow_user` — дополнительно разрешить доступ текущему пользователю (консольный режим отладки).
pub async fn serve(engine: Arc<Engine>, mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
    let sddl = PIPE_SDDL.to_string();
    let mut server = create(true, &sddl)?;
    tracing::info!("IPC: {}", pipe_name());
    loop {
        tokio::select! {
            r = server.connect() => r?,
            _ = shutdown.changed() => return Ok(()),
        }
        let client = std::mem::replace(&mut server, create(false, &sddl)?);
        let e = engine.clone();
        tokio::spawn(async move {
            if let Err(err) = handle(e, client).await {
                tracing::debug!("IPC клиент: {err:#}");
            }
        });
    }
}

fn create(first: bool, sddl: &str) -> Result<NamedPipeServer> {
    let mut opts = ServerOptions::new();
    opts.first_pipe_instance(first).reject_remote_clients(true);
    unsafe { create_secure(&opts, sddl) }
}

/// Консольный режим (`steady console`): процесс работает от имени пользователя, а не SYSTEM,
/// и прав сверх своих выдать не может. Клиент того же пользователя считается администратором —
/// иначе в неповышенной консоли GUI открывается только для просмотра. Служба этот флаг не ставит.
static TRUST_CONSOLE_USER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn trust_console_user() {
    TRUST_CONSOLE_USER.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// SID пользователя из токена. `None` — токен не прочитан.
unsafe fn token_user_sid(token: windows_sys::Win32::Foundation::HANDLE) -> Option<Vec<u8>> {
    use windows_sys::Win32::Security::*;
    let mut buf = vec![0u8; 256];
    let mut len = 0u32;
    if GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as _, buf.len() as u32, &mut len) == 0 {
        return None;
    }
    let user = &*(buf.as_ptr() as *const TOKEN_USER);
    let n = GetLengthSid(user.User.Sid);
    Some(std::slice::from_raw_parts(user.User.Sid as *const u8, n as usize).to_vec())
}

/// SID пользователя текущего процесса.
fn own_user_sid() -> Option<Vec<u8>> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::TOKEN_QUERY;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let sid = token_user_sid(token);
        CloseHandle(token);
        sid
    }
}

/// Клиент канала работает с правами администратора: повышенный (UAC) процесс администратора
/// или SYSTEM. Неповышенный процесс того же пользователя администратором не считается —
/// иначе любая программа в сеансе администратора могла бы через службу выполнять код от SYSTEM
/// в обход подтверждения UAC. Исключение — консольный режим (`trust_console_user`).
///
/// Токен берётся олицетворением клиента канала (ImpersonateNamedPipeClient), а не по PID:
/// PID мог быть переиспользован другим процессом. CheckTokenMembership учитывает только
/// включённые группы, поэтому в отфильтрованном UAC-токене (группа «Администраторы» там
/// помечена deny-only) проверка не проходит.
///
/// Вызывать до первого чтения из канала не нужно: олицетворение доступно после connect().
fn client_is_admin(pipe: &NamedPipeServer) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::*;
    use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
    use windows_sys::Win32::System::Threading::*;
    unsafe {
        // Олицетворение действует на текущий поток: снимаем его в этой же функции до любого await.
        if ImpersonateNamedPipeClient(pipe.as_raw_handle() as HANDLE) == 0 {
            return false;
        }
        let mut token: HANDLE = std::ptr::null_mut();
        // OpenAsSelf = TRUE: доступ к токену проверяется от имени службы, а не клиента.
        let opened = OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) != 0;
        RevertToSelf();
        if !opened {
            return false;
        }
        if TRUST_CONSOLE_USER.load(std::sync::atomic::Ordering::SeqCst) {
            let same = token_user_sid(token).is_some_and(|s| Some(s) == own_user_sid());
            if same {
                CloseHandle(token);
                return true;
            }
        }
        let mut sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut result = false;
        for kind in [WinBuiltinAdministratorsSid, WinLocalSystemSid] {
            let mut size = sid.len() as u32;
            if CreateWellKnownSid(kind, std::ptr::null_mut(), sid.as_mut_ptr() as _, &mut size) == 0 {
                continue;
            }
            let mut member = 0;
            if CheckTokenMembership(token, sid.as_mut_ptr() as _, &mut member) != 0 && member != 0 {
                result = true;
                break;
            }
        }
        CloseHandle(token);
        result
    }
}

unsafe fn create_secure(opts: &ServerOptions, sddl: &str) -> Result<NamedPipeServer> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::*;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    let sddl: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut sd = std::ptr::null_mut();
    if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) == 0 {
        bail!("SDDL: {}", std::io::Error::last_os_error());
    }
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let name = pipe_name();
    let r = opts.create_with_security_attributes_raw(&name, &mut sa as *mut _ as *mut _);
    LocalFree(sd as _);
    r.with_context(|| format!("создание канала {name} (служба уже запущена?)"))
}

async fn handle(engine: Arc<Engine>, pipe: NamedPipeServer) -> Result<()> {
    let admin = client_is_admin(&pipe);
    let (r, mut w) = tokio::io::split(pipe);
    let mut reader = BufReader::new(r);
    loop {
        let mut buf = Vec::new();
        let n = reader.read_until(b'\n', &mut buf).await?;
        if n == 0 {
            break;
        }
        // Экспорт настроек — один JSON со всеми заданиями и ключами SSH.
        if buf.len() > 8 * 1024 * 1024 {
            let reply = json!({ "ok": false, "error": "запрос слишком большой" });
            let mut s = serde_json::to_string(&reply)?;
            s.push('\n');
            w.write_all(s.as_bytes()).await?;
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut raw: Value = serde_json::from_str(line).unwrap_or(Value::Null);
        let id = take_correlation_id(&mut raw);
        let e = engine.clone();
        let res = tokio::task::spawn_blocking(move || dispatch(&e, raw, admin)).await?;
        let reply = match res {
            Ok(v) => json!({ "id": id, "ok": true, "result": v }),
            Err(err) => json!({ "id": id, "ok": false, "error": format!("{err:#}") }),
        };
        let mut s = serde_json::to_string(&reply)?;
        s.push('\n');
        w.write_all(s.as_bytes()).await?;
        w.flush().await?;
    }
    Ok(())
}

/// Номер запроса GUI — целое `id` или `req_id`. Строковый `id` остаётся параметром команды.
fn take_correlation_id(raw: &mut Value) -> Value {
    let Some(obj) = raw.as_object_mut() else {
        return Value::Null;
    };
    if let Some(req) = obj.remove("req_id") {
        if obj.get("id").is_some_and(|v| v.is_number()) {
            obj.remove("id");
        }
        return req;
    }
    if obj.get("id").is_some_and(|v| v.is_number()) {
        return obj.remove("id").unwrap_or(Value::Null);
    }
    Value::Null
}

fn dispatch(engine: &Arc<Engine>, raw: Value, admin: bool) -> Result<Value> {
    let req: Request = serde_json::from_value(raw).map_err(|e| anyhow!("неверный запрос: {e}"))?;
    let read_only = matches!(
        req,
        Request::Status
            | Request::ListJobs
            | Request::History { .. }
            | Request::Find1c
            | Request::ListVms
            | Request::ListStorages
            | Request::ListConnections
            | Request::GetSettings
            | Request::ListGroups
            | Request::UpdateStatus
    );
    if !read_only && !admin {
        bail!("доступ запрещён: команда доступна только администраторам");
    }
    Ok(match req {
        Request::Status => {
            let (running, queue) = engine.snapshot();
            json!({ "version": env!("CARGO_PKG_VERSION"), "running": running, "queue": queue, "admin": admin,
                    "update": engine.updater.brief() })
        }
        Request::ListJobs => {
            let jobs = engine.store.load_jobs()?;
            let (running, queue) = engine.snapshot();
            let items: Vec<Value> = jobs
                .iter()
                .map(|j| {
                    let last = engine.store.last(&j.id);
                    json!({
                        "id": j.id,
                        "name": j.name,
                        "enabled": j.enabled,
                        "source": source_kind(j),
                        "destinations": j.destinations.len(),
                        "schedule": j.schedule,
                        "next_run": engine.next_run(j),
                        "last_run": last.as_ref().map(|h| h.finished),
                        "last_ok": last.as_ref().map(|h| h.ok && h.warnings.is_empty()),
                        "last_status": last.as_ref().map(|h| if !h.ok { "error" } else if h.warnings.is_empty() { "ok" } else { "warning" }),
                        "last_error": last.and_then(|h| h.error.clone().or_else(|| h.warnings.first().cloned())),
                        "running": running.as_ref().is_some_and(|r| r.job_id == j.id),
                        "queued": queue.contains(&j.id),
                        "group": j.group,
                    })
                })
                .collect();
            json!(items)
        }
        Request::GetJob { job_id } => {
            let job = engine.store.load_jobs()?.into_iter().find(|j| j.id == job_id).context("задание не найдено")?;
            serde_json::to_value(job)?
        }
        Request::SaveJob { job } => {
            let mut job = *job;
            if job.name.trim().is_empty() {
                bail!("не задано имя задания");
            }
            if job.id.is_nil() {
                job.id = Uuid::new_v4();
            }
            let existing = engine.store.load_jobs()?;
            let safe = steady_core::pipeline::safe_name(&job.name);
            if let Some(other) = existing.iter().find(|j| j.id != job.id && steady_core::pipeline::safe_name(&j.name) == safe) {
                bail!("имя «{}» после очистки совпадает с заданием «{}» — в одном назначении они сотрут копиям друг друга", job.name, other.name);
            }
            if let Some(gid) = job.group {
                if !engine.store.load_groups()?.iter().any(|g| g.id == gid) {
                    bail!("группа задания не найдена (удалена?) — выберите другую");
                }
            }
            let previous = existing.into_iter().find(|j| j.id == job.id);
            job.protect_secrets()?;
            engine.store.save_job(&job)?;
            engine.on_job_saved(previous.as_ref(), &job, chrono::Local::now().naive_local());
            engine.wake();
            json!({ "id": job.id })
        }
        Request::DeleteJob { job_id } => {
            engine.store.delete_job(&job_id)?;
            Value::Null
        }
        Request::RunJob { job_id } => json!({ "queued": engine.enqueue(job_id) }),
        Request::CancelJob { job_id } => {
            engine.cancel_job(job_id).map_err(|e| anyhow!(e))?;
            json!(null)
        }
        Request::History { limit } => serde_json::to_value(engine.store.history(limit)?)?,
        Request::Protect { secret } => json!(steady_core::secret::protect_str(&secret)?),
        Request::TestDestination { destination } => {
            let mut t = steady_core::destination::open(&destination, &engine.data_dir)?;
            let n = t.list()?.len();
            json!({ "target": t.describe(), "objects": n })
        }
        Request::GdriveAccounts => {
            let mut v = Vec::new();
            if let Ok(rd) = std::fs::read_dir(engine.data_dir.join("gdrive")) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.extension().and_then(|x| x.to_str()) == Some("json") {
                        if let Some(s) = p.file_stem().and_then(|s| s.to_str()) {
                            v.push(s.to_string());
                        }
                    }
                }
            }
            json!(v)
        }
        Request::Find1c => json!(steady_core::source::onec::find_platform().map(|p| p.display().to_string())),

        Request::ListStorages => {
            let res = Resources::new(&engine.data_dir);
            let jobs = engine.store.load_jobs()?;
            let items: Vec<Value> = res
                .storages_masked()?
                .into_iter()
                .map(|s| {
                    let used: Vec<&str> = jobs
                        .iter()
                        .filter(|j| {
                            j.destinations.iter().any(|d| {
                                matches!(d, steady_core::Destination::Storage { storage, .. } if *storage == s.id)
                            })
                        })
                        .map(|j| j.name.as_str())
                        .collect();
                    let mut v = serde_json::to_value(&s).unwrap_or_default();
                    v["used_by"] = json!(used);
                    v
                })
                .collect();
            json!(items)
        }
        Request::SaveStorage { storage } => {
            let s = Resources::new(&engine.data_dir).save_storage(storage)?;
            json!({ "id": s.id })
        }
        Request::DeleteStorage { id } => {
            Resources::new(&engine.data_dir).delete_storage(&id, &engine.store.load_jobs()?)?;
            Value::Null
        }
        Request::TestStorage { id, storage, folder } => {
            let res = Resources::new(&engine.data_dir);
            let target = match (id, storage) {
                (_, Some(mut s)) => {
                    // Несохранённая форма: секреты "dpapi:stored" берём из сохранённой версии.
                    if let Ok(old) = res.storage(&s.id) {
                        steady_core::resources::keep_stored_destination(&mut s.target, &old.target);
                    }
                    s.target.protect_secrets()?;
                    s.target
                }
                (Some(id), None) => res.storage(&id)?.target,
                (None, None) => bail!("не указано хранилище"),
            };
            let target = steady_core::resources::with_subfolder(target, folder.as_deref().unwrap_or(""));
            let mut t = steady_core::destination::open(&target, &engine.data_dir)?;
            let n = t.list()?.len();
            let space = t.space();
            json!({
                "target": t.describe(),
                "objects": n,
                "total": space.map(|s| s.0),
                "free": space.map(|s| s.1),
                "warnings": t.warnings(),
                "host_key": t.host_key(),
            })
        }
        Request::ListConnections => {
            let jobs = engine.store.load_jobs()?;
            let items: Vec<Value> = Resources::new(&engine.data_dir)
                .connections_masked()?
                .into_iter()
                .map(|c| {
                    let used: Vec<&str> = jobs
                        .iter()
                        .filter(|j| steady_core::resources::source_connection(&j.source) == Some(c.id.as_str()))
                        .map(|j| j.name.as_str())
                        .collect();
                    let mut v = serde_json::to_value(&c).unwrap_or_default();
                    v["used_by"] = json!(used);
                    v
                })
                .collect();
            json!(items)
        }
        Request::SaveConnection { connection } => {
            let c = Resources::new(&engine.data_dir).save_connection(connection)?;
            json!({ "id": c.id })
        }
        Request::DeleteConnection { id } => {
            Resources::new(&engine.data_dir).delete_connection(&id, &engine.store.load_jobs()?)?;
            Value::Null
        }
        Request::MigrateJobs => {
            let mut jobs = engine.store.load_jobs()?;
            let (created, changed) = Resources::new(&engine.data_dir).migrate_jobs(&mut jobs)?;
            for j in &jobs {
                if changed.contains(&j.name) {
                    engine.store.save_job(j)?;
                }
            }
            json!({ "storages_created": created, "jobs_changed": changed })
        }
        Request::GdriveAuthStart { client_id } => {
            let custom = client_id.filter(|s| !s.trim().is_empty());
            let id = custom.clone().unwrap_or_else(|| gdrive::BUILTIN_CLIENT_ID.to_string());
            let flow = gdrive::AuthFlow::start(&id)?;
            let url = flow.url.clone();
            let key = Uuid::new_v4().to_string();
            {
                let mut flows = GDRIVE_FLOWS.lock().unwrap();
                purge_flows(&mut flows);
                flows.insert(key.clone(), (custom, flow, std::time::Instant::now()));
            }
            json!({ "flow": key, "url": url })
        }
        Request::GdriveAuthFinish { flow, client_secret, storage_id } => {
            let (custom, f, started) = {
                let mut flows = GDRIVE_FLOWS.lock().unwrap();
                purge_flows(&mut flows);
                flows.remove(&flow).context("сеанс входа не найден или уже завершён")?
            };
            if started.elapsed() >= FLOW_TTL {
                bail!("сеанс входа истёк, начните снова");
            }
            let (id, secret) = match &custom {
                None => (gdrive::BUILTIN_CLIENT_ID.to_string(), gdrive::BUILTIN_CLIENT_SECRET.to_string()),
                Some(id) => {
                    let s = client_secret.filter(|s| !s.is_empty()).context("не указан Client Secret")?;
                    let s = if s == steady_core::resources::STORED {
                        let st = Resources::new(&engine.data_dir).storage(storage_id.as_deref().unwrap_or(""))?;
                        match st.target {
                            steady_core::Destination::GoogleDrive { client_secret: Some(cs), .. } => {
                                steady_core::secret::reveal(&cs)?
                            }
                            _ => bail!("сохранённый Client Secret не найден, введите его заново"),
                        }
                    } else {
                        s
                    };
                    (id.clone(), s)
                }
            };
            let acc = f.finish(&id, &secret, std::time::Duration::from_secs(300))?;
            gdrive_account_json(acc, custom.is_some())?
        }
        Request::GdriveImport { client_id, client_secret, refresh_token } => {
            let custom = client_id.filter(|s| !s.trim().is_empty());
            let acc = gdrive::Account {
                client_id: custom.clone().unwrap_or_else(|| gdrive::BUILTIN_CLIENT_ID.to_string()),
                client_secret: match &custom {
                    Some(_) => steady_core::secret::protect_str(client_secret.as_deref().unwrap_or(""))?,
                    None => gdrive::BUILTIN_CLIENT_SECRET.to_string(),
                },
                refresh_token: steady_core::secret::protect_str(&refresh_token)?,
                email: None,
            };
            gdrive_account_json(acc, custom.is_some())?
        }
        Request::GetSettings => {
            let s = steady_core::settings::ServiceSettings::load(&engine.data_dir);
            let effective = steady_core::settings::work_dir(&engine.data_dir).unwrap_or_else(|_| engine.data_dir.join("work"));
            json!({ "work_dir": s.work_dir, "effective": effective })
        }
        Request::UpdateStatus => engine.updater.status(),
        Request::CheckUpdate => {
            engine.updater.check_now()?;
            engine.updater.status()
        }
        Request::InstallUpdate => {
            let v = crate::updater::Updater::start_install(engine)?;
            json!({ "version": v })
        }
        Request::SaveUpdateSettings { settings } => {
            let mut s = steady_core::settings::ServiceSettings::load(&engine.data_dir);
            s.update = settings;
            s.save(&engine.data_dir)?;
            engine.updater.status()
        }
        Request::SaveSettings { work_dir } => {
            // Остальные настройки (обновление) сохраняются как были.
            let s = steady_core::settings::ServiceSettings {
                work_dir: work_dir.filter(|p| !p.trim().is_empty()),
                ..steady_core::settings::ServiceSettings::load(&engine.data_dir)
            };
            let path = steady_core::settings::resolve_work_dir(&engine.data_dir, &s)?;
            // Свой каталог вне data\ — те же права, что у каталога данных. По умолчанию он уже под ним.
            if s.work_dir.is_some() {
                steady_core::acl::secure_data_dir(&path)?;
            }
            s.save(&engine.data_dir)?;
            json!({ "effective": path })
        }
        Request::GetNotify => {
            // Секреты наружу не отдаём: GUI видит только признак «сохранён».
            let mut s = steady_core::notify::NotifySettings::load(&engine.data_dir);
            if let Some(e) = s.email.as_mut() {
                e.password = e.password.as_ref().filter(|p| !p.is_empty()).map(|_| "dpapi:stored".into());
            }
            if let Some(t) = s.telegram.as_mut() {
                if !t.bot_token.is_empty() {
                    t.bot_token = "dpapi:stored".into();
                }
            }
            serde_json::to_value(s)?
        }
        Request::SaveNotify { settings } => {
            let settings = merge_stored(settings, &engine.data_dir);
            settings.save(&engine.data_dir)?;
            Value::Null
        }
        Request::TestNotify { channel, settings } => {
            let settings = merge_stored(settings, &engine.data_dir);
            steady_core::notify::send_test(&settings, &channel)?;
            Value::Null
        }
        Request::TelegramChats { bot_token } => {
            let token = if bot_token == "dpapi:stored" {
                steady_core::notify::NotifySettings::load(&engine.data_dir)
                    .telegram
                    .map(|t| t.bot_token)
                    .unwrap_or_default()
            } else {
                bot_token
            };
            let chats = steady_core::notify::telegram_recent_chats(&token)?;
            json!(chats.into_iter().map(|(id, title)| json!({ "id": id, "title": title })).collect::<Vec<_>>())
        }
        Request::ListGroups => {
            let jobs = engine.store.load_jobs()?;
            let items: Vec<Value> = engine
                .store
                .load_groups()?
                .iter()
                .map(|g| {
                    let members: Vec<Value> = steady_core::group::members(g, &jobs)
                        .into_iter()
                        .map(|j| json!({ "id": j.id, "name": j.name, "enabled": j.enabled }))
                        .collect();
                    json!({
                        "id": g.id,
                        "name": g.name,
                        "enabled": g.enabled,
                        "schedule": g.schedule,
                        "run_missed": g.run_missed,
                        "restarts": g.restarts,
                        "restart_delay_minutes": g.restart_delay_minutes,
                        "attempts": g.attempts,
                        "next_run": engine.next_group_run(g),
                        "jobs": members,
                    })
                })
                .collect();
            json!(items)
        }
        Request::SaveGroup { mut group, jobs: members } => {
            if group.name.trim().is_empty() {
                bail!("не задано имя группы");
            }
            if group.id.is_nil() {
                group.id = Uuid::new_v4();
            }
            let groups = engine.store.load_groups()?;
            if groups.iter().any(|g| g.id != group.id && g.name.trim().eq_ignore_ascii_case(group.name.trim())) {
                bail!("группа «{}» уже есть", group.name.trim());
            }
            let all = engine.store.load_jobs()?;
            if let Some(id) = members.iter().find(|id| !all.iter().any(|j| j.id == **id)) {
                bail!("задание {id} не найдено");
            }
            group.order = members.clone();
            let previous = groups.into_iter().find(|g| g.id == group.id);
            engine.store.save_group(&group)?;
            // Состав группы хранится в заданиях: одно задание — одна группа.
            for mut j in all {
                let want = members.contains(&j.id);
                let changed = if want && j.group != Some(group.id) {
                    j.group = Some(group.id);
                    true
                } else if !want && j.group == Some(group.id) {
                    j.group = None;
                    true
                } else {
                    false
                };
                if changed {
                    engine.store.save_job(&j)?;
                }
            }
            engine.on_group_saved(previous.as_ref(), &group, chrono::Local::now().naive_local());
            engine.wake();
            json!({ "id": group.id })
        }
        Request::DeleteGroup { group_id } => {
            engine.store.delete_group(&group_id)?;
            Value::Null
        }
        Request::RunGroup { group_id } => {
            let g = engine.store.load_groups()?.into_iter().find(|g| g.id == group_id).context("группа не найдена")?;
            let n = engine.enqueue_group(&g, &engine.store.load_jobs()?);
            json!({ "queued": n })
        }
        Request::CopyJob { job_id } => {
            let existing = engine.store.load_jobs()?;
            let job = existing.iter().find(|j| j.id == job_id).context("задание не найдено")?;
            let mut copy = job.clone();
            copy.id = Uuid::new_v4();
            copy.name = steady_core::bundle::copy_name(&job.name, |n| {
                existing.iter().any(|j| steady_core::pipeline::safe_name(&j.name) == steady_core::pipeline::safe_name(n))
            });
            copy.protect_secrets()?;
            engine.store.save_job(&copy)?;
            engine.on_job_saved(None, &copy, chrono::Local::now().naive_local());
            engine.wake();
            json!({ "id": copy.id, "name": copy.name })
        }
        Request::ExportSettings => {
            let (bundle, warnings) = steady_core::bundle::export_dir(&engine.data_dir)?;
            // В журнал — только числа. Текст файла содержит пароли.
            tracing::info!(
                "экспорт настроек: заданий {}, хранилищ {}, подключений {}",
                bundle.jobs.len(),
                bundle.storages.len(),
                bundle.connections.len()
            );
            json!({
                "text": serde_json::to_string_pretty(&bundle)?,
                "warnings": warnings,
                "jobs": bundle.jobs.len(),
                "storages": bundle.storages.len(),
                "connections": bundle.connections.len(),
            })
        }
        Request::ImportSettings { text, replace } => {
            let bundle = steady_core::bundle::parse(&text)?;
            let mut report = steady_core::bundle::import_dir(&engine.data_dir, &bundle, replace)?;
            if bundle.settings.as_ref().is_some_and(|s| s.work_dir.is_some()) {
                if let Ok(path) = steady_core::settings::work_dir(&engine.data_dir) {
                    if path != engine.data_dir.join("work") {
                        if let Err(e) = steady_core::acl::secure_data_dir(&path) {
                            report.warnings.push(format!("права на рабочий каталог не выставлены: {e:#}"));
                        }
                    }
                }
            }
            tracing::info!(
                "импорт настроек: заданий {}, хранилищ {}, подключений {}, удалено заданий {}",
                report.jobs,
                report.storages,
                report.connections,
                report.deleted_jobs
            );
            engine.wake();
            serde_json::to_value(&report)?
        }
        Request::ListDatabases { source } => {
            // Запрос из GUI: ждать дольше минуты нет смысла.
            const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
            let source = Resources::new(&engine.data_dir).resolve_source(&source)?;
            let names = match &source {
                steady_core::Source::MsSql { server, user, password, .. } => {
                    let pw = reveal_form_secret(password.as_deref())?;
                    steady_core::source::mssql::list_databases(server, user.as_deref(), pw.as_deref(), TIMEOUT)?
                }
                steady_core::Source::PostgreSql { host, port, user, password, pg_dump, .. } => {
                    let pw = reveal_form_secret(password.as_deref())?;
                    steady_core::source::postgres::list_databases(
                        host.as_deref(),
                        *port,
                        user.as_deref(),
                        pw.as_deref(),
                        pg_dump.as_deref(),
                        TIMEOUT,
                    )?
                }
                steady_core::Source::MySql { ssh, db_host, db_port, db_user, db_password, .. } => {
                    let pw = reveal_form_secret(db_password.as_deref())?;
                    let login = ssh.as_ref().context("выберите подключение SSH")?;
                    check_form_ssh(login)?;
                    steady_core::source::mysql::list_databases(&steady_core::source::mysql::MySqlParams {
                        ssh: login,
                        db_host: db_host.as_deref(),
                        db_port: *db_port,
                        db_user,
                        db_password: pw.as_deref(),
                        databases: &[],
                        timeout: TIMEOUT,
                        attempts: 1,
                    })?
                }
                _ => bail!("список баз есть только у источников MS SQL, PostgreSQL и MySQL"),
            };
            json!(names)
        }
        Request::ListItsConfigurations { username, password, proxy, insecure_tls } => {
            // Запрос из GUI: вход на портал и разбор страницы — не дольше двух минут.
            const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
            // Пароль приходит из формы открытым текстом или в DPAPI — наружу не возвращается.
            list_its_configurations(username, password, proxy, insecure_tls, TIMEOUT)?
        }
        Request::CheckSshPaths { source } => {
            let source = Resources::new(&engine.data_dir).resolve_source(&source)?;
            let steady_core::Source::SshFiles { ssh: Some(login), paths, .. } = &source else {
                bail!("выберите подключение SSH");
            };
            check_form_ssh(login)?;
            let mut sess = steady_core::ssh::SshSession::open(login, false)?;
            let mut items = Vec::new();
            for p in paths.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                let r: Result<_> = sess.sftp().and_then(|(rt, sftp)| Ok(rt.block_on(sftp.metadata(p))?));
                items.push(match r {
                    Ok(md) => json!({ "path": p, "ok": true, "dir": md.is_dir() }),
                    Err(e) => json!({ "path": p, "ok": false, "error": format!("{e:#}") }),
                });
            }
            json!({ "target": sess.label(), "host_key": sess.unpinned_host_key, "paths": items })
        }
        Request::ListVms => {
            let mut cmd = std::process::Command::new(steady_core::util::powershell_exe());
            cmd.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Console]::OutputEncoding=[Text.Encoding]::UTF8; @(Get-VM | Select-Object Name, @{n='State';e={\"$($_.State)\"}}) | ConvertTo-Json -Compress",
            ]);
            // Запрос из GUI: ждать дольше минуты нет смысла.
            let out = steady_core::process::run(cmd, std::time::Duration::from_secs(60), "Hyper-V: список ВМ")?;
            let text = String::from_utf8_lossy(&out.stdout);
            if !out.status.success() {
                bail!("Hyper-V: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            let v: Value = serde_json::from_str(text.trim().trim_start_matches('\u{feff}')).unwrap_or(json!([]));
            if v.is_array() { v } else { json!([v]) }
        }
    })
}

/// Сеанс входа → (свой client_id или None для встроенного, ожидание ответа Google).
type Flows = std::collections::HashMap<String, (Option<String>, gdrive::AuthFlow, std::time::Instant)>;
const FLOW_TTL: std::time::Duration = std::time::Duration::from_secs(600);

fn purge_flows(flows: &mut Flows) {
    flows.retain(|_, (_, _, started)| started.elapsed() < FLOW_TTL);
}
static GDRIVE_FLOWS: std::sync::LazyLock<std::sync::Mutex<Flows>> = std::sync::LazyLock::new(Default::default);

/// Проверяет учётную запись запросом к Диску и возвращает поля для хранилища (секреты в DPAPI).
/// Для встроенного клиента client_id/client_secret не возвращаются — они не хранятся в хранилище.
fn gdrive_account_json(mut acc: gdrive::Account, custom: bool) -> Result<Value> {
    let info = gdrive::account_info(&acc).context("проверка доступа к Google Drive")?;
    if acc.email.is_none() {
        acc.email = info.email;
    }
    Ok(json!({
        "client_id": if custom { Some(&acc.client_id) } else { None },
        "client_secret": if custom { Some(&acc.client_secret) } else { None },
        "refresh_token": acc.refresh_token,
        "email": acc.email,
        "total": info.total,
        "used": info.used,
    }))
}

/// Конфигурации ИТС для формы задания. Сетевой вызов идёт в отдельном потоке: прервать его
/// извне нельзя, а GUI не должен ждать дольше `timeout`. Поток, не уложившийся в срок,
/// завершится сам по тайм-аутам ureq и ничего не запишет в ответ.
fn list_its_configurations(
    username: String,
    password: String,
    proxy: Option<steady_core::DownloadProxy>,
    insecure_tls: bool,
    timeout: std::time::Duration,
) -> Result<Value> {
    let password = match password.as_str() {
        // Маркер «сохранён» из GUI расшифровать не из чего: пароль ИТС вводится в форме.
        steady_core::resources::STORED => bail!("введите пароль учётной записи ИТС"),
        other => steady_core::secret::reveal(other)?,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = steady_core::source::onec_download::list_available(
            &username,
            &password,
            proxy.as_ref(),
            insecure_tls,
        );
        // Приёмник мог уйти по тайм-ауту: ошибку отправки игнорируем.
        let _ = tx.send(result);
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(list)) => Ok(json!(list)),
        Ok(Err(e)) => Err(e),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => bail!(
            "releases.1c.ru не ответил за {} с — проверьте прокси и доступ в интернет",
            timeout.as_secs()
        ),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            bail!("не удалось получить список конфигураций ИТС")
        }
    }
}

/// Пароль из формы: открытый текст или `dpapi:...`. Маркер GUI без сохранённого значения
/// расшифровать нельзя — понятная ошибка вместо ошибки base64.
fn reveal_form_secret(v: Option<&str>) -> Result<Option<String>> {
    if v == Some(steady_core::resources::STORED) {
        bail!("пароль хранится в задании; выберите подключение из списка «Подключения»");
    }
    steady_core::secret::reveal_opt(v)
}

/// SSH из формы задания: маркер GUI без сохранённого значения не расшифровать.
fn check_form_ssh(login: &steady_core::SshLogin) -> Result<()> {
    if [&login.password, &login.key_passphrase].iter().any(|v| v.as_deref() == Some(steady_core::resources::STORED)) {
        bail!("пароль SSH хранится в задании; выберите подключение из списка «Подключения»");
    }
    Ok(())
}

/// Подставляет сохранённые секреты вместо маркера "dpapi:stored" из GUI.
fn merge_stored(
    mut s: steady_core::notify::NotifySettings,
    data_dir: &std::path::Path,
) -> steady_core::notify::NotifySettings {
    let old = steady_core::notify::NotifySettings::load(data_dir);
    if let Some(e) = s.email.as_mut() {
        if e.password.as_deref() == Some("dpapi:stored") {
            e.password = old.email.as_ref().and_then(|o| o.password.clone());
        }
    }
    if let Some(t) = s.telegram.as_mut() {
        if t.bot_token == "dpapi:stored" {
            t.bot_token = old.telegram.as_ref().map(|o| o.bot_token.clone()).unwrap_or_default();
        }
    }
    s
}

fn source_kind(j: &Job) -> &'static str {
    match j.source {
        steady_core::Source::Files { .. } => "files",
        steady_core::Source::OneC { .. } => "one_c",
        steady_core::Source::OneCUpdate { .. } => "one_c_update",
        steady_core::Source::OneCDownload { .. } => "one_c_download",
        steady_core::Source::MsSql { .. } => "ms_sql",
        steady_core::Source::HyperV { .. } => "hyper_v",
        steady_core::Source::Firebird { .. } => "firebird",
        steady_core::Source::PostgreSql { .. } => "postgresql",
        steady_core::Source::MySql { .. } => "mysql",
        steady_core::Source::SshFiles { .. } => "ssh_files",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_request_id_is_not_a_storage_id() {
        let mut raw = json!({
            "cmd": "test_storage",
            "id": 356,
            "storage": {
                "id": "",
                "name": "Диск",
                "target": {"type": "google_drive", "folder": "SteadyBackup", "refresh_token": "dpapi:x"}
            }
        });
        assert_eq!(take_correlation_id(&mut raw), json!(356));
        let req: Request = serde_json::from_value(raw).unwrap();
        match req {
            Request::TestStorage { id, storage, .. } => {
                assert!(id.is_none());
                assert_eq!(storage.unwrap().name, "Диск");
            }
            _ => panic!("ожидалась проверка хранилища"),
        }
    }

    #[test]
    fn string_id_is_kept_when_correlation_is_separate() {
        let mut raw = json!({"cmd": "delete_storage", "id": "nas", "req_id": 7});
        assert_eq!(take_correlation_id(&mut raw), json!(7));
        let req: Request = serde_json::from_value(raw).unwrap();
        match req {
            Request::DeleteStorage { id } => assert_eq!(id, "nas"),
            _ => panic!("ожидалось удаление хранилища"),
        }
    }
}
