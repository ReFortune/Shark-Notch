//! Notifications: Windows toasts (and, from phase 11, the iPhone's) as a short list, with a brief
//! banner for each new one that then tucks away by itself.
//!
//! * **Banner, then gone.** A fresh notification asks the shell for a peek of a few seconds; nothing
//!   stays on screen and nothing needs a click. The list is a page you open when you want it.
//! * **Never while a game is in front.** While the shell is suspended (fullscreen app, lock, pause)
//!   notifications are *kept silently* and counted; when it comes back a collapsed-pill badge and a
//!   short "while you were away" banner say how many you missed. Opening the page clears the badge.
//! * **Honest about access.** Windows only lets a process with *package identity* read other apps'
//!   notifications. The page says plainly when that is missing, and phone notifications keep working.
//! * **Non-destructive.** Dismissing here hides the entry in the notch; whether it is also removed
//!   from Windows' notification centre is the platform's decision (`notifications.dismiss_in_windows`).

use std::sync::Arc;
use std::time::Duration;

use crate::config::{Config, NotificationsCfg};
use crate::draw::{Align, Canvas, CursorKind, HitId, ImageId, TextStyle, Weight};
use crate::events::{Event, EventKind, EventMask, Kind, Notification, NotificationAccess, Source};
use crate::geom::{Rect, Size};
use crate::icons::Icon;
use crate::input::Input;
use crate::module::{Command, Cx, DrawCx, Module, ModuleId, NotifCmd, Visibility};
use crate::modules::clipboard::fmt_age;
use crate::spring::{Spring, SpringParams};

pub const ROW_H: f32 = 48.0;
pub const HEADER_H: f32 = 28.0;
pub const VISIBLE_ROWS: usize = 4;
const BTN: f32 = 22.0;
/// Width of the "missed" badge in the collapsed pill.
const CHIP_W: f32 = 44.0;
/// How long the "while you were away" banner stays.
const SUMMARY_SECS: f64 = 3.2;

const HIT_CLEAR: HitId = HitId(1);
const HIT_SETTINGS: HitId = HitId(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Row(usize),
    Dismiss(usize),
}

fn hit(p: Part) -> HitId {
    HitId(match p {
        Part::Row(i) => 100 + i as u32,
        Part::Dismiss(i) => 1_000 + i as u32,
    })
}

fn part_of(h: HitId) -> Option<Part> {
    match h.0 {
        100..=999 => Some(Part::Row(h.0 as usize - 100)),
        1_000..=1_999 => Some(Part::Dismiss(h.0 as usize - 1_000)),
        _ => None,
    }
}

