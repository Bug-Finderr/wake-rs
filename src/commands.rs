use crate::error::{AppError, Result};
use crate::lid;
use crate::run::{ChargePlan, Mode, RunSpec, Trigger, plan_charge, until_deadline};
use crate::session::{self, Session};
use crate::supervisor::read_battery_status;
use crate::sysutil;
use crate::{durations, platform};
use chrono::{DateTime, Local, Utc};
use std::io::IsTerminal;
use std::process::Child;
use std::time::{Duration, Instant};

#[cfg_attr(windows, allow(dead_code))]
pub fn is_console() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

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
                    .map_err(|_| AppError::usage(format!("pid {pid} is not running")))?,
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
    let _lock = session::acquire_lock()?;
    recover_stale_lid_session_unlocked()?;
    session::reconcile_stop()?;
    if let Some(existing) = session::read_if_alive()? {
        return Err(AppError::fail(format!(
            "session already active (pid {}, {} {}); run 'wake stop' first",
            existing.owner.pid,
            existing.spec.trigger.label(),
            existing.spec.trigger.detail()
        )));
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
    let (mut child, saved) = match spawn_supervisor(&spec) {
        Ok(started) => started,
        Err(error) => {
            if let Some(prepared) = &prepared
                && let Err(rollback) = lid::rollback_start(prepared)
            {
                return Err(AppError::fail(format!(
                    "{error}; lid rollback failed: {rollback}"
                )));
            }
            return Err(error);
        }
    };
    if prepared.is_some()
        && let Err(error) = lid::launch_watchdog(&saved)
    {
        stop_child(&mut child);
        return Err(error);
    }
    print_start_confirmation(&saved);
    Ok(())
}

pub fn start_forever(args: &[String]) -> Result<()> {
    if args[1..]
        .iter()
        .any(|arg| !matches!(arg.as_str(), "--no-display" | "--even-lid"))
    {
        return Err(AppError::usage(
            "forever only accepts --no-display and --even-lid",
        ));
    }
    start(&args[1..])
}

fn spawn_supervisor(spec: &RunSpec) -> Result<(Child, Session)> {
    spec.validate()?;
    let command = vec![
        sysutil::self_exe()?,
        "__supervise__".into(),
        serde_json::to_string(spec)
            .map_err(|error| AppError::fail(format!("could not encode run: {error}")))?,
    ];
    let mut child = sysutil::spawn_named(&command)?;
    match wait_for_session(&mut child, spec) {
        Ok(saved) => Ok((child, saved)),
        Err(error) => {
            stop_child(&mut child);
            if let Ok(Some(saved)) = session::read_saved_for_recovery()
                && saved.owner.pid == child.id()
            {
                let _ = session::remove_if_matches(&saved);
                let _ = session::clear_stop(&saved);
            }
            Err(error)
        }
    }
}

fn wait_for_session(child: &mut Child, spec: &RunSpec) -> Result<Session> {
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
        if let Some(saved) = session::read_saved_for_recovery()? {
            if saved.owner.pid != child.id() || saved.spec != *spec || !saved.matches_live_process()
            {
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

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub fn status() -> Result<()> {
    let _lock = session::acquire_lock()?;
    recover_stale_lid_session_unlocked()?;
    let Some(saved) = session::read_if_alive()? else {
        println!("wake: no active session");
        return Ok(());
    };
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
    if saved.spec.even_lid {
        println!(
            "  even lid  : active (recovery state {})",
            session::lid_restore_file().display()
        );
    }
    Ok(())
}

pub fn stop() -> Result<()> {
    let lock = session::acquire_lock()?;
    recover_stale_lid_session_unlocked()?;
    let Some(saved) = session::read_if_alive()? else {
        println!("wake: no active session");
        session::remove_state_file()?;
        return Ok(());
    };
    session::request_stop(&saved)?;
    let owner = saved.identity();
    drop(lock);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && sysutil::process_matches(&owner) {
        std::thread::sleep(Duration::from_millis(100));
    }
    let fallback = sysutil::process_matches(&owner).then(|| sysutil::terminate_exact(&owner));
    if let Some(message) = stop_failure(fallback, sysutil::process_matches(&owner)) {
        return Err(AppError::fail(message));
    }
    let _lock = session::acquire_lock()?;
    if saved.spec.even_lid {
        lid::finish_stop(&saved)?;
    } else {
        session::remove_if_matches(&saved)?;
        session::clear_stop(&saved)?;
    }
    println!(
        "wake: stopped (pid {}, {})",
        saved.owner.pid,
        saved.spec.trigger.label()
    );
    Ok(())
}

fn stop_failure(fallback: Option<bool>, exact_process_alive: bool) -> Option<&'static str> {
    if !exact_process_alive {
        None
    } else if fallback == Some(false) {
        Some("could not terminate supervisor")
    } else {
        Some("supervisor remained alive")
    }
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

pub fn recover_stale_lid_session_foreground() -> Result<()> {
    lid::recover_foreground()
}

pub fn recover_stale_lid_session_unlocked() -> Result<()> {
    lid::recover_unlocked()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn stop_confirmation_requires_exact_process_exit() {
        let cases = [
            (None, false, None),
            (Some(true), false, None),
            (Some(false), false, None),
            (Some(false), true, Some("could not terminate supervisor")),
            (Some(true), true, Some("supervisor remained alive")),
        ];
        for (fallback, alive, expected) in cases {
            assert_eq!(stop_failure(fallback, alive), expected);
        }
    }
}
