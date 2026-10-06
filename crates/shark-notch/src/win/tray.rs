//! Tray icon and context menu. The icon is rasterised at runtime from the same notch silhouette
//! code the notch itself uses, so there is no `.ico` resource to ship.

use notch_core::color::Color;
use notch_core::geom::Vec2;
use notch_core::path::{NotchShape, rounded_rect_path};
use notch_core::raster::fill_polygons;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject, HGDIOBJ,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos,
    GetSystemMetrics, HICON, ICONINFO, MF_CHECKED, MF_SEPARATOR, MF_STRING, PostMessageW,
    SM_CXSMICON, SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
    WM_NULL,
};
use windows::core::PCWSTR;

use super::util::wide;

pub const WM_TRAY: u32 = 0x8000 + 2; // WM_APP + 2
const ICON_ID: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuCmd {
    Toggle = 1,
    Pause,
    OpenConfig,
    ReloadConfig,
    Autostart,
    CopyFrames,
    CopyDiagnostics,
    HideTray,
    Quit,
}

impl MenuCmd {
    fn from_id(id: i32) -> Option<MenuCmd> {
        use MenuCmd::*;
        [
            Toggle,
            Pause,
            OpenConfig,
            ReloadConfig,
            Autostart,
            CopyFrames,
            CopyDiagnostics,
            HideTray,
            Quit,
        ]
        .into_iter()
        .find(|c| *c as i32 == id)
    }
}

pub struct MenuState {
    pub paused: bool,
    pub autostart: bool,
    pub hotkey: String,
    pub status: String,
}

/// RGBA-premultiplied BGRA pixels of the tray glyph at `size` px: a dark rounded square with the
/// white notch silhouette hanging from its top edge.
pub fn icon_bgra(size: usize) -> Vec<u8> {
    let s = size as f32;
    let bg = rounded_rect_path(
        notch_core::geom::Rect::new(0.0, 0.0, s, s),
        [s * 0.24; 4],
        0.6,
    );
    let bg_cov = fill_polygons(&bg.flatten(0.05), size, size);
    let notch = NotchShape {
        w: s * 0.66,
        h: s * 0.40,
        radius_top: 0.0,
        radius_bottom: s * 0.16,
        ear: s * 0.09,
        smoothing: 0.6,
    };
    let np = notch.to_path().translated(Vec2::new(s * 0.17, s * 0.10));
    let n_cov = fill_polygons(&np.flatten(0.05), size, size);

    let dark = Color::rgb8(0x1E, 0x1E, 0x24);
    let mut out = Vec::with_capacity(size * size * 4);
    for i in 0..size * size {
        let a_bg = bg_cov.data[i] as f32 / 255.0;
        let a_n = n_cov.data[i] as f32 / 255.0;
        // White notch over the dark square (both premultiplied by the square's coverage).
        let (r, g, b) = (
            dark.r * (1.0 - a_n) + a_n,
            dark.g * (1.0 - a_n) + a_n,
            dark.b * (1.0 - a_n) + a_n,
        );
        let px = |v: f32| ((v * a_bg).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        out.extend_from_slice(&[
            px(b),
            px(g),
            px(r),
            (a_bg.clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
        ]);
    }
    out
}

fn create_icon(size: i32) -> Option<HICON> {
    let bgra = icon_bgra(size as usize);
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, bgra.len());
        let mask = CreateBitmap(size, size, 1, 1, None);
        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
        icon
    }
}

pub struct Tray {
    hwnd: HWND,
    icon: HICON,
    added: bool,
}

impl Tray {
    pub fn new(hwnd: HWND) -> Option<Tray> {
        let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
        let icon = create_icon(size)?;
        Some(Tray {
            hwnd,
            icon,
            added: false,
        })
    }

