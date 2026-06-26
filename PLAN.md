# wake-rs — Rust port of wake-cli

Port of [AbhinavGupta-de/wake-cli](https://github.com/AbhinavGupta-de/wake-cli) (Java/GraalVM, v0.4.1)
to Rust. Binary is named **`wake`** (package `wake-rs`). Goal: same observable behavior, not a
line-for-line transliteration. Reference source kept (gitignored) under `wake-cli/`.

Scope (confirmed with user): **Full 3-OS port** — Windows + macOS + Linux, every feature including
macOS `--even-lid` and the Unix TUI picker. Windows is run + end-to-end tested here; macOS/Linux are
compile-checked via cross `cargo check` (cannot be run on this machine).

## Behavior contract (must match)

Commands / flags:
- `wake` (no args): TTY + interactive-capable → picker (macOS/Linux); else start indefinite. Windows → indefinite.
- `wake <duration>` / `wake -t <dur>` / `wake --for <dur>` → timed
- `wake forever` | `wake indefinite` → indefinite (only `--no-display`, `--even-lid` allowed after)
- `wake --until HH:MM` → until clock time (rolls to next day if already past)
- `wake --until-charge N` (1-100) → until battery hits N%
- `wake --while-pid PID` → while pid alive
- `wake --while-app NAME` → while named process alive
- `wake --no-display` → system-only (allow display sleep)
- `wake --even-lid` → macOS only (sudo + SleepDisabled); error elsewhere
- `wake status` / `wake stop`
- `wake help|-h|--help` ; `wake version|-v|--version` → `wake 0.4.1`
- hidden: `__supervise_charge__`, `__supervise_lid__`

Errors / exit codes:
- UsageError → stderr `wake: <msg>` + `try 'wake --help'`, exit **2**
- other error → stderr `wake: <msg>`, exit **1**
- conflicting triggers → UsageError ("conflicting triggers: X and Y")
- unknown flag `-x` → UsageError
- session already active → 2 stderr lines, exit **1**

State: `~/.local/state/wake/session.properties` (override `WAKE_STATE_DIR`); `wake.lock` advisory lock;
atomic write via tmp+rename. Process-identity check (start time + exe + expected basename / "wake" in cmdline)
to decide a saved session is still live & ours.

Duration syntax: `90s 5m 1h 1h30m 2h45m30s 1d`, or plain seconds; units in order d,h,m,s, each ≤once; max 30d.

## Architecture (Rust)

Free-function-per-platform, selected by `cfg` (no `dyn` trait). Shared command logic calls
`platform::*`; each platform provides the full surface (even-lid fns are stubs on win/linux).

```
src/
  main.rs         entry, dispatch, AppError(Usage/Fail)->exit codes, help/version text
  commands.rs     start/status/stop/forever, confirmations, even-lid + recovery machinery,
                  seconds_until, pretty_duration
  durations.rs    duration parser (hand-rolled tokenizer, no regex dep)
  session.rs      Session, properties read/write, lock guard, recovery (SavedState/Malformed)
  supervisor.rs   BatteryStatus, ChargePlan, run_charge, run_lid
  sysutil.rs      sysinfo-backed: is_alive, identity capture/match, terminate, spawn_detached, self_exe
  platform/mod.rs trait-less shared helpers (pgrep, first_allowed_pid, resolve_on_path) + cfg re-export
  platform/windows.rs   PowerShell SetThreadExecutionState, Win32_Battery, tasklist
  platform/macos.rs     caffeinate, pmset, sudo, SleepDisabled
  platform/linux.rs     systemd-inhibit (+ probe/degrade), sysfs battery, pgrep
  interactive.rs  cfg(unix) crossterm picker, ANSI rendering identical to Java
```

Crates (pinned via `cargo add`, latest stable): sysinfo 0.39, fs4 1.1 (sync), base64 0.22,
chrono 0.4, crossterm 0.29 (unix only). Edition 2024. No regex (hand-parsed). Manual arg parsing
(faithful error strings; clap would change them).

## Testing strategy

- Unit tests: durations (valid/invalid/overflow/cap), CSV parser (Windows), charge planning matrix,
  pretty_duration, seconds_until rollover, properties round-trip.
- Integration (Windows, run here): replicate the upstream CI smoke test —
  version/help/conflict/no-battery-graceful/forever+status+duplicate+stop/timed lifecycle, and the
  `wake` on PATH resolves test.
- Cross-check: `cargo check --target x86_64-unknown-linux-gnu` and `aarch64-apple-darwin` to typecheck
  the macOS/Linux code paths (run targets unavailable).

## Notable divergences from the Java reference (intentional, all toward correctness)

1. **PowerShell ES flags as decimal, not hex.** The reference builds `[uint32]0x80000003`; PowerShell
   parses `0x80000003` as a *negative Int32*, so the cast throws and `SetThreadExecutionState` silently
   no-ops (the upstream CI only checks the child is alive, not that sleep is actually blocked). We emit
   decimal `2147483651` / `2147483649`, which stay in uint32 range and genuinely block sleep
   (verified: the call returns the prior ES state instead of erroring).
2. **No leaked handles to the detached child (Windows).** We clear `HANDLE_FLAG_INHERIT` on our std
   handles before spawning, and spawn with `CREATE_NO_WINDOW` (not `DETACHED_PROCESS`, which makes
   PowerShell exit immediately). Without this the long-lived child inherits a captured stdout pipe and
   hangs pipe-reading callers (CI / scripts) forever. The JVM doesn't leak handles, so the reference
   never hit this.
3. **`std::fs::File` native locking instead of an `fs4` crate.** Rust 1.89+ stabilized `File::try_lock`
   / `unlock`; using std drops a dependency and is the current idiom.
4. Manual arg parsing (faithful error strings) and a hand-rolled duration tokenizer (no `regex` dep);
   platform tools are shelled out exactly as upstream (no battery/daemon crates).

## Progress log

- [x] Research reference (9 Java files), capture behavior contract & CI acceptance spec
- [x] Scaffold cargo project, pin deps, verify sysinfo/std-lock APIs
- [x] durations.rs + tests
- [x] session.rs (+ native std file lock) + sysutil.rs (+ handle-inheritance fix)
- [x] platform/* (windows tested; macos/linux written)
- [x] supervisor.rs (charge + lid, unix stop-signal flag)
- [x] commands.rs + main.rs (help/version/dispatch/start/status/stop + even-lid recovery)
- [x] interactive.rs (crossterm picker, unix)
- [x] Build clean on Windows; 14 unit tests green; clippy clean
- [x] Integration smoke test green (Windows) — `tests/smoke_windows.ps1`, mirrors upstream CI
- [x] Cross `cargo check` linux + macos targets — both compile warning-free (rustup 1.96 toolchain)

## How to build / test

- Build:   `cargo build --release`  → `target/release/wake.exe` (binary name is `wake`)
- Unit:    `cargo test`
- E2E:     `pwsh tests/smoke_windows.ps1` (or `powershell -File tests/smoke_windows.ps1`)
- Cross:   with the rustup toolchain on PATH, `cargo check --target x86_64-unknown-linux-gnu`
           and `--target aarch64-apple-darwin` (run-tested only on Windows here)
