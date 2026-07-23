//! Native Windows sleep inhibition, battery status, and lid-close control.

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

pub fn supports_even_lid() -> bool {
    true
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
        // SAFETY: ES_CONTINUOUS clears this thread's previous requirements.
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
    }
}

pub fn read_battery() -> Result<BatteryStatus> {
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: `status` is a valid writable SYSTEM_POWER_STATUS.
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
            return Err(AppError::fail(format!(
                "could not read the active power scheme (error {code})"
            )));
        }
        let scheme = *raw;
        LocalFree(raw.cast());
        Ok(scheme)
    }
}

pub fn read_lid_values(scheme: &GUID) -> Result<(u32, u32)> {
    let mut ac = 0;
    let mut dc = 0;
    // SAFETY: All GUID pointers and output pointers remain valid for each call.
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

pub fn write_lid_ac(scheme: &GUID, value: u32) -> Result<()> {
    // SAFETY: All GUID pointers remain valid for the call.
    let code = unsafe {
        PowerWriteACValueIndex(
            std::ptr::null_mut(),
            scheme,
            &SUB_BUTTONS,
            &LID_ACTION,
            value,
        )
    };
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(power_error("write the AC lid action", code))
    }
}

pub fn write_lid_dc(scheme: &GUID, value: u32) -> Result<()> {
    // SAFETY: All GUID pointers remain valid for the call.
    let code = unsafe {
        PowerWriteDCValueIndex(
            std::ptr::null_mut(),
            scheme,
            &SUB_BUTTONS,
            &LID_ACTION,
            value,
        )
    };
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(power_error("write the DC lid action", code))
    }
}

pub fn reactivate_if_still_active(scheme: &GUID) -> Result<()> {
    if !guid_eq(&active_scheme()?, scheme) {
        return Ok(());
    }
    // SAFETY: `scheme` is a valid GUID for the duration of the call.
    let code = unsafe { PowerSetActiveScheme(std::ptr::null_mut(), scheme) };
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(power_error("reactivate the power scheme", code))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreDecision {
    AlreadyRestored,
    RestoreAppliedValues,
    Conflict,
}

pub fn restoration_decision(current: (u32, u32), original: (u32, u32)) -> RestoreDecision {
    if current == original {
        RestoreDecision::AlreadyRestored
    } else if current == (0, 0) {
        RestoreDecision::RestoreAppliedValues
    } else {
        RestoreDecision::Conflict
    }
}

pub fn restore_lid_snapshot(snapshot: &LidSnapshot) -> Result<()> {
    let current = read_lid_values(&snapshot.scheme)?;
    match restoration_decision(current, (snapshot.ac, snapshot.dc)) {
        RestoreDecision::AlreadyRestored => return Ok(()),
        RestoreDecision::Conflict => {
            return Err(AppError::fail(format!(
                "lid action changed after wake enabled it (current AC={} DC={}, original AC={} DC={}); refusing to overwrite it",
                current.0, current.1, snapshot.ac, snapshot.dc
            )));
        }
        RestoreDecision::RestoreAppliedValues => {}
    }
    write_lid_ac(&snapshot.scheme, snapshot.ac)?;
    let after_ac = read_lid_values(&snapshot.scheme)?;
    if after_ac != (snapshot.ac, 0) {
        return Err(AppError::fail(format!(
            "lid values changed during restoration (current AC={} DC={}); refusing the DC write",
            after_ac.0, after_ac.1
        )));
    }
    write_lid_dc(&snapshot.scheme, snapshot.dc)?;
    reactivate_if_still_active(&snapshot.scheme)?;
    let after = read_lid_values(&snapshot.scheme)?;
    if after != (snapshot.ac, snapshot.dc) {
        return Err(AppError::fail(format!(
            "lid restoration did not verify (current AC={} DC={}, expected AC={} DC={})",
            after.0, after.1, snapshot.ac, snapshot.dc
        )));
    }
    Ok(())
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
    if !raw.is_ascii()
        || raw.len() != 36
        || raw.as_bytes().get(8) != Some(&b'-')
        || raw.as_bytes().get(13) != Some(&b'-')
        || raw.as_bytes().get(18) != Some(&b'-')
        || raw.as_bytes().get(23) != Some(&b'-')
    {
        return Err(AppError::fail("invalid canonical power-scheme GUID"));
    }
    let hex = |range: std::ops::Range<usize>| {
        u32::from_str_radix(&raw[range], 16)
            .map_err(|_| AppError::fail("invalid canonical power-scheme GUID"))
    };
    let mut data4 = [0u8; 8];
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
    if format_guid(&guid) != raw {
        return Err(AppError::fail("power-scheme GUID is not canonical"));
    }
    Ok(guid)
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
        let charging = map_power_status(power(1, 0x08, 72)).unwrap();
        assert_eq!(charging.percent, 72);
        assert!(charging.charging);
        assert!(!charging.discharging);

        let discharging = map_power_status(power(0, 0x01, 41)).unwrap();
        assert!(!discharging.charging);
        assert!(discharging.discharging);

        let neutral = map_power_status(power(1, 0x02, 100)).unwrap();
        assert!(!neutral.charging);
        assert!(!neutral.discharging);
        assert!(neutral.neutral_state.is_some());

        for invalid in [
            power(1, 0x80, 50),
            power(1, u8::MAX, 50),
            power(1, 0, u8::MAX),
        ] {
            assert!(map_power_status(invalid).is_err());
        }
    }

    #[test]
    fn guid_round_trip_is_canonical() {
        let text = "381b4222-f694-41f0-9685-ff5bb260df2e";
        assert_eq!(format_guid(&parse_guid(text).unwrap()), text);
        for invalid in [
            "{381b4222-f694-41f0-9685-ff5bb260df2e}",
            "381B4222-f694-41f0-9685-ff5bb260df2e",
            "381b4222-f694-41f0-9685-ff5bb260df2g",
        ] {
            assert!(parse_guid(invalid).is_err(), "guid={invalid}");
        }
    }

    #[test]
    fn restoration_decision_never_clobbers_a_third_party_value() {
        assert_eq!(
            restoration_decision((1, 2), (1, 2)),
            RestoreDecision::AlreadyRestored
        );
        assert_eq!(
            restoration_decision((0, 0), (0, 0)),
            RestoreDecision::AlreadyRestored
        );
        assert_eq!(
            restoration_decision((0, 0), (1, 2)),
            RestoreDecision::RestoreAppliedValues
        );
        assert_eq!(
            restoration_decision((0, 2), (1, 2)),
            RestoreDecision::Conflict
        );
        assert_eq!(
            restoration_decision((3, 3), (1, 2)),
            RestoreDecision::Conflict
        );
    }
}
