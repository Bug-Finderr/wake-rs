use crate::error::{AppError, Result};
use crate::run::{ProcessIdentity, RunSpec};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::OnceLock;

pub const STATE_SCHEMA: u32 = 1;
const STATE_FILE: &str = "state.json";
const WAKE_LOCK_FILE: &str = "wake.lock";
const WATCHDOG_LOCK_FILE: &str = "lid-watchdog.lock";
const LEGACY_FILES: &[&str] = &[
    "session.properties",
    "session.json",
    "stop.json",
    "lid-restore.json",
    "lid-watchdog.json",
];

#[cfg(windows)]
static HELPER_STATE_DIR: OnceLock<PathBuf> = OnceLock::new();

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OwnerIdentity {
    pub token: String,
    pub process: ProcessIdentity,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionState {
    pub owner: OwnerIdentity,
    pub spec: RunSpec,
    pub started_at: Option<DateTime<Utc>>,
    pub note: Option<String>,
    pub stop_requested: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LidState {
    pub ready: bool,
    pub restore: LidRestore,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct State {
    pub schema: u32,
    pub session: SessionState,
    pub lid: Option<LidState>,
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

impl State {
    pub fn validate(&self) -> Result<()> {
        if self.schema != STATE_SCHEMA {
            return Err(AppError::fail(format!(
                "unsupported wake state schema {}",
                self.schema
            )));
        }
        if !valid_token(&self.session.owner.token) || !self.session.owner.process.is_valid() {
            return Err(AppError::fail("invalid session owner identity"));
        }
        self.session.spec.validate()?;
        if let Some(started_at) = self.session.started_at {
            self.session.spec.trigger.deadline(started_at)?;
        }
        if self.session.started_at.is_none() && self.session.note.is_some() {
            return Err(AppError::fail(
                "a starting session cannot publish an inhibitor note",
            ));
        }
        if let Some(lid) = &self.lid {
            if !self.session.spec.even_lid || self.session.started_at.is_none() {
                return Err(AppError::fail(
                    "lid state requires a started even-lid session",
                ));
            }
            lid.restore.validate_for_platform()?;
        }
        Ok(())
    }
}

impl LidRestore {
    fn validate_for_platform(&self) -> Result<()> {
        match self {
            Self::Macos {
                sleep_disabled: 0 | 1,
            } => {}
            Self::Macos { .. } => {
                return Err(AppError::fail("SleepDisabled must be 0 or 1"));
            }
            Self::Windows {
                scheme_guid,
                ac_action,
                dc_action,
            } if is_guid(scheme_guid)
                && (0..=3).contains(ac_action)
                && (0..=3).contains(dc_action) => {}
            Self::Windows { .. } => {
                return Err(AppError::fail(
                    "Windows lid restoration requires a scheme GUID and AC/DC actions in 0..=3",
                ));
            }
        }
        #[cfg(target_os = "linux")]
        {
            Err(AppError::fail("lid state is not valid on Linux"))
        }
        #[cfg(target_os = "macos")]
        {
            if matches!(self, Self::Macos { .. }) {
                Ok(())
            } else {
                Err(AppError::fail("Windows lid state is not valid on macOS"))
            }
        }
        #[cfg(windows)]
        {
            if matches!(self, Self::Windows { .. }) {
                Ok(())
            } else {
                Err(AppError::fail("macOS lid state is not valid on Windows"))
            }
        }
    }
}

fn valid_token(token: &str) -> bool {
    token.len() == 32
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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

pub fn new_token() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| AppError::fail(format!("could not create session identity: {error}")))?;
    Ok(format!("{:032x}", u128::from_ne_bytes(bytes)))
}

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
    validate_existing_directory(&dir)?;
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
        let path = PathBuf::from(xdg);
        if path.is_absolute() {
            return path.join("wake");
        }
    }
    home().join(".local").join("state").join("wake")
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

pub fn state_file() -> PathBuf {
    state_dir().join(STATE_FILE)
}

#[cfg(any(windows, target_os = "macos"))]
pub fn watchdog_lock_file() -> PathBuf {
    state_dir().join(WATCHDOG_LOCK_FILE)
}

#[cfg(target_os = "macos")]
pub fn validate_helper_state_dir() -> Result<()> {
    let dir = state_dir();
    if !dir.is_absolute() {
        return Err(AppError::fail("helper state directory must be absolute"));
    }
    let expected_owner = macos_helper_owner(&dir)?;
    validate_macos_private_layout(&dir, expected_owner)
}

#[cfg(windows)]
pub fn windows_user_sid() -> Result<String> {
    windows_security::current_user_sid()
}

#[cfg(windows)]
pub fn validate_helper_state_dir(expected_owner: &str) -> Result<()> {
    let dir = state_dir();
    if !dir.is_absolute() {
        return Err(AppError::fail("helper state directory must be absolute"));
    }
    validate_windows_state_dir(&dir, expected_owner)
}

#[cfg(windows)]
pub fn validate_windows_state_dir(dir: &Path, expected_owner: &str) -> Result<()> {
    let expected_owner = canonical_windows_sid(expected_owner)?;
    if windows_user_sid()? != expected_owner {
        return Err(AppError::fail(
            "elevated helper must run as the same Windows account",
        ));
    }
    validate_windows_private_layout(dir, &expected_owner, true)
}

#[cfg(target_os = "macos")]
fn macos_helper_owner(dir: &Path) -> Result<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;

    validate_existing_directory(dir)?;
    let metadata = fs::metadata(dir).map_err(|error| state_io_err(dir, error))?;
    match (
        std::env::var("SUDO_UID").ok(),
        std::env::var("SUDO_GID").ok(),
    ) {
        (Some(uid), Some(gid)) => Ok((
            uid.parse::<u32>()
                .map_err(|_| AppError::fail("invalid SUDO_UID"))?,
            gid.parse::<u32>()
                .map_err(|_| AppError::fail("invalid SUDO_GID"))?,
        )),
        (None, None) if metadata.uid() == 0 => Ok((0, metadata.gid())),
        _ => Err(AppError::fail("cannot determine the sudo caller identity")),
    }
}

#[cfg(target_os = "macos")]
fn validate_macos_private_layout(dir: &Path, expected_owner: (u32, u32)) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    validate_existing_directory(dir)?;
    let metadata = fs::metadata(dir).map_err(|error| state_io_err(dir, error))?;
    if metadata.permissions().mode() & 0o777 != 0o700
        || (metadata.uid(), metadata.gid()) != expected_owner
        || crate::platform::has_extended_acl(dir)?
    {
        return Err(AppError::fail(
            "helper state directory must be caller-owned with mode 0700 and no extended ACL",
        ));
    }
    for path in [
        dir.join(WAKE_LOCK_FILE),
        dir.join(WATCHDOG_LOCK_FILE),
        dir.join(STATE_FILE),
    ] {
        let metadata = fs::symlink_metadata(&path).map_err(|error| state_io_err(&path, error))?;
        validate_regular_metadata(&path, &metadata)?;
        if metadata.permissions().mode() & 0o777 != 0o600
            || (metadata.uid(), metadata.gid()) != expected_owner
            || crate::platform::has_extended_acl(&path)?
        {
            return Err(AppError::fail(format!(
                "helper state file must be caller-owned with mode 0600 and no extended ACL: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn canonical_windows_sid(sid: &str) -> Result<String> {
    windows_security::canonical_sid(sid)
}

#[cfg(windows)]
fn validate_windows_private_layout(
    dir: &Path,
    expected_owner: &str,
    require_state: bool,
) -> Result<()> {
    windows_security::validate_layout(dir, expected_owner, require_state)
}

#[cfg(all(test, windows))]
fn set_windows_path_sddl(path: &Path, sddl: &str) -> Result<()> {
    windows_security::set_path_sddl(path, sddl)
}

#[cfg(all(test, windows))]
fn validate_windows_directory_sddl(sddl: &str, expected_owner: &str) -> Result<()> {
    windows_security::validate_directory_sddl(sddl, expected_owner)
}

#[cfg(windows)]
mod windows_security {
    use super::*;
    use std::ffi::c_void;
    use std::mem::{size_of, size_of_val};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE,
        HLOCAL, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        ConvertStringSidToSidW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    #[cfg(test)]
    use windows_sys::Win32::Security::SetFileSecurityW;
    use windows_sys::Win32::Security::{
        ACE_HEADER, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION, EqualSid,
        GetAce, GetAclInformation, GetLengthSid, GetSecurityDescriptorControl, GetTokenInformation,
        INHERIT_ONLY_ACE, IsValidSid, IsWellKnownSid, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    #[cfg(test)]
    use windows_sys::Win32::Security::{GetSecurityDescriptorDacl, GetSecurityDescriptorOwner};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ALL_ACCESS, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
    };
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    struct LocalAllocation(*mut c_void);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: this pointer was allocated by a Windows API documented for LocalFree.
                unsafe { LocalFree(self.0 as HLOCAL) };
            }
        }
    }

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: this is a live token handle returned by OpenProcessToken.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    struct OwnedSid {
        ptr: PSID,
        _allocation: LocalAllocation,
    }

    impl OwnedSid {
        fn parse(value: &str) -> Result<Self> {
            if value.is_empty() || value.encode_utf16().any(|unit| unit == 0) {
                return Err(AppError::fail("invalid Windows caller SID"));
            }
            let wide = value
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>();
            let mut ptr = std::ptr::null_mut();
            // SAFETY: wide is a retained NUL-terminated buffer and ptr is an out parameter.
            if unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut ptr) } == 0 || ptr.is_null() {
                return Err(last_error("invalid Windows caller SID"));
            }
            let allocation = LocalAllocation(ptr);
            // SAFETY: ptr came from ConvertStringSidToSidW and remains owned by allocation.
            if unsafe { IsValidSid(ptr) } == 0 {
                return Err(AppError::fail("invalid Windows caller SID"));
            }
            Ok(Self {
                ptr,
                _allocation: allocation,
            })
        }
    }

    pub(super) fn canonical_sid(value: &str) -> Result<String> {
        let sid = OwnedSid::parse(value)?;
        sid_string(sid.ptr)
    }

    pub(super) fn current_user_sid() -> Result<String> {
        let mut token = std::ptr::null_mut();
        // SAFETY: GetCurrentProcess returns a process pseudo-handle and token is an out parameter.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(last_error("could not inspect the Windows account"));
        }
        let token = OwnedHandle(token);
        let mut needed = 0;
        // SAFETY: a null zero-length buffer is the documented size query.
        let first = unsafe {
            GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed)
        };
        let size_error = std::io::Error::last_os_error();
        if first != 0
            || needed < size_of::<TOKEN_USER>() as u32
            || size_error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
        {
            return Err(AppError::fail(format!(
                "could not size the Windows account identity: {size_error}"
            )));
        }
        let words = (needed as usize).div_ceil(size_of::<usize>());
        let mut buffer = vec![0_usize; words];
        // SAFETY: buffer is aligned and has at least needed writable bytes.
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(last_error("could not inspect the Windows account"));
        }
        // SAFETY: GetTokenInformation initialized a TOKEN_USER at the aligned buffer start.
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        sid_string(user.User.Sid)
    }

    fn sid_string(sid: PSID) -> Result<String> {
        // SAFETY: callers provide a SID returned by a Windows security API.
        if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
            return Err(AppError::fail("Windows account SID is invalid"));
        }
        let mut text = std::ptr::null_mut();
        // SAFETY: sid is valid and text is an out parameter.
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 || text.is_null() {
            return Err(last_error("could not encode the Windows account SID"));
        }
        let allocation = LocalAllocation(text.cast());
        let len = (0..256)
            // SAFETY: ConvertSidToStringSidW returned a NUL-terminated SID string.
            .find(|offset| unsafe { *text.add(*offset) == 0 })
            .ok_or_else(|| AppError::fail("Windows account SID is too long"))?;
        // SAFETY: the terminating NUL was found within the allocated string.
        let units = unsafe { std::slice::from_raw_parts(text, len) };
        let value = String::from_utf16(units)
            .map_err(|_| AppError::fail("Windows account SID is not valid UTF-16"));
        drop(allocation);
        value
    }

    pub(super) fn create_private_directory(path: &Path, owner: &str) -> Result<()> {
        let owner = canonical_sid(owner)?;
        let sddl = format!("O:{owner}D:P(A;OICI;FA;;;{owner})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
        create_directory_with_sddl(path, &sddl)
    }

    pub(super) fn create_directory_with_sddl(path: &Path, sddl: &str) -> Result<()> {
        use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

        let descriptor = descriptor_from_sddl(sddl)?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let path_wide = path_wide(path)?;
        // SAFETY: path_wide and attributes point to retained initialized buffers.
        if unsafe { CreateDirectoryW(path_wide.as_ptr(), &attributes) } == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
                return Err(state_io_err(path, error));
            }
        }
        Ok(())
    }

    pub(super) fn validate_layout(
        dir: &Path,
        expected_owner: &str,
        require_state: bool,
    ) -> Result<()> {
        let expected_owner = OwnedSid::parse(expected_owner)?;
        validate_path(dir, &expected_owner, true)?;
        for path in [dir.join(WAKE_LOCK_FILE), dir.join(WATCHDOG_LOCK_FILE)] {
            validate_path(&path, &expected_owner, false)?;
        }
        let state = dir.join(STATE_FILE);
        match fs::symlink_metadata(&state) {
            Ok(_) => validate_path(&state, &expected_owner, false),
            Err(error) if !require_state && error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(state_io_err(&state, error)),
        }
    }

    pub(super) fn validate_directory(path: &Path, expected_owner: &str) -> Result<()> {
        let expected_owner = OwnedSid::parse(expected_owner)?;
        validate_path(path, &expected_owner, true)
    }

    pub(super) fn validate_file(path: &Path, expected_owner: &str) -> Result<()> {
        let expected_owner = OwnedSid::parse(expected_owner)?;
        validate_path(path, &expected_owner, false)
    }

    fn validate_path(path: &Path, expected_owner: &OwnedSid, directory: bool) -> Result<()> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(
                FILE_FLAG_OPEN_REPARSE_POINT
                    | if directory {
                        FILE_FLAG_BACKUP_SEMANTICS
                    } else {
                        0
                    },
            );
        let file = options
            .open(path)
            .map_err(|error| state_io_err(path, error))?;
        let metadata = file.metadata().map_err(|error| state_io_err(path, error))?;
        if directory {
            if !metadata.is_dir() || metadata_is_reparse(&metadata) {
                return Err(AppError::fail(format!(
                    "state directory is not a regular non-link directory: {}",
                    path.display()
                )));
            }
        } else {
            validate_regular_metadata(path, &metadata)?;
        }
        validate_handle(&file, path, expected_owner, directory)
    }

    fn validate_handle(
        file: &File,
        path: &Path,
        expected_owner: &OwnedSid,
        directory: bool,
    ) -> Result<()> {
        let mut owner = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: file owns a live handle and all requested output pointers are valid.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle() as HANDLE,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(status_error(
                &format!("could not inspect Windows security at {}", path.display()),
                status,
            ));
        }
        let _descriptor = LocalAllocation(descriptor);
        validate_descriptor(descriptor, owner, dacl, expected_owner, path, directory)
    }

    fn validate_descriptor(
        descriptor: PSECURITY_DESCRIPTOR,
        owner: PSID,
        dacl: *mut windows_sys::Win32::Security::ACL,
        expected_owner: &OwnedSid,
        path: &Path,
        directory: bool,
    ) -> Result<()> {
        if owner.is_null()
            // SAFETY: both pointers came from the retained security descriptor.
            || unsafe { EqualSid(owner, expected_owner.ptr) } == 0
        {
            return Err(AppError::fail(format!(
                "state path is not owned by the expected Windows account: {}",
                path.display()
            )));
        }
        if dacl.is_null() {
            return Err(AppError::fail(format!(
                "state path has a null Windows DACL: {}",
                path.display()
            )));
        }
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: descriptor is retained and both output pointers are valid.
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
            return Err(last_error(
                "could not inspect Windows security descriptor control",
            ));
        }
        if directory && control & SE_DACL_PROTECTED == 0 {
            return Err(AppError::fail(format!(
                "state directory Windows DACL must be protected: {}",
                path.display()
            )));
        }
        validate_dacl(dacl, expected_owner, path)
    }

    fn validate_dacl(
        dacl: *mut windows_sys::Win32::Security::ACL,
        expected_owner: &OwnedSid,
        path: &Path,
    ) -> Result<()> {
        let mut info = ACL_SIZE_INFORMATION::default();
        // SAFETY: dacl belongs to the retained descriptor and info is a writable output buffer.
        if unsafe {
            GetAclInformation(
                dacl,
                (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                size_of_val(&info) as u32,
                AclSizeInformation,
            )
        } == 0
        {
            return Err(last_error("could not inspect the Windows state DACL"));
        }
        if info.AclBytesInUse < size_of::<windows_sys::Win32::Security::ACL>() as u32 {
            return Err(AppError::fail("Windows state DACL is malformed"));
        }
        if info.AceCount != 3 {
            return Err(private_dacl_error(path));
        }
        let acl_start = dacl as usize;
        let acl_end = acl_start
            .checked_add(info.AclBytesInUse as usize)
            .ok_or_else(|| AppError::fail("Windows state DACL is malformed"))?;
        let first_ace = acl_start + size_of::<windows_sys::Win32::Security::ACL>();
        let mut previous_end = first_ace;
        let mut trusted = [false; 3];
        for index in 0..info.AceCount {
            let mut ace = std::ptr::null_mut();
            // SAFETY: dacl is valid and ace is an out pointer for this bounded index.
            if unsafe { GetAce(dacl, index, &mut ace) } == 0 || ace.is_null() {
                return Err(last_error("could not inspect a Windows state DACL entry"));
            }
            let start = ace as usize;
            let header_end = start
                .checked_add(size_of::<ACE_HEADER>())
                .ok_or_else(|| AppError::fail("Windows state DACL entry is malformed"))?;
            if start < previous_end || header_end > acl_end {
                return Err(AppError::fail("Windows state DACL entry is malformed"));
            }
            // SAFETY: the complete ACE header was bounds-checked within the ACL allocation.
            let header = unsafe { std::ptr::read_unaligned(ace.cast::<ACE_HEADER>()) };
            let size = header.AceSize as usize;
            let end = start
                .checked_add(size)
                .ok_or_else(|| AppError::fail("Windows state DACL entry is malformed"))?;
            if size < size_of::<ACE_HEADER>() || !size.is_multiple_of(4) || end > acl_end {
                return Err(AppError::fail("Windows state DACL entry is malformed"));
            }
            previous_end = end;
            // SAFETY: the ACE's complete reported size was bounds-checked within the ACL.
            let bytes = unsafe { std::slice::from_raw_parts(ace.cast::<u8>(), size) };
            if header.AceType as u32 != ACCESS_ALLOWED_ACE_TYPE
                || u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0
                || read_u32(bytes, 4)? != FILE_ALL_ACCESS
            {
                return Err(private_dacl_error(path));
            }
            let (sid, sid_len) = sid_at(bytes, 8)?;
            if size != 8 + sid_len {
                return Err(AppError::fail("Windows state DACL entry is malformed"));
            }
            let index =
                trusted_sid_index(sid, expected_owner).ok_or_else(|| private_dacl_error(path))?;
            if std::mem::replace(&mut trusted[index], true) {
                return Err(private_dacl_error(path));
            }
        }
        trusted
            .iter()
            .all(|present| *present)
            .then_some(())
            .ok_or_else(|| private_dacl_error(path))
    }

    fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
        let bytes = bytes
            .get(offset..offset + size_of::<u32>())
            .ok_or_else(|| AppError::fail("Windows state DACL entry is malformed"))?;
        Ok(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
    }

    fn sid_at(ace: &[u8], offset: usize) -> Result<(PSID, usize)> {
        let bytes = ace
            .get(offset..)
            .ok_or_else(|| AppError::fail("Windows state DACL SID is malformed"))?;
        if bytes.len() < 8 {
            return Err(AppError::fail("Windows state DACL SID is malformed"));
        }
        let length = 8 + usize::from(bytes[1]) * 4;
        if length > bytes.len() {
            return Err(AppError::fail("Windows state DACL SID is malformed"));
        }
        // SAFETY: the SID's count-derived length was bounds-checked inside the ACE.
        let sid = unsafe { ace.as_ptr().add(offset) as PSID };
        // SAFETY: sid points to the bounds-checked SID bytes retained by ace.
        if unsafe { IsValidSid(sid) } == 0 || unsafe { GetLengthSid(sid) } as usize != length {
            return Err(AppError::fail("Windows state DACL SID is malformed"));
        }
        Ok((sid, length))
    }

    fn trusted_sid_index(sid: PSID, expected_owner: &OwnedSid) -> Option<usize> {
        // SAFETY: both SIDs were validated before this comparison.
        if unsafe { EqualSid(sid, expected_owner.ptr) } != 0 {
            Some(0)
        } else if unsafe { IsWellKnownSid(sid, WinLocalSystemSid) } != 0 {
            Some(1)
        } else if unsafe { IsWellKnownSid(sid, WinBuiltinAdministratorsSid) } != 0 {
            Some(2)
        } else {
            None
        }
    }

    fn private_dacl_error(path: &Path) -> AppError {
        AppError::fail(format!(
            "state path must grant full control only to its owner, SYSTEM, and Administrators: {}",
            path.display()
        ))
    }

    fn descriptor_from_sddl(sddl: &str) -> Result<LocalAllocation> {
        if sddl.encode_utf16().any(|unit| unit == 0) {
            return Err(AppError::fail("Windows security descriptor contains NUL"));
        }
        let wide = sddl
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: wide is retained and NUL-terminated; descriptor is an out parameter.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
            || descriptor.is_null()
        {
            return Err(last_error("invalid Windows security descriptor"));
        }
        Ok(LocalAllocation(descriptor))
    }

    fn path_wide(path: &Path) -> Result<Vec<u16>> {
        let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if wide.contains(&0) {
            return Err(AppError::fail(format!(
                "state path contains NUL: {}",
                path.display()
            )));
        }
        wide.push(0);
        Ok(wide)
    }

    #[cfg(test)]
    pub(super) fn validate_directory_sddl(sddl: &str, expected_owner: &str) -> Result<()> {
        let descriptor = descriptor_from_sddl(sddl)?;
        let expected_owner = OwnedSid::parse(expected_owner)?;
        let mut owner = std::ptr::null_mut();
        let mut owner_defaulted = 0;
        let mut dacl = std::ptr::null_mut();
        let mut dacl_present = 0;
        let mut dacl_defaulted = 0;
        // SAFETY: descriptor is retained and all output pointers are valid.
        if unsafe { GetSecurityDescriptorOwner(descriptor.0, &mut owner, &mut owner_defaulted) }
            == 0
            || unsafe {
                GetSecurityDescriptorDacl(
                    descriptor.0,
                    &mut dacl_present,
                    &mut dacl,
                    &mut dacl_defaulted,
                )
            } == 0
            || dacl_present == 0
        {
            return Err(last_error(
                "could not inspect test Windows security descriptor",
            ));
        }
        validate_descriptor(
            descriptor.0,
            owner,
            dacl,
            &expected_owner,
            Path::new("<test>"),
            true,
        )
    }

    #[cfg(test)]
    pub(super) fn set_path_sddl(path: &Path, sddl: &str) -> Result<()> {
        let descriptor = descriptor_from_sddl(sddl)?;
        let path_wide = path_wide(path)?;
        // SAFETY: path and descriptor remain valid through the call.
        if unsafe { SetFileSecurityW(path_wide.as_ptr(), DACL_SECURITY_INFORMATION, descriptor.0) }
            == 0
        {
            return Err(last_error("could not set the test Windows DACL"));
        }
        Ok(())
    }

    fn last_error(context: &str) -> AppError {
        AppError::fail(format!("{context}: {}", std::io::Error::last_os_error()))
    }

    fn status_error(context: &str, status: u32) -> AppError {
        AppError::fail(format!(
            "{context}: {}",
            std::io::Error::from_raw_os_error(status as i32)
        ))
    }
}

