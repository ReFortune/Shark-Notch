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
use notch_core::draw::{Canvas, DrawList, ImageId};
use notch_core::events::{
    CalEvent, CalendarData, ClipKind, ClipboardItem, Event, EventKind, FileEntry, MediaSnapshot,
    Notification, NotificationAccess, Source, StoreItem,
};
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
const CELL_H: f32 = 290.0;

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

    render_cells(out, &theme, &cells, &pages, &Images::default(), &mut Demo);
}

/// Render a contact sheet of frames, drawn by `content` (the demo pages or the real module host).
fn render_cells(
    out: &str,
    theme: &Theme,
    cells: &[(String, notch_core::shell::ShellFrame)],
    pages: &[notch_core::geom::Size],
    images: &Images,
    content: &mut dyn Content,
) {
    let fonts = Fonts::load();
    let cols = 4;
    let rows = cells.len().div_ceil(cols);
    let mut sheet = tiny_skia::Pixmap::new(
        (CELL_W * SCALE) as u32 * cols as u32,
        (CELL_H * SCALE) as u32 * rows as u32,
    )
    .unwrap();
    sheet.fill(tiny_skia::Color::from_rgba8(30, 30, 34, 255));
    for (i, (label, frame)) in cells.iter().enumerate() {
        let mut cell = Renderer::new(CELL_W, CELL_H, SCALE, &fonts, images);
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

/// A synthetic album cover: diagonal gradient with a ring, enough to judge cropping and corners.
fn fake_art() -> tiny_skia::Pixmap {
    let n = 160u32;
    let mut px = tiny_skia::Pixmap::new(n, n).unwrap();
    for y in 0..n {
        for x in 0..n {
            let t = (x + y) as f32 / (2.0 * n as f32);
            let (cx, cy) = (x as f32 - 80.0, y as f32 - 80.0);
            let ring = ((cx * cx + cy * cy).sqrt() - 44.0).abs() < 5.0;
            let (r, g, b) = if ring {
                (255, 244, 230)
            } else {
                (
                    (250.0 - 120.0 * t) as u8,
                    (110.0 + 20.0 * t) as u8,
                    (70.0 + 150.0 * t) as u8,
                )
            };
            px.pixels_mut()[(y * n + x) as usize] =
                tiny_skia::PremultipliedColorU8::from_rgba(r, g, b, 255).unwrap();
        }
    }
    px
}

/// The real [`ModuleHost`] with the default config and a made-up media session: chips, every page
/// expanding, the peek banners.
fn modules_sheet(out: &str, theme: Theme) {
    let mut cfg = Config::default();
    cfg.calendar.feeds = vec!["https://example.com/team.ics".into()];
    cfg.phone.listen = true;
    let mut host = ModuleHost::new(modules::registry(), std::sync::Arc::new(cfg), theme);
    // A fixed moment so the output is reproducible: Tuesday 6 October 2026, 14:05:09.
    host.set_context(
        0.0,
        Env {
            local: LocalTime::new(2026, 10, 6, 14, 5, 9),
            unix: notch_core::civil::unix_from_civil(2026, 10, 6, 14, 5, 9),
            system_24h: true,
            audio: notch_core::module::Audio::Level(0.7),
        },
    );
    let mut images = Images::default();
    images.insert(ImageId(1), fake_art());
    let song = |title: &str, playing: bool| {
        Event::new(
            Source::Local,
            EventKind::MediaChanged(std::sync::Arc::new(MediaSnapshot {
                app: "Spotify".into(),
                title: title.into(),
                artist: "M83".into(),
                album: "Hurry Up, We're Dreaming".into(),
                playing,
                position_ms: 83_000,
                duration_ms: 243_000,
                art: 1,
                accent: Some([250, 110, 70]),
                can_play_pause: true,
                can_next: true,
                can_prev: true,
                can_seek: true,
            })),
        )
    };
    host.dispatch(vec![song("Midnight City", true)]);
    let clip = |id: u64, kind: ClipKind, text: &str, thumb: u64, pinned: bool, src: Source| {
        Event::new(
            src,
            EventKind::ClipboardItem(ClipboardItem {
                id,
                kind,
                preview: text.into(),
                thumb,
                pinned,
            }),
        )
    };
    images.insert(ImageId(2), fake_art());
    host.dispatch(vec![
        clip(
            1,
            ClipKind::Link,
            "github.com/ReFortune/Shark-Notch",
            0,
            true,
            Source::Local,
        ),
        clip(
            2,
            ClipKind::Text,
            "meeting notes: ship phase 4, then the file shelf",
            0,
            false,
            Source::Local,
        ),
        clip(
            3,
            ClipKind::Image,
            "Image · 1920×1080",
            2,
            false,
            Source::Local,
        ),
        clip(
            4,
            ClipKind::Text,
            "The quick brown fox jumps over the lazy dog and keeps running far past the edge",
            0,
            false,
            Source::Local,
        ),
        clip(
            5,
            ClipKind::Link,
            "example.com/sent-from-my-phone",
            0,
            false,
            Source::Phone,
        ),
        clip(6, ClipKind::Text, "an older entry", 0, false, Source::Local),
    ]);
    let file = |id: u64, name: &str, size: u64, thumb: u64, is_dir: bool| FileEntry {
        id,
        name: name.into(),
        path: format!("C:\\Users\\me\\{name}").into(),
        size,
        thumb,
        is_dir,
    };
    images.insert(ImageId(3), fake_art());
    host.dispatch(vec![Event::new(
        Source::Local,
        EventKind::FileDropped(vec![
            file(1, "Q3-report.pdf", 2_400_000, 0, false),
            file(2, "holiday.jpg", 5_100_000, 3, false),
            file(3, "Projects", 0, 0, true),
            file(4, "budget.xlsx", 88_000, 0, false),
            file(5, "notes.md", 1_200, 0, false),
            file(6, "demo-recording.mp4", 143_000_000, 3, false),
            file(7, "backup.zip", 920_000_000, 0, false),
        ]),
    )]);
    // Notifications: some history (one with an app logo), then a trip to a fullscreen game with
    // two arrivals (the badge), then a fresh one (the banner).
    images.insert(ImageId(4), fake_art());
    let note = |id: u64, app: &str, title: &str, body: &str, icon: u64, ago: u32, fresh: bool| {
        Event::new(
            if id >= 1 << 32 {
                Source::Phone
            } else {
                Source::Local
            },
            EventKind::Notification(Notification {
                id,
                app: app.into(),
                title: title.into(),
                body: body.into(),
                icon,
                fresh,
                ago_secs: ago,
                quiet: false,
            }),
        )
    };
    host.dispatch(vec![
        Event::new(
            Source::Local,
            EventKind::NotificationAccess(NotificationAccess::Granted),
        ),
        note(
            1,
            "Outlook",
            "Weekly sync moved",
            "Now Thursday at 10:30 in Room 4",
            0,
            5400,
            false,
        ),
        note(
            2,
            "Teams",
            "Priya Shah",
            "Can you review the notch PR before standup?",
            4,
            1260,
            false,
        ),
        note(
            3,
            "Calendar",
            "Design review in 10 minutes",
            "Join the meeting from the notch",
            0,
            600,
            false,
        ),
        note(
            4,
            "Windows Security",
            "Scan finished",
            "No threats found",
            0,
            45,
            false,
        ),
    ]);
    host.dispatch(vec![Event::new(Source::Local, EventKind::Suspended(true))]);
    host.dispatch(vec![
        note(5, "Teams", "Priya Shah", "ping?", 4, 0, true),
        note(
            6,
            "Mail",
            "Receipt for your order",
            "Thanks for shopping with us",
            0,
            0,
            true,
        ),
    ]);
    host.dispatch(vec![Event::new(Source::Local, EventKind::Suspended(false))]);
    host.dispatch(vec![note(
        (1 << 32) | 7,
        "Messages",
        "Mum",
        "Don't forget dinner on Sunday!",
        0,
        0,
        true,
    )]);
    // Calendar: a day of meetings around 14:05, one starting in three minutes with a call link.
    let unix = notch_core::civil::unix_from_civil;
    let cal = |title: &str, (h, m): (u32, u32), mins: i64, join: bool, day: u32, feed: u8| {
        let start = unix(2026, 10, day, h, m, 0);
        CalEvent {
            title: title.into(),
            location: "".into(),
            start_utc: start,
            end_utc: start + mins * 60,
            start_local: start,
            end_local: start + mins * 60,
            all_day: false,
            join_url: join.then(|| "https://meet.google.com/abc-defg-hij".into()),
            feed,
        }
    };
    let mut holiday = cal("Public holiday", (0, 0), 0, false, 12, 1);
    holiday.all_day = true;
    holiday.end_local = holiday.start_local + 86_400;
    host.dispatch(vec![Event::new(
        Source::Local,
        EventKind::Calendar(std::sync::Arc::new(CalendarData {
            events: vec![
                cal("Standup", (9, 30), 15, true, 6, 0),
                cal("Design review", (14, 8), 45, true, 6, 0),
                cal("1:1 with Priya", (16, 0), 30, false, 6, 1),
                cal("Dentist", (17, 30), 60, false, 6, 2),
                cal("Sprint planning", (10, 0), 90, true, 8, 0),
                cal("Lunch with Sam", (12, 30), 60, false, 9, 1),
                holiday,
                cal("Team offsite", (9, 0), 480, false, 14, 0),
            ],
            fetched_unix: unix(2026, 10, 6, 14, 0, 0),
            feeds: 1,
            failed: 0,
            error: None,
        })),
    )]);
    // Pomodoro: a focus session in progress with a few tasks, then it ends and a break begins.
    let now = unix(2026, 10, 6, 14, 5, 9);
    let saved = format!(
        r#"{{"timer":{{"phase":"Focus","end":{},"left":1500,"cycle":2}},"todos":{{"items":[
            {{"id":1,"title":"Write the notch design notes","done":false,"sessions":2}},
            {{"id":2,"title":"Review the calendar parser","done":false,"sessions":1}},
            {{"id":3,"title":"Reply to Priya","done":false,"sessions":0}},
            {{"id":4,"title":"Book the dentist","done":true,"sessions":0}}],
            "next_id":5,"current":1}},"day":{},"sessions":3}}"#,
        now + 1112,
        notch_core::civil::days_from_civil(2026, 10, 6)
    );
    host.dispatch(vec![Event::new(
        Source::Local,
        EventKind::StoreLoaded(StoreItem {
            key: "pomodoro".into(),
            data: Some(saved.into()),
        }),
    )]);
    // Command centre: Wi-Fi on, Bluetooth off, a volume and a brightness level.
    host.dispatch(vec![Event::new(
        Source::Local,
        EventKind::Control(std::sync::Arc::new(notch_core::events::ControlState {
            volume: Some((0.62, false)),
            brightness: Some(0.7),
            wifi: notch_core::events::Radio::On,
            bluetooth: notch_core::events::Radio::Off,
            dnd: Some(false),
        })),
    )]);
    // System stats: a minute of readings (a busy stretch in the middle), on battery.
    let reading = |i: usize| {
        let x = i as f32;
        let load = 22.0 + 18.0 * (x * 0.21).sin() + if (24..40).contains(&i) { 38.0 } else { 0.0 };
        Event::new(
            Source::Local,
            EventKind::Stats(std::sync::Arc::new(notch_core::events::StatsSnapshot {
                cpu: Some(load.clamp(0.0, 100.0)),
                mem_used: (9.4 + 0.02 * x as f64 * 1.0e0) as u64 * 1024 * 1024 * 1024
                    + (x as u64) * 12_000_000,
                mem_total: 16 * 1024 * 1024 * 1024,
                gpu: Some(
                    (8.0 + 10.0 * (x * 0.33).cos().abs()
                        + if (30..38).contains(&i) { 40.0 } else { 0.0 })
                    .clamp(0.0, 100.0),
                ),
                net: Some((
                    (900_000.0
                        + 700_000.0 * (x * 0.4).sin().abs()
                        + if (26..44).contains(&i) {
                            5_200_000.0
                        } else {
                            0.0
                        }) as f64,
                    (60_000.0 + 40_000.0 * (x * 0.9).cos().abs()) as f64,
                )),
                power: Some(notch_core::events::PowerStatus {
                    battery: notch_core::events::BatteryInfo {
                        percent: 82,
                        charging: false,
                    },
                    plugged: false,
                    secs_left: Some(3 * 3600 + 14 * 60),
                    saver: false,
                }),
            })),
        )
    };
    host.dispatch((0..60).map(reading).collect());
    // The iPhone link: listening on the home network, the phone's battery and Focus, what it sent.
    host.dispatch(vec![
        Event::new(
            Source::Local,
            EventKind::PhoneLink(std::sync::Arc::new(notch_core::events::PhoneLink {
                addrs: vec!["192.168.1.20:8765".into(), "172.20.80.1:8765".into()],
                port: 8765,
                error: None,
                last: Some((
                    notch_core::civil::unix_from_civil(2026, 10, 6, 14, 4, 40),
                    "clipboard".into(),
                )),
                accepted: 14,
                refused: 2,
            })),
        ),
        Event::new(
            Source::Phone,
            EventKind::Battery(notch_core::events::BatteryInfo {
                percent: 73,
                charging: true,
            }),
        ),
        Event::new(
            Source::Phone,
            EventKind::FocusChanged(notch_core::events::FocusInfo {
                name: "Work".into(),
                active: true,
            }),
        ),
    ]);
    // Live activities: a call on the microphone and camera, two downloads, two quick timers.
    let timers = format!(
        r#"{{"items":[{{"id":1,"label":"10 min","end":{},"total":600}},
            {{"id":2,"label":"30 min","end":{},"total":1800}}],"next_id":3}}"#,
        now + 420,
        now + 1500
    );
    host.dispatch(vec![
        Event::new(
            Source::Local,
            EventKind::Privacy(std::sync::Arc::new(notch_core::events::PrivacyState {
                mic: vec!["Zoom".into()],
                camera: vec!["Teams".into()],
            })),
        ),
        Event::new(
            Source::Local,
            EventKind::Downloads(std::sync::Arc::new(vec![
                notch_core::events::ActiveDownload {
                    name: "ubuntu-26.04-desktop-amd64.iso".into(),
                    bytes: 1_840_000_000,
                    speed_bps: 31_500_000,
                },
                notch_core::events::ActiveDownload {
                    name: "setup.exe".into(),
                    bytes: 31_000_000,
                    speed_bps: 0,
                },
            ])),
        ),
        Event::new(
            Source::Local,
            EventKind::StoreLoaded(StoreItem {
                key: "timers".into(),
                data: Some(timers.into()),
            }),
        ),
    ]);
    let _ = host.take_out();
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
    let mut cells = vec![("idle".to_string(), shell.frame())];
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
    // The focus session ends 20 minutes on (the break starts by itself): its banner and chip.
    host.set_context(
        1200.0,
        Env {
            local: LocalTime::new(2026, 10, 6, 14, 25, 30),
            unix: unix(2026, 10, 6, 14, 25, 30),
            system_24h: true,
            audio: notch_core::module::Audio::Idle,
        },
    );
    host.tick();
    let _ = host.take_out();
    for id in host.module_ids() {
        if let (Some(owner), Some(size)) = (host.peek_owner(id), host.peek_size(id)) {
            shell.peek(t, owner, size, 5.0);
            run(&mut shell, &mut t, 0.6);
            cells.push((format!("peek: {id}"), shell.frame()));
            shell.collapse(t);
            run(&mut shell, &mut t, 0.6);
        }
    }
    render_cells(out, &theme, &cells, &pages, &images, &mut host);

    // The live module shows one banner at a time (the latest), so the download one gets its own
    // picture: a finished download, with the button that shows the file in its folder.
    host.dispatch(vec![Event::new(
        Source::Local,
        EventKind::DownloadDone(notch_core::events::DownloadDone {
            name: "ubuntu-26.04-desktop-amd64.iso".into(),
            path: "C:\\Users\\you\\Downloads\\ubuntu-26.04-desktop-amd64.iso".into(),
            bytes: 5_900_000_000,
        }),
    )]);
    let _ = host.take_out();
    if let (Some(owner), Some(size)) = (host.peek_owner("live"), host.peek_size("live")) {
        shell.peek(t, owner, size, 5.0);
        run(&mut shell, &mut t, 0.6);
        let cells = vec![("peek: live (download)".to_string(), shell.frame())];
        let second = out.replace(".png", "-download.png");
        render_cells(&second, &theme, &cells, &pages, &images, &mut host);
    }
}

