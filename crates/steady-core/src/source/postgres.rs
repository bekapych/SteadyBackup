//! PostgreSQL: `pg_dump -Fc` по каждой базе, по желанию роли через `pg_dumpall --globals-only`.
//!
//! Пароль передаётся переменной PGPASSWORD, не аргументом: иначе он виден в списке процессов.
//! Утилиты берутся из одного каталога `bin` установки PostgreSQL и перед запуском проверяются
//! `acl::check_trusted_exe`: служба запускает их от SYSTEM.

use super::{dir_size, Prepared};
use crate::job::BackupKind;
use crate::state::JobState;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct PgParams<'a> {
    pub host: Option<&'a str>,
    pub port: Option<u16>,
    pub user: Option<&'a str>,
    pub password: Option<&'a str>,
    pub databases: &'a [String],
    pub pg_dump: Option<&'a str>,
    pub globals: bool,
    pub verify: bool,
    pub timeout: std::time::Duration,
    /// Попыток каждой базы.
    pub attempts: u32,
}

/// Базы, которые нельзя открыть или нет смысла копировать при выборе «все базы».
const LIST_SQL: &str = "SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY datname";

fn locate_pg_dump(explicit: Option<&str>) -> Result<PathBuf> {
    let pg_dump = match explicit.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => PathBuf::from(s),
        None => find_pg_dump().context("не найден pg_dump.exe (установите PostgreSQL или укажите путь в задании)")?,
    };
    crate::acl::check_trusted_exe(&pg_dump).context("PostgreSQL: pg_dump")?;
    Ok(pg_dump)
}

fn server_databases(bin: &Path, p: &PgParams) -> Result<Vec<String>> {
    let psql = tool(bin, "psql.exe")?;
    let mut cmd = base_command(&psql, p);
    cmd.args(["-X", "-A", "-t", "-v", "ON_ERROR_STOP=1", "-c", LIST_SQL]).arg("-d").arg(conninfo("postgres"));
    let out = run(cmd, p.timeout, "PostgreSQL: список баз")?;
    Ok(parse_db_list(&out))
}

/// Базы сервера для выбора в GUI (заодно проверка подключения и наличия утилит).
pub fn list_databases(
    host: Option<&str>,
    port: Option<u16>,
    user: Option<&str>,
    password: Option<&str>,
    pg_dump: Option<&str>,
    timeout: std::time::Duration,
) -> Result<Vec<String>> {
    let pg_dump_path = locate_pg_dump(pg_dump)?;
    let bin = pg_dump_path.parent().context("PostgreSQL: каталог pg_dump")?.to_path_buf();
    let p = PgParams { host, port, user, password, databases: &[], pg_dump, globals: false, verify: false, timeout, attempts: 1 };
    server_databases(&bin, &p)
}

