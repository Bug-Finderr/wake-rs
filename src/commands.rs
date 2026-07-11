//! Foreground commands: start / status / stop / forever, plus the macOS even-lid enable + crash
//! recovery machinery and shared formatting helpers.

use crate::error::{AppError, Result};
use crate::run::{ChargePlan, Mode, RunSpec, Trigger, plan_charge, until_deadline};
use crate::session::{self, Session};
use crate::supervisor::read_battery_status;
use crate::sysutil;
use crate::{durations, platform};
use chrono::{DateTime, Local, Utc};
use std::io::IsTerminal;
use std::process::Child;
use std::time::{Duration as StdDuration, Instant};

// Used by the unix picker and the macOS sudo prompt; the Windows even-lid path never prompts.
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
    let mut p = Parsed {
        trigger: ParsedTrigger::Indefinite,
        no_display: false,
        even_lid: false,
    };
    let mut trigger_flag: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--no-display" => p.no_display = true,
            "--even-lid" => p.even_lid = true,
            "-t" | "--for" => {
                let v = next_value(args, i, a)?;
                trigger_flag = claim_trigger(trigger_flag, a)?;
                p.trigger = ParsedTrigger::Timed {
                    input: v.clone(),
                    bare: false,
                };
                i += 1;
            }
            "--until" => {
                let v = next_value(args, i, "--until")?;
                trigger_flag = claim_trigger(trigger_flag, a)?;
                p.trigger = ParsedTrigger::Until(v.clone());
                i += 1;
            }
            "--until-charge" => {
                let v = next_value(args, i, "--until-charge")?;
                trigger_flag = claim_trigger(trigger_flag, a)?;
                p.trigger = ParsedTrigger::Charge(v.clone());
                i += 1;
            }
            "--while-pid" => {
                let v = next_value(args, i, "--while-pid")?;
                trigger_flag = claim_trigger(trigger_flag, a)?;
                p.trigger = ParsedTrigger::Pid(v.clone());
                i += 1;
            }
            "--while-app" => {
                let v = next_value(args, i, "--while-app")?;
                trigger_flag = claim_trigger(trigger_flag, a)?;
                p.trigger = ParsedTrigger::App(v.clone());
                i += 1;
            }
            "forever" | "indefinite" => {
                trigger_flag = claim_trigger(trigger_flag, a)?;
                p.trigger = ParsedTrigger::Indefinite;
            }
            other => {
                if other.starts_with('-') {
                    return Err(AppError::usage(format!("unknown flag: {other}")));
                }
                trigger_flag = claim_trigger(trigger_flag, "duration")?;
                p.trigger = ParsedTrigger::Timed {
                    input: other.to_string(),
                    bare: true,
                };
            }
        }
        i += 1;
    }
    Ok(p)
}

