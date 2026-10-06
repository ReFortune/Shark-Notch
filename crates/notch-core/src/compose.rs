//! Turns a [`ShellFrame`] into a display list. Shared by the Windows renderer and the preview tool,
//! so what is inspected as a PNG is exactly what the app draws.
//!
//! Layout rule: content is laid out for the **nominal** (destination) size and *clipped* to the
//! animated shape, so while the shape springs, text neither stretches nor reflows. The content group
//! also fades and scales in slightly (0.94 → 1) driven by the `content` spring — that is the "content
//! follows the shape" half of the stagger.

use crate::draw::{Canvas, DrawCmd, DrawList};
use crate::geom::{Rect, Size, Vec2};
use crate::shell::{ContentKind, ShellFrame};
use crate::theme::Theme;

/// Where module content comes from. Implemented by the module host (and by the Phase 1 demo).
pub trait Content {
    fn draw_page(&mut self, page: usize, cv: &mut Canvas, area: Rect);
    fn draw_peek(&mut self, owner: u32, cv: &mut Canvas, area: Rect);
    fn draw_chips(&mut self, cv: &mut Canvas, area: Rect);
    /// Number of pages (for the indicator dots).
    fn page_count(&self) -> usize;
}

#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub pad_x: f32,
    pub pad_top: f32,
    pub pad_bottom: f32,
    /// Draw a faint hairline around the notch.
    pub outline: bool,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            pad_x: 18.0,
            pad_top: 14.0,
            pad_bottom: 26.0,
            outline: true,
        }
    }
}

/// Where a page's content rect sits for a given nominal size: horizontally centred on the window,
/// hanging from the top of the shape.
pub fn content_rect(frame_y: f32, nominal: Size, window_w: f32, m: &Metrics) -> Rect {
    let x = (window_w - nominal.w) * 0.5;
    Rect::new(
        x + m.pad_x,
        frame_y + m.pad_top,
        (nominal.w - 2.0 * m.pad_x).max(0.0),
        (nominal.h - m.pad_top - m.pad_bottom).max(0.0),
    )
}