pub fn prepare(p: &PgParams, staging: &Path) -> Result<Prepared> {
    let pg_dump = locate_pg_dump(p.pg_dump)?;
    let bin = pg_dump.parent().context("PostgreSQL: каталог pg_dump")?.to_path_buf();
    std::fs::create_dir_all(staging)?;

    let mut databases: Vec<String> = p.databases.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if databases.is_empty() {
        databases = server_databases(&bin, p)?;
        if databases.is_empty() {
            bail!("PostgreSQL: на сервере нет баз для копирования");
        }
        tracing::info!("PostgreSQL: все базы сервера: {}", databases.len());
    }

    let labels = super::unique_names(&databases, &["globals"]);
    let pause = |i: u32| crate::util::sleep_cancellable(crate::util::backoff(i));
    let mut parts = Vec::new();
    let mut failures = Vec::new();
    for (db, label) in databases.iter().zip(&labels) {
        crate::process::check_cancel()?;
        let file = staging.join(format!("{label}.dump"));
        let res = crate::util::retry(&format!("PostgreSQL: база {db}"), p.attempts, pause, || {
            let r = dump_one(p, &pg_dump, &bin, db, &file);
            if r.is_err() {
                let _ = std::fs::remove_file(&file);
            }
            r
        });
        match res {
            Ok(()) => parts.push((label.clone(), vec![file])),
            Err(e) => {
                crate::process::check_cancel()?;
                tracing::error!("PostgreSQL: база {db}: {e:#}");
                failures.push(format!("база {db}: {e:#}"));
            }
        }
    }

    if p.globals {
        let file = staging.join("globals.sql");
        let res = crate::util::retry("PostgreSQL: роли", p.attempts, pause, || {
            let dumpall = tool(&bin, "pg_dumpall.exe")?;
            let mut cmd = base_command(&dumpall, p);
            cmd.arg("--globals-only").arg("-f").arg(&file).arg("-d").arg(conninfo("postgres"));
            run(cmd, p.timeout, "PostgreSQL: роли и табличные пространства").map(|_| ())
        });
        match res {
            Ok(()) => parts.push(("globals".to_string(), vec![file])),
            Err(e) => {
                crate::process::check_cancel()?;
                let _ = std::fs::remove_file(&file);
                failures.push(format!("{e:#}"));
            }
        }
    }
    if !parts.iter().any(|(l, _)| l != "globals") {
        bail!("PostgreSQL: не сохранена ни одна база: {}", failures.join("; "));
    }

    let (items, bytes) = dir_size(staging);
    Ok(Prepared {
        kind: BackupKind::Full,
        // Нужно только для признака force_full при частичной отправке.
        new_state: Some(JobState { force_full: false, ..Default::default() }),
        items,
        bytes,
        warnings: vec![],
        // Формат -Fc уже сжат: 7-Zip только упаковывает (пароль, тома).
        packed: true,
        parts,
        failures,
    })
}

/// pg_dump одной базы в `file` и, по желанию, проверка чтением оглавления.
fn dump_one(p: &PgParams, pg_dump: &Path, bin: &Path, db: &str, file: &Path) -> Result<()> {
    tracing::info!("PostgreSQL: pg_dump {db}");
    let mut cmd = base_command(pg_dump, p);
    cmd.args(["-F", "c", "-f"]).arg(file).arg("-d").arg(conninfo(db));
    run(cmd, p.timeout, &format!("PostgreSQL: pg_dump {db}"))?;
    if !file.is_file() {
        bail!("PostgreSQL: pg_dump не создал {}", file.display());
    }
    if p.verify {
        let pg_restore = tool(bin, "pg_restore.exe")?;
        let mut cmd = Command::new(&pg_restore);
        cmd.current_dir(bin).arg("--list").arg(file);
        run(cmd, p.timeout, &format!("PostgreSQL: проверка копии {db}"))
            .with_context(|| format!("копия {db} не читается"))?;
    }
    Ok(())
}

/// Утилита из того же каталога, что pg_dump, с той же проверкой прав.
fn tool(bin: &Path, name: &str) -> Result<PathBuf> {
    let p = bin.join(name);
    crate::acl::check_trusted_exe(&p).with_context(|| format!("PostgreSQL: {name} (должен лежать рядом с pg_dump.exe)"))?;
    Ok(p)
}

fn base_command(exe: &Path, p: &PgParams) -> Command {
    let mut cmd = Command::new(exe);
    // Каталог bin: оттуда грузятся libpq.dll и её зависимости.
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    let host = p.host.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("localhost");
    let user = p.user.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("postgres");
    cmd.arg("-h").arg(host).arg("-p").arg(p.port.unwrap_or(5432).to_string()).arg("-U").arg(user);
    // -w: без пароля не ждать ввода (у службы нет консоли), а завершиться ошибкой.
    cmd.arg("-w");
    // Пароль и файл паролей пользователя, от которого когда-то запускали утилиты, не смешиваем.
    cmd.env_remove("PGPASSFILE").env_remove("PGSERVICEFILE");
    match p.password.filter(|s| !s.is_empty()) {
        Some(pw) => {
            cmd.env("PGPASSWORD", pw);
        }
        None => {
            cmd.env_remove("PGPASSWORD");
        }
    }
    cmd.env("PGCLIENTENCODING", "UTF8").env("PGCONNECT_TIMEOUT", "30");
    cmd
}

