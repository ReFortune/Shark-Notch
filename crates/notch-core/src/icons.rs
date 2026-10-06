//! Vector icons on a 24x24 grid, drawn by every backend from the same path data.
//!
//! Style: 2-unit strokes with round caps/joins (Feather-like), a few solid glyphs. No font
//! dependency means icons are crisp at any DPI, identical in the preview PNGs, and cost nothing
//! to load.

use crate::geom::Vec2;
use crate::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    Play,
    Pause,
    Next,
    Prev,
    Close,
    Check,
    Plus,
    ChevronLeft,
    ChevronRight,
    ChevronUp,
    ChevronDown,
    Clock,
}

#[derive(Clone, Debug)]
pub enum IconOp {
    Fill(Path),
    /// Stroke with round caps and joins; width is in grid units (scaled with the icon).
    Stroke(Path, f32),
}

/// Side length of the design grid.
pub const GRID: f32 = 24.0;

fn p(x: f32, y: f32) -> Vec2 {
    Vec2::new(x, y)
}

fn polyline(pts: &[(f32, f32)], closed: bool) -> Path {
    let mut path = Path::new();
    for (i, &(x, y)) in pts.iter().enumerate() {
        if i == 0 {
            path.move_to(p(x, y));
        } else {
            path.line_to(p(x, y));
        }
    }
    if closed {
        path.close();
    }
    path
}

/// Full circle as four cubic Béziers.
pub fn circle_path(cx: f32, cy: f32, r: f32) -> Path {
    let k = 0.552_284_75 * r;
    let mut path = Path::new();
    path.move_to(p(cx + r, cy));
    path.cubic_to(p(cx + r, cy + k), p(cx + k, cy + r), p(cx, cy + r));
    path.cubic_to(p(cx - k, cy + r), p(cx - r, cy + k), p(cx - r, cy));
    path.cubic_to(p(cx - r, cy - k), p(cx - k, cy - r), p(cx, cy - r));
    path.cubic_to(p(cx + k, cy - r), p(cx + r, cy - k), p(cx + r, cy));
    path.close();
    path
}

/// Circular arc; angles in degrees, 0 = 12 o'clock, clockwise. Open path.
pub fn arc_path(cx: f32, cy: f32, r: f32, start_deg: f32, sweep_deg: f32) -> Path {
    let mut path = Path::new();
    let segs = ((sweep_deg.abs() / 90.0).ceil() as usize).max(1);
    let step = sweep_deg / segs as f32;
    let at = |deg: f32| {
        let a = deg.to_radians();
        (cx + r * a.sin(), cy - r * a.cos())
    };
    let (sx, sy) = at(start_deg);
    path.move_to(p(sx, sy));
    for i in 0..segs {
        let a0 = start_deg + step * i as f32;
        let a1 = a0 + step;
        let h = 4.0 / 3.0 * ((a1 - a0).to_radians() / 4.0).tan() * r;
        let (x0, y0) = at(a0);
        let (x3, y3) = at(a1);
        // Tangent for clockwise travel at angle a (0 = top): (cos a, sin a).
        let t0 = (a0.to_radians().cos(), a0.to_radians().sin());
        let t1 = (a1.to_radians().cos(), a1.to_radians().sin());
        path.cubic_to(
            p(x0 + h * t0.0, y0 + h * t0.1),
            p(x3 - h * t1.0, y3 - h * t1.1),
            p(x3, y3),
        );
    }
    path
}

/// Rounded rectangle with a uniform circular radius (for icon bodies).
pub fn rrect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Path {
    let r = r.min(w * 0.5).min(h * 0.5);
    let k = 0.552_284_75 * r;
    let (x1, y1) = (x + w, y + h);
    let mut path = Path::new();
    path.move_to(p(x + r, y));
    path.line_to(p(x1 - r, y));
    path.cubic_to(p(x1 - r + k, y), p(x1, y + r - k), p(x1, y + r));
    path.line_to(p(x1, y1 - r));
    path.cubic_to(p(x1, y1 - r + k), p(x1 - r + k, y1), p(x1 - r, y1));
    path.line_to(p(x + r, y1));
    path.cubic_to(p(x + r - k, y1), p(x, y1 - r + k), p(x, y1 - r));
    path.line_to(p(x, y + r));
    path.cubic_to(p(x, y + r - k), p(x + r - k, y), p(x + r, y));
    path.close();
    path
}

