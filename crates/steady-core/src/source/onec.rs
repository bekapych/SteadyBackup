//! Файловые базы 1С: выгрузка .dt средствами платформы или копия 1Cv8.1CD.

use super::{dir_size, Prepared};
use crate::job::{BackupKind, OneCMode};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct OneCParams<'a> {
    pub ib_path: &'a str,
    pub platform_exe: Option<&'a str>,
    pub user: Option<&'a str>,
    pub password: Option<&'a str>,
    pub mode: OneCMode,
    pub unlock_code: Option<&'a str>,
    pub timeout: std::time::Duration,
}

pub fn prepare(p: &OneCParams, staging: &Path) -> Result<Prepared> {
    let ib = Path::new(p.ib_path);
    let cd = ib.join("1Cv8.1CD");
    if !cd.exists() {
        bail!("в каталоге {} нет файла 1Cv8.1CD", ib.display());
    }
    std::fs::create_dir_all(staging)?;
    match p.mode {
        OneCMode::DumpDt => dump_dt(p, ib, staging)?,
        OneCMode::CopyFile => {
            std::fs::copy(&cd, staging.join("1Cv8.1CD"))
                .context("копирование 1Cv8.1CD (база открыта? используйте выгрузку .dt или VSS)")?;
        }
    }
    let (items, bytes) = dir_size(staging);
    Ok(Prepared { kind: BackupKind::Full, new_state: None, items, bytes, warnings: vec![], packed: false, parts: vec![], failures: vec![] })
}

fn dump_dt(p: &OneCParams, ib: &Path, staging: &Path) -> Result<()> {
    let exe = match p.platform_exe {
        Some(e) if !e.is_empty() => PathBuf::from(e),
        _ => find_platform().context("не найдена платформа 1С (1cv8.exe); укажите путь в задании")?,
    };
    // Путь берётся из задания и запускается от SYSTEM.
    crate::acl::check_trusted_exe(&exe).context("1С: платформа")?;
    let base_name = ib.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "ib".into());
    let dt = staging.join(format!("{base_name}.dt"));
    let log = staging.join("1c_dump.log");

    let mut cmd = Command::new(&exe);
    cmd.arg("DESIGNER")
        .arg(format!("/F{}", ib.display()))
        .arg("/DisableStartupDialogs")
        .arg("/DisableStartupMessages")
        .arg("/DumpIB")
        .arg(&dt)
        .arg("/Out")
        .arg(&log);
    if let Some(u) = p.user.filter(|s| !s.is_empty()) {
        cmd.arg(format!("/N{u}"));
    }
    // Платформа 1С принимает пароль только аргументом /P (и код /UC). Другого канала нет.
    if let Some(pw) = p.password.filter(|s| !s.is_empty()) {
        cmd.arg(format!("/P{pw}"));
    }
    if let Some(uc) = p.unlock_code.filter(|s| !s.is_empty()) {
        cmd.arg(format!("/UC{uc}"));
    }
    tracing::info!("1С: выгрузка {} → {}", ib.display(), dt.display());
    let status = crate::process::run(cmd, p.timeout, "1С: выгрузка .dt")?.status;
    let log_text = read_log(&log);

    if !status.success() || !dt.exists() || std::fs::metadata(&dt).map(|m| m.len()).unwrap_or(0) == 0 {
        bail!("1С: выгрузка не выполнена (код {:?}). Лог: {}", status.code(), log_text.trim());
    }
    tracing::info!("1С: {}", log_text.trim());
    Ok(())
}

/// Лог 1С пишется в UTF-8 с BOM или в UTF-16LE в зависимости от версии.
fn read_log(path: &Path) -> String {
    let Ok(b) = std::fs::read(path) else { return String::new() };
    if b.starts_with(&[0xFF, 0xFE]) {
        let u: Vec<u16> = b[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&u);
    }
    let s = b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&b);
    String::from_utf8_lossy(s).into_owned()
}

/// Ищет последнюю версию платформы в Program Files\1cv8\<версия>\bin\1cv8.exe.
pub fn find_platform() -> Option<PathBuf> {
    let mut found: Vec<(Vec<u32>, PathBuf)> = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        let Ok(pf) = std::env::var(var) else { continue };
        let root = Path::new(&pf).join("1cv8");
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let ver: Vec<u32> = name.split('.').filter_map(|x| x.parse().ok()).collect();
            if ver.len() != 4 {
                continue;
            }
            let exe = e.path().join("bin").join("1cv8.exe");
            if exe.exists() {
                found.push((ver, exe));
            }
        }
    }
    found.into_iter().max_by(|a, b| a.0.cmp(&b.0)).map(|x| x.1)
}
