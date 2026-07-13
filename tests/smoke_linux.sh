#!/usr/bin/env bash
set -u

wake="${1:-${CARGO_TARGET_DIR:-target}/release/wake}"
if ! WAKE_STATE_DIR="$(mktemp -d)" || [[ -z "$WAKE_STATE_DIR" ]]; then
  echo "FAIL : could not create a temporary state directory" >&2
  exit 1
fi
export WAKE_STATE_DIR
renamed_pid=""
cleanup() {
  "$wake" stop >/dev/null 2>&1
  if [[ -n "$renamed_pid" ]]; then
    kill "$renamed_pid" >/dev/null 2>&1
    wait "$renamed_pid" >/dev/null 2>&1
  fi
  rm -rf "$WAKE_STATE_DIR"
}
trap cleanup EXIT

# Isolate lifecycle coverage from the runner's systemd and polkit state.
fake_bin="$WAKE_STATE_DIR/bin"
mkdir -p "$fake_bin" || {
  echo "FAIL : could not create the fake command directory" >&2
  exit 1
}
if ! cat > "$fake_bin/systemd-inhibit" <<'EOF'
#!/usr/bin/env bash
if [[ "${WAKE_TEST_DENY_LID:-0}" == 1 && " $* " == *handle-lid-switch* ]]; then
  exit 1
fi
while [[ "${1:-}" == --* ]]; do
  shift
done
exec "$@"
EOF
then
  echo "FAIL : could not write the fake systemd-inhibit" >&2
  exit 1
fi
chmod +x "$fake_bin/systemd-inhibit" || {
  echo "FAIL : could not make the fake systemd-inhibit executable" >&2
  exit 1
}
export PATH="$fake_bin:$PATH"

fail=0
run() {
  local expected="$1" needle="$2"
  shift 3
  local output code
  output="$("$wake" "$@" 2>&1)"
  code=$?
  if [ "$code" -eq "$expected" ] &&
    [[ "$output" == *"$needle"* ]] &&
    [[ "$output" != *panicked* ]] &&
    [[ "$output" != *RUST_BACKTRACE* ]]
  then
    printf 'ok   : wake %s  [exit %s]\n' "$*" "$code"
  else
    printf 'FAIL : wake %s  [expected %s, got %s]\n%s\n' "$*" "$expected" "$code" "$output"
    fail=1
  fi
}

state_files_are() {
  local expected="$1" actual
  actual="$(find "$WAKE_STATE_DIR" -maxdepth 1 -type f -printf '%f\n' | sort | paste -sd, -)"
  if [[ "$actual" == "$expected" ]]; then
    printf 'ok   : state files are %s\n' "$expected"
  else
    printf 'FAIL : expected state files %s, found %s\n' "$expected" "$actual"
    fail=1
  fi
}

run 0 "wake "                    -- --version
run 0 "--until-charge N"        -- --help
run 0 "--until-charge N"        -- forever --help
run 2 "conflicting triggers"    -- --until-charge 80 --while-pid 1
run 2 "unknown flag"            -- --bogus
run 2 "invalid duration"        -- 5x
run 0 "no active session"       -- status
run 0 "no active session"       -- stop
run 0 "session active"          --
state_files_are "lid-watchdog.lock,state.json,wake.lock"
run 0 "session active"          -- status
run 0 "stopped"                 -- stop
state_files_are "lid-watchdog.lock,wake.lock"

PATH="$fake_bin" run 1 "supervisor exited during startup" -- --even-lid 30s
run 0 "no active session" -- status

export WAKE_TEST_DENY_LID=1
run 0 "session active" -- 30s
run 0 "stopped" -- stop
run 1 "handle-lid-switch inhibitor required" -- --even-lid 30s
run 0 "no active session" -- status
unset WAKE_TEST_DENY_LID
run 0 "session active" -- --even-lid 30s
run 0 "--even-lid request active" -- status
run 0 "stopped" -- stop

battery_output="$("$wake" --until-charge 80 2>&1)"
battery_code=$?
case "$battery_code:$battery_output" in
  0:*"wake: session active"*|\
    0:*"wake: battery already at"*"target 80% reached"*|\
    1:*"wake: no usable battery found"*|\
    2:*"wake: --until-charge 80 is unreachable"*|\
    2:*"wake: cannot determine battery charging direction"*) battery_expected=1 ;;
  *) battery_expected=0 ;;
esac
if [ "$battery_expected" -eq 1 ] &&
  [[ "$battery_output" != *panicked* ]] &&
  [[ "$battery_output" != *RUST_BACKTRACE* ]]; then
  [ "$battery_code" -ne 0 ] || "$wake" stop >/dev/null 2>&1
  printf 'ok   : wake --until-charge 80  [exit %s, graceful]\n' "$battery_code"
else
  printf 'FAIL : wake --until-charge 80  [unexpected result, exit %s]\n%s\n' "$battery_code" "$battery_output"
  fail=1
fi

# Native process identity must remain stable when the observed process changes its display name.
rename_ready="$fake_bin/rename-ready"
rename_go="$fake_bin/rename-go"
rename_done="$fake_bin/rename-done"

wait_for_marker() {
  local marker="$1" description="$2" attempt
  for attempt in {1..100}; do
    [[ -e "$marker" ]] && return 0
    sleep 0.05
  done
  [[ -e "$marker" ]] && return 0
  printf 'FAIL : timed out waiting for %s\n' "$description"
  fail=1
  return 1
}

(
  printf 'wake-before\n' > /proc/self/comm || exit 1
  : > "$rename_ready" || exit 1
  while [[ ! -e "$rename_go" ]]; do sleep 0.05; done
  printf 'wake-after\n' > /proc/self/comm || exit 1
  : > "$rename_done" || exit 1
  exec tail -f /dev/null
) &
renamed_pid=$!
if wait_for_marker "$rename_ready" "the observed process to become ready"; then
  run 0 "session active"          -- --while-pid "$renamed_pid"
  if : > "$rename_go"; then
    if wait_for_marker "$rename_done" "the observed process to change its name"; then
      sleep 2
      run 0 "session active"          -- status
      run 0 "stopped"                 -- stop
    fi
  else
    printf 'FAIL : could not release the observed process\n'
    fail=1
  fi
fi
if kill "$renamed_pid" 2>/dev/null; then
  wait "$renamed_pid" 2>/dev/null
else
  printf 'FAIL : renamed process exited before explicit cleanup\n'
  wait "$renamed_pid" 2>/dev/null
  fail=1
fi
renamed_pid=""

run 0 "session active"          -- forever
run 0 "session active"          -- status
run 1 "session already active"  -- 30s
run 0 "stopped"                 -- stop
run 0 "no active session"       -- status

run 0 "session active"          -- 30s
run 0 "session active"          -- status
run 0 "stopped"                 -- stop

if [ "$fail" -eq 0 ]; then
  echo "ALL LINUX SMOKE TESTS PASSED"
else
  echo "LINUX SMOKE FAILED"
fi
exit "$fail"
