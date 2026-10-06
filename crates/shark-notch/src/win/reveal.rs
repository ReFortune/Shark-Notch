//! "Show in folder": open Explorer on a file's folder with the file selected.
//!
//! This only ever *selects* the file. It never opens, runs or previews it, so a download that turns
//! out to be hostile is not touched by being revealed. Only plain absolute paths are accepted (see
//! `notch_core::downloads::is_plain_absolute_path`).
//!
//! The shell call can take a while (it may start Explorer), so it runs on a short-lived thread with
//! its own COM apartment and never on the UI thread.

use notch_core::downloads::is_plain_absolute_path;
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize, IBindCtx,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{SHOpenFolderAndSelectItems, SHParseDisplayName};
use windows::core::PCWSTR;

use super::util::wide;

/// Ask Explorer to show `path`. Returns whether the request was accepted (not whether Explorer
/// managed to show it: that happens later, on the worker thread).
pub fn reveal(path: &str) -> bool {
    if !is_plain_absolute_path(path) {
        crate::warn!("refused to reveal something that is not a plain absolute path");
        return false;
    }
    let path = path.to_string();
    std::thread::Builder::new()
        .name("reveal".into())
        .stack_size(512 * 1024)
        .spawn(move || reveal_now(&path))
        .is_ok()
}

fn reveal_now(path: &str) {
    let w = wide(path);
    unsafe {
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        let parsed = SHParseDisplayName(PCWSTR(w.as_ptr()), None::<&IBindCtx>, &mut pidl, 0, None);
        match parsed {
            // With no children, the item itself is the one to select in its parent folder.
            Ok(()) if !pidl.is_null() => {
                if let Err(e) = SHOpenFolderAndSelectItems(pidl, None, 0) {
                    crate::warn!("could not show the file in Explorer ({e})");
                }
                CoTaskMemFree(Some(pidl.cast_const().cast()));
            }
            _ => crate::debug!("the file to reveal is not there any more"),
        }
        if com {
            CoUninitialize();
        }
    }
}
