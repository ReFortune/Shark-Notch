//! Executes a `notch-core` display list with Direct2D / DirectWrite.
//!
//! The semantics mirror `notch-preview` (tiny-skia) so layouts inspected as PNGs match the app:
//! * `PushGroup` multiplies alpha and scales about an origin (no offscreen surface needed);
//! * `PushClip` is an anti-aliased rounded mask (a D2D layer) or an axis-aligned clip when square;
//! * text is DirectWrite, grayscale AA, vertically centred in its rect, ellipsis-trimmed;
//! * icons are the shared vector paths, geometry cached per icon.

use std::collections::HashMap;
use std::ffi::c_void;
use std::hash::{Hash, Hasher};
use std::mem::ManuallyDrop;
use std::sync::Arc;

use notch_core::color::Color;
use notch_core::draw::{Align, DrawCmd, DrawList, ImageId, TextStyle, Weight};
use notch_core::geom::{Rect, Vec2};
use notch_core::icons::{self, GRID, Icon, IconOp};
use notch_core::image::{ImageCache, ImageData};
use notch_core::path::{Path, PathCmd, rounded_rect_path};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_BEZIER_SEGMENT, D2D1_COLOR_F,
    D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN, D2D1_FILL_MODE_WINDING,
    D2D1_GRADIENT_STOP, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_BRUSH_PROPERTIES1, D2D1_BITMAP_OPTIONS_NONE,
    D2D1_BITMAP_PROPERTIES1, D2D1_BRUSH_PROPERTIES, D2D1_BUFFER_PRECISION_8BPC_UNORM,
    D2D1_CAP_STYLE_ROUND, D2D1_COLOR_INTERPOLATION_MODE_PREMULTIPLIED, D2D1_COLOR_SPACE_SRGB,
    D2D1_DASH_STYLE_SOLID, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_DRAW_TEXT_OPTIONS_NO_SNAP,
    D2D1_ELLIPSE, D2D1_EXTEND_MODE_CLAMP, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
    D2D1_LAYER_OPTIONS1_NONE, D2D1_LAYER_PARAMETERS1, D2D1_LINE_JOIN_ROUND,
    D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES, D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES,
    D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES1, D2D1_STROKE_TRANSFORM_TYPE_NORMAL,
    ID2D1Bitmap1, ID2D1Brush, ID2D1DeviceContext, ID2D1Geometry, ID2D1Layer, ID2D1PathGeometry1,
    ID2D1SolidColorBrush, ID2D1StrokeStyle,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_MEDIUM, DWRITE_FONT_WEIGHT_REGULAR,
    DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TRIMMING,
    DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP, DWRITE_WORD_WRAPPING_WRAP,
    IDWriteFactory, IDWriteFontCollection, IDWriteTextFormat, IDWriteTextLayout,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::core::{BOOL, Interface, PCWSTR, Result};
use windows_numerics::{Matrix3x2, Vector2};

use super::stack::GpuStack;
use crate::win::util::wide;

/// A 2-D affine transform with explicit, documented composition (`a.then(b)` applies `a` first).
#[derive(Clone, Copy, Debug)]
struct Mat {
    m11: f32,
    m12: f32,
    m21: f32,
    m22: f32,
    dx: f32,
    dy: f32,
}

impl Mat {
    const IDENTITY: Mat = Mat {
        m11: 1.0,
        m12: 0.0,
        m21: 0.0,
        m22: 1.0,
        dx: 0.0,
        dy: 0.0,
    };

    fn scale_about(k: f32, o: Vec2) -> Mat {
        Mat {
            m11: k,
            m12: 0.0,
            m21: 0.0,
            m22: k,
            dx: o.x - k * o.x,
            dy: o.y - k * o.y,
        }
    }

    fn translate(dx: f32, dy: f32) -> Mat {
        Mat {
            dx,
            dy,
            ..Mat::IDENTITY
        }
    }

    fn scale(k: f32) -> Mat {
        Mat {
            m11: k,
            m22: k,
            ..Mat::IDENTITY
        }
    }

