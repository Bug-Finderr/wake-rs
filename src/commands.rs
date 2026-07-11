//! Foreground commands: start / status / stop / forever, plus the macOS even-lid enable + crash
//! recovery machinery and shared formatting helpers.

use crate::error::{AppError, Result};
use crate::session::{self, Session};
use crate::supervisor::{plan_charge, read_battery_status};
use crate::sysutil;
use crate::{durations, platform};
use chrono::{DateTime, Duration, Local, Utc};
use std::io::IsTerminal;

// Used by the unix picker and the macOS sudo prompt; the Windows even-lid path never prompts.
#[cfg_attr(windows, allow(dead_code))]
pub fn is_console() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

// ---- start ----

struct Parsed {
    timeout_sec: Option<i64>,
    charge_target: Option<i32>,
    wait_pid: Option<u32>,
    trigger: String,
    trigger_detail: String,
    no_display: bool,
    even_lid: bool,
}

fn parse_start_args(args: &[String]) -> Result<Parsed> {
    let mut p = Parsed {
        timeout_sec: None,
        charge_target: None,
        wait_pid: None,
        trigger: "indefinite".into(),
        trigger_detail: "indefinite".into(),
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
                p.timeout_sec = Some(durations::parse(v)?);
                p.trigger_detail = v.clone();
                p.trigger = "timed".into();
                i += 1;
            }
            "--until" => {
                let v = next_value(args, i, "--until")?;
                trigger_flag = claim_trigger(trigger_flag, a)?;
                p.timeout_sec = Some(seconds_until(v)?);
                p.trigger_detail = format!("until {v}");
                p.trigger = "until-time".into();
                i += 1;
            }
            "--until-charge" => {
                let v = next_value(args, i, "--until-charge")?;
                trigger_flag = claim_trigger(trigger_flag, a)?;
                let target = parse_int(v, "--until-charge")?;
                if !(1..=100).contains(&target) {
                    return Err(AppError::usage("--until-charge must be 1-100"));
                }
                p.charge_target = Some(target);
                p.trigger_detail = format!("{target}%");
                p.trigger = "until-charge".into();
                i += 1;
            }
            "--while-pid" => {
                let v = next_value(args, i, "--while-pid")?;
                trigger_flag = claim_trigger(trigger_flag, a)?;
                let pid: u32 = v
                    .trim()
                    .parse()
                    .map_err(|_| AppError::fail(format!("invalid pid: '{v}'")))?;
                if !sysutil::is_alive(pid) {
                    return Err(AppError::usage(format!("pid {pid} is not running")));
                }
                p.wait_pid = Some(pid);
                p.trigger_detail = format!("pid {pid}");
                p.trigger = "while-pid".into();
                i += 1;
            }
            "--while-app" => {
                let v = next_value(args, i, "--while-app")?;
                trigger_flag = claim_trigger(trigger_flag, a)?;
                let pid = platform::find_app_pid(v)?
                    .ok_or_else(|| AppError::usage(format!("no running process matching '{v}'")))?;
                p.wait_pid = Some(pid);
                p.trigger_detail = format!("app '{v}' (pid {pid})");
                p.trigger = "while-app".into();
                i += 1;
            }
            "forever" | "indefinite" => {
                trigger_flag = claim_trigger(trigger_flag, a)?;
                p.trigger_detail = "indefinite".into();
                p.trigger = "indefinite".into();
            }
            other => {
                if other.starts_with('-') {
                    return Err(AppError::usage(format!("unknown flag: {other}")));
                }
                trigger_flag = claim_trigger(trigger_flag, "duration")?;
                p.timeout_sec = Some(durations::parse(other).map_err(|e| {
                    // A bare all-alphabetic token (e.g. `wake statsu`) is a mistyped subcommand, not a
                    // duration; report that instead of the misleading "invalid duration" error.
                    if !other.is_empty() && other.chars().all(|c| c.is_ascii_alphabetic()) {
                        AppError::usage(format!("unknown command or duration: {other}"))
                    } else {
                        e
                    }
                })?);
                p.trigger_detail = other.to_string();
                p.trigger = "timed".into();
            }
        }
        i += 1;
    }
    Ok(p)
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
    let p = parse_start_args(args)?;

    if p.even_lid && !platform::supports_even_lid() {
        return Err(AppError::usage(even_lid_unsupported_message()));
    }

    let _lock = session::acquire_lock()?;
    recover_stale_lid_session_unlocked()?;
    if let Some(existing) = session::read_if_alive()? {
        eprintln!(
            "wake: session already active (pid {}, {} {})",
            existing.pid, existing.trigger, existing.detail
        );
        eprintln!("run 'wake stop' first");
        std::process::exit(1);
    }

    let mode = if p.no_display {
        "system-only"
    } else {
        "display+system"
    }
    .to_string();

    // macOS/Linux route even-lid through the lid supervisor (sudo + SleepDisabled). Windows instead
    // overlays the power-plan lid action onto the normal session, so it falls through to the standard
    // start path below and only diverges to set/restore the lid action.
    #[cfg(not(windows))]
    {
        if let Some(charge) = p.charge_target {
            if p.even_lid {
                return start_lid_supervisor(&p, &mode, Some(charge));
            }
            return start_charge_supervisor(charge, &mode, p.no_display);
        }
        if p.even_lid {
            return start_lid_supervisor(&p, &mode, None);
        }
    }

    #[cfg(windows)]
    let prior_lid = if p.even_lid {
        Some(platform::read_lid_action()?)
    } else {
        None
    };

    if let Some(charge) = p.charge_target {
        #[cfg(windows)]
        return start_charge_supervisor_windows(charge, &mode, p.no_display, prior_lid);
        #[cfg(not(windows))]
        return start_charge_supervisor(charge, &mode, p.no_display);
    }

    let ka = platform::keep_awake_command(p.no_display, p.timeout_sec, p.wait_pid)?;
    let now = Utc::now();
    let mut s = Session {
        mode,
        trigger: p.trigger.clone(),
        detail: p.trigger_detail.clone(),
        started_at: Some(now),
        ends_at: p.timeout_sec.map(|t| now + Duration::seconds(t)),
        ..Default::default()
    };
    #[cfg(windows)]
    if let Some((ac, dc)) = prior_lid {
        s.even_lid = true;
        s.prior_disable_sleep = platform::encode_lid(ac, dc);
    }

    let mut child = sysutil::spawn_named(&ka.cmd)?;
    s.pid = child.id();
    sysutil::require_child_alive(s.pid, &ka.cmd);
    if let Err(e) = s
        .capture_process_identity()
        .and_then(|_| session::write(&s))
    {
        let _ = child.kill();
        return Err(e);
    }

    #[cfg(windows)]
    if s.even_lid
        && let Err(e) = enable_even_lid_windows()
    {
        let _ = child.kill();
        session::delete_state_file();
        return Err(e);
    }

    print_start_confirmation(&s, ka.note.as_deref());
    Ok(())
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

