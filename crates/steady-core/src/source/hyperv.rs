//! Hyper-V: recovery-checkpoint (VSS гостя) → экспорт (полный или изменённые блоки RCT) →
//! преобразование checkpoint'а в reference point. Сам сценарий — hyperv.ps1 (WMI v2).

use super::{dir_size, Prepared};
use crate::job::{BackupKind, HyperVConsistency};
use crate::state::JobState;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;

const SCRIPT: &str = include_str!("hyperv.ps1");
const RESTORE_SCRIPT: &str = include_str!("hyperv-restore.ps1");
/// Восстановление запускается вручную из CLI; предел только от бесконечного зависания.
const RESTORE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

fn with_bom(s: &str) -> Vec<u8> {
    // Windows PowerShell 5 читает скрипты без BOM в ANSI-кодировке.
    [b"\xEF\xBB\xBF".as_slice(), s.trim_start_matches('\u{feff}').as_bytes()].concat()
}

/// Восстанавливает ВМ из цепочки архивов (полная + инкрементные, по порядку).
/// Архивы распаковываются во временный каталог внутри `target`.
pub fn restore(
    seven_zip: &Path,
    archives: &[std::path::PathBuf],
    password: Option<&str>,
    target: &Path,
    new_name: Option<&str>,
    register: bool,
) -> Result<String> {
    if archives.is_empty() {
        bail!("не указаны архивы");
    }
    let tmp = target.join(".steady-restore");
    std::fs::create_dir_all(&tmp)?;
    let mut exports = Vec::new();
    for (i, a) in archives.iter().enumerate() {
        let dir = tmp.join(format!("{i:03}"));
        let mut cmd = Command::new(seven_zip);
        cmd.arg("x").arg("-y").arg("-bd").arg(format!("-o{}", dir.display()));
        cmd.arg(match password {
            Some(p) if !p.is_empty() => format!("-p{p}"),
            _ => "-p".into(),
        });
        // Многотомный архив: достаточно указать первый том.
        cmd.arg(a);
        let out = crate::process::run(cmd, RESTORE_TIMEOUT, "распаковка 7-Zip")?;
        if !out.status.success() {
            bail!("распаковка {}: {}", a.display(), String::from_utf8_lossy(&out.stdout).lines().last().unwrap_or(""));
        }
        exports.push(dir);
    }
    let script = tmp.join("restore.ps1");
    std::fs::write(&script, with_bom(RESTORE_SCRIPT))?;
    let mut cmd = Command::new(crate::util::powershell_exe());
    cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"]).arg(&script);
    cmd.arg("-Exports").arg(exports.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(","));
    cmd.arg("-Target").arg(target);
    if let Some(n) = new_name.filter(|s| !s.is_empty()) {
        cmd.arg("-NewName").arg(n);
    }
    if register {
        cmd.arg("-Register");
    }
    let out = crate::process::run(cmd, RESTORE_TIMEOUT, "Hyper-V: восстановление");
    let _ = std::fs::remove_dir_all(&tmp);
    let out = out?;
    if !out.status.success() {
        bail!("Hyper-V: восстановление: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub struct HyperVParams<'a> {
    pub vm_name: &'a str,
    pub kind: BackupKind,
    pub full_every: u32,
    pub consistency: HyperVConsistency,
    pub timeout: std::time::Duration,
}

#[derive(Deserialize)]
struct ScriptResult {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    r#ref: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    consistency: Option<String>,
    #[serde(default)]
    warnings: Vec<String>,
}

/// Удаляет recovery-точки ВМ, оставшиеся от прерванного запуска (сценарий убит по тайм-ауту
/// или служба остановлена). Recovery-точки создаёт только резервное копирование, их нет в
/// списке контрольных точек Hyper-V Manager, поэтому пользовательские точки не затрагиваются.
///
/// `DestroySnapshot` асинхронный: без ожидания задания WMI следующий экспорт стартует, пока
/// точка ещё есть. Имя ВМ передаётся аргументом, в текст сценария не подставляется.
fn cleanup_orphan_checkpoints(vm_name: &str, timeout: std::time::Duration) {
    const SCRIPT: &str = r#"param([string]$VmName)
$ErrorActionPreference = 'Stop'
$ns = 'root\virtualization\v2'

function Wait-WmiJob($result, [string]$what) {
    if ($null -eq $result) { return }
    if ($result.ReturnValue -eq 0) { return }
    if ($result.ReturnValue -ne 4096) { throw "${what}: код $($result.ReturnValue)" }
    $job = [wmi]$result.Job
    while ($job.JobState -in 2, 3, 4) { Start-Sleep -Milliseconds 500; $job = [wmi]$result.Job }
    if ($job.JobState -ne 7) { throw "${what}: $($job.ErrorDescription) (код $($job.ErrorCode))" }
}

# В фильтре WMI одинарная кавычка экранируется удвоением.
$escaped = $VmName.Replace("'", "''")
$vms = @(Get-WmiObject -Namespace $ns -Class Msvm_ComputerSystem -Filter "ElementName='$escaped'")
if ($vms.Count -eq 0) { exit 0 }
$svc = Get-WmiObject -Namespace $ns -Class Msvm_VirtualSystemSnapshotService
foreach ($vm in $vms) {
    $id = $vm.Name.Replace("'", "''")
    $snaps = @(Get-WmiObject -Namespace $ns -Class Msvm_VirtualSystemSettingData -Filter "VirtualSystemIdentifier='$id'" |
        Where-Object { $_.VirtualSystemType -eq 'Microsoft:Hyper-V:Snapshot:Recovery' })
    foreach ($s in $snaps) {
        Wait-WmiJob ($svc.DestroySnapshot($s.__PATH)) 'удаление контрольной точки'
        "удалена $($s.InstanceID)"
    }
}
"#;
    let dir = std::env::temp_dir().join(format!("steady-hv-{}", uuid::Uuid::new_v4().simple()));
    let res = (|| -> Result<String> {
        std::fs::create_dir_all(&dir)?;
        let script = dir.join("cleanup.ps1");
        std::fs::write(&script, with_bom(SCRIPT))?;
        let mut cmd = Command::new(crate::util::powershell_exe());
        cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&script)
            .arg("-VmName")
            .arg(vm_name);
        let out = crate::process::run(cmd, timeout, "Hyper-V: уборка контрольных точек")?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    match res {
        Ok(s) if !s.is_empty() => tracing::warn!("Hyper-V: «{vm_name}»: остаточные контрольные точки: {s}"),
        Ok(_) => {}
        Err(e) => tracing::error!("Hyper-V: «{vm_name}»: уборка контрольных точек: {e:#}"),
    }
}

pub fn prepare(p: &HyperVParams, state: &JobState, staging: &Path) -> Result<Prepared> {
    // Точки от прерванного прошлого запуска мешают созданию новой и занимают место.
    cleanup_orphan_checkpoints(p.vm_name, p.timeout);
    let want_full = p.kind == BackupKind::Full
        || state.force_full
        || state.hyperv_ref.is_none()
        || (p.full_every > 0 && state.since_full >= p.full_every);
    let mode = if want_full { "full" } else { "incremental" };

    let work = staging.parent().unwrap_or(staging).join("hyperv");
    std::fs::create_dir_all(&work)?;
    let script = work.join("backup.ps1");
    std::fs::write(&script, with_bom(SCRIPT))?;
    let result_file = work.join("result.json");
    let out_dir = staging.join("vm");

    let mut cmd = Command::new(crate::util::powershell_exe());
    cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .arg("-VmName")
        .arg(p.vm_name)
        .arg("-OutDir")
        .arg(&out_dir)
        .arg("-ResultFile")
        .arg(&result_file)
        .arg("-Mode")
        .arg(mode)
        .arg("-Consistency")
        .arg(match p.consistency {
            HyperVConsistency::Application => "application",
            HyperVConsistency::Crash => "crash",
        });
    if !want_full {
        cmd.arg("-BaseRef").arg(state.hyperv_ref.as_deref().unwrap_or_default());
    }
    tracing::info!("Hyper-V: {} копия ВМ «{}»", if want_full { "полная" } else { "инкрементная" }, p.vm_name);
    let out = match crate::process::run(cmd, p.timeout, "Hyper-V: экспорт ВМ") {
        Ok(o) => o,
        Err(e) => {
            // Сценарий убит до своего catch — временная контрольная точка могла остаться.
            cleanup_orphan_checkpoints(p.vm_name, p.timeout);
            return Err(e);
        }
    };

    let res: ScriptResult = match std::fs::read(&result_file) {
        Ok(b) => serde_json::from_slice(b.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&b))
            .context("Hyper-V: неверный ответ сценария")?,
        Err(_) => bail!(
            "Hyper-V: сценарий завершился без результата: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    };
    if !res.ok {
        bail!("Hyper-V: {}", res.error.unwrap_or_else(|| "неизвестная ошибка".into()));
    }
    for w in &res.warnings {
        tracing::warn!("Hyper-V: {w}");
    }
    let kind = if res.mode.as_deref() == Some("incremental") { BackupKind::Incremental } else { BackupKind::Full };
    tracing::info!("Hyper-V: согласованность копии: {}", res.consistency.as_deref().unwrap_or("?"));

    let new_state = JobState {
        hyperv_ref: res.r#ref,
        since_full: if kind == BackupKind::Full { 0 } else { state.since_full + 1 },
        ..Default::default()
    };
    let (items, bytes) = dir_size(staging);
    Ok(Prepared { kind, new_state: Some(new_state), items, bytes, warnings: res.warnings, packed: false, parts: vec![], failures: vec![] })
}
