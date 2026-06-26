#requires -version 5
# End-to-end smoke test for wake-rs on Windows. Mirrors the upstream wake-cli CI smoke test plus
# the extra triggers. Usage:  pwsh tests/smoke_windows.ps1 [path\to\wake.exe]
# Exits non-zero on the first failed assertion.

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

$wake = if ($args.Count -ge 1) { $args[0] } else { Join-Path $PSScriptRoot '..\target\release\wake.exe' }
$wake = (Resolve-Path $wake).Path
$env:WAKE_STATE_DIR = Join-Path $env:TEMP ('wake-smoke-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $env:WAKE_STATE_DIR | Out-Null
Write-Host "wake   = $wake"
Write-Host "state  = $env:WAKE_STATE_DIR`n"

function Invoke-Wake {
  param([string[]] $WakeArgs)
  # Lower ErrorActionPreference locally so native stderr (captured via 2>&1) is not turned into a
  # terminating error under Windows PowerShell 5.1 (which lacks $PSNativeCommandUseErrorActionPreference).
  $ErrorActionPreference = 'Continue'
  $out = & $wake @WakeArgs 2>&1
  $code = $LASTEXITCODE
  [pscustomobject]@{ Code = $code; Output = ($out -join "`n") }
}

function Assert-Success {
  param([string[]] $WakeArgs)
  $r = Invoke-Wake $WakeArgs
  if ($r.Code -ne 0) { throw "expected success from 'wake $($WakeArgs -join ' ')' (got $($r.Code)):`n$($r.Output)" }
  Write-Host "ok   : wake $($WakeArgs -join ' ')  [exit 0]"
  return $r.Output
}

function Assert-Failure {
  param([string[]] $WakeArgs, [int] $ExpectedCode = -1)
  $r = Invoke-Wake $WakeArgs
  if ($r.Code -eq 0) { throw "expected failure from 'wake $($WakeArgs -join ' ')':`n$($r.Output)" }
  if ($ExpectedCode -ge 0 -and $r.Code -ne $ExpectedCode) {
    throw "expected exit $ExpectedCode from 'wake $($WakeArgs -join ' ')', got $($r.Code):`n$($r.Output)"
  }
  Write-Host "ok   : wake $($WakeArgs -join ' ')  [exit $($r.Code)]"
  return $r
}

function Assert-Contains {
  param([string] $Text, [string] $Needle)
  if (-not $Text.Contains($Needle)) { throw "expected output to contain '$Needle', got:`n$Text" }
}

$failed = $false
try {
  Assert-Contains (Assert-Success @('--version')) 'wake '
  Assert-Contains (Assert-Success @('version'))   'wake 0.4.1'
  Assert-Contains (Assert-Success @('--help'))    'wake --until-charge N'

  $conflict = Assert-Failure @('--until-charge', '80', '--while-pid', '1') -ExpectedCode 2
  Assert-Contains $conflict.Output 'conflicting triggers'

  Assert-Contains (Assert-Failure @('--bogus') -ExpectedCode 2).Output 'unknown flag'
  Assert-Contains (Assert-Failure @('5x') -ExpectedCode 2).Output 'invalid duration'

  # Battery path must fail gracefully (no battery -> "no usable battery found"; battery present but
  # an unreachable/neutral target -> a clean usage error). Either way: non-zero, "wake:", no panic.
  $battery = Assert-Failure @('--until-charge', '80')
  Assert-Contains $battery.Output 'wake:'
  if ($battery.Output -match 'panicked|RUST_BACKTRACE|Exception') { throw "battery failure leaked an internal error:`n$($battery.Output)" }

  # Indefinite lifecycle
  Assert-Contains (Assert-Success @('forever', '--no-display')) 'session active'
  Assert-Contains (Assert-Success @('status')) 'session active'
  Assert-Contains (Assert-Failure @('5s')).Output 'session already active'
  Assert-Contains (Assert-Success @('stop')) 'stopped'
  Assert-Contains (Assert-Success @('status')) 'no active session'

  # Timed lifecycle
  Assert-Contains (Assert-Success @('5s')) 'session active'
  Assert-Contains (Assert-Success @('status')) 'session active'
  Assert-Contains (Assert-Success @('stop')) 'stopped'

  # Resolves as `wake` when its directory is on PATH (guards the shipped binary name).
  $dir = Split-Path $wake
  $env:Path = "$dir;$env:Path"
  $v = wake --version
  if ($v -notmatch 'wake ') { throw "release binary does not resolve as 'wake' on PATH: $v" }
  Write-Host "ok   : wake resolves on PATH  [$v]"

  Write-Host "`nALL SMOKE TESTS PASSED"
} catch {
  $failed = $true
  Write-Host "`nSMOKE TEST FAILED: $_" -ForegroundColor Red
} finally {
  & $wake stop *> $null
  Remove-Item -Recurse -Force $env:WAKE_STATE_DIR -ErrorAction SilentlyContinue
}
if ($failed) { exit 1 }
