//! Chart geometry: the smooth line (and the area under it) of a sparkline.
//!
//! The newest sample is always at the right edge and older ones scroll left, so a chart that has
//! only a few samples so far fills from the right instead of stretching them across the width.
//! The curve passes through every sample (a Catmull-Rom spline converted to cubic Béziers) and is
//! kept inside the rectangle: an overshoot above the largest sample would otherwise draw outside
//! the tile.

use crate::geom::{Rect, Vec2};
use crate::path::Path;

/// Points for `values` (oldest first) in `rect`, `lo..=hi` mapped bottom to top, `slots` samples
/// wide. Values outside `lo..=hi` (and NaN) are clamped.
pub fn points(rect: Rect, values: &[f32], slots: usize, lo: f32, hi: f32) -> Vec<Vec2> {
    let n = values.len().min(slots.max(1));
    let values = &values[values.len() - n..];
    let span = (hi - lo).max(f32::EPSILON);
    let step = if slots > 1 {
        rect.w / (slots - 1) as f32
    } else {
        0.0
    };
    values
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let v = if v.is_nan() { lo } else { v };
            let f = ((v - lo) / span).clamp(0.0, 1.0);
            Vec2::new(
                rect.right() - (n - 1 - i) as f32 * step,
                rect.bottom() - f * rect.h,
            )
        })
        .collect()
}

/// The open curve through `pts`.
fn curve(pts: &[Vec2], top: f32, bottom: f32) -> Path {
    let mut path = Path::new();
    let Some(&first) = pts.first() else {
        return path;
    };
    path.move_to(first);
    let at = |i: isize| pts[i.clamp(0, pts.len() as isize - 1) as usize];
    for i in 0..pts.len() - 1 {
        let (p0, p1, p2, p3) = (at(i as isize - 1), pts[i], pts[i + 1], at(i as isize + 2));
        let c1 = p1 + (p2 - p0) * (1.0 / 6.0);
        let c2 = p2 - (p3 - p1) * (1.0 / 6.0);
        let keep = |p: Vec2| Vec2::new(p.x, p.y.clamp(top, bottom));
        path.cubic_to(keep(c1), keep(c2), p2);
    }
    path
}

