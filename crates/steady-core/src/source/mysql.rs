//! MySQL/MariaDB на сервере Linux через SSH: `mysqldump` выполняется на сервере, вывод
//! идёт по каналу SSH (со сжатием) прямо в файл `<база>.sql` в staging.
//!
//! Учётные данные MySQL передаются команде через stdin и записываются на сервере во временный
//! файл параметров с правами 0600 (`--defaults-extra-file`), который удаляется по выходе.
//! В командной строке (её видят все пользователи сервера через `ps`) пароля нет.

use super::{dir_size, Prepared};
use crate::job::{BackupKind, SshLogin};
use crate::ssh::{sh_quote, SshSession};
use crate::state::JobState;
use anyhow::{bail, Context, Result};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

pub struct MySqlParams<'a> {
    pub ssh: &'a SshLogin,
    pub db_host: Option<&'a str>,
    pub db_port: Option<u16>,
    pub db_user: &'a str,
    /// Открытый текст.
    pub db_password: Option<&'a str>,
    pub databases: &'a [String],
    pub timeout: Duration,
    /// Попыток каждой базы.
    pub attempts: u32,
}

/// Системные базы: при выборе «все базы» не копируются.
const SYSTEM_DBS: &[&str] = &["information_schema", "performance_schema", "mysql", "sys"];

/// Файл параметров клиента MySQL. Значения в двойных кавычках, `\` и `"` экранируются.
fn defaults_file(p: &MySqlParams) -> String {
    fn q(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))
    }
    let mut s = String::from("[client]\n");
    s.push_str(&format!("user={}\n", q(p.db_user.trim())));
    if let Some(pw) = p.db_password.filter(|s| !s.is_empty()) {
        s.push_str(&format!("password={}\n", q(pw)));
    }
    if let Some(h) = p.db_host.map(str::trim).filter(|s| !s.is_empty()) {
        s.push_str(&format!("host={}\n", q(h)));
    }
    if let Some(port) = p.db_port {
        s.push_str(&format!("port={port}\n"));
        // Иначе `localhost` означает сокет, и порт не учитывается.
        if p.db_host.map(str::trim).filter(|s| !s.is_empty()).is_none() {
            s.push_str("protocol=TCP\n");
        }
    }
    s
}

/// Сценарий для `sh`: принимает файл параметров из stdin, находит утилиту (`mysqldump` или,
/// в новых MariaDB, `mariadb-dump`) и запускает её. `tool` — пары имён, `args` — уже в кавычках.
fn script(tools: &[&str], args: &str) -> String {
    let find = tools.iter().map(|t| format!("command -v {t}")).collect::<Vec<_>>().join(" || ");
    format!(
        "umask 077; f=$(mktemp) || exit 1; trap 'rm -f \"$f\"' EXIT; cat > \"$f\"; \
         t=$({find}) || {{ echo 'на сервере не найдена утилита {}' >&2; exit 127; }}; \
         \"$t\" --defaults-extra-file=\"$f\" {args}",
        tools[0]
    )
}

fn sh(script: &str) -> String {
    format!("sh -c {}", sh_quote(script))
}

fn list_command() -> String {
    sh(&script(&["mysql", "mariadb"], "-N -B -e 'SHOW DATABASES'"))
}

fn dump_command(db: &str) -> String {
    // --single-transaction: согласованный снимок InnoDB без блокировки сайта.
    // --no-tablespaces: иначе MySQL 8 требует привилегию PROCESS, которой у хостинга нет.
    // --databases: в дампе будут CREATE DATABASE и USE — восстановление одной командой.
    let args = format!(
        "--single-transaction --quick --routines --triggers --hex-blob --no-tablespaces \
         --default-character-set=utf8mb4 --databases {}",
        sh_quote(db)
    );
    sh(&script(&["mysqldump", "mariadb-dump"], &args))
}

