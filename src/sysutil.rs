//! Process discovery, identity, termination, and detached spawning.

use crate::error::{AppError, Result};
use crate::session::Session;
#[cfg(not(windows))]
use std::process::{Child, Command, Stdio};
#[cfg(not(windows))]
use std::thread::sleep;
#[cfg(not(windows))]
use std::time::{Duration, Instant};
use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, System};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub start: u64,
    pub command: String,
}

#[cfg(not(windows))]
fn refreshed(pid: u32) -> System {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::everything(),
    );
    sys
}

#[cfg(not(windows))]
fn process_exists(pid: u32) -> bool {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(Pid::from_u32(pid)).is_some()
}

#[cfg(not(windows))]
fn identity_of(sys: &System, pid: u32) -> Option<Identity> {
    let process = sys.process(Pid::from_u32(pid))?;
    let command = process
        .exe()
        .map(|exe| exe.to_string_lossy().into_owned())
        .unwrap_or_else(|| process.name().to_string_lossy().into_owned());
    Some(Identity {
        start: process.start_time(),
        command,
    })
}

pub fn is_alive(pid: u32) -> bool {
    #[cfg(windows)]
    return win::open_process(pid, false)
        .ok()
        .flatten()
        .is_some_and(|process| process.is_running().unwrap_or(false));
    #[cfg(not(windows))]
    process_exists(pid)
}

#[cfg(not(windows))]
pub fn live_identity(pid: u32) -> Option<Identity> {
    identity_of(&refreshed(pid), pid)
}

#[cfg(not(windows))]
pub fn capture_identity(pid: u32) -> Result<Identity> {
    live_identity(pid).ok_or_else(|| AppError::fail(format!("process {pid} is not running")))
}

pub fn current_pid() -> u32 {
    std::process::id()
}

pub fn parent_pid() -> Option<u32> {
    let me = current_pid();
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(me)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    system
        .process(Pid::from_u32(me))
        .and_then(|process| process.parent())
        .map(|pid| pid.as_u32())
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum AppMatch {
    Exact,
    Substring,
}

pub fn find_app_pid(raw: &str) -> Result<Option<u32>> {
    let query = raw.trim().to_lowercase();
    if query.is_empty() {
        return Err(AppError::usage("app/process name cannot be empty"));
    }
    let self_pid = current_pid();
    let parent_pid = parent_pid();
    let system = System::new_all();
    Ok(choose_app_pid(system.processes().iter().filter_map(
        |(pid, process)| {
            let pid = pid.as_u32();
            if pid == self_pid || Some(pid) == parent_pid || is_wake_process(process) {
                return None;
            }
            app_match(&query, process).map(|rank| (rank, pid))
        },
    )))
}

fn choose_app_pid(matches: impl IntoIterator<Item = (AppMatch, u32)>) -> Option<u32> {
    matches
        .into_iter()
        .min_by_key(|&(rank, pid)| (rank, pid))
        .map(|(_, pid)| pid)
}

fn app_match(query: &str, process: &Process) -> Option<AppMatch> {
    let name = process.name().to_string_lossy().to_lowercase();
    let exe = process
        .exe()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match_names(query, &name, &exe)
}

fn match_names(query: &str, process_name: &str, exe_basename: &str) -> Option<AppMatch> {
    let query = query.to_lowercase();
    let process_name = process_name.to_lowercase();
    let exe_basename = exe_basename.to_lowercase();
    if process_name == query || exe_basename == query {
        Some(AppMatch::Exact)
    } else if process_name.contains(&query) || exe_basename.contains(&query) {
        Some(AppMatch::Substring)
    } else {
        None
    }
}

fn is_wake_process(process: &Process) -> bool {
    let name = process.name().to_string_lossy();
    let exe = process
        .exe()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy());
    is_wake_name(&name) || exe.as_deref().is_some_and(is_wake_name)
}

fn is_wake_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("wake") || name.eq_ignore_ascii_case("wake.exe")
}

#[cfg(not(windows))]
pub fn terminate_session(session: &Session) -> Result<()> {
    if !signal_session(session, false)? || wait_gone(session.pid, Duration::from_secs(5)) {
        return Ok(());
    }
    signal_session(session, true)?;
    wait_gone(session.pid, Duration::from_secs(1));
    Ok(())
}

