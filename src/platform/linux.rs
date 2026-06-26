//! Linux: `systemd-inhibit` for sleep locks (degrading gracefully when polkit denies lid-switch),
//! sysfs `/sys/class/power_supply` for battery.

use super::KeepAwake;
use crate::error::{AppError, Result};
use crate::supervisor::BatteryStatus;
use std::path::Path;
use std::process::{Command, Stdio};

const POWER_SUPPLY: &str = "/sys/class/power_supply";
const EXPECTED: &[&str] = &["systemd-inhibit", "sleep", "tail", "wake"];
const DISPLAY_SYSTEM_INHIBITORS: &[&str] = &["idle:sleep:handle-lid-switch", "idle:sleep", "sleep"];
const SYSTEM_ONLY_INHIBITORS: &[&str] = &["sleep:handle-lid-switch", "sleep"];
const INHIBIT_DENIED_MESSAGE: &str = "systemd-inhibit cannot take inhibitor locks in this session (polkit denied); try from a local desktop session or as root";

pub fn expected_command_basenames() -> &'static [&'static str] {
    EXPECTED
}

pub fn supports_interactive() -> bool {
    true
}

pub fn supports_even_lid() -> bool {
    false
}

pub fn static_start_note() -> Option<String> {
    None
}

pub fn keep_awake_command(
    no_display: bool,
    timeout_sec: Option<i64>,
    wait_pid: Option<u32>,
) -> Result<KeepAwake> {
    let systemd_inhibit = super::resolve_on_path(
        "systemd-inhibit",
        "systemd-inhibit not found on PATH; wake requires systemd on Linux",
    )?;
    let (requested, what) = choose_inhibitor_what(no_display, &systemd_inhibit)?;
    let note = start_note_for(requested, &what);
    let mut cmd = vec![
        systemd_inhibit,
        format!("--what={what}"),
        "--who=wake".into(),
        "--why=wake CLI".into(),
    ];
    if let Some(p) = wait_pid {
        cmd.push("tail".into());
        cmd.push(format!("--pid={p}"));
        cmd.push("-f".into());
        cmd.push("/dev/null".into());
    } else {
        cmd.push("sleep".into());
        cmd.push(
            timeout_sec
                .map(|t| t.to_string())
                .unwrap_or_else(|| "infinity".into()),
        );
    }
    Ok(KeepAwake { cmd, note })
}

pub fn find_app_pid(name: &str) -> Result<Option<u32>> {
    let pattern = case_insensitive_ere(name);
    let exact = super::first_allowed_pid(&super::pgrep(&["pgrep", "-x", &pattern]));
    if exact.is_some() {
        return Ok(exact);
    }
    if name.chars().count() > 15 {
        let short: String = name.chars().take(15).collect();
        let exact = super::first_allowed_pid(&super::pgrep(&[
            "pgrep",
            "-x",
            &case_insensitive_ere(&short),
        ]));
        if exact.is_some() {
            return Ok(exact);
        }
    }
    Ok(super::first_allowed_pid(&super::pgrep(&[
        "pgrep", "-f", &pattern,
    ])))
}

pub fn read_battery() -> Result<BatteryStatus> {
    let base = Path::new(POWER_SUPPLY);
    if !base.is_dir() {
        return Err(AppError::fail("no usable battery found"));
    }
    let mut batteries = Vec::new();
    for entry in std::fs::read_dir(base).map_err(|_| AppError::fail("no usable battery found"))? {
        let Ok(entry) = entry else { continue };
        let dir = entry.path();
        if dir.is_dir()
            && let Some(b) = read_battery_dir(&dir)
        {
            batteries.push(b);
        }
    }
    if batteries.is_empty() {
        return Err(AppError::fail("no usable battery found"));
    }

    let mut charging = false;
    let mut any_discharging = false;
    let mut now_sum: u64 = 0;
    let mut full_sum: u64 = 0;
    let mut capacity_sum: i64 = 0;
    for b in &batteries {
        match b.status.to_lowercase().as_str() {
            "charging" => charging = true,
            "discharging" => any_discharging = true,
            _ => {}
        }
        if let Some((now, full)) = b.measurement {
            now_sum += now;
            full_sum += full;
        }
        if let Some(c) = b.capacity {
            capacity_sum += c as i64;
        }
    }
    let discharging = !charging && any_discharging;
    let percent = if full_sum > 0 {
        ((100.0 * now_sum as f64) / full_sum as f64).round() as i32
    } else {
        (capacity_sum as f64 / batteries.len() as f64).round() as i32
    }
    .clamp(0, 100);
    let neutral_state =
        (!charging && !discharging).then(|| "not charging or discharging".to_string());
    Ok(BatteryStatus {
        percent,
        charging,
        discharging,
        neutral_state,
    })
}