fn check(r: &crate::ssh::ExecResult, what: &str) -> Result<()> {
    match r.code {
        Some(0) => Ok(()),
        code => {
            let mut text = r.stderr.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("; ");
            if text.chars().count() > 500 {
                text = text.chars().take(500).collect::<String>() + "…";
            }
            if text.is_empty() {
                text = "без текста ошибки".into();
            }
            match code {
                Some(c) => bail!("{what}: код {c}: {text}"),
                None => bail!("{what}: соединение закрыто без кода завершения: {text}"),
            }
        }
    }
}

fn parse_db_list(out: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(out)
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .filter(|l| !l.trim().is_empty())
        .collect()
}

fn server_databases(sess: &mut SshSession, p: &MySqlParams, timeout: Duration) -> Result<Vec<String>> {
    let (r, out) = sess.exec_capture(&list_command(), defaults_file(p).as_bytes(), timeout)?;
    check(&r, "MySQL: список баз")?;
    Ok(parse_db_list(&out))
}

/// Базы сервера для выбора в GUI (заодно проверка SSH и входа в MySQL). Системные не показываются.
pub fn list_databases(p: &MySqlParams) -> Result<Vec<String>> {
    let mut sess = SshSession::open(p.ssh, false)?;
    let all = server_databases(&mut sess, p, p.timeout)?;
    Ok(all.into_iter().filter(|d| !SYSTEM_DBS.contains(&d.to_lowercase().as_str())).collect())
}

/// Дамп завершён полностью: mysqldump пишет эту строку последней. Обрыв посередине
/// (нехватка места на сервере, kill) даёт файл без неё.
fn dump_complete(file: &Path) -> Result<bool> {
    let mut f = std::fs::File::open(file)?;
    let len = f.metadata()?.len();
    let tail = len.min(512);
    f.seek(SeekFrom::Start(len - tail))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).contains("-- Dump completed"))
}

pub fn prepare(p: &MySqlParams, staging: &Path) -> Result<Prepared> {
    if p.db_user.trim().is_empty() {
        bail!("MySQL: не указан пользователь базы данных");
    }
    std::fs::create_dir_all(staging)?;
    let pause = |i: u32| crate::util::sleep_cancellable(crate::util::backoff(i));
    let mut sess = crate::util::retry("SSH: подключение", p.attempts, pause, || SshSession::open(p.ssh, true))?;
    let mut warnings: Vec<String> = sess.host_key_warning().into_iter().collect();

    let mut databases: Vec<String> = p.databases.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if databases.is_empty() {
        databases = server_databases(&mut sess, p, Duration::from_secs(300))?
            .into_iter()
            .filter(|d| !SYSTEM_DBS.contains(&d.to_lowercase().as_str()))
            .collect();
        if databases.is_empty() {
            bail!("MySQL: пользователю {} не видна ни одна база", p.db_user.trim());
        }
        tracing::info!("MySQL: все базы сервера: {}", databases.len());
    }

    let labels = super::unique_names(&databases, &[]);
    let defaults = defaults_file(p);
    let mut parts = Vec::new();
    let mut failures = Vec::new();
    for (db, label) in databases.iter().zip(&labels) {
        crate::process::check_cancel()?;
        let file = staging.join(format!("{label}.sql"));
        let mut attempt = 0;
        let res = crate::util::retry(&format!("MySQL: база {db}"), p.attempts, pause, || {
            attempt += 1;
            // Обрыв соединения портит сеанс: повтор — через новое подключение.
            if attempt > 1 {
                sess = SshSession::open(p.ssh, true)?;
            }
            let r = dump_one(&mut sess, db, &file, defaults.as_bytes(), p.timeout);
            if r.is_err() {
                let _ = std::fs::remove_file(&file);
            }
            r
        });
        match res {
            Ok(()) => parts.push((label.clone(), vec![file])),
            Err(e) => {
                crate::process::check_cancel()?;
                tracing::error!("MySQL: база {db}: {e:#}");
                failures.push(format!("база {db}: {e:#}"));
            }
        }
    }
    if parts.is_empty() {
        bail!("MySQL: не сохранена ни одна база: {}", failures.join("; "));
    }
    warnings.dedup();

    let (items, bytes) = dir_size(staging);
    Ok(Prepared {
        kind: BackupKind::Full,
        new_state: Some(JobState { force_full: false, ..Default::default() }),
        items,
        bytes,
        warnings,
        packed: false,
        parts,
        failures,
    })
}

