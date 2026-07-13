use crate::error::{AppError, Result};
use crate::run::{BatteryStatus, Mode};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_SUCCESS, GetLastError, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::System::Power::{
    GetSystemPowerStatus, PowerClearRequest, PowerCreateRequest, PowerGetActiveScheme,
    PowerReadACValueIndex, PowerReadDCValueIndex, PowerRequestDisplayRequired,
    PowerRequestSystemRequired, PowerSetActiveScheme, PowerSetRequest, PowerWriteACValueIndex,
    PowerWriteDCValueIndex, SYSTEM_POWER_STATUS,
};
use windows_sys::Win32::System::SystemServices::{
    GUID_LIDCLOSE_ACTION, GUID_SYSTEM_BUTTON_SUBGROUP, POWER_REQUEST_CONTEXT_VERSION,
};
use windows_sys::Win32::System::Threading::{
    POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
};
use windows_sys::core::GUID;

pub struct Inhibitor {
    handle: windows_sys::Win32::Foundation::HANDLE,
    display: bool,
}

impl Inhibitor {
    pub fn start(mode: Mode, _even_lid: bool) -> Result<Self> {
        let mut reason = "wake CLI\0".encode_utf16().collect::<Vec<_>>();
        let context = REASON_CONTEXT {
            Version: POWER_REQUEST_CONTEXT_VERSION,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: reason.as_mut_ptr(),
            },
        };
        // SAFETY: the context and NUL-terminated reason remain valid for this call.
        let handle = unsafe { PowerCreateRequest(&context) };
        if handle == INVALID_HANDLE_VALUE {
            // SAFETY: reads this thread's error immediately after the failed call.
            return Err(last_error("could not create power request", unsafe {
                GetLastError()
            }));
        }
        // SAFETY: handle is an owned power-request handle.
        if unsafe { PowerSetRequest(handle, PowerRequestSystemRequired) } == 0 {
            // SAFETY: error is read before the owned handle is closed.
            let error = last_error("could not prevent system sleep", unsafe { GetLastError() });
            // SAFETY: closes the valid owned handle once.
            unsafe { CloseHandle(handle) };
            return Err(error);
        }
        let display = mode == Mode::DisplaySystem;
        // SAFETY: the handle may own one request of each distinct type.
        if display && unsafe { PowerSetRequest(handle, PowerRequestDisplayRequired) } == 0 {
            // SAFETY: error is captured before clearing and closing the valid handle.
            let error = last_error("could not prevent display sleep", unsafe { GetLastError() });
            // SAFETY: clears the one successful request and closes the handle once.
            unsafe {
                PowerClearRequest(handle, PowerRequestSystemRequired);
                CloseHandle(handle);
            }
            return Err(error);
        }
        Ok(Self { handle, display })
    }

    pub fn note(&self) -> Option<&str> {
        None
    }

    pub fn alive(&mut self) -> bool {
        self.handle != INVALID_HANDLE_VALUE && !self.handle.is_null()
    }
}

