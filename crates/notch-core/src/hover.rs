//! Hover sensing as a pure state machine.
//!
//! The platform samples the cursor (cheaply, and only while it makes sense to) and feeds
//! [`HoverInput`]s here; the machine decides *when* to expand/collapse and how often the platform
//! should sample next ([`Cadence`]). No OS calls, so dwell timing, suppression and the
//! drag-to-shelf heuristic are all unit-tested.
//!
//! Coordinates: DIPs relative to the **top-centre** of the notch's monitor — `x` to the right of
//! centre, `y` down from the top edge.

use crate::geom::{Rect, Vec2};

#[derive(Clone, Copy, Debug)]
pub struct HoverParams {
    pub zone_half_width: f32,
    pub zone_height: f32,
    /// Extra margin around the zone that counts as "approaching" (pre-warm + faster sampling).
    pub approach_margin: f32,
    pub dwell: f64,
    pub leave_grace: f64,
    pub typing_suppress: f64,
    /// Region (from the top edge) in which a drag in progress arms the file shelf.
    pub drag_half_width: f32,
    pub drag_height: f32,
    pub drag_disarm_after: f64,
}

impl Default for HoverParams {
    fn default() -> Self {
        Self {
            zone_half_width: 100.0,
            zone_height: 6.0,
            approach_margin: 140.0,
            dwell: 0.150,
            leave_grace: 0.280,
            typing_suppress: 0.600,
            drag_half_width: 260.0,
            drag_height: 140.0,
            drag_disarm_after: 0.400,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HoverInput {
    pub now: f64,
    /// `None` when the cursor is not on the notch's monitor.
    pub cursor: Option<Vec2>,
    pub button_down: bool,
    /// Time of the most recent non-pointer input (typing), if the platform saw any.
    pub typed_at: Option<f64>,
    /// While expanded: the on-screen rect (same space as `cursor`) that keeps the notch open.
    pub keep_open: Option<Rect>,
    /// The platform believes an OLE-style drag is in progress.
    pub drag_likely: bool,
    /// A window move/size loop is running somewhere (dragging a title bar, resizing).
    pub move_size_active: bool,
}

impl HoverInput {
    pub fn at(now: f64, cursor: Option<Vec2>) -> Self {
        Self {
            now,
            cursor,
            button_down: false,
            typed_at: None,
            keep_open: None,
            drag_likely: false,
            move_size_active: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoverAction {
    None,
    /// Cursor entered the approach region: a good time to pre-warm the GPU stack.
    Approach,
    Expand,
    Collapse,
    ArmDrag,
    DisarmDrag,
}

/// How soon the platform should sample again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cadence {
    /// Nothing nearby: slow (default 10 Hz).
    Idle,
    /// Cursor near the zone or the notch is open: medium (~30 Hz).
    Near,
    /// Dwelling inside the zone: precise (~60 Hz) so 150 ms dwell is accurate.
    Dwell,
}

#[derive(Debug)]
pub struct HoverFsm {
    p: HoverParams,
    dwell_start: Option<f64>,
    left_at: Option<f64>,
    suppress_until: f64,
    approaching: bool,
    drag_armed: bool,
    drag_out_since: Option<f64>,
    /// After a manual collapse (hotkey/Esc/click) the cursor must leave the zone before hover can
    /// re-expand it, otherwise closing with the mouse parked at the top would instantly reopen.
    need_exit: bool,
}

impl HoverFsm {
    pub fn new(p: HoverParams) -> Self {
        Self {
            p,
            dwell_start: None,
            left_at: None,
            suppress_until: f64::NEG_INFINITY,
            approaching: false,
            drag_armed: false,
            drag_out_since: None,
            need_exit: false,
        }
    }

    pub fn params(&self) -> &HoverParams {
        &self.p
    }

    pub fn set_params(&mut self, p: HoverParams) {
        self.p = p;
    }

    pub fn notify_manual_collapse(&mut self) {
        self.need_exit = true;
        self.dwell_start = None;
    }

    pub fn is_drag_armed(&self) -> bool {
        self.drag_armed
    }

    /// Forget any in-progress dwell/drag (used when the shell is suspended).
    pub fn reset(&mut self) {
        self.dwell_start = None;
        self.left_at = None;
        self.approaching = false;
        self.drag_armed = false;
        self.drag_out_since = None;
        self.need_exit = false;
    }

    pub fn update(&mut self, i: &HoverInput, expanded: bool) -> (HoverAction, Cadence) {
        let p = &self.p;
        let c = i.cursor;
        let in_zone = c.is_some_and(|c| c.x.abs() <= p.zone_half_width && c.y <= p.zone_height);
        let near = c.is_some_and(|c| {
            c.x.abs() <= p.zone_half_width + p.approach_margin
                && c.y <= p.zone_height + p.approach_margin
        });
        let in_drag_zone =
            c.is_some_and(|c| c.x.abs() <= p.drag_half_width && c.y <= p.drag_height);

        if let Some(t) = i.typed_at {
            self.suppress_until = self.suppress_until.max(t + p.typing_suppress);
        }
        if !in_zone {
            self.need_exit = false;
        }

        // --- Drag-to-shelf arming (works whether or not the notch is expanded) -----------------
        if self.drag_armed {
            let release = !i.button_down;
            let strayed = if in_drag_zone {
                self.drag_out_since = None;
                false
            } else {
                let since = *self.drag_out_since.get_or_insert(i.now);
                i.now - since >= p.drag_disarm_after
            };
            if release || strayed {
                self.drag_armed = false;
                self.drag_out_since = None;
                return (HoverAction::DisarmDrag, Cadence::Near);
            }
            return (HoverAction::None, Cadence::Dwell);
        } else if i.button_down && i.drag_likely && in_drag_zone && !i.move_size_active {
            self.drag_armed = true;
            self.drag_out_since = None;
            self.dwell_start = None;
            return (HoverAction::ArmDrag, Cadence::Dwell);
        }

        // --- Expanded: keep open while the pointer is over the panel, collapse after a grace ----
        if expanded {
            let inside = match i.keep_open {
                Some(r) => c.is_some_and(|c| r.contains(c)),
                None => in_zone,
            };
            self.dwell_start = None;
            if inside {
                self.left_at = None;
            } else {
                let since = *self.left_at.get_or_insert(i.now);
                if i.now - since >= p.leave_grace {
                    self.left_at = None;
                    return (HoverAction::Collapse, Cadence::Near);
                }
            }
            return (HoverAction::None, Cadence::Near);
        }

        // --- Collapsed: dwell inside the narrow zone, unless suppressed -------------------------
        self.left_at = None;
        let suppressed =
            i.button_down || i.move_size_active || i.now < self.suppress_until || self.need_exit;
        if in_zone && !suppressed {
            let start = *self.dwell_start.get_or_insert(i.now);
            if i.now - start >= p.dwell {
                self.dwell_start = None;
                return (HoverAction::Expand, Cadence::Dwell);
            }
            return (HoverAction::None, Cadence::Dwell);
        }
        self.dwell_start = None;

        let action = if near && !self.approaching {
            self.approaching = true;
            HoverAction::Approach
        } else {
            HoverAction::None
        };
        if !near {
            self.approaching = false;
        }
        let cadence = if in_zone {
            Cadence::Dwell
        } else if near {
            Cadence::Near
        } else {
            Cadence::Idle
        };
        (action, cadence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fsm() -> HoverFsm {
        HoverFsm::new(HoverParams::default())
    }

    fn zone() -> Option<Vec2> {
        Some(Vec2::new(20.0, 2.0))
    }

    /// Sample at 60 Hz from `t0` for `dur` seconds with a fixed cursor; return the first non-None action.
    fn run(
        f: &mut HoverFsm,
        t0: f64,
        dur: f64,
        cursor: Option<Vec2>,
        expanded: bool,
    ) -> Option<(f64, HoverAction)> {
        let mut t = t0;
        while t <= t0 + dur {
            let (a, _) = f.update(&HoverInput::at(t, cursor), expanded);
            if !matches!(a, HoverAction::None | HoverAction::Approach) {
                return Some((t, a));
            }
            t += 1.0 / 60.0;
        }
        None
    }

    #[test]
    fn dwell_of_150ms_expands_and_not_before() {
        let mut f = fsm();
        let (t, a) = run(&mut f, 1.0, 1.0, zone(), false).unwrap();
        assert_eq!(a, HoverAction::Expand);
        assert!((t - 1.0 - 0.150).abs() < 0.02, "expanded after {}", t - 1.0);
        let mut g = fsm();
        assert!(
            run(&mut g, 1.0, 0.14, zone(), false).is_none(),
            "no expand before the dwell elapses"
        );
    }

    #[test]
    fn leaving_the_zone_resets_the_dwell() {
        let mut f = fsm();
        assert!(run(&mut f, 0.0, 0.10, zone(), false).is_none());
        assert!(run(&mut f, 0.11, 0.05, Some(Vec2::new(400.0, 300.0)), false).is_none());
        // Returning starts a fresh 150 ms dwell, not the remaining 50 ms.
        let (t, _) = run(&mut f, 0.17, 1.0, zone(), false).unwrap();
        assert!(t - 0.17 >= 0.14);
    }

    #[test]
    fn zone_is_narrow() {
        let mut f = fsm();
        // 7 DIP below the top edge is outside a 6 DIP zone; 101 DIP off-centre is outside 100.
        assert!(run(&mut f, 0.0, 1.0, Some(Vec2::new(0.0, 7.0)), false).is_none());
        assert!(run(&mut f, 0.0, 1.0, Some(Vec2::new(101.0, 2.0)), false).is_none());
        assert!(run(&mut f, 0.0, 1.0, Some(Vec2::new(-99.0, 0.0)), false).is_some());
    }

    #[test]
    fn mouse_button_or_window_drag_suppresses() {
        let mut f = fsm();
        let mut t = 0.0;
        while t < 1.0 {
            let mut i = HoverInput::at(t, zone());
            i.button_down = true;
            // (An `Approach` pre-warm hint is fine; expanding is what must not happen.)
            assert_ne!(f.update(&i, false).0, HoverAction::Expand);
            t += 0.016;
        }
        let mut g = fsm();
        let mut i = HoverInput::at(0.0, zone());
        i.move_size_active = true;
        for k in 0..60 {
            i.now = k as f64 * 0.016;
            assert_ne!(g.update(&i, false).0, HoverAction::Expand);
        }
    }

    #[test]
    fn recent_typing_suppresses_then_releases() {
        let mut f = fsm();
        let mut t = 0.0;
        let mut expanded_at = None;
        while t < 2.0 && expanded_at.is_none() {
            let mut i = HoverInput::at(t, zone());
            i.typed_at = Some(0.5); // user typed at 0.5 s
            if f.update(&i, false).0 == HoverAction::Expand {
                expanded_at = Some(t);
            }
            t += 1.0 / 60.0;
        }
        let at = expanded_at.expect("eventually expands");
        assert!(
            at >= 0.5 + 0.6,
            "must wait out the 600 ms typing window, got {at}"
        );
    }

    #[test]
    fn collapse_needs_the_grace_period_and_inside_cancels_it() {
        let mut f = fsm();
        let panel = Rect::new(-150.0, 0.0, 300.0, 120.0);
        let outside = Some(Vec2::new(0.0, 400.0));
        let mut t = 0.0;
        let mut i = HoverInput::at(t, outside);
        i.keep_open = Some(panel);
        // 200 ms outside: still open.
        while t < 0.2 {
            i.now = t;
            assert_eq!(f.update(&i, true).0, HoverAction::None);
            t += 0.016;
        }
        // Back inside: grace resets.
        i.cursor = Some(Vec2::new(0.0, 50.0));
        i.now = t;
        assert_eq!(f.update(&i, true).0, HoverAction::None);
        // Out again for 300 ms: collapses.
        i.cursor = outside;
        let mut collapsed = None;
        for k in 0..40 {
            i.now = t + 0.016 + k as f64 * 0.016;
            if f.update(&i, true).0 == HoverAction::Collapse {
                collapsed = Some(i.now - (t + 0.016));
                break;
            }
        }
        assert!(collapsed.expect("collapses") >= 0.27);
    }

    #[test]
    fn manual_collapse_requires_leaving_before_reexpanding() {
        let mut f = fsm();
        f.notify_manual_collapse();
        assert!(
            run(&mut f, 0.0, 1.0, zone(), false).is_none(),
            "cursor still parked in the zone"
        );
        // Leave the zone, come back: works again.
        run(&mut f, 1.0, 0.05, Some(Vec2::new(0.0, 300.0)), false);
        assert!(run(&mut f, 1.1, 1.0, zone(), false).is_some());
    }

    #[test]
    fn approach_fires_once_and_sets_cadence() {
        let mut f = fsm();
        let near = Some(Vec2::new(150.0, 60.0));
        let (a1, c1) = f.update(&HoverInput::at(0.0, near), false);
        assert_eq!((a1, c1), (HoverAction::Approach, Cadence::Near));
        let (a2, _) = f.update(&HoverInput::at(0.05, near), false);
        assert_eq!(a2, HoverAction::None, "only once per approach");
        let (_, c3) = f.update(&HoverInput::at(0.1, Some(Vec2::new(900.0, 700.0))), false);
        assert_eq!(c3, Cadence::Idle);
        // After leaving entirely, a new approach fires again.
        assert_eq!(
            f.update(&HoverInput::at(0.2, near), false).0,
            HoverAction::Approach
        );
        let (_, c) = f.update(&HoverInput::at(0.3, zone()), false);
        assert_eq!(c, Cadence::Dwell);
        let (_, off) = f.update(&HoverInput::at(0.4, None), false);
        assert_eq!(off, Cadence::Idle, "cursor on another monitor is idle");
    }

    #[test]
    fn drag_toward_the_top_arms_the_shelf_and_releasing_disarms() {
        let mut f = fsm();
        let drag_pt = Some(Vec2::new(80.0, 90.0));
        let mut i = HoverInput::at(0.0, drag_pt);
        i.button_down = true;
        i.drag_likely = true;
        assert_eq!(f.update(&i, false).0, HoverAction::ArmDrag);
        assert!(f.is_drag_armed());
        i.now = 0.1;
        assert_eq!(
            f.update(&i, true).0,
            HoverAction::None,
            "stays armed while dragging inside the zone"
        );
        i.button_down = false;
        i.now = 0.2;
        assert_eq!(f.update(&i, true).0, HoverAction::DisarmDrag);
        assert!(!f.is_drag_armed());
    }

    #[test]
    fn drag_that_strays_disarms_and_plain_button_holds_do_not_arm() {
        let mut f = fsm();
        let mut i = HoverInput::at(0.0, Some(Vec2::new(0.0, 50.0)));
        i.button_down = true;
        i.drag_likely = true;
        assert_eq!(f.update(&i, false).0, HoverAction::ArmDrag);
        i.cursor = Some(Vec2::new(800.0, 600.0));
        i.now = 0.1;
        assert_eq!(
            f.update(&i, true).0,
            HoverAction::None,
            "tolerates a brief stray"
        );
        i.now = 0.6;
        assert_eq!(f.update(&i, true).0, HoverAction::DisarmDrag);

        // A held button that is not a drag (drag_likely false) never arms.
        let mut g = fsm();
        let mut j = HoverInput::at(0.0, Some(Vec2::new(0.0, 50.0)));
        j.button_down = true;
        assert_ne!(g.update(&j, false).0, HoverAction::ArmDrag);
        // Neither does a move/size loop (dragging a window title bar).
        j.drag_likely = true;
        j.move_size_active = true;
        assert_ne!(g.update(&j, false).0, HoverAction::ArmDrag);
    }
}
