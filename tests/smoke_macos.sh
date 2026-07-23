#!/usr/bin/env bash
set -u

wake="${1:-${CARGO_TARGET_DIR:-target}/release/wake}"
tmp="$(mktemp -d)"
export WAKE_STATE_DIR="$tmp/state"
mkdir -p "$WAKE_STATE_DIR"

cleanup() {
  "$wake" stop >/dev/null 2>&1 || true
  rm -rf "$tmp"
}
trap cleanup EXIT

if [[ ! -x /usr/bin/caffeinate ]]; then
  echo "FAIL: /usr/bin/caffeinate is unavailable"
  exit 1
fi

failed=0
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
  local output code
  for ((attempt = 0; attempt < 80; attempt++)); do
    output="$("$wake" status 2>&1)"
    code=$?
    if [[ "$code" -ne 0 ]]; then
      printf 'FAIL: expiry status [expected 0, got %s]\n%s\n' "$code" "$output"
      failed=1
      return
    fi
    if [[ "$output" == *"no active session"* ]]; then
      echo "ok: short session expired"
      return
    fi
    sleep 0.1
  done
  printf 'FAIL: short session did not expire\n%s\n' "$output"
  failed=1
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
wait_inactive
if [[ -e "$WAKE_STATE_DIR/session.properties" ]]; then
  echo "FAIL: expired session left state"
  failed=1
fi

if [[ "$failed" -eq 0 ]]; then
  echo "ALL MACOS SMOKE TESTS PASSED"
else
  echo "MACOS SMOKE TEST FAILED"
fi
exit "$failed"