pub fn inhibitor_startup_error(_even_lid: bool) -> AppError {
    AppError::inhibitor_startup("sleep inhibitor exited during startup")
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        // SAFETY: flags correspond to successful requests; the owned handle is closed once.
        unsafe {
            if self.display {
                PowerClearRequest(self.handle, PowerRequestDisplayRequired);
            }
            PowerClearRequest(self.handle, PowerRequestSystemRequired);
            CloseHandle(self.handle);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LidSnapshot {
    pub scheme_guid: String,
    pub ac_action: u32,
    pub dc_action: u32,
}

impl LidSnapshot {
    pub fn new(scheme_guid: String, ac_action: u32, dc_action: u32) -> Result<Self> {
        let scheme_guid = format_guid(&parse_guid(&scheme_guid)?);
        if !(0..=3).contains(&ac_action) || !(0..=3).contains(&dc_action) {
            return Err(AppError::fail("lid actions must be in 0..=3"));
        }
        Ok(Self {
            scheme_guid,
            ac_action,
            dc_action,
        })
    }

    fn scheme(&self) -> Result<GUID> {
        parse_guid(&self.scheme_guid)
    }
}

pub fn read_lid_snapshot() -> Result<LidSnapshot> {
    let scheme = active_scheme()?;
    let (ac_action, dc_action) = read_lid_values(&scheme)?;
    LidSnapshot::new(format_guid(&scheme), ac_action, dc_action)
}

pub fn disable_lid(snapshot: &LidSnapshot) -> Result<()> {
    let scheme = snapshot.scheme()?;
    if !lid_snapshot_matches(snapshot)? {
        return Err(AppError::fail(
            "power configuration changed before the lid override",
        ));
    }
    write_ac(&scheme, 0)?;
    write_dc(&scheme, 0)?;
    apply_if_active(&scheme, "lid override")?;
    verify_lid_values(&scheme, 0, 0, "enable")
}

pub fn restore_lid(snapshot: &LidSnapshot) -> Result<()> {
    let scheme = snapshot.scheme()?;
    let (ac, dc) = restore_values(snapshot, read_lid_values(&scheme)?);
    if let Some(value) = ac {
        write_ac(&scheme, value)?;
    }
    if let Some(value) = dc {
        write_dc(&scheme, value)?;
    }
    if ac.is_some() || dc.is_some() {
        let active = active_scheme()?;
        if guid_eq(&scheme, &active) {
            apply_if_active(&scheme, "lid restoration")?;
        }
    }
    if lid_is_restored(snapshot)? {
        Ok(())
    } else {
        Err(AppError::fail(
            "failed to remove the wake-owned lid override",
        ))
    }
}

pub fn lid_override_is_active(snapshot: &LidSnapshot) -> Result<bool> {
    let scheme = snapshot.scheme()?;
    if !guid_eq(&scheme, &active_scheme()?) {
        return Ok(false);
    }
    let (ac, dc) = read_lid_values(&scheme)?;
    Ok((snapshot.ac_action == 0 || ac == 0) && (snapshot.dc_action == 0 || dc == 0))
}

pub fn lid_is_restored(snapshot: &LidSnapshot) -> Result<bool> {
    let scheme = snapshot.scheme()?;
    Ok(restore_values(snapshot, read_lid_values(&scheme)?) == (None, None))
}

pub fn lid_snapshot_matches(snapshot: &LidSnapshot) -> Result<bool> {
    let scheme = snapshot.scheme()?;
    Ok(guid_eq(&scheme, &active_scheme()?)
        && read_lid_values(&scheme)? == (snapshot.ac_action, snapshot.dc_action))
}

fn restore_values(snapshot: &LidSnapshot, current: (u32, u32)) -> (Option<u32>, Option<u32>) {
    let restore = |original, current| (original != 0 && current == 0).then_some(original);
    (
        restore(snapshot.ac_action, current.0),
        restore(snapshot.dc_action, current.1),
    )
}

pub fn set_current_lid_actions(ac: u32, dc: u32) -> Result<()> {
    if !(0..=3).contains(&ac) || !(0..=3).contains(&dc) {
        return Err(AppError::fail("lid actions must be in 0..=3"));
    }
    let scheme = active_scheme()?;
    write_ac(&scheme, ac)?;
    write_dc(&scheme, dc)?;
    apply_if_active(&scheme, "lid action update")?;
    verify_lid_values(&scheme, ac, dc, "set")
}

fn active_scheme() -> Result<GUID> {
    let mut pointer = std::ptr::null_mut();
    // SAFETY: receives an allocated GUID pointer that is copied and freed on every non-null path.
    let code = unsafe { PowerGetActiveScheme(std::ptr::null_mut(), &mut pointer) };
    if code != ERROR_SUCCESS {
        if !pointer.is_null() {
            // SAFETY: a non-null error result is still the allocation returned by the API.
            unsafe { LocalFree(pointer.cast()) };
        }
        return Err(power_error("could not read active power scheme", code));
    }
    if pointer.is_null() {
        return Err(AppError::fail(
            "could not read active power scheme: null result",
        ));
    }
    // SAFETY: successful PowerGetActiveScheme returned a non-null GUID allocation.
    let scheme = unsafe { *pointer };
    // SAFETY: pointer is the allocation returned by PowerGetActiveScheme and is freed once.
    unsafe { LocalFree(pointer.cast()) };
    Ok(scheme)
}

fn read_lid_values(scheme: &GUID) -> Result<(u32, u32)> {
    let mut ac = 0;
    let mut dc = 0;
    // SAFETY: all GUID pointers and output pointers remain valid for each synchronous call.
    let code = unsafe {
        PowerReadACValueIndex(
            std::ptr::null_mut(),
            scheme,
            &GUID_SYSTEM_BUTTON_SUBGROUP,
            &GUID_LIDCLOSE_ACTION,
            &mut ac,
        )
    };
    if code != ERROR_SUCCESS {
        return Err(power_error("could not read AC lid action", code));
    }
    // SAFETY: all GUID pointers and the output pointer remain valid for the synchronous call.
    let code = unsafe {
        PowerReadDCValueIndex(
            std::ptr::null_mut(),
            scheme,
            &GUID_SYSTEM_BUTTON_SUBGROUP,
            &GUID_LIDCLOSE_ACTION,
            &mut dc,
        )
    };
    if code != ERROR_SUCCESS {
        return Err(power_error("could not read DC lid action", code));
    }
    Ok((ac, dc))
}

fn write_ac(scheme: &GUID, value: u32) -> Result<()> {
    // SAFETY: GUID pointers remain valid for the synchronous write.
    let code = unsafe {
        PowerWriteACValueIndex(
            std::ptr::null_mut(),
            scheme,
            &GUID_SYSTEM_BUTTON_SUBGROUP,
            &GUID_LIDCLOSE_ACTION,
            value,
        )
    };
    (code == ERROR_SUCCESS)
        .then_some(())
        .ok_or_else(|| power_error("could not write AC lid action", code))
}

fn write_dc(scheme: &GUID, value: u32) -> Result<()> {
    // SAFETY: GUID pointers remain valid for the synchronous write.
    let code = unsafe {
        PowerWriteDCValueIndex(
            std::ptr::null_mut(),
            scheme,
            &GUID_SYSTEM_BUTTON_SUBGROUP,
            &GUID_LIDCLOSE_ACTION,
            value,
        )
    };
    (code == ERROR_SUCCESS)
        .then_some(())
        .ok_or_else(|| power_error("could not write DC lid action", code))
}

fn set_active(scheme: &GUID) -> Result<()> {
    // SAFETY: scheme points to a valid GUID for the synchronous call.
    let code = unsafe { PowerSetActiveScheme(std::ptr::null_mut(), scheme) };
    (code == ERROR_SUCCESS)
        .then_some(())
        .ok_or_else(|| power_error("could not apply lid action", code))
}

fn apply_if_active(scheme: &GUID, action: &str) -> Result<()> {
    if !guid_eq(&active_scheme()?, scheme) {
        return Err(AppError::fail(format!(
            "active power scheme changed before {action}"
        )));
    }
    set_active(scheme)?;
    if !guid_eq(&active_scheme()?, scheme) {
        return Err(AppError::fail(format!(
            "active power scheme changed while applying {action}"
        )));
    }
    Ok(())
}

fn verify_lid_values(scheme: &GUID, ac: u32, dc: u32, action: &str) -> Result<()> {
    let actual = read_lid_values(scheme)?;
    if actual == (ac, dc) {
        Ok(())
    } else {
        Err(AppError::fail(format!(
            "failed to {action} lid action: expected AC={ac} DC={dc}, found AC={} DC={}",
            actual.0, actual.1
        )))
    }
}

fn parse_guid(value: &str) -> Result<GUID> {
    if value.len() != 36
        || value.bytes().enumerate().any(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte != b'-',
            _ => !byte.is_ascii_hexdigit(),
        })
    {
        return Err(AppError::fail(format!(
            "invalid power scheme GUID: {value}"
        )));
    }
    u128::from_str_radix(&value.replace('-', ""), 16)
        .map(GUID::from_u128)
        .map_err(|_| AppError::fail(format!("invalid power scheme GUID: {value}")))
}

fn format_guid(value: &GUID) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        value.data1,
        value.data2,
        value.data3,
        value.data4[0],
        value.data4[1],
        value.data4[2],
        value.data4[3],
        value.data4[4],
        value.data4[5],
        value.data4[6],
        value.data4[7]
    )
}

