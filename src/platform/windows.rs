use crate::error::{AppError, Result};
use crate::supervisor::BatteryStatus;
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
use windows_sys::Win32::System::Power::{
    ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, GetSystemPowerStatus,
    PowerGetActiveScheme, PowerReadACValueIndex, PowerReadDCValueIndex, PowerSetActiveScheme,
    PowerWriteACValueIndex, PowerWriteDCValueIndex, SYSTEM_POWER_STATUS, SetThreadExecutionState,
};
use windows_sys::core::GUID;

const EXPECTED: &[&str] = &["wake.exe", "wake"];
const SUB_BUTTONS: GUID = GUID {
    data1: 0x4f97_1e89,
    data2: 0xeebd,
    data3: 0x4455,
    data4: [0xa8, 0xde, 0x9e, 0x59, 0x04, 0x0e, 0x73, 0x47],
};
const LID_ACTION: GUID = GUID {
    data1: 0x5ca8_3367,
    data2: 0x6e45,
    data3: 0x459f,
    data4: [0xa2, 0x7b, 0x47, 0x6b, 0x1d, 0x01, 0xc9, 0x36],
};

pub fn expected_command_basenames() -> &'static [&'static str] {
    EXPECTED
}

pub fn static_start_note() -> Option<String> {
    None
}

pub struct ExecutionStateGuard;

impl ExecutionStateGuard {
    pub fn acquire(no_display: bool) -> Result<Self> {
        let flags =
            ES_CONTINUOUS | ES_SYSTEM_REQUIRED | if no_display { 0 } else { ES_DISPLAY_REQUIRED };
        // SAFETY: This changes only the calling thread's execution state.
        if unsafe { SetThreadExecutionState(flags) } == 0 {
            return Err(AppError::fail("SetThreadExecutionState failed"));
        }
        Ok(Self)
    }
}

impl Drop for ExecutionStateGuard {
    fn drop(&mut self) {
        // SAFETY: ES_CONTINUOUS clears this thread's requirements.
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
    }
}

pub fn read_battery() -> Result<BatteryStatus> {
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: `status` is valid writable storage.
    if unsafe { GetSystemPowerStatus(&mut status) } == 0 {
        return Err(AppError::fail("GetSystemPowerStatus failed"));
    }
    map_power_status(status)
}

fn map_power_status(status: SYSTEM_POWER_STATUS) -> Result<BatteryStatus> {
    if status.BatteryFlag == u8::MAX
        || status.BatteryFlag & 0x80 != 0
        || status.BatteryLifePercent == u8::MAX
        || status.BatteryLifePercent > 100
    {
        return Err(AppError::fail("no usable battery found"));
    }
    let charging = status.BatteryFlag & 0x08 != 0;
    let discharging = !charging && status.ACLineStatus == 0;
    Ok(BatteryStatus {
        percent: i32::from(status.BatteryLifePercent),
        charging,
        discharging,
        neutral_state: (!charging && !discharging)
            .then(|| "not charging or discharging".to_string()),
    })
}

#[derive(Clone, Copy)]
pub struct LidSnapshot {
    pub scheme: GUID,
    pub ac: u32,
    pub dc: u32,
}

pub fn capture_lid_snapshot() -> Result<LidSnapshot> {
    let scheme = active_scheme()?;
    let (ac, dc) = read_lid_values(&scheme)?;
    Ok(LidSnapshot { scheme, ac, dc })
}

pub fn active_scheme() -> Result<GUID> {
    // SAFETY: PowerGetActiveScheme allocates the GUID and LocalFree releases it.
    unsafe {
        let mut raw = std::ptr::null_mut();
        let code = PowerGetActiveScheme(std::ptr::null_mut(), &mut raw);
        if code != ERROR_SUCCESS || raw.is_null() {
            return Err(power_error("read the active power scheme", code));
        }
        let scheme = *raw;
        LocalFree(raw.cast());
        Ok(scheme)
    }
}

