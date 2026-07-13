use crate::error::{AppError, Result, combine_cleanup};
use crate::lid;
use crate::platform;
use crate::run::{ChargeDirection, Trigger};
use crate::session::{self, OwnerIdentity, State};
use crate::sysutil;
use chrono::Utc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

const BATTERY_INTERVAL: Duration = Duration::from_secs(30);
const RUN_INTERVAL: Duration = Duration::from_secs(1);
const STARTUP_SETTLE: Duration = Duration::from_millis(300);

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
    let token = supervisor_token(args)?;
    let owner = OwnerIdentity {
        token: token.clone(),
        process: sysutil::current_process_identity()?,
    };
    supervise(owner)
}

fn supervisor_token(args: &[String]) -> Result<&String> {
    let [token] = args else {
        return Err(AppError::fail("supervisor expects a session token"));
    };
    Ok(token)
}

fn supervise(owner: OwnerIdentity) -> Result<()> {
    let (mut inhibitor, saved, started) = start_session(owner)?;
    let primary = supervise_loop(&saved, &mut inhibitor, started);
    drop(inhibitor);
    if lid::uses_watchdog(&saved.session.spec) {
        return primary.map(|_| ());
    }
    finish_ordinary(primary, cleanup_ordinary)
}

fn start_session(owner: OwnerIdentity) -> Result<(platform::Inhibitor, State, Instant)> {
    let lock = session::acquire_existing_lock_wait()?;
    let starting =
        session::read()?.ok_or_else(|| AppError::fail("supervisor startup was cancelled"))?;
    if starting.session.owner != owner
        || starting.session.started_at.is_some()
        || starting.session.stop_requested
        || starting.lid.is_some()
    {
        return Err(AppError::fail("starting supervisor state changed"));
    }

    let spec = &starting.session.spec;
    let mut inhibitor = match platform::Inhibitor::start(spec.mode, spec.even_lid) {
        Ok(inhibitor) => inhibitor,
        Err(error) => {
            session::remove_exact(&lock, &starting)?;
            return Err(error);
        }
    };
    sleep(STARTUP_SETTLE);
    if !inhibitor.alive() {
        let error = platform::inhibitor_startup_error(spec.even_lid);
        session::remove_exact(&lock, &starting)?;
        return Err(error);
    }
    let started = Instant::now();
    let started_at = Utc::now();
    let saved = match session::update(&lock, &owner, |state| {
        state.session.started_at = Some(started_at);
        state.session.note = inhibitor.note().map(str::to_owned);
        Ok(())
    }) {
        Ok(saved) => saved,
        Err(error) => {
            let cleanup = session::remove_exact(&lock, &starting).map(|_| ());
            return combine_cleanup(Err(error), cleanup);
        }
    };
    drop(lock);
    Ok((inhibitor, saved, started))
}

fn supervise_loop(
    saved: &State,
    inhibitor: &mut platform::Inhibitor,
    started: Instant,
) -> Result<State> {
    let signal = install_stop_flag();
    let mut next_battery = Instant::now();
    loop {
        let current =
            session::read()?.ok_or_else(|| AppError::fail("active session state disappeared"))?;
        if !session_matches_saved(saved, &current) {
            return Err(AppError::fail("active session state changed"));
        }
        if current.session.stop_requested || signal.load(Ordering::Relaxed) {
            return Ok(current);
        }
        if !inhibitor.alive() {
            return Err(AppError::fail("sleep inhibitor exited unexpectedly"));
        }
        if current
            .session
            .spec
            .trigger
            .is_complete(Utc::now(), started.elapsed())
        {
            return Ok(current);
        }
        if let Some(process) = current.session.spec.trigger.process()
            && !sysutil::process_identity_matches(process)?
        {
            return Ok(current);
        }
        if Instant::now() >= next_battery {
            next_battery = Instant::now() + BATTERY_INTERVAL;
            if let Trigger::Charge {
                target, direction, ..
            } = current.session.spec.trigger
                && charge_reached(target, direction)?
            {
                return Ok(current);
            }
        }
        sleep(RUN_INTERVAL);
    }
}

