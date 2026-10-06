//! The shell state machine: what the notch *is* at any moment and where each animated property is
//! heading. Pure and time-injected; the platform feeds it triggers and a clock and renders
//! [`Shell::frame`].
//!
//! Design points that matter for the "premium feel":
//!
//! * Width, height, corner radius, ears and vertical position are **independent springs**, each with
//!   its own response, so the shape does not move as one rigid thing.
//! * **Stagger**: expanding moves the shape first and lets the content spring in afterwards;
//!   collapsing clears the content first and lets the shape follow.
//! * Every transition is a *retarget*: springs keep position and velocity, so changing your mind
//!   mid-flight (hover out while still opening, scroll twice quickly) bends the motion.
//! * `wake()` re-bases the clock when the shell goes from idle to animating, so the first frame after
//!   a long idle starts moving immediately instead of jumping by the idle duration.

use crate::geom::{Rect, Size};
use crate::input::WheelGesture;
use crate::path::NotchShape;
use crate::spring::{Spring, SpringParams};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    /// Stepped aside (fullscreen app, paused, locked): slid off-screen and not drawn.
    Hidden,
    Collapsed,
    /// A transient banner (e.g. a notification) shown without a full expansion.
    Peek,
    Expanded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Hover,
    Hotkey,
    Attention,
    Drag,
    Api,
}

#[derive(Clone, Debug)]
pub struct ShellConfig {
    /// Idle pill size.
    pub collapsed: Size,
    /// Height of the pill while it carries chips (live activity icons).
    pub chip_height: f32,
    pub radius_collapsed: f32,
    pub radius_expanded: f32,
    pub ear_collapsed: f32,
    pub ear_expanded: f32,
    pub smoothing: f32,
    /// Gap between the screen's top edge and the shape. `0` is the flush "notch"; `> 0` is the
    /// floating "island" (no ears, all four corners rounded).
    pub floating_gap: f32,
    /// Show the idle pill at all. When false it grows out of the top edge on demand.
    pub idle_visible: bool,
    pub speed: f32,
    /// `0..=1`; higher means more overshoot.
    pub bounciness: f32,
    pub stagger_in: f64,
    pub stagger_out: f64,
    pub stagger_switch: f64,
    /// `true`: no spring motion; geometry snaps and content does a short fade.
    pub reduce_motion: bool,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            collapsed: Size::new(112.0, 6.0),
            chip_height: 30.0,
            radius_collapsed: 10.0,
            radius_expanded: 26.0,
            ear_collapsed: 4.0,
            ear_expanded: 12.0,
            smoothing: 0.6,
            floating_gap: 0.0,
            idle_visible: true,
            speed: 1.0,
            bounciness: 0.5,
            stagger_in: 0.070,
            stagger_out: 0.040,
            stagger_switch: 0.045,
            reduce_motion: false,
        }
    }
}

/// What the content layer is showing. Kept after a collapse starts until the fade-out settles, so
/// the content is drawn fading away rather than vanishing the instant the state flips.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ContentKind {
    None,
    Page,
    Peek { owner: u32, size: Size },
}

#[derive(Clone, Copy, Debug)]
struct Peek {
    owner: u32,
    size: Size,
    until: f64,
}

/// A snapshot of everything a renderer needs. Cheap to copy.
#[derive(Clone, Copy, Debug)]
pub struct ShellFrame {
    pub visible: bool,
    pub shape: NotchShape,
    /// Vertical offset of the shape from the top of the window (negative = above the screen edge).
    pub y: f32,
    pub presence: Presence,
    /// Index of the page being shown (valid when expanded).
    pub page: usize,
    /// 0..=1 opacity/scale driver of the current page's content.
    pub content: f32,
    /// Page that is fading out after a switch, with its remaining opacity.
    pub outgoing: Option<(usize, f32)>,
    /// Opacity of the collapsed-pill chips.
    pub chip_alpha: f32,
    /// Owner id of the active peek, if any.
    pub peek_owner: Option<u32>,
    /// What the content layer shows right now (may outlive `presence` while it fades out).
    pub content_kind: ContentKind,
    /// The size the shape is heading to.
    pub nominal: Size,
    /// The size the *content* is laid out for: the destination size of the page/peek, *not* the
    /// animated one, so text never stretches or reflows while the shape springs (it is clipped to
    /// the shape instead).
    pub content_size: Size,
    /// Whether the window should accept the mouse (otherwise it must be click-through).
    pub interactive: bool,
}

#[derive(Debug)]
pub struct Shell {
    cfg: ShellConfig,
    pages: Vec<Size>,
    page: usize,
    prev_page: Option<usize>,
    presence: Presence,
    trigger: Trigger,
    sticky: bool,
    peek: Option<Peek>,
    content_kind: ContentKind,
    pointer_inside: bool,
    chip_w: f32,

