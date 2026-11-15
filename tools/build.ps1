# One-step build: service (Rust), GUI (Qt 6 / MinGW), payload folder, MSI.
#
#   build.cmd                                  double-click or run from a console
#   powershell -ExecutionPolicy Bypass -File tools\build.ps1 [-SkipTests]
#
# Output:  dist\gui\            GUI with Qt runtime
#          dist\payload\        files that go into the MSI
#          dist\SteadyBackup-<version>.msi
# Logs:    build-logs\<step>.log, build-logs\summary.txt
#
# The script does not install anything and does not touch the running service.
param(
    [switch]$SkipTests,
    [string]$Msys = 'C:\msys64\ucrt64',
    [string]$WixBin = (Join-Path $env:TEMP 'wix314')
)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$logs = Join-Path $root 'build-logs'
New-Item -ItemType Directory $logs -Force | Out-Null
Get-ChildItem $logs -File | Remove-Item -Force

$summary = New-Object System.Collections.Generic.List[string]
function Note([string]$text) {
    $summary.Add($text)
    Write-Host $text
}
function Save-Summary {
    [IO.File]::WriteAllLines((Join-Path $logs 'summary.txt'), $summary)
}
function Fail([string]$text) {
    Note "FAILED: $text"
    Save-Summary
    exit 1
}

