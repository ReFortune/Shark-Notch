//! A tiny anti-aliased polygon rasteriser (analytic coverage in x, 8 sub-scanlines in y).
//!
//! It exists so the idle pill can be drawn with **no GPU stack loaded at all**: the pill is a few
//! hundred pixels, rasterised once into a premultiplied BGRA bitmap and handed to a layered
//! window. The preview tool uses the same code for the pill/shape so the two stay in sync.

use crate::color::Color;
use crate::geom::Vec2;

const SUB: usize = 8;

/// 8-bit coverage mask.
#[derive(Clone, Debug, PartialEq)]
pub struct Coverage {
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
}

impl Coverage {
    pub fn at(&self, x: usize, y: usize) -> u8 {
        self.data[y * self.w + x]
    }
}

/// Fill closed polygons (even-odd) into a `w`x`h` mask. Polygon coordinates are in pixels.
pub fn fill_polygons(polys: &[Vec<Vec2>], w: usize, h: usize) -> Coverage {
    let mut data = vec![0u8; w * h];
    if w == 0 || h == 0 {
        return Coverage { w, h, data };
    }
    let mut acc = vec![0f32; w];
    let mut xs: Vec<f32> = Vec::with_capacity(16);
    for y in 0..h {
        acc.iter_mut().for_each(|a| *a = 0.0);
        for sub in 0..SUB {
            let ys = y as f32 + (sub as f32 + 0.5) / SUB as f32;
            xs.clear();
            for poly in polys {
                let n = poly.len();
                if n < 3 {
                    continue;
                }
                for i in 0..n {
                    let (a, b) = (poly[i], poly[(i + 1) % n]);
                    if (a.y <= ys) != (b.y <= ys) {
                        let t = (ys - a.y) / (b.y - a.y);
                        xs.push(a.x + t * (b.x - a.x));
                    }
                }
            }
            xs.sort_by(|a, b| a.total_cmp(b));
            let (pairs, _) = xs.as_chunks::<2>();
            for [x0, x1] in pairs {
                add_span(&mut acc, *x0, *x1, 1.0 / SUB as f32);
            }
        }
        for (x, a) in acc.iter().enumerate() {
            data[y * w + x] = (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
    }
    Coverage { w, h, data }
}

fn add_span(row: &mut [f32], x0: f32, x1: f32, weight: f32) {
    let w = row.len() as f32;
    let (x0, x1) = (x0.clamp(0.0, w), x1.clamp(0.0, w));
    if x1 <= x0 {
        return;
    }
    let (i0, i1) = (x0.floor() as usize, x1.floor() as usize);
    if i0 == i1 {
        if i0 < row.len() {
            row[i0] += (x1 - x0) * weight;
        }
        return;
    }
    row[i0] += (i0 as f32 + 1.0 - x0) * weight;
    for cell in row.iter_mut().take(i1).skip(i0 + 1) {
        *cell += weight;
    }
    if i1 < row.len() {
        row[i1] += (x1 - i1 as f32) * weight;
    }
}

/// Convert a coverage mask into **premultiplied BGRA** pixels (what `UpdateLayeredWindow` wants).
pub fn to_premultiplied_bgra(cov: &Coverage, color: Color) -> Vec<u8> {
    let mut out = Vec::with_capacity(cov.data.len() * 4);
    for &c in &cov.data {
        let a = (c as f32 / 255.0) * color.a;
        let px = |v: f32| ((v * a).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        out.extend_from_slice(&[
            px(color.b),
            px(color.g),
            px(color.r),
            (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
        ]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> Vec<Vec2> {
        vec![
            Vec2::new(x0, y0),
            Vec2::new(x1, y0),
            Vec2::new(x1, y1),
            Vec2::new(x0, y1),
        ]
    }

    #[test]
    fn pixel_aligned_rect_is_solid() {
        let c = fill_polygons(&[rect(2.0, 1.0, 6.0, 4.0)], 8, 6);
        assert_eq!(c.at(3, 2), 255);
        assert_eq!(c.at(1, 2), 0);
        assert_eq!(c.at(6, 2), 0);
        assert_eq!(c.at(3, 0), 0);
        assert_eq!(c.at(3, 4), 0);
    }

    #[test]
    fn fractional_edges_give_proportional_coverage() {
        let c = fill_polygons(&[rect(1.5, 0.0, 3.25, 2.0)], 5, 2);
        assert_eq!(c.at(1, 0), 128, "half-covered left pixel");
        assert_eq!(c.at(2, 0), 255);
        assert_eq!(c.at(3, 0), 64, "quarter-covered right pixel");
        // Vertical fractions: 8 sub-scanlines resolve eighths.
        let v = fill_polygons(&[rect(0.0, 0.5, 4.0, 1.0)], 4, 1);
        assert_eq!(v.at(1, 0), 128);
    }

    #[test]
    fn total_coverage_equals_area() {
        let tri = vec![
            Vec2::new(1.0, 1.0),
            Vec2::new(9.0, 1.0),
            Vec2::new(1.0, 9.0),
        ];
        let c = fill_polygons(&[tri], 10, 10);
        let sum: f32 = c.data.iter().map(|&v| v as f32 / 255.0).sum();
        assert!((sum - 32.0).abs() < 0.6, "area {sum}");
    }

    #[test]
    fn degenerate_inputs_are_safe() {
        assert!(fill_polygons(&[], 4, 4).data.iter().all(|&v| v == 0));
        assert_eq!(
            fill_polygons(&[rect(0.0, 0.0, 1.0, 1.0)], 0, 0).data.len(),
            0
        );
        let off = fill_polygons(&[rect(-50.0, -50.0, 500.0, 500.0)], 3, 3);
        assert!(off.data.iter().all(|&v| v == 255), "clipped to the bitmap");
    }

    #[test]
    fn premultiplied_output_is_bgra() {
        let cov = Coverage {
            w: 2,
            h: 1,
            data: vec![255, 128],
        };
        let px = to_premultiplied_bgra(&cov, Color::rgb8(255, 0, 0));
        assert_eq!(
            &px[0..4],
            &[0, 0, 255, 255],
            "full coverage: opaque red in BGRA order"
        );
        assert_eq!(
            &px[4..8],
            &[0, 0, 128, 128],
            "half coverage: red premultiplied by alpha"
        );
        let black = to_premultiplied_bgra(&cov, Color::BLACK);
        assert_eq!(&black[4..8], &[0, 0, 0, 128]);
    }

    #[test]
    fn rasterised_notch_matches_expected_silhouette() {
        use crate::path::NotchShape;
        let shape = NotchShape {
            w: 60.0,
            h: 20.0,
            radius_top: 0.0,
            radius_bottom: 10.0,
            ear: 6.0,
            smoothing: 0.6,
        };
        let polys = shape.to_path().flatten(0.05);
        let c = fill_polygons(&polys, 60, 20);
        assert_eq!(c.at(30, 10), 255, "solid body");
        // The ear is a concave fillet: thin at its very tip, fully solid near the body.
        assert!(c.at(0, 0) < 60, "ear tip is a sliver, got {}", c.at(0, 0));
        assert!(
            c.at(5, 0) > 200,
            "ear fills in towards the body, got {}",
            c.at(5, 0)
        );
        assert_eq!(c.at(0, 8), 0, "outside the body below the ear");
        assert!(
            c.at(0, 19) == 0 && c.at(59, 19) == 0,
            "bottom corners are cut away"
        );
        assert!(
            c.at(8, 19) < 255,
            "rounded bottom-left corner edge is anti-aliased"
        );
    }
}
