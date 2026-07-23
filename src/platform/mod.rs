//! Platform abstraction as free functions selected by `cfg`. Each platform module provides the
//! full surface; even-lid functions are real on macOS and unsupported stubs elsewhere.

#[cfg(not(windows))]
use crate::error::AppError;
use crate::error::Result;
use crate::sysutil;
#[cfg(not(windows))]
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

/// The platform command used by Unix supervisors to keep the machine awake.
#[cfg(not(windows))]
pub struct KeepAwake {
    pub cmd: Vec<String>,
    pub note: Option<String>,
}

pub fn find_app_pid(name: &str) -> Result<Option<u32>> {
    sysutil::find_app_pid(name)
}

/// Find an executable named `executable` on PATH and return its full path.
#[cfg(not(windows))]
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
