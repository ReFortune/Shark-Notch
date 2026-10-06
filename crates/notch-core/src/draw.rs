//! The display list: the *only* thing modules produce and renderers consume.
//!
//! A module never touches pixels or Direct2D. It appends [`DrawCmd`]s and hit regions to a
//! [`DrawList`] through a [`Canvas`]. The Windows backend executes the list with Direct2D; the
//! `notch-preview` tool executes the very same list with tiny-skia, which is how layouts are
//! inspected without a Windows machine.

use std::sync::Arc;

use crate::color::Color;
use crate::geom::{Rect, Vec2};
use crate::icons::Icon;
use crate::path::{NotchShape, Path};
use crate::theme::Theme;

/// Module-defined identifier of an interactive region ("play button", "item 3", ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HitId(pub u32);

/// Key into the platform's decoded-image store (album art, clipboard thumbnails, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImageId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CursorKind {
    #[default]
    Arrow,
    Hand,
    IBeam,
    ResizeH,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Weight {
    #[default]
    Regular,
    Medium,
    SemiBold,
    Bold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextStyle {
    /// Font size in DIPs.
    pub size: f32,
    pub weight: Weight,
    pub align: Align,
    /// Fixed-width digits so counters do not jitter.
    pub tabular: bool,
    /// Truncate with an ellipsis instead of clipping.
    pub ellipsis: bool,
    /// Maximum number of lines (1 = single line, no wrapping).
    pub lines: u8,
}

impl TextStyle {
    pub const fn new(size: f32, weight: Weight) -> Self {
        Self {
            size,
            weight,
            align: Align::Start,
            tabular: false,
            ellipsis: true,
            lines: 1,
        }
    }
    pub const fn title() -> Self {
        Self::new(15.0, Weight::SemiBold)
    }
    pub const fn body() -> Self {
        Self::new(13.0, Weight::Regular)
    }
    pub const fn label() -> Self {
        Self::new(12.0, Weight::Medium)
    }
    pub const fn caption() -> Self {
        Self::new(11.0, Weight::Regular)
    }
    pub const fn big() -> Self {
        Self::new(28.0, Weight::SemiBold)
    }
    pub const fn align(mut self, a: Align) -> Self {
        self.align = a;
        self
    }
    pub const fn tabular(mut self) -> Self {
        self.tabular = true;
        self
    }
    pub const fn lines(mut self, n: u8) -> Self {
        self.lines = n;
        self
    }
}

/// Text that is free to clone per frame: static literals or reference-counted strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Text {
    Static(&'static str),
    Shared(Arc<str>),
}

impl Text {
    pub fn as_str(&self) -> &str {
        match self {
            Text::Static(s) => s,
            Text::Shared(s) => s,
        }
    }
}

impl From<&'static str> for Text {
    fn from(s: &'static str) -> Self {
        Text::Static(s)
    }
}

impl From<Arc<str>> for Text {
    fn from(s: Arc<str>) -> Self {
        Text::Shared(s)
    }
}

impl From<&Arc<str>> for Text {
    fn from(s: &Arc<str>) -> Self {
        Text::Shared(s.clone())
    }
}

impl From<String> for Text {
    fn from(s: String) -> Self {
        Text::Shared(s.into())
    }
}

#[derive(Clone, Debug)]
pub enum DrawCmd {
    /// The notch silhouette itself, with its top-left corner at `origin`.
    Notch {
        shape: NotchShape,
        origin: Vec2,
        fill: Color,
        outline: Option<(f32, Color)>,
    },
    /// Plain circular-corner rounded rectangle (capsules, small buttons).
    RoundRect {
        rect: Rect,
        radius: f32,
        color: Color,
    },
    /// Continuous-corner rectangle with per-corner radii `[tl, tr, br, bl]` (cards, album art).
    Squircle {
        rect: Rect,
        radii: [f32; 4],
        color: Color,
    },
    StrokeRoundRect {
        rect: Rect,
        radius: f32,
        width: f32,
        color: Color,
    },
    Circle {
        center: Vec2,
        radius: f32,
        color: Color,
    },
    /// Arc of a circle drawn as a stroke. Angles in degrees, 0 = 12 o'clock, clockwise.
    Ring {
        center: Vec2,
        radius: f32,
        width: f32,
        start_deg: f32,
        sweep_deg: f32,
        color: Color,
    },
    Line {
        a: Vec2,
        b: Vec2,
        width: f32,
        color: Color,
    },
    Text {
        rect: Rect,
        text: Text,
        style: TextStyle,
        color: Color,
    },
    Icon {
        icon: Icon,
        rect: Rect,
        color: Color,
    },
    Image {
        id: ImageId,
        rect: Rect,
        radius: f32,
        opacity: f32,
    },
    /// A vector path, filled and/or stroked (charts). Coordinates are in the list's space.
    Shape {
        path: Arc<Path>,
        fill: Option<Color>,
        /// Stroke width and colour (round caps and joins).
        stroke: Option<(f32, Color)>,
    },
    /// Two-colour linear gradient inside a rounded rect.
    Gradient {
        rect: Rect,
        radius: f32,
        from: Color,
        to: Color,
        vertical: bool,
    },
    /// Radial glow fading to transparent at `radius`.
    Glow {
        center: Vec2,
        radius: f32,
        color: Color,
    },
    PushClip {
        rect: Rect,
        radius: f32,
    },
    PopClip,
    /// Multiply alpha and scale about `origin` for everything until the matching `PopGroup`.
    PushGroup {
        alpha: f32,
        scale: f32,
        origin: Vec2,
    },
    PopGroup,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HitRegion {
    pub rect: Rect,
    pub id: HitId,
    pub cursor: CursorKind,
    pub draggable: bool,
}

#[derive(Clone, Debug, Default)]
pub struct DrawList {
    pub cmds: Vec<DrawCmd>,
    pub hits: Vec<HitRegion>,
}

impl DrawList {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear but keep capacity, so steady-state frames do not allocate.
    pub fn clear(&mut self) {
        self.cmds.clear();
        self.hits.clear();
    }

