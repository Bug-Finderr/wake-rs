use crate::error::{AppError, Result};
use crate::platform;
use crate::session::{self, Session};
use crate::sysutil;
use chrono::Utc;
use std::thread::sleep;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const MAX_BATTERY_FAILURES: u8 = 3;

fn poll_interval(
    deadline: Option<chrono::DateTime<Utc>>,
    now: chrono::DateTime<Utc>,
    maximum: Duration,
) -> Option<Duration> {
    match deadline {
        Some(deadline) => (deadline - now)
            .to_std()
            .ok()
            .filter(|remaining| !remaining.is_zero())
            .map(|remaining| remaining.min(maximum)),
        None => Some(maximum),
    }
}

pub struct BatteryStatus {
    pub percent: i32,
    pub charging: bool,
    pub discharging: bool,
    pub neutral_state: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChargePlan {
    AlreadyMet,
    Wait(bool),
}

pub fn plan_charge(target: i32, status: &BatteryStatus) -> Result<ChargePlan> {
    if status.discharging {
        if status.percent == target {
            return Ok(ChargePlan::AlreadyMet);
        }
        if status.percent < target {
            return Err(AppError::usage(format!(
                "--until-charge {target} is unreachable while battery is discharging at {}%; connect power or choose a target at or below the current charge",
                status.percent
            )));
        }
        return Ok(ChargePlan::Wait(false));
    }
    if status.charging {
        return Ok(if status.percent >= target {
            ChargePlan::AlreadyMet
        } else {
            ChargePlan::Wait(true)
        });
    }
    if status.percent == target {
        return Ok(ChargePlan::AlreadyMet);
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
    Ok(match plan_charge(target, &status)? {
        ChargePlan::AlreadyMet => ChargePreparation::AlreadyMet(status.percent),
        ChargePlan::Wait(charging_up) => ChargePreparation::Wait(PreparedCharge {
            initial_percent: status.percent,
            charging_up,
        }),
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

    #[cfg(any(target_os = "macos", test))]
    fn reject_macos_ordinary_even_lid(even_lid: bool) -> Result<()> {
        if even_lid {
            Err(AppError::fail(
                "ordinary supervisor received unexpected lid authority",
            ))
        } else {
            Ok(())
        }
    }

    struct UntilArgs {
        deadline: chrono::DateTime<Utc>,
        no_display: bool,
        detail: String,
        even_lid: bool,
        inhibitor_scope: String,
    }

    fn parse_until_args(args: &[String]) -> Result<UntilArgs> {
        if args.len() != 6 {
            return Err(AppError::fail("until supervisor expects 5 arguments"));
        }
        let deadline = session::parse_utc(&args[1], "until deadline")?;
        let no_display = session::parse_bool(&args[2], "no-display")?;
        let detail = args[3].clone();
        let even_lid = session::parse_bool(&args[4], "even-lid")?;
        #[cfg(target_os = "macos")]
        reject_macos_ordinary_even_lid(even_lid)?;
        Ok(UntilArgs {
            deadline,
            no_display,
            detail,
            even_lid,
            inhibitor_scope: args[5].clone(),
        })
    }

    pub fn run_until(args: &[String]) -> Result<()> {
        let args = parse_until_args(args)?;
        if !commands::deadline_still_pending(args.deadline, Utc::now()) {
            return Ok(());
        }
        let (child_timeout, child_wait_pid) = supervisor_inhibitor_lifetime();
        #[cfg(target_os = "linux")]
        let keep_awake = platform::keep_awake_command_for_scope(
            args.no_display,
            args.even_lid,
            &args.inhibitor_scope,
            child_timeout,
            child_wait_pid,
        )?;
        #[cfg(target_os = "macos")]
        let keep_awake = {
            if args.even_lid || !args.inhibitor_scope.is_empty() {
                return Err(AppError::fail(
                    "until supervisor received unexpected lid authority",
                ));
            }
            platform::keep_awake_command(args.no_display, child_timeout, child_wait_pid)?
        };
        let mut child = sysutil::spawn_detached(&keep_awake.cmd)?;
        sysutil::require_child_alive(&mut child, &keep_awake.cmd)?;
        let now = Utc::now();
        if !commands::deadline_still_pending(args.deadline, now) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
        let mut published = Session {
            pid: std::process::id(),
            mode: session::mode_for(args.no_display).into(),
            trigger: "until-time".into(),
            detail: args.detail,
            started_at: Some(now),
            ends_at: Some(args.deadline),
            even_lid: args.even_lid,
            ..Session::default()
        };
        if let Err(error) = published
            .capture_process_identity()
            .and_then(|()| session::write(&published))
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        if !commands::deadline_still_pending(args.deadline, Utc::now()) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = session::delete_state_file();
            return Ok(());
        }

        let stop = install_stop_flag();
        while !stop.load(Ordering::Relaxed) && child.try_wait()?.is_none() {
            let Some(interval) =
                poll_interval(Some(args.deadline), Utc::now(), Duration::from_secs(1))
            else {
                break;
            };
            sleep(interval);
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = session::delete_state_file();
        Ok(())
    }

    struct ChargeArgs {
        target: i32,
        no_display: bool,
        even_lid: bool,
        inhibitor_scope: String,
    }

    fn parse_charge_args(args: &[String]) -> Result<ChargeArgs> {
        if args.len() != 5 {
            return Err(AppError::fail("charge supervisor expects 4 arguments"));
        }
        let target = parse_charge_target(&args[1])?;
        let no_display = session::parse_bool(&args[2], "no-display")?;
        let even_lid = session::parse_bool(&args[3], "even-lid")?;
        #[cfg(target_os = "macos")]
        reject_macos_ordinary_even_lid(even_lid)?;
        Ok(ChargeArgs {
            target,
            no_display,
            even_lid,
            inhibitor_scope: args[4].clone(),
        })
    }

    pub fn run_charge(args: &[String]) -> Result<()> {
        let args = parse_charge_args(args)?;
        let charge = match prepare_charge(args.target) {
            Ok(ChargePreparation::Wait(charge)) => charge,
            Ok(ChargePreparation::AlreadyMet(_)) => return Ok(()),
            Err(error) => {
                eprintln!("wake supervisor: {error}");
                return Ok(());
            }
        };
        let (child_timeout, child_wait_pid) = supervisor_inhibitor_lifetime();
        #[cfg(target_os = "linux")]
        let keep_awake = platform::keep_awake_command_for_scope(
            args.no_display,
            args.even_lid,
            &args.inhibitor_scope,
            child_timeout,
            child_wait_pid,
        )?;
        #[cfg(target_os = "macos")]
        let keep_awake = {
            if !args.inhibitor_scope.is_empty() {
                return Err(AppError::fail(
                    "charge supervisor received an unexpected inhibitor scope",
                ));
            }
            platform::keep_awake_command(args.no_display, child_timeout, child_wait_pid)?
        };
        let mut child = sysutil::spawn_detached(&keep_awake.cmd)?;
        sysutil::require_child_alive(&mut child, &keep_awake.cmd)?;

        let mut session = Session {
            pid: std::process::id(),
            mode: session::mode_for(args.no_display).into(),
            trigger: "until-charge".into(),
            detail: charge_detail(args.target, &charge),
            started_at: Some(Utc::now()),
            even_lid: args.even_lid,
            ..Session::default()
        };
        if let Err(error) = session
            .capture_process_identity()
            .and_then(|()| session::write(&session))
        {
            let _ = child.kill();
            let _ = child.wait();
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
                    poll_battery(args.target, charge.charging_up, &mut failures),
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
        if args.len() != 9 {
            return Err(AppError::fail("lid supervisor expects 8 arguments"));
        }
        let prior_disable_sleep = session::parse_disable_sleep(&args[5])?;
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
        let until_deadline = if args[3].is_empty() {
            None
        } else {
            Some(session::parse_utc(&args[3], "lid supervisor deadline")?)
        };
        if timeout_sec.is_some() && until_deadline.is_some() {
            return Err(AppError::fail(
                "lid supervisor received both a relative timeout and an absolute deadline",
            ));
        }
        let wait_pid = optional_positive(&args[4], "lid supervisor pid")?;
        let trigger = args[6].clone();
        let detail = args[7].clone();
        let charge_target = if args[8].is_empty() {
            None
        } else {
            Some(parse_charge_target(&args[8])?)
        };
        let charge = match charge_target {
            Some(target) => match prepare_charge(target)? {
                ChargePreparation::Wait(charge) => Some(charge),
                ChargePreparation::AlreadyMet(_) => return Ok(()),
            },
            None => None,
        };

        if until_deadline.is_some_and(|deadline| Utc::now() >= deadline) {
            return Ok(());
        }
        let (child_timeout, child_wait_pid) = supervisor_inhibitor_lifetime();
        let keep_awake = platform::keep_awake_command(no_display, child_timeout, child_wait_pid)?;
        let mut child = sysutil::spawn_detached(&keep_awake.cmd)?;
        sysutil::require_child_alive(&mut child, &keep_awake.cmd)?;
        cleanup.child = Some(child);
        let now = Utc::now();
        if until_deadline.is_some_and(|deadline| now >= deadline) {
            return Ok(());
        }
        let mut session = Session {
            pid: std::process::id(),
            mode: session::mode_for(no_display).into(),
            trigger,
            detail,
            started_at: Some(now),
            ends_at: until_deadline
                .or_else(|| timeout_sec.map(|timeout| now + chrono::Duration::seconds(timeout))),
            even_lid: true,
            prior_disable_sleep,
            ..Session::default()
        };
        session.capture_process_identity()?;
        session::write(&session)?;
        if until_deadline.is_some_and(|deadline| Utc::now() >= deadline) {
            return Ok(());
        }

        let stop = install_stop_flag();
        let start = Instant::now();
        let mut next_sudo = start + SUDO_HEARTBEAT;
        let mut last_check = Instant::now();
        let mut failures = 0;
        while let Some(interval) = poll_interval(until_deadline, Utc::now(), Duration::from_secs(1))
        {
            sleep(interval);
            let child_exited = match cleanup.child.as_mut() {
                Some(child) => child.try_wait()?.is_some(),
                None => true,
            };
            if stop.load(Ordering::Relaxed) || child_exited {
                break;
            }
            if until_deadline.is_some_and(|deadline| Utc::now() >= deadline)
                || timeout_sec
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
            let restored = if prior != 0 {
                true
            } else {
                if platform::read_disable_sleep()
                    .is_ok_and(|current| commands::sleep_restore_needed(prior, current))
                {
                    let _ = platform::set_disable_sleep_non_interactive(prior);
                }
                platform::read_disable_sleep()
                    .is_ok_and(|current| commands::sleep_restored(prior, current))
            };
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

    #[cfg(test)]
    mod tests {
        use super::*;

        fn strings(values: &[&str]) -> Vec<String> {
            values.iter().map(|value| (*value).into()).collect()
        }

        #[test]
        fn supervisor_owns_native_inhibitor_lifetime() {
            assert_eq!(
                supervisor_inhibitor_lifetime(),
                (None, Some(std::process::id()))
            );
        }

        #[test]
        fn charge_supervisor_protocol_uses_exact_arity_without_mode() {
            let parsed = parse_charge_args(&strings(&[
                "__supervise_charge__",
                "80",
                "true",
                "false",
                "",
            ]))
            .unwrap();
            assert_eq!(parsed.target, 80);
            assert!(parsed.no_display);
            assert!(!parsed.even_lid);
            assert!(parsed.inhibitor_scope.is_empty());
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn linux_lid_inclusive_scope_does_not_imply_explicit_even_lid() {
            let charge = parse_charge_args(&strings(&[
                "__supervise_charge__",
                "80",
                "false",
                "false",
                "sleep:handle-lid-switch",
            ]))
            .unwrap();
            assert!(!charge.even_lid);
            assert_eq!(charge.inhibitor_scope, "sleep:handle-lid-switch");

            let until = parse_until_args(&strings(&[
                "__supervise_until__",
                "2024-01-02T03:04:05+00:00",
                "false",
                "until 03:04",
                "false",
                "sleep:handle-lid-switch",
            ]))
            .unwrap();
            assert!(!until.even_lid);
            assert_eq!(until.inhibitor_scope, "sleep:handle-lid-switch");
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn linux_protocol_preserves_explicit_even_lid() {
            let charge = parse_charge_args(&strings(&[
                "__supervise_charge__",
                "80",
                "true",
                "true",
                "sleep:handle-lid-switch",
            ]))
            .unwrap();
            assert!(charge.even_lid);

            let until = parse_until_args(&strings(&[
                "__supervise_until__",
                "2024-01-02T03:04:05+00:00",
                "true",
                "until 03:04",
                "true",
                "sleep:handle-lid-switch",
            ]))
            .unwrap();
            assert!(until.even_lid);
        }

        #[test]
        fn macos_ordinary_authority_policy_rejects_even_lid() {
            assert!(reject_macos_ordinary_even_lid(true).is_err());
            assert!(reject_macos_ordinary_even_lid(false).is_ok());
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn macos_ordinary_hidden_protocols_reject_even_lid() {
            assert!(
                parse_charge_args(&strings(&[
                    "__supervise_charge__",
                    "80",
                    "false",
                    "true",
                    "",
                ]))
                .is_err()
            );
            assert!(
                parse_until_args(&strings(&[
                    "__supervise_until__",
                    "2024-01-02T03:04:05+00:00",
                    "false",
                    "until 03:04",
                    "true",
                    "",
                ]))
                .is_err()
            );
        }

        #[test]
        fn until_supervisor_protocol_requires_a_canonical_deadline() {
            let parsed = parse_until_args(&strings(&[
                "__supervise_until__",
                "2024-01-02T03:04:05+00:00",
                "false",
                "until 03:04",
                "false",
                "idle:sleep",
            ]))
            .unwrap();
            assert_eq!(parsed.deadline.to_rfc3339(), "2024-01-02T03:04:05+00:00");
            assert!(!parsed.no_display);
            assert_eq!(parsed.detail, "until 03:04");
            assert!(!parsed.even_lid);
            assert_eq!(parsed.inhibitor_scope, "idle:sleep");

            let malformed = strings(&[
                "__supervise_until__",
                "not-a-deadline",
                "false",
                "until 03:04",
                "false",
                "idle:sleep",
            ]);
            assert!(parse_until_args(&malformed).is_err());

            let missing = strings(&[
                "__supervise_until__",
                "2024-01-02T03:04:05+00:00",
                "false",
                "until 03:04",
                "false",
            ]);
            assert!(parse_until_args(&missing).is_err());

            let extra = strings(&[
                "__supervise_until__",
                "2024-01-02T03:04:05+00:00",
                "false",
                "until 03:04",
                "false",
                "idle:sleep",
                "extra",
            ]);
            assert!(parse_until_args(&extra).is_err());
        }

        #[test]
        fn charge_supervisor_protocol_rejects_wrong_arity_or_malformed_even_lid() {
            let missing = strings(&["__supervise_charge__", "80", "false", "false"]);
            assert!(parse_charge_args(&missing).is_err());

            let extra = strings(&[
                "__supervise_charge__",
                "80",
                "false",
                "false",
                "idle:sleep",
                "extra",
            ]);
            assert!(parse_charge_args(&extra).is_err());

            let malformed = strings(&["__supervise_charge__", "80", "false", "True", "idle:sleep"]);
            assert!(parse_charge_args(&malformed).is_err());
        }
    }
}

#[cfg(target_os = "macos")]
pub use unix::run_lid;
#[cfg(not(windows))]
pub use unix::{run_charge, run_until};

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
        session: Session,
    }

    impl WorkerSpec {
        fn lifetime_elapsed(&self, started: Instant) -> bool {
            if self.session.trigger == "until-time" {
                self.session
                    .ends_at
                    .is_some_and(|deadline| Utc::now() >= deadline)
            } else {
                self.timeout
                    .is_some_and(|timeout| started.elapsed() >= timeout)
            }
        }
    }

    pub fn worker_command(
        session: &Session,
        timeout_sec: Option<i64>,
        target: Option<(u32, u64)>,
        charge: Option<(i32, bool)>,
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
        ])
    }

    pub fn run_worker(args: &[String]) -> Result<()> {
        let spec = parse_worker_args(args)?;
        let _execution_state = platform::ExecutionStateGuard::acquire(spec.no_display)?;
        if spec.lifetime_elapsed(Instant::now()) {
            return Ok(());
        }
        session::write(&spec.session)?;
        run_worker_lifetime(&spec)?;
        delete_owned_worker_state(&spec.session);
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
        Ok(WorkerSpec {
            no_display,
            timeout,
            target,
            charge: charge_target.zip(charge_up),
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
                // Only guardian promotion can grant even-lid authority.
                even_lid: false,
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
            if spec.lifetime_elapsed(start) {
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
            let deadline = if spec.session.trigger == "until-time" {
                spec.session.ends_at
            } else {
                None
            };
            let Some(interval) = poll_interval(deadline, Utc::now(), Duration::from_millis(250))
            else {
                break;
            };
            sleep(interval);
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
        restore_without_worker: bool,
        deadline: Option<chrono::DateTime<Utc>>,
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
        if args.restore_without_worker {
            wait_for_recovery_authorization(&args, guardian)?;
            return platform::restore_lid_snapshot(&snapshot);
        }
        let Some(worker) = sysutil::open_exact_process(args.worker_pid, args.worker_start, false)?
        else {
            return Ok(());
        };
        let authority = startup_guardian_authority(&args, guardian, &worker)?;
        if !startup_guardian_should_enable(args.deadline, Utc::now(), worker.is_running()?) {
            return Ok(());
        }
        platform::preflight_lid_enable(&snapshot)?;
        if !startup_guardian_should_enable(args.deadline, Utc::now(), worker.is_running()?) {
            return Ok(());
        }
        session::write_at(&args.state_path, &authority)?;
        platform::enable_lid(&snapshot)?;
        while worker.is_running()? {
            sleep(Duration::from_millis(250));
        }
        platform::restore_lid_snapshot(&snapshot)
    }

    fn startup_guardian_authority(
        args: &GuardianArgs,
        guardian: (u32, u64),
        worker: &sysutil::ProcessHandle,
    ) -> Result<Session> {
        let Some(session::SavedState::Valid(saved)) = session::read_saved_at(&args.state_path)
        else {
            return Err(AppError::fail(
                "startup guardian did not find provisional worker state",
            ));
        };
        promote_startup_guardian_authority(saved, args, guardian, worker.identity())
    }

    fn promote_startup_guardian_authority(
        mut saved: Session,
        args: &GuardianArgs,
        guardian: (u32, u64),
        worker_identity: &sysutil::Identity,
    ) -> Result<Session> {
        let expected_deadline = if saved.trigger == "until-time" {
            saved.ends_at
        } else {
            None
        };
        if !saved.owned_non_lid_by(args.worker_pid, args.worker_start)
            || !saved.matches_identity(worker_identity)
            || args.deadline != expected_deadline
        {
            return Err(AppError::fail(
                "startup guardian state does not match its immutable arguments",
            ));
        }
        saved.even_lid = true;
        saved.guardian_pid = guardian.0;
        saved.guardian_start = guardian.1;
        saved.original_scheme.clone_from(&args.scheme);
        saved.original_ac = args.ac;
        saved.original_dc = args.dc;
        Ok(saved)
    }

    fn startup_guardian_should_enable(
        deadline: Option<chrono::DateTime<Utc>>,
        now: chrono::DateTime<Utc>,
        worker_running: bool,
    ) -> bool {
        worker_running && deadline.is_none_or(|deadline| now < deadline)
    }

    fn wait_for_recovery_authorization(args: &GuardianArgs, guardian: (u32, u64)) -> Result<()> {
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
        if args.len() != 9 {
            return Err(AppError::fail(
                "Windows guardian expects eight immutable arguments",
            ));
        }
        let scheme = args[3].clone();
        platform::parse_guid(&scheme)?;
        let restore_without_worker = session::parse_bool(&args[6], "restore-without-worker")?;
        let deadline = if args[7].is_empty() {
            None
        } else {
            Some(session::parse_utc(&args[7], "guardian deadline")?)
        };
        if restore_without_worker && deadline.is_some() {
            return Err(AppError::fail(
                "recovery guardian received an unexpected deadline",
            ));
        }
        let state_path = PathBuf::from(&args[8]);
        if !state_path.is_absolute() {
            return Err(AppError::fail("guardian state path must be absolute"));
        }
        Ok(GuardianArgs {
            worker_pid: session::parse_positive(&args[1], "worker pid")?,
            worker_start: session::parse_positive(&args[2], "worker creation time")?,
            scheme,
            ac: session::parse_u32(&args[4], "original AC value")?,
            dc: session::parse_u32(&args[5], "original DC value")?,
            restore_without_worker,
            deadline,
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
        fn worker_argument_parser_is_strict_without_publish() {
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
            ]);
            assert!(!parse_worker_args(&valid).unwrap().session.even_lid);
            assert!(parse_worker_args(&valid[..10]).is_err());
            let mut extra = valid.clone();
            extra.push("true".into());
            assert!(parse_worker_args(&extra).is_err());
            let mut incomplete = valid;
            incomplete[3] = "42".into();
            assert!(parse_worker_args(&incomplete).is_err());
        }

        #[test]
        fn worker_command_omits_publish_and_parses_as_provisional_non_lid_state() {
            let session = Session {
                mode: "system-only".into(),
                trigger: "until-charge".into(),
                detail: "80%".into(),
                started_at: Some(
                    session::parse_utc("2024-01-02T03:04:05+00:00", "start time").unwrap(),
                ),
                ..Session::default()
            };
            let command = worker_command(&session, Some(60), None, Some((80, true))).unwrap();
            assert_eq!(command.len(), 12);
            assert_eq!(
                &command[1..],
                [
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
                ]
            );
            assert!(!parse_worker_args(&command[1..]).unwrap().session.even_lid);
        }

        #[test]
        fn until_worker_uses_the_absolute_end_without_changing_relative_timeouts() {
            let mut spec = WorkerSpec {
                no_display: false,
                timeout: Some(Duration::from_secs(3_600)),
                target: None,
                charge: None,
                session: Session {
                    trigger: "until-time".into(),
                    ends_at: Some(Utc::now() - chrono::Duration::seconds(1)),
                    ..Session::default()
                },
            };
            assert!(spec.lifetime_elapsed(Instant::now()));
            spec.session.trigger = "timed".into();
            assert!(!spec.lifetime_elapsed(Instant::now()));
        }

        #[test]
        fn guardian_protocol_requires_explicit_dead_worker_authority() {
            for restore_without_worker in ["false", "true"] {
                let args = strings(&[
                    "__guard_windows__",
                    "7",
                    "9",
                    "381b4222-f694-41f0-9685-ff5bb260df2e",
                    "1",
                    "2",
                    restore_without_worker,
                    "",
                    "C:\\state\\session.properties",
                ]);
                let parsed = parse_guardian_args(&args).unwrap();
                assert_eq!(
                    parsed.restore_without_worker,
                    restore_without_worker == "true"
                );
                assert!(parsed.deadline.is_none());
            }
        }

        #[test]
        fn startup_guardian_checks_the_deadline_at_the_transition() {
            let now = chrono::DateTime::parse_from_rfc3339("2024-01-02T03:04:05+00:00")
                .unwrap()
                .with_timezone(&Utc);
            assert!(startup_guardian_should_enable(
                Some(now + chrono::Duration::seconds(1)),
                now,
                true,
            ));
            assert!(!startup_guardian_should_enable(Some(now), now, true));
            assert!(!startup_guardian_should_enable(None, now, false));
        }

        #[test]
        fn startup_guardian_promotes_only_matching_provisional_state() {
            let deadline = chrono::DateTime::parse_from_rfc3339("2024-01-02T03:04:05+00:00")
                .unwrap()
                .with_timezone(&Utc);
            let identity = sysutil::Identity {
                start: 9,
                command: "C:\\tools\\wake.exe".into(),
            };
            let args = GuardianArgs {
                worker_pid: 7,
                worker_start: 9,
                scheme: "381b4222-f694-41f0-9685-ff5bb260df2e".into(),
                ac: 1,
                dc: 2,
                restore_without_worker: false,
                deadline: Some(deadline),
                state_path: PathBuf::from("C:\\state\\session.properties"),
            };
            let saved = Session {
                pid: 7,
                mode: "display+system".into(),
                trigger: "until-time".into(),
                detail: "until 03:04".into(),
                started_at: Some(deadline - chrono::Duration::minutes(1)),
                ends_at: Some(deadline),
                process_start: 9,
                process_command: identity.command.clone(),
                ..Session::default()
            };
            let authority =
                promote_startup_guardian_authority(saved, &args, (11, 13), &identity).unwrap();
            assert!(authority.even_lid);
            assert_eq!((authority.guardian_pid, authority.guardian_start), (11, 13));
            assert_eq!(
                (
                    authority.original_scheme.as_str(),
                    authority.original_ac,
                    authority.original_dc,
                ),
                (args.scheme.as_str(), 1, 2)
            );
        }

        #[test]
        fn immutable_args_must_match_the_complete_session() {
            let args = GuardianArgs {
                worker_pid: 7,
                worker_start: 9,
                scheme: "381b4222-f694-41f0-9685-ff5bb260df2e".into(),
                ac: 1,
                dc: 2,
                restore_without_worker: false,
                deadline: None,
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
            (status(80, false, true, None), ChargePlan::AlreadyMet),
            (status(90, false, true, None), ChargePlan::Wait(false)),
            (status(80, true, false, None), ChargePlan::AlreadyMet),
            (status(60, true, false, None), ChargePlan::Wait(true)),
            (status(80, false, false, None), ChargePlan::AlreadyMet),
        ] {
            assert_eq!(plan_charge(80, &battery).unwrap(), expected);
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
    fn absolute_poll_interval_never_sleeps_past_the_deadline() {
        let now = chrono::DateTime::parse_from_rfc3339("2024-01-02T03:04:05+00:00")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            poll_interval(
                Some(now + chrono::Duration::milliseconds(100)),
                now,
                Duration::from_secs(1),
            ),
            Some(Duration::from_millis(100))
        );
        assert_eq!(poll_interval(Some(now), now, Duration::from_secs(1)), None);
        assert_eq!(
            poll_interval(None, now, Duration::from_millis(250)),
            Some(Duration::from_millis(250))
        );
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
