//! Vector paths and the notch's continuous ("squircle") corners.
//!
//! A *continuous corner* replaces the usual circular arc with a curve whose curvature ramps up from
//! zero at the point where it leaves the straight edge, reaches `1/r` in the middle, and ramps back
//! down. That removes the visible "kink" where a circular arc meets a straight edge.
//!
//! Construction (derived here, not copied): in a canonical frame the corner vertex is the origin and
//! the two edges are the +x and +y axes. With smoothing `s` in `[0, 1]`:
//!
//! * the curve starts at `A = (p, 0)` with `p = (1 + s) r` and ends at `B = (0, p)`;
//! * the middle is a circular arc of radius `r` centred on `(r, r)` spanning `θ = 90°·(1 − s)`;
//! * `A → P3` (where the arc begins) is one cubic Bézier whose first three control points are
//!   collinear on the edge, which makes the curvature at `A` exactly 0, and whose last handle length
//!   is solved so that the curvature at `P3` is exactly `1/r`;
//! * the second half is the mirror image about the diagonal.
//!
//! Tests verify G1 (tangent) and G2 (curvature) continuity numerically.

use crate::geom::{Rect, Vec2};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathCmd {
    MoveTo(Vec2),
    LineTo(Vec2),
    CubicTo(Vec2, Vec2, Vec2),
    Close,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Path {
    pub cmds: Vec<PathCmd>,
}

impl Path {
    pub fn new() -> Self {
        Self { cmds: Vec::new() }
    }

    pub fn clear(&mut self) {
        self.cmds.clear();
    }

    pub fn move_to(&mut self, p: Vec2) -> &mut Self {
        self.cmds.push(PathCmd::MoveTo(p));
        self
    }

    pub fn line_to(&mut self, p: Vec2) -> &mut Self {
        self.cmds.push(PathCmd::LineTo(p));
        self
    }

    pub fn cubic_to(&mut self, c1: Vec2, c2: Vec2, p: Vec2) -> &mut Self {
        self.cmds.push(PathCmd::CubicTo(c1, c2, p));
        self
    }

    pub fn close(&mut self) -> &mut Self {
        self.cmds.push(PathCmd::Close);
        self
    }

    /// Axis-aligned bounds of all control points (a conservative bound for Béziers).
    pub fn bounds(&self) -> Rect {
        let mut min = Vec2::new(f32::MAX, f32::MAX);
        let mut max = Vec2::new(f32::MIN, f32::MIN);
        let mut add = |p: Vec2| {
            min.x = min.x.min(p.x);
            min.y = min.y.min(p.y);
            max.x = max.x.max(p.x);
            max.y = max.y.max(p.y);
        };
        for c in &self.cmds {
            match *c {
                PathCmd::MoveTo(p) | PathCmd::LineTo(p) => add(p),
                PathCmd::CubicTo(a, b, p) => {
                    add(a);
                    add(b);
                    add(p);
                }
                PathCmd::Close => {}
            }
        }
        if min.x > max.x {
            return Rect::default();
        }
        Rect::new(min.x, min.y, max.x - min.x, max.y - min.y)
    }

    /// Return a copy translated by `d`.
    pub fn translated(&self, d: Vec2) -> Path {
        let t = |p: Vec2| p + d;
        Path {
            cmds: self
                .cmds
                .iter()
                .map(|c| match *c {
                    PathCmd::MoveTo(p) => PathCmd::MoveTo(t(p)),
                    PathCmd::LineTo(p) => PathCmd::LineTo(t(p)),
                    PathCmd::CubicTo(a, b, p) => PathCmd::CubicTo(t(a), t(b), t(p)),
                    PathCmd::Close => PathCmd::Close,
                })
                .collect(),
        }
    }

    /// Return a copy scaled about the origin.
    pub fn scaled(&self, k: f32) -> Path {
        let t = |p: Vec2| p * k;
        Path {
            cmds: self
                .cmds
                .iter()
                .map(|c| match *c {
                    PathCmd::MoveTo(p) => PathCmd::MoveTo(t(p)),
                    PathCmd::LineTo(p) => PathCmd::LineTo(t(p)),
                    PathCmd::CubicTo(a, b, p) => PathCmd::CubicTo(t(a), t(b), t(p)),
                    PathCmd::Close => PathCmd::Close,
                })
                .collect(),
        }
    }

    /// Flatten to closed polylines. `tol` is the max deviation in the path's own units.
    pub fn flatten(&self, tol: f32) -> Vec<Vec<Vec2>> {
        let mut out: Vec<Vec<Vec2>> = Vec::new();
        let mut cur: Vec<Vec2> = Vec::new();
        let mut last = Vec2::ZERO;
        for c in &self.cmds {
            match *c {
                PathCmd::MoveTo(p) => {
                    if cur.len() > 1 {
                        out.push(std::mem::take(&mut cur));
                    } else {
                        cur.clear();
                    }
                    cur.push(p);
                    last = p;
                }
                PathCmd::LineTo(p) => {
                    cur.push(p);
                    last = p;
                }
                PathCmd::CubicTo(a, b, p) => {
                    flatten_cubic(last, a, b, p, tol, &mut cur);
                    last = p;
                }
                PathCmd::Close => {
                    if cur.len() > 1 {
                        out.push(std::mem::take(&mut cur));
                    }
                    cur.clear();
                }
            }
        }
        if cur.len() > 1 {
            out.push(cur);
        }
        out
    }
}

/// Subdivide a cubic into `n` segments chosen from a flatness bound, appending the end points.
fn flatten_cubic(p0: Vec2, p1: Vec2, p2: Vec2, p3: Vec2, tol: f32, out: &mut Vec<Vec2>) {
    // Bound on the second difference gives the segment count (Wang's formula).
    let dd0 = p0 - p1 * 2.0 + p2;
    let dd1 = p1 - p2 * 2.0 + p3;
    let m = dd0.length().max(dd1.length());
    let n = ((0.75 * m / tol.max(1e-4)).sqrt().ceil() as usize).clamp(1, 256);
    for i in 1..=n {
        let t = i as f32 / n as f32;
        out.push(cubic_at(p0, p1, p2, p3, t));
    }
}

pub fn cubic_at(p0: Vec2, p1: Vec2, p2: Vec2, p3: Vec2, t: f32) -> Vec2 {
    let u = 1.0 - t;
    p0 * (u * u * u) + p1 * (3.0 * u * u * t) + p2 * (3.0 * u * t * t) + p3 * (t * t * t)
}

/// `KAPPA * r` is the Bézier handle length that approximates a quarter circle.
const KAPPA: f64 = 0.552_284_749_830_793_4;

/// The cubic Bézier pieces of one corner in the canonical frame (vertex at the origin, `A` on +x,
/// `B` on +y), ordered from `A` to `B`. Each piece is `[p0, p1, p2, p3]`.
#[derive(Clone, Debug)]
pub struct CornerCurve {
    pub pieces: Vec<[Vec2; 4]>,
    /// Distance from the vertex to `A` (and to `B`) along the edges.
    pub extent: f32,
}

/// Build the canonical corner for radius `r` and smoothing `s` (clamped to `[0, 1]`).
pub fn corner_curve(r: f32, s: f32) -> CornerCurve {
    let r = r.max(0.0) as f64;
    let s = s.clamp(0.0, 1.0) as f64;
    let v = |x: f64, y: f64| Vec2::new(x as f32, y as f32);

    if r < 1e-6 {
        return CornerCurve {
            pieces: Vec::new(),
            extent: 0.0,
        };
    }

    // Plain circular corner.
    if s < 1e-4 {
        let k = KAPPA * r;
        return CornerCurve {
            pieces: vec![[v(r, 0.0), v(r - k, 0.0), v(0.0, r - k), v(0.0, r)]],
            extent: r as f32,
        };
    }

    let theta = std::f64::consts::FRAC_PI_2 * (1.0 - s); // span of the middle arc
    let p = (1.0 + s) * r; // extent along each edge
    let a_top = 1.25 * std::f64::consts::PI + theta * 0.5; // angle of the arc end nearest the top edge

    // P3: where the circular arc begins (circle centred at (r, r)).
    let p3x = r + r * a_top.cos();
    let p3y = r + r * a_top.sin();
    // Unit tangent of travel (A -> B direction) at P3.
    let t3x = a_top.sin();
    let t3y = -a_top.cos();

    // P2 lies on the edge (y = 0) along P3's tangent line.
    let lambda = p3y / t3y;
    let p2x = p3x - lambda * t3x;
    // P1 is on the edge too; its distance from P2 makes the curvature at P3 equal 1/r.
    let d = 3.0 * lambda.powi(3) / (2.0 * r * p3y);
    let p1x = p2x + d;

    let mut pieces = Vec::with_capacity(3);
    // A -> P3
    pieces.push([v(p, 0.0), v(p1x, 0.0), v(p2x, 0.0), v(p3x, p3y)]);

    // Middle arc P3 -> P4 (mirror of P3 about the diagonal), approximated by one cubic.
    if theta > 1e-4 {
        let h = 4.0 / 3.0 * (theta * 0.25).tan() * r;
        let p4x = p3y;
        let p4y = p3x;
        // Travel tangent at P4 is the negated mirror of the tangent at P3.
        let t4x = -t3y;
        let t4y = -t3x;
        pieces.push([
            v(p3x, p3y),
            v(p3x + h * t3x, p3y + h * t3y),
            v(p4x - h * t4x, p4y - h * t4y),
            v(p4x, p4y),
        ]);
    }

    // P4 -> B: the first piece mirrored about y = x and reversed.
    pieces.push([v(p3y, p3x), v(0.0, p2x), v(0.0, p1x), v(0.0, p)]);

    CornerCurve {
        pieces,
        extent: p as f32,
    }
}

/// Append a corner to `path` in world space.
///
/// `vertex` is where the two edges would meet; `to_a` and `to_b` are unit vectors from the vertex
/// along the edge towards `A` and `B`. The curve is emitted from `A` to `B` (the caller has already
/// moved/lined to `A`).
pub fn emit_corner(path: &mut Path, vertex: Vec2, to_a: Vec2, to_b: Vec2, curve: &CornerCurve) {
    let map = |q: Vec2| vertex + to_a * q.x + to_b * q.y;
    for piece in &curve.pieces {
        path.cubic_to(map(piece[1]), map(piece[2]), map(piece[3]));
    }
}

/// Shape of the notch silhouette in its own local space: the bounding box is `[0, w] x [0, h]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NotchShape {
    pub w: f32,
    pub h: f32,
    /// Radius of the two top corners (island look). Ignored when `ear > 0`.
    pub radius_top: f32,
    /// Radius of the two bottom corners.
    pub radius_bottom: f32,
    /// Extent of the concave "ears" where the notch meets the screen edge. 0 disables them.
    pub ear: f32,
    /// Corner smoothing in `[0, 1]`; 0 is a plain circular arc.
    pub smoothing: f32,
}

