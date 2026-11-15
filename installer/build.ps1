# Build SteadyBackup-<version>.msi from a payload folder (steady.exe, steady-gui.exe, Qt DLLs, 7z).
# WiX 3.14 (candle/light/heat). Do not use WiX 7: it requires a paid OSMF EULA.
# Normally called by tools\build.ps1, which assembles the payload in dist\payload.
param(
    [string]$WixBin = (Join-Path $env:TEMP 'wix314'),
    [string]$Source = (Join-Path $PSScriptRoot '..\dist\payload'),
    [string]$Out = (Join-Path $PSScriptRoot '..\dist\SteadyBackup-0.1.9.msi'),
    [string]$Icon = (Join-Path $PSScriptRoot '..\gui\res\steady.ico')
)
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
foreach ($tool in 'candle.exe', 'light.exe', 'heat.exe') {
    if (-not (Test-Path (Join-Path $WixBin $tool))) { throw "Missing $tool in $WixBin" }
}
if (-not (Test-Path (Join-Path $Source 'steady.exe'))) { throw "Payload not found: $Source" }
if (-not (Test-Path (Join-Path $Source 'steady-gui.exe'))) { throw "steady-gui.exe is missing in payload: $Source" }
if (-not (Test-Path $Icon)) { throw "Icon not found: $Icon" }
$Icon = (Resolve-Path $Icon).Path

$stage = Join-Path $env:TEMP 'steady-msi-stage'
$files = Join-Path $env:TEMP 'steady-files.wxs'
$work = Join-Path $env:TEMP 'steady-msi-work'
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
if (Test-Path $work) { Remove-Item $work -Recurse -Force }
New-Item -ItemType Directory $stage, $work | Out-Null
Get-ChildItem $Source -Force | Where-Object { $_.Name -notin @('install.ps1', 'README.txt') } | ForEach-Object {
    Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $stage $_.Name) -Recurse -Force
}

& (Join-Path $WixBin 'heat.exe') dir $stage -nologo -cg AppFiles -dr INSTALLFOLDER -srd -sreg -gg -g1 -var var.SourceDir -out $files
if ($LASTEXITCODE -ne 0) { throw "heat failed: $LASTEXITCODE" }

$xml = [IO.File]::ReadAllText($files)
$marker = 'Source="$(var.SourceDir)\steady.exe"'
$pos = $xml.IndexOf($marker)
if ($pos -lt 0) { throw "steady.exe was not harvested" }
$start = $xml.LastIndexOf('<File ', $pos)
$id = [regex]::Match($xml.Substring($start, $pos - $start), 'Id="[^"]+"')
if (-not $id.Success) { throw "steady.exe File Id not found" }
$xml = $xml.Remove($start + $id.Index, $id.Length).Insert($start + $id.Index, 'Id="filSteadyExe"')
# x64 package: every component must be Win64 or ICE80 fails and files land in WOW64.
$xml = $xml.Replace('<Component ', '<Component Win64="yes" ')
$utf8 = New-Object Text.UTF8Encoding $false
[IO.File]::WriteAllText($files, $xml, $utf8)

$candleOut = $work.TrimEnd('\') + '\'
& (Join-Path $WixBin 'candle.exe') -nologo -arch x64 "-dSourceDir=$stage" "-dIconFile=$Icon" -ext (Join-Path $WixBin 'WixUtilExtension.dll') -out $candleOut (Join-Path $here 'Product.wxs') $files
if ($LASTEXITCODE -ne 0) { throw "candle failed: $LASTEXITCODE" }
# ICE43/ICE57 treat the all-users Start Menu as per-user data. The shortcut component
# is rooted in Program Files with an HKLM key path, which is the per-machine pattern.
$outDir = Split-Path $Out -Parent
if (-not (Test-Path $outDir)) { New-Item -ItemType Directory $outDir | Out-Null }
& (Join-Path $WixBin 'light.exe') -nologo -ext (Join-Path $WixBin 'WixUtilExtension.dll') -sw1076 -sice:ICE43 -sice:ICE57 -out $Out (Join-Path $work 'Product.wixobj') (Join-Path $work 'steady-files.wixobj')
if ($LASTEXITCODE -ne 0) { throw "light failed: $LASTEXITCODE" }
Get-Item $Out | Format-List FullName, Length, LastWriteTime
