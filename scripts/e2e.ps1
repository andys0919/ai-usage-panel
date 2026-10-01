<#
.SYNOPSIS
  End-to-end check against the REAL accounts on this machine.
  Read-only: it never refreshes a token and never writes a credential file.

.DESCRIPTION
  1. builds the probe and the desktop app
  2. probe (no network writes): every account has a known state, healthy ones carry sane
     windows, and nothing that looks like a token appears in the output
  3. launches the desktop app in read-only mode, waits for its first update cycle, checks the
     cache file, the window and a screenshot
  4. with -Cadence: also waits one full interval and proves the automatic 5-minute refresh

.PARAMETER Cadence
  Wait for the second update cycle (about 5.5 minutes).
#>
param([switch]$Cadence)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Set-Location $root

$failures = New-Object System.Collections.Generic.List[string]
function Check([bool]$ok, [string]$msg) {
    if ($ok) { Write-Host "  PASS  $msg" } else { Write-Host "  FAIL  $msg" -ForegroundColor Red; $script:failures.Add($msg) }
}

$secretPattern = 'sk-ant-|ya29\.|1//0|eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.'
$knownStates = 'ok', 'stale', 'waiting', 'token_expired', 'login_required', 'error'
$dataDir = Join-Path $env:APPDATA 'ai-usage-panel'
$cacheFile = Join-Path $dataDir 'cache.json'
$settingsFile = Join-Path $dataDir 'settings.json'
$exe = Join-Path $root 'target\debug\ai-usage-panel.exe'

Write-Host '[1] build'
cargo build -q -p usage-core --bin probe
cargo build -q -p ai-usage-panel
Check (Test-Path $exe) 'desktop app builds'

Write-Host '[2] probe (read-only)'
$json = (cargo run -q -p usage-core --bin probe -- --json --no-refresh) -join "`n"
Check ($json -notmatch $secretPattern) 'probe output contains no token-like text'
$accounts = $json | ConvertFrom-Json
Check ($accounts.Count -ge 1) "found $($accounts.Count) account(s)"
foreach ($a in $accounts) {
    Check ($knownStates -contains $a.state) "$($a.provider) $($a.label): state '$($a.state)' is a known state"
    if ($a.state -eq 'ok') {
        Check ($a.windows.Count -ge 1) "$($a.provider) $($a.label): has usage windows"
        $bad = @($a.windows | Where-Object { $_.used_percent -lt 0 -or $_.used_percent -gt 100 })
        Check ($bad.Count -eq 0) "$($a.provider) $($a.label): every percentage is within 0-100"
    }
}

Write-Host '[3] desktop app (read-only mode)'
Get-Process -Name 'ai-usage-panel' -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
$backup = $null
if (Test-Path $settingsFile) { $backup = Get-Content $settingsFile -Raw }
New-Item -ItemType Directory -Force $dataDir | Out-Null
'{"interval_secs":300,"auto_refresh_tokens":false}' | Set-Content $settingsFile -Encoding UTF8
Remove-Item $cacheFile -ErrorAction SilentlyContinue   # a fresh cache would (rightly) skip the first cycle

$launched = Get-Date
$proc = Start-Process -FilePath $exe -WorkingDirectory $root -PassThru
try {
    $cache = $null
    for ($i = 0; $i -lt 90; $i++) {
        Start-Sleep -Seconds 1
        if (Test-Path $cacheFile) {
            try { $c = Get-Content $cacheFile -Raw | ConvertFrom-Json } catch { continue }
            $finished = $c.accounts.Count -gt 0 -and -not ($c.accounts | Where-Object { $_.state -eq 'loading' })
            if ($finished) { $cache = $c; break }
        }
    }
    Check ($null -ne $cache) 'first update cycle finished within 90 s'
    if ($cache) {
        Check ($cache.accounts.Count -eq $accounts.Count) "app shows the same number of accounts as the probe ($($cache.accounts.Count))"
        Check ((Get-Content $cacheFile -Raw) -notmatch $secretPattern) 'cache file contains no token-like text'
        $first = [DateTimeOffset]::FromUnixTimeMilliseconds($cache.cycle_started_ms).LocalDateTime
        Check ($first -ge $launched.AddSeconds(-2)) "cycle started after launch ($($first.ToString('HH:mm:ss')))"
    }

    $p = Get-Process -Id $proc.Id
    Check ($p.MainWindowTitle -eq 'AI 用量面板') 'window is open with the expected title'
    $shot = Join-Path $env:TEMP 'ai-usage-panel-e2e.png'
    & (Join-Path $PSScriptRoot 'shot-window.ps1') -Out $shot | Out-Host
    Check ((Test-Path $shot) -and ((Get-Item $shot).Length -gt 20000)) "screenshot saved: $shot"

    if ($Cadence -and $cache) {
        Write-Host '[4] waiting for the second automatic cycle (about 5 minutes) ...'
        $second = $null
        for ($i = 0; $i -lt 360; $i++) {
            Start-Sleep -Seconds 1
            try { $c = Get-Content $cacheFile -Raw | ConvertFrom-Json } catch { continue }
            if ($c.cycle_started_ms -ne $cache.cycle_started_ms) { $second = $c; break }
        }
        Check ($null -ne $second) 'a second cycle started by itself'
        if ($second) {
            $delta = ($second.cycle_started_ms - $cache.cycle_started_ms) / 1000
            Check ($delta -ge 295 -and $delta -le 330) ("cycles are {0:N0} s apart (expected about 300)" -f $delta)
        }
    }
}
finally {
    Get-Process -Name 'ai-usage-panel' -ErrorAction SilentlyContinue | Stop-Process -Force
    if ($null -ne $backup) { $backup | Set-Content $settingsFile -Encoding UTF8 } else { Remove-Item $settingsFile -ErrorAction SilentlyContinue }
}

Write-Host ''
if ($failures.Count -eq 0) { Write-Host 'E2E RESULT: PASS' -ForegroundColor Green; exit 0 }
Write-Host "E2E RESULT: FAIL ($($failures.Count))" -ForegroundColor Red
$failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
exit 1
