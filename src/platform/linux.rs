use super::KeepAwake;
use crate::error::{AppError, Result};
use crate::supervisor::BatteryStatus;
use std::path::Path;
use std::process::{Command, Stdio};

const POWER_SUPPLY: &str = "/sys/class/power_supply";
const EXPECTED: &[&str] = &["systemd-inhibit", "sleep", "tail", "wake"];
const DISPLAY_INHIBITORS: &[&str] = &["idle:sleep:handle-lid-switch", "idle:sleep", "sleep"];
const SYSTEM_INHIBITORS: &[&str] = &["sleep:handle-lid-switch", "sleep"];

pub fn expected_command_basenames() -> &'static [&'static str] {
    EXPECTED
}

pub fn supports_even_lid() -> bool {
    false
}

pub fn static_start_note() -> Option<String> {
    None
}

pub fn keep_awake_command(
    no_display: bool,
    even_lid: bool,
    timeout_sec: Option<i64>,
    wait_pid: Option<u32>,
) -> Result<KeepAwake> {
    let systemd_inhibit = super::resolve_on_path(
        "systemd-inhibit",
        "systemd-inhibit not found on PATH; wake requires systemd on Linux",
    )?;
    let (what, note) = select_inhibitor(no_display, even_lid, |what| {
        probe_inhibitor(&systemd_inhibit, what)
    })?;
    Ok(KeepAwake {
        cmd: inhibitor_command(systemd_inhibit, what, timeout_sec, wait_pid),
        note,
    })
}

fn inhibitor_command(
    systemd_inhibit: String,
    what: &str,
    timeout_sec: Option<i64>,
    wait_pid: Option<u32>,
) -> Vec<String> {
    let mut cmd = vec![
        systemd_inhibit,
        format!("--what={what}"),
        "--who=wake".into(),
        "--why=wake CLI".into(),
    ];
    if let Some(pid) = wait_pid {
        cmd.extend([
            "tail".into(),
            format!("--pid={pid}"),
            "-f".into(),
            "/dev/null".into(),
        ]);
    } else {
        cmd.extend([
            "sleep".into(),
            timeout_sec
                .map(|timeout| timeout.to_string())
                .unwrap_or_else(|| "infinity".into()),
        ]);
    }
    cmd
}

pub fn read_battery() -> Result<BatteryStatus> {
    read_battery_from(Path::new(POWER_SUPPLY))
}

fn read_battery_from(base: &Path) -> Result<BatteryStatus> {
    if !base.is_dir() {
        return Err(AppError::fail("no usable battery found"));
    }
    let batteries: Vec<_> = std::fs::read_dir(base)
        .map_err(|_| AppError::fail("no usable battery found"))?
        .filter_map(|entry| read_battery_dir(&entry.ok()?.path()))
        .collect();
    if batteries.is_empty() {
        return Err(AppError::fail("no usable battery found"));
    }

    let charging = batteries
        .iter()
        .any(|battery| battery.status.eq_ignore_ascii_case("charging"));
    let discharging = !charging
        && batteries
            .iter()
            .any(|battery| battery.status.eq_ignore_ascii_case("discharging"));
    let percent = aggregate_percent(&batteries);
    Ok(BatteryStatus {
        percent,
        charging,
        discharging,
        neutral_state: (!charging && !discharging)
            .then(|| "not charging or discharging".to_string()),
    })
}

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

fn select_inhibitor(
    no_display: bool,
    even_lid: bool,
    mut probe: impl FnMut(&str) -> bool,
) -> Result<(&'static str, Option<String>)> {
    let candidates = if no_display {
        SYSTEM_INHIBITORS
    } else {
        DISPLAY_INHIBITORS
    };
    if even_lid {
        let what = candidates[0];
        return if probe(what) {
            Ok((what, None))
        } else {
            Err(AppError::fail(format!(
                "--even-lid requires systemd inhibitor scope {what}"
            )))
        };
    }
    for (index, &what) in candidates.iter().enumerate() {
        if probe(what) {
            let note = (index > 0)
                .then(|| format!("note: inhibitor degraded to {what}; lid closure may suspend"));
            return Ok((what, note));
        }
    }
    Err(AppError::fail("no supported systemd inhibitor scope"))
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
        .is_ok_and(|status| status.success())
}

struct Battery {
    capacity: Option<i32>,
    energy: Option<(u64, u64)>,
    charge: Option<(u64, u64)>,
    status: String,
}

fn aggregate_percent(batteries: &[Battery]) -> i32 {
    let use_energy = if batteries.iter().all(|battery| battery.energy.is_some()) {
        true
    } else if batteries.iter().all(|battery| battery.charge.is_some()) {
        false
    } else {
        return (batteries.iter().map(own_percent).sum::<f64>() / batteries.len() as f64)
            .round()
            .clamp(0.0, 100.0) as i32;
    };
    let (now, full) = batteries
        .iter()
        .map(|battery| {
            if use_energy {
                battery.energy
            } else {
                battery.charge
            }
            .expect("all batteries have compatible measurements")
        })
        .fold((0u128, 0u128), |acc, pair| {
            (acc.0 + pair.0 as u128, acc.1 + pair.1 as u128)
        });
    (100.0 * now as f64 / full as f64).round().clamp(0.0, 100.0) as i32
}

fn own_percent(battery: &Battery) -> f64 {
    battery
        .energy
        .or(battery.charge)
        .map(|(now, full)| (100.0 * now as f64 / full as f64).clamp(0.0, 100.0))
        .or_else(|| battery.capacity.map(f64::from))
        .expect("usable battery has a percentage")
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_string())
}

