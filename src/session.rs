use crate::error::{AppError, Result};
use crate::run::RunSpec;
use crate::sysutil;
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::OnceLock;

#[cfg(windows)]
static HELPER_STATE_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn state_dir() -> PathBuf {
    #[cfg(windows)]
    if let Some(dir) = HELPER_STATE_DIR.get() {
        return dir.clone();
    }
    if let Some(dir) = std::env::var_os("WAKE_STATE_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    default_state_dir()
}

#[cfg(any(windows, target_os = "macos"))]
pub fn absolute_state_dir() -> Result<PathBuf> {
    std::path::absolute(state_dir()).map_err(AppError::from)
}

#[cfg(windows)]
pub fn set_helper_state_dir(dir: PathBuf) -> Result<()> {
    if !dir.is_absolute() {
        return Err(AppError::fail("helper state directory must be absolute"));
    }
    HELPER_STATE_DIR
        .set(dir)
        .map_err(|_| AppError::fail("helper state directory is already configured"))
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LeaseRef {
    pub pid: u32,
    pub token: String,
}

impl LeaseRef {
    fn has_valid_token(&self) -> bool {
        valid_lease_token(&self.token)
    }

    fn is_valid(&self) -> bool {
        self.pid > 0 && self.has_valid_token()
    }

    pub(crate) fn same_lease(&self, other: &Self) -> bool {
        self.token == other.token
    }
}

pub struct ProcessLeaseReservation {
    token: String,
    path: PathBuf,
    keep: bool,
}

impl ProcessLeaseReservation {
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn commit(mut self) -> String {
        self.keep = true;
        std::mem::take(&mut self.token)
    }

    pub fn is_claimed(&self) -> Result<bool> {
        process_lease_is_claimed_at(&self.path)
    }
}

impl Drop for ProcessLeaseReservation {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub struct ProcessLease {
    file: Option<File>,
    path: PathBuf,
    reference: LeaseRef,
}

impl Drop for ProcessLease {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
            drop(file);
        }
        let _ = fs::remove_file(&self.path);
    }
}

impl ProcessLease {
    pub fn reference(&self) -> LeaseRef {
        self.reference.clone()
    }
}

fn valid_lease_token(token: &str) -> bool {
    token.len() == 32
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn process_lease_path(dir: &Path, token: &str) -> Result<PathBuf> {
    if !valid_lease_token(token) {
        return Err(AppError::fail("invalid process lease token"));
    }
    Ok(dir.join(format!("process-{token}.lock")))
}

fn reserve_process_lease_at(dir: &Path) -> Result<ProcessLeaseReservation> {
    fs::create_dir_all(dir).map_err(|error| state_io_err(dir, error))?;
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| AppError::fail(format!("could not create process identity: {error}")))?;
    let token = format!("{:032x}", u128::from_ne_bytes(bytes));
    let path = process_lease_path(dir, &token)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| state_io_err(&path, error))?;
    Ok(ProcessLeaseReservation {
        token,
        path,
        keep: false,
    })
}

fn claim_process_lease_at(dir: &Path, token: &str, pid: u32) -> Result<ProcessLease> {
    let reference = LeaseRef {
        pid,
        token: token.into(),
    };
    if !reference.is_valid() {
        return Err(AppError::fail("invalid process lease"));
    }
    let path = process_lease_path(dir, token)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| state_io_err(&path, error))?;
    finish_process_lease_claim(file, path, reference)
}

fn finish_process_lease_claim(
    file: File,
    path: PathBuf,
    reference: LeaseRef,
) -> Result<ProcessLease> {
    file.lock().map_err(|error| {
        AppError::fail(format!(
            "could not claim process lease at {}: {error}",
            path.display()
        ))
    })?;
    if !path
        .try_exists()
        .map_err(|error| state_io_err(&path, error))?
    {
        return Err(AppError::fail("process lease was cancelled"));
    }
    Ok(ProcessLease {
        file: Some(file),
        path,
        reference,
    })
}

