# Architecture

`wake` is one binary with no installed service. A foreground command validates the request and starts a detached supervisor for the active session. The supervisor owns the platform sleep inhibitor until the trigger completes or `wake stop` requests shutdown.

## Lifecycle

1. `main.rs` routes hidden helpers and side-effect-free help/version requests. Command paths serialize recovery with `wake.lock`.
2. `commands.rs` rejects existing or unfinished sessions, resolves the trigger, and builds a `RunSpec`.
3. For `--even-lid`, `lid.rs` records the original platform setting in `lid-restore.json` before any mutation.
4. Before spawning `__supervise__`, the foreground writes pending `session.json` with a unique file-lock lease. The child claims it, revalidates under `wake.lock`, starts the inhibitor, then publishes its PID.
5. An even-lid session repeats that handoff for privileged `__lid_watchdog__`. The watchdog revalidates its session and restoration marker before changing power settings, then marks itself ready.
6. The supervisor polls the stop marker, inhibitor, trigger, tracked process, and optional watchdog. Duration triggers use monotonic elapsed time; clock deadlines use wall time.
7. Ordinary teardown drops the inhibitor and removes matching state. Even-lid teardown is owned by the watchdog, which restores and verifies the recorded setting before removing the session and restoration marker.

`wake stop` writes a lease-bound `stop.json` and waits up to five seconds for graceful exit. It reports an unresponsive supervisor instead of sending a signal to a PID that may have been reused. PID/app triggers are observational, best-effort checks and are never terminated by `wake`.

PID/app triggers pair the PID with its native creation identifier: Linux start ticks, macOS start microseconds, or Windows creation `FILETIME`. The identifier is sampled around process metadata capture so PID reuse cannot silently rebind a trigger to another process.

## Durable State

All JSON is strict and written through atomic replacement. `wake.lock` serializes foreground state changes. Privileged helpers receive the resolved state directory explicitly rather than inferring the elevated user's home.

| File | Purpose |
|---|---|
| `session.json` | Pending or active supervisor lease, run specification, timing, and platform note. |
| `stop.json` | Stop request bound to one supervisor lease. |
| `lid-restore.json` | Authoritative pre-mutation setting retained until verified restoration. |
| `lid-watchdog.json` | Matching supervisor and watchdog leases plus startup readiness. |
| `process-*.lock` | OS-released lifetime lease for one wake-owned process. |

Every session command attempts safe recovery. A valid live watchdog is preserved. Stale ordinary state is removed. Unresolved lid state is restored through the Windows `__lid_restore__` helper or a fixed macOS `sudo pmset` argument vector; malformed or conflicting live state fails closed instead of being overwritten.

## Platform Boundary

`platform/mod.rs` selects one trait-free implementation at compile time:

- macOS owns a `caffeinate` child and uses `pmset` for battery and lid state.
- Linux owns a `systemd-inhibit` child and reads batteries from sysfs. It has no even-lid mode.
- Windows owns a `PowerCreateRequest` handle and reads battery state natively. Its watchdog checks that the recorded power scheme is still active before reapplying changes. A concurrent scheme change between that check and the API call remains possible.

The macOS watchdog executes `wake` as root, so even-lid startup resolves the canonical executable and rejects any executable or ancestor not owned by root, carrying an extended ACL, or writable by the invoking user, group, or others. This prevents a user-writable `current_exe` path from being replaced between the unprivileged process and `sudo` execution.

The JSON lifecycle does not guess through the older `session.properties` format. Foreground commands fail closed while that file exists so an active legacy inhibitor cannot become invisible beside a new session.

`sysutil.rs` centralizes external process observation, detached spawning, Windows elevation, and handle ownership. `session.rs` owns serialization, state locking, and process leases. `run.rs` owns validated trigger semantics. `supervisor.rs` contains the single condition loop.