pub fn create(cfg: &Config) -> Option<Box<dyn Module>> {
    cfg.notifications.enabled.then(|| {
        Box::new(Notifications::new(
            cfg.notifications.clone(),
            cfg.fullscreen.show_missed_indicator,
        )) as Box<dyn Module>
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: u64,
    pub app: Arc<str>,
    pub title: Arc<str>,
    pub body: Arc<str>,
    pub icon: u64,
    pub source: Source,
    pub at: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeekMode {
    /// The newest notification.
    Latest,
    /// "N notifications while you were away".
    Missed,
}

pub struct Notifications {
    cfg: NotificationsCfg,
    show_missed: bool,
    /// Newest first.
    rows: Vec<Row>,
    access: NotificationAccess,
    suspended: bool,
    /// Notifications that arrived while the shell was suspended and have not been looked at.
    missed: u32,
    peek_mode: PeekMode,
    hover: Option<HitId>,
    scroll: Spring,
    last_frame: f64,
    list: Rect,
}

impl Notifications {
    pub fn new(cfg: NotificationsCfg, show_missed: bool) -> Notifications {
        Notifications {
            cfg,
            show_missed,
            rows: Vec::new(),
            access: NotificationAccess::Unknown,
            suspended: false,
            missed: 0,
            peek_mode: PeekMode::Latest,
            hover: None,
            scroll: Spring::new(0.0, SpringParams::new(24.0, 1.0)),
            last_frame: 0.0,
            list: Rect::default(),
        }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn missed(&self) -> u32 {
        self.missed
    }

    fn ignored(&self, app: &str) -> bool {
        self.cfg
            .ignore_apps
            .iter()
            .any(|a| a.trim().eq_ignore_ascii_case(app.trim()))
    }

    fn max_scroll(&self) -> f32 {
        self.rows.len().saturating_sub(VISIBLE_ROWS) as f32
    }

    fn clamp_scroll(&mut self) {
        let t = self.scroll.target().min(self.max_scroll());
        self.scroll.set_target(t);
    }

    fn apply(&mut self, n: &Notification, source: Source, now: f64) {
        self.rows.retain(|r| r.id != n.id);
        let row = Row {
            id: n.id,
            app: n.app.clone(),
            title: n.title.clone(),
            body: n.body.clone(),
            icon: n.icon,
            source,
            at: now - f64::from(n.ago_secs),
        };
        // Newest first, even when a backlog arrives oldest-first; equal ages go in front.
        let at = self
            .rows
            .iter()
            .position(|r| r.at <= row.at)
            .unwrap_or(self.rows.len());
        self.rows.insert(at, row);
        self.rows.truncate(self.cfg.max_items as usize);
        if at == 0 {
            self.scroll.set_target(0.0);
        }
    }

    fn draw_tile(&self, cv: &mut Canvas, tile: Rect, icon: u64) {
        let th = *cv.theme;
        // App logos are usually transparent glyphs: give them a plate so they read on black.
        cv.squircle(tile, 8.0, th.surface_hi);
        if icon != 0 {
            cv.image(ImageId(icon), tile.inset(tile.w * 0.1), 6.0);
        } else {
            cv.icon(Icon::Bell, tile.inset(tile.w * 0.22), th.text_dim);
        }
    }

    fn draw_row(&self, cv: &mut Canvas, r: Rect, row: &Row, vi: usize, now: f64, visible: Rect) {
        let th = *cv.theme;
        let hovered = matches!(
            self.hover.and_then(part_of),
            Some(Part::Row(i) | Part::Dismiss(i)) if i == vi
        );
        if hovered {
            cv.round_rect(r, 10.0, th.surface);
        }
        let tile = Rect::new(r.x + 6.0, r.y + (r.h - 30.0) * 0.5, 30.0, 30.0);
        self.draw_tile(cv, tile, row.icon);

        let mut right = r.right() - 8.0;
        if hovered {
            let b = Rect::new(right - BTN, r.y + 4.0, BTN, BTN);
            if self.hover == Some(hit(Part::Dismiss(vi))) {
                cv.capsule(b, th.surface_hi);
            }
            cv.icon(Icon::Close, b.inset(5.0), th.text_dim);
            if let Some(h) = b.intersect(&visible) {
                cv.hit(h, hit(Part::Dismiss(vi)), CursorKind::Hand);
            }
            right -= BTN + 4.0;
        }
        let x = tile.right() + 10.0;
        let age = fmt_age(now - row.at);
        let age_w = 46.0;
        let w = (right - x).max(0.0);
        // Title line, with the age right-aligned on it.
        cv.text(
            Rect::new(x, r.y + 5.0, (w - age_w).max(0.0), 18.0),
            if row.title.is_empty() {
                &row.app
            } else {
                &row.title
            },
            TextStyle::new(13.0, Weight::SemiBold),
            th.text,
        );
        if !hovered {
            cv.text(
                Rect::new(right - age_w, r.y + 6.0, age_w, 14.0),
                age,
                TextStyle::caption().align(Align::End),
                th.text_faint,
            );
        }
        let sub = if row.title.is_empty() {
            String::new()
        } else if row.body.is_empty() {
            row.app.to_string()
        } else {
            row.body.to_string()
        };
        if !sub.is_empty() {
            cv.text(
                Rect::new(x, r.y + 24.0, w, 16.0),
                sub,
                TextStyle::new(12.0, Weight::Regular),
                th.text_dim,
            );
        }
    }

    fn setup_card(&self, cv: &mut Canvas, area: Rect) {
        let th = *cv.theme;
        let (title, hint, button) = match self.access {
            NotificationAccess::Granted => (
                "No notifications",
                "New ones show up here for a few seconds, then tuck away.",
                false,
            ),
            NotificationAccess::Unknown => (
                "Waiting for Windows…",
                "Asking for access to notifications.",
                false,
            ),
            NotificationAccess::Denied => (
                "Notification access is off",
                "Allow Shark Notch under Settings → Privacy → Notifications.",
                true,
            ),
            NotificationAccess::NoIdentity => (
                "Windows notifications need app identity",
                "Phone notifications still appear here. See docs/NOTIFICATIONS.md.",
                false,
            ),
        };
        let c = area.centered(area.w, 96.0);
        cv.icon(
            Icon::Bell,
            Rect::new(c.center().x - 14.0, c.y, 28.0, 28.0),
            th.text_faint,
        );
        cv.text(
            Rect::new(c.x, c.y + 34.0, c.w, 20.0),
            title,
            TextStyle::body().align(Align::Center),
            th.text_dim,
        );
        cv.text(
            Rect::new(c.x, c.y + 54.0, c.w, 16.0),
            hint,
            TextStyle::caption().align(Align::Center),
            th.text_faint,
        );
        if button {
            let b = Rect::new(c.center().x - 62.0, c.y + 74.0, 124.0, 22.0);
            let hot = self.hover == Some(HIT_SETTINGS);
            cv.capsule(b, if hot { th.surface_hi } else { th.surface });
            cv.text(
                b,
                "Open settings",
                TextStyle::label().align(Align::Center),
                th.text,
            );
            cv.hit(b, HIT_SETTINGS, CursorKind::Hand);
        }
    }
}

impl Module for Notifications {
    fn id(&self) -> ModuleId {
        "notifications"
    }

    fn title(&self) -> &'static str {
        "Notifications"
    }

    fn icon(&self) -> Icon {
        Icon::Bell
    }

    fn subscriptions(&self) -> EventMask {
        EventMask::of(&[
            Kind::Notification,
            Kind::NotificationRemoved,
            Kind::NotificationAccess,
            Kind::Suspended,
        ])
    }

    /// Keeps the "5 min ago" labels fresh while the page is open. (Honoured only while expanded.)
    fn poll_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(20))
    }

    fn wants_frames(&self) -> bool {
        !self.scroll.is_settled()
    }

    fn expanded_size(&self) -> Size {
        Size::new(
            424.0,
            14.0 + HEADER_H + 4.0 + ROW_H * VISIBLE_ROWS as f32 + 26.0,
        )
    }

    fn peek_size(&self) -> Option<Size> {
        Some(Size::new(354.0, 76.0))
    }

    fn chip_width(&self) -> Option<f32> {
        (self.missed > 0 && self.show_missed).then_some(CHIP_W)
    }

    fn chip_priority(&self) -> i32 {
        50
    }

    fn on_event(&mut self, ev: &Event, cx: &mut Cx) {
        match &ev.kind {
            EventKind::Notification(n) => {
                if self.ignored(&n.app) {
                    return;
                }
                self.apply(n, ev.source, cx.now);
                if n.fresh {
                    if self.suspended {
                        self.missed += 1;
                    } else if self.cfg.peek && !n.quiet {
                        self.peek_mode = PeekMode::Latest;
                        cx.peek(f64::from(self.cfg.peek_secs));
                    }
                }
                cx.request_redraw();
            }
            EventKind::NotificationRemoved(id) => {
                let before = self.rows.len();
                self.rows.retain(|r| r.id != *id);
                if self.rows.len() != before {
                    self.clamp_scroll();
                    cx.request_redraw();
                }
            }
            EventKind::NotificationAccess(a) if self.access != *a => {
                self.access = *a;
                cx.request_redraw();
            }
            EventKind::Suspended(on) => {
                self.suspended = *on;
                if !on && self.missed > 0 && self.show_missed {
                    // Back from the game: say what happened, once, briefly.
                    self.peek_mode = PeekMode::Missed;
                    cx.peek(SUMMARY_SECS);
                    cx.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn on_config(&mut self, cfg: &Config, cx: &mut Cx) {
        self.cfg = cfg.notifications.clone();
        self.show_missed = cfg.fullscreen.show_missed_indicator;
        self.rows.truncate(self.cfg.max_items as usize);
        cx.request_redraw();
    }

    fn on_visibility(&mut self, v: Visibility, cx: &mut Cx) {
        match v {
            Visibility::Expanded => {
                if self.missed > 0 {
                    self.missed = 0; // seen: the badge goes away
                    cx.request_redraw();
                }
                if matches!(
                    self.access,
                    NotificationAccess::Unknown | NotificationAccess::Denied
                ) {
                    // The user may just have flipped the switch in Settings.
                    cx.command(Command::Notifications(NotifCmd::Recheck));
                }
            }
            _ => {
                self.hover = None;
                self.scroll.snap(0.0);
            }
        }
    }

    fn on_suspend(&mut self, _cx: &mut Cx) {
        self.hover = None;
    }

    fn on_poll(&mut self, cx: &mut Cx) {
        if !self.rows.is_empty() {
            cx.request_redraw();
        }
    }

    fn on_input(&mut self, hit_id: Option<HitId>, input: &Input, cx: &mut Cx) -> bool {
        match input {
            Input::Move(_) => {
                if hit_id != self.hover {
                    self.hover = hit_id;
                    cx.request_redraw();
                }
                false
            }
            Input::Leave => {
                if self.hover.take().is_some() {
                    cx.request_redraw();
                }
                false
            }
            Input::Wheel { pos, dy, .. } => {
                if self.max_scroll() <= 0.0 || !self.list.contains(*pos) {
                    return false;
                }
                let t = (self.scroll.target() - dy / 120.0).clamp(0.0, self.max_scroll());
                self.scroll.set_target(t);
                cx.request_redraw();
                true
            }
            Input::Click(_) => {
                let Some(h) = hit_id else { return false };
                if h == HIT_CLEAR {
                    self.rows.clear();
                    self.scroll.snap(0.0);
                    cx.command(Command::Notifications(NotifCmd::ClearAll));
                    cx.request_redraw();
                    return true;
                }
                if h == HIT_SETTINGS {
                    cx.command(Command::Notifications(NotifCmd::OpenSettings));
                    return true;
                }
                if let Some(Part::Dismiss(vi)) = part_of(h) {
                    if let Some(id) = self.rows.get(vi).map(|r| r.id) {
                        self.rows.retain(|r| r.id != id);
                        self.clamp_scroll();
                        cx.command(Command::Notifications(NotifCmd::Dismiss(id)));
                        cx.request_redraw();
                    }
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    fn draw_chip(&mut self, cv: &mut Canvas, area: Rect, _dx: &DrawCx) {
        let th = *cv.theme;
        let icon = Rect::new(area.x, area.center().y - 8.0, 16.0, 16.0);
        cv.icon(Icon::Bell, icon, th.accent);
        let n = if self.missed > 9 {
            "9+".to_string()
        } else {
            self.missed.to_string()
        };
        cv.text(
            Rect::new(
                icon.right() + 3.0,
                area.y,
                area.right() - icon.right() - 3.0,
                area.h,
            ),
            n,
            TextStyle::new(12.0, Weight::SemiBold).tabular(),
            th.text,
        );
    }

    fn draw_peek(&mut self, cv: &mut Canvas, area: Rect, _dx: &DrawCx) {
        let th = *cv.theme;
        let tile = Rect::new(area.x, area.y, area.h, area.h);
        let x = tile.right() + 12.0;
        let w = (area.right() - x).max(0.0);
        // Three lines (app, title, body) stacked from the top of the area.
        let line = |y: f32, h: f32| Rect::new(x, area.y + y, w, h);
        match self.peek_mode {
            PeekMode::Missed => {
                self.draw_tile(cv, tile, 0);
                let n = self.missed.max(1);
                cv.text(
                    line(6.0, 16.0),
                    "While you were away",
                    TextStyle::caption(),
                    th.accent,
                );
                cv.text(
                    line(22.0, 20.0),
                    if n == 1 {
                        "1 notification".to_string()
                    } else {
                        format!("{n} notifications")
                    },
                    TextStyle::new(13.5, Weight::SemiBold),
                    th.text,
                );
            }
            PeekMode::Latest => {
                let Some(row) = self.rows.first() else { return };
                self.draw_tile(cv, tile, row.icon);
                let app = if row.source == Source::Phone {
                    format!("{} · iPhone", row.app)
                } else {
                    row.app.to_string()
                };
                cv.text(line(0.0, 15.0), app, TextStyle::caption(), th.accent);
                let (main, sub) = if row.title.is_empty() {
                    (&row.body, &row.title)
                } else {
                    (&row.title, &row.body)
                };
                cv.text(
                    line(15.0, 18.0),
                    main,
                    TextStyle::new(13.0, Weight::SemiBold),
                    th.text,
                );
                if !sub.is_empty() {
                    cv.text(line(33.0, 15.0), sub, TextStyle::caption(), th.text_dim);
                }
            }
        }
    }

    fn draw_expanded(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let th = *cv.theme;
        let now = dx.now;
        let dt = (now - self.last_frame).clamp(0.0, 0.25) as f32;
        self.last_frame = now;
        if !self.scroll.is_settled() {
            self.scroll.step(dt);
        }
        let (header, rest) = area.split_top(HEADER_H);
        cv.text(
            Rect::new(header.x, header.y, 160.0, header.h),
            "Notifications",
            TextStyle::title(),
            th.text,
        );
        let mut right = header.right();
        if !self.rows.is_empty() {
            let b = Rect::new(right - 52.0, header.y + 3.0, 52.0, header.h - 6.0);
            if self.hover == Some(HIT_CLEAR) {
                cv.capsule(b, th.surface);
            }
            cv.text(
                b,
                "Clear",
                TextStyle::label().align(Align::Center),
                th.text_dim,
            );
            cv.hit(b, HIT_CLEAR, CursorKind::Hand);
            right -= 60.0;
            let caption = if self.access == NotificationAccess::Granted {
                format!("{}", self.rows.len())
            } else {
                "phone only".to_string()
            };
            cv.text(
                Rect::new(right - 90.0, header.y, 90.0, header.h),
                caption,
                TextStyle::caption().align(Align::End),
                th.text_faint,
            );
        }

        let list = Rect::new(rest.x, rest.y + 4.0, rest.w, ROW_H * VISIBLE_ROWS as f32);
        self.list = list;
        if self.rows.is_empty() {
            self.setup_card(cv, list);
            return;
        }
        let offset = self.scroll.value() * ROW_H;
        cv.push_clip(list, 0.0);
        for (vi, row) in self.rows.iter().enumerate() {
            let y = list.y + vi as f32 * ROW_H - offset;
            if y + ROW_H < list.y || y > list.bottom() {
                continue;
            }
            let r = Rect::new(list.x, y, list.w - 8.0, ROW_H - 4.0);
            let before = cv.list.hits.len();
            self.draw_row(cv, r, row, vi, now, list);
            if let Some(rh) = r.intersect(&list) {
                cv.hit(rh, hit(Part::Row(vi)), CursorKind::Arrow);
                // The dismiss button (registered first) must stay on top of the row region.
                if cv.list.hits.len() - before >= 2 {
                    let row_region = cv.list.hits.pop().unwrap();
                    cv.list.hits.insert(before, row_region);
                }
            }
        }
        cv.pop_clip();
        let max = self.max_scroll();
        if max > 0.0 {
            let track = Rect::new(list.right() - 4.0, list.y + 2.0, 3.0, list.h - 4.0);
            let thumb_h = (track.h * VISIBLE_ROWS as f32 / (VISIBLE_ROWS as f32 + max)).max(18.0);
            let y = track.y + (track.h - thumb_h) * (self.scroll.value() / max).clamp(0.0, 1.0);
            cv.capsule(Rect::new(track.x, y, track.w, thumb_h), th.surface_hi);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::{DrawCmd, DrawList};
    use crate::geom::Vec2;
    use crate::module::{Env, ModuleHost, Out};
    use crate::theme::Theme;

    fn note(id: u64, app: &str, title: &str, body: &str, fresh: bool) -> Notification {
        Notification {
            id,
            app: app.into(),
            title: title.into(),
            body: body.into(),
            icon: 0,
            fresh,
            ago_secs: 0,
            quiet: false,
        }
    }

    struct T {
        m: Notifications,
        theme: Theme,
        cfg: Config,
        env: Env,
        out: Out,
        now: f64,
    }

    impl T {
        fn new() -> T {
            T {
                m: Notifications::new(NotificationsCfg::default(), true),
                theme: Theme::default(),
                cfg: Config::default(),
                env: Env::default(),
                out: Out::default(),
                now: 100.0,
            }
        }
        fn send(&mut self, source: Source, kind: EventKind) {
            let ev = Event::new(source, kind);
            let mut cx = Cx::for_test(self.now, &self.env, &self.theme, &self.cfg, &mut self.out);
            self.m.on_event(&ev, &mut cx);
        }
        fn notify(&mut self, n: Notification) {
            self.send(Source::Local, EventKind::Notification(n));
        }
        fn input(&mut self, hit: Option<HitId>, i: Input) -> bool {
            let mut cx = Cx::for_test(self.now, &self.env, &self.theme, &self.cfg, &mut self.out);
            self.m.on_input(hit, &i, &mut cx)
        }
        fn draw(&mut self) -> DrawList {
            let mut list = DrawList::new();
            let mut cv = Canvas::new(&mut list, &self.theme);
            self.m.draw_expanded(
                &mut cv,
                Rect::new(0.0, 0.0, 340.0, 220.0),
                &DrawCx {
                    now: self.now,
                    env: &self.env,
                    config: &self.cfg,
                },
            );
            list
        }
        fn visibility(&mut self, v: Visibility) {
            let mut cx = Cx::for_test(self.now, &self.env, &self.theme, &self.cfg, &mut self.out);
            self.m.on_visibility(v, &mut cx);
        }
        fn commands(&mut self) -> Vec<Command> {
            std::mem::take(&mut self.out.commands)
        }
    }

    fn texts(l: &DrawList) -> Vec<String> {
        l.cmds
            .iter()
            .filter_map(|c| {
                if let DrawCmd::Text { text, .. } = c {
                    Some(text.as_str().to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn hit_ids_round_trip() {
        for i in [0usize, 3, 19, 99] {
            for p in [Part::Row(i), Part::Dismiss(i)] {
                assert_eq!(part_of(hit(p)), Some(p));
            }
        }
        assert_eq!(part_of(HIT_CLEAR), None);
        assert_eq!(part_of(HIT_SETTINGS), None);
    }

    #[test]
    fn a_fresh_notification_peeks_and_lands_at_the_top() {
        let mut t = T::new();
        t.notify(note(1, "Mail", "Hello", "World", true));
        t.notify(note(2, "Chat", "Ping", "", true));
        assert_eq!(
            t.m.rows().iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert_eq!(
            t.out.shell.len(),
            2,
            "each fresh notification asks for a banner"
        );
        assert_eq!(t.m.missed(), 0);
    }

    #[test]
    fn backlog_is_listed_but_never_announced() {
        let mut t = T::new();
        t.notify(note(1, "Mail", "Old news", "", false));
        assert_eq!(t.m.rows().len(), 1);
        assert!(t.out.shell.is_empty());
    }

    #[test]
    fn a_backlog_keeps_its_real_age_and_stays_ordered_newest_first() {
        let mut t = T::new();
        // Windows hands the backlog over oldest-first; the page must still read newest-first and
        // show how old each entry really is, not "just now".
        for (id, ago) in [(1u64, 7200u32), (2, 600), (3, 30)] {
            let mut n = note(id, "Mail", &format!("m{id}"), "", false);
            n.ago_secs = ago;
            t.notify(n);
        }
        assert_eq!(
            t.m.rows().iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
        assert!((t.m.rows()[2].at - (t.now - 7200.0)).abs() < 1e-6);
        let tx = texts(&t.draw());
        assert!(
            tx.iter().any(|s| s == "2 h") && tx.iter().any(|s| s == "10 min"),
            "{tx:?}"
        );
    }

    #[test]
    fn opening_the_page_without_access_asks_the_platform_to_look_again() {
        for (access, expect) in [
            (NotificationAccess::Unknown, true),
            (NotificationAccess::Denied, true),
            (NotificationAccess::Granted, false),
            // Identity cannot appear while the process runs: nothing to re-check.
            (NotificationAccess::NoIdentity, false),
        ] {
            let mut t = T::new();
            t.send(Source::Local, EventKind::NotificationAccess(access));
            t.visibility(Visibility::Expanded);
            let asked = t
                .out
                .commands
                .contains(&Command::Notifications(NotifCmd::Recheck));
            assert_eq!(asked, expect, "{access:?}");
        }
    }

    #[test]
    fn do_not_disturb_lists_silently_but_a_game_in_front_still_counts() {
        let mut t = T::new();
        let mut n = note(1, "Chat", "shh", "", true);
        n.quiet = true;
        t.notify(n.clone());
        assert_eq!(t.m.rows().len(), 1, "still in the list");
        assert!(t.out.shell.is_empty(), "no banner during do-not-disturb");
        assert_eq!(t.m.missed(), 0);
        // Away from the screen the count is information, not an interruption.
        t.send(Source::Local, EventKind::Suspended(true));
        n.id = 2;
        t.notify(n);
        assert_eq!(t.m.missed(), 1);
    }

    #[test]
    fn the_banner_can_be_switched_off() {
        let mut t = T::new();
        t.m = Notifications::new(
            NotificationsCfg {
                peek: false,
                ..Default::default()
            },
            true,
        );
        t.notify(note(1, "Mail", "Hello", "", true));
        assert!(t.out.shell.is_empty());
        assert_eq!(t.m.rows().len(), 1, "still in the list");
    }

    #[test]
    fn the_same_id_updates_in_place_instead_of_duplicating() {
        let mut t = T::new();
        t.notify(note(7, "Mail", "Draft", "v1", false));
        t.notify(note(7, "Mail", "Draft", "v2", false));
        assert_eq!(t.m.rows().len(), 1);
        assert_eq!(t.m.rows()[0].body.as_ref(), "v2");
    }

    #[test]
    fn ignored_apps_never_show() {
        let mut t = T::new();
        t.m = Notifications::new(
            NotificationsCfg {
                ignore_apps: vec!["  spotify ".into()],
                ..Default::default()
            },
            true,
        );
        t.notify(note(1, "Spotify", "Now playing", "", true));
        t.notify(note(2, "Mail", "Hi", "", true));
        assert_eq!(t.m.rows().iter().map(|r| r.id).collect::<Vec<_>>(), vec![2]);
        assert_eq!(t.out.shell.len(), 1);
    }

    #[test]
    fn the_list_is_capped() {
        let mut t = T::new();
        t.m = Notifications::new(
            NotificationsCfg {
                max_items: 3,
                ..Default::default()
            },
            true,
        );
        for i in 1..=6 {
            t.notify(note(i, "A", "t", "", false));
        }
        assert_eq!(
            t.m.rows().iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![6, 5, 4]
        );
    }

    #[test]
    fn while_suspended_notifications_are_kept_silently_and_counted() {
        let mut t = T::new();
        t.send(Source::Local, EventKind::Suspended(true));
        t.notify(note(1, "Chat", "gg", "", true));
        t.notify(note(2, "Mail", "Invoice", "", true));
        assert!(t.out.shell.is_empty(), "nothing interrupts a game");
        assert_eq!(t.m.missed(), 2);
        assert_eq!(t.m.rows().len(), 2, "but they are kept");
        assert_eq!(
            t.m.chip_width(),
            Some(CHIP_W),
            "a badge says so (the pill grows once the shell is back)"
        );
    }

    #[test]
    fn coming_back_shows_a_one_off_summary_and_opening_the_page_clears_the_badge() {
        let mut t = T::new();
        t.send(Source::Local, EventKind::Suspended(true));
        t.notify(note(1, "Chat", "a", "", true));
        t.notify(note(2, "Chat", "b", "", true));
        t.notify(note(3, "Chat", "c", "", true));
        t.send(Source::Local, EventKind::Suspended(false));
        assert_eq!(t.out.shell.len(), 1, "one summary banner");
        let mut list = DrawList::new();
        {
            let mut cv = Canvas::new(&mut list, &t.theme);
            t.m.draw_peek(
                &mut cv,
                Rect::new(0.0, 0.0, 294.0, 30.0),
                &DrawCx {
                    now: 0.0,
                    env: &t.env,
                    config: &t.cfg,
                },
            );
        }
        let tx = texts(&list);
        assert!(
            tx.contains(&"While you were away".to_string())
                && tx.contains(&"3 notifications".to_string()),
            "{tx:?}"
        );
        assert!(t.m.chip_width().is_some());
        t.visibility(Visibility::Expanded);
        assert_eq!(t.m.missed(), 0);
        assert_eq!(t.m.chip_width(), None, "seen: the badge is gone");
    }

    #[test]
    fn the_missed_indicator_follows_the_fullscreen_setting() {
        let mut t = T::new();
        t.m = Notifications::new(NotificationsCfg::default(), false);
        t.send(Source::Local, EventKind::Suspended(true));
        t.notify(note(1, "Chat", "a", "", true));
        t.send(Source::Local, EventKind::Suspended(false));
        assert!(t.out.shell.is_empty(), "indicator disabled: no summary");
        assert_eq!(t.m.chip_width(), None);
        assert_eq!(
            t.m.rows().len(),
            1,
            "the notification itself is still listed"
        );
    }

    #[test]
    fn nothing_missed_means_no_summary() {
        let mut t = T::new();
        t.send(Source::Local, EventKind::Suspended(true));
        t.send(Source::Local, EventKind::Suspended(false));
        assert!(t.out.shell.is_empty());
    }

    #[test]
    fn the_latest_banner_names_app_title_and_body() {
        let mut t = T::new();
        t.notify(note(1, "Mail", "Invoice due", "Pay by Friday", true));
        let mut list = DrawList::new();
        {
            let mut cv = Canvas::new(&mut list, &t.theme);
            t.m.draw_peek(
                &mut cv,
                Rect::new(0.0, 0.0, 294.0, 30.0),
                &DrawCx {
                    now: 0.0,
                    env: &t.env,
                    config: &t.cfg,
                },
            );
        }
        let tx = texts(&list);
        assert!(
            tx.contains(&"Mail".to_string())
                && tx.contains(&"Invoice due".to_string())
                && tx.contains(&"Pay by Friday".to_string()),
            "{tx:?}"
        );
        assert!(list.is_balanced());
    }

    #[test]
    fn phone_notifications_look_the_same_with_a_device_tag() {
        let mut t = T::new();
        t.send(
            Source::Phone,
            EventKind::Notification(note((1 << 32) | 5, "Messages", "Mum", "Call me", true)),
        );
        assert_eq!(t.m.rows()[0].source, Source::Phone);
        let mut list = DrawList::new();
        {
            let mut cv = Canvas::new(&mut list, &t.theme);
            t.m.draw_peek(
                &mut cv,
                Rect::new(0.0, 0.0, 294.0, 30.0),
                &DrawCx {
                    now: 0.0,
                    env: &t.env,
                    config: &t.cfg,
                },
            );
        }
        assert!(texts(&list).contains(&"Messages · iPhone".to_string()));
    }

    #[test]
    fn the_page_lists_rows_with_titles_bodies_and_ages() {
        let mut t = T::new();
        t.notify(note(1, "Mail", "Invoice due", "Pay by Friday", false));
        t.now += 130.0;
        let l = t.draw();
        let tx = texts(&l);
        assert!(
            tx.contains(&"Invoice due".to_string()) && tx.contains(&"Pay by Friday".to_string()),
            "{tx:?}"
        );
        assert!(tx.contains(&"2 min".to_string()), "{tx:?}");
        assert!(l.is_balanced());
    }

    #[test]
    fn dismissing_forgets_the_row_and_tells_the_platform() {
        let mut t = T::new();
        t.notify(note(1, "A", "one", "", false));
        t.notify(note(2, "B", "two", "", false));
        t.input(Some(hit(Part::Row(0))), Input::Move(Vec2::ZERO));
        let l = t.draw();
        let x = l
            .hits
            .iter()
            .find(|h| h.id == hit(Part::Dismiss(0)))
            .expect("a dismiss button on hover");
        assert_eq!(
            l.hit_test(x.rect.center()).unwrap().id,
            hit(Part::Dismiss(0)),
            "it is on top of the row"
        );
        assert!(t.input(Some(hit(Part::Dismiss(0))), Input::Click(Vec2::ZERO)));
        assert_eq!(t.m.rows().iter().map(|r| r.id).collect::<Vec<_>>(), vec![1]);
        assert_eq!(
            t.commands(),
            vec![Command::Notifications(NotifCmd::Dismiss(2))]
        );
        assert!(t.input(Some(HIT_CLEAR), Input::Click(Vec2::ZERO)));
        assert!(t.m.rows().is_empty());
        assert_eq!(
            t.commands(),
            vec![Command::Notifications(NotifCmd::ClearAll)]
        );
    }

    #[test]
    fn removal_events_from_the_platform_drop_the_row() {
        let mut t = T::new();
        t.notify(note(1, "A", "one", "", false));
        t.send(Source::Local, EventKind::NotificationRemoved(1));
        assert!(t.m.rows().is_empty());
        t.send(Source::Local, EventKind::NotificationRemoved(99));
    }

    #[test]
    fn the_empty_page_explains_what_is_missing_for_each_access_state() {
        let cases = [
            (NotificationAccess::Unknown, "Waiting for Windows"),
            (NotificationAccess::Granted, "No notifications"),
            (NotificationAccess::Denied, "Notification access is off"),
            (NotificationAccess::NoIdentity, "need app identity"),
        ];
        for (access, needle) in cases {
            let mut t = T::new();
            t.send(Source::Local, EventKind::NotificationAccess(access));
            let l = t.draw();
            assert!(
                texts(&l).iter().any(|s| s.contains(needle)),
                "{access:?}: {:?}",
                texts(&l)
            );
            assert!(l.is_balanced());
        }
    }

    #[test]
    fn only_a_denied_state_offers_the_settings_button() {
        let mut t = T::new();
        t.send(
            Source::Local,
            EventKind::NotificationAccess(NotificationAccess::Denied),
        );
        let l = t.draw();
        assert!(l.hits.iter().any(|h| h.id == HIT_SETTINGS));
        assert!(t.input(Some(HIT_SETTINGS), Input::Click(Vec2::ZERO)));
        assert_eq!(
            t.commands(),
            vec![Command::Notifications(NotifCmd::OpenSettings)]
        );
        let mut t2 = T::new();
        t2.send(
            Source::Local,
            EventKind::NotificationAccess(NotificationAccess::NoIdentity),
        );
        assert!(
            !t2.draw().hits.iter().any(|h| h.id == HIT_SETTINGS),
            "nothing a button could fix"
        );
    }

    #[test]
    fn with_items_but_no_windows_access_the_header_says_phone_only() {
        let mut t = T::new();
        t.send(
            Source::Local,
            EventKind::NotificationAccess(NotificationAccess::NoIdentity),
        );
        t.send(
            Source::Phone,
            EventKind::Notification(note(1 << 32, "Messages", "Hi", "", false)),
        );
        assert!(texts(&t.draw()).contains(&"phone only".to_string()));
    }

    #[test]
    fn scrolling_clamps_and_stops_asking_for_frames() {
        let mut t = T::new();
        for i in 1..=9 {
            t.notify(note(i, "A", "t", "", false));
        }
        t.draw();
        let inside = t.m.list.center();
        assert!(t.input(
            None,
            Input::Wheel {
                pos: inside,
                dx: 0.0,
                dy: -120.0 * 20.0
            }
        ));
        assert_eq!(t.m.scroll.target(), 5.0, "9 rows - 4 visible");
        assert!(t.m.wants_frames());
        for _ in 0..200 {
            t.now += 1.0 / 60.0;
            t.draw();
        }
        assert!(!t.m.wants_frames());
    }

    #[test]
    fn works_inside_the_host_with_a_badge_chip() {
        let mut base = Config::default();
        base.modules.order = vec!["notifications".into(), "clock".into()];
        let mut host = ModuleHost::new(
            crate::modules::registry(),
            Arc::new(base.clone()),
            Theme::default(),
        );
        assert!(host.page_ids().contains(&"notifications"));
        assert_eq!(
            host.chips_width(),
            0.0,
            "nothing missed: the pill stays tiny"
        );
        host.dispatch(vec![
            Event::new(Source::Local, EventKind::Suspended(true)),
            Event::new(
                Source::Local,
                EventKind::Notification(note(1, "Chat", "a", "", true)),
            ),
        ]);
        let out = host.take_out();
        assert!(out.shell.is_empty(), "silent while suspended");
        assert!(host.chips_width() > 0.0, "the badge widens the pill");
        let mut cfg = base;
        cfg.notifications.enabled = false;
        assert!(!cfg.module_active("notifications"));
        host.apply_config(Arc::new(cfg));
        assert!(!host.page_ids().contains(&"notifications"));
        assert_eq!(host.chips_width(), 0.0);
    }

    #[test]
    fn the_chip_draws_a_bell_and_a_capped_count() {
        let mut t = T::new();
        t.send(Source::Local, EventKind::Suspended(true));
        for i in 1..=12 {
            t.notify(note(i, "A", "t", "", true));
        }
        let mut list = DrawList::new();
        {
            let mut cv = Canvas::new(&mut list, &t.theme);
            t.m.draw_chip(
                &mut cv,
                Rect::new(0.0, 0.0, CHIP_W, 20.0),
                &DrawCx {
                    now: 0.0,
                    env: &t.env,
                    config: &t.cfg,
                },
            );
        }
        assert!(list.cmds.iter().any(|c| matches!(
            c,
            DrawCmd::Icon {
                icon: Icon::Bell,
                ..
            }
        )));
        assert!(texts(&list).contains(&"9+".to_string()));
    }
}
