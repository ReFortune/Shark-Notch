//! Session lock/unlock and display on/off notifications (event-driven; no polling).

use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::System::Power::{
    POWERBROADCAST_SETTING, RegisterPowerSettingNotification, UnregisterPowerSettingNotification,
};
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
use windows::Win32::UI::WindowsAndMessaging::DEVICE_NOTIFY_WINDOW_HANDLE;

pub const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
pub const WM_POWERBROADCAST: u32 = 0x0218;
pub const PBT_POWERSETTINGCHANGE: usize = 0x8013;
pub const WTS_SESSION_LOCK: usize = 7;
pub const WTS_SESSION_UNLOCK: usize = 8;

pub struct SessionWatch {
    hwnd: HWND,
    power: Option<windows::Win32::System::Power::HPOWERNOTIFY>,
    wts: bool,
}

impl SessionWatch {
    pub fn register(hwnd: HWND) -> SessionWatch {
        let wts = unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) }.is_ok();
        let power = unsafe {
            RegisterPowerSettingNotification(
                HANDLE(hwnd.0),
                &GUID_CONSOLE_DISPLAY_STATE,
                DEVICE_NOTIFY_WINDOW_HANDLE,
            )
        }
        .ok();
        if !wts || power.is_none() {
            crate::warn!(
                "session notifications partially unavailable (wts={wts}, display-state={})",
                power.is_some()
            );
        }
        SessionWatch { hwnd, power, wts }
    }
}

impl Drop for SessionWatch {
    fn drop(&mut self) {
        unsafe {
            if self.wts {
                let _ = WTSUnRegisterSessionNotification(self.hwnd);
            }
            if let Some(p) = self.power.take() {
                let _ = UnregisterPowerSettingNotification(p);
            }
        }
    }
}

/// Decode a `PBT_POWERSETTINGCHANGE` payload: `Some(true)` display on, `Some(false)` off/dimmed-off.
///
/// # Safety
/// `lparam` must be the `LPARAM` of a `WM_POWERBROADCAST` / `PBT_POWERSETTINGCHANGE` message.
pub unsafe fn display_state_from(lparam: isize) -> Option<bool> {
    let s = lparam as *const POWERBROADCAST_SETTING;
    if s.is_null() {
        return None;
    }
    let s = unsafe { &*s };
    if s.PowerSetting != GUID_CONSOLE_DISPLAY_STATE || s.DataLength < 1 {
        return None;
    }
    // 0 = off, 1 = on, 2 = dimmed.
    Some(s.Data[0] != 0)
}