fn resolve_trigger(parsed: ParsedTrigger) -> Result<ResolvedTrigger> {
    let trigger = match parsed {
        ParsedTrigger::Indefinite => Trigger::Indefinite,
        ParsedTrigger::Timed { input, bare } => Trigger::Timed {
            seconds: durations::parse(&input).map_err(|error| {
                if bare && input.chars().all(|c| c.is_ascii_alphabetic()) {
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
                .map_err(|_| AppError::fail(format!("invalid pid: '{raw}'")))?;
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

fn next_value<'a>(args: &'a [String], i: usize, flag: &str) -> Result<&'a String> {
    args.get(i + 1)
        .ok_or_else(|| AppError::usage(format!("missing value for {flag}")))
}

fn claim_trigger(current: Option<String>, next: &str) -> Result<Option<String>> {
    match current {
        Some(cur) => Err(AppError::usage(format!(
            "conflicting triggers: {cur} and {next}"
        ))),
        None => Ok(Some(next.to_string())),
    }
}

pub fn start(args: &[String]) -> Result<()> {
    // Honor help/version anywhere in the args (not just as the first token), so `wake 1h --help`
    // prints help instead of erroring on an "unknown flag".
    if args.iter().any(|a| a == "-h" || a == "--help") {
        crate::print_help();
        return Ok(());
    }
    if args.iter().any(|a| a == "-v" || a == "--version") {
        println!("wake {}", crate::VERSION);
        return Ok(());
    }
    let parsed = parse_start_args(args)?;

    if parsed.even_lid && !platform::supports_even_lid() {
        return Err(AppError::usage(even_lid_unsupported_message()));
    }

    let _lock = session::acquire_lock()?;
    recover_stale_lid_session_unlocked()?;
    session::reconcile_stop()?;
    if let Some(existing) = session::read_if_alive()? {
        return Err(AppError::fail(format!(
            "session already active (pid {}, {} {}); run 'wake stop' first",
            existing.pid, existing.trigger, existing.detail
        )));
    }

    let trigger = match resolve_trigger(parsed.trigger)? {
        ResolvedTrigger::Run(trigger) => trigger,
        ResolvedTrigger::ChargeReached { target, percent } => {
            println!("wake: battery already at {percent}%; target {target}% reached");
            return Ok(());
        }
    };

    let mode = if parsed.no_display {
        Mode::SystemOnly
    } else {
        Mode::DisplaySystem
    };

    #[cfg(not(windows))]
    if parsed.even_lid {
        return start_lid_supervisor(&trigger, parsed.no_display, mode.label());
    }

    #[cfg(windows)]
    if parsed.even_lid {
        return start_even_lid_windows(&trigger, parsed.no_display, mode);
    }

    start_supervisor(RunSpec { mode, trigger })
}

pub fn start_forever(args: &[String]) -> Result<()> {
    let rest = &args[1..];
    for a in rest {
        if a != "--no-display" && a != "--even-lid" {
            return Err(AppError::usage(
                "forever only accepts --no-display and --even-lid",
            ));
        }
    }
    start(rest)
}

fn start_supervisor(spec: RunSpec) -> Result<()> {
    spec.validate()?;
    let cmd = vec![
        sysutil::self_exe()?,
        "__supervise__".into(),
        serde_json::to_string(&spec)
            .map_err(|error| AppError::fail(format!("could not encode run: {error}")))?,
    ];
    let mut child = sysutil::spawn_named(&cmd)?;
    let result = wait_for_session(&mut child, false);
    if result.is_err() {
        stop_child(&mut child);
        if let Ok(Some(saved)) = session::read_saved_for_recovery()
            && saved.pid == child.id()
        {
            let _ = session::remove_if_matches(&saved);
            let _ = session::clear_stop(&saved);
        }
    }
    let saved = result?;
    print_start_confirmation(&saved, saved.note.as_deref());
    Ok(())
}

fn wait_for_session(child: &mut Child, even_lid: bool) -> Result<Session> {
    let deadline = Instant::now() + StdDuration::from_secs(5);
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
            if saved.pid != child.id()
                || saved.even_lid != even_lid
                || !saved.matches_live_process()
            {
                return Err(AppError::fail(
                    "supervisor published mismatched session state",
                ));
            }
            return Ok(saved);
        }
        std::thread::sleep(StdDuration::from_millis(100));
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
    let Some(s) = session::read_if_alive()? else {
        println!("wake: no active session");
        return Ok(());
    };
    let now = Utc::now();
    let started = s.started_at.unwrap_or(now);
    let elapsed = (now - started).num_seconds();
    let remaining = match s.ends_at {
        None => "-".to_string(),
        Some(e) => pretty_duration((e - now).num_seconds().max(0)),
    };
    println!("wake: session active (pid {})", s.pid);
    println!("  mode      : {}", s.mode);
    println!("  trigger   : {} ({})", s.trigger, s.detail);
    println!(
        "  started   : {} ({} ago)",
        hms(started),
        pretty_duration(elapsed)
    );
    println!("  remaining : {remaining}");
    if s.even_lid {
        #[cfg(windows)]
        {
            let (ac, dc) = platform::decode_lid(s.prior_disable_sleep);
            println!(
                "  even lid  : active (restore lid action AC={ac} DC={dc}, state {})",
                session::state_file().display()
            );
        }
        #[cfg(not(windows))]
        println!(
            "  even lid  : active (restore SleepDisabled={}, state {})",
            s.prior_disable_sleep,
            session::state_file().display()
        );
    }
    Ok(())
}

pub fn stop() -> Result<()> {
    let lock = session::acquire_lock()?;
    recover_stale_lid_session_unlocked()?;
    let Some(s) = session::read_if_alive()? else {
        println!("wake: no active session");
        session::remove_state_file()?;
        return Ok(());
    };
    if s.even_lid {
        let process = s.identity();
        let terminated = sysutil::terminate_exact(&process);
        if let Some(message) = stop_failure(Some(terminated), sysutil::process_matches(&process)) {
            return Err(AppError::fail(message));
        }
        #[cfg(windows)]
        restore_even_lid_windows(&s)?;
        #[cfg(not(windows))]
        verify_disable_sleep_restored_after_stop(&s)?;
        session::remove_state_file()?;
    } else {
        session::request_stop(&s)?;
        let process = s.identity();
        drop(lock);
        let deadline = Instant::now() + StdDuration::from_secs(5);
        while Instant::now() < deadline && sysutil::process_matches(&process) {
            std::thread::sleep(StdDuration::from_millis(100));
        }
        let fallback = if sysutil::process_matches(&process) {
            Some(sysutil::terminate_exact(&process))
        } else {
            None
        };
        if let Some(message) = stop_failure(fallback, sysutil::process_matches(&process)) {
            return Err(AppError::fail(message));
        }
        let _lock = session::acquire_lock()?;
        session::remove_if_matches(&s)?;
        session::clear_stop(&s)?;
    }
    println!("wake: stopped (pid {}, {})", s.pid, s.trigger);
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

fn print_start_confirmation(s: &Session, note: Option<&str>) {
    let started = s.started_at.map(hms).unwrap_or_else(|| "-".into());
    let ends = s.ends_at.map(hms).unwrap_or_else(|| "-".into());
    println!("wake: session active (pid {})", s.pid);
    println!("  mode    : {}", s.mode);
    println!("  trigger : {} ({})", s.trigger, s.detail);
    println!("  started : {started}");
    println!("  ends    : {ends}");
    if s.even_lid {
        #[cfg(windows)]
        println!("note: --even-lid active; lid close will not sleep until this session ends");
        #[cfg(not(windows))]
        println!(
            "note: --even-lid is active; this Mac should stay awake with the lid closed until the session ends"
        );
        println!(
            "caution: closed lid + battery + no external display can run hot and drain quickly"
        );
    } else if let Some(n) = note
        .map(str::to_string)
        .or_else(platform::static_start_note)
    {
        println!("{n}");
    }
}

fn hms(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%H:%M:%S").to_string()
}

pub fn pretty_duration(sec: i64) -> String {
    let sec = sec.max(0);
    let (h, m, s) = (sec / 3600, (sec % 3600) / 60, sec % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

fn parse_int(s: &str, name: &str) -> Result<i32> {
    s.trim()
        .parse::<i32>()
        .map_err(|_| AppError::usage(format!("{name}: not an integer: '{s}'")))
}

fn even_lid_unsupported_message() -> String {
    "--even-lid is unsupported on Linux; lid-switch inhibition is handled through systemd when privileged".into()
}

/// Set the lid-close action to (ac, dc). Tries a direct write first (it succeeds unprivileged for
/// admin accounts); only if the OS denies it does it retry via the elevated `__set_lid__` helper (UAC).
#[cfg(windows)]
fn set_lid(ac: u32, dc: u32) -> Result<()> {
    if platform::write_lid_action(ac, dc).is_ok() {
        return Ok(());
    }
    let (a, d) = (ac.to_string(), dc.to_string());
    if sysutil::run_elevated_self(&["__set_lid__", &a, &d])? != 0 {
        return Err(AppError::fail("could not set the lid-close action"));
    }
    Ok(())
}

/// Set the lid-close action to "Do nothing" (0,0) for a freshly recorded Windows even-lid session.
#[cfg(windows)]
fn enable_even_lid_windows() -> Result<()> {
    set_lid(0, 0)?;
    let after = platform::read_lid_action()?;
    if after != (0, 0) {
        return Err(AppError::fail(format!(
            "failed to enable --even-lid; lid action is AC={} DC={}",
            after.0, after.1
        )));
    }
    Ok(())
}

/// Restore the prior lid action recorded in `s` when stopping a Windows even-lid session.
#[cfg(windows)]
fn restore_even_lid_windows(s: &Session) -> Result<()> {
    let (ac, dc) = platform::decode_lid(s.prior_disable_sleep);
    if (ac, dc) == (0, 0) {
        // Prior action was already "do nothing"; nothing to restore.
        return Ok(());
    }
    set_lid(ac, dc)?;
    let after = platform::read_lid_action()?;
    if after != (ac, dc) {
        return Err(AppError::fail(format!(
            "failed to restore the lid action to AC={ac} DC={dc}; current is AC={} DC={}",
            after.0, after.1
        )));
    }
    eprintln!("wake: restored the lid action to AC={ac} DC={dc}");
    Ok(())
}

#[cfg(windows)]
fn start_even_lid_windows(trigger: &Trigger, no_display: bool, mode: Mode) -> Result<()> {
    let prior = platform::read_lid_action()?;
    if let Trigger::Charge { target, .. } = trigger {
        return start_charge_supervisor_windows(*target, mode, prior);
    }
    let wait_pid = trigger.process().map(|process| process.pid);
    let ka = platform::keep_awake_command(no_display, legacy_timeout(trigger), wait_pid)?;
    let now = Utc::now();
    let mut child = sysutil::spawn_named(&ka.cmd)?;
    sysutil::require_child_alive(child.id(), &ka.cmd)?;
    let mut saved = Session {
        pid: child.id(),
        mode: mode.label().into(),
        trigger: trigger.label().into(),
        detail: trigger.detail(),
        started_at: Some(now),
        ends_at: trigger.session_ends_at(now),
        note: ka.note,
        even_lid: true,
        prior_disable_sleep: platform::encode_lid(prior.0, prior.1),
        ..Default::default()
    };
    if let Err(error) = saved
        .capture_process_identity()
        .and_then(|_| session::write(&saved))
        .and_then(|_| enable_even_lid_windows())
    {
        stop_child(&mut child);
        let _ = session::remove_if_matches(&saved);
        return Err(error);
    }
    print_start_confirmation(&saved, saved.note.as_deref());
    Ok(())
}

fn legacy_timeout(trigger: &Trigger) -> Option<i64> {
    match trigger {
        Trigger::Timed { seconds, .. } => Some(*seconds),
        Trigger::Until { ends_at, .. } => Some((*ends_at - Utc::now()).num_seconds().max(0)),
        _ => None,
    }
}

#[cfg(windows)]
fn start_charge_supervisor_windows(charge: i32, mode: Mode, prior_lid: (u32, u32)) -> Result<()> {
    let prior_encoded = platform::encode_lid(prior_lid.0, prior_lid.1);
    let cmd = vec![
        sysutil::self_exe()?,
        "__supervise_charge__".into(),
        charge.to_string(),
        mode.no_display().to_string(),
        mode.label().into(),
        prior_encoded.to_string(),
    ];
    let mut child = sysutil::spawn_named(&cmd)?;
    let saved = match wait_for_session(&mut child, true) {
        Ok(saved) => saved,
        Err(error) => {
            stop_child(&mut child);
            return Err(error);
        }
    };
    if let Err(error) = enable_even_lid_windows() {
        stop_child(&mut child);
        let _ = session::remove_if_matches(&saved);
        return Err(error);
    }
    print_start_confirmation(&saved, saved.note.as_deref());
    Ok(())
}

#[cfg(not(windows))]
fn ensure_sudo_for_even_lid() -> Result<()> {
    if !is_console() {
        return Err(AppError::fail(
            "--even-lid needs an interactive terminal for the sudo prompt",
        ));
    }
    if platform::refresh_sudo_non_interactive().unwrap_or(false) {
        return Ok(());
    }
    if !platform::authenticate_sudo()? {
        return Err(AppError::fail(
            "sudo authentication failed; --even-lid was not enabled",
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn start_lid_supervisor(trigger: &Trigger, no_display: bool, mode: &str) -> Result<()> {
    let supervisor_detail = trigger.detail();

    let prior = platform::read_disable_sleep()?;
    ensure_sudo_for_even_lid()?;
    write_lid_startup_recovery_record(trigger, mode, prior)?;

    match lid_enable_and_launch(trigger, no_display, &supervisor_detail, prior) {
        Ok(s) => {
            print_start_confirmation(&s, None);
            Ok(())
        }
        Err(e) => {
            if restore_disable_sleep_best_effort(prior)
                && let Err(cleanup) = finish_mac_lid_restore(prior)
            {
                eprintln!("wake: {cleanup}");
            }
            Err(e)
        }
    }
}

#[cfg(not(windows))]
fn lid_enable_and_launch(
    trigger: &Trigger,
    no_display: bool,
    supervisor_detail: &str,
    prior: i32,
) -> Result<Session> {
    platform::set_disable_sleep_foreground(1)?;
    let current = platform::read_disable_sleep()?;
    if current != 1 {
        restore_disable_sleep_foreground(prior)?;
        return Err(AppError::fail(format!(
            "failed to enable --even-lid; SleepDisabled is {current}"
        )));
    }
    let cmd = vec![
        sysutil::self_exe()?,
        "__supervise_lid__".into(),
        if no_display { "i" } else { "d" }.into(),
        legacy_timeout(trigger)
            .map(|timeout| timeout.to_string())
            .unwrap_or_default(),
        trigger
            .process()
            .map(|process| process.pid.to_string())
            .unwrap_or_default(),
        prior.to_string(),
        trigger.label().into(),
        supervisor_detail.to_string(),
        match trigger {
            Trigger::Charge { target, .. } => target.to_string(),
            _ => String::new(),
        },
    ];
    let mut child = sysutil::spawn_named(&cmd)?;
    match wait_for_session(&mut child, true) {
        Ok(saved) => Ok(saved),
        Err(error) => {
            stop_child(&mut child);
            Err(error)
        }
    }
}

#[cfg(not(windows))]
fn write_lid_startup_recovery_record(trigger: &Trigger, mode: &str, prior: i32) -> Result<()> {
    let now = Utc::now();
    let mut s = Session {
        pid: sysutil::current_pid(),
        mode: mode.to_string(),
        trigger: trigger.label().into(),
        detail: trigger.detail(),
        started_at: Some(now),
        ends_at: trigger.session_ends_at(now),
        even_lid: true,
        prior_disable_sleep: prior,
        ..Default::default()
    };
    s.capture_process_identity()?;
    session::write_lid_restore(&session::LidRestore::Macos {
        sleep_disabled: prior,
    })?;
    session::write(&s)
}

#[cfg(not(windows))]
fn verify_disable_sleep_restored_after_stop(s: &Session) -> Result<()> {
    for _ in 0..20 {
        if platform::read_disable_sleep()? == s.prior_disable_sleep {
            return finish_mac_lid_restore(s.prior_disable_sleep);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    if platform::read_disable_sleep()? == s.prior_disable_sleep {
        return finish_mac_lid_restore(s.prior_disable_sleep);
    }
    restore_disable_sleep_with_prompt_if_possible(
        s.prior_disable_sleep,
        "lid supervisor did not restore SleepDisabled, and no interactive terminal is available for sudo recovery",
    )?;
    let after = platform::read_disable_sleep()?;
    if after != s.prior_disable_sleep {
        print_sleep_restore_rescue(s.prior_disable_sleep);
        return Err(AppError::fail(format!(
            "failed to restore SleepDisabled to {}; current value is {after}",
            s.prior_disable_sleep
        )));
    }
    eprintln!(
        "wake: restored SleepDisabled to {} after lid supervisor exit",
        s.prior_disable_sleep
    );
    finish_mac_lid_restore(s.prior_disable_sleep)
}

#[cfg(not(windows))]
pub(crate) fn finish_mac_lid_restore(prior: i32) -> Result<()> {
    let expected = session::LidRestore::Macos {
        sleep_disabled: prior,
    };
    match session::read_lid_restore()? {
        None => session::remove_state_file(),
        Some(saved) if saved == expected => {
            session::remove_state_file()?;
            session::clear_lid_restore(&expected)
        }
        Some(_) => Err(AppError::fail("lid restoration marker does not match")),
    }
}

#[cfg(not(windows))]
fn restore_disable_sleep_foreground(prior: i32) -> Result<()> {
    if let Err(e) = platform::set_disable_sleep_foreground(prior) {
        print_sleep_restore_rescue(prior);
        return Err(e);
    }
    let after =
        platform::read_disable_sleep().inspect_err(|_| print_sleep_restore_rescue(prior))?;
    if after != prior {
        print_sleep_restore_rescue(prior);
        return Err(AppError::fail(format!(
            "failed to restore SleepDisabled to {prior}; current value is {after}"
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
fn restore_disable_sleep_with_prompt_if_possible(
    prior: i32,
    no_console_message: &str,
) -> Result<()> {
    if platform::set_disable_sleep_non_interactive(prior).unwrap_or(false)
        && platform::read_disable_sleep().ok() == Some(prior)
    {
        return Ok(());
    }
    if !is_console() {
        print_sleep_restore_rescue(prior);
        return Err(AppError::fail(no_console_message.to_string()));
    }
    restore_disable_sleep_foreground(prior)
}

#[cfg(not(windows))]
fn restore_disable_sleep_best_effort(prior: i32) -> bool {
    let _ = platform::set_disable_sleep_foreground(prior);
    platform::read_disable_sleep()
        .map(|c| c == prior)
        .unwrap_or(false)
}

#[cfg(not(windows))]
pub fn print_sleep_restore_rescue(value: i32) {
    eprintln!("wake: could not restore sleep; run: sudo pmset -a disablesleep {value}");
    eprintln!(
        "wake: recovery state: {}",
        session::lid_restore_file().display()
    );
}

#[cfg_attr(windows, allow(dead_code))]
fn live_marker_matches(saved: &Session, marker: Option<&session::LidRestore>) -> bool {
    match (saved.even_lid, marker) {
        (false, None) => true,
        (true, Some(session::LidRestore::Macos { sleep_disabled })) => {
            *sleep_disabled == saved.prior_disable_sleep
        }
        _ => false,
    }
}

pub fn recover_stale_lid_session_foreground() -> Result<()> {
    let _lock = session::acquire_lock()?;
    recover_stale_lid_session_unlocked()
}

pub fn recover_stale_lid_session_unlocked() -> Result<()> {
    #[cfg(windows)]
    return recover_stale_lid_session_windows();
    #[cfg(not(windows))]
    recover_stale_lid_session_unix()
}

#[cfg(windows)]
fn recover_stale_lid_session_windows() -> Result<()> {
    let saved = match session::read_saved_for_recovery()? {
        None => return Ok(()),
        Some(s) => s,
    };
    if saved.matches_live_process() {
        return Ok(());
    }
    if saved.even_lid {
        if !platform::supports_even_lid() {
            return Err(AppError::fail(
                "stale --even-lid session found, but this platform cannot restore the lid action",
            ));
        }
        recover_crashed_even_lid_windows(&saved)?;
    }
    session::remove_state_file()
}

#[cfg(not(windows))]
fn recover_stale_lid_session_unix() -> Result<()> {
    let marker = session::read_lid_restore()?;
    let saved = session::read_saved_for_recovery();
    if let Ok(Some(saved)) = &saved
        && saved.matches_live_process()
    {
        return if live_marker_matches(saved, marker.as_ref()) {
            Ok(())
        } else {
            Err(AppError::fail("live session and lid marker do not match"))
        };
    }

    match (marker, saved) {
        (
            Some(
                marker @ session::LidRestore::Macos {
                    sleep_disabled: prior,
                },
            ),
            _,
        ) => {
            recover_mac_lid_marker(prior)?;
            session::remove_state_file()?;
            session::clear_lid_restore(&marker)
        }
        (Some(session::LidRestore::Windows { .. }), _) => Err(AppError::fail(
            "Windows lid restoration marker found on this platform",
        )),
        (None, Err(error)) => Err(error),
        (None, Ok(None)) => Ok(()),
        (None, Ok(Some(saved))) if !saved.even_lid => session::remove_state_file(),
        (None, Ok(Some(_))) => Err(AppError::fail(
            "stale --even-lid session has no restoration marker",
        )),
    }
}

#[cfg(not(windows))]
fn recover_mac_lid_marker(prior: i32) -> Result<()> {
    if !platform::supports_even_lid() {
        return Err(AppError::fail(
            "lid restoration marker found, but this platform cannot restore the lid setting",
        ));
    }
    let current = platform::read_disable_sleep()?;
    if current == prior {
        return Ok(());
    }
    restore_disable_sleep_with_prompt_if_possible(
        prior,
        "crashed lid session needs sudo recovery, but no interactive terminal is available",
    )?;
    eprintln!("wake: recovered a crashed lid session");
    Ok(())
}

#[cfg(windows)]
fn recover_crashed_even_lid_windows(saved: &Session) -> Result<()> {
    let (ac, dc) = platform::decode_lid(saved.prior_disable_sleep);
    let current = platform::read_lid_action()?;
    if current == (ac, dc) {
        return Ok(());
    }
    set_lid(ac, dc)?;
    let after = platform::read_lid_action()?;
    if after != (ac, dc) {
        return Err(AppError::fail(format!(
            "failed to recover crashed lid session; lid action is AC={} DC={}",
            after.0, after.1
        )));
    }
    eprintln!("wake: recovered a crashed lid session; restored the prior lid action");
    Ok(())
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
            let error = match parse_start_args(&args) {
                Ok(_) => panic!("expected syntax error"),
                Err(error) => error,
            };
            assert!(error.message().contains(expected), "{}", error.message());
        }
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

    #[test]
    fn live_session_marker_consistency_table() {
        let matching = session::LidRestore::Macos { sleep_disabled: 1 };
        let wrong = session::LidRestore::Macos { sleep_disabled: 0 };
        let cases = [
            (false, None, true),
            (false, Some(&matching), false),
            (true, None, false),
            (true, Some(&matching), true),
            (true, Some(&wrong), false),
        ];
        for (even_lid, marker, expected) in cases {
            let saved = Session {
                even_lid,
                prior_disable_sleep: 1,
                ..Default::default()
            };
            assert_eq!(live_marker_matches(&saved, marker), expected);
        }
    }
}
