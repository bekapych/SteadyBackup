//! MS SQL Server: BACKUP DATABASE / BACKUP LOG через sqlcmd.
//! TODO: заменить sqlcmd на нативный клиент (tiberius), чтобы не зависеть от установленных утилит.

use super::{dir_size, Prepared};
use crate::job::{BackupKind, MsSqlKind};
use crate::state::JobState;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct MsSqlParams<'a> {
    pub server: &'a str,
    /// Пусто — все пользовательские базы сервера.
    pub databases: &'a [String],
    pub user: Option<&'a str>,
    pub password: Option<&'a str>,
    pub kind: MsSqlKind,
    pub backup_dir: &'a str,
    pub verify: bool,
    pub timeout: std::time::Duration,
    /// Прошлая копия дошла не до всех назначений — эту сделать полной, иначе в цепочке дыра.
    pub force_full: bool,
    /// Попыток каждой базы.
    pub attempts: u32,
}

/// Все пользовательские базы в сети. Журнал есть только у баз с моделью FULL / BULK_LOGGED.
fn auto_list_sql(kind: MsSqlKind) -> String {
    let mut sql = String::from(
        "SET NOCOUNT ON; SELECT name FROM sys.databases WHERE name NOT IN ('master','model','msdb','tempdb') \
         AND state_desc = 'ONLINE' AND source_database_id IS NULL",
    );
    if kind == MsSqlKind::Log {
        sql.push_str(" AND recovery_model_desc <> 'SIMPLE'");
    }
    sql.push_str(" ORDER BY name");
    sql
}

pub fn prepare(p: &MsSqlParams, staging: &Path) -> Result<Prepared> {
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let mut warnings = Vec::new();
    let sql_kind = if p.force_full && p.kind != crate::job::MsSqlKind::Full {
        warnings.push("предыдущая копия дошла не до всех назначений — выполняется полная".into());
        MsSqlKind::Full
    } else {
        p.kind
    };
    let kind = match sql_kind {
        MsSqlKind::Full => BackupKind::Full,
        MsSqlKind::Differential => BackupKind::Differential,
        MsSqlKind::Log => BackupKind::Incremental,
    };

    let mut databases: Vec<String> = p.databases.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if databases.is_empty() {
        databases = parse_names(&sqlcmd_output(p, &["-h", "-1", "-W", "-f", "65001"], &auto_list_sql(sql_kind))?);
        if databases.is_empty() {
            bail!("MSSQL: на сервере нет пользовательских баз для копирования");
        }
        tracing::info!("MSSQL: все пользовательские базы сервера: {}", databases.len());
    }
    std::fs::create_dir_all(staging)?;

    let labels = super::unique_names(&databases, &[]);
    // Express не умеет COMPRESSION: узнав это на первой базе, остальные сразу пишем без него.
    let mut compression = true;
    let mut parts = Vec::new();
    let mut failures = Vec::new();
    for (db, label) in databases.iter().zip(&labels) {
        crate::process::check_cancel()?;
        let res = crate::util::retry(
            &format!("MSSQL: база {db}"),
            p.attempts,
            |i| crate::util::sleep_cancellable(crate::util::backoff(i)),
            || backup_one(p, db, label, &stamp, sql_kind, &mut compression, staging),
        );
        match res {
            Ok(file) => parts.push((label.clone(), vec![file])),
            Err(e) => {
                crate::process::check_cancel()?;
                tracing::error!("MSSQL: база {db}: {e:#}");
                failures.push(format!("база {db}: {e:#}"));
            }
        }
    }
    if parts.is_empty() {
        bail!("MSSQL: не сохранена ни одна база: {}", failures.join("; "));
    }
    if !compression {
        warnings.push("SQL Server не поддерживает сжатие резервных копий (Express): копия не сжата средствами SQL Server".into());
    }

    let (items, bytes) = dir_size(staging);
    // Состояние нужно, чтобы запомнить force_full, если копия дойдёт не до всех назначений.
    let new_state = JobState { force_full: false, ..Default::default() };
    Ok(Prepared { kind, new_state: Some(new_state), items, bytes, warnings, packed: compression, parts, failures })
}

