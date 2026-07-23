#requires -version 5
# Native Windows lifecycle smoke test. It never enables --even-lid.

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

$wake = if ($args.Count -ge 1) { $args[0] } else { Join-Path $PSScriptRoot '..\target\release\wake.exe' }
$wake = (Resolve-Path $wake).Path
$oldPath = $env:Path
$env:WAKE_STATE_DIR = Join-Path $env:TEMP ('wake smoke ' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $env:WAKE_STATE_DIR | Out-Null
Write-Host "wake   = $wake"
Write-Host "state  = $env:WAKE_STATE_DIR`n"

function Invoke-Wake {
  param([string[]] $WakeArgs)
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

function Get-WorkerPid {
  $line = Get-Content (Join-Path $env:WAKE_STATE_DIR 'session.properties') | Where-Object { $_ -match '^pid=' } | Select-Object -First 1
  if (-not $line) { throw 'state has no pid field' }
  return [int]($line.Substring(4))
}

function Wait-NoSession {
  param([int] $Seconds = 6)
  $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
  do {
    $status = Invoke-Wake @('status')
    if ($status.Code -eq 0 -and $status.Output.Contains('no active session')) { return }
    Start-Sleep -Milliseconds 200
  } while ([DateTime]::UtcNow -lt $deadline)
  throw "session did not finish naturally:`n$($status.Output)"
}

$failed = $false
try {
  Assert-Contains (Assert-Success @('--version')) 'wake 0.1.1'
  Assert-Contains (Assert-Success @('--help')) 'native SetThreadExecutionState'

  # Strict public and hidden parsing.
  Assert-Contains (Assert-Failure @('--until-charge', '80', '--while-pid', '1') -ExpectedCode 2).Output 'conflicting triggers'
  Assert-Contains (Assert-Failure @('--no-display', '--no-display') -ExpectedCode 2).Output 'duplicate flag'
  Assert-Contains (Assert-Failure @('status', 'extra') -ExpectedCode 2).Output 'does not accept arguments'
  Assert-Contains (Assert-Failure @('--bogus') -ExpectedCode 2).Output 'unknown flag'
  Assert-Contains (Assert-Failure @('__worker_windows__') -ExpectedCode 1).Output 'expects exactly'
  Assert-Contains (Assert-Failure @('__guard_windows__') -ExpectedCode 1).Output 'expects six immutable'
  Assert-Contains (Assert-Failure @('--until-charge', '101') -ExpectedCode 2).Output 'must be 1-100'

  # Prove the lifecycle does not need powershell.exe on PATH. The state directory also contains spaces.
  $env:Path = Split-Path $wake
  Assert-Contains (Assert-Success @('forever', '--no-display')) 'session active'
  $state = Get-Content (Join-Path $env:WAKE_STATE_DIR 'session.properties') -Raw
  Assert-Contains $state 'version=2'
  Assert-Contains $state 'processCommand='
  if ($state -match '(?i)powershell|EncodedCommand|Add-Type') { throw "worker state references PowerShell:`n$state" }
  $workerPid = Get-WorkerPid
  if ((Get-Process -Id $workerPid).ProcessName -ne 'wake') { throw "managed process $workerPid is not wake.exe" }
  Assert-Contains (Assert-Success @('status')) 'session active'
  Assert-Contains (Assert-Failure @('5s')).Output 'session already active'
  Assert-Contains (Assert-Success @('stop')) 'stopped'
  Assert-Contains (Assert-Success @('status')) 'no active session'

  # Timed natural completion removes its own state.
  Assert-Contains (Assert-Success @('1s')) 'session active'
  Wait-NoSession
  if (Test-Path (Join-Path $env:WAKE_STATE_DIR 'session.properties')) { throw 'timed worker left stale state' }
  Write-Host 'ok   : timed worker completed naturally'

  # PID lifetime uses the same direct Rust worker.
  Assert-Contains (Assert-Success @('--while-pid', $PID.ToString())) 'session active'
  Assert-Contains (Assert-Success @('status')) "pid $(Get-WorkerPid)"
  Assert-Contains (Assert-Success @('stop')) 'stopped'

  # A hard-killed non-lid worker is reconciled without touching power settings.
  Assert-Contains (Assert-Success @('forever')) 'session active'
  $stalePid = Get-WorkerPid
  Stop-Process -Id $stalePid -Force -Confirm:$false
  Start-Sleep -Milliseconds 200
  Assert-Contains (Assert-Success @('status')) 'no active session'
  $statePath = Join-Path $env:WAKE_STATE_DIR 'session.properties'
  if (Test-Path $statePath) { throw 'stale non-lid state was not removed' }
  Write-Host 'ok   : stale non-lid worker reconciled'

  # Malformed recovery hints fail closed and remain byte-for-byte unchanged.
  $malformed = [Text.Encoding]::UTF8.GetBytes("evenLid=true`noriginalScheme=broken`n")
  [IO.File]::WriteAllBytes($statePath, $malformed)
  $before = [Convert]::ToBase64String([IO.File]::ReadAllBytes($statePath))
  Assert-Contains (Assert-Failure @('status') -ExpectedCode 1).Output 'retained byte-for-byte'
  $after = [Convert]::ToBase64String([IO.File]::ReadAllBytes($statePath))
  if ($before -ne $after) { throw 'malformed state bytes changed during reconciliation' }
  Remove-Item -Force $statePath -Confirm:$false
  Write-Host 'ok   : malformed recovery state retained byte-for-byte'

  # Relative overrides resolve once to the absolute foreground working directory.
  $absoluteStateDir = $env:WAKE_STATE_DIR
  $relativeStateDir = 'wake-relative-' + [guid]::NewGuid().ToString('N')
  $env:WAKE_STATE_DIR = $relativeStateDir
  Assert-Contains (Assert-Success @('1s')) 'session active'
  $relativeStatePath = Join-Path (Join-Path (Get-Location) $relativeStateDir) 'session.properties'
  if (-not (Test-Path $relativeStatePath)) { throw 'relative state directory was not resolved from the foreground directory' }
  Wait-NoSession
  Remove-Item -Recurse -Force $relativeStateDir -Confirm:$false
  $env:WAKE_STATE_DIR = $absoluteStateDir
  Write-Host 'ok   : relative WAKE_STATE_DIR resolved consistently'

  $v = wake --version
  if ($v -notmatch '^wake ') { throw "release binary does not resolve as 'wake' on PATH: $v" }
  Write-Host "ok   : wake resolves on restricted PATH  [$v]"

  Write-Host "`nALL SMOKE TESTS PASSED"
} catch {
  $failed = $true
  Write-Host "`nSMOKE TEST FAILED: $_" -ForegroundColor Red
} finally {
  & $wake stop *> $null
  $env:Path = $oldPath
  Remove-Item -Recurse -Force $env:WAKE_STATE_DIR -ErrorAction SilentlyContinue -Confirm:$false
}
if ($failed) { exit 1 }
