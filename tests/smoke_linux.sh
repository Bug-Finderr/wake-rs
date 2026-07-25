#!/usr/bin/env bash
set -u

wake="${1:-${CARGO_TARGET_DIR:-target}/release/wake}"
tmp="$(mktemp -d)"
export WAKE_STATE_DIR="$tmp/state"
mkdir -p "$WAKE_STATE_DIR"

cleanup() {
  "$wake" stop >/dev/null 2>&1 || true
  if [[ -n "${WAKE_SHIM_PIDS:-}" && -f "$WAKE_SHIM_PIDS" ]]; then
    while IFS= read -r pid; do
      [[ "$pid" =~ ^[1-9][0-9]*$ ]] && kill "$pid" 2>/dev/null || true
    done <"$WAKE_SHIM_PIDS"
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

if ! command -v systemd-inhibit >/dev/null 2>&1 ||
  ! systemd-inhibit --what=sleep --who=wake-smoke --why=probe true >/dev/null 2>&1; then
  mkdir "$tmp/bin"
  cat >"$tmp/bin/systemd-inhibit" <<'SH'
#!/bin/sh
while [ "$#" -gt 0 ]; do
  case "$1" in
    --*) shift ;;
    *) exec "$@" ;;
  esac
done
exit 64
SH
  chmod +x "$tmp/bin/systemd-inhibit"
  export PATH="$tmp/bin:$PATH"
fi

failed=0
last_output=""
fail() {
  printf 'FAIL: %s\n' "$1"
  failed=1
}

expect() {
  local expected="$1" needle="$2"
  shift 2
  local output code
  output="$("$wake" "$@" 2>&1)"
  code=$?
  if [[ "$code" -ne "$expected" || "$output" != *"$needle"* ]]; then
    printf 'FAIL: wake %s [expected %s, got %s]\n%s\n' "$*" "$expected" "$code" "$output"
    failed=1
  else
    printf 'ok: wake %s [exit %s]\n' "$*" "$code"
  fi
}

wait_inactive() {
  local label="$1" output code
  for ((attempt = 0; attempt < 80; attempt++)); do
    output="$("$wake" status 2>&1)"
    code=$?
    if [[ "$code" -ne 0 ]]; then
      printf 'FAIL: %s status [expected 0, got %s]\n%s\n' "$label" "$code" "$output"
      failed=1
      return
    fi
    if [[ "$output" == *"no active session"* ]]; then
      last_output="$output"
      printf 'ok: %s\n' "$label"
      return
    fi
    sleep 0.1
  done
  last_output="$output"
  printf 'FAIL: %s did not become inactive\n%s\n' "$label" "$output"
  failed=1
}

expect_state_value() {
  local expected="$1" line found=0
  if [[ -f "$WAKE_STATE_DIR/session.properties" ]]; then
    while IFS= read -r line; do
      [[ "$line" == "$expected" ]] && found=1
    done <"$WAKE_STATE_DIR/session.properties"
  fi
  [[ "$found" -eq 1 ]] || fail "state does not contain $expected"
}

expect_no_state() {
  local label="$1"
  [[ ! -e "$WAKE_STATE_DIR/session.properties" ]] || fail "$label left state"
}

expect_scopes() {
  local label="$1" expected="$2" actual=""
  [[ -f "$WAKE_INHIBIT_LOG" ]] && actual="$(<"$WAKE_INHIBIT_LOG")"
  if [[ "$actual" != "$expected" ]]; then
    printf 'FAIL: %s scopes\nexpected:\n%s\nactual:\n%s\n' "$label" "$expected" "$actual"
    failed=1
  else
    printf 'ok: %s scopes\n' "$label"
  fi
}

wait_dead() {
  local pid="$1" label="$2"
  for ((attempt = 0; attempt < 80; attempt++)); do
    if ! kill -0 "$pid" 2>/dev/null; then
      printf 'ok: %s process exited\n' "$label"
      return
    fi
    sleep 0.1
  done
  fail "$label process $pid is still running"
}

expect 0 "wake " --version
expect 2 "conflicting triggers" 1s --while-pid "$$"
expect 2 "unknown flag" --bogus
expect 2 "does not accept arguments" status extra
expect 0 "no active session" status

expect 0 "session active" forever
expect 0 "session active" status
expect 1 "session already active" 5s
expect 0 "stopped" stop
expect 0 "no active session" status

expect 0 "session active" 1s
wait_inactive "short session expired"
expect_no_state "expired session"