fn reject_stale_state_at(dir: &Path) -> Result<()> {
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
        if LEGACY_FILES.contains(&name)
            || valid_process_lease_name(name)
            || valid_atomic_artifact_name(name)
        {
            let path = entry.path();
            return Err(AppError::fail(format!(
                "legacy or incomplete wake state found at {}; stop it with the wake binary that created it, then inspect and remove it before retrying",
                path.display()
            )));
        }
    }
    Ok(())
}

fn valid_process_lease_name(name: &str) -> bool {
    name.strip_prefix("process-")
        .and_then(|name| name.strip_suffix(".lock"))
        .is_some_and(valid_token)
}

fn valid_atomic_artifact_name(name: &str) -> bool {
    name.strip_prefix(".state-")
        .and_then(|name| {
            name.strip_suffix(".tmp")
                .or_else(|| name.strip_suffix(".bak"))
        })
        .is_some_and(valid_token)
}

pub fn read() -> Result<Option<State>> {
    read_at(&state_file())
}

fn read_at(path: &Path) -> Result<Option<State>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(state_io_err(path, error)),
        Ok(metadata) => validate_regular_metadata(path, &metadata)?,
    }
    let file = open_existing_state(path)?;
    let state: State = serde_json::from_reader(file)
        .map_err(|error| AppError::fail(format!("invalid JSON at {}: {error}", path.display())))?;
    state.validate()?;
    Ok(Some(state))
}

