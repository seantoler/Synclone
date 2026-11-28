# Installs what the WebDriver tests (crates/synclone-app/tests/webdriver.rs)
# need, inside the repository only (target\tools):
#   - tauri-driver 2.x via cargo
#   - msedgedriver matching the installed WebView2 runtime, signature checked
# Then run:  $env:SYNCLONE_WEBDRIVER='1'; cargo test -p synclone-app --test webdriver
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$tools = Join-Path $repo 'target\tools'
cargo install --locked --root $tools tauri-driver@2
$key = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
$ver = (Get-ItemProperty $key).pv
if (-not $ver) { throw 'the WebView2 runtime is not installed' }
$zip = Join-Path $tools 'edgedriver.zip'
Invoke-WebRequest "https://msedgedriver.microsoft.com/$ver/edgedriver_win64.zip" -OutFile $zip
Expand-Archive -Force $zip (Join-Path $tools 'edgedriver')
$exe = Join-Path $tools 'edgedriver\msedgedriver.exe'
$sig = Get-AuthenticodeSignature $exe
if ($sig.Status -ne 'Valid' -or $sig.SignerCertificate.Subject -notmatch 'O=Microsoft Corporation') {
    Remove-Item -Recurse -Force (Join-Path $tools 'edgedriver')
    throw "msedgedriver is not signed by Microsoft ($($sig.Status))"
}
Write-Output "tauri-driver and msedgedriver $ver are in $tools"
