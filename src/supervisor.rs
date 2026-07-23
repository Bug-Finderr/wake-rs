//! Detached charge and macOS lid supervisors.

#[cfg(not(windows))]
use crate::commands;
use crate::error::{AppError, Result};
use crate::platform;
use crate::session::{self, Session};
use crate::sysutil;
use chrono::Utc;
#[cfg(not(windows))]
use std::process::Child;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const LID_POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_BATTERY_FAILURES: u8 = 3;
#[cfg(not(windows))]
const SUDO_HEARTBEAT: Duration = Duration::from_secs(180);

#[derive(Clone)]
pub struct BatteryStatus {
    pub percent: i32,
    pub charging: bool,
    pub discharging: bool,
    pub neutral_state: Option<String>,
}

pub struct ChargePlan {
    pub already_met: bool,
    pub charging_up: bool,
}

impl ChargePlan {
    fn already_met() -> Self {
        ChargePlan {
            already_met: true,
            charging_up: false,
        }
    }
    fn waiting(charging_up: bool) -> Self {
        ChargePlan {
            already_met: false,
            charging_up,
        }
    }
}

pub fn read_battery_status() -> Result<BatteryStatus> {
    platform::read_battery()
}

pub fn plan_charge(target: i32, status: &BatteryStatus) -> Result<ChargePlan> {
    if status.discharging {
        if status.percent == target {
            return Ok(ChargePlan::already_met());
        }
        if status.percent < target {
            return Err(AppError::usage(format!(
                "--until-charge {target} is unreachable while battery is discharging at {}%; \
                 connect power or choose a target at or below the current charge",
                status.percent
            )));
        }
        return Ok(ChargePlan::waiting(false));
    }
    if status.charging {
        if status.percent >= target {
            return Ok(ChargePlan::already_met());
        }
        return Ok(ChargePlan::waiting(true));
    }
    if status.percent == target {
        return Ok(ChargePlan::already_met());
    }
    if let Some(state) = &status.neutral_state {
        return Err(AppError::usage(format!(
            "--until-charge {target} is unreachable while battery is {state} at {}%",
            status.percent
        )));
    }
    Err(AppError::usage(
        "cannot determine battery charging direction",
    ))
}

/// SIGTERM, SIGINT, and SIGHUP request graceful cleanup on Unix.
fn install_stop_flag() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    for signal in [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGHUP,
    ] {
        let _ = signal_hook::flag::register(signal, Arc::clone(&flag));
    }
    flag
}

