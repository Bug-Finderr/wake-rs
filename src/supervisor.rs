//! Unix supervisors and the native Windows worker/guardian lifecycle.

use crate::error::{AppError, Result};
use crate::platform;
use crate::session::{self, Session};
use crate::sysutil;
use chrono::Utc;
use std::thread::sleep;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
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
        return Ok(if status.percent >= target {
            ChargePlan::already_met()
        } else {
            ChargePlan::waiting(true)
        });
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
        let mut failures = 0;
        loop {
            sleep(Duration::from_secs(1));
            if stop.load(Ordering::Relaxed) || !sysutil::is_alive(child_pid) {
                break;
            }
            if last_check.elapsed() >= POLL_INTERVAL {
                last_check = Instant::now();
                match read_battery_status() {
                    Ok(status) => {
                        failures = 0;
                        if (charging_up && status.percent >= target)
                            || (!charging_up && status.percent <= target)
                        {
                            break;
                        }
                    }
                    Err(error) if battery_failures_exhausted(&mut failures, &error) => break,
                    Err(_) => {}
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = session::delete_state_file();
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
        let no_display = match args[1].as_str() {
            "i" => true,
            "d" => false,
            _ => return Err(AppError::fail("lid supervisor: bad caffeinate mode")),
        };
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
            let plan = plan_charge(target, &read_battery_status()?)?;
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
        cleanup.as_mut().unwrap().child = Some(child);
        sysutil::require_child_alive(child_pid, &keep_awake.cmd)?;

        let now = Utc::now();
        let mut session = Session::new();
        session.pid = sysutil::current_pid();
        session.mode = if no_display {
            "system-only"
        } else {
            "display+system"
        }
        .into();
        session.trigger = trigger;
        session.detail = detail;
        session.started_at = Some(now);
        session.ends_at = timeout_sec.map(|timeout| now + chrono::Duration::seconds(timeout));
        session.even_lid = true;
        session.prior_disable_sleep = prior_disable_sleep;
        session.capture_process_identity()?;
        session::write(&session)?;

        let stop = install_stop_flag();
        let start = Instant::now();
        let mut next_sudo = start + SUDO_HEARTBEAT;
        let mut last_check = Instant::now();
        let mut failures = 0;
        loop {
            sleep(Duration::from_secs(1));
            if stop.load(Ordering::Relaxed) || !sysutil::is_alive(child_pid) {
                break;
            }
            if timeout_sec
                .is_some_and(|timeout| start.elapsed() >= Duration::from_secs(timeout as u64))
                || wait_pid.is_some_and(|pid| !sysutil::is_alive(pid))
            {
                break;
            }
            if last_check.elapsed() >= POLL_INTERVAL {
                last_check = Instant::now();
                if let (Some(target), Some(up)) = (charge_target, charging_up) {
                    match charge_reached(target, up) {
                        Ok(true) => break,
                        Ok(false) => failures = 0,
                        Err(error) if battery_failures_exhausted(&mut failures, &error) => break,
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
            if platform::read_disable_sleep().is_ok_and(|current| current != prior) {
                let _ = platform::set_disable_sleep_non_interactive(prior);
            }
            let restored = platform::read_disable_sleep().is_ok_and(|current| current == prior);
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            if restored {
                let _ = session::delete_state_file();
            } else {
                commands::print_sleep_restore_rescue(prior);
            }
        }
    }

    fn charge_reached(target: i32, up: bool) -> Result<bool> {
        let status = read_battery_status()?;
        Ok(if up {
            status.percent >= target
        } else {
            status.percent <= target
        })
    }

    fn optional_i64(raw: &str) -> Result<Option<i64>> {
        if raw.is_empty() {
            return Ok(None);
        }
        match raw.parse() {
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
        match raw.parse() {
            Ok(value) if value > 0 => Ok(Some(value)),
            _ => Err(AppError::fail(
                "lid supervisor pid must be a positive integer",
            )),
        }
    }

    fn parse_disable_sleep(raw: &str) -> Result<i32> {
        match raw.parse() {
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
    use chrono::DateTime;
    use std::path::PathBuf;

    struct WorkerSpec {
        no_display: bool,
        timeout: Option<Duration>,
        target: Option<sysutil::ProcessHandle>,
        charge: Option<(i32, bool)>,
        publish: bool,
        session: Session,
    }

    pub fn worker_command(
        session: &Session,
        timeout_sec: Option<i64>,
        target: Option<(u32, u64)>,
        charge: Option<(i32, bool)>,
        publish: bool,
    ) -> Result<Vec<String>> {
        if timeout_sec.is_some_and(|timeout| timeout <= 0) {
            return Err(AppError::fail("worker timeout must be positive"));
        }
        if !matches!(session.mode.as_str(), "system-only" | "display+system") {
            return Err(AppError::fail("invalid worker mode"));
        }
        let (target_pid, target_start) = target
            .map(|(pid, start)| (pid.to_string(), start.to_string()))
            .unwrap_or_default();
        let (charge_target, direction) = charge
            .map(|(value, up)| (value.to_string(), if up { "up" } else { "down" }.into()))
            .unwrap_or_default();
        Ok(vec![
            sysutil::self_exe()?,
            "__worker_windows__".into(),
            session.mode.clone(),
            timeout_sec
                .map(|value| value.to_string())
                .unwrap_or_default(),
            target_pid,
            target_start,
            charge_target,
            direction,
            session.trigger.clone(),
            session.detail.clone(),
            session
                .started_at
                .ok_or_else(|| AppError::fail("missing start time"))?
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
        run_worker_lifetime(&spec)?;
        if spec.publish {
            delete_owned_worker_state(&spec.session);
        }
        Ok(())
    }

    fn parse_worker_args(args: &[String]) -> Result<WorkerSpec> {
        if args.len() != 12 {
            return Err(AppError::fail(
                "Windows worker expects exactly 11 arguments",
            ));
        }
        let no_display = match args[1].as_str() {
            "system-only" => true,
            "display+system" => false,
            _ => return Err(AppError::fail("Windows worker has an invalid mode")),
        };
        let timeout = optional_u64(&args[2], "timeout")?.map(Duration::from_secs);
        let target_pid = optional_u32(&args[3], "target pid")?;
        let target_start = optional_u64(&args[4], "target creation time")?;
        if target_pid.is_some() != target_start.is_some() {
            return Err(AppError::fail(
                "Windows worker target identity is incomplete",
            ));
        }
        let target = match target_pid.zip(target_start) {
            Some((pid, start)) => match sysutil::open_exact_process(pid, start, false)? {
                Some(process) if process.is_running()? => Some(process),
                _ => return Err(AppError::fail("target process identity is no longer live")),
            },
            None => None,
        };
        let charge_target = if args[5].is_empty() {
            None
        } else {
            Some(parse_charge_target(&args[5])?)
        };
        let charge_up = match args[6].as_str() {
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
                "Windows worker charge identity is incomplete",
            ));
        }
        let identity = sysutil::current_identity()?;
        Ok(WorkerSpec {
            no_display,
            timeout,
            target,
            charge: charge_target.zip(charge_up),
            publish: parse_bool(&args[11], "publish")?,
            session: Session {
                pid: sysutil::current_pid(),
                mode: args[1].clone(),
                trigger: args[7].clone(),
                detail: args[8].clone(),
                started_at: Some(parse_timestamp(&args[9], "start time")?),
                ends_at: if args[10].is_empty() {
                    None
                } else {
                    Some(parse_timestamp(&args[10], "end time")?)
                },
                process_start: identity.start,
                process_command: identity.command,
                even_lid: !parse_bool(&args[11], "publish")?,
                guardian_pid: 0,
                guardian_start: 0,
                original_scheme: String::new(),
                original_ac: 0,
                original_dc: 0,
            },
        })
    }

    fn run_worker_lifetime(spec: &WorkerSpec) -> Result<()> {
        let start = Instant::now();
        let mut last_battery = Instant::now();
        let mut failures = 0;
        loop {
            if spec
                .timeout
                .is_some_and(|timeout| start.elapsed() >= timeout)
                || spec
                    .target
                    .as_ref()
                    .is_some_and(|target| !target.is_running().unwrap_or(false))
            {
                break;
            }
            if let Some((target, up)) = spec.charge
                && last_battery.elapsed() >= POLL_INTERVAL
            {
                last_battery = Instant::now();
                match read_battery_status() {
                    Ok(status) => {
                        failures = 0;
                        if (up && status.percent >= target) || (!up && status.percent <= target) {
                            break;
                        }
                    }
                    Err(error) if battery_failures_exhausted(&mut failures, &error) => break,
                    Err(_) => {}
                }
            }
            sleep(Duration::from_millis(250));
        }
        Ok(())
    }

    fn delete_owned_worker_state(worker: &Session) {
        for _ in 0..50 {
            if let Ok(_lock) = session::acquire_lock() {
                if let Some(session::SavedState::Valid(saved)) = session::read_saved_for_recovery()
                    && !saved.even_lid
                    && saved.pid == worker.pid
                    && saved.process_start == worker.process_start
                {
                    let _ = session::delete_state_file();
                }
                return;
            }
            sleep(Duration::from_millis(100));
        }
    }

    #[derive(Clone)]
    struct GuardianArgs {
        worker_pid: u32,
        worker_start: u64,
        scheme: String,
        ac: u32,
        dc: u32,
        state_path: PathBuf,
    }

    pub fn run_guardian(args: &[String]) -> Result<()> {
        let args = parse_guardian_args(args)?;
        let guardian = sysutil::current_identity()?;
        let snapshot = LidSnapshot {
            scheme: platform::parse_guid(&args.scheme)?,
            ac: args.ac,
            dc: args.dc,
        };
        let worker = sysutil::open_exact_process(args.worker_pid, args.worker_start, false)?;
        wait_for_authorization(&args, &guardian)?;
        if let Some(worker) = worker.filter(|worker| worker.is_running().unwrap_or(false)) {
            platform::enable_lid(&snapshot)?;
            while worker.is_running()? {
                sleep(Duration::from_millis(250));
            }
        }
        platform::restore_lid_snapshot(&snapshot)
    }

    fn wait_for_authorization(args: &GuardianArgs, guardian: &sysutil::Identity) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Some(session::SavedState::Valid(saved)) =
                session::read_saved_at(&args.state_path)
                && session_authorizes(&saved, args, sysutil::current_pid(), guardian.start)
            {
                return Ok(());
            }
            sleep(Duration::from_millis(100));
        }
        Err(AppError::fail(
            "guardian authorization state did not appear",
        ))
    }

    fn session_authorizes(
        saved: &Session,
        args: &GuardianArgs,
        guardian_pid: u32,
        guardian_start: u64,
    ) -> bool {
        saved.even_lid
            && saved.pid == args.worker_pid
            && saved.process_start == args.worker_start
            && saved.guardian_pid == guardian_pid
            && saved.guardian_start == guardian_start
            && saved.original_scheme == args.scheme
            && saved.original_ac == args.ac
            && saved.original_dc == args.dc
    }

    fn parse_guardian_args(args: &[String]) -> Result<GuardianArgs> {
        if args.len() != 7 {
            return Err(AppError::fail(
                "Windows guardian expects six immutable arguments",
            ));
        }
        let scheme = args[3].clone();
        platform::parse_guid(&scheme)?;
        let state_path = PathBuf::from(&args[6]);
        if !state_path.is_absolute() {
            return Err(AppError::fail("guardian state path must be absolute"));
        }
        Ok(GuardianArgs {
            worker_pid: positive(&args[1], "worker pid")?,
            worker_start: positive(&args[2], "worker creation time")?,
            scheme,
            ac: number(&args[4], "original AC value")?,
            dc: number(&args[5], "original DC value")?,
            state_path,
        })
    }

    fn optional_u64(raw: &str, name: &str) -> Result<Option<u64>> {
        if raw.is_empty() {
            Ok(None)
        } else {
            positive(raw, name).map(Some)
        }
    }

    fn optional_u32(raw: &str, name: &str) -> Result<Option<u32>> {
        optional_u64(raw, name)?.map_or(Ok(None), |value| {
            u32::try_from(value)
                .map(Some)
                .map_err(|_| AppError::fail(format!("Windows worker {name} is too large")))
        })
    }

    fn positive<T>(raw: &str, name: &str) -> Result<T>
    where
        T: std::str::FromStr + Default + PartialEq + ToString,
    {
        let value = raw
            .parse::<T>()
            .map_err(|_| AppError::fail(format!("invalid {name}")))?;
        if value == T::default() || value.to_string() != raw {
            Err(AppError::fail(format!("invalid {name}")))
        } else {
            Ok(value)
        }
    }

    fn number(raw: &str, name: &str) -> Result<u32> {
        let value = raw
            .parse::<u32>()
            .map_err(|_| AppError::fail(format!("invalid {name}")))?;
        if value.to_string() == raw {
            Ok(value)
        } else {
            Err(AppError::fail(format!("invalid {name}")))
        }
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

        fn strings(values: &[&str]) -> Vec<String> {
            values.iter().map(|value| (*value).into()).collect()
        }

        #[test]
        fn worker_argument_parser_is_strict() {
            let valid = strings(&[
                "__worker_windows__",
                "system-only",
                "60",
                "",
                "",
                "80",
                "up",
                "until-charge",
                "80%",
                "2024-01-02T03:04:05+00:00",
                "",
                "true",
            ]);
            assert!(parse_worker_args(&valid).unwrap().publish);
            assert!(parse_worker_args(&valid[..11]).is_err());
            let mut incomplete = valid;
            incomplete[3] = "42".into();
            assert!(parse_worker_args(&incomplete).is_err());
        }

        #[test]
        fn immutable_args_must_match_the_complete_session() {
            let args = GuardianArgs {
                worker_pid: 7,
                worker_start: 9,
                scheme: "381b4222-f694-41f0-9685-ff5bb260df2e".into(),
                ac: 1,
                dc: 2,
                state_path: PathBuf::from("C:\\state\\session.properties"),
            };
            let mut saved = Session {
                pid: 7,
                process_start: 9,
                guardian_pid: 11,
                guardian_start: 13,
                original_scheme: args.scheme.clone(),
                original_ac: 1,
                original_dc: 2,
                even_lid: true,
                ..Session::default()
            };
            assert!(session_authorizes(&saved, &args, 11, 13));
            saved.original_dc = 3;
            assert!(!session_authorizes(&saved, &args, 11, 13));
            saved.original_dc = 2;
            assert!(!session_authorizes(&saved, &args, 11, 14));
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
            assert!(parse_charge_target(invalid).is_err());
        }
        assert!(parse_bool("true", "test").unwrap());
        assert!(!parse_bool("false", "test").unwrap());
    }
}
