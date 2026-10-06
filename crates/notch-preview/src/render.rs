//! Executes a `notch-core` display list with tiny-skia + fontdue. Dev tool only: it mirrors what the
//! Direct2D backend does so layouts can be inspected as PNGs on any OS.

use std::collections::HashMap;

use fontdue::{Font, FontSettings};
use notch_core::color::Color;
use notch_core::draw::{Align, DrawCmd, DrawList, ImageId, TextStyle, Weight};
use notch_core::geom::{Rect, Vec2};
use notch_core::icons::{self, GRID, IconOp};
use notch_core::path::{Path as NPath, PathCmd, corner_curve, emit_corner};
use tiny_skia::{
    BlendMode, FillRule, GradientStop, LineCap, LineJoin, LinearGradient, Mask, Paint, PathBuilder,
    Pixmap, PixmapPaint, Point, RadialGradient, SpreadMode, Stroke, Transform,
};

pub struct Fonts {
    regular: Font,
    medium: Font,
    semibold: Font,
    bold: Font,
}

impl Fonts {
    pub fn load() -> Fonts {
        let load = |names: &[&str]| -> Font {
            for n in names {
                if let Ok(bytes) = std::fs::read(n)
                    && let Ok(f) = Font::from_bytes(bytes, FontSettings::default())
                {
                    return f;
                }
            }
            panic!("no usable font found among {names:?}");
        };
        let dejavu = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf";
        let dejavu_b = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf";
        Fonts {
            regular: load(&["/usr/share/fonts/opentype/inter/Inter-Regular.otf", dejavu]),
            medium: load(&["/usr/share/fonts/opentype/inter/Inter-Medium.otf", dejavu]),
            semibold: load(&[
                "/usr/share/fonts/opentype/inter/Inter-SemiBold.otf",
                dejavu_b,
            ]),
            bold: load(&["/usr/share/fonts/opentype/inter/Inter-Bold.otf", dejavu_b]),
        }
    }

    fn pick(&self, w: Weight) -> &Font {
        match w {
            Weight::Regular => &self.regular,
            Weight::Medium => &self.medium,
            Weight::SemiBold => &self.semibold,
            Weight::Bold => &self.bold,
        }
    }

    pub fn measure(&self, text: &str, style: &TextStyle) -> f32 {
        let font = self.pick(style.weight);
        text.chars()
            .map(|c| font.metrics(c, style.size).advance_width)
            .sum()
    }
}

/// Synthetic images for previews (the real app decodes album art etc.).
#[derive(Default)]
pub struct Images {
    map: HashMap<u64, Pixmap>,
}

impl Images {
    pub fn insert(&mut self, id: ImageId, px: Pixmap) {
        self.map.insert(id.0, px);
    }
}

pub struct Renderer<'a> {
    pub pix: Pixmap,
    pub scale: f32,
    fonts: &'a Fonts,
    images: &'a Images,
}

struct State {
    alpha: f32,
    transform: Transform,
    mask: Option<Mask>,
}

fn paint_for(c: Color, alpha: f32) -> Paint<'static> {
    let mut p = Paint::default();
    let c = c.mul_alpha(alpha);
    p.set_color_rgba8(
        (c.r * 255.0 + 0.5) as u8,
        (c.g * 255.0 + 0.5) as u8,
        (c.b * 255.0 + 0.5) as u8,
        (c.a * 255.0 + 0.5) as u8,
    );
    p.anti_alias = true;
    p
}

pub fn to_skia_path(path: &NPath) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    for c in &path.cmds {
        match *c {
            PathCmd::MoveTo(p) => pb.move_to(p.x, p.y),
            PathCmd::LineTo(p) => pb.line_to(p.x, p.y),
            PathCmd::CubicTo(a, b, p) => pb.cubic_to(a.x, a.y, b.x, b.y, p.x, p.y),
            PathCmd::Close => pb.close(),
        }
    }
    pb.finish()
}

fn rrect_path(rect: Rect, radii: [f32; 4], smooth: bool) -> Option<tiny_skia::Path> {
    if smooth {
        return to_skia_path(&notch_core::path::rounded_rect_path(rect, radii, 0.6));
    }
    // Plain circular corners.
    let mut p = NPath::new();
    let max = (rect.w * 0.5).min(rect.h * 0.5);
    let r: Vec<f32> = radii.iter().map(|r| r.clamp(0.0, max)).collect();
    let c: Vec<_> = r.iter().map(|&r| corner_curve(r, 0.0)).collect();
    let (x0, y0, x1, y1) = (rect.x, rect.y, rect.right(), rect.bottom());
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
    to_skia_path(&p)
}

