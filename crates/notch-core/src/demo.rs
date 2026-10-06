//! Placeholder pages used by Phase 1 to exercise the shell (springs, stagger, page switching, hit
//! regions) before real modules exist. Phase 2 replaces these with the module host; the shell and
//! renderers do not know the difference.

use crate::color::Color;
use crate::draw::{Canvas, CursorKind, HitId, TextStyle, Weight};
use crate::geom::{Rect, Size, Vec2};
use crate::icons::Icon;

pub const PAGE_COUNT: usize = 3;

pub fn page_size(i: usize) -> Size {
    match i {
        0 => Size::new(340.0, 130.0),
        1 => Size::new(380.0, 190.0),
        _ => Size::new(300.0, 96.0),
    }
}

pub fn page_sizes() -> Vec<Size> {
    (0..PAGE_COUNT).map(page_size).collect()
}

/// Draw demo page `i` into `area` (the content rect inside the shape).
pub fn draw_page(i: usize, cv: &mut Canvas, area: Rect) {
    let th = *cv.theme;
    match i {
        0 => {
            let (top, rest) = area.split_top(24.0);
            cv.text(top, "Shark Notch", TextStyle::title(), th.text);
            let (sub, rest) = rest.split_top(20.0);
            cv.text(
                sub,
                "Hover, press Ctrl+Alt+N, or scroll to switch pages",
                TextStyle::caption(),
                th.text_dim,
            );
            let row = Rect::new(rest.x, rest.bottom() - 36.0, rest.w, 36.0);
            let mut x = row.x;
            for (n, icon) in [Icon::Prev, Icon::Play, Icon::Next].into_iter().enumerate() {
                let r = Rect::new(x, row.y, 36.0, 36.0);
                cv.icon_button(r, icon, HitId(n as u32), th.text, Some(th.surface));
                x += 44.0;
            }
            cv.bar(
                Rect::new(x + 8.0, row.y + 16.0, row.right() - x - 8.0, 4.0),
                0.42,
                th.surface_hi,
                th.accent,
            );
        }
        1 => {
            let (head, rest) = area.split_top(26.0);
            cv.text(head, "Typography and surfaces", TextStyle::title(), th.text);
            let tile_w = (rest.w - 20.0) / 3.0;
            for k in 0..3 {
                let r = Rect::new(
                    rest.x + k as f32 * (tile_w + 10.0),
                    rest.y + 8.0,
                    tile_w,
                    64.0,
                );
                cv.squircle(r, 16.0, th.surface);
                cv.hit(r, HitId(10 + k), CursorKind::Hand);
                let icon = [Icon::Clock, Icon::Check, Icon::Plus][k as usize];
                cv.icon(
                    icon,
                    Rect::new(r.x + 12.0, r.y + 12.0, 20.0, 20.0),
                    th.accent,
                );
                cv.text(
                    Rect::new(r.x + 12.0, r.bottom() - 24.0, r.w - 16.0, 18.0),
                    ["Timer", "Done", "Add"][k as usize],
                    TextStyle::label(),
                    th.text,
                );
            }
            let lower = Rect::new(rest.x, rest.y + 80.0, rest.w, 18.0);
            cv.text(lower, "A longer line of body text that has to be truncated with an ellipsis because it is too long to fit", TextStyle::body(), th.text_dim);
            cv.text(
                Rect::new(rest.x, rest.y + 100.0, 120.0, 24.0),
                "12:34",
                TextStyle::new(20.0, Weight::SemiBold).tabular(),
                th.accent,
            );
        }
        _ => {
            let c = area.center();
            cv.text(
                Rect::new(area.x, c.y - 10.0, area.w, 20.0),
                "Compact page",
                TextStyle::label().align(crate::draw::Align::Center),
                th.text_dim,
            );
            for k in 0..3 {
                cv.circle(
                    Vec2::new(c.x - 16.0 + k as f32 * 16.0, c.y + 18.0),
                    3.0,
                    if k == 1 {
                        th.accent
                    } else {
                        Color::WHITE.with_alpha(0.25)
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::DrawList;
    use crate::theme::Theme;

    #[test]
    fn demo_pages_balance_and_register_hits() {
        let theme = Theme::default();
        for i in 0..PAGE_COUNT {
            let mut list = DrawList::new();
            let mut cv = Canvas::new(&mut list, &theme);
            let s = page_size(i);
            draw_page(i, &mut cv, Rect::new(16.0, 16.0, s.w - 32.0, s.h - 40.0));
            assert!(list.is_balanced());
            assert!(!list.cmds.is_empty());
        }
    }
}
