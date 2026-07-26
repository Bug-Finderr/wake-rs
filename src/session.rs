use crate::error::{AppError, Result};
use crate::{platform, sysutil};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const STATE_VERSION: &str = "2";

pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("WAKE_STATE_DIR")
        && !dir.is_empty()
    {
        return absolute_path(PathBuf::from(dir));
    }
    absolute_path(default_state_dir())
}

fn absolute_path(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

#[cfg(windows)]
fn default_state_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("AppData").join("Local"))
        .join("wake")
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

pub fn state_file() -> PathBuf {
    state_dir().join("session.properties")
}

#[cfg(any(not(windows), test))]
pub fn mode_for(no_display: bool) -> &'static str {
    if no_display {
        "system-only"
    } else {
        "display+system"
    }
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
    #[cfg(target_os = "macos")]
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
        self.process_start == live.start && command_matches && expected_command(&live.command)
    }

    #[cfg(not(windows))]
    pub fn matches_live_process(&self) -> bool {
        sysutil::live_identity(self.pid).is_some_and(|live| self.matches_identity(&live))
    }

    #[cfg(windows)]
    pub fn matches_lid_authority(
        &self,
        worker: (u32, u64),
        guardian: (u32, u64),
        snapshot: (&str, u32, u32),
    ) -> bool {
        self.even_lid
            && (self.pid, self.process_start) == worker
            && (self.guardian_pid, self.guardian_start) == guardian
            && (
                self.original_scheme.as_str(),
                self.original_ac,
                self.original_dc,
            ) == snapshot
    }

    #[cfg(windows)]
    pub fn owned_non_lid_by(&self, pid: u32, start: u64) -> bool {
        !self.even_lid && (self.pid, self.process_start) == (pid, start)
    }
}

fn expected_command(command: &str) -> bool {
    let base = Path::new(command)
        .file_name()
        .map(|file| file.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    platform::expected_command_basenames().contains(&base.as_str())
}

pub enum SavedState {
    Valid(Session),
    Malformed(bool),
}

pub fn read_saved_for_recovery() -> Option<SavedState> {
    read_saved_at(&state_file())
}

pub fn read_saved_at(path: &Path) -> Option<SavedState> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some(SavedState::Malformed(true)),
    };
    Some(match parse_session(&bytes) {
        Ok(session) => SavedState::Valid(session),
        Err(_) => SavedState::Malformed(lid_hints(&bytes)),
    })
}

#[cfg(not(windows))]
pub fn read_if_alive(delete_unhinted_malformed: bool) -> Option<Session> {
    match read_saved_for_recovery() {
        Some(SavedState::Valid(session)) if session.matches_live_process() => Some(session),
        Some(SavedState::Valid(_)) => {
            let _ = delete_state_file();
            None
        }
        Some(SavedState::Malformed(lid_hints)) => {
            if delete_unhinted_malformed && !retain_malformed_state(lid_hints) {
                let _ = delete_state_file();
            }
            None
        }
        None => None,
    }
}

