# wake

Keep your machine awake from the CLI — macOS, Linux, Windows. No daemon. The binary is `wake`.

Rust port of [AbhinavGupta-de/wake-cli](https://github.com/AbhinavGupta-de/wake-cli) (originally
Java/GraalVM). Design notes: [architecture.md](architecture.md).

## Install

```sh
cargo build --release      # -> target/release/wake[.exe]
```

Or grab a binary from [Releases](../../releases). Put it on PATH (it must be named `wake`).

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
wake --even-lid          # macOS only: stay awake with the lid closed (sudo)
wake status | stop | help | version
```

State lives at `~/.local/state/wake/session.properties` (override with `WAKE_STATE_DIR`).

| Platform | Mechanism |
|---|---|
| macOS | `caffeinate` + `pmset`; `--even-lid` via `sudo pmset -a disablesleep` |
| Linux | `systemd-inhibit` (systemd ≥ 190), degrading when polkit denies lid locks; sysfs battery |
| Windows | PowerShell `SetThreadExecutionState`; `Win32_Battery`; `tasklist` (no picker) |

## wake-rs vs wake-cli

Same commands, flags, and output. What differs:

| | wake-cli | wake-rs |
|---|---|---|
| Language / build | Java 21, GraalVM `native-image`, Maven | Rust 2024, `cargo` |
| Binary size | multi-MB | ~350 KB |
| Windows sleep blocking | `[uint32]0x80000003` casts to a negative `Int32`, so the assertion silently no-ops | decimal flags stay in range, so sleep is actually blocked |
| Windows charge-session stop | the supervisor's child is orphaned and keeps the machine awake | the child is in a kill-on-close Job Object and dies with the supervisor |
| File locking | `java.nio` `FileLock` | native `std::fs` locks (Rust 1.89+) |
| Interactive picker | raw mode via `stty` | `crossterm` |
| Tests | CI smoke | unit + Windows/Linux smoke + macOS compile check |

The two Windows rows are bugs fixed in the port; the rest are implementation choices.

## Tests

```sh
cargo test                       # unit
pwsh tests/smoke_windows.ps1     # Windows e2e (mirrors upstream CI)
```

## Contributing

External contributions are not accepted — pull requests are closed automatically. Open an issue instead.

## License

[MIT](LICENSE). Port of the MIT-licensed [wake-cli](https://github.com/AbhinavGupta-de/wake-cli).
