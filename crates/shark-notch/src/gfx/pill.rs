//! The CPU "pill": a tiny layered window holding a premultiplied-BGRA bitmap, shown while the GPU
//! stack is released. No Direct3D, Direct2D or DirectWrite is loaded to draw it, so the resident
//! cost of an idle notch is a few kilobytes of bitmap.

use std::ffi::c_void;

use windows::Win32::Foundation::{COLORREF, HWND, POINT, SIZE};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HBITMAP,
    HDC, HGDIOBJ, ReleaseDC, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, HWND_TOPMOST, SW_HIDE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    SetWindowDisplayAffinity, SetWindowPos, ShowWindow, ULW_ALPHA, UpdateLayeredWindow,
    WDA_EXCLUDEFROMCAPTURE, WDA_NONE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::Result;

use crate::win::window::{self, WndProc};

pub const PILL_CLASS: &str = "SharkNotch.Pill";

pub struct PillWindow {
    pub hwnd: HWND,
    mem_dc: HDC,
    dib: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    size: (i32, i32),
    pub shown: bool,
    exclude: bool,
}

impl PillWindow {
    pub fn create(proc: WndProc, exclude_from_capture: bool) -> Result<PillWindow> {
        window::register_class(PILL_CLASS, proc)?;
        let ex =
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        let hwnd = window::create_window(ex, PILL_CLASS, "Shark Notch Pill", WS_POPUP, 0, 0, 1, 1)?;
        unsafe {
            let screen = GetDC(None);
            let mem_dc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);
            let _ = SetWindowDisplayAffinity(
                hwnd,
                if exclude_from_capture {
                    WDA_EXCLUDEFROMCAPTURE
                } else {
                    WDA_NONE
                },
            );
            Ok(PillWindow {
                hwnd,
                mem_dc,
                dib: HBITMAP::default(),
                old: HGDIOBJ::default(),
                bits: std::ptr::null_mut(),
                size: (0, 0),
                shown: false,
                exclude: exclude_from_capture,
            })
        }
    }

    pub fn set_capture_exclusion(&mut self, on: bool) {
        self.exclude = on;
        unsafe {
            let _ = SetWindowDisplayAffinity(
                self.hwnd,
                if on { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE },
            );
        }
    }

    fn ensure_dib(&mut self, w: i32, h: i32) -> Result<()> {
        if self.size == (w, h) && !self.bits.is_null() {
            return Ok(());
        }
        unsafe {
            if !self.dib.is_invalid() {
                SelectObject(self.mem_dc, self.old);
                let _ = DeleteObject(HGDIOBJ(self.dib.0));
            }
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let dib =
                CreateDIBSection(Some(self.mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
            self.old = SelectObject(self.mem_dc, HGDIOBJ(dib.0));
            self.dib = dib;
            self.bits = bits as *mut u8;
            self.size = (w, h);
        }
        Ok(())
    }

    /// Upload `bgra` (premultiplied, `w*h*4` bytes) and place the window at `(x, y)` (screen pixels).
    pub fn update(&mut self, bgra: &[u8], w: i32, h: i32, x: i32, y: i32) -> Result<()> {
        if w <= 0 || h <= 0 || bgra.len() < (w * h * 4) as usize {
            return Ok(());
        }
        self.ensure_dib(w, h)?;
        unsafe {
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), self.bits, (w * h * 4) as usize);
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let (pos, size, src) = (POINT { x, y }, SIZE { cx: w, cy: h }, POINT { x: 0, y: 0 });
            UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&pos),
                Some(&size),
                Some(self.mem_dc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )?;
        }
        Ok(())
    }

    pub fn show(&mut self) {
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
        self.shown = true;
    }

    pub fn hide(&mut self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.shown = false;
    }

    pub fn raise(&self) {
        if self.shown {
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    }
}

impl Drop for PillWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            if !self.dib.is_invalid() {
                SelectObject(self.mem_dc, self.old);
                let _ = DeleteObject(HGDIOBJ(self.dib.0));
            }
            let _ = DeleteDC(self.mem_dc);
        }
    }
}
