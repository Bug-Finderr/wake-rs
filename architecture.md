# Architecture

`wake` is one binary with no installed service. A foreground command validates the request and starts a detached supervisor for the active session. The supervisor owns the platform sleep inhibitor until the trigger completes or `wake stop` requests shutdown.

## Lifecycle

1. `main.rs` routes hidden helper commands, then reconciles stale lid state before normal command dispatch.
2. `commands.rs` acquires the state lock, rejects an existing live session, resolves the trigger, and builds a `RunSpec`.
3. For `--even-lid`, `lid.rs` records the original platform setting in `lid-restore.json` before any mutation.
4. The foreground process starts `__supervise__` with the serialized run specification. The supervisor starts the inhibitor, verifies it remains alive, and writes `session.json` with its exact process identity.
5. An even-lid session also starts a privileged `__lid_watchdog__`. The supervisor does not proceed until the watchdog publishes matching ready state.
6. The supervisor polls the stop marker, inhibitor, trigger, tracked process, and optional watchdog. Duration triggers use monotonic elapsed time; clock deadlines use wall time.
7. Ordinary teardown drops the inhibitor and removes matching state. Even-lid teardown is owned by the watchdog, which restores and verifies the recorded setting before removing the session and restoration marker.

`wake stop` writes an identity-bound `stop.json`, waits for graceful exit, then terminates only the recorded process identity if needed. A reused PID is never treated as the same session.

## Durable State

All JSON is strict and written through atomic replacement. `wake.lock` serializes foreground state changes.

| File | Purpose |
|---|---|
| `session.json` | Supervisor identity, run specification, start time, deadline, and platform note. |
| `stop.json` | Stop request bound to one session identity. |
| `lid-restore.json` | Authoritative pre-mutation setting retained until verified restoration. |
| `lid-watchdog.json` | Matching supervisor and watchdog identities. |

Every foreground invocation attempts safe recovery. A valid live watchdog is preserved. Stale ordinary state is removed. Unresolved lid state is restored through `__lid_restore__`; malformed or conflicting live state fails closed instead of being overwritten.

## Platform Boundary

`platform/mod.rs` selects one trait-free implementation at compile time:

- macOS owns a `caffeinate` child and uses `pmset` for battery and lid state.
- Linux owns a `systemd-inhibit` child and reads batteries from sysfs. It has no even-lid mode.
- Windows owns a `PowerCreateRequest` handle and reads battery state natively. Its watchdog checks that the recorded power scheme is still active before reapplying changes. A concurrent scheme change between that check and the API call remains possible.

`sysutil.rs` centralizes process identity, exact termination, detached spawning, Windows elevation, and handle ownership. `session.rs` owns serialization and locking. `run.rs` owns validated trigger semantics. `supervisor.rs` contains the single condition loop.