#[cfg(not(windows))]
fn start_charge_supervisor(charge: i32, mode: &str, no_display: bool) -> Result<()> {
    let status = read_battery_status()?;
    let plan = plan_charge(charge, &status)?;
    if plan.already_met {
        println!(
            "wake: battery already at {}%; target {charge}% reached",
            status.percent
        );
        return Ok(());
    }
    let cmd = vec![
        sysutil::self_exe()?,
        "__supervise_charge__".into(),
        charge.to_string(),
        no_display.to_string(),
        mode.to_string(),
    ];
    let _child = sysutil::spawn_named(&cmd)?;
    wait_for_state_file();
    match session::read_if_alive()? {
        Some(s) => print_start_confirmation(&s, None),
        None => {
            eprintln!("wake: supervisor failed to start");
            std::process::exit(1);
        }
    }
    Ok(())
}

fn wait_for_state_file() {
    for _ in 0..30 {
        if session::state_file().exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

// ---- status / stop ----

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
    let _lock = session::acquire_lock()?;
    recover_stale_lid_session_unlocked()?;
    let Some(s) = session::read_if_alive()? else {
        println!("wake: no active session");
        session::delete_state_file();
        return Ok(());
    };
    sysutil::terminate(s.pid);
    if s.even_lid {
        #[cfg(windows)]
        restore_even_lid_windows(&s)?;
        #[cfg(not(windows))]
        verify_disable_sleep_restored_after_stop(&s)?;
    }
    session::delete_state_file();
    println!("wake: stopped (pid {}, {})", s.pid, s.trigger);
    Ok(())
}

// ---- start confirmation + formatting ----

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

fn seconds_until(hhmm: &str) -> Result<i64> {
    let parts: Vec<&str> = hhmm.split(':').collect();
    if parts.len() != 2 {
        return Err(AppError::usage(format!(
            "--until expects HH:MM, got '{hhmm}'"
        )));
    }
    let h = parse_int(parts[0], "--until hour")?;
    let m = parse_int(parts[1], "--until minute")?;
    if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
        return Err(AppError::usage(format!("--until: invalid time '{hhmm}'")));
    }
    let now = Local::now();
    // h/m were range-checked to 0..=23 / 0..=59 just above, so this is always Some.
    let time = chrono::NaiveTime::from_hms_opt(h as u32, m as u32, 0).expect("valid HH:MM");
    let naive = now.date_naive().and_time(time);
    let mut target = match naive.and_local_timezone(Local) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => t,
        chrono::LocalResult::None => now,
    };
    if target <= now {
        target += Duration::days(1);
    }
    Ok((target - now).num_seconds())
}

