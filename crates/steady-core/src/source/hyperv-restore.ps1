# Восстановление ВМ Hyper-V из цепочки: полная копия + инкрементные (.avhdx с изменёнными блоками).
# Каждый каталог — распакованный экспорт (содержит <VM>\Virtual Hard Disks и <VM>\Virtual Machines).
param(
    [Parameter(Mandatory)][string[]]$Exports,     # по порядку: полная, инкр.1, инкр.2 ...
    [Parameter(Mandatory)][string]$Target,        # куда положить восстановленную ВМ
    [string]$NewName = '',
    [switch]$Register                             # зарегистрировать в Hyper-V (Import-VM)
)
$ErrorActionPreference = 'Stop'
# Из командной строки (-File) массив приходит одной строкой через запятую.
if ($Exports.Count -eq 1 -and $Exports[0].Contains(',')) { $Exports = $Exports[0].Split(',') }

function Find-Vhds($dir) {
    Get-ChildItem $dir -Recurse -Include *.vhdx, *.avhdx, *.vhd, *.avhd | Where-Object { $_.DirectoryName -like '*Virtual Hard Disks*' }
}

# Ключ диска — имя базового .vhdx без суффикса _GUID у дифференциальных дисков.
function Disk-Key($file) {
    $n = [IO.Path]::GetFileNameWithoutExtension($file.Name)
    $n -replace '_[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$', ''
}

New-Item -ItemType Directory $Target -Force | Out-Null
$diskDir = Join-Path $Target 'Virtual Hard Disks'
New-Item -ItemType Directory $diskDir -Force | Out-Null

# 1. Базовые диски из полной копии.
$full = $Exports[0]
$disks = @{}
foreach ($v in Find-Vhds $full) {
    $dst = Join-Path $diskDir $v.Name
    Copy-Item $v.FullName $dst -Force
    $disks[(Disk-Key $v)] = $dst
}
if ($disks.Count -eq 0) { throw "в $full нет виртуальных дисков" }

# 2. Каждый инкремент: .avhdx привязывается к текущему диску и сливается в него.
for ($i = 1; $i -lt $Exports.Count; $i++) {
    foreach ($d in Find-Vhds $Exports[$i]) {
        $key = Disk-Key $d
        if (-not $disks.ContainsKey($key)) { throw "инкремент $($Exports[$i]): нет базового диска для $($d.Name)" }
        $tmp = Join-Path $diskDir ("inc_{0}_{1}" -f $i, $d.Name)
        Copy-Item $d.FullName $tmp -Force
        Set-VHD -Path $tmp -ParentPath $disks[$key] -IgnoreIdMismatch
        Merge-VHD -Path $tmp -DestinationPath $disks[$key]
    }
}

# 3. Конфигурация берётся из последнего экспорта (самая свежая).
$last = $Exports[$Exports.Count - 1]
$vmcx = Get-ChildItem $last -Recurse -Filter *.vmcx | Where-Object { $_.DirectoryName -like '*Virtual Machines*' } | Select-Object -First 1
if (-not $vmcx) { throw "в $last нет конфигурации ВМ (.vmcx)" }
$cfgDir = Join-Path $Target 'Virtual Machines'
New-Item -ItemType Directory $cfgDir -Force | Out-Null
Copy-Item (Join-Path $vmcx.DirectoryName '*') $cfgDir -Recurse -Force

$result = [ordered]@{ disks = @($disks.Values); config = (Join-Path $cfgDir $vmcx.Name) }

if ($Register) {
    $report = Compare-VM -Path $result.config -Copy -GenerateNewId -VirtualMachinePath $Target -VhdDestinationPath $diskDir
    foreach ($inc in $report.Incompatibilities) {
        # Диски: указываем восстановленные файлы; сеть: отключаем (сеть может отличаться).
        if ($inc.MessageId -in 40010, 40012) {
            $drive = $inc.Source
            $name = Split-Path $drive.Path -Leaf
            $key = $name -replace '_[0-9A-Fa-f-]{36}\.avhdx$', '' -replace '\.(a?vhdx?)$', ''
            $p = $disks[$key]; if (-not $p) { $p = @($disks.Values)[0] }
            $drive | Set-VMHardDiskDrive -Path $p
        } elseif ($inc.MessageId -eq 33012) {
            $inc.Source | Disconnect-VMNetworkAdapter
        }
    }
    $vm = Import-VM -CompatibilityReport $report
    if ($NewName) { Rename-VM $vm -NewName $NewName; $vm = Get-VM -Id $vm.Id }
    $result.vm = $vm.Name
    $result.vm_id = "$($vm.Id)"
}
$result | ConvertTo-Json -Compress