fn parse_session(bytes: &[u8]) -> Result<Session> {
    let text = std::str::from_utf8(bytes).map_err(|_| AppError::fail("state is not UTF-8"))?;
    let properties = properties(text)?;
    if field(&properties, "version")? != STATE_VERSION {
        return Err(AppError::fail("unsupported state version"));
    }
    let even_lid = parse_bool(field(&properties, "evenLid")?, "evenLid")?;
    #[cfg(target_os = "linux")]
    let platform_keys: &[&str] = &[];
    #[cfg(target_os = "macos")]
    let platform_keys: &[&str] = if even_lid {
        &["priorDisableSleep"]
    } else {
        &[]
    };
    #[cfg(windows)]
    let platform_keys: &[&str] = if even_lid {
        &[
            "guardianPid",
            "guardianStart",
            "originalScheme",
            "originalAc",
            "originalDc",
        ]
    } else {
        &[]
    };
    let keys: HashSet<&str> = [
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
    .chain(platform_keys.iter().copied())
    .collect();
    if properties.len() != keys.len() || properties.keys().any(|key| !keys.contains(key.as_str())) {
        return Err(AppError::fail("state has missing or unknown fields"));
    }

    let session = Session {
        pid: parse_positive(field(&properties, "pid")?, "pid")?,
        mode: field(&properties, "mode")?.into(),
        trigger: field(&properties, "trigger")?.into(),
        detail: field(&properties, "detail")?.into(),
        started_at: Some(parse_utc(field(&properties, "startedAt")?, "startedAt")?),
        ends_at: match field(&properties, "endsAt")? {
            "" => None,
            value => Some(parse_utc(value, "endsAt")?),
        },
        process_start: parse_positive(field(&properties, "processStart")?, "processStart")?,
        process_command: field(&properties, "processCommand")?.into(),
        even_lid,
        #[cfg(target_os = "macos")]
        prior_disable_sleep: if even_lid {
            disable_sleep(field(&properties, "priorDisableSleep")?)?
        } else {
            0
        },
        #[cfg(windows)]
        guardian_pid: if even_lid {
            parse_positive(field(&properties, "guardianPid")?, "guardianPid")?
        } else {
            0
        },
        #[cfg(windows)]
        guardian_start: if even_lid {
            parse_positive(field(&properties, "guardianStart")?, "guardianStart")?
        } else {
            0
        },
        #[cfg(windows)]
        original_scheme: if even_lid {
            let value = field(&properties, "originalScheme")?;
            platform::parse_guid(value)?;
            value.into()
        } else {
            String::new()
        },
        #[cfg(windows)]
        original_ac: if even_lid {
            parse_u32(field(&properties, "originalAc")?, "originalAc")?
        } else {
            0
        },
        #[cfg(windows)]
        original_dc: if even_lid {
            parse_u32(field(&properties, "originalDc")?, "originalDc")?
        } else {
            0
        },
    };
    validate(&session)?;
    Ok(session)
}

fn properties(text: &str) -> Result<HashMap<String, String>> {
    let mut output = HashMap::new();
    for line in text.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| AppError::fail("state contains a line without '='"))?;
        if key.is_empty() || key.trim() != key || output.insert(key.into(), value.into()).is_some()
        {
            return Err(AppError::fail("state contains an invalid or duplicate key"));
        }
    }
    if output.is_empty() {
        Err(AppError::fail("state is empty"))
    } else {
        Ok(output)
    }
}

fn validate(session: &Session) -> Result<()> {
    for value in [
        &session.mode,
        &session.trigger,
        &session.detail,
        &session.process_command,
    ] {
        if value.is_empty() || value.contains(['\r', '\n']) {
            return Err(AppError::fail("state contains an invalid required value"));
        }
    }
    if !matches!(session.mode.as_str(), "system-only" | "display+system")
        || !matches!(
            session.trigger.as_str(),
            "indefinite" | "timed" | "until-time" | "until-charge" | "while-pid" | "while-app"
        )
        || !expected_command(&session.process_command)
    {
        return Err(AppError::fail("state contains invalid session metadata"));
    }
    let timed = matches!(session.trigger.as_str(), "timed" | "until-time");
    if session.ends_at.is_some() != timed
        || session
            .ends_at
            .zip(session.started_at)
            .is_some_and(|(end, start)| end <= start)
    {
        return Err(AppError::fail("state contains inconsistent timestamps"));
    }
    Ok(())
}

fn serialize(session: &Session) -> Result<Vec<u8>> {
    validate(session)?;
    if session.pid == 0 || session.process_start == 0 || session.started_at.is_none() {
        return Err(AppError::fail("session identity or start time is missing"));
    }
    let fields = vec![
        ("version", STATE_VERSION.to_string()),
        ("pid", session.pid.to_string()),
        ("mode", session.mode.clone()),
        ("trigger", session.trigger.clone()),
        ("detail", session.detail.clone()),
        ("startedAt", session.started_at.unwrap().to_rfc3339()),
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
    #[cfg(target_os = "macos")]
    let fields = {
        let mut fields = fields;
        if session.even_lid {
            disable_sleep(&session.prior_disable_sleep.to_string())?;
            fields.push(("priorDisableSleep", session.prior_disable_sleep.to_string()));
        }
        fields
    };
    #[cfg(windows)]
    let fields = {
        let mut fields = fields;
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
        fields
    };
    Ok(fields
        .into_iter()
        .flat_map(|(key, value)| format!("{key}={value}\n").into_bytes())
        .collect())
}

fn field<'a>(properties: &'a HashMap<String, String>, key: &str) -> Result<&'a str> {
    properties
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| AppError::fail(format!("state is missing {key}")))
}

