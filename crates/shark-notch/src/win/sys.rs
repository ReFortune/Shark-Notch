//! System queries: monitors and DPI, theme/accent, the "animation effects" setting, and this
//! process's own memory/CPU numbers (for diagnostics and the self-test).

use std::ffi::c_void;

use notch_core::color::Color;
use windows::Win32::Foundation::{FILETIME, HANDLE, LPARAM, RECT};
use windows::Win32::Globalization::{GetLocaleInfoEx, LOCALE_ITIME};
use windows::Win32::Graphics::Dwm::DwmGetColorizationColor;
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2,
};
use windows::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, GetProcessTimes};
use windows::Win32::System::WindowsProgramming::{QueryProcessCycleTime, QueryThreadCycleTime};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};
use windows::core::{BOOL, PCWSTR};

use super::util::{from_wide, wide};

#[derive(Clone, Debug)]
pub struct MonitorInfo {
    /// Full monitor rectangle in physical pixels (virtual-screen coordinates).
    pub rect: RECT,
    pub device: String,
    pub primary: bool,
    pub dpi: u32,
}

impl MonitorInfo {
    pub fn width(&self) -> i32 {
        self.rect.right - self.rect.left
    }

    pub fn height(&self) -> i32 {
        self.rect.bottom - self.rect.top
    }
}

unsafe extern "system" fn enum_proc(
    hmon: HMONITOR,
    _dc: HDC,
    _rc: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let list = unsafe { &mut *(data.0 as *mut Vec<MonitorInfo>) };
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if unsafe { GetMonitorInfoW(hmon, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) }
        .as_bool()
    {
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
        list.push(MonitorInfo {
            rect: info.monitorInfo.rcMonitor,
            device: from_wide(&info.szDevice),
            primary: info.monitorInfo.dwFlags & 1 != 0, // MONITORINFOF_PRIMARY
            dpi: dx.max(96),
        });
    }
    BOOL(1)
}

/// All attached monitors, primary first.
pub fn monitors() -> Vec<MonitorInfo> {
    let mut list: Vec<MonitorInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(enum_proc),
            LPARAM(&mut list as *mut _ as isize),
        );
    }
    list.sort_by_key(|m| !m.primary);
    list
}

/// Pick the monitor named by the config: `"primary"`, a 1-based index, or a device name. Falls back
/// to the primary monitor so a disconnected display never leaves the notch homeless.
pub fn select_monitor(spec: &str) -> Option<MonitorInfo> {
    let all = monitors();
    let spec = spec.trim();
    let chosen = if spec.eq_ignore_ascii_case("primary") || spec.is_empty() {
        None
    } else if let Ok(n) = spec.parse::<usize>() {
        // Index is in left-to-right order, which is what people count.
        let mut by_x = all.clone();
        by_x.sort_by_key(|m| (m.rect.left, m.rect.top));
        n.checked_sub(1).and_then(|i| by_x.get(i).cloned())
    } else {
        all.iter()
            .find(|m| m.device.eq_ignore_ascii_case(spec))
            .cloned()
    };
    chosen.or_else(|| all.into_iter().next())
}

// ----- theme, accent, animations -------------------------------------------------------------

fn reg_dword(root: HKEY, subkey: &str, name: &str) -> Option<u32> {
    let (sk, nm) = (wide(subkey), wide(name));
    let mut data = 0u32;
    let mut size = 4u32;
    let rc = unsafe {
        RegGetValueW(
            root,
            windows::core::PCWSTR(sk.as_ptr()),
            windows::core::PCWSTR(nm.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut c_void),
            Some(&mut size),
        )
    };
    (rc.0 == 0).then_some(data)
}

/// `true` when Windows apps are set to dark mode.
pub fn system_dark() -> bool {
    reg_dword(
        HKEY_CURRENT_USER,
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        "AppsUseLightTheme",
    )
    .map(|v| v == 0)
    .unwrap_or(true)
}

/// The Windows accent colour (DWM colourisation colour).
pub fn system_accent() -> Color {
    let (mut argb, mut opaque) = (0u32, BOOL(0));
    if unsafe { DwmGetColorizationColor(&mut argb, &mut opaque) }.is_ok() {
        return Color::rgb8((argb >> 16) as u8, (argb >> 8) as u8, argb as u8);
    }
    notch_core::theme::FALLBACK_ACCENT
}

