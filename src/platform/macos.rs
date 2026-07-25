use super::KeepAwake;
use crate::error::{AppError, Result};
use crate::supervisor::BatteryStatus;
use std::process::{Command, Stdio};

const CAFFEINATE: &str = "/usr/bin/caffeinate";
const PMSET: &str = "/usr/bin/pmset";
const SUDO: &str = "/usr/bin/sudo";
const LID_CLOSE_NOTE: &str = "note: closing the lid still sleeps the mac unless you use --even-lid";
const EXPECTED: &[&str] = &["caffeinate", "wake"];

pub fn expected_command_basenames() -> &'static [&'static str] {
    EXPECTED
}

pub fn supports_even_lid() -> bool {
    true
}

pub fn static_start_note() -> Option<String> {
    Some(LID_CLOSE_NOTE.to_string())
}

pub fn keep_awake_command(
    no_display: bool,
    _even_lid: bool,
    timeout_sec: Option<i64>,
    wait_pid: Option<u32>,
) -> Result<KeepAwake> {
    let mut cmd = vec![
        CAFFEINATE.to_string(),
        if no_display { "-i" } else { "-di" }.into(),
    ];
    if let Some(t) = timeout_sec {
        cmd.push("-t".into());
        cmd.push(t.to_string());
    }
    if let Some(p) = wait_pid {
        cmd.push("-w".into());
        cmd.push(p.to_string());
    }
    Ok(KeepAwake {
        cmd,
        note: Some(LID_CLOSE_NOTE.to_string()),
    })
}

pub fn read_battery() -> Result<BatteryStatus> {
    parse_pmset_batt(&capture(PMSET, &["-g", "batt"])?)
}

fn parse_pmset_batt(out: &str) -> Result<BatteryStatus> {
    let record = out
        .lines()
        .find(|line| line.contains("InternalBattery"))
        .ok_or_else(|| AppError::fail("cannot find InternalBattery in pmset output"))?;
    let mut fields = record.split(';');
    let description = fields.next().unwrap_or_default();
    let percent = description
        .split_whitespace()
        .find_map(|part| part.strip_suffix('%'))
        .and_then(|part| part.parse::<i32>().ok())
        .filter(|percent| (0..=100).contains(percent))
        .ok_or_else(|| AppError::fail("cannot parse InternalBattery percentage from pmset"))?;
    let state = fields
        .next()
        .map(str::trim)
        .filter(|state| !state.is_empty())
        .ok_or_else(|| AppError::fail("cannot parse InternalBattery status from pmset"))?
        .to_ascii_lowercase();
    let (charging, discharging, neutral_state) = match state.as_str() {
        "charging" | "finishing charge" => (true, false, None),
        "discharging" => (false, true, None),
        _ => (false, false, Some(state)),
    };
    Ok(BatteryStatus {
        percent,
        charging,
        discharging,
        neutral_state,
    })
}

pub fn read_disable_sleep() -> Result<i32> {
    parse_pmset_disable_sleep(&capture(PMSET, &["-g"])?)
}

fn parse_pmset_disable_sleep(out: &str) -> Result<i32> {
    for line in out.lines() {
        let mut parts = line.split_whitespace();
        if parts
            .next()
            .is_some_and(|first| first.eq_ignore_ascii_case("SleepDisabled"))
        {
            return match (parts.next(), parts.next()) {
                (Some(raw), None) => parse_disable_sleep_value(raw),
                _ => Err(AppError::fail(
                    "cannot parse SleepDisabled value from pmset",
                )),
            };
        }
    }
    Err(AppError::fail("SleepDisabled is missing from pmset output"))
}

pub fn authenticate_sudo() -> Result<bool> {
    Ok(run_foreground(&[SUDO, "-v"])? == 0)
}

pub fn set_disable_sleep_foreground(value: i32) -> Result<()> {
    let v = disable_sleep_value(value)?;
    if run_foreground(&[SUDO, PMSET, "-a", "disablesleep", &v])? != 0 {
        return Err(AppError::fail("sudo pmset -a disablesleep failed"));
    }
    Ok(())
}

