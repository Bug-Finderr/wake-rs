use crate::error::{AppError, Result};
use crate::run::{BatteryStatus, Mode};
use std::process::{Child, Command, Stdio};

const CAFFEINATE: &str = "/usr/bin/caffeinate";
const PMSET: &str = "/usr/bin/pmset";
const SUDO: &str = "/usr/bin/sudo";

pub fn trusted_helper_executable() -> Result<String> {
    use std::os::unix::fs::MetadataExt;

    let executable = std::fs::canonicalize(std::env::current_exe()?)?;
    if !executable.is_file() {
        return Err(AppError::fail("wake executable is not a regular file"));
    }
    let ancestors = executable.ancestors().collect::<Vec<_>>();
    for path in ancestors.into_iter().rev() {
        let metadata = std::fs::metadata(path)?;
        if !trusted_permissions(metadata.uid(), metadata.mode())
            || has_extended_acl(path)?
            || caller_can_write(path)?
        {
            return Err(AppError::fail(format!(
                "--even-lid requires a root-owned protected install with no extended ACLs; {} is not protected from non-root changes",
                path.display()
            )));
        }
    }
    executable
        .into_os_string()
        .into_string()
        .map_err(|_| AppError::fail("wake executable path is not valid UTF-8"))
}

fn trusted_permissions(uid: u32, mode: u32) -> bool {
    uid == 0 && mode & 0o022 == 0
}

fn ffi_path(path: &std::path::Path) -> Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| AppError::fail(format!("path contains a null byte: {}", path.display())))
}

unsafe extern "C" {
    fn acl_get_file(path: *const libc::c_char, acl_type: libc::c_int) -> *mut libc::c_void;
    fn acl_free(object: *mut libc::c_void) -> libc::c_int;
}

pub(crate) fn has_extended_acl(path: &std::path::Path) -> Result<bool> {
    const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;
    let path_c = ffi_path(path)?;
    // SAFETY: __error returns this thread's errno slot and path_c is a live null-terminated path.
    let acl = unsafe {
        *libc::__error() = 0;
        acl_get_file(path_c.as_ptr(), ACL_TYPE_EXTENDED)
    };
    if !acl.is_null() {
        // SAFETY: acl was returned by acl_get_file and is released exactly once.
        if unsafe { acl_free(acl) } != 0 {
            return Err(AppError::from(std::io::Error::last_os_error()));
        }
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(false)
    } else {
        Err(AppError::fail(format!(
            "cannot inspect access control list at {}: {error}",
            path.display()
        )))
    }
}

fn caller_is_root() -> bool {
    // SAFETY: getuid has no preconditions and returns the real user ID.
    unsafe { libc::getuid() == 0 }
}

fn caller_can_write(path: &std::path::Path) -> Result<bool> {
    if caller_is_root() {
        return Ok(false);
    }
    let path_c = ffi_path(path)?;
    // SAFETY: path_c is a live null-terminated path and access does not retain its pointer.
    if unsafe { libc::access(path_c.as_ptr(), libc::W_OK) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EACCES | libc::EPERM | libc::EROFS | libc::ETXTBSY) => Ok(false),
        _ => Err(AppError::fail(format!(
            "cannot verify write access at {}: {error}",
            path.display()
        ))),
    }
}

pub struct Inhibitor {
    child: Child,
}

