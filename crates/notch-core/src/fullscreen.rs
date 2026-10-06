//! Pure decision logic for "is a game / fullscreen app in the foreground?".
//!
//! The Windows layer gathers the facts (`GetWindowRect`, `MonitorFromWindow`, window styles,
//! `SHQueryUserNotificationState`) and this module decides. Keeping the decision pure means the
//! tricky heuristics (borderless-fullscreen vs. a maximised window with auto-hide taskbar, F11
//! browsers, the desktop itself) are unit-tested.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl IRect {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// True if `self` fully covers `other` (allowing `slack` px of leeway per edge).
    pub fn covers(&self, other: &IRect, slack: i32) -> bool {
        self.left <= other.left + slack
            && self.top <= other.top + slack
            && self.right >= other.right - slack
            && self.bottom >= other.bottom - slack
    }
}

/// Mirror of `QUERY_USER_NOTIFICATION_STATE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserNotifState {
    NotPresent,
    Busy,
    D3dFullScreen,
    Presentation,
    AcceptsNotifications,
    QuietTime,
    App,
    Unknown,
}

impl UserNotifState {
    /// Map the raw Win32 enum value.
    pub fn from_raw(v: i32) -> Self {
        match v {
            1 => Self::NotPresent,
            2 => Self::Busy,
            3 => Self::D3dFullScreen,
            4 => Self::Presentation,
            5 => Self::AcceptsNotifications,
            6 => Self::QuietTime,
            7 => Self::App,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct WindowFacts {
    pub rect: IRect,
    /// Full rect of the monitor the window is mostly on (not the work area).
    pub monitor: IRect,
    pub has_caption: bool,
    /// Desktop, taskbar, our own windows, Start/search surfaces and similar shell chrome.
    pub is_shell_or_own: bool,
    pub is_visible: bool,
    pub is_minimized: bool,
    /// DWM-cloaked (hidden UWP/virtual-desktop windows still look "visible" to `IsWindowVisible`).
    pub is_cloaked: bool,
}

/// Decide whether the notch should step aside.
///
/// * `D3D_FULL_SCREEN` and `PRESENTATION_MODE` are strong signals and always win.
/// * Otherwise the foreground window must cover its whole monitor *and* have no caption bar. That
///   excludes a normally maximised window (it has a caption and leaves the taskbar visible) while
///   catching borderless-fullscreen games, F11 browsers and fullscreen video.
/// * `BUSY` and `APP` alone are deliberately *not* enough: they are set for things like a
///   maximised window while the taskbar auto-hides, and would hide the notch needlessly.
pub fn is_fullscreen(f: &WindowFacts, q: UserNotifState) -> bool {
    if matches!(
        q,
        UserNotifState::D3dFullScreen | UserNotifState::Presentation
    ) {
        return true;
    }
    if f.is_shell_or_own || !f.is_visible || f.is_minimized || f.is_cloaked {
        return false;
    }
    f.rect.covers(&f.monitor, 0) && !f.has_caption
}

#[cfg(test)]
mod tests {
    use super::*;

    const MON: IRect = IRect::new(0, 0, 1920, 1080);

    fn facts(rect: IRect) -> WindowFacts {
        WindowFacts {
            rect,
            monitor: MON,
            has_caption: false,
            is_shell_or_own: false,
            is_visible: true,
            is_minimized: false,
            is_cloaked: false,
        }
    }

    #[test]
    fn borderless_game_is_fullscreen() {
        assert!(is_fullscreen(
            &facts(MON),
            UserNotifState::AcceptsNotifications
        ));
        // Windows sometimes overshoots the monitor by the invisible resize border.
        assert!(is_fullscreen(
            &facts(IRect::new(-8, -8, 1928, 1088)),
            UserNotifState::AcceptsNotifications
        ));
    }

    #[test]
    fn maximised_normal_window_is_not() {
        let mut f = facts(IRect::new(-8, -8, 1928, 1032)); // leaves the taskbar visible
        f.has_caption = true;
        assert!(!is_fullscreen(&f, UserNotifState::AcceptsNotifications));
        // Even with an auto-hide taskbar (covers everything) a captioned window is "maximised".
        let mut g = facts(MON);
        g.has_caption = true;
        assert!(
            !is_fullscreen(&g, UserNotifState::Busy),
            "BUSY alone must not trigger"
        );
    }

    #[test]
    fn d3d_exclusive_and_presentation_always_win() {
        let mut f = facts(IRect::new(100, 100, 400, 300));
        f.is_shell_or_own = true;
        assert!(is_fullscreen(&f, UserNotifState::D3dFullScreen));
        assert!(is_fullscreen(&f, UserNotifState::Presentation));
    }

    #[test]
    fn shell_hidden_minimized_cloaked_never_count() {
        for mutate in [
            (|f: &mut WindowFacts| f.is_shell_or_own = true) as fn(&mut WindowFacts),
            |f| f.is_visible = false,
            |f| f.is_minimized = true,
            |f| f.is_cloaked = true,
        ] {
            let mut f = facts(MON);
            mutate(&mut f);
            assert!(!is_fullscreen(&f, UserNotifState::AcceptsNotifications));
        }
    }

    #[test]
    fn partial_cover_is_not_fullscreen() {
        assert!(!is_fullscreen(
            &facts(IRect::new(0, 0, 1920, 1040)),
            UserNotifState::AcceptsNotifications
        ));
        assert!(!is_fullscreen(
            &facts(IRect::new(0, 0, 1900, 1080)),
            UserNotifState::AcceptsNotifications
        ));
    }

    #[test]
    fn works_on_a_secondary_monitor_with_offsets() {
        let mon2 = IRect::new(1920, -200, 3840, 880);
        let f = WindowFacts {
            monitor: mon2,
            rect: mon2,
            ..facts(MON)
        };
        assert!(is_fullscreen(&f, UserNotifState::AcceptsNotifications));
    }

    #[test]
    fn raw_state_mapping() {
        assert_eq!(UserNotifState::from_raw(3), UserNotifState::D3dFullScreen);
        assert_eq!(
            UserNotifState::from_raw(5),
            UserNotifState::AcceptsNotifications
        );
        assert_eq!(UserNotifState::from_raw(99), UserNotifState::Unknown);
    }
}
