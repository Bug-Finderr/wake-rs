//! Windows power requests, battery status, and the temporary power-plan lid override.

use super::KeepAwake;
use crate::error::{AppError, Result};
use crate::run::{BatteryStatus, Mode};
use base64::Engine;
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

const POWERSHELL_MISSING: &str =
    "powershell not found on PATH; wake requires Windows PowerShell on Windows";
#[allow(dead_code)] // part of the platform surface; the picker is gated to unix
pub fn supports_interactive() -> bool {
    false
}

pub fn supports_even_lid() -> bool {
    true
}

pub struct Inhibitor {
    handle: windows_sys::Win32::Foundation::HANDLE,
    system: bool,
    display: bool,
}

impl Inhibitor {
    pub fn start(mode: Mode) -> Result<Self> {
        let mut reason = "wake CLI\0".encode_utf16().collect::<Vec<_>>();
        let context = REASON_CONTEXT {
            Version: POWER_REQUEST_CONTEXT_VERSION,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: reason.as_mut_ptr(),
            },
        };
        // SAFETY: `context` and its NUL-terminated reason buffer remain valid for the call. The
        // returned handle is owned here and closed on every failure path or by Drop.
        let handle = unsafe { PowerCreateRequest(&context) };
        if handle == INVALID_HANDLE_VALUE {
            // SAFETY: this reads the calling thread's error code immediately after the failed call.
            let code = unsafe { GetLastError() };
            return Err(win_error("could not create power request", code));
        }

        // SAFETY: `handle` is a valid owned power-request handle.
        if unsafe { PowerSetRequest(handle, PowerRequestSystemRequired) } == 0 {
            // SAFETY: capture the error before closing the handle, which may overwrite it.
            let code = unsafe { GetLastError() };
            // SAFETY: the valid owned handle has no successful requests to clear.
            unsafe { CloseHandle(handle) };
            return Err(win_error("could not prevent system sleep", code));
        }

        let display = mode == Mode::DisplaySystem;
        // SAFETY: the same valid handle can own one request of each distinct type.
        if display && unsafe { PowerSetRequest(handle, PowerRequestDisplayRequired) } == 0 {
            // SAFETY: capture the error before cleanup, then clear exactly the successful request.
            let code = unsafe { GetLastError() };
            unsafe {
                PowerClearRequest(handle, PowerRequestSystemRequired);
                CloseHandle(handle);
            }
            return Err(win_error("could not prevent display sleep", code));
        }

        Ok(Self {
            handle,
            system: true,
            display,
        })
    }

    pub fn note(&self) -> Option<&str> {
        None
    }

    pub fn alive(&mut self) -> bool {
        self.handle != INVALID_HANDLE_VALUE && !self.handle.is_null()
    }
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        // SAFETY: each flag records one successful PowerSetRequest. Each is cleared once, then the
        // owned valid handle is closed once.
        unsafe {
            if self.display {
                PowerClearRequest(self.handle, PowerRequestDisplayRequired);
            }
            if self.system {
                PowerClearRequest(self.handle, PowerRequestSystemRequired);
            }
            CloseHandle(self.handle);
        }
    }
}