fn read_battery_dir(dir: &Path) -> Option<Battery> {
    if !dir.is_dir() || read_trimmed(&dir.join("type"))? != "Battery" {
        return None;
    }
    let energy = read_measurement(dir, "energy_now", "energy_full");
    let charge = read_measurement(dir, "charge_now", "charge_full");
    let capacity = read_trimmed(&dir.join("capacity"))
        .and_then(|value| value.parse::<i32>().ok())
        .map(|value| value.clamp(0, 100));
    if energy.is_none() && charge.is_none() && capacity.is_none() {
        return None;
    }
    Some(Battery {
        capacity,
        energy,
        charge,
        status: read_trimmed(&dir.join("status")).unwrap_or_default(),
    })
}

fn read_measurement(dir: &Path, now_name: &str, full_name: &str) -> Option<(u64, u64)> {
    let now: i64 = read_trimmed(&dir.join(now_name))?.parse().ok()?;
    let full: i64 = read_trimmed(&dir.join(full_name))?.parse().ok()?;
    (now >= 0 && full > 0).then_some((now as u64, full as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn from_fixture(fixture: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "wake-rs-battery-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            for line in fixture.lines().filter(|line| !line.is_empty()) {
                let (relative, value) = line.split_once('=').unwrap();
                let file = path.join(relative);
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(file, value).unwrap();
            }
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn inhibitor_wait_command_tracks_the_supervisor_pid() {
        let command = inhibitor_command("systemd-inhibit".into(), "sleep", None, Some(42));
        assert_eq!(&command[4..], ["tail", "--pid=42", "-f", "/dev/null"]);
    }

    #[test]
    fn non_explicit_inhibitor_probes_keep_order_and_explain_lid_degradation() {
        let mut probed = Vec::new();
        let selected = select_inhibitor(false, false, |what| {
            probed.push(what.to_string());
            what == "idle:sleep"
        })
        .unwrap();
        assert_eq!(probed, ["idle:sleep:handle-lid-switch", "idle:sleep"]);
        assert_eq!(selected.0, "idle:sleep");
        assert_eq!(
            selected.1.as_deref(),
            Some("note: inhibitor degraded to idle:sleep; lid closure may suspend")
        );

        let mut probed = Vec::new();
        let selected = select_inhibitor(true, false, |what| {
            probed.push(what.to_string());
            what == "sleep"
        })
        .unwrap();
        assert_eq!(probed, ["sleep:handle-lid-switch", "sleep"]);
        assert_eq!(selected.0, "sleep");
        assert_eq!(
            selected.1.as_deref(),
            Some("note: inhibitor degraded to sleep; lid closure may suspend")
        );
    }

    #[test]
    fn explicit_display_inhibitor_probes_only_the_required_scope() {
        let mut probed = Vec::new();
        let selected = select_inhibitor(false, true, |what| {
            probed.push(what.to_string());
            true
        })
        .unwrap();
        assert_eq!(probed, ["idle:sleep:handle-lid-switch"]);
        assert_eq!(selected, ("idle:sleep:handle-lid-switch", None));
    }

    #[test]
    fn explicit_system_inhibitor_probes_only_the_required_scope() {
        let mut probed = Vec::new();
        let selected = select_inhibitor(true, true, |what| {
            probed.push(what.to_string());
            true
        })
        .unwrap();
        assert_eq!(probed, ["sleep:handle-lid-switch"]);
        assert_eq!(selected, ("sleep:handle-lid-switch", None));
    }

    #[test]
    fn explicit_inhibitor_refusal_never_falls_back_and_names_the_required_scope() {
        for (no_display, required) in [
            (false, "idle:sleep:handle-lid-switch"),
            (true, "sleep:handle-lid-switch"),
        ] {
            let mut probed = Vec::new();
            let error = select_inhibitor(no_display, true, |what| {
                probed.push(what.to_string());
                false
            })
            .unwrap_err();
            assert_eq!(probed, [required]);
            assert!(error.message().contains("--even-lid"));
            assert!(error.message().contains(required));
        }
    }

    #[test]
    fn aggregates_battery_fixtures() {
        for (name, fixture, percent, charging, discharging) in [
            (
                "compatible energy is full-weighted with charging precedence",
                "BAT0/type=Battery\nBAT0/status=Discharging\nBAT0/energy_now=90\nBAT0/energy_full=100\nBAT1/type=Battery\nBAT1/status=Charging\nBAT1/energy_now=100\nBAT1/energy_full=900\n",
                19,
                true,
                false,
            ),
            (
                "incompatible units average each battery",
                "BAT0/type=Battery\nBAT0/status=Discharging\nBAT0/energy_now=90\nBAT0/energy_full=100\nBAT1/type=Battery\nBAT1/status=Discharging\nBAT1/charge_now=10\nBAT1/charge_full=100\n",
                50,
                false,
                true,
            ),
            (
                "missing pair falls back to per-battery capacity",
                "BAT0/type=Battery\nBAT0/status=Unknown\nBAT0/energy_now=80\nBAT0/energy_full=100\nBAT1/type=Battery\nBAT1/status=Full\nBAT1/capacity=20\nAC/type=Mains\nAC/capacity=99\n",
                50,
                false,
                false,
            ),
        ] {
            let base = TempDir::from_fixture(fixture);
            let status = read_battery_from(&base.0).unwrap();
            assert_eq!(status.percent, percent, "{name}");
            assert_eq!(status.charging, charging, "{name}");
            assert_eq!(status.discharging, discharging, "{name}");
        }
    }

    #[test]
    fn rejects_a_fixture_without_usable_batteries() {
        let base = TempDir::from_fixture("AC/type=Mains\nBAT0/type=Battery\nBAT0/status=Unknown\n");
        assert!(read_battery_from(&base.0).is_err());
    }
}
