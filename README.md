# wake

Keep macOS, Linux, and Windows awake from one command. `wake` is a single binary with no service to configure.

This is a Rust port of [AbhinavGupta-de/wake-cli](https://github.com/AbhinavGupta-de/wake-cli). See the [architecture](architecture.md) for the internal design.

## Install

Build from source:

```sh
cargo build --release --locked
```

The binary is written to `target/release/wake` or `target/release/wake.exe`. Prebuilt binaries are also available from [Releases](../../releases). A downloaded macOS or Linux binary may need its execute bit set:

```sh
chmod +x wake
```

Put the binary on your `PATH` as `wake`.

## Usage

```sh
wake                              # indefinitely
wake 1h30m                        # for a duration
wake --until 23:00 --no-display   # until local time, allowing display sleep
wake --until-charge 80            # until battery reaches 80%
wake --while-pid 1234             # while a process is alive
wake --while-app Slack            # while a named process is alive
wake --even-lid                   # request closed-lid operation
wake status
wake stop
```

Run `wake --help` for the complete command reference and duration syntax.

## Platform behavior

| Platform | Sleep inhibition | `--even-lid` |
|---|---|---|
| macOS | `caffeinate` | Uses a sudo-backed watchdog to apply `pmset disablesleep` and make a best-effort conditional restoration of the prior value. |
| Linux | `systemd-inhibit` through systemd-logind | Requires logind to grant a blocking `handle-lid-switch` inhibitor. It does not use elevation or fall back to a weaker lock. |
| Windows | Native power requests | Uses a UAC-elevated watchdog to change the active plan's AC/DC lid actions and make a best-effort conditional restoration. |

Closed-lid operation can increase heat and battery drain. It is a request to the operating system, not a hardware guarantee.

On Linux, wake requires systemd-logind and GNU `tail` with `--pid` support. `--even-lid` refuses to start if logind does not grant `handle-lid-switch`. A granted lock covers the logind lid path only. It cannot stop firmware handling or bypass suspend paths outside logind.

On macOS, `--even-lid` needs an interactive terminal for sudo and a protected installation. The executable and every parent directory must be root-owned, have no extended ACL, and be unwritable without root. One suitable installation is:

```sh
sudo install -o root -g wheel -m 0755 target/release/wake /usr/local/bin/wake
```

On Windows, `--even-lid` requires administrator elevation by the same Windows account that started `wake`. Elevating as a different administrator account is rejected so private recovery state does not change ownership.

On macOS and Windows, wake rereads the lid values and restores only fields that still contain its override. This is best-effort because the OS APIs do not provide an atomic compare-and-set: a user or system change between wake's read and write can be overwritten. On Windows, a power-scheme switch in the narrow interval between wake's active-scheme check and apply call can also reactivate the recorded scheme.

## State and recovery

Private session state is stored under `$XDG_STATE_HOME/wake` or `~/.local/state/wake` on Unix, and `%LOCALAPPDATA%\wake` on Windows. Set `WAKE_STATE_DIR` to override the directory.

After an interrupted run, the next start, status, or stop command attempts recovery before continuing. Recovering a macOS or Windows even-lid session may request sudo or UAC again. If restoration cannot be verified, wake keeps the recovery state and reports an error.

Older state formats are not migrated silently. Stop an active session with the binary that created it before removing its legacy state.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --release --locked
cargo build --release --locked
```

After the release build, run `bash tests/smoke_linux.sh target/release/wake` on Linux or `pwsh tests/smoke_windows.ps1 target/release/wake.exe` on Windows. CI builds and tests all three platforms.

## Contributing

External pull requests are closed automatically. Open an issue instead.

## License

[MIT](LICENSE), including the upstream [wake-cli](https://github.com/AbhinavGupta-de/wake-cli) attribution.
