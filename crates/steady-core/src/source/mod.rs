pub mod files;
pub mod firebird;
pub mod hyperv;
pub mod mssql;
pub mod mysql;
pub mod onec;
pub mod onec_update;
pub mod postgres;
pub mod sshfiles;
pub mod vss;

use crate::job::{BackupKind, Source};
use crate::secret::reveal_opt;
use crate::state::JobState;
use anyhow::{Context, Result};
use std::path::Path;

/// Результат подготовки источника в staging-каталоге.
pub struct Prepared {
    pub kind: BackupKind,
    /// Новое состояние задания, записывается только после успешной отправки.
    pub new_state: Option<JobState>,
    pub items: u64,
    pub bytes: u64,
    /// Некритичные проблемы (копия создана, но требует внимания).
    pub warnings: Vec<String>,
    /// Данные уже сжаты самим источником (MS SQL COMPRESSION): 7-Zip пишет без сжатия,
    /// иначе тратится время и не выигрывается место.
    pub packed: bool,
    /// Части для отдельных архивов: (метка, файлы в staging). Пусто — один архив на весь staging.
    /// Заполняют источники с несколькими базами; используется при `Source::separate_archives`.
    pub parts: Vec<(String, Vec<std::path::PathBuf>)>,
    /// Базы, которые не удалось сохранить (остальные сохранены). Непусто — запуск завершится
    /// ошибкой после отправки сохранённого.
    pub failures: Vec<String>,
}

/// Безопасные для имён файлов и архивов метки, уникальные без учёта регистра.
/// Совпадения получают суффикс `_2`, `_3`; `reserved` заняты заранее.
pub(crate) fn unique_names(items: &[String], reserved: &[&str]) -> Vec<String> {
    let mut used: std::collections::HashSet<String> = reserved.iter().map(|s| s.to_lowercase()).collect();
    items
        .iter()
        .map(|db| {
            let base = crate::pipeline::safe_name(db);
            let mut name = base.clone();
            let mut i = 2;
            while !used.insert(name.to_lowercase()) {
                name = format!("{base}_{i}");
                i += 1;
            }
            name
        })
        .collect()
}

/// `timeout` — предельное время каждого внешнего процесса источника (1С, sqlcmd, PowerShell)
/// и ожидания создания VSS-снимка.
/// `attempts` — попыток каждой базы у источников с `Source::retries_per_item`.
pub fn prepare(
    source: &Source,
    state: &JobState,
    staging: &Path,
    timeout: std::time::Duration,
    attempts: u32,
) -> Result<Prepared> {
    match source {
        Source::Files { paths, user, password, include, exclude, date_filter, vss, kind, full_every, .. } => files::prepare(
            &files::FilesParams {
                paths,
                user: user.as_deref(),
                password: reveal_opt(password.as_deref())?.as_deref(),
                include,
                exclude,
                date_filter,
                requested: *kind,
                full_every: *full_every,
                vss: *vss,
                timeout,
            },
            state,
            staging,
        ),
        Source::OneC { ib_path, platform_exe, user, password, mode, unlock_code, .. } => onec::prepare(
            &onec::OneCParams {
                ib_path,
                platform_exe: platform_exe.as_deref(),
                user: user.as_deref(),
                password: reveal_opt(password.as_deref())?.as_deref(),
                mode: *mode,
                unlock_code: unlock_code.as_deref(),
                timeout,
            },
            staging,
        ),
        Source::MsSql { server, database, databases, user, password, kind, backup_dir, verify, .. } => mssql::prepare(
            &mssql::MsSqlParams {
                server,
                // Прежние задания хранят одну базу в `database`.
                databases: &if databases.is_empty() && !database.trim().is_empty() {
                    vec![database.clone()]
                } else {
                    databases.clone()
                },
                attempts,
                user: user.as_deref(),
                password: reveal_opt(password.as_deref())?.as_deref(),
                kind: *kind,
                backup_dir,
                verify: *verify,
                timeout,
                force_full: state.force_full,
            },
            staging,
        ),
        Source::Firebird {
            gbak,
            user,
            password,
            databases,
            names_from,
            path_template,
            host,
            verify,
            ..
        } => firebird::prepare(
            &firebird::FirebirdParams {
                gbak,
                user: user.as_deref(),
                password: reveal_opt(password.as_deref())?.as_deref(),
                databases,
                names_from: names_from.as_deref(),
                path_template: path_template.as_deref(),
                host: host.as_deref(),
                verify: *verify,
                timeout,
            },
            staging,
        ),
        Source::PostgreSql { host, port, user, password, databases, pg_dump, globals, verify, .. } => {
            postgres::prepare(
                &postgres::PgParams {
                    host: host.as_deref(),
                    port: *port,
                    user: user.as_deref(),
                    password: reveal_opt(password.as_deref())?.as_deref(),
                    databases,
                    pg_dump: pg_dump.as_deref(),
                    globals: *globals,
                    verify: *verify,
                    timeout,
                    attempts,
                },
                staging,
            )
        }
        Source::MySql { ssh, db_host, db_port, db_user, db_password, databases, .. } => mysql::prepare(
            &mysql::MySqlParams {
                ssh: ssh.as_ref().context("MySQL: не задано подключение SSH")?,
                db_host: db_host.as_deref(),
                db_port: *db_port,
                db_user,
                db_password: reveal_opt(db_password.as_deref())?.as_deref(),
                databases,
                timeout,
                attempts,
            },
            staging,
        ),
        Source::SshFiles { ssh, paths, include, exclude, date_filter, kind, full_every, .. } => sshfiles::prepare(
            &sshfiles::SshFilesParams {
                ssh: ssh.as_ref().context("не задано подключение SSH")?,
                paths,
                include,
                exclude,
                date_filter,
                requested: *kind,
                full_every: *full_every,
            },
            state,
            staging,
        ),
        Source::OneCUpdate { .. } => Err(anyhow::anyhow!(
            "обновление 1С выполняется отдельной веткой конвейера, без staging"
        )),
        Source::HyperV { vm_name, kind, full_every, consistency } => hyperv::prepare(
            &hyperv::HyperVParams { vm_name, kind: *kind, full_every: *full_every, consistency: *consistency, timeout },
            state,
            staging,
        ),
    }
}

pub(crate) fn dir_size(p: &Path) -> (u64, u64) {
    let mut n = 0;
    let mut b = 0;
    for e in walkdir::WalkDir::new(p).into_iter().flatten() {
        if e.file_type().is_file() {
            n += 1;
            b += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    (n, b)
}