policy_bin="$tmp/policy-bin"
mkdir "$policy_bin"
export WAKE_INHIBIT_LOG="$tmp/inhibit-scopes"
export WAKE_SHIM_PIDS="$tmp/inhibit-pids"
cat >"$policy_bin/systemd-inhibit" <<'SH'
#!/bin/sh
what=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --what=*) what=${1#--what=} ;;
    --) shift; break ;;
    --*) ;;
    *) break ;;
  esac
  shift
done
[ -n "$what" ] || exit 64
printf '%s\n' "$what" >>"$WAKE_INHIBIT_LOG"
case "${WAKE_INHIBIT_REFUSE_LID:-0}:$what" in
  1:*handle-lid-switch*) exit 1 ;;
esac
[ "$#" -gt 0 ] || exit 64
if [ "${1##*/}" != true ]; then
  printf '%s\n' "$$" >>"$WAKE_SHIM_PIDS"
fi
exec "$@"
SH
chmod +x "$policy_bin/systemd-inhibit"
export PATH="$policy_bin:$PATH"

strict_lifecycle() {
  local label="$1" scope="$2" worker_pid="" key value
  shift 2
  : >"$WAKE_INHIBIT_LOG"
  : >"$WAKE_SHIM_PIDS"
  unset WAKE_INHIBIT_REFUSE_LID
  expect 0 "session active" forever --even-lid "$@"
  expect_scopes "$label" "$scope"$'\n'"$scope"
  expect_state_value "evenLid=true"
  expect 0 "session active" status
  expect 0 "even lid  : logind inhibitor active" status
  while IFS='=' read -r key value; do
    [[ "$key" == pid ]] && worker_pid="$value"
  done <"$WAKE_STATE_DIR/session.properties"
  expect 0 "stopped" stop
  expect_no_state "$label stop"
  if [[ "$worker_pid" =~ ^[1-9][0-9]*$ ]]; then
    wait_dead "$worker_pid" "$label"
  else
    fail "$label state did not contain a valid pid"
  fi
}

strict_lifecycle "explicit display" "idle:sleep:handle-lid-switch"
strict_lifecycle "explicit system-only" "sleep:handle-lid-switch" --no-display

strict_refusal() {
  local label="$1" scope="$2"
  shift 2
  : >"$WAKE_INHIBIT_LOG"
  : >"$WAKE_SHIM_PIDS"
  export WAKE_INHIBIT_REFUSE_LID=1
  expect 1 "--even-lid requires systemd inhibitor scope $scope" forever --even-lid "$@"
  expect_scopes "$label" "$scope"
  expect_no_state "$label"
  [[ ! -s "$WAKE_SHIM_PIDS" ]] || fail "$label launched a payload"
}

strict_refusal "explicit display refusal" "idle:sleep:handle-lid-switch"
strict_refusal "explicit system-only refusal" "sleep:handle-lid-switch" --no-display

: >"$WAKE_INHIBIT_LOG"
: >"$WAKE_SHIM_PIDS"
expect 0 "session active" forever
expect_scopes "ordinary fallback" $'idle:sleep:handle-lid-switch\nidle:sleep\nidle:sleep'
expect_state_value "evenLid=false"
expect 0 "stopped" stop
expect_no_state "ordinary fallback stop"

: >"$WAKE_INHIBIT_LOG"
: >"$WAKE_SHIM_PIDS"
unset WAKE_INHIBIT_REFUSE_LID
expect 0 "session active" forever --even-lid
expect_scopes "strict stale session" $'idle:sleep:handle-lid-switch\nidle:sleep:handle-lid-switch'
worker_pid=""
while IFS='=' read -r key value; do
  [[ "$key" == pid ]] && worker_pid="$value"
done <"$WAKE_STATE_DIR/session.properties"
if [[ ! "$worker_pid" =~ ^[1-9][0-9]*$ ]] || ! kill -KILL "$worker_pid" 2>/dev/null; then
  fail "could not kill the managed strict process"
else
  wait_inactive "strict stale session cleaned"
  expect_no_state "strict stale session"
  if [[ "$last_output" == *"restore"* || "$last_output" == *"recovery"* || "$last_output" == *"run 'wake stop'"* ]]; then
    fail "strict stale cleanup emitted restoration guidance: $last_output"
  fi
fi

if [[ "$failed" -eq 0 ]]; then
  echo "ALL LINUX SMOKE TESTS PASSED"
else
  echo "LINUX SMOKE TEST FAILED"
fi
exit "$failed"