// even-lid unsupported on Linux (systemd handles lid inhibition when privileged)
fn unsupported<T>() -> Result<T> {
    Err(AppError::fail(
        "--even-lid is not supported on this platform",
    ))
}
pub fn read_disable_sleep() -> Result<i32> {
    unsupported()
}
pub fn authenticate_sudo() -> Result<bool> {
    unsupported()
}
pub fn set_disable_sleep_foreground(_value: i32) -> Result<()> {
    unsupported()
}
pub fn set_disable_sleep_non_interactive(_value: i32) -> Result<bool> {
    unsupported()
}
pub fn refresh_sudo_non_interactive() -> Result<bool> {
    unsupported()
}

// ---- helpers ----

fn choose_inhibitor_what(
    no_display: bool,
    systemd_inhibit: &str,
) -> Result<(&'static str, String)> {
    let candidates = if no_display {
        SYSTEM_ONLY_INHIBITORS
    } else {
        DISPLAY_SYSTEM_INHIBITORS
    };
    let requested = candidates[0];
    for candidate in candidates {
        if probe_inhibitor(systemd_inhibit, candidate) {
            return Ok((requested, candidate.to_string()));
        }
    }
    Err(AppError::fail(INHIBIT_DENIED_MESSAGE))
}

fn probe_inhibitor(systemd_inhibit: &str, what: &str) -> bool {
    Command::new(systemd_inhibit)
        .args([
            &format!("--what={what}"),
            "--who=wake",
            "--why=probe",
            "true",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn start_note_for(requested: &str, what: &str) -> Option<String> {
    if requested == what {
        return None;
    }
    if what == "idle:sleep" {
        return Some(
            "note: lid-switch inhibition unavailable in this session; idle/sleep inhibition active"
                .into(),
        );
    }
    if what == "sleep" && requested.contains("idle") {
        return Some("note: lid-switch and idle inhibition unavailable in this session; sleep inhibition active".into());
    }
    Some("note: lid-switch inhibition unavailable in this session; sleep inhibition active".into())
}

struct Battery {
    capacity: Option<i32>,
    measurement: Option<(u64, u64)>,
    status: String,
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn read_battery_dir(dir: &Path) -> Option<Battery> {
    if read_trimmed(&dir.join("type"))? != "Battery" {
        return None;
    }
    let status = read_trimmed(&dir.join("status")).unwrap_or_default();
    let measurement = read_measurement(dir, "energy_now", "energy_full")
        .or_else(|| read_measurement(dir, "charge_now", "charge_full"));
    let capacity = read_trimmed(&dir.join("capacity"))
        .and_then(|s| s.parse::<i32>().ok())
        .map(|c| c.clamp(0, 100));
    if measurement.is_none() && capacity.is_none() {
        return None;
    }
    Some(Battery {
        capacity,
        measurement,
        status,
    })
}

fn read_measurement(dir: &Path, now_name: &str, full_name: &str) -> Option<(u64, u64)> {
    let now: i64 = read_trimmed(&dir.join(now_name))?.parse().ok()?;
    let full: i64 = read_trimmed(&dir.join(full_name))?.parse().ok()?;
    if full <= 0 || now < 0 {
        return None;
    }
    Some((now as u64, full as u64))
}

fn case_insensitive_ere(name: &str) -> String {
    let mut out = String::with_capacity(name.len() * 4);
    for c in name.chars() {
        if c.is_ascii_lowercase() {
            out.push('[');
            out.push(c);
            out.push(c.to_ascii_uppercase());
            out.push(']');
        } else if c.is_ascii_uppercase() {
            out.push('[');
            out.push(c.to_ascii_lowercase());
            out.push(c);
            out.push(']');
        } else if is_ere_metacharacter(c) {
            out.push('\\');
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out
}

fn is_ere_metacharacter(c: char) -> bool {
    matches!(
        c,
        '.' | '[' | ']' | '\\' | '^' | '$' | '*' | '+' | '?' | '{' | '}' | '|' | '(' | ')'
    )
}
