# Regenerates docs/evidence/windows-explorer-status.png (PLAN.md §10 Phase 4
# exit): Explorer showing Synclone's status icons for cloud-only,
# downloaded, kept-on-device and syncing items.
#
# Everything is sandboxed under target\demo: its own rclone config (an
# alias remote onto demo files), its own state dir, and its own sync root
# registration (account "demo"), removed at the end. The user's real rclone
# config and Synclone folder are never touched. Only Explorer's file list is
# captured (not the navigation pane or address bar).
#
# Usage (from the repo root, after `cargo build`):
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\evidence-windows.ps1

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$base = Join-Path $repo 'target\demo'
$ctl = Join-Path $repo 'target\debug\synclonectl.exe'
$rclone = Join-Path $repo 'vendor\rclone\windows-amd64\rclone.exe'
$out = Join-Path $repo 'docs\evidence\windows-explorer-status.png'
if (-not (Test-Path $ctl)) { throw "build first: cargo build" }

$env:SYNCLONE_STATE_DIR = Join-Path $base 'state'
$env:SYNCLONE_ROOT = Join-Path $base 'Synclone'
$env:SYNCLONE_SOCKET = '\\.\pipe\synclone-evidence-demo'
$env:SYNCLONE_RCLONE_CONFIG = Join-Path $base 'rclone.conf'
$env:RCLONE_CONFIG = $env:SYNCLONE_RCLONE_CONFIG
$env:SYNCLONE_RCLONE = $rclone
$env:SYNCLONE_PROVIDER = 'native'
$env:SYNCLONE_SYNC_ROOT_ACCOUNT = 'demo'
$env:SYNCLONE_DEBOUNCE_MS = '500'
$env:SYNCLONE_NO_BROWSER = '1'

# Windows PowerShell turns a native program's stderr into an error under
# 'Stop'; run synclonectl with that relaxed and report its exit code.
function Invoke-Ctl {
    $ErrorActionPreference = 'Continue'
    & $ctl @args 2>$null
}

function Unregister-DemoRoot {
    $sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $id = "Synclone!$sid!demo"
    $null = [Windows.Storage.Provider.StorageProviderSyncRootManager, Windows.Storage, ContentType = WindowsRuntime]
    try { [Windows.Storage.Provider.StorageProviderSyncRootManager]::Unregister($id) } catch { }
}

Unregister-DemoRoot
if (Test-Path $base) { Remove-Item -Recurse -Force $base }
$cloud = Join-Path $base 'cloud'
New-Item -ItemType Directory -Force "$cloud\Audiobooks", "$cloud\Photos 2026", "$cloud\Projects" | Out-Null
$rnd = New-Object System.Random 7
foreach ($f in @('Audiobooks\Chapter 01.m4b', 'Audiobooks\Chapter 02.m4b', 'Photos 2026\Beach.jpg', 'Budget 2027.xlsx', 'Trip plan.docx')) {
    $bytes = New-Object byte[] (300KB + $rnd.Next(0, 900KB))
    $rnd.NextBytes($bytes)
    [IO.File]::WriteAllBytes((Join-Path $cloud $f), $bytes)
}
[IO.File]::WriteAllText((Join-Path $cloud 'Notes.txt'), 'Synclone demo notes')
New-Item -ItemType File -Force $env:SYNCLONE_RCLONE_CONFIG | Out-Null

$daemon = Start-Process -FilePath $ctl -ArgumentList 'daemon' -PassThru -WindowStyle Hidden `
    -RedirectStandardError (Join-Path $base 'daemon.log')
try {
    for ($i = 0; $i -lt 100; $i++) {
        Invoke-Ctl status | Out-Null
        if ($LASTEXITCODE -eq 0) { break }
        Start-Sleep -Milliseconds 200
    }
    Invoke-Ctl drive add alias --name 'Demo Cloud' --param "remote=$cloud" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'adding the demo drive failed; see target\demo\daemon.log' }
    $drive = Join-Path $env:SYNCLONE_ROOT 'Demo Cloud'
    Invoke-Ctl sync | Out-Null

    # Browse (placeholders appear), open two files (downloaded, kept),
    # keep a folder on this device, and drop in a new file (uploads).
    Get-ChildItem -LiteralPath $drive | Out-Null
    [IO.File]::ReadAllText((Join-Path $drive 'Notes.txt')) | Out-Null
    [IO.File]::ReadAllBytes((Join-Path $drive 'Trip plan.docx')) | Out-Null
    attrib +P (Join-Path $drive 'Audiobooks')
    [IO.File]::WriteAllText((Join-Path $drive 'New idea.txt'), 'written locally')
    Start-Sleep -Seconds 8

    Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Drawing
    Add-Type @'
using System; using System.Runtime.InteropServices;
public static class W {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
}
'@
    [W]::SetProcessDPIAware() | Out-Null
    $shell = New-Object -ComObject Shell.Application
    Start-Process explorer.exe "`"$drive`""
    $win = $null
    for ($i = 0; $i -lt 50 -and -not $win; $i++) {
        Start-Sleep -Milliseconds 200
        $win = $shell.Windows() | Where-Object { try { $_.Document.Folder.Self.Path -eq $drive } catch { $false } } | Select-Object -First 1
    }
    if (-not $win) { throw 'the Explorer window did not open' }
    $win.Document.CurrentViewMode = 4   # Details, which has the Status column
    [W]::ShowWindow([IntPtr]$win.HWND, 3) | Out-Null   # maximized
    [W]::SetForegroundWindow([IntPtr]$win.HWND) | Out-Null
    Start-Sleep -Seconds 3
    $root = [System.Windows.Automation.AutomationElement]::FromHandle([IntPtr]$win.HWND)
    $cond = New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::ClassNameProperty, 'UIItemsView')
    $items = $root.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $cond)
    if (-not $items) { throw 'could not find the file list' }
    $r = $items.Current.BoundingRectangle
    # Render the Explorer window itself (PrintWindow), never the screen:
    # whatever else is open, or on top, can never end up in the picture.
    $wr = New-Object W+RECT
    [W]::GetWindowRect([IntPtr]$win.HWND, [ref]$wr) | Out-Null
    $full = New-Object System.Drawing.Bitmap ($wr.R - $wr.L), ($wr.B - $wr.T)
    $g = [System.Drawing.Graphics]::FromImage($full)
    $hdc = $g.GetHdc()
    $ok = [W]::PrintWindow([IntPtr]$win.HWND, $hdc, 2)   # PW_RENDERFULLCONTENT
    $g.ReleaseHdc($hdc)
    if (-not $ok) { throw 'PrintWindow failed' }
    # Crop to the file list; the rows sit at the top of it.
    $h = [Math]::Min([int]$r.Height, 330)
    $crop = New-Object System.Drawing.Rectangle ([int]$r.X - $wr.L), ([int]$r.Y - $wr.T), ([int]$r.Width), $h
    $bmp = $full.Clone($crop, $full.PixelFormat)
    New-Item -ItemType Directory -Force (Split-Path $out) | Out-Null
    $bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
    $win.Quit()
    Invoke-Ctl --json ls --drive 'Demo Cloud' | Out-File -Encoding utf8 (Join-Path $base 'ls.json')
    Write-Output "saved $out"
}
finally {
    Invoke-Ctl shutdown | Out-Null
    Start-Sleep -Seconds 2
    if (-not $daemon.HasExited) { Stop-Process -Id $daemon.Id -Force }
    Unregister-DemoRoot
}