/// Имя базы передаётся строкой подключения `dbname='...'`, а не позиционным аргументом:
/// имя, начинающееся с `-` или содержащее `=`, иначе было бы разобрано как ключ или conninfo.
pub fn conninfo(db: &str) -> String {
    format!("dbname='{}'", db.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn run(cmd: Command, timeout: std::time::Duration, what: &str) -> Result<Vec<u8>> {
    let out = crate::process::run(cmd, timeout, what)?;
    if !out.status.success() {
        bail!("{what}: {}", failure_text(&out.stdout, &out.stderr));
    }
    Ok(out.stdout)
}

fn failure_text(stdout: &[u8], stderr: &[u8]) -> String {
    let err = String::from_utf8_lossy(stderr);
    let out = String::from_utf8_lossy(stdout);
    let lines: Vec<&str> = err.lines().chain(out.lines()).map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.is_empty() {
        return "завершился с ошибкой без текста".into();
    }
    let start = lines.len().saturating_sub(3);
    let mut text = lines[start..].join("; ");
    if text.chars().count() > 500 {
        text = text.chars().take(500).collect::<String>() + "…";
    }
    text
}

fn parse_db_list(out: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(out)
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .filter(|l| !l.trim().is_empty())
        .collect()
}

/// Самая новая установленная версия: `Program Files\PostgreSQL\<версия>\bin\pg_dump.exe`.
pub fn find_pg_dump() -> Option<PathBuf> {
    let mut found: Vec<(Vec<u32>, PathBuf)> = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        let Ok(pf) = std::env::var(var) else { continue };
        let Ok(rd) = std::fs::read_dir(Path::new(&pf).join("PostgreSQL")) else { continue };
        for e in rd.flatten() {
            let Some(ver) = parse_version(&e.file_name().to_string_lossy()) else { continue };
            let exe = e.path().join("bin").join("pg_dump.exe");
            if exe.is_file() {
                found.push((ver, exe));
            }
        }
    }
    found.into_iter().max_by(|a, b| a.0.cmp(&b.0)).map(|x| x.1)
}

fn parse_version(s: &str) -> Option<Vec<u32>> {
    let v: Vec<u32> = s.split('.').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    (!v.is_empty()).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conninfo_escapes_quotes_and_backslashes() {
        assert_eq!(conninfo("erp"), "dbname='erp'");
        assert_eq!(conninfo(r"it's\x"), r"dbname='it\'s\\x'");
        assert_eq!(conninfo("-h evil"), "dbname='-h evil'");
    }

    #[test]
    fn file_names_are_unique() {
        let dbs = ["Учёт", "a b", "a_b", "A_B", "globals"].map(String::from);
        assert_eq!(super::super::unique_names(&dbs, &["globals"]), vec!["Учёт", "a_b", "a_b_2", "A_B_3", "globals_2"]);
    }

    #[test]
    fn versions_compare_numerically() {
        let mut v = [parse_version("9.6").unwrap(), parse_version("16").unwrap(), parse_version("10").unwrap()];
        v.sort();
        assert_eq!(v.last().unwrap(), &vec![16]);
        assert!(parse_version("pgAdmin 4").is_none());
    }

    #[test]
    fn db_list_skips_blank_lines() {
        assert_eq!(parse_db_list(b"erp\r\npostgres\r\n\r\n"), vec!["erp", "postgres"]);
    }

    #[test]
    fn failure_text_keeps_last_lines() {
        let t = failure_text(b"", b"pg_dump: error: connection failed\npg_dump: detail: password authentication failed\n");
        assert!(t.contains("password authentication failed"), "{t}");
        assert_eq!(failure_text(b"", b""), "завершился с ошибкой без текста");
    }

    /// Старые задания не затронуты, новое задание читается с умолчаниями.
    #[test]
    fn job_json_defaults() {
        let j: crate::Job = serde_json::from_str(
            r#"{"id":"11111111-1111-1111-1111-111111111111","name":"pg","source":{"type":"postgresql"},"destinations":[]}"#,
        )
        .unwrap();
        assert_eq!(j.process_timeout(), std::time::Duration::from_secs(6 * 3600));
        match j.source {
            crate::Source::PostgreSql { databases, verify, globals, port, .. } => {
                assert!(databases.is_empty() && verify && !globals && port.is_none());
            }
            _ => panic!("ожидался источник PostgreSQL"),
        }
    }
}