pub(crate) fn parse_bool(raw: &str, name: &str) -> Result<bool> {
    match raw {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(AppError::fail(format!("invalid {name}"))),
    }
}
pub(crate) fn parse_positive<T>(raw: &str, name: &str) -> Result<T>
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
#[cfg(any(target_os = "macos", windows))]
pub(crate) fn parse_u32(raw: &str, name: &str) -> Result<u32> {
    let value = raw
        .parse::<u32>()
        .map_err(|_| AppError::fail(format!("invalid {name}")))?;
    if value.to_string() == raw {
        Ok(value)
    } else {
        Err(AppError::fail(format!("invalid {name}")))
    }
}
pub(crate) fn parse_utc(raw: &str, name: &str) -> Result<DateTime<Utc>> {
    let value = DateTime::parse_from_rfc3339(raw)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|_| AppError::fail(format!("invalid {name}")))?;
    if value.to_rfc3339() == raw {
        Ok(value)
    } else {
        Err(AppError::fail(format!("{name} is not canonical UTC")))
    }
}
#[cfg(target_os = "macos")]
fn disable_sleep(raw: &str) -> Result<i32> {
    match parse_u32(raw, "priorDisableSleep")? {
        value @ (0 | 1) => Ok(value as i32),
        _ => Err(AppError::fail("priorDisableSleep must be 0 or 1")),
    }
}
#[cfg(not(windows))]
pub(crate) fn retain_malformed_state(lid_hints: bool) -> bool {
    cfg!(target_os = "macos") || lid_hints
}

#[cfg(any(target_os = "macos", test))]
fn pending_lid_recovery_bytes(prior_disable_sleep: i32) -> Vec<u8> {
    format!(
        "version={STATE_VERSION}\nevenLid=true\npriorDisableSleep={prior_disable_sleep}\nstartupPending=true\n"
    )
    .into_bytes()
}

fn lid_hints(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let persistent_recovery = [
        "priorDisableSleep=",
        "guardianPid=",
        "guardianStart=",
        "originalScheme=",
        "originalAc=",
        "originalDc=",
    ]
    .iter()
    .any(|hint| text.contains(hint));
    persistent_recovery || text.contains("evenLid=true")
}

fn io_error(path: &Path, error: std::io::Error) -> AppError {
    AppError::fail(format!(
        "state IO failed at {}: {error}; set WAKE_STATE_DIR to a writable directory",
        path.display()
    ))
}

