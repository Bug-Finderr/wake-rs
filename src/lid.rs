use crate::error::{AppError, Result};
#[cfg(any(windows, target_os = "macos"))]
use crate::platform;
use crate::run::{ProcessRef, RunSpec};
#[cfg(any(windows, target_os = "macos"))]
use crate::session::LidRestore;
use crate::session::{self, Session, WatchdogState};
use crate::sysutil;
#[cfg(target_os = "macos")]
use std::io::IsTerminal;
use std::thread::sleep;
use std::time::{Duration, Instant};

#[cfg(any(test, windows, target_os = "macos"))]
fn begin_change(
    write_marker: impl FnOnce() -> Result<()>,
    mutate: impl FnOnce() -> Result<()>,
    restore: impl FnOnce() -> Result<()>,
    clear_marker: impl FnOnce() -> Result<()>,
) -> Result<()> {
    write_marker()?;
    let Err(change_error) = mutate() else {
        return Ok(());
    };
    match restore().and_then(|()| clear_marker()) {
        Ok(()) => Err(change_error),
        Err(rollback_error) => Err(AppError::fail(format!(
            "{change_error}; rollback failed: {rollback_error}"
        ))),
    }
}

#[cfg(any(test, windows, target_os = "macos"))]
fn finish_change(
    restore: impl FnOnce() -> Result<()>,
    remove_session: impl FnOnce() -> Result<()>,
    clear_marker: impl FnOnce() -> Result<()>,
) -> Result<()> {
    restore()?;
    remove_session()?;
    clear_marker()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WatchdogHealth {
    Ready,
    Missing,
    Mismatch,
    Dead,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoverySession {
    Missing,
    Malformed,
    LiveOrdinary,
    LiveLid,
    Stale,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryWatchdog {
    Missing,
    Malformed,
    Valid {
        watchdog_live: bool,
        owner_live: bool,
        owner_matches_session: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryDecision {
    Keep,
    Remove,
    Restore,
    QuiesceRestore,
    Reject,
}

fn decide_recovery(
    marker: bool,
    session: RecoverySession,
    watchdog: RecoveryWatchdog,
) -> RecoveryDecision {
    use RecoveryDecision::{Keep, QuiesceRestore, Reject, Remove, Restore};
    use RecoverySession::{LiveLid, LiveOrdinary, Malformed, Missing, Stale};
    use RecoveryWatchdog::{Malformed as BadWatchdog, Missing as NoWatchdog, Valid};

    if !marker {
        return match (session, watchdog) {
            (Malformed | LiveLid, _) | (_, BadWatchdog) => Reject,
            (
                _,
                Valid {
                    watchdog_live: true,
                    ..
                }
                | Valid {
                    owner_live: true, ..
                },
            ) => Reject,
            (Stale, _) | (_, Valid { .. }) => Remove,
            (Missing | LiveOrdinary, NoWatchdog) => Keep,
        };
    }

    match (session, watchdog) {
        (_, BadWatchdog) | (LiveOrdinary, _) => Reject,
        (
            LiveLid | Malformed | Stale,
            Valid {
                watchdog_live,
                owner_live,
                owner_matches_session: false,
            },
        ) if watchdog_live || owner_live => Reject,
        (
            LiveLid,
            Valid {
                watchdog_live: true,
                owner_matches_session: true,
                ..
            },
        ) => Keep,
        (
            Missing | Malformed | Stale,
            Valid {
                watchdog_live: true,
                ..
            },
        ) => QuiesceRestore,
        (
            _,
            Valid {
                owner_live: true, ..
            },
        )
        | (LiveLid, _) => QuiesceRestore,
        (Missing | Malformed | Stale, NoWatchdog | Valid { .. }) => Restore,
    }
}

#[cfg(any(test, windows, target_os = "macos"))]
fn finish_after_mutation(
    primary: Result<()>,
    restore: impl FnOnce() -> Result<()>,
    remove_session: impl FnOnce() -> Result<()>,
    clear_marker: impl FnOnce() -> Result<()>,
) -> Result<()> {
    merge_results(
        primary,
        finish_change(restore, remove_session, clear_marker),
    )
}

#[cfg(any(test, windows, target_os = "macos"))]
fn merge_results(primary: Result<()>, cleanup: Result<()>) -> Result<()> {
    match (primary, cleanup) {
        (Err(primary), Err(cleanup)) => Err(AppError::fail(format!(
            "{primary}; cleanup failed: {cleanup}"
        ))),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

#[cfg(any(test, windows, target_os = "macos"))]
fn abort_start_cleanup(
    stop_owner: impl FnOnce() -> Result<()>,
    wait_helper: impl FnOnce() -> Result<()>,
    restore: impl FnOnce() -> Result<()>,
) -> Result<()> {
    stop_owner()?;
    wait_helper()?;
    restore()
}

fn watchdog_health(
    owner: &ProcessRef,
    state: Option<&WatchdogState>,
    is_live: impl FnOnce(&ProcessRef) -> bool,
) -> WatchdogHealth {
    let Some(state) = state else {
        return WatchdogHealth::Missing;
    };
    if &state.owner != owner {
        WatchdogHealth::Mismatch
    } else if is_live(&state.watchdog) {
        WatchdogHealth::Ready
    } else {
        WatchdogHealth::Dead
    }
}

pub struct Prepared {
    #[cfg(any(windows, target_os = "macos"))]
    marker: LidRestore,
}

pub fn prepare_start(spec: &RunSpec) -> Result<Option<Prepared>> {
    if !spec.even_lid {
        return Ok(None);
    }
    #[cfg(target_os = "linux")]
    return Err(AppError::usage(
        "--even-lid is unsupported on this platform",
    ));
    #[cfg(target_os = "macos")]
    {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return Err(AppError::fail(
                "--even-lid needs an interactive terminal for sudo authentication",
            ));
        }
        platform::authenticate_privilege()?;
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        let snapshot = platform::read_lid_snapshot()?;
        let marker = marker_from_snapshot(&snapshot);
        session::write_lid_restore(&marker)?;
        Ok(Some(Prepared { marker }))
    }
}

pub fn rollback_start(_prepared: &Prepared) -> Result<()> {
    #[cfg(target_os = "linux")]
    return Err(AppError::fail("unexpected Linux lid recovery state"));
    #[cfg(any(windows, target_os = "macos"))]
    {
        let snapshot = snapshot_from_marker(&_prepared.marker)?;
        if platform::lid_is_restored(&snapshot)? {
            session::clear_lid_restore(&_prepared.marker)
        } else {
            restore_elevated()?;
            ensure_marker_cleared()
        }
    }
}

pub fn launch_watchdog(saved: &Session) -> Result<()> {
    if !saved.spec.even_lid {
        return Ok(());
    }
    #[cfg(windows)]
    {
        let mut child = match sysutil::launch_elevated_self("__lid_watchdog__") {
            Ok(child) => child,
            Err(error) => return abort_watchdog_start(saved, error, || Ok(())),
        };
        match wait_for_child_ready(saved, Some(child.id()), || child.try_wait()) {
            Ok(()) => Ok(()),
            Err(error) => {
                abort_watchdog_start(saved, error, || wait_for_helper_exit(|| child.try_wait()))
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let command = mac_helper_command("__lid_watchdog__")?;
        let mut child = match sysutil::spawn_detached(&command) {
            Ok(child) => child,
            Err(error) => {
                return abort_watchdog_start(
                    saved,
                    AppError::fail(format!("could not launch lid watchdog: {error}")),
                    || Ok(()),
                );
            }
        };
        let mut status = || {
            child
                .try_wait()
                .map(|status| status.map(|status| status.code().unwrap_or(1) as u32))
                .map_err(AppError::from)
        };
        match wait_for_child_ready(saved, None, &mut status) {
            Ok(()) => Ok(()),
            Err(error) => abort_watchdog_start(saved, error, || wait_for_helper_exit(&mut status)),
        }
    }
    #[cfg(target_os = "linux")]
    Err(AppError::fail("lid watchdog is unavailable on Linux"))
}

#[cfg(any(windows, target_os = "macos"))]
fn abort_watchdog_start(
    saved: &Session,
    error: AppError,
    wait_helper: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let cleanup = abort_start_cleanup(
        || stop_exact_owner(saved),
        || {
            wait_helper()?;
            wait_recorded_watchdog_exit(saved)
        },
        || {
            rollback_pending_marker()?;
            finish_recovered_state(saved)
        },
    );
    merge_results(Err(error), cleanup)
}

#[cfg(any(windows, target_os = "macos"))]
fn wait_recorded_watchdog_exit(saved: &Session) -> Result<()> {
    let Some(state) = session::read_watchdog()? else {
        return Ok(());
    };
    if state.owner != saved.owner {
        return Err(AppError::fail(
            "lid watchdog state changed during startup cleanup",
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && sysutil::process_matches(&state.watchdog) {
        sleep(Duration::from_millis(100));
    }
    if sysutil::process_matches(&state.watchdog) {
        Err(AppError::fail(
            "lid watchdog remained alive after its owner",
        ))
    } else {
        Ok(())
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn wait_for_helper_exit(mut status: impl FnMut() -> Result<Option<u32>>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if status()?.is_some() {
            return Ok(());
        }
        sleep(Duration::from_millis(100));
    }
    Err(AppError::fail(
        "lid watchdog did not exit after startup failure",
    ))
}

#[cfg(any(windows, target_os = "macos"))]
fn stop_exact_owner(saved: &Session) -> Result<()> {
    session::request_stop(saved)?;
    if sysutil::process_matches(&saved.owner)
        && !sysutil::terminate_exact(&saved.owner)
        && sysutil::process_matches(&saved.owner)
    {
        Err(AppError::fail("could not stop lid session owner"))
    } else {
        Ok(())
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn rollback_pending_marker() -> Result<()> {
    let Some(marker) = session::read_lid_restore()? else {
        return Ok(());
    };
    let snapshot = snapshot_from_marker(&marker)?;
    if platform::lid_is_restored(&snapshot)? {
        session::clear_lid_restore(&marker)
    } else {
        restore_pending_marker()
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn wait_for_child_ready(
    saved: &Session,
    expected_pid: Option<u32>,
    mut child_status: impl FnMut() -> Result<Option<u32>>,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if ready_state(saved, expected_pid)?.is_some() {
            return if child_status()?.is_none() {
                Ok(())
            } else {
                Err(AppError::fail("lid watchdog exited during startup"))
            };
        }
        if let Some(code) = child_status()? {
            return Err(AppError::fail(format!(
                "lid watchdog exited during startup with code {code}"
            )));
        }
        sleep(Duration::from_millis(100));
    }
    Err(AppError::fail("lid watchdog did not publish ready state"))
}

fn ready_state(saved: &Session, expected_pid: Option<u32>) -> Result<Option<WatchdogState>> {
    let Some(state) = session::read_watchdog()? else {
        return Ok(None);
    };
    if state.owner != saved.owner || expected_pid.is_some_and(|pid| state.watchdog.pid != pid) {
        return Err(AppError::fail(
            "lid watchdog published mismatched ready state",
        ));
    }
    if !sysutil::process_matches(&state.watchdog) {
        return Err(AppError::fail("lid watchdog exited during startup"));
    }
    Ok(Some(state))
}

pub fn wait_ready(saved: &Session) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if session::stop_requested(saved)? {
            return Err(AppError::fail("session stopped during lid startup"));
        }
        if ready_state(saved, None)?.is_some() {
            return Ok(());
        }
        sleep(Duration::from_millis(100));
    }
    Err(AppError::fail("lid watchdog did not become ready"))
}

pub fn ensure_ready(saved: &Session) -> Result<()> {
    match watchdog_health(
        &saved.owner,
        session::read_watchdog()?.as_ref(),
        sysutil::process_matches,
    ) {
        WatchdogHealth::Ready => Ok(()),
        _ => Err(AppError::fail("lid watchdog is no longer active")),
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub fn run_watchdog() -> Result<()> {
    let saved = session::read_saved_for_recovery()?
        .ok_or_else(|| AppError::fail("lid watchdog found no session"))?;
    if !saved.spec.even_lid || !saved.matches_live_process() {
        return Err(AppError::fail(
            "lid watchdog requires a matching live even-lid session",
        ));
    }
    let marker = session::read_lid_restore()?
        .ok_or_else(|| AppError::fail("lid watchdog found no restoration marker"))?;
    let snapshot = snapshot_from_marker(&marker)?;
    #[cfg(windows)]
    let owner = sysutil::OwnerHandle::open(&saved.owner)?;

    begin_change(
        || session::write_lid_restore(&marker),
        || platform::disable_lid(&snapshot),
        || platform::restore_lid(&snapshot),
        || session::clear_lid_restore(&marker),
    )?;
    let primary = (|| -> Result<()> {
        session::write_watchdog(&WatchdogState {
            owner: saved.owner.clone(),
            watchdog: sysutil::capture_process(sysutil::current_pid())?,
        })?;
        loop {
            #[cfg(windows)]
            let owner_alive = owner.alive()?;
            #[cfg(target_os = "macos")]
            let owner_alive = sysutil::process_matches(&saved.owner);
            if !owner_alive {
                return Ok(());
            }
            if !platform::lid_override_is_active(&snapshot)? {
                return Err(AppError::fail(
                    "lid protection ended because the power configuration changed",
                ));
            }
            sleep(Duration::from_secs(1));
        }
    })();
    let primary = if primary.is_err() {
        #[cfg(windows)]
        let stopped = owner
            .alive()
            .and_then(|alive| if alive { owner.terminate() } else { Ok(()) });
        #[cfg(target_os = "macos")]
        let stopped = if sysutil::process_matches(&saved.owner) {
            stop_exact_owner(&saved)
        } else {
            Ok(())
        };
        merge_results(primary, stopped)
    } else {
        primary
    };

    finish_after_mutation(
        primary,
        || platform::restore_lid(&snapshot),
        || remove_matching_session(&saved),
        || {
            session::clear_stop(&saved)?;
            session::remove_watchdog_if_owner(&saved.owner)?;
            session::clear_lid_restore(&marker)
        },
    )
}

#[cfg(any(windows, target_os = "macos"))]
pub fn run_restore() -> Result<()> {
    let marker = session::read_lid_restore()?
        .ok_or_else(|| AppError::fail("no lid restoration marker found"))?;
    let snapshot = snapshot_from_marker(&marker)?;
    restore_and_clear(&marker, &snapshot)
}

pub fn finish_stop(_saved: &Session) -> Result<()> {
    #[cfg(target_os = "linux")]
    return Err(AppError::fail("unexpected Linux lid session"));
    #[cfg(any(windows, target_os = "macos"))]
    {
        let watchdog = session::read_watchdog()?;
        quiesce_recovery(Some(_saved), watchdog.as_ref())?;
        restore_pending_marker()?;
        finish_recovered_state(_saved)
    }
}

pub fn recover_foreground() -> Result<()> {
    let _lock = session::acquire_lock()?;
    recover_unlocked()
}

pub fn recover_unlocked() -> Result<()> {
    let marker = session::read_lid_restore()?;
    let saved = session::read_saved_for_recovery();
    let watchdog = session::read_watchdog();
    let session_state = recovery_session(&saved);
    let watchdog_state = recovery_watchdog(&saved, &watchdog);
    let decision = decide_recovery(marker.is_some(), session_state, watchdog_state);
    match decision {
        RecoveryDecision::Keep => Ok(()),
        RecoveryDecision::Reject => match (saved, watchdog) {
            (Err(error), _) | (_, Err(error)) => Err(error),
            _ => Err(AppError::fail("inconsistent lid recovery state")),
        },
        RecoveryDecision::Remove => {
            if session_state == RecoverySession::Stale {
                session::remove_state_file()?;
            }
            if matches!(watchdog_state, RecoveryWatchdog::Valid { .. }) {
                session::remove_watchdog_file()?;
            }
            Ok(())
        }
        RecoveryDecision::Restore | RecoveryDecision::QuiesceRestore => {
            #[cfg(target_os = "linux")]
            return Err(AppError::fail(
                "lid restoration marker found on unsupported platform",
            ));
            #[cfg(any(windows, target_os = "macos"))]
            {
                let marker = marker.expect("restore decision requires a marker");
                snapshot_from_marker(&marker)?;
                if decision == RecoveryDecision::QuiesceRestore {
                    quiesce_recovery(
                        saved.as_ref().ok().and_then(Option::as_ref),
                        watchdog.as_ref().ok().and_then(Option::as_ref),
                    )?;
                }
                restore_pending_marker()?;
                session::remove_state_file()?;
                session::remove_watchdog_file()?;
                session::reconcile_stop()
            }
        }
    }
}

fn recovery_session(saved: &Result<Option<Session>>) -> RecoverySession {
    match saved {
        Err(_) => RecoverySession::Malformed,
        Ok(None) => RecoverySession::Missing,
        Ok(Some(saved)) if !saved.matches_live_process() => RecoverySession::Stale,
        Ok(Some(saved)) if saved.spec.even_lid => RecoverySession::LiveLid,
        Ok(Some(_)) => RecoverySession::LiveOrdinary,
    }
}

fn recovery_watchdog(
    saved: &Result<Option<Session>>,
    watchdog: &Result<Option<WatchdogState>>,
) -> RecoveryWatchdog {
    match watchdog {
        Err(_) => RecoveryWatchdog::Malformed,
        Ok(None) => RecoveryWatchdog::Missing,
        Ok(Some(state)) => RecoveryWatchdog::Valid {
            watchdog_live: sysutil::process_matches(&state.watchdog),
            owner_live: sysutil::process_matches(&state.owner),
            owner_matches_session: saved
                .as_ref()
                .ok()
                .and_then(Option::as_ref)
                .is_some_and(|saved| saved.owner == state.owner),
        },
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn quiesce_recovery(saved: Option<&Session>, watchdog: Option<&WatchdogState>) -> Result<()> {
    let owner = saved
        .filter(|saved| saved.spec.even_lid)
        .map(|saved| &saved.owner)
        .or_else(|| watchdog.map(|state| &state.owner));
    if let Some(saved) = saved.filter(|saved| saved.spec.even_lid) {
        session::request_stop(saved)?;
    }
    if let Some(owner) = owner
        && sysutil::process_matches(owner)
        && !sysutil::terminate_exact(owner)
        && sysutil::process_matches(owner)
    {
        return Err(AppError::fail("could not stop lid session owner"));
    }
    if let Some(state) = watchdog {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && sysutil::process_matches(&state.watchdog) {
            sleep(Duration::from_millis(100));
        }
        if sysutil::process_matches(&state.watchdog) {
            return Err(AppError::fail("lid watchdog did not exit after its owner"));
        }
    }
    Ok(())
}

#[cfg(any(windows, target_os = "macos"))]
fn restore_pending_marker() -> Result<()> {
    if session::read_lid_restore()?.is_none() {
        return Ok(());
    }
    restore_elevated()?;
    ensure_marker_cleared()
}

#[cfg(any(windows, target_os = "macos"))]
fn remove_matching_session(saved: &Session) -> Result<()> {
    match session::read_saved_for_recovery()? {
        None => Ok(()),
        Some(actual) if actual.owner == saved.owner && actual.matches_live_process() => Err(
            AppError::fail("lid session owner is still running after watchdog exit"),
        ),
        Some(actual) if actual.owner == saved.owner => {
            session::remove_if_matches(saved).map(|_| ())
        }
        Some(_) => Err(AppError::fail(
            "session owner changed before lid restoration",
        )),
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn finish_recovered_state(saved: &Session) -> Result<()> {
    session::remove_if_matches(saved)?;
    session::clear_stop(saved)?;
    session::remove_watchdog_if_owner(&saved.owner)?;
    Ok(())
}

#[cfg(any(windows, target_os = "macos"))]
fn ensure_marker_cleared() -> Result<()> {
    if session::read_lid_restore()?.is_none() {
        Ok(())
    } else {
        Err(AppError::fail("lid restoration marker was not cleared"))
    }
}

#[cfg(windows)]
fn marker_from_snapshot(snapshot: &platform::LidSnapshot) -> LidRestore {
    LidRestore::Windows {
        scheme_guid: snapshot.scheme_guid.clone(),
        ac_action: snapshot.ac_action,
        dc_action: snapshot.dc_action,
    }
}

#[cfg(target_os = "macos")]
fn marker_from_snapshot(snapshot: &platform::LidSnapshot) -> LidRestore {
    LidRestore::Macos {
        sleep_disabled: snapshot.sleep_disabled,
    }
}

#[cfg(windows)]
fn snapshot_from_marker(marker: &LidRestore) -> Result<platform::LidSnapshot> {
    match marker {
        LidRestore::Windows {
            scheme_guid,
            ac_action,
            dc_action,
        } => platform::LidSnapshot::new(scheme_guid.clone(), *ac_action, *dc_action),
        LidRestore::Macos { .. } => Err(AppError::fail(
            "macOS lid restoration marker found on Windows",
        )),
    }
}

#[cfg(target_os = "macos")]
fn snapshot_from_marker(marker: &LidRestore) -> Result<platform::LidSnapshot> {
    match marker {
        LidRestore::Macos { sleep_disabled } => Ok(platform::LidSnapshot {
            sleep_disabled: *sleep_disabled,
        }),
        LidRestore::Windows { .. } => Err(AppError::fail(
            "Windows lid restoration marker found on macOS",
        )),
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn restore_and_clear(marker: &LidRestore, snapshot: &platform::LidSnapshot) -> Result<()> {
    platform::restore_lid(snapshot)?;
    session::clear_lid_restore(marker)
}

#[cfg(windows)]
fn restore_elevated() -> Result<()> {
    let code = sysutil::run_elevated_self("__lid_restore__")?;
    if code == 0 {
        Ok(())
    } else {
        Err(AppError::fail(format!(
            "elevated lid restoration exited with code {code}"
        )))
    }
}

#[cfg(target_os = "macos")]
fn restore_elevated() -> Result<()> {
    let run = || -> Result<bool> {
        let command = mac_helper_command("__lid_restore__")?;
        Ok(std::process::Command::new(&command[0])
            .args(&command[1..])
            .status()?
            .success())
    };
    if run()? {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(AppError::fail(
            "lid recovery needs an interactive terminal for sudo authentication",
        ));
    }
    platform::authenticate_privilege()?;
    if run()? {
        Ok(())
    } else {
        Err(AppError::fail("elevated lid restoration failed"))
    }
}

#[cfg(target_os = "macos")]
fn mac_helper_command(action: &str) -> Result<Vec<String>> {
    Ok(vec![
        "/usr/bin/sudo".into(),
        "-n".into(),
        "/usr/bin/env".into(),
        format!("WAKE_STATE_DIR={}", session::state_dir().display()),
        sysutil::self_exe()?,
        action.into(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn process(pid: u32) -> ProcessRef {
        ProcessRef {
            pid,
            start: u64::from(pid) * 10,
            command: format!("wake-{pid}"),
        }
    }

    #[test]
    fn startup_records_marker_before_mutation() {
        let events = RefCell::new(Vec::new());
        begin_change(
            || {
                events.borrow_mut().push("marker");
                Ok(())
            },
            || {
                events.borrow_mut().push("mutate");
                Ok(())
            },
            || Ok(()),
            || Ok(()),
        )
        .unwrap();

        assert_eq!(*events.borrow(), ["marker", "mutate"]);
    }

    #[test]
    fn startup_failure_restores_before_clearing_marker() {
        let events = RefCell::new(Vec::new());
        let result = begin_change(
            || {
                events.borrow_mut().push("marker");
                Ok(())
            },
            || {
                events.borrow_mut().push("mutate");
                Err(AppError::fail("change failed"))
            },
            || {
                events.borrow_mut().push("restore");
                Ok(())
            },
            || {
                events.borrow_mut().push("clear");
                Ok(())
            },
        );

        assert_eq!(result.unwrap_err().message(), "change failed");
        assert_eq!(*events.borrow(), ["marker", "mutate", "restore", "clear"]);
    }

    #[test]
    fn failed_restore_retains_marker() {
        let events = RefCell::new(Vec::new());
        let result = begin_change(
            || {
                events.borrow_mut().push("marker");
                Ok(())
            },
            || Err(AppError::fail("change failed")),
            || {
                events.borrow_mut().push("restore");
                Err(AppError::fail("restore failed"))
            },
            || {
                events.borrow_mut().push("clear");
                Ok(())
            },
        );

        assert!(result.unwrap_err().message().contains("restore failed"));
        assert_eq!(*events.borrow(), ["marker", "restore"]);
    }

    #[test]
    fn natural_cleanup_restores_then_removes_session_then_marker() {
        let events = RefCell::new(Vec::new());
        finish_change(
            || {
                events.borrow_mut().push("restore");
                Ok(())
            },
            || {
                events.borrow_mut().push("session");
                Ok(())
            },
            || {
                events.borrow_mut().push("marker");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(*events.borrow(), ["restore", "session", "marker"]);
    }

    #[test]
    fn watchdog_health_checks_exact_identities_and_liveness() {
        let owner = process(10);
        let watchdog = process(20);
        let ready = WatchdogState {
            owner: owner.clone(),
            watchdog: watchdog.clone(),
        };
        let wrong = WatchdogState {
            owner: process(11),
            watchdog: watchdog.clone(),
        };

        assert_eq!(
            watchdog_health(&owner, Some(&ready), |candidate| candidate == &watchdog),
            WatchdogHealth::Ready
        );
        assert_eq!(
            watchdog_health(&owner, None, |_| true),
            WatchdogHealth::Missing
        );
        assert_eq!(
            watchdog_health(&owner, Some(&wrong), |_| true),
            WatchdogHealth::Mismatch
        );
        assert_eq!(
            watchdog_health(&owner, Some(&ready), |_| false),
            WatchdogHealth::Dead
        );
    }

    #[test]
    fn watchdog_state_json_is_strict() {
        let state = WatchdogState {
            owner: process(10),
            watchdog: process(20),
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<WatchdogState>(&json).unwrap(), state);
        let unknown = json.replacen('{', r#"{"extra":true,"#, 1);
        assert!(serde_json::from_str::<WatchdogState>(&unknown).is_err());
    }

    #[test]
    fn recovery_decision_covers_serialized_state_without_unsafe_guessing() {
        use RecoveryDecision::{Keep, QuiesceRestore, Reject, Remove, Restore};
        use RecoverySession::{LiveLid, LiveOrdinary, Malformed, Missing, Stale};
        use RecoveryWatchdog::{Malformed as BadWatchdog, Missing as NoWatchdog, Valid};

        let cases = [
            (false, Missing, NoWatchdog, Keep),
            (false, LiveOrdinary, NoWatchdog, Keep),
            (false, Stale, NoWatchdog, Remove),
            (false, LiveLid, NoWatchdog, Reject),
            (false, Malformed, NoWatchdog, Reject),
            (false, Missing, BadWatchdog, Reject),
            (
                false,
                Missing,
                Valid {
                    watchdog_live: true,
                    owner_live: true,
                    owner_matches_session: false,
                },
                Reject,
            ),
            (
                true,
                LiveLid,
                Valid {
                    watchdog_live: true,
                    owner_live: true,
                    owner_matches_session: true,
                },
                Keep,
            ),
            (
                true,
                LiveLid,
                Valid {
                    watchdog_live: true,
                    owner_live: true,
                    owner_matches_session: false,
                },
                Reject,
            ),
            (true, LiveLid, NoWatchdog, QuiesceRestore),
            (
                true,
                Malformed,
                Valid {
                    watchdog_live: true,
                    owner_live: true,
                    owner_matches_session: false,
                },
                Reject,
            ),
            (
                true,
                Stale,
                Valid {
                    watchdog_live: true,
                    owner_live: false,
                    owner_matches_session: false,
                },
                Reject,
            ),
            (
                true,
                Missing,
                Valid {
                    watchdog_live: true,
                    owner_live: false,
                    owner_matches_session: false,
                },
                QuiesceRestore,
            ),
            (
                true,
                Malformed,
                Valid {
                    watchdog_live: false,
                    owner_live: false,
                    owner_matches_session: false,
                },
                Restore,
            ),
            (true, Missing, NoWatchdog, Restore),
            (true, LiveOrdinary, NoWatchdog, Reject),
            (true, Missing, BadWatchdog, Reject),
        ];
        for (marker, session, watchdog, expected) in cases {
            assert_eq!(decide_recovery(marker, session, watchdog), expected);
        }
    }

    #[test]
    fn post_mutation_error_restores_before_cleanup_and_is_preserved() {
        let events = RefCell::new(Vec::new());
        let result = finish_after_mutation(
            Err(AppError::fail("monitor failed")),
            || {
                events.borrow_mut().push("restore");
                Ok(())
            },
            || {
                events.borrow_mut().push("session");
                Ok(())
            },
            || {
                events.borrow_mut().push("marker");
                Ok(())
            },
        );

        assert_eq!(result.unwrap_err().message(), "monitor failed");
        assert_eq!(*events.borrow(), ["restore", "session", "marker"]);
    }

    #[test]
    fn post_mutation_restore_failure_retains_session_and_marker() {
        let events = RefCell::new(Vec::new());
        let result = finish_after_mutation(
            Err(AppError::fail("monitor failed")),
            || {
                events.borrow_mut().push("restore");
                Err(AppError::fail("restore failed"))
            },
            || {
                events.borrow_mut().push("session");
                Ok(())
            },
            || {
                events.borrow_mut().push("marker");
                Ok(())
            },
        );

        assert!(result.unwrap_err().message().contains("restore failed"));
        assert_eq!(*events.borrow(), ["restore"]);
    }

    #[test]
    fn failed_helper_start_stops_owner_and_proves_exit_before_restore() {
        let events = RefCell::new(Vec::new());
        abort_start_cleanup(
            || {
                events.borrow_mut().push("stop-owner");
                Ok(())
            },
            || {
                events.borrow_mut().push("wait-helper");
                Ok(())
            },
            || {
                events.borrow_mut().push("restore");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*events.borrow(), ["stop-owner", "wait-helper", "restore"]);
    }

    #[test]
    fn failed_helper_wait_prevents_restore() {
        let events = RefCell::new(Vec::new());
        let result = abort_start_cleanup(
            || {
                events.borrow_mut().push("stop-owner");
                Ok(())
            },
            || {
                events.borrow_mut().push("wait-helper");
                Err(AppError::fail("helper still running"))
            },
            || {
                events.borrow_mut().push("restore");
                Ok(())
            },
        );

        assert_eq!(result.unwrap_err().message(), "helper still running");
        assert_eq!(*events.borrow(), ["stop-owner", "wait-helper"]);
    }
}
