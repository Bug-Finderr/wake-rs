# wake

Keep your machine awake from the CLI — macOS, Linux, Windows. No daemon. The binary is `wake`.

Rust port of [AbhinavGupta-de/wake-cli](https://github.com/AbhinavGupta-de/wake-cli) (originally
Java/GraalVM). Architecture: [architecture.md](architecture.md) · Behavior contract: [PLAN.md](PLAN.md).

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

| Aspect | wake-cli (Java) | wake-rs (Rust) |
|---|---|---|
| Runtime | Java 21 + GraalVM native-image | Rust, edition 2024 |
| Binary | GraalVM native (multi-MB) | ~350 KB |
| Build deps | GraalVM JDK 25, Maven, MSVC | `cargo` + 5 crates |
| Commands / flags / output | — | identical |
| Windows sleep blocking | `[uint32]0x80000003` cast throws → silently **no-ops** | decimal flags → **actually blocks** |
| Charge supervisor stop (Windows) | child **orphaned** on `TerminateProcess` → stays awake | Job Object kill-on-close → child dies |
| Detached child handle hygiene | JVM does not leak handles | explicit `HANDLE_FLAG_INHERIT` clear + `CREATE_NO_WINDOW` |
| File lock | `java.nio` `FileLock` | native `std::fs::File` lock (1.89+) |
| Interactive picker | raw `stty` shelling | `crossterm` |
| Tests | CI smoke | 14 unit + Windows e2e smoke + Linux/macOS `cargo check` |
| License | MIT | MIT |

Same observable behavior, with the two Windows correctness bugs above fixed. Internal architecture
differs (free-function platform layer, process-identity verification, native locking).

## Tests

```sh
cargo test                       # unit
pwsh tests/smoke_windows.ps1     # Windows e2e (mirrors upstream CI)
```

## Contributing

This repository does **not accept external contributions**. Pull requests are **closed automatically**
by [`.github/workflows/close-prs.yml`](.github/workflows/close-prs.yml) (same approach as
[Bug-Finderr/api-proxy](https://github.com/Bug-Finderr/api-proxy); see also
[mitchellh/vouch](https://github.com/mitchellh/vouch)). Open an issue instead.

## License

[MIT](LICENSE). Port of the MIT-licensed [wake-cli](https://github.com/AbhinavGupta-de/wake-cli).
