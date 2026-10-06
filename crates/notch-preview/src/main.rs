//! `notch-preview`: render notch-core display lists to PNG on any OS (dev tool, never shipped).
//!
//! ```text
//! notch-preview shell  out.png     # contact sheet of the shell: expand, switch page, collapse
//! notch-preview shapes out.png     # silhouettes at several sizes (corner/ear inspection)
//! ```

mod render;

use notch_core::compose::{self, Content, Metrics};
use notch_core::demo;
use notch_core::draw::{Canvas, DrawList};
use notch_core::geom::{Rect, Vec2};
use notch_core::path::NotchShape;
use notch_core::shell::{Shell, ShellConfig, Trigger};
use notch_core::theme::Theme;
use render::{Fonts, Images, Renderer};

struct Demo;
impl Content for Demo {
    fn draw_page(&mut self, page: usize, cv: &mut Canvas, area: Rect) {
        demo::draw_page(page, cv, area);
    }
    fn draw_peek(&mut self, _: u32, _: &mut Canvas, _: Rect) {}
    fn draw_chips(&mut self, _: &mut Canvas, _: Rect) {}
    fn page_count(&self) -> usize {
        demo::PAGE_COUNT
    }
}

const SCALE: f32 = 2.0;
const CELL_W: f32 = 440.0;
const CELL_H: f32 = 230.0;

fn save(pix: &tiny_skia::Pixmap, path: &str) {
    pix.save_png(path).expect("write png");
    println!("wrote {path} ({}x{})", pix.width(), pix.height());
}

fn blit(dst: &mut tiny_skia::Pixmap, src: &tiny_skia::Pixmap, x: i32, y: i32) {
    dst.draw_pixmap(
        x,
        y,
        src.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        tiny_skia::Transform::identity(),
        None,
    );
}

fn shell_sheet(out: &str, theme: Theme) {
    let fonts = Fonts::load();
    let images = Images::default();
    let mut shell = Shell::new(ShellConfig::default());
    shell.set_pages(demo::page_sizes());
    let pages = demo::page_sizes();

    let dt = 1.0 / 60.0;

    let mut cells: Vec<(String, notch_core::shell::ShellFrame)> = Vec::new();
    let mut t = 0.0;
    cells.push(("idle".into(), shell.frame()));
    shell.expand(t, Trigger::Hover);
    let marks = [50.0, 100.0, 150.0, 220.0, 400.0];
    let mut mi = 0;
    let t_start = t;
    while mi < marks.len() {
        t += dt;
        shell.step(t);
        if (t - t_start) * 1000.0 >= marks[mi] {
            cells.push((format!("expand +{:.0}ms", marks[mi]), shell.frame()));
            mi += 1;
        }
    }
    // Page switch.
    let t_sw = t;
    shell.set_page(t, 1);
    let marks = [40.0, 90.0, 300.0];
    mi = 0;
    while mi < marks.len() {
        t += dt;
        shell.step(t);
        if (t - t_sw) * 1000.0 >= marks[mi] {
            cells.push((format!("page 2 +{:.0}ms", marks[mi]), shell.frame()));
            mi += 1;
        }
    }
    // Collapse.
    let t_co = t;
    shell.collapse(t);
    let marks = [30.0, 70.0, 120.0, 300.0];
    mi = 0;
    while mi < marks.len() {
        t += dt;
        shell.step(t);
        if (t - t_co) * 1000.0 >= marks[mi] {
            cells.push((format!("collapse +{:.0}ms", marks[mi]), shell.frame()));
            mi += 1;
        }
    }

    let cols = 4;
    let rows = cells.len().div_ceil(cols);
    let mut sheet = tiny_skia::Pixmap::new(
        (CELL_W * SCALE) as u32 * cols as u32,
        (CELL_H * SCALE) as u32 * rows as u32,
    )
    .unwrap();
    sheet.fill(tiny_skia::Color::from_rgba8(30, 30, 34, 255));
    for (i, (label, frame)) in cells.iter().enumerate() {
        let mut cell = Renderer::new(CELL_W, CELL_H, SCALE, &fonts, &images);
        render::fake_desktop(&mut cell.pix, SCALE);
        let mut list = DrawList::new();
        compose::compose(
            frame,
            &pages,
            &theme,
            CELL_W,
            &Metrics::default(),
            &mut list,
            &mut Demo,
        );
        cell.draw_list(&list, Vec2::ZERO, 1.0);
        // label
        let mut lab = DrawList::new();
        {
            let mut cv = Canvas::new(&mut lab, &theme);
            cv.text(
                Rect::new(8.0, CELL_H - 20.0, 260.0, 16.0),
                label.clone(),
                notch_core::draw::TextStyle::caption(),
                notch_core::color::Color::rgb8(20, 20, 20),
            );
        }
        cell.draw_list(&lab, Vec2::ZERO, 1.0);
        blit(
            &mut sheet,
            &cell.pix,
            ((i % cols) as f32 * CELL_W * SCALE) as i32,
            ((i / cols) as f32 * CELL_H * SCALE) as i32,
        );
    }
    save(&sheet, out);
}

fn shapes_sheet(out: &str) {
    let fonts = Fonts::load();
    let images = Images::default();
    let theme = Theme::dark(notch_core::theme::FALLBACK_ACCENT);
    let sizes = [(112.0, 6.0), (190.0, 30.0), (340.0, 130.0), (380.0, 190.0)];
    let mut r = Renderer::new(420.0, 4.0 * 130.0 + 20.0, 3.0, &fonts, &images);
    render::fake_desktop(&mut r.pix, 3.0);
    let mut y = 6.0;
    for (w, h) in sizes {
        let shape = NotchShape {
            w,
            h,
            radius_top: 0.0,
            radius_bottom: if h > 40.0 { 26.0 } else { 10.0 },
            ear: if h > 20.0 { 12.0 } else { 4.0 },
            smoothing: 0.6,
        };
        r.draw_shape_path(&shape.to_path(), Vec2::new((420.0 - w) * 0.5, y), theme.bg);
        r.stroke_shape_path(
            &shape.to_path(),
            Vec2::new((420.0 - w) * 0.5, y),
            theme.hairline,
            1.0,
        );
        y += h.max(14.0) + 10.0;
    }
    // Plain circular corners next to continuous ones, for comparison.
    save(&r.pix, out);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("shell");
    let out = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| format!("preview-out/{cmd}.png"));
    if let Some(dir) = std::path::Path::new(&out).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match cmd {
        "shell" => shell_sheet(&out, Theme::dark(notch_core::theme::FALLBACK_ACCENT)),
        "shell-light" => shell_sheet(&out, Theme::light(notch_core::theme::FALLBACK_ACCENT)),
        "shapes" => shapes_sheet(&out),
        other => {
            eprintln!("unknown command '{other}' (try: shell, shell-light, shapes)");
            std::process::exit(2);
        }
    }
}