fn parse_int(s: &str, name: &str) -> Result<i32> {
    s.trim()
        .parse::<i32>()
        .map_err(|_| AppError::usage(format!("{name}: not an integer: '{s}'")))
}

fn even_lid_unsupported_message() -> String {
    "--even-lid is unsupported on Linux; lid-switch inhibition is handled through systemd when privileged".into()
}

// ---- even-lid: power-plan lid action (Windows) ----

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

/// Windows charge + even-lid: spawn the normal charge supervisor (carrying the encoded prior lid
/// action so teardown can restore it), then set the lid action once the session is published.
#[cfg(windows)]
fn start_charge_supervisor_windows(
    charge: i32,
    mode: &str,
    no_display: bool,
    prior_lid: Option<(u32, u32)>,
) -> Result<()> {
    let status = read_battery_status()?;
    let plan = plan_charge(charge, &status)?;
    if plan.already_met {
        println!(
            "wake: battery already at {}%; target {charge}% reached",
            status.percent
        );
        return Ok(());
    }
    let prior_encoded = prior_lid.map(|(ac, dc)| platform::encode_lid(ac, dc));
    let cmd = vec![
        sysutil::self_exe()?,
        "__supervise_charge__".into(),
        charge.to_string(),
        no_display.to_string(),
        mode.to_string(),
        prior_encoded.map(|v| v.to_string()).unwrap_or_default(),
    ];
    let _child = sysutil::spawn_named(&cmd)?;
    wait_for_state_file();
    let Some(s) = session::read_if_alive()? else {
        eprintln!("wake: supervisor failed to start");
        std::process::exit(1);
    };
    if prior_lid.is_some()
        && let Err(e) = enable_even_lid_windows()
    {
        sysutil::terminate(s.pid);
        session::delete_state_file();
        return Err(e);
    }
    print_start_confirmation(&s, None);
    Ok(())
}

// ---- even-lid: sudo + SleepDisabled (macOS) ----

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
fn start_lid_supervisor(p: &Parsed, mode: &str, charge_target: Option<i32>) -> Result<()> {
    let mut supervisor_detail = p.trigger_detail.clone();
    if let Some(target) = charge_target {
        let status = read_battery_status()?;
        let plan = plan_charge(target, &status)?;
        if plan.already_met {
            println!(
                "wake: battery already at {}%; target {target}% reached",
                status.percent
            );
            return Ok(());
        }
        supervisor_detail = format!(
            "{target}% (was {}%, {})",
            status.percent,
            if plan.charging_up {
                "charging up"
            } else {
                "discharging down"
            }
        );
    }

    let prior = platform::read_disable_sleep()?;
    ensure_sudo_for_even_lid()?;
    write_lid_startup_recovery_record(&p.trigger, &supervisor_detail, mode, p.timeout_sec, prior)?;

    match lid_enable_and_launch(p, mode, &supervisor_detail, charge_target, prior) {
        Ok(s) => {
            print_start_confirmation(&s, None);
            Ok(())
        }
        Err(e) => {
            if restore_disable_sleep_best_effort(prior) {
                if let Err(cleanup) = finish_mac_lid_restore(prior, true) {
                    eprintln!("wake: {cleanup}");
                }
            }
            Err(e)
        }
    }
}

#[cfg(not(windows))]
fn lid_enable_and_launch(
    p: &Parsed,
    _mode: &str,
    supervisor_detail: &str,
    charge_target: Option<i32>,
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
        if p.no_display { "i" } else { "d" }.into(),
        p.timeout_sec.map(|t| t.to_string()).unwrap_or_default(),
        p.wait_pid.map(|w| w.to_string()).unwrap_or_default(),
        prior.to_string(),
        p.trigger.clone(),
        supervisor_detail.to_string(),
        charge_target.map(|c| c.to_string()).unwrap_or_default(),
    ];
    let child = sysutil::spawn_named(&cmd)?;
    wait_for_supervisor_session(child.id(), true)?
        .ok_or_else(|| AppError::fail("lid supervisor failed to publish session state"))
}