fn open_existing_state(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .map_err(|error| state_io_err(path, error))?;
    let opened = file.metadata().map_err(|error| state_io_err(path, error))?;
    validate_regular_metadata(path, &opened)?;
    Ok(file)
}

pub fn create(_lock: &LockGuard, state: &State) -> Result<()> {
    create_at(&state_file(), state)
}

fn create_at(path: &Path, state: &State) -> Result<()> {
    state.validate()?;
    if read_at(path)?.is_some() {
        return Err(AppError::fail("session state already exists"));
    }
    write_atomic(path, state)
}

pub fn update(
    _lock: &LockGuard,
    owner: &OwnerIdentity,
    change: impl FnOnce(&mut State) -> Result<()>,
) -> Result<State> {
    update_at(&state_file(), owner, change)
}

fn update_at(
    path: &Path,
    owner: &OwnerIdentity,
    change: impl FnOnce(&mut State) -> Result<()>,
) -> Result<State> {
    let mut state =
        read_at(path)?.ok_or_else(|| AppError::fail("active session state is missing"))?;
    if &state.session.owner != owner {
        return Err(AppError::fail("active session ownership changed"));
    }
    let before = state.clone();
    change(&mut state)?;
    if state.session.owner != before.session.owner || state.session.spec != before.session.spec {
        return Err(AppError::fail(
            "session owner and run specification are immutable",
        ));
    }
    validate_transition(&before, &state)?;
    state.validate()?;
    write_atomic(path, &state)?;
    Ok(state)
}

