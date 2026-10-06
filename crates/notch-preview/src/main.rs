//! `notch-preview`: render notch-core display lists to PNG on any OS (dev tool, never shipped).
//!
//! ```text
//! notch-preview shell  out.png     # contact sheet of the shell: expand, switch page, collapse
//! notch-preview shapes out.png     # silhouettes at several sizes (corner/ear inspection)
//! notch-preview modules out.png    # the real module host: chips, every page, the peek banner
//! ```

// A developer tool that is never shipped: favour straightforward code over lint-driven restructuring.
#![allow(
    dead_code,
    clippy::too_many_arguments,
    clippy::field_reassign_with_default
)]

mod render;

use notch_core::civil::LocalTime;
use notch_core::compose::{self, Content, Metrics};
use notch_core::config::Config;
use notch_core::demo;
use notch_core::draw::{Canvas, DrawList};
use notch_core::geom::{Rect, Vec2};
use notch_core::module::{Env, ModuleHost};
use notch_core::modules;
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

    render_cells(out, &theme, &cells, &pages, &mut Demo);
}

/// Render a contact sheet of frames, drawn by `content` (the demo pages or the real module host).
fn render_cells(
    out: &str,
    theme: &Theme,
    cells: &[(String, notch_core::shell::ShellFrame)],
    pages: &[notch_core::geom::Size],
    content: &mut dyn Content,
) {
    let fonts = Fonts::load();
    let images = Images::default();
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
            pages,
            theme,
            CELL_W,
            &Metrics::default(),
            &mut list,
            content,
        );
        cell.draw_list(&list, Vec2::ZERO, 1.0);
        // label
        let mut lab = DrawList::new();
        {
            let mut cv = Canvas::new(&mut lab, theme);
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

/// The real [`ModuleHost`] with the default config: collapsed chips, each page expanding, the peek.
fn modules_sheet(out: &str, theme: Theme) {
    let mut host = ModuleHost::new(
        modules::registry(),
        std::sync::Arc::new(Config::default()),
        theme,
    );
    // A fixed moment so the output is reproducible: Tuesday 6 October 2026, 14:05:09.
    host.set_context(
        0.0,
        Env {
            local: LocalTime::new(2026, 10, 6, 14, 5, 9),
            system_24h: true,
        },
    );
    let pages = host.pages();
    let mut shell = Shell::new(ShellConfig::default());
    shell.set_pages(pages.clone());
    shell.set_chip_width(0.0, host.chips_width());
    let dt = 1.0 / 60.0;
    let mut t = 0.0;
    let run = |shell: &mut Shell, t: &mut f64, secs: f64| {
        let end = *t + secs;
        while *t < end {
            *t += dt;
            shell.step(*t);
        }
    };
    run(&mut shell, &mut t, 1.0);
    let mut cells = vec![("idle with chips".to_string(), shell.frame())];
    for page in 0..pages.len() {
        shell.set_page(t, page);
        shell.expand(t, Trigger::Hotkey);
        run(&mut shell, &mut t, 0.1);
        cells.push((format!("page {} +100ms", page + 1), shell.frame()));
        run(&mut shell, &mut t, 0.6);
        cells.push((format!("page {} settled", page + 1), shell.frame()));
        shell.collapse(t);
        run(&mut shell, &mut t, 0.6);
    }
    for id in host.module_ids() {
        if let (Some(owner), Some(size)) = (host.peek_owner(id), host.peek_size(id)) {
            shell.peek(t, owner, size, 5.0);
            run(&mut shell, &mut t, 0.6);
            cells.push((format!("peek: {id}"), shell.frame()));
            shell.collapse(t);
            run(&mut shell, &mut t, 0.6);
        }
    }
    render_cells(out, &theme, &cells, &pages, &mut host);
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
        "modules" => modules_sheet(&out, Theme::dark(notch_core::theme::FALLBACK_ACCENT)),
        "modules-light" => modules_sheet(&out, Theme::light(notch_core::theme::FALLBACK_ACCENT)),
        other => {
            eprintln!(
                "unknown command '{other}' (try: shell, shell-light, shapes, modules, modules-light)"
            );
            std::process::exit(2);
        }
    }
}
