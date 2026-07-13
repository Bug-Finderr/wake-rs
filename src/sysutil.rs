use crate::error::{AppError, Result};
use crate::run::ProcessIdentity;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

fn refreshed(pid: u32) -> System {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::everything().without_tasks(),
    );
    system
}

struct ProcessObservation {
    identity: ProcessIdentity,
    name: String,
    executable: String,
}

fn process_of(system: &System, pid: u32, native_start: u64) -> Option<ProcessObservation> {
    let process = system.process(Pid::from_u32(pid))?;
    let name = process.name().to_string_lossy().into_owned();
    let executable = process
        .exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.clone());
    Some(ProcessObservation {
        identity: ProcessIdentity { pid, native_start },
        name,
        executable,
    })
}

fn observed_process(pid: u32) -> Result<Option<ProcessObservation>> {
    let Some(before) = native_process_start(pid)? else {
        return Ok(None);
    };
    let system = refreshed(pid);
    let Some(process) = process_of(&system, pid, before) else {
        return Ok(None);
    };
    let Some(after) = native_process_start(pid)? else {
        return Ok(None);
    };
    Ok((before == after).then_some(process))
}

pub fn capture_process(pid: u32) -> Result<ProcessIdentity> {
    let native_start = native_process_start(pid)?
        .ok_or_else(|| AppError::fail(format!("process {pid} is not running")))?;
    let process = ProcessIdentity { pid, native_start };
    if !process.is_valid() {
        return Err(AppError::fail(format!(
            "process {pid} does not expose a stable identity"
        )));
    }
    Ok(process)
}

fn process_identity_with(
    pid: u32,
    read_start: impl FnOnce(u32) -> Result<Option<u64>>,
) -> Result<Option<ProcessIdentity>> {
    read_start(pid).map(|start| start.map(|native_start| ProcessIdentity { pid, native_start }))
}

pub fn process_identity(pid: u32) -> Result<Option<ProcessIdentity>> {
    process_identity_with(pid, native_process_start)
}

pub fn current_process_identity() -> Result<ProcessIdentity> {
    process_identity(std::process::id())?
        .ok_or_else(|| AppError::fail("wake process does not expose a stable identity"))
}

pub fn process_identity_matches(expected: &ProcessIdentity) -> Result<bool> {
    Ok(native_process_start(expected.pid)? == Some(expected.native_start))
}

pub fn wait_process_exit(reference: &ProcessIdentity, within: Duration) -> Result<bool> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !process_identity_matches(reference)? {
            return Ok(true);
        }
        sleep(Duration::from_millis(100));
    }
    process_identity_matches(reference).map(|live| !live)
}

pub fn find_app_process(name: &str) -> Result<Option<ProcessIdentity>> {
    if name.trim().is_empty() {
        return Err(AppError::usage("app/process name cannot be blank"));
    }
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything().without_tasks(),
    );
    let current = Pid::from_u32(current_pid());
    let parent = system.process(current).and_then(|process| process.parent());
    let mut found = None;
    for (&pid, process) in system.processes() {
        if pid == current || Some(pid) == parent {
            continue;
        }
        let command = process
            .exe()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| process.name().to_string_lossy().into_owned());
        if !app_name_matches(name, &process.name().to_string_lossy(), &command)
            || app_name_matches("wake", &process.name().to_string_lossy(), &command)
        {
            continue;
        }
        let Some(candidate) = observed_process(pid.as_u32())? else {
            continue;
        };
        if !app_name_matches(name, &candidate.name, &candidate.executable) {
            continue;
        }
        if !candidate.identity.is_valid() {
            continue;
        }
        if found
            .as_ref()
            .is_none_or(|current: &ProcessIdentity| candidate.identity.pid < current.pid)
        {
            found = Some(candidate.identity);
        }
    }
    Ok(found)
}

#[cfg(target_os = "linux")]
fn native_process_start(pid: u32) -> Result<Option<u64>> {
    let path = format!("/proc/{pid}/stat");
    let Some(stat) = classify_linux_stat_read(pid, &path, std::fs::read(&path))? else {
        return Ok(None);
    };
    parse_linux_process_start(&stat).map(Some)
}