impl Inhibitor {
    pub fn start(mode: Mode, _even_lid: bool) -> Result<Self> {
        let mut command = Command::new(CAFFEINATE);
        command.arg("-i");
        if mode == Mode::DisplaySystem {
            command.arg("-d");
        }
        let child = command
            .args(["-w", &std::process::id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| AppError::fail(format!("could not start caffeinate: {error}")))?;
        Ok(Self { child })
    }

    pub fn note(&self) -> Option<&str> {
        Some("note: closing the lid still sleeps the Mac unless you use --even-lid")
    }

    pub fn alive(&mut self) -> bool {
        self.child.try_wait().is_ok_and(|status| status.is_none())
    }
}

pub fn inhibitor_startup_error(_even_lid: bool) -> AppError {
    AppError::inhibitor_startup("sleep inhibitor exited during startup")
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LidSnapshot {
    pub sleep_disabled: i32,
}

pub fn read_lid_snapshot() -> Result<LidSnapshot> {
    Ok(LidSnapshot {
        sleep_disabled: read_disable_sleep()?,
    })
}

pub fn authenticate_privilege() -> Result<()> {
    if Command::new(SUDO).arg("-v").status()?.success() {
        Ok(())
    } else {
        Err(AppError::fail(
            "sudo authentication failed; --even-lid was not enabled",
        ))
    }
}

pub fn disable_lid(snapshot: &LidSnapshot) -> Result<()> {
    if !lid_snapshot_matches(snapshot)? {
        return Err(AppError::fail(
            "power configuration changed before the lid override",
        ));
    }
    write_disable_sleep(1)
}

pub fn restore_lid(snapshot: &LidSnapshot) -> Result<()> {
    let current = read_disable_sleep()?;
    if let Some(value) = restore_value(snapshot.sleep_disabled, current) {
        write_disable_sleep(value)?;
    }
    if lid_is_restored(snapshot)? {
        Ok(())
    } else {
        Err(AppError::fail(
            "failed to remove the wake-owned lid override",
        ))
    }
}

pub fn restore_lid_elevated(snapshot: &LidSnapshot) -> Result<()> {
    let Some(value) = restore_value(snapshot.sleep_disabled, read_disable_sleep()?) else {
        return Ok(());
    };
    if !write_disable_sleep_with_sudo(value)? {
        authenticate_privilege()?;
        if !write_disable_sleep_with_sudo(value)? {
            return Err(AppError::fail("elevated lid restoration failed"));
        }
    }
    if lid_is_restored(snapshot)? {
        Ok(())
    } else {
        Err(AppError::fail(
            "failed to remove the wake-owned lid override",
        ))
    }
}

pub fn lid_override_is_active(_snapshot: &LidSnapshot) -> Result<bool> {
    Ok(read_disable_sleep()? == 1)
}

pub fn lid_is_restored(snapshot: &LidSnapshot) -> Result<bool> {
    Ok(restore_value(snapshot.sleep_disabled, read_disable_sleep()?).is_none())
}

pub fn lid_snapshot_matches(snapshot: &LidSnapshot) -> Result<bool> {
    Ok(read_disable_sleep()? == snapshot.sleep_disabled)
}

fn restore_value(original: i32, current: i32) -> Option<i32> {
    (original != 1 && current == 1).then_some(original)
}

pub fn read_battery() -> Result<BatteryStatus> {
    battery_from_pmset(&capture(PMSET, &["-g", "batt"])?)
}

fn battery_from_pmset(output: &str) -> Result<BatteryStatus> {
    let percent = first_percent(output)
        .ok_or_else(|| AppError::fail("cannot parse battery percentage from pmset"))?;
    let state = output
        .lines()
        .find(|line| line.contains('%'))
        .and_then(|line| line.split(';').nth(1))
        .map(|state| state.trim().to_lowercase())
        .ok_or_else(|| AppError::fail("cannot parse battery state from pmset"))?;
    let charging = matches!(state.as_str(), "charging" | "finishing charge");
    let discharging = state == "discharging";
    Ok(BatteryStatus {
        percent,
        charging,
        discharging,
        neutral_state: (!charging && !discharging).then_some(state),
    })
}

fn read_disable_sleep() -> Result<i32> {
    disable_sleep_from_pmset(&capture(PMSET, &["-g"])?)
}

fn disable_sleep_from_pmset(output: &str) -> Result<i32> {
    for line in output.lines() {
        let mut parts = line.split_whitespace();
        if parts
            .next()
            .is_some_and(|key| key.eq_ignore_ascii_case("SleepDisabled"))
        {
            return match (parts.next(), parts.next()) {
                (Some("0"), None) => Ok(0),
                (Some("1"), None) => Ok(1),
                _ => Err(AppError::fail(
                    "cannot parse SleepDisabled value from pmset",
                )),
            };
        }
    }
    Ok(0)
}

fn write_disable_sleep(value: i32) -> Result<()> {
    if !matches!(value, 0 | 1) {
        return Err(AppError::fail("SleepDisabled must be 0 or 1"));
    }
    let status = Command::new(PMSET)
        .args(["-a", "disablesleep", &value.to_string()])
        .status()?;
    if !status.success() {
        return Err(AppError::fail(format!(
            "pmset disablesleep exited with status {}",
            status.code().unwrap_or(-1)
        )));
    }
    if read_disable_sleep()? == value {
        Ok(())
    } else {
        Err(AppError::fail(format!(
            "failed to set SleepDisabled to {value}"
        )))
    }
}

fn write_disable_sleep_with_sudo(value: i32) -> Result<bool> {
    if !matches!(value, 0 | 1) {
        return Err(AppError::fail("SleepDisabled must be 0 or 1"));
    }
    Command::new(SUDO)
        .args(["-n", PMSET, "-a", "disablesleep", &value.to_string()])
        .status()
        .map(|status| status.success())
        .map_err(AppError::from)
}

fn first_percent(output: &str) -> Option<i32> {
    let bytes = output.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_digit() {
            let start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            if bytes.get(index) == Some(&b'%') {
                return output[start..index].parse().ok();
            }
        } else {
            index += 1;
        }
    }
    None
}

fn capture(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(AppError::fail(format!(
            "{program} exited with status {}",
            output.status.code().unwrap_or(-1)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_sleep_disabled_means_the_default_off_state() {
        assert_eq!(
            disable_sleep_from_pmset("System-wide power settings:\n").unwrap(),
            0
        );
    }

    #[test]
    fn sleep_disabled_still_rejects_malformed_values() {
        assert_eq!(disable_sleep_from_pmset(" SleepDisabled 1\n").unwrap(), 1);
        assert!(disable_sleep_from_pmset(" SleepDisabled 2\n").is_err());
        assert!(disable_sleep_from_pmset(" SleepDisabled 0 extra\n").is_err());
    }

    #[test]
    fn elevated_helper_requires_root_owned_protected_paths() {
        assert!(trusted_permissions(0, 0o755));
        assert!(!trusted_permissions(501, 0o755));
        assert!(!trusted_permissions(0, 0o775));
        assert!(!trusted_permissions(0, 0o777));
    }

    #[test]
    fn acl_probe_and_write_probe_handle_an_ordinary_file() {
        let path = std::env::temp_dir().join(format!("wake-acl-probe-{}", std::process::id()));
        std::fs::write(&path, b"probe").unwrap();

        assert!(!has_extended_acl(&path).unwrap());
        if !caller_is_root() {
            assert!(caller_can_write(&path).unwrap());
        }

        let status = Command::new("/bin/chmod")
            .args(["+a", "admin allow write"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(has_extended_acl(&path).unwrap());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn restoration_changes_only_a_still_owned_value() {
        assert_eq!(restore_value(0, 1), Some(0));
        assert_eq!(restore_value(0, 0), None);
        assert_eq!(restore_value(1, 1), None);
    }
}
