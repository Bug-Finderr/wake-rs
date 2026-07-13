use crate::error::{AppError, Result};
use crate::run::{BatteryStatus, Mode};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

const POWER_SUPPLY: &str = "/sys/class/power_supply";
const DISPLAY_SYSTEM_INHIBITORS: &[&str] = &["idle:sleep:handle-lid-switch", "idle:sleep", "sleep"];
const SYSTEM_ONLY_INHIBITORS: &[&str] = &["sleep:handle-lid-switch", "sleep"];
const INHIBIT_DENIED_MESSAGE: &str = "systemd-inhibit cannot take inhibitor locks in this session (polkit denied); try from a local desktop session or as root";
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

pub fn supports_even_lid() -> bool {
    false
}

pub struct Inhibitor {
    child: Child,
    note: Option<String>,
}

impl Inhibitor {
    pub fn start(mode: Mode) -> Result<Self> {
        let program = super::resolve_on_path(
            "systemd-inhibit",
            "systemd-inhibit not found on PATH; wake requires systemd on Linux",
        )?;
        let tail = super::resolve_on_path("tail", "tail not found on PATH")?;
        let (requested, what) = choose_inhibitor_what(mode.no_display(), &program)?;
        let child = Command::new(program)
            .args([
                format!("--what={what}"),
                "--who=wake".into(),
                "--why=wake CLI".into(),
                "--mode=block".into(),
                tail,
                format!("--pid={}", std::process::id()),
                "-f".into(),
                "/dev/null".into(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| AppError::fail(format!("could not start systemd-inhibit: {error}")))?;
        Ok(Self {
            child,
            note: start_note_for(requested, &what),
        })
    }

    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
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
    for b in &batteries {
        match b.status.to_lowercase().as_str() {
            "charging" => charging = true,
            "discharging" => any_discharging = true,
            _ => {}
        }
    }
    let discharging = !charging && any_discharging;
    let percent =
        battery_percent(&batteries).ok_or_else(|| AppError::fail("no usable battery found"))?;
    let neutral_state =
        (!charging && !discharging).then(|| "not charging or discharging".to_string());
    Ok(BatteryStatus {
        percent,
        charging,
        discharging,
        neutral_state,
    })
}

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
    let child = Command::new(systemd_inhibit)
        .args([
            &format!("--what={what}"),
            "--who=wake",
            "--why=probe",
            "true",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    child.is_ok_and(|mut child| wait_for_probe(&mut child))
}

fn wait_for_probe(child: &mut Child) -> bool {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
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
    measurement: Option<Measurement>,
    status: String,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum MeasurementKind {
    Energy,
    Charge,
}

#[derive(Clone, Copy)]
struct Measurement {
    kind: MeasurementKind,
    now: u64,
    full: u64,
}

fn battery_percent(batteries: &[Battery]) -> Option<i32> {
    if let Some(kind) = batteries
        .first()
        .and_then(|battery| battery.measurement)
        .map(|value| value.kind)
        && batteries
            .iter()
            .all(|battery| battery.measurement.is_some_and(|value| value.kind == kind))
    {
        let (now, full) = batteries
            .iter()
            .filter_map(|battery| battery.measurement)
            .fold((0_u128, 0_u128), |totals, measurement| {
                (
                    totals.0 + u128::from(measurement.now),
                    totals.1 + u128::from(measurement.full),
                )
            });
        return Some(((100.0 * now as f64 / full as f64).round() as i32).clamp(0, 100));
    }

    let percentages = batteries
        .iter()
        .filter_map(|battery| {
            battery
                .measurement
                .map(|value| 100.0 * value.now as f64 / value.full as f64)
                .or_else(|| battery.capacity.map(f64::from))
        })
        .collect::<Vec<_>>();
    (!percentages.is_empty()).then(|| {
        ((percentages.iter().sum::<f64>() / percentages.len() as f64).round() as i32).clamp(0, 100)
    })
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
    let measurement = read_measurement(dir, "energy_now", "energy_full", MeasurementKind::Energy)
        .or_else(|| read_measurement(dir, "charge_now", "charge_full", MeasurementKind::Charge));
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

fn read_measurement(
    dir: &Path,
    now_name: &str,
    full_name: &str,
    kind: MeasurementKind,
) -> Option<Measurement> {
    let now: i64 = read_trimmed(&dir.join(now_name))?.parse().ok()?;
    let full: i64 = read_trimmed(&dir.join(full_name))?.parse().ok()?;
    if full <= 0 || now < 0 {
        return None;
    }
    Some(Measurement {
        kind,
        now: now as u64,
        full: full as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measured(kind: MeasurementKind, now: u64, full: u64) -> Battery {
        Battery {
            capacity: None,
            measurement: Some(Measurement { kind, now, full }),
            status: String::new(),
        }
    }

    #[test]
    fn same_unit_batteries_are_weighted_by_full_capacity() {
        let batteries = [
            measured(MeasurementKind::Energy, 9, 10),
            measured(MeasurementKind::Energy, 10, 100),
        ];

        assert_eq!(battery_percent(&batteries), Some(17));
    }

    #[test]
    fn incompatible_battery_units_are_not_summed() {
        let batteries = [
            measured(MeasurementKind::Energy, 9, 10),
            measured(MeasurementKind::Charge, 10, 100),
        ];

        assert_eq!(battery_percent(&batteries), Some(50));
    }

    #[test]
    fn capacity_only_batteries_still_use_their_reported_percentages() {
        let batteries = [Battery {
            capacity: Some(73),
            measurement: None,
            status: String::new(),
        }];

        assert_eq!(battery_percent(&batteries), Some(73));
    }
}
