//! Detached supervisors: `until-charge` (all platforms) and `even-lid` (macOS), plus the battery
//! status / charge-plan model they share with the foreground command.

#[cfg(not(windows))]
use crate::commands;
use crate::error::{AppError, Result};
use crate::platform;
#[cfg(not(windows))]
use crate::session::PHASE_ACTIVE;
use crate::session::{self, Session};
use crate::sysutil;
use chrono::Utc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const LID_POLL_INTERVAL: Duration = Duration::from_secs(1);
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

/// Cross-platform stop flag: on Unix, SIGTERM/SIGINT set it (so `wake stop` lets us tear down
/// cleanly instead of dying instantly); on Windows it is never set (stop is a forcible kill).
fn install_stop_flag() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    {
        let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&flag));
        let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&flag));
    }
    flag
}

// ---- until-charge supervisor ----

pub fn run_charge(args: &[String]) -> Result<()> {
    if args.len() < 4 {
        return Err(AppError::fail("supervisor: bad args"));
    }
    let target: i32 = args[1]
        .parse()
        .map_err(|_| AppError::fail("supervisor: bad target"))?;
    let no_display = args[2] == "true";
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
    sysutil::require_child_alive(child.id(), &ka.cmd);

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
    loop {
        sleep(LID_POLL_INTERVAL);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if !sysutil::is_alive(child.id()) {
            break;
        }
        if last_check.elapsed() >= POLL_INTERVAL {
            last_check = Instant::now();
            if let Ok(status) = read_battery_status() {
                let reached = if charging_up {
                    status.percent >= target
                } else {
                    status.percent <= target
                };
                if reached {
                    break;
                }
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

/// Restore the prior lid action when a Windows even-lid charge supervisor tears down (target reached
/// or stop). Re-elevates via the `__set_lid__` helper; best-effort, never blocks teardown.
#[cfg(windows)]
fn restore_lid_on_windows(prior: i32) {
    let (ac, dc) = platform::decode_lid(prior);
    if (ac, dc) == (0, 0) {
        return;
    }
    let _ = sysutil::run_elevated_self(&["__set_lid__", &ac.to_string(), &dc.to_string()]);
}

// ---- even-lid supervisor (macOS) ----

/// Windows never spawns the lid supervisor (even-lid is overlaid on the normal session via the
/// power-plan lid action), so this is an inert stub there.
#[cfg(windows)]
pub fn run_lid(_args: &[String]) -> Result<()> {
    Ok(())
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

    let mut charging_up: Option<bool> = None;
    if let Some(target) = charge_target {
        let initial = read_battery_status()?;
        let plan = plan_charge(target, &initial)?;
        if plan.already_met {
            return Ok(());
        }
        charging_up = Some(plan.charging_up);
    }

    let ka = platform::keep_awake_command(no_display, timeout_sec, wait_pid)?;
    let mut child = sysutil::spawn_supervised_child(&ka.cmd)?;
    sysutil::require_child_alive(child.id(), &ka.cmd);

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
    s.phase = PHASE_ACTIVE.into();
    if let Err(e) = s
        .capture_process_identity()
        .and_then(|_| session::write(&s))
    {
        let _ = child.kill();
        lid_cleanup(child.id(), prior_disable_sleep);
        return Err(e);
    }

    let stop = install_stop_flag();
    let start = Instant::now();
    let mut next_sudo = start + SUDO_HEARTBEAT;
    let mut last_check = Instant::now();
    loop {
        sleep(LID_POLL_INTERVAL);
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
        // Gate the battery poll to POLL_INTERVAL: forking pmset every second would run it ~86k
        // times/day. Liveness/timeout/wait_pid stay at 1s; the sudo heartbeat keeps its own cadence.
        if last_check.elapsed() >= POLL_INTERVAL {
            last_check = Instant::now();
            if let Some(target) = charge_target
                && charge_reached(target, charging_up)
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
    lid_cleanup(child.id(), prior_disable_sleep);
    Ok(())
}

/// Restore SleepDisabled and remove state, matching the reference lid teardown.
#[cfg(not(windows))]
fn lid_cleanup(child_pid: u32, prior_disable_sleep: i32) {
    if let Ok(current) = platform::read_disable_sleep()
        && current != prior_disable_sleep
    {
        let _ = platform::set_disable_sleep_non_interactive(prior_disable_sleep);
    }
    let restored = platform::read_disable_sleep()
        .map(|c| c == prior_disable_sleep)
        .unwrap_or(false);
    sysutil::terminate(child_pid);
    if restored {
        session::delete_state_file();
    } else {
        commands::print_sleep_restore_rescue(prior_disable_sleep);
    }
}

#[cfg(not(windows))]
fn charge_reached(target: i32, charging_up: Option<bool>) -> bool {
    match charging_up {
        None => false,
        Some(up) => read_battery_status()
            .map(|s| {
                if up {
                    s.percent >= target
                } else {
                    s.percent <= target
                }
            })
            .unwrap_or(false),
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
    fn discharging_at_target_is_already_met() {
        let p = plan_charge(80, &status(80, false, true, None)).unwrap();
        assert!(p.already_met);
    }

    #[test]
    fn discharging_below_target_errors() {
        assert!(plan_charge(80, &status(70, false, true, None)).is_err());
    }

    #[test]
    fn discharging_above_target_waits_not_charging_up() {
        let p = plan_charge(80, &status(90, false, true, None)).unwrap();
        assert!(!p.already_met);
        assert!(!p.charging_up);
    }

    #[test]
    fn charging_at_or_above_target_is_already_met() {
        let p = plan_charge(80, &status(80, true, false, None)).unwrap();
        assert!(p.already_met);
    }

    #[test]
    fn charging_below_target_waits_charging_up() {
        let p = plan_charge(80, &status(60, true, false, None)).unwrap();
        assert!(!p.already_met);
        assert!(p.charging_up);
    }

    #[test]
    fn neutral_at_target_is_already_met() {
        let p = plan_charge(80, &status(80, false, false, None)).unwrap();
        assert!(p.already_met);
    }

    #[test]
    fn neutral_off_target_with_state_errors() {
        assert!(
            plan_charge(
                80,
                &status(70, false, false, Some("not charging or discharging"))
            )
            .is_err()
        );
    }

    #[test]
    fn neutral_off_target_indeterminate_errors() {
        assert!(plan_charge(80, &status(70, false, false, None)).is_err());
    }
}
