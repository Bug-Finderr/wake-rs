//! Process helpers backed by `sysinfo`: liveness, identity capture/match, termination,
//! detached spawning, and locating our own executable.

use crate::error::{AppError, Result};
use crate::run::ProcessRef;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System};

/// Identity fingerprint of a process: start time (epoch seconds), executable path, full command line.
pub struct Identity {
    pub start: u64,
    pub command: String,
    pub command_line: String,
}

fn refreshed(pid: u32) -> System {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::everything().without_tasks(),
    );
    sys
}

/// Existence-only refresh: `nothing()` skips exe/cmd/cwd/user/disk/mem, but the process is still
/// found by the underlying scan, so `.process(pid).is_some()` reflects liveness. Used on the hot
/// per-second liveness path where the heavier `everything()` fields are not read.
fn process_exists(pid: u32) -> bool {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    sys.process(Pid::from_u32(pid)).is_some()
}

fn identity_of(sys: &System, pid: u32) -> Option<Identity> {
    let p = sys.process(Pid::from_u32(pid))?;
    let command = p
        .exe()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.name().to_string_lossy().into_owned());
    let command_line = {
        let joined = p
            .cmd()
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        if joined.is_empty() {
            command.clone()
        } else {
            joined
        }
    };
    Some(Identity {
        start: p.start_time(),
        command,
        command_line,
    })
}

pub fn is_alive(pid: u32) -> bool {
    process_exists(pid)
}

/// Live identity of a running pid, or None if it is gone.
pub fn live_identity(pid: u32) -> Option<Identity> {
    let sys = refreshed(pid);
    identity_of(&sys, pid)
}

pub fn capture_identity(pid: u32) -> Result<Identity> {
    live_identity(pid).ok_or_else(|| AppError::fail(format!("process {pid} is not running")))
}

pub fn capture_process(pid: u32) -> Result<ProcessRef> {
    let identity = capture_identity(pid)?;
    Ok(ProcessRef {
        pid,
        start: identity.start,
        command: identity.command,
    })
}

pub fn process_matches(reference: &ProcessRef) -> bool {
    live_identity(reference.pid)
        .is_some_and(|identity| reference.matches(reference.pid, identity.start, &identity.command))
}

pub fn find_app_process(name: &str) -> Result<Option<ProcessRef>> {
    if name.trim().is_empty() {
        return Err(AppError::usage("app/process name cannot be blank"));
    }
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything().without_tasks(),
    );
    let current = current_pid();
    let parent = parent_pid();
    let mut matches = system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            let pid = pid.as_u32();
            if pid == current || Some(pid) == parent {
                return None;
            }
            let command = process
                .exe()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|| process.name().to_string_lossy().into_owned());
            let rank = app_match_rank(name, &process.name().to_string_lossy(), &command)?;
            let wake = std::path::Path::new(&command)
                .file_stem()
                .is_some_and(|stem| stem.eq_ignore_ascii_case("wake"));
            (!wake).then_some((rank, pid, process.start_time(), command))
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|(rank, pid, ..)| (*rank, *pid));
    Ok(matches
        .into_iter()
        .next()
        .map(|(_, pid, start, command)| ProcessRef {
            pid,
            start,
            command,
        }))
}

fn app_match_rank(needle: &str, process_name: &str, executable: &str) -> Option<u8> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    let wanted = needle.strip_suffix(".exe").unwrap_or(&needle);
    let name = process_name.trim().to_lowercase();
    let simple = name.strip_suffix(".exe").unwrap_or(&name);
    let executable = executable.trim().to_lowercase();
    let executable = executable
        .find(".exe")
        .map_or(executable.as_str(), |end| &executable[..end + 4]);
    let executable = std::path::Path::new(executable)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    if simple == wanted {
        Some(0)
    } else if simple.contains(wanted) || executable.contains(wanted) {
        Some(1)
    } else {
        None
    }
}

pub fn current_pid() -> u32 {
    std::process::id()
}

pub fn parent_pid() -> Option<u32> {
    let me = current_pid();
    refreshed(me)
        .process(Pid::from_u32(me))
        .and_then(|p| p.parent())
        .map(|p| p.as_u32())
}

pub fn terminate(pid: u32) {
    if let Ok(process) = capture_process(pid) {
        terminate_exact(&process);
    }
}

