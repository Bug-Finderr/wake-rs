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

#[cfg(any(test, windows, target_os = "macos"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OrphanDecision {
    None,
    Remove,
    Invalid,
}

#[cfg(any(test, windows, target_os = "macos"))]
fn orphan_watchdog_decision(
    state_exists: bool,
    watchdog_live: bool,
    owner_live: bool,
) -> OrphanDecision {
    match (state_exists, watchdog_live || owner_live) {
        (false, _) => OrphanDecision::None,
        (true, false) => OrphanDecision::Remove,
        (true, true) => OrphanDecision::Invalid,
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryDecision {
    None,
    Restore,
    Invalid,
}

#[cfg(test)]
fn recovery_decision(
    session: Option<(bool, bool)>,
    marker_exists: bool,
    watchdog: WatchdogHealth,
) -> RecoveryDecision {
    match (session, marker_exists) {
        (None, false) => RecoveryDecision::None,
        (None, true) => RecoveryDecision::Restore,
        (Some((true, true)), true) if watchdog == WatchdogHealth::Ready => RecoveryDecision::None,
        (Some((true, _)), true) => RecoveryDecision::Restore,
        (Some((false, true)), true) | (Some((true, true)), false) => RecoveryDecision::Invalid,
        (Some(_), _) => RecoveryDecision::None,
    }
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
        let mut child = sysutil::launch_elevated_self("__lid_watchdog__")?;
        wait_for_child_ready(saved, Some(child.id()), || child.try_wait())
    }
    #[cfg(target_os = "macos")]
    {
        let command = mac_helper_command("__lid_watchdog__")?;
        let mut child = sysutil::spawn_detached(&command)
            .map_err(|error| AppError::fail(format!("could not launch lid watchdog: {error}")))?;
        wait_for_child_ready(saved, None, || {
            child
                .try_wait()
                .map(|status| status.map(|status| status.code().unwrap_or(1) as u32))
                .map_err(AppError::from)
        })
    }
    #[cfg(target_os = "linux")]
    Err(AppError::fail("lid watchdog is unavailable on Linux"))
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
    let state = WatchdogState {
        owner: saved.owner.clone(),
        watchdog: sysutil::capture_process(sysutil::current_pid())?,
    };
    if let Err(error) = session::write_watchdog(&state) {
        let rollback = restore_and_clear(&marker, &snapshot);
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback) => Err(AppError::fail(format!(
                "{error}; rollback failed: {rollback}"
            ))),
        };
    }

    let mut protection_lost = false;
    loop {
        #[cfg(windows)]
        let owner_alive = owner.alive()?;
        #[cfg(target_os = "macos")]
        let owner_alive = sysutil::process_matches(&saved.owner);
        if !owner_alive {
            break;
        }
        if !platform::lid_override_is_active(&snapshot)? {
            protection_lost = true;
            #[cfg(windows)]
            owner.terminate()?;
            #[cfg(target_os = "macos")]
            if !sysutil::terminate_exact(&saved.owner) {
                return Err(AppError::fail(
                    "lid protection was lost and the supervisor could not be stopped",
                ));
            }
            break;
        }
        sleep(Duration::from_secs(1));
    }

    finish_change(
        || platform::restore_lid(&snapshot),
        || remove_matching_session(&saved),
        || session::clear_lid_restore(&marker),
    )?;
    session::clear_stop(&saved)?;
    session::remove_watchdog_if_owner(&saved.owner)?;
    if protection_lost {
        Err(AppError::fail(
            "lid protection ended because the active power configuration changed",
        ))
    } else {
        Ok(())
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub fn run_restore() -> Result<()> {
    let marker = session::read_lid_restore()?
        .ok_or_else(|| AppError::fail("no lid restoration marker found"))?;
    let snapshot = snapshot_from_marker(&marker)?;
    restore_and_clear(&marker, &snapshot)
}

pub fn finish_stop(saved: &Session) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if session::read_lid_restore()?.is_none() {
            return finish_recovered_state(saved);
        }
        sleep(Duration::from_millis(100));
    }
    restore_elevated()?;
    ensure_marker_cleared()?;
    finish_recovered_state(saved)
}

pub fn recover_foreground() -> Result<()> {
    let _lock = session::acquire_lock()?;
    recover_unlocked()
}

