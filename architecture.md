# Architecture

`wake` is one CLI binary with no daemon. A foreground invocation validates arguments, reconciles durable state under an advisory lock, starts the platform sleep inhibitor, publishes the session, and exits.

## Source layout

| File | Responsibility |
|---|---|
| `main.rs` | Dispatch, help, and process exit codes |
| `commands.rs` | Public start, status, stop, and recovery flows |
| `durations.rs` | Duration parsing |
| `session.rs` | Strict state records, atomic writes, and advisory locking |
| `supervisor.rs` | Conditional Unix supervisors and Windows worker/guardian commands |
| `sysutil.rs` | Process identity, spawning, termination, and Windows handles |
| `platform/*.rs` | Compile-time-selected OS operations |

Platform modules expose free functions selected with `cfg`; there is no runtime trait layer.

## Process model

### macOS and Linux

Ordinary indefinite, timed, and process-bound sessions use a detached native inhibitor:

- macOS: `caffeinate`
- Linux: `systemd-inhibit`

The inhibitor owns the session lifetime. `status` and `stop` validate its process identity before trusting or terminating it.

Conditions that require polling use a detached copy of `wake` as a supervisor:

- `--until-charge` polls battery state.
- macOS `--even-lid` owns the `SleepDisabled` change and its restoration.

The supervisor owns the native inhibitor child, publishes itself as the session process, handles termination signals, and tears down toward allowing sleep. Repeated battery-read failures also end the session rather than leaving an unbounded inhibitor.

Linux does not support `--even-lid`.

### Windows

Every session uses a detached non-elevated `wake` worker. Its main thread calls `SetThreadExecutionState`, waits for the selected condition, and clears the assertion before exiting. Timed, process-bound, and charge conditions therefore share one native lifecycle.

`--even-lid` additionally launches one elevated guardian through `ShellExecuteExW`:

1. The foreground captures the active power-scheme GUID and raw AC/DC lid actions.
2. The worker and guardian identities plus that exact snapshot are atomically published.
3. The guardian accepts its write authority only from immutable launch arguments and waits for the durable record to match them.
4. It sets the recorded scheme to Do Nothing and verifies the active scheme and values before startup succeeds.
5. It holds an exact handle to the worker, then restores wake-owned AC/DC fields when that worker exits.

The guardian never chooses privileged write targets from mutable state. It does not overwrite a third-party lid value, reactivate a scheme the user switched away from, or delete the state record. A later non-elevated invocation verifies restoration and removes the record.

A guardian crash or power loss can require one later UAC-approved recovery. No user-mode process can guarantee immediate cleanup after its own forced termination without becoming a persistent service, which `wake` deliberately is not.

## Session state

`session.properties` is a strict, versioned record. Common fields include:

- process ID, native start identity, and executable path
- mode and trigger details
- start and optional end timestamps
- whether persistent lid recovery is required

Windows lid sessions also record guardian identity, the exact power-scheme GUID, and raw AC/DC values. macOS lid sessions record the prior `SleepDisabled` value.

State operations follow these rules:

- `wake.lock` serializes foreground start, status, stop, and recovery.
- Records are written to a new temporary file, flushed, and atomically renamed.
- Unknown, duplicate, missing, or inconsistent fields make a record malformed.
- Malformed lid-hinted state is retained and never authorizes an OS write.
- A stale ordinary session is deleted only after its process identity is no longer live.
- A valid stale lid session is deleted only after exact restoration verifies.

There is no compatibility parser for older state schemas because the product has no released state-compatibility requirement.

## Process identity

Bare process IDs are never sufficient for managed wake processes.

- Unix compares the PID, process start time, and expected executable.
- Windows opens one process handle, verifies its creation `FILETIME`, and uses that retained handle for waiting or termination.
- Windows process-bound sessions also retain a handle to the watched target, so PID reuse cannot extend the session.

Windows temporarily clears inheritance on the parent's standard handles while spawning a detached worker. Null child stdio alone does not prevent another inherited copy of a captured pipe from keeping the caller blocked on EOF.

## Time and battery conditions

`--until` resolves local wall time by calendar date. During a clock fold it chooses the earliest occurrence still in the future; during a clock gap it advances to the first valid instant after the requested time.

Charge sessions first determine whether the target is reachable in the current direction. Polling stops when the target is reached or after bounded consecutive read failures.

- macOS parses the battery record from `pmset`.
- Linux combines compatible energy/charge measurements, otherwise averages per-battery percentages.
- Windows uses aggregate `GetSystemPowerStatus` values and rejects unknown battery state.

## Unsafe code

Unsafe code is confined to small Windows FFI boundaries. Each block documents the pointer, handle, allocation, or thread-state invariant it relies on. Owned process handles close through RAII; power-scheme allocations are copied before `LocalFree`; persistent-setting writes always use an explicit GUID and verify their result.
