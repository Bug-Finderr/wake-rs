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
    let [json, token] = args else {
        return Err(AppError::fail(
            "supervisor expects a run specification and process lease",
        ));
    };
    let spec: RunSpec = serde_json::from_str(json)
        .map_err(|error| AppError::fail(format!("invalid supervisor specification: {error}")))?;
    spec.validate()?;
    let lease = session::claim_process_lease(token)?;
    supervise(spec, lease.reference())
}

fn supervise(spec: RunSpec, owner: session::LeaseRef) -> Result<()> {
    let startup = start_session(spec, owner.clone());
    let (mut inhibitor, saved, started) = match startup {
        Ok(started) => started,
        Err(error) => {
            let cleanup = session::remove_if_owner(&owner).map(|_| ());
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(AppError::fail(format!(
                    "{error}; startup cleanup failed: {cleanup}"
                ))),
            };
        }
    };
    if lid::uses_watchdog(&saved.spec) {
        lid::wait_ready(&saved)?;
    }

    let result = supervise_loop(&saved, &mut inhibitor, started);
    drop(inhibitor);
    if lid::uses_watchdog(&saved.spec) {
        return result;
    }
    let remove = session::remove_if_matches(&saved).map(|_| ());
    let clear = session::clear_stop(&saved).map(|_| ());
    result.and(remove).and(clear)
}

fn start_session(
    spec: RunSpec,
    owner: session::LeaseRef,
) -> Result<(platform::Inhibitor, Session, Instant)> {
    let pending = session::read_saved_for_recovery()?
        .ok_or_else(|| AppError::fail("supervisor startup was cancelled"))?;
    if pending.owner.pid != 0 || pending.owner.token != owner.token || pending.spec != spec {
        return Err(AppError::fail("pending supervisor state changed"));
    }

    let lock = loop {
        if session::stop_requested(&pending)? {
            return Err(AppError::fail("session stopped during startup"));
        }
        if let Some(lock) = session::try_acquire_lock()? {
            break lock;
        }
        sleep(Duration::from_millis(100));
    };
    let current = session::read_saved_for_recovery()?;
    if current
        .as_ref()
        .is_none_or(|current| current.owner != pending.owner || current.spec != pending.spec)
        || session::stop_requested(&pending)?
    {
        return Err(AppError::fail("supervisor startup was cancelled"));
    }

    let mut inhibitor = platform::Inhibitor::start(spec.mode, spec.even_lid)?;
    sleep(STARTUP_SETTLE);
    if !inhibitor.alive() {
        return Err(platform::inhibitor_startup_error(spec.even_lid));
    }
    let started = Instant::now();
    let started_at = Utc::now();
    let saved = Session {
        owner,
        ends_at: spec.trigger.deadline(started_at)?,
        note: inhibitor.note().map(str::to_string),
        spec,
        started_at,
    };
    session::write_ready(&saved)?;
    drop(lock);
    Ok((inhibitor, saved, started))
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
        if lid::uses_watchdog(&saved.spec) {
            lid::ensure_ready(saved)?;
        }
        if saved
            .spec
            .trigger
            .is_complete(Utc::now(), started.elapsed())
        {
            return Ok(());
        }
        if let Some(process) = saved.spec.trigger.process()
            && !sysutil::process_identity_matches(&process.identity())?
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
