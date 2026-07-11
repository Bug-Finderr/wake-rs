#requires -version 5

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

$wake = if ($args.Count -ge 1) { $args[0] } else { Join-Path $PSScriptRoot '..\target\release\wake.exe' }
$wake = (Resolve-Path $wake).Path
$env:WAKE_STATE_DIR = Join-Path $env:TEMP ('wake-smoke-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $env:WAKE_STATE_DIR | Out-Null

function Test-Wake {
  param([int] $ExpectedCode, [string] $Needle, [string[]] $WakeArgs)

  # PowerShell 5.1 can promote captured native stderr to a terminating error.
  $ErrorActionPreference = 'Continue'
  $output = (& $wake @WakeArgs 2>&1) -join "`n"
  $code = $LASTEXITCODE
  if ($code -ne $ExpectedCode) {
    throw "expected exit $ExpectedCode from 'wake $($WakeArgs -join ' ')', got $code`n$output"
  }
  if (-not $output.Contains($Needle)) {
    throw "expected 'wake $($WakeArgs -join ' ')' to contain '$Needle'`n$output"
  }
  if ($output -match 'panicked|RUST_BACKTRACE|Exception') {
    throw "internal error from 'wake $($WakeArgs -join ' ')'`n$output"
  }
  Write-Host "ok   : wake $($WakeArgs -join ' ')  [exit $code]"
}

$failed = $false
try {
  Test-Wake 0 'wake '                  @('--version')
  Test-Wake 0 'wake '                  @('version')
  Test-Wake 0 'wake --until-charge N' @('--help')
  Test-Wake 0 'wake --until-charge N' @('forever', '--help')
  Test-Wake 2 'conflicting triggers'  @('--until-charge', '80', '--while-pid', '1')
  Test-Wake 2 'unknown flag'           @('--bogus')
  Test-Wake 2 'invalid duration'       @('5x')

  $ErrorActionPreference = 'Continue'
  $batteryOutput = (& $wake --until-charge 80 2>&1) -join "`n"
  $batteryCode = $LASTEXITCODE
  $ErrorActionPreference = 'Stop'
  $batteryExpected = switch ($batteryCode) {
    0 { $batteryOutput.Contains('wake: session active') -or $batteryOutput.Contains('wake: battery already at') }
    1 { $batteryOutput.Contains('wake: no usable battery found') -or $batteryOutput.Contains('wake: could not read battery status') }
    2 { $batteryOutput.Contains('wake: --until-charge 80 is unreachable') -or $batteryOutput.Contains('wake: cannot determine battery charging direction') }
    default { $false }
  }
  if (-not $batteryExpected -or $batteryOutput -match 'panicked|RUST_BACKTRACE|Exception') {
    throw "unexpected battery result from 'wake --until-charge 80', exit $batteryCode`n$batteryOutput"
  }
  if ($batteryCode -eq 0) { & $wake stop *> $null }
  Write-Host "ok   : wake --until-charge 80  [exit $batteryCode, graceful]"

  Test-Wake 0 'session active'         @('forever', '--no-display')
  Test-Wake 0 'session active'         @('status')
  Test-Wake 1 'session already active' @('30s')
  Test-Wake 0 'stopped'                @('stop')
  Test-Wake 0 'no active session'      @('status')

  Test-Wake 0 'session active'         @('30s')
  Test-Wake 0 'session active'         @('status')
  Test-Wake 0 'stopped'                @('stop')

  $env:Path = "$(Split-Path $wake);$env:Path"
  $version = (& wake --version 2>&1) -join "`n"
  if ($LASTEXITCODE -ne 0 -or $version -notmatch '^wake ') {
    throw "release binary does not resolve as 'wake' on PATH: $version"
  }

  Write-Host "`nALL WINDOWS SMOKE TESTS PASSED"
} catch {
  $failed = $true
  Write-Host "`nWINDOWS SMOKE FAILED: $_" -ForegroundColor Red
} finally {
  & $wake stop *> $null
  Remove-Item -Recurse -Force $env:WAKE_STATE_DIR -ErrorAction SilentlyContinue
}
if ($failed) { exit 1 }