impl<'a> Renderer<'a> {
    pub fn new(w_dip: f32, h_dip: f32, scale: f32, fonts: &'a Fonts, images: &'a Images) -> Self {
        let pix = Pixmap::new((w_dip * scale).ceil() as u32, (h_dip * scale).ceil() as u32)
            .expect("pixmap");
        Self {
            pix,
            scale,
            fonts,
            images,
        }
    }

    pub fn draw_shape_path(&mut self, path: &NPath, offset: Vec2, color: Color) {
        if let Some(p) = to_skia_path(path) {
            let t = Transform::from_scale(self.scale, self.scale).pre_translate(offset.x, offset.y);
            self.pix
                .fill_path(&p, &paint_for(color, 1.0), FillRule::Winding, t, None);
        }
    }

    pub fn stroke_shape_path(&mut self, path: &NPath, offset: Vec2, color: Color, width: f32) {
        if let Some(p) = to_skia_path(path) {
            let t = Transform::from_scale(self.scale, self.scale).pre_translate(offset.x, offset.y);
            let stroke = Stroke {
                width,
                ..Default::default()
            };
            self.pix
                .stroke_path(&p, &paint_for(color, 1.0), &stroke, t, None);
        }
    }

    /// Execute `list` with an extra origin offset (DIPs).
    pub fn draw_list(&mut self, list: &DrawList, origin: Vec2, base_alpha: f32) {
        let base = Transform::from_scale(self.scale, self.scale).pre_translate(origin.x, origin.y);
        let mut stack = vec![State {
            alpha: base_alpha,
            transform: base,
            mask: None,
        }];
        for cmd in &list.cmds {
            match cmd {
                DrawCmd::PushClip { rect, radius } => {
                    let top = stack.last().unwrap();
                    let mut mask = top
                        .mask
                        .clone()
                        .unwrap_or_else(|| Mask::new(self.pix.width(), self.pix.height()).unwrap());
                    if top.mask.is_none() {
                        mask.fill_path(
                            &tiny_skia::PathBuilder::from_rect(
                                tiny_skia::Rect::from_xywh(
                                    0.0,
                                    0.0,
                                    self.pix.width() as f32,
                                    self.pix.height() as f32,
                                )
                                .unwrap(),
                            ),
                            FillRule::Winding,
                            false,
                            Transform::identity(),
                        );
                    }
                    if let Some(p) = rrect_path(*rect, [*radius; 4], false) {
                        mask.intersect_path(&p, FillRule::Winding, true, top.transform);
                    }
                    let (alpha, transform) = (top.alpha, top.transform);
                    stack.push(State {
                        alpha,
                        transform,
                        mask: Some(mask),
                    });
                }
                DrawCmd::PopClip | DrawCmd::PopGroup => {
                    if stack.len() > 1 {
                        stack.pop();
                    }
                }
                DrawCmd::PushGroup {
                    alpha,
                    scale,
                    origin,
                } => {
                    let top = stack.last().unwrap();
                    let t = top
                        .transform
                        .pre_translate(origin.x, origin.y)
                        .pre_scale(*scale, *scale)
                        .pre_translate(-origin.x, -origin.y);
                    stack.push(State {
                        alpha: top.alpha * alpha,
                        transform: t,
                        mask: top.mask.clone(),
                    });
                }
                other => self.draw_one(other, stack.last().unwrap()),
            }
        }
    }

