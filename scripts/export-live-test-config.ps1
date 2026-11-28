<#
.SYNOPSIS
  Copy only the live-test remotes out of your rclone config into a separate
  file, for the layer 4 tests (PLAN.md section 7, DECISIONS.md D-029).

.DESCRIPTION
  Run this yourself; the agent never reads your real rclone config. Each
  remote named must be an alias into a folder called _synclone-test, or the
  script stops without writing anything. It also copies the remotes those
  aliases point at (they hold the sign-in), and nothing else. It prints
  names and types only, never tokens. The output file is readable by you
  only, and lives outside the repository.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\export-live-test-config.ps1 gdrive-test1,gdrive-test2,onedrive-test,drime-test
#>
param(
    [Parameter(Mandatory = $true, Position = 0)][string[]]$Remotes,
    [string]$Out = (Join-Path $env:USERPROFILE '.synclone-live-test\rclone.conf')
)
$ErrorActionPreference = 'Stop'

$vendored = Join-Path $PSScriptRoot '..\vendor\rclone\windows-amd64\rclone.exe'
$rclone = if (Test-Path $vendored) { (Resolve-Path $vendored).Path } else { (Get-Command rclone).Source }

# Every remote's stored values, read once (kept in memory, never printed).
$dumpText = & $rclone config dump
if ($LASTEXITCODE -ne 0) { throw "rclone config dump failed" }
$dump = ($dumpText -join "`n") | ConvertFrom-Json

function Get-Section([string]$name) {
    $v = $dump.PSObject.Properties[$name]
    if (-not $v) { throw "rclone has no remote named '$name'" }
    $lines = @("[$name]")
    foreach ($p in $v.Value.PSObject.Properties) { $lines += "$($p.Name) = $($p.Value)" }
    [pscustomobject]@{ Name = $name; Text = ($lines -join "`n"); Type = $v.Value.type; Remote = $v.Value.remote }
}

# The remote a "remote = name:path" value points at, or $null for a local path.
function Get-Target([string]$value) {
    if (-not $value) { return $null }
    if ($value -match '^[A-Za-z]:[\\/]' -or $value -match '^[\\/]') { return $null }
    $i = $value.IndexOf(':')
    if ($i -le 0) { return $null }
    $name = $value.Substring(0, $i)
    if ($name.Contains(',')) { throw "on-the-fly remote options ('$value') are not supported here" }
    return $name
}

# Barrier 1, checked before anything is written.
$sections = [ordered]@{}
foreach ($r in ($Remotes | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() } | Where-Object { $_ })) {
    $s = Get-Section $r
    $parts = $s.Remote -split '[\\/:]'
    if ($s.Type -ne 'alias') { throw "$r is a '$($s.Type)' remote; only aliases into a _synclone-test folder are allowed" }
    if ($parts -notcontains '_synclone-test' -or $parts -contains '..') { throw "$r points at '$($s.Remote)', not a _synclone-test folder" }
    $sections[$r] = $s
    Write-Host ("  {0,-16} alias -> {1}" -f $r, $s.Remote)
}

# The remotes they rely on (recursively, for wrappers such as crypt).
$queue = New-Object System.Collections.Queue
foreach ($s in @($sections.Values)) { $queue.Enqueue($s) }
while ($queue.Count -gt 0) {
    $s = $queue.Dequeue()
    $t = Get-Target $s.Remote
    if ($t -and -not $sections.Contains($t)) {
        $dep = Get-Section $t
        $sections[$t] = $dep
        Write-Host ("  {0,-16} {1} (sign-in for {2})" -f $t, $dep.Type, $s.Name)
        $queue.Enqueue($dep)
    }
}

$dir = Split-Path $Out
New-Item -ItemType Directory -Force $dir | Out-Null
$body = ($sections.Values | ForEach-Object { $_.Text }) -join "`n`n"
[IO.File]::WriteAllText($Out, $body + "`n", (New-Object Text.UTF8Encoding($false)))
# Readable by you only.
& icacls $Out /inheritance:r /grant:r "$($env:USERNAME):(R,W)" | Out-Null

Write-Host ""
Write-Host "Wrote $($sections.Count) remotes to $Out"
Write-Host "Run the live tests with:"
Write-Host "  `$env:SYNCLONE_LIVE_TEST_CONFIG = '$Out'"
Write-Host "  `$env:SYNCLONE_LIVE_TEST_REMOTES = '$(($Remotes | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() }) -join ',')'"
Write-Host "  cargo test -p synclone-provider-stub --test live -- --nocapture"
