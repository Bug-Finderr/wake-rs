//! Strict session state, startup requests, marker files, and the advisory lock.

use crate::error::{AppError, Result};
use crate::platform;
use crate::sysutil;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const STATE_VERSION: &str = "2";
#[cfg(windows)]
const REQUEST_VERSION: &str = "1";

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
        let path = PathBuf::from(xdg);
        if path.is_absolute() {
            return path.join("wake");
        }
    }
    home().join(".local").join("state").join("wake")
}

pub fn state_file() -> PathBuf {
    state_dir().join("session.properties")
}

fn home() -> PathBuf {
    #[cfg(windows)]
    let value = std::env::var_os("USERPROFILE");
    #[cfg(unix)]
    let value = std::env::var_os("HOME");
    value
        .map(PathBuf::from)
        .or_else(std::env::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Clone, Default, Debug)]
pub struct Session {
    pub pid: u32,
    pub mode: String,
    pub trigger: String,
    pub detail: String,
    pub started_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub process_start: u64,
    pub process_command: String,
    pub even_lid: bool,
    #[cfg(not(windows))]
    pub prior_disable_sleep: i32,
    #[cfg(windows)]
    pub guardian_pid: u32,
    #[cfg(windows)]
    pub guardian_start: u64,
    #[cfg(windows)]
    pub original_scheme: String,
    #[cfg(windows)]
    pub original_ac: u32,
    #[cfg(windows)]
    pub original_dc: u32,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(not(windows))]
    pub fn capture_process_identity(&mut self) -> Result<()> {
        let identity = sysutil::capture_identity(self.pid)?;
        self.process_start = identity.start;
        self.process_command = identity.command;
        Ok(())
    }

    pub fn matches_identity(&self, live: &sysutil::Identity) -> bool {
        let command_matches = if cfg!(windows) {
            self.process_command.eq_ignore_ascii_case(&live.command)
        } else {
            self.process_command == live.command
        };
        self.process_start == live.start && command_matches && is_expected_command(&live.command)
    }

    #[cfg(not(windows))]
    pub fn matches_live_process(&self) -> bool {
        sysutil::live_identity(self.pid).is_some_and(|live| self.matches_identity(&live))
    }
}

fn is_expected_command(command: &str) -> bool {
    let base = Path::new(command)
        .file_name()
        .map(|file| file.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    platform::expected_command_basenames().contains(&base.as_str())
}

pub enum SavedState {
    Valid(Session),
    Malformed(MalformedState),
}

pub struct MalformedState {
    lid_recovery_hints: bool,
}

impl MalformedState {
    pub fn has_lid_recovery_hints(&self) -> bool {
        self.lid_recovery_hints
    }
}

pub fn read_saved_for_recovery() -> Option<SavedState> {
    let path = state_file();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => {
            return Some(SavedState::Malformed(MalformedState {
                lid_recovery_hints: true,
            }));
        }
    };
    Some(match parse_session_bytes(&bytes) {
        Ok(session) => SavedState::Valid(session),
        Err(_) => SavedState::Malformed(malformed_from_bytes(&bytes)),
    })
}

#[cfg(windows)]
pub fn read_state_bytes() -> Result<Vec<u8>> {
    let path = state_file();
    fs::read(&path).map_err(|error| state_io_err(&path, error))
}

/// Valid live-or-not session, deleting only stale valid state or malformed state without recovery data.
#[cfg(not(windows))]
pub fn read_if_alive(may_delete_malformed: bool) -> Option<Session> {
    match read_saved_for_recovery() {
        Some(SavedState::Valid(session)) => {
            if session.matches_live_process() {
                Some(session)
            } else {
                delete_state_file();
                None
            }
        }
        Some(SavedState::Malformed(malformed)) => {
            if may_delete_malformed && !malformed.has_lid_recovery_hints() {
                delete_state_file();
            }
            None
        }
        None => None,
    }
}

fn parse_session_bytes(bytes: &[u8]) -> Result<Session> {
    let text = std::str::from_utf8(bytes).map_err(|_| AppError::fail("state is not UTF-8"))?;
    let properties = parse_strict_properties(text)?;
    build_session(&properties)
}