fn session_matches_saved(saved: &State, current: &State) -> bool {
    if saved.session.stop_requested && !current.session.stop_requested {
        return false;
    }
    let mut session = current.session.clone();
    session.stop_requested = saved.session.stop_requested;
    session == saved.session
}

fn finish_ordinary(
    primary: Result<State>,
    cleanup: impl FnOnce(&State) -> Result<()>,
) -> Result<()> {
    match primary {
        Ok(completed) => cleanup(&completed),
        Err(error) => Err(error),
    }
}

fn cleanup_ordinary(completed: &State) -> Result<()> {
    let lock = session::acquire_existing_lock_wait()?;
    require_exact_removal(session::remove_exact(&lock, completed)?)
}

fn require_exact_removal(removed: bool) -> Result<()> {
    if removed {
        Ok(())
    } else {
        Err(AppError::fail(
            "active session state changed before cleanup",
        ))
    }
}

fn charge_reached(target: i32, direction: ChargeDirection) -> Result<bool> {
    let status = platform::read_battery()?;
    Ok(match direction {
        ChargeDirection::Up => status.percent >= target,
        ChargeDirection::Down => status.percent <= target,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{Mode, ProcessIdentity, RunSpec};
    use crate::session::{STATE_SCHEMA, SessionState};
    use std::cell::Cell;

    fn ordinary_state() -> State {
        State {
            schema: STATE_SCHEMA,
            session: SessionState {
                owner: OwnerIdentity {
                    token: "0123456789abcdef0123456789abcdef".into(),
                    process: ProcessIdentity {
                        pid: 10,
                        native_start: 11,
                    },
                },
                spec: RunSpec {
                    mode: Mode::DisplaySystem,
                    trigger: Trigger::Indefinite,
                    even_lid: false,
                },
                started_at: Some("2024-01-02T03:04:05Z".parse().unwrap()),
                note: None,
                stop_requested: false,
            },
            lid: None,
        }
    }

    #[test]
    fn supervisor_accepts_only_one_session_token() {
        let token = "0123456789abcdef0123456789abcdef".to_string();
        assert_eq!(
            supervisor_token(std::slice::from_ref(&token)).unwrap(),
            &token
        );
        assert!(supervisor_token(&[]).is_err());
        assert!(supervisor_token(&[token.clone(), "extra".into()]).is_err());
    }

    #[test]
    fn active_session_allows_only_the_monotonic_stop_transition() {
        let saved = ordinary_state();
        assert!(session_matches_saved(&saved, &saved));

        let mut stopped = saved.clone();
        stopped.session.stop_requested = true;
        assert!(session_matches_saved(&saved, &stopped));

        let mut drifted = stopped;
        drifted.session.note = Some("changed".into());
        assert!(!session_matches_saved(&saved, &drifted));
    }

    #[test]
    fn ordinary_primary_error_retains_state() {
        let cleaned = Cell::new(false);
        let result = finish_ordinary(Err(AppError::fail("monitor failed")), |_| {
            cleaned.set(true);
            Ok(())
        });

        assert!(result.is_err());
        assert!(!cleaned.get());
    }

    #[test]
    fn ordinary_clean_completion_removes_only_the_returned_state() {
        let completed = ordinary_state();
        let removed = Cell::new(false);
        finish_ordinary(Ok(completed.clone()), |expected| {
            assert_eq!(expected, &completed);
            removed.set(true);
            Ok(())
        })
        .unwrap();

        assert!(removed.get());
    }

    #[test]
    fn ordinary_cleanup_reports_an_exact_removal_race() {
        let error = require_exact_removal(false).unwrap_err();

        assert_eq!(
            error.message(),
            "active session state changed before cleanup"
        );
    }
}
