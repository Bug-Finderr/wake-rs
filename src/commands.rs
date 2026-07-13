use crate::error::{AppError, INHIBITOR_STARTUP_EXIT_CODE, Result};
use crate::lid;
use crate::run::{ChargePlan, Mode, RunSpec, Trigger, plan_charge, until_deadline};
use crate::session::{self, Session};
use crate::supervisor::read_battery_status;
use crate::sysutil;
use crate::{durations, platform};
use chrono::{DateTime, Local, Utc};
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

struct Parsed {
    trigger: ParsedTrigger,
    no_display: bool,
    even_lid: bool,
}

enum ParsedTrigger {
    Indefinite,
    Timed { input: String, bare: bool },
    Until(String),
    Charge(String),
    Pid(String),
    App(String),
}

enum ResolvedTrigger {
    Run(Trigger),
    ChargeReached { target: i32, percent: i32 },
}

fn parse_start_args(args: &[String]) -> Result<Parsed> {
    let mut parsed = Parsed {
        trigger: ParsedTrigger::Indefinite,
        no_display: false,
        even_lid: false,
    };
    let mut selected = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--no-display" => parsed.no_display = true,
            "--even-lid" => parsed.even_lid = true,
            "-t" | "--for" => {
                selected = claim_trigger(selected, arg)?;
                parsed.trigger = ParsedTrigger::Timed {
                    input: next_value(args, index, arg)?.clone(),
                    bare: false,
                };
                index += 1;
            }
            "--until" => {
                selected = claim_trigger(selected, arg)?;
                parsed.trigger = ParsedTrigger::Until(next_value(args, index, arg)?.clone());
                index += 1;
            }
            "--until-charge" => {
                selected = claim_trigger(selected, arg)?;
                parsed.trigger = ParsedTrigger::Charge(next_value(args, index, arg)?.clone());
                index += 1;
            }
            "--while-pid" => {
                selected = claim_trigger(selected, arg)?;
                parsed.trigger = ParsedTrigger::Pid(next_value(args, index, arg)?.clone());
                index += 1;
            }
            "--while-app" => {
                selected = claim_trigger(selected, arg)?;
                parsed.trigger = ParsedTrigger::App(next_value(args, index, arg)?.clone());
                index += 1;
            }
            "forever" | "indefinite" => {
                selected = claim_trigger(selected, arg)?;
                parsed.trigger = ParsedTrigger::Indefinite;
            }
            other if other.starts_with('-') => {
                return Err(AppError::usage(format!("unknown flag: {other}")));
            }
            other => {
                selected = claim_trigger(selected, "duration")?;
                parsed.trigger = ParsedTrigger::Timed {
                    input: other.into(),
                    bare: true,
                };
            }
        }
        index += 1;
    }
    Ok(parsed)
}

fn next_value<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a String> {
    args.get(index + 1)
        .filter(|value| !value.starts_with('-'))
        .ok_or_else(|| AppError::usage(format!("missing value for {flag}")))
}

fn claim_trigger(current: Option<String>, next: &str) -> Result<Option<String>> {
    current.map_or_else(
        || Ok(Some(next.into())),
        |current| {
            Err(AppError::usage(format!(
                "conflicting triggers: {current} and {next}"
            )))
        },
    )
}

fn resolve_trigger(parsed: ParsedTrigger) -> Result<ResolvedTrigger> {
    let trigger = match parsed {
        ParsedTrigger::Indefinite => Trigger::Indefinite,
        ParsedTrigger::Timed { input, bare } => Trigger::Timed {
            seconds: durations::parse(&input).map_err(|error| {
                if bare
                    && input
                        .chars()
                        .all(|character| character.is_ascii_alphabetic())
                {
                    AppError::usage(format!("unknown command or duration: {input}"))
                } else {
                    error
                }
            })?,
            input,
        },
        ParsedTrigger::Until(time) => Trigger::Until {
            ends_at: until_deadline(&time)?,
            time,
        },
        ParsedTrigger::Pid(raw) => {
            let pid = raw
                .trim()
                .parse::<u32>()
                .map_err(|_| AppError::usage(format!("invalid pid: '{raw}'")))?;
            Trigger::Pid {
                process: sysutil::capture_process(pid)
                    .map_err(|error| AppError::usage(error.message().to_owned()))?,
            }
        }
        ParsedTrigger::App(name) => Trigger::App {
            process: sysutil::find_app_process(&name)?
                .ok_or_else(|| AppError::usage(format!("no running process matching '{name}'")))?,
            name,
        },
        ParsedTrigger::Charge(raw) => {
            let target = parse_int(&raw, "--until-charge")?;
            if !(1..=100).contains(&target) {
                return Err(AppError::usage("--until-charge must be 1-100"));
            }
            let status = read_battery_status()?;
            match plan_charge(target, &status)? {
                ChargePlan::Reached => {
                    return Ok(ResolvedTrigger::ChargeReached {
                        target,
                        percent: status.percent,
                    });
                }
                ChargePlan::Wait(direction) => Trigger::Charge {
                    target,
                    initial: status.percent,
                    direction,
                },
            }
        }
    };
    Ok(ResolvedTrigger::Run(trigger))
}

