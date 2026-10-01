<#
.SYNOPSIS
  Drives the REAL desktop window through Windows UI Automation (the same accessibility tree a
  screen reader sees): opens the settings, toggles "auto-refresh tokens", checks that switching
  it ON starts an update cycle at once, and that the refresh button shows its cool-down.
  Restores the settings file afterwards. Needs no extra tools and changes no system setting.

  The update cycles it triggers are ordinary usage queries; shared logins are never refreshed.
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
Set-Location $root
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes

$failures = New-Object System.Collections.Generic.List[string]
function Check([bool]$ok, [string]$msg) {
    if ($ok) { Write-Host "  PASS  $msg" } else { Write-Host "  FAIL  $msg" -ForegroundColor Red; $script:failures.Add($msg) }
}

$AE = [System.Windows.Automation.AutomationElement]
$Cond = [System.Windows.Automation.PropertyCondition]
$dataDir = Join-Path $env:APPDATA 'ai-usage-panel'
$cacheFile = Join-Path $dataDir 'cache.json'
$settingsFile = Join-Path $dataDir 'settings.json'
$exe = Join-Path $root 'target\debug\ai-usage-panel.exe'

function Get-Window { $AE::RootElement.FindFirst('Children', (New-Object $Cond($AE::NameProperty, 'AI 用量面板'))) }
function All-Elements($w) { $w.FindAll('Descendants', [System.Windows.Automation.Condition]::TrueCondition) }
function Find-By($w, [string]$type, [string]$nameLike) {
    foreach ($e in (All-Elements $w)) {
        if ($e.Current.ControlType.ProgrammaticName -eq "ControlType.$type" -and $e.Current.Name -like $nameLike) { return $e }
    }
    $null
}
function Cycle-Start { try { (Get-Content $cacheFile -Raw | ConvertFrom-Json).cycle_started_ms } catch { $null } }
# A button with aria-expanded is exposed as "expandable"; a plain one as "invokable".
function Press($el) {
    try { $el.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke(); return } catch { }
    try { $el.GetCurrentPattern([System.Windows.Automation.ExpandCollapsePattern]::Pattern).Expand(); return } catch { }
    throw "cannot press '$($el.Current.Name)'"
}

cargo build -q -p ai-usage-panel
Get-Process -Name 'ai-usage-panel' -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
$backup = $null
if (Test-Path $settingsFile) { $backup = Get-Content $settingsFile -Raw }
Remove-Item $cacheFile -ErrorAction SilentlyContinue   # force a first cycle

try {
    Start-Process -FilePath $exe -WorkingDirectory $root | Out-Null
    $win = $null
    for ($i = 0; $i -lt 40 -and -not $win; $i++) { Start-Sleep -Seconds 1; $win = Get-Window }
    Check ($null -ne $win) 'the window opens'
    if (-not $win) { throw 'no window' }

    # first cycle done?
    for ($i = 0; $i -lt 90; $i++) {
        Start-Sleep -Seconds 1
        try { $c = Get-Content $cacheFile -Raw | ConvertFrom-Json; if ($c.accounts.Count -gt 0 -and -not ($c.accounts | Where-Object { $_.state -eq 'loading' })) { break } } catch { }
    }
    $null = All-Elements $win; Start-Sleep -Seconds 2   # let Chromium build its accessibility tree

    Write-Host '[1] what a screen reader sees'
    $bars = @(All-Elements $win | Where-Object { $_.Current.ControlType.ProgrammaticName -eq 'ControlType.ProgressBar' })
    Check ($bars.Count -ge 8) "progress bars are exposed ($($bars.Count))"
    $named = @($bars | Where-Object { $_.Current.Name -match '用量$' })
    Check ($named.Count -eq $bars.Count) 'every progress bar has a readable name ending in 用量'
    $badValue = 0
    foreach ($b in $bars) {
        try { $v = $b.GetCurrentPattern([System.Windows.Automation.RangeValuePattern]::Pattern).Current.Value; if ($v -lt 0 -or $v -gt 100) { $badValue++ } } catch { $badValue++ }
    }
    Check ($badValue -eq 0) 'every progress bar reports a value within 0-100'
    $btnRefresh = Find-By $win 'Button' '*更新*'
    $btnSettings = Find-By $win 'Button' '設定'
    Check ($null -ne $btnRefresh -and $null -ne $btnSettings) 'the refresh and settings buttons have names'

    Write-Host '[2] settings panel'
    Press $btnSettings
    Start-Sleep -Seconds 1
    $check = Find-By $win 'CheckBox' '*自動更新過期的登入 token*'
    Check ($null -ne $check) 'the settings panel opens and shows the token checkbox'
    $combo = Find-By $win 'ComboBox' '*'
    Check ($null -ne $combo) 'the update-interval list is there'
    if (-not $check) { throw 'checkbox missing' }
    $toggle = $check.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern)

    Write-Host '[3] switching token refresh OFF then ON'
    if ($toggle.Current.ToggleState -eq 'Off') { $toggle.Toggle(); Start-Sleep -Seconds 1 }   # make sure it is ON first
    $toggle.Toggle()                                                                          # ON -> OFF
    Start-Sleep -Seconds 2
    $s = Get-Content $settingsFile -Raw | ConvertFrom-Json
    Check ($s.auto_refresh_tokens -eq $false) 'OFF is saved to settings.json'
    Check ($s.interval_secs -eq 300) 'the interval stays at 5 minutes (300 s)'
    $before = Cycle-Start
    $toggle.Toggle()                                                                          # OFF -> ON
    $started = $false
    for ($i = 0; $i -lt 40; $i++) { Start-Sleep -Milliseconds 500; if ((Cycle-Start) -ne $before) { $started = $true; break } }
    Check $started 'switching ON starts an update cycle right away (no 5-minute wait)'
    $s = Get-Content $settingsFile -Raw | ConvertFrom-Json
    Check ($s.auto_refresh_tokens -eq $true) 'ON is saved to settings.json'

    Write-Host '[4] refresh button cool-down'
    Start-Sleep -Seconds 14   # let the cycle finish (the Gemini CLI call takes a few seconds)
    $btnRefresh = Find-By $win 'Button' '*更新*'
    $name = $btnRefresh.Current.Name
    Check ((-not $btnRefresh.Current.IsEnabled) -and $name -match '秒後可更新') "refresh button is disabled during the cool-down [$name]"
}
finally {
    Get-Process -Name 'ai-usage-panel' -ErrorAction SilentlyContinue | Stop-Process -Force
    if ($null -ne $backup) { $backup | Set-Content $settingsFile -Encoding UTF8 } else { Remove-Item $settingsFile -ErrorAction SilentlyContinue }
}

Write-Host ''
if ($failures.Count -eq 0) { Write-Host 'UI E2E RESULT: PASS' -ForegroundColor Green; exit 0 }
Write-Host "UI E2E RESULT: FAIL ($($failures.Count))" -ForegroundColor Red
$failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
exit 1
