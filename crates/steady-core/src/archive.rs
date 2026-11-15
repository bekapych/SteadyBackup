//! Упаковка в 7-Zip через внешний 7z.exe / 7za.exe.

use crate::job::ArchiveOptions;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// 7-Zip запускается от имени службы, поэтому найденный файл проверяется на права
/// (`acl::check_trusted_exe`): копия, которую может подменить пользователь, не используется.
pub fn find_7zip(explicit: Option<&Path>) -> Result<PathBuf> {
    let p = locate_7zip(explicit)?;
    crate::acl::check_trusted_exe(&p).context("7-Zip")?;
    Ok(p)
}

fn locate_7zip(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if p.exists() {
            return Ok(p.to_path_buf());
        }
        bail!("7-Zip не найден: {}", p.display());
    }
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("7za.exe"));
            candidates.push(dir.join("7z.exe"));
        }
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(pf) = std::env::var(var) {
            candidates.push(Path::new(&pf).join("7-Zip").join("7z.exe"));
        }
    }
    candidates
        .into_iter()
        .find(|p| p.exists())
        .context("7-Zip не найден: положите 7za.exe рядом с программой или установите 7-Zip")
}

/// Упаковывает содержимое `src_dir` в `out_file` (.7z).
/// Тома и предупреждения (код 7-Zip 1: архив создан, часть файлов не попала).
pub fn pack(
    seven_zip: &Path,
    src_dir: &Path,
    out_file: &Path,
    opt: &ArchiveOptions,
    timeout: std::time::Duration,
) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let mut cmd = Command::new(seven_zip);
    cmd.arg("a")
        .arg("-t7z")
        .arg(format!("-mx={}", opt.level.min(9)))
        .arg("-mmt=on")
        .arg("-bd")
        .arg("-y")
        .arg("-ssw");
    // 7-Zip не принимает пароль через stdin или файл: только -p в командной строке
    // (виден в списке процессов). Это известное ограничение 7z.exe.
    if let Some(pw) = opt.password.as_deref().filter(|p| !p.is_empty()) {
        cmd.arg(format!("-p{pw}"));
        if opt.encrypt_headers {
            cmd.arg("-mhe=on");
        }
    }
    if let Some(v) = opt.volume_size.as_deref().filter(|v| !v.is_empty()) {
        cmd.arg(format!("-v{v}"));
    }
    cmd.arg(out_file).arg(".\\*").current_dir(src_dir);
    let mut warnings = Vec::new();
    if let Some(w) = run(cmd, "упаковка 7-Zip", timeout)? {
        warnings.push(w);
    }

    let volumes = collect_volumes(out_file)?;

    if opt.test_after {
        let first = volumes.first().context("7-Zip не создал архив")?;
        let mut t = Command::new(seven_zip);
        t.arg("t").arg("-bd").arg("-y");
        match opt.password.as_deref().filter(|p| !p.is_empty()) {
            Some(pw) => t.arg(format!("-p{pw}")),
            None => t.arg("-p"),
        };
        t.arg(first);
        if let Some(w) = run(t, "проверка архива 7-Zip", timeout)? {
            warnings.push(w);
        }
    }
    Ok((volumes, warnings))
}

fn collect_volumes(out_file: &Path) -> Result<Vec<PathBuf>> {
    if out_file.exists() {
        return Ok(vec![out_file.to_path_buf()]);
    }
    let mut vols = Vec::new();
    for i in 1.. {
        let p = PathBuf::from(format!("{}.{:03}", out_file.display(), i));
        if !p.exists() {
            break;
        }
        vols.push(p);
    }
    Ok(vols)
}

/// `Some` — код 1: архив создан, но часть файлов не упакована. Это предупреждение, не провал.
fn run(cmd: Command, what: &str, timeout: std::time::Duration) -> Result<Option<String>> {
    let out = crate::process::run(cmd, timeout, what)?;
    classify_exit(out.status.code(), &out.stdout, &out.stderr, what)
}

fn classify_exit(code: Option<i32>, stdout: &[u8], stderr: &[u8], what: &str) -> Result<Option<String>> {
    // 0 — OK, 1 — предупреждения (напр. файл занят), остальное — ошибка.
    match code {
        Some(0) => Ok(None),
        Some(1) => {
            let text = warning_text(stdout, stderr);
            tracing::warn!("{what}: {text}");
            Ok(Some(format!("{what}: {text}")))
        }
        code => bail!("{what}: код {code:?}: {}", warning_text(stdout, stderr)),
    }
}

/// Короткий текст для отчёта: stderr, если он есть, иначе stdout; не длиннее одной строки.
fn warning_text(stdout: &[u8], stderr: &[u8]) -> String {
    let err = String::from_utf8_lossy(stderr).trim().to_string();
    let text = if err.is_empty() { String::from_utf8_lossy(stdout).trim().to_string() } else { err };
    let line = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("предупреждение").trim();
    if line.chars().count() > 300 {
        format!("{}…", line.chars().take(300).collect::<String>())
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_one_is_warning_not_failure() {
        let w = classify_exit(Some(1), b"ok\nWARNING: file locked\n", b"", "упаковка 7-Zip").unwrap();
        assert_eq!(w.as_deref(), Some("упаковка 7-Zip: WARNING: file locked"));
        assert!(classify_exit(Some(0), b"", b"", "упаковка 7-Zip").unwrap().is_none());
        let err = classify_exit(Some(2), b"", b"disk full", "упаковка 7-Zip").unwrap_err().to_string();
        assert!(err.contains("код Some(2)") && err.contains("disk full"), "{err}");
    }
}