    fn draw_one(&mut self, cmd: &DrawCmd, st: &State) {
        let mask = st.mask.as_ref();
        match cmd {
            DrawCmd::Notch {
                shape,
                origin,
                fill,
                outline,
            } => {
                let path = shape.to_path();
                if let Some(p) = to_skia_path(&path) {
                    let t = st.transform.pre_translate(origin.x, origin.y);
                    self.pix
                        .fill_path(&p, &paint_for(*fill, st.alpha), FillRule::Winding, t, mask);
                    if let Some((w, c)) = outline {
                        let stroke = Stroke {
                            width: *w,
                            ..Default::default()
                        };
                        self.pix
                            .stroke_path(&p, &paint_for(*c, st.alpha), &stroke, t, mask);
                    }
                }
            }
            DrawCmd::RoundRect {
                rect,
                radius,
                color,
            } => {
                if let Some(p) = rrect_path(*rect, [*radius; 4], false) {
                    self.pix.fill_path(
                        &p,
                        &paint_for(*color, st.alpha),
                        FillRule::Winding,
                        st.transform,
                        mask,
                    );
                }
            }
            DrawCmd::Squircle { rect, radii, color } => {
                if let Some(p) = rrect_path(*rect, *radii, true) {
                    self.pix.fill_path(
                        &p,
                        &paint_for(*color, st.alpha),
                        FillRule::Winding,
                        st.transform,
                        mask,
                    );
                }
            }
            DrawCmd::StrokeRoundRect {
                rect,
                radius,
                width,
                color,
            } => {
                if let Some(p) = rrect_path(rect.inset(width * 0.5), [*radius; 4], false) {
                    let stroke = Stroke {
                        width: *width,
                        ..Default::default()
                    };
                    self.pix.stroke_path(
                        &p,
                        &paint_for(*color, st.alpha),
                        &stroke,
                        st.transform,
                        mask,
                    );
                }
            }
            DrawCmd::Circle {
                center,
                radius,
                color,
            } => {
                if let Some(p) = PathBuilder::from_circle(center.x, center.y, *radius) {
                    self.pix.fill_path(
                        &p,
                        &paint_for(*color, st.alpha),
                        FillRule::Winding,
                        st.transform,
                        mask,
                    );
                }
            }
            DrawCmd::Ring {
                center,
                radius,
                width,
                start_deg,
                sweep_deg,
                color,
            } => {
                let arc = icons::arc_path(center.x, center.y, *radius, *start_deg, *sweep_deg);
                if let Some(p) = to_skia_path(&arc) {
                    let stroke = Stroke {
                        width: *width,
                        line_cap: LineCap::Round,
                        line_join: LineJoin::Round,
                        ..Default::default()
                    };
                    self.pix.stroke_path(
                        &p,
                        &paint_for(*color, st.alpha),
                        &stroke,
                        st.transform,
                        mask,
                    );
                }
            }
            DrawCmd::Line { a, b, width, color } => {
                let mut pb = PathBuilder::new();
                pb.move_to(a.x, a.y);
                pb.line_to(b.x, b.y);
                if let Some(p) = pb.finish() {
                    let stroke = Stroke {
                        width: *width,
                        line_cap: LineCap::Round,
                        ..Default::default()
                    };
                    self.pix.stroke_path(
                        &p,
                        &paint_for(*color, st.alpha),
                        &stroke,
                        st.transform,
                        mask,
                    );
                }
            }
            DrawCmd::Icon { icon, rect, color } => {
                let k = rect.w.min(rect.h) / GRID;
                let ox = rect.x + (rect.w - GRID * k) * 0.5;
                let oy = rect.y + (rect.h - GRID * k) * 0.5;
                let t = st.transform.pre_translate(ox, oy).pre_scale(k, k);
                for op in icons::ops(*icon) {
                    match op {
                        IconOp::Fill(p) => {
                            if let Some(p) = to_skia_path(&p) {
                                self.pix.fill_path(
                                    &p,
                                    &paint_for(*color, st.alpha),
                                    FillRule::Winding,
                                    t,
                                    mask,
                                );
                            }
                        }
                        IconOp::Stroke(p, w) => {
                            if let Some(p) = to_skia_path(&p) {
                                let stroke = Stroke {
                                    width: w,
                                    line_cap: LineCap::Round,
                                    line_join: LineJoin::Round,
                                    ..Default::default()
                                };
                                self.pix.stroke_path(
                                    &p,
                                    &paint_for(*color, st.alpha),
                                    &stroke,
                                    t,
                                    mask,
                                );
                            }
                        }
                    }
                }
            }
            DrawCmd::Image {
                id,
                rect,
                radius,
                opacity,
            } => {
                if let Some(img) = self.images.map.get(&id.0) {
                    let sx = rect.w / img.width() as f32;
                    let sy = rect.h / img.height() as f32;
                    let t = st.transform.pre_translate(rect.x, rect.y).pre_scale(sx, sy);
                    let mut m = Mask::new(self.pix.width(), self.pix.height()).unwrap();
                    if let Some(p) = rrect_path(*rect, [*radius; 4], true) {
                        m.fill_path(&p, FillRule::Winding, true, st.transform);
                    }
                    if let Some(prev) = mask {
                        for (d, p) in m.data_mut().iter_mut().zip(prev.data()) {
                            *d = ((*d as u16 * *p as u16) / 255) as u8;
                        }
                    }
                    let paint = PixmapPaint {
                        opacity: opacity * st.alpha,
                        blend_mode: BlendMode::SourceOver,
                        quality: tiny_skia::FilterQuality::Bilinear,
                    };
                    self.pix
                        .draw_pixmap(0, 0, img.as_ref(), &paint, t, Some(&m));
                }
            }
            DrawCmd::Gradient {
                rect,
                radius,
                from,
                to,
                vertical,
            } => {
                if let Some(p) = rrect_path(*rect, [*radius; 4], false) {
                    let (a, b) = if *vertical {
                        (
                            Point::from_xy(rect.x, rect.y),
                            Point::from_xy(rect.x, rect.bottom()),
                        )
                    } else {
                        (
                            Point::from_xy(rect.x, rect.y),
                            Point::from_xy(rect.right(), rect.y),
                        )
                    };
                    let col = |c: &Color| {
                        tiny_skia::Color::from_rgba(c.r, c.g, c.b, (c.a * st.alpha).clamp(0.0, 1.0))
                            .unwrap()
                    };
                    if let Some(sh) = LinearGradient::new(
                        a,
                        b,
                        vec![
                            GradientStop::new(0.0, col(from)),
                            GradientStop::new(1.0, col(to)),
                        ],
                        SpreadMode::Pad,
                        Transform::identity(),
                    ) {
                        let mut paint = Paint::default();
                        paint.shader = sh;
                        paint.anti_alias = true;
                        self.pix
                            .fill_path(&p, &paint, FillRule::Winding, st.transform, mask);
                    }
                }
            }
            DrawCmd::Glow {
                center,
                radius,
                color,
            } => {
                let c0 = tiny_skia::Color::from_rgba(
                    color.r,
                    color.g,
                    color.b,
                    (color.a * st.alpha).clamp(0.0, 1.0),
                )
                .unwrap();
                let c1 = tiny_skia::Color::from_rgba(color.r, color.g, color.b, 0.0).unwrap();
                if let Some(sh) = RadialGradient::new(
                    Point::from_xy(center.x, center.y),
                    Point::from_xy(center.x, center.y),
                    *radius,
                    vec![GradientStop::new(0.0, c0), GradientStop::new(1.0, c1)],
                    SpreadMode::Pad,
                    Transform::identity(),
                ) {
                    let mut paint = Paint::default();
                    paint.shader = sh;
                    if let Some(r) = tiny_skia::Rect::from_xywh(
                        center.x - radius,
                        center.y - radius,
                        radius * 2.0,
                        radius * 2.0,
                    ) {
                        self.pix.fill_rect(r, &paint, st.transform, mask);
                    }
                }
            }
            DrawCmd::Text {
                rect,
                text,
                style,
                color,
            } => self.draw_text(*rect, text.as_str(), style, *color, st),
            DrawCmd::PushClip { .. }
            | DrawCmd::PopClip
            | DrawCmd::PushGroup { .. }
            | DrawCmd::PopGroup => unreachable!(),
        }
    }

