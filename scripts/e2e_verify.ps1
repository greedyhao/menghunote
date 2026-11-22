# E2E verification: start relay dev_server (--setup-only), run sync e2e client.
# ASCII only: Windows PowerShell 5.1 mis-parses UTF-8 without BOM.
param(
    [int]$Port = 18281
)

$ErrorActionPreference = "Stop"
$root = (Resolve-Path "$PSScriptRoot\..").Path
Set-Location $root
$work = Join-Path $root "target\tmp\e2e-verify"
if (Test-Path $work) { Remove-Item -Recurse -Force $work }
New-Item -ItemType Directory -Path $work | Out-Null

$serverOut = Join-Path $work "server.out"
$serverErr = Join-Path $work "server.err"
$db = Join-Path $work "relay.sqlite3"
$vault = Join-Path $work "vault"

Write-Output "== build =="
& cargo build -q -p menghunote-relay --example dev_server -p menghunote-sync --example e2e_localhost
if ($LASTEXITCODE -ne 0) { Write-Output "BUILD_FAILED=$LASTEXITCODE"; exit 1 }

Write-Output "== start dev_server on $Port =="
$exe = Join-Path $root "target\debug\examples\dev_server.exe"
$client = Join-Path $root "target\debug\examples\e2e_localhost.exe"
$p = Start-Process -FilePath $exe -ArgumentList @("--reset","--setup-only","--listen","127.0.0.1:$Port","--db",$db) `
    -RedirectStandardOutput $serverOut -RedirectStandardError $serverErr -PassThru -WindowStyle Hidden

$secret = $null
for ($i = 0; $i -lt 120; $i++) {
    Start-Sleep -Milliseconds 250
    if (Test-Path $serverOut) {
        $line = Select-String -Path $serverOut -Pattern "setup secret" | Select-Object -First 1
        if ($line -and ($line.Line -match "([A-Za-z0-9_\-]{20,})")) { $secret = $Matches[1]; break }
        if ($p.HasExited) { break }
    }
}
if (-not $secret) {
    Write-Output "NO_SETUP_SECRET"
    Get-Content $serverOut -ErrorAction SilentlyContinue
    Get-Content $serverErr -ErrorAction SilentlyContinue
    Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
    exit 1
}
Write-Output "setup secret length = $($secret.Length)"

Write-Output "== run e2e_localhost =="
& $client --relay "http://127.0.0.1:$Port" --setup $secret --vault $vault 2>&1
$clientExit = $LASTEXITCODE
Write-Output "CLIENT_EXIT=$clientExit"

Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 300

Write-Output "== server log tail =="
Get-Content $serverOut -Tail 20 -ErrorAction SilentlyContinue
Write-Output "== server err tail =="
Get-Content $serverErr -Tail 10 -ErrorAction SilentlyContinue
exit $clientExit