fn parse_strict_properties(text: &str) -> Result<HashMap<String, String>> {
    let mut properties = HashMap::new();
    for line in text.lines() {
        if line.is_empty() {
            return Err(AppError::fail("state contains an empty line"));
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| AppError::fail("state contains a line without '='"))?;
        if key.is_empty()
            || key.trim() != key
            || properties.insert(key.into(), value.into()).is_some()
        {
            return Err(AppError::fail("state contains an invalid or duplicate key"));
        }
    }
    if properties.is_empty() {
        return Err(AppError::fail("state is empty"));
    }
    Ok(properties)
}

fn build_session(properties: &HashMap<String, String>) -> Result<Session> {
    require_value(properties, "version", STATE_VERSION)?;
    let even_lid = parse_bool(required(properties, "evenLid")?)?;
    let mut expected: HashSet<&str> = [
        "version",
        "pid",
        "mode",
        "trigger",
        "detail",
        "startedAt",
        "endsAt",
        "processStart",
        "processCommand",
        "evenLid",
    ]
    .into_iter()
    .collect();
    #[cfg(not(windows))]
    if even_lid {
        expected.insert("priorDisableSleep");
    }
    #[cfg(windows)]
    if even_lid {
        expected.extend([
            "guardianPid",
            "guardianStart",
            "originalScheme",
            "originalAc",
            "originalDc",
        ]);
    }
    require_exact_keys(properties, &expected)?;

    let started_at = parse_timestamp(required(properties, "startedAt")?)?;
    let ends_at = match required(properties, "endsAt")? {
        "" => None,
        value => Some(parse_timestamp(value)?),
    };
    let session = Session {
        pid: parse_positive(required(properties, "pid")?, "pid")?,
        mode: required(properties, "mode")?.to_string(),
        trigger: required(properties, "trigger")?.to_string(),
        detail: required(properties, "detail")?.to_string(),
        started_at: Some(started_at),
        ends_at,
        process_start: parse_positive(required(properties, "processStart")?, "processStart")?,
        process_command: required(properties, "processCommand")?.to_string(),
        even_lid,
        #[cfg(not(windows))]
        prior_disable_sleep: if even_lid {
            parse_disable_sleep(required(properties, "priorDisableSleep")?)?
        } else {
            0
        },
        #[cfg(windows)]
        guardian_pid: if even_lid {
            parse_positive(required(properties, "guardianPid")?, "guardianPid")?
        } else {
            0
        },
        #[cfg(windows)]
        guardian_start: if even_lid {
            parse_positive(required(properties, "guardianStart")?, "guardianStart")?
        } else {
            0
        },
        #[cfg(windows)]
        original_scheme: if even_lid {
            let value = required(properties, "originalScheme")?;
            platform::parse_guid(value)?;
            value.to_string()
        } else {
            String::new()
        },
        #[cfg(windows)]
        original_ac: if even_lid {
            parse_u32(required(properties, "originalAc")?, "originalAc")?
        } else {
            0
        },
        #[cfg(windows)]
        original_dc: if even_lid {
            parse_u32(required(properties, "originalDc")?, "originalDc")?
        } else {
            0
        },
    };
    validate_session_strings(&session)?;
    Ok(session)
}

fn malformed_from_bytes(bytes: &[u8]) -> MalformedState {
    let text = String::from_utf8_lossy(bytes);
    MalformedState {
        lid_recovery_hints: [
            "evenLid=true",
            "priorDisableSleep=",
            "guardianPid=",
            "originalScheme=",
            "originalAc=",
            "originalDc=",
        ]
        .iter()
        .any(|hint| text.contains(hint)),
    }
}

fn serialize_session(session: &Session) -> Result<String> {
    validate_session_strings(session)?;
    if session.pid == 0 || session.process_start == 0 || session.started_at.is_none() {
        return Err(AppError::fail("session identity or start time is missing"));
    }
    let mut fields = vec![
        ("version", STATE_VERSION.to_string()),
        ("pid", session.pid.to_string()),
        ("mode", session.mode.clone()),
        ("trigger", session.trigger.clone()),
        ("detail", session.detail.clone()),
        ("startedAt", timestamp(session.started_at)),
        (
            "endsAt",
            session
                .ends_at
                .map(|time| time.to_rfc3339())
                .unwrap_or_default(),
        ),
        ("processStart", session.process_start.to_string()),
        ("processCommand", session.process_command.clone()),
        ("evenLid", session.even_lid.to_string()),
    ];
    #[cfg(not(windows))]
    if session.even_lid {
        parse_disable_sleep(&session.prior_disable_sleep.to_string())?;
        fields.push(("priorDisableSleep", session.prior_disable_sleep.to_string()));
    }
    #[cfg(windows)]
    if session.even_lid {
        if session.guardian_pid == 0 || session.guardian_start == 0 {
            return Err(AppError::fail("guardian identity is missing"));
        }
        platform::parse_guid(&session.original_scheme)?;
        fields.extend([
            ("guardianPid", session.guardian_pid.to_string()),
            ("guardianStart", session.guardian_start.to_string()),
            ("originalScheme", session.original_scheme.clone()),
            ("originalAc", session.original_ac.to_string()),
            ("originalDc", session.original_dc.to_string()),
        ]);
    }
    Ok(fields
        .into_iter()
        .map(|(key, value)| format!("{key}={value}\n"))
        .collect())
}