pub fn run_charge(args: &[String]) -> Result<()> {
    #[cfg(not(windows))]
    if args.len() != 4 {
        return Err(AppError::fail("charge supervisor expects 3 arguments"));
    }
    #[cfg(windows)]
    if !(4..=5).contains(&args.len()) {
        return Err(AppError::fail("charge supervisor: bad arguments"));
    }
    let target = parse_charge_target(&args[1])?;
    let no_display = parse_bool(&args[2], "no-display")?;
    let mode = args[3].clone();
    // arg[4] (Windows even-lid only): encoded prior lid action to restore on teardown.
    #[cfg(windows)]
    let prior_lid: Option<i32> = args
        .get(4)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok());

    let (initial, plan) =
        match read_battery_status().and_then(|s| plan_charge(target, &s).map(|p| (s, p))) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("wake supervisor: {e}");
                return Ok(());
            }
        };
    if plan.already_met {
        return Ok(());
    }
    let charging_up = plan.charging_up;

    let ka = platform::keep_awake_command(no_display, None, None)?;
    let mut child = sysutil::spawn_supervised_child(&ka.cmd)?;
    let child_pid = child.id();
    sysutil::require_child_alive(child_pid, &ka.cmd)?;

    let mut s = Session::new();
    s.pid = sysutil::current_pid();
    s.mode = mode;
    s.trigger = "until-charge".into();
    s.detail = format!(
        "{target}% (was {}%, {})",
        initial.percent,
        if charging_up {
            "charging up"
        } else {
            "discharging down"
        }
    );
    s.started_at = Some(Utc::now());
    s.ends_at = None;
    #[cfg(windows)]
    if let Some(prior) = prior_lid {
        s.even_lid = true;
        s.prior_disable_sleep = prior;
    }
    if let Err(e) = s
        .capture_process_identity()
        .and_then(|_| session::write(&s))
    {
        let _ = child.kill();
        return Err(e);
    }

    let stop = install_stop_flag();
    let mut last_check = Instant::now();
    let mut battery_failures = 0;
    loop {
        sleep(LID_POLL_INTERVAL);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if !sysutil::is_alive(child_pid) {
            break;
        }
        if last_check.elapsed() >= POLL_INTERVAL {
            last_check = Instant::now();
            match read_battery_status() {
                Ok(status) => {
                    battery_failures = 0;
                    let reached = if charging_up {
                        status.percent >= target
                    } else {
                        status.percent <= target
                    };
                    if reached {
                        break;
                    }
                }
                Err(error) if battery_failures_exhausted(&mut battery_failures, &error) => break,
                Err(_) => {}
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    #[cfg(windows)]
    if let Some(prior) = prior_lid {
        restore_lid_on_windows(prior);
    }
    session::delete_state_file();
    Ok(())
}

/// Best-effort Windows lid recovery must not block supervisor teardown.
#[cfg(windows)]
fn restore_lid_on_windows(prior: i32) {
    let (ac, dc) = platform::decode_lid(prior);
    if (ac, dc) == (0, 0) {
        return;
    }
    let _ = sysutil::run_elevated_self(&["__set_lid__", &ac.to_string(), &dc.to_string()]);
}

#[cfg(windows)]
pub fn run_lid(_args: &[String]) -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
pub fn run_lid(args: &[String]) -> Result<()> {
    if args.len() != 8 {
        return Err(AppError::fail("lid supervisor expects 7 arguments"));
    }
    let prior_disable_sleep = parse_disable_sleep(&args[4])?;
    let mut cleanup = platform::supports_even_lid().then_some(LidCleanup {
        child: None,
        prior_disable_sleep,
    });
    let mode_char = args[1].as_str();
    if !matches!(mode_char, "d" | "i") {
        return Err(AppError::fail("lid supervisor: bad caffeinate mode"));
    }
    let no_display = mode_char == "i";
    let timeout_sec = optional_i64(&args[2])?;
    let wait_pid = optional_u32(&args[3])?;
    let trigger = args[5].clone();
    let detail = args[6].clone();
    let charge_target = if args[7].is_empty() {
        None
    } else {
        Some(parse_charge_target(&args[7])?)
    };
    if cleanup.is_none() {
        return Ok(());
    }

    let charging_up = if let Some(target) = charge_target {
        let initial = read_battery_status()?;
        let plan = plan_charge(target, &initial)?;
        if plan.already_met {
            return Ok(());
        }
        Some(plan.charging_up)
    } else {
        None
    };

    let ka = platform::keep_awake_command(no_display, timeout_sec, wait_pid)?;
    let child = sysutil::spawn_supervised_child(&ka.cmd)?;
    let child_pid = child.id();
    cleanup.as_mut().expect("supported platform").child = Some(child);
    sysutil::require_child_alive(child_pid, &ka.cmd)?;

    let mut s = Session::new();
    s.pid = sysutil::current_pid();
    s.mode = if no_display {
        "system-only".into()
    } else {
        "display+system".into()
    };
    s.trigger = trigger;
    s.detail = detail;
    s.started_at = Some(Utc::now());
    s.ends_at = timeout_sec.map(|t| Utc::now() + chrono::Duration::seconds(t));
    s.even_lid = true;
    s.prior_disable_sleep = prior_disable_sleep;
    s.capture_process_identity()?;
    session::write(&s)?;

    let stop = install_stop_flag();
    let start = Instant::now();
    let mut next_sudo = start + SUDO_HEARTBEAT;
    let mut last_check = Instant::now();
    let mut battery_failures = 0;
    loop {
        sleep(LID_POLL_INTERVAL);
        if stop.load(Ordering::Relaxed) || !sysutil::is_alive(child_pid) {
            break;
        }
        if let Some(timeout) = timeout_sec
            && start.elapsed() >= Duration::from_secs(timeout as u64)
        {
            break;
        }
        if let Some(pid) = wait_pid
            && !sysutil::is_alive(pid)
        {
            break;
        }
        // Battery commands stay on a 30-second cadence; liveness and timeout checks stay responsive.
        if last_check.elapsed() >= POLL_INTERVAL {
            last_check = Instant::now();
            if let (Some(target), Some(up)) = (charge_target, charging_up) {
                match charge_reached(target, up) {
                    Ok(true) => break,
                    Ok(false) => battery_failures = 0,
                    Err(error) if battery_failures_exhausted(&mut battery_failures, &error) => {
                        break;
                    }
                    Err(_) => {}
                }
            }
        }
        if Instant::now() >= next_sudo {
            let _ = platform::refresh_sudo_non_interactive();
            next_sudo = Instant::now() + SUDO_HEARTBEAT;
        }
    }
    Ok(())
}

#[cfg(not(windows))]
struct LidCleanup {
    child: Option<Child>,
    prior_disable_sleep: i32,
}

#[cfg(not(windows))]
impl Drop for LidCleanup {
    fn drop(&mut self) {
        let prior = self.prior_disable_sleep;
        if let Ok(current) = platform::read_disable_sleep()
            && current != prior
        {
            let _ = platform::set_disable_sleep_non_interactive(prior);
        }
        let restored = platform::read_disable_sleep().is_ok_and(|current| current == prior);
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if restored {
            session::delete_state_file();
        } else {
            commands::print_sleep_restore_rescue(prior);
        }
    }
}

#[cfg(not(windows))]
fn charge_reached(target: i32, charging_up: bool) -> Result<bool> {
    let status = read_battery_status()?;
    Ok(if charging_up {
        status.percent >= target
    } else {
        status.percent <= target
    })
}

#[cfg(not(windows))]
fn optional_i64(raw: &str) -> Result<Option<i64>> {
    if raw.is_empty() {
        return Ok(None);
    }
    match raw.parse::<i64>() {
        Ok(value) if value > 0 => Ok(Some(value)),
        _ => Err(AppError::fail(
            "lid supervisor timeout must be a positive integer",
        )),
    }
}

#[cfg(not(windows))]
fn optional_u32(raw: &str) -> Result<Option<u32>> {
    if raw.is_empty() {
        return Ok(None);
    }
    match raw.parse::<u32>() {
        Ok(value) if value > 0 => Ok(Some(value)),
        _ => Err(AppError::fail(
            "lid supervisor pid must be a positive integer",
        )),
    }
}

#[cfg(not(windows))]
fn parse_disable_sleep(raw: &str) -> Result<i32> {
    match raw.parse::<i32>() {
        Ok(value @ (0 | 1)) => Ok(value),
        _ => Err(AppError::fail("priorDisableSleep must be 0 or 1")),
    }
}

fn parse_charge_target(raw: &str) -> Result<i32> {
    match raw.parse::<i32>() {
        Ok(target @ 1..=100) => Ok(target),
        _ => Err(AppError::fail(
            "charge supervisor target must be an integer from 1 to 100",
        )),
    }
}

fn parse_bool(raw: &str, name: &str) -> Result<bool> {
    match raw {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(AppError::fail(format!(
            "charge supervisor {name} must be true or false"
        ))),
    }
}

fn battery_failures_exhausted(failures: &mut u8, error: &AppError) -> bool {
    *failures += 1;
    if *failures < MAX_BATTERY_FAILURES {
        return false;
    }
    eprintln!(
        "wake supervisor: battery read failed {MAX_BATTERY_FAILURES} consecutive times: {error}; stopping"
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(
        percent: i32,
        charging: bool,
        discharging: bool,
        neutral: Option<&str>,
    ) -> BatteryStatus {
        BatteryStatus {
            percent,
            charging,
            discharging,
            neutral_state: neutral.map(str::to_string),
        }
    }

    #[test]
    fn charge_plan_table() {
        for (battery, expected) in [
            (status(80, false, true, None), (true, false)),
            (status(90, false, true, None), (false, false)),
            (status(80, true, false, None), (true, false)),
            (status(90, true, false, None), (true, false)),
            (status(60, true, false, None), (false, true)),
            (status(80, false, false, None), (true, false)),
        ] {
            let plan = plan_charge(80, &battery).unwrap();
            assert_eq!((plan.already_met, plan.charging_up), expected);
        }
    }

    #[test]
    fn unreachable_charge_plan_table() {
        for battery in [
            status(70, false, true, None),
            status(70, false, false, Some("not charging or discharging")),
            status(70, false, false, None),
        ] {
            assert!(plan_charge(80, &battery).is_err());
        }
    }

    #[test]
    fn hidden_value_parsers_are_strict() {
        assert_eq!(parse_charge_target("80").unwrap(), 80);
        for invalid in ["", "0", "101", " 80", "8.0"] {
            assert!(parse_charge_target(invalid).is_err(), "target={invalid:?}");
        }
        assert!(parse_bool("true", "test").unwrap());
        assert!(!parse_bool("false", "test").unwrap());
        for invalid in ["", "True", "0", " false"] {
            assert!(parse_bool(invalid, "test").is_err(), "bool={invalid:?}");
        }

        #[cfg(not(windows))]
        {
            assert_eq!(optional_i64("").unwrap(), None);
            assert_eq!(optional_i64("60").unwrap(), Some(60));
            assert_eq!(optional_u32("42").unwrap(), Some(42));
            for invalid in ["0", "-1", " 1", "nope"] {
                assert!(optional_i64(invalid).is_err(), "timeout={invalid:?}");
            }
            for invalid in ["0", "-1", " 1", "nope"] {
                assert!(optional_u32(invalid).is_err(), "pid={invalid:?}");
            }
        }
    }
}
