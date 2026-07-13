#!/usr/bin/env bash
set -u

wake="${1:-${CARGO_TARGET_DIR:-target}/release/wake}"
export WAKE_STATE_DIR="$(mktemp -d)"
trap '"$wake" stop >/dev/null 2>&1; rm -rf "$WAKE_STATE_DIR"' EXIT

# Isolate lifecycle coverage from the runner's systemd and polkit state.
fake_bin="$WAKE_STATE_DIR/bin"
mkdir -p "$fake_bin"
cat > "$fake_bin/systemd-inhibit" <<'EOF'
#!/usr/bin/env bash
while [[ "${1:-}" == --* ]]; do
  shift
done
exec "$@"
EOF
chmod +x "$fake_bin/systemd-inhibit"
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

run 0 "wake "                    -- --version
run 0 "wake --until-charge N"   -- --help
run 0 "wake --until-charge N"   -- forever --help
run 2 "conflicting triggers"    -- --until-charge 80 --while-pid 1
run 2 "unknown flag"            -- --bogus
run 2 "invalid duration"        -- 5x
run 0 "no active session"       -- status
run 0 "no active session"       -- stop
run 0 "session active"          --
run 0 "session active"          -- status
run 0 "stopped"                 -- stop

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

(
  printf 'wake-before\n' > /proc/self/comm
  sleep 1
  printf 'wake-after\n' > /proc/self/comm
  sleep 4
) &
renamed_pid=$!
run 0 "session active"          -- --while-pid "$renamed_pid"
sleep 2
run 0 "session active"          -- status
run 0 "stopped"                 -- stop
wait "$renamed_pid"

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
