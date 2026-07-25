use crate::error::{AppError, Result};
use crate::platform;
use crate::session::{self, Session};
use crate::sysutil;
#[cfg(not(windows))]
use chrono::Utc;
use std::thread::sleep;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const MAX_BATTERY_FAILURES: u8 = 3;

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

pub struct PreparedCharge {
    pub initial_percent: i32,
    pub charging_up: bool,
}

pub enum ChargePreparation {
    AlreadyMet(i32),
    Wait(PreparedCharge),
}

pub fn prepare_charge(target: i32) -> Result<ChargePreparation> {
    let status = platform::read_battery()?;
    let plan = plan_charge(target, &status)?;
    Ok(if plan.already_met {
        ChargePreparation::AlreadyMet(status.percent)
    } else {
        ChargePreparation::Wait(PreparedCharge {
            initial_percent: status.percent,
            charging_up: plan.charging_up,
        })
    })
}

pub fn charge_detail(target: i32, charge: &PreparedCharge) -> String {
    format!(
        "{target}% (was {}%, {})",
        charge.initial_percent,
        if charge.charging_up {
            "charging up"
        } else {
            "discharging down"
        }
    )
}

enum BatteryPoll {
    Continue,
    Reached,
    Failed,
}

fn poll_battery(target: i32, up: bool, failures: &mut u8) -> BatteryPoll {
    match platform::read_battery() {
        Ok(status) => {
            *failures = 0;
            if (up && status.percent >= target) || (!up && status.percent <= target) {
                BatteryPoll::Reached
            } else {
                BatteryPoll::Continue
            }
        }
        Err(error) if battery_failures_exhausted(failures, &error) => BatteryPoll::Failed,
        Err(_) => BatteryPoll::Continue,
    }
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
    #[cfg(target_os = "macos")]
    use crate::commands;
    #[cfg(target_os = "macos")]
    use std::process::Child;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[cfg(target_os = "macos")]
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

    fn supervisor_inhibitor_lifetime() -> (Option<i64>, Option<u32>) {
        (None, Some(std::process::id()))
    }

    pub fn run_charge(args: &[String]) -> Result<()> {
        if args.len() != 4 {
            return Err(AppError::fail("charge supervisor expects 3 arguments"));
        }
        let target = parse_charge_target(&args[1])?;
        let no_display = session::parse_bool(&args[2], "no-display")?;
        let mode = args[3].clone();
        let charge = match prepare_charge(target) {
            Ok(ChargePreparation::Wait(charge)) => charge,
            Ok(ChargePreparation::AlreadyMet(_)) => return Ok(()),
            Err(error) => {
                eprintln!("wake supervisor: {error}");
                return Ok(());
            }
        };
        let (child_timeout, child_wait_pid) = supervisor_inhibitor_lifetime();
        let keep_awake =
            platform::keep_awake_command(no_display, false, child_timeout, child_wait_pid)?;
        let mut child = sysutil::spawn_detached(&keep_awake.cmd)?;
        sysutil::require_child_alive(&mut child, &keep_awake.cmd)?;

        let mut session = Session {
            pid: std::process::id(),
            mode,
            trigger: "until-charge".into(),
            detail: charge_detail(target, &charge),
            started_at: Some(Utc::now()),
            ..Session::default()
        };
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
            if stop.load(Ordering::Relaxed) || child.try_wait()?.is_some() {
                break;
            }
            if last_check.elapsed() >= POLL_INTERVAL {
                last_check = Instant::now();
                if !matches!(
                    poll_battery(target, charge.charging_up, &mut failures),
                    BatteryPoll::Continue
                ) {
                    break;
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = session::delete_state_file();
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub fn run_lid(args: &[String]) -> Result<()> {
        if args.len() != 8 {
            return Err(AppError::fail("lid supervisor expects 7 arguments"));
        }
        let prior_disable_sleep = parse_disable_sleep(&args[4])?;
        let mut cleanup = LidCleanup {
            child: None,
            prior_disable_sleep,
        };
        let no_display = match args[1].as_str() {
            "i" => true,
            "d" => false,
            _ => return Err(AppError::fail("lid supervisor: bad caffeinate mode")),
        };
        let timeout_sec = optional_positive(&args[2], "lid supervisor timeout")?;
        let wait_pid = optional_positive(&args[3], "lid supervisor pid")?;
        let trigger = args[5].clone();
        let detail = args[6].clone();
        let charge_target = if args[7].is_empty() {
            None
        } else {
            Some(parse_charge_target(&args[7])?)
        };
        let charge = match charge_target {
            Some(target) => match prepare_charge(target)? {
                ChargePreparation::Wait(charge) => Some(charge),
                ChargePreparation::AlreadyMet(_) => return Ok(()),
            },
            None => None,
        };

        let (child_timeout, child_wait_pid) = supervisor_inhibitor_lifetime();
        let keep_awake =
            platform::keep_awake_command(no_display, true, child_timeout, child_wait_pid)?;
        let mut child = sysutil::spawn_detached(&keep_awake.cmd)?;
        sysutil::require_child_alive(&mut child, &keep_awake.cmd)?;
        cleanup.child = Some(child);

        let now = Utc::now();
        let mut session = Session {
            pid: std::process::id(),
            mode: if no_display {
                "system-only"
            } else {
                "display+system"
            }
            .into(),
            trigger,
            detail,
            started_at: Some(now),
            ends_at: timeout_sec.map(|timeout| now + chrono::Duration::seconds(timeout)),
            even_lid: true,
            prior_disable_sleep,
            ..Session::default()
        };
        session.capture_process_identity()?;
        session::write(&session)?;

        let stop = install_stop_flag();
        let start = Instant::now();
        let mut next_sudo = start + SUDO_HEARTBEAT;
        let mut last_check = Instant::now();
        let mut failures = 0;
        loop {
            sleep(Duration::from_secs(1));
            let child_exited = match cleanup.child.as_mut() {
                Some(child) => child.try_wait()?.is_some(),
                None => true,
            };
            if stop.load(Ordering::Relaxed) || child_exited {
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
                if let (Some(target), Some(charge)) = (charge_target, &charge)
                    && !matches!(
                        poll_battery(target, charge.charging_up, &mut failures),
                        BatteryPoll::Continue
                    )
                {
                    break;
                }
            }
            if Instant::now() >= next_sudo {
                let _ = platform::refresh_sudo_non_interactive();
                next_sudo = Instant::now() + SUDO_HEARTBEAT;
            }
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    struct LidCleanup {
        child: Option<Child>,
        prior_disable_sleep: i32,
    }

    #[cfg(target_os = "macos")]
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

    #[cfg(target_os = "macos")]
    fn parse_disable_sleep(raw: &str) -> Result<i32> {
        match session::parse_u32(raw, "priorDisableSleep")? {
            value @ (0 | 1) => Ok(value as i32),
            _ => Err(AppError::fail("priorDisableSleep must be 0 or 1")),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn supervisor_owns_native_inhibitor_lifetime() {
            assert_eq!(
                supervisor_inhibitor_lifetime(),
                (None, Some(std::process::id()))
            );
        }
    }
}

#[cfg(not(windows))]
pub use unix::run_charge;
#[cfg(target_os = "macos")]
pub use unix::run_lid;

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::platform::LidSnapshot;
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
        let timeout = optional_positive(&args[2], "timeout")?.map(Duration::from_secs);
        let target_pid = optional_positive(&args[3], "target pid")?;
        let target_start = optional_positive(&args[4], "target creation time")?;
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
        let publish = session::parse_bool(&args[11], "publish")?;
        Ok(WorkerSpec {
            no_display,
            timeout,
            target,
            charge: charge_target.zip(charge_up),
            publish,
            session: Session {
                pid: std::process::id(),
                mode: args[1].clone(),
                trigger: args[7].clone(),
                detail: args[8].clone(),
                started_at: Some(session::parse_utc(&args[9], "start time")?),
                ends_at: if args[10].is_empty() {
                    None
                } else {
                    Some(session::parse_utc(&args[10], "end time")?)
                },
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

    fn run_worker_lifetime(spec: &WorkerSpec) -> Result<()> {
        let start = Instant::now();
        let mut last_battery = Instant::now();
        let mut failures = 0;
        loop {
            if spec
                .timeout
                .is_some_and(|timeout| start.elapsed() >= timeout)
            {
                break;
            }
            if let Some(target) = &spec.target
                && !target.is_running()?
            {
                break;
            }
            if let Some((target, up)) = spec.charge
                && last_battery.elapsed() >= POLL_INTERVAL
            {
                last_battery = Instant::now();
                match poll_battery(target, up, &mut failures) {
                    BatteryPoll::Continue => {}
                    BatteryPoll::Reached => break,
                    BatteryPoll::Failed => {
                        return Err(AppError::fail("battery status remained unavailable"));
                    }
                }
            }
            sleep(Duration::from_millis(250));
        }
        Ok(())
    }

    fn delete_owned_worker_state(worker: &Session) {
        if let Ok(_lock) = session::acquire_lock()
            && let Some(session::SavedState::Valid(saved)) = session::read_saved_for_recovery()
            && saved.owned_non_lid_by(worker.pid, worker.process_start)
        {
            let _ = session::delete_state_file();
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
        let guardian = (std::process::id(), sysutil::current_identity()?.start);
        let snapshot = LidSnapshot {
            scheme: platform::parse_guid(&args.scheme)?,
            ac: args.ac,
            dc: args.dc,
        };
        let worker = sysutil::open_exact_process(args.worker_pid, args.worker_start, false)?;
        wait_for_authorization(&args, guardian)?;
        if let Some(worker) = worker
            && worker.is_running()?
        {
            platform::enable_lid(&snapshot)?;
            while worker.is_running()? {
                sleep(Duration::from_millis(250));
            }
        }
        platform::restore_lid_snapshot(&snapshot)
    }

    fn wait_for_authorization(args: &GuardianArgs, guardian: (u32, u64)) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Some(session::SavedState::Valid(saved)) =
                session::read_saved_at(&args.state_path)
                && saved.matches_lid_authority(
                    (args.worker_pid, args.worker_start),
                    guardian,
                    (&args.scheme, args.ac, args.dc),
                )
            {
                return Ok(());
            }
            sleep(Duration::from_millis(100));
        }
        Err(AppError::fail(
            "guardian authorization state did not appear",
        ))
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
            worker_pid: session::parse_positive(&args[1], "worker pid")?,
            worker_start: session::parse_positive(&args[2], "worker creation time")?,
            scheme,
            ac: session::parse_u32(&args[4], "original AC value")?,
            dc: session::parse_u32(&args[5], "original DC value")?,
            state_path,
        })
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
            let matches = |saved: &Session, guardian| {
                saved.matches_lid_authority(
                    (args.worker_pid, args.worker_start),
                    guardian,
                    (&args.scheme, args.ac, args.dc),
                )
            };
            assert!(matches(&saved, (11, 13)));
            saved.original_dc = 3;
            assert!(!matches(&saved, (11, 13)));
            saved.original_dc = 2;
            assert!(!matches(&saved, (11, 14)));
        }
    }
}

#[cfg(windows)]
pub use windows::{run_guardian, run_worker, worker_command};

#[cfg(any(target_os = "macos", windows))]
fn optional_positive<T>(raw: &str, name: &str) -> Result<Option<T>>
where
    T: std::str::FromStr + Default + PartialEq + ToString,
{
    if raw.is_empty() {
        Ok(None)
    } else {
        session::parse_positive(raw, name).map(Some)
    }
}

fn parse_charge_target(raw: &str) -> Result<i32> {
    match session::parse_positive(raw, "charge target")? {
        target @ 1..=100 => Ok(target),
        _ => Err(AppError::fail(
            "charge target must be an integer from 1 to 100",
        )),
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
        assert!(session::parse_bool("true", "test").unwrap());
        assert!(!session::parse_bool("false", "test").unwrap());
    }
}