    /// Topmost (last registered) region containing `p`.
    pub fn hit_test(&self, p: Vec2) -> Option<&HitRegion> {
        self.hits.iter().rev().find(|h| h.rect.contains(p))
    }

    pub fn is_empty(&self) -> bool {
        self.cmds.is_empty()
    }

    /// Check clip/group push/pop pairs balance (renderers rely on it).
    pub fn is_balanced(&self) -> bool {
        let (mut clip, mut group) = (0i32, 0i32);
        for c in &self.cmds {
            match c {
                DrawCmd::PushClip { .. } => clip += 1,
                DrawCmd::PopClip => clip -= 1,
                DrawCmd::PushGroup { .. } => group += 1,
                DrawCmd::PopGroup => group -= 1,
                _ => {}
            }
            if clip < 0 || group < 0 {
                return false;
            }
        }
        clip == 0 && group == 0
    }
}

/// Builder handed to modules. Thin helpers over [`DrawList`].
pub struct Canvas<'a> {
    pub list: &'a mut DrawList,
    pub theme: &'a Theme,
}

impl<'a> Canvas<'a> {
    pub fn new(list: &'a mut DrawList, theme: &'a Theme) -> Self {
        Self { list, theme }
    }

    pub fn push(&mut self, cmd: DrawCmd) {
        self.list.cmds.push(cmd);
    }

    pub fn round_rect(&mut self, rect: Rect, radius: f32, color: Color) {
        self.push(DrawCmd::RoundRect {
            rect,
            radius,
            color,
        });
    }

    pub fn capsule(&mut self, rect: Rect, color: Color) {
        self.push(DrawCmd::RoundRect {
            rect,
            radius: rect.w.min(rect.h) * 0.5,
            color,
        });
    }

    pub fn squircle(&mut self, rect: Rect, radius: f32, color: Color) {
        self.push(DrawCmd::Squircle {
            rect,
            radii: [radius; 4],
            color,
        });
    }

    pub fn circle(&mut self, center: Vec2, radius: f32, color: Color) {
        self.push(DrawCmd::Circle {
            center,
            radius,
            color,
        });
    }

    pub fn text(&mut self, rect: Rect, text: impl Into<Text>, style: TextStyle, color: Color) {
        self.push(DrawCmd::Text {
            rect,
            text: text.into(),
            style,
            color,
        });
    }

    pub fn icon(&mut self, icon: Icon, rect: Rect, color: Color) {
        self.push(DrawCmd::Icon { icon, rect, color });
    }

    pub fn image(&mut self, id: ImageId, rect: Rect, radius: f32) {
        self.push(DrawCmd::Image {
            id,
            rect,
            radius,
            opacity: 1.0,
        });
    }

    pub fn push_clip(&mut self, rect: Rect, radius: f32) {
        self.push(DrawCmd::PushClip { rect, radius });
    }

    pub fn pop_clip(&mut self) {
        self.push(DrawCmd::PopClip);
    }

    pub fn push_group(&mut self, alpha: f32, scale: f32, origin: Vec2) {
        self.push(DrawCmd::PushGroup {
            alpha,
            scale,
            origin,
        });
    }

    pub fn pop_group(&mut self) {
        self.push(DrawCmd::PopGroup);
    }

    /// Register an interactive region.
    pub fn hit(&mut self, rect: Rect, id: HitId, cursor: CursorKind) {
        self.list.hits.push(HitRegion {
            rect,
            id,
            cursor,
            draggable: false,
        });
    }

    pub fn hit_drag(&mut self, rect: Rect, id: HitId) {
        self.list.hits.push(HitRegion {
            rect,
            id,
            cursor: CursorKind::Hand,
            draggable: true,
        });
    }