fn guid_eq(left: &GUID, right: &GUID) -> bool {
    left.data1 == right.data1
        && left.data2 == right.data2
        && left.data3 == right.data3
        && left.data4 == right.data4
}

pub fn read_battery() -> Result<BatteryStatus> {
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: status is a valid writable struct for the synchronous call.
    if unsafe { GetSystemPowerStatus(&mut status) } == 0 {
        // SAFETY: reads this thread's error immediately after the failed call.
        let code = unsafe { GetLastError() };
        return Err(last_error("could not read battery status", code));
    }
    battery_from_values(
        status.ACLineStatus,
        status.BatteryFlag,
        status.BatteryLifePercent,
    )
}

fn battery_from_values(ac: u8, flags: u8, percent: u8) -> Result<BatteryStatus> {
    if ac == u8::MAX || flags == u8::MAX || flags & 128 != 0 || percent > 100 {
        return Err(AppError::fail("no usable battery found"));
    }
    let charging = flags & 8 != 0;
    let discharging = !charging && ac == 0;
    Ok(BatteryStatus {
        percent: i32::from(percent),
        charging,
        discharging,
        neutral_state: (!charging && !discharging).then(|| "not charging or discharging".into()),
    })
}

fn power_error(action: &str, code: u32) -> AppError {
    AppError::fail(format!(
        "{action} (Windows code {code}): {}",
        std::io::Error::from_raw_os_error(code as i32)
    ))
}

