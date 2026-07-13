#[cfg(any(test, windows, target_os = "macos"))]
use crate::error::combine_cleanup;
use crate::error::{AppError, Result};
#[cfg(any(windows, target_os = "macos"))]
use crate::platform;
use crate::run::RunSpec;
#[cfg(any(test, windows, target_os = "macos"))]
use crate::session::OwnerIdentity;
use crate::session::{self, State};
#[cfg(any(windows, target_os = "macos"))]
use crate::session::{LidRestore, LidState};
use crate::sysutil;
#[cfg(target_os = "macos")]
use std::io::IsTerminal;
use std::thread::sleep;
use std::time::{Duration, Instant};

#[cfg(any(windows, target_os = "macos"))]
const OWNER_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(any(windows, target_os = "macos"))]
const CONFIG_INTERVAL: Duration = Duration::from_secs(30);
const STARTUP_WAIT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryAction {
    Keep,
    Remove,
    Wait,
    Restore,
    Reject,
}

fn decide_recovery(state: Option<&State>, owner_live: bool, watchdog_busy: bool) -> RecoveryAction {
    let Some(state) = state else {
        return if watchdog_busy {
            RecoveryAction::Wait
        } else {
            RecoveryAction::Keep
        };
    };
    if state.lid.is_some() {
        if watchdog_busy {
            if owner_live {
                RecoveryAction::Keep
            } else {
                RecoveryAction::Wait
            }
        } else {
            RecoveryAction::Restore
        }
    } else if uses_watchdog(&state.session.spec) {
        if watchdog_busy || owner_live {
            RecoveryAction::Wait
        } else {
            RecoveryAction::Remove
        }
    } else if watchdog_busy {
        RecoveryAction::Reject
    } else if owner_live {
        RecoveryAction::Keep
    } else {
        RecoveryAction::Remove
    }
}

