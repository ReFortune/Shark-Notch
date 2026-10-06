//! Cross-thread mailbox: workers push a message and wake the UI thread; the UI thread drains it.
//! (Phase 2 generalises this into the typed event bus; phase 1 only needs config loading.)

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use notch_core::config::Loaded;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

pub const WM_APP_WAKE: u32 = 0x8000 + 1; // WM_APP + 1

pub enum Msg {
    Config(Result<Loaded, String>),
}

static QUEUE: Mutex<VecDeque<Msg>> = Mutex::new(VecDeque::new());
static PENDING: AtomicBool = AtomicBool::new(false);
static TARGET: AtomicIsize = AtomicIsize::new(0);

/// Set the window that is woken. Call once the controller window exists.
pub fn set_target(hwnd: HWND) {
    TARGET.store(hwnd.0 as isize, Ordering::SeqCst);
}

pub fn push(msg: Msg) {
    if let Ok(mut q) = QUEUE.lock() {
        q.push_back(msg);
    }
    wake();
}

/// Post at most one wake-up message no matter how many pushes happen before the UI thread drains.
pub fn wake() {
    if !PENDING.swap(true, Ordering::SeqCst) {
        let h = TARGET.load(Ordering::SeqCst);
        if h != 0 {
            unsafe {
                let _ = PostMessageW(Some(HWND(h as *mut _)), WM_APP_WAKE, WPARAM(0), LPARAM(0));
            }
        } else {
            PENDING.store(false, Ordering::SeqCst);
        }
    }
}

pub fn drain() -> Vec<Msg> {
    PENDING.store(false, Ordering::SeqCst);
    QUEUE
        .lock()
        .map(|mut q| q.drain(..).collect())
        .unwrap_or_default()
}