pub fn set_disable_sleep_non_interactive(value: i32) -> Result<bool> {
    let v = disable_sleep_value(value)?;
    Ok(run_quiet(&[SUDO, "-n", PMSET, "-a", "disablesleep", &v])? == 0)
}

pub fn refresh_sudo_non_interactive() -> Result<bool> {
    Ok(run_quiet(&[SUDO, "-n", "-v"])? == 0)
}

fn disable_sleep_value(value: i32) -> Result<String> {
    match value {
        0 | 1 => Ok(value.to_string()),
        _ => Err(AppError::fail("disablesleep value must be 0 or 1")),
    }
}

fn parse_disable_sleep_value(raw: &str) -> Result<i32> {
    match raw.parse::<i32>() {
        Ok(v @ (0 | 1)) => Ok(v),
        _ => Err(AppError::fail(
            "cannot parse SleepDisabled value from pmset",
        )),
    }
}

fn capture(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .stderr(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(AppError::fail(format!(
            "{program} exited with status {}",
            out.status.code().unwrap_or(-1)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn run_foreground(cmd: &[&str]) -> Result<i32> {
    let status = Command::new(cmd[0]).args(&cmd[1..]).status()?;
    Ok(status.code().unwrap_or(-1))
}

fn run_quiet(cmd: &[&str]) -> Result<i32> {
    let status = Command::new(cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(status.code().unwrap_or(-1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caffeinate_assertions_match_display_mode() {
        assert_eq!(
            keep_awake_command(false, false, None, None).unwrap().cmd,
            [CAFFEINATE, "-di"]
        );
        assert_eq!(
            keep_awake_command(true, false, None, None).unwrap().cmd,
            [CAFFEINATE, "-i"]
        );
        assert_eq!(
            keep_awake_command(false, false, None, Some(42))
                .unwrap()
                .cmd,
            [CAFFEINATE, "-di", "-w", "42"]
        );
    }

    #[test]
    fn parses_internal_battery_status_table() {
        for (state, charging, discharging, neutral) in [
            ("charging", true, false, None),
            ("finishing charge", true, false, None),
            ("discharging", false, true, None),
            ("charged", false, false, Some("charged")),
            ("not charging", false, false, Some("not charging")),
            ("calibrating", false, false, Some("calibrating")),
        ] {
            let fixture = format!(
                "Now drawing from 'AC Power'\n -InternalBattery-0 (id=123)\t72%; {state}; 1:23 remaining present: true\n"
            );
            let parsed = parse_pmset_batt(&fixture).unwrap();
            assert_eq!(parsed.percent, 72, "state={state}");
            assert_eq!(parsed.charging, charging, "state={state}");
            assert_eq!(parsed.discharging, discharging, "state={state}");
            assert_eq!(parsed.neutral_state.as_deref(), neutral, "state={state}");
        }
    }

    #[test]
    fn rejects_unparseable_battery_fixtures() {
        for fixture in [
            "Now drawing from 'AC Power'\n",
            "-InternalBattery-0 (id=123) 72%\n",
            "-InternalBattery-0 (id=123) nope; charging; present: true\n",
            "-InternalBattery-0 (id=123) 101%; charged; present: true\n",
        ] {
            assert!(parse_pmset_batt(fixture).is_err(), "fixture={fixture:?}");
        }
    }

    #[test]
    fn sleep_disabled_requires_one_exact_value() {
        for (fixture, expected) in [
            ("System-wide power settings:\n SleepDisabled 0\n", 0),
            ("SleepDisabled 1\n", 1),
        ] {
            assert_eq!(parse_pmset_disable_sleep(fixture).unwrap(), expected);
        }
        for fixture in [
            "System-wide power settings:\n",
            "SleepDisabled\n",
            "SleepDisabled nope\n",
            "SleepDisabled 2\n",
            "SleepDisabled 0 extra\n",
        ] {
            assert!(
                parse_pmset_disable_sleep(fixture).is_err(),
                "fixture={fixture:?}"
            );
        }
    }
}
