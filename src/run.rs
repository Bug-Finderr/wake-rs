use crate::error::{AppError, Result};
use chrono::{DateTime, Days, Local, LocalResult, NaiveDate, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RunSpec {
    pub mode: Mode,
    pub trigger: Trigger,
    pub even_lid: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    DisplaySystem,
    SystemOnly,
}

impl Mode {
    #[cfg(target_os = "linux")]
    pub fn no_display(self) -> bool {
        self == Self::SystemOnly
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::DisplaySystem => "display+system",
            Self::SystemOnly => "system-only",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", tag = "kind")]
pub enum Trigger {
    Indefinite,
    Timed {
        seconds: i64,
        input: String,
    },
    Until {
        #[serde(rename = "endsAt")]
        ends_at: DateTime<Utc>,
        time: String,
    },
    Pid {
        process: ProcessRef,
    },
    App {
        name: String,
        process: ProcessRef,
    },
    Charge {
        target: i32,
        initial: i32,
        direction: ChargeDirection,
    },
}

impl Trigger {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Indefinite => "indefinite",
            Self::Timed { .. } => "timed",
            Self::Until { .. } => "until-time",
            Self::Pid { .. } => "while-pid",
            Self::App { .. } => "while-app",
            Self::Charge { .. } => "until-charge",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Self::Indefinite => "indefinite".into(),
            Self::Timed { input, .. } => input.clone(),
            Self::Until { time, .. } => format!("until {time}"),
            Self::Pid { process } => format!("pid {}", process.pid),
            Self::App { name, process } => format!("app '{name}' (pid {})", process.pid),
            Self::Charge {
                target,
                initial,
                direction,
            } => format!(
                "{target}% (was {initial}%, {})",
                match direction {
                    ChargeDirection::Up => "charging up",
                    ChargeDirection::Down => "discharging down",
                }
            ),
        }
    }

    pub fn session_ends_at(&self, started_at: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Self::Timed { seconds, .. } => Some(started_at + chrono::Duration::seconds(*seconds)),
            Self::Until { ends_at, .. } => Some(*ends_at),
            _ => None,
        }
    }

    pub fn is_complete(&self, wall_now: DateTime<Utc>, elapsed: std::time::Duration) -> bool {
        match self {
            Self::Timed { seconds, .. } => elapsed.as_secs() >= *seconds as u64,
            Self::Until { ends_at, .. } => wall_now >= *ends_at,
            _ => false,
        }
    }

    pub fn process(&self) -> Option<&ProcessRef> {
        match self {
            Self::Pid { process } | Self::App { process, .. } => Some(process),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProcessRef {
    pub pid: u32,
    pub start: u64,
    pub command: String,
}

impl ProcessRef {
    pub(crate) fn is_valid(&self) -> bool {
        self.pid > 0 && self.start > 0 && !self.command.trim().is_empty()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChargeDirection {
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChargePlan {
    Reached,
    Wait(ChargeDirection),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatteryStatus {
    pub percent: i32,
    pub charging: bool,
    pub discharging: bool,
    pub neutral_state: Option<String>,
}

impl RunSpec {
    pub fn validate(&self) -> Result<()> {
        let valid = match &self.trigger {
            Trigger::Indefinite => true,
            Trigger::Timed { seconds, input } => {
                (1..=crate::durations::MAX_SECONDS).contains(seconds) && !input.trim().is_empty()
            }
            Trigger::Until { time, .. } => !time.trim().is_empty(),
            Trigger::Pid { process } => process.is_valid(),
            Trigger::App { name, process } => !name.trim().is_empty() && process.is_valid(),
            Trigger::Charge {
                target,
                initial,
                direction,
            } => {
                (1..=100).contains(target)
                    && (0..=100).contains(initial)
                    && match direction {
                        ChargeDirection::Up => initial < target,
                        ChargeDirection::Down => initial > target,
                    }
            }
        };
        if valid {
            Ok(())
        } else {
            Err(AppError::fail("invalid supervisor run specification"))
        }
    }
}

pub fn plan_charge(target: i32, status: &BatteryStatus) -> Result<ChargePlan> {
    if status.discharging {
        return match status.percent.cmp(&target) {
            std::cmp::Ordering::Equal => Ok(ChargePlan::Reached),
            std::cmp::Ordering::Greater => Ok(ChargePlan::Wait(ChargeDirection::Down)),
            std::cmp::Ordering::Less => Err(AppError::usage(format!(
                "--until-charge {target} is unreachable while battery is discharging at {}%; connect power or choose a target at or below the current charge",
                status.percent
            ))),
        };
    }
    if status.charging {
        return if status.percent >= target {
            Ok(ChargePlan::Reached)
        } else {
            Ok(ChargePlan::Wait(ChargeDirection::Up))
        };
    }
    if status.percent == target {
        return Ok(ChargePlan::Reached);
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

pub fn until_deadline(input: &str) -> Result<DateTime<Utc>> {
    let (hour, minute) = parse_clock(input)?;
    let time = NaiveTime::from_hms_opt(hour, minute, 0).expect("validated clock time");
    let now = Local::now();
    resolve_future_local(now, now.date_naive(), time, |naive| {
        Local.from_local_datetime(&naive)
    })
    .map(|date| date.with_timezone(&Utc))
    .ok_or_else(|| AppError::fail(format!("could not resolve local time '{input}'")))
}

fn parse_clock(input: &str) -> Result<(u32, u32)> {
    let Some((hour, minute)) = input.split_once(':') else {
        return Err(AppError::usage(format!(
            "--until expects HH:MM, got '{input}'"
        )));
    };
    if minute.contains(':') {
        return Err(AppError::usage(format!(
            "--until expects HH:MM, got '{input}'"
        )));
    }
    let parsed = (hour.trim().parse::<u32>(), minute.trim().parse::<u32>());
    match parsed {
        (Ok(hour @ 0..=23), Ok(minute @ 0..=59)) => Ok((hour, minute)),
        _ => Err(AppError::usage(format!("--until: invalid time '{input}'"))),
    }
}

fn resolve_future_local<T: Copy + Ord>(
    now: T,
    date: NaiveDate,
    time: NaiveTime,
    mut resolve: impl FnMut(chrono::NaiveDateTime) -> LocalResult<T>,
) -> Option<T> {
    for offset in 0..=370 {
        let date = date.checked_add_days(Days::new(offset))?;
        let result = resolve(date.and_time(time));
        let candidate = match result {
            LocalResult::Single(value) => (value > now).then_some(value),
            LocalResult::Ambiguous(first, second) => [first, second]
                .into_iter()
                .filter(|value| *value > now)
                .min(),
            LocalResult::None => None,
        };
        if candidate.is_some() {
            return candidate;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{LocalResult, NaiveDate, NaiveTime, TimeZone, Utc};

    fn process() -> ProcessRef {
        ProcessRef {
            pid: 42,
            start: 1_700_000_000,
            command: "/usr/bin/editor".into(),
        }
    }

    #[test]
    fn run_spec_json_is_strict_and_round_trips() {
        let spec = RunSpec {
            mode: Mode::DisplaySystem,
            trigger: Trigger::App {
                name: "Editor".into(),
                process: process(),
            },
            even_lid: true,
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<RunSpec>(&json).unwrap(), spec);

        let unknown = json.replacen('{', r#"{"extra":true,"#, 1);
        assert!(serde_json::from_str::<RunSpec>(&unknown).is_err());
        assert!(serde_json::from_str::<RunSpec>(
            r#"{"mode":"system-only","trigger":{"kind":"pid","process":{"pid":1,"start":2,"command":"x","extra":true}}}"#,
        )
        .is_err());
    }

    #[test]
    fn run_spec_validation_table() {
        let end = Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap();
        let valid = [
            Trigger::Indefinite,
            Trigger::Timed {
                seconds: 300,
                input: "5m".into(),
            },
            Trigger::Until {
                ends_at: end,
                time: "12:00".into(),
            },
            Trigger::Pid { process: process() },
            Trigger::App {
                name: "Editor".into(),
                process: process(),
            },
            Trigger::Charge {
                target: 80,
                initial: 50,
                direction: ChargeDirection::Up,
            },
        ];
        for trigger in valid {
            assert!(
                RunSpec {
                    mode: Mode::SystemOnly,
                    trigger,
                    even_lid: false,
                }
                .validate()
                .is_ok()
            );
        }

        let invalid = [
            Trigger::Timed {
                seconds: 0,
                input: " ".into(),
            },
            Trigger::Timed {
                seconds: i64::MAX,
                input: "too long".into(),
            },
            Trigger::Pid {
                process: ProcessRef {
                    pid: 0,
                    ..process()
                },
            },
            Trigger::App {
                name: String::new(),
                process: process(),
            },
            Trigger::Charge {
                target: 101,
                initial: 50,
                direction: ChargeDirection::Up,
            },
            Trigger::Charge {
                target: 80,
                initial: 90,
                direction: ChargeDirection::Up,
            },
        ];
        for trigger in invalid {
            assert!(
                RunSpec {
                    mode: Mode::DisplaySystem,
                    trigger,
                    even_lid: false,
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn charge_plan_table_has_exact_results_and_errors() {
        let cases = [
            (80, battery(80, false, true, None), Ok(ChargePlan::Reached)),
            (
                80,
                battery(70, false, true, None),
                Err(
                    "--until-charge 80 is unreachable while battery is discharging at 70%; connect power or choose a target at or below the current charge",
                ),
            ),
            (
                80,
                battery(90, false, true, None),
                Ok(ChargePlan::Wait(ChargeDirection::Down)),
            ),
            (80, battery(80, true, false, None), Ok(ChargePlan::Reached)),
            (
                80,
                battery(60, true, false, None),
                Ok(ChargePlan::Wait(ChargeDirection::Up)),
            ),
            (80, battery(80, false, false, None), Ok(ChargePlan::Reached)),
            (
                80,
                battery(70, false, false, Some("idle")),
                Err("--until-charge 80 is unreachable while battery is idle at 70%"),
            ),
            (
                80,
                battery(70, false, false, None),
                Err("cannot determine battery charging direction"),
            ),
        ];

        for (target, status, expected) in cases {
            let actual = plan_charge(target, &status).map_err(|error| error.message().to_string());
            assert_eq!(actual, expected.map_err(str::to_string));
        }
    }

    #[test]
    fn completion_uses_monotonic_time_only_for_durations() {
        let wall = Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap();
        let timed = Trigger::Timed {
            seconds: 60,
            input: "1m".into(),
        };
        assert!(!timed.is_complete(
            wall + chrono::Duration::days(1),
            std::time::Duration::from_secs(59)
        ));
        assert!(timed.is_complete(
            wall - chrono::Duration::days(1),
            std::time::Duration::from_secs(60)
        ));

        let until = Trigger::Until {
            ends_at: wall,
            time: "12:00".into(),
        };
        assert!(!until.is_complete(
            wall - chrono::Duration::seconds(1),
            std::time::Duration::from_secs(999)
        ));
        assert!(until.is_complete(wall, std::time::Duration::ZERO));
    }

    #[test]
    fn future_local_time_handles_fold_gap_and_rollover() {
        let date = NaiveDate::from_ymd_opt(2026, 10, 25).unwrap();
        let time = NaiveTime::from_hms_opt(1, 30, 0).unwrap();
        let cases = [
            (
                "second fold occurrence",
                100,
                vec![LocalResult::Ambiguous(90, 110)],
                110,
            ),
            (
                "gap skips date",
                100,
                vec![LocalResult::None, LocalResult::Single(200)],
                200,
            ),
            (
                "past time rolls forward",
                100,
                vec![LocalResult::Single(90), LocalResult::Single(200)],
                200,
            ),
            (
                "earliest future fold occurrence",
                80,
                vec![LocalResult::Ambiguous(90, 110)],
                90,
            ),
        ];

        for (name, now, resolutions, expected) in cases {
            let mut resolutions = resolutions.into_iter();
            let actual = resolve_future_local(now, date, time, |_| {
                resolutions.next().unwrap_or(LocalResult::None)
            });
            assert_eq!(actual, Some(expected), "{name}");
        }
    }

    fn battery(
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
}
