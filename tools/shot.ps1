# Снимок окна процесса (для проверки GUI). shot.ps1 <имя процесса> <png> [заголовок]
param([string]$Process, [string]$Out, [string]$Title = '')
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System; using System.Runtime.InteropServices; using System.Text;
public class W {
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc f, IntPtr p);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint f);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  public struct RECT { public int L, T, R, B; }
}
"@
[W]::SetProcessDPIAware() | Out-Null
$pids = (Get-Process $Process).Id
$found = $null
$cb = [W+EnumProc]{ param($h, $p)
  $wp = 0; [W]::GetWindowThreadProcessId($h, [ref]$wp) | Out-Null
  if ($pids -contains $wp -and [W]::IsWindowVisible($h)) {
    $sb = New-Object Text.StringBuilder 256; [W]::GetWindowText($h, $sb, 256) | Out-Null
    if ($sb.Length -gt 0 -and ($Title -eq '' -or $sb.ToString() -like "*$Title*")) { $script:found = $h; return $false }
  }
  return $true }
[W]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
if (-not $found) { throw "окно не найдено" }
$r = New-Object W+RECT; [W]::GetWindowRect($found, [ref]$r) | Out-Null
$bmp = New-Object Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
$g = [Drawing.Graphics]::FromImage($bmp); $dc = $g.GetHdc()
[W]::PrintWindow($found, $dc, 2) | Out-Null
$g.ReleaseHdc($dc); $g.Dispose(); $bmp.Save($Out); $bmp.Dispose()
$Out