#[cfg(not(windows))]
fn atomic_rename(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn atomic_rename(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: Both NUL-terminated path buffers remain valid for the call.
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn write(session: &Session) -> Result<()> {
    write_bytes_at(&state_file(), &serialize(session)?)
}

#[cfg(windows)]
pub fn write_at(path: &Path, session: &Session) -> Result<()> {
    write_bytes_at(path, &serialize(session)?)
}

#[cfg(target_os = "macos")]
pub fn write_pending_lid_recovery(prior_disable_sleep: i32) -> Result<()> {
    if !matches!(prior_disable_sleep, 0 | 1) {
        return Err(AppError::fail("priorDisableSleep must be 0 or 1"));
    }
    write_bytes_at(
        &state_file(),
        &pending_lid_recovery_bytes(prior_disable_sleep),
    )
}

fn write_bytes_at(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| AppError::fail("state path has no parent directory"))?;
    fs::create_dir_all(dir).map_err(|error| io_error(dir, error))?;
    let tmp = dir.join(format!("session.properties.tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .map_err(|error| io_error(&tmp, error))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| io_error(&tmp, error))?;
        atomic_rename(&tmp, path).map_err(|error| io_error(path, error))
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

pub fn delete_state_file() -> Result<()> {
    delete_path(&state_file())
}

fn delete_path(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(path, error)),
    }
}

pub struct LockGuard {
    file: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn lock_error(error: std::fs::TryLockError) -> AppError {
    match error {
        std::fs::TryLockError::WouldBlock => {
            AppError::fail("another wake invocation is in progress; try again")
        }
        std::fs::TryLockError::Error(error) => AppError::fail(error.to_string()),
    }
}

pub fn acquire_lock() -> Result<LockGuard> {
    let dir = state_dir();
    fs::create_dir_all(&dir).map_err(|error| io_error(&dir, error))?;
    let path = dir.join("wake.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|error| io_error(&path, error))?;
    file.try_lock()
        .map(|()| LockGuard { file })
        .map_err(lock_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_is_derived_from_no_display() {
        assert_eq!(mode_for(false), "display+system");
        assert_eq!(mode_for(true), "system-only");
    }

    fn valid(even_lid: bool) -> String {
        let command = if cfg!(windows) {
            "C:\\tools\\wake.exe"
        } else if cfg!(target_os = "macos") {
            "/usr/bin/caffeinate"
        } else {
            "/usr/bin/systemd-inhibit"
        };
        let platform_fields = if !even_lid {
            ""
        } else if cfg!(target_os = "macos") {
            "priorDisableSleep=0\n"
        } else if cfg!(windows) {
            "guardianPid=99\nguardianStart=1800000000\noriginalScheme=381b4222-f694-41f0-9685-ff5bb260df2e\noriginalAc=4294967295\noriginalDc=2\n"
        } else {
            ""
        };
        format!(
            "version=2\npid=4321\nmode=display+system\ntrigger=timed\ndetail=1h\nstartedAt=2024-01-02T03:04:05+00:00\nendsAt=2024-01-02T04:04:05+00:00\nprocessStart=1700000000\nprocessCommand={command}\nevenLid={even_lid}\n{platform_fields}"
        )
    }
    #[test]
    fn pending_lid_recovery_is_retained_without_becoming_write_authority() {
        let bytes = pending_lid_recovery_bytes(0);
        assert!(parse_session(&bytes).is_err());
        assert!(lid_hints(&bytes));
    }

    #[test]
    fn strict_state_accepts_only_complete_versioned_records() {
        assert_eq!(parse_session(valid(false).as_bytes()).unwrap().pid, 4321);
        assert!(parse_session(valid(true).as_bytes()).unwrap().even_lid);
        for invalid in [
            valid(false).replacen("version=2", "version=1", 1),
            format!("{}unknown=x\n", valid(false)),
            valid(false).replace("detail=1h\n", ""),
            format!("{}pid=4321\n", valid(false)),
        ] {
            assert!(parse_session(invalid.as_bytes()).is_err());
        }
        let session = parse_session(valid(false).as_bytes()).unwrap();
        assert!(!session.matches_identity(&sysutil::Identity {
            start: session.process_start + 1,
            command: session.process_command.clone(),
        }));
        assert!(!lid_hints(b"broken=true\n"));
        assert!(lid_hints(b"originalScheme=broken\n"));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_even_lid_round_trips_without_restoration_state() {
        let text = valid(true);
        let session = parse_session(text.as_bytes()).unwrap();
        assert!(session.even_lid);
        assert_eq!(serialize(&session).unwrap(), text.as_bytes());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_even_lid_requires_and_round_trips_prior_sleep_state() {
        let text = valid(true);
        let session = parse_session(text.as_bytes()).unwrap();
        assert_eq!(session.prior_disable_sleep, 0);
        assert_eq!(serialize(&session).unwrap(), text.as_bytes());
        assert!(parse_session(text.replace("priorDisableSleep=0\n", "").as_bytes()).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_even_lid_round_trips_guardian_and_power_plan_state() {
        let text = valid(true);
        let session = parse_session(text.as_bytes()).unwrap();
        assert_eq!(session.guardian_pid, 99);
        assert_eq!(session.guardian_start, 1_800_000_000);
        assert_eq!(
            session.original_scheme,
            "381b4222-f694-41f0-9685-ff5bb260df2e"
        );
        assert_eq!((session.original_ac, session.original_dc), (u32::MAX, 2));
        assert_eq!(serialize(&session).unwrap(), text.as_bytes());
    }

    #[test]
    fn relative_paths_are_absolute_and_delete_failures_are_preserved() {
        assert!(absolute_path(PathBuf::from("relative state")).is_absolute());
        let path = std::env::temp_dir().join(format!("wake-delete-test-{}", std::process::id()));
        fs::create_dir(&path).unwrap();
        assert!(delete_path(&path).is_err());
        assert!(path.is_dir());
        fs::remove_dir(path).unwrap();
    }

    #[cfg(not(windows))]
    #[test]
    fn malformed_retention_preserves_even_lid_management_breadcrumbs() {
        assert!(retain_malformed_state(lid_hints(b"priorDisableSleep=0\n")));
        assert!(retain_malformed_state(lid_hints(b"guardianStart=broken\n")));
        assert!(retain_malformed_state(lid_hints(
            b"originalScheme=broken\n"
        )));
        assert!(retain_malformed_state(lid_hints(b"evenLid=true\n")));
        assert_eq!(retain_malformed_state(false), cfg!(target_os = "macos"));
    }

    #[test]
    fn lock_contention_is_a_runtime_failure() {
        let error = lock_error(std::fs::TryLockError::WouldBlock);
        assert!(matches!(error, AppError::Fail(_)));
        assert_eq!(error.exit_code(), 1);
    }
}
