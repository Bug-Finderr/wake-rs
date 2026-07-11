use crate::error::{AppError, Result};
use crate::run::{BatteryStatus, Mode};
use std::process::{Child, Command, Stdio};

const CAFFEINATE: &str = "/usr/bin/caffeinate";
const PMSET: &str = "/usr/bin/pmset";
const SUDO: &str = "/usr/bin/sudo";

pub fn supports_interactive() -> bool {
    true
}

pub fn supports_even_lid() -> bool {
    true
}

pub struct Inhibitor {
    child: Child,
}

impl Inhibitor {
    pub fn start(mode: Mode) -> Result<Self> {
        let mut command = Command::new(CAFFEINATE);
        command.arg("-i");
        if mode == Mode::DisplaySystem {
            command.arg("-d");
        }
        let child = command
            .args(["-w", &std::process::id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| AppError::fail(format!("could not start caffeinate: {error}")))?;
        Ok(Self { child })
    }

    pub fn note(&self) -> Option<&str> {
        Some("note: closing the lid still sleeps the Mac unless you use --even-lid")
    }

    pub fn alive(&mut self) -> bool {
        self.child.try_wait().is_ok_and(|status| status.is_none())
    }
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LidSnapshot {
    pub sleep_disabled: i32,
}

pub fn read_lid_snapshot() -> Result<LidSnapshot> {
    Ok(LidSnapshot {
        sleep_disabled: read_disable_sleep()?,
    })
}

pub fn authenticate_privilege() -> Result<()> {
    if Command::new(SUDO).arg("-v").status()?.success() {
        Ok(())
    } else {
        Err(AppError::fail(
            "sudo authentication failed; --even-lid was not enabled",
        ))
    }
}

pub fn disable_lid(_snapshot: &LidSnapshot) -> Result<()> {
    write_disable_sleep(1)
}

pub fn restore_lid(snapshot: &LidSnapshot) -> Result<()> {
    write_disable_sleep(snapshot.sleep_disabled)
}

pub fn lid_override_is_active(_snapshot: &LidSnapshot) -> Result<bool> {
    Ok(read_disable_sleep()? == 1)
}

pub fn lid_is_restored(snapshot: &LidSnapshot) -> Result<bool> {
    Ok(read_disable_sleep()? == snapshot.sleep_disabled)
}

pub fn read_battery() -> Result<BatteryStatus> {
    battery_from_pmset(&capture(PMSET, &["-g", "batt"])?)
}

fn battery_from_pmset(output: &str) -> Result<BatteryStatus> {
    let percent = first_percent(output)
        .ok_or_else(|| AppError::fail("cannot parse battery percentage from pmset"))?;
    let state = output
        .lines()
        .find(|line| line.contains('%'))
        .and_then(|line| line.split(';').nth(1))
        .map(|state| state.trim().to_lowercase())
        .ok_or_else(|| AppError::fail("cannot parse battery state from pmset"))?;
    let charging = matches!(state.as_str(), "charging" | "finishing charge");
    let discharging = state == "discharging";
    Ok(BatteryStatus {
        percent,
        charging,
        discharging,
        neutral_state: (!charging && !discharging).then_some(state),
    })
}

fn read_disable_sleep() -> Result<i32> {
    for line in capture(PMSET, &["-g"])?.lines() {
        let mut parts = line.split_whitespace();
        if parts
            .next()
            .is_some_and(|key| key.eq_ignore_ascii_case("SleepDisabled"))
        {
            return match (parts.next(), parts.next()) {
                (Some("0"), None) => Ok(0),
                (Some("1"), None) => Ok(1),
                _ => Err(AppError::fail(
                    "cannot parse SleepDisabled value from pmset",
                )),
            };
        }
    }
    Err(AppError::fail("pmset did not report SleepDisabled"))
}

fn write_disable_sleep(value: i32) -> Result<()> {
    if !matches!(value, 0 | 1) {
        return Err(AppError::fail("SleepDisabled must be 0 or 1"));
    }
    let status = Command::new(PMSET)
        .args(["-a", "disablesleep", &value.to_string()])
        .status()?;
    if !status.success() {
        return Err(AppError::fail(format!(
            "pmset disablesleep exited with status {}",
            status.code().unwrap_or(-1)
        )));
    }
    if read_disable_sleep()? == value {
        Ok(())
    } else {
        Err(AppError::fail(format!(
            "failed to set SleepDisabled to {value}"
        )))
    }
}

fn first_percent(output: &str) -> Option<i32> {
    let bytes = output.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_digit() {
            let start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            if bytes.get(index) == Some(&b'%') {
                return output[start..index].parse().ok();
            }
        } else {
            index += 1;
        }
    }
    None
}

fn capture(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(AppError::fail(format!(
            "{program} exited with status {}",
            output.status.code().unwrap_or(-1)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