    fn draw_text(&mut self, rect: Rect, text: &str, style: &TextStyle, color: Color, st: &State) {
        let font = self.fonts.pick(style.weight);
        let px = style.size * st.transform.sx; // device pixels per em (uniform scale assumed)
        let line_metrics = font
            .horizontal_line_metrics(px)
            .unwrap_or(fontdue::LineMetrics {
                ascent: px * 0.9,
                descent: -px * 0.25,
                line_gap: 0.0,
                new_line_size: px * 1.2,
            });
        let line_h = line_metrics.new_line_size.max(px * 1.2);
        let max_w = rect.w * st.transform.sx;

        // Greedy layout: split into at most `lines` lines; ellipsize the last if needed.
        let adv = |s: &str| -> f32 { s.chars().map(|c| font.metrics(c, px).advance_width).sum() };
        let mut lines: Vec<String> = Vec::new();
        let words: Vec<&str> = text.split(' ').collect();
        let mut cur = String::new();
        for w in words {
            let cand = if cur.is_empty() {
                w.to_string()
            } else {
                format!("{cur} {w}")
            };
            if style.lines > 1
                && adv(&cand) > max_w
                && !cur.is_empty()
                && (lines.len() as u8) + 1 < style.lines
            {
                lines.push(std::mem::take(&mut cur));
                cur = w.to_string();
            } else {
                cur = cand;
            }
        }
        lines.push(cur);
        let n = lines.len();
        for (i, l) in lines.iter_mut().enumerate() {
            if style.ellipsis && adv(l) > max_w && (i == n - 1) {
                while !l.is_empty() && adv(&format!("{l}…")) > max_w {
                    l.pop();
                }
                l.push('…');
            }
        }

        let block_h = line_h * n as f32;
        let top =
            rect.y * st.transform.sy + st.transform.ty + (rect.h * st.transform.sy - block_h) * 0.5;
        let content_h = line_metrics.ascent - line_metrics.descent; // descent is negative
        for (i, l) in lines.iter().enumerate() {
            let w = adv(l);
            let left = rect.x * st.transform.sx + st.transform.tx;
            let x0 = match style.align {
                Align::Start => left,
                Align::Center => left + (max_w - w) * 0.5,
                Align::End => left + (max_w - w),
            };
            let baseline =
                top + line_h * i as f32 + (line_h - content_h) * 0.5 + line_metrics.ascent;
            let mut x = x0;
            for ch in l.chars() {
                let (m, bitmap) = font.rasterize(ch, px);
                let gx = (x + m.xmin as f32).round() as i32;
                let gy = (baseline - m.height as f32 - m.ymin as f32).round() as i32;
                self.blit_glyph(
                    &bitmap,
                    m.width,
                    m.height,
                    gx,
                    gy,
                    color.mul_alpha(st.alpha),
                    st.mask.as_ref(),
                );
                x += m.advance_width;
            }
        }
    }

