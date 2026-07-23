//! Platform abstraction as free functions selected by `cfg`. Each platform module provides the
//! full surface; even-lid functions are real on macOS and unsupported stubs elsewhere.

use crate::error::{AppError, Result};
use crate::sysutil;
use std::path::Path;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

/// The command to keep the machine awake plus an optional one-line note to show at start.
pub struct KeepAwake {
    pub cmd: Vec<String>,
    pub note: Option<String>,
}

pub fn find_app_pid(name: &str) -> Result<Option<u32>> {
    sysutil::find_app_pid(name)
}

/// Find an executable named `executable` on PATH and return its full path.
/// Used by Windows (powershell) and Linux (systemd-inhibit); macOS uses absolute tool paths.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn resolve_on_path(executable: &str, missing_message: &str) -> Result<String> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let candidate = dir.join(executable);
            if is_runnable_file(&candidate) {
                return Ok(candidate.to_string_lossy().into_owned());
            }
        }
    }
    Err(AppError::fail(missing_message.to_string()))
}

#[cfg(unix)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn is_runnable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file()
        && std::fs::metadata(p)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(windows)]
fn is_runnable_file(p: &Path) -> bool {
    p.is_file()
}