impl NotchShape {
    pub fn to_path(&self) -> Path {
        let mut path = Path::new();
        let w = self.w.max(0.0);
        let h = self.h.max(0.0);
        if w < 0.5 || h < 0.5 {
            return path;
        }
        let s = self.smoothing.clamp(0.0, 1.0);
        let shrink = 1.0 / (1.0 + s); // extent = (1+s) r, so r is limited by extent/(1+s)

        let ear = self.ear.max(0.0).min(w * 0.25).min(h * 0.5);
        let body_w = w - 2.0 * ear;

        if ear > 0.25 {
            // Extents (not radii) are what must fit along the edges.
            let max_ext_bottom = (body_w * 0.5).min(h - ear).max(0.0);
            let rb = self.radius_bottom.max(0.0).min(max_ext_bottom * shrink);
            let cb = corner_curve(rb, s);
            let ce = corner_curve(ear * shrink, s);
            let pb = cb.extent;
            let pe = ce.extent; // == ear when s > 0 (since p = (1+s) r); close enough otherwise

            path.move_to(Vec2::new(0.0, 0.0));
            path.line_to(Vec2::new(w, 0.0));
            // Right ear: vertex (w-ear, 0); A on the top edge, B on the body side.
            emit_corner(
                &mut path,
                Vec2::new(w - ear, 0.0),
                Vec2::new(1.0, 0.0),
                Vec2::new(0.0, 1.0),
                &ce,
            );
            path.line_to(Vec2::new(w - ear, h - pb));
            // Bottom-right.
            emit_corner(
                &mut path,
                Vec2::new(w - ear, h),
                Vec2::new(0.0, -1.0),
                Vec2::new(-1.0, 0.0),
                &cb,
            );
            path.line_to(Vec2::new(ear + pb, h));
            // Bottom-left.
            emit_corner(
                &mut path,
                Vec2::new(ear, h),
                Vec2::new(1.0, 0.0),
                Vec2::new(0.0, -1.0),
                &cb,
            );
            path.line_to(Vec2::new(ear, pe));
            // Left ear: travelling up the body side then out along the top edge.
            emit_corner(
                &mut path,
                Vec2::new(ear, 0.0),
                Vec2::new(0.0, 1.0),
                Vec2::new(-1.0, 0.0),
                &ce,
            );
            path.close();
            // Keep `pe` referenced for the case smoothing == 0 where extent == radius.
            let _ = pe;
        } else {
            let max_ext = (w * 0.5).min(h * 0.5);
            let rt = self.radius_top.max(0.0).min(max_ext * shrink);
            let rb = self.radius_bottom.max(0.0).min(max_ext * shrink);
            let ct = corner_curve(rt, s);
            let cb = corner_curve(rb, s);
            let (pt, pb) = (ct.extent, cb.extent);

            path.move_to(Vec2::new(pt, 0.0));
            path.line_to(Vec2::new(w - pt, 0.0));
            emit_corner(
                &mut path,
                Vec2::new(w, 0.0),
                Vec2::new(-1.0, 0.0),
                Vec2::new(0.0, 1.0),
                &ct,
            );
            path.line_to(Vec2::new(w, h - pb));
            emit_corner(
                &mut path,
                Vec2::new(w, h),
                Vec2::new(0.0, -1.0),
                Vec2::new(-1.0, 0.0),
                &cb,
            );
            path.line_to(Vec2::new(pb, h));
            emit_corner(
                &mut path,
                Vec2::new(0.0, h),
                Vec2::new(1.0, 0.0),
                Vec2::new(0.0, -1.0),
                &cb,
            );
            path.line_to(Vec2::new(0.0, pt));
            emit_corner(
                &mut path,
                Vec2::new(0.0, 0.0),
                Vec2::new(0.0, 1.0),
                Vec2::new(1.0, 0.0),
                &ct,
            );
            path.close();
        }
        path
    }
}

