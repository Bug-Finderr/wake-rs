//! Windows: PowerShell `SetThreadExecutionState` for sleep blocking, `Win32_Battery` for charge,
//! `tasklist` for app lookup.

use super::KeepAwake;
use crate::error::{AppError, Result};
use crate::supervisor::BatteryStatus;
use base64::Engine;
use std::process::{Command, Stdio};

const POWERSHELL_MISSING: &str =
    "powershell not found on PATH; wake requires Windows PowerShell on Windows";
const EXPECTED: &[&str] = &["powershell.exe", "powershell", "wake.exe", "wake"];

pub fn expected_command_basenames() -> &'static [&'static str] {
    EXPECTED
}

#[allow(dead_code)] // part of the platform surface; the picker is gated to unix
pub fn supports_interactive() -> bool {
    false
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
    let powershell = resolve_powershell()?;
    // ES_CONTINUOUS|ES_SYSTEM_REQUIRED(|ES_DISPLAY_REQUIRED) as decimal: a hex literal like
    // 0x80000003 parses as a negative Int32 in PowerShell, so the [uint32] cast throws and the
    // assertion silently no-ops. Decimal stays in uint32 range and actually blocks sleep.
    let flags = if no_display {
        "2147483649"
    } else {
        "2147483651"
    };
    let type_definition = r#"using System;
using System.Runtime.InteropServices;
namespace Wake {
    public static class Native {
        [DllImport("kernel32.dll")]
        public static extern uint SetThreadExecutionState(uint esFlags);
    }
}
"#;
    let block = if let Some(pid) = wait_pid {
        format!("Wait-Process -Id {pid} -ErrorAction SilentlyContinue")
    } else if let Some(t) = timeout_sec {
        format!("Start-Sleep -Seconds {t}")
    } else {
        "while ($true) { Start-Sleep -Seconds 3600 }".to_string()
    };
    // INVARIANT: only interpolate values that render as a fixed numeric/known literal here
    // (`flags` is a constant; `pid`/`t` are typed integers). A free-form `String` would not be
    // escaped by the base64 step below and could alter the script.
    let script = format!(
        "Add-Type -TypeDefinition @'\n{type_definition}'@\n\
         $r = [Wake.Native]::SetThreadExecutionState([uint32]{flags})\n\
         if ($r -eq 0) {{ exit 1 }}\n{block}\n"
    );
    let utf16: Vec<u8> = script
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    Ok(KeepAwake {
        cmd: vec![
            powershell,
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-EncodedCommand".into(),
            encoded,
        ],
        note: None,
    })
}

pub fn read_battery() -> Result<BatteryStatus> {
    let powershell = resolve_powershell()?;
    let script = "Get-CimInstance Win32_Battery \
                  | Select-Object -Property EstimatedChargeRemaining,BatteryStatus \
                  | ConvertTo-Csv -NoTypeInformation";
    let out = run_capture(
        &powershell,
        &["-NoProfile", "-NonInteractive", "-Command", script],
    )?;
    summarize_batteries(&parse_csv(&out)?)
}

pub fn find_app_pid(name: &str) -> Result<Option<u32>> {
    if name.contains('"') {
        return Err(AppError::usage(
            "app/process name cannot contain double quotes",
        ));
    }
    for image in image_name_candidates(name) {
        let filter = format!("IMAGENAME eq {image}");
        let out = run_capture("tasklist", &["/FO", "CSV", "/NH", "/FI", filter.as_str()])?;
        let pids: Vec<u32> = parse_csv(&out)?
            .iter()
            .filter(|row| row.len() >= 2)
            .filter_map(|row| row[1].trim().parse::<u32>().ok())
            .collect();
        if let Some(allowed) = super::first_allowed_pid(&pids) {
            return Ok(Some(allowed));
        }
    }
    Ok(None)
}

// ---- even-lid: unsupported on Windows ----

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

fn resolve_powershell() -> Result<String> {
    super::resolve_on_path("powershell.exe", POWERSHELL_MISSING)
        .or_else(|_| super::resolve_on_path("powershell", POWERSHELL_MISSING))
}

fn image_name_candidates(name: &str) -> Vec<String> {
    let trimmed = name.trim();
    if trimmed.to_lowercase().ends_with(".exe") {
        vec![trimmed.to_string()]
    } else {
        vec![format!("{trimmed}.exe"), trimmed.to_string()]
    }
}