fn win_error(action: &str, code: u32) -> AppError {
    AppError::fail(format!(
        "{action}: {}",
        std::io::Error::from_raw_os_error(code as i32)
    ))
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
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: `status` is a valid writable struct for the duration of the call.
    if unsafe { GetSystemPowerStatus(&mut status) } == 0 {
        return Err(AppError::fail(format!(
            "could not read battery status: {}",
            std::io::Error::last_os_error()
        )));
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

// Power requests cannot override the lid-close action, so the legacy --even-lid path changes the
// active power plan through an elevated helper.

/// Pack the AC and DC lid actions into the session's `prior_disable_sleep` (`i32`) field. Each value
/// is 0..=3, so a nibble each is plenty.
pub fn encode_lid(ac: u32, dc: u32) -> i32 {
    ac as i32 | ((dc as i32) << 4)
}

/// Inverse of [`encode_lid`].
pub fn decode_lid(v: i32) -> (u32, u32) {
    ((v & 0xF) as u32, ((v >> 4) & 0xF) as u32)
}

/// Read the active power plan's AC and DC lid-close actions. Unprivileged.
pub fn read_lid_action() -> Result<(u32, u32)> {
    // SAFETY: FFI into powrprof. `PowerGetActiveScheme` allocates a GUID we must `LocalFree`. The
    // read calls only borrow `scheme`/our stack `out` for their duration.
    unsafe {
        let mut scheme: *mut GUID = std::ptr::null_mut();
        if PowerGetActiveScheme(std::ptr::null_mut(), &mut scheme) != ERROR_SUCCESS {
            return Err(AppError::fail("could not read the active power scheme"));
        }
        let result = (|| {
            let mut ac: u32 = 0;
            let mut dc: u32 = 0;
            if PowerReadACValueIndex(
                std::ptr::null_mut(),
                scheme,
                &GUID_SYSTEM_BUTTON_SUBGROUP,
                &GUID_LIDCLOSE_ACTION,
                &mut ac,
            ) != ERROR_SUCCESS
            {
                return Err(AppError::fail("could not read the AC lid action"));
            }
            if PowerReadDCValueIndex(
                std::ptr::null_mut(),
                scheme,
                &GUID_SYSTEM_BUTTON_SUBGROUP,
                &GUID_LIDCLOSE_ACTION,
                &mut dc,
            ) != ERROR_SUCCESS
            {
                return Err(AppError::fail("could not read the DC lid action"));
            }
            Ok((ac, dc))
        })();
        LocalFree(scheme.cast());
        result
    }
}

/// Set the active power plan's AC and DC lid-close actions, then re-activate the scheme so the
/// change takes effect. Requires administrator rights.
pub fn write_lid_action(ac: u32, dc: u32) -> Result<()> {
    // SAFETY: FFI into powrprof. `PowerGetActiveScheme` allocates a GUID we must `LocalFree`; the
    // write/set calls only borrow `scheme` for their duration.
    unsafe {
        let mut scheme: *mut GUID = std::ptr::null_mut();
        if PowerGetActiveScheme(std::ptr::null_mut(), &mut scheme) != ERROR_SUCCESS {
            return Err(AppError::fail("could not read the active power scheme"));
        }
        let result = (|| {
            // ERROR_ACCESS_DENIED is the common case (not elevated); any failure here means the
            // write did not take, so report the same admin-rights guidance regardless of `rc`.
            let denied = || AppError::fail("setting the lid action requires administrator rights");
            if PowerWriteACValueIndex(
                std::ptr::null_mut(),
                scheme,
                &GUID_SYSTEM_BUTTON_SUBGROUP,
                &GUID_LIDCLOSE_ACTION,
                ac,
            ) != ERROR_SUCCESS
            {
                return Err(denied());
            }
            if PowerWriteDCValueIndex(
                std::ptr::null_mut(),
                scheme,
                &GUID_SYSTEM_BUTTON_SUBGROUP,
                &GUID_LIDCLOSE_ACTION,
                dc,
            ) != ERROR_SUCCESS
            {
                return Err(denied());
            }
            if PowerSetActiveScheme(std::ptr::null_mut(), scheme) != ERROR_SUCCESS {
                return Err(denied());
            }
            Ok(())
        })();
        LocalFree(scheme.cast());
        result
    }
}

fn resolve_powershell() -> Result<String> {
    super::resolve_on_path("powershell.exe", POWERSHELL_MISSING)
        .or_else(|_| super::resolve_on_path("powershell", POWERSHELL_MISSING))
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
    fn lid_encode_roundtrip_all_combos() {
        for ac in 0..=3u32 {
            for dc in 0..=3u32 {
                assert_eq!(decode_lid(encode_lid(ac, dc)), (ac, dc));
            }
        }
    }
}
