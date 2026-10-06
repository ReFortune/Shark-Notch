//! Wait handle that signals when the config directory changes (`FindFirstChangeNotification`).
//! It is added to the main loop's wait set, so config hot-reload costs nothing while idle.

use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FindFirstChangeNotificationW, FindNextChangeNotification,
};

use super::util::{pcwstr, wide};

pub struct ConfigWatch {
    handle: HANDLE,
}

impl ConfigWatch {
    pub fn new(dir: &Path) -> Option<ConfigWatch> {
        let _ = std::fs::create_dir_all(dir);
        let d = wide(&dir.to_string_lossy());
        let h = unsafe {
            FindFirstChangeNotificationW(
                pcwstr(&d),
                false,
                FILE_NOTIFY_CHANGE_LAST_WRITE
                    | FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_SIZE,
            )
        }
        .ok()?;
        Some(ConfigWatch { handle: h })
    }

    pub fn handle(&self) -> HANDLE {
        self.handle
    }

    /// Re-arm after the handle signalled.
    pub fn rearm(&self) {
        unsafe {
            let _ = FindNextChangeNotification(self.handle);
        }
    }
}

impl Drop for ConfigWatch {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}
