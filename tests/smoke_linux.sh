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

run ok   "wake 0.1.0"               -- --version
run ok   "wake --until-charge N"    -- --help
run fail "conflicting triggers"     -- --until-charge 80 --while-pid 1
run fail "unknown flag"             -- --bogus
run fail "invalid duration"         -- 5x
run fail "no usable battery found"  -- --until-charge 80
run ok   "no active session"        -- status
run ok   "no active session"        -- stop

# `forever` depends on whether this host can take systemd inhibitor locks: a plain container has no
# systemd-inhibit; CI runners have it but polkit may deny. Accept a started session OR any graceful
# "wake:" error; only a panic (or empty output) is a failure. Always clean up afterwards.
fout="$("$wake" forever 2>&1)"; fcode=$?
"$wake" stop >/dev/null 2>&1
case "$fout" in
  *panicked*|*RUST_BACKTRACE*) printf 'FAIL : wake forever panicked [exit %s]\n%s\n' "$fcode" "$fout"; fail=1 ;;
  *"session active"*|*"wake:"*) printf 'ok   : wake forever  [exit %s, graceful]\n' "$fcode" ;;
  *) printf 'FAIL : wake forever - unrecognized output [exit %s]\n%s\n' "$fcode" "$fout"; fail=1 ;;
esac

if [ "$fail" = 0 ]; then echo; echo "ALL LINUX SMOKE TESTS PASSED"; else echo; echo "LINUX SMOKE FAILED"; fi
exit "$fail"
