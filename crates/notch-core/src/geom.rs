//! Small geometry vocabulary shared by every layer.
//!
//! Units are DIPs (1/96 inch) unless a comment says otherwise; the y axis grows downwards.

use std::ops::{Add, Mul, Neg, Sub};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub const ZERO: Vec2 = Vec2 { x: 0.0, y: 0.0 };

    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub fn length(self) -> f32 {
        self.x.hypot(self.y)
    }

    pub fn lerp(self, other: Vec2, t: f32) -> Vec2 {
        Vec2::new(lerp(self.x, other.x, t), lerp(self.y, other.y, t))
    }
}

impl Add for Vec2 {
    type Output = Vec2;
    fn add(self, o: Vec2) -> Vec2 {
        Vec2::new(self.x + o.x, self.y + o.y)
    }
}

impl Sub for Vec2 {
    type Output = Vec2;
    fn sub(self, o: Vec2) -> Vec2 {
        Vec2::new(self.x - o.x, self.y - o.y)
    }
}

impl Mul<f32> for Vec2 {
    type Output = Vec2;
    fn mul(self, k: f32) -> Vec2 {
        Vec2::new(self.x * k, self.y * k)
    }
}

impl Neg for Vec2 {
    type Output = Vec2;
    fn neg(self) -> Vec2 {
        Vec2::new(-self.x, -self.y)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub w: f32,
    pub h: f32,
}

impl Size {
    pub const fn new(w: f32, h: f32) -> Self {
        Self { w, h }
    }

    pub fn max(self, o: Size) -> Size {
        Size::new(self.w.max(o.w), self.h.max(o.h))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn from_center_size(c: Vec2, s: Size) -> Self {
        Self::new(c.x - s.w * 0.5, c.y - s.h * 0.5, s.w, s.h)
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn center(&self) -> Vec2 {
        Vec2::new(self.x + self.w * 0.5, self.y + self.h * 0.5)
    }

    pub fn size(&self) -> Size {
        Size::new(self.w, self.h)
    }

    pub fn contains(&self, p: Vec2) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }

    /// Grow (positive) or shrink (negative) on every side.
    pub fn inflate(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(
            self.x - dx,
            self.y - dy,
            (self.w + 2.0 * dx).max(0.0),
            (self.h + 2.0 * dy).max(0.0),
        )
    }

    pub fn inset(&self, d: f32) -> Rect {
        self.inflate(-d, -d)
    }

    pub fn translate(&self, d: Vec2) -> Rect {
        Rect::new(self.x + d.x, self.y + d.y, self.w, self.h)
    }

    pub fn intersect(&self, o: &Rect) -> Option<Rect> {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        (r > x && b > y).then(|| Rect::new(x, y, r - x, b - y))
    }

    pub fn union(&self, o: &Rect) -> Rect {
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        Rect::new(
            x,
            y,
            self.right().max(o.right()) - x,
            self.bottom().max(o.bottom()) - y,
        )
    }

    /// Split off `h` from the top; returns `(top, rest)`.
    pub fn split_top(&self, h: f32) -> (Rect, Rect) {
        let h = h.clamp(0.0, self.h);
        (
            Rect::new(self.x, self.y, self.w, h),
            Rect::new(self.x, self.y + h, self.w, self.h - h),
        )
    }

    /// Split off `w` from the left; returns `(left, rest)`.
    pub fn split_left(&self, w: f32) -> (Rect, Rect) {
        let w = w.clamp(0.0, self.w);
        (
            Rect::new(self.x, self.y, w, self.h),
            Rect::new(self.x + w, self.y, self.w - w, self.h),
        )
    }

    /// Split off `w` from the right; returns `(rest, right)`.
    pub fn split_right(&self, w: f32) -> (Rect, Rect) {
        let w = w.clamp(0.0, self.w);
        (
            Rect::new(self.x, self.y, self.w - w, self.h),
            Rect::new(self.right() - w, self.y, w, self.h),
        )
    }

    /// A `w`x`h` rect centred inside `self`.
    pub fn centered(&self, w: f32, h: f32) -> Rect {
        Rect::new(
            self.x + (self.w - w) * 0.5,
            self.y + (self.h - h) * 0.5,
            w,
            h,
        )
    }
}

pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

pub fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

/// Hermite smoothstep on `[e0, e1]`.
pub fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = clamp01((x - e0) / (e1 - e0));
    t * t * (3.0 - 2.0 * t)
}

/// Map `v` from `[a0, a1]` to `[b0, b1]` without clamping.
pub fn remap(v: f32, a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    if (a1 - a0).abs() < f32::EPSILON {
        return b0;
    }
    b0 + (v - a0) / (a1 - a0) * (b1 - b0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_basics() {
        let r = Rect::new(10.0, 20.0, 30.0, 40.0);
        assert_eq!(r.right(), 40.0);
        assert_eq!(r.bottom(), 60.0);
        assert!(r.contains(Vec2::new(10.0, 20.0)));
        assert!(
            !r.contains(Vec2::new(40.0, 20.0)),
            "right edge is exclusive"
        );
        let c = r.centered(10.0, 10.0);
        assert_eq!(c.center(), r.center());
    }

    #[test]
    fn rect_split_and_inflate() {
        let r = Rect::new(0.0, 0.0, 100.0, 50.0);
        let (t, rest) = r.split_top(20.0);
        assert_eq!((t.h, rest.y, rest.h), (20.0, 20.0, 30.0));
        let (rest, right) = r.split_right(30.0);
        assert_eq!((rest.w, right.x, right.w), (70.0, 70.0, 30.0));
        assert_eq!(r.inflate(5.0, 5.0), Rect::new(-5.0, -5.0, 110.0, 60.0));
        assert_eq!(r.inset(100.0).w, 0.0, "never negative");
    }

    #[test]
    fn rect_intersect_union() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 5.0, 10.0, 10.0);
        assert_eq!(a.intersect(&b), Some(Rect::new(5.0, 5.0, 5.0, 5.0)));
        assert_eq!(a.intersect(&Rect::new(20.0, 0.0, 1.0, 1.0)), None);
        assert_eq!(a.union(&b), Rect::new(0.0, 0.0, 15.0, 15.0));
    }

    #[test]
    fn easing_helpers() {
        assert_eq!(smoothstep(0.0, 1.0, -1.0), 0.0);
        assert_eq!(smoothstep(0.0, 1.0, 2.0), 1.0);
        assert!((smoothstep(0.0, 1.0, 0.5) - 0.5).abs() < 1e-6);
        assert_eq!(remap(5.0, 0.0, 10.0, 0.0, 100.0), 50.0);
        assert_eq!(
            remap(1.0, 3.0, 3.0, 7.0, 9.0),
            7.0,
            "degenerate input range is safe"
        );
    }
}
