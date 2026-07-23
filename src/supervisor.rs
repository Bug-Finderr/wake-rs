//! Unix supervisors and the native Windows worker/guardian lifecycle.

use crate::error::{AppError, Result};
use crate::platform;
use crate::session::{self, Session};
use crate::sysutil;
use chrono::Utc;
use std::thread::sleep;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
#[cfg(windows)]
const LIFETIME_POLL_INTERVAL: Duration = Duration::from_millis(250);
const MAX_BATTERY_FAILURES: u8 = 3;

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
        Self {
            already_met: true,
            charging_up: false,
        }
    }

    fn waiting(charging_up: bool) -> Self {
        Self {
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
                "--until-charge {target} is unreachable while battery is discharging at {}%; connect power or choose a target at or below the current charge",
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

fn battery_failures_exhausted(failures: &mut u8, error: &AppError) -> bool {
    *failures += 1;
    if *failures < MAX_BATTERY_FAILURES {
        return false;
    }
    eprintln!(
        "wake worker: battery read failed {MAX_BATTERY_FAILURES} consecutive times: {error}; stopping"
    );
    true
}

#[cfg(not(windows))]
mod unix {
    use super::*;
    use crate::commands;
    use std::process::Child;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    const SUDO_HEARTBEAT: Duration = Duration::from_secs(180);

    fn install_stop_flag() -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
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
        if args.len() != 4 {
            return Err(AppError::fail("charge supervisor expects 3 arguments"));
        }
        let target = parse_charge_target(&args[1])?;
        let no_display = parse_bool(&args[2], "no-display")?;
        let mode = args[3].clone();
        let (initial, plan) = match read_battery_status()
            .and_then(|status| plan_charge(target, &status).map(|plan| (status, plan)))
        {
            Ok(value) => value,
            Err(error) => {
                eprintln!("wake supervisor: {error}");
                return Ok(());
            }
        };
        if plan.already_met {
            return Ok(());
        }
        let charging_up = plan.charging_up;

        let keep_awake = platform::keep_awake_command(no_display, None, None)?;
        let mut child = sysutil::spawn_supervised_child(&keep_awake.cmd)?;
        let child_pid = child.id();
        sysutil::require_child_alive(child_pid, &keep_awake.cmd)?;

        let mut session = Session::new();
        session.pid = sysutil::current_pid();
        session.mode = mode;
        session.trigger = "until-charge".into();
        session.detail = format!(
            "{target}% (was {}%, {})",
            initial.percent,
            if charging_up {
                "charging up"
            } else {
                "discharging down"
            }
        );
        session.started_at = Some(Utc::now());
        session.capture_process_identity()?;
        if let Err(error) = session::write(&session) {
            let _ = child.kill();
            return Err(error);
        }

        let stop = install_stop_flag();
        let mut last_check = Instant::now();
        let mut battery_failures = 0;
        loop {
            sleep(Duration::from_secs(1));
            if stop.load(Ordering::Relaxed) || !sysutil::is_alive(child_pid) {
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
                    Err(error) if battery_failures_exhausted(&mut battery_failures, &error) => {
                        break;
                    }
                    Err(_) => {}
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        session::delete_state_file();
        Ok(())
    }

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

        let keep_awake = platform::keep_awake_command(no_display, timeout_sec, wait_pid)?;
        let child = sysutil::spawn_supervised_child(&keep_awake.cmd)?;
        let child_pid = child.id();
        cleanup.as_mut().expect("supported platform").child = Some(child);
        sysutil::require_child_alive(child_pid, &keep_awake.cmd)?;

        let mut session = Session::new();
        session.pid = sysutil::current_pid();
        session.mode = if no_display {
            "system-only".into()
        } else {
            "display+system".into()
        };
        session.trigger = trigger;
        session.detail = detail;
        session.started_at = Some(Utc::now());
        session.ends_at =
            timeout_sec.map(|timeout| Utc::now() + chrono::Duration::seconds(timeout));
        session.even_lid = true;
        session.prior_disable_sleep = prior_disable_sleep;
        session.capture_process_identity()?;
        session::write(&session)?;

        let stop = install_stop_flag();
        let start = Instant::now();
        let mut next_sudo = start + SUDO_HEARTBEAT;
        let mut last_check = Instant::now();
        let mut battery_failures = 0;
        loop {
            sleep(Duration::from_secs(1));
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

    struct LidCleanup {
        child: Option<Child>,
        prior_disable_sleep: i32,
    }

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

    fn charge_reached(target: i32, charging_up: bool) -> Result<bool> {
        let status = read_battery_status()?;
        Ok(if charging_up {
            status.percent >= target
        } else {
            status.percent <= target
        })
    }

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

    fn parse_disable_sleep(raw: &str) -> Result<i32> {
        match raw.parse::<i32>() {
            Ok(value @ (0 | 1)) => Ok(value),
            _ => Err(AppError::fail("priorDisableSleep must be 0 or 1")),
        }
    }

    fn parse_charge_target(raw: &str) -> Result<i32> {
        super::parse_charge_target(raw)
    }

    fn parse_bool(raw: &str, name: &str) -> Result<bool> {
        super::parse_bool(raw, name)
    }
}

#[cfg(not(windows))]
pub use unix::{run_charge, run_lid};

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::platform::LidSnapshot;
    #[cfg(test)]
    use crate::platform::RestoreDecision;
    use crate::session::{GuardianMode, GuardianRequest};
    use chrono::DateTime;
    use std::fs;
    use std::path::Path;

    struct WorkerSpec {
        no_display: bool,
        timeout: Option<Duration>,
        wait_pid: Option<u32>,
        charge: Option<(i32, bool)>,
        publish: bool,
        session: Session,
    }

    pub fn worker_command(
        session: &Session,
        timeout_sec: Option<i64>,
        wait_pid: Option<u32>,
        charge: Option<(i32, bool)>,
        publish: bool,
    ) -> Result<Vec<String>> {
        if timeout_sec.is_some_and(|timeout| timeout <= 0) {
            return Err(AppError::fail("worker timeout must be positive"));
        }
        let no_display = match session.mode.as_str() {
            "system-only" => true,
            "display+system" => false,
            _ => return Err(AppError::fail("invalid worker mode")),
        };
        let (charge_target, charge_direction) = charge
            .map(|(target, up)| {
                (
                    target.to_string(),
                    if up { "up" } else { "down" }.to_string(),
                )
            })
            .unwrap_or_default();
        Ok(vec![
            sysutil::self_exe()?,
            "__worker_windows__".into(),
            if no_display {
                "system-only"
            } else {
                "display+system"
            }
            .into(),
            timeout_sec
                .map(|value| value.to_string())
                .unwrap_or_default(),
            wait_pid.map(|value| value.to_string()).unwrap_or_default(),
            charge_target,
            charge_direction,
            session.trigger.clone(),
            session.detail.clone(),
            session
                .started_at
                .ok_or_else(|| AppError::fail("worker session has no start time"))?
                .to_rfc3339(),
            session
                .ends_at
                .map(|time| time.to_rfc3339())
                .unwrap_or_default(),
            publish.to_string(),
        ])
    }

    pub fn run_worker(args: &[String]) -> Result<()> {
        let spec = parse_worker_args(args)?;
        let _execution_state = platform::ExecutionStateGuard::acquire(spec.no_display)?;
        if spec.publish {
            session::write(&spec.session)?;
        }
        run_worker_lifetime(&spec);
        if spec.publish {
            delete_owned_worker_state(&spec.session);
        }
        Ok(())
    }

    fn parse_worker_args(args: &[String]) -> Result<WorkerSpec> {
        if args.len() != 11 {
            return Err(AppError::fail(
                "Windows worker expects exactly 10 arguments",
            ));
        }
        let no_display = match args[1].as_str() {
            "system-only" => true,
            "display+system" => false,
            _ => return Err(AppError::fail("Windows worker has an invalid mode")),
        };
        let timeout = optional_positive_u64(&args[2], "timeout")?.map(Duration::from_secs);
        let wait_pid = optional_positive_u32(&args[3], "wait pid")?;
        let charge_target = if args[4].is_empty() {
            None
        } else {
            Some(parse_charge_target(&args[4])?)
        };
        let charge_up = match args[5].as_str() {
            "" => None,
            "up" => Some(true),
            "down" => Some(false),
            _ => {
                return Err(AppError::fail(
                    "Windows worker has an invalid charge direction",
                ));
            }
        };
        if charge_target.is_some() != charge_up.is_some() {
            return Err(AppError::fail(
                "Windows worker charge arguments are incomplete",
            ));
        }
        let started_at = parse_timestamp(&args[8], "start time")?;
        let ends_at = if args[9].is_empty() {
            None
        } else {
            Some(parse_timestamp(&args[9], "end time")?)
        };
        let publish = parse_bool(&args[10], "publish")?;
        let identity = sysutil::current_identity()?;
        Ok(WorkerSpec {
            no_display,
            timeout,
            wait_pid,
            charge: charge_target.zip(charge_up),
            publish,
            session: Session {
                pid: sysutil::current_pid(),
                mode: args[1].clone(),
                trigger: args[6].clone(),
                detail: args[7].clone(),
                started_at: Some(started_at),
                ends_at,
                process_start: identity.start,
                process_command: identity.command,
                even_lid: !publish,
                guardian_pid: 0,
                guardian_start: 0,
                original_scheme: String::new(),
                original_ac: 0,
                original_dc: 0,
            },
        })
    }

    fn run_worker_lifetime(spec: &WorkerSpec) {
        let start = Instant::now();
        let mut last_battery_check = Instant::now();
        let mut battery_failures = 0;
        loop {
            if spec
                .timeout
                .is_some_and(|timeout| start.elapsed() >= timeout)
            {
                break;
            }
            if spec.wait_pid.is_some_and(|pid| !sysutil::is_alive(pid)) {
                break;
            }
            if let Some((target, charging_up)) = spec.charge
                && last_battery_check.elapsed() >= POLL_INTERVAL
            {
                last_battery_check = Instant::now();
                match read_battery_status() {
                    Ok(status) => {
                        battery_failures = 0;
                        if (charging_up && status.percent >= target)
                            || (!charging_up && status.percent <= target)
                        {
                            break;
                        }
                    }
                    Err(error) if battery_failures_exhausted(&mut battery_failures, &error) => {
                        break;
                    }
                    Err(_) => {}
                }
            }
            sleep(LIFETIME_POLL_INTERVAL);
        }
    }

    fn delete_owned_worker_state(worker: &Session) {
        for _ in 0..50 {
            match session::acquire_lock() {
                Ok(_lock) => {
                    if let Some(session::SavedState::Valid(saved)) =
                        session::read_saved_for_recovery()
                        && saved.pid == worker.pid
                        && saved.process_start == worker.process_start
                        && !saved.even_lid
                    {
                        session::delete_state_file();
                    }
                    return;
                }
                Err(_) => sleep(Duration::from_millis(100)),
            }
        }
    }

    pub fn run_guardian(args: &[String]) -> Result<()> {
        if args.len() != 2 {
            return Err(AppError::fail(
                "Windows guardian expects one absolute request path",
            ));
        }
        let request_path = Path::new(&args[1]);
        let request = match session::read_guardian_request(request_path) {
            Ok(request) => request,
            Err(error) => {
                if request_path.is_absolute() {
                    let _ = session::write_marker(
                        &request_path.with_extension("error"),
                        error.message(),
                    );
                }
                return Err(error);
            }
        };
        let paths = session::marker_paths_for_request(&request);
        let result = if request_path != paths.request {
            Err(AppError::fail(
                "guardian request name does not match the worker identity",
            ))
        } else {
            match request.mode {
                GuardianMode::Start => start_guardian(request_path, &request, &paths.ready),
                GuardianMode::Recover => recover_guardian(request_path, &request, &paths.ready),
            }
        };
        if let Err(error) = &result {
            let _ = session::write_marker(&paths.error, error.message());
        }
        result
    }

    fn start_guardian(request_path: &Path, request: &GuardianRequest, ready: &Path) -> Result<()> {
        let Some(worker) = sysutil::open_session_process(&request.session, false)? else {
            return Err(AppError::fail("worker exited before the guardian started"));
        };
        if !worker.is_running()? {
            return Err(AppError::fail("worker exited before the guardian started"));
        }
        let guardian = sysutil::current_identity()?;
        let snapshot = platform::capture_lid_snapshot()?;
        let mut saved = request.session.clone();
        saved.guardian_pid = sysutil::current_pid();
        saved.guardian_start = guardian.start;
        saved.original_scheme = platform::format_guid(&snapshot.scheme);
        saved.original_ac = snapshot.ac;
        saved.original_dc = snapshot.dc;
        let state_path = session::request_state_file(request_path)?;
        session::write_state_at(&state_path, &saved)?;

        match enable_lid(&snapshot) {
            Ok(()) => {}
            Err(failure) => {
                if !failure.wrote_any
                    && let Err(delete_error) = session::delete_state_at(&state_path)
                {
                    return Err(AppError::fail(format!(
                        "{}; unwritten recovery state could not be removed: {delete_error}",
                        failure.error
                    )));
                }
                return Err(failure.error);
            }
        }
        if let Err(marker_error) = session::write_marker(ready, "ready") {
            let rollback = platform::restore_lid_snapshot(&snapshot);
            return Err(match rollback {
                Ok(()) => AppError::fail(format!(
                    "{marker_error}; the lid change was rolled back, but recovery state was retained"
                )),
                Err(rollback_error) => AppError::fail(format!(
                    "{marker_error}; rollback also failed: {rollback_error}; recovery state was retained"
                )),
            });
        }

        while worker.is_running()? {
            sleep(Duration::from_millis(250));
        }
        platform::restore_lid_snapshot(&snapshot).map_err(|error| {
            AppError::fail(format!(
                "{error}; recovery state retained at {} and a later recovery will require UAC",
                state_path.display()
            ))
        })?;
        session::delete_state_at(&state_path)?;
        Ok(())
    }

    struct EnableFailure {
        error: AppError,
        wrote_any: bool,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum EnableStep {
        WriteAc,
        WriteDc,
        Reactivate,
        Verify,
    }

    const ENABLE_STEPS: [EnableStep; 4] = [
        EnableStep::WriteAc,
        EnableStep::WriteDc,
        EnableStep::Reactivate,
        EnableStep::Verify,
    ];

    fn enable_lid(snapshot: &LidSnapshot) -> std::result::Result<(), EnableFailure> {
        let mut wrote_any = false;
        let mut expected = (snapshot.ac, snapshot.dc);
        for step in ENABLE_STEPS {
            let result = match step {
                EnableStep::WriteAc => platform::write_lid_ac(&snapshot.scheme, 0).map(|()| {
                    wrote_any = true;
                    expected.0 = 0;
                }),
                EnableStep::WriteDc => match platform::read_lid_values(&snapshot.scheme) {
                    Ok(current) if current == expected => {
                        platform::write_lid_dc(&snapshot.scheme, 0).map(|()| {
                            wrote_any = true;
                            expected.1 = 0;
                        })
                    }
                    Ok((ac, dc)) => Err(AppError::fail(format!(
                        "lid values changed during startup (current AC={ac} DC={dc}); refusing the DC write"
                    ))),
                    Err(error) => Err(error),
                },
                EnableStep::Reactivate => platform::reactivate_if_still_active(&snapshot.scheme),
                EnableStep::Verify => match platform::read_lid_values(&snapshot.scheme) {
                    Ok((0, 0)) => Ok(()),
                    Ok((ac, dc)) => Err(AppError::fail(format!(
                        "failed to verify --even-lid (current AC={ac} DC={dc})"
                    ))),
                    Err(error) => Err(error),
                },
            };
            if let Err(error) = result {
                let error = if wrote_any {
                    match rollback_startup(snapshot, expected) {
                        Ok(()) => AppError::fail(format!(
                            "{error}; the attempted lid change was rolled back, but recovery state was retained"
                        )),
                        Err(rollback) => AppError::fail(format!(
                            "{error}; rollback also failed: {rollback}; recovery state was retained"
                        )),
                    }
                } else {
                    error
                };
                return Err(EnableFailure { error, wrote_any });
            }
        }
        Ok(())
    }

    fn rollback_startup(snapshot: &LidSnapshot, expected: (u32, u32)) -> Result<()> {
        let current = platform::read_lid_values(&snapshot.scheme)?;
        if current == (snapshot.ac, snapshot.dc) {
            return Ok(());
        }
        if current != expected {
            return Err(AppError::fail(format!(
                "lid values changed during startup (current AC={} DC={}); refusing rollback",
                current.0, current.1
            )));
        }
        platform::write_lid_ac(&snapshot.scheme, snapshot.ac)?;
        let after_ac = platform::read_lid_values(&snapshot.scheme)?;
        if after_ac != (snapshot.ac, expected.1) {
            return Err(AppError::fail(format!(
                "lid values changed during startup rollback (current AC={} DC={}); refusing the DC write",
                after_ac.0, after_ac.1
            )));
        }
        platform::write_lid_dc(&snapshot.scheme, snapshot.dc)?;
        platform::reactivate_if_still_active(&snapshot.scheme)?;
        let after = platform::read_lid_values(&snapshot.scheme)?;
        if after == (snapshot.ac, snapshot.dc) {
            Ok(())
        } else {
            Err(AppError::fail(format!(
                "startup rollback did not verify (current AC={} DC={})",
                after.0, after.1
            )))
        }
    }

    fn recover_guardian(
        request_path: &Path,
        request: &GuardianRequest,
        ready: &Path,
    ) -> Result<()> {
        let state_path = session::request_state_file(request_path)?;
        let expected = request
            .expected_state
            .as_ref()
            .ok_or_else(|| AppError::fail("recovery request has no expected state"))?;
        let current = fs::read(&state_path)
            .map_err(|error| AppError::fail(format!("could not read recovery state: {error}")))?;
        if &current != expected {
            return Err(AppError::fail(
                "recovery state changed after authorization; no power write was attempted",
            ));
        }
        if exact_process_is_running(request.session.pid, request.session.process_start)? {
            return Err(AppError::fail(
                "worker is still running; refusing crash recovery",
            ));
        }
        if exact_process_is_running(request.session.guardian_pid, request.session.guardian_start)? {
            return Err(AppError::fail(
                "original guardian is still running; refusing duplicate recovery",
            ));
        }
        let snapshot = LidSnapshot {
            scheme: platform::parse_guid(&request.session.original_scheme)?,
            ac: request.session.original_ac,
            dc: request.session.original_dc,
        };
        platform::restore_lid_snapshot(&snapshot).map_err(|error| {
            AppError::fail(format!(
                "{error}; recovery state retained at {}",
                state_path.display()
            ))
        })?;
        session::delete_state_at(&state_path)?;
        session::write_marker(ready, "recovered")?;
        Ok(())
    }

    fn exact_process_is_running(pid: u32, start: u64) -> Result<bool> {
        Ok(sysutil::open_exact_process(pid, start, false)?
            .is_some_and(|process| process.is_running().unwrap_or(false)))
    }

    fn optional_positive_u64(raw: &str, name: &str) -> Result<Option<u64>> {
        if raw.is_empty() {
            return Ok(None);
        }
        match raw.parse() {
            Ok(value) if value > 0 => Ok(Some(value)),
            _ => Err(AppError::fail(format!(
                "Windows worker {name} must be a positive integer"
            ))),
        }
    }

    fn optional_positive_u32(raw: &str, name: &str) -> Result<Option<u32>> {
        optional_positive_u64(raw, name)?.map_or(Ok(None), |value| {
            u32::try_from(value)
                .map(Some)
                .map_err(|_| AppError::fail(format!("Windows worker {name} is too large")))
        })
    }

    fn parse_timestamp(raw: &str, name: &str) -> Result<DateTime<Utc>> {
        let time = DateTime::parse_from_rfc3339(raw)
            .map(|time| time.with_timezone(&Utc))
            .map_err(|_| AppError::fail(format!("Windows worker has an invalid {name}")))?;
        if time.to_rfc3339() == raw {
            Ok(time)
        } else {
            Err(AppError::fail(format!(
                "Windows worker {name} is not canonical UTC"
            )))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn args(values: &[&str]) -> Vec<String> {
            values.iter().map(|value| (*value).to_string()).collect()
        }

        #[test]
        fn worker_argument_parser_is_strict() {
            let valid = args(&[
                "__worker_windows__",
                "system-only",
                "60",
                "42",
                "80",
                "up",
                "until-charge",
                "80%",
                "2024-01-02T03:04:05+00:00",
                "2024-01-02T03:05:05+00:00",
                "true",
            ]);
            let parsed = parse_worker_args(&valid).unwrap();
            assert!(parsed.no_display);
            assert_eq!(parsed.wait_pid, Some(42));
            assert_eq!(parsed.charge, Some((80, true)));
            assert!(parsed.publish);

            for invalid in [
                &valid[..10],
                &args(&[
                    "__worker_windows__",
                    "bad",
                    "",
                    "",
                    "",
                    "",
                    "timed",
                    "1m",
                    "2024-01-02T03:04:05+00:00",
                    "",
                    "true",
                ]),
                &args(&[
                    "__worker_windows__",
                    "system-only",
                    "0",
                    "",
                    "",
                    "",
                    "timed",
                    "1m",
                    "2024-01-02T03:04:05+00:00",
                    "",
                    "true",
                ]),
                &args(&[
                    "__worker_windows__",
                    "system-only",
                    "",
                    "",
                    "80",
                    "",
                    "until-charge",
                    "80%",
                    "2024-01-02T03:04:05+00:00",
                    "",
                    "true",
                ]),
            ] {
                assert!(parse_worker_args(invalid).is_err());
            }
        }

        #[test]
        fn guardian_enable_order_is_write_write_activate_verify() {
            assert_eq!(
                ENABLE_STEPS,
                [
                    EnableStep::WriteAc,
                    EnableStep::WriteDc,
                    EnableStep::Reactivate,
                    EnableStep::Verify,
                ]
            );
            assert_eq!(
                platform::restoration_decision((0, 0), (1, 2)),
                RestoreDecision::RestoreAppliedValues
            );
        }
    }
}

#[cfg(windows)]
pub use windows::{run_guardian, run_worker, worker_command};

fn parse_charge_target(raw: &str) -> Result<i32> {
    match raw.parse::<i32>() {
        Ok(target @ 1..=100) => Ok(target),
        _ => Err(AppError::fail(
            "charge target must be an integer from 1 to 100",
        )),
    }
}

fn parse_bool(raw: &str, name: &str) -> Result<bool> {
    match raw {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(AppError::fail(format!("{name} must be true or false"))),
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
    }
}
