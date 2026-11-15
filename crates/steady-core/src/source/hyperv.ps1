# Резервная копия ВМ Hyper-V через WMI (root\virtualization\v2):
#   1. Recovery-checkpoint (по умолчанию application-consistent через VSS гостя; без интеграционных
#      служб — откат на crash-consistent);
#   2. экспорт checkpoint'а; для инкрементной копии — только изменённые блоки относительно
#      reference point предыдущей копии (Resilient Change Tracking);
#   3. checkpoint преобразуется в reference point (данные сливаются в основной диск, .avhdx не остаётся);
#   4. старые reference point этой ВМ удаляются.
# Результат пишется в JSON-файл $ResultFile.
param(
    [Parameter(Mandatory)][string]$VmName,
    [Parameter(Mandatory)][string]$OutDir,
    [Parameter(Mandatory)][string]$ResultFile,
    [ValidateSet('full', 'incremental')][string]$Mode = 'full',
    [string]$BaseRef = '',
    [ValidateSet('application', 'crash')][string]$Consistency = 'application'
)
$ErrorActionPreference = 'Stop'
$ns = 'root\virtualization\v2'
$warnings = New-Object System.Collections.Generic.List[string]

function Wait-WmiJob($result, [string]$what) {
    if ($result.ReturnValue -eq 0) { return $null }
    if ($result.ReturnValue -ne 4096) { throw "${what}: код $($result.ReturnValue)" }
    $job = [wmi]$result.Job
    while ($job.JobState -in 2, 3, 4) { Start-Sleep -Milliseconds 500; $job = [wmi]$result.Job }
    if ($job.JobState -ne 7) { throw "${what}: $($job.ErrorDescription) (код $($job.ErrorCode))" }
    return $job
}

function Write-Result($obj) {
    [IO.File]::WriteAllText($ResultFile, ($obj | ConvertTo-Json -Depth 4 -Compress), (New-Object Text.UTF8Encoding($false)))
}

$snap = $null
try {
    $vm = Get-WmiObject -Namespace $ns -Class Msvm_ComputerSystem -Filter "ElementName='$($VmName.Replace("'", "\'"))'"
    if (-not $vm) { throw "виртуальная машина «$VmName» не найдена" }
    if (@($vm).Count -gt 1) { throw "найдено несколько ВМ с именем «$VmName»" }

    $snapSvc = Get-WmiObject -Namespace $ns -Class Msvm_VirtualSystemSnapshotService
    $mgmtSvc = Get-WmiObject -Namespace $ns -Class Msvm_VirtualSystemManagementService
    $refSvc = Get-WmiObject -Namespace $ns -Class Msvm_VirtualSystemReferencePointService

    # --- 1. Recovery checkpoint ---
    function New-RecoverySnapshot([int]$level) {
        $sd = ([wmiclass]"\\.\${ns}:Msvm_VirtualSystemSnapshotSettingData").CreateInstance()
        $sd.ConsistencyLevel = $level          # 1 = application consistent, 2 = crash consistent
        $sd.IgnoreNonSnapshottableDisks = $true
        $sd.GuestBackupType = 1                # полная копия VSS (с усечением журналов в госте)
        $r = $snapSvc.CreateSnapshot($vm.__PATH, $sd.GetText(1), 32768)   # 32768 = Recovery
        $job = Wait-WmiJob $r 'создание контрольной точки'
        if ($r.ResultingSnapshot) { return [wmi]$r.ResultingSnapshot }
        $s = $job.GetRelated('Msvm_VirtualSystemSettingData') | Where-Object { $_.VirtualSystemType -like '*Snapshot*' } | Select-Object -First 1
        if (-not $s) { throw 'Hyper-V не вернул созданную контрольную точку' }
        return $s
    }
    $consistencyUsed = $Consistency
    if ($Consistency -eq 'application' -and $vm.EnabledState -eq 2) {
        try { $snap = New-RecoverySnapshot 1 }
        catch {
            $warnings.Add('гостевая ОС не подготовила данные к копированию (службы интеграции Hyper-V / VSS в ВМ не отвечают) — сделана аварийно-согласованная копия, как после отключения питания')
            $consistencyUsed = 'crash'
        }
    } elseif ($Consistency -eq 'application') {
        $consistencyUsed = 'crash'   # ВМ выключена: данные на диске и так согласованы
    }
    if (-not $snap) { $snap = New-RecoverySnapshot 2 }

    # --- 2. Экспорт ---
    $base = $null
    $modeUsed = $Mode
    if ($Mode -eq 'incremental') {
        if ($BaseRef) {
            $base = Get-WmiObject -Namespace $ns -Class Msvm_VirtualSystemReferencePoint -Filter "InstanceID='$BaseRef'"
        }
        if (-not $base) {
            $modeUsed = 'full'
            $warnings.Add('базовая точка отслеживания изменений не найдена — выполнена полная копия')
        }
    }
    $es = ([wmiclass]"\\.\${ns}:Msvm_VirtualSystemExportSettingData").CreateInstance()
    # Экспортируется состояние на момент контрольной точки, без самой точки в конфигурации.
    $es.CopySnapshotConfiguration = 3        # ExportNoSnapshots
    $es.SnapshotVirtualSystem = $snap.__PATH
    $es.CopyVmRuntimeInformation = $false
    $es.CopyVmStorage = $true
    $es.CreateVmExportSubdirectory = $true
    $es.CaptureLiveState = 0                 # CaptureCrashConsistentState: память не сохраняется
    $es.BackupIntent = 1                     # PreserveChain: для инкремента выгружается .avhdx с изменёнными блоками
    if ($base) { $es.DifferentialBackupBase = $base.__PATH }
    New-Item -ItemType Directory $OutDir -Force | Out-Null
    $r = $mgmtSvc.ExportSystemDefinition($vm.__PATH, $OutDir, $es.GetText(1))
    Wait-WmiJob $r 'экспорт' | Out-Null

    # --- 3. Checkpoint → reference point ---
    $r = $snapSvc.ConvertToReferencePoint($snap.__PATH, $null)
    $job = Wait-WmiJob $r 'создание точки отслеживания изменений'
    $snap = $null
    $ref = if ($r.ResultingReferencePoint) { [wmi]$r.ResultingReferencePoint } else { $job.GetRelated('Msvm_VirtualSystemReferencePoint') | Select-Object -First 1 }
    if (-not $ref) { throw 'Hyper-V не вернул точку отслеживания изменений' }

    # --- 4. Старые reference point этой ВМ больше не нужны ---
    Get-WmiObject -Namespace $ns -Class Msvm_VirtualSystemReferencePoint -Filter "VirtualSystemIdentifier='$($vm.Name)'" |
        Where-Object { $_.InstanceID -ne $ref.InstanceID } |
        ForEach-Object {
            try { Wait-WmiJob ($refSvc.DestroyReferencePoint($_.__PATH)) 'удаление старой точки' | Out-Null }
            catch { $warnings.Add("не удалось удалить старую точку отслеживания: $($_.Exception.Message)") }
        }

    Write-Result @{ ok = $true; ref = $ref.InstanceID; mode = $modeUsed; consistency = $consistencyUsed; vm_id = $vm.Name; warnings = $warnings }
} catch {
    if ($snap) {
        try { Wait-WmiJob ($snapSvc.DestroySnapshot($snap.__PATH)) 'удаление контрольной точки' | Out-Null } catch { }
    }
    Write-Result @{ ok = $false; error = $_.Exception.Message; warnings = $warnings }
    exit 1
}