pub fn compose(
    frame: &ShellFrame,
    pages: &[Size],
    theme: &Theme,
    window_w: f32,
    m: &Metrics,
    list: &mut DrawList,
    content: &mut dyn Content,
) {
    list.clear();
    let shape = frame.shape;
    if !frame.visible || shape.w < 1.0 || shape.h < 0.5 {
        return;
    }
    let rect = Rect::new((window_w - shape.w) * 0.5, frame.y, shape.w, shape.h);
    let outline = m.outline.then_some((1.0, theme.hairline));
    let mut cv = Canvas::new(list, theme);
    cv.push(DrawCmd::Notch {
        shape,
        origin: Vec2::new(rect.x, rect.y),
        fill: theme.bg,
        outline,
    });

    // Everything below is clipped to the animated shape's body (inside the ears and corner radii).
    let clip = rect.inset(2.0);
    let radius = (shape.radius_bottom * 0.7).max(0.0);

    // Collapsed pill content (chips). Their alpha is already 0 unless the pill is collapsed.
    if frame.chip_alpha > 0.01 {
        cv.push_clip(clip, radius);
        cv.push_group(frame.chip_alpha, 1.0, rect.center());
        content.draw_chips(&mut cv, rect.inset(6.0));
        cv.pop_group();
        cv.pop_clip();
    }

    match frame.content_kind {
        ContentKind::Page => {
            cv.push_clip(clip, radius);
            if let Some((page, alpha)) = frame.outgoing
                && alpha > 0.01
                && let Some(&size) = pages.get(page)
            {
                let area = content_rect(frame.y, size, window_w, m);
                cv.push_group(alpha, 0.98 + 0.02 * alpha, area.center());
                content.draw_page(page, &mut cv, area);
                cv.pop_group();
            }
            if frame.content > 0.01 {
                let area = content_rect(frame.y, frame.content_size, window_w, m);
                cv.push_group(frame.content, 0.94 + 0.06 * frame.content, area.center());
                content.draw_page(frame.page, &mut cv, area);
                cv.pop_group();
                if content.page_count() > 1 {
                    cv.push_group(frame.content, 1.0, rect.center());
                    let nominal = Rect::new(
                        (window_w - frame.content_size.w) * 0.5,
                        frame.y,
                        frame.content_size.w,
                        frame.content_size.h,
                    );
                    crate::demo::draw_dots(&mut cv, nominal, content.page_count(), frame.page);
                    cv.pop_group();
                }
            }
            cv.pop_clip();
        }
        ContentKind::Peek { owner, size } => {
            if frame.content > 0.01 {
                cv.push_clip(clip, radius);
                let area = content_rect(
                    frame.y,
                    size,
                    window_w,
                    &Metrics {
                        pad_bottom: m.pad_top,
                        ..*m
                    },
                );
                cv.push_group(frame.content, 0.96 + 0.04 * frame.content, area.center());
                content.draw_peek(owner, &mut cv, area);
                cv.pop_group();
                cv.pop_clip();
            }
        }
        ContentKind::None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo;
    use crate::shell::{Shell, ShellConfig, Trigger};

    struct Demo;
    impl Content for Demo {
        fn draw_page(&mut self, page: usize, cv: &mut Canvas, area: Rect) {
            demo::draw_page(page, cv, area);
        }
        fn draw_peek(&mut self, _: u32, cv: &mut Canvas, area: Rect) {
            let th = *cv.theme;
            cv.text(area, "peek", crate::draw::TextStyle::body(), th.text);
        }
        fn draw_chips(&mut self, cv: &mut Canvas, area: Rect) {
            cv.circle(area.center(), 3.0, cv.theme.accent);
        }
        fn page_count(&self) -> usize {
            demo::PAGE_COUNT
        }
    }

    fn shell() -> Shell {
        let mut s = Shell::new(ShellConfig::default());
        s.set_pages(demo::page_sizes());
        s
    }

    #[test]
    fn collapsed_idle_is_just_the_pill() {
        let s = shell();
        let mut list = DrawList::new();
        compose(
            &s.frame(),
            &demo::page_sizes(),
            &Theme::default(),
            400.0,
            &Metrics::default(),
            &mut list,
            &mut Demo,
        );
        assert_eq!(list.cmds.len(), 1);
        assert!(matches!(list.cmds[0], DrawCmd::Notch { .. }));
        assert!(list.hits.is_empty());
    }

    #[test]
    fn expanded_draws_content_clipped_and_balanced() {
        let mut s = shell();
        s.expand(0.0, Trigger::Hotkey);
        let mut t = 0.0;
        while s.animating() {
            t += 1.0 / 60.0;
            s.step(t);
        }
        let mut list = DrawList::new();
        compose(
            &s.frame(),
            &demo::page_sizes(),
            &Theme::default(),
            400.0,
            &Metrics::default(),
            &mut list,
            &mut Demo,
        );
        assert!(list.is_balanced());
        assert!(list.cmds.len() > 10);
        assert!(!list.hits.is_empty(), "interactive regions registered");
        // Every hit region lies inside the shape.
        let r = s.current_rect(400.0);
        for h in &list.hits {
            assert!(
                h.rect.x >= r.x - 0.5 && h.rect.right() <= r.right() + 0.5,
                "{h:?} vs {r:?}"
            );
        }
    }

    #[test]
    fn content_is_laid_out_for_the_destination_size_while_the_shape_animates() {
        let mut s = shell();
        s.expand(0.0, Trigger::Hotkey);
        s.step(0.12); // mid-flight
        let f = s.frame();
        assert!(f.shape.w < f.nominal.w, "shape still growing");
        let mut a = DrawList::new();
        compose(
            &f,
            &demo::page_sizes(),
            &Theme::default(),
            400.0,
            &Metrics::default(),
            &mut a,
            &mut Demo,
        );
        let mut t = 0.12;
        while s.animating() {
            t += 1.0 / 60.0;
            s.step(t);
        }
        let mut b = DrawList::new();
        compose(
            &s.frame(),
            &demo::page_sizes(),
            &Theme::default(),
            400.0,
            &Metrics::default(),
            &mut b,
            &mut Demo,
        );
        // Same hit-region geometry mid-flight and settled: nothing reflowed.
        assert_eq!(a.hits.len(), b.hits.len());
        for (x, y) in a.hits.iter().zip(&b.hits) {
            assert!((x.rect.x - y.rect.x).abs() < 0.01 && (x.rect.w - y.rect.w).abs() < 0.01);
        }
    }

    #[test]
    fn hidden_and_degenerate_frames_draw_nothing() {
        let mut s = shell();
        s.set_suspended(0.0, true);
        let mut t = 0.0;
        while s.animating() {
            t += 1.0 / 60.0;
            s.step(t);
        }
        let mut list = DrawList::new();
        compose(
            &s.frame(),
            &demo::page_sizes(),
            &Theme::default(),
            400.0,
            &Metrics::default(),
            &mut list,
            &mut Demo,
        );
        assert!(list.is_empty());
    }

    #[test]
    fn content_is_drawn_fading_out_during_a_collapse() {
        let mut s = shell();
        s.expand(0.0, Trigger::Hotkey);
        let mut t = 0.0;
        while s.animating() {
            t += 1.0 / 60.0;
            s.step(t);
        }
        s.collapse(t);
        t += 2.0 / 60.0;
        s.step(t);
        t += 1.0 / 60.0;
        s.step(t);
        let f = s.frame();
        assert!(
            f.shape.h > 100.0,
            "shape has barely started to shrink: {}",
            f.shape.h
        );
        let mut list = DrawList::new();
        compose(
            &f,
            &demo::page_sizes(),
            &Theme::default(),
            400.0,
            &Metrics::default(),
            &mut list,
            &mut Demo,
        );
        let alpha = list
            .cmds
            .iter()
            .find_map(|c| {
                if let DrawCmd::PushGroup { alpha, .. } = c {
                    Some(*alpha)
                } else {
                    None
                }
            })
            .expect("content group present");
        assert!(alpha > 0.0 && alpha < 0.6, "fading, got {alpha}");
        assert!(list.is_balanced());
    }

    #[test]
    fn peek_draws_the_peek_view() {
        let mut s = shell();
        s.peek(0.0, 3, Size::new(320.0, 64.0), 3.0);
        let mut t = 0.0;
        while t < 0.6 {
            t += 1.0 / 60.0;
            s.step(t);
        }
        let mut list = DrawList::new();
        compose(
            &s.frame(),
            &demo::page_sizes(),
            &Theme::default(),
            400.0,
            &Metrics::default(),
            &mut list,
            &mut Demo,
        );
        assert!(
            list.cmds
                .iter()
                .any(|c| matches!(c, DrawCmd::Text { text, .. } if text.as_str() == "peek"))
        );
        assert!(list.is_balanced());
    }
}