/// BACKUP одной базы в `backup_dir`, проверка и перенос в staging. Возвращает файл в staging.
fn backup_one(
    p: &MsSqlParams,
    database: &str,
    label: &str,
    stamp: &str,
    sql_kind: MsSqlKind,
    compression: &mut bool,
    staging: &Path,
) -> Result<PathBuf> {
    let ext = match sql_kind {
        MsSqlKind::Full => "bak",
        MsSqlKind::Differential => "dif",
        MsSqlKind::Log => "trn",
    };
    let file_name = format!("{label}_{stamp}.{ext}");
    let bak = PathBuf::from(p.backup_dir).join(&file_name);
    let db = quote_ident(database);
    let disk = quote_str(&bak.display().to_string());
    let sql = match sql_kind {
        MsSqlKind::Full => format!("BACKUP DATABASE {db} TO DISK = {disk} WITH INIT, CHECKSUM, COMPRESSION, STATS = 10"),
        MsSqlKind::Differential => {
            format!("BACKUP DATABASE {db} TO DISK = {disk} WITH DIFFERENTIAL, INIT, CHECKSUM, COMPRESSION, STATS = 10")
        }
        MsSqlKind::Log => format!("BACKUP LOG {db} TO DISK = {disk} WITH INIT, CHECKSUM, COMPRESSION, STATS = 10"),
    };
    let sql = if *compression { sql } else { sql.replace(", COMPRESSION", "") };
    tracing::info!("MSSQL: {sql}");
    let r = sqlcmd(p, &sql).or_else(|e| {
        // COMPRESSION недоступно в Express — повторяем без него. Тогда сжимает 7-Zip.
        if *compression && e.to_string().contains("COMPRESSION") {
            *compression = false;
            sqlcmd(p, &sql.replace(", COMPRESSION", ""))
        } else {
            Err(e)
        }
    });
    if let Err(e) = r {
        let _ = std::fs::remove_file(&bak);
        return Err(e);
    }

    if p.verify {
        if let Err(e) = sqlcmd(p, &format!("RESTORE VERIFYONLY FROM DISK = {disk} WITH CHECKSUM")) {
            let _ = std::fs::remove_file(&bak);
            return Err(e);
        }
    }

    let dst = staging.join(&file_name);
    if let Err(e) = std::fs::copy(&bak, &dst) {
        let _ = std::fs::remove_file(&bak);
        let _ = std::fs::remove_file(&dst);
        return Err(e).context(format!("служба не может прочитать {}", bak.display()));
    }
    std::fs::remove_file(&bak).ok();
    Ok(dst)
}

/// Полный путь к sqlcmd: каталоги Program Files, затем PATH. Поиск по PATH службы опасен
/// (в PATH может быть каталог, доступный пользователю на запись), поэтому найденный файл
/// проверяется `acl::check_trusted_exe`.
fn find_sqlcmd() -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        let Ok(pf) = std::env::var(var) else { continue };
        let base = Path::new(&pf).join("Microsoft SQL Server");
        // Client SDK\ODBC\<ver>\Tools\Binn и <ver>\Tools\Binn — новые версии первыми.
        for sub in [base.join("Client SDK").join("ODBC"), base.clone()] {
            let Ok(rd) = std::fs::read_dir(&sub) else { continue };
            let mut vers: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
            vers.sort();
            vers.reverse();
            for v in vers {
                candidates.push(v.join("Tools").join("Binn").join("SQLCMD.EXE"));
            }
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&path) {
            candidates.push(d.join("sqlcmd.exe"));
        }
    }
    let mut last_err = None;
    for c in candidates.into_iter().filter(|c| c.is_absolute() && c.is_file()) {
        match crate::acl::check_trusted_exe(&c) {
            Ok(()) => return Ok(c),
            Err(e) => last_err = Some(e),
        }
    }
    match last_err {
        Some(e) => Err(e.context("sqlcmd найден, но не может запускаться от имени службы")),
        None => bail!("не найден sqlcmd (установите SQL Server Command Line Utilities)"),
    }
}

