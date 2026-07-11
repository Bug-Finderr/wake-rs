//! Detached run loop and the temporary even-lid supervisors.

#[cfg(not(windows))]
use crate::commands;
use crate::error::{AppError, Result};
use crate::platform;
use crate::run::{BatteryStatus, ChargeDirection, ChargePlan, RunSpec, Trigger, plan_charge};
use crate::session::{self, Session};
use crate::sysutil;
use chrono::Utc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

const BATTERY_INTERVAL: Duration = Duration::from_secs(30);
const RUN_INTERVAL: Duration = Duration::from_secs(1);
const STARTUP_SETTLE: Duration = Duration::from_millis(300);
#[cfg(not(windows))]
const SUDO_HEARTBEAT: Duration = Duration::from_secs(180);

pub fn read_battery_status() -> Result<BatteryStatus> {
    platform::read_battery()
}

fn install_stop_flag() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    {
        let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&flag));
        let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&flag));
    }
    flag
}

pub fn run(args: &[String]) -> Result<()> {
    let [json] = args else {
        return Err(AppError::fail("supervisor expects one run specification"));
    };
    let spec: RunSpec = serde_json::from_str(json)
        .map_err(|error| AppError::fail(format!("invalid supervisor specification: {error}")))?;
    spec.validate()?;
    supervise(spec)
}

fn supervise(spec: RunSpec) -> Result<()> {
    let started = Instant::now();
    let started_at = Utc::now();
    let mut inhibitor = platform::Inhibitor::start(spec.mode)?;
    sleep(STARTUP_SETTLE);
    if !inhibitor.alive() {
        return Err(AppError::fail("sleep inhibitor exited during startup"));
    }
    let mut session = Session {
        pid: sysutil::current_pid(),
        mode: spec.mode.label().into(),
        trigger: spec.trigger.label().into(),
        detail: spec.trigger.detail(),
        started_at: Some(started_at),
        ends_at: spec.trigger.session_ends_at(started_at),
        note: inhibitor.note().map(str::to_string),
        ..Default::default()
    };
    session.capture_process_identity()?;
    session::write(&session)?;

    let result = supervise_loop(&spec, &session, &mut inhibitor, started);
    drop(inhibitor);
    let remove = session::remove_if_matches(&session).map(|_| ());
    let clear = session::clear_stop(&session).map(|_| ());
    result.and(remove).and(clear)
}

fn supervise_loop(
    spec: &RunSpec,
    session: &Session,
    inhibitor: &mut platform::Inhibitor,
    started: Instant,
) -> Result<()> {
    let signal = install_stop_flag();
    let mut next_battery = Instant::now();
    loop {
        if signal.load(Ordering::Relaxed)
            || session::stop_requested(session).is_ok_and(|requested| requested)
        {
            return Ok(());
        }
        if !inhibitor.alive() {
            return Err(AppError::fail("sleep inhibitor exited unexpectedly"));
        }
        if spec.trigger.is_complete(Utc::now(), started.elapsed()) {
            return Ok(());
        }
        if spec
            .trigger
            .process()
            .is_some_and(|process| !sysutil::process_matches(process))
        {
            return Ok(());
        }
        if Instant::now() >= next_battery {
            next_battery = Instant::now() + BATTERY_INTERVAL;
            if let Trigger::Charge {
                target, direction, ..
            } = &spec.trigger
                && charge_reached(*target, *direction)
            {
                return Ok(());
            }
        }
        sleep(RUN_INTERVAL);
    }
}

fn charge_reached(target: i32, direction: ChargeDirection) -> bool {
    read_battery_status().is_ok_and(|status| match direction {
        ChargeDirection::Up => status.percent >= target,
        ChargeDirection::Down => status.percent <= target,
    })
}