#[cfg(target_os = "linux")]
fn classify_linux_stat_read(
    pid: u32,
    path: &str,
    read: std::io::Result<Vec<u8>>,
) -> Result<Option<Vec<u8>>> {
    match read {
        Ok(stat) => Ok(Some(stat)),
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            Ok(None)
        }
        Err(error) => Err(AppError::fail(format!(
            "could not inspect process {pid} at {path}: {error}"
        ))),
    }
}

#[cfg(target_os = "linux")]
fn parse_linux_process_start(stat: &[u8]) -> Result<u64> {
    let closing_paren = stat
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or_else(|| AppError::fail("process start time is missing from /proc stat"))?;
    std::str::from_utf8(
        stat.get(closing_paren + 1..)
            .ok_or_else(|| AppError::fail("process start time is missing from /proc stat"))?,
    )
    .map_err(|error| AppError::fail(format!("process stat is not UTF-8: {error}")))?
    .split_whitespace()
    .nth(19)
    .ok_or_else(|| AppError::fail("process start time is missing from /proc stat"))?
    .parse()
    .map_err(|error| AppError::fail(format!("invalid process start time in /proc stat: {error}")))
}

#[cfg(target_os = "macos")]
fn native_process_start(pid: u32) -> Result<Option<u64>> {
    let native_pid = i32::try_from(pid)
        .map_err(|_| AppError::fail(format!("process ID {pid} is outside the supported range")))?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // SAFETY: info points to writable storage of the exact size passed to proc_pidinfo. It is
    // read only when the call reports that it initialized the entire proc_bsdinfo value. errno is
    // cleared through the thread-local pointer so a zero return can be classified precisely.
    let read = unsafe {
        *libc::__error() = 0;
        libc::proc_pidinfo(
            native_pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size as i32,
        )
    };
    if read == 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(None)
        } else {
            Err(AppError::fail(format!(
                "could not inspect process {pid}: {error}"
            )))
        };
    }
    if read != size as i32 {
        return Err(AppError::fail(format!(
            "could not inspect process {pid}: proc_pidinfo returned {read} of {size} bytes"
        )));
    }
    // SAFETY: the successful call above initialized all bytes of info.
    let info = unsafe { info.assume_init() };
    let native_start = info
        .pbi_start_tvsec
        .checked_mul(1_000_000)
        .and_then(|seconds| seconds.checked_add(info.pbi_start_tvusec))
        .ok_or_else(|| AppError::fail(format!("process {pid} start time is out of range")))?;
    Ok(Some(native_start))
}

#[cfg(windows)]
fn native_process_start(pid: u32) -> Result<Option<u64>> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, GetLastError, WAIT_FAILED, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        WaitForSingleObject,
    };

    // SAFETY: the process handle is checked before use, every FILETIME points to valid writable
    // storage for the synchronous call, and the owned handle is closed exactly once.
    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        );
        if handle.is_null() {
            let code = GetLastError();
            return if code == ERROR_INVALID_PARAMETER {
                Ok(None)
            } else {
                Err(AppError::fail(format!(
                    "could not inspect process {pid}: {}",
                    std::io::Error::from_raw_os_error(code as i32)
                )))
            };
        }
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let read = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user);
        if read == 0 {
            let error = std::io::Error::last_os_error();
            CloseHandle(handle);
            return Err(AppError::fail(format!(
                "could not read process {pid} start time: {error}"
            )));
        }
        let native_start =
            (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        let wait = WaitForSingleObject(handle, 0);
        let wait_error = (wait == WAIT_FAILED).then(std::io::Error::last_os_error);
        CloseHandle(handle);
        match wait {
            WAIT_OBJECT_0 => Ok(None),
            WAIT_TIMEOUT => Ok(Some(native_start)),
            WAIT_FAILED => Err(AppError::fail(format!(
                "could not inspect process {pid} state: {}",
                wait_error.expect("WAIT_FAILED captures an OS error")
            ))),
            other => Err(AppError::fail(format!(
                "unexpected process {pid} wait result {other}"
            ))),
        }
    }
}

fn app_name_matches(wanted: &str, process_name: &str, executable: &str) -> bool {
    let Some(wanted) = normalized_name(wanted) else {
        return false;
    };
    [process_name, executable]
        .into_iter()
        .filter_map(normalized_name)
        .any(|candidate| candidate == wanted)
}

