//! Session lock/unlock, display on/off and power-source/battery-level notifications (event-driven;
//! no polling).

use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::System::Power::{
    HPOWERNOTIFY, POWERBROADCAST_SETTING, RegisterPowerSettingNotification,
    UnregisterPowerSettingNotification,
};
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::System::SystemServices::{
    GUID_ACDC_POWER_SOURCE, GUID_BATTERY_PERCENTAGE_REMAINING, GUID_CONSOLE_DISPLAY_STATE,
};
use windows::Win32::UI::WindowsAndMessaging::DEVICE_NOTIFY_WINDOW_HANDLE;

pub const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
pub const WM_POWERBROADCAST: u32 = 0x0218;
pub const PBT_POWERSETTINGCHANGE: usize = 0x8013;
pub const WTS_SESSION_LOCK: usize = 7;
pub const WTS_SESSION_UNLOCK: usize = 8;

pub struct SessionWatch {
    hwnd: HWND,
    power: Vec<HPOWERNOTIFY>,
    wts: bool,
}

impl SessionWatch {
    pub fn register(hwnd: HWND) -> SessionWatch {
        let wts = unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) }.is_ok();
        // The display state, then the power source and battery level (for the charger banner).
        let wanted = [
            GUID_CONSOLE_DISPLAY_STATE,
            GUID_ACDC_POWER_SOURCE,
            GUID_BATTERY_PERCENTAGE_REMAINING,
        ];
        let power: Vec<HPOWERNOTIFY> = wanted
            .iter()
            .filter_map(|g| {
                unsafe {
                    RegisterPowerSettingNotification(HANDLE(hwnd.0), g, DEVICE_NOTIFY_WINDOW_HANDLE)
                }
                .ok()
            })
            .collect();
        if !wts || power.len() != wanted.len() {
            crate::warn!(
                "session notifications partially unavailable (wts={wts}, power settings={}/{})",
                power.len(),
                wanted.len()
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
            for p in self.power.drain(..) {
                let _ = UnregisterPowerSettingNotification(p);
            }
        }
    }
}

/// What a `PBT_POWERSETTINGCHANGE` message reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerSetting {
    /// `true` display on, `false` off / dimmed-off.
    Display(bool),
    /// The power source or the battery level changed (read the state with `GetSystemPowerStatus`).
    Battery,
}

/// Decode a `PBT_POWERSETTINGCHANGE` payload.
///
/// # Safety
/// `lparam` must be the `LPARAM` of a `WM_POWERBROADCAST` / `PBT_POWERSETTINGCHANGE` message.
pub unsafe fn power_setting_from(lparam: isize) -> Option<PowerSetting> {
    let s = lparam as *const POWERBROADCAST_SETTING;
    if s.is_null() {
        return None;
    }
    let s = unsafe { &*s };
    if s.PowerSetting == GUID_ACDC_POWER_SOURCE
        || s.PowerSetting == GUID_BATTERY_PERCENTAGE_REMAINING
    {
        return Some(PowerSetting::Battery);
    }
    if s.PowerSetting != GUID_CONSOLE_DISPLAY_STATE || s.DataLength < 1 {
        return None;
    }
    // 0 = off, 1 = on, 2 = dimmed.
    Some(PowerSetting::Display(s.Data[0] != 0))
}
