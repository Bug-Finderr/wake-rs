//! Session state: the `session.properties` file, the advisory lock, and crash-recovery parsing.

use crate::error::{AppError, Result};
use crate::platform;
use crate::sysutil;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;

#[cfg(not(windows))]
pub const PHASE_ENABLING: &str = "enabling";
pub const PHASE_ACTIVE: &str = "active";

pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("WAKE_STATE_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    default_state_dir()
}

// NOTE: the Windows base moved from ~/.local/state to %LOCALAPPDATA%; any session written under the
// old location is orphaned, but recovery is best-effort and a stale child dies on its own.
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
    // Only the macOS malformed-recovery path inspects the parsed prior; Windows recovery restores a
    // safe default instead.
    #[cfg_attr(windows, allow(dead_code))]
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

// The `priorDisableSleep` field is reused on Windows to store the encoded prior lid action
// (`ac | (dc << 4)`, each nibble 0..=3), so accept that range there; elsewhere it is macOS's
// SleepDisabled which is strictly 0 or 1.
#[cfg(not(windows))]
fn parse_disable_sleep(raw: &str) -> Option<i32> {
    match raw.trim().parse::<i32>().ok()? {
        v @ (0 | 1) => Some(v),
        _ => None,
    }
}

#[cfg(windows)]
fn parse_disable_sleep(raw: &str) -> Option<i32> {
    let v = raw.trim().parse::<i32>().ok()?;
    let (ac, dc) = (v & 0xF, (v >> 4) & 0xF);
    if v == (ac | (dc << 4)) && (0..=3).contains(&ac) && (0..=3).contains(&dc) {
        Some(v)
    } else {
        None
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

/// Wrap a state-file IO error with the offending path and the `WAKE_STATE_DIR` escape hatch, so
/// permission/quota failures point at the directory instead of surfacing a bare OS error.
fn state_io_err(path: &std::path::Path, e: std::io::Error) -> AppError {
    AppError::fail(format!(
        "state IO failed at {}: {e}; set WAKE_STATE_DIR to a writable directory",
        path.display()
    ))
}

pub fn write(s: &Session) -> Result<()> {
    let dir = state_dir();
    fs::create_dir_all(&dir).map_err(|e| state_io_err(&dir, e))?;
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
    fs::write(&tmp, out).map_err(|e| state_io_err(&tmp, e))?;
    if let Err(e) = fs::rename(&tmp, state_file()) {
        let _ = fs::remove_file(&tmp);
        return Err(state_io_err(&state_file(), e));
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

    #[test]
    fn properties_round_trip_builds_session() {
        let text = "pid=4321\n\
             mode=display+system\n\
             trigger=timed\n\
             detail=1h\n\
             startedAt=2024-01-02T03:04:05+00:00\n\
             endsAt=2024-01-02T04:04:05+00:00\n\
             processStartMs=1700000000\n\
             processCommand=/usr/bin/caffeinate\n\
             processCommandLine=/usr/bin/caffeinate -d\n\
             evenLid=false\n\
             priorDisableSleep=0\n\
             phase=active\n";
        let s = build_session(&parse_properties(text)).expect("valid session");
        assert_eq!(s.pid, 4321);
        assert_eq!(s.mode, "display+system");
        assert_eq!(s.trigger, "timed");
        assert_eq!(s.detail, "1h");
        assert_eq!(s.process_start, 1_700_000_000);
        assert_eq!(s.process_command, "/usr/bin/caffeinate");
        assert!(s.started_at.is_some());
        assert!(s.ends_at.is_some());
        assert!(!s.even_lid);
        assert_eq!(s.phase, PHASE_ACTIVE);
    }

    #[test]
    fn missing_started_at_is_none() {
        let text = "pid=1\nprocessStartMs=10\n";
        assert!(build_session(&parse_properties(text)).is_none());
    }

    #[test]
    fn parse_properties_skips_comments_and_blanks() {
        let text = "# a comment\n! also a comment\n\n  \nkey=value\n  spaced = trimmed-key\n";
        let p = parse_properties(text);
        assert_eq!(p.get("key").map(String::as_str), Some("value"));
        assert_eq!(p.get("spaced").map(String::as_str), Some(" trimmed-key"));
        assert!(!p.contains_key("# a comment"));
        assert_eq!(p.len(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn parse_disable_sleep_packed_nibbles() {
        // 0x21 = 33 -> ac=1, dc=2 (both in 0..=3): accepted.
        assert_eq!(parse_disable_sleep("33"), Some(33));
        assert_eq!(platform::decode_lid(33), (1, 2));
        // 0x44 = 68 -> ac=4, dc=4 (out of 0..=3): rejected.
        assert_eq!(parse_disable_sleep("68"), None);
    }
}