fn normalized_name(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let basename = std::path::Path::new(value)
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new(value))
        .to_string_lossy()
        .to_lowercase();
    Some(basename.strip_suffix(".exe").unwrap_or(&basename).into())
}

pub fn current_pid() -> u32 {
    std::process::id()
}

pub fn self_exe() -> Result<String> {
    std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| AppError::fail(format!("can't determine executable path: {error}")))
}

pub fn spawn_named(command: &[String]) -> Result<Child> {
    spawn_detached(command).map_err(|error| {
        AppError::fail(format!(
            "couldn't launch {}: {error}",
            command_basename(command)
        ))
    })
}

pub fn spawn_detached(command: &[String]) -> std::io::Result<Child> {
    let (executable, args) = command.split_first().expect("command must be non-empty");
    let mut process = Command::new(executable);
    process
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut process);
    process.spawn()
}

fn command_basename(command: &[String]) -> String {
    command
        .first()
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| std::path::Path::new(value).file_name())
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(windows)]
pub use win::ElevatedChild;

#[cfg(windows)]
pub fn launch_elevated_self(command: &str) -> Result<ElevatedChild> {
    let executable = std::env::current_exe()
        .map_err(|error| AppError::fail(format!("can't determine executable path: {error}")))?;
    win::launch_elevated(&executable, command)
}

#[cfg(windows)]
pub fn run_elevated_self(command: &str) -> Result<u32> {
    launch_elevated_self(command)?.wait()
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
    win::prevent_std_handle_inheritance();
}

