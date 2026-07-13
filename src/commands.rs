use crate::error::{AppError, INHIBITOR_STARTUP_EXIT_CODE, Result, combine_cleanup};
use crate::lid;
use crate::run::{
    ChargePlan, Mode, ProcessIdentity, RunSpec, Trigger, plan_charge, until_deadline,
};
use crate::session::{self, OwnerIdentity, STATE_SCHEMA, SessionState, State};
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
            let status = platform::read_battery()?;
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
    spec.validate()?;
    lid::prepare_start(&spec)?;
    lid::recover()?;

    let lock = session::acquire_lock()?;
    if let Some(existing) = session::read()? {
        let state = if existing.session.started_at.is_some() {
            "active"
        } else {
            "starting"
        };
        return Err(AppError::fail(format!(
            "session already {state} (pid {}, {} {}); run 'wake stop' first",
            existing.session.owner.process.pid,
            existing.session.spec.trigger.label(),
            existing.session.spec.trigger.detail()
        )));
    }

    let (mut child, saved) = spawn_supervisor(&spec, lock)?;
    if lid::uses_watchdog(&saved.session.spec) {
        let lock = session::acquire_lock()?;
        let current = session::read()?;
        if current.as_ref() != Some(&saved)
            || !sysutil::process_identity_matches(&saved.session.owner.process)?
            || saved.session.stop_requested
        {
            drop(lock);
            let cleanup = request_stop_and_wait(&saved.session.owner, Duration::from_secs(15));
            return combine_cleanup(
                Err(AppError::fail("lid session changed during startup")),
                cleanup,
            );
        }
        if let Err(error) = lid::launch_watchdog(&saved, lock) {
            let cleanup = request_stop_and_wait(&saved.session.owner, Duration::from_secs(15))
                .and_then(|()| lid::recover());
            return combine_cleanup(Err(error), cleanup);
        }
        finish_watchdog_readiness(lid::ensure_ready(&saved), || {
            request_stop_and_wait(&saved.session.owner, Duration::from_secs(15))
                .and_then(|()| lid::recover())
        })?;
    }
    let status = child
        .try_wait()
        .map(|status| status.map(|status| status.to_string()))
        .map_err(|error| AppError::fail(format!("could not inspect supervisor: {error}")));
    finish_start_confirmation(
        status,
        || {
            request_stop_and_wait(&saved.session.owner, Duration::from_secs(15))
                .and_then(|()| lid::recover())
        },
        || print_start_confirmation(&saved),
    )
}

fn spawn_supervisor(spec: &RunSpec, lock: session::LockGuard) -> Result<(Child, State)> {
    let token = session::new_token()?;
    let command = vec![sysutil::self_exe()?, "__supervise__".into(), token.clone()];
    let mut child = sysutil::spawn_named(&command)?;
    let process = sysutil::process_identity(child.id());
    let starting = prepare_handoff(
        spec,
        token,
        process,
        |starting| session::create(&lock, starting),
        |starting| cleanup_before_handoff(&mut child, &lock, starting),
    )?;
    drop(lock);
    match wait_for_session(&mut child, &starting) {
        Ok(saved) => Ok((child, saved)),
        Err(error) => {
            let cleanup = request_stop_and_wait(&starting.session.owner, Duration::from_secs(5))
                .and_then(|()| lid::recover());
            combine_cleanup(Err(error), cleanup)
        }
    }
}

fn prepare_handoff(
    spec: &RunSpec,
    token: String,
    process: Result<Option<ProcessIdentity>>,
    publish: impl FnOnce(&State) -> Result<()>,
    cleanup: impl FnOnce(Option<&State>) -> Result<()>,
) -> Result<State> {
    let process = match process {
        Ok(Some(process)) => process,
        Ok(None) => {
            return combine_cleanup(
                Err(AppError::fail(
                    "supervisor exited before publishing its identity",
                )),
                cleanup(None),
            );
        }
        Err(error) => return combine_cleanup(Err(error), cleanup(None)),
    };
    let starting = State {
        schema: STATE_SCHEMA,
        session: SessionState {
            owner: OwnerIdentity { token, process },
            spec: spec.clone(),
            started_at: None,
            note: None,
            stop_requested: false,
        },
        lid: None,
    };
    match publish(&starting) {
        Ok(()) => Ok(starting),
        Err(error) => combine_cleanup(Err(error), cleanup(Some(&starting))),
    }
}

fn cleanup_before_handoff(
    child: &mut Child,
    lock: &session::LockGuard,
    starting: Option<&State>,
) -> Result<()> {
    let terminated = terminate_owned_child(child);
    let removed = match starting {
        Some(starting) => session::remove_exact(lock, starting).map(|_| ()),
        None => Ok(()),
    };
    combine_cleanup(terminated, removed)
}

