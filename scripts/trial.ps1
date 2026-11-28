<#
.SYNOPSIS
  Start Synclone for a trial against the live-test aliases only, kept apart
  from a real install (D-030).

.DESCRIPTION
  - rclone config: a copy of the file scripts/export-live-test-config.ps1
    made (copied once, so remotes added in the trial stay in the trial).
  - Drives: only the listed remotes (SYNCLONE_ONLY_REMOTES).
  - Its own database and logs (%USERPROFILE%\.synclone-trial\state), its own
    folder (%USERPROFILE%\Synclone-trial) and its own Explorer entry
    (sync root account "trial").
  Quit from the tray menu stops it. -Reset deletes the trial's state and
  folder first (nothing in the cloud is touched by that).

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts/trial.ps1
#>
param(
    [string[]]$Remotes = @('gdrive-test1', 'gdrive-test2', 'onedrive-test', 'drime-test'),
    [switch]$Release,
    [switch]$Reset
)
$ErrorActionPreference = 'Stop'

$repo = Resolve-Path (Join-Path $PSScriptRoot '..')
$trial = Join-Path $env:USERPROFILE '.synclone-trial'
$folder = Join-Path $env:USERPROFILE 'Synclone-trial'
$source = Join-Path $env:USERPROFILE '.synclone-live-test\rclone.conf'
$conf = Join-Path $trial 'rclone.conf'
$exe = Join-Path $repo $(if ($Release) { 'target\release\Synclone.exe' } else { 'target\debug\Synclone.exe' })

if (-not (Test-Path $exe)) { throw "$exe is not built; run: cargo build -p synclone-app $(if ($Release) { '--release' })" }
if ($Reset) {
    if (Get-Process Synclone -ErrorAction SilentlyContinue) { throw 'Quit Synclone from the tray first.' }
    Remove-Item -Recurse -Force (Join-Path $trial 'state') -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $folder -ErrorAction SilentlyContinue
    Write-Host 'Trial state and folder removed.'
}
if (-not (Test-Path $conf)) {
    if (-not (Test-Path $source)) { throw "No $source; run scripts/export-live-test-config.ps1 first." }
    New-Item -ItemType Directory -Force $trial | Out-Null
    Copy-Item $source $conf
    & icacls $conf /inheritance:r /grant:r "$($env:USERNAME):(R,W)" | Out-Null
}

$env:SYNCLONE_RCLONE_CONFIG = $conf
$env:SYNCLONE_ONLY_REMOTES = ($Remotes | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() } | Where-Object { $_ }) -join ','
$env:SYNCLONE_STATE_DIR = Join-Path $trial 'state'
$env:SYNCLONE_ROOT = $folder
$env:SYNCLONE_SYNC_ROOT_ACCOUNT = 'trial'
# Its own service pipe, so it never talks to a real install's service.
$env:SYNCLONE_SOCKET = "\\.\pipe\synclone-trial-$($env:USERNAME)"
Start-Process $exe
Write-Host "Synclone trial started: drives $($env:SYNCLONE_ONLY_REMOTES) in $folder"