    /// Round icon button on a subtle surface; registers a hit region.
    pub fn icon_button(&mut self, rect: Rect, icon: Icon, id: HitId, fg: Color, bg: Option<Color>) {
        if let Some(bg) = bg {
            self.capsule(rect, bg);
        }
        let pad = rect.w.min(rect.h) * 0.24;
        self.icon(icon, rect.inset(pad), fg);
        self.hit(rect, id, CursorKind::Hand);
    }

    /// A filled and/or stroked path.
    pub fn shape(&mut self, path: Path, fill: Option<Color>, stroke: Option<(f32, Color)>) {
        if !path.cmds.is_empty() {
            self.push(DrawCmd::Shape {
                path: Arc::new(path),
                fill,
                stroke,
            });
        }
    }

    /// A smooth sparkline of `values` (oldest first, newest at the right edge) in `rect`, `slots`
    /// samples wide, `lo..=hi` bottom to top: the area under it in `area`, then the line itself.
    #[allow(clippy::too_many_arguments)]
    pub fn sparkline(
        &mut self,
        rect: Rect,
        values: &[f32],
        slots: usize,
        (lo, hi): (f32, f32),
        line: Color,
        width: f32,
        area: Option<Color>,
    ) {
        if let Some((line_path, area_path)) = crate::chart::sparkline(rect, values, slots, lo, hi) {
            if let Some(a) = area {
                self.shape(area_path, Some(a), None);
            }
            self.shape(line_path, None, Some((width, line)));
        }
    }

    /// Horizontal progress/seek bar. `frac` is clamped to `[0, 1]`.
    pub fn bar(&mut self, rect: Rect, frac: f32, track: Color, fill: Color) {
        let frac = frac.clamp(0.0, 1.0);
        self.capsule(rect, track);
        if frac > 0.0 {
            let w = (rect.w * frac).max(rect.h);
            self.capsule(Rect::new(rect.x, rect.y, w.min(rect.w), rect.h), fill);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_testing_prefers_the_topmost_region() {
        let theme = Theme::default();
        let mut list = DrawList::new();
        {
            let mut cv = Canvas::new(&mut list, &theme);
            cv.hit(
                Rect::new(0.0, 0.0, 100.0, 100.0),
                HitId(1),
                CursorKind::Arrow,
            );
            cv.hit(
                Rect::new(10.0, 10.0, 20.0, 20.0),
                HitId(2),
                CursorKind::Hand,
            );
        }
        assert_eq!(list.hit_test(Vec2::new(15.0, 15.0)).unwrap().id, HitId(2));
        assert_eq!(list.hit_test(Vec2::new(50.0, 50.0)).unwrap().id, HitId(1));
        assert!(list.hit_test(Vec2::new(200.0, 50.0)).is_none());
    }

    #[test]
    fn balance_check_catches_unmatched_groups() {
        let theme = Theme::default();
        let mut list = DrawList::new();
        let mut cv = Canvas::new(&mut list, &theme);
        cv.push_clip(Rect::new(0.0, 0.0, 1.0, 1.0), 0.0);
        cv.push_group(0.5, 1.0, Vec2::ZERO);
        cv.pop_group();
        assert!(!cv.list.is_balanced());
        cv.pop_clip();
        assert!(cv.list.is_balanced());
        cv.pop_clip();
        assert!(!cv.list.is_balanced(), "underflow");
    }

    #[test]
    fn clear_keeps_capacity_so_frames_do_not_allocate() {
        let theme = Theme::default();
        let mut list = DrawList::new();
        for _ in 0..100 {
            let mut cv = Canvas::new(&mut list, &theme);
            cv.capsule(Rect::new(0.0, 0.0, 10.0, 4.0), Color::WHITE);
        }
        let cap = list.cmds.capacity();
        list.clear();
        assert!(list.is_empty());
        assert_eq!(list.cmds.capacity(), cap);
    }

    #[test]
    fn bar_never_draws_narrower_than_a_dot_and_clamps() {
        let theme = Theme::default();
        let mut list = DrawList::new();
        let mut cv = Canvas::new(&mut list, &theme);
        cv.bar(
            Rect::new(0.0, 0.0, 100.0, 4.0),
            5.0,
            Color::BLACK,
            Color::WHITE,
        );
        match &cv.list.cmds[1] {
            DrawCmd::RoundRect { rect, .. } => assert_eq!(rect.w, 100.0),
            other => panic!("{other:?}"),
        }
        let mut list2 = DrawList::new();
        let mut cv2 = Canvas::new(&mut list2, &theme);
        cv2.bar(
            Rect::new(0.0, 0.0, 100.0, 4.0),
            0.0,
            Color::BLACK,
            Color::WHITE,
        );
        assert_eq!(cv2.list.cmds.len(), 1, "empty bar draws only the track");
    }

    #[test]
    fn text_conversions() {
        let a: Text = "hi".into();
        let b: Text = String::from("hi").into();
        assert_eq!(a.as_str(), b.as_str());
        let shared: Arc<str> = "x".into();
        let t: Text = (&shared).into();
        assert_eq!(t.as_str(), "x");
    }
}
