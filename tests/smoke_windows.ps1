#requires -version 5

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

$wake = if ($args.Count -ge 1) { $args[0] } else { Join-Path $PSScriptRoot '..\target\release\wake.exe' }
$wake = (Resolve-Path $wake).Path
$oldPath = $env:Path
$env:WAKE_STATE_DIR = Join-Path $env:TEMP ('wake smoke ' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $env:WAKE_STATE_DIR | Out-Null

function Invoke-Wake {
  param([string[]] $WakeArgs)
  $ErrorActionPreference = 'Continue'
  $out = & $wake @WakeArgs 2>&1
  [pscustomobject]@{ Code = $LASTEXITCODE; Output = ($out -join "`n") }
}

function Assert-Wake {
  param([int] $ExpectedCode, [string] $Needle, [string[]] $WakeArgs)
  $r = Invoke-Wake $WakeArgs
  if ($r.Code -ne $ExpectedCode) {
    throw "expected exit $ExpectedCode from 'wake $($WakeArgs -join ' ')', got $($r.Code):`n$($r.Output)"
  }
  if ($Needle -and -not $r.Output.Contains($Needle)) {
    throw "expected 'wake $($WakeArgs -join ' ')' to contain '$Needle':`n$($r.Output)"
  }
  Write-Host "ok: wake $($WakeArgs -join ' ') [exit $($r.Code)]"
  return $r.Output
}

function Get-WorkerPid {
  $line = Get-Content (Join-Path $env:WAKE_STATE_DIR 'session.properties') | Where-Object { $_ -match '^pid=' } | Select-Object -First 1
  if (-not $line) { throw 'state has no pid field' }
  return [int]$line.Substring(4)
}

function Wait-NoSession {
  $deadline = [DateTime]::UtcNow.AddSeconds(8)
  do {
    $status = Invoke-Wake @('status')
    if ($status.Code -eq 0 -and $status.Output.Contains('no active session')) { return }
    if ($status.Code -ne 0) { throw "status failed while waiting for completion:`n$($status.Output)" }
    Start-Sleep -Milliseconds 200
  } while ([DateTime]::UtcNow -lt $deadline)
  throw "session did not finish:`n$($status.Output)"
}

$failed = $false
try {
  $version = Assert-Wake 0 '' @('--version')
  if ($version -notmatch '^wake \S+$') { throw "unexpected version output: $version" }
  Assert-Wake 2 'conflicting triggers' @('--until-charge', '80', '--while-pid', '1') | Out-Null
  Assert-Wake 2 'does not accept arguments' @('status', 'extra') | Out-Null
  Assert-Wake 2 'unknown flag' @('--bogus') | Out-Null

  $env:Path = Split-Path $wake
  Assert-Wake 0 'session active' @('forever', '--no-display') | Out-Null
  $state = Get-Content (Join-Path $env:WAKE_STATE_DIR 'session.properties') -Raw
  if ($state -notmatch '(?m)^processCommand=.+wake\.exe\r?$') { throw "state does not identify a direct wake.exe worker:`n$state" }
  if ($state -match '(?i)powershell|EncodedCommand|Add-Type') { throw "worker state references PowerShell:`n$state" }
  $workerPid = Get-WorkerPid
  if ((Get-Process -Id $workerPid).ProcessName -ne 'wake') { throw "managed process $workerPid is not wake.exe" }
  Assert-Wake 0 'session active' @('status') | Out-Null
  Assert-Wake 1 'session already active' @('5s') | Out-Null
  Assert-Wake 0 'stopped' @('stop') | Out-Null
  Assert-Wake 0 'no active session' @('status') | Out-Null

  Assert-Wake 0 'session active' @('--while-pid', $PID.ToString()) | Out-Null
  Assert-Wake 0 "pid $(Get-WorkerPid)" @('status') | Out-Null
  Assert-Wake 0 'stopped' @('stop') | Out-Null

  Assert-Wake 0 'session active' @('1s') | Out-Null
  Wait-NoSession
  $statePath = Join-Path $env:WAKE_STATE_DIR 'session.properties'
  if (Test-Path $statePath) { throw 'timed worker left stale state' }
  Write-Host 'ok: timed worker completed naturally'

  Assert-Wake 0 'session active' @('forever') | Out-Null
  Stop-Process -Id (Get-WorkerPid) -Force -Confirm:$false
  Wait-NoSession
  if (Test-Path $statePath) { throw 'stale non-lid state was not removed' }
  Write-Host 'ok: stale non-lid worker reconciled'

  $malformed = [Text.Encoding]::UTF8.GetBytes("evenLid=true`noriginalScheme=broken`n")
  [IO.File]::WriteAllBytes($statePath, $malformed)
  $before = [Convert]::ToBase64String([IO.File]::ReadAllBytes($statePath))
  Assert-Wake 1 'retained byte-for-byte' @('status') | Out-Null
  $after = [Convert]::ToBase64String([IO.File]::ReadAllBytes($statePath))
  if ($before -ne $after) { throw 'malformed state bytes changed during reconciliation' }
  Remove-Item -Force $statePath -Confirm:$false
  Write-Host 'ok: malformed recovery state retained byte-for-byte'

  $resolved = wake --version
  if ($LASTEXITCODE -ne 0 -or $resolved -notmatch '^wake \S+$') { throw "wake does not resolve on restricted PATH: $resolved" }
  Write-Host "ok: wake resolves on restricted PATH [$resolved]"
  Write-Host "`nALL WINDOWS SMOKE TESTS PASSED"
} catch {
  $failed = $true
  Write-Host "`nWINDOWS SMOKE TEST FAILED: $_" -ForegroundColor Red
} finally {
  & $wake stop *> $null
  $env:Path = $oldPath
  Remove-Item -Recurse -Force $env:WAKE_STATE_DIR -ErrorAction SilentlyContinue -Confirm:$false
}
if ($failed) { exit 1 }
