# wake

Keep your machine awake from the command line on macOS, Linux, and Windows. `wake` is one executable with no daemon.

## Install

Build with Cargo:

```sh
cargo build --release
```

The binary is `target/release/wake` (`wake.exe` on Windows). Release assets are:

- `wake-linux-x64.tar.gz` (glibc 2.35 or newer)
- `wake-macos-x64.tar.gz`
- `wake-macos-arm64.tar.gz`
- `wake.exe`

On Linux or macOS, extract the matching archive and put `wake` on your `PATH`:

```sh
mkdir -p ~/.local/bin
tar -xzf wake-linux-x64.tar.gz
install -m 0755 wake ~/.local/bin/wake
```

On Windows, place `wake.exe` in a directory on your `PATH`. Optionally verify any downloaded asset's provenance:

```sh
gh attestation verify <asset> --repo Bug-Finderr/wake-rs
```

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
| macOS | `caffeinate` | `pmset` | Changes `SleepDisabled` 0 to 1 with `sudo` |
| Linux | `systemd-inhibit` | `/sys/class/power_supply` | Requires a systemd-logind lid-switch inhibitor; errors if refused |
| Windows | Native `SetThreadExecutionState` worker | `GetSystemPowerStatus` | One UAC prompt starts a narrow guardian that restores the exact power-plan values |

On Linux, `--even-lid` requires `idle:sleep:handle-lid-switch`, or `sleep:handle-lid-switch` with `--no-display`. It never falls back, needs no `sudo`, and leaves no persistent setting; the inhibitor ends with the `systemd-inhibit` process. Without `--even-lid`, `wake` may fall back and report what was lost. This covers only logind-managed suspend, not privileged or non-logind paths such as direct `/sys/power/state` writes, acpid, WSL, or containers.

Windows reports aggregate system battery percentage. macOS and Linux use the battery information exposed by their native platform interfaces.

## State and recovery

Session state is stored at:

- macOS/Linux: `$XDG_STATE_HOME/wake/session.properties`, or `~/.local/state/wake/session.properties`
- Windows: `%LOCALAPPDATA%\wake\session.properties`

Set `WAKE_STATE_DIR` to use another directory.

State writes are locked and atomic. On macOS, recovery restores `SleepDisabled=0` only when `wake` owned the 0-to-1 transition; if it was already `1`, the record grants no later write. A crash before that ownership is published retains a non-authoritative pending record for manual inspection. Windows restores only the exact power-plan values recorded before `wake` changed them. Linux lid sessions record no persistent value; a stale Linux lid record is deleted once its inhibitor process is dead, because the lock dies with that process. Malformed lid recovery state is retained and reported without changing system settings.

## Verification

```sh
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
