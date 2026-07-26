use crate::error::{AppError, Result};
use crate::session::{self, Session};
#[cfg(any(target_os = "macos", windows))]
use crate::supervisor::charge_detail;
use crate::supervisor::{ChargePreparation, prepare_charge};
use crate::sysutil;
use crate::{durations, platform};
use chrono::{DateTime, Duration, Local, NaiveDateTime, NaiveTime, TimeZone, Utc};
#[cfg(target_os = "macos")]
use std::io::IsTerminal;
#[cfg(windows)]
use std::process::Child;
#[cfg(windows)]
use std::time::{Duration as StdDuration, Instant};

#[cfg(target_os = "macos")]
pub fn is_console() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

struct Parsed {
    timeout_sec: Option<i64>,
    until_deadline: Option<DateTime<Utc>>,
    charge_target: Option<i32>,
    wait_pid: Option<u32>,
    #[cfg(windows)]
    wait_start: Option<u64>,
    trigger: String,
    trigger_detail: String,
    no_display: bool,
    even_lid: bool,
}

struct LaunchLifetime {
    started_at: DateTime<Utc>,
    ends_at: Option<DateTime<Utc>>,
}

pub(crate) fn deadline_still_pending(deadline: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now < deadline
}

fn launch_lifetime(
    timeout_sec: Option<i64>,
    until_deadline: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<LaunchLifetime> {
    if let Some(deadline) = until_deadline {
        if !deadline_still_pending(deadline, now) {
            return None;
        }
        return Some(LaunchLifetime {
            started_at: now,
            ends_at: Some(deadline),
        });
    }
    Some(LaunchLifetime {
        started_at: now,
        ends_at: timeout_sec.map(|timeout| now + Duration::seconds(timeout)),
    })
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn sleep_restore_needed(prior: i32, current: i32) -> bool {
    prior == 0 && current != 0
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn sleep_restored(prior: i32, current: i32) -> bool {
    !sleep_restore_needed(prior, current)
}

fn parse_start_args(args: &[String]) -> Result<Parsed> {
    let mut p = Parsed {
        timeout_sec: None,
        until_deadline: None,
        charge_target: None,
        wait_pid: None,
        #[cfg(windows)]
        wait_start: None,
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
            "-h" | "--help" | "help" | "-v" | "--version" | "version" => {
                return Err(AppError::usage("help and version must be used alone"));
            }
            "--no-display" => claim_boolean(&mut p.no_display, a)?,
            "--even-lid" => claim_boolean(&mut p.even_lid, a)?,
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
                p.until_deadline = Some(until_deadline(v)?);
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
                    .map_err(|_| AppError::usage(format!("invalid pid: '{v}'")))?;
                #[cfg(windows)]
                {
                    p.wait_start = Some(sysutil::open_process_for_wait(pid)?.identity().start);
                }
                #[cfg(not(windows))]
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
                #[cfg(windows)]
                {
                    p.wait_start = Some(sysutil::open_process_for_wait(pid)?.identity().start);
                }
                p.wait_pid = Some(pid);
                p.trigger_detail = format!("app '{v}' (pid {pid})");
                p.trigger = "while-app".into();
                i += 1;
            }
            "forever" | "indefinite" => {
                trigger_flag = claim_trigger(trigger_flag, a)?;
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

fn claim_boolean(value: &mut bool, flag: &str) -> Result<()> {
    if *value {
        Err(AppError::usage(format!("duplicate flag: {flag}")))
    } else {
        *value = true;
        Ok(())
    }
}

pub fn start(args: &[String]) -> Result<()> {
    let parsed = parse_start_args(args)?;
    #[cfg(windows)]
    return start_windows(parsed);
    #[cfg(not(windows))]
    start_unix(parsed)
}

#[cfg(not(windows))]
fn start_unix(p: Parsed) -> Result<()> {
    let _lock = session::acquire_lock()?;
    recover_stale_lid_session_unlocked()?;
    if let Some(existing) = session::read_if_alive(true) {
        return Err(AppError::fail(format!(
            "session already active (pid {}, {} {}); run 'wake stop' first",
            existing.pid, existing.trigger, existing.detail
        )));
    }
    let mode = session::mode_for(p.no_display).to_string();
    if let Some(charge) = p.charge_target {
        #[cfg(target_os = "macos")]
        if p.even_lid {
            return start_lid_supervisor(&p, Some(charge));
        }
        return start_charge_supervisor(charge, p.no_display, p.even_lid);
    }
    #[cfg(target_os = "macos")]
    if p.even_lid {
        return start_lid_supervisor(&p, None);
    }
    if p.until_deadline.is_some() {
        return start_until_supervisor(&p);
    }

    #[cfg(target_os = "linux")]
    let prepared = platform::prepare_keep_awake(p.no_display, p.even_lid)?;
    let lifetime = launch_lifetime(p.timeout_sec, None, Utc::now())
        .expect("relative and indefinite lifetimes are always pending");
    #[cfg(target_os = "linux")]
    let keep_awake = prepared.command(p.timeout_sec, p.wait_pid);
    #[cfg(target_os = "macos")]
    let keep_awake = platform::keep_awake_command(p.no_display, p.timeout_sec, p.wait_pid)?;
    let mut child = sysutil::spawn_named(&keep_awake.cmd)?;
    sysutil::require_child_alive(&mut child, &keep_awake.cmd)?;
    let mut saved = Session {
        pid: child.id(),
        mode,
        trigger: p.trigger,
        detail: p.trigger_detail,
        started_at: Some(lifetime.started_at),
        ends_at: lifetime.ends_at,
        even_lid: p.even_lid,
        ..Session::default()
    };
    if let Err(error) = saved
        .capture_process_identity()
        .and_then(|()| session::write(&saved))
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    print_start_confirmation(&saved, keep_awake.note.as_deref());
    Ok(())
}

#[cfg(not(windows))]
fn start_until_supervisor(p: &Parsed) -> Result<()> {
    let deadline = p.until_deadline.expect("until route requires a deadline");
    if !deadline_still_pending(deadline, Utc::now()) {
        print_deadline_elapsed();
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    let prepared = match platform::prepare_keep_awake(p.no_display, p.even_lid) {
        Ok(prepared) => prepared,
        Err(_) if !deadline_still_pending(deadline, Utc::now()) => {
            print_deadline_elapsed();
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    #[cfg(target_os = "linux")]
    let (inhibitor_scope, note) = (prepared.scope(), prepared.note().map(str::to_string));
    #[cfg(target_os = "macos")]
    let (inhibitor_scope, note) = ("", None::<String>);
    let command = until_supervisor_command(
        deadline,
        p.no_display,
        &p.trigger_detail,
        p.even_lid,
        inhibitor_scope,
    )?;
    let mut child = sysutil::spawn_named(&command)?;
    if let Some(published) = wait_for_supervisor_session(&mut child, p.even_lid)? {
        if !deadline_still_pending(deadline, Utc::now()) {
            terminate_owned_child(&mut child)?;
            recover_stale_lid_session_unlocked()?;
            print_deadline_elapsed();
            return Ok(());
        }
        print_start_confirmation(&published, note.as_deref());
        return Ok(());
    }
    child
        .try_wait()?
        .ok_or_else(|| AppError::fail("supervisor cleanup did not reap the owned process"))?;
    let expired = !deadline_still_pending(deadline, Utc::now());
    recover_stale_lid_session_unlocked()?;
    if expired {
        print_deadline_elapsed();
        Ok(())
    } else {
        Err(AppError::fail("supervisor failed to publish session state"))
    }
}

#[cfg(not(windows))]
fn until_supervisor_command(
    deadline: DateTime<Utc>,
    no_display: bool,
    detail: &str,
    even_lid: bool,
    inhibitor_scope: &str,
) -> Result<Vec<String>> {
    Ok(vec![
        sysutil::self_exe()?,
        "__supervise_until__".into(),
        deadline.to_rfc3339(),
        no_display.to_string(),
        detail.to_string(),
        even_lid.to_string(),
        inhibitor_scope.to_string(),
    ])
}

#[cfg(not(windows))]
fn start_charge_supervisor(target: i32, no_display: bool, even_lid: bool) -> Result<()> {
    // Strict Linux policy errors must reach the foreground before battery probing can mask them.
    #[cfg(target_os = "linux")]
    let strict_preflight = if even_lid {
        Some(platform::prepare_keep_awake(no_display, true)?)
    } else {
        None
    };
    let charge = match prepare_charge(target)? {
        ChargePreparation::AlreadyMet(percent) => {
            print_charge_met(percent, target);
            return Ok(());
        }
        ChargePreparation::Wait(charge) => charge,
    };
    // The supervisor receives the exact foreground-selected scope, while its paired degradation
    // note stays available to the foreground confirmation.
    #[cfg(target_os = "linux")]
    let prepared = match strict_preflight {
        Some(prepared) => prepared,
        None => platform::prepare_keep_awake(no_display, false)?,
    };
    #[cfg(target_os = "linux")]
    let launch = charge_supervisor_launch(
        target,
        no_display,
        even_lid,
        prepared.scope(),
        prepared.note(),
    )?;
    #[cfg(target_os = "macos")]
    let launch = charge_supervisor_launch(target, no_display, even_lid, "", None)?;
    let mut child = sysutil::spawn_named(&launch.command)?;
    if let Some(published) = wait_for_supervisor_session(&mut child, even_lid)? {
        print_start_confirmation(&published, launch.note.as_deref());
        return Ok(());
    }
    let status = child
        .try_wait()?
        .ok_or_else(|| AppError::fail("supervisor cleanup did not reap the owned process"))?;
    if status.success()
        && let Some(percent) = current_charge_met(target, charge.charging_up)
    {
        print_charge_met(percent, target);
        return Ok(());
    }
    recover_stale_lid_session_unlocked()?;
    Err(AppError::fail("supervisor failed to publish session state"))
}

#[cfg(not(windows))]
struct ChargeSupervisorLaunch {
    command: Vec<String>,
    note: Option<String>,
}

#[cfg(not(windows))]
fn charge_supervisor_launch(
    target: i32,
    no_display: bool,
    even_lid: bool,
    inhibitor_scope: &str,
    note: Option<&str>,
) -> Result<ChargeSupervisorLaunch> {
    Ok(ChargeSupervisorLaunch {
        command: vec![
            sysutil::self_exe()?,
            "__supervise_charge__".into(),
            target.to_string(),
            no_display.to_string(),
            even_lid.to_string(),
            inhibitor_scope.to_string(),
        ],
        note: note.map(str::to_string),
    })
}

#[cfg(not(windows))]
fn current_charge_met(target: i32, charging_up: bool) -> Option<i32> {
    platform::read_battery()
        .ok()
        .map(|status| status.percent)
        .filter(|&percent| charge_target_met(target, charging_up, percent))
}

#[cfg(any(not(windows), test))]
fn charge_target_met(target: i32, charging_up: bool, percent: i32) -> bool {
    if charging_up {
        percent >= target
    } else {
        percent <= target
    }
}

fn print_charge_met(percent: i32, target: i32) {
    println!("wake: battery already at {percent}%; target {target}% reached");
}

fn print_deadline_elapsed() {
    println!("wake: requested end time passed during startup");
}

pub fn status() -> Result<()> {
    #[cfg(windows)]
    return status_windows();
    #[cfg(not(windows))]
    status_unix()
}

#[cfg(not(windows))]
fn status_unix() -> Result<()> {
    let _lock = session::acquire_lock()?;
    recover_stale_lid_session_unlocked()?;
    let Some(saved) = session::read_if_alive(false) else {
        println!("wake: no active session");
        return Ok(());
    };
    print_status(&saved);
    Ok(())
}

pub fn stop() -> Result<()> {
    #[cfg(windows)]
    return stop_windows();
    #[cfg(not(windows))]
    stop_unix()
}

#[cfg(not(windows))]
fn stop_unix() -> Result<()> {
    let _lock = session::acquire_lock()?;
    recover_stale_lid_session_unlocked()?;
    let Some(saved) = session::read_if_alive(false) else {
        println!("wake: no active session");
        session::delete_state_file()?;
        return Ok(());
    };
    sysutil::terminate_session(&saved)?;
    #[cfg(target_os = "macos")]
    if saved.even_lid {
        verify_disable_sleep_restored_after_stop(&saved)?;
    }
    session::delete_state_file()?;
    println!("wake: stopped (pid {}, {})", saved.pid, saved.trigger);
    Ok(())
}

fn print_status(saved: &Session) {
    let now = Utc::now();
    let started = saved.started_at.unwrap_or(now);
    let elapsed = (now - started).num_seconds();
    let remaining = match saved.ends_at {
        None => "-".to_string(),
        Some(end) => pretty_duration((end - now).num_seconds().max(0)),
    };
    println!("wake: session active (pid {})", saved.pid);
    println!("  mode      : {}", saved.mode);
    println!("  trigger   : {} ({})", saved.trigger, saved.detail);
    println!(
        "  started   : {} ({} ago)",
        hms(started),
        pretty_duration(elapsed)
    );
    println!("  remaining : {remaining}");
    #[cfg(target_os = "macos")]
    if saved.even_lid {
        if saved.prior_disable_sleep == 0 {
            println!(
                "  even lid  : active (restore SleepDisabled=0, state {})",
                session::state_file().display()
            );
        } else {
            println!(
                "  even lid  : active (SleepDisabled was already 1; no setting change owned, state {})",
                session::state_file().display()
            );
        }
    }
    #[cfg(target_os = "linux")]
    if saved.even_lid {
        println!("  even lid  : logind inhibitor active");
    }
}

fn start_confirmation_lines(s: &Session, note: Option<&str>) -> Vec<String> {
    let started = s.started_at.map(hms).unwrap_or_else(|| "-".into());
    let ends = s.ends_at.map(hms).unwrap_or_else(|| "-".into());
    let mut lines = vec![
        format!("wake: session active (pid {})", s.pid),
        format!("  mode    : {}", s.mode),
        format!("  trigger : {} ({})", s.trigger, s.detail),
        format!("  started : {started}"),
        format!("  ends    : {ends}"),
    ];
    if s.even_lid {
        #[cfg(windows)]
        lines.push(
            "note: --even-lid override verified; use 'wake status' to detect later power changes"
                .into(),
        );
        #[cfg(target_os = "macos")]
        lines.push(
            "note: --even-lid is active; this Mac should stay awake with the lid closed until the session ends"
                .into(),
        );
        #[cfg(target_os = "linux")]
        lines.push(
            "note: --even-lid holds a logind inhibitor for lid-switch handling; privileged or non-logind suspend paths are not covered"
                .into(),
        );
        lines.push(
            "caution: closed lid + battery + no external display can run hot and drain quickly"
                .into(),
        );
    } else if let Some(note) = note
        .map(str::to_string)
        .or_else(platform::static_start_note)
    {
        lines.push(note);
    }
    lines
}

fn print_start_confirmation(s: &Session, note: Option<&str>) {
    for line in start_confirmation_lines(s, note) {
        println!("{line}");
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

fn until_deadline(hhmm: &str) -> Result<DateTime<Utc>> {
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
    let time = NaiveTime::from_hms_opt(h as u32, m as u32, 0).expect("validated HH:MM");
    let target = next_wall_time(&now, time, |wall| Local.from_local_datetime(&wall))?;
    Ok(target.with_timezone(&Utc))
}

fn next_wall_time<Tz: TimeZone>(
    now: &DateTime<Tz>,
    time: NaiveTime,
    mut localize: impl FnMut(NaiveDateTime) -> chrono::LocalResult<DateTime<Tz>>,
) -> Result<DateTime<Tz>> {
    let today = now.date_naive();
    let tomorrow = today
        .succ_opt()
        .ok_or_else(|| AppError::fail("cannot resolve tomorrow's calendar date"))?;
    for date in [today, tomorrow] {
        if let Some(target) = wall_candidates(date.and_time(time), &mut localize)?
            .into_iter()
            .filter(|candidate| candidate > now)
            .min()
        {
            return Ok(target);
        }
    }
    Err(AppError::fail("cannot resolve the next --until time"))
}

fn wall_candidates<Tz: TimeZone>(
    requested: NaiveDateTime,
    localize: &mut impl FnMut(NaiveDateTime) -> chrono::LocalResult<DateTime<Tz>>,
) -> Result<Vec<DateTime<Tz>>> {
    for seconds in 0..=3 * 60 * 60 {
        match localize(requested + Duration::seconds(seconds)) {
            chrono::LocalResult::Single(time) => return Ok(vec![time]),
            chrono::LocalResult::Ambiguous(first, second) => return Ok(vec![first, second]),
            chrono::LocalResult::None => {}
        }
    }
    Err(AppError::fail(
        "cannot resolve --until local time within three hours",
    ))
}

fn parse_int(s: &str, name: &str) -> Result<i32> {
    s.trim()
        .parse::<i32>()
        .map_err(|_| AppError::usage(format!("{name}: not an integer: '{s}'")))
}

#[cfg(windows)]
enum WindowsState {
    None,
    Active {
        saved: Session,
        worker: sysutil::ProcessHandle,
        guardian: Option<sysutil::ProcessHandle>,
    },
    BrokenGuardian {
        saved: Session,
        worker: sysutil::ProcessHandle,
    },
}

#[cfg(windows)]
fn start_windows(mut parsed: Parsed) -> Result<()> {
    let state_path = session::state_file();
    let _lock = session::acquire_lock()?;
    match reconcile_windows(false)? {
        WindowsState::None => {}
        WindowsState::Active { saved, .. } => {
            return Err(AppError::fail(format!(
                "session already active (pid {}, {} {}); run 'wake stop' first",
                saved.pid, saved.trigger, saved.detail
            )));
        }
        WindowsState::BrokenGuardian { saved, .. } => return Err(broken_guardian_error(&saved)),
    }

    let charge = match parsed.charge_target {
        Some(target) => match prepare_charge(target)? {
            ChargePreparation::AlreadyMet(percent) => {
                print_charge_met(percent, target);
                return Ok(());
            }
            ChargePreparation::Wait(charge) => {
                parsed.trigger_detail = charge_detail(target, &charge);
                Some((target, charge.charging_up))
            }
        },
        None => None,
    };

    let target = parsed.wait_pid.zip(parsed.wait_start);
    let snapshot = if parsed.even_lid {
        Some(platform::capture_lid_snapshot()?)
    } else {
        None
    };
    let Some(lifetime) = launch_lifetime(parsed.timeout_sec, parsed.until_deadline, Utc::now())
    else {
        print_deadline_elapsed();
        return Ok(());
    };
    let mut saved = Session {
        mode: session::mode_for(parsed.no_display).into(),
        trigger: parsed.trigger,
        detail: parsed.trigger_detail,
        started_at: Some(lifetime.started_at),
        ends_at: lifetime.ends_at,
        even_lid: parsed.even_lid,
        ..Session::default()
    };
    let command = crate::supervisor::worker_command(&saved, target, charge)?;
    let mut worker = sysutil::spawn_worker(&command, &session::state_dir())?;
    saved.pid = worker.id();
    let identity = sysutil::child_identity(&worker)?;
    saved.process_start = identity.start;
    saved.process_command = identity.command.clone();
    let published = match wait_for_windows_worker_state(&mut worker, &saved) {
        Ok(published) => published,
        Err(error) => {
            let worker_status = worker.try_wait()?;
            cleanup_provisional_worker(&mut worker, &saved)?;
            if let Some(message) = windows_condition_completed(
                worker_status.is_some_and(|status| status.success()),
                &saved,
                charge,
            ) {
                println!("{message}");
                return Ok(());
            }
            return Err(error);
        }
    };
    if saved.trigger == "until-time"
        && saved
            .ends_at
            .is_some_and(|deadline| !deadline_still_pending(deadline, Utc::now()))
    {
        cleanup_provisional_worker(&mut worker, &saved)?;
        print_deadline_elapsed();
        return Ok(());
    }
    if !parsed.even_lid {
        print_start_confirmation(&published, None);
        return Ok(());
    }

    let snapshot = snapshot.expect("even-lid snapshot was captured");
    let scheme = platform::format_guid(&snapshot.scheme);
    let guardian = match sysutil::spawn_elevated_guardian(
        saved.pid,
        saved.process_start,
        &scheme,
        snapshot.ac,
        snapshot.dc,
        sysutil::GuardianMode::Startup(if saved.trigger == "until-time" {
            saved.ends_at
        } else {
            None
        }),
        &state_path,
    ) {
        Ok(guardian) => guardian,
        Err(error) => {
            let worker_status = worker.try_wait()?;
            cleanup_provisional_worker(&mut worker, &saved)?;
            if let Some(message) = windows_condition_completed(
                worker_status.is_some_and(|status| status.success()),
                &saved,
                charge,
            ) {
                println!("{message}");
                return Ok(());
            }
            return Err(error);
        }
    };
    saved.guardian_pid = guardian.pid();
    saved.guardian_start = guardian.identity().start;
    saved.original_scheme = scheme;
    saved.original_ac = snapshot.ac;
    saved.original_dc = snapshot.dc;
    if let Err(startup_error) = wait_for_lid_ready(&mut worker, &guardian, &saved, &snapshot) {
        let worker_status = worker.try_wait()?;
        if worker_status.is_none() {
            let _ = worker.kill();
            let _ = worker.wait();
        }
        wait_for_guardian_exit(&guardian, StdDuration::from_secs(15))?;
        if let Some(message) = windows_condition_completed(
            worker_status.is_some_and(|status| status.success()),
            &saved,
            charge,
        ) {
            session::delete_state_file()?;
            println!("{message}");
            return Ok(());
        }
        if session::read_saved_for_recovery().is_some() {
            verify_lid_restored(&saved)?;
            session::delete_state_file()?;
        }
        return Err(startup_error);
    }
    if let Some(message) = windows_condition_completed(false, &saved, charge) {
        cleanup_provisional_worker(&mut worker, &saved)?;
        wait_for_guardian_exit(&guardian, StdDuration::from_secs(15))?;
        session::delete_state_file()?;
        println!("{message}");
        return Ok(());
    }
    print_start_confirmation(&saved, None);
    Ok(())
}

#[cfg(windows)]
fn cleanup_provisional_worker(worker: &mut Child, expected: &Session) -> Result<()> {
    let _ = worker.kill();
    let _ = worker.wait();
    if let Some(session::SavedState::Valid(current)) = session::read_saved_for_recovery()
        && current.owned_non_lid_by(expected.pid, expected.process_start)
    {
        session::delete_state_file()?;
    }
    Ok(())
}

#[cfg(windows)]
fn windows_condition_completed(
    worker_succeeded: bool,
    saved: &Session,
    charge: Option<(i32, bool)>,
) -> Option<String> {
    let timing = if saved.even_lid {
        "before --even-lid became ready"
    } else {
        "during startup"
    };
    if saved.trigger == "until-time"
        && saved
            .ends_at
            .is_some_and(|deadline| !deadline_still_pending(deadline, Utc::now()))
    {
        return Some(format!("wake: requested end time passed {timing}"));
    }
    if !worker_succeeded {
        return None;
    }
    match saved.trigger.as_str() {
        "timed" => Some(format!("wake: requested end time passed {timing}")),
        "while-pid" | "while-app" => Some(format!("wake: watched process exited {timing}")),
        "until-charge" => {
            charge.map(|(target, _)| format!("wake: battery target {target}% reached {timing}"))
        }
        _ => None,
    }
}

#[cfg(windows)]
fn wait_for_windows_worker_state(worker: &mut Child, expected: &Session) -> Result<Session> {
    let deadline = Instant::now() + StdDuration::from_secs(5);
    while Instant::now() < deadline {
        match session::read_saved_for_recovery() {
            Some(session::SavedState::Valid(saved))
                if !saved.even_lid
                    && saved.pid == expected.pid
                    && saved.process_start == expected.process_start
                    && saved
                        .process_command
                        .eq_ignore_ascii_case(&expected.process_command) =>
            {
                if worker.try_wait()?.is_none() {
                    return Ok(saved);
                }
                break;
            }
            Some(session::SavedState::Malformed(_)) => {
                return Err(AppError::fail(
                    "Windows worker published malformed state; state retained",
                ));
            }
            _ => {}
        }
        if worker.try_wait()?.is_some() {
            break;
        }
        std::thread::sleep(StdDuration::from_millis(100));
    }
    Err(AppError::fail(
        "Windows worker failed to publish session state",
    ))
}

#[cfg(windows)]
fn wait_for_lid_ready(
    worker: &mut Child,
    guardian: &sysutil::ProcessHandle,
    expected: &Session,
    snapshot: &platform::LidSnapshot,
) -> Result<()> {
    let deadline = Instant::now() + StdDuration::from_secs(30);
    while Instant::now() < deadline {
        if worker.try_wait()?.is_some() {
            return Err(AppError::fail("worker exited during even-lid startup"));
        }
        if !guardian.is_running()? {
            return Err(AppError::fail("elevated guardian exited before readiness"));
        }
        if let Some(session::SavedState::Valid(saved)) = session::read_saved_for_recovery() {
            let health = platform::lid_health(
                platform::scheme_is_active(&snapshot.scheme)?,
                platform::read_lid_values(&snapshot.scheme)?,
            );
            if health == platform::LidHealth::Healthy
                && saved.matches_lid_authority(
                    (expected.pid, expected.process_start),
                    (expected.guardian_pid, expected.guardian_start),
                    (
                        &expected.original_scheme,
                        expected.original_ac,
                        expected.original_dc,
                    ),
                )
                && saved
                    .process_command
                    .eq_ignore_ascii_case(&expected.process_command)
            {
                return Ok(());
            }
        }
        std::thread::sleep(StdDuration::from_millis(100));
    }
    Err(AppError::fail("timed out waiting for even-lid readiness"))
}

#[cfg(windows)]
fn status_windows() -> Result<()> {
    let _lock = session::acquire_lock()?;
    match reconcile_windows(false)? {
        WindowsState::None => println!("wake: no active session"),
        WindowsState::Active { saved, .. } => {
            print_status(&saved);
            if saved.even_lid {
                print_lid_health(&saved)?;
            }
        }
        WindowsState::BrokenGuardian { saved, .. } => return Err(broken_guardian_error(&saved)),
    }
    Ok(())
}

#[cfg(windows)]
fn print_lid_health(saved: &Session) -> Result<()> {
    let scheme = platform::parse_guid(&saved.original_scheme)?;
    let values = platform::read_lid_values(&scheme)?;
    match platform::lid_health(platform::scheme_is_active(&scheme)?, values) {
        platform::LidHealth::Healthy => println!(
            "  even lid  : healthy (scheme {}, AC=0 DC=0)",
            saved.original_scheme
        ),
        platform::LidHealth::DegradedScheme => println!(
            "  even lid  : degraded (active scheme changed; recorded scheme {} is AC={} DC={})",
            saved.original_scheme, values.0, values.1
        ),
        platform::LidHealth::DegradedValues => println!(
            "  even lid  : degraded (recorded scheme {} is AC={} DC={})",
            saved.original_scheme, values.0, values.1
        ),
    }
    Ok(())
}

#[cfg(windows)]
fn stop_windows() -> Result<()> {
    let _lock = session::acquire_lock()?;
    match reconcile_windows(true)? {
        WindowsState::None => {
            println!("wake: no active session");
            Ok(())
        }
        WindowsState::BrokenGuardian { saved, worker } => {
            worker.terminate_and_wait(StdDuration::from_secs(6))?;
            Err(AppError::fail(format!(
                "guardian for worker {} is not running; the exact worker was stopped, but recovery state was retained at {}; run 'wake stop' again to start elevated recovery",
                saved.pid,
                session::state_file().display()
            )))
        }
        WindowsState::Active {
            saved,
            worker,
            guardian,
        } => {
            worker.terminate_and_wait(StdDuration::from_secs(6))?;
            if let Some(guardian) = guardian {
                wait_for_guardian_exit(&guardian, StdDuration::from_secs(15))?;
                verify_lid_restored(&saved)?;
            }
            session::delete_state_file()?;
            println!("wake: stopped (pid {}, {})", saved.pid, saved.trigger);
            Ok(())
        }
    }
}

#[cfg(windows)]
fn reconcile_windows(for_stop: bool) -> Result<WindowsState> {
    let saved = match session::read_saved_for_recovery() {
        None => return Ok(WindowsState::None),
        Some(session::SavedState::Malformed(lid_hints)) => {
            let hint = if lid_hints {
                " with lid-recovery fields"
            } else {
                ""
            };
            return Err(AppError::fail(format!(
                "malformed wake state{hint} retained byte-for-byte at {}; no power write was attempted",
                session::state_file().display()
            )));
        }
        Some(session::SavedState::Valid(saved)) => saved,
    };

    let worker = exact_worker(&saved, for_stop)?;
    if !saved.even_lid {
        if let Some(worker) = worker {
            return Ok(WindowsState::Active {
                saved,
                worker,
                guardian: None,
            });
        }
        session::delete_state_file()?;
        return Ok(WindowsState::None);
    }

    let guardian = exact_running(saved.guardian_pid, saved.guardian_start, false)?;
    match (worker, guardian) {
        (Some(worker), Some(guardian)) => Ok(WindowsState::Active {
            saved,
            worker,
            guardian: Some(guardian),
        }),
        (Some(worker), None) => Ok(WindowsState::BrokenGuardian { saved, worker }),
        (None, guardian) => {
            if let Some(guardian) = guardian {
                wait_for_guardian_exit(&guardian, StdDuration::from_secs(15))?;
            }
            if session::read_saved_for_recovery().is_some() {
                finish_or_recover_lid(&saved)?;
            }
            Ok(WindowsState::None)
        }
    }
}

#[cfg(windows)]
fn exact_running(pid: u32, start: u64, terminate: bool) -> Result<Option<sysutil::ProcessHandle>> {
    match sysutil::open_exact_process(pid, start, terminate)? {
        Some(process) if process.is_running()? => Ok(Some(process)),
        _ => Ok(None),
    }
}

#[cfg(windows)]
fn exact_worker(saved: &Session, terminate: bool) -> Result<Option<sysutil::ProcessHandle>> {
    let process = exact_running(saved.pid, saved.process_start, terminate)?;
    if process
        .as_ref()
        .is_some_and(|process| !saved.matches_identity(process.identity()))
    {
        return Err(AppError::fail(format!(
            "worker {} has an unexpected executable identity; state retained",
            saved.pid
        )));
    }
    Ok(process)
}

#[cfg(windows)]
fn finish_or_recover_lid(saved: &Session) -> Result<()> {
    let snapshot = platform::LidSnapshot {
        scheme: platform::parse_guid(&saved.original_scheme)?,
        ac: saved.original_ac,
        dc: saved.original_dc,
    };
    if platform::restore_lid_snapshot(&snapshot).is_ok() {
        return session::delete_state_file();
    }
    recover_even_lid_windows(saved)
}

#[cfg(windows)]
fn recover_even_lid_windows(saved: &Session) -> Result<()> {
    let state_path = session::state_file();
    let guardian = sysutil::spawn_elevated_guardian(
        saved.pid,
        saved.process_start,
        &saved.original_scheme,
        saved.original_ac,
        saved.original_dc,
        sysutil::GuardianMode::Recovery,
        &state_path,
    )?;
    let mut authorized = saved.clone();
    authorized.guardian_pid = guardian.pid();
    authorized.guardian_start = guardian.identity().start;
    session::write(&authorized)?;
    wait_for_guardian_exit(&guardian, StdDuration::from_secs(30))?;
    verify_lid_restored(&authorized)?;
    session::delete_state_file()?;
    eprintln!("wake: recovered the recorded lid values");
    Ok(())
}

#[cfg(windows)]
fn wait_for_guardian_exit(guardian: &sysutil::ProcessHandle, timeout: StdDuration) -> Result<()> {
    if !guardian.wait(timeout)? {
        return Err(AppError::fail(format!(
            "guardian {} did not exit; it was not terminated and state remains at {}",
            guardian.pid(),
            session::state_file().display()
        )));
    }
    let code = guardian.exit_code().map_err(|error| {
        AppError::fail(format!(
            "{error}; guardian result is unknown and state remains at {}",
            session::state_file().display()
        ))
    })?;
    if code == 0 {
        Ok(())
    } else {
        Err(AppError::fail(format!(
            "guardian {} exited with code {code}; state remains at {}",
            guardian.pid(),
            session::state_file().display()
        )))
    }
}

#[cfg(windows)]
fn lid_restored(saved: &Session) -> Result<bool> {
    let scheme = platform::parse_guid(&saved.original_scheme)?;
    Ok(platform::read_lid_values(&scheme)? == (saved.original_ac, saved.original_dc))
}

#[cfg(windows)]
fn verify_lid_restored(saved: &Session) -> Result<()> {
    if lid_restored(saved)? {
        Ok(())
    } else {
        Err(AppError::fail(format!(
            "lid restoration did not verify; state retained at {}",
            session::state_file().display()
        )))
    }
}

#[cfg(windows)]
fn broken_guardian_error(saved: &Session) -> AppError {
    AppError::fail(format!(
        "even-lid worker {} is live but guardian {} is not; run 'wake stop' to stop the worker while retaining recovery state",
        saved.pid, saved.guardian_pid
    ))
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
enum LidLaunch {
    Published(Session),
    ChargeMet { target: i32, percent: i32 },
    DeadlineElapsed,
    CleanupUnverified { error: AppError, pid: u32 },
}

#[cfg(target_os = "macos")]
fn start_lid_supervisor(p: &Parsed, charge_target: Option<i32>) -> Result<()> {
    let mut supervisor_detail = p.trigger_detail.clone();
    let mut charging_up = None;
    if let Some(target) = charge_target {
        match prepare_charge(target)? {
            ChargePreparation::AlreadyMet(percent) => {
                print_charge_met(percent, target);
                return Ok(());
            }
            ChargePreparation::Wait(charge) => {
                supervisor_detail = charge_detail(target, &charge);
                charging_up = Some(charge.charging_up);
            }
        }
    }

    if launch_lifetime(p.timeout_sec, p.until_deadline, Utc::now()).is_none() {
        print_deadline_elapsed();
        return Ok(());
    }
    let mut prior = platform::read_disable_sleep()?;
    if prior == 0 {
        ensure_sudo_for_even_lid()?;
        prior = platform::read_disable_sleep()?;
    }
    if launch_lifetime(p.timeout_sec, p.until_deadline, Utc::now()).is_none() {
        print_deadline_elapsed();
        return Ok(());
    }
    session::write_pending_lid_recovery(prior)?;

    match lid_enable_and_launch(p, &supervisor_detail, charge_target, charging_up, prior) {
        Ok(LidLaunch::Published(s)) => {
            print_start_confirmation(&s, None);
            Ok(())
        }
        Ok(LidLaunch::ChargeMet { target, percent }) => {
            restore_lid_startup_state(prior)?;
            print_charge_met(percent, target);
            Ok(())
        }
        Ok(LidLaunch::DeadlineElapsed) => {
            restore_lid_startup_state(prior)?;
            print_deadline_elapsed();
            Ok(())
        }
        Ok(LidLaunch::CleanupUnverified { error, pid }) => {
            retain_lid_startup_recovery_record(prior);
            Err(AppError::fail(format!(
                "{error}; lid supervisor {pid} may still be running, so no SleepDisabled restoration was attempted; recovery state retained at {}",
                session::state_file().display()
            )))
        }
        Err(e) => {
            if restore_disable_sleep_best_effort(prior) {
                session::delete_state_file()?;
            } else {
                retain_lid_startup_recovery_record(prior);
                print_sleep_restore_rescue(prior);
            }
            Err(e)
        }
    }
}

#[cfg(target_os = "macos")]
fn lid_enable_and_launch(
    p: &Parsed,
    supervisor_detail: &str,
    charge_target: Option<i32>,
    charging_up: Option<bool>,
    prior: i32,
) -> Result<LidLaunch> {
    if prior == 0 {
        platform::set_disable_sleep_foreground(1)?;
    }
    let current = platform::read_disable_sleep()?;
    if current != 1 {
        if sleep_restore_needed(prior, current) {
            restore_disable_sleep_foreground(prior)?;
        }
        return Err(AppError::fail(format!(
            "failed to enable --even-lid; SleepDisabled is {current}"
        )));
    }
    if launch_lifetime(p.timeout_sec, p.until_deadline, Utc::now()).is_none() {
        return Ok(LidLaunch::DeadlineElapsed);
    }
    let cmd = vec![
        sysutil::self_exe()?,
        "__supervise_lid__".into(),
        if p.no_display { "i" } else { "d" }.into(),
        p.timeout_sec.map(|t| t.to_string()).unwrap_or_default(),
        p.until_deadline
            .map(|deadline| deadline.to_rfc3339())
            .unwrap_or_default(),
        p.wait_pid.map(|w| w.to_string()).unwrap_or_default(),
        prior.to_string(),
        p.trigger.clone(),
        supervisor_detail.to_string(),
        charge_target.map(|c| c.to_string()).unwrap_or_default(),
    ];
    let mut child = sysutil::spawn_named(&cmd)?;
    let published = match wait_for_supervisor_session(&mut child, true) {
        Ok(published) => published,
        Err(error) => {
            let stopped = child.try_wait().is_ok_and(|status| status.is_some());
            if !stopped {
                return Ok(LidLaunch::CleanupUnverified {
                    error,
                    pid: child.id(),
                });
            }
            return Err(error);
        }
    };
    if let Some(session) = published {
        if p.until_deadline
            .is_some_and(|deadline| !deadline_still_pending(deadline, Utc::now()))
        {
            return match terminate_owned_child(&mut child) {
                Ok(()) => Ok(LidLaunch::DeadlineElapsed),
                Err(error) => Ok(LidLaunch::CleanupUnverified {
                    error,
                    pid: child.id(),
                }),
            };
        }
        return Ok(LidLaunch::Published(session));
    }
    let status = child
        .try_wait()?
        .ok_or_else(|| AppError::fail("lid supervisor cleanup did not reap the owned process"))?;
    if p.until_deadline
        .is_some_and(|deadline| Utc::now() >= deadline)
    {
        return Ok(LidLaunch::DeadlineElapsed);
    }
    if status.success()
        && let Some((target, up)) = charge_target.zip(charging_up)
        && let Some(percent) = current_charge_met(target, up)
    {
        return Ok(LidLaunch::ChargeMet { target, percent });
    }
    Err(AppError::fail(
        "lid supervisor failed to publish session state",
    ))
}

#[cfg(target_os = "macos")]
fn restore_lid_startup_state(prior: i32) -> Result<()> {
    if prior != 0 {
        return session::delete_state_file();
    }
    retain_lid_startup_recovery_record(prior);
    let current =
        platform::read_disable_sleep().inspect_err(|_| print_sleep_restore_rescue(prior))?;
    if sleep_restore_needed(prior, current) {
        restore_disable_sleep_foreground(prior)?;
    }
    session::delete_state_file()
}

#[cfg(target_os = "macos")]
fn retain_lid_startup_recovery_record(prior: i32) {
    match session::read_saved_for_recovery() {
        Some(session::SavedState::Valid(saved))
            if saved.even_lid && saved.prior_disable_sleep == prior => {}
        Some(session::SavedState::Malformed(_)) => {}
        _ => {
            if let Err(error) = session::write_pending_lid_recovery(prior) {
                eprintln!("wake: could not recreate lid recovery state: {error}");
            }
        }
    }
}

#[cfg(not(windows))]
fn wait_for_supervisor_session(
    child: &mut std::process::Child,
    even_lid: bool,
) -> Result<Option<Session>> {
    wait_for_supervisor_session_with(
        child,
        even_lid,
        std::time::Duration::from_secs(5),
        session::read_saved_for_recovery,
    )
}

#[cfg(not(windows))]
fn wait_for_supervisor_session_with(
    child: &mut std::process::Child,
    even_lid: bool,
    timeout: std::time::Duration,
    mut read_state: impl FnMut() -> Option<session::SavedState>,
) -> Result<Option<Session>> {
    let deadline = std::time::Instant::now() + timeout;
    let result = (|| {
        loop {
            if child.try_wait()?.is_some() {
                break Ok(None);
            }
            if let Some(session::SavedState::Valid(saved)) = read_state()
                && saved.pid == child.id()
                && saved.even_lid == even_lid
                && saved.matches_live_process()
            {
                if child.try_wait()?.is_none() {
                    break Ok(Some(saved));
                }
                break Ok(None);
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                break Ok(None);
            }
            std::thread::sleep(
                deadline
                    .saturating_duration_since(now)
                    .min(std::time::Duration::from_millis(100)),
            );
        }
    })();
    if result.as_ref().is_ok_and(Option::is_some) {
        return result;
    }
    terminate_owned_child(child)?;
    result
}

#[cfg(not(windows))]
fn terminate_owned_child(child: &mut std::process::Child) -> Result<()> {
    if child.try_wait()?.is_none()
        && let Err(error) = child.kill()
        && child.try_wait()?.is_none()
    {
        return Err(AppError::fail(format!(
            "could not terminate supervisor {}: {error}",
            child.id()
        )));
    }
    child.wait()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_disable_sleep_restored_after_stop(s: &Session) -> Result<()> {
    if s.prior_disable_sleep != 0 {
        return Ok(());
    }
    for _ in 0..20 {
        if sleep_restored(s.prior_disable_sleep, platform::read_disable_sleep()?) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    if sleep_restored(s.prior_disable_sleep, platform::read_disable_sleep()?) {
        return Ok(());
    }
    restore_disable_sleep_with_prompt_if_possible(
        s.prior_disable_sleep,
        "lid supervisor did not restore SleepDisabled, and no interactive terminal is available for sudo recovery",
    )?;
    let after = platform::read_disable_sleep()?;
    if !sleep_restored(s.prior_disable_sleep, after) {
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
    Ok(())
}

#[cfg(target_os = "macos")]
fn restore_disable_sleep_foreground(prior: i32) -> Result<()> {
    if prior != 0 {
        return Ok(());
    }
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

#[cfg(target_os = "macos")]
fn restore_disable_sleep_with_prompt_if_possible(
    prior: i32,
    no_console_message: &str,
) -> Result<()> {
    if prior != 0 {
        return Ok(());
    }
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

#[cfg(target_os = "macos")]
fn restore_disable_sleep_best_effort(prior: i32) -> bool {
    if prior != 0 {
        return true;
    }
    let _ = platform::set_disable_sleep_foreground(prior);
    platform::read_disable_sleep()
        .map(|current| sleep_restored(prior, current))
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
pub fn print_sleep_restore_rescue(value: i32) {
    eprintln!("wake: could not restore sleep; run: sudo pmset -a disablesleep {value}");
    print_sleep_state_path();
}

#[cfg(target_os = "macos")]
fn print_sleep_state_path() {
    eprintln!("wake: recovery state: {}", session::state_file().display());
}

#[cfg(not(windows))]
pub fn recover_stale_lid_session_unlocked() -> Result<()> {
    let state = match session::read_saved_for_recovery() {
        None => return Ok(()),
        Some(s) => s,
    };
    let saved = match state {
        session::SavedState::Malformed(lid_hints) => {
            return recover_malformed_lid_session_unlocked(lid_hints);
        }
        session::SavedState::Valid(s) => s,
    };
    if saved.matches_live_process() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    if saved.even_lid {
        recover_crashed_even_lid_unix(&saved)?;
    }
    session::delete_state_file()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn recover_crashed_even_lid_unix(saved: &Session) -> Result<()> {
    if saved.prior_disable_sleep != 0 {
        return Ok(());
    }
    let current = platform::read_disable_sleep()?;
    if sleep_restore_needed(saved.prior_disable_sleep, current) {
        restore_disable_sleep_with_prompt_if_possible(
            saved.prior_disable_sleep,
            "crashed lid session needs sudo recovery, but no interactive terminal is available",
        )?;
        let after = platform::read_disable_sleep()?;
        if !sleep_restored(saved.prior_disable_sleep, after) {
            print_sleep_restore_rescue(saved.prior_disable_sleep);
            return Err(AppError::fail(format!(
                "failed to recover crashed lid session; SleepDisabled is {after}"
            )));
        }
        eprintln!("wake: recovered a crashed lid session; restored normal sleep");
    }
    Ok(())
}

#[cfg(not(windows))]
fn recover_malformed_lid_session_unlocked(lid_hints: bool) -> Result<()> {
    if session::retain_malformed_state(lid_hints) {
        #[cfg(target_os = "macos")]
        return Err(AppError::fail(format!(
            "malformed or pending wake state retained byte-for-byte at {}; automatic recovery made no OS changes; inspect SleepDisabled with 'pmset -g', restore it manually with 'sudo pmset -a disablesleep <0-or-1>', then remove the state file",
            session::state_file().display()
        )));
        #[cfg(target_os = "linux")]
        return Err(AppError::fail(format!(
            "malformed or unreadable wake state retained unchanged at {}; it may identify a live wake/systemd-inhibit process; Linux wake changed no persistent OS setting, so inspect the recorded process identity and stop only that matching process before removing the state file",
            session::state_file().display()
        )));
    }
    session::delete_state_file()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, LocalResult, NaiveDate};

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn fixed(wall: NaiveDateTime, offset: i32) -> DateTime<FixedOffset> {
        FixedOffset::east_opt(offset)
            .unwrap()
            .from_local_datetime(&wall)
            .single()
            .unwrap()
    }

    #[test]
    fn startup_charge_race_uses_the_original_direction() {
        assert!(charge_target_met(80, true, 80));
        assert!(charge_target_met(80, true, 81));
        assert!(!charge_target_met(80, true, 79));
        assert!(charge_target_met(80, false, 80));
        assert!(charge_target_met(80, false, 79));
        assert!(!charge_target_met(80, false, 81));
    }

    #[cfg(windows)]
    #[test]
    fn completed_windows_condition_uses_the_matching_startup_message() {
        let mut saved = Session {
            trigger: "until-time".into(),
            ends_at: Some(Utc::now() - Duration::seconds(1)),
            ..Session::default()
        };
        assert_eq!(
            windows_condition_completed(true, &saved, None).as_deref(),
            Some("wake: requested end time passed during startup")
        );
        saved.even_lid = true;
        assert_eq!(
            windows_condition_completed(true, &saved, None).as_deref(),
            Some("wake: requested end time passed before --even-lid became ready")
        );
        assert_eq!(
            windows_condition_completed(false, &saved, None).as_deref(),
            Some("wake: requested end time passed before --even-lid became ready")
        );
    }

    #[test]
    fn start_confirmation_output_covers_direct_charge_and_until_paths() {
        for (trigger, detail) in [
            ("indefinite", "indefinite"),
            ("until-charge", "80% (was 60%, charging up)"),
            ("until-time", "until 23:59"),
        ] {
            let session = Session {
                pid: 42,
                mode: "display+system".into(),
                trigger: trigger.into(),
                detail: detail.into(),
                ..Session::default()
            };
            assert_eq!(
                start_confirmation_lines(&session, Some("note: route note")),
                [
                    "wake: session active (pid 42)".to_string(),
                    "  mode    : display+system".to_string(),
                    format!("  trigger : {trigger} ({detail})"),
                    "  started : -".to_string(),
                    "  ends    : -".to_string(),
                    "note: route note".to_string(),
                ]
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_default_note_covers_direct_charge_and_until_paths() {
        for trigger in ["indefinite", "until-charge", "until-time"] {
            let session = Session {
                trigger: trigger.into(),
                ..Session::default()
            };
            assert_eq!(
                start_confirmation_lines(&session, None)
                    .last()
                    .map(String::as_str),
                Some("note: closing the lid still sleeps the mac unless you use --even-lid")
            );
        }
    }

    #[test]
    fn parser_routes_indefinite_and_rejects_duplicates() {
        let parsed = parse_start_args(&args(&["forever", "--no-display"])).unwrap();
        assert_eq!(parsed.trigger, "indefinite");
        assert!(parsed.no_display);
        for values in [
            &["--no-display", "--no-display"][..],
            &["--even-lid", "--even-lid"],
        ] {
            assert!(matches!(
                parse_start_args(&args(values)),
                Err(AppError::Usage(_))
            ));
        }
    }

    #[test]
    fn parser_rejects_mixed_help_and_version_with_the_top_level_message() {
        for values in [&["1h", "--help"][..], &["--no-display", "version"]] {
            let error = parse_start_args(&args(values)).err().unwrap();
            assert_eq!(error.message(), "help and version must be used alone");
        }
    }

    #[test]
    fn parser_consumes_help_and_version_as_while_app_values() {
        for value in ["help", "version", "--help"] {
            if let Err(error) = parse_start_args(&args(&["--while-app", value])) {
                assert_ne!(error.message(), "help and version must be used alone");
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_routes_until_charge_with_even_lid() {
        let parsed = parse_start_args(&args(&["--until-charge", "80", "--even-lid"])).unwrap();
        assert_eq!(parsed.charge_target, Some(80));
        assert!(parsed.even_lid);
    }

    #[cfg(not(windows))]
    #[test]
    fn charge_supervisor_launch_omits_mode_but_preserves_lid_intent_and_scope() {
        let explicit =
            charge_supervisor_launch(80, true, true, "sleep:handle-lid-switch", None).unwrap();
        assert_eq!(explicit.command.len(), 6);
        assert_eq!(
            &explicit.command[1..],
            [
                "__supervise_charge__",
                "80",
                "true",
                "true",
                "sleep:handle-lid-switch",
            ]
        );

        let ordinary = charge_supervisor_launch(
            80,
            false,
            false,
            "sleep:handle-lid-switch",
            Some("note: best-effort inhibitor includes lid scope"),
        )
        .unwrap();
        assert_eq!(ordinary.command.len(), 6);
        assert_eq!(
            &ordinary.command[1..],
            [
                "__supervise_charge__",
                "80",
                "false",
                "false",
                "sleep:handle-lid-switch",
            ]
        );
        assert_eq!(
            ordinary.note.as_deref(),
            Some("note: best-effort inhibitor includes lid scope")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn until_supervisor_command_omits_mode_but_preserves_deadline_and_scope() {
        let deadline = DateTime::parse_from_rfc3339("2024-01-02T03:04:05+00:00")
            .unwrap()
            .with_timezone(&Utc);
        let command = until_supervisor_command(
            deadline,
            false,
            "until 03:04",
            false,
            "sleep:handle-lid-switch",
        )
        .unwrap();
        assert_eq!(command.len(), 7);
        assert_eq!(
            &command[1..],
            [
                "__supervise_until__",
                "2024-01-02T03:04:05+00:00",
                "false",
                "until 03:04",
                "false",
                "sleep:handle-lid-switch",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn supervisor_publication_stops_polling_when_the_owned_child_exits() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 17"])
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        let published = wait_for_supervisor_session_with(
            &mut child,
            false,
            std::time::Duration::from_secs(5),
            || None,
        )
        .unwrap();
        assert!(published.is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(child.try_wait().unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn supervisor_publication_timeout_terminates_and_reaps_the_owned_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let published = wait_for_supervisor_session_with(
            &mut child,
            false,
            std::time::Duration::from_millis(20),
            || None,
        )
        .unwrap();
        assert!(published.is_none());
        assert!(child.try_wait().unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn supervisor_publication_rejects_state_without_the_owned_child_identity() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let forged = Session {
            pid: child.id(),
            mode: "display+system".into(),
            trigger: "indefinite".into(),
            detail: "indefinite".into(),
            started_at: Some(Utc::now()),
            process_start: u64::MAX,
            process_command: "/usr/bin/wake".into(),
            even_lid: false,
            ..Session::default()
        };
        let published = wait_for_supervisor_session_with(
            &mut child,
            false,
            std::time::Duration::from_millis(20),
            || Some(session::SavedState::Valid(forged.clone())),
        )
        .unwrap();
        assert!(published.is_none());
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn invalid_pid_is_usage() {
        assert!(matches!(
            parse_start_args(&args(&["--while-pid", "not-a-pid"])),
            Err(AppError::Usage(_))
        ));
    }

    #[test]
    fn launch_lifetime_preserves_exact_absolute_and_relative_ends() {
        let launch = DateTime::parse_from_rfc3339("2024-01-02T03:04:05+00:00")
            .unwrap()
            .with_timezone(&Utc);
        let deadline = launch + Duration::seconds(5);
        let lifetime = launch_lifetime(None, Some(deadline), launch).unwrap();
        assert_eq!(lifetime.started_at, launch);
        assert_eq!(lifetime.ends_at, Some(deadline));
        assert!(launch_lifetime(None, Some(deadline), deadline).is_none());
        assert!(deadline_still_pending(
            deadline,
            deadline - Duration::milliseconds(1)
        ));
        assert!(!deadline_still_pending(deadline, deadline));

        let relative = launch_lifetime(Some(60), None, launch).unwrap();
        assert_eq!(relative.started_at, launch);
        assert_eq!(relative.ends_at, Some(launch + Duration::seconds(60)));
    }

    #[test]
    fn until_parser_retains_an_absolute_deadline() {
        let parsed = parse_start_args(&args(&["--until", "23:59"])).unwrap();
        assert!(parsed.timeout_sec.is_none());
        assert!(parsed.until_deadline.is_some());
    }

    #[test]
    fn prior_enabled_sleep_is_not_a_wake_owned_transition() {
        assert!(sleep_restore_needed(0, 1));
        assert!(!sleep_restore_needed(1, 0));
        for (prior, current) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
            assert_eq!(
                sleep_restored(prior, current),
                !sleep_restore_needed(prior, current)
            );
        }
    }

    #[test]
    fn until_fold_chooses_earliest_future_occurrence() {
        let date = NaiveDate::from_ymd_opt(2024, 11, 3).unwrap();
        let requested = date.and_hms_opt(1, 30, 0).unwrap();
        let now = fixed(date.and_hms_opt(5, 45, 0).unwrap(), 0);
        let target = next_wall_time(&now, requested.time(), |wall| {
            if wall == requested {
                LocalResult::Ambiguous(fixed(wall, -4 * 3600), fixed(wall, -5 * 3600))
            } else {
                LocalResult::Single(fixed(wall, -5 * 3600))
            }
        })
        .unwrap();
        assert_eq!(target.timestamp(), fixed(requested, -5 * 3600).timestamp());
    }

    #[test]
    fn until_gap_uses_first_valid_wall_time() {
        let date = NaiveDate::from_ymd_opt(2024, 3, 10).unwrap();
        let gap_start = date.and_hms_opt(2, 0, 0).unwrap();
        let gap_end = date.and_hms_opt(3, 0, 0).unwrap();
        let now = fixed(date.and_hms_opt(1, 0, 0).unwrap(), -5 * 3600);
        let target = next_wall_time(&now, date.and_hms_opt(2, 30, 0).unwrap().time(), |wall| {
            if (gap_start..gap_end).contains(&wall) {
                LocalResult::None
            } else {
                LocalResult::Single(fixed(wall, -4 * 3600))
            }
        })
        .unwrap();
        assert_eq!(target.naive_local(), gap_end);
    }

    #[test]
    fn until_tomorrow_uses_the_next_calendar_date() {
        let today = NaiveDate::from_ymd_opt(2024, 3, 9).unwrap();
        let tomorrow = today.succ_opt().unwrap();
        let now = fixed(today.and_hms_opt(23, 0, 0).unwrap(), -5 * 3600);
        let target = next_wall_time(&now, NaiveTime::from_hms_opt(12, 0, 0).unwrap(), |wall| {
            let offset = if wall.date() == today {
                -5 * 3600
            } else {
                -4 * 3600
            };
            LocalResult::Single(fixed(wall, offset))
        })
        .unwrap();
        assert_eq!(target.date_naive(), tomorrow);
        assert_eq!(target.offset().local_minus_utc(), -4 * 3600);
    }
}