    /// `self` first, then `o` (row-vector convention, as Direct2D uses).
    fn then(self, o: Mat) -> Mat {
        Mat {
            m11: self.m11 * o.m11 + self.m12 * o.m21,
            m12: self.m11 * o.m12 + self.m12 * o.m22,
            m21: self.m21 * o.m11 + self.m22 * o.m21,
            m22: self.m21 * o.m12 + self.m22 * o.m22,
            dx: self.dx * o.m11 + self.dy * o.m21 + o.dx,
            dy: self.dx * o.m12 + self.dy * o.m22 + o.dy,
        }
    }

    fn to_d2d(self) -> Matrix3x2 {
        Matrix3x2 {
            M11: self.m11,
            M12: self.m12,
            M21: self.m21,
            M22: self.m22,
            M31: self.dx,
            M32: self.dy,
        }
    }
}

fn d2d_color(c: Color, alpha: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: c.r,
        g: c.g,
        b: c.b,
        a: (c.a * alpha).clamp(0.0, 1.0),
    }
}

fn rect_f(r: Rect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.x,
        top: r.y,
        right: r.right(),
        bottom: r.bottom(),
    }
}

fn v2(p: Vec2) -> Vector2 {
    Vector2 { X: p.x, Y: p.y }
}

/// GPU copies of decoded images, uploaded on first use from the shared CPU cache.
///
/// The cache (owned by the app, filled by worker threads) is the source of truth; this store only
/// holds Direct2D bitmaps for the images that were actually drawn, and forgets the ones the cache
/// no longer has. Releasing the GPU stack drops the whole store; the next draw re-uploads.
pub struct ImageStore {
    cache: Arc<ImageCache>,
    bitmaps: HashMap<u64, ID2D1Bitmap1>,
    seen_generation: u64,
}

impl ImageStore {
    pub fn new(cache: Arc<ImageCache>) -> ImageStore {
        ImageStore {
            seen_generation: cache.generation(),
            cache,
            bitmaps: HashMap::new(),
        }
    }

    /// The GPU bitmap for `id`, uploading it now if needed. `None` if the image no longer exists.
    fn bitmap(&mut self, dc: &ID2D1DeviceContext, id: ImageId) -> Option<ID2D1Bitmap1> {
        let generation = self.cache.generation();
        if generation != self.seen_generation {
            self.seen_generation = generation;
            let cache = &self.cache;
            self.bitmaps.retain(|k, _| cache.contains(ImageId(*k)));
        }
        if let Some(b) = self.bitmaps.get(&id.0) {
            return Some(b.clone());
        }
        let data = self.cache.get(id)?;
        let bmp = upload(dc, &data)
            .map_err(|e| crate::warn!("cannot upload image {}: {e}", id.0))
            .ok()?;
        self.bitmaps.insert(id.0, bmp.clone());
        Some(bmp)
    }

    /// How many images currently have a GPU copy (diagnostics).
    pub fn uploaded(&self) -> usize {
        self.bitmaps.len()
    }
}

/// Create a premultiplied-BGRA Direct2D bitmap from decoded pixels.
fn upload(dc: &ID2D1DeviceContext, data: &ImageData) -> Result<ID2D1Bitmap1> {
    let props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
        colorContext: ManuallyDrop::new(None),
    };
    unsafe {
        dc.CreateBitmap(
            D2D_SIZE_U {
                width: data.w,
                height: data.h,
            },
            Some(data.bgra.as_ptr() as *const c_void),
            data.w * 4,
            &props,
        )
    }
}

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
struct FormatKey {
    size_bits: u32,
    weight: u8,
    align: u8,
    wrap: bool,
    ellipsis: bool,
}

struct IconGeom {
    geom: ID2D1PathGeometry1,
    /// `Some(width)` for strokes, `None` for fills.
    stroke: Option<f32>,
}

pub struct Renderer {
    brush: ID2D1SolidColorBrush,
    round_stroke: ID2D1StrokeStyle,
    formats: HashMap<FormatKey, IDWriteTextFormat>,
    layouts: HashMap<u64, IDWriteTextLayout>,
    icons: HashMap<Icon, Vec<IconGeom>>,
    family_text: Vec<u16>,
    family_display: Vec<u16>,
    xform: Vec<Mat>,
    alpha: Vec<f32>,
    /// For each pushed clip: `true` if it was a layer (needs `PopLayer`), `false` for an axis clip.
    clips: Vec<bool>,
}