fn validate_session_strings(session: &Session) -> Result<()> {
    for value in [
        &session.mode,
        &session.trigger,
        &session.detail,
        &session.process_command,
    ] {
        validate_value(value)?;
        if value.is_empty() {
            return Err(AppError::fail("state contains an empty required value"));
        }
    }
    if !matches!(session.mode.as_str(), "system-only" | "display+system") {
        return Err(AppError::fail("state contains an invalid mode"));
    }
    if !matches!(
        session.trigger.as_str(),
        "indefinite" | "timed" | "until-time" | "until-charge" | "while-pid" | "while-app"
    ) {
        return Err(AppError::fail("state contains an invalid trigger"));
    }
    if !is_expected_command(&session.process_command) {
        return Err(AppError::fail(
            "state contains an unexpected process command",
        ));
    }
    let expects_end = matches!(session.trigger.as_str(), "timed" | "until-time");
    if session.ends_at.is_some() != expects_end
        || session
            .ends_at
            .zip(session.started_at)
            .is_some_and(|(end, start)| end <= start)
    {
        return Err(AppError::fail("state contains inconsistent timestamps"));
    }
    Ok(())
}

fn validate_value(value: &str) -> Result<()> {
    if value.contains(['\r', '\n']) {
        Err(AppError::fail("state values cannot contain line breaks"))
    } else {
        Ok(())
    }
}

fn required<'a>(properties: &'a HashMap<String, String>, key: &str) -> Result<&'a str> {
    properties
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| AppError::fail(format!("state is missing {key}")))
}

fn require_value(properties: &HashMap<String, String>, key: &str, expected: &str) -> Result<()> {
    if required(properties, key)? == expected {
        Ok(())
    } else {
        Err(AppError::fail(format!("unsupported {key}")))
    }
}

fn require_exact_keys(
    properties: &HashMap<String, String>,
    expected: &HashSet<&str>,
) -> Result<()> {
    if properties.len() == expected.len()
        && properties.keys().all(|key| expected.contains(key.as_str()))
    {
        Ok(())
    } else {
        Err(AppError::fail("state has missing or unknown fields"))
    }
}

fn parse_timestamp(raw: &str) -> Result<DateTime<Utc>> {
    let time = DateTime::parse_from_rfc3339(raw)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|_| AppError::fail("invalid state timestamp"))?;
    if time.to_rfc3339() == raw {
        Ok(time)
    } else {
        Err(AppError::fail("state timestamp is not canonical UTC"))
    }
}

fn parse_bool(raw: &str) -> Result<bool> {
    match raw {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(AppError::fail("invalid state boolean")),
    }
}

fn parse_positive<T>(raw: &str, name: &str) -> Result<T>
where
    T: std::str::FromStr + Default + PartialEq + ToString,
{
    let value = raw
        .parse::<T>()
        .map_err(|_| AppError::fail(format!("invalid {name}")))?;
    if value == T::default() || value.to_string() != raw {
        Err(AppError::fail(format!("invalid {name}")))
    } else {
        Ok(value)
    }
}

#[cfg(windows)]
fn parse_u32(raw: &str, name: &str) -> Result<u32> {
    let value = raw
        .parse::<u32>()
        .map_err(|_| AppError::fail(format!("invalid {name}")))?;
    if value.to_string() == raw {
        Ok(value)
    } else {
        Err(AppError::fail(format!("invalid {name}")))
    }
}

#[cfg(not(windows))]
fn parse_disable_sleep(raw: &str) -> Result<i32> {
    match raw.parse() {
        Ok(value @ (0 | 1)) => Ok(value),
        _ => Err(AppError::fail("priorDisableSleep must be 0 or 1")),
    }
}