fn last_error(action: &str, code: u32) -> AppError {
    power_error(action, code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_status_classification_table() {
        let cases = [
            (0, 0, 75, (false, true, None)),
            (1, 8, 50, (true, false, None)),
            (
                1,
                0,
                100,
                (false, false, Some("not charging or discharging")),
            ),
        ];
        for (ac, flags, percent, expected) in cases {
            let status = battery_from_values(ac, flags, percent).unwrap();
            assert_eq!(
                (
                    status.charging,
                    status.discharging,
                    status.neutral_state.as_deref()
                ),
                expected
            );
        }
        assert!(battery_from_values(1, 128, 255).is_err());
        assert!(battery_from_values(255, 0, 50).is_err());
        assert!(battery_from_values(1, 8, 101).is_err());
    }

    #[test]
    fn canonical_guid_round_trips() {
        let text = "381b4222-f694-41f0-9685-ff5bb260df2e";
        let guid = parse_guid(text).unwrap();
        assert_eq!(format_guid(&guid), text);
        for invalid in [
            "381b4222f69441f09685ff5bb260df2e",
            "381b4222-f694-41f0-9685-ff5bb260df2z",
            "{381b4222-f694-41f0-9685-ff5bb260df2e}",
        ] {
            assert!(parse_guid(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn lid_snapshot_is_canonical_and_validated() {
        let snapshot =
            LidSnapshot::new("381B4222-F694-41F0-9685-FF5BB260DF2E".into(), 1, 2).unwrap();
        assert_eq!(snapshot.scheme_guid, "381b4222-f694-41f0-9685-ff5bb260df2e");
        assert!(LidSnapshot::new(snapshot.scheme_guid.clone(), 4, 0).is_err());
    }

    #[test]
    fn restoration_changes_only_still_owned_fields() {
        let saved = LidSnapshot::new("381b4222-f694-41f0-9685-ff5bb260df2e".into(), 1, 2).unwrap();
        assert_eq!(restore_values(&saved, (0, 3)), (Some(1), None));
        assert_eq!(restore_values(&saved, (3, 0)), (None, Some(2)));
        assert_eq!(restore_values(&saved, (3, 3)), (None, None));
    }

    #[test]
    fn original_override_has_no_restore_intent() {
        let saved = LidSnapshot::new("381b4222-f694-41f0-9685-ff5bb260df2e".into(), 0, 2).unwrap();
        assert_eq!(restore_values(&saved, (0, 0)), (None, Some(2)));
    }
}