pub fn start(args: &[String]) -> Result<()> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        crate::print_help();
        return Ok(());
    }
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-v" | "--version"))
    {
        println!("wake {}", crate::VERSION);
        return Ok(());
    }

    let parsed = parse_start_args(args)?;
    if parsed.even_lid && !platform::supports_even_lid() {
        return Err(AppError::usage(
            "--even-lid is unsupported on this platform",
        ));
    }
    let lock = session::acquire_lock()?;
    recover_stale_lid_session_unlocked()?;
    session::reconcile_stop()?;
    if let Some(existing) = session::read_current()? {
        let state = if existing.owner.pid > 0 {
            "active"
        } else {
            "starting"
        };
        return Err(AppError::fail(format!(
            "session already {state} (pid {}, {} {}); run 'wake stop' first",
            existing.owner.pid,
            existing.spec.trigger.label(),
            existing.spec.trigger.detail()
        )));
    }
    if session::read_lid_restore()?.is_some() || session::read_watchdog()?.is_some() {
        return Err(AppError::fail(
            "lid session cleanup is still in progress; try again",
        ));
    }

    let trigger = match resolve_trigger(parsed.trigger)? {
        ResolvedTrigger::Run(trigger) => trigger,
        ResolvedTrigger::ChargeReached { target, percent } => {
            println!("wake: battery already at {percent}%; target {target}% reached");
            return Ok(());
        }
    };
    let spec = RunSpec {
        mode: if parsed.no_display {
            Mode::SystemOnly
        } else {
            Mode::DisplaySystem
        },
        trigger,
        even_lid: parsed.even_lid,
    };
    let prepared = lid::prepare_start(&spec)?;
    let (mut child, saved) = match spawn_supervisor(&spec, lock) {
        Ok(started) => started,
        Err(error) => {
            if let Some(prepared) = &prepared {
                let _lock = session::acquire_lock_wait()?;
                if session::read_saved_for_recovery()?.is_none()
                    && let Err(rollback) = lid::rollback_start(prepared)
                {
                    return Err(AppError::fail(format!(
                        "{error}; lid rollback failed: {rollback}"
                    )));
                }
            }
            return Err(error);
        }
    };
    if let Some(prepared) = &prepared {
        let lock = session::acquire_lock_wait()?;
        let current = session::read_saved_for_recovery()?;
        let changed = current
            .as_ref()
            .is_none_or(|current| current.owner != saved.owner)
            || !saved.owner_is_live()?
            || session::stop_requested(&saved)?;
        if changed {
            stop_child(&mut child);
            let cleanup = if current
                .as_ref()
                .is_none_or(|current| current.owner.token == saved.owner.token)
            {
                lid::rollback_start(prepared)
                    .and_then(|()| session::remove_if_owner(&saved.owner).map(|_| ()))
                    .and_then(|()| session::clear_stop(&saved).map(|_| ()))
            } else {
                Ok(())
            };
            return match cleanup {
                Ok(()) => Err(AppError::fail("lid session changed during startup")),
                Err(cleanup) => Err(AppError::fail(format!(
                    "lid session changed during startup; cleanup failed: {cleanup}"
                ))),
            };
        }
        if let Err(error) = lid::launch_watchdog(&saved, lock) {
            stop_child(&mut child);
            return Err(error);
        }
    }
    print_start_confirmation(&saved);
    Ok(())
}