/// Windows' "Animation effects" setting (Settings > Accessibility > Visual effects).
pub fn animations_enabled() -> bool {
    let mut on = BOOL(1);
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(&mut on as *mut BOOL as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .is_ok();
    !ok || on.as_bool()
}

// ----- own process metrics --------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcMetrics {
    /// Task Manager's "Memory" column (private working set), bytes.
    pub private_ws: u64,
    pub working_set: u64,
    pub private_commit: u64,
    /// The largest the working set has been so far, bytes.
    pub peak_working_set: u64,
    /// Total CPU time (kernel + user) used by the process so far, seconds. Advances in scheduler
    /// ticks (~15.6 ms), so it cannot resolve a 0.01 % load over a few seconds; see `cycles`.
    pub cpu_secs: f64,
    /// CPU cycles consumed by all threads so far (`QueryProcessCycleTime`): exact, tick-free.
    pub cycles: u64,
}

fn filetime_secs(ft: FILETIME) -> f64 {
    ((u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)) as f64 / 1e7
}

pub fn proc_metrics() -> ProcMetrics {
    let mut m = ProcMetrics::default();
    unsafe {
        let me: HANDLE = GetCurrentProcess();
        let mut c = PROCESS_MEMORY_COUNTERS_EX2 {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32,
            ..Default::default()
        };
        if GetProcessMemoryInfo(
            me,
            &mut c as *mut PROCESS_MEMORY_COUNTERS_EX2 as *mut PROCESS_MEMORY_COUNTERS,
            c.cb,
        )
        .is_ok()
        {
            m.private_ws = c.PrivateWorkingSetSize as u64;
            m.working_set = c.WorkingSetSize as u64;
            m.private_commit = c.PrivateUsage as u64;
            m.peak_working_set = c.PeakWorkingSetSize as u64;
        }
        let (mut c0, mut e0, mut k, mut u) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        if GetProcessTimes(me, &mut c0, &mut e0, &mut k, &mut u).is_ok() {
            m.cpu_secs = filetime_secs(k) + filetime_secs(u);
        }
        let mut cycles = 0u64;
        if QueryProcessCycleTime(me, &mut cycles).is_ok() {
            m.cycles = cycles;
        }
    }
    m
}

/// How many process cycles pass per second of one fully busy core, measured by spinning this thread
/// for ~25 ms against the QPC. Dividing a cycle delta by this (and the elapsed time) gives a CPU
/// percentage with a resolution of a few microseconds instead of one scheduler tick.
pub fn cycles_per_sec() -> f64 {
    unsafe {
        let me = GetCurrentThread();
        let (mut c0, mut c1) = (0u64, 0u64);
        let t0 = crate::win::clock::now();
        let _ = QueryThreadCycleTime(me, &mut c0);
        while crate::win::clock::now() - t0 < 0.025 {
            std::hint::spin_loop();
        }
        let _ = QueryThreadCycleTime(me, &mut c1);
        let t1 = crate::win::clock::now();
        c1.saturating_sub(c0) as f64 / (t1 - t0).max(1e-6)
    }
}

/// Ask Windows to move this process's idle pages out of the working set. Cosmetic for "private bytes"
/// but it is what Task Manager's Memory column shows, and the pages come back on demand.
pub fn trim_working_set() {
    unsafe {
        let _ = windows::Win32::System::ProcessStatus::EmptyWorkingSet(GetCurrentProcess());
    }
}

/// The wall-clock time in the user's time zone (DST included; Windows does the conversion).
pub fn local_time() -> notch_core::civil::LocalTime {
    let t = unsafe { GetLocalTime() };
    notch_core::civil::LocalTime::new(
        i32::from(t.wYear),
        u32::from(t.wMonth),
        u32::from(t.wDay),
        u32::from(t.wHour),
        u32::from(t.wMinute),
        u32::from(t.wSecond),
    )
}

/// The regional preference for 24-hour time (`LOCALE_ITIME`: "0" = 12-hour, "1" = 24-hour).
pub fn system_24h() -> bool {
    let mut buf = [0u16; 4];
    let n = unsafe { GetLocaleInfoEx(PCWSTR::null(), LOCALE_ITIME, Some(&mut buf)) };
    n <= 0 || buf[0] != u16::from(b'0')
}

pub fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}