fn state_io_err(path: &Path, error: std::io::Error) -> AppError {
    AppError::fail(format!(
        "state IO failed at {}: {error}; set WAKE_STATE_DIR to a writable directory",
        path.display()
    ))
}

pub fn write(session: &Session) -> Result<()> {
    write_at(&state_file(), serialize_session(session)?.as_bytes())
}

pub fn delete_state_file() {
    let _ = fs::remove_file(state_file());
}

fn timestamp(time: Option<DateTime<Utc>>) -> String {
    time.map(|time| time.to_rfc3339()).unwrap_or_default()
}

fn write_at(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| AppError::fail("state path has no parent directory"))?;
    fs::create_dir_all(dir).map_err(|error| state_io_err(dir, error))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| AppError::fail("state path has no file name"))?
        .to_string_lossy();
    let tmp = dir.join(format!("{file_name}.tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .map_err(|error| state_io_err(&tmp, error))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| state_io_err(&tmp, error))?;
        fs::rename(&tmp, path).map_err(|error| state_io_err(path, error))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
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
    fs::create_dir_all(&dir).map_err(|error| state_io_err(&dir, error))?;
    let path = dir.join("wake.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|error| state_io_err(&path, error))?;
    match file.try_lock() {
        Ok(()) => Ok(LockGuard { file }),
        Err(std::fs::TryLockError::WouldBlock) => Err(AppError::usage(
            "another wake invocation is in progress; try again",
        )),
        Err(std::fs::TryLockError::Error(error)) => Err(AppError::fail(error.to_string())),
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardianMode {
    Start,
    Recover,
}

#[cfg(windows)]
#[derive(Clone, Debug)]
pub struct GuardianRequest {
    pub mode: GuardianMode,
    pub session: Session,
    pub expected_state: Option<Vec<u8>>,
}

#[cfg(windows)]
pub struct GuardianPaths {
    pub request: PathBuf,
    pub ready: PathBuf,
    pub error: PathBuf,
}

#[cfg(windows)]
pub fn guardian_paths(mode: GuardianMode, pid: u32, start: u64) -> GuardianPaths {
    let mode = match mode {
        GuardianMode::Start => "start",
        GuardianMode::Recover => "recover",
    };
    let stem = format!("guard-{mode}-{pid}-{start}");
    GuardianPaths {
        request: state_dir().join(format!("{stem}.request")),
        ready: state_dir().join(format!("{stem}.ready")),
        error: state_dir().join(format!("{stem}.error")),
    }
}

#[cfg(windows)]
pub fn write_guardian_request(request: &GuardianRequest) -> Result<GuardianPaths> {
    let paths = guardian_paths(
        request.mode,
        request.session.pid,
        request.session.process_start,
    );
    for path in [&paths.request, &paths.ready, &paths.error] {
        let _ = fs::remove_file(path);
    }
    let text = serialize_guardian_request(request)?;
    write_at(&paths.request, text.as_bytes())?;
    Ok(paths)
}

#[cfg(windows)]
pub fn read_guardian_request(path: &Path) -> Result<GuardianRequest> {
    if !path.is_absolute() {
        return Err(AppError::fail("guardian request path must be absolute"));
    }
    let bytes = fs::read(path).map_err(|error| state_io_err(path, error))?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| AppError::fail("guardian request is not UTF-8"))?;
    let properties = parse_strict_properties(text)?;
    require_value(&properties, "requestVersion", REQUEST_VERSION)?;
    match required(&properties, "requestMode")? {
        "start" => parse_start_request(properties),
        "recover" => parse_recovery_request(properties),
        _ => Err(AppError::fail("invalid guardian request mode")),
    }
}

#[cfg(windows)]
fn serialize_guardian_request(request: &GuardianRequest) -> Result<String> {
    validate_session_strings(&request.session)?;
    let mode = match request.mode {
        GuardianMode::Start => "start",
        GuardianMode::Recover => "recover",
    };
    let mut fields = vec![
        ("requestVersion", REQUEST_VERSION.to_string()),
        ("requestMode", mode.to_string()),
        ("pid", request.session.pid.to_string()),
        ("processStart", request.session.process_start.to_string()),
    ];
    match request.mode {
        GuardianMode::Start => {
            if !request.session.even_lid || request.expected_state.is_some() {
                return Err(AppError::fail("invalid guardian start request"));
            }
            fields.extend([
                ("mode", request.session.mode.clone()),
                ("trigger", request.session.trigger.clone()),
                ("detail", request.session.detail.clone()),
                ("startedAt", timestamp(request.session.started_at)),
                (
                    "endsAt",
                    request
                        .session
                        .ends_at
                        .map(|time| time.to_rfc3339())
                        .unwrap_or_default(),
                ),
                ("processCommand", request.session.process_command.clone()),
            ]);
        }
        GuardianMode::Recover => {
            let expected = request
                .expected_state
                .as_ref()
                .ok_or_else(|| AppError::fail("recovery request is missing state bytes"))?;
            fields.push(("stateHex", hex_encode(expected)));
        }
    }
    Ok(fields
        .into_iter()
        .map(|(key, value)| format!("{key}={value}\n"))
        .collect())
}

#[cfg(windows)]
fn parse_start_request(properties: HashMap<String, String>) -> Result<GuardianRequest> {
    let expected = [
        "requestVersion",
        "requestMode",
        "pid",
        "processStart",
        "mode",
        "trigger",
        "detail",
        "startedAt",
        "endsAt",
        "processCommand",
    ]
    .into_iter()
    .collect();
    require_exact_keys(&properties, &expected)?;
    let session = Session {
        pid: parse_positive(required(&properties, "pid")?, "pid")?,
        mode: required(&properties, "mode")?.to_string(),
        trigger: required(&properties, "trigger")?.to_string(),
        detail: required(&properties, "detail")?.to_string(),
        started_at: Some(parse_timestamp(required(&properties, "startedAt")?)?),
        ends_at: match required(&properties, "endsAt")? {
            "" => None,
            value => Some(parse_timestamp(value)?),
        },
        process_start: parse_positive(required(&properties, "processStart")?, "processStart")?,
        process_command: required(&properties, "processCommand")?.to_string(),
        even_lid: true,
        guardian_pid: 0,
        guardian_start: 0,
        original_scheme: String::new(),
        original_ac: 0,
        original_dc: 0,
    };
    validate_session_strings(&session)?;
    Ok(GuardianRequest {
        mode: GuardianMode::Start,
        session,
        expected_state: None,
    })
}

#[cfg(windows)]
fn parse_recovery_request(properties: HashMap<String, String>) -> Result<GuardianRequest> {
    let expected = [
        "requestVersion",
        "requestMode",
        "pid",
        "processStart",
        "stateHex",
    ]
    .into_iter()
    .collect();
    require_exact_keys(&properties, &expected)?;
    let expected_state = hex_decode(required(&properties, "stateHex")?)?;
    let session = parse_session_bytes(&expected_state)?;
    if !session.even_lid
        || session.pid != parse_positive(required(&properties, "pid")?, "pid")?
        || session.process_start
            != parse_positive(required(&properties, "processStart")?, "processStart")?
    {
        return Err(AppError::fail(
            "recovery request identity does not match its state",
        ));
    }
    Ok(GuardianRequest {
        mode: GuardianMode::Recover,
        session,
        expected_state: Some(expected_state),
    })
}

#[cfg(windows)]
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(windows)]
fn hex_decode(raw: &str) -> Result<Vec<u8>> {
    if !raw.len().is_multiple_of(2) {
        return Err(AppError::fail("invalid recovery state encoding"));
    }
    raw.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).expect("ASCII-sized chunk");
            u8::from_str_radix(pair, 16)
                .map_err(|_| AppError::fail("invalid recovery state encoding"))
        })
        .collect()
}

