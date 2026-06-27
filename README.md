# wake

Keep your machine awake from the CLI on macOS, Linux, and Windows. No daemon. The binary is `wake`.

Rust port of [AbhinavGupta-de/wake-cli](https://github.com/AbhinavGupta-de/wake-cli) (originally
Java/GraalVM). Design notes: [architecture.md](architecture.md).

## Install

```sh
cargo build --release      # -> target/release/wake[.exe]
```

Or download a binary from [Releases](../../releases): one self-contained executable,
no installer. Put it on your `PATH` as `wake` (`wake.exe` on Windows).

- **Linux**: `install -Dm755 wake-linux-x64 ~/.local/bin/wake`
- **macOS**: `install -m755 wake-macos-arm64 /usr/local/bin/wake`
- **Windows** (PowerShell, then reopen the terminal):

  ```powershell
  $dir = "$env:LOCALAPPDATA\Programs\wake"; mkdir -Force $dir
  Move-Item wake.exe "$dir\wake.exe"
  [Environment]::SetEnvironmentVariable("Path",
    [Environment]::GetEnvironmentVariable("Path", "User") + ";$dir", "User")
  ```

## Usage

```sh
wake                     # picker (macOS/Linux); indefinite (Windows)
wake forever             # indefinite
wake 1h | 30m | 1h30m    # timed
wake --until 23:00       # until a clock time
wake --until-charge 80   # until battery hits N% (1-100)
wake --while-pid 1234    # while a pid is alive
wake --while-app Slack   # while a process is alive
wake --no-display        # block system sleep, allow display sleep
wake --even-lid          # stay awake with the lid closed (macOS: sudo; Windows: sets lid-close action)
wake status | stop | help | version
```

State lives at `~/.local/state/wake/session.properties` (override with `WAKE_STATE_DIR`).

| Platform | Mechanism |
|---|---|
| macOS | `caffeinate` + `pmset`; `--even-lid` via `sudo pmset -a disablesleep` |
| Linux | `systemd-inhibit` (systemd ≥ 190), degrading when polkit denies lid locks; sysfs battery |
| Windows | PowerShell `SetThreadExecutionState`; `Win32_Battery`; `tasklist` (no picker); `--even-lid` sets the power-plan lid-close action to Do Nothing (UAC only if the direct write is denied) |

## wake-rs vs wake-cli

Same commands, flags, and output. What differs:

| | wake-cli | wake-rs |
|---|---|---|
| Language / build | Java 21, GraalVM `native-image`, Maven | Rust 2024, `cargo` |
| Binary size | multi-MB | ~350 KB |
| File locking | `java.nio` `FileLock` | native `std::fs` locks (Rust 1.89+) |
| Interactive picker | raw mode via `stty` | `crossterm` |
| Tests | CI smoke | unit + Windows/Linux smoke + macOS compile check |

## Tests

```sh
cargo test                       # unit
pwsh tests/smoke_windows.ps1     # Windows e2e (mirrors upstream CI)
```

## Contributing

External contributions are not accepted; pull requests are closed automatically. Open an issue instead.

## License

[MIT](LICENSE). Port of the MIT-licensed [wake-cli](https://github.com/AbhinavGupta-de/wake-cli).