#[cfg(not(windows))]
fn signal_session(session: &Session, force: bool) -> Result<bool> {
    use sysinfo::Signal;
    let system = refreshed(session.pid);
    let Some(process) = system.process(Pid::from_u32(session.pid)) else {
        return Ok(false);
    };
    let Some(identity) = identity_of(&system, session.pid) else {
        return Ok(false);
    };
    if !session.matches_identity(&identity) {
        return Err(AppError::fail(format!(
            "session process {} changed identity; refusing to terminate it",
            session.pid
        )));
    }
    if force || process.kill_with(Signal::Term).is_none() {
        process.kill();
    }
    Ok(true)
}

#[cfg(not(windows))]
fn wait_gone(pid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !is_alive(pid) {
            return true;
        }
        sleep(Duration::from_millis(100));
    }
    !is_alive(pid)
}

#[cfg(not(windows))]
pub fn verify_child_alive(pid: u32) -> bool {
    sleep(Duration::from_millis(300));
    is_alive(pid)
}

#[cfg(not(windows))]
pub fn require_child_alive(pid: u32, cmd: &[String]) -> Result<()> {
    if verify_child_alive(pid) {
        Ok(())
    } else {
        Err(AppError::fail(format!(
            "keep-awake process exited immediately ({}); see platform requirements",
            command_basename(cmd)
        )))
    }
}

#[cfg(not(windows))]
fn command_basename(cmd: &[String]) -> String {
    match cmd.first() {
        Some(exe) if !exe.trim().is_empty() => std::path::Path::new(exe)
            .file_name()
            .map(|file| file.to_string_lossy().into_owned())
            .unwrap_or_else(|| exe.clone()),
        _ => "unknown".to_string(),
    }
}

pub fn self_exe() -> Result<String> {
    std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| AppError::fail(format!("can't determine executable path: {error}")))
}

#[cfg(not(windows))]
pub fn spawn_named(cmd: &[String]) -> Result<Child> {
    spawn_detached(cmd).map_err(|error| {
        AppError::fail(format!(
            "couldn't launch {}: {error}; check it is installed and on PATH",
            command_basename(cmd)
        ))
    })
}

#[cfg(not(windows))]
pub fn spawn_detached(cmd: &[String]) -> std::io::Result<Child> {
    let (exe, args) = cmd.split_first().expect("command must be non-empty");
    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command);
    command.spawn()
}