/// Базы сервера для выбора в GUI (заодно проверка подключения). Без tempdb и баз не в сети.
pub fn list_databases(server: &str, user: Option<&str>, password: Option<&str>, timeout: std::time::Duration) -> Result<Vec<String>> {
    if server.trim().is_empty() {
        bail!("не указан сервер SQL Server");
    }
    let p = MsSqlParams {
        server,
        databases: &[],
        user,
        password,
        kind: MsSqlKind::Full,
        backup_dir: "",
        verify: false,
        timeout,
        force_full: false,
        attempts: 1,
    };
    // -h -1: без заголовка, -W: без хвостовых пробелов, -f 65001: вывод в UTF-8 (имена баз на кириллице).
    let out = sqlcmd_output(
        &p,
        &["-h", "-1", "-W", "-f", "65001"],
        "SET NOCOUNT ON; SELECT name FROM sys.databases WHERE name <> 'tempdb' AND state_desc = 'ONLINE' ORDER BY name",
    )?;
    Ok(parse_names(&out))
}

fn parse_names(out: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(out)
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

fn sqlcmd(p: &MsSqlParams, sql: &str) -> Result<()> {
    sqlcmd_output(p, &[], sql).map(|_| ())
}

fn sqlcmd_output(p: &MsSqlParams, extra: &[&str], sql: &str) -> Result<Vec<u8>> {
    let mut cmd = Command::new(find_sqlcmd()?);
    cmd.args(["-S", p.server, "-b", "-r", "1"]).args(extra).args(["-Q", sql]);
    match p.user.filter(|s| !s.is_empty()) {
        Some(u) => {
            cmd.args(["-U", u]);
            // Не -P: пароль в командной строке виден в списке процессов. SQLCMDPASSWORD читает сам sqlcmd.
            if let Some(pw) = p.password.filter(|s| !s.is_empty()) {
                cmd.env("SQLCMDPASSWORD", pw);
            }
        }
        None => {
            cmd.arg("-E");
        }
    }
    // sqlcmd сам не ограничивает время запроса (-t 0 по умолчанию): BACKUP, ожидающий блокировку,
    // может висеть бесконечно.
    let out = crate::process::run(cmd, p.timeout, "MSSQL: sqlcmd")?;
    if !out.status.success() {
        bail!(
            "MSSQL: {}{}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn quote_ident(s: &str) -> String {
    format!("[{}]", s.replace(']', "]]"))
}

fn quote_str(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_sqlcmd_output() {
        let out = "\u{feff}ERP\r\nmaster\r\nУчёт 2024\r\n\r\n".as_bytes();
        assert_eq!(parse_names(out), vec!["ERP", "master", "Учёт 2024"]);
    }

    #[test]
    fn auto_list_skips_system_and_simple_for_log() {
        let full = auto_list_sql(MsSqlKind::Full);
        assert!(full.contains("'master','model','msdb','tempdb'") && !full.contains("SIMPLE"));
        assert!(auto_list_sql(MsSqlKind::Log).contains("recovery_model_desc <> 'SIMPLE'"));
    }

    /// Старое задание с одной базой читается; новые поля по умолчанию выключены.
    #[test]
    fn old_single_database_job_reads() {
        let j: crate::Job = serde_json::from_str(
            r#"{"id":"11111111-1111-1111-1111-111111111111","name":"s","source":{"type":"ms_sql","server":"s","database":"ERP","kind":"full","backup_dir":"C:\\b"},"destinations":[]}"#,
        )
        .unwrap();
        assert!(!j.source.separate_archives());
        match &j.source {
            crate::Source::MsSql { database, databases, .. } => assert!(database == "ERP" && databases.is_empty()),
            _ => panic!("ожидался MS SQL"),
        }
        assert!(!serde_json::to_string(&j).unwrap().contains("\"databases\""));
    }

    #[test]
    fn empty_server_is_rejected_before_sqlcmd() {
        let err = list_databases(" ", None, None, std::time::Duration::from_secs(1)).unwrap_err().to_string();
        assert!(err.contains("не указан сервер"), "{err}");
    }
}
