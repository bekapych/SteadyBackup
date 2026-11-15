# Установка SteadyBackup: служба + GUI в Program Files, автозапуск GUI в трее, ярлык в меню «Пуск».
# Запускать от администратора.
param(
    [string]$Root = (Join-Path $PSScriptRoot '..'),
    [string]$Log = (Join-Path $env:TEMP 'steady-install.log')
)
$ErrorActionPreference = 'Stop'
Start-Transcript -Path $Log -Force | Out-Null
try {
    $dir = Join-Path $env:ProgramFiles 'SteadyBackup'
    $exe = Join-Path $dir 'steady.exe'
    if (Get-Service SteadyBackup -ErrorAction SilentlyContinue) {
        & $exe uninstall
    }
    Get-Process steady-gui -ErrorAction SilentlyContinue | Stop-Process -Force
    New-Item -ItemType Directory $dir -Force | Out-Null
    Copy-Item (Join-Path $Root 'target\release\steady.exe') $exe -Force

    $sz = Join-Path $env:ProgramFiles '7-Zip'
    if (Test-Path "$sz\7z.exe") {
        Copy-Item "$sz\7z.exe", "$sz\7z.dll" $dir -Force
    }

    $gui = Join-Path $Root 'dist\gui'
    if (Test-Path "$gui\steady-gui.exe") {
        Copy-Item "$gui\*" $dir -Recurse -Force
        $guiExe = Join-Path $dir 'steady-gui.exe'
        Set-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run' -Name 'SteadyBackup' -Value "`"$guiExe`" --tray"
        $lnk = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\SteadyBackup.lnk'
        $sh = New-Object -ComObject WScript.Shell
        $s = $sh.CreateShortcut($lnk); $s.TargetPath = $guiExe; $s.WorkingDirectory = $dir; $s.Save()
    }

    # steady install выставляет права на каталог данных: только SYSTEM и Администраторы.
    & $exe install
    if ($LASTEXITCODE -ne 0) { throw "steady install: код $LASTEXITCODE" }
    Get-Service SteadyBackup | Format-Table Name, Status, StartType
    icacls (Join-Path $env:ProgramData 'SteadyBackup')
} finally {
    Stop-Transcript | Out-Null
}