#[cfg(not(windows))]
fn write_lid_startup_recovery_record(
    trigger: &str,
    detail: &str,
    mode: &str,
    timeout_sec: Option<i64>,
    prior: i32,
) -> Result<()> {
    let now = Utc::now();
    let mut s = Session::default();
    s.pid = sysutil::current_pid();
    s.mode = mode.to_string();
    s.trigger = trigger.to_string();
    s.detail = detail.to_string();
    s.started_at = Some(now);
    s.ends_at = timeout_sec.map(|t| now + Duration::seconds(t));
    s.even_lid = true;
    s.prior_disable_sleep = prior;
    s.capture_process_identity()?;
    session::write_lid_restore(&session::LidRestore::Macos {
        sleep_disabled: prior,
    })?;
    session::write(&s)
}

#[cfg(not(windows))]
fn wait_for_supervisor_session(supervisor_pid: u32, even_lid: bool) -> Result<Option<Session>> {
    for _ in 0..50 {
        if let Some(s) = session::read_if_alive()?
            && s.pid == supervisor_pid
            && s.even_lid == even_lid
        {
            return Ok(Some(s));
        }
        if !sysutil::is_alive(supervisor_pid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(None)
}

#[cfg(not(windows))]
fn verify_disable_sleep_restored_after_stop(s: &Session) -> Result<()> {
    for _ in 0..20 {
        if platform::read_disable_sleep()? == s.prior_disable_sleep {
            return finish_mac_lid_restore(s.prior_disable_sleep, true);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    if platform::read_disable_sleep()? == s.prior_disable_sleep {
        return finish_mac_lid_restore(s.prior_disable_sleep, true);
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
    finish_mac_lid_restore(s.prior_disable_sleep, true)
}

#[cfg(not(windows))]
pub(crate) fn finish_mac_lid_restore(prior: i32, remove_session: bool) -> Result<()> {
    let expected = session::LidRestore::Macos {
        sleep_disabled: prior,
    };
    match marker_clear_action(session::read_lid_restore()?.as_ref(), &expected)? {
        MarkerClearAction::AlreadyCleared => {
            if remove_session {
                session::remove_state_file()?;
            }
            Ok(())
        }
        MarkerClearAction::Clear => {
            finish_restored_state(remove_session, session::remove_state_file, || {
                session::clear_lid_restore(&expected)
            })
        }
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
    print_sleep_state_path();
}

#[cfg(not(windows))]
fn print_sleep_state_path() {
    eprintln!(
        "wake: recovery state: {}",
        session::lid_restore_file().display()
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MarkerClearAction {
    AlreadyCleared,
    Clear,
}

#[cfg_attr(windows, allow(dead_code))]
fn marker_clear_action(
    saved: Option<&session::LidRestore>,
    expected: &session::LidRestore,
) -> Result<MarkerClearAction> {
    match saved {
        None => Ok(MarkerClearAction::AlreadyCleared),
        Some(saved) if saved == expected => Ok(MarkerClearAction::Clear),
        Some(_) => Err(AppError::fail("lid restoration marker does not match")),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(windows, allow(dead_code))]
enum RecoverySession {
    Missing,
    Live,
    StalePlain,
    StaleLid,
    Malformed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryAction {
    None,
    RemoveSession,
    RestoreAndRemoveSession,
    RestoreAndRetainSession,
    MissingMarker,
    ReportMalformed,
}

#[cfg_attr(windows, allow(dead_code))]
fn recovery_action(has_marker: bool, saved: RecoverySession) -> RecoveryAction {
    match (has_marker, saved) {
        (_, RecoverySession::Live) | (false, RecoverySession::Missing) => RecoveryAction::None,
        (true, RecoverySession::Malformed) => RecoveryAction::RestoreAndRetainSession,
        (true, _) => RecoveryAction::RestoreAndRemoveSession,
        (false, RecoverySession::StalePlain) => RecoveryAction::RemoveSession,
        (false, RecoverySession::StaleLid) => RecoveryAction::MissingMarker,
        (false, RecoverySession::Malformed) => RecoveryAction::ReportMalformed,
    }
}

#[cfg_attr(windows, allow(dead_code))]
fn finish_restored_state(
    remove_session: bool,
    remove: impl FnOnce() -> Result<()>,
    clear: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if remove_session {
        remove()?;
    }
    clear()
}

// ---- crash recovery ----

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
    let prior = match marker.as_ref() {
        Some(session::LidRestore::Macos { sleep_disabled }) => Some(*sleep_disabled),
        Some(session::LidRestore::Windows { .. }) => {
            return Err(AppError::fail(format!(
                "Windows lid restoration marker found on this platform at {}",
                session::lid_restore_file().display()
            )));
        }
        None => None,
    };
    let saved = session::read_saved_for_recovery();
    let state = match &saved {
        Ok(None) => RecoverySession::Missing,
        Ok(Some(saved)) if saved.matches_live_process() => RecoverySession::Live,
        Ok(Some(saved)) if saved.even_lid => RecoverySession::StaleLid,
        Ok(Some(_)) => RecoverySession::StalePlain,
        Err(_) => RecoverySession::Malformed,
    };

    match recovery_action(prior.is_some(), state) {
        RecoveryAction::None => Ok(()),
        RecoveryAction::RemoveSession => session::remove_state_file(),
        RecoveryAction::MissingMarker => Err(AppError::fail(format!(
            "stale --even-lid session has no restoration marker at {}",
            session::lid_restore_file().display()
        ))),
        RecoveryAction::ReportMalformed => saved.map(|_| ()),
        RecoveryAction::RestoreAndRemoveSession => {
            let prior = prior.expect("recovery action requires marker");
            recover_mac_lid_marker(prior)?;
            finish_mac_lid_restore(prior, true)
        }
        RecoveryAction::RestoreAndRetainSession => {
            let prior = prior.expect("recovery action requires marker");
            recover_mac_lid_marker(prior)?;
            finish_mac_lid_restore(prior, false)?;
            eprintln!(
                "wake: restored sleep settings; retained malformed session state at {}",
                session::state_file().display()
            );
            saved.map(|_| ())
        }
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
    if current != prior {
        restore_disable_sleep_with_prompt_if_possible(
            prior,
            "crashed lid session needs sudo recovery, but no interactive terminal is available",
        )?;
        let after = platform::read_disable_sleep()?;
        if after != prior {
            print_sleep_restore_rescue(prior);
            return Err(AppError::fail(format!(
                "failed to recover crashed lid session; SleepDisabled is {after}"
            )));
        }
        if prior == 0 {
            eprintln!("wake: recovered a crashed lid session; restored normal sleep");
        } else {
            eprintln!("wake: recovered a crashed lid session; restored prior SleepDisabled value");
        }
    }
    Ok(())
}

/// Crash recovery for a stale Windows even-lid session: re-elevate and restore the prior lid action.
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
    use std::cell::RefCell;

    fn marker(prior: i32) -> session::LidRestore {
        session::LidRestore::Macos {
            sleep_disabled: prior,
        }
    }

    #[test]
    fn missing_marker_is_an_idempotent_completed_clear() {
        assert_eq!(
            marker_clear_action(None, &marker(0)).unwrap(),
            MarkerClearAction::AlreadyCleared
        );
    }

    #[test]
    fn mismatched_marker_is_not_treated_as_completed() {
        assert!(marker_clear_action(Some(&marker(1)), &marker(0)).is_err());
    }

    #[test]
    fn marker_drives_recovery_without_a_session() {
        assert_eq!(
            recovery_action(true, RecoverySession::Missing),
            RecoveryAction::RestoreAndRemoveSession
        );
    }

    #[test]
    fn marker_restores_but_retains_a_malformed_session() {
        assert_eq!(
            recovery_action(true, RecoverySession::Malformed),
            RecoveryAction::RestoreAndRetainSession
        );
    }

    #[test]
    fn stale_lid_session_without_marker_is_not_guessed() {
        assert_eq!(
            recovery_action(false, RecoverySession::StaleLid),
            RecoveryAction::MissingMarker
        );
    }

    #[test]
    fn restored_state_removes_session_before_marker() {
        let events = RefCell::new(Vec::new());

        finish_restored_state(
            true,
            || {
                events.borrow_mut().push("session");
                Ok(())
            },
            || {
                events.borrow_mut().push("marker");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(*events.borrow(), ["session", "marker"]);
    }
}