#[cfg(windows)]
pub fn marker_paths_for_request(request: &GuardianRequest) -> GuardianPaths {
    guardian_paths(
        request.mode,
        request.session.pid,
        request.session.process_start,
    )
}

#[cfg(windows)]
pub fn write_marker(path: &Path, message: &str) -> Result<()> {
    validate_value(message)?;
    write_at(path, format!("{message}\n").as_bytes())
}

#[cfg(windows)]
pub fn remove_guardian_artifacts(paths: &GuardianPaths) {
    for path in [&paths.request, &paths.ready, &paths.error] {
        let _ = fs::remove_file(path);
    }
}

#[cfg(windows)]
pub fn request_state_file(request_path: &Path) -> Result<PathBuf> {
    request_path
        .parent()
        .map(|parent| parent.join("session.properties"))
        .ok_or_else(|| AppError::fail("guardian request has no parent directory"))
}

#[cfg(windows)]
pub fn write_state_at(path: &Path, session: &Session) -> Result<()> {
    write_at(path, serialize_session(session)?.as_bytes())
}

#[cfg(windows)]
pub fn delete_state_at(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(state_io_err(path, error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_state(even_lid: bool) -> String {
        let mut text = "version=2\npid=4321\nmode=display+system\ntrigger=timed\ndetail=1h\nstartedAt=2024-01-02T03:04:05+00:00\nendsAt=2024-01-02T04:04:05+00:00\nprocessStart=1700000000\nprocessCommand=".to_string();
        text.push_str(if cfg!(windows) {
            "C:\\tools\\wake.exe\n"
        } else if cfg!(target_os = "macos") {
            "/usr/bin/caffeinate\n"
        } else {
            "/usr/bin/systemd-inhibit\n"
        });
        text.push_str(&format!("evenLid={even_lid}\n"));
        #[cfg(not(windows))]
        if even_lid {
            text.push_str("priorDisableSleep=0\n");
        }
        #[cfg(windows)]
        if even_lid {
            text.push_str("guardianPid=99\nguardianStart=1800000000\noriginalScheme=381b4222-f694-41f0-9685-ff5bb260df2e\noriginalAc=4294967295\noriginalDc=2\n");
        }
        text
    }

    #[test]
    fn strict_state_parses_complete_records() {
        let session = parse_session_bytes(valid_state(false).as_bytes()).unwrap();
        assert_eq!(session.pid, 4321);
        assert_eq!(session.process_start, 1_700_000_000);
        assert!(!session.even_lid);

        let lid = parse_session_bytes(valid_state(true).as_bytes()).unwrap();
        assert!(lid.even_lid);
        #[cfg(windows)]
        assert_eq!(lid.original_ac, u32::MAX);
    }

    #[test]
    fn strict_state_rejects_version_unknown_missing_and_duplicate_fields() {
        for text in [
            valid_state(false).replacen("version=2", "version=1", 1),
            format!("{}unknown=value\n", valid_state(false)),
            valid_state(false).replace("detail=1h\n", ""),
            format!("{}pid=4321\n", valid_state(false)),
        ] {
            assert!(
                parse_session_bytes(text.as_bytes()).is_err(),
                "state={text:?}"
            );
        }
    }

    #[test]
    fn malformed_lid_hints_are_detected_without_parsing_values() {
        assert!(!malformed_from_bytes(b"broken=true\n").has_lid_recovery_hints());
        for bytes in [
            b"evenLid=true\n".as_slice(),
            b"priorDisableSleep=nope\n",
            b"originalScheme=nope\n",
        ] {
            assert!(malformed_from_bytes(bytes).has_lid_recovery_hints());
        }
    }

    #[test]
    fn identity_mismatch_is_rejected() {
        let session = parse_session_bytes(valid_state(false).as_bytes()).unwrap();
        let wrong_start = sysutil::Identity {
            start: session.process_start + 1,
            command: session.process_command.clone(),
        };
        let wrong_command = sysutil::Identity {
            start: session.process_start,
            command: "not-wake.exe".into(),
        };
        assert!(!session.matches_identity(&wrong_start));
        assert!(!session.matches_identity(&wrong_command));
    }

    #[cfg(windows)]
    #[test]
    fn strict_guardian_requests_round_trip_and_reject_unknown_fields() {
        let session = parse_session_bytes(valid_state(true).as_bytes()).unwrap();
        let recovery = GuardianRequest {
            mode: GuardianMode::Recover,
            session: session.clone(),
            expected_state: Some(valid_state(true).into_bytes()),
        };
        let encoded = serialize_guardian_request(&recovery).unwrap();
        let properties = parse_strict_properties(&encoded).unwrap();
        let parsed = parse_recovery_request(properties).unwrap();
        assert_eq!(parsed.session.pid, session.pid);

        let start = GuardianRequest {
            mode: GuardianMode::Start,
            session: Session {
                guardian_pid: 0,
                guardian_start: 0,
                original_scheme: String::new(),
                original_ac: 0,
                original_dc: 0,
                ..session
            },
            expected_state: None,
        };
        let encoded = serialize_guardian_request(&start).unwrap();
        assert!(parse_start_request(parse_strict_properties(&encoded).unwrap()).is_ok());
        assert!(
            parse_strict_properties(&(encoded + "extra=value\n"))
                .and_then(parse_start_request)
                .is_err()
        );
    }
}
