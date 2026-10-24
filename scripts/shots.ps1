<#
.SYNOPSIS
  Runs vox.exe's in-app screenshot harness (crates/vox-app/src/shots.rs; ARCHITECTURE.md §9, P-APP-INT).

.DESCRIPTION
  Deletes <OutDir>\report.json, starts vox.exe with VOX_SHOTS_DIR / VOX_DEMO / VOX_MOCK_TTS set (demo profile
  %APPDATA%\Vox-Demo, mock speech, no hotkeys, hook, bridge or AUMID registration), waits for it to exit, kills it
  after -TimeoutSec (only the process this script started; any other Vox.exe is left alone), then prints the summary
  from report.json.

  Shots mode re-seeds the demo history as of now (app.rs), so Home and Analytics always show fresh demo data.

  Exit codes: 0 ok · 1 failures, console errors, timeout or no report · 2 the exe is missing.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\shots.ps1
  powershell -ExecutionPolicy Bypass -File scripts\shots.ps1 -Exe target\release\vox.exe -OutDir $env:TEMP\vox-shots
#>
[CmdletBinding()]
param(
    # vox.exe to run (default: the debug build of this repository).
    [string]$Exe = '',
    # Where the PNGs and report.json go (default: shots\app of this repository).
    [string]$OutDir = '',
    # Hard limit for the whole run (the harness's own watchdog fires at 100 s).
    [int]$TimeoutSec = 120
)

$ErrorActionPreference = 'Stop'
# (Windows PowerShell 5.1 leaves $PSScriptRoot empty inside param() defaults.)
$repo = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
if (-not $Exe) { $Exe = Join-Path $repo 'target\debug\vox.exe' }
if (-not $OutDir) { $OutDir = Join-Path $repo 'shots\app' }

if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) {
    Write-Host "shots: vox.exe not found at $Exe (build it with: cargo build --workspace)"
    exit 2
}
$Exe = (Resolve-Path -LiteralPath $Exe).Path
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path
$report = Join-Path $OutDir 'report.json'
if (Test-Path -LiteralPath $report) { Remove-Item -LiteralPath $report -Force }

# The child inherits these; restore the caller's environment afterwards.
$saved = @{}
foreach ($name in 'VOX_SHOTS_DIR', 'VOX_DEMO', 'VOX_MOCK_TTS') { $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$env:VOX_SHOTS_DIR = $OutDir
$env:VOX_DEMO = '1'
$env:VOX_MOCK_TTS = '1'

$started = Get-Date
$timedOut = $false
try {
    $proc = Start-Process -FilePath $Exe -WorkingDirectory (Split-Path -Parent $Exe) -PassThru
    if (-not $proc.WaitForExit($TimeoutSec * 1000)) {
        $timedOut = $true
        Write-Host "shots: no exit after $TimeoutSec s; stopping pid $($proc.Id)"
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        $proc.WaitForExit(5000) | Out-Null
    }
    $exitCode = if ($timedOut) { -1 } else { $proc.ExitCode }
} finally {
    foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
}
$elapsed = [math]::Round(((Get-Date) - $started).TotalSeconds, 1)

if (-not (Test-Path -LiteralPath $report)) {
    Write-Host "shots: no report.json in $OutDir (exit code $exitCode, $elapsed s)"
    exit 1
}

$r = Get-Content -LiteralPath $report -Raw -Encoding UTF8 | ConvertFrom-Json
$shots = @($r.shots)
$failures = @($r.failures)
$errors = @($r.consoleErrors)
$warnings = @($r.consoleWarnings)
$blank = @($shots | Where-Object { $_.blank })

Write-Host ("shots: ok={0} shots={1} blank={2} failures={3} consoleErrors={4} consoleWarnings={5} exit={6} ({7} s, harness {8} ms)" -f `
        $r.ok, $shots.Count, $blank.Count, $failures.Count, $errors.Count, $warnings.Count, $exitCode, $elapsed, $r.durationMs)
foreach ($s in $shots) { Write-Host ("  {0,-28} {1}x{2}{3}" -f $s.file, $s.width, $s.height, $(if ($s.blank) { '  BLANK' } else { '' })) }
foreach ($f in $failures) { Write-Host "  failure: $f" }
foreach ($e in $errors) { Write-Host ("  error [{0}] {1}" -f $e.source, $e.message) }
foreach ($w in $warnings) { Write-Host ("  warning [{0}] {1}" -f $w.source, $w.message) }

if ($timedOut -or -not $r.ok -or $failures.Count -gt 0 -or $errors.Count -gt 0 -or $blank.Count -gt 0) { exit 1 }
exit 0