#[cfg(not(windows))]
pub fn spawn_supervised_child(cmd: &[String]) -> std::io::Result<Child> {
    spawn_detached(cmd)
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
pub use win::{
    ProcessHandle, current_identity, open_exact_process, open_session_process,
    spawn_elevated_guardian, spawn_worker,
};

#[cfg(windows)]
mod win {
    use super::{Identity, Session};
    use crate::error::{AppError, Result};
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_CANCELLED, ERROR_INVALID_PARAMETER, FILETIME, GetLastError, HANDLE,
        WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CreateProcessW, GetCurrentProcess, GetProcessId, GetProcessTimes,
        OpenProcess, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        PROCESS_TERMINATE, QueryFullProcessImageNameW, STARTUPINFOW, TerminateProcess,
        WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };

    pub struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn new(handle: HANDLE) -> Result<Self> {
            if handle.is_null() {
                Err(last_error("received a null process handle"))
            } else {
                Ok(Self(handle))
            }
        }

        fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: `self.0` is an owned process handle and is closed exactly once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub struct ProcessHandle {
        handle: OwnedHandle,
        pid: u32,
        identity: Identity,
    }

    impl ProcessHandle {
        fn from_owned(handle: OwnedHandle) -> Result<Self> {
            // SAFETY: `handle` remains valid throughout both queries.
            let pid = unsafe { GetProcessId(handle.raw()) };
            if pid == 0 {
                return Err(last_error("could not read process id"));
            }
            let identity = identity_from_raw(handle.raw())?;
            Ok(Self {
                handle,
                pid,
                identity,
            })
        }

        pub fn pid(&self) -> u32 {
            self.pid
        }

        pub fn identity(&self) -> &Identity {
            &self.identity
        }

        pub fn is_running(&self) -> Result<bool> {
            match wait_raw(self.handle.raw(), 0)? {
                WAIT_TIMEOUT => Ok(true),
                WAIT_OBJECT_0 => Ok(false),
                _ => unreachable!("wait_raw accepts only successful wait results"),
            }
        }

        pub fn wait(&self, timeout: Duration) -> Result<bool> {
            match wait_raw(self.handle.raw(), millis(timeout))? {
                WAIT_OBJECT_0 => Ok(true),
                WAIT_TIMEOUT => Ok(false),
                _ => unreachable!("wait_raw accepts only successful wait results"),
            }
        }

        pub fn terminate_and_wait(&self, timeout: Duration) -> Result<()> {
            if !self.is_running()? {
                return Ok(());
            }
            // SAFETY: The retained handle has PROCESS_TERMINATE access.
            if unsafe { TerminateProcess(self.handle.raw(), 1) } == 0 {
                return Err(last_error("could not terminate the worker"));
            }
            if self.wait(timeout)? {
                Ok(())
            } else {
                Err(AppError::fail(format!(
                    "worker {} did not exit after termination",
                    self.pid
                )))
            }
        }
    }

    pub fn open_process(pid: u32, terminate: bool) -> Result<Option<ProcessHandle>> {
        let access = PROCESS_QUERY_LIMITED_INFORMATION
            | PROCESS_SYNCHRONIZE
            | if terminate { PROCESS_TERMINATE } else { 0 };
        // SAFETY: OpenProcess returns a new owned handle on success.
        let raw = unsafe { OpenProcess(access, 0, pid) };
        if raw.is_null() {
            // SAFETY: This immediately follows the failed OpenProcess call.
            let code = unsafe { GetLastError() };
            if code == ERROR_INVALID_PARAMETER {
                return Ok(None);
            }
            return Err(AppError::fail(format!(
                "could not open process {pid} (error {code})"
            )));
        }
        ProcessHandle::from_owned(OwnedHandle::new(raw)?).map(Some)
    }

    pub fn open_exact_process(
        pid: u32,
        creation_time: u64,
        terminate: bool,
    ) -> Result<Option<ProcessHandle>> {
        let Some(process) = open_process(pid, terminate)? else {
            return Ok(None);
        };
        if process.identity.start != creation_time {
            return Ok(None);
        }
        Ok(Some(process))
    }

    pub fn open_session_process(
        session: &Session,
        terminate: bool,
    ) -> Result<Option<ProcessHandle>> {
        let Some(process) = open_process(session.pid, terminate)? else {
            return Ok(None);
        };
        if !session.matches_identity(process.identity()) {
            return Err(AppError::fail(format!(
                "session process {} changed identity; refusing to use it",
                session.pid
            )));
        }
        Ok(Some(process))
    }

    pub fn current_identity() -> Result<Identity> {
        // SAFETY: GetCurrentProcess returns a pseudo-handle valid in this process.
        identity_from_raw(unsafe { GetCurrentProcess() })
    }

    fn identity_from_raw(handle: HANDLE) -> Result<Identity> {
        Ok(Identity {
            start: creation_time(handle)?,
            command: image_name(handle)?,
        })
    }

    fn creation_time(handle: HANDLE) -> Result<u64> {
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: All FILETIME output pointers are valid and `handle` is retained by the caller.
        if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } == 0
        {
            return Err(last_error("could not read process creation time"));
        }
        Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
    }

    fn image_name(handle: HANDLE) -> Result<String> {
        let mut buffer = vec![0u16; 32_768];
        let mut length = buffer.len() as u32;
        // SAFETY: `buffer` is writable for `length` UTF-16 code units and the handle is retained.
        if unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length) } == 0 {
            return Err(last_error("could not read process executable path"));
        }
        Ok(String::from_utf16_lossy(&buffer[..length as usize]))
    }

    fn wait_raw(handle: HANDLE, timeout_ms: u32) -> Result<u32> {
        // SAFETY: `handle` is retained for the duration of the wait.
        let result = unsafe { WaitForSingleObject(handle, timeout_ms) };
        if result == WAIT_FAILED {
            Err(last_error("process wait failed"))
        } else if result == WAIT_OBJECT_0 || result == WAIT_TIMEOUT {
            Ok(result)
        } else {
            Err(AppError::fail(format!(
                "process wait returned unexpected status {result}"
            )))
        }
    }

    fn millis(duration: Duration) -> u32 {
        duration.as_millis().min(u128::from(u32::MAX - 1)) as u32
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    pub fn quote_windows_arg(arg: &str) -> String {
        if !arg.is_empty() && !arg.chars().any(|ch| ch.is_whitespace() || ch == '"') {
            return arg.to_string();
        }
        let mut quoted = String::from("\"");
        let mut backslashes = 0;
        for ch in arg.chars() {
            if ch == '\\' {
                backslashes += 1;
            } else if ch == '"' {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            } else {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(ch);
                backslashes = 0;
            }
        }
        quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
        quoted.push('"');
        quoted
    }

    pub fn spawn_worker(command: &[String]) -> Result<ProcessHandle> {
        let (exe, _) = command
            .split_first()
            .ok_or_else(|| AppError::fail("worker command is empty"))?;
        let exe = wide(OsStr::new(exe));
        let command_line = command
            .iter()
            .map(|arg| quote_windows_arg(arg))
            .collect::<Vec<_>>()
            .join(" ");
        let mut command_line = wide(OsStr::new(&command_line));
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            ..STARTUPINFOW::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        // SAFETY: The application and mutable command-line buffers outlive CreateProcessW. Handle
        // inheritance is disabled, and both returned handles are owned on success.
        if unsafe {
            CreateProcessW(
                exe.as_ptr(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_NO_WINDOW,
                std::ptr::null(),
                std::ptr::null(),
                &startup,
                &mut process,
            )
        } == 0
        {
            return Err(last_error("could not launch the Windows worker"));
        }
        // SAFETY: hThread is a distinct owned handle that wake does not need.
        unsafe {
            CloseHandle(process.hThread);
        }
        ProcessHandle::from_owned(OwnedHandle::new(process.hProcess)?)
    }

    pub fn spawn_elevated_guardian(request: &Path) -> Result<ProcessHandle> {
        if !request.is_absolute() {
            return Err(AppError::fail("guardian request path must be absolute"));
        }
        let exe = std::env::current_exe()
            .map_err(|error| AppError::fail(format!("can't determine executable path: {error}")))?;
        let params = format!(
            "{} {}",
            quote_windows_arg("__guard_windows__"),
            quote_windows_arg(&request.to_string_lossy())
        );
        let verb = wide(OsStr::new("runas"));
        let file = wide(exe.as_os_str());
        let params = wide(OsStr::new(&params));

        // SAFETY: The structure and wide strings remain valid through ShellExecuteExW. hProcess is
        // transferred into OwnedHandle on success.
        unsafe {
            let mut info = SHELLEXECUTEINFOW {
                cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
                fMask: SEE_MASK_NOCLOSEPROCESS,
                lpVerb: verb.as_ptr(),
                lpFile: file.as_ptr(),
                lpParameters: params.as_ptr(),
                nShow: 0,
                ..SHELLEXECUTEINFOW::default()
            };
            if ShellExecuteExW(&mut info) == 0 {
                let code = GetLastError();
                if code == ERROR_CANCELLED {
                    return Err(AppError::fail(
                        "elevation was cancelled; --even-lid was not enabled",
                    ));
                }
                return Err(AppError::fail(format!(
                    "could not launch the elevated guardian (error {code})"
                )));
            }
            ProcessHandle::from_owned(OwnedHandle::new(info.hProcess)?)
        }
    }

    fn last_error(message: &str) -> AppError {
        // SAFETY: Reads the calling thread's last-error value.
        let code = unsafe { GetLastError() };
        AppError::fail(format!("{message} (error {code})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_app_query_is_usage() {
        assert!(matches!(find_app_pid("  \t"), Err(AppError::Usage(_))));
    }

    #[test]
    fn app_match_prefers_exact_then_literal_substring() {
        assert_eq!(
            match_names("SLACK.EXE", "slack", "slack.exe"),
            Some(AppMatch::Exact)
        );
        assert_eq!(
            match_names("app[1]", "my-app[1]-helper", "helper"),
            Some(AppMatch::Substring)
        );
        assert_eq!(match_names("app.", "appx", "other"), None);
    }

    #[test]
    fn app_selection_prefers_exact_then_lowest_pid() {
        assert_eq!(
            choose_app_pid([
                (AppMatch::Substring, 1),
                (AppMatch::Exact, 9),
                (AppMatch::Exact, 3),
            ]),
            Some(3)
        );
    }

    #[test]
    fn wake_exclusion_is_exact() {
        assert!(is_wake_name("WAKE.EXE"));
        assert!(!is_wake_name("wake-helper.exe"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_argv_quoting_handles_spaces_quotes_and_trailing_slashes() {
        assert_eq!(win::quote_windows_arg("plain"), "plain");
        assert_eq!(win::quote_windows_arg(""), "\"\"");
        assert_eq!(win::quote_windows_arg("two words"), "\"two words\"");
        assert_eq!(win::quote_windows_arg("a\\\"b"), "\"a\\\\\\\"b\"");
        assert_eq!(
            win::quote_windows_arg("C:\\dir with space\\"),
            "\"C:\\dir with space\\\\\""
        );
    }
}
