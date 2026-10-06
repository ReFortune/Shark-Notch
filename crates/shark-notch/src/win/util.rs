//! Small helpers shared by the Win32 wrappers.

use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::CreateEventW;
use windows::core::{Error, PCWSTR};

/// UTF-16, NUL-terminated.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn pcwstr(buf: &[u16]) -> PCWSTR {
    PCWSTR(buf.as_ptr())
}

/// Decode a fixed-size UTF-16 buffer up to the first NUL.
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// A kernel event handle that may be signalled from any thread and is closed on drop. Services use
/// one to tell their worker thread to quit.
pub struct EventHandle(pub HANDLE);

// SAFETY: a kernel handle is just an id; the event APIs are thread-safe.
unsafe impl Send for EventHandle {}
unsafe impl Sync for EventHandle {}

impl EventHandle {
    /// An initially non-signalled event; `manual_reset` events stay signalled until reset.
    pub fn new(manual_reset: bool) -> Option<EventHandle> {
        unsafe { CreateEventW(None, manual_reset, false, PCWSTR::null()) }
            .ok()
            .map(EventHandle)
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub fn fail(msg: &str) -> Error {
    Error::new(E_FAIL, msg)
}

/// `HH:MM:SS.mmm` in local time, for log lines.
pub fn local_time_string() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

/// Signed x coordinate packed in an `LPARAM` (works for negative multi-monitor coordinates).
pub fn x_of(lparam: isize) -> i32 {
    (lparam & 0xFFFF) as u16 as i16 as i32
}

pub fn y_of(lparam: isize) -> i32 {
    ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_round_trips_and_terminates() {
        let w = wide("Shark ✓");
        assert_eq!(*w.last().unwrap(), 0);
        assert_eq!(from_wide(&w), "Shark ✓");
        assert_eq!(
            from_wide(&[72, 105, 0, 99, 99]),
            "Hi",
            "stops at the first NUL"
        );
    }

    #[test]
    fn packed_coordinates_are_signed() {
        let lp = ((-5i16 as u16 as isize) << 16) | (-12i16 as u16 as isize);
        assert_eq!((x_of(lp), y_of(lp)), (-12, -5));
    }
}