    fn blit_glyph(
        &mut self,
        cov: &[u8],
        w: usize,
        h: usize,
        x: i32,
        y: i32,
        color: Color,
        mask: Option<&Mask>,
    ) {
        let (pw, ph) = (self.pix.width() as i32, self.pix.height() as i32);
        let data = self.pix.data_mut();
        for row in 0..h as i32 {
            for col in 0..w as i32 {
                let (px, py) = (x + col, y + row);
                if px < 0 || py < 0 || px >= pw || py >= ph {
                    continue;
                }
                let mut a = cov[row as usize * w + col as usize] as f32 / 255.0 * color.a;
                if let Some(m) = mask {
                    a *= m.data()[(py * pw + px) as usize] as f32 / 255.0;
                }
                if a <= 0.0 {
                    continue;
                }
                let idx = ((py * pw + px) * 4) as usize;
                // tiny-skia pixmaps are premultiplied RGBA.
                let blend = |dst: u8, src: f32| {
                    (src * a * 255.0 + dst as f32 * (1.0 - a))
                        .round()
                        .clamp(0.0, 255.0) as u8
                };
                data[idx] = blend(data[idx], color.r);
                data[idx + 1] = blend(data[idx + 1], color.g);
                data[idx + 2] = blend(data[idx + 2], color.b);
                data[idx + 3] = ((a * 255.0) + data[idx + 3] as f32 * (1.0 - a))
                    .round()
                    .clamp(0.0, 255.0) as u8;
            }
        }
    }
}

/// Paint a fake desktop (wallpaper gradient + a browser-like window with a tab strip) so the notch's
/// edges and ears can be judged against realistic content.
pub fn fake_desktop(pix: &mut Pixmap, scale: f32) {
    let (w, h) = (pix.width() as f32, pix.height() as f32);
    let mut paint = Paint::default();
    paint.shader = LinearGradient::new(
        Point::from_xy(0.0, 0.0),
        Point::from_xy(w, h),
        vec![
            GradientStop::new(0.0, tiny_skia::Color::from_rgba8(36, 70, 120, 255)),
            GradientStop::new(1.0, tiny_skia::Color::from_rgba8(200, 120, 90, 255)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap();
    pix.fill_rect(
        tiny_skia::Rect::from_xywh(0.0, 0.0, w, h).unwrap(),
        &paint,
        Transform::identity(),
        None,
    );
    // A maximised window with a light tab strip right at the top edge (the thing the notch must not hide).
    let mut bar = Paint::default();
    bar.set_color_rgba8(236, 238, 242, 255);
    pix.fill_rect(
        tiny_skia::Rect::from_xywh(0.0, 0.0, w, 36.0 * scale).unwrap(),
        &bar,
        Transform::identity(),
        None,
    );
    let mut tab = Paint::default();
    tab.set_color_rgba8(255, 255, 255, 255);
    let mut x = 20.0 * scale;
    for _ in 0..5 {
        if let Some(r) = tiny_skia::Rect::from_xywh(x, 6.0 * scale, 150.0 * scale, 30.0 * scale) {
            pix.fill_rect(r, &tab, Transform::identity(), None);
        }
        x += 156.0 * scale;
    }
    let mut body = Paint::default();
    body.set_color_rgba8(250, 250, 252, 255);
    pix.fill_rect(
        tiny_skia::Rect::from_xywh(0.0, 36.0 * scale, w, h - 36.0 * scale).unwrap(),
        &body,
        Transform::identity(),
        None,
    );
}