    w: Spring,
    h: Spring,
    rb: Spring,
    ear: Spring,
    y: Spring,
    content: Spring,
    content_in: SpringParams,
    content_out: SpringParams,
    out_alpha: Spring,
    chip: Spring,

    content_at: Option<(f64, f32)>,
    shape_at: Option<f64>,
    t_last: f64,

    gesture: WheelGesture,
}

impl Shell {
    pub fn new(cfg: ShellConfig) -> Self {
        let mut s = Self {
            pages: Vec::new(),
            page: 0,
            prev_page: None,
            presence: Presence::Collapsed,
            trigger: Trigger::Api,
            sticky: false,
            peek: None,
            content_kind: ContentKind::None,
            pointer_inside: false,
            chip_w: 0.0,
            w: Spring::new(0.0, SpringParams::new(1.0, 1.0)),
            h: Spring::new(0.0, SpringParams::new(1.0, 1.0)),
            rb: Spring::new(0.0, SpringParams::new(1.0, 1.0)),
            ear: Spring::new(0.0, SpringParams::new(1.0, 1.0)),
            y: Spring::new(0.0, SpringParams::new(1.0, 1.0)),
            content: Spring::new(0.0, SpringParams::new(1.0, 1.0)),
            content_in: SpringParams::new(1.0, 1.0),
            content_out: SpringParams::new(1.0, 1.0),
            out_alpha: Spring::new(0.0, SpringParams::new(1.0, 1.0)),
            chip: Spring::new(0.0, SpringParams::new(1.0, 1.0)),
            content_at: None,
            shape_at: None,
            t_last: 0.0,
            gesture: WheelGesture::default(),
            cfg,
        };
        s.apply_spring_params();
        s.retarget_shape();
        s.snap_to_targets();
        s
    }

    // ----- configuration ----------------------------------------------------------------------

    pub fn config(&self) -> &ShellConfig {
        &self.cfg
    }

    pub fn set_config(&mut self, cfg: ShellConfig) {
        self.cfg = cfg;
        self.apply_spring_params();
        self.retarget_shape();
        if self.cfg.reduce_motion {
            self.snap_to_targets();
        }
    }

    pub fn gesture_mut(&mut self) -> &mut WheelGesture {
        &mut self.gesture
    }

    fn apply_spring_params(&mut self) {
        let c = &self.cfg;
        // Base feel (bounciness 0.5): slightly underdamped width/height/radius so panels settle with a
        // small overshoot; position and content are critically damped (no bounce on a slide or a fade).
        let tune = |omega: f32, zeta: f32| {
            let z = (zeta + (0.5 - c.bounciness) * 0.5).clamp(0.35, 1.3);
            SpringParams::new(omega * c.speed, z)
        };
        let crit = |omega: f32| SpringParams::new(omega * c.speed, 1.0);
        self.w.set_params(tune(21.0, 0.78));
        self.h.set_params(tune(22.5, 0.74));
        self.rb.set_params(tune(30.0, 0.70));
        self.ear.set_params(crit(30.0));
        self.y.set_params(crit(26.0));
        self.content_in = if c.reduce_motion {
            SpringParams::new(55.0, 1.0)
        } else {
            crit(27.0)
        };
        // Content leaves faster than it arrives so it is (almost) gone before the shape shrinks
        // under it; otherwise clipped text is visible mid-collapse.
        self.content_out = if c.reduce_motion {
            SpringParams::new(70.0, 1.0)
        } else {
            crit(60.0)
        };
        self.content.set_params(self.content_in);
        self.out_alpha
            .set_params(SpringParams::new(42.0 * c.speed.max(0.5), 1.0));
        self.chip.set_params(crit(30.0));
        // Opacity-like springs need tight rest tolerances, geometry is fine at 0.01 DIP.
        self.content = self.content.with_rest(0.002, 0.02);
        self.out_alpha = self.out_alpha.with_rest(0.002, 0.02);
        self.chip = self.chip.with_rest(0.002, 0.02);
    }

    // ----- pages ------------------------------------------------------------------------------

    /// Declare the expanded size of each page (index = page).
    pub fn set_pages(&mut self, pages: Vec<Size>) {
        self.pages = pages;
        if self.page >= self.pages.len() {
            self.page = 0;
            self.prev_page = None;
        }
        self.retarget_shape();
    }