fn validate_transition(before: &State, after: &State) -> Result<()> {
    if before.schema != after.schema {
        return Err(AppError::fail("state schema is immutable"));
    }
    if before.session.stop_requested && !after.session.stop_requested {
        return Err(AppError::fail("a session stop request cannot be cleared"));
    }
    if before.session.started_at.is_some()
        && (before.session.started_at != after.session.started_at
            || before.session.note != after.session.note)
    {
        return Err(AppError::fail("started session metadata is immutable"));
    }
    match (&before.lid, &after.lid) {
        (None, Some(new))
            if new.ready || before.session.stop_requested || after.session.stop_requested =>
        {
            return Err(AppError::fail(
                "lid startup must publish ready=false before a stop request",
            ));
        }
        (Some(_), None) => return Err(AppError::fail("lid state cannot be removed by update")),
        (Some(old), Some(new)) => {
            if old.restore != new.restore {
                return Err(AppError::fail("lid restoration state is immutable"));
            }
            if old.ready && !new.ready {
                return Err(AppError::fail("lid readiness cannot move backwards"));
            }
            if !old.ready && new.ready && after.session.stop_requested {
                return Err(AppError::fail(
                    "lid readiness cannot be published after a stop request",
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn remove_exact(_lock: &LockGuard, expected: &State) -> Result<bool> {
    remove_exact_at(&state_file(), expected)
}

fn remove_exact_at(path: &Path, expected: &State) -> Result<bool> {
    let Some(actual) = read_at(path)? else {
        return Ok(false);
    };
    if actual != *expected {
        return Ok(false);
    }
    fs::remove_file(path).map_err(|error| state_io_err(path, error))?;
    if let Some(dir) = path.parent() {
        sync_parent(dir).map_err(|error| state_io_err(dir, error))?;
    }
    Ok(true)
}

fn write_atomic(path: &Path, state: &State) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| AppError::fail(format!("state path has no parent: {}", path.display())))?;
    validate_existing_directory(dir)?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        validate_regular_metadata(path, &metadata)?;
    }
    #[cfg(windows)]
    let expected_owner = windows_user_sid()?;
    let data = serde_json::to_vec(state)
        .map_err(|error| AppError::fail(format!("could not encode {}: {error}", path.display())))?;
    let (tmp, mut file) = create_random_temp(dir)?;
    let prepared = (|| {
        prepare_temp_permissions(&file, dir)?;
        file.write_all(&data)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|error| state_io_err(&tmp, error))
    })();
    drop(file);
    if let Err(error) = prepared {
        return Err(remove_temp_after_failure(&tmp, error));
    }
    #[cfg(windows)]
    replace_file(&tmp, path, &expected_owner, state)?;
    #[cfg(not(windows))]
    replace_file(&tmp, path)?;
    sync_parent(dir).map_err(|error| state_io_err(dir, error))
}

fn create_random_temp(dir: &Path) -> Result<(PathBuf, File)> {
    for _ in 0..8 {
        let path = dir.join(format!(".state-{}.tmp", new_token()?));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(state_io_err(&path, error)),
        }
    }
    Err(AppError::fail(
        "could not allocate a unique state temporary file",
    ))
}

#[cfg(target_os = "macos")]
fn prepare_temp_permissions(file: &File, dir: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    let metadata = fs::metadata(dir).map_err(|error| state_io_err(dir, error))?;
    let expected = match (
        std::env::var("SUDO_UID").ok(),
        std::env::var("SUDO_GID").ok(),
    ) {
        (Some(uid), Some(gid)) => {
            let uid = uid
                .parse::<u32>()
                .map_err(|_| AppError::fail("invalid SUDO_UID"))?;
            let gid = gid
                .parse::<u32>()
                .map_err(|_| AppError::fail("invalid SUDO_GID"))?;
            if metadata.uid() != uid || metadata.gid() != gid {
                return Err(AppError::fail(
                    "state directory ownership does not match the sudo caller",
                ));
            }
            (uid, gid)
        }
        (None, None) if metadata.uid() == 0 => (0, metadata.gid()),
        _ => return Err(AppError::fail("cannot determine the sudo caller identity")),
    };
    // SAFETY: file owns a live descriptor; the validated IDs and mode are passed directly.
    if unsafe { libc::fchown(file.as_raw_fd(), expected.0, expected.1) } != 0
        || unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0
    {
        return Err(AppError::from(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn prepare_temp_permissions(_file: &File, _dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(tmp: &Path, path: &Path) -> Result<()> {
    fs::rename(tmp, path).map_err(|error| remove_temp_after_failure(tmp, state_io_err(path, error)))
}

#[cfg(windows)]
fn replace_file(tmp: &Path, path: &Path, expected_owner: &str, expected: &State) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(remove_temp_after_failure(tmp, state_io_err(path, error)));
        }
    };
    let Some(metadata) = metadata else {
        return match move_file(tmp, path) {
            Ok(()) => validate_expected_windows_state(path, expected_owner, expected),
            Err(error) => Err(remove_temp_after_failure(tmp, state_io_err(path, error))),
        };
    };
    if let Err(error) = validate_regular_metadata(path, &metadata) {
        return Err(remove_temp_after_failure(tmp, error));
    }
    let dir = path
        .parent()
        .ok_or_else(|| AppError::fail(format!("state path has no parent: {}", path.display())))?;
    let backup = match random_backup_path(dir) {
        Ok(path) => path,
        Err(error) => return Err(remove_temp_after_failure(tmp, error)),
    };
    let result = raw_replace_file(path, tmp, &backup);
    finish_existing_replace(
        tmp,
        path,
        &backup,
        expected_owner,
        expected,
        result,
        move_file,
    )
}

#[cfg(windows)]
fn raw_replace_file(path: &Path, tmp: &Path, backup: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    let wide = |value: &Path| {
        value
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>()
    };
    let tmp = wide(tmp);
    let path = wide(path);
    let backup = wide(backup);
    // SAFETY: all paths are retained NUL-terminated UTF-16 buffers. Flags are zero so metadata,
    // including the replaced file's DACL, is preserved instead of ignoring merge errors.
    let ok = unsafe {
        ReplaceFileW(
            path.as_ptr(),
            tmp.as_ptr(),
            backup.as_ptr(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn move_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let wide = |value: &Path| {
        value
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>()
    };
    let source = wide(source);
    let destination = wide(destination);
    // SAFETY: both paths are retained NUL-terminated UTF-16 buffers through the call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn random_backup_path(dir: &Path) -> Result<PathBuf> {
    for _ in 0..8 {
        let path = dir.join(format!(".state-{}.bak", new_token()?));
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(path),
            Ok(_) => continue,
            Err(error) => return Err(state_io_err(&path, error)),
        }
    }
    Err(AppError::fail(
        "could not allocate a unique state backup path",
    ))
}

fn remove_temp_after_failure(tmp: &Path, error: AppError) -> AppError {
    match fs::remove_file(tmp) {
        Ok(()) => error,
        Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => error,
        Err(cleanup) => AppError::fail(format!(
            "{error}; attempted state remains at {} because cleanup failed: {cleanup}",
            tmp.display()
        )),
    }
}

#[cfg(windows)]
fn validate_complete_windows_state(path: &Path, expected_owner: &str) -> Result<State> {
    windows_security::validate_file(path, expected_owner)?;
    read_at(path)?.ok_or_else(|| AppError::fail("canonical state is missing"))
}

#[cfg(windows)]
fn validate_expected_windows_state(
    path: &Path,
    expected_owner: &str,
    expected: &State,
) -> Result<()> {
    if validate_complete_windows_state(path, expected_owner)? != *expected {
        return Err(AppError::fail(
            "canonical state does not match the intended state",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn finish_existing_replace(
    tmp: &Path,
    path: &Path,
    backup: &Path,
    expected_owner: &str,
    expected: &State,
    replace_result: std::io::Result<()>,
    restore: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    use windows_sys::Win32::Foundation::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2;

    match replace_result {
        Err(error) if error.raw_os_error() == Some(ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 as i32) => {
            let primary = AppError::fail(format!(
                "state replacement failed at {}: {error}",
                path.display()
            ));
            if let Err(rollback) = restore(backup, path) {
                return Err(AppError::fail(format!(
                    "{primary}; rollback failed: {rollback}; previous state remains at {}; attempted state remains at {}",
                    backup.display(),
                    tmp.display()
                )));
            }
            if let Err(validation) = validate_complete_windows_state(path, expected_owner) {
                return Err(AppError::fail(format!(
                    "{primary}; restored state validation failed: {validation}; attempted state remains at {}",
                    tmp.display()
                )));
            }
            if let Err(cleanup) = fs::remove_file(tmp) {
                return Err(AppError::fail(format!(
                    "{primary}; attempted state remains at {} because cleanup failed: {cleanup}",
                    tmp.display()
                )));
            }
            Err(primary)
        }
        Err(error) => {
            let primary = AppError::fail(format!(
                "state replacement failed at {}: {error}",
                path.display()
            ));
            if let Err(validation) = validate_complete_windows_state(path, expected_owner) {
                return Err(AppError::fail(format!(
                    "{primary}; canonical state validation failed: {validation}; attempted state remains at {}",
                    tmp.display()
                )));
            }
            if let Err(cleanup) = fs::remove_file(tmp) {
                return Err(AppError::fail(format!(
                    "{primary}; attempted state remains at {} because cleanup failed: {cleanup}",
                    tmp.display()
                )));
            }
            Err(primary)
        }
        Ok(()) => {
            if let Err(validation) = validate_expected_windows_state(path, expected_owner, expected)
            {
                return Err(AppError::fail(format!(
                    "state replacement validation failed: {validation}; attempted state is at {}; previous state remains at {}",
                    path.display(),
                    backup.display()
                )));
            }
            fs::remove_file(backup).map_err(|cleanup| {
                AppError::fail(format!(
                    "state replacement committed at {}, but previous state remains at {} because cleanup failed: {cleanup}",
                    path.display(),
                    backup.display()
                ))
            })
        }
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

pub struct LockGuard {
    file: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub struct WatchdogLock {
    file: File,
}

impl Drop for WatchdogLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub fn try_acquire_lock() -> Result<Option<LockGuard>> {
    try_acquire_lock_at(&state_dir())
}

fn try_acquire_lock_at(dir: &Path) -> Result<Option<LockGuard>> {
    ensure_state_dir(dir)?;
    ensure_private_lock(&dir.join(WATCHDOG_LOCK_FILE))?;
    let file = ensure_private_lock(&dir.join(WAKE_LOCK_FILE))?;
    #[cfg(windows)]
    validate_windows_private_layout(dir, &windows_user_sid()?, false)?;
    match file.try_lock() {
        Ok(()) => {
            reject_stale_state_at(dir)?;
            Ok(Some(LockGuard { file }))
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(state_io_err(dir, error)),
    }
}

pub fn acquire_lock() -> Result<LockGuard> {
    try_acquire_lock()?
        .ok_or_else(|| AppError::fail("another wake invocation is in progress; try again"))
}

pub fn acquire_existing_lock_wait() -> Result<LockGuard> {
    acquire_existing_lock_at(&state_dir())
}

fn acquire_existing_lock_at(dir: &Path) -> Result<LockGuard> {
    let path = dir.join(WAKE_LOCK_FILE);
    let file = open_existing_lock(&path)?;
    file.lock()
        .map_err(|error| AppError::fail(format!("could not acquire state lock: {error}")))?;
    reject_stale_state_at(dir)?;
    Ok(LockGuard { file })
}

#[cfg(any(windows, target_os = "macos"))]
pub fn acquire_watchdog_lock() -> Result<WatchdogLock> {
    let path = watchdog_lock_file();
    let file = open_existing_lock(&path)?;
    file.lock().map_err(|error| {
        AppError::fail(format!(
            "could not acquire lid watchdog lock at {}: {error}",
            path.display()
        ))
    })?;
    Ok(WatchdogLock { file })
}

pub fn try_watchdog_lock() -> Result<Option<WatchdogLock>> {
    try_watchdog_lock_at(&state_dir())
}

fn try_watchdog_lock_at(dir: &Path) -> Result<Option<WatchdogLock>> {
    let path = dir.join(WATCHDOG_LOCK_FILE);
    let file = open_existing_lock(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(WatchdogLock { file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(state_io_err(&path, error)),
    }
}

fn ensure_state_dir(dir: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let owner = windows_user_sid()?;
        match fs::symlink_metadata(dir) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let absolute = std::path::absolute(dir).map_err(AppError::from)?;
                let parent = absolute.parent().ok_or_else(|| {
                    AppError::fail(format!(
                        "state directory has no parent: {}",
                        absolute.display()
                    ))
                })?;
                match fs::symlink_metadata(parent) {
                    Ok(_) => validate_existing_directory(parent)?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Err(AppError::fail(format!(
                            "state directory parent must already exist: {}",
                            parent.display()
                        )));
                    }
                    Err(error) => return Err(state_io_err(parent, error)),
                }
                windows_security::create_private_directory(&absolute, &owner)?;
            }
            Err(error) => return Err(state_io_err(dir, error)),
        }
        validate_existing_directory(dir)?;
        windows_security::validate_directory(dir, &owner)
    }
    #[cfg(unix)]
    {
        fs::create_dir_all(dir).map_err(|error| state_io_err(dir, error))?;
        validate_existing_directory(dir)?;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = fs::metadata(dir).map_err(|error| state_io_err(dir, error))?;
        // SAFETY: geteuid has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(AppError::fail(format!(
                "state directory is not owned by the current user: {}",
                dir.display()
            )));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .map_err(|error| state_io_err(dir, error))?;
        }
        Ok(())
    }
}

fn validate_existing_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| state_io_err(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || metadata_is_reparse(&metadata) {
        return Err(AppError::fail(format!(
            "state directory is not a regular non-link directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_private_lock(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .map_err(|error| state_io_err(path, error))?;
    let metadata = file.metadata().map_err(|error| state_io_err(path, error))?;
    validate_regular_metadata(path, &metadata)?;
    #[cfg(unix)]
    secure_foreground_lock(path, &file, &metadata)?;
    Ok(file)
}

#[cfg(unix)]
fn secure_foreground_lock(path: &Path, file: &File, metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    // SAFETY: geteuid has no preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(AppError::fail(format!(
            "state lock is not owned by the current user: {}",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| state_io_err(path, error))?;
    }
    Ok(())
}

fn open_existing_lock(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path).map_err(|error| state_io_err(path, error))?;
    validate_regular_metadata(path, &metadata)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .map_err(|error| state_io_err(path, error))?;
    let opened = file.metadata().map_err(|error| state_io_err(path, error))?;
    validate_regular_metadata(path, &opened)?;
    Ok(file)
}

fn validate_regular_metadata(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata_is_reparse(metadata) {
        return Err(AppError::fail(format!(
            "state path is not a regular non-link file: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse(_metadata: &fs::Metadata) -> bool {
    false
}

fn state_io_err(path: &Path, error: std::io::Error) -> AppError {
    AppError::fail(format!(
        "state IO failed at {}: {error}; set WAKE_STATE_DIR to a writable directory",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{Mode, Trigger};
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
            ensure_state_dir(&path).unwrap();
            Self(path)
        }

        fn state(&self) -> PathBuf {
            self.0.join(STATE_FILE)
        }

        fn lock(&self) -> LockGuard {
            try_acquire_lock_at(&self.0).unwrap().unwrap()
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sample_state() -> State {
        State {
            schema: STATE_SCHEMA,
            session: SessionState {
                owner: OwnerIdentity {
                    token: "0123456789abcdef0123456789abcdef".into(),
                    process: ProcessIdentity {
                        pid: 42,
                        native_start: 99,
                    },
                },
                spec: RunSpec {
                    mode: Mode::SystemOnly,
                    trigger: Trigger::Indefinite,
                    even_lid: false,
                },
                started_at: Some("2024-01-02T03:04:05Z".parse().unwrap()),
                note: None,
                stop_requested: false,
            },
            lid: None,
        }
    }

    #[test]
    fn state_json_is_strict_and_round_trips() {
        let state = sample_state();
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<State>(&json).unwrap(), state);
        let unknown = json.replacen('{', "{\"unknown\":true,", 1);
        assert!(serde_json::from_str::<State>(&unknown).is_err());
    }

    #[test]
    fn validation_rejects_invalid_state_table() {
        let mut cases: Vec<State> = Vec::new();
        let mut schema = sample_state();
        schema.schema = 2;
        cases.push(schema);
        let mut token = sample_state();
        token.session.owner.token = "bad".into();
        cases.push(token);
        let mut pid = sample_state();
        pid.session.owner.process.pid = 0;
        cases.push(pid);
        let mut start = sample_state();
        start.session.owner.process.native_start = 0;
        cases.push(start);
        let mut note = sample_state();
        note.session.started_at = None;
        note.session.note = Some("early".into());
        cases.push(note);
        for state in cases {
            assert!(state.validate().is_err());
        }

        let mut overflow = sample_state();
        overflow.session.started_at = Some(DateTime::<Utc>::MAX_UTC);
        overflow.session.spec.trigger = Trigger::Timed {
            seconds: 1,
            input: "1s".into(),
        };
        assert!(overflow.validate().is_err());
    }

    #[test]
    fn lid_validation_is_platform_bound() {
        let mut state = sample_state();
        state.lid = Some(LidState {
            ready: false,
            restore: LidRestore::Windows {
                scheme_guid: "381b4222-f694-41f0-9685-ff5bb260df2e".into(),
                ac_action: 1,
                dc_action: 2,
            },
        });
        assert!(state.validate().is_err());
        state.session.spec.even_lid = true;
        #[cfg(windows)]
        assert!(state.validate().is_ok());
        #[cfg(not(windows))]
        assert!(state.validate().is_err());
    }

    #[test]
    fn lid_state_serializes_only_ready_and_restore() {
        let lid = LidState {
            ready: false,
            restore: LidRestore::Windows {
                scheme_guid: "381b4222-f694-41f0-9685-ff5bb260df2e".into(),
                ac_action: 1,
                dc_action: 2,
            },
        };

        let json = serde_json::to_string(&lid).unwrap();

        assert!(!json.contains(r#""watchdog""#));
        assert!(json.contains(r#""ready""#));
        assert!(json.contains(r#""restore""#));
    }

    #[test]
    fn lifecycle_updates_preserve_exact_owner_and_monotonic_state() {
        let dir = TestDir::new("state-update");
        let path = dir.state();
        let lock = dir.lock();
        let mut starting = sample_state();
        starting.session.started_at = None;
        create_at(&path, &starting).unwrap();
        let mut wrong_owner = starting.session.owner.clone();
        wrong_owner.token = "11111111111111111111111111111111".into();
        assert!(update_at(&path, &wrong_owner, |_| Ok(())).is_err());
        let ready = update_at(&path, &starting.session.owner, |current| {
            current.session.started_at = Some("2024-01-02T03:04:05Z".parse().unwrap());
            current.session.note = Some("ready".into());
            Ok(())
        })
        .unwrap();
        let stopped = update_at(&path, &starting.session.owner, |current| {
            current.session.stop_requested = true;
            Ok(())
        })
        .unwrap();
        assert!(
            update_at(&path, &starting.session.owner, |current| {
                current.session.owner.token = "11111111111111111111111111111111".into();
                Ok(())
            })
            .is_err()
        );
        assert!(
            update_at(&path, &starting.session.owner, |current| {
                current.session.stop_requested = false;
                Ok(())
            })
            .is_err()
        );
        assert_eq!(stopped.session.owner, ready.session.owner);
        assert!(!remove_exact_at(&path, &ready).unwrap());
        assert!(remove_exact_at(&path, &stopped).unwrap());
        drop(lock);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn lid_updates_follow_the_write_ahead_transition() {
        let dir = TestDir::new("lid-transition");
        let path = dir.state();
        let _lock = dir.lock();
        let mut state = sample_state();
        state.session.spec.even_lid = true;
        create_at(&path, &state).unwrap();
        #[cfg(windows)]
        let restore = LidRestore::Windows {
            scheme_guid: "381b4222-f694-41f0-9685-ff5bb260df2e".into(),
            ac_action: 1,
            dc_action: 2,
        };
        #[cfg(target_os = "macos")]
        let restore = LidRestore::Macos { sleep_disabled: 0 };
        assert!(
            update_at(&path, &state.session.owner, |current| {
                current.lid = Some(LidState {
                    ready: true,
                    restore: restore.clone(),
                });
                Ok(())
            })
            .is_err()
        );
        let starting = update_at(&path, &state.session.owner, |current| {
            current.lid = Some(LidState {
                ready: false,
                restore: restore.clone(),
            });
            Ok(())
        })
        .unwrap();
        assert!(
            update_at(&path, &state.session.owner, |current| {
                #[cfg(windows)]
                let changed = LidRestore::Windows {
                    scheme_guid: "381b4222-f694-41f0-9685-ff5bb260df2e".into(),
                    ac_action: 3,
                    dc_action: 2,
                };
                #[cfg(target_os = "macos")]
                let changed = LidRestore::Macos { sleep_disabled: 1 };
                current.lid.as_mut().unwrap().restore = changed;
                Ok(())
            })
            .is_err()
        );
        let ready = update_at(&path, &state.session.owner, |current| {
            current.lid.as_mut().unwrap().ready = true;
            Ok(())
        })
        .unwrap();
        assert!(!starting.lid.unwrap().ready);
        assert!(ready.lid.unwrap().ready);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn lid_cannot_start_after_stop() {
        let dir = TestDir::new("lid-after-stop");
        let path = dir.state();
        let _lock = dir.lock();
        let mut state = sample_state();
        state.session.spec.even_lid = true;
        state.session.stop_requested = true;
        create_at(&path, &state).unwrap();
        #[cfg(windows)]
        let restore = LidRestore::Windows {
            scheme_guid: "381b4222-f694-41f0-9685-ff5bb260df2e".into(),
            ac_action: 1,
            dc_action: 2,
        };
        #[cfg(target_os = "macos")]
        let restore = LidRestore::Macos { sleep_disabled: 0 };
        assert!(
            update_at(&path, &state.session.owner, |current| {
                current.lid = Some(LidState {
                    ready: false,
                    restore,
                });
                Ok(())
            })
            .is_err()
        );
    }

    #[test]
    fn malformed_json_is_retained() {
        let dir = TestDir::new("state-malformed");
        let path = dir.state();
        fs::write(&path, b"not json").unwrap();
        assert!(read_at(&path).is_err());
        assert!(path.exists());
    }

    #[test]
    fn all_old_formats_are_rejected_after_the_writer_lock() {
        for name in LEGACY_FILES
            .iter()
            .copied()
            .chain(["process-0123456789abcdef0123456789abcdef.lock"])
        {
            let dir = TestDir::new("legacy");
            fs::write(dir.0.join(name), b"old").unwrap();
            assert!(try_acquire_lock_at(&dir.0).is_err(), "{name}");
        }
    }

    #[test]
    fn incomplete_atomic_state_is_checked_only_after_the_writer_lock() {
        for name in [
            ".state-0123456789abcdef0123456789abcdef.tmp",
            ".state-0123456789abcdef0123456789abcdef.bak",
        ] {
            let dir = TestDir::new("incomplete-atomic-state");
            let held = dir.lock();
            fs::write(dir.0.join(name), b"interrupted").unwrap();
            assert!(try_acquire_lock_at(&dir.0).unwrap().is_none());
            drop(held);
            let error = match try_acquire_lock_at(&dir.0) {
                Err(error) => error,
                Ok(_) => panic!("incomplete state was accepted"),
            };
            assert!(error.message().contains(name), "{name}");
        }
    }

    #[test]
    fn watchdog_lock_file_is_permanent() {
        let dir = TestDir::new("watchdog-lock");
        let guard = dir.lock();
        let path = dir.0.join(WATCHDOG_LOCK_FILE);
        assert!(path.exists());
        let first = try_watchdog_lock_at(&dir.0).unwrap().unwrap();
        assert!(try_watchdog_lock_at(&dir.0).unwrap().is_none());
        drop(first);
        assert!(try_watchdog_lock_at(&dir.0).unwrap().is_some());
        drop(guard);
        assert!(path.exists());
    }

    #[test]
    fn helper_lock_open_never_creates_a_missing_file() {
        let dir = TestDir::new("existing-lock");
        let guard = dir.lock();
        drop(guard);
        let path = dir.0.join(WAKE_LOCK_FILE);
        fs::remove_file(&path).unwrap();
        assert!(acquire_existing_lock_at(&dir.0).is_err());
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn link_policy_rejects_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = TestDir::new("state-link");
        let target = dir.0.join("target");
        let link = dir.state();
        fs::write(&target, b"keep").unwrap();
        symlink(&target, &link).unwrap();
        assert!(read_at(&link).is_err());
        assert!(open_existing_state(&link).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn foreground_corrects_a_loose_existing_lock_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new("lock-mode");
        let path = dir.0.join(WAKE_LOCK_FILE);
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();

        let guard = try_acquire_lock_at(&dir.0).unwrap().unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;

        drop(guard);
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn tokens_are_random_lowercase_hex() {
        let left = new_token().unwrap();
        let right = new_token().unwrap();
        assert!(valid_token(&left));
        assert!(valid_token(&right));
        assert_ne!(left, right);
    }

    #[cfg(windows)]
    fn replacement_fixture(name: &str) -> (TestDir, PathBuf, PathBuf, PathBuf, State, State) {
        let dir = TestDir::new(name);
        let path = dir.state();
        let old = sample_state();
        create_at(&path, &old).unwrap();

        let mut new = old.clone();
        new.session.stop_requested = true;
        let (tmp, mut file) = create_random_temp(&dir.0).unwrap();
        serde_json::to_writer(&mut file, &new).unwrap();
        file.write_all(b"\n").unwrap();
        file.sync_all().unwrap();
        drop(file);

        let backup = dir.0.join(".state-0123456789abcdef0123456789abcdef.bak");
        (dir, path, tmp, backup, old, new)
    }

    #[cfg(windows)]
    #[test]
    fn windows_non_1177_replace_errors_retain_canonical_and_discard_temp() {
        use windows_sys::Win32::Foundation::{
            ERROR_INVALID_PARAMETER, ERROR_UNABLE_TO_MOVE_REPLACEMENT,
            ERROR_UNABLE_TO_REMOVE_REPLACED,
        };

        for (name, code) in [
            ("windows-replace-1175", ERROR_UNABLE_TO_REMOVE_REPLACED),
            ("windows-replace-1176", ERROR_UNABLE_TO_MOVE_REPLACEMENT),
            ("windows-replace-other-error", ERROR_INVALID_PARAMETER),
        ] {
            let (_dir, path, tmp, backup, old, new) = replacement_fixture(name);
            let owner = windows_user_sid().unwrap();
            let error = finish_existing_replace(
                &tmp,
                &path,
                &backup,
                &owner,
                &new,
                Err(std::io::Error::from_raw_os_error(code as i32)),
                |from, to| fs::rename(from, to),
            )
            .unwrap_err();

            assert!(error.message().contains("state replacement failed"));
            assert_eq!(read_at(&path).unwrap(), Some(old));
            assert!(!tmp.exists());
            assert!(!backup.exists());
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_replace_error_preserves_and_names_temp_when_canonical_is_untrusted() {
        use windows_sys::Win32::Foundation::ERROR_UNABLE_TO_REMOVE_REPLACED;

        let (_dir, path, tmp, backup, _old, new) =
            replacement_fixture("windows-replace-untrusted-canonical");
        let error = finish_existing_replace(
            &tmp,
            &path,
            &backup,
            "S-1-1-0",
            &new,
            Err(std::io::Error::from_raw_os_error(
                ERROR_UNABLE_TO_REMOVE_REPLACED as i32,
            )),
            |from, to| fs::rename(from, to),
        )
        .unwrap_err();

        assert_eq!(read_at(&tmp).unwrap(), Some(new));
        assert!(error.message().contains(&tmp.display().to_string()));
    }

    #[cfg(windows)]
    #[test]
    fn windows_replace_error_preserves_temp_when_canonical_json_is_invalid() {
        use windows_sys::Win32::Foundation::ERROR_UNABLE_TO_REMOVE_REPLACED;

        let (_dir, path, tmp, backup, _old, new) =
            replacement_fixture("windows-replace-invalid-canonical");
        fs::write(&path, b"invalid json").unwrap();
        let owner = windows_user_sid().unwrap();
        let error = finish_existing_replace(
            &tmp,
            &path,
            &backup,
            &owner,
            &new,
            Err(std::io::Error::from_raw_os_error(
                ERROR_UNABLE_TO_REMOVE_REPLACED as i32,
            )),
            |from, to| fs::rename(from, to),
        )
        .unwrap_err();

        assert_eq!(read_at(&tmp).unwrap(), Some(new));
        assert!(error.message().contains(&tmp.display().to_string()));
    }

    #[cfg(windows)]
    #[test]
    fn windows_replace_error_1177_rolls_back_before_discarding_temp() {
        use windows_sys::Win32::Foundation::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2;

        let (_dir, path, tmp, backup, old, new) =
            replacement_fixture("windows-replace-1177-rollback");
        fs::rename(&path, &backup).unwrap();
        let owner = windows_user_sid().unwrap();
        let error = finish_existing_replace(
            &tmp,
            &path,
            &backup,
            &owner,
            &new,
            Err(std::io::Error::from_raw_os_error(
                ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 as i32,
            )),
            |from, to| fs::rename(from, to),
        )
        .unwrap_err();

        assert!(error.message().contains("state replacement failed"));
        assert_eq!(read_at(&path).unwrap(), Some(old));
        assert!(!tmp.exists());
        assert!(!backup.exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_replace_error_1177_preserves_both_copies_when_rollback_fails() {
        use windows_sys::Win32::Foundation::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2;

        let (_dir, path, tmp, backup, old, new) =
            replacement_fixture("windows-replace-1177-failed-rollback");
        fs::rename(&path, &backup).unwrap();
        let owner = windows_user_sid().unwrap();
        let error = finish_existing_replace(
            &tmp,
            &path,
            &backup,
            &owner,
            &new,
            Err(std::io::Error::from_raw_os_error(
                ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 as i32,
            )),
            |_, _| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        )
        .unwrap_err();

        assert!(!path.exists());
        assert_eq!(read_at(&backup).unwrap(), Some(old));
        assert_eq!(read_at(&tmp).unwrap(), Some(new));
        assert!(error.message().contains(&backup.display().to_string()));
        assert!(error.message().contains(&tmp.display().to_string()));
    }

    #[cfg(windows)]
    #[test]
    fn windows_1177_rollback_preserves_and_names_temp_when_canonical_is_untrusted() {
        use windows_sys::Win32::Foundation::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2;

        let (_dir, path, tmp, backup, old, new) =
            replacement_fixture("windows-replace-1177-untrusted-canonical");
        fs::rename(&path, &backup).unwrap();
        let error = finish_existing_replace(
            &tmp,
            &path,
            &backup,
            "S-1-1-0",
            &new,
            Err(std::io::Error::from_raw_os_error(
                ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 as i32,
            )),
            |from, to| fs::rename(from, to),
        )
        .unwrap_err();

        assert_eq!(read_at(&path).unwrap(), Some(old));
        assert_eq!(read_at(&tmp).unwrap(), Some(new));
        assert!(!backup.exists());
        assert!(error.message().contains(&tmp.display().to_string()));
    }

    #[cfg(windows)]
    #[test]
    fn windows_successful_replace_discards_backup_after_validation() {
        let (_dir, path, tmp, backup, _old, new) = replacement_fixture("windows-replace-success");
        fs::rename(&path, &backup).unwrap();
        fs::rename(&tmp, &path).unwrap();
        let owner = windows_user_sid().unwrap();

        finish_existing_replace(&tmp, &path, &backup, &owner, &new, Ok(()), |from, to| {
            fs::rename(from, to)
        })
        .unwrap();

        assert_eq!(read_at(&path).unwrap(), Some(new));
        assert!(!tmp.exists());
        assert!(!backup.exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_successful_replace_preserves_and_names_backup_when_validation_fails() {
        let (_dir, path, tmp, backup, old, new) =
            replacement_fixture("windows-replace-success-invalid");
        fs::rename(&path, &backup).unwrap();
        fs::rename(&tmp, &path).unwrap();

        let error =
            finish_existing_replace(&tmp, &path, &backup, "S-1-1-0", &new, Ok(()), |from, to| {
                fs::rename(from, to)
            })
            .unwrap_err();

        assert_eq!(read_at(&path).unwrap(), Some(new));
        assert_eq!(read_at(&backup).unwrap(), Some(old));
        assert!(error.message().contains(&backup.display().to_string()));
    }

    #[cfg(windows)]
    #[test]
    fn windows_successful_replace_rejects_an_unexpected_canonical_state() {
        let (_dir, path, tmp, backup, old, new) =
            replacement_fixture("windows-replace-success-wrong-state");
        fs::rename(&path, &backup).unwrap();
        fs::rename(&tmp, &path).unwrap();
        fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        let owner = windows_user_sid().unwrap();

        let error =
            finish_existing_replace(&tmp, &path, &backup, &owner, &new, Ok(()), |from, to| {
                fs::rename(from, to)
            })
            .unwrap_err();

        assert_eq!(read_at(&path).unwrap(), Some(old.clone()));
        assert_eq!(read_at(&backup).unwrap(), Some(old));
        assert!(error.message().contains(&backup.display().to_string()));
        assert_ne!(read_at(&path).unwrap(), Some(new));
    }

    #[cfg(windows)]
    #[test]
    fn windows_private_layout_survives_atomic_replace() {
        let dir = TestDir::new("windows-private-layout");
        let path = dir.state();
        let _lock = dir.lock();
        let state = sample_state();
        create_at(&path, &state).unwrap();
        let sid = windows_user_sid().unwrap();

        assert_eq!(canonical_windows_sid(&sid).unwrap(), sid);
        assert!(canonical_windows_sid("not-a-sid").is_err());
        validate_windows_state_dir(&dir.0, &sid).unwrap();
        assert!(validate_windows_state_dir(&dir.0, "S-1-1-0").is_err());
        validate_windows_private_layout(&dir.0, &sid, true).unwrap();
        update_at(&path, &state.session.owner, |current| {
            current.session.stop_requested = true;
            Ok(())
        })
        .unwrap();
        validate_windows_private_layout(&dir.0, &sid, true).unwrap();
        let mut names = fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            [WATCHDOG_LOCK_FILE, STATE_FILE, WAKE_LOCK_FILE]
                .map(std::ffi::OsString::from)
                .to_vec()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_extra_file_grant_is_rejected() {
        let dir = TestDir::new("windows-extra-grant");
        let path = dir.state();
        let _lock = dir.lock();
        create_at(&path, &sample_state()).unwrap();
        let sid = windows_user_sid().unwrap();

        set_windows_path_sddl(
            &path,
            &format!("D:P(A;;FA;;;{sid})(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)"),
        )
        .unwrap();
        assert!(validate_windows_private_layout(&dir.0, &sid, true).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_directory_dacl_is_effective_and_protected() {
        let sid = windows_user_sid().unwrap();
        let entries =
            |flags: &str| format!("(A;{flags};FA;;;{sid})(A;{flags};FA;;;SY)(A;{flags};FA;;;BA)");

        assert!(
            validate_windows_directory_sddl(&format!("O:{sid}D:P{}", entries("OICIIO")), &sid,)
                .is_err()
        );
        assert!(
            validate_windows_directory_sddl(&format!("O:{sid}D:{}", entries("OICI")), &sid,)
                .is_err()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_permissive_directory_and_wrong_owner_are_rejected() {
        let path = std::env::temp_dir().join(format!(
            "wake-rs-windows-permissive-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let sid = windows_user_sid().unwrap();
        windows_security::create_directory_with_sddl(
            &path,
            &format!(
                "O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;WD)"
            ),
        )
        .unwrap();
        assert!(validate_windows_private_layout(&path, &sid, false).is_err());
        fs::remove_dir(&path).unwrap();

        let private = TestDir::new("windows-wrong-owner");
        private.lock();
        assert!(validate_windows_private_layout(&private.0, "S-1-1-0", false).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn default_runner_ancestry_passes_helper_layout_validation() {
        use std::os::unix::fs::OpenOptionsExt;

        let parent = default_state_dir()
            .parent()
            .expect("default state directory has a parent")
            .to_path_buf();
        fs::create_dir_all(&parent).unwrap();
        let dir = parent.join(format!(
            ".wake-layout-test-{}-{}",
            std::process::id(),
            new_token().unwrap()
        ));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(
            &dir,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        for name in [WAKE_LOCK_FILE, WATCHDOG_LOCK_FILE, STATE_FILE] {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(dir.join(name))
                .unwrap();
        }

        // SAFETY: getuid and getgid have no preconditions.
        let owner = unsafe { (libc::getuid(), libc::getgid()) };
        let result = validate_macos_private_layout(&dir, owner);
        fs::remove_dir_all(&dir).unwrap();

        result.unwrap();
    }
}
