# wake

Keep your machine awake from the command line on macOS, Linux, and Windows. `wake` is one executable with no daemon.

## Install

Build with Cargo:

```sh
cargo build --release
```

The binary is `target/release/wake` (`wake.exe` on Windows). Releases provide Unix binaries in `.tar.gz` archives and Windows as `.exe`. On Linux or macOS, extract the archive before putting `wake` on your `PATH`.

## Usage

```sh
wake                     # indefinitely
wake forever             # indefinitely, explicitly
wake 1h                  # for a duration
wake --until 23:00       # until the next local clock time
wake --until-charge 80   # until battery reaches 80%
wake --while-pid 1234    # while this process is running
wake --while-app Slack   # while a matching app is running
wake --no-display        # allow display sleep
wake --even-lid          # keep awake with the lid closed
wake status
wake stop
wake help
wake version
```

Durations accept plain seconds or ordered `d`, `h`, `m`, and `s` units, such as `90s`, `1h30m`, or `2h45m30s`. The maximum is 30 days.

`--until-charge` follows the current battery direction. A target that cannot be reached while the battery keeps its current direction returns an error instead of creating an indefinite session.

Only one session can be active. `wake stop` is safe to repeat.

## Platform behavior

| Platform | Sleep inhibition | Battery | `--even-lid` |
|---|---|---|---|
| macOS | `caffeinate` | `pmset` | Temporarily changes `SleepDisabled` through `sudo` |
| Linux | `systemd-inhibit` | `/sys/class/power_supply` | Requires the exact systemd-logind `handle-lid-switch` inhibitor and errors if logind refuses it |
| Windows | Native `SetThreadExecutionState` worker | `GetSystemPowerStatus` | One UAC prompt starts a narrow guardian that restores the exact power-plan values |

On Linux, `--even-lid` never degrades: the default display+system session requires the exact `idle:sleep:handle-lid-switch` scope, `--no-display` requires `sleep:handle-lid-switch`, and startup fails unless logind grants the requested scope. It uses no `sudo` and no persistent setting; the lock is held by the `systemd-inhibit` process and releases when that process exits. Sessions without `--even-lid` still request lid handling best-effort and may degrade to a narrower scope with an explicit note. This covers normal logind-managed suspend only, not root-initiated suspend, direct `/sys/power/state` writes, custom acpid scripts, WSL, containers, or non-logind stacks.

Windows reports aggregate system battery percentage. macOS and Linux use the battery information exposed by their native platform interfaces.

## State and recovery

Session state is stored at:

- macOS/Linux: `$XDG_STATE_HOME/wake/session.properties`, or `~/.local/state/wake/session.properties`
- Windows: `%LOCALAPPDATA%\wake\session.properties`

Set `WAKE_STATE_DIR` to use another directory.

State writes are locked and atomic. On macOS and Windows, a valid stale lid record restores only the exact value recorded before `wake` changed it. Linux lid sessions record no persistent value; a stale Linux lid record is deleted once its inhibitor process is dead, because the lock dies with that process. Malformed lid recovery state is retained and reported without changing system settings.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --release --locked
cargo build --release --locked
bash tests/smoke_linux.sh
bash tests/smoke_macos.sh
pwsh tests/smoke_windows.ps1
```

Run the smoke test for the current platform.

## Contributing

External pull requests are closed automatically. Open an issue instead.

## License

[MIT](LICENSE). This project is a Rust port of the MIT-licensed [wake-cli](https://github.com/AbhinavGupta-de/wake-cli).
