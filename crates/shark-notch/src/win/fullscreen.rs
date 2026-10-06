//! Fullscreen / game detection **without polling or injection**.
//!
//! * `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` with `WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS`:
//!   the callback runs on *our* thread via our message loop; nothing is loaded into other processes.
//! * A second hook for `EVENT_OBJECT_LOCATIONCHANGE`, scoped to the foreground window's process and
//!   thread (re-registered on every foreground change), catches in-place transitions such as F11 or
//!   a video player going fullscreen.
//! * The decision itself is pure (`notch_core::fullscreen`); this file only gathers the facts, with
//!   `SHQueryUserNotificationState` as the secondary signal.
//!
//! We never `OpenProcess` the foreground application and never touch its input.

use notch_core::fullscreen::{IRect, UserNotifState, WindowFacts, is_fullscreen};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::Shell::SHQueryUserNotificationState;
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_FOREGROUND, GWL_STYLE, GetClassNameW,
    GetForegroundWindow, GetShellWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS,
};

use super::util::from_wide;

/// Window classes that cover the screen but are shell chrome, not a game.
const SHELL_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "MultitaskingViewFrame",
    "XamlExplorerHostIslandWindow",
    "Windows.UI.Core.CoreWindow",
    "NotifyIconOverflowWindow",
    "TopLevelWindowForOverflowXamlIsland",
    "ForegroundStaging",
    "LockScreenBackstopFrame",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    pub fullscreen: bool,
    /// Monitor rectangle the (full-screen) window is on.
    pub monitor: Option<IRect>,
}

fn irect(r: RECT) -> IRect {
    IRect::new(r.left, r.top, r.right, r.bottom)
}

fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    from_wide(&buf[..n.max(0) as usize])
}

/// Gather the facts about `hwnd` and decide.
pub fn probe(hwnd: HWND) -> Probe {
    let none = Probe {
        fullscreen: false,
        monitor: None,
    };
    if hwnd.0.is_null() {
        return none;
    }
    unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return none;
        }
        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
            return none;
        }
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4);
        let class = class_of(hwnd);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let facts = WindowFacts {
            rect: irect(rect),
            monitor: irect(mi.rcMonitor),
            has_caption: style & 0x00C0_0000 == 0x00C0_0000,
            is_shell_or_own: hwnd == GetShellWindow()
                || SHELL_CLASSES.contains(&class.as_str())
                || pid == std::process::id(),
            is_visible: IsWindowVisible(hwnd).as_bool(),
            is_minimized: IsIconic(hwnd).as_bool(),
            is_cloaked: cloaked != 0,
        };
        let quns = SHQueryUserNotificationState()
            .map(|s| UserNotifState::from_raw(s.0))
            .unwrap_or(UserNotifState::Unknown);
        let fullscreen = is_fullscreen(&facts, quns);
        Probe {
            fullscreen,
            monitor: fullscreen.then_some(facts.monitor),
        }
    }
}

pub fn probe_foreground() -> Probe {
    probe(unsafe { GetForegroundWindow() })
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    crate::app::on_win_event(event, hwnd, id_object, id_child);
}

/// Owns the two hooks. Dropping it unhooks.
#[derive(Default)]
pub struct Watcher {
    fg_hook: Option<HWINEVENTHOOK>,
    loc_hook: Option<HWINEVENTHOOK>,
    pub tracked: HWND,
}

impl Watcher {
    pub fn install(&mut self) {
        if self.fg_hook.is_some() {
            return;
        }
        let h = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(win_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        if h.0.is_null() {
            crate::warn!("SetWinEventHook(foreground) failed");
        } else {
            self.fg_hook = Some(h);
        }
    }

    /// Follow `hwnd`'s location changes (only its own process/thread are delivered to us).
    pub fn track(&mut self, hwnd: HWND) {
        if let Some(h) = self.loc_hook.take() {
            unsafe {
                let _ = UnhookWinEvent(h);
            }
        }
        self.tracked = hwnd;
        if hwnd.0.is_null() {
            return;
        }
        unsafe {
            let mut pid = 0u32;
            let tid = GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if tid == 0 {
                return;
            }
            let h = SetWinEventHook(
                EVENT_OBJECT_LOCATIONCHANGE,
                EVENT_OBJECT_LOCATIONCHANGE,
                None,
                Some(win_event_proc),
                pid,
                tid,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
            if !h.0.is_null() {
                self.loc_hook = Some(h);
            }
        }
    }

    pub fn uninstall(&mut self) {
        unsafe {
            if let Some(h) = self.fg_hook.take() {
                let _ = UnhookWinEvent(h);
            }
            if let Some(h) = self.loc_hook.take() {
                let _ = UnhookWinEvent(h);
            }
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.uninstall();
    }
}
