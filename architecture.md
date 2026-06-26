# Architecture

`wake` is a single binary with no daemon. It drives the OS's native sleep-inhibition tool, records a
session in a state file, and reconciles that state on every invocation.

## Modules

```mermaid
graph TD
  main["main.rs<br/>dispatch · errors · help"] --> commands
  main --> supervisor
  main --> interactive["interactive.rs<br/>(unix picker)"]
  commands["commands.rs<br/>start/status/stop · even-lid recovery"] --> session
  commands --> platform
  commands --> sysutil
  commands --> durations
  supervisor["supervisor.rs<br/>charge + lid loops"] --> platform
  supervisor --> sysutil
  supervisor --> session
  session["session.rs<br/>state file · lock · identity"] --> sysutil
  session --> platform
  sysutil["sysutil.rs<br/>spawn · liveness · terminate · Job Object"] --> ext1(["sysinfo"])
  platform -. cfg .-> windows["windows.rs"]
  platform -. cfg .-> macos["macos.rs"]
  platform -. cfg .-> linux["linux.rs"]
```

`platform` is a trait-free abstraction: each OS module exposes the same free functions, selected at
compile time by `cfg` and re-exported from `platform/mod.rs`. Even-lid functions are real on macOS and
unsupported stubs elsewhere.

## Command dispatch

```mermaid
flowchart TD
  A["wake &lt;args&gt;"] --> B{first arg}
  B -->|help / version| H[print and exit]
  B -->|status / stop| L[lock to recover stale state to read session]
  B -->|forever / duration / flags| ST[start]
  B -->|__supervise_charge__ / __supervise_lid__| SUP[detached supervisor loop]
  B -->|none| C{TTY and interactive?}
  C -->|yes - macOS/Linux| P[picker]
  C -->|no / Windows| ST
```

## Session lifecycle

A session is the OS sleep-inhibitor process plus a `session.properties` record. The record stores the
pid **and** a process-identity fingerprint (start time, exe, command line); every read verifies the pid
is still that same live process before trusting it, so a recycled pid never looks like a live session.

```mermaid
sequenceDiagram
  participant U as user
  participant W as wake
  participant K as keep-awake child
  participant F as state file
  U->>W: wake 1h
  W->>W: acquire lock, reconcile stale state
  W->>K: spawn detached (caffeinate / PowerShell / systemd-inhibit)
  W->>W: verify child alive (~300ms)
  W->>F: write {pid, identity, ends_at}
  W-->>U: session active
  Note over K: blocks sleep until timeout / pid gone / stop
```

## Supervisors

`--until-charge` (all OSes) and `--even-lid` (macOS) need a process that outlives the foreground
command, so `wake` spawns a detached copy of itself (`__supervise_*`). The supervisor owns the
keep-awake child and polls until its condition is met. The recorded session pid is the supervisor.

```mermaid
sequenceDiagram
  participant W as wake (foreground)
  participant S as supervisor (detached self)
  participant K as keep-awake child
  W->>S: spawn __supervise_charge__
  S->>K: spawn child
  S->>S: write session (pid = supervisor)
  W-->>W: read session, print, exit
  loop poll
    S->>S: battery / pid / clock / charge
  end
  Note over S,K: teardown kills K and (lid) restores SleepDisabled
```

Teardown must survive a forcible `wake stop`:

- **Windows** — the child is placed in a kill-on-close Job Object; when `stop` calls `TerminateProcess`
  on the supervisor, the job closes and the OS kills the child. No orphan keeps the machine awake.
- **Unix** — the supervisor installs a SIGTERM/SIGINT flag that breaks its poll loop into the normal
  cleanup path; `stop` also re-verifies and restores macOS `SleepDisabled` as a safety net.

## Key decisions

- **No `dyn`/trait objects** for platforms — `cfg`-selected free functions; only the target OS compiles.
- **Process identity over bare pid** — guards against pid reuse without a daemon.
- **Native `std::fs` file locking** (Rust 1.89+) instead of a crate.
- **Shell out to platform tools**, exactly as the original, passing values as argv (never a shell
  string) so app/pid names can't inject commands. The one generated script (Windows PowerShell) embeds
  only validated numerics and a base64-encoded body.
- Minimal `unsafe`: confined to `sysutil.rs` for the Windows Job Object and clearing handle
  inheritance, via `windows-sys`.
