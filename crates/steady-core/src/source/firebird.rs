//! Firebird: резервная копия через gbak.exe той же версии, что и сервер.
//!
//! Пароль передаётся переменной ISC_PASSWORD, не аргументом -password: иначе он виден
//! в списке процессов. Результат — файл .fbk в staging; дальше его пакует общий конвейер.

use super::{dir_size, Prepared};
use crate::job::BackupKind;
use crate::state::JobState;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct FirebirdParams<'a> {
    pub gbak: &'a str,
    pub user: Option<&'a str>,
    pub password: Option<&'a str>,
    pub databases: &'a [String],
    pub names_from: Option<&'a str>,
    pub path_template: Option<&'a str>,
    pub host: Option<&'a str>,
    pub verify: bool,
    pub timeout: std::time::Duration,
}

pub fn prepare(p: &FirebirdParams, staging: &Path) -> Result<Prepared> {
    let gbak = PathBuf::from(p.gbak);
    crate::acl::check_trusted_exe(&gbak).context("Firebird: gbak")?;
    let specs = expand_databases(p.databases, p.names_from, p.path_template, p.host)?;
    std::fs::create_dir_all(staging)?;
    let mut warnings = Vec::new();
    for spec in &specs {
        let file = staging.join(backup_file_name(spec));
        tracing::info!("Firebird: gbak -b {spec}");
        gbak_run(&gbak, p.user, p.password, &["-b", spec, &file.display().to_string()], p.timeout, "Firebird: резервная копия")?;
        if p.verify {
            let restored = staging.join(format!("{}.restored.fdb", file.file_stem().unwrap_or_default().to_string_lossy()));
            let restore = gbak_run(
                &gbak,
                p.user,
                p.password,
                &["-c", &file.display().to_string(), &restored.display().to_string()],
                p.timeout,
                "Firebird: проверка восстановления",
            );
            let _ = std::fs::remove_file(&restored);
            restore.with_context(|| format!("копия {spec} не восстанавливается"))?;
        }
        if !file.is_file() {
            bail!("Firebird: gbak не создал {}", file.display());
        }
    }
    if specs.len() > 1 {
        warnings.push(format!("баз Firebird: {}", specs.len()));
    }
    let (items, bytes) = dir_size(staging);
    Ok(Prepared {
        kind: BackupKind::Full,
        new_state: Some(JobState { force_full: false, ..Default::default() }),
        items,
        bytes,
        warnings,
        packed: false,
        parts: vec![],
        failures: vec![],
    })
}

/// Явный список плюс подпапки `names_from` по шаблону `{name}`.
pub fn expand_databases(
    databases: &[String],
    names_from: Option<&str>,
    path_template: Option<&str>,
    host: Option<&str>,
) -> Result<Vec<String>> {
    let mut out: Vec<String> = databases.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if let Some(dir) = names_from.map(str::trim).filter(|s| !s.is_empty()) {
        let template = path_template.map(str::trim).filter(|s| !s.is_empty()).context(
            "для папки имён укажите шаблон пути, например E:\\{name}\\STANDART.FDB",
        )?;
        if !template.contains("{name}") {
            bail!("шаблон пути должен содержать {{name}}");
        }
        let mut names = Vec::new();
        for e in std::fs::read_dir(dir).with_context(|| format!("папка имён {dir}"))? {
            let e = e?;
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                names.push(e.file_name().to_string_lossy().into_owned());
            }
        }
        names.sort();
        let host = host.map(str::trim).filter(|s| !s.is_empty());
        for name in names {
            let path = template.replace("{name}", &name);
            out.push(match host {
                Some(h) => format!("{h}:{path}"),
                None => path,
            });
        }
    }
    if out.is_empty() {
        bail!("не указаны базы Firebird");
    }
    Ok(out)
}

/// `localhost:E:\Shop\STANDART.FDB` → `Shop_STANDART.fbk`. Буква диска не считается сервером.
pub fn backup_file_name(spec: &str) -> String {
    let path = database_path(spec);
    let path = path.trim_end_matches(['\\', '/']);
    let file = path.rsplit(['\\', '/']).next().unwrap_or("database");
    let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
    let parent = path.rsplit_once(['\\', '/']).map(|(p, _)| p.rsplit(['\\', '/']).next().unwrap_or(p));
    let name = match parent.filter(|p| !p.is_empty() && !p.ends_with(':')) {
        Some(dir) => format!("{dir}_{stem}"),
        None => stem.to_string(),
    };
    format!("{}.fbk", crate::pipeline::safe_name(&name))
}

