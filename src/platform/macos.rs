//! macOS: `caffeinate` for sleep assertions, `pmset` for battery + SleepDisabled, `sudo` for --even-lid.

use super::KeepAwake;
use crate::error::{AppError, Result};
use crate::supervisor::BatteryStatus;
use std::process::{Command, Stdio};

const CAFFEINATE: &str = "/usr/bin/caffeinate";
const PMSET: &str = "/usr/bin/pmset";
const PGREP: &str = "/usr/bin/pgrep";
const SUDO: &str = "/usr/bin/sudo";
const LID_CLOSE_NOTE: &str = "note: closing the lid still sleeps the mac unless you use --even-lid";
const EXPECTED: &[&str] = &["caffeinate", "wake"];

pub fn expected_command_basenames() -> &'static [&'static str] {
    EXPECTED
}

pub fn supports_interactive() -> bool {
    true
}

pub fn supports_even_lid() -> bool {
    true
}

pub fn static_start_note() -> Option<String> {
    Some(LID_CLOSE_NOTE.to_string())
}

pub fn keep_awake_command(
    no_display: bool,
    timeout_sec: Option<i64>,
    wait_pid: Option<u32>,
) -> Result<KeepAwake> {
    let mut cmd = vec![
        CAFFEINATE.to_string(),
        format!("-{}", if no_display { 'i' } else { 'd' }),
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
    let out = capture(PMSET, &["-g", "batt"])?;
    let percent = first_percent(&out)
        .ok_or_else(|| AppError::fail("cannot parse battery percentage from pmset"))?;
    let lower = out.to_lowercase();
    let discharging = lower.contains("discharging") || lower.contains("battery power");
    let charging = lower.contains("; charging;") || lower.contains("ac power");
    Ok(BatteryStatus {
        percent,
        charging,
        discharging,
        neutral_state: None,
    })
}

pub fn find_app_pid(name: &str) -> Result<Option<u32>> {
    let exact = super::first_allowed_pid(&super::pgrep(&[PGREP, "-i", "-x", name]));
    if exact.is_some() {
        return Ok(exact);
    }
    Ok(super::first_allowed_pid(&super::pgrep(&[
        PGREP, "-i", "-f", name,
    ])))
}

pub fn read_disable_sleep() -> Result<i32> {
    let out = capture(PMSET, &["-g"])?;
    for line in out.lines() {
        let mut parts = line.trim().split_whitespace();
        if let Some(first) = parts.next()
            && first.eq_ignore_ascii_case("SleepDisabled")
            && let (Some(num), None) = (parts.next(), parts.clone().next())
        {
            return parse_disable_sleep_value(num);
        }
    }
    Ok(0)
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

// ---- helpers ----

fn first_percent(out: &str) -> Option<i32> {
    let bytes = out.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'%' {
                return out[start..i].parse().ok();
            }
        } else {
            i += 1;
        }
    }
    None
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
