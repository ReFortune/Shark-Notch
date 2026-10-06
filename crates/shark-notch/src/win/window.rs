//! Window-class registration and style helpers shared by the controller, stage and pill windows.

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, GWL_EXSTYLE, GetWindowLongPtrW, IDC_ARROW, LoadCursorW, RegisterClassExW,
    SetWindowLongPtrW, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSEXW,
};
use windows::core::{PCWSTR, Result};

use super::util::wide;

pub type WndProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

/// Register a window class (idempotent: "class already exists" is treated as success).
pub fn register_class(name: &str, proc: WndProc) -> Result<()> {
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let class = wide(name);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            let err = windows::core::Error::from_thread();
            // ERROR_CLASS_ALREADY_EXISTS (1410)
            if err.code().0 as u32 & 0xFFFF != 1410 {
                return Err(err);
            }
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub fn create_window(
    ex: WINDOW_EX_STYLE,
    class: &str,
    title: &str,
    style: WINDOW_STYLE,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Result<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let (class, title) = (wide(class), wide(title));
        CreateWindowExW(
            ex,
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            style,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinst.into()),
            None,
        )
    }
}

pub fn ex_style(hwnd: HWND) -> u32 {
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 }
}

/// Set or clear one extended-style flag. Returns whether anything changed.
pub fn set_ex_flag(hwnd: HWND, flag: WINDOW_EX_STYLE, on: bool) -> bool {
    let cur = ex_style(hwnd);
    let new = if on { cur | flag.0 } else { cur & !flag.0 };
    if new == cur {
        return false;
    }
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new as isize);
    }
    true
}