#[cfg(windows)]
pub fn run_charge(args: &[String]) -> Result<()> {
    if args.len() < 5 || args[4].is_empty() {
        return Err(AppError::fail("legacy lid supervisor: bad args"));
    }
    let target: i32 = args[1]
        .parse()
        .map_err(|_| AppError::fail("legacy lid supervisor: bad target"))?;
    let no_display = args[2] == "true";
    let mode = args[3].clone();
    let prior_lid: i32 = args[4]
        .parse()
        .map_err(|_| AppError::fail("legacy lid supervisor: bad lid state"))?;

    let initial = read_battery_status()?;
    let direction = match plan_charge(target, &initial)? {
        ChargePlan::Reached => return Ok(()),
        ChargePlan::Wait(direction) => direction,
    };

    let ka = platform::keep_awake_command(no_display, None, None)?;
    let mut child = sysutil::spawn_supervised_child(&ka.cmd)?;
    sysutil::require_child_alive(child.id(), &ka.cmd)?;

    let mut s = Session {
        pid: sysutil::current_pid(),
        mode,
        trigger: "until-charge".into(),
        detail: format!(
            "{target}% (was {}%, {})",
            initial.percent,
            match direction {
                ChargeDirection::Up => "charging up",
                ChargeDirection::Down => "discharging down",
            }
        ),
        started_at: Some(Utc::now()),
        even_lid: true,
        prior_disable_sleep: prior_lid,
        ..Default::default()
    };
    if let Err(e) = s
        .capture_process_identity()
        .and_then(|_| session::write(&s))
    {
        let _ = child.kill();
        return Err(e);
    }

    let stop = install_stop_flag();
    let mut last_check = Instant::now();
    loop {
        sleep(RUN_INTERVAL);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if !sysutil::is_alive(child.id()) {
            break;
        }
        if last_check.elapsed() >= BATTERY_INTERVAL {
            last_check = Instant::now();
            if charge_reached(target, direction) {
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    restore_lid_on_windows(prior_lid);
    session::remove_state_file()?;
    Ok(())
}

#[cfg(windows)]
fn restore_lid_on_windows(prior: i32) {
    let (ac, dc) = platform::decode_lid(prior);
    if (ac, dc) == (0, 0) {
        return;
    }
    let _ = sysutil::run_elevated_self(&["__set_lid__", &ac.to_string(), &dc.to_string()]);
}

#[cfg(not(windows))]
pub fn run_lid(args: &[String]) -> Result<()> {
    if args.len() < 7 {
        return Err(AppError::fail("lid supervisor: bad args"));
    }
    if !platform::supports_even_lid() {
        return Ok(());
    }
    let mode_char = &args[1];
    if mode_char != "d" && mode_char != "i" {
        return Err(AppError::fail("lid supervisor: bad caffeinate mode"));
    }
    let no_display = mode_char == "i";
    let timeout_sec = optional_i64(&args[2]);
    let wait_pid = optional_u32(&args[3]);
    let prior_disable_sleep = parse_disable_sleep(&args[4])?;
    let trigger = args[5].clone();
    let detail = args[6].clone();
    let charge_target: Option<i32> = args
        .get(7)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok());

    let mut charge_direction = None;
    if let Some(target) = charge_target {
        let initial = read_battery_status()?;
        match plan_charge(target, &initial)? {
            ChargePlan::Reached => return Ok(()),
            ChargePlan::Wait(direction) => charge_direction = Some(direction),
        }
    }

    let ka = platform::keep_awake_command(no_display, timeout_sec, wait_pid)?;
    let mut child = sysutil::spawn_supervised_child(&ka.cmd)?;
    sysutil::require_child_alive(child.id(), &ka.cmd)?;

    let now = Utc::now();
    let mut s = Session {
        pid: sysutil::current_pid(),
        mode: if no_display {
            "system-only".into()
        } else {
            "display+system".into()
        },
        trigger,
        detail,
        started_at: Some(now),
        ends_at: timeout_sec.map(|t| now + chrono::Duration::seconds(t)),
        even_lid: true,
        prior_disable_sleep,
        ..Default::default()
    };
    if let Err(e) = s
        .capture_process_identity()
        .and_then(|_| session::write(&s))
    {
        let _ = child.kill();
        lid_cleanup(prior_disable_sleep);
        return Err(e);
    }

    let stop = install_stop_flag();
    let start = Instant::now();
    let mut next_sudo = start + SUDO_HEARTBEAT;
    let mut last_check = Instant::now();
    loop {
        sleep(RUN_INTERVAL);
        if stop.load(Ordering::Relaxed) || !sysutil::is_alive(child.id()) {
            break;
        }
        if let Some(t) = timeout_sec
            && start.elapsed() >= Duration::from_secs(t.max(0) as u64)
        {
            break;
        }
        if let Some(p) = wait_pid
            && !sysutil::is_alive(p)
        {
            break;
        }
        if last_check.elapsed() >= BATTERY_INTERVAL {
            last_check = Instant::now();
            if let Some(target) = charge_target
                && charge_direction.is_some_and(|direction| charge_reached(target, direction))
            {
                break;
            }
        }
        if Instant::now() >= next_sudo {
            let _ = platform::refresh_sudo_non_interactive();
            next_sudo = Instant::now() + SUDO_HEARTBEAT;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    lid_cleanup(prior_disable_sleep);
    Ok(())
}

#[cfg(not(windows))]
fn lid_cleanup(prior_disable_sleep: i32) {
    if let Ok(current) = platform::read_disable_sleep()
        && current != prior_disable_sleep
    {
        let _ = platform::set_disable_sleep_non_interactive(prior_disable_sleep);
    }
    let restored = platform::read_disable_sleep()
        .map(|c| c == prior_disable_sleep)
        .unwrap_or(false);
    if restored {
        if let Err(error) = commands::finish_mac_lid_restore(prior_disable_sleep) {
            eprintln!("wake supervisor: {error}");
        }
    } else {
        commands::print_sleep_restore_rescue(prior_disable_sleep);
    }
}

#[cfg(not(windows))]
fn optional_i64(raw: &str) -> Option<i64> {
    if raw.trim().is_empty() {
        None
    } else {
        raw.trim().parse().ok()
    }
}

#[cfg(not(windows))]
fn optional_u32(raw: &str) -> Option<u32> {
    if raw.trim().is_empty() {
        None
    } else {
        raw.trim().parse().ok()
    }
}

#[cfg(not(windows))]
fn parse_disable_sleep(raw: &str) -> Result<i32> {
    match raw.trim().parse::<i32>() {
        Ok(v @ (0 | 1)) => Ok(v),
        _ => Err(AppError::fail("priorDisableSleep must be 0 or 1")),
    }
}