# Runs a program with stdout and stderr written to build-logs\<name>.log. Returns the exit code.
function Step([string]$name, [string]$exe, [string]$arguments, [string]$workdir = $root) {
    $log = Join-Path $logs "$name.log"
    $out = Join-Path $logs "$name.out.tmp"
    $err = Join-Path $logs "$name.err.tmp"
    Write-Host "== ${name}: $exe $arguments"
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $p = Start-Process -FilePath $exe -ArgumentList $arguments -WorkingDirectory $workdir -NoNewWindow -Wait -PassThru `
        -RedirectStandardOutput $out -RedirectStandardError $err
    $code = $p.ExitCode
    $text = @()
    if (Test-Path $out) { $text += Get-Content $out; Remove-Item $out -Force }
    if (Test-Path $err) { $text += Get-Content $err; Remove-Item $err -Force }
    [IO.File]::WriteAllLines($log, [string[]]$text)
    $state = if ($code -eq 0) { 'ok' } else { "exit code $code" }
    Note ("{0,-14} {1}  ({2:n0} s)" -f $name, $state, $sw.Elapsed.TotalSeconds)
    if ($code -ne 0) {
        Write-Host "---- last lines of $log"
        $text | Select-Object -Last 40 | ForEach-Object { Write-Host $_ }
    }
    return $code
}

function Find-Tool([string]$name, [string[]]$candidates) {
    foreach ($c in $candidates) { if ($c -and (Test-Path $c)) { return $c } }
    $cmd = Get-Command $name -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    return $null
}

$version = '0.0.0'
$m = [regex]::Match([IO.File]::ReadAllText((Join-Path $root 'Cargo.toml')), '(?m)^version\s*=\s*"([^"]+)"')
if ($m.Success) { $version = $m.Groups[1].Value }
Note "SteadyBackup $version build, $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"
Note "root: $root"

$cargo = Find-Tool 'cargo.exe' @((Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'))
$cmake = Find-Tool 'cmake.exe' @((Join-Path $Msys 'bin\cmake.exe'))
if (-not $cargo) { Fail 'cargo.exe not found (Rust toolchain)' }
if (-not $cmake) { Fail "cmake.exe not found (expected in $Msys\bin)" }
if (-not (Test-Path (Join-Path $Msys 'bin\c++.exe'))) { Fail "MinGW compiler not found in $Msys\bin" }

# ---- 1. service ---------------------------------------------------------------------------
if ((Step 'rust-build' $cargo 'build --release') -ne 0) { Fail 'cargo build --release' }
if ($SkipTests) {
    Note 'rust-test      skipped (-SkipTests)'
} elseif ((Step 'rust-test' $cargo 'test --workspace') -ne 0) {
    Note 'WARNING: cargo test failed, see build-logs\rust-test.log. The build continues.'
}
$steady = Join-Path $root 'target\release\steady.exe'
if (-not (Test-Path $steady)) { Fail "not built: $steady" }

# ---- 2. GUI -------------------------------------------------------------------------------
# MinGW goes first in PATH only for the GUI steps: the Rust build above uses the MSVC linker.
$basePath = $env:PATH
$env:PATH = (Join-Path $Msys 'bin') + ';' + $basePath
$prefix = $Msys -replace '\\', '/'
$cfg = "-S gui -B build-gui -G Ninja -DCMAKE_BUILD_TYPE=Release -DCMAKE_PREFIX_PATH=$prefix"
if ((Step 'gui-configure' $cmake $cfg) -ne 0) { Fail 'cmake configure' }
if ((Step 'gui-build' $cmake '--build build-gui') -ne 0) { Fail 'cmake --build' }
$guiExe = Join-Path $root 'build-gui\steady-gui.exe'
if (-not (Test-Path $guiExe)) { Fail "not built: $guiExe" }

# ---- 3. GUI runtime in dist\gui -------------------------------------------------------------
$dist = Join-Path $root 'dist\gui'
New-Item -ItemType Directory $dist -Force | Out-Null
try {
    Copy-Item $guiExe (Join-Path $dist 'steady-gui.exe') -Force
} catch {
    Fail "cannot replace dist\gui\steady-gui.exe (is it running?): $($_.Exception.Message)"
}

# Every DLL the GUI imports must be next to it or in System32. Missing ones are taken from MSYS2.
$objdump = Join-Path $Msys 'bin\objdump.exe'
function Get-Imports([string]$file) {
    $lines = & $objdump -p $file 2>$null
    foreach ($l in $lines) {
        if ($l -match 'DLL Name:\s*(\S+)') { $Matches[1] }
    }
}
$missing = @()
$copied = @()
if (Test-Path $objdump) {
    $queue = New-Object System.Collections.Generic.Queue[string]
    $queue.Enqueue((Join-Path $dist 'steady-gui.exe'))
    $seen = @{}
    while ($queue.Count -gt 0) {
        $file = $queue.Dequeue()
        foreach ($dll in (Get-Imports $file)) {
            $key = $dll.ToLowerInvariant()
            if ($seen.ContainsKey($key)) { continue }
            $seen[$key] = $true
            if ($key -like 'api-ms-*' -or $key -like 'ext-ms-*') { continue }
            if (Test-Path (Join-Path $env:WINDIR "System32\$dll")) { continue }
            $local = Join-Path $dist $dll
            if (-not (Test-Path $local)) {
                $src = Join-Path $Msys "bin\$dll"
                if (Test-Path $src) {
                    Copy-Item $src $local -Force
                    $copied += $dll
                } else {
                    $missing += $dll
                    continue
                }
            }
            $queue.Enqueue($local)
        }
    }
} else {
    Note 'WARNING: objdump.exe not found, DLL dependencies were not checked.'
}
# The style sheet draws arrows from SVG resources: the image plugin must be deployed.
$svgPlugin = Join-Path $dist 'imageformats\qsvg.dll'
if (-not (Test-Path $svgPlugin)) {
    $src = Join-Path $Msys 'share\qt6\plugins\imageformats\qsvg.dll'
    if (Test-Path $src) {
        New-Item -ItemType Directory (Split-Path $svgPlugin -Parent) -Force | Out-Null
        Copy-Item $src $svgPlugin -Force
        $copied += 'imageformats\qsvg.dll'
    } else {
        $missing += 'imageformats\qsvg.dll'
    }
}
if (-not (Test-Path (Join-Path $dist 'platforms\qwindows.dll'))) { $missing += 'platforms\qwindows.dll' }
if ($copied.Count -gt 0) { Note ("gui-deploy     added from MSYS2: " + ($copied -join ', ')) }
if ($missing.Count -gt 0) { Fail ("GUI runtime is incomplete, missing: " + ($missing -join ', ')) }
Note 'gui-deploy     ok'
$env:PATH = $basePath

# ---- 4. payload ---------------------------------------------------------------------------
$payload = Join-Path $root 'dist\payload'
if (Test-Path $payload) { Remove-Item $payload -Recurse -Force }
New-Item -ItemType Directory $payload | Out-Null
Copy-Item (Join-Path $dist '*') $payload -Recurse -Force
Copy-Item $steady $payload -Force
$sevenZip = Join-Path $env:ProgramFiles '7-Zip'
if ((Test-Path (Join-Path $sevenZip '7z.exe')) -and (Test-Path (Join-Path $sevenZip '7z.dll'))) {
    Copy-Item (Join-Path $sevenZip '7z.exe'), (Join-Path $sevenZip '7z.dll') $payload -Force
    Note 'payload        ok (with 7-Zip)'
} else {
    Note 'payload        ok. WARNING: 7-Zip not found in Program Files, the MSI will not contain 7z.exe.'
}

# ---- 5. MSI -------------------------------------------------------------------------------
$msi = Join-Path $root "dist\SteadyBackup-$version.msi"
if (-not (Test-Path (Join-Path $WixBin 'candle.exe'))) {
    Note "msi            skipped: WiX 3.14 not found in $WixBin"
    Save-Summary
    exit 2
}
$ps = Join-Path $env:WINDIR 'System32\WindowsPowerShell\v1.0\powershell.exe'
$msiArgs = "-NoProfile -ExecutionPolicy Bypass -File `"$(Join-Path $root 'installer\build.ps1')`" -WixBin `"$WixBin`" -Source `"$payload`" -Out `"$msi`""
if ((Step 'msi' $ps $msiArgs) -ne 0) { Fail 'installer\build.ps1' }
if (-not (Test-Path $msi)) { Fail "MSI was not created: $msi" }

Note ''
foreach ($f in @($steady, (Join-Path $dist 'steady-gui.exe'), $msi)) {
    $i = Get-Item $f
    Note ("{0}  {1:n0} bytes  {2:yyyy-MM-dd HH:mm:ss}" -f $i.FullName, $i.Length, $i.LastWriteTime)
}
Note ("SHA256 " + (Get-FileHash $msi -Algorithm SHA256).Hash + "  " + (Split-Path $msi -Leaf))
Note 'BUILD OK'
Save-Summary
exit 0