pub fn recover_unlocked() -> Result<()> {
    let marker = session::read_lid_restore()?;
    let saved = session::read_saved_for_recovery();
    if marker.is_none() {
        #[cfg(any(windows, target_os = "macos"))]
        if let Some(state) = session::read_watchdog()? {
            match orphan_watchdog_decision(
                true,
                sysutil::process_matches(&state.watchdog),
                sysutil::process_matches(&state.owner),
            ) {
                OrphanDecision::Remove => session::remove_watchdog_file()?,
                OrphanDecision::Invalid => {
                    return Err(AppError::fail(
                        "live lid watchdog state has no restoration marker",
                    ));
                }
                OrphanDecision::None => {}
            }
        }
        return match saved {
            Ok(Some(saved)) if saved.matches_live_process() && saved.spec.even_lid => Err(
                AppError::fail("live even-lid session has no restoration marker"),
            ),
            Ok(Some(saved)) if !saved.matches_live_process() => session::remove_state_file(),
            result => result.map(|_| ()),
        };
    }
    #[cfg(target_os = "linux")]
    return Err(AppError::fail(
        "lid restoration marker found on unsupported platform",
    ));
    #[cfg(any(windows, target_os = "macos"))]
    {
        let marker = marker.expect("checked above");
        snapshot_from_marker(&marker)?;
        if let Ok(Some(saved)) = &saved
            && saved.matches_live_process()
        {
            if !saved.spec.even_lid {
                return Err(AppError::fail(
                    "live ordinary session conflicts with lid restoration state",
                ));
            }
            let watchdog = session::read_watchdog().ok().flatten();
            if watchdog_health(&saved.owner, watchdog.as_ref(), sysutil::process_matches)
                == WatchdogHealth::Ready
            {
                return Ok(());
            }
            session::request_stop(saved)?;
            if !sysutil::terminate_exact(&saved.owner) && saved.matches_live_process() {
                return Err(AppError::fail(
                    "could not stop unprotected even-lid supervisor",
                ));
            }
        }
        restore_elevated()?;
        ensure_marker_cleared()?;
        session::remove_state_file()?;
        session::remove_watchdog_file()?;
        session::reconcile_stop()
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn remove_matching_session(saved: &Session) -> Result<()> {
    match session::read_saved_for_recovery()? {
        None => Ok(()),
        Some(actual) if actual.owner == saved.owner => {
            session::remove_if_matches(saved).map(|_| ())
        }
        Some(_) => Err(AppError::fail(
            "session owner changed before lid restoration",
        )),
    }
}

fn finish_recovered_state(saved: &Session) -> Result<()> {
    session::remove_if_matches(saved)?;
    session::clear_stop(saved)?;
    session::remove_watchdog_if_owner(&saved.owner)?;
    Ok(())
}

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

#[cfg(target_os = "linux")]
fn restore_elevated() -> Result<()> {
    Err(AppError::fail("lid restoration is unavailable on Linux"))
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
    fn watchdog_state_json_is_strict() -> Result<()> {
        let state = WatchdogState {
            owner: process(10),
            watchdog: process(20),
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<WatchdogState>(&json).unwrap(), state);
        let unknown = json.replacen('{', r#"{"extra":true,"#, 1);
        assert!(serde_json::from_str::<WatchdogState>(&unknown).is_err());
        Ok(())
    }

    #[test]
    fn recovery_decision_covers_owner_and_marker_states() {
        let cases = [
            (None, false, WatchdogHealth::Missing, RecoveryDecision::None),
            (
                Some((false, true)),
                false,
                WatchdogHealth::Missing,
                RecoveryDecision::None,
            ),
            (
                Some((true, true)),
                false,
                WatchdogHealth::Missing,
                RecoveryDecision::Invalid,
            ),
            (
                Some((false, true)),
                true,
                WatchdogHealth::Missing,
                RecoveryDecision::Invalid,
            ),
            (
                Some((true, true)),
                true,
                WatchdogHealth::Ready,
                RecoveryDecision::None,
            ),
            (
                Some((true, true)),
                true,
                WatchdogHealth::Dead,
                RecoveryDecision::Restore,
            ),
            (
                Some((true, false)),
                true,
                WatchdogHealth::Missing,
                RecoveryDecision::Restore,
            ),
            (
                None,
                true,
                WatchdogHealth::Missing,
                RecoveryDecision::Restore,
            ),
        ];
        for (session, marker, health, expected) in cases {
            assert_eq!(recovery_decision(session, marker, health), expected);
        }
    }

    #[test]
    fn orphan_watchdog_state_is_removed_only_when_both_processes_are_dead() {
        assert_eq!(
            orphan_watchdog_decision(false, false, false),
            OrphanDecision::None
        );
        assert_eq!(
            orphan_watchdog_decision(true, false, false),
            OrphanDecision::Remove
        );
        assert_eq!(
            orphan_watchdog_decision(true, true, false),
            OrphanDecision::Invalid
        );
        assert_eq!(
            orphan_watchdog_decision(true, false, true),
            OrphanDecision::Invalid
        );
    }
}
