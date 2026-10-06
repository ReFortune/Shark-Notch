//! Maps between DIPs and physical pixels for the notch window on its monitor.
//!
//! The window is sized once for the *largest* panel (plus slack for spring overshoot) and centred at
//! the top of the monitor. Shapes animate inside it, so the window is never resized mid-animation.

use notch_core::geom::{Size, Vec2};
use windows::Win32::Foundation::POINT;

use super::sys::MonitorInfo;

/// DIPs of slack around the largest panel for spring overshoot and anti-aliasing.
const SLACK: f32 = 40.0;

#[derive(Clone, Debug)]
pub struct Layout {
    pub mon: MonitorInfo,
    /// Physical pixels per DIP (monitor DPI / 96, times the user's extra UI scale).
    pub px_per_dip: f32,
    pub win_dip: Size,
    /// Window rectangle in physical pixels: x, y, w, h.
    pub win_px: (i32, i32, i32, i32),
}

impl Layout {
    pub fn new(mon: MonitorInfo, ui_scale: f32, largest_panel: Size, pill: Size) -> Layout {
        let px_per_dip = (mon.dpi as f32 / 96.0) * ui_scale;
        let max_w = (mon.width() as f32 / px_per_dip).max(100.0);
        let max_h = (mon.height() as f32 / px_per_dip).max(60.0);
        let win_dip = Size::new(
            (largest_panel.w.max(pill.w) + SLACK).min(max_w),
            (largest_panel.h.max(pill.h) + SLACK * 0.5).min(max_h),
        );
        let w = (win_dip.w * px_per_dip).ceil() as i32;
        let h = (win_dip.h * px_per_dip).ceil() as i32;
        let x = mon.rect.left + (mon.width() - w) / 2;
        let y = mon.rect.top;
        Layout {
            mon,
            px_per_dip,
            win_dip,
            win_px: (x, y, w, h),
        }
    }

    /// Cursor position (screen pixels) relative to the monitor's top-centre, in DIPs. `None` when the
    /// cursor is on another monitor.
    pub fn hover_space(&self, pt: POINT) -> Option<Vec2> {
        let r = &self.mon.rect;
        if pt.x < r.left || pt.x >= r.right || pt.y < r.top || pt.y >= r.bottom {
            return None;
        }
        let cx = r.left as f32 + self.mon.width() as f32 * 0.5;
        Some(Vec2::new(
            (pt.x as f32 - cx) / self.px_per_dip,
            (pt.y - r.top) as f32 / self.px_per_dip,
        ))
    }

    /// Window-local pixel position -> window DIPs.
    pub fn window_px_to_dip(&self, x: i32, y: i32) -> Vec2 {
        Vec2::new(x as f32 / self.px_per_dip, y as f32 / self.px_per_dip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::RECT;

    fn mon(l: i32, t: i32, w: i32, h: i32, dpi: u32) -> MonitorInfo {
        MonitorInfo {
            rect: RECT {
                left: l,
                top: t,
                right: l + w,
                bottom: t + h,
            },
            device: "x".into(),
            primary: true,
            dpi,
        }
    }

    #[test]
    fn centres_the_window_at_the_top_of_the_monitor() {
        let l = Layout::new(
            mon(0, 0, 1920, 1080, 96),
            1.0,
            Size::new(380.0, 190.0),
            Size::new(112.0, 6.0),
        );
        let (x, y, w, h) = l.win_px;
        assert_eq!(y, 0);
        assert_eq!(x * 2 + w, 1920, "symmetric about the monitor centre");
        assert_eq!((w, h), (420, 210));
    }

    #[test]
    fn dpi_and_user_scale_multiply() {
        let l = Layout::new(
            mon(0, 0, 3840, 2160, 192),
            1.25,
            Size::new(380.0, 190.0),
            Size::new(112.0, 6.0),
        );
        assert!((l.px_per_dip - 2.5).abs() < 1e-6);
        assert_eq!(l.win_px.2, (420.0f32 * 2.5).ceil() as i32);
    }

    #[test]
    fn secondary_monitor_with_negative_origin() {
        let l = Layout::new(
            mon(-2560, -300, 2560, 1440, 144),
            1.0,
            Size::new(380.0, 190.0),
            Size::new(112.0, 6.0),
        );
        assert_eq!(l.win_px.1, -300);
        let centre = -2560 + 1280;
        assert_eq!(
            l.win_px.0 + l.win_px.2 / 2,
            centre,
            "centred on its own monitor"
        );
        // Cursor at the monitor's top-centre maps to (0, 0); other monitors map to None.
        let p = l.hover_space(POINT { x: centre, y: -300 }).unwrap();
        assert!(p.x.abs() < 1e-3 && p.y.abs() < 1e-3);
        assert!(l.hover_space(POINT { x: 100, y: 100 }).is_none());
    }

    #[test]
    fn window_never_exceeds_a_small_monitor() {
        let l = Layout::new(
            mon(0, 0, 400, 300, 96),
            1.0,
            Size::new(900.0, 900.0),
            Size::new(112.0, 6.0),
        );
        assert!(l.win_px.2 <= 400 && l.win_px.3 <= 300);
    }
}