/// The drawing operations for `icon` on the 24x24 grid.
pub fn ops(icon: Icon) -> Vec<IconOp> {
    use IconOp::{Fill, Stroke};
    match icon {
        Icon::Play => {
            let tri = polyline(&[(8.0, 5.5), (18.5, 12.0), (8.0, 18.5)], true);
            vec![Fill(tri.clone()), Stroke(tri, 2.0)]
        }
        Icon::Pause => vec![
            Fill(rrect_path(6.0, 5.0, 4.2, 14.0, 1.4)),
            Fill(rrect_path(13.8, 5.0, 4.2, 14.0, 1.4)),
        ],
        Icon::Next => {
            let tri = polyline(&[(5.5, 5.5), (15.5, 12.0), (5.5, 18.5)], true);
            vec![
                Fill(tri.clone()),
                Stroke(tri, 2.0),
                Stroke(polyline(&[(19.0, 5.5), (19.0, 18.5)], false), 2.4),
            ]
        }
        Icon::Prev => {
            let tri = polyline(&[(18.5, 5.5), (8.5, 12.0), (18.5, 18.5)], true);
            vec![
                Fill(tri.clone()),
                Stroke(tri, 2.0),
                Stroke(polyline(&[(5.0, 5.5), (5.0, 18.5)], false), 2.4),
            ]
        }
        Icon::Close => vec![
            Stroke(polyline(&[(6.0, 6.0), (18.0, 18.0)], false), 2.0),
            Stroke(polyline(&[(18.0, 6.0), (6.0, 18.0)], false), 2.0),
        ],
        Icon::Check => vec![Stroke(
            polyline(&[(5.0, 12.5), (10.0, 17.5), (19.0, 7.0)], false),
            2.2,
        )],
        Icon::Plus => vec![
            Stroke(polyline(&[(12.0, 5.0), (12.0, 19.0)], false), 2.0),
            Stroke(polyline(&[(5.0, 12.0), (19.0, 12.0)], false), 2.0),
        ],
        Icon::ChevronLeft => vec![Stroke(
            polyline(&[(14.5, 6.0), (8.5, 12.0), (14.5, 18.0)], false),
            2.2,
        )],
        Icon::ChevronRight => vec![Stroke(
            polyline(&[(9.5, 6.0), (15.5, 12.0), (9.5, 18.0)], false),
            2.2,
        )],
        Icon::ChevronUp => vec![Stroke(
            polyline(&[(6.0, 14.5), (12.0, 8.5), (18.0, 14.5)], false),
            2.2,
        )],
        Icon::ChevronDown => vec![Stroke(
            polyline(&[(6.0, 9.5), (12.0, 15.5), (18.0, 9.5)], false),
            2.2,
        )],
        Icon::Clock => vec![
            Stroke(circle_path(12.0, 12.0, 8.5), 2.0),
            Stroke(
                polyline(&[(12.0, 7.0), (12.0, 12.0), (15.5, 14.0)], false),
                2.0,
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Icon; 12] = [
        Icon::Play,
        Icon::Pause,
        Icon::Next,
        Icon::Prev,
        Icon::Close,
        Icon::Check,
        Icon::Plus,
        Icon::ChevronLeft,
        Icon::ChevronRight,
        Icon::ChevronUp,
        Icon::ChevronDown,
        Icon::Clock,
    ];

    #[test]
    fn every_icon_fits_the_grid_with_room_for_its_stroke() {
        for icon in ALL {
            let o = ops(icon);
            assert!(!o.is_empty(), "{icon:?}");
            for op in o {
                let (path, half) = match &op {
                    IconOp::Fill(p) => (p, 0.0),
                    IconOp::Stroke(p, w) => (p, w * 0.5),
                };
                let b = path.bounds();
                assert!(b.x - half >= -0.01 && b.y - half >= -0.01, "{icon:?} {b:?}");
                assert!(
                    b.right() + half <= GRID + 0.01 && b.bottom() + half <= GRID + 0.01,
                    "{icon:?} {b:?}"
                );
            }
        }
    }

    #[test]
    fn circle_and_arc_are_round() {
        let c = circle_path(12.0, 12.0, 8.0);
        let poly = &c.flatten(0.01)[0];
        for v in poly {
            let d = ((v.x - 12.0).powi(2) + (v.y - 12.0).powi(2)).sqrt();
            assert!((d - 8.0).abs() < 0.05, "off-circle by {}", d - 8.0);
        }
        // Quarter arc from 12 o'clock to 3 o'clock.
        let a = arc_path(0.0, 0.0, 10.0, 0.0, 90.0);
        let pts = &a.flatten(0.01)[0];
        let first = pts.first().unwrap();
        let last = pts.last().unwrap();
        assert!(
            (first.x).abs() < 1e-3 && (first.y + 10.0).abs() < 1e-3,
            "starts at the top: {first:?}"
        );
        assert!(
            (last.x - 10.0).abs() < 1e-3 && last.y.abs() < 1e-3,
            "ends at 3 o'clock: {last:?}"
        );
        for v in pts {
            assert!((v.x.hypot(v.y) - 10.0).abs() < 0.02);
        }
    }

    #[test]
    fn full_sweep_arc_closes_on_itself() {
        let a = arc_path(5.0, 5.0, 4.0, 0.0, 360.0);
        let pts = &a.flatten(0.01)[0];
        let (f, l) = (pts.first().unwrap(), pts.last().unwrap());
        assert!((f.x - l.x).abs() < 1e-3 && (f.y - l.y).abs() < 1e-3);
    }
}