#[cfg(any(test, windows, target_os = "macos"))]
fn cleanup_matches(expected: &State, current: &State) -> bool {
    if expected.session.stop_requested && !current.session.stop_requested {
        return false;
    }
    let mut normalized = current.clone();
    normalized.session.stop_requested = expected.session.stop_requested;
    &normalized == expected
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

pub fn prepare_start(spec: &RunSpec) -> Result<()> {
    if !uses_watchdog(spec) {
        return Ok(());
    }
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
    Ok(())
}

pub fn launch_watchdog(saved: &State, lock: session::LockGuard) -> Result<()> {
    if !uses_watchdog(&saved.session.spec) {
        drop(lock);
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        drop(lock);
        Ok(())
    }
    #[cfg(windows)]
    {
        let state_dir = canonical_state_dir()?;
        let caller_sid = session::windows_user_sid()?;
        session::validate_windows_state_dir(&state_dir, &caller_sid)?;
        let command = format!(
            "__lid_watchdog__ {} {} {}",
            saved.session.owner.token,
            encode_path(&state_dir),
            caller_sid
        );
        let mut child = match sysutil::launch_elevated_self(&command) {
            Ok(child) => child,
            Err(error) => {
                drop(lock);
                return Err(error);
            }
        };
        drop(lock);
        wait_for_ready(saved, || child.try_wait())
    }
    #[cfg(target_os = "macos")]
    {
        let command =
            match mac_helper_command(&["__lid_watchdog__", saved.session.owner.token.as_str()]) {
                Ok(command) => command,
                Err(error) => {
                    drop(lock);
                    return Err(error);
                }
            };
        let mut child = match sysutil::spawn_detached(&command) {
            Ok(child) => child,
            Err(error) => {
                drop(lock);
                return Err(AppError::fail(format!(
                    "could not launch lid watchdog: {error}"
                )));
            }
        };
        drop(lock);
        wait_for_ready(saved, || {
            child
                .try_wait()
                .map(|status| status.map(|status| status.code().unwrap_or(1) as u32))
                .map_err(AppError::from)
        })
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn wait_for_ready(
    saved: &State,
    mut child_status: impl FnMut() -> Result<Option<u32>>,
) -> Result<()> {
    let deadline = Instant::now() + STARTUP_WAIT;
    while Instant::now() < deadline {
        if let Some(code) = child_status()? {
            return Err(AppError::fail(format!(
                "lid watchdog exited during startup with code {code}"
            )));
        }
        match session::read()? {
            Some(current)
                if current.session.owner == saved.session.owner
                    && current.session.spec == saved.session.spec
                    && !current.session.stop_requested =>
            {
                if current.lid.as_ref().is_some_and(|lid| lid.ready) {
                    return match session::try_watchdog_lock()? {
                        None => Ok(()),
                        Some(_guard) => Err(AppError::fail("lid watchdog exited during startup")),
                    };
                }
            }
            Some(_) => {
                return Err(AppError::fail(
                    "lid watchdog published mismatched session state",
                ));
            }
            None => {
                return Err(AppError::fail(
                    "lid session state disappeared during startup",
                ));
            }
        }
        sleep(Duration::from_millis(100));
    }
    Err(AppError::fail("lid watchdog did not publish ready state"))
}

pub fn ensure_ready(saved: &State) -> Result<()> {
    if !uses_watchdog(&saved.session.spec) {
        return Ok(());
    }
    let current =
        session::read()?.ok_or_else(|| AppError::fail("lid session state disappeared"))?;
    if current.session.owner != saved.session.owner
        || current.session.spec != saved.session.spec
        || current.session.stop_requested
        || !current.lid.as_ref().is_some_and(|lid| lid.ready)
    {
        return Err(AppError::fail("lid watchdog is not ready"));
    }
    match session::try_watchdog_lock()? {
        None => Ok(()),
        Some(_guard) => Err(AppError::fail("lid watchdog is no longer active")),
    }
}

pub fn finish_stop(saved: &State) -> Result<()> {
    recover()?;
    if session::read()?.is_some_and(|current| current.session.owner == saved.session.owner) {
        Err(AppError::fail("lid cleanup is still in progress"))
    } else {
        Ok(())
    }
}

pub fn recover() -> Result<()> {
    let deadline = Instant::now() + STARTUP_WAIT;
    loop {
        let lock = session::acquire_lock()?;
        let state = session::read()?;
        let owner_live = state
            .as_ref()
            .map(|state| sysutil::process_identity_matches(&state.session.owner.process))
            .transpose()?
            .unwrap_or(false);
        let watchdog_guard = session::try_watchdog_lock()?;
        let watchdog_busy = watchdog_guard.is_none();
        match decide_recovery(state.as_ref(), owner_live, watchdog_busy) {
            RecoveryAction::Keep => return Ok(()),
            RecoveryAction::Reject => {
                return Err(AppError::fail(
                    "inconsistent lid watchdog and session state",
                ));
            }
            RecoveryAction::Remove => {
                let state = state.expect("remove requires state");
                let _guard = watchdog_guard.expect("free watchdog lock has a guard");
                session::remove_exact(&lock, &state)?;
                return Ok(());
            }
            RecoveryAction::Restore => {
                let state = state.expect("restore requires state");
                let guard = watchdog_guard.expect("free watchdog lock has a guard");
                return recover_lid_state(state, lock, guard, owner_live);
            }
            RecoveryAction::Wait => {
                if let Some(state) = state
                    && state.lid.is_none()
                    && uses_watchdog(&state.session.spec)
                    && owner_live
                    && !watchdog_busy
                {
                    if Instant::now() < deadline {
                        drop(lock);
                        drop(watchdog_guard);
                        sleep(Duration::from_millis(100));
                        continue;
                    }
                    let guard = watchdog_guard.expect("free watchdog lock has a guard");
                    return stop_orphan(state, lock, guard);
                }
                drop(lock);
                drop(watchdog_guard);
                if Instant::now() >= deadline {
                    return Err(AppError::fail("lid cleanup is still in progress"));
                }
                sleep(Duration::from_millis(100));
            }
        }
    }
}

fn stop_orphan(
    state: State,
    lock: session::LockGuard,
    _guard: session::WatchdogLock,
) -> Result<()> {
    let stopped = if state.session.stop_requested {
        state
    } else {
        session::update(&lock, &state.session.owner, |current| {
            current.session.stop_requested = true;
            Ok(())
        })?
    };
    drop(lock);
    if !sysutil::wait_process_exit(&stopped.session.owner.process, STARTUP_WAIT)? {
        return Err(AppError::fail("could not stop orphaned lid session owner"));
    }
    let lock = session::acquire_existing_lock_wait()?;
    if session::read()?.as_ref() == Some(&stopped) {
        session::remove_exact(&lock, &stopped)?;
    }
    Ok(())
}

fn recover_lid_state(
    state: State,
    lock: session::LockGuard,
    guard: session::WatchdogLock,
    owner_live: bool,
) -> Result<()> {
    let latest = if owner_live && !state.session.stop_requested {
        session::update(&lock, &state.session.owner, |current| {
            current.session.stop_requested = true;
            Ok(())
        })?
    } else {
        state
    };
    drop(lock);
    if owner_live && !sysutil::wait_process_exit(&latest.session.owner.process, STARTUP_WAIT)? {
        return Err(AppError::fail("could not stop lid session owner"));
    }
    restore_recovered(&latest, guard)
}

#[cfg(target_os = "linux")]
fn restore_recovered(_state: &State, _guard: session::WatchdogLock) -> Result<()> {
    Err(AppError::fail("unexpected Linux lid recovery state"))
}

#[cfg(target_os = "macos")]
fn restore_recovered(state: &State, _guard: session::WatchdogLock) -> Result<()> {
    let snapshot = snapshot_from_state(state)?;
    platform::restore_lid_elevated(&snapshot)?;
    if !platform::lid_is_restored(&snapshot)? {
        return Err(AppError::fail("lid restoration could not be verified"));
    }
    let lock = session::acquire_existing_lock_wait()?;
    if session::read()?.as_ref() == Some(state) {
        session::remove_exact(&lock, state)?;
    }
    Ok(())
}

#[cfg(windows)]
fn restore_recovered(state: &State, _guard: session::WatchdogLock) -> Result<()> {
    let state_json = serde_json::to_vec(state)
        .map_err(|error| AppError::fail(format!("could not encode recovery state: {error}")))?;
    let state_dir = canonical_state_dir()?;
    let caller_sid = session::windows_user_sid()?;
    session::validate_windows_state_dir(&state_dir, &caller_sid)?;
    let command = format!(
        "__lid_restore__ {} {} {}",
        encode_bytes(&state_json),
        encode_path(&state_dir),
        caller_sid
    );
    let code = sysutil::run_elevated_self(&command)?;
    if code == 0 {
        Ok(())
    } else {
        Err(AppError::fail(format!(
            "elevated lid restoration exited with code {code}"
        )))
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub fn run_watchdog(args: &[String]) -> Result<()> {
    #[cfg(windows)]
    let [token, state_dir, caller_sid] = args else {
        return Err(AppError::fail(
            "lid watchdog expects a session token, state directory, and caller SID",
        ));
    };
    #[cfg(windows)]
    session::set_helper_state_dir(decode_path(state_dir)?)?;
    #[cfg(target_os = "macos")]
    let [token] = args else {
        return Err(AppError::fail("lid watchdog expects a session token"));
    };
    #[cfg(windows)]
    session::validate_helper_state_dir(caller_sid)?;
    #[cfg(target_os = "macos")]
    session::validate_helper_state_dir()?;
    let _watchdog_lock = session::acquire_watchdog_lock()?;
    let lock = session::acquire_existing_lock_wait()?;
    let saved = session::read()?.ok_or_else(|| AppError::fail("lid watchdog found no session"))?;
    if saved.session.owner.token != *token
        || !uses_watchdog(&saved.session.spec)
        || saved.session.started_at.is_none()
        || saved.session.stop_requested
        || saved.lid.is_some()
        || !sysutil::process_identity_matches(&saved.session.owner.process)?
    {
        return Err(AppError::fail(
            "lid watchdog requires the exact live even-lid session",
        ));
    }
    let snapshot = platform::read_lid_snapshot()?;
    let starting = session::update(&lock, &saved.session.owner, |state| {
        state.lid = Some(LidState {
            ready: false,
            restore: marker_from_snapshot(&snapshot),
        });
        Ok(())
    })?;
    let startup = complete_lid_start(
        || platform::lid_snapshot_matches(&snapshot),
        || platform::disable_lid(&snapshot),
        || platform::lid_override_is_active(&snapshot),
        || {
            session::update(&lock, &saved.session.owner, |state| {
                state.lid.as_mut().expect("starting lid state exists").ready = true;
                Ok(())
            })
        },
    );
    drop(lock);
    let ready = match startup {
        Ok(ready) => ready,
        Err(error) => {
            return watchdog_teardown(&saved.session.owner, &snapshot, Some(starting), Err(error));
        }
    };
    let primary = monitor(&ready, &snapshot);
    watchdog_teardown(&saved.session.owner, &snapshot, Some(ready), primary)
}

#[cfg(any(test, windows, target_os = "macos"))]
fn complete_lid_start(
    snapshot_matches: impl FnOnce() -> Result<bool>,
    mutate: impl FnOnce() -> Result<()>,
    override_is_active: impl FnOnce() -> Result<bool>,
    publish_ready: impl FnOnce() -> Result<State>,
) -> Result<State> {
    if !snapshot_matches()? {
        return Err(AppError::fail(
            "power configuration changed before the lid override",
        ));
    }
    mutate()?;
    if !override_is_active()? {
        return Err(AppError::fail("lid override could not be verified"));
    }
    publish_ready()
}

#[cfg(any(windows, target_os = "macos"))]
fn monitor(saved: &State, snapshot: &platform::LidSnapshot) -> Result<()> {
    let mut next_config = Instant::now() + CONFIG_INTERVAL;
    loop {
        if !sysutil::process_identity_matches(&saved.session.owner.process)? {
            return Ok(());
        }
        let current =
            session::read()?.ok_or_else(|| AppError::fail("lid session state disappeared"))?;
        if current.session.owner != saved.session.owner || current.lid != saved.lid {
            return Err(AppError::fail("lid session state changed ownership"));
        }
        if current.session.stop_requested {
            return Ok(());
        }
        if Instant::now() >= next_config {
            next_config = Instant::now() + CONFIG_INTERVAL;
            if !platform::lid_override_is_active(snapshot)? {
                return Err(AppError::fail(
                    "lid protection ended because the power configuration changed",
                ));
            }
        }
        sleep(OWNER_INTERVAL);
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn watchdog_teardown(
    owner: &OwnerIdentity,
    snapshot: &platform::LidSnapshot,
    latest: Option<State>,
    primary: Result<()>,
) -> Result<()> {
    run_teardown_sequence(
        primary,
        || quiesce_owner(owner, latest),
        || platform::restore_lid(snapshot),
        || platform::lid_is_restored(snapshot),
        |latest| {
            let lock = session::acquire_existing_lock_wait()?;
            let current = session::read()?;
            if let (Some(current), Some(expected)) = (current, latest)
                && cleanup_matches(&expected, &current)
            {
                session::remove_exact(&lock, &current)?;
            }
            Ok(())
        },
    )
}

#[cfg(any(windows, target_os = "macos"))]
fn quiesce_owner(owner: &OwnerIdentity, latest: Option<State>) -> Result<Option<State>> {
    quiesce_owner_with(
        owner,
        latest,
        || sysutil::process_identity_matches(&owner.process),
        || {
            let lock = session::acquire_existing_lock_wait()?;
            let Some(mut current) = session::read()? else {
                return Ok(None);
            };
            if current.session.owner == *owner
                && !current.session.stop_requested
                && let Ok(stopped) = session::update(&lock, owner, |state| {
                    state.session.stop_requested = true;
                    Ok(())
                })
            {
                current = stopped;
            }
            Ok(Some(current))
        },
        || sleep(OWNER_INTERVAL),
    )
}

#[cfg(any(test, windows, target_os = "macos"))]
fn quiesce_owner_with(
    owner: &OwnerIdentity,
    mut cleanup_expected: Option<State>,
    mut owner_live: impl FnMut() -> Result<bool>,
    mut observe: impl FnMut() -> Result<Option<State>>,
    mut pause: impl FnMut(),
) -> Result<Option<State>> {
    loop {
        match owner_live() {
            Ok(false) => return Ok(cleanup_expected),
            Ok(true) => {
                let matches_expected = match observe() {
                    Ok(Some(current)) => cleanup_expected.as_ref().is_some_and(|expected| {
                        current.session.owner == *owner && cleanup_matches(expected, &current)
                    }),
                    Ok(None) | Err(_) => false,
                };
                if !matches_expected {
                    cleanup_expected = None;
                }
            }
            Err(_) => {}
        }
        pause();
    }
}

#[cfg(any(test, windows, target_os = "macos"))]
fn run_teardown_sequence(
    primary: Result<()>,
    quiesce: impl FnOnce() -> Result<Option<State>>,
    restore: impl FnOnce() -> Result<()>,
    verify: impl FnOnce() -> Result<bool>,
    cleanup: impl FnOnce(Option<State>) -> Result<()>,
) -> Result<()> {
    let latest = match quiesce() {
        Ok(latest) => latest,
        Err(error) => return combine_cleanup(primary, Err(error)),
    };
    if let Err(error) = restore() {
        return combine_cleanup(primary, Err(error));
    }
    match verify() {
        Ok(true) => combine_cleanup(primary, cleanup(latest)),
        Ok(false) => combine_cleanup(
            primary,
            Err(AppError::fail("lid restoration could not be verified")),
        ),
        Err(error) => combine_cleanup(primary, Err(error)),
    }
}

#[cfg(windows)]
pub fn run_restore(args: &[String]) -> Result<()> {
    let [encoded_state, state_dir, caller_sid] = args else {
        return Err(AppError::fail(
            "lid restore expects an exact state, state directory, and caller SID",
        ));
    };
    session::set_helper_state_dir(decode_path(state_dir)?)?;
    session::validate_helper_state_dir(caller_sid)?;
    let expected: State = serde_json::from_slice(&decode_bytes(encoded_state)?)
        .map_err(|error| AppError::fail(format!("invalid recovery state: {error}")))?;
    expected.validate()?;
    let lock = session::acquire_existing_lock_wait()?;
    if session::read()?.as_ref() != Some(&expected) {
        return Err(AppError::fail("lid recovery state changed"));
    }
    if sysutil::process_identity_matches(&expected.session.owner.process)? {
        return Err(AppError::fail("lid session owner is still active"));
    }
    let snapshot = snapshot_from_state(&expected)?;
    platform::restore_lid(&snapshot)?;
    if !platform::lid_is_restored(&snapshot)? {
        return Err(AppError::fail("lid restoration could not be verified"));
    }
    session::remove_exact(&lock, &expected)?;
    Ok(())
}

#[cfg(windows)]
pub fn run_set_lid(args: &[String]) -> Result<()> {
    let (ac, dc) = parse_set_lid_args(args)?;
    platform::set_current_lid_actions(ac, dc)
}

#[cfg(windows)]
fn parse_set_lid_args(args: &[String]) -> Result<(u32, u32)> {
    let [ac, dc] = args else {
        return Err(AppError::fail(
            "lid compatibility helper expects AC and DC values",
        ));
    };
    let parse = |value: &str| {
        value
            .parse::<u32>()
            .ok()
            .filter(|value| (0..=3).contains(value))
            .ok_or_else(|| AppError::fail("lid compatibility values must be integers in 0..=3"))
    };
    Ok((parse(ac)?, parse(dc)?))
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
fn snapshot_from_state(state: &State) -> Result<platform::LidSnapshot> {
    match &state
        .lid
        .as_ref()
        .ok_or_else(|| AppError::fail("lid recovery state is missing"))?
        .restore
    {
        LidRestore::Windows {
            scheme_guid,
            ac_action,
            dc_action,
        } => platform::LidSnapshot::new(scheme_guid.clone(), *ac_action, *dc_action),
        LidRestore::Macos { .. } => Err(AppError::fail("macOS lid state found on Windows")),
    }
}

#[cfg(target_os = "macos")]
fn snapshot_from_state(state: &State) -> Result<platform::LidSnapshot> {
    match &state
        .lid
        .as_ref()
        .ok_or_else(|| AppError::fail("lid recovery state is missing"))?
        .restore
    {
        LidRestore::Macos { sleep_disabled } => Ok(platform::LidSnapshot {
            sleep_disabled: *sleep_disabled,
        }),
        LidRestore::Windows { .. } => Err(AppError::fail("Windows lid state found on macOS")),
    }
}

#[cfg(target_os = "macos")]
fn mac_helper_command(args: &[&str]) -> Result<Vec<String>> {
    let dir = canonical_state_dir()?;
    let mut command = vec![
        "/usr/bin/sudo".into(),
        "-n".into(),
        "/usr/bin/env".into(),
        format!("WAKE_STATE_DIR={}", dir.display()),
        platform::trusted_helper_executable()?,
    ];
    command.extend(args.iter().map(|arg| (*arg).into()));
    Ok(command)
}

#[cfg(any(windows, target_os = "macos"))]
fn canonical_state_dir() -> Result<std::path::PathBuf> {
    let dir = session::absolute_state_dir()?;
    std::fs::canonicalize(&dir).map_err(|error| {
        AppError::fail(format!(
            "could not canonicalize state directory {}: {error}",
            dir.display()
        ))
    })
}

#[cfg(windows)]
fn encode_path(path: &std::path::Path) -> String {
    use std::os::windows::ffi::OsStrExt;
    let bytes = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_be_bytes)
        .collect::<Vec<_>>();
    encode_bytes(&bytes)
}

#[cfg(windows)]
fn decode_path(encoded: &str) -> Result<std::path::PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    let bytes = decode_bytes(encoded)?;
    if bytes.is_empty() || !bytes.len().is_multiple_of(2) {
        return Err(AppError::fail("invalid helper state directory"));
    }
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    if units.contains(&0) {
        return Err(AppError::fail("invalid helper state directory"));
    }
    let path = std::path::PathBuf::from(OsString::from_wide(&units));
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(AppError::fail("helper state directory must be absolute"))
    }
}

#[cfg(windows)]
fn encode_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0xf)] as char);
    }
    encoded
}