fn terminate_owned_child(child: &mut Child) -> Result<()> {
    match child.kill() {
        Ok(()) => child
            .wait()
            .map(|_| ())
            .map_err(|error| AppError::fail(format!("could not reap supervisor: {error}"))),
        Err(kill_error) => match child.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(AppError::fail(format!(
                "could not terminate supervisor: {kill_error}"
            ))),
            Err(inspect_error) => Err(AppError::fail(format!(
                "could not terminate supervisor: {kill_error}; could not inspect it: {inspect_error}"
            ))),
        },
    }
}

fn wait_for_session(child: &mut Child, starting: &State) -> Result<State> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| AppError::fail(format!("could not inspect supervisor: {error}")))?
        {
            return Err(supervisor_startup_error(
                status,
                starting.session.spec.even_lid,
            ));
        }
        if let Some(saved) = session::read()? {
            if saved.session.owner != starting.session.owner
                || saved.session.spec != starting.session.spec
                || saved.lid.is_some()
            {
                return Err(AppError::fail(
                    "supervisor published mismatched session state",
                ));
            }
            if saved.session.started_at.is_some() {
                if !sysutil::process_identity_matches(&saved.session.owner.process)? {
                    return Err(AppError::fail("supervisor exited during startup"));
                }
                return Ok(saved);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(AppError::fail("supervisor did not publish session state"))
}

fn request_stop_and_wait(owner: &OwnerIdentity, within: Duration) -> Result<()> {
    let lock = session::acquire_existing_lock_wait()?;
    if let Some(current) = session::read()? {
        if &current.session.owner != owner {
            return Err(AppError::fail("active session ownership changed"));
        }
        if !current.session.stop_requested {
            session::update(&lock, owner, |state| {
                state.session.stop_requested = true;
                Ok(())
            })?;
        }
    }
    drop(lock);
    if sysutil::wait_process_exit(&owner.process, within)? {
        Ok(())
    } else {
        Err(AppError::fail(format!(
            "supervisor did not stop within {} seconds",
            within.as_secs()
        )))
    }
}

fn supervisor_startup_error(status: ExitStatus, even_lid: bool) -> AppError {
    if status.code() == Some(INHIBITOR_STARTUP_EXIT_CODE) {
        let error = platform::inhibitor_startup_error(even_lid);
        AppError::fail(error.message().to_owned())
    } else {
        AppError::fail(format!("supervisor exited during startup with {status}"))
    }
}

pub fn status() -> Result<()> {
    lid::recover()?;
    let _lock = session::acquire_lock()?;
    let Some(saved) = session::read()? else {
        println!("wake: no active session");
        return Ok(());
    };
    let Some(started_at) = saved.session.started_at else {
        println!("wake: session starting");
        return Ok(());
    };
    let now = Utc::now();
    let remaining = saved
        .session
        .spec
        .trigger
        .deadline(started_at)?
        .map(|end| pretty_duration((end - now).num_seconds().max(0)))
        .unwrap_or_else(|| "-".into());
    println!(
        "wake: session active (pid {})",
        saved.session.owner.process.pid
    );
    println!("  mode      : {}", saved.session.spec.mode.label());
    println!(
        "  trigger   : {} ({})",
        saved.session.spec.trigger.label(),
        saved.session.spec.trigger.detail()
    );
    println!(
        "  started   : {} ({} ago)",
        hms(started_at),
        pretty_duration((now - started_at).num_seconds())
    );
    println!("  remaining : {remaining}");
    if saved.session.spec.even_lid {
        println!("  even lid  : --even-lid request active");
    }
    Ok(())
}

pub fn stop() -> Result<()> {
    lid::recover()?;
    let lock = session::acquire_lock()?;
    let Some(saved) = session::read()? else {
        println!("wake: no active session");
        return Ok(());
    };
    let stopped = if saved.session.stop_requested {
        saved
    } else {
        session::update(&lock, &saved.session.owner, |state| {
            state.session.stop_requested = true;
            Ok(())
        })?
    };
    drop(lock);
    if !sysutil::wait_process_exit(&stopped.session.owner.process, Duration::from_secs(5))? {
        return Err(AppError::fail("supervisor did not stop within 5 seconds"));
    }
    if lid::uses_watchdog(&stopped.session.spec) {
        lid::finish_stop(&stopped)?;
    } else {
        lid::recover()?;
    }
    println!(
        "wake: stopped (pid {}, {})",
        stopped.session.owner.process.pid,
        stopped.session.spec.trigger.label()
    );
    Ok(())
}

fn print_start_confirmation(saved: &State) -> Result<()> {
    let started_at = saved
        .session
        .started_at
        .ok_or_else(|| AppError::fail("supervisor did not publish a start time"))?;
    println!(
        "wake: session active (pid {})",
        saved.session.owner.process.pid
    );
    println!("  mode    : {}", saved.session.spec.mode.label());
    println!(
        "  trigger : {} ({})",
        saved.session.spec.trigger.label(),
        saved.session.spec.trigger.detail()
    );
    println!("  started : {}", hms(started_at));
    println!(
        "  ends    : {}",
        saved
            .session
            .spec
            .trigger
            .deadline(started_at)?
            .map(hms)
            .unwrap_or_else(|| "-".into())
    );
    if saved.session.spec.even_lid {
        println!("note: --even-lid request active for this session");
        println!("caution: a closed lid can increase heat and battery drain");
    } else if let Some(note) = &saved.session.note {
        println!("{note}");
    }
    Ok(())
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

fn finish_watchdog_readiness(
    readiness: Result<()>,
    cleanup: impl FnOnce() -> Result<()>,
) -> Result<()> {
    match readiness {
        Ok(()) => Ok(()),
        Err(error) => combine_cleanup(Err(error), cleanup()),
    }
}

fn finish_start_confirmation(
    status: Result<Option<String>>,
    cleanup: impl FnOnce() -> Result<()>,
    confirm: impl FnOnce() -> Result<()>,
) -> Result<()> {
    match status {
        Ok(None) => confirm(),
        Ok(Some(status)) => combine_cleanup(
            Err(AppError::fail(format!(
                "supervisor exited before start confirmation with {status}"
            ))),
            cleanup(),
        ),
        Err(error) => combine_cleanup(Err(error), cleanup()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::ProcessIdentity;
    use std::cell::{Cell, RefCell};
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
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inhibitor_supervisor_failure_is_translated_to_public_error() {
        let error = supervisor_startup_error(exit_status(INHIBITOR_STARTUP_EXIT_CODE), true);
        assert_eq!(
            error.message(),
            "could not acquire the systemd-logind handle-lid-switch inhibitor required by --even-lid; logind may be unavailable or this session may lack permission"
        );
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
        let invalid_pid = ["--while-pid", "not-a-pid"].map(str::to_string);
        let parsed = parse_start_args(&invalid_pid).expect("valid syntax");
        assert!(matches!(
            resolve_trigger(parsed.trigger),
            Err(AppError::Usage(_))
        ));
    }

    #[test]
    fn readiness_failure_runs_cleanup() {
        let cleaned = Cell::new(false);
        let result = finish_watchdog_readiness(Err(AppError::fail("not ready")), || {
            cleaned.set(true);
            Ok(())
        });

        assert!(result.is_err());
        assert!(cleaned.get());
    }

    fn indefinite_spec() -> RunSpec {
        RunSpec {
            mode: Mode::DisplaySystem,
            trigger: Trigger::Indefinite,
            even_lid: false,
        }
    }

    fn process_identity() -> ProcessIdentity {
        ProcessIdentity {
            pid: 10,
            native_start: 11,
        }
    }

    #[test]
    fn identity_failure_after_spawn_terminates_owned_child() {
        let terminated = Cell::new(false);
        let result = prepare_handoff(
            &indefinite_spec(),
            "0123456789abcdef0123456789abcdef".into(),
            Err(AppError::fail("could not inspect supervisor")),
            |_| unreachable!("identity failure must not publish state"),
            |starting| {
                assert!(starting.is_none());
                terminated.set(true);
                Ok(())
            },
        );

        assert!(result.is_err());
        assert!(terminated.get());
    }

    #[test]
    fn committed_create_error_terminates_child_and_removes_exact_state() {
        let durable = RefCell::new(None);
        let terminated = Cell::new(false);
        let result = prepare_handoff(
            &indefinite_spec(),
            "0123456789abcdef0123456789abcdef".into(),
            Ok(Some(process_identity())),
            |starting| {
                durable.replace(Some(starting.clone()));
                Err(AppError::fail("parent sync failed"))
            },
            |starting| {
                terminated.set(true);
                if starting.is_some_and(|starting| durable.borrow().as_ref() == Some(starting)) {
                    durable.replace(None);
                }
                Ok(())
            },
        );

        assert!(result.is_err());
        assert!(terminated.get());
        assert!(durable.borrow().is_none());
    }

    #[test]
    fn exited_child_before_confirmation_is_cleaned_and_reported() {
        let cleaned = Cell::new(false);
        let confirmed = Cell::new(false);
        let result = finish_start_confirmation(
            Ok(Some("exit status: 1".into())),
            || {
                cleaned.set(true);
                Ok(())
            },
            || {
                confirmed.set(true);
                Ok(())
            },
        );

        assert!(result.is_err());
        assert!(cleaned.get());
        assert!(!confirmed.get());
    }

    #[test]
    fn child_inspection_error_before_confirmation_is_cleaned_and_reported() {
        let cleaned = Cell::new(false);
        let confirmed = Cell::new(false);
        let result = finish_start_confirmation(
            Err(AppError::fail("could not inspect supervisor")),
            || {
                cleaned.set(true);
                Ok(())
            },
            || {
                confirmed.set(true);
                Ok(())
            },
        );

        assert!(result.is_err());
        assert!(cleaned.get());
        assert!(!confirmed.get());
    }
}