    pub fn page(&self) -> usize {
        self.page
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn set_page(&mut self, now: f64, idx: usize) {
        if idx >= self.pages.len() || idx == self.page {
            return;
        }
        self.wake(now);
        if self.presence == Presence::Expanded {
            self.prev_page = Some(self.page);
            self.out_alpha.snap(self.content.value());
            self.out_alpha.set_target(0.0);
            self.content.snap(0.0);
            self.content.set_params(self.content_in);
            let delay = if self.cfg.reduce_motion {
                0.0
            } else {
                self.cfg.stagger_switch
            };
            self.content_at = Some((now + delay, 1.0));
        }
        self.page = idx;
        self.retarget_shape();
    }

    /// Feed a wheel event. Returns the new page index if it changed.
    pub fn scroll(&mut self, now: f64, dx: f32, dy: f32) -> Option<usize> {
        if self.presence != Presence::Expanded || self.pages.len() < 2 {
            return None;
        }
        let dir = self.gesture.feed(now, dx, dy)?;
        let n = self.pages.len() as i32;
        let next = (self.page as i32 + dir).rem_euclid(n) as usize;
        self.set_page(now, next);
        Some(next)
    }

    // ----- chips ------------------------------------------------------------------------------

    /// Width needed by the collapsed-pill chips (0 = none).
    pub fn set_chip_width(&mut self, now: f64, w: f32) {
        if (self.chip_w - w).abs() < 0.01 {
            return;
        }
        self.wake(now);
        self.chip_w = w.max(0.0);
        self.retarget_shape();
    }

    // ----- triggers ---------------------------------------------------------------------------

    pub fn presence(&self) -> Presence {
        self.presence
    }

    pub fn is_expanded(&self) -> bool {
        self.presence == Presence::Expanded
    }

    /// Hotkey/API expansions stay open until toggled; hover expansions close when the pointer leaves.
    pub fn is_sticky(&self) -> bool {
        self.sticky
    }

    pub fn expand(&mut self, now: f64, trigger: Trigger) {
        if self.presence == Presence::Hidden || self.pages.is_empty() {
            return;
        }
        self.wake(now);
        let was = self.presence;
        self.presence = Presence::Expanded;
        self.trigger = trigger;
        self.sticky = matches!(trigger, Trigger::Hotkey | Trigger::Api);
        self.peek = None;
        self.shape_at = None;
        self.content_kind = ContentKind::Page;
        self.content.set_params(self.content_in);
        self.retarget_shape();
        match was {
            Presence::Expanded => {}
            Presence::Peek => {
                // Content is already visible; keep it.
                self.content_at = None;
                self.content.set_target(1.0);
            }
            _ => {
                let delay = if self.cfg.reduce_motion {
                    0.0
                } else {
                    self.cfg.stagger_in
                };
                self.content_at = Some((now + delay, 1.0));
            }
        }
    }

    pub fn collapse(&mut self, now: f64) {
        if matches!(self.presence, Presence::Collapsed | Presence::Hidden) {
            return;
        }
        self.wake(now);
        self.presence = Presence::Collapsed;
        self.peek = None;
        self.sticky = false;
        self.pointer_inside = false;
        self.content_at = None;
        self.content.set_params(self.content_out);
        self.content.set_target(0.0);
        let delay = if self.cfg.reduce_motion {
            0.0
        } else {
            self.cfg.stagger_out
        };
        if delay > 0.0 {
            self.shape_at = Some(now + delay);
        } else {
            self.shape_at = None;
            self.retarget_shape();
        }
    }

    pub fn toggle(&mut self, now: f64) {
        if self.presence == Presence::Expanded {
            self.collapse(now);
        } else {
            self.expand(now, Trigger::Hotkey);
        }
    }

    /// Show a transient banner of `size` for `duration` seconds. Ignored while expanded or hidden.
    pub fn peek(&mut self, now: f64, owner: u32, size: Size, duration: f64) {
        if matches!(self.presence, Presence::Expanded | Presence::Hidden) {
            return;
        }
        self.wake(now);
        let was_peek = self.presence == Presence::Peek;
        self.presence = Presence::Peek;
        self.peek = Some(Peek {
            owner,
            size,
            until: now + duration,
        });
        self.content_kind = ContentKind::Peek { owner, size };
        self.shape_at = None;
        self.content.set_params(self.content_in);
        self.retarget_shape();
        if !was_peek {
            let delay = if self.cfg.reduce_motion {
                0.0
            } else {
                self.cfg.stagger_in * 0.6
            };
            self.content_at = Some((now + delay, 1.0));
        }
    }

    /// Tell the shell whether the pointer is over a peek banner (extends it and makes it clickable).
    pub fn set_pointer_inside(&mut self, now: f64, inside: bool) {
        if self.pointer_inside == inside {
            return;
        }
        self.pointer_inside = inside;
        if let (true, Some(p)) = (inside, self.peek.as_mut()) {
            p.until = p.until.max(now + 1.0);
        }
    }

    /// Step aside (fullscreen app, paused, locked) or come back.
    pub fn set_suspended(&mut self, now: f64, suspended: bool) {
        if suspended {
            if self.presence == Presence::Hidden {
                return;
            }
            self.wake(now);
            self.presence = Presence::Hidden;
            self.peek = None;
            self.sticky = false;
            self.content_at = None;
            self.content.set_params(self.content_out);
            self.content.set_target(0.0);
            self.shape_at = None;
            self.retarget_shape();
        } else if self.presence == Presence::Hidden {
            self.wake(now);
            self.presence = Presence::Collapsed;
            self.retarget_shape();
        }
    }

    /// Hide *immediately*, without animating (fullscreen app, session lock, display off). Nothing
    /// may start a GPU frame at the moment a game takes over the screen.
    pub fn suspend_now(&mut self, now: f64) {
        self.presence = Presence::Hidden;
        self.peek = None;
        self.sticky = false;
        self.pointer_inside = false;
        self.content_at = None;
        self.shape_at = None;
        self.prev_page = None;
        self.content_kind = ContentKind::None;
        self.retarget_shape();
        self.content.set_target(0.0);
        self.chip.set_target(0.0);
        self.out_alpha.set_target(0.0);
        self.snap_to_targets();
        self.t_last = now;
    }

    /// Come back *immediately* as the collapsed pill (no slide-in).
    pub fn resume_now(&mut self, now: f64) {
        if self.presence != Presence::Hidden {
            return;
        }
        self.presence = Presence::Collapsed;
        self.retarget_shape();
        self.snap_to_targets();
        self.t_last = now;
    }

    // ----- time -------------------------------------------------------------------------------

    /// Re-base the clock when going from idle to animating (see module docs).
    fn wake(&mut self, now: f64) {
        if !self.animating() {
            self.t_last = now;
        }
    }

    /// Advance to monotonic time `t` (seconds). `t` should be the *display* time of the frame being
    /// prepared (see [`crate::frame::sample_time`]).
    pub fn step(&mut self, t: f64) {
        let dt = (t - self.t_last).max(0.0) as f32;
        self.t_last = self.t_last.max(t);
        self.run_pending(t);
        for s in [
            &mut self.w,
            &mut self.h,
            &mut self.rb,
            &mut self.ear,
            &mut self.y,
            &mut self.content,
            &mut self.out_alpha,
            &mut self.chip,
        ] {
            s.step(dt);
        }
        if self.prev_page.is_some() && self.out_alpha.is_settled() {
            self.prev_page = None;
        }
        if self.content_kind != ContentKind::None
            && !matches!(self.presence, Presence::Expanded | Presence::Peek)
            && self.content.value() <= 0.001
            && self.content.is_settled()
        {
            self.content_kind = ContentKind::None;
        }
    }

    fn run_pending(&mut self, t: f64) {
        if let Some((at, v)) = self.content_at
            && t >= at
        {
            self.content.set_target(v);
            self.content_at = None;
        }
        if let Some(at) = self.shape_at
            && t >= at
        {
            self.shape_at = None;
            self.retarget_shape();
        }
        if let Some(p) = self.peek
            && t >= p.until
            && !self.pointer_inside
        {
            self.collapse(t);
        }
    }

    /// True while frames are needed: some spring is moving or a staggered action is pending.
    pub fn animating(&self) -> bool {
        !self.springs_settled()
            || self.content_at.is_some()
            || self.shape_at.is_some()
            || self.prev_page.is_some()
            || self.content_kind != ContentKind::None
                && !matches!(self.presence, Presence::Expanded | Presence::Peek)
    }

    fn springs_settled(&self) -> bool {
        [
            &self.w,
            &self.h,
            &self.rb,
            &self.ear,
            &self.y,
            &self.content,
            &self.out_alpha,
            &self.chip,
        ]
        .iter()
        .all(|s| s.is_settled())
    }

    /// Earliest time something needs attention even if nothing is animating (peek expiry etc.).
    pub fn next_deadline(&self) -> Option<f64> {
        [
            self.content_at.map(|c| c.0),
            self.shape_at,
            self.peek.map(|p| p.until),
        ]
        .into_iter()
        .flatten()
        .min_by(|a, b| a.total_cmp(b))
    }

    // ----- targets ----------------------------------------------------------------------------

    fn collapsed_size(&self) -> Size {
        if self.chip_w > 0.0 {
            Size::new(self.chip_w.max(self.cfg.collapsed.w), self.cfg.chip_height)
        } else if self.cfg.idle_visible {
            self.cfg.collapsed
        } else {
            // Nothing to show: shrink to nothing so the notch grows out of the screen edge later.
            Size::new(self.cfg.collapsed.w * 0.5, 0.0)
        }
    }

    fn retarget_shape(&mut self) {
        let c = &self.cfg;
        let floating = c.floating_gap > 0.0;
        let collapsed = self.collapsed_size();
        let (size, rb, ear) = match self.presence {
            Presence::Hidden | Presence::Collapsed => {
                (collapsed, c.radius_collapsed, c.ear_collapsed)
            }
            Presence::Peek => (
                self.peek.map(|p| p.size).unwrap_or(collapsed),
                c.radius_expanded * 0.85,
                c.ear_expanded * 0.85,
            ),
            Presence::Expanded => (
                self.pages.get(self.page).copied().unwrap_or(collapsed),
                c.radius_expanded,
                c.ear_expanded,
            ),
        };
        let y = if self.presence == Presence::Hidden {
            -(size.h + c.floating_gap + 10.0)
        } else {
            c.floating_gap
        };
        let ear = if floating { 0.0 } else { ear };
        self.w.set_target(size.w);
        self.h.set_target(size.h);
        self.rb.set_target(rb);
        self.ear.set_target(ear);
        self.y.set_target(y);

        let chips_on = matches!(self.presence, Presence::Collapsed) && self.chip_w > 0.0;
        self.chip.set_target(if chips_on { 1.0 } else { 0.0 });

        if self.cfg.reduce_motion {
            for s in [
                &mut self.w,
                &mut self.h,
                &mut self.rb,
                &mut self.ear,
                &mut self.y,
            ] {
                let t = s.target();
                s.snap(t);
            }
        }
    }

    fn snap_to_targets(&mut self) {
        for s in [
            &mut self.w,
            &mut self.h,
            &mut self.rb,
            &mut self.ear,
            &mut self.y,
            &mut self.content,
            &mut self.out_alpha,
            &mut self.chip,
        ] {
            let t = s.target();
            s.snap(t);
        }
    }

    // ----- output -----------------------------------------------------------------------------

    fn nominal_size(&self) -> Size {
        match self.presence {
            Presence::Expanded => self.pages.get(self.page).copied().unwrap_or_default(),
            Presence::Peek => self.peek.map(|p| p.size).unwrap_or_default(),
            _ => self.collapsed_size(),
        }
    }

    pub fn interactive(&self) -> bool {
        match self.presence {
            Presence::Expanded => true,
            Presence::Peek => self.pointer_inside,
            _ => false,
        }
    }

    pub fn frame(&self) -> ShellFrame {
        let floating = self.cfg.floating_gap > 0.0;
        let rb = self.rb.value().max(0.0);
        let shape = NotchShape {
            w: self.w.value().max(0.0),
            h: self.h.value().max(0.0),
            radius_top: if floating { rb } else { 0.0 },
            radius_bottom: rb,
            ear: self.ear.value().max(0.0),
            smoothing: self.cfg.smoothing,
        };
        ShellFrame {
            visible: !(self.presence == Presence::Hidden && self.y.is_settled()),
            shape,
            y: self.y.value(),
            presence: self.presence,
            page: self.page,
            content: self.content.value().clamp(0.0, 1.0),
            outgoing: self
                .prev_page
                .map(|p| (p, self.out_alpha.value().clamp(0.0, 1.0))),
            chip_alpha: self.chip.value().clamp(0.0, 1.0),
            peek_owner: self.peek.map(|p| p.owner),
            content_kind: self.content_kind,
            nominal: self.nominal_size(),
            content_size: match self.content_kind {
                ContentKind::Page => self.pages.get(self.page).copied().unwrap_or_default(),
                ContentKind::Peek { size, .. } => size,
                ContentKind::None => Size::default(),
            },
            interactive: self.interactive(),
        }
    }

    /// Where the shape currently is, in window coordinates, for a window `window_w` DIPs wide.
    pub fn current_rect(&self, window_w: f32) -> Rect {
        let f = self.frame();
        Rect::new((window_w - f.shape.w) * 0.5, f.y, f.shape.w, f.shape.h)
    }

    /// Where the shape is *heading* (for hit-test regions that must cover the whole animation).
    pub fn target_rect(&self, window_w: f32) -> Rect {
        let w = self.w.target();
        Rect::new((window_w - w) * 0.5, self.y.target(), w, self.h.target())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f64 = 1.0 / 60.0;

    fn shell() -> Shell {
        let mut s = Shell::new(ShellConfig::default());
        s.set_pages(vec![
            Size::new(340.0, 130.0),
            Size::new(380.0, 210.0),
            Size::new(300.0, 90.0),
        ]);
        s
    }

    /// Run frames until the shell stops animating; returns (time elapsed, frames).
    fn settle(s: &mut Shell, mut t: f64) -> (f64, usize) {
        let t0 = t;
        let mut frames = 0;
        while s.animating() {
            t += DT;
            s.step(t);
            frames += 1;
            assert!(frames < 600, "never settled");
        }
        (t - t0, frames)
    }

    #[test]
    fn starts_collapsed_and_idle() {
        let s = shell();
        let f = s.frame();
        assert_eq!(f.presence, Presence::Collapsed);
        assert!(f.visible && !f.interactive);
        assert_eq!((f.shape.w, f.shape.h), (112.0, 6.0));
        assert!(!s.animating(), "an idle notch must not request frames");
        assert_eq!(
            s.next_deadline(),
            None,
            "and must not need a wake-up either"
        );
    }

    #[test]
    fn expand_grows_the_shape_before_the_content_appears() {
        let mut s = shell();
        s.expand(1.0, Trigger::Hover);
        assert!(s.animating());
        let mut t = 1.0;
        let mut first_content = None;
        let mut first_shape = None;
        for k in 0..120 {
            t += DT;
            s.step(t);
            let f = s.frame();
            if first_shape.is_none() && f.shape.h > 8.0 {
                first_shape = Some(k);
            }
            if first_content.is_none() && f.content > 0.02 {
                first_content = Some(k);
            }
        }
        let (fs, fc) = (first_shape.unwrap(), first_content.unwrap());
        assert!(
            fs < fc,
            "shape moves first: shape frame {fs}, content frame {fc}"
        );
        assert!(
            fc - fs <= 8,
            "but content follows closely (stagger, not delay)"
        );
        let f = s.frame();
        assert_eq!((f.shape.w, f.shape.h), (340.0, 130.0));
        assert_eq!(f.content, 1.0);
        assert!(f.interactive);
        assert!(!s.animating(), "settled: the frame loop can sleep");
    }

    #[test]
    fn first_frame_after_a_long_idle_starts_from_the_trigger_not_from_the_idle_duration() {
        let mut s = shell();
        s.expand(1000.0, Trigger::Hover); // 1000 s after construction
        s.step(1000.0 + DT);
        let f = s.frame();
        assert!(
            f.shape.h < 20.0 && f.shape.h > 6.0,
            "moved one frame's worth, not teleported: {}",
            f.shape.h
        );
    }

    #[test]
    fn collapse_clears_content_first_then_the_shape_follows() {
        let mut s = shell();
        s.expand(0.0, Trigger::Hotkey);
        settle(&mut s, 0.0);
        let t0 = 5.0;
        s.collapse(t0);
        let mut t = t0;
        let mut first_content_drop = None;
        let mut first_shape_drop = None;
        let mut content_when_shape_visibly_shrinks = None;
        for k in 0..120 {
            t += DT;
            s.step(t);
            let f = s.frame();
            if first_content_drop.is_none() && f.content < 0.95 {
                first_content_drop = Some(k);
            }
            if first_shape_drop.is_none() && f.shape.h < 129.0 {
                first_shape_drop = Some(k);
            }
            // 5 % shorter than the expanded height: from here the shape would start clipping text.
            if content_when_shape_visibly_shrinks.is_none() && f.shape.h < 130.0 * 0.95 {
                content_when_shape_visibly_shrinks = Some(f.content);
            }
        }
        assert!(
            first_content_drop.unwrap() < first_shape_drop.unwrap(),
            "content starts leaving before the shape moves"
        );
        let c = content_when_shape_visibly_shrinks.unwrap();
        assert!(
            c < 0.25,
            "content is already nearly gone ({c}) when the shape starts clipping it"
        );
        let f = s.frame();
        assert_eq!((f.shape.w, f.shape.h), (112.0, 6.0));
        assert!(!f.interactive);
    }

    #[test]
    fn content_stays_on_screen_while_it_fades_out_after_collapse() {
        let mut s = shell();
        s.expand(0.0, Trigger::Hotkey);
        settle(&mut s, 0.0);
        s.collapse(5.0);
        s.step(5.0 + DT);
        let f = s.frame();
        assert_eq!(f.presence, Presence::Collapsed);
        assert_eq!(f.content_kind, ContentKind::Page, "still drawn, fading");
        assert!(f.content > 0.0 && f.content < 1.0);
        assert_eq!(
            f.content_size,
            Size::new(340.0, 130.0),
            "keeps its expanded layout"
        );
        settle(&mut s, 5.0 + DT);
        assert_eq!(
            s.frame().content_kind,
            ContentKind::None,
            "retired once invisible"
        );
        assert!(!s.animating());
    }

    #[test]
    fn retargeting_midflight_is_continuous() {
        let mut s = shell();
        s.expand(0.0, Trigger::Hover);
        let mut t = 0.0;
        for _ in 0..8 {
            t += DT;
            s.step(t);
        }
        let before = s.frame().shape;
        s.collapse(t);
        t += 0.001;
        s.step(t);
        let after = s.frame().shape;
        assert!(
            (after.w - before.w).abs() < 8.0 && (after.h - before.h).abs() < 8.0,
            "no teleport: {before:?} -> {after:?}"
        );
        settle(&mut s, t);
        assert_eq!(s.frame().shape.h, 6.0);
    }

    #[test]
    fn overshoot_is_small_and_bounded() {
        let mut s = shell();
        s.expand(0.0, Trigger::Hover);
        let mut t = 0.0;
        let mut max_w: f32 = 0.0;
        while s.animating() {
            t += DT;
            s.step(t);
            max_w = max_w.max(s.frame().shape.w);
        }
        assert!(max_w >= 340.0, "underdamped width overshoots a little");
        assert!(max_w < 340.0 * 1.06, "but not wildly: {max_w}");
    }

    #[test]
    fn page_switch_crossfades_and_resizes() {
        let mut s = shell();
        s.expand(0.0, Trigger::Hotkey);
        let (_, _) = settle(&mut s, 0.0);
        s.set_page(5.0, 1);
        let f = s.frame();
        assert_eq!(f.page, 1);
        assert_eq!(f.outgoing.map(|o| o.0), Some(0));
        assert!(
            (f.outgoing.unwrap().1 - 1.0).abs() < 1e-3,
            "outgoing starts opaque"
        );
        assert_eq!(f.content, 0.0, "incoming starts transparent");
        let mut t = 5.0;
        while s.animating() {
            t += DT;
            s.step(t);
        }
        let f = s.frame();
        assert_eq!((f.shape.w, f.shape.h), (380.0, 210.0));
        assert!(f.outgoing.is_none() && f.content == 1.0);
    }

    #[test]
    fn scroll_wraps_around_and_only_works_when_expanded() {
        let mut s = shell();
        assert_eq!(
            s.scroll(0.0, 0.0, -120.0),
            None,
            "collapsed: scroll does nothing"
        );
        s.expand(0.0, Trigger::Hotkey);
        assert_eq!(s.scroll(1.0, 0.0, -120.0), Some(1));
        assert_eq!(s.scroll(2.0, 0.0, -120.0), Some(2));
        assert_eq!(s.scroll(3.0, 0.0, -120.0), Some(0), "wraps");
        assert_eq!(s.scroll(4.0, 0.0, 120.0), Some(2), "and wraps backwards");
    }

    #[test]
    fn hover_expansion_is_not_sticky_but_hotkey_is() {
        let mut s = shell();
        s.expand(0.0, Trigger::Hover);
        assert!(!s.is_sticky());
        s.collapse(1.0);
        s.expand(2.0, Trigger::Hotkey);
        assert!(s.is_sticky());
    }

    #[test]
    fn suspend_hides_and_blocks_expansion_until_resumed() {
        let mut s = shell();
        s.set_suspended(0.0, true);
        settle(&mut s, 0.0);
        let f = s.frame();
        assert!(!f.visible, "slid away and not drawn");
        assert!(f.y < 0.0);
        s.expand(10.0, Trigger::Hotkey);
        assert_eq!(
            s.presence(),
            Presence::Hidden,
            "fullscreen app: hotkey/hover cannot open it"
        );
        s.peek(10.0, 1, Size::new(200.0, 50.0), 3.0);
        assert_eq!(s.presence(), Presence::Hidden);
        s.set_suspended(11.0, false);
        settle(&mut s, 11.0);
        let f = s.frame();
        assert!(f.visible && f.presence == Presence::Collapsed);
        assert_eq!(f.y, 0.0);
    }

    #[test]
    fn instant_suspend_and_resume_never_request_frames() {
        let mut s = shell();
        s.expand(0.0, Trigger::Hotkey);
        s.step(0.1); // mid-animation when the game launches
        assert!(s.animating());
        s.suspend_now(0.1);
        assert!(!s.animating(), "no animation, no frames");
        let f = s.frame();
        assert!(
            !f.visible && f.presence == Presence::Hidden && f.content_kind == ContentKind::None
        );
        s.resume_now(5.0);
        assert!(!s.animating());
        let f = s.frame();
        assert!(f.visible && f.presence == Presence::Collapsed);
        assert_eq!((f.shape.w, f.shape.h, f.y), (112.0, 6.0, 0.0));
        // Resume is a no-op unless actually hidden.
        s.expand(6.0, Trigger::Hotkey);
        s.resume_now(6.1);
        assert_eq!(s.presence(), Presence::Expanded);
    }

    #[test]
    fn peek_expires_and_pointer_keeps_it_alive() {
        let mut s = shell();
        s.peek(0.0, 7, Size::new(320.0, 64.0), 2.0);
        assert_eq!(s.frame().peek_owner, Some(7));
        assert!(
            !s.frame().interactive,
            "banner is click-through until hovered"
        );
        // The only deadlines are the content stagger and the expiry.
        s.step(0.1);
        assert_eq!(
            s.next_deadline().map(|d| d.round()),
            Some(2.0),
            "then just the expiry"
        );
        s.set_pointer_inside(1.5, true);
        assert!(s.frame().interactive);
        let mut t = 0.0;
        while t < 2.6 {
            t += DT;
            s.step(t);
        }
        assert_eq!(
            s.presence(),
            Presence::Peek,
            "hovered peek outlives its timer"
        );
        s.set_pointer_inside(t, false);
        while t < 4.5 {
            t += DT;
            s.step(t);
        }
        assert_eq!(s.presence(), Presence::Collapsed);
    }

    #[test]
    fn expanding_from_a_peek_keeps_content_visible() {
        let mut s = shell();
        s.peek(0.0, 1, Size::new(320.0, 64.0), 5.0);
        let mut t = 0.0;
        while s.frame().content < 0.99 {
            t += DT;
            s.step(t);
        }
        s.expand(t, Trigger::Hover);
        t += DT;
        s.step(t);
        assert!(
            s.frame().content > 0.9,
            "no flicker when promoting a banner to a panel"
        );
    }

    #[test]
    fn reduce_motion_snaps_geometry_without_overshoot() {
        let mut s = Shell::new(ShellConfig {
            reduce_motion: true,
            ..Default::default()
        });
        s.set_pages(vec![Size::new(340.0, 130.0)]);
        s.expand(0.0, Trigger::Hover);
        let f = s.frame();
        assert_eq!(
            (f.shape.w, f.shape.h),
            (340.0, 130.0),
            "geometry is instant"
        );
        let mut t = 0.0;
        let mut max_content: f32 = 0.0;
        let mut frames = 0;
        while s.animating() {
            t += DT;
            s.step(t);
            max_content = max_content.max(s.frame().content);
            frames += 1;
        }
        assert!(max_content <= 1.0);
        assert!(frames <= 12, "short fade only ({frames} frames)");
    }

    #[test]
    fn chips_grow_the_pill_and_fade_in_only_when_collapsed() {
        let mut s = shell();
        s.set_chip_width(0.0, 190.0);
        let mut t = 0.0;
        while s.animating() {
            t += DT;
            s.step(t);
        }
        let f = s.frame();
        assert_eq!((f.shape.w, f.shape.h), (190.0, 30.0));
        assert_eq!(f.chip_alpha, 1.0);
        s.expand(t, Trigger::Hover);
        t += 0.2;
        s.step(t);
        assert!(s.frame().chip_alpha < 1.0, "chips yield to the panel");
        s.collapse(t);
        settle(&mut s, t);
        assert_eq!(s.frame().chip_alpha, 1.0);
        s.set_chip_width(t + 10.0, 0.0);
        settle(&mut s, t + 10.0);
        assert_eq!((s.frame().shape.w, s.frame().shape.h), (112.0, 6.0));
    }

    #[test]
    fn invisible_idle_pill_grows_out_of_the_edge() {
        let mut s = Shell::new(ShellConfig {
            idle_visible: false,
            ..Default::default()
        });
        s.set_pages(vec![Size::new(340.0, 130.0)]);
        assert_eq!(s.frame().shape.h, 0.0);
        assert!(
            s.frame().shape.to_path().cmds.is_empty(),
            "nothing drawn while idle"
        );
        s.expand(0.0, Trigger::Hover);
        s.step(0.05);
        assert!(s.frame().shape.h > 0.5);
    }

    #[test]
    fn floating_island_style_rounds_the_top_and_drops_the_ears() {
        let mut s = Shell::new(ShellConfig {
            floating_gap: 8.0,
            ..Default::default()
        });
        s.set_pages(vec![Size::new(340.0, 130.0)]);
        let f = s.frame();
        assert_eq!(f.y, 8.0);
        assert_eq!(f.shape.ear, 0.0);
        assert_eq!(f.shape.radius_top, f.shape.radius_bottom);
    }

    #[test]
    fn target_rect_covers_the_destination_even_mid_animation() {
        let mut s = shell();
        s.expand(0.0, Trigger::Hotkey);
        s.step(0.02);
        let target = s.target_rect(400.0);
        assert_eq!((target.w, target.h), (340.0, 130.0));
        assert_eq!(target.x, 30.0);
        let now = s.current_rect(400.0);
        assert!(now.w < target.w);
    }

    #[test]
    fn empty_page_list_never_expands() {
        let mut s = Shell::new(ShellConfig::default());
        s.expand(0.0, Trigger::Hotkey);
        assert_eq!(s.presence(), Presence::Collapsed);
    }
}