fn spawn_supervisor(spec: &RunSpec, lock: session::LockGuard) -> Result<(Child, Session)> {
    spec.validate()?;
    let reservation = session::reserve_process_lease()?;
    let command = vec![
        sysutil::self_exe()?,
        "__supervise__".into(),
        serde_json::to_string(spec)
            .map_err(|error| AppError::fail(format!("could not encode run: {error}")))?,
        reservation.token().into(),
    ];
    let pending_started = Utc::now();
    let pending = Session {
        owner: session::LeaseRef {
            pid: 0,
            token: reservation.token().into(),
        },
        ends_at: spec.trigger.deadline(pending_started)?,
        note: None,
        spec: spec.clone(),
        started_at: pending_started,
    };
    session::write_pending(&pending)?;
    let mut child = match sysutil::spawn_named(&command) {
        Ok(child) => child,
        Err(error) => {
            session::remove_if_matches(&pending)?;
            return Err(error);
        }
    };
    if let Err(error) = wait_for_lease_claim(&mut child, &reservation) {
        stop_child(&mut child);
        let cleanup = session::remove_if_matches(&pending)
            .and_then(|_| session::clear_stop(&pending).map(|_| ()));
        return match cleanup {
            Ok(()) => Err(error),
            Err(cleanup) => Err(AppError::fail(format!(
                "{error}; process lease cleanup failed: {cleanup}"
            ))),
        };
    }
    let token = reservation.commit();
    drop(lock);
    match wait_for_session(&mut child, spec, &token) {
        Ok(saved) => Ok((child, saved)),
        Err(error) => {
            stop_child(&mut child);
            let _lock = session::acquire_lock_wait()?;
            finish_supervisor_start_error(error, &token, &pending)
        }
    }
}

fn wait_for_lease_claim(
    child: &mut Child,
    reservation: &session::ProcessLeaseReservation,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| AppError::fail(format!("could not inspect supervisor: {error}")))?
        {
            return Err(AppError::fail(format!(
                "supervisor exited during startup with {status}"
            )));
        }
        if reservation.is_claimed()? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(AppError::fail("supervisor did not claim its process lease"))
}

fn finish_supervisor_start_error(
    error: AppError,
    token: &str,
    pending: &Session,
) -> Result<(Child, Session)> {
    let cleanup = (|| {
        session::remove_if_owner(&pending.owner)?;
        session::clear_stop(pending)?;
        session::discard_process_lease(token)
    })();
    match cleanup {
        Ok(()) => Err(error),
        Err(cleanup) => Err(AppError::fail(format!(
            "{error}; process lease cleanup failed: {cleanup}"
        ))),
    }
}

