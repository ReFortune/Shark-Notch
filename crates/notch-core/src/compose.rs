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
    /// Extra horizontal inset for the concave "ears": the visible body of a notch is `2 * ear`
    /// narrower than its shape, so content must clear them (0 for the floating island style).
    pub ear_inset: f32,
    pub pad_top: f32,
    pub pad_bottom: f32,
    /// Draw a faint hairline around the notch.
    pub outline: bool,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            pad_x: 18.0,
            ear_inset: 12.0,
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
    let pad_x = m.pad_x + m.ear_inset;
    Rect::new(
        x + pad_x,
        frame_y + m.pad_top,
        (nominal.w - 2.0 * pad_x).max(0.0),
        (nominal.h - m.pad_top - m.pad_bottom).max(0.0),
    )
}

/// Page indicator dots along the bottom of the expanded shape.
pub fn draw_dots(cv: &mut Canvas, shape_rect: Rect, count: usize, active: usize) {
    if count < 2 {
        return;
    }
    let th = *cv.theme;
    let gap = 11.0;
    let total = gap * (count as f32 - 1.0);
    let y = shape_rect.bottom() - 11.0;
    let x0 = shape_rect.center().x - total * 0.5;
    for k in 0..count {
        let on = k == active;
        cv.circle(
            Vec2::new(x0 + k as f32 * gap, y),
            if on { 2.6 } else { 2.0 },
            if on { th.text } else { th.text_faint },
        );
    }
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

    // Content is clipped to the animated shape's body (inside the ears and corner radii) — but only
    // while the shape is still smaller than the laid-out content. Once it has grown to the page's
    // size the content cannot overflow (it has its own padding), and a rounded mask layer per frame
    // is real GPU work we can skip.
    let clip = rect.inset(2.0);
    let radius = (shape.radius_bottom * 0.7).max(0.0);
    let (mut lw, mut lh) = (frame.content_size.w, frame.content_size.h);
    if let Some((p, a)) = frame.outgoing
        && a > 0.01
        && let Some(sz) = pages.get(p)
    {
        lw = lw.max(sz.w);
        lh = lh.max(sz.h);
    }
    let needs_clip = shape.w < lw - 1.0 || shape.h < lh - 1.0;

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
            if needs_clip {
                cv.push_clip(clip, radius);
            }
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
                    draw_dots(&mut cv, nominal, content.page_count(), frame.page);
                    cv.pop_group();
                }
            }
            if needs_clip {
                cv.pop_clip();
            }
        }
        ContentKind::Peek { owner, size } => {
            if frame.content > 0.01 {
                if needs_clip {
                    cv.push_clip(clip, radius);
                }
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
                if needs_clip {
                    cv.pop_clip();
                }
            }
        }
        ContentKind::None => {}
    }
}

/// Everything the first animation would otherwise have to create on the spot.
///
/// [`Warmup::run`] builds one display list per *kind* of frame the shell can show — every page, every
/// peek banner and the collapsed chips — and hands each to a sink that renders it off-screen. That
/// fills the platform's text-format, layout and icon-geometry caches and makes the GPU driver take
/// its first-use paths while nobody is watching, so the first hover animates from the first frame
/// ("pre-warm the first render"). Pages are drawn inside a shape slightly smaller than their content
/// so the clip-layer path is exercised as well.
pub struct Warmup<'a> {
    pub pages: &'a [Size],
    /// `(owner id, size)` of every module that has a peek banner.
    pub peeks: &'a [(u32, Size)],
    /// Width of the collapsed pill with chips (0 = no chips).
    pub chips_width: f32,
    pub theme: &'a Theme,
    pub window_w: f32,
    pub metrics: &'a Metrics,
}