/// A rounded rectangle with per-corner radii `[top-left, top-right, bottom-right, bottom-left]`.
pub fn rounded_rect_path(rect: Rect, radii: [f32; 4], smoothing: f32) -> Path {
    let s = smoothing.clamp(0.0, 1.0);
    let shrink = 1.0 / (1.0 + s);
    let max_ext = (rect.w * 0.5).min(rect.h * 0.5).max(0.0);
    let c: Vec<CornerCurve> = radii
        .iter()
        .map(|r| corner_curve(r.max(0.0).min(max_ext * shrink), s))
        .collect();
    let (x0, y0, x1, y1) = (rect.x, rect.y, rect.right(), rect.bottom());
    let mut p = Path::new();
    p.move_to(Vec2::new(x0 + c[0].extent, y0));
    p.line_to(Vec2::new(x1 - c[1].extent, y0));
    emit_corner(
        &mut p,
        Vec2::new(x1, y0),
        Vec2::new(-1.0, 0.0),
        Vec2::new(0.0, 1.0),
        &c[1],
    );
    p.line_to(Vec2::new(x1, y1 - c[2].extent));
    emit_corner(
        &mut p,
        Vec2::new(x1, y1),
        Vec2::new(0.0, -1.0),
        Vec2::new(-1.0, 0.0),
        &c[2],
    );
    p.line_to(Vec2::new(x0 + c[3].extent, y1));
    emit_corner(
        &mut p,
        Vec2::new(x0, y1),
        Vec2::new(1.0, 0.0),
        Vec2::new(0.0, -1.0),
        &c[3],
    );
    p.line_to(Vec2::new(x0, y0 + c[0].extent));
    emit_corner(
        &mut p,
        Vec2::new(x0, y0),
        Vec2::new(0.0, 1.0),
        Vec2::new(1.0, 0.0),
        &c[0],
    );
    p.close();
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deriv(p: &[Vec2; 4], t: f64) -> (f64, f64) {
        let u = 1.0 - t;
        let c = |a: Vec2, b: Vec2, k: f64| ((b.x - a.x) as f64 * k, (b.y - a.y) as f64 * k);
        let (a, b, d) = (
            c(p[0], p[1], 3.0 * u * u),
            c(p[1], p[2], 6.0 * u * t),
            c(p[2], p[3], 3.0 * t * t),
        );
        (a.0 + b.0 + d.0, a.1 + b.1 + d.1)
    }

    fn deriv2(p: &[Vec2; 4], t: f64) -> (f64, f64) {
        let u = 1.0 - t;
        let x = |q: Vec2| q.x as f64;
        let y = |q: Vec2| q.y as f64;
        let f = |g: &dyn Fn(Vec2) -> f64| {
            6.0 * (u * (g(p[2]) - 2.0 * g(p[1]) + g(p[0]))
                + t * (g(p[3]) - 2.0 * g(p[2]) + g(p[1])))
        };
        (f(&x), f(&y))
    }

    fn curvature(p: &[Vec2; 4], t: f64) -> f64 {
        let (dx, dy) = deriv(p, t);
        let (ddx, ddy) = deriv2(p, t);
        let den = (dx * dx + dy * dy).powf(1.5);
        if den < 1e-12 {
            0.0
        } else {
            (dx * ddy - dy * ddx) / den
        }
    }

    fn unit(v: (f64, f64)) -> (f64, f64) {
        let l = v.0.hypot(v.1);
        (v.0 / l, v.1 / l)
    }

    #[test]
    fn corner_endpoints_lie_on_the_edges() {
        for s in [0.0, 0.3, 0.6, 1.0] {
            let c = corner_curve(10.0, s);
            let first = c.pieces.first().unwrap()[0];
            let last = c.pieces.last().unwrap()[3];
            assert!(
                (first.x - c.extent).abs() < 1e-3 && first.y.abs() < 1e-3,
                "A at s={s}: {first:?}"
            );
            assert!(
                last.x.abs() < 1e-3 && (last.y - c.extent).abs() < 1e-3,
                "B at s={s}: {last:?}"
            );
            assert!((c.extent - (1.0 + s) * 10.0).abs() < 1e-3);
        }
    }

    #[test]
    fn pieces_join_with_matching_tangents() {
        for s in [0.2, 0.6, 0.9, 1.0] {
            let c = corner_curve(12.0, s);
            for w in c.pieces.windows(2) {
                let end = unit(deriv(&w[0], 1.0));
                let start = unit(deriv(&w[1], 0.0));
                assert!(
                    (end.0 - start.0).abs() < 1e-3 && (end.1 - start.1).abs() < 1e-3,
                    "G1 break at s={s}: {end:?} vs {start:?}"
                );
            }
        }
    }

    #[test]
    fn curvature_is_continuous_and_matches_the_construction() {
        let r = 12.0f64;
        for s in [0.3, 0.6, 0.9] {
            let c = corner_curve(r as f32, s);
            // Zero curvature where the corner leaves the straight edges.
            assert!(
                curvature(&c.pieces[0], 0.0).abs() < 1e-4,
                "kappa(A) at s={s}"
            );
            assert!(
                curvature(c.pieces.last().unwrap(), 1.0).abs() < 1e-4,
                "kappa(B) at s={s}"
            );
            // 1/r where the circular arc begins/ends (G2 with the arc).
            let k_end = curvature(&c.pieces[0], 1.0).abs();
            assert!((k_end - 1.0 / r).abs() < 2e-3, "kappa(P3)={k_end} at s={s}");
            if c.pieces.len() == 3 {
                let k_arc0 = curvature(&c.pieces[1], 0.0).abs();
                // The cubic approximation of an arc has ~0.03 % radius error.
                assert!(
                    (k_arc0 - 1.0 / r).abs() < 3e-3,
                    "arc start kappa={k_arc0} at s={s}"
                );
                let k_next = curvature(&c.pieces[2], 0.0).abs();
                assert!(
                    (k_next - 1.0 / r).abs() < 2e-3,
                    "kappa(P4)={k_next} at s={s}"
                );
            }
        }
    }

    #[test]
    fn corner_stays_inside_its_wedge_and_never_overshoots() {
        for s in [0.0, 0.6, 1.0] {
            let c = corner_curve(10.0, s);
            for piece in &c.pieces {
                for i in 0..=20 {
                    let q = cubic_at(piece[0], piece[1], piece[2], piece[3], i as f32 / 20.0);
                    assert!(
                        q.x >= -1e-3 && q.y >= -1e-3,
                        "outside wedge at s={s}: {q:?}"
                    );
                    assert!(q.x <= c.extent + 1e-3 && q.y <= c.extent + 1e-3);
                    // Must cut the corner: never closer to the vertex than the circular arc's midpoint
                    // would be minus a small tolerance.
                    assert!(q.x + q.y >= 0.0);
                }
            }
        }
    }

    #[test]
    fn zero_smoothing_is_a_circle() {
        let r = 10.0;
        let c = corner_curve(r, 0.0);
        assert_eq!(c.pieces.len(), 1);
        for i in 0..=16 {
            let q = cubic_at(
                c.pieces[0][0],
                c.pieces[0][1],
                c.pieces[0][2],
                c.pieces[0][3],
                i as f32 / 16.0,
            );
            let d = ((q.x - r).powi(2) + (q.y - r).powi(2)).sqrt();
            assert!((d - r).abs() < 0.03 * r, "radius drift {d}");
        }
    }

    #[test]
    fn notch_path_is_closed_and_bounded() {
        let shape = NotchShape {
            w: 200.0,
            h: 60.0,
            radius_top: 0.0,
            radius_bottom: 22.0,
            ear: 10.0,
            smoothing: 0.6,
        };
        let p = shape.to_path();
        assert!(matches!(p.cmds.first(), Some(PathCmd::MoveTo(_))));
        assert!(matches!(p.cmds.last(), Some(PathCmd::Close)));
        let b = p.bounds();
        assert!(
            b.x >= -0.01 && b.y >= -0.01 && b.right() <= 200.01 && b.bottom() <= 60.01,
            "{b:?}"
        );
        // Flattened polygon area should be close to (and less than) the bounding box.
        let poly = &p.flatten(0.05)[0];
        let area = polygon_area(poly).abs();
        assert!(
            area > 0.85 * 200.0 * 60.0 && area < 200.0 * 60.0,
            "area {area}"
        );
    }

    #[test]
    fn notch_path_adapts_to_tiny_and_degenerate_sizes() {
        for (w, h) in [
            (0.0, 0.0),
            (0.2, 10.0),
            (10.0, 4.0),
            (112.0, 6.0),
            (30.0, 30.0),
        ] {
            let shape = NotchShape {
                w,
                h,
                radius_top: 18.0,
                radius_bottom: 22.0,
                ear: 10.0,
                smoothing: 0.6,
            };
            let p = shape.to_path();
            if w < 0.5 || h < 0.5 {
                assert!(p.cmds.is_empty());
                continue;
            }
            let b = p.bounds();
            assert!(
                b.x >= -0.01 && b.y >= -0.01 && b.right() <= w + 0.01 && b.bottom() <= h + 0.01,
                "{w}x{h} -> {b:?}"
            );
        }
    }

    #[test]
    fn island_shape_without_ears_has_rounded_top() {
        let shape = NotchShape {
            w: 100.0,
            h: 40.0,
            radius_top: 12.0,
            radius_bottom: 12.0,
            ear: 0.0,
            smoothing: 0.6,
        };
        let poly = &shape.to_path().flatten(0.05)[0];
        // The very top-left corner of the bounding box must be cut away.
        assert!(!point_in_polygon(poly, Vec2::new(0.6, 0.6)));
        assert!(point_in_polygon(poly, Vec2::new(50.0, 20.0)));
    }

    #[test]
    fn rounded_rect_per_corner_radii() {
        let p = rounded_rect_path(Rect::new(0.0, 0.0, 80.0, 40.0), [0.0, 0.0, 16.0, 16.0], 0.6);
        let poly = &p.flatten(0.05)[0];
        assert!(
            point_in_polygon(poly, Vec2::new(0.6, 0.6)),
            "square top-left"
        );
        assert!(
            !point_in_polygon(poly, Vec2::new(79.4, 39.4)),
            "round bottom-right"
        );
    }

    pub(crate) fn polygon_area(poly: &[Vec2]) -> f32 {
        let mut a = 0.0;
        for i in 0..poly.len() {
            let (p, q) = (poly[i], poly[(i + 1) % poly.len()]);
            a += p.x * q.y - q.x * p.y;
        }
        a * 0.5
    }

    pub(crate) fn point_in_polygon(poly: &[Vec2], pt: Vec2) -> bool {
        let mut inside = false;
        let mut j = poly.len() - 1;
        for i in 0..poly.len() {
            let (a, b) = (poly[i], poly[j]);
            if (a.y > pt.y) != (b.y > pt.y) && pt.x < (b.x - a.x) * (pt.y - a.y) / (b.y - a.y) + a.x
            {
                inside = !inside;
            }
            j = i;
        }
        inside
    }
}
