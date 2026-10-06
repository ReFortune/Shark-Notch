//! Input vocabulary delivered to modules, and the scroll/swipe gesture accumulator.

use crate::geom::Vec2;

/// Pointer and keyboard input in the module's coordinate space (DIPs, window origin).
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// Pointer moved (hover). Delivered only while the notch is interactive.
    Move(Vec2),
    Down(Vec2),
    Up(Vec2),
    /// Press and release landed on the same hit region (the region id arrives as `hit`).
    Click(Vec2),
    /// Pointer left the notch.
    Leave,
    /// Wheel / touchpad scroll in raw `WHEEL_DELTA` units (120 = one notch). Positive `dy` is
    /// "scroll up"; positive `dx` is "scroll right".
    Wheel {
        pos: Vec2,
        dx: f32,
        dy: f32,
    },
    /// Pointer dragged with the primary button held; `start` is where the press began.
    Drag {
        start: Vec2,
        pos: Vec2,
    },
    /// Text typed (only delivered while a module has requested keyboard focus).
    Char(char),
    Key(Key),
    /// Text pasted with Ctrl+V (the platform read the clipboard), only while the keyboard is requested.
    Text(std::sync::Arc<str>),
    /// The window lost keyboard focus (the user clicked elsewhere): finish or cancel any editing.
    FocusLost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Enter,
    Escape,
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Tab,
}

/// Turns a stream of wheel deltas into discrete "switch page" decisions.
///
/// A classic mouse wheel sends 120 per notch and should switch one page per notch. A precision
/// touchpad sends many tiny deltas; those are accumulated, and one swipe switches one page, after
/// which a cooldown ignores the inertial tail of the same swipe (a 250 ms cooldown is also what
/// keeps a single flick from skipping several pages).
#[derive(Debug, Clone)]
pub struct WheelGesture {
    acc: f32,
    last_event: f64,
    cooldown_until: f64,
    /// After a *touchpad* switch, ignore input until the stream goes quiet (see `quiet_gap`).
    locked: bool,
    pub threshold: f32,
    pub cooldown: f64,
    /// A gap this long ends the current swipe and clears the accumulator.
    pub reset_gap: f64,
    /// A gap this long unlocks a touchpad swipe, so a long inertial tail cannot switch twice.
    pub quiet_gap: f64,
    pub reverse: bool,
}

impl Default for WheelGesture {
    fn default() -> Self {
        Self {
            acc: 0.0,
            last_event: f64::NEG_INFINITY,
            cooldown_until: f64::NEG_INFINITY,
            locked: false,
            threshold: 80.0,
            cooldown: 0.25,
            reset_gap: 0.2,
            quiet_gap: 0.12,
            reverse: false,
        }
    }
}

/// Deltas at least this large per event are discrete mouse-wheel notches (120), not a touchpad.
const NOTCH: f32 = 100.0;

impl WheelGesture {
    /// Feed a wheel event. Returns `Some(+1)` for "next page", `Some(-1)` for "previous page".
    ///
    /// Horizontal scrolling wins when it dominates. Scrolling *down* (negative `dy`) or *right*
    /// (positive `dx`) advances.
    pub fn feed(&mut self, now: f64, dx: f32, dy: f32) -> Option<i32> {
        let gap = now - self.last_event;
        if gap > self.reset_gap {
            self.acc = 0.0;
        }
        if gap > self.quiet_gap {
            self.locked = false;
        }
        self.last_event = now;
        if now < self.cooldown_until || self.locked {
            // Swallow the inertial tail; keep the accumulator at zero so it cannot "bank" a switch.
            self.acc = 0.0;
            return None;
        }
        let delta = if dx.abs() > dy.abs() { dx } else { -dy };
        self.acc += if self.reverse { -delta } else { delta };
        if self.acc.abs() >= self.threshold {
            let dir = if self.acc > 0.0 { 1 } else { -1 };
            self.acc = 0.0;
            self.cooldown_until = now + self.cooldown;
            // A mouse wheel is discrete, so only the cooldown applies; a touchpad swipe keeps
            // streaming small deltas, so lock until it goes quiet.
            self.locked = delta.abs() < NOTCH;
            return Some(dir);
        }
        None
    }

    pub fn reset(&mut self) {
        self.acc = 0.0;
        self.locked = false;
        self.cooldown_until = f64::NEG_INFINITY;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_mouse_notch_switches_one_page() {
        let mut g = WheelGesture::default();
        assert_eq!(
            g.feed(0.0, 0.0, -120.0),
            Some(1),
            "wheel toward the user advances"
        );
        assert_eq!(g.feed(1.0, 0.0, 120.0), Some(-1));
    }

    #[test]
    fn horizontal_swipe_wins_and_direction_is_natural() {
        let mut g = WheelGesture::default();
        assert_eq!(g.feed(0.0, 120.0, 30.0), Some(1));
        assert_eq!(g.feed(1.0, -120.0, 30.0), Some(-1));
    }

    #[test]
    fn touchpad_swipe_accumulates_then_cools_down() {
        let mut g = WheelGesture::default();
        let mut t = 0.0;
        let mut fired = 0;
        // A 600 ms swipe of small deltas with inertial tail: exactly one switch.
        for _ in 0..60 {
            if g.feed(t, 18.0, 0.0).is_some() {
                fired += 1;
            }
            t += 0.01;
        }
        assert_eq!(fired, 1, "one swipe == one page");
    }

    #[test]
    fn fast_mouse_wheel_spinning_can_cross_several_pages() {
        let mut g = WheelGesture::default();
        // Notches 300 ms apart (faster than a lazy scroll, slower than the cooldown).
        assert_eq!(g.feed(0.0, 0.0, -120.0), Some(1));
        assert_eq!(g.feed(0.1, 0.0, -120.0), None, "inside the cooldown");
        assert_eq!(
            g.feed(0.3, 0.0, -120.0),
            Some(1),
            "discrete notches are never locked out"
        );
        assert_eq!(g.feed(0.6, 0.0, -120.0), Some(1));
    }

    #[test]
    fn separate_swipes_each_switch() {
        let mut g = WheelGesture::default();
        assert_eq!(g.feed(0.0, 100.0, 0.0), Some(1));
        // After the cooldown and a gap, a new swipe works.
        assert_eq!(g.feed(1.0, 100.0, 0.0), Some(1));
    }

    #[test]
    fn tiny_jitter_never_switches() {
        let mut g = WheelGesture::default();
        for i in 0..100 {
            assert_eq!(
                g.feed(i as f64 * 0.5, 3.0, 0.0),
                None,
                "gaps clear the accumulator"
            );
        }
    }

    #[test]
    fn reverse_flips_direction() {
        let mut g = WheelGesture {
            reverse: true,
            ..Default::default()
        };
        assert_eq!(g.feed(0.0, 0.0, -120.0), Some(-1));
    }
}