fn wait_for_session(child: &mut Child, spec: &RunSpec, token: &str) -> Result<Session> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| AppError::fail(format!("could not inspect supervisor: {error}")))?
        {
            return Err(supervisor_startup_error(status, spec.even_lid));
        }
        if let Some(saved) = session::read_saved_for_recovery()? {
            if saved.owner.token != token || saved.spec != *spec {
                return Err(AppError::fail(
                    "supervisor published mismatched session state",
                ));
            }
            if saved.owner.pid == 0 {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            if saved.owner.pid != child.id() || !saved.owner_is_live()? {
                return Err(AppError::fail(
                    "supervisor published mismatched session state",
                ));
            }
            return Ok(saved);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(AppError::fail("supervisor did not publish session state"))
}

fn supervisor_startup_error(status: ExitStatus, even_lid: bool) -> AppError {
    if status.code() == Some(INHIBITOR_STARTUP_EXIT_CODE) {
        let error = platform::inhibitor_startup_error(even_lid);
        AppError::fail(error.message().to_owned())
    } else {
        AppError::fail(format!("supervisor exited during startup with {status}"))
    }
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub fn status() -> Result<()> {
    let _lock = session::acquire_lock()?;
    recover_stale_lid_session_unlocked()?;
    let Some(saved) = session::read_current()? else {
        println!("wake: no active session");
        return Ok(());
    };
    if saved.owner.pid == 0 {
        println!("wake: session starting");
        return Ok(());
    }
    let now = Utc::now();
    let remaining = saved
        .ends_at
        .map(|end| pretty_duration((end - now).num_seconds().max(0)))
        .unwrap_or_else(|| "-".into());
    println!("wake: session active (pid {})", saved.owner.pid);
    println!("  mode      : {}", saved.spec.mode.label());
    println!(
        "  trigger   : {} ({})",
        saved.spec.trigger.label(),
        saved.spec.trigger.detail()
    );
    println!(
        "  started   : {} ({} ago)",
        hms(saved.started_at),
        pretty_duration((now - saved.started_at).num_seconds())
    );
    println!("  remaining : {remaining}");
    if lid::uses_watchdog(&saved.spec) {
        println!(
            "  even lid  : --even-lid active (recovery state {})",
            session::lid_restore_file().display()
        );
    } else if saved.spec.even_lid {
        println!("  even lid  : --even-lid active");
    }
    Ok(())
}

pub fn stop() -> Result<()> {
    let lock = session::acquire_lock()?;
    recover_stale_lid_session_unlocked()?;
    let Some(saved) = session::read_current()? else {
        println!("wake: no active session");
        session::remove_state_file()?;
        return Ok(());
    };
    session::request_stop(&saved)?;
    let owner = saved.identity();
    drop(lock);
    if !sysutil::wait_lease_exit(&owner, Duration::from_secs(5))? {
        return Err(AppError::fail("supervisor did not stop within 5 seconds"));
    }
    let _lock = session::acquire_lock_wait()?;
    let changed = session::read_saved_for_recovery()?
        .is_some_and(|current| !current.owner.same_lease(&saved.owner));
    if !changed {
        if lid::uses_watchdog(&saved.spec) {
            lid::finish_stop(&saved)?;
        } else {
            session::remove_if_owner(&saved.owner)?;
            session::clear_stop(&saved)?;
        }
    }
    println!(
        "wake: stopped (pid {}, {})",
        saved.owner.pid,
        saved.spec.trigger.label()
    );
    Ok(())
}

fn print_start_confirmation(saved: &Session) {
    println!("wake: session active (pid {})", saved.owner.pid);
    println!("  mode    : {}", saved.spec.mode.label());
    println!(
        "  trigger : {} ({})",
        saved.spec.trigger.label(),
        saved.spec.trigger.detail()
    );
    println!("  started : {}", hms(saved.started_at));
    println!(
        "  ends    : {}",
        saved.ends_at.map(hms).unwrap_or_else(|| "-".into())
    );
    if saved.spec.even_lid {
        println!("note: --even-lid active; lid close will not sleep until this session ends");
        println!("caution: a closed lid can increase heat and battery drain");
    } else if let Some(note) = &saved.note {
        println!("{note}");
    }
}

fn hms(time: DateTime<Utc>) -> String {
    time.with_timezone(&Local).format("%H:%M:%S").to_string()
}

pub fn pretty_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn parse_int(value: &str, name: &str) -> Result<i32> {
    value
        .trim()
        .parse::<i32>()
        .map_err(|_| AppError::usage(format!("{name}: not an integer: '{value}'")))
}

pub fn recover_stale_lid_session_unlocked() -> Result<()> {
    session::ensure_no_legacy_state()?;
    lid::recover_unlocked()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use crate::error::INHIBITOR_STARTUP_EXIT_CODE;
    #[cfg(target_os = "linux")]
    use std::os::unix::process::ExitStatusExt;

    #[cfg(target_os = "linux")]
    fn exit_status(code: i32) -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(code << 8)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unrelated_even_lid_supervisor_failure_keeps_generic_error() {
        let error = supervisor_startup_error(exit_status(1), true);

        assert_eq!(
            error.message(),
            "supervisor exited during startup with exit status: 1"
        );
        assert_eq!(error.exit_code(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inhibitor_supervisor_failure_is_translated_to_public_error() {
        let error = supervisor_startup_error(exit_status(INHIBITOR_STARTUP_EXIT_CODE), true);

        assert_eq!(
            error.message(),
            "could not acquire the systemd-logind handle-lid-switch inhibitor required by --even-lid; logind may be unavailable or this session may lack permission"
        );
        assert_eq!(error.exit_code(), 1);
    }

    #[test]
    fn syntax_errors_win_before_process_or_app_resolution() {
        let cases = [
            (
                vec!["--while-pid", "4294967295", "--bogus"],
                "unknown flag: --bogus",
            ),
            (
                vec!["--while-pid", "not-a-pid", "--bogus"],
                "unknown flag: --bogus",
            ),
            (vec!["12fortnights", "--bogus"], "unknown flag: --bogus"),
            (
                vec!["--while-app", "wake-no-such-app-42", "1m"],
                "conflicting triggers",
            ),
        ];
        for (args, expected) in cases {
            let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
            let error = parse_start_args(&args).err().expect("syntax error");
            assert!(error.message().contains(expected), "{}", error.message());
        }

        let flag_value = ["--while-pid", "--bogus"].map(str::to_string);
        assert_eq!(
            parse_start_args(&flag_value).err().unwrap().message(),
            "missing value for --while-pid"
        );
        let invalid_pid = ["--while-pid", "not-a-pid"].map(str::to_string);
        let parsed = parse_start_args(&invalid_pid).expect("valid syntax");
        assert!(matches!(
            resolve_trigger(parsed.trigger),
            Err(AppError::Usage(_))
        ));
    }
}
