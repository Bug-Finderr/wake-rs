//! Process helpers backed by `sysinfo`: liveness, identity capture/match, termination,
//! detached spawning, and locating our own executable.

use crate::error::{AppError, Result};
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
        ProcessRefreshKind::everything(),
    );
    sys
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
    refreshed(pid).process(Pid::from_u32(pid)).is_some()
}

/// Live identity of a running pid, or None if it is gone.
pub fn live_identity(pid: u32) -> Option<Identity> {
    let sys = refreshed(pid);
    identity_of(&sys, pid)
}

/// Capture identity, erroring (like the reference) if the process or its start time is unreadable.
pub fn capture_identity(pid: u32) -> Result<Identity> {
    live_identity(pid).ok_or_else(|| AppError::fail(format!("process {pid} is not running")))
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

/// SIGTERM then SIGKILL (Unix) / TerminateProcess (Windows), matching the reference's grace window.
pub fn terminate(pid: u32) {
    {
        let sys = refreshed(pid);
        match sys.process(Pid::from_u32(pid)) {
            Some(p) => {
                if p.kill_with(Signal::Term).is_none() {
                    p.kill();
                }
            }
            None => return,
        }
    }
    if wait_gone(pid, Duration::from_secs(5)) {
        return;
    }
    if let Some(p) = refreshed(pid).process(Pid::from_u32(pid)) {
        p.kill();
    }
    wait_gone(pid, Duration::from_secs(1));
}

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

/// True if the child is still alive after a short settle delay.
pub fn verify_child_alive(pid: u32) -> bool {
    sleep(Duration::from_millis(300));
    is_alive(pid)
}

/// Reference parity: if the keep-awake child died immediately, report and exit 1.
pub fn require_child_alive(pid: u32, cmd: &[String]) {
    if verify_child_alive(pid) {
        return;
    }
    eprintln!(
        "wake: keep-awake process exited immediately ({}); see platform requirements",
        command_basename(cmd)
    );
    std::process::exit(1);
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
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use windows_sys::Win32::Foundation::{
        CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    pub fn prevent_std_handle_inheritance() {
        for n in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
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