pub fn scheme_is_active(scheme: &GUID) -> Result<bool> {
    Ok(guid_eq(&active_scheme()?, scheme))
}

pub fn read_lid_values(scheme: &GUID) -> Result<(u32, u32)> {
    let mut ac = 0;
    let mut dc = 0;
    // SAFETY: GUID and output pointers remain valid for both calls.
    unsafe {
        let code = PowerReadACValueIndex(
            std::ptr::null_mut(),
            scheme,
            &SUB_BUTTONS,
            &LID_ACTION,
            &mut ac,
        );
        if code != ERROR_SUCCESS {
            return Err(power_error("read the AC lid action", code));
        }
        let code = PowerReadDCValueIndex(
            std::ptr::null_mut(),
            scheme,
            &SUB_BUTTONS,
            &LID_ACTION,
            &mut dc,
        );
        if code != ERROR_SUCCESS {
            return Err(power_error("read the DC lid action", code));
        }
    }
    Ok((ac, dc))
}

fn write_ac(scheme: &GUID, value: u32) -> Result<()> {
    // SAFETY: GUID pointers remain valid for the call.
    let code = unsafe {
        PowerWriteACValueIndex(
            std::ptr::null_mut(),
            scheme,
            &SUB_BUTTONS,
            &LID_ACTION,
            value,
        )
    };
    (code == ERROR_SUCCESS)
        .then_some(())
        .ok_or_else(|| power_error("write the AC lid action", code))
}

fn write_dc(scheme: &GUID, value: u32) -> Result<()> {
    // SAFETY: GUID pointers remain valid for the call.
    let code = unsafe {
        PowerWriteDCValueIndex(
            std::ptr::null_mut(),
            scheme,
            &SUB_BUTTONS,
            &LID_ACTION,
            value,
        )
    };
    (code == ERROR_SUCCESS)
        .then_some(())
        .ok_or_else(|| power_error("write the DC lid action", code))
}