/// Every icon on the notch background, large and at real size, for visual review.
fn icons_sheet(out: &str) {
    let fonts = Fonts::load();
    let images = Images::default();
    let theme = Theme::dark(notch_core::theme::FALLBACK_ACCENT);
    let cols = 8usize;
    let rows = notch_core::icons::Icon::ALL.len().div_ceil(cols);
    let (cw, ch) = (84.0f32, 96.0f32);
    let mut r = Renderer::new(cols as f32 * cw, rows as f32 * ch, 3.0, &fonts, &images);
    r.pix.fill(tiny_skia::Color::from_rgba8(0, 0, 0, 255));
    let mut list = DrawList::new();
    {
        let mut cv = Canvas::new(&mut list, &theme);
        for (i, icon) in notch_core::icons::Icon::ALL.iter().enumerate() {
            let (x, y) = ((i % cols) as f32 * cw, (i / cols) as f32 * ch);
            cv.icon(*icon, Rect::new(x + 16.0, y + 8.0, 52.0, 52.0), theme.text);
            cv.icon(
                *icon,
                Rect::new(x + 12.0, y + 68.0, 16.0, 16.0),
                theme.text_dim,
            );
            cv.icon(
                *icon,
                Rect::new(x + 36.0, y + 66.0, 20.0, 20.0),
                theme.accent,
            );
            cv.text(
                Rect::new(x + 2.0, y + 56.0, cw - 4.0, 12.0),
                format!("{icon:?}"),
                notch_core::draw::TextStyle::new(9.0, notch_core::draw::Weight::Regular)
                    .align(notch_core::draw::Align::Center),
                theme.text_faint,
            );
        }
    }
    r.draw_list(&list, Vec2::ZERO, 1.0);
    save(&r.pix, out);
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
        "icons" => icons_sheet(&out),
        "modules" => modules_sheet(&out, Theme::dark(notch_core::theme::FALLBACK_ACCENT)),
        "modules-light" => modules_sheet(&out, Theme::light(notch_core::theme::FALLBACK_ACCENT)),
        other => {
            eprintln!(
                "unknown command '{other}' (try: shell, shell-light, shapes, icons, modules, modules-light)"
            );
            std::process::exit(2);
        }
    }
}