fn run_capture(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .map_err(|e| AppError::fail(format!("{program}: {e}")))?;
    if !out.status.success() {
        return Err(AppError::fail(format!(
            "{program} exited with status {}",
            out.status.code().unwrap_or(-1)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn summarize_batteries(rows: &[Vec<String>]) -> Result<BatteryStatus> {
    let mut count = 0;
    let mut percent_sum = 0;
    let mut any_charging = false;
    let mut any_discharging = false;

    for row in rows {
        if row.len() < 2 || row[0].eq_ignore_ascii_case("EstimatedChargeRemaining") {
            continue;
        }
        let (Ok(percent), Ok(status)) =
            (row[0].trim().parse::<i32>(), row[1].trim().parse::<i32>())
        else {
            continue;
        };
        // Win32_Battery.BatteryStatus: 1/4/5 discharging variants, 6-9 charging variants.
        if (6..=9).contains(&status) {
            any_charging = true;
        }
        if status == 1 || status == 4 || status == 5 {
            any_discharging = true;
        }
        percent_sum += percent.clamp(0, 100);
        count += 1;
    }

    if count == 0 {
        return Err(AppError::fail("no usable battery found"));
    }
    let charging = any_charging;
    let discharging = !charging && any_discharging;
    let neutral_state =
        (!charging && !discharging).then(|| "not charging or discharging".to_string());
    let percent = ((percent_sum as f64) / (count as f64)).round() as i32;
    Ok(BatteryStatus {
        percent,
        charging,
        discharging,
        neutral_state,
    })
}

/// Minimal RFC-4180-ish CSV parser matching the reference: quotes, `""` escapes, CR/LF rows.
fn parse_csv(input: &str) -> Result<Vec<Vec<String>>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if in_quotes {
            if c == '"' {
                if i + 1 < chars.len() && chars[i + 1] == '"' {
                    field.push('"');
                    i += 1;
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
        } else if c == '"' {
            in_quotes = true;
        } else if c == ',' {
            row.push(std::mem::take(&mut field));
        } else if c == '\r' || c == '\n' {
            row.push(std::mem::take(&mut field));
            if !is_blank_row(&row) {
                rows.push(std::mem::take(&mut row));
            } else {
                row.clear();
            }
            if c == '\r' && i + 1 < chars.len() && chars[i + 1] == '\n' {
                i += 1;
            }
        } else {
            field.push(c);
        }
        i += 1;
    }

    if in_quotes {
        return Err(AppError::fail("unterminated CSV quote"));
    }
    if !chars.is_empty() || !field.is_empty() || !row.is_empty() {
        row.push(field);
        if !is_blank_row(&row) {
            rows.push(row);
        }
    }
    Ok(rows)
}

fn is_blank_row(row: &[String]) -> bool {
    row.iter().all(|f| f.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_basic() {
        let rows = parse_csv("\"a\",\"b\"\r\n\"1\",\"2\"\r\n").unwrap();
        assert_eq!(rows, vec![vec!["a", "b"], vec!["1", "2"]]);
    }

    #[test]
    fn csv_escaped_quotes_and_blank_rows() {
        let rows = parse_csv("\"x\"\"y\",z\n\n\"p\",\"q\"").unwrap();
        assert_eq!(rows, vec![vec!["x\"y", "z"], vec!["p", "q"]]);
    }

    #[test]
    fn csv_unterminated_quote_errs() {
        assert!(parse_csv("\"oops").is_err());
    }

    #[test]
    fn battery_no_rows_errors() {
        assert!(summarize_batteries(&[]).is_err());
    }

    #[test]
    fn battery_averages_and_classifies() {
        let rows = vec![
            vec!["EstimatedChargeRemaining".into(), "BatteryStatus".into()],
            vec!["80".into(), "1".into()], // discharging
            vec!["60".into(), "2".into()], // neither
        ];
        let b = summarize_batteries(&rows).unwrap();
        assert_eq!(b.percent, 70);
        assert!(!b.charging);
        assert!(b.discharging); // any_discharging && !charging
    }

    #[test]
    fn battery_charging_wins() {
        let rows = vec![vec!["50".into(), "6".into()]];
        let b = summarize_batteries(&rows).unwrap();
        assert!(b.charging);
        assert!(!b.discharging);
        assert!(b.neutral_state.is_none());
    }

    #[test]
    fn image_candidates() {
        assert_eq!(image_name_candidates("foo"), vec!["foo.exe", "foo"]);
        assert_eq!(image_name_candidates("Foo.EXE"), vec!["Foo.EXE"]);
    }
}
