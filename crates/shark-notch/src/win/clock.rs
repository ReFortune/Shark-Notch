//! One monotonic time base for the whole app: QueryPerformanceCounter, in seconds since start.
//!
//! DWM reports vblank times in QPC ticks, so using the same counter lets frame pacing line up exactly
//! (`std::time::Instant` hides the raw tick value).

use std::sync::OnceLock;

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

struct Base {
    freq: f64,
    origin: i64,
}

static BASE: OnceLock<Base> = OnceLock::new();

fn base() -> &'static Base {
    BASE.get_or_init(|| {
        let (mut f, mut o) = (0i64, 0i64);
        // Cannot fail on Windows XP and later.
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
            let _ = QueryPerformanceCounter(&mut o);
        }
        Base {
            freq: f.max(1) as f64,
            origin: o,
        }
    })
}

/// Raw QPC ticks.
pub fn qpc() -> i64 {
    let mut v = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut v);
    }
    v
}

/// Seconds since the first call to any clock function.
pub fn now() -> f64 {
    qpc_to_secs(qpc())
}

pub fn qpc_to_secs(ticks: i64) -> f64 {
    let b = base();
    (ticks - b.origin) as f64 / b.freq
}

pub fn ticks_to_duration_secs(ticks: u64) -> f64 {
    ticks as f64 / base().freq
}

/// Convert milliseconds since boot (as `GetLastInputInfo`/`GetTickCount` report) into this clock.
/// Only valid for recent values (the 32-bit tick counter wraps every ~49 days).
pub fn tick_ms_to_secs(now_ticks_ms: u32, then_ticks_ms: u32) -> f64 {
    now() - (now_ticks_ms.wrapping_sub(then_ticks_ms)) as f64 / 1000.0
}