fn database_path(spec: &str) -> &str {
    let b = spec.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
        return spec;
    }
    if let Some((host, path)) = spec.split_once(':') {
        if !host.is_empty() && !host.contains('\\') && !host.contains('/') {
            return path;
        }
    }
    spec
}

fn gbak_run(exe: &Path, user: Option<&str>, password: Option<&str>, args: &[&str], timeout: std::time::Duration, what: &str) -> Result<()> {
    let mut cmd = Command::new(exe);
    // Каталог gbak: оттуда грузятся fbclient.dll и firebird.msg. Иначе служба,
    // стартующая из System32, не находит файлы установки.
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    if let Some(root) = firebird_root(exe) {
        cmd.env("FIREBIRD", root);
    }
    cmd.env("ISC_USER", user.filter(|s| !s.is_empty()).unwrap_or("SYSDBA"));
    if let Some(pw) = password.filter(|s| !s.is_empty()) {
        cmd.env("ISC_PASSWORD", pw);
    }
    cmd.args(args);
    let out = crate::process::run(cmd, timeout, what)?;
    if !out.status.success() {
        bail!("{what}: {}", gbak_failure_text(&out.stdout, &out.stderr));
    }
    Ok(())
}

/// Корень установки: каталог над `bin`, либо каталог, где лежит firebird.conf.
fn firebird_root(gbak: &Path) -> Option<PathBuf> {
    let dir = gbak.parent()?;
    if dir.file_name().is_some_and(|n| n.eq_ignore_ascii_case("bin")) {
        return dir.parent().map(Path::to_path_buf);
    }
    if dir.join("firebird.conf").is_file() {
        return Some(dir.to_path_buf());
    }
    None
}

/// Последняя строка gbak всегда «Exiting before completion» и не говорит, что сломалось.
fn gbak_failure_text(stdout: &[u8], stderr: &[u8]) -> String {
    let err = String::from_utf8_lossy(stderr);
    let out = String::from_utf8_lossy(stdout);
    let lines: Vec<&str> = err
        .lines()
        .chain(out.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| !l.to_ascii_lowercase().contains("exiting before completion"))
        .collect();
    let errors: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| {
            let low = l.to_ascii_lowercase();
            low.contains("error") || low.contains("ошибк") || low.contains("unavailable") || low.contains("failed")
        })
        .collect();
    let chosen: &[&str] = if errors.is_empty() { &lines } else { &errors };
    if chosen.is_empty() {
        return "gbak завершился с ошибкой без текста. Нужны путь к базе, пароль и fbclient.dll той же версии рядом с gbak.exe".into();
    }
    let start = chosen.len().saturating_sub(4);
    let mut text = chosen[start..].join("; ");
    if text.len() > 500 {
        text.truncate(500);
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_do_not_treat_drive_letter_as_host() {
        assert_eq!(backup_file_name(r"E:\Shop\STANDART.FDB"), "Shop_STANDART.fbk");
        assert_eq!(backup_file_name(r"localhost:E:\Shop\STANDART.FD3"), "Shop_STANDART.fbk");
        assert_eq!(backup_file_name(r"localhost:E:\Другой магазин\STANDART.FDB"), "Другой_магазин_STANDART.fbk");
    }

    #[test]
    fn expand_from_folder_names() {
        let root = std::env::temp_dir().join(format!("steady-fb-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(root.join("beta")).unwrap();
        std::fs::create_dir_all(root.join("alpha")).unwrap();
        std::fs::write(root.join("skip.txt"), b"x").unwrap();
        let specs = expand_databases(
            &[r"localhost:E:\Extra\STANDART.FDB".into()],
            Some(&root.display().to_string()),
            Some(r"E:\{name}\STANDART.FD3"),
            Some("localhost"),
        )
        .unwrap();
        assert_eq!(
            specs,
            vec![
                r"localhost:E:\Extra\STANDART.FDB".to_string(),
                r"localhost:E:\alpha\STANDART.FD3".to_string(),
                r"localhost:E:\beta\STANDART.FD3".to_string(),
            ]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn failure_text_skips_the_generic_exit_line() {
        let text = gbak_failure_text(
            b"",
            b"gbak: ERROR: I/O error for file \"E:\\Shop\\STANDART.FDB\"\r\n\
              gbak: ERROR: Error while trying to open file\r\n\
              gbak: Exiting before completion due to errors\r\n",
        );
        assert!(text.contains("I/O error"), "{text}");
        assert!(!text.to_ascii_lowercase().contains("exiting before completion"), "{text}");
    }

    #[test]
    fn template_must_contain_name() {
        let err = expand_databases(&[], Some(r"C:\names"), Some(r"E:\fixed.fdb"), None).unwrap_err().to_string();
        assert!(err.contains("{name}"), "{err}");
    }
}