impl Warmup<'_> {
    /// Feed every warm-up list to `sink` (which returns `false` to stop early). Returns how many
    /// lists were produced.
    pub fn run(
        &self,
        base: &ShellFrame,
        list: &mut DrawList,
        content: &mut dyn Content,
        mut sink: impl FnMut(&DrawList) -> bool,
    ) -> usize {
        let mut frame = *base;
        frame.visible = true;
        frame.y = 0.0;
        frame.outgoing = None;
        frame.peek_owner = None;
        frame.chip_alpha = 0.0;
        frame.content = 1.0;
        let shrink = 0.92;
        let mut n = 0;
        let mut emit = |frame: &ShellFrame, list: &mut DrawList, content: &mut dyn Content| {
            compose(
                frame,
                self.pages,
                self.theme,
                self.window_w,
                self.metrics,
                list,
                content,
            );
            n += 1;
            sink(list)
        };

        for (page, &size) in self.pages.iter().enumerate() {
            frame.shape.w = size.w * shrink;
            frame.shape.h = size.h * shrink;
            frame.page = page;
            frame.content_kind = ContentKind::Page;
            frame.content_size = size;
            if !emit(&frame, list, content) {
                return n;
            }
        }
        for &(owner, size) in self.peeks {
            frame.shape.w = size.w * shrink;
            frame.shape.h = size.h * shrink;
            frame.peek_owner = Some(owner);
            frame.content_kind = ContentKind::Peek { owner, size };
            frame.content_size = size;
            if !emit(&frame, list, content) {
                return n;
            }
        }
        if self.chips_width > 0.0 {
            frame.shape.w = self.chips_width;
            frame.content_kind = ContentKind::None;
            frame.peek_owner = None;
            frame.chip_alpha = 1.0;
            emit(&frame, list, content);
        }
        n
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

    fn count_clips(list: &DrawList) -> usize {
        list.cmds
            .iter()
            .filter(|c| matches!(c, DrawCmd::PushClip { .. }))
            .count()
    }

    #[test]
    fn a_mask_is_only_used_while_the_shape_is_smaller_than_its_content() {
        let mut s = shell();
        s.expand(0.0, Trigger::Hotkey);
        s.step(0.1); // still growing
        let mut growing = DrawList::new();
        compose(
            &s.frame(),
            &demo::page_sizes(),
            &Theme::default(),
            400.0,
            &Metrics::default(),
            &mut growing,
            &mut Demo,
        );
        assert!(
            count_clips(&growing) >= 1,
            "needs the rounded mask while growing"
        );
        let mut t = 0.1;
        while s.animating() {
            t += 1.0 / 60.0;
            s.step(t);
        }
        let mut settled = DrawList::new();
        compose(
            &s.frame(),
            &demo::page_sizes(),
            &Theme::default(),
            400.0,
            &Metrics::default(),
            &mut settled,
            &mut Demo,
        );
        assert_eq!(
            count_clips(&settled),
            0,
            "no per-frame layer once the page fits"
        );
        assert!(settled.is_balanced());
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

    #[test]
    fn warmup_covers_every_page_peek_and_the_chips() {
        let s = shell();
        let pages = demo::page_sizes();
        let peeks = [(0u32, Size::new(260.0, 44.0))];
        let theme = Theme::default();
        let m = Metrics::default();
        let plan = Warmup {
            pages: &pages,
            peeks: &peeks,
            chips_width: 90.0,
            theme: &theme,
            window_w: 400.0,
            metrics: &m,
        };
        let mut list = DrawList::new();
        let mut seen = Vec::new();
        let n = plan.run(&s.frame(), &mut list, &mut Demo, |l| {
            assert!(l.is_balanced(), "every warm-up list is well formed");
            seen.push((l.cmds.len(), count_clips(l)));
            true
        });
        assert_eq!(n, pages.len() + peeks.len() + 1);
        assert_eq!(seen.len(), n);
        assert!(
            seen[..pages.len()]
                .iter()
                .all(|&(cmds, clips)| cmds > 3 && clips >= 1),
            "pages draw content and exercise the clip layer"
        );
        assert!(
            seen.last().is_some_and(|&(cmds, _)| cmds > 1),
            "chips are drawn"
        );
    }

    #[test]
    fn warmup_stops_when_the_sink_says_so_and_skips_absent_parts() {
        let s = shell();
        let pages = demo::page_sizes();
        let theme = Theme::default();
        let m = Metrics::default();
        let plan = Warmup {
            pages: &pages,
            peeks: &[],
            chips_width: 0.0,
            theme: &theme,
            window_w: 400.0,
            metrics: &m,
        };
        let mut list = DrawList::new();
        assert_eq!(
            plan.run(&s.frame(), &mut list, &mut Demo, |_| true),
            pages.len(),
            "no peeks, no chips"
        );
        assert_eq!(
            plan.run(&s.frame(), &mut list, &mut Demo, |_| false),
            1,
            "stops after the first failure"
        );
    }
}