pub fn terminate_exact(reference: &ProcessRef) -> bool {
    if !process_matches(reference) {
        return false;
    }
    {
        let system = refreshed(reference.pid);
        let Some(process) = system.process(Pid::from_u32(reference.pid)) else {
            return false;
        };
        let Some(identity) = identity_of(&system, reference.pid) else {
            return false;
        };
        if !reference.matches(reference.pid, identity.start, &identity.command) {
            return false;
        }
        if process.kill_with(Signal::Term).is_none() {
            process.kill();
        }
    }
    if wait_identity_gone(reference, Duration::from_secs(5)) {
        return true;
    }
    let system = refreshed(reference.pid);
    if let Some(identity) = identity_of(&system, reference.pid)
        && reference.matches(reference.pid, identity.start, &identity.command)
        && let Some(process) = system.process(Pid::from_u32(reference.pid))
    {
        process.kill();
    }
    wait_identity_gone(reference, Duration::from_secs(1))
}

fn wait_identity_gone(reference: &ProcessRef, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !process_matches(reference) {
            return true;
        }
        sleep(Duration::from_millis(100));
    }
    !process_matches(reference)
}

/// True if the child is still alive after a short settle delay.
pub fn verify_child_alive(pid: u32) -> bool {
    sleep(Duration::from_millis(300));
    is_alive(pid)
}

pub fn require_child_alive(pid: u32, cmd: &[String]) -> Result<()> {
    if verify_child_alive(pid) {
        return Ok(());
    }
    Err(AppError::fail(format!(
        "keep-awake process exited immediately ({}); see platform requirements",
        command_basename(cmd)
    )))
}

fn command_basename(cmd: &[String]) -> String {
    match cmd.first() {
        Some(exe) if !exe.trim().is_empty() => std::path::Path::new(exe)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| exe.clone()),
        _ => "unknown".to_string(),
    }
}

/// Absolute path to our own executable, used to relaunch detached supervisors.
pub fn self_exe() -> Result<String> {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| AppError::fail(format!("can't determine executable path: {e}")))
}

/// Like [`spawn_detached`], but on failure names the program basename and adds a hint, so a bare
/// `Access is denied. (os error 5)` becomes `couldn't launch <prog>: <err> ...`.
pub fn spawn_named(cmd: &[String]) -> Result<Child> {
    spawn_detached(cmd).map_err(|e| {
        AppError::fail(format!(
            "couldn't launch {}: {e}; check it is installed and on PATH",
            command_basename(cmd)
        ))
    })
}

/// Spawn a fully detached child with null stdio. The returned `Child` is not waited on by callers
/// that fire-and-forget; dropping it does not kill the child.
pub fn spawn_detached(cmd: &[String]) -> std::io::Result<Child> {
    let (exe, args) = cmd.split_first().expect("command must be non-empty");
    let mut c = Command::new(exe);
    c.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut c);
    c.spawn()
}

/// Spawn a keep-awake child for a supervisor, tied to the supervisor's lifetime so it cannot
/// outlive it. On Windows a kill-on-close Job Object kills the child even if the supervisor is
/// force-terminated (`wake stop` uses TerminateProcess, which runs no cleanup). On Unix the
/// supervisor's SIGTERM/SIGINT handler tears the child down instead.
pub fn spawn_supervised_child(cmd: &[String]) -> std::io::Result<Child> {
    let child = spawn_detached(cmd)?;
    #[cfg(windows)]
    win::tie_child_to_job(&child);
    Ok(child)
}

/// Relaunch our own executable elevated (UAC `runas`) with `args`, wait for it, and return its exit
/// code. Used on Windows for `--even-lid`, where changing the power-plan lid action needs admin.
#[cfg(windows)]
pub fn run_elevated_self(args: &[&str]) -> Result<i32> {
    let exe = std::env::current_exe()
        .map_err(|e| AppError::fail(format!("can't determine executable path: {e}")))?;
    win::shell_execute_runas(&exe, args)
}

#[cfg(windows)]
fn detach(c: &mut Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW gives the child its own hidden console, decoupled from ours; it survives
    // our exit by default. (DETACHED_PROCESS makes PowerShell exit immediately, so we avoid it.)
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    c.creation_flags(CREATE_NO_WINDOW);
    // Stop the detached child from inheriting our std handles. If our stdout is a captured pipe
    // (CI, scripts), an inherited copy in the long-lived child would keep that pipe open forever
    // and hang the caller waiting on EOF, even though we exit promptly.
    win::prevent_std_handle_inheritance();
}