fn pick_family(dw: &IDWriteFactory, candidates: &[&str]) -> String {
    unsafe {
        let mut coll: Option<IDWriteFontCollection> = None;
        if dw.GetSystemFontCollection(&mut coll, false).is_ok()
            && let Some(coll) = coll
        {
            for name in candidates {
                let w = wide(name);
                let (mut idx, mut exists) = (0u32, BOOL(0));
                if coll
                    .FindFamilyName(PCWSTR(w.as_ptr()), &mut idx, &mut exists)
                    .is_ok()
                    && exists.as_bool()
                {
                    return (*name).to_string();
                }
            }
        }
    }
    "Segoe UI".to_string()
}

impl Renderer {
    pub fn new(gpu: &GpuStack) -> Result<Renderer> {
        unsafe {
            let brush = gpu.dc.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                },
                None,
            )?;
            let props = D2D1_STROKE_STYLE_PROPERTIES1 {
                startCap: D2D1_CAP_STYLE_ROUND,
                endCap: D2D1_CAP_STYLE_ROUND,
                dashCap: D2D1_CAP_STYLE_ROUND,
                lineJoin: D2D1_LINE_JOIN_ROUND,
                miterLimit: 10.0,
                dashStyle: D2D1_DASH_STYLE_SOLID,
                dashOffset: 0.0,
                transformType: D2D1_STROKE_TRANSFORM_TYPE_NORMAL,
            };
            let round_stroke: ID2D1StrokeStyle =
                gpu.d2d_factory.CreateStrokeStyle(&props, None)?.cast()?;
            let text = pick_family(&gpu.dwrite, &["Segoe UI Variable Text", "Segoe UI"]);
            let display = pick_family(
                &gpu.dwrite,
                &[
                    "Segoe UI Variable Display",
                    "Segoe UI Variable Text",
                    "Segoe UI",
                ],
            );
            crate::info!("fonts: text='{text}' display='{display}'");
            Ok(Renderer {
                brush,
                round_stroke,
                formats: HashMap::new(),
                layouts: HashMap::new(),
                icons: HashMap::new(),
                family_text: wide(&text),
                family_display: wide(&display),
                xform: vec![Mat::IDENTITY],
                alpha: vec![1.0],
                clips: Vec::new(),
            })
        }
    }

    /// Drop cached text layouts (they are cheap to rebuild and otherwise accumulate).
    pub fn trim(&mut self) {
        self.layouts.clear();
    }

    fn set_brush(&self, c: Color, alpha: f32) -> &ID2D1SolidColorBrush {
        unsafe { self.brush.SetColor(&d2d_color(c, alpha)) };
        &self.brush
    }

    fn cur_alpha(&self) -> f32 {
        *self.alpha.last().unwrap_or(&1.0)
    }

    fn cur_xform(&self) -> Mat {
        *self.xform.last().unwrap_or(&Mat::IDENTITY)
    }

    // ----- geometry -------------------------------------------------------------------------

    fn geometry(&self, gpu: &GpuStack, path: &Path) -> Result<ID2D1PathGeometry1> {
        unsafe {
            let geom = gpu.d2d_factory.CreatePathGeometry()?;
            let sink = geom.Open()?;
            sink.SetFillMode(D2D1_FILL_MODE_WINDING);
            let mut open = false;
            for cmd in &path.cmds {
                match *cmd {
                    PathCmd::MoveTo(p) => {
                        if open {
                            sink.EndFigure(D2D1_FIGURE_END_OPEN);
                        }
                        sink.BeginFigure(v2(p), D2D1_FIGURE_BEGIN_FILLED);
                        open = true;
                    }
                    PathCmd::LineTo(p) => sink.AddLine(v2(p)),
                    PathCmd::CubicTo(a, b, p) => sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                        point1: v2(a),
                        point2: v2(b),
                        point3: v2(p),
                    }),
                    PathCmd::Close => {
                        if open {
                            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                            open = false;
                        }
                    }
                }
            }
            if open {
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
            }
            sink.Close()?;
            Ok(geom)
        }
    }

    fn icon_geoms(&mut self, gpu: &GpuStack, icon: Icon) -> Result<&Vec<IconGeom>> {
        if !self.icons.contains_key(&icon) {
            let mut v = Vec::new();
            for op in icons::ops(icon) {
                match op {
                    IconOp::Fill(p) => v.push(IconGeom {
                        geom: self.geometry(gpu, &p)?,
                        stroke: None,
                    }),
                    IconOp::Stroke(p, w) => v.push(IconGeom {
                        geom: self.geometry(gpu, &p)?,
                        stroke: Some(w),
                    }),
                }
            }
            self.icons.insert(icon, v);
        }
        Ok(&self.icons[&icon])
    }

    // ----- text -----------------------------------------------------------------------------

    fn text_format(&mut self, gpu: &GpuStack, style: &TextStyle) -> Result<IDWriteTextFormat> {
        let key = FormatKey {
            size_bits: style.size.to_bits(),
            weight: style.weight as u8,
            align: style.align as u8,
            wrap: style.lines > 1,
            ellipsis: style.ellipsis,
        };
        if let Some(f) = self.formats.get(&key) {
            return Ok(f.clone());
        }
        let weight: DWRITE_FONT_WEIGHT = match style.weight {
            Weight::Regular => DWRITE_FONT_WEIGHT_REGULAR,
            Weight::Medium => DWRITE_FONT_WEIGHT_MEDIUM,
            Weight::SemiBold => DWRITE_FONT_WEIGHT_SEMI_BOLD,
            Weight::Bold => DWRITE_FONT_WEIGHT_BOLD,
        };
        // Segoe UI Variable has an optical-size axis; use its Display cut for large text.
        let family = if style.size >= 20.0 {
            &self.family_display
        } else {
            &self.family_text
        };
        let locale = wide("en-us");
        unsafe {
            let f = gpu.dwrite.CreateTextFormat(
                PCWSTR(family.as_ptr()),
                None::<&IDWriteFontCollection>,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                style.size,
                PCWSTR(locale.as_ptr()),
            )?;
            f.SetTextAlignment(match style.align {
                Align::Start => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::End => DWRITE_TEXT_ALIGNMENT_TRAILING,
            })?;
            f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            f.SetWordWrapping(if style.lines > 1 {
                DWRITE_WORD_WRAPPING_WRAP
            } else {
                DWRITE_WORD_WRAPPING_NO_WRAP
            })?;
            if style.ellipsis {
                let sign = gpu.dwrite.CreateEllipsisTrimmingSign(&f)?;
                let trimming = DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    delimiter: 0,
                    delimiterCount: 0,
                };
                f.SetTrimming(&trimming, &sign)?;
            }
            self.formats.insert(key, f.clone());
            Ok(f)
        }
    }

    fn text_layout(
        &mut self,
        gpu: &GpuStack,
        text: &str,
        style: &TextStyle,
        w: f32,
        h: f32,
    ) -> Result<IDWriteTextLayout> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        (
            style.size.to_bits(),
            style.weight as u8,
            style.align as u8,
            style.lines,
            style.ellipsis,
            style.tabular,
            w.to_bits(),
            h.to_bits(),
        )
            .hash(&mut hasher);
        let key = hasher.finish();
        if let Some(l) = self.layouts.get(&key) {
            return Ok(l.clone());
        }
        let format = self.text_format(gpu, style)?;
        let utf16: Vec<u16> = text.encode_utf16().collect();
        let layout = unsafe {
            gpu.dwrite
                .CreateTextLayout(&utf16, &format, w.max(1.0), h.max(1.0))?
        };
        if self.layouts.len() > 256 {
            self.layouts.clear();
        }
        self.layouts.insert(key, layout.clone());
        Ok(layout)
    }

    // ----- the executor ---------------------------------------------------------------------

    /// Draw the list. The caller has already called `BeginDraw` and cleared the target.
    pub fn draw(&mut self, gpu: &GpuStack, list: &DrawList, images: &mut ImageStore) -> Result<()> {
        self.xform.clear();
        self.xform.push(Mat::IDENTITY);
        self.alpha.clear();
        self.alpha.push(1.0);
        self.clips.clear();
        let dc = &gpu.dc;
        unsafe { dc.SetTransform(&Mat::IDENTITY.to_d2d()) };

        for cmd in &list.cmds {
            let alpha = self.cur_alpha();
            match cmd {
                DrawCmd::Notch {
                    shape,
                    origin,
                    fill,
                    outline,
                } => {
                    let path = shape.to_path().translated(*origin);
                    if path.cmds.is_empty() {
                        continue;
                    }
                    let geom = self.geometry(gpu, &path)?;
                    unsafe {
                        dc.FillGeometry(&geom, self.set_brush(*fill, alpha), None::<&ID2D1Brush>);
                        if let Some((w, c)) = outline {
                            dc.DrawGeometry(
                                &geom,
                                self.set_brush(*c, alpha),
                                *w,
                                None::<&ID2D1StrokeStyle>,
                            );
                        }
                    }
                }
                DrawCmd::RoundRect {
                    rect,
                    radius,
                    color,
                } => unsafe {
                    let r = (*radius).min(rect.w * 0.5).min(rect.h * 0.5).max(0.0);
                    dc.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: rect_f(*rect),
                            radiusX: r,
                            radiusY: r,
                        },
                        self.set_brush(*color, alpha),
                    );
                },
                DrawCmd::Squircle { rect, radii, color } => {
                    let geom = self.geometry(gpu, &rounded_rect_path(*rect, *radii, 0.6))?;
                    unsafe {
                        dc.FillGeometry(&geom, self.set_brush(*color, alpha), None::<&ID2D1Brush>)
                    };
                }
                DrawCmd::StrokeRoundRect {
                    rect,
                    radius,
                    width,
                    color,
                } => unsafe {
                    let r = (*radius).min(rect.w * 0.5).min(rect.h * 0.5).max(0.0);
                    let inset = rect.inset(width * 0.5);
                    dc.DrawRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: rect_f(inset),
                            radiusX: r,
                            radiusY: r,
                        },
                        self.set_brush(*color, alpha),
                        *width,
                        None::<&ID2D1StrokeStyle>,
                    );
                },
                DrawCmd::Circle {
                    center,
                    radius,
                    color,
                } => unsafe {
                    dc.FillEllipse(
                        &D2D1_ELLIPSE {
                            point: v2(*center),
                            radiusX: *radius,
                            radiusY: *radius,
                        },
                        self.set_brush(*color, alpha),
                    );
                },
                DrawCmd::Ring {
                    center,
                    radius,
                    width,
                    start_deg,
                    sweep_deg,
                    color,
                } => {
                    let arc = icons::arc_path(center.x, center.y, *radius, *start_deg, *sweep_deg);
                    let geom = self.geometry(gpu, &arc)?;
                    unsafe {
                        dc.DrawGeometry(
                            &geom,
                            self.set_brush(*color, alpha),
                            *width,
                            &self.round_stroke,
                        )
                    };
                }
                DrawCmd::Line { a, b, width, color } => unsafe {
                    dc.DrawLine(
                        v2(*a),
                        v2(*b),
                        self.set_brush(*color, alpha),
                        *width,
                        &self.round_stroke,
                    );
                },
                DrawCmd::Text {
                    rect,
                    text,
                    style,
                    color,
                } => {
                    if text.as_str().is_empty() || rect.w < 1.0 || rect.h < 1.0 {
                        continue;
                    }
                    let layout = self.text_layout(gpu, text.as_str(), style, rect.w, rect.h)?;
                    unsafe {
                        dc.DrawTextLayout(
                            v2(Vec2::new(rect.x, rect.y)),
                            &layout,
                            self.set_brush(*color, alpha),
                            D2D1_DRAW_TEXT_OPTIONS_NO_SNAP | D2D1_DRAW_TEXT_OPTIONS_CLIP,
                        );
                    }
                }
                DrawCmd::Icon { icon, rect, color } => {
                    let k = rect.w.min(rect.h) / GRID;
                    let ox = rect.x + (rect.w - GRID * k) * 0.5;
                    let oy = rect.y + (rect.h - GRID * k) * 0.5;
                    let local = Mat::scale(k)
                        .then(Mat::translate(ox, oy))
                        .then(self.cur_xform());
                    let geoms = self
                        .icon_geoms(gpu, *icon)?
                        .iter()
                        .map(|g| (g.geom.clone(), g.stroke))
                        .collect::<Vec<_>>();
                    unsafe {
                        dc.SetTransform(&local.to_d2d());
                        for (geom, stroke) in geoms {
                            match stroke {
                                None => dc.FillGeometry(
                                    &geom,
                                    self.set_brush(*color, alpha),
                                    None::<&ID2D1Brush>,
                                ),
                                Some(w) => dc.DrawGeometry(
                                    &geom,
                                    self.set_brush(*color, alpha),
                                    w,
                                    &self.round_stroke,
                                ),
                            }
                        }
                        dc.SetTransform(&self.cur_xform().to_d2d());
                    }
                }
                DrawCmd::Image {
                    id,
                    rect,
                    radius,
                    opacity,
                } => {
                    if let Some(bmp) = images.bitmap(&gpu.dc, *id) {
                        self.draw_image(gpu, &bmp, *rect, *radius, opacity * alpha)?;
                    }
                }
                DrawCmd::Gradient {
                    rect,
                    radius,
                    from,
                    to,
                    vertical,
                } => {
                    let stops = [
                        D2D1_GRADIENT_STOP {
                            position: 0.0,
                            color: d2d_color(*from, alpha),
                        },
                        D2D1_GRADIENT_STOP {
                            position: 1.0,
                            color: d2d_color(*to, alpha),
                        },
                    ];
                    unsafe {
                        let coll = dc.CreateGradientStopCollection(
                            &stops,
                            D2D1_COLOR_SPACE_SRGB,
                            D2D1_COLOR_SPACE_SRGB,
                            D2D1_BUFFER_PRECISION_8BPC_UNORM,
                            D2D1_EXTEND_MODE_CLAMP,
                            D2D1_COLOR_INTERPOLATION_MODE_PREMULTIPLIED,
                        )?;
                        let (a, b) = if *vertical {
                            (Vec2::new(rect.x, rect.y), Vec2::new(rect.x, rect.bottom()))
                        } else {
                            (Vec2::new(rect.x, rect.y), Vec2::new(rect.right(), rect.y))
                        };
                        let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                            startPoint: v2(a),
                            endPoint: v2(b),
                        };
                        let brush = dc.CreateLinearGradientBrush(&props, None, &coll)?;
                        let r = (*radius).min(rect.w * 0.5).min(rect.h * 0.5).max(0.0);
                        dc.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: rect_f(*rect),
                                radiusX: r,
                                radiusY: r,
                            },
                            &brush,
                        );
                    }
                }
                DrawCmd::Glow {
                    center,
                    radius,
                    color,
                } => unsafe {
                    let stops = [
                        D2D1_GRADIENT_STOP {
                            position: 0.0,
                            color: d2d_color(*color, alpha),
                        },
                        D2D1_GRADIENT_STOP {
                            position: 1.0,
                            color: D2D1_COLOR_F {
                                r: color.r,
                                g: color.g,
                                b: color.b,
                                a: 0.0,
                            },
                        },
                    ];
                    let coll = dc.CreateGradientStopCollection(
                        &stops,
                        D2D1_COLOR_SPACE_SRGB,
                        D2D1_COLOR_SPACE_SRGB,
                        D2D1_BUFFER_PRECISION_8BPC_UNORM,
                        D2D1_EXTEND_MODE_CLAMP,
                        D2D1_COLOR_INTERPOLATION_MODE_PREMULTIPLIED,
                    )?;
                    let props = D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
                        center: v2(*center),
                        gradientOriginOffset: Vector2 { X: 0.0, Y: 0.0 },
                        radiusX: *radius,
                        radiusY: *radius,
                    };
                    let brush = dc.CreateRadialGradientBrush(&props, None, &coll)?;
                    dc.FillEllipse(
                        &D2D1_ELLIPSE {
                            point: v2(*center),
                            radiusX: *radius,
                            radiusY: *radius,
                        },
                        &brush,
                    );
                },
                DrawCmd::PushClip { rect, radius } => self.push_clip(gpu, *rect, *radius)?,
                DrawCmd::PopClip => self.pop_clip(gpu),
                DrawCmd::PushGroup {
                    alpha: a,
                    scale,
                    origin,
                } => {
                    let m = Mat::scale_about(*scale, *origin).then(self.cur_xform());
                    self.xform.push(m);
                    self.alpha.push(alpha * a);
                    unsafe { dc.SetTransform(&m.to_d2d()) };
                }
                DrawCmd::PopGroup => {
                    if self.xform.len() > 1 {
                        self.xform.pop();
                        self.alpha.pop();
                        unsafe { dc.SetTransform(&self.cur_xform().to_d2d()) };
                    }
                }
            }
        }
        // Defensive: a module that forgot a pop must not corrupt the next frame.
        while !self.clips.is_empty() {
            self.pop_clip(gpu);
        }
        unsafe { dc.SetTransform(&Mat::IDENTITY.to_d2d()) };
        Ok(())
    }

    fn push_clip(&mut self, gpu: &GpuStack, rect: Rect, radius: f32) -> Result<()> {
        let dc = &gpu.dc;
        unsafe {
            if radius <= 0.5 {
                dc.PushAxisAlignedClip(&rect_f(rect), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
                self.clips.push(false);
                return Ok(());
            }
            let r = radius.min(rect.w * 0.5).min(rect.h * 0.5);
            let rr = gpu
                .d2d_factory
                .CreateRoundedRectangleGeometry(&D2D1_ROUNDED_RECT {
                    rect: rect_f(rect),
                    radiusX: r,
                    radiusY: r,
                })?;
            let geom: ID2D1Geometry = rr.cast()?;
            // Tight bounds matter: with "infinite" bounds the layer's temporary surface covers the
            // whole render target on every push (expensive, especially on software rasterisers).
            let mut params = D2D1_LAYER_PARAMETERS1 {
                contentBounds: rect_f(rect.inflate(1.0, 1.0)),
                geometricMask: ManuallyDrop::new(Some(geom)),
                maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                maskTransform: self.cur_xform().to_d2d(),
                opacity: 1.0,
                opacityBrush: ManuallyDrop::new(None),
                layerOptions: D2D1_LAYER_OPTIONS1_NONE,
            };
            dc.PushLayer(&params, None::<&ID2D1Layer>);
            // PushLayer took its own reference; release ours.
            ManuallyDrop::drop(&mut params.geometricMask);
            self.clips.push(true);
        }
        Ok(())
    }

    fn pop_clip(&mut self, gpu: &GpuStack) {
        if let Some(was_layer) = self.clips.pop() {
            unsafe {
                if was_layer {
                    gpu.dc.PopLayer();
                } else {
                    gpu.dc.PopAxisAlignedClip();
                }
            }
        }
    }

    /// Draw `bmp` filling `rect` ("cover": scaled to fill, cropping the overflow, centred) with
    /// continuous rounded corners. A bitmap brush fills the rounded geometry directly, so no mask
    /// layer (a full offscreen pass) is needed per image.
    fn draw_image(
        &mut self,
        gpu: &GpuStack,
        bmp: &ID2D1Bitmap1,
        rect: Rect,
        radius: f32,
        opacity: f32,
    ) -> Result<()> {
        let px = unsafe { bmp.GetPixelSize() };
        let (bw, bh) = (px.width as f32, px.height as f32);
        if bw < 1.0 || bh < 1.0 || rect.w <= 0.0 || rect.h <= 0.0 {
            return Ok(());
        }
        let k = (rect.w / bw).max(rect.h / bh);
        let (dx, dy) = (
            rect.x + (rect.w - bw * k) * 0.5,
            rect.y + (rect.h - bh * k) * 0.5,
        );
        let brush = unsafe {
            gpu.dc.CreateBitmapBrush(
                bmp,
                Some(&D2D1_BITMAP_BRUSH_PROPERTIES1 {
                    extendModeX: D2D1_EXTEND_MODE_CLAMP,
                    extendModeY: D2D1_EXTEND_MODE_CLAMP,
                    interpolationMode: D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
                }),
                Some(&D2D1_BRUSH_PROPERTIES {
                    opacity: opacity.clamp(0.0, 1.0),
                    transform: Matrix3x2 {
                        M11: k,
                        M12: 0.0,
                        M21: 0.0,
                        M22: k,
                        M31: dx,
                        M32: dy,
                    },
                }),
            )?
        };
        let geom = self.geometry(gpu, &rounded_rect_path(rect, [radius; 4], 0.6))?;
        unsafe { gpu.dc.FillGeometry(&geom, &brush, None::<&ID2D1Brush>) };
        Ok(())
    }
}
