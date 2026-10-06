//! Single-instance guard. A second launch tells the first one (so a hidden tray icon can be brought
//! back) and exits.

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, LPARAM, WPARAM,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};
use windows::core::PCWSTR;

use super::util::wide;

pub const CONTROLLER_CLASS: &str = "SharkNotch.Controller";
/// Posted to the running instance by a second launch.
pub const WM_SECOND_INSTANCE: u32 = 0x8000 + 3; // WM_APP + 3

pub struct InstanceGuard(HANDLE);

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// `Some(guard)` if we are the first instance, `None` if another is running (it has been notified).
pub fn acquire(name: &str) -> Option<InstanceGuard> {
    let n = wide(&format!("Local\\{name}"));
    unsafe {
        match CreateMutexW(None, true, PCWSTR(n.as_ptr())) {
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    let _ = CloseHandle(h);
                    notify_existing();
                    None
                } else {
                    Some(InstanceGuard(h))
                }
            }
            Err(_) => Some(InstanceGuard(HANDLE::default())),
        }
    }
}

fn notify_existing() {
    let class = wide(CONTROLLER_CLASS);
    unsafe {
        if let Ok(hwnd) = FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()) {
            let _ = PostMessageW(Some(hwnd), WM_SECOND_INSTANCE, WPARAM(0), LPARAM(0));
        }
    }
}
