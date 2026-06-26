//! Session state: the `session.properties` file, the advisory lock, and crash-recovery parsing.

use crate::error::{AppError, Result};
use crate::platform;
use crate::sysutil;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;

pub const PHASE_ENABLING: &str = "enabling";
pub const PHASE_ACTIVE: &str = "active";

pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("WAKE_STATE_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    home().join(".local").join("state").join("wake")
}

pub fn state_file() -> PathBuf {
    state_dir().join("session.properties")
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

#[derive(Default)]
pub struct Session {
    pub pid: u32,
    pub mode: String,
    pub trigger: String,
    pub detail: String,
    pub started_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub process_start: u64,
    pub process_command: String,
    pub process_command_line: String,
    pub even_lid: bool,
    pub prior_disable_sleep: i32,
    pub phase: String,
}

impl Session {
    pub fn new() -> Self {
        Session {
            phase: PHASE_ACTIVE.to_string(),
            ..Default::default()
        }
    }

    /// Fill identity fields from the live process at `self.pid`.
    pub fn capture_process_identity(&mut self) -> Result<()> {
        let id = sysutil::capture_identity(self.pid)?;
        self.process_start = id.start;
        self.process_command = id.command;
        self.process_command_line = id.command_line;
        Ok(())
    }

    /// True if the recorded pid is still the same live process we started.
    pub fn matches_live_process(&self) -> bool {
        match sysutil::live_identity(self.pid) {
            None => false,
            Some(live) => {
                self.process_start == live.start
                    && self.process_command == live.command
                    && is_expected_command(&live.command, &live.command_line)
            }
        }
    }
}

fn is_expected_command(command: &str, command_line: &str) -> bool {
    let base = std::path::Path::new(command)
        .file_name()
        .map(|f| f.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let line = command_line.to_lowercase();
    platform::expected_command_basenames().contains(&base.as_str()) || line.contains("wake")
}

// ---- read ----

pub enum SavedState {
    Valid(Session),
    Malformed(MalformedState),
}

pub struct MalformedState {
    pub even_lid_true: bool,
    pub has_prior_disable_sleep: bool,
    pub parsed_prior_disable_sleep: Option<i32>,
}

impl MalformedState {
    pub fn has_lid_recovery_hints(&self) -> bool {
        self.even_lid_true || self.has_prior_disable_sleep
    }
}

pub fn read_saved_for_recovery() -> Option<SavedState> {
    let path = state_file();
    if !path.exists() {
        return None;
    }
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Some(SavedState::Malformed(malformed_from(&HashMap::new()))),
    };
    let props = parse_properties(&text);
    Some(match build_session(&props) {
        Some(s) => SavedState::Valid(s),
        None => SavedState::Malformed(malformed_from(&props)),
    })
}

/// Valid live-or-not session, deleting a stale/dead file. `may_delete_malformed` removes a malformed
/// file only when it carries no lid-recovery hints.
pub fn read_if_alive(may_delete_malformed: bool) -> Option<Session> {
    match read_saved_for_recovery() {
        Some(SavedState::Valid(s)) => {
            if s.matches_live_process() {
                Some(s)
            } else {
                let _ = fs::remove_file(state_file());
                None
            }
        }
        Some(SavedState::Malformed(m)) => {
            if may_delete_malformed && !m.has_lid_recovery_hints() {
                let _ = fs::remove_file(state_file());
            }
            None
        }
        None => None,
    }
}

fn parse_properties(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.to_string());
        }
    }
    map
}

fn build_session(p: &HashMap<String, String>) -> Option<Session> {
    let pid = p.get("pid").map_or(Some(0), |v| v.trim().parse().ok())?;
    let started_at = parse_ts(p.get("startedAt")?)?;
    let ends_at = match p.get("endsAt").map(String::as_str) {
        None | Some("") => None,
        Some(s) => Some(parse_ts(s)?),
    };
    let process_start = p.get("processStartMs")?.trim().parse().ok()?;
    let prior_disable_sleep = parse_disable_sleep(p.get("priorDisableSleep").map_or("0", |v| v))?;

    Some(Session {
        pid,
        mode: p.get("mode").cloned().unwrap_or_default(),
        trigger: p.get("trigger").cloned().unwrap_or_default(),
        detail: p.get("detail").cloned().unwrap_or_default(),
        started_at: Some(started_at),
        ends_at,
        process_start,
        process_command: p.get("processCommand").cloned().unwrap_or_default(),
        process_command_line: p.get("processCommandLine").cloned().unwrap_or_default(),
        even_lid: p
            .get("evenLid")
            .map(|v| v.trim() == "true")
            .unwrap_or(false),
        prior_disable_sleep,
        phase: p
            .get("phase")
            .cloned()
            .unwrap_or_else(|| PHASE_ACTIVE.to_string()),
    })
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn parse_disable_sleep(raw: &str) -> Option<i32> {
    match raw.trim().parse::<i32>().ok()? {
        v @ (0 | 1) => Some(v),
        _ => None,
    }
}

fn malformed_from(p: &HashMap<String, String>) -> MalformedState {
    MalformedState {
        even_lid_true: p
            .get("evenLid")
            .map(|v| v.trim() == "true")
            .unwrap_or(false),
        has_prior_disable_sleep: p.contains_key("priorDisableSleep"),
        parsed_prior_disable_sleep: p
            .get("priorDisableSleep")
            .and_then(|v| parse_disable_sleep(v)),
    }
}

// ---- write ----

pub fn write(s: &Session) -> Result<()> {
    let dir = state_dir();
    fs::create_dir_all(&dir)?;
    let mut out = String::new();
    let push = |out: &mut String, k: &str, v: &str| {
        out.push_str(k);
        out.push('=');
        out.push_str(v);
        out.push('\n');
    };
    push(&mut out, "pid", &s.pid.to_string());
    push(&mut out, "mode", &s.mode);
    push(&mut out, "trigger", &s.trigger);
    push(&mut out, "detail", &s.detail);
    push(&mut out, "startedAt", &ts(s.started_at));
    push(
        &mut out,
        "endsAt",
        &s.ends_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
    );
    push(&mut out, "processStartMs", &s.process_start.to_string());
    push(&mut out, "processCommand", &s.process_command);
    push(&mut out, "processCommandLine", &s.process_command_line);
    push(&mut out, "evenLid", &s.even_lid.to_string());
    push(
        &mut out,
        "priorDisableSleep",
        &s.prior_disable_sleep.to_string(),
    );
    push(
        &mut out,
        "phase",
        if s.phase.is_empty() {
            PHASE_ACTIVE
        } else {
            &s.phase
        },
    );

    let tmp = dir.join("session.properties.tmp");
    fs::write(&tmp, out)?;
    if let Err(e) = fs::rename(&tmp, state_file()) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

fn ts(t: Option<DateTime<Utc>>) -> String {
    t.map(|t| t.to_rfc3339()).unwrap_or_default()
}

pub fn delete_state_file() {
    let _ = fs::remove_file(state_file());
}

// ---- lock ----

pub struct LockGuard {
    file: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub fn acquire_lock() -> Result<LockGuard> {
    fs::create_dir_all(state_dir())?;
    let path = state_dir().join("wake.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(LockGuard { file }),
        Err(std::fs::TryLockError::WouldBlock) => Err(AppError::usage(
            "another wake invocation is in progress; try again",
        )),
        Err(std::fs::TryLockError::Error(e)) => Err(AppError::fail(e.to_string())),
    }
}
