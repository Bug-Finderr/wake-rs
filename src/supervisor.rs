use crate::error::{AppError, Result};
use crate::lid;
use crate::platform;
use crate::run::{BatteryStatus, ChargeDirection, RunSpec, Trigger};
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
    let mut inhibitor = platform::Inhibitor::start(spec.mode)?;
    sleep(STARTUP_SETTLE);
    if !inhibitor.alive() {
        return Err(AppError::fail("sleep inhibitor exited during startup"));
    }
    let started = Instant::now();
    let started_at = Utc::now();
    let saved = Session {
        owner: sysutil::capture_process(sysutil::current_pid())?,
        ends_at: spec.trigger.session_ends_at(started_at),
        note: inhibitor.note().map(str::to_string),
        spec,
        started_at,
    };
    session::write(&saved)?;
    if saved.spec.even_lid {
        lid::wait_ready(&saved)?;
    }

    let result = supervise_loop(&saved, &mut inhibitor, started);
    drop(inhibitor);
    if saved.spec.even_lid {
        return result;
    }
    let remove = session::remove_if_matches(&saved).map(|_| ());
    let clear = session::clear_stop(&saved).map(|_| ());
    result.and(remove).and(clear)
}

fn supervise_loop(
    saved: &Session,
    inhibitor: &mut platform::Inhibitor,
    started: Instant,
) -> Result<()> {
    let signal = install_stop_flag();
    let mut next_battery = Instant::now();
    loop {
        if signal.load(Ordering::Relaxed)
            || session::stop_requested(saved).is_ok_and(|requested| requested)
        {
            return Ok(());
        }
        if !inhibitor.alive() {
            return Err(AppError::fail("sleep inhibitor exited unexpectedly"));
        }
        if saved.spec.even_lid {
            lid::ensure_ready(saved)?;
        }
        if saved
            .spec
            .trigger
            .is_complete(Utc::now(), started.elapsed())
        {
            return Ok(());
        }
        if saved
            .spec
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
            } = saved.spec.trigger
                && charge_reached(target, direction)
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