fn reactivate_if_active(scheme: &GUID) -> Result<()> {
    if !scheme_is_active(scheme)? {
        return Ok(());
    }
    // SAFETY: `scheme` remains valid for the call.
    let code = unsafe { PowerSetActiveScheme(std::ptr::null_mut(), scheme) };
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(power_error("reactivate the active power scheme", code))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestoreField {
    Keep,
    Write,
    Conflict,
}

fn restore_field(current: u32, original: u32) -> RestoreField {
    if current == original {
        RestoreField::Keep
    } else if current == 0 {
        RestoreField::Write
    } else {
        RestoreField::Conflict
    }
}

fn should_reactivate_lid(
    wrote_any: bool,
    current: (u32, u32),
    original: (u32, u32),
    recorded_scheme_is_active: bool,
) -> bool {
    recorded_scheme_is_active && (wrote_any || current == original)
}

pub fn restore_lid_snapshot(snapshot: &LidSnapshot) -> Result<()> {
    let mut issues = Vec::new();
    let mut wrote_any = false;
    match restore_field(read_lid_values(&snapshot.scheme)?.0, snapshot.ac) {
        RestoreField::Write => match write_ac(&snapshot.scheme, snapshot.ac) {
            Ok(()) => wrote_any = true,
            Err(error) => issues.push(format!("AC write failed: {error}")),
        },
        RestoreField::Conflict => issues.push("AC has a third-party value".into()),
        RestoreField::Keep => {}
    }
    match restore_field(read_lid_values(&snapshot.scheme)?.1, snapshot.dc) {
        RestoreField::Write => match write_dc(&snapshot.scheme, snapshot.dc) {
            Ok(()) => wrote_any = true,
            Err(error) => issues.push(format!("DC write failed: {error}")),
        },
        RestoreField::Conflict => issues.push("DC has a third-party value".into()),
        RestoreField::Keep => {}
    }
    let after = read_lid_values(&snapshot.scheme)?;
    let original = (snapshot.ac, snapshot.dc);
    if should_reactivate_lid(
        wrote_any,
        after,
        original,
        scheme_is_active(&snapshot.scheme)?,
    ) {
        reactivate_if_active(&snapshot.scheme)?;
    }
    if after == original {
        return Ok(());
    }
    let detail = if issues.is_empty() {
        String::new()
    } else {
        format!("; {}", issues.join("; "))
    };
    Err(AppError::fail(format!(
        "lid restoration did not verify (current AC={} DC={}, expected AC={} DC={}){detail}",
        after.0, after.1, snapshot.ac, snapshot.dc
    )))
}

fn validate_lid_enable_preflight(
    active: bool,
    current: (u32, u32),
    original: (u32, u32),
) -> Result<()> {
    if !active {
        Err(AppError::fail("active power scheme changed before enable"))
    } else if current != original {
        Err(AppError::fail("lid values changed before enable"))
    } else {
        Ok(())
    }
}

pub fn preflight_lid_enable(snapshot: &LidSnapshot) -> Result<()> {
    validate_lid_enable_preflight(
        scheme_is_active(&snapshot.scheme)?,
        read_lid_values(&snapshot.scheme)?,
        (snapshot.ac, snapshot.dc),
    )
}

fn enable_field(current: u32, original: u32) -> Result<bool> {
    if current == 0 {
        Ok(false)
    } else if current == original {
        Ok(true)
    } else {
        Err(AppError::fail("lid value changed before enable"))
    }
}

pub fn enable_lid(snapshot: &LidSnapshot) -> Result<()> {
    let result = (|| {
        if !scheme_is_active(&snapshot.scheme)? {
            return Err(AppError::fail("active power scheme changed before enable"));
        }
        let current = read_lid_values(&snapshot.scheme)?;
        let write_ac_value = enable_field(current.0, snapshot.ac)?;
        enable_field(current.1, snapshot.dc)?;
        if write_ac_value {
            write_ac(&snapshot.scheme, 0)?;
        }
        if !scheme_is_active(&snapshot.scheme)? {
            return Err(AppError::fail("active power scheme changed during enable"));
        }
        let current = read_lid_values(&snapshot.scheme)?;
        if current.0 != 0 {
            return Err(AppError::fail(
                "AC lid value did not remain at wake's value",
            ));
        }
        if enable_field(current.1, snapshot.dc)? {
            write_dc(&snapshot.scheme, 0)?;
        }
        reactivate_if_active(&snapshot.scheme)?;
        if !scheme_is_active(&snapshot.scheme)? || read_lid_values(&snapshot.scheme)? != (0, 0) {
            return Err(AppError::fail(
                "lid override did not verify before readiness",
            ));
        }
        Ok(())
    })();
    if let Err(error) = result {
        let rollback = restore_lid_snapshot(snapshot);
        return Err(match rollback {
            Ok(()) => error,
            Err(rollback) => AppError::fail(format!("{error}; rollback failed: {rollback}")),
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LidHealth {
    Healthy,
    DegradedScheme,
    DegradedValues,
}

pub fn lid_health(active_scheme_matches: bool, values: (u32, u32)) -> LidHealth {
    if !active_scheme_matches {
        LidHealth::DegradedScheme
    } else if values != (0, 0) {
        LidHealth::DegradedValues
    } else {
        LidHealth::Healthy
    }
}

fn power_error(action: &str, code: u32) -> AppError {
    AppError::fail(format!("could not {action} (error {code})"))
}

fn guid_eq(left: &GUID, right: &GUID) -> bool {
    left.data1 == right.data1
        && left.data2 == right.data2
        && left.data3 == right.data3
        && left.data4 == right.data4
}

pub fn format_guid(guid: &GUID) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        guid.data1,
        guid.data2,
        guid.data3,
        guid.data4[0],
        guid.data4[1],
        guid.data4[2],
        guid.data4[3],
        guid.data4[4],
        guid.data4[5],
        guid.data4[6],
        guid.data4[7]
    )
}

pub fn parse_guid(raw: &str) -> Result<GUID> {
    if !raw.is_ascii() || raw.len() != 36 {
        return Err(AppError::fail("invalid canonical power-scheme GUID"));
    }
    let hex = |range: std::ops::Range<usize>| {
        u32::from_str_radix(&raw[range], 16)
            .map_err(|_| AppError::fail("invalid canonical power-scheme GUID"))
    };
    let mut data4 = [0; 8];
    for (index, range) in [
        19..21,
        21..23,
        24..26,
        26..28,
        28..30,
        30..32,
        32..34,
        34..36,
    ]
    .into_iter()
    .enumerate()
    {
        data4[index] = hex(range)? as u8;
    }
    let guid = GUID {
        data1: hex(0..8)?,
        data2: hex(9..13)? as u16,
        data3: hex(14..18)? as u16,
        data4,
    };
    if format_guid(&guid) == raw {
        Ok(guid)
    } else {
        Err(AppError::fail("power-scheme GUID is not canonical"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn power(ac: u8, flags: u8, percent: u8) -> SYSTEM_POWER_STATUS {
        SYSTEM_POWER_STATUS {
            ACLineStatus: ac,
            BatteryFlag: flags,
            BatteryLifePercent: percent,
            ..SYSTEM_POWER_STATUS::default()
        }
    }

    #[test]
    fn system_power_status_mapping() {
        assert!(map_power_status(power(1, 0x08, 72)).unwrap().charging);
        assert!(map_power_status(power(0, 0x01, 41)).unwrap().discharging);
        assert!(map_power_status(power(1, 0x80, 50)).is_err());
        assert!(map_power_status(power(1, 0, u8::MAX)).is_err());
    }

    #[test]
    fn guid_round_trip_is_canonical() {
        let text = "381b4222-f694-41f0-9685-ff5bb260df2e";
        assert_eq!(format_guid(&parse_guid(text).unwrap()), text);
        assert!(parse_guid("381B4222-f694-41f0-9685-ff5bb260df2e").is_err());
    }

    #[test]
    fn partial_and_mixed_conflict_states_decide_each_rail() {
        use RestoreField::{Conflict, Keep, Write};
        for (current, original, expected) in [
            ((1, 2), (1, 2), (Keep, Keep)),
            ((0, 2), (1, 2), (Write, Keep)),
            ((1, 0), (1, 2), (Keep, Write)),
            ((0, 0), (1, 2), (Write, Write)),
            ((3, 0), (1, 2), (Conflict, Write)),
            ((0, 3), (1, 2), (Write, Conflict)),
            ((3, 4), (1, 2), (Conflict, Conflict)),
        ] {
            assert_eq!(
                (
                    restore_field(current.0, original.0),
                    restore_field(current.1, original.1),
                ),
                expected
            );
        }
    }

    #[test]
    fn restored_values_reactivate_only_the_still_active_scheme() {
        assert!(should_reactivate_lid(false, (1, 2), (1, 2), true));
        assert!(!should_reactivate_lid(false, (1, 2), (1, 2), false));
        assert!(!should_reactivate_lid(false, (3, 2), (1, 2), true));
        assert!(should_reactivate_lid(true, (3, 2), (1, 2), true));
        assert!(!should_reactivate_lid(true, (3, 2), (1, 2), false));
    }

    #[test]
    fn startup_preflight_rejects_changes_before_authority_publication() {
        assert!(validate_lid_enable_preflight(true, (1, 2), (1, 2)).is_ok());
        assert!(validate_lid_enable_preflight(false, (1, 2), (1, 2)).is_err());
        assert!(validate_lid_enable_preflight(true, (0, 2), (1, 2)).is_err());
        assert!(validate_lid_enable_preflight(true, (1, 0), (1, 2)).is_err());
    }

    #[test]
    fn scheme_switch_and_value_changes_degrade_health() {
        assert_eq!(lid_health(true, (0, 0)), LidHealth::Healthy);
        assert_eq!(lid_health(false, (0, 0)), LidHealth::DegradedScheme);
        assert_eq!(lid_health(true, (0, 1)), LidHealth::DegradedValues);
    }
}