    fn data(&self, tooltip: &str) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: WM_TRAY,
            hIcon: self.icon,
            ..Default::default()
        };
        let t = wide(tooltip);
        let n = t.len().min(nid.szTip.len() - 1);
        nid.szTip[..n].copy_from_slice(&t[..n]);
        nid
    }

    /// Add (or re-add after Explorer restarted) the icon.
    pub fn add(&mut self, tooltip: &str) {
        let mut nid = self.data(tooltip);
        unsafe {
            if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
                self.added = true;
            } else {
                crate::warn!("Shell_NotifyIconW(NIM_ADD) failed");
            }
        }
    }

    pub fn set_tooltip(&self, tooltip: &str) {
        if self.added {
            let nid = self.data(tooltip);
            unsafe {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
            }
        }
    }

    pub fn remove(&mut self) {
        if self.added {
            let nid = self.data("");
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            }
            self.added = false;
        }
    }

    pub fn is_added(&self) -> bool {
        self.added
    }

    /// Show the context menu at the cursor and return the chosen command.
    pub fn show_menu(&self, st: &MenuState) -> Option<MenuCmd> {
        unsafe {
            let menu = CreatePopupMenu().ok()?;
            let item = |cmd: MenuCmd, text: String, checked: bool| {
                let w = wide(&text);
                let _ = AppendMenuW(
                    menu,
                    MF_STRING
                        | if checked {
                            MF_CHECKED
                        } else {
                            Default::default()
                        },
                    cmd as usize,
                    PCWSTR(w.as_ptr()),
                );
            };
            let sep = || {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            };
            if !st.status.is_empty() {
                let w = wide(&st.status);
                // Disabled informational line at the top.
                let _ = AppendMenuW(
                    menu,
                    MF_STRING | windows::Win32::UI::WindowsAndMessaging::MF_GRAYED,
                    0,
                    PCWSTR(w.as_ptr()),
                );
                sep();
            }
            let toggle = if st.hotkey.is_empty() {
                "Open / close notch".to_string()
            } else {
                format!("Open / close notch\t{}", st.hotkey)
            };
            item(MenuCmd::Toggle, toggle, false);
            item(MenuCmd::Pause, "Pause".into(), st.paused);
            sep();
            item(
                MenuCmd::OpenConfig,
                "Open settings (config.toml)".into(),
                false,
            );
            item(MenuCmd::ReloadConfig, "Reload settings".into(), false);
            item(
                MenuCmd::Autostart,
                "Start with Windows".into(),
                st.autostart,
            );
            sep();
            item(MenuCmd::CopyFrames, "Copy frame-time report".into(), false);
            item(MenuCmd::CopyDiagnostics, "Copy diagnostics".into(), false);
            sep();
            item(MenuCmd::HideTray, "Hide tray icon".into(), false);
            item(MenuCmd::Quit, "Quit".into(), false);

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // The menu only dismisses correctly if its owner is foreground (a documented quirk).
            let _ = SetForegroundWindow(self.hwnd);
            let picked = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                pt.x,
                pt.y,
                None,
                self.hwnd,
                None,
            );
            let _ = PostMessageW(
                Some(self.hwnd),
                WM_NULL,
                Default::default(),
                Default::default(),
            );
            let _ = DestroyMenu(menu);
            MenuCmd::from_id(picked.0)
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
        unsafe {
            let _ = DestroyIcon(self.icon);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_is_a_rounded_square_with_a_bright_notch() {
        let size = 32;
        let px = icon_bgra(size);
        assert_eq!(px.len(), size * size * 4);
        let at = |x: usize, y: usize| &px[(y * size + x) * 4..(y * size + x) * 4 + 4];
        assert_eq!(at(0, 0)[3], 0, "rounded corner is transparent");
        assert_eq!(at(size - 1, size - 1)[3], 0);
        assert_eq!(at(size / 2, size - 3)[3], 255, "body is opaque");
        let dark = at(size / 2, size - 4);
        assert!(
            dark[0] < 80 && dark[1] < 80 && dark[2] < 80,
            "dark square: {dark:?}"
        );
        let notch = at(size / 2, 6);
        assert!(
            notch[0] > 200 && notch[1] > 200 && notch[2] > 200,
            "white notch: {notch:?}"
        );
    }
}
