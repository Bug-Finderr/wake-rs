use crate::error::{AppError, Result};
#[cfg(any(windows, target_os = "macos"))]
use crate::platform;
use crate::run::RunSpec;
#[cfg(any(windows, target_os = "macos"))]
use crate::session::LidRestore;
use crate::session::{self, LeaseRef, Session, WatchdogState};
use crate::sysutil;
#[cfg(target_os = "macos")]
use std::io::IsTerminal;
use std::thread::sleep;
use std::time::{Duration, Instant};

#[cfg(any(test, windows, target_os = "macos"))]
fn begin_change(
    publish_watchdog: impl FnOnce() -> Result<()>,
    mutate: impl FnOnce() -> Result<()>,
    publish_ready: impl FnOnce() -> Result<()>,
    restore: impl FnOnce() -> Result<()>,
    clear_marker: impl FnOnce() -> Result<()>,
    clear_watchdog: impl FnOnce() -> Result<()>,
) -> Result<()> {
    publish_watchdog()?;
    let Err(change_error) = mutate().and_then(|()| publish_ready()) else {
        return Ok(());
    };
    match restore()
        .and_then(|()| clear_marker())
        .and_then(|()| clear_watchdog())
    {
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
    clear_stop: impl FnOnce() -> Result<()>,
    clear_marker: impl FnOnce() -> Result<()>,
    clear_watchdog: impl FnOnce() -> Result<()>,
) -> Result<()> {
    restore()?;
    remove_session()?;
    clear_stop()?;
    clear_marker()?;
    clear_watchdog()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WatchdogHealth {
    Ready,
    Starting,
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
        (LiveLid, NoWatchdog) => Keep,
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
    clear_stop: impl FnOnce() -> Result<()>,
    clear_marker: impl FnOnce() -> Result<()>,
    clear_watchdog: impl FnOnce() -> Result<()>,
) -> Result<()> {
    merge_results(
        primary,
        finish_change(
            restore,
            remove_session,
            clear_stop,
            clear_marker,
            clear_watchdog,
        ),
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
    owner: &LeaseRef,
    state: Option<&WatchdogState>,
    is_live: impl FnOnce(&LeaseRef) -> Result<bool>,
) -> Result<WatchdogHealth> {
    let Some(state) = state else {
        return Ok(WatchdogHealth::Missing);
    };
    if &state.owner != owner {
        Ok(WatchdogHealth::Mismatch)
    } else if !is_live(&state.watchdog)? {
        Ok(WatchdogHealth::Dead)
    } else if state.ready {
        Ok(WatchdogHealth::Ready)
    } else {
        Ok(WatchdogHealth::Starting)
    }
}

pub struct Prepared {
    #[cfg(any(windows, target_os = "macos"))]
    marker: LidRestore,
}

pub fn uses_watchdog(spec: &RunSpec) -> bool {
    #[cfg(any(windows, target_os = "macos"))]
    {
        spec.even_lid
    }
    #[cfg(target_os = "linux")]
    {
        let _ = spec;
        false
    }
}

pub fn prepare_start(spec: &RunSpec) -> Result<Option<Prepared>> {
    if !spec.even_lid {
        return Ok(None);
    }
    #[cfg(target_os = "linux")]
    return Ok(None);
    #[cfg(target_os = "macos")]
    {
        platform::trusted_helper_executable()?;
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

pub fn launch_watchdog(saved: &Session, lock: session::LockGuard) -> Result<()> {
    if !uses_watchdog(&saved.spec) {
        return Ok(());
    }
    #[cfg(windows)]
    let helper_state = match session::absolute_state_dir() {
        Ok(dir) => encode_helper_state_dir(&dir),
        Err(error) => {
            drop(lock);
            return abort_watchdog_start(saved, error, || Ok(()));
        }
    };
    #[cfg(any(windows, target_os = "macos"))]
    let reservation = match session::reserve_process_lease() {
        Ok(reservation) => reservation,
        Err(error) => {
            drop(lock);
            return abort_watchdog_start(saved, error, || Ok(()));
        }
    };
    #[cfg(any(windows, target_os = "macos"))]
    if let Err(error) = session::write_watchdog(&WatchdogState {
        owner: saved.owner.clone(),
        watchdog: LeaseRef {
            pid: 0,
            token: reservation.token().into(),
        },
        ready: false,
    }) {
        drop(lock);
        return abort_watchdog_start(saved, error, || Ok(()));
    }
    #[cfg(windows)]
    {
        let command = format!("__lid_watchdog__ {} {helper_state}", reservation.token());
        let mut child = match sysutil::launch_elevated_self(&command) {
            Ok(child) => child,
            Err(error) => {
                drop(lock);
                return abort_watchdog_start(saved, error, || Ok(()));
            }
        };
        let pid = child.id();
        if let Err(error) = wait_for_child_claimed(&reservation, || child.try_wait()) {
            drop(lock);
            return abort_watchdog_start(saved, error, || {
                wait_for_helper_exit(|| child.try_wait())
            });
        }
        let token = reservation.commit();
        drop(lock);
        let result = match wait_for_child_ready(saved, Some(pid), || child.try_wait()) {
            Ok(()) => Ok(()),
            Err(error) => {
                abort_watchdog_start(saved, error, || wait_for_helper_exit(|| child.try_wait()))
            }
        };
        finish_watchdog_launch(result, &token)
    }
    #[cfg(target_os = "macos")]
    {
        let command = match mac_helper_command(&["__lid_watchdog__", reservation.token()]) {
            Ok(command) => command,
            Err(error) => {
                drop(lock);
                return abort_watchdog_start(saved, error, || Ok(()));
            }
        };
        let mut child = match sysutil::spawn_detached(&command) {
            Ok(child) => child,
            Err(error) => {
                drop(lock);
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
        if let Err(error) = wait_for_child_claimed(&reservation, &mut status) {
            drop(lock);
            return abort_watchdog_start(saved, error, || wait_for_helper_exit(&mut status));
        }
        let token = reservation.commit();
        drop(lock);
        let result = match wait_for_child_ready(saved, None, &mut status) {
            Ok(()) => Ok(()),
            Err(error) => abort_watchdog_start(saved, error, || wait_for_helper_exit(&mut status)),
        };
        finish_watchdog_launch(result, &token)
    }
    #[cfg(target_os = "linux")]
    {
        drop(lock);
        Err(AppError::fail("lid watchdog is unavailable on Linux"))
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn wait_for_child_claimed(
    reservation: &session::ProcessLeaseReservation,
    mut child_status: impl FnMut() -> Result<Option<u32>>,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Some(code) = child_status()? {
            return Err(AppError::fail(format!(
                "lid watchdog exited during startup with code {code}"
            )));
        }
        if reservation.is_claimed()? {
            return Ok(());
        }
        sleep(Duration::from_millis(100));
    }
    Err(AppError::fail(
        "lid watchdog did not claim its process lease",
    ))
}

#[cfg(any(windows, target_os = "macos"))]
fn finish_watchdog_launch(result: Result<()>, token: &str) -> Result<()> {
    if result.is_ok() {
        result
    } else {
        merge_results(result, session::discard_process_lease(token))
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn abort_watchdog_start(
    saved: &Session,
    error: AppError,
    wait_helper: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let cleanup = abort_start_cleanup(
        || request_owner_stop(saved),
        || {
            wait_helper()?;
            wait_recorded_watchdog_exit(saved)
        },
        || {
            let _lock = session::acquire_lock_wait()?;
            if session::read_saved_for_recovery()?
                .is_some_and(|current| current.owner != saved.owner)
                || session::read_watchdog()?.is_some_and(|current| current.owner != saved.owner)
            {
                return Ok(());
            }
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
    if !sysutil::wait_lease_exit(&state.watchdog, Duration::from_secs(15))? {
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
fn request_owner_stop(saved: &Session) -> Result<()> {
    session::request_stop(saved)?;
    if !sysutil::wait_lease_exit(&saved.owner, Duration::from_secs(15))? {
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

#[cfg(any(windows, target_os = "macos"))]
fn ready_state(saved: &Session, expected_pid: Option<u32>) -> Result<Option<WatchdogState>> {
    let Some(state) = published_watchdog_state(saved, expected_pid)? else {
        return Ok(None);
    };
    if !state.ready {
        return Ok(None);
    }
    Ok(Some(state))
}

#[cfg(any(windows, target_os = "macos"))]
fn published_watchdog_state(
    saved: &Session,
    expected_pid: Option<u32>,
) -> Result<Option<WatchdogState>> {
    let Some(state) = session::read_watchdog()? else {
        return Ok(None);
    };
    if state.owner != saved.owner {
        return Err(AppError::fail(
            "lid watchdog published mismatched ready state",
        ));
    }
    if state.watchdog.pid == 0 {
        return Ok(None);
    }
    if expected_pid.is_some_and(|pid| state.watchdog.pid != pid) {
        return Err(AppError::fail(
            "lid watchdog published mismatched ready state",
        ));
    }
    if !sysutil::lease_is_live(&state.watchdog)? {
        return Err(AppError::fail("lid watchdog exited during startup"));
    }
    Ok(Some(state))
}

pub fn wait_ready(saved: &Session) -> Result<()> {
    let publish_deadline = Instant::now() + Duration::from_secs(15);
    let ready_deadline = Instant::now() + Duration::from_secs(300);
    let mut published = false;
    loop {
        if session::stop_requested(saved)? {
            return Err(AppError::fail("session stopped during lid startup"));
        }
        match session::read_watchdog()? {
            Some(state) if state.owner != saved.owner => {
                return Err(AppError::fail(
                    "lid watchdog published mismatched ready state",
                ));
            }
            Some(state) => {
                published = true;
                if state.watchdog.pid > 0 {
                    if !sysutil::lease_is_live(&state.watchdog)? {
                        return Err(AppError::fail("lid watchdog exited during startup"));
                    }
                    if state.ready {
                        return Ok(());
                    }
                }
            }
            None if published => {
                return Err(AppError::fail(
                    "lid watchdog state disappeared during startup",
                ));
            }
            None if Instant::now() >= publish_deadline => {
                return Err(AppError::fail("lid watchdog did not publish startup state"));
            }
            None => {}
        }
        if published && Instant::now() >= ready_deadline {
            return Err(AppError::fail("lid watchdog did not become ready"));
        }
        sleep(Duration::from_millis(100));
    }
}

pub fn ensure_ready(saved: &Session) -> Result<()> {
    match watchdog_health(
        &saved.owner,
        session::read_watchdog()?.as_ref(),
        sysutil::lease_is_live,
    )? {
        WatchdogHealth::Ready => Ok(()),
        _ => Err(AppError::fail("lid watchdog is no longer active")),
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub fn run_watchdog(args: &[String]) -> Result<()> {
    #[cfg(windows)]
    let [token, state_dir] = args else {
        return Err(AppError::fail(
            "lid watchdog expects a process lease and state directory",
        ));
    };
    #[cfg(windows)]
    session::set_helper_state_dir(decode_helper_state_dir(state_dir)?)?;
    #[cfg(target_os = "macos")]
    let [token] = args else {
        return Err(AppError::fail("lid watchdog expects a process lease"));
    };
    let lease = session::claim_process_lease(token)?;
    let saved = session::read_saved_for_recovery()?
        .ok_or_else(|| AppError::fail("lid watchdog found no session"))?;
    if !saved.spec.even_lid || !saved.owner_is_live()? {
        return Err(AppError::fail(
            "lid watchdog requires a matching live even-lid session",
        ));
    }
    let marker = session::read_lid_restore()?
        .ok_or_else(|| AppError::fail("lid watchdog found no restoration marker"))?;
    let snapshot = snapshot_from_marker(&marker)?;
    let pending = session::read_watchdog()?
        .ok_or_else(|| AppError::fail("lid watchdog startup was cancelled"))?;
    if pending.owner != saved.owner
        || pending.ready
        || pending.watchdog.pid != 0
        || pending.watchdog.token != token.as_str()
    {
        return Err(AppError::fail("lid watchdog startup state changed"));
    }
    let starting = WatchdogState {
        owner: saved.owner.clone(),
        watchdog: lease.reference(),
        ready: false,
    };
    let ready = WatchdogState {
        ready: true,
        ..starting.clone()
    };

    let lock = loop {
        if !saved.owner_is_live()? || session::stop_requested(&saved)? {
            let error = AppError::fail("lid watchdog startup was cancelled");
            return merge_results(
                Err(error),
                session::remove_watchdog_if_owner(&saved.owner).map(|_| ()),
            );
        }
        if let Some(lock) = session::try_acquire_lock()? {
            break lock;
        }
        sleep(Duration::from_millis(100));
    };
    if let Err(error) = validate_watchdog_start(&saved, &marker, &pending) {
        return merge_results(
            Err(error),
            session::remove_watchdog_if_owner(&saved.owner).map(|_| ()),
        );
    }

    begin_change(
        || session::write_watchdog(&starting),
        || platform::disable_lid(&snapshot),
        || session::write_watchdog(&ready),
        || platform::restore_lid(&snapshot),
        || session::clear_lid_restore(&marker),
        || session::remove_watchdog_if_owner(&saved.owner).map(|_| ()),
    )?;
    drop(lock);
    let primary = (|| -> Result<()> {
        loop {
            let owner_alive = sysutil::lease_is_live(&saved.owner)?;
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
        let stopped = sysutil::lease_is_live(&saved.owner).and_then(|alive| {
            if alive {
                request_owner_stop(&saved)
            } else {
                Ok(())
            }
        });
        merge_results(primary, stopped)
    } else {
        primary
    };

    finish_after_mutation(
        primary,
        || platform::restore_lid(&snapshot),
        || remove_matching_session(&saved),
        || session::clear_stop(&saved).map(|_| ()),
        || session::clear_lid_restore(&marker),
        || {
            session::remove_watchdog_if_owner(&saved.owner)?;
            Ok(())
        },
    )
}

#[cfg(any(windows, target_os = "macos"))]
fn validate_watchdog_start(
    saved: &Session,
    marker: &LidRestore,
    pending: &WatchdogState,
) -> Result<()> {
    let current = session::read_saved_for_recovery()?;
    if current.as_ref().is_none_or(|current| {
        current.owner != saved.owner || !current.spec.even_lid || current.spec != saved.spec
    }) || !saved.owner_is_live()?
        || session::stop_requested(saved)?
        || session::read_lid_restore()?.as_ref() != Some(marker)
        || session::read_watchdog()?.as_ref() != Some(pending)
    {
        return Err(AppError::fail("lid watchdog startup was cancelled"));
    }
    Ok(())
}

#[cfg(windows)]
pub fn run_restore(args: &[String]) -> Result<()> {
    let [state_dir] = args else {
        return Err(AppError::fail("lid restore expects a state directory"));
    };
    session::set_helper_state_dir(decode_helper_state_dir(state_dir)?)?;
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
        if session::read_saved_for_recovery()?
            .is_some_and(|current| !current.owner.same_lease(&_saved.owner))
            || watchdog
                .as_ref()
                .is_some_and(|current| !current.owner.same_lease(&_saved.owner))
        {
            return Ok(());
        }
        quiesce_recovery(Some(_saved), watchdog.as_ref())?;
        rollback_pending_marker()?;
        finish_recovered_state(_saved)
    }
}

pub fn recover_unlocked() -> Result<()> {
    session::cleanup_process_leases()?;
    let marker = session::read_lid_restore()?;
    let saved = session::read_saved_for_recovery();
    let watchdog = session::read_watchdog();
    let session_state = recovery_session(&saved)?;
    let watchdog_state = recovery_watchdog(&saved, &watchdog)?;
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
                rollback_pending_marker()?;
                session::remove_state_file()?;
                session::remove_watchdog_file()?;
                session::reconcile_stop()
            }
        }
    }
}

fn recovery_session(saved: &Result<Option<Session>>) -> Result<RecoverySession> {
    Ok(match saved {
        Err(_) => RecoverySession::Malformed,
        Ok(None) => RecoverySession::Missing,
        Ok(Some(saved)) if !saved.owner_is_live()? => RecoverySession::Stale,
        Ok(Some(saved)) if uses_watchdog(&saved.spec) => RecoverySession::LiveLid,
        Ok(Some(_)) => RecoverySession::LiveOrdinary,
    })
}

fn recovery_watchdog(
    saved: &Result<Option<Session>>,
    watchdog: &Result<Option<WatchdogState>>,
) -> Result<RecoveryWatchdog> {
    Ok(match watchdog {
        Err(_) => RecoveryWatchdog::Malformed,
        Ok(None) => RecoveryWatchdog::Missing,
        Ok(Some(state)) => RecoveryWatchdog::Valid {
            watchdog_live: sysutil::lease_is_live(&state.watchdog)?,
            owner_live: sysutil::lease_is_live(&state.owner)?,
            owner_matches_session: saved
                .as_ref()
                .ok()
                .and_then(Option::as_ref)
                .is_some_and(|saved| saved.owner == state.owner),
        },
    })
}

#[cfg(any(windows, target_os = "macos"))]
fn quiesce_recovery(saved: Option<&Session>, watchdog: Option<&WatchdogState>) -> Result<()> {
    let owner = saved
        .filter(|saved| saved.spec.even_lid)
        .map(|saved| &saved.owner)
        .or_else(|| watchdog.map(|state| &state.owner));
    if let Some(owner) = owner {
        session::request_stop_owner(owner)?;
        if !sysutil::wait_lease_exit(owner, Duration::from_secs(15))? {
            return Err(AppError::fail("could not stop lid session owner"));
        }
    }
    if let Some(state) = watchdog
        && !sysutil::wait_lease_exit(&state.watchdog, Duration::from_secs(15))?
    {
        return Err(AppError::fail("lid watchdog did not exit after its owner"));
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
        Some(actual) if actual.owner == saved.owner => {
            if actual.owner_is_live()? {
                Err(AppError::fail(
                    "lid session owner is still running after watchdog exit",
                ))
            } else {
                session::remove_if_matches(saved).map(|_| ())
            }
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

#[cfg(windows)]
fn restore_and_clear(marker: &LidRestore, snapshot: &platform::LidSnapshot) -> Result<()> {
    platform::restore_lid(snapshot)?;
    session::clear_lid_restore(marker)
}

#[cfg(windows)]
fn restore_elevated() -> Result<()> {
    let state_dir = encode_helper_state_dir(&session::absolute_state_dir()?);
    let code = sysutil::run_elevated_self(&format!("__lid_restore__ {state_dir}"))?;
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
    let marker = session::read_lid_restore()?
        .ok_or_else(|| AppError::fail("no lid restoration marker found"))?;
    let snapshot = snapshot_from_marker(&marker)?;
    platform::restore_lid_elevated(&snapshot)?;
    session::clear_lid_restore(&marker)
}

#[cfg(target_os = "macos")]
fn mac_helper_command(args: &[&str]) -> Result<Vec<String>> {
    let mut command = vec![
        "/usr/bin/sudo".into(),
        "-n".into(),
        "/usr/bin/env".into(),
        format!(
            "WAKE_STATE_DIR={}",
            session::absolute_state_dir()?.display()
        ),
        platform::trusted_helper_executable()?,
    ];
    command.extend(args.iter().map(|arg| (*arg).into()));
    Ok(command)
}

#[cfg(windows)]
fn encode_helper_state_dir(path: &std::path::Path) -> String {
    use std::os::windows::ffi::OsStrExt;

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::new();
    for unit in path.as_os_str().encode_wide() {
        for shift in [12, 8, 4, 0] {
            encoded.push(HEX[usize::from((unit >> shift) & 0xf)] as char);
        }
    }
    encoded
}

#[cfg(windows)]
fn decode_helper_state_dir(encoded: &str) -> Result<std::path::PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    if encoded.is_empty() || !encoded.len().is_multiple_of(4) {
        return Err(AppError::fail("invalid helper state directory"));
    }
    let mut units = Vec::with_capacity(encoded.len() / 4);
    for chunk in encoded.as_bytes().chunks_exact(4) {
        let value = std::str::from_utf8(chunk)
            .ok()
            .and_then(|chunk| u16::from_str_radix(chunk, 16).ok())
            .filter(|value| *value != 0)
            .ok_or_else(|| AppError::fail("invalid helper state directory"))?;
        units.push(value);
    }
    let path = std::path::PathBuf::from(OsString::from_wide(&units));
    if !path.is_absolute() {
        return Err(AppError::fail("helper state directory must be absolute"));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use crate::run::{Mode, Trigger};
    use std::cell::RefCell;

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_even_lid_does_not_use_watchdog() {
        let spec = RunSpec {
            mode: Mode::DisplaySystem,
            trigger: Trigger::Indefinite,
            even_lid: true,
        };

        assert!(!uses_watchdog(&spec));
    }

    fn process(pid: u32) -> LeaseRef {
        LeaseRef {
            pid,
            token: format!("{pid:032x}"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn elevated_helper_state_directory_round_trips() {
        let path = std::path::PathBuf::from(r"C:\Users\Δ user\状態");
        let encoded = encode_helper_state_dir(&path);

        assert!(encoded.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(decode_helper_state_dir(&encoded).unwrap(), path);
        let relative = encode_helper_state_dir(std::path::Path::new("relative"));
        for invalid in ["", "0", "zzzz", relative.as_str()] {
            assert!(decode_helper_state_dir(invalid).is_err());
        }
    }

    #[test]
    fn startup_publishes_watchdog_before_mutation_and_readiness() {
        let events = RefCell::new(Vec::new());
        begin_change(
            || {
                events.borrow_mut().push("watchdog");
                Ok(())
            },
            || {
                events.borrow_mut().push("mutate");
                Ok(())
            },
            || {
                events.borrow_mut().push("ready");
                Ok(())
            },
            || Ok(()),
            || Ok(()),
            || Ok(()),
        )
        .unwrap();

        assert_eq!(*events.borrow(), ["watchdog", "mutate", "ready"]);
    }

    #[test]
    fn startup_failure_restores_then_clears_marker_and_watchdog() {
        let events = RefCell::new(Vec::new());
        let result = begin_change(
            || {
                events.borrow_mut().push("watchdog");
                Ok(())
            },
            || {
                events.borrow_mut().push("mutate");
                Err(AppError::fail("change failed"))
            },
            || Ok(()),
            || {
                events.borrow_mut().push("restore");
                Ok(())
            },
            || {
                events.borrow_mut().push("marker");
                Ok(())
            },
            || {
                events.borrow_mut().push("watchdog-clear");
                Ok(())
            },
        );

        assert_eq!(result.unwrap_err().message(), "change failed");
        assert_eq!(
            *events.borrow(),
            ["watchdog", "mutate", "restore", "marker", "watchdog-clear"]
        );
    }

    #[test]
    fn readiness_publication_failure_rolls_back_the_mutation() {
        let events = RefCell::new(Vec::new());
        let result = begin_change(
            || Ok(()),
            || {
                events.borrow_mut().push("mutate");
                Ok(())
            },
            || Err(AppError::fail("ready failed")),
            || {
                events.borrow_mut().push("restore");
                Ok(())
            },
            || {
                events.borrow_mut().push("marker");
                Ok(())
            },
            || {
                events.borrow_mut().push("watchdog");
                Ok(())
            },
        );

        assert_eq!(result.unwrap_err().message(), "ready failed");
        assert_eq!(
            *events.borrow(),
            ["mutate", "restore", "marker", "watchdog"]
        );
    }

    #[test]
    fn failed_restore_retains_marker_and_watchdog() {
        let events = RefCell::new(Vec::new());
        let result = begin_change(
            || {
                events.borrow_mut().push("watchdog");
                Ok(())
            },
            || Err(AppError::fail("change failed")),
            || Ok(()),
            || {
                events.borrow_mut().push("restore");
                Err(AppError::fail("restore failed"))
            },
            || {
                events.borrow_mut().push("marker");
                Ok(())
            },
            || {
                events.borrow_mut().push("watchdog-clear");
                Ok(())
            },
        );

        assert!(result.unwrap_err().message().contains("restore failed"));
        assert_eq!(*events.borrow(), ["watchdog", "restore"]);
    }

    #[test]
    fn natural_cleanup_keeps_watchdog_recorded_until_marker_is_clear() {
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
                events.borrow_mut().push("stop");
                Ok(())
            },
            || {
                events.borrow_mut().push("marker");
                Ok(())
            },
            || {
                events.borrow_mut().push("watchdog");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            *events.borrow(),
            ["restore", "session", "stop", "marker", "watchdog"]
        );
    }

    #[test]
    fn watchdog_health_checks_exact_identities_and_liveness() {
        let owner = process(10);
        let watchdog = process(20);
        let ready = WatchdogState {
            owner: owner.clone(),
            watchdog: watchdog.clone(),
            ready: true,
        };
        let starting = WatchdogState {
            ready: false,
            ..ready.clone()
        };
        let wrong = WatchdogState {
            owner: process(11),
            watchdog: watchdog.clone(),
            ready: true,
        };

        assert_eq!(
            watchdog_health(&owner, Some(&ready), |candidate| Ok(candidate == &watchdog)).unwrap(),
            WatchdogHealth::Ready
        );
        assert_eq!(
            watchdog_health(&owner, None, |_| Ok(true)).unwrap(),
            WatchdogHealth::Missing
        );
        assert_eq!(
            watchdog_health(&owner, Some(&wrong), |_| Ok(true)).unwrap(),
            WatchdogHealth::Mismatch
        );
        assert_eq!(
            watchdog_health(&owner, Some(&starting), |_| Ok(true)).unwrap(),
            WatchdogHealth::Starting
        );
        assert_eq!(
            watchdog_health(&owner, Some(&ready), |_| Ok(false)).unwrap(),
            WatchdogHealth::Dead
        );
        assert!(
            watchdog_health(&owner, Some(&ready), |_| Err(AppError::fail(
                "unreadable lease"
            )))
            .is_err()
        );
    }

    #[test]
    fn watchdog_state_json_is_strict() {
        let state = WatchdogState {
            owner: process(10),
            watchdog: process(20),
            ready: true,
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
            (true, LiveLid, NoWatchdog, Keep),
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
                events.borrow_mut().push("stop");
                Ok(())
            },
            || {
                events.borrow_mut().push("marker");
                Ok(())
            },
            || {
                events.borrow_mut().push("watchdog");
                Ok(())
            },
        );

        assert_eq!(result.unwrap_err().message(), "monitor failed");
        assert_eq!(
            *events.borrow(),
            ["restore", "session", "stop", "marker", "watchdog"]
        );
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
            || Ok(()),
            || {
                events.borrow_mut().push("marker");
                Ok(())
            },
            || Ok(()),
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
