# wake

Keep macOS, Linux, and Windows awake from one command. `wake` installs as a single binary with no service to configure.

This is a Rust port of [AbhinavGupta-de/wake-cli](https://github.com/AbhinavGupta-de/wake-cli). See [architecture.md](architecture.md) for the internal lifecycle.

## Install

```sh
cargo build --release --locked
```

The binary is written to `target/release/wake` or `target/release/wake.exe`. Prebuilt binaries are also available from [Releases](../../releases). Put the binary on your `PATH` as `wake`.

## Usage

```sh
wake                     # picker on macOS/Linux; indefinite on Windows
wake forever             # indefinite
wake 1h | 30m | 1h30m    # timed
wake --until 23:00       # until a local clock time
wake --until-charge 80   # until battery reaches 80%
wake --while-pid 1234    # while an exact process is alive
wake --while-app Slack   # while a named process is alive
wake --no-display        # allow display sleep
wake --even-lid          # keep running with the lid closed
wake status | stop
```

`--even-lid` is available on macOS and Windows. Closed-lid use can increase heat and battery drain.

| Platform | Sleep inhibition | Even-lid behavior |
|---|---|---|
| macOS | `caffeinate` | A sudo-backed watchdog controls `pmset disablesleep` and restores the prior value. |
| Linux | `systemd-inhibit` | Unsupported. |
| Windows | Native power requests | An elevated watchdog snapshots a power plan's AC/DC lid actions, applies the override, then restores the recorded values. |

Each session runs through a detached supervisor. State is stored under `~/.local/state/wake`, `$XDG_STATE_HOME/wake`, or `%LOCALAPPDATA%\wake`. Set `WAKE_STATE_DIR` to override the directory.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --release --locked
cargo build --release --locked
```

After the release build, run `bash tests/smoke_linux.sh target/release/wake` on Linux or `pwsh tests/smoke_windows.ps1 target/release/wake.exe` on Windows. CI runs unit and smoke coverage on all three platforms.

## Contributing

External pull requests are closed automatically. Open an issue instead.

## License

[MIT](LICENSE), including the upstream [wake-cli](https://github.com/AbhinavGupta-de/wake-cli) attribution.