#[cfg(windows)]
mod win {
    use crate::error::{AppError, Result};
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_CANCELLED, GetLastError, HANDLE, HANDLE_FLAG_INHERIT,
        SetHandleInformation, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, INFINITE, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };

    pub struct ElevatedChild {
        handle: HANDLE,
    }

    impl ElevatedChild {
        pub fn try_wait(&mut self) -> Result<Option<u32>> {
            match wait_handle(self.handle, 0)? {
                Some(()) => exit_code(self.handle).map(Some),
                None => Ok(None),
            }
        }

        pub fn wait(mut self) -> Result<u32> {
            let result = wait_handle(self.handle, INFINITE)?
                .ok_or_else(|| AppError::fail("elevated helper wait timed out"))
                .and_then(|()| exit_code(self.handle));
            close(&mut self.handle);
            result
        }
    }

    impl Drop for ElevatedChild {
        fn drop(&mut self) {
            close(&mut self.handle);
        }
    }

    pub fn launch_elevated(executable: &Path, command: &str) -> Result<ElevatedChild> {
        let verb = wide(OsStr::new("runas"));
        let file = wide(executable.as_os_str());
        let parameters = wide(OsStr::new(command));
        // SAFETY: info is initialized with buffers that outlive ShellExecuteExW; the returned
        // process handle is transferred to ElevatedChild.
        unsafe {
            let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
            info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_NOASYNC | SEE_MASK_NOCLOSEPROCESS;
            info.lpVerb = verb.as_ptr();
            info.lpFile = file.as_ptr();
            info.lpParameters = parameters.as_ptr();
            info.nShow = 0;
            if ShellExecuteExW(&mut info) == 0 {
                return if GetLastError() == ERROR_CANCELLED {
                    Err(AppError::fail(
                        "elevation was cancelled; --even-lid needs administrator rights",
                    ))
                } else {
                    Err(os_error("failed to launch elevated helper"))
                };
            }
            if info.hProcess.is_null() {
                return Err(AppError::fail("elevated helper did not return a process"));
            }
            Ok(ElevatedChild {
                handle: info.hProcess,
            })
        }
    }

    pub(super) fn decode_wait(status: u32) -> std::io::Result<Option<()>> {
        match status {
            WAIT_OBJECT_0 => Ok(Some(())),
            WAIT_TIMEOUT => Ok(None),
            WAIT_FAILED => Err(std::io::Error::last_os_error()),
            other => Err(std::io::Error::other(format!(
                "unexpected process wait result {other}"
            ))),
        }
    }

    fn wait_handle(handle: HANDLE, timeout: u32) -> Result<Option<()>> {
        // SAFETY: handle is a live owned or borrowed process handle for this synchronous wait.
        decode_wait(unsafe { WaitForSingleObject(handle, timeout) }).map_err(AppError::from)
    }

    fn exit_code(handle: HANDLE) -> Result<u32> {
        let mut code = 0;
        // SAFETY: called only after the process handle was signaled; code is writable.
        if unsafe { GetExitCodeProcess(handle, &mut code) } == 0 {
            Err(os_error("could not read elevated helper exit code"))
        } else {
            Ok(code)
        }
    }

    fn close(handle: &mut HANDLE) {
        if !handle.is_null() {
            // SAFETY: closes the owned handle once and immediately marks it null.
            unsafe { CloseHandle(*handle) };
            *handle = std::ptr::null_mut();
        }
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn os_error(action: &str) -> AppError {
        AppError::fail(format!("{action}: {}", std::io::Error::last_os_error()))
    }

    pub fn prevent_std_handle_inheritance() {
        for number in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: invalid standard handles are rejected before SetHandleInformation.
            unsafe {
                let handle = GetStdHandle(number);
                if !handle.is_null() && handle as isize != -1 {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
    }
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn elevated_wait_state_decodes_only_documented_results() {
        use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};

        assert_eq!(win::decode_wait(WAIT_OBJECT_0).unwrap(), Some(()));
        assert_eq!(win::decode_wait(WAIT_TIMEOUT).unwrap(), None);
        assert!(win::decode_wait(WAIT_FAILED).is_err());
        assert!(win::decode_wait(7).is_err());
    }

    #[test]
    fn app_name_matching_is_exact_and_normalized() {
        let cases = [
            ("Awake", "Awake.exe", "C:/bin/Awake.exe", true),
            ("awake.exe", "Awake", "C:/bin/Awake.exe", true),
            ("me", "someapp", "/usr/bin/someapp", false),
            ("visual", "Code", "Visual Studio Code", false),
            (" ", "Code", "C:/Code.exe", false),
        ];
        for (wanted, name, executable, expected) in cases {
            assert_eq!(app_name_matches(wanted, name, executable), expected);
        }
    }

    #[test]
    fn current_process_exposes_a_stable_native_identity() {
        let first = capture_process(current_pid()).unwrap();
        let second = capture_process(current_pid()).unwrap();

        assert!(first.is_valid());
        assert_eq!(first, second);
    }

    #[test]
    fn native_identity_matches_the_current_process() {
        let identity = current_process_identity().unwrap();

        assert_eq!(identity.pid, std::process::id());
        assert!(identity.is_valid());
        assert!(process_identity_matches(&identity).unwrap());
    }

    #[test]
    fn native_identity_reader_errors_are_not_treated_as_absence() {
        let error = process_identity_with(42, |_| {
            Err(AppError::fail("native process inspection failed"))
        })
        .unwrap_err();

        assert_eq!(error.message(), "native process inspection failed");
    }

    #[cfg(windows)]
    #[test]
    fn retained_terminated_process_object_does_not_match() {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sysutil::tests::retained_process_identity_test_child",
            ])
            .env("WAKE_TEST_RETAINED_PROCESS_CHILD", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let identity = process_identity(child.id()).unwrap().unwrap();
        assert!(process_identity_matches(&identity).unwrap());

        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());

        assert!(!process_identity_matches(&identity).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn retained_process_identity_test_child() {
        if std::env::var_os("WAKE_TEST_RETAINED_PROCESS_CHILD").is_none() {
            return;
        }
        use std::io::Read as _;
        let mut byte = [0_u8];
        std::io::stdin().read_exact(&mut byte).ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn malformed_linux_process_stat_is_an_error() {
        let error = parse_linux_process_start(b"42 (wake) S malformed").unwrap_err();

        assert!(error.message().contains("process start time"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn disappearing_proc_read_is_confirmed_absence() {
        let result = classify_linux_stat_read(
            42,
            "/proc/42/stat",
            Err(std::io::Error::from_raw_os_error(libc::ESRCH)),
        )
        .unwrap();

        assert!(result.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_identity_uses_native_start_ticks() {
        let process = process_identity(current_pid())
            .unwrap()
            .expect("current process");
        let stat = std::fs::read(format!("/proc/{}/stat", current_pid())).unwrap();
        let closing_paren = stat.iter().rposition(|byte| *byte == b')').unwrap();
        let expected = std::str::from_utf8(&stat[closing_paren + 1..])
            .unwrap()
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse::<u64>()
            .unwrap();

        assert_eq!(process.native_start, expected);
    }
}
