use crate::error::{AppError, Result};
use crate::run::{ProcessRef, RunSpec};
use crate::sysutil;
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("WAKE_STATE_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    default_state_dir()
}

#[cfg(windows)]
fn default_state_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("AppData").join("Local"));
    base.join("wake")
}

#[cfg(unix)]
fn default_state_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        let p = PathBuf::from(xdg);
        if p.is_absolute() {
            return p.join("wake");
        }
    }
    home().join(".local").join("state").join("wake")
}

pub fn state_file() -> PathBuf {
    state_dir().join("session.json")
}

pub fn stop_file() -> PathBuf {
    state_dir().join("stop.json")
}

#[cfg_attr(windows, allow(dead_code))]
pub fn lid_restore_file() -> PathBuf {
    state_dir().join("lid-restore.json")
}

pub fn lid_watchdog_file() -> PathBuf {
    state_dir().join("lid-watchdog.json")
}

fn home() -> PathBuf {
    #[cfg(windows)]
    let var = std::env::var_os("USERPROFILE");
    #[cfg(unix)]
    let var = std::env::var_os("HOME");
    var.map(PathBuf::from)
        .or_else(std::env::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Session {
    pub owner: ProcessRef,
    pub spec: RunSpec,
    pub started_at: DateTime<Utc>,
    pub ends_at: Option<DateTime<Utc>>,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WatchdogState {
    pub owner: ProcessRef,
    pub watchdog: ProcessRef,
}

impl WatchdogState {
    fn is_valid(&self) -> bool {
        self.owner.is_valid() && self.watchdog.is_valid() && self.owner != self.watchdog
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase", tag = "platform")]
pub enum LidRestore {
    Macos {
        sleep_disabled: i32,
    },
    Windows {
        scheme_guid: String,
        ac_action: u32,
        dc_action: u32,
    },
}

impl LidRestore {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Macos {
                sleep_disabled: 0 | 1,
            } => Ok(()),
            Self::Macos { .. } => Err(AppError::fail("SleepDisabled must be 0 or 1")),
            Self::Windows {
                scheme_guid,
                ac_action,
                dc_action,
            } if is_guid(scheme_guid)
                && (0..=3).contains(ac_action)
                && (0..=3).contains(dc_action) =>
            {
                Ok(())
            }
            Self::Windows { .. } => Err(AppError::fail(
                "Windows lid restoration requires a scheme GUID and AC/DC actions in 0..=3",
            )),
        }
    }
}

fn is_guid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

impl Session {
    fn is_valid(&self) -> bool {
        self.owner.is_valid()
            && self.spec.validate().is_ok()
            && self.ends_at == self.spec.trigger.session_ends_at(self.started_at)
    }

    pub fn matches_live_process(&self) -> bool {
        sysutil::process_matches(&self.owner)
    }

    pub fn identity(&self) -> ProcessRef {
        self.owner.clone()
    }
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct StopRequest {
    session: ProcessRef,
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(state_io_err(path, error)),
    };
    serde_json::from_reader(file)
        .map(Some)
        .map_err(|error| AppError::fail(format!("invalid JSON at {}: {error}", path.display())))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| AppError::fail(format!("state path has no parent: {}", path.display())))?;
    fs::create_dir_all(dir).map_err(|error| state_io_err(dir, error))?;
    let data = serde_json::to_vec(value)
        .map_err(|error| AppError::fail(format!("could not encode {}: {error}", path.display())))?;
    let tmp = path.with_extension("json.tmp");
    let result = (|| {
        let mut file = File::create(&tmp).map_err(|error| state_io_err(&tmp, error))?;
        file.write_all(&data)
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_all())
            .map_err(|error| state_io_err(&tmp, error))?;
        replace_file(&tmp, path).map_err(|error| state_io_err(path, error))?;
        sync_parent(dir).map_err(|error| state_io_err(dir, error))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(not(windows))]
fn replace_file(tmp: &Path, path: &Path) -> std::io::Result<()> {
    fs::rename(tmp, path)
}

#[cfg(windows)]
fn replace_file(tmp: &Path, path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let wide = |value: &Path| {
        value
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>()
    };
    let (tmp, path) = (wide(tmp), wide(path));
    // SAFETY: both paths are valid, NUL-terminated UTF-16 buffers retained for the call.
    if unsafe {
        MoveFileExW(
            tmp.as_ptr(),
            path.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_parent(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(windows)]
fn sync_parent(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

fn read_session_at(path: &Path) -> Result<Option<Session>> {
    let saved: Option<Session> = read_json(path)?;
    match saved {
        Some(saved) if !saved.is_valid() => Err(AppError::fail(format!(
            "invalid session at {}",
            path.display()
        ))),
        saved => Ok(saved),
    }
}

fn write_session_at(path: &Path, session: &Session) -> Result<()> {
    write_json(path, session)
}

fn write_stop_at(path: &Path, session: &Session) -> Result<()> {
    write_json(
        path,
        &StopRequest {
            session: session.identity(),
        },
    )
}

fn read_stop_at(path: &Path) -> Result<Option<StopRequest>> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(state_io_err(path, error)),
    };
    match serde_json::from_slice(&data) {
        Ok(request) => Ok(Some(request)),
        Err(_) => {
            remove_state_file_at(path)?;
            Ok(None)
        }
    }
}

fn stop_requested_at(path: &Path, session: &Session) -> Result<bool> {
    let request = read_stop_at(path)?;
    Ok(request.is_some_and(|request| request.session == session.identity()))
}

fn clear_stop_at(path: &Path, session: &Session) -> Result<bool> {
    if !stop_requested_at(path, session)? {
        return Ok(false);
    }
    remove_state_file_at(path)?;
    Ok(true)
}

fn reconcile_stop_at(path: &Path, is_live: impl FnOnce(&ProcessRef) -> bool) -> Result<()> {
    let Some(request) = read_stop_at(path)? else {
        return Ok(());
    };
    if is_live(&request.session) {
        return Err(AppError::fail(format!(
            "stop request still targets a live process at {}",
            path.display()
        )));
    }
    remove_state_file_at(path)
}

fn remove_session_if_matches_at(path: &Path, expected: &Session) -> Result<bool> {
    let Some(actual) = read_session_at(path)? else {
        return Ok(false);
    };
    if actual.identity() != expected.identity() {
        return Ok(false);
    }
    remove_state_file_at(path)?;
    Ok(true)
}

fn read_lid_restore_at(path: &Path) -> Result<Option<LidRestore>> {
    let marker: Option<LidRestore> = read_json(path)?;
    if let Some(marker) = &marker {
        marker.validate()?;
    }
    Ok(marker)
}

#[cfg(any(test, windows, target_os = "macos"))]
fn write_lid_restore_at(path: &Path, marker: &LidRestore) -> Result<()> {
    marker.validate()?;
    if let Some(saved) = read_lid_restore_at(path)?
        && saved != *marker
    {
        return Err(AppError::fail(format!(
            "unresolved lid restoration marker already exists at {}",
            path.display()
        )));
    }
    write_json(path, marker)
}

fn read_watchdog_at(path: &Path) -> Result<Option<WatchdogState>> {
    let state: Option<WatchdogState> = read_json(path)?;
    match state {
        Some(state) if !state.is_valid() => Err(AppError::fail(format!(
            "invalid lid watchdog state at {}",
            path.display()
        ))),
        state => Ok(state),
    }
}

#[cfg(any(test, windows, target_os = "macos"))]
fn write_watchdog_at(path: &Path, state: &WatchdogState) -> Result<()> {
    if !state.is_valid() {
        return Err(AppError::fail("invalid lid watchdog state"));
    }
    if let Some(saved) = read_watchdog_at(path)?
        && saved != *state
    {
        return Err(AppError::fail(format!(
            "unresolved lid watchdog state already exists at {}",
            path.display()
        )));
    }
    write_json(path, state)
}

#[cfg(any(test, windows, target_os = "macos"))]
fn remove_watchdog_if_owner_at(path: &Path, owner: &ProcessRef) -> Result<bool> {
    let Some(state) = read_watchdog_at(path)? else {
        return Ok(false);
    };
    if &state.owner != owner {
        return Ok(false);
    }
    remove_state_file_at(path)?;
    Ok(true)
}

#[cfg(any(test, windows, target_os = "macos"))]
fn clear_lid_restore_at(path: &Path, expected: &LidRestore) -> Result<()> {
    match read_lid_restore_at(path)? {
        Some(actual) if &actual == expected => {
            fs::remove_file(path).map_err(|error| state_io_err(path, error))?;
            if let Some(dir) = path.parent() {
                sync_parent(dir).map_err(|error| state_io_err(dir, error))?;
            }
            Ok(())
        }
        Some(_) => Err(AppError::fail(format!(
            "lid restoration marker changed at {}; refusing to remove it",
            path.display()
        ))),
        None => Err(AppError::fail(format!(
            "lid restoration marker is missing at {}",
            path.display()
        ))),
    }
}

#[cfg_attr(windows, allow(dead_code))]
pub fn read_lid_restore() -> Result<Option<LidRestore>> {
    read_lid_restore_at(&lid_restore_file())
}

#[cfg_attr(windows, allow(dead_code))]
#[cfg(any(windows, target_os = "macos"))]
pub fn write_lid_restore(marker: &LidRestore) -> Result<()> {
    write_lid_restore_at(&lid_restore_file(), marker)
}

#[cfg_attr(windows, allow(dead_code))]
#[cfg(any(windows, target_os = "macos"))]
pub fn clear_lid_restore(expected: &LidRestore) -> Result<()> {
    clear_lid_restore_at(&lid_restore_file(), expected)
}

pub fn read_watchdog() -> Result<Option<WatchdogState>> {
    read_watchdog_at(&lid_watchdog_file())
}

#[cfg(any(windows, target_os = "macos"))]
pub fn write_watchdog(state: &WatchdogState) -> Result<()> {
    write_watchdog_at(&lid_watchdog_file(), state)
}

#[cfg(any(windows, target_os = "macos"))]
pub fn remove_watchdog_if_owner(owner: &ProcessRef) -> Result<bool> {
    remove_watchdog_if_owner_at(&lid_watchdog_file(), owner)
}

pub fn remove_watchdog_file() -> Result<()> {
    remove_state_file_at(&lid_watchdog_file())
}

pub fn read_saved_for_recovery() -> Result<Option<Session>> {
    read_session_at(&state_file())
}

pub fn read_if_alive() -> Result<Option<Session>> {
    let Some(session) = read_saved_for_recovery()? else {
        return Ok(None);
    };
    if session.matches_live_process() {
        Ok(Some(session))
    } else {
        remove_state_file()?;
        Ok(None)
    }
}

fn state_io_err(path: &Path, e: std::io::Error) -> AppError {
    AppError::fail(format!(
        "state IO failed at {}: {e}; set WAKE_STATE_DIR to a writable directory",
        path.display()
    ))
}

pub fn write(s: &Session) -> Result<()> {
    write_session_at(&state_file(), s)
}

pub fn request_stop(session: &Session) -> Result<()> {
    write_stop_at(&stop_file(), session)
}

pub fn stop_requested(session: &Session) -> Result<bool> {
    stop_requested_at(&stop_file(), session)
}

pub fn clear_stop(session: &Session) -> Result<bool> {
    clear_stop_at(&stop_file(), session)
}

pub fn reconcile_stop() -> Result<()> {
    reconcile_stop_at(&stop_file(), sysutil::process_matches)
}

pub fn remove_if_matches(session: &Session) -> Result<bool> {
    remove_session_if_matches_at(&state_file(), session)
}

fn remove_state_file_at(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(dir) = path.parent() {
                sync_parent(dir).map_err(|error| state_io_err(dir, error))?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(state_io_err(path, error)),
    }
}

pub fn remove_state_file() -> Result<()> {
    remove_state_file_at(&state_file())
}

pub struct LockGuard {
    file: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub fn acquire_lock() -> Result<LockGuard> {
    let dir = state_dir();
    fs::create_dir_all(&dir).map_err(|e| state_io_err(&dir, e))?;
    let path = dir.join("wake.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| state_io_err(&path, e))?;
    match file.try_lock() {
        Ok(()) => Ok(LockGuard { file }),
        Err(std::fs::TryLockError::WouldBlock) => Err(AppError::usage(
            "another wake invocation is in progress; try again",
        )),
        Err(std::fs::TryLockError::Error(e)) => Err(AppError::fail(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "wake-rs-{name}-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sample_session() -> Session {
        Session {
            owner: ProcessRef {
                pid: 4321,
                start: 1_700_000_000,
                command: "/usr/bin/wake".into(),
            },
            spec: crate::run::RunSpec {
                mode: crate::run::Mode::DisplaySystem,
                trigger: crate::run::Trigger::Timed {
                    seconds: 3600,
                    input: "1h".into(),
                },
                even_lid: false,
            },
            started_at: "2024-01-02T03:04:05Z".parse().unwrap(),
            ends_at: Some("2024-01-02T04:04:05Z".parse().unwrap()),
            note: None,
        }
    }

    #[test]
    fn session_json_round_trips() {
        let dir = TestDir::new("session-round-trip");
        let path = dir.join("session.json");
        let expected = sample_session();

        write_session_at(&path, &expected).unwrap();
        let saved = read_session_at(&path).unwrap().unwrap();

        assert_eq!(saved.owner, expected.owner);
        assert_eq!(saved.spec, expected.spec);
        assert_eq!(saved.started_at, expected.started_at);
        assert_eq!(saved.ends_at, expected.ends_at);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn missing_session_is_not_an_error() {
        let dir = TestDir::new("session-missing");
        assert!(
            read_session_at(&dir.join("missing.json"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn malformed_session_is_an_error_and_is_retained() {
        let dir = TestDir::new("session-malformed");
        let path = dir.join("session.json");
        fs::write(&path, br#"{"pid": "not valid"}"#).unwrap();

        let error = read_session_at(&path).unwrap_err();

        assert!(error.message().contains("invalid JSON"));
        assert!(path.exists());
    }

    #[test]
    fn semantic_session_validation_table() {
        let dir = TestDir::new("session-invalid");
        let path = dir.join("session.json");
        let mut cases: [Session; 3] = std::array::from_fn(|_| sample_session());
        cases[0].owner.pid = 0;
        cases[1].owner.start = 0;
        cases[2].owner.command.clear();

        for saved in cases {
            write_json(&path, &saved).unwrap();
            assert!(read_session_at(&path).is_err());
        }
    }

    #[test]
    fn lid_restore_markers_round_trip() {
        let dir = TestDir::new("lid-round-trip");
        let markers = [
            LidRestore::Macos { sleep_disabled: 1 },
            LidRestore::Windows {
                scheme_guid: "381b4222-f694-41f0-9685-ff5bb260df2e".into(),
                ac_action: 1,
                dc_action: 2,
            },
        ];

        for (index, marker) in markers.into_iter().enumerate() {
            let path = dir.join(&format!("lid-restore-{index}.json"));
            write_lid_restore_at(&path, &marker).unwrap();
            assert_eq!(read_lid_restore_at(&path).unwrap(), Some(marker));
        }
    }

    #[test]
    fn watchdog_state_round_trips_and_removes_only_exact_owner() {
        let dir = TestDir::new("watchdog-round-trip");
        let path = dir.join("lid-watchdog.json");
        let state = WatchdogState {
            owner: sample_session().owner,
            watchdog: ProcessRef {
                pid: 9876,
                start: 1_700_000_100,
                command: "/usr/bin/wake".into(),
            },
        };

        write_watchdog_at(&path, &state).unwrap();
        assert_eq!(read_watchdog_at(&path).unwrap(), Some(state.clone()));
        assert!(
            !remove_watchdog_if_owner_at(
                &path,
                &ProcessRef {
                    pid: 1,
                    ..state.owner.clone()
                }
            )
            .unwrap()
        );
        assert!(path.exists());
        assert!(remove_watchdog_if_owner_at(&path, &state.owner).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn malformed_watchdog_state_is_retained() {
        let dir = TestDir::new("watchdog-malformed");
        let path = dir.join("lid-watchdog.json");
        fs::write(&path, br#"{"owner":{}}"#).unwrap();

        assert!(read_watchdog_at(&path).is_err());
        assert!(path.exists());
    }

    #[test]
    fn watchdog_state_does_not_replace_another_owner() {
        let dir = TestDir::new("watchdog-owner");
        let path = dir.join("lid-watchdog.json");
        let mut first = WatchdogState {
            owner: sample_session().owner,
            watchdog: ProcessRef {
                pid: 9876,
                start: 1_700_000_100,
                command: "/usr/bin/wake".into(),
            },
        };
        write_watchdog_at(&path, &first).unwrap();
        first.owner.pid += 1;

        assert!(write_watchdog_at(&path, &first).is_err());
        assert_ne!(read_watchdog_at(&path).unwrap(), Some(first));
    }

    #[test]
    fn write_lid_restore_does_not_replace_an_unresolved_marker() {
        let dir = TestDir::new("lid-no-overwrite");
        let path = dir.join("lid-restore.json");
        let saved = LidRestore::Macos { sleep_disabled: 1 };
        let replacement = LidRestore::Macos { sleep_disabled: 0 };
        write_lid_restore_at(&path, &saved).unwrap();

        assert!(write_lid_restore_at(&path, &replacement).is_err());
        assert_eq!(read_lid_restore_at(&path).unwrap(), Some(saved));
    }

    #[test]
    fn invalid_lid_restore_is_an_error_and_is_retained() {
        let dir = TestDir::new("lid-invalid");
        let path = dir.join("lid-restore.json");
        fs::write(&path, br#"{"platform":"macos","sleep_disabled":2}"#).unwrap();

        let error = read_lid_restore_at(&path).unwrap_err();

        assert!(error.message().contains("SleepDisabled"));
        assert!(path.exists());
    }

    #[test]
    fn invalid_windows_scheme_guid_is_rejected() {
        let dir = TestDir::new("lid-invalid-guid");
        let path = dir.join("lid-restore.json");
        fs::write(
            &path,
            br#"{"platform":"windows","scheme_guid":"not-a-guid","ac_action":1,"dc_action":1}"#,
        )
        .unwrap();

        assert!(read_lid_restore_at(&path).is_err());
        assert!(path.exists());
    }

    #[test]
    fn clear_lid_restore_requires_the_exact_marker() {
        let dir = TestDir::new("lid-retention");
        let path = dir.join("lid-restore.json");
        let saved = LidRestore::Macos { sleep_disabled: 1 };
        let wrong = LidRestore::Macos { sleep_disabled: 0 };
        write_lid_restore_at(&path, &saved).unwrap();

        assert!(clear_lid_restore_at(&path, &wrong).is_err());
        assert_eq!(read_lid_restore_at(&path).unwrap(), Some(saved.clone()));

        clear_lid_restore_at(&path, &saved).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn removing_session_is_idempotent_and_keeps_lid_marker() {
        let dir = TestDir::new("session-remove");
        let session = dir.join("session.json");
        let marker = dir.join("lid-restore.json");
        write_session_at(&session, &sample_session()).unwrap();
        write_lid_restore_at(&marker, &LidRestore::Macos { sleep_disabled: 0 }).unwrap();

        remove_state_file_at(&session).unwrap();
        remove_state_file_at(&session).unwrap();

        assert!(!session.exists());
        assert!(marker.exists());
    }

    #[test]
    fn stop_request_replaces_disposable_marker_and_clears_exactly() {
        let dir = TestDir::new("stop-replace");
        let path = dir.join("stop.json");
        let expected = sample_session();
        let mut other = sample_session();
        other.owner.start += 1;

        write_stop_at(&path, &other).unwrap();
        write_stop_at(&path, &expected).unwrap();
        assert!(stop_requested_at(&path, &expected).unwrap());
        assert!(!stop_requested_at(&path, &other).unwrap());

        assert!(!clear_stop_at(&path, &other).unwrap());
        assert!(path.exists());
        assert!(clear_stop_at(&path, &expected).unwrap());
        assert!(!path.exists());

        fs::write(&path, b"not json").unwrap();
        assert!(!stop_requested_at(&path, &expected).unwrap());
        assert!(!path.exists());
        write_stop_at(&path, &expected).unwrap();
        assert!(stop_requested_at(&path, &expected).unwrap());
    }

    #[test]
    fn stop_marker_reconciliation_table() {
        let dir = TestDir::new("stop-reconcile");
        let session = sample_session();

        let missing = dir.join("missing.json");
        reconcile_stop_at(&missing, |_| false).unwrap();

        let dead = dir.join("dead.json");
        write_stop_at(&dead, &session).unwrap();
        reconcile_stop_at(&dead, |_| false).unwrap();
        assert!(!dead.exists());

        let live = dir.join("live.json");
        write_stop_at(&live, &session).unwrap();
        assert!(reconcile_stop_at(&live, |_| true).is_err());
        assert!(live.exists());

        let malformed = dir.join("malformed.json");
        fs::write(&malformed, b"not json").unwrap();
        reconcile_stop_at(&malformed, |_| false).unwrap();
        assert!(!malformed.exists());
    }

    #[test]
    fn conditional_session_removal_retains_changed_and_malformed_state() {
        let dir = TestDir::new("conditional-remove");
        let path = dir.join("session.json");
        let expected = sample_session();
        let mut changed = sample_session();
        changed.owner.pid += 1;

        write_session_at(&path, &changed).unwrap();
        assert!(!remove_session_if_matches_at(&path, &expected).unwrap());
        assert!(path.exists());

        fs::write(&path, b"not json").unwrap();
        assert!(remove_session_if_matches_at(&path, &expected).is_err());
        assert!(path.exists());

        write_session_at(&path, &expected).unwrap();
        assert!(remove_session_if_matches_at(&path, &expected).unwrap());
        assert!(!path.exists());
    }
}