fn dump_one(sess: &mut SshSession, db: &str, file: &Path, defaults: &[u8], timeout: Duration) -> Result<()> {
    tracing::info!("MySQL: mysqldump {db} ({})", sess.label());
    let mut out = std::io::BufWriter::with_capacity(
        1024 * 1024,
        std::fs::File::create(file).with_context(|| format!("создание {}", file.display()))?,
    );
    let r = sess.exec_to(&dump_command(db), defaults, &mut out, timeout)?;
    std::io::Write::flush(&mut out).context("запись дампа")?;
    drop(out);
    check(&r, &format!("MySQL: mysqldump {db}"))?;
    if !dump_complete(file)? {
        bail!("MySQL: дамп {db} оборван (нет строки «Dump completed»)");
    }
    if !r.stderr.is_empty() {
        tracing::warn!("MySQL: mysqldump {db}: {}", r.stderr);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params<'a>(login: &'a SshLogin, host: Option<&'a str>, port: Option<u16>, pw: Option<&'a str>) -> MySqlParams<'a> {
        MySqlParams {
            ssh: login,
            db_host: host,
            db_port: port,
            db_user: "site",
            db_password: pw,
            databases: &[],
            timeout: Duration::from_secs(1),
            attempts: 1,
        }
    }

    #[test]
    fn defaults_file_escapes_values() {
        let l = SshLogin::default();
        let f = defaults_file(&params(&l, None, None, Some(r#"p"a\ss"#)));
        assert_eq!(f, "[client]\nuser=\"site\"\npassword=\"p\\\"a\\\\ss\"\n");
        let f = defaults_file(&params(&l, None, Some(3307), None));
        assert!(f.contains("port=3307\nprotocol=TCP\n"), "{f}");
        let f = defaults_file(&params(&l, Some("db.local"), Some(3307), None));
        assert!(f.contains("host=\"db.local\"") && !f.contains("protocol"), "{f}");
    }

    /// Имя базы не выходит из кавычек ни во внутренней, ни во внешней оболочке; пароля в команде нет.
    #[test]
    fn dump_command_quotes_database() {
        let c = dump_command("a'; rm -rf / #");
        assert!(c.starts_with("sh -c '"), "{c}");
        assert!(!c.contains("password"), "{c}");
        assert!(c.contains("--databases"), "{c}");
    }

    #[test]
    fn complete_marker_is_checked() {
        let d = std::env::temp_dir().join(format!("steady-mysql-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        let ok = d.join("ok.sql");
        std::fs::write(&ok, "CREATE TABLE t (id int);\n-- Dump completed on 2026-11-15 10:00:00\n").unwrap();
        let bad = d.join("bad.sql");
        std::fs::write(&bad, "CREATE TABLE t (id int);\nINSERT INTO").unwrap();
        assert!(dump_complete(&ok).unwrap());
        assert!(!dump_complete(&bad).unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn db_list_skips_blank_lines() {
        assert_eq!(parse_db_list(b"site\nshop\r\n\n"), vec!["site", "shop"]);
    }

    #[test]
    fn job_json_defaults() {
        let j: crate::Job = serde_json::from_str(
            r#"{"id":"11111111-1111-1111-1111-111111111111","name":"m","source":{"type":"mysql","connection":"web","db_user":"u"},"destinations":[]}"#,
        )
        .unwrap();
        match j.source {
            crate::Source::MySql { databases, separate_archives, ssh, .. } => {
                assert!(databases.is_empty() && !separate_archives && ssh.is_none());
            }
            _ => panic!("ожидался источник MySQL"),
        }
    }
}