/// The line of a sparkline and the closed area between it and the bottom edge. `None` with fewer
/// than two samples (one point is not a line).
pub fn sparkline(
    rect: Rect,
    values: &[f32],
    slots: usize,
    lo: f32,
    hi: f32,
) -> Option<(Path, Path)> {
    let pts = points(rect, values, slots, lo, hi);
    if pts.len() < 2 {
        return None;
    }
    let line = curve(&pts, rect.y, rect.bottom());
    let mut area = line.clone();
    let (first, last) = (pts[0], pts[pts.len() - 1]);
    area.line_to(Vec2::new(last.x, rect.bottom()));
    area.line_to(Vec2::new(first.x, rect.bottom()));
    area.close();
    Some((line, area))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::PathCmd;

    const R: Rect = Rect {
        x: 10.0,
        y: 20.0,
        w: 100.0,
        h: 40.0,
    };

    fn every_point(p: &Path) -> Vec<Vec2> {
        p.cmds
            .iter()
            .flat_map(|c| match *c {
                PathCmd::MoveTo(a) | PathCmd::LineTo(a) => vec![a],
                PathCmd::CubicTo(a, b, c) => vec![a, b, c],
                PathCmd::Close => vec![],
            })
            .collect()
    }

    #[test]
    fn the_newest_sample_is_at_the_right_edge_and_the_scale_runs_bottom_to_top() {
        let pts = points(R, &[0.0, 50.0, 100.0], 11, 0.0, 100.0);
        assert_eq!(pts.len(), 3);
        // Ten steps of 10 px: three samples occupy the last 20 px.
        assert!((pts[2].x - 110.0).abs() < 1e-4);
        assert!((pts[1].x - 100.0).abs() < 1e-4);
        assert!((pts[0].x - 90.0).abs() < 1e-4);
        assert!((pts[0].y - 60.0).abs() < 1e-4, "0 is the bottom");
        assert!((pts[1].y - 40.0).abs() < 1e-4);
        assert!((pts[2].y - 20.0).abs() < 1e-4, "100 is the top");
    }

    #[test]
    fn a_full_history_spans_the_width_and_older_samples_fall_off_the_left() {
        let values: Vec<f32> = (0..70).map(|i| i as f32).collect();
        let pts = points(R, &values, 60, 0.0, 70.0);
        assert_eq!(pts.len(), 60, "only the newest `slots` samples");
        assert!((pts[0].x - 10.0).abs() < 1e-3);
        assert!((pts[59].x - 110.0).abs() < 1e-3);
    }

    #[test]
    fn out_of_range_and_nan_values_are_clamped() {
        let pts = points(R, &[-5.0, f32::NAN, 500.0], 3, 0.0, 100.0);
        assert!(
            pts.iter()
                .all(|p| p.y >= R.y - 1e-4 && p.y <= R.bottom() + 1e-4)
        );
        assert!((pts[0].y - R.bottom()).abs() < 1e-4);
        assert!(
            (pts[1].y - R.bottom()).abs() < 1e-4,
            "NaN reads as the minimum"
        );
        assert!((pts[2].y - R.y).abs() < 1e-4);
        // A degenerate range does not divide by zero.
        assert!(
            points(R, &[1.0, 1.0], 2, 5.0, 5.0)
                .iter()
                .all(|p| p.y.is_finite())
        );
    }

    #[test]
    fn fewer_than_two_samples_make_no_line() {
        assert!(sparkline(R, &[], 60, 0.0, 1.0).is_none());
        assert!(sparkline(R, &[0.5], 60, 0.0, 1.0).is_none());
        assert!(sparkline(R, &[0.5, 0.6], 60, 0.0, 1.0).is_some());
    }

    #[test]
    fn the_curve_stays_inside_the_tile_even_on_a_spike() {
        // A sharp spike makes a spline overshoot; the control points are clamped to the tile.
        let values = [0.0, 0.0, 0.0, 100.0, 0.0, 0.0, 0.0, 100.0, 100.0, 0.0];
        let (line, area) = sparkline(R, &values, 10, 0.0, 100.0).unwrap();
        for p in every_point(&line).into_iter().chain(every_point(&area)) {
            assert!(
                p.x >= R.x - 1e-3 && p.x <= R.right() + 1e-3,
                "x {} outside",
                p.x
            );
            assert!(
                p.y >= R.y - 1e-3 && p.y <= R.bottom() + 1e-3,
                "y {} outside",
                p.y
            );
        }
    }

    #[test]
    fn the_curve_passes_through_every_sample_and_the_area_is_closed_along_the_bottom() {
        let values = [10.0, 80.0, 30.0, 60.0];
        let pts = points(R, &values, 4, 0.0, 100.0);
        let (line, area) = sparkline(R, &values, 4, 0.0, 100.0).unwrap();
        let ends: Vec<Vec2> = line
            .cmds
            .iter()
            .filter_map(|c| match *c {
                PathCmd::MoveTo(a) => Some(a),
                PathCmd::CubicTo(_, _, p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(ends, pts, "the knots of the spline are the samples");
        assert_eq!(*area.cmds.last().unwrap(), PathCmd::Close);
        let below: Vec<&PathCmd> = area
            .cmds
            .iter()
            .filter(|c| matches!(c, PathCmd::LineTo(p) if (p.y - R.bottom()).abs() < 1e-4))
            .collect();
        assert_eq!(below.len(), 2, "down to the baseline and back along it");
    }

    #[test]
    fn a_flat_series_is_a_flat_line() {
        let (line, _) = sparkline(R, &[0.3; 8], 8, 0.0, 1.0).unwrap();
        let ys: Vec<f32> = every_point(&line).iter().map(|p| p.y).collect();
        assert!(ys.windows(2).all(|w| (w[0] - w[1]).abs() < 1e-3), "{ys:?}");
    }
}