#[cfg(windows)]
mod win {
    use crate::error::{AppError, Result};
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::process::Child;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_CANCELLED, GetLastError, HANDLE, HANDLE_FLAG_INHERIT,
        SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, INFINITE, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };

    /// NUL-terminated UTF-16 buffer for a Win32 wide-string argument.
    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Run `exe` elevated via the shell `runas` verb (triggers a UAC prompt), wait for it to exit,
    /// and return its exit code. The child window is hidden.
    pub fn shell_execute_runas(exe: &Path, args: &[&str]) -> Result<i32> {
        // Quote each argument so spaces are preserved; the args we pass are our own literals/numbers.
        let params: String = args
            .iter()
            .map(|a| format!("\"{a}\""))
            .collect::<Vec<_>>()
            .join(" ");
        let verb = wide(OsStr::new("runas"));
        let file = wide(exe.as_os_str());
        let params_w = wide(OsStr::new(&params));

        // SAFETY: FFI into shell32/kernel32. `info` is zeroed then fully initialized; the wide-string
        // buffers outlive the `ShellExecuteExW` call. On success `hProcess` is an owned handle we wait
        // on and then close.
        unsafe {
            let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
            info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_NOCLOSEPROCESS;
            info.lpVerb = verb.as_ptr();
            info.lpFile = file.as_ptr();
            info.lpParameters = params_w.as_ptr();
            info.nShow = 0; // SW_HIDE

            if ShellExecuteExW(&mut info) == 0 {
                if GetLastError() == ERROR_CANCELLED {
                    return Err(AppError::fail(
                        "elevation was cancelled; --even-lid needs administrator rights to change the lid action",
                    ));
                }
                return Err(AppError::fail("failed to launch the elevated helper"));
            }
            if info.hProcess.is_null() {
                return Err(AppError::fail("elevated helper did not start"));
            }
            WaitForSingleObject(info.hProcess, INFINITE);
            let mut code: u32 = 0;
            let got = GetExitCodeProcess(info.hProcess, &mut code);
            CloseHandle(info.hProcess);
            if got == 0 {
                return Err(AppError::fail(
                    "could not read the elevated helper's exit code",
                ));
            }
            Ok(code as i32)
        }
    }

    pub fn prevent_std_handle_inheritance() {
        for n in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: each constant names a process standard handle; invalid handles are rejected
            // before SetHandleInformation.
            unsafe {
                let h = GetStdHandle(n);
                if !h.is_null() && h as isize != -1 {
                    SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
    }

    /// Best-effort: put `child` in a kill-on-close Job Object and intentionally leak the job handle,
    /// so the OS kills the child when this process exits for any reason. If anything fails we fall
    /// back to the supervisor's normal-exit cleanup.
    pub fn tie_child_to_job(child: &Child) {
        // SAFETY: the created handle is owned here; `child` supplies a valid process handle. All
        // failure paths close the job, while success deliberately retains it for process lifetime.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let sized = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if sized == 0 || AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) == 0 {
                CloseHandle(job);
            }
            // On success `job` is leaked on purpose: the handle must stay open for our lifetime so
            // kill-on-close fires when we die. Closing it now would kill the child immediately.
        }
    }
}

#[cfg(unix)]
fn detach(c: &mut Command) {
    use std::os::unix::process::CommandExt;
    // New process group so terminal SIGINT/SIGTSTP don't reach the detached child.
    c.process_group(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_name_matching_prefers_exact_and_rejects_blank() {
        assert_eq!(app_match_rank("Code", "Code.exe", "C:/Code.exe"), Some(0));
        assert_eq!(app_match_rank("code.exe", "Code", "C:/Code.exe"), Some(0));
        assert_eq!(
            app_match_rank("visual", "Code", "Visual Studio Code"),
            Some(1)
        );
        assert_eq!(app_match_rank("report", "Code", "C:/Code.exe report"), None);
        assert_eq!(app_match_rank("other", "Code", "C:/Code.exe"), None);
        assert_eq!(app_match_rank(" ", "Code", "C:/Code.exe"), None);
    }
}
