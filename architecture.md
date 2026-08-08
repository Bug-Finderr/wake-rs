# Architecture

`wake` is one CLI binary with no daemon. A foreground invocation validates arguments, reconciles durable state under an advisory lock, starts the platform-specific lifetime owner, publishes or waits for its session state, and exits.

## Source layout

| File | Responsibility |
|---|---|
| `main.rs` | Dispatch, help, and process exit codes |
| `commands.rs` | Public start, status, stop, and recovery flows |
| `durations.rs` | Duration parsing |
| `error.rs` | Usage and runtime errors with their exit codes |
| `session.rs` | Strict state records, atomic writes, and advisory locking |
| `supervisor.rs` | Conditional Unix supervisors and Windows worker/guardian commands |
| `sysutil.rs` | Process identity, spawning, termination, and Windows handles |
| `platform/*.rs` | Compile-time-selected OS operations |

Platform modules expose free functions selected with `cfg`; there is no runtime trait layer.

## Process model

### macOS and Linux

Indefinite, duration-based, and process-bound sessions use a detached native inhibitor unless macOS `--even-lid` needs a supervisor:

- macOS: `caffeinate`
- Linux: `systemd-inhibit`

The inhibitor owns the session lifetime. `status` and `stop` validate its process identity before trusting or terminating it.

Conditions that require polling use a detached copy of `wake` as a supervisor:

- `--until` enforces an absolute local-time deadline.
- `--until-charge` polls battery state.
- macOS `--even-lid` restores only its `SleepDisabled` 0-to-1 change.

The supervisor owns the native inhibitor child, publishes itself as the session process, handles termination signals, and tears down toward allowing sleep. Repeated battery-read failures also end the session rather than leaving an unbounded inhibitor.

Linux `--even-lid` requires the exact systemd-logind inhibitor scope for the session: `idle:sleep:handle-lid-switch` for the default display+system session, `sleep:handle-lid-switch` with `--no-display`. The scope is probed at startup; if logind refuses it, the session errors instead of degrading. Sessions without `--even-lid` probe the same lid-inclusive scope first and may degrade to a narrower scope with an explicit note. There is no `sudo` and no persistent setting: the lock is a file descriptor held by the `systemd-inhibit` process and releases when that process exits, so Linux needs no restoration lifecycle. macOS and Windows retain theirs: macOS restores only its wake-owned `SleepDisabled` 0-to-1 transition and Windows restores the recorded power-plan values. The Linux boundary is logind-managed suspend only; root bypasses, direct `/sys/power/state` writes, custom acpid handlers, WSL, containers, and non-logind stacks are out of scope.

### Windows

Every session uses a detached `wake` worker. Its main thread calls `SetThreadExecutionState`, waits for the selected condition, and clears the assertion before exiting. Timed, process-bound, and charge conditions therefore share one native lifecycle.

`--even-lid` additionally launches one detached guardian. Windows grants standard users write access to power-plan values by default, so nothing in the lifecycle requests elevation:

1. The foreground checks lid write access through `PowerSettingAccessCheckEx` and captures the active power-scheme GUID and raw AC/DC lid actions. A group-policy override or restricted access fails the start before anything launches.
2. The worker first publishes ordinary, non-lid session state.
3. The guardian validates that provisional worker identity, rechecks the active scheme and captured values, rejects an elapsed absolute deadline, then atomically publishes durable restoration authority immediately before its first power write.
4. It sets the recorded scheme to Do Nothing and verifies the active scheme and values before startup succeeds.
5. It holds an exact handle to the worker, then restores wake-owned AC/DC fields when that worker exits.

The guardian never chooses write targets from mutable state. It does not overwrite a third-party lid value, reactivate a scheme the user switched away from, or delete the state record. A later invocation verifies restoration and removes the record.

A guardian crash, a correlated tree-kill (for example a kill-on-close job object tearing down the launching context, which the guardian shares as an ordinary child process), or power loss can leave the override in place. The next `wake` invocation restores the recorded values directly in the foreground and removes the record; there is no separate recovery process, because a respawned guardian would hold the same token and could not do more than the foreground write. No user-mode process can guarantee immediate cleanup after its own forced termination without becoming a persistent service, which `wake` deliberately is not.

Restricted machines fail with distinct errors, at startup and equally during a recovery pass. A group-policy override reports the lid action as policy-managed, because policy binds elevated writers too and a plan-store write would be silently ineffective. A tightened power-setting ACL reports restricted settings and suggests an elevated terminal, which a wake started there inherits.

## Session state

`session.properties` is a strict, versioned record. Common fields include:

- process ID, native start identity, and executable path
- mode and trigger details
- start and optional end timestamps
- whether lid coverage was requested (`evenLid`)

Windows lid sessions also record guardian identity, the exact power-scheme GUID, and raw AC/DC values. macOS lid sessions record the prior `SleepDisabled` value; `0` marks the wake-owned transition that recovery may reverse, while `1` grants no restoration write.

State operations follow these rules:

- `wake.lock` serializes foreground start, status, stop, and recovery.
- Records are written to a new temporary file, flushed, and atomically renamed.
- Unknown, duplicate, missing, or inconsistent fields make a record malformed.
- Malformed lid-hinted state is retained and never authorizes an OS write.
- A pre-transition macOS startup marker is deliberately non-authoritative and requires manual recovery if the foreground dies before supervisor publication.
- A stale ordinary session is deleted only after its process identity is no longer live.
- A valid stale macOS or Windows lid session is deleted only after any wake-owned restoration verifies; on macOS, a prior value of `1` means no transition was owned and no write is allowed.
- A stale Linux lid session holds no restoration value — the inhibitor is a file descriptor that dies with its process — so it is treated as ordinary stale state.

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
