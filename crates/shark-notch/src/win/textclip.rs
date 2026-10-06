//! Put plain text on the clipboard (used by the tray's "copy report" items).

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;

use super::util::wide;

pub fn copy_text(owner: HWND, text: &str) -> bool {
    let w = wide(text);
    let bytes = w.len() * 2;
    unsafe {
        if OpenClipboard(Some(owner)).is_err() {
            return false;
        }
        let ok = (|| -> windows::core::Result<()> {
            EmptyClipboard()?;
            let h: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes)?;
            let p = GlobalLock(h) as *mut u16;
            if p.is_null() {
                let _ = GlobalFree(Some(h));
                return Err(super::util::fail("GlobalLock failed"));
            }
            std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
            let _ = GlobalUnlock(h);
            // On success the clipboard owns the memory.
            if let Err(e) = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(h.0))) {
                let _ = GlobalFree(Some(h));
                return Err(e);
            }
            Ok(())
        })()
        .is_ok();
        let _ = CloseClipboard();
        ok
    }
}