fn process_lease_is_claimed_at(path: &Path) -> Result<bool> {
    process_lease_is_locked_at(path, false)
}

fn process_lease_is_held_at(dir: &Path, token: &str) -> Result<bool> {
    process_lease_is_locked_at(&process_lease_path(dir, token)?, true)
}

fn process_lease_is_locked_at(path: &Path, remove_unlocked: bool) -> Result<bool> {
    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(state_io_err(path, error)),
    };
    match file.try_lock_shared() {
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(error)) => Err(state_io_err(path, error)),
        Ok(()) => {
            if remove_unlocked {
                remove_lease_file(path)?;
            }
            file.unlock().map_err(|error| state_io_err(path, error))?;
            drop(file);
            Ok(false)
        }
    }
}

fn remove_lease_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(state_io_err(path, error)),
    }
}

fn cleanup_process_leases_at(dir: &Path) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(state_io_err(dir, error)),
    };
    for entry in entries {
        let entry = entry.map_err(|error| state_io_err(dir, error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(token) = name
            .strip_prefix("process-")
            .and_then(|name| name.strip_suffix(".lock"))
            .filter(|token| valid_lease_token(token))
        else {
            continue;
        };
        process_lease_is_held_at(dir, token)?;
    }
    Ok(())
}

pub fn reserve_process_lease() -> Result<ProcessLeaseReservation> {
    reserve_process_lease_at(&state_dir())
}

pub fn claim_process_lease(token: &str) -> Result<ProcessLease> {
    claim_process_lease_at(&state_dir(), token, std::process::id())
}

pub fn process_lease_is_held(reference: &LeaseRef) -> Result<bool> {
    if !reference.has_valid_token() {
        return Err(AppError::fail("invalid process lease"));
    }
    process_lease_is_held_at(&state_dir(), &reference.token)
}

pub fn discard_process_lease(token: &str) -> Result<()> {
    process_lease_is_held_at(&state_dir(), token).map(|_| ())
}

pub fn cleanup_process_leases() -> Result<()> {
    cleanup_process_leases_at(&state_dir())
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
    pub owner: LeaseRef,
    pub spec: RunSpec,
    pub started_at: DateTime<Utc>,
    pub ends_at: Option<DateTime<Utc>>,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WatchdogState {
    pub owner: LeaseRef,
    pub watchdog: LeaseRef,
    pub ready: bool,
}

impl WatchdogState {
    fn is_valid(&self) -> bool {
        self.owner.is_valid()
            && self.watchdog.has_valid_token()
            && (!self.ready || self.watchdog.pid > 0)
            && self.owner.token != self.watchdog.token
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
        self.owner.has_valid_token()
            && self.spec.validate().is_ok()
            && self.ends_at == self.spec.trigger.session_ends_at(self.started_at)
    }

    pub fn owner_is_live(&self) -> Result<bool> {
        sysutil::lease_is_live(&self.owner)
    }

    pub fn identity(&self) -> LeaseRef {
        self.owner.clone()
    }
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct StopRequest {
    session: LeaseRef,
}

impl StopRequest {
    fn is_valid(&self) -> bool {
        self.session.has_valid_token()
    }
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
        let mut file = create_temp(&tmp).map_err(|error| state_io_err(&tmp, error))?;
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

fn create_temp(path: &Path) -> std::io::Result<File> {
    let open = || OpenOptions::new().write(true).create_new(true).open(path);
    match open() {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            fs::remove_file(path)?;
            open()
        }
        result => result,
    }
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
    write_stop_owner_at(path, &session.owner)
}

fn write_stop_owner_at(path: &Path, owner: &LeaseRef) -> Result<()> {
    if !owner.has_valid_token() {
        return Err(AppError::fail("invalid process lease"));
    }
    write_json(
        path,
        &StopRequest {
            session: owner.clone(),
        },
    )
}

fn read_stop_at(path: &Path) -> Result<Option<StopRequest>> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(state_io_err(path, error)),
    };
    match serde_json::from_slice::<StopRequest>(&data) {
        Ok(request) if request.is_valid() => Ok(Some(request)),
        _ => {
            remove_state_file_at(path)?;
            Ok(None)
        }
    }
}

fn stop_requested_at(path: &Path, session: &Session) -> Result<bool> {
    let request = read_stop_at(path)?;
    Ok(request.is_some_and(|request| request.session.same_lease(&session.owner)))
}

fn clear_stop_at(path: &Path, session: &Session) -> Result<bool> {
    if !stop_requested_at(path, session)? {
        return Ok(false);
    }
    remove_state_file_at(path)?;
    Ok(true)
}

fn reconcile_stop_at(path: &Path, is_live: impl FnOnce(&LeaseRef) -> Result<bool>) -> Result<()> {
    let Some(request) = read_stop_at(path)? else {
        return Ok(());
    };
    if is_live(&request.session)? {
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

fn remove_session_if_owner_at(path: &Path, owner: &LeaseRef) -> Result<bool> {
    let Some(actual) = read_session_at(path)? else {
        return Ok(false);
    };
    if !actual.owner.same_lease(owner) {
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
        && (saved.owner != state.owner
            || !saved.watchdog.same_lease(&state.watchdog)
            || (saved.watchdog.pid != state.watchdog.pid && saved.watchdog.pid != 0)
            || (saved.ready && !state.ready))
    {
        return Err(AppError::fail(format!(
            "unresolved lid watchdog state already exists at {}",
            path.display()
        )));
    }
    write_json(path, state)
}

#[cfg(any(test, windows, target_os = "macos"))]
fn remove_watchdog_if_owner_at(path: &Path, owner: &LeaseRef) -> Result<bool> {
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
pub fn remove_watchdog_if_owner(owner: &LeaseRef) -> Result<bool> {
    remove_watchdog_if_owner_at(&lid_watchdog_file(), owner)
}

pub fn remove_watchdog_file() -> Result<()> {
    remove_state_file_at(&lid_watchdog_file())
}

pub fn read_saved_for_recovery() -> Result<Option<Session>> {
    read_session_at(&state_file())
}

pub fn read_current() -> Result<Option<Session>> {
    let Some(session) = read_saved_for_recovery()? else {
        return Ok(None);
    };
    if session.owner_is_live()? || session.spec.even_lid {
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

fn write_pending_at(path: &Path, session: &Session) -> Result<()> {
    if session.owner.pid != 0 || !session.is_valid() {
        return Err(AppError::fail("invalid pending session"));
    }
    if read_session_at(path)?.is_some() {
        return Err(AppError::fail("session state already exists"));
    }
    write_session_at(path, session)
}

fn write_ready_at(path: &Path, session: &Session) -> Result<()> {
    if !session.owner.is_valid() || !session.is_valid() {
        return Err(AppError::fail("invalid ready session"));
    }
    let Some(pending) = read_session_at(path)? else {
        return Err(AppError::fail("pending session was cancelled"));
    };
    if pending.owner.pid != 0
        || !pending.owner.same_lease(&session.owner)
        || pending.spec != session.spec
    {
        return Err(AppError::fail("pending session changed before readiness"));
    }
    write_session_at(path, session)
}

pub fn write_pending(session: &Session) -> Result<()> {
    write_pending_at(&state_file(), session)
}

pub fn write_ready(session: &Session) -> Result<()> {
    write_ready_at(&state_file(), session)
}

pub fn request_stop(session: &Session) -> Result<()> {
    write_stop_at(&stop_file(), session)
}

#[cfg(any(windows, target_os = "macos"))]
pub fn request_stop_owner(owner: &LeaseRef) -> Result<()> {
    write_stop_owner_at(&stop_file(), owner)
}

pub fn stop_requested(session: &Session) -> Result<bool> {
    stop_requested_at(&stop_file(), session)
}

pub fn clear_stop(session: &Session) -> Result<bool> {
    clear_stop_at(&stop_file(), session)
}

pub fn reconcile_stop() -> Result<()> {
    reconcile_stop_at(&stop_file(), sysutil::lease_is_live)
}

pub fn remove_if_matches(session: &Session) -> Result<bool> {
    remove_session_if_matches_at(&state_file(), session)
}

pub fn remove_if_owner(owner: &LeaseRef) -> Result<bool> {
    remove_session_if_owner_at(&state_file(), owner)
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

fn open_lock_file() -> Result<File> {
    let dir = state_dir();
    fs::create_dir_all(&dir).map_err(|e| state_io_err(&dir, e))?;
    let path = dir.join("wake.lock");
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| state_io_err(&path, e))
}

pub fn try_acquire_lock() -> Result<Option<LockGuard>> {
    let file = open_lock_file()?;
    match file.try_lock() {
        Ok(()) => Ok(Some(LockGuard { file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(AppError::fail(e.to_string())),
    }
}

pub fn acquire_lock() -> Result<LockGuard> {
    try_acquire_lock()?
        .ok_or_else(|| AppError::usage("another wake invocation is in progress; try again"))
}

pub fn acquire_lock_wait() -> Result<LockGuard> {
    let file = open_lock_file()?;
    file.lock()
        .map_err(|error| AppError::fail(format!("could not acquire state lock: {error}")))?;
    Ok(LockGuard { file })
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
            owner: LeaseRef {
                pid: 4321,
                token: "0123456789abcdef0123456789abcdef".into(),
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
    fn session_startup_transition_is_identity_bound_and_monotonic() {
        let dir = TestDir::new("session-startup");
        let path = dir.join("session.json");
        let mut pending = sample_session();
        pending.owner.pid = 0;
        write_pending_at(&path, &pending).unwrap();

        let mut ready = pending.clone();
        ready.owner.pid = 42;
        let mut wrong = ready.clone();
        wrong.owner.token = "11111111111111111111111111111111".into();
        assert!(write_ready_at(&path, &wrong).is_err());

        write_ready_at(&path, &ready).unwrap();
        assert_eq!(read_session_at(&path).unwrap().unwrap().owner, ready.owner);
        assert!(write_ready_at(&path, &ready).is_err());

        fs::remove_file(&path).unwrap();
        assert!(write_ready_at(&path, &ready).is_err());
    }

    #[test]
    fn atomic_write_does_not_follow_an_existing_temp_link() {
        let dir = TestDir::new("state-temp-link");
        let path = dir.join("session.json");
        let victim = dir.join("victim");
        fs::write(&victim, b"keep").unwrap();
        fs::hard_link(&victim, path.with_extension("json.tmp")).unwrap();

        write_session_at(&path, &sample_session()).unwrap();

        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert!(read_session_at(&path).unwrap().is_some());
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
    fn process_lease_is_live_only_while_held() {
        let dir = TestDir::new("process-lease");
        let abandoned = reserve_process_lease_at(&dir.0).unwrap();
        let abandoned_path = process_lease_path(&dir.0, abandoned.token()).unwrap();
        drop(abandoned);
        assert!(!abandoned_path.exists());

        let reservation = reserve_process_lease_at(&dir.0).unwrap();
        assert!(!reservation.is_claimed().unwrap());
        let token = reservation.token().to_owned();
        let lease = claim_process_lease_at(&dir.0, &token, 42).unwrap();
        assert!(reservation.is_claimed().unwrap());
        let token = reservation.commit();

        assert!(process_lease_is_held_at(&dir.0, &token).unwrap());
        cleanup_process_leases_at(&dir.0).unwrap();
        assert!(process_lease_path(&dir.0, &token).unwrap().exists());
        drop(lease);
        assert!(!process_lease_is_held_at(&dir.0, &token).unwrap());
        assert!(!process_lease_path(&dir.0, &token).unwrap().exists());

        let crashed = reserve_process_lease_at(&dir.0).unwrap().commit();
        cleanup_process_leases_at(&dir.0).unwrap();
        assert!(!process_lease_path(&dir.0, &crashed).unwrap().exists());

        let delayed = reserve_process_lease_at(&dir.0).unwrap().commit();
        let delayed_path = process_lease_path(&dir.0, &delayed).unwrap();
        let delayed_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&delayed_path)
            .unwrap();
        assert!(!process_lease_is_held_at(&dir.0, &delayed).unwrap());
        assert!(
            finish_process_lease_claim(
                delayed_file,
                delayed_path,
                LeaseRef {
                    pid: 42,
                    token: delayed,
                },
            )
            .is_err()
        );

        let concurrent = reserve_process_lease_at(&dir.0).unwrap().commit();
        let concurrent_path = process_lease_path(&dir.0, &concurrent).unwrap();
        let probe = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&concurrent_path)
            .unwrap();
        probe.try_lock_shared().unwrap();
        assert!(!process_lease_is_held_at(&dir.0, &concurrent).unwrap());
        probe.unlock().unwrap();
        assert!(!concurrent_path.exists());
    }

    #[test]
    fn process_lease_rejects_unsafe_tokens() {
        let dir = TestDir::new("process-lease-token");
        let short = "a".repeat(31);
        for token in ["", "../escape", "ABCDEF", short.as_str()] {
            assert!(process_lease_path(&dir.0, token).is_err());
            assert!(claim_process_lease_at(&dir.0, token, 42).is_err());
        }
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
        let mut cases: [Session; 2] = std::array::from_fn(|_| sample_session());
        cases[0].owner.token.clear();
        cases[1].ends_at = None;

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
            watchdog: LeaseRef {
                pid: 0,
                token: "fedcba9876543210fedcba9876543210".into(),
            },
            ready: false,
        };

        write_watchdog_at(&path, &state).unwrap();
        assert_eq!(read_watchdog_at(&path).unwrap(), Some(state.clone()));
        let starting = WatchdogState {
            watchdog: LeaseRef {
                pid: 9876,
                ..state.watchdog.clone()
            },
            ..state.clone()
        };
        write_watchdog_at(&path, &starting).unwrap();
        let ready = WatchdogState {
            ready: true,
            ..starting.clone()
        };
        write_watchdog_at(&path, &ready).unwrap();
        assert_eq!(read_watchdog_at(&path).unwrap(), Some(ready));
        assert!(write_watchdog_at(&path, &starting).is_err());
        assert!(
            !remove_watchdog_if_owner_at(
                &path,
                &LeaseRef {
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
            watchdog: LeaseRef {
                pid: 9876,
                token: "fedcba9876543210fedcba9876543210".into(),
            },
            ready: false,
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
        other.owner.token = "11111111111111111111111111111111".into();

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
        write_json(
            &path,
            &StopRequest {
                session: LeaseRef {
                    pid: 0,
                    token: expected.owner.token.clone(),
                },
            },
        )
        .unwrap();
        assert!(stop_requested_at(&path, &expected).unwrap());
        assert!(clear_stop_at(&path, &expected).unwrap());
        write_stop_at(&path, &expected).unwrap();
        assert!(stop_requested_at(&path, &expected).unwrap());
    }

    #[test]
    fn stop_marker_reconciliation_table() {
        let dir = TestDir::new("stop-reconcile");
        let session = sample_session();

        let missing = dir.join("missing.json");
        reconcile_stop_at(&missing, |_| Ok(false)).unwrap();

        let dead = dir.join("dead.json");
        write_stop_at(&dead, &session).unwrap();
        reconcile_stop_at(&dead, |_| Ok(false)).unwrap();
        assert!(!dead.exists());

        let live = dir.join("live.json");
        write_stop_at(&live, &session).unwrap();
        assert!(reconcile_stop_at(&live, |_| Ok(true)).is_err());
        assert!(live.exists());

        let unknown = dir.join("unknown.json");
        write_stop_at(&unknown, &session).unwrap();
        assert!(reconcile_stop_at(&unknown, |_| Err(AppError::fail("unreadable lease"))).is_err());
        assert!(unknown.exists());

        let malformed = dir.join("malformed.json");
        fs::write(&malformed, b"not json").unwrap();
        reconcile_stop_at(&malformed, |_| Ok(false)).unwrap();
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
