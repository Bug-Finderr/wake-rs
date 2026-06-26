#!/usr/bin/env bash
# Linux smoke test for wake-rs. Validates CLI + error-path behavior that does NOT need systemd,
# a real battery, or a TTY (so it runs in a plain container). Usage: smoke_linux.sh [path/to/wake]
set -u
wake="${1:-${CARGO_TARGET_DIR:-target}/release/wake}"
export WAKE_STATE_DIR="$(mktemp -d)"
trap '"$wake" stop >/dev/null 2>&1; rm -rf "$WAKE_STATE_DIR"' EXIT
echo "wake = $wake"

fail=0
run() { # run <expect-exit:ok|fail> <needle> -- <args...>
  local mode="$1" needle="$2"; shift 3
  local out code
  out="$("$wake" "$@" 2>&1)"; code=$?
  local ok=1
  if [ "$mode" = ok ] && [ "$code" -ne 0 ]; then ok=0; fi
  if [ "$mode" = fail ] && [ "$code" -eq 0 ]; then ok=0; fi
  case "$out" in *"$needle"*) ;; *) ok=0;; esac
  case "$out" in *panicked*|*RUST_BACKTRACE*) ok=0;; esac
  if [ "$ok" = 1 ]; then printf 'ok   : wake %s  [exit %s]\n' "$*" "$code"
  else printf 'FAIL : wake %s  [exit %s]\n%s\n' "$*" "$code" "$out"; fail=1; fi
}

run ok   "wake 0.4.1"               -- --version
run ok   "wake --until-charge N"    -- --help
run fail "conflicting triggers"     -- --until-charge 80 --while-pid 1
run fail "unknown flag"             -- --bogus
run fail "invalid duration"         -- 5x
run fail "no usable battery found"  -- --until-charge 80
run ok   "no active session"        -- status
run ok   "no active session"        -- stop
# No systemd in a plain container -> graceful "not found", never a panic.
run fail "systemd-inhibit not found" -- forever

if [ "$fail" = 0 ]; then echo; echo "ALL LINUX SMOKE TESTS PASSED"; else echo; echo "LINUX SMOKE FAILED"; fi
exit "$fail"