#[cfg(windows)]
fn decode_bytes(encoded: &str) -> Result<Vec<u8>> {
    if encoded.is_empty() || !encoded.len().is_multiple_of(2) {
        return Err(AppError::fail("invalid hexadecimal helper value"));
    }
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|chunk| {
            let text = std::str::from_utf8(chunk)
                .map_err(|_| AppError::fail("invalid hexadecimal helper value"))?;
            u8::from_str_radix(text, 16)
                .map_err(|_| AppError::fail("invalid hexadecimal helper value"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{Mode, ProcessIdentity, Trigger};
    use crate::session::{LidRestore, LidState, OwnerIdentity, STATE_SCHEMA, SessionState};
    use std::cell::{Cell, RefCell};

    fn state(even_lid: bool, lid: bool) -> State {
        State {
            schema: STATE_SCHEMA,
            session: SessionState {
                owner: OwnerIdentity {
                    token: "0123456789abcdef0123456789abcdef".into(),
                    process: ProcessIdentity {
                        pid: 10,
                        native_start: 11,
                    },
                },
                spec: RunSpec {
                    mode: Mode::DisplaySystem,
                    trigger: Trigger::Indefinite,
                    even_lid,
                },
                started_at: Some("2024-01-02T03:04:05Z".parse().unwrap()),
                note: None,
                stop_requested: false,
            },
            lid: lid.then_some(LidState {
                ready: true,
                restore: LidRestore::Macos { sleep_disabled: 0 },
            }),
        }
    }

    #[test]
    fn recovery_actions_use_busy_lock_semantics() {
        assert_eq!(decide_recovery(None, false, false), RecoveryAction::Keep);
        assert_eq!(decide_recovery(None, false, true), RecoveryAction::Wait);
        let ordinary = state(false, false);
        assert_eq!(
            decide_recovery(Some(&ordinary), false, false),
            RecoveryAction::Remove
        );
        assert_eq!(
            decide_recovery(Some(&ordinary), true, true),
            RecoveryAction::Reject
        );
        let lid = state(true, true);
        assert_eq!(
            decide_recovery(Some(&lid), true, true),
            RecoveryAction::Keep
        );
        assert_eq!(
            decide_recovery(Some(&lid), false, false),
            RecoveryAction::Restore
        );
        #[cfg(any(windows, target_os = "macos"))]
        {
            let starting = state(true, false);
            assert_eq!(
                decide_recovery(Some(&starting), true, true),
                RecoveryAction::Wait
            );
        }
    }

    #[test]
    fn quiesce_observation_never_reenables_cleanup_after_drift() {
        let expected = state(true, true);
        let owner = expected.session.owner.clone();
        let mut drift = expected.clone();
        drift.session.note = Some("changed".into());
        let mut stopped = expected.clone();
        stopped.session.stop_requested = true;
        let mut liveness = [true, true, false].into_iter();
        let mut observations = [Some(drift), Some(stopped)].into_iter();

        let cleanup = quiesce_owner_with(
            &owner,
            Some(expected),
            || Ok(liveness.next().unwrap()),
            || Ok(observations.next().unwrap()),
            || {},
        )
        .unwrap();

        assert!(cleanup.is_none());
    }

    #[test]
    fn quiesce_missing_observation_permanently_revokes_cleanup() {
        let expected = state(true, true);
        let owner = expected.session.owner.clone();
        let mut liveness = [true, true, false].into_iter();
        let mut observations = [None, Some(expected.clone())].into_iter();

        let cleanup = quiesce_owner_with(
            &owner,
            Some(expected),
            || Ok(liveness.next().unwrap()),
            || Ok(observations.next().unwrap()),
            || {},
        )
        .unwrap();

        assert!(cleanup.is_none());
    }

    #[test]
    fn quiesce_read_error_permanently_revokes_cleanup() {
        let expected = state(true, true);
        let owner = expected.session.owner.clone();
        let mut liveness = [true, true, false].into_iter();
        let mut observations = [
            Err(AppError::fail("invalid state")),
            Ok(Some(expected.clone())),
        ]
        .into_iter();

        let cleanup = quiesce_owner_with(
            &owner,
            Some(expected),
            || Ok(liveness.next().unwrap()),
            || observations.next().unwrap(),
            || {},
        )
        .unwrap();

        assert!(cleanup.is_none());
    }

    #[test]
    fn cleanup_accepts_only_exact_state_or_its_stop_transition() {
        let expected = state(true, true);
        assert!(cleanup_matches(&expected, &expected));
        let mut stopped = expected.clone();
        stopped.session.stop_requested = true;
        assert!(cleanup_matches(&expected, &stopped));
        let changes: [fn(&mut State); 5] = [
            |state: &mut State| {
                state.session.owner.token = "11111111111111111111111111111111".into()
            },
            |state| state.session.spec.mode = Mode::SystemOnly,
            |state| state.session.started_at = Some("2024-01-02T03:04:06Z".parse().unwrap()),
            |state| state.session.note = Some("changed".into()),
            |state| state.lid.as_mut().unwrap().ready = false,
        ];
        for change in changes {
            let mut changed = stopped.clone();
            change(&mut changed);
            assert!(!cleanup_matches(&expected, &changed));
        }
        assert!(!cleanup_matches(&stopped, &expected));
    }

    #[test]
    fn lid_start_orders_effects_and_requires_verified_override() {
        let events = RefCell::new(Vec::new());
        let ready = state(true, true);
        let saved = complete_lid_start(
            || {
                events.borrow_mut().push("recheck");
                Ok(true)
            },
            || {
                events.borrow_mut().push("mutate");
                Ok(())
            },
            || {
                events.borrow_mut().push("verify");
                Ok(true)
            },
            || {
                events.borrow_mut().push("ready");
                Ok(ready)
            },
        )
        .unwrap();

        assert!(saved.lid.unwrap().ready);
        assert_eq!(*events.borrow(), ["recheck", "mutate", "verify", "ready"]);
        let ready_called = Cell::new(false);
        let result = complete_lid_start(
            || Ok(true),
            || Ok(()),
            || Ok(false),
            || {
                ready_called.set(true);
                Ok(state(true, true))
            },
        );

        assert!(result.is_err());
        assert!(!ready_called.get());
    }

    #[test]
    fn drift_teardown_quiesces_then_restores_and_cleans() {
        let events = RefCell::new(Vec::new());
        run_teardown_sequence(
            Ok(()),
            || {
                events.borrow_mut().extend(["stop", "wait"]);
                Ok(Some(state(true, true)))
            },
            || {
                events.borrow_mut().push("restore");
                Ok(())
            },
            || {
                events.borrow_mut().push("verify");
                Ok(true)
            },
            |_| {
                events.borrow_mut().push("cleanup");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            *events.borrow(),
            ["stop", "wait", "restore", "verify", "cleanup"]
        );
    }

    #[test]
    fn failed_restore_verification_retains_durable_state() {
        let cleanup_called = Cell::new(false);
        let result = run_teardown_sequence(
            Err(AppError::fail("drift")),
            || Ok(Some(state(true, true))),
            || Ok(()),
            || Ok(false),
            |_| {
                cleanup_called.set(true);
                Ok(())
            },
        );

        assert!(result.is_err());
        assert!(!cleanup_called.get());
    }

    #[test]
    fn malformed_or_mismatched_state_is_restored_but_not_deleted() {
        let expected = state(true, true);
        let mut durable = expected.clone();
        durable.session.note = Some("changed".into());
        let restored = Cell::new(false);
        let deleted = Cell::new(false);

        let result = run_teardown_sequence(
            Err(AppError::fail("state changed")),
            || Ok(Some(expected.clone())),
            || {
                restored.set(true);
                Ok(())
            },
            || Ok(true),
            |latest| {
                if latest
                    .as_ref()
                    .is_some_and(|latest| cleanup_matches(latest, &durable))
                {
                    deleted.set(true);
                }
                Ok(())
            },
        );

        assert!(result.is_err());
        assert!(restored.get());
        assert!(!deleted.get());

        restored.set(false);
        let malformed = run_teardown_sequence(
            Ok(()),
            || Ok(Some(expected)),
            || {
                restored.set(true);
                Ok(())
            },
            || Ok(true),
            |_| Err(AppError::fail("invalid JSON")),
        );
        assert!(malformed.is_err());
        assert!(restored.get());
    }

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

    #[cfg(windows)]
    #[test]
    fn compatibility_parser_is_strict() {
        assert_eq!(
            parse_set_lid_args(&["0".into(), "3".into()]).unwrap(),
            (0, 3)
        );
        for args in [vec![], vec!["0".into()], vec!["0".into(), "4".into()]] {
            assert!(parse_set_lid_args(&args).is_err());
        }
    }

    #[cfg(windows)]
    #[test]
    fn helper_encodings_round_trip() {
        let path = std::path::PathBuf::from(r"C:\Users\Δ user\状態");
        assert_eq!(decode_path(&encode_path(&path)).unwrap(), path);
        let bytes = br#"{\"state\":true}"#;
        assert_eq!(decode_bytes(&encode_bytes(bytes)).unwrap(), bytes);
    }
}
