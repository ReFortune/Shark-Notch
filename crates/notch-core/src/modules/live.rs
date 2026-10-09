//! Live activities: things that are happening right now and deserve a place in the collapsed pill.
//!
//! * **Privacy**: a chip while a program is using the microphone or the camera, with the program's
//!   name (read from Windows' own usage records by the platform; see `privacy`).
//! * **Timers**: quick countdowns (1, 5, 10 ... minutes) with a chip and a banner when they end.
//! * **Downloads**: a browser download in progress shows its size and speed (a percentage is not
//!   available: nothing tells the file system how big the finished file will be), and a banner with a
//!   *Show in folder* button when it completes. Nothing here ever opens or runs a downloaded file.
//!
//! The pill has room for the most important activity only: privacy first (a microphone you did not
//! expect to be live must never be hidden behind a timer), then a timer, then a download. The page
//! lists everything. Nothing polls: the platform pushes events, and the only wake-ups the module
//! asks for are timer ends and the minute count on the chip.

use std::sync::Arc;
use std::time::Duration;

use crate::config::{Config, LiveCfg};
use crate::downloads::{fmt_bytes, fmt_speed};
use crate::draw::{Align, Canvas, CursorKind, HitId, Text, TextStyle, Weight};
use crate::events::{
    ActiveDownload, DownloadDone, Event, EventKind, EventMask, Kind, PrivacyState,
};
use crate::geom::{Rect, Size};
use crate::icons::Icon;
use crate::input::Input;
use crate::module::{Command, ControlCmd, Cx, DrawCx, Env, Module, ModuleId, StoreCmd, Visibility};
use crate::pomodoro::{chip_minutes, format_clock, secs_to_chip_change};
use crate::theme::Theme;
use crate::timers::{Timers, label_for};

const KEY: &str = "timers";
const ROW_H: f32 = 36.0;
const MAX_ACTIVITY_ROWS: usize = 4;

const HIT_SHOW: HitId = HitId(900);
const HIT_MIC: HitId = HitId(300);

fn hit_preset(i: usize) -> HitId {
    HitId(100 + i as u32)
}
fn hit_cancel(i: usize) -> HitId {
    HitId(200 + i as u32)
}

pub fn create(cfg: &Config) -> Option<Box<dyn Module>> {
    cfg.live.enabled.then(|| {
        let mut m = Live::new(cfg.live.clone());
        m.mic_mute = cfg.control.mic_mute;
        Box::new(m) as Box<dyn Module>
    })
}

#[derive(Clone, Debug, PartialEq)]
enum Notice {
    /// One or more timers ran out (the label of the first, and how many).
    Timer {
        label: String,
        count: usize,
    },
    Download(DownloadDone),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChipKind {
    Privacy,
    Timer,
    Download,
}

/// What a row of the activity list shows.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    icon: Icon,
    warn: bool,
    title: String,
    sub: String,
    /// A microphone row: it carries the mute button.
    mic: bool,
}

pub struct Live {
    cfg: LiveCfg,
    timers: Timers,
    privacy: Arc<PrivacyState>,
    downloads: Arc<Vec<ActiveDownload>>,
    notice: Option<Notice>,
    unseen: Option<Notice>,
    hover: Option<HitId>,
    suspended: bool,
    last_saved: String,
    /// Show a mute button next to a program that is using the microphone (`[control] mic_mute`).
    mic_mute: bool,
    /// Whether the microphone is muted, as last read or just toggled (with when the toggle stops
    /// being trusted over a reading).
    mic: Option<bool>,
    mic_hold_until: f64,
}

fn preset_label(minutes: u32) -> String {
    if minutes.is_multiple_of(60) {
        format!("{}h", minutes / 60)
    } else {
        format!("{minutes}m")
    }
}

impl Live {
    pub fn new(cfg: LiveCfg) -> Live {
        Live {
            cfg,
            timers: Timers::default(),
            privacy: Arc::new(PrivacyState::default()),
            downloads: Arc::new(Vec::new()),
            notice: None,
            unseen: None,
            hover: None,
            suspended: false,
            last_saved: String::new(),
            mic_mute: true,
            mic: None,
            mic_hold_until: 0.0,
        }
    }

    fn chip_kind(&self) -> Option<ChipKind> {
        if self.cfg.privacy && (!self.privacy.mic.is_empty() || !self.privacy.camera.is_empty()) {
            Some(ChipKind::Privacy)
        } else if !self.timers.is_empty() {
            Some(ChipKind::Timer)
        } else if self.cfg.downloads && !self.downloads.is_empty() {
            Some(ChipKind::Download)
        } else {
            None
        }
    }

    /// The microphone is in use and its mute state is worth knowing (the page asks the platform).
    fn wants_mic_state(&self) -> bool {
        self.mic_mute && self.cfg.privacy && !self.privacy.mic.is_empty()
    }

    /// Mute state to draw: the toggle just made, else the last reading.
    fn mic_muted(&self) -> Option<bool> {
        self.mic
    }

    fn save(&mut self, cx: &mut Cx) {
        let Ok(json) = serde_json::to_string(&self.timers) else {
            return;
        };
        if json != self.last_saved {
            self.last_saved = json.clone();
            cx.command(Command::Store(StoreCmd::Save {
                key: KEY,
                data: json.into(),
            }));
        }
    }

    fn announce(&mut self, n: Notice, cx: &mut Cx) {
        if self.suspended {
            self.unseen = Some(n);
            return;
        }
        self.notice = Some(n);
        cx.peek(f64::from(self.cfg.peek_secs));
        if self.cfg.sound && matches!(self.notice, Some(Notice::Timer { .. })) {
            cx.command(Command::Chime);
        }
        cx.request_redraw();
    }

    /// The rows of the activity list, most important first.
    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        if self.cfg.privacy {
            for app in self.privacy.mic.iter() {
                rows.push(Row {
                    icon: Icon::Mic,
                    warn: true,
                    title: app.to_string(),
                    sub: "Using the microphone".into(),
                    mic: true,
                });
            }
            for app in self.privacy.camera.iter() {
                rows.push(Row {
                    icon: Icon::Video,
                    warn: true,
                    title: app.to_string(),
                    sub: "Using the camera".into(),
                    mic: false,
                });
            }
        }
        if self.cfg.downloads {
            for d in self.downloads.iter() {
                let sub = if d.speed_bps > 0 {
                    format!("{} · {}", fmt_bytes(d.bytes), fmt_speed(d.speed_bps as f64))
                } else {
                    fmt_bytes(d.bytes)
                };
                rows.push(Row {
                    icon: Icon::Download,
                    warn: false,
                    title: d.name.to_string(),
                    sub,
                    mic: false,
                });
            }
        }
        rows
    }

    fn draw_timers(&self, cv: &mut Canvas, area: Rect, env: &Env) {
        let th = *cv.theme;
        cv.text(
            Rect::new(area.x, area.y, area.w, 20.0),
            "Timers",
            TextStyle::title(),
            th.text,
        );
        let (bw, bh, gap) = (54.0, 26.0, 7.0);
        for (i, &m) in self.cfg.timer_presets.iter().take(6).enumerate() {
            let r = Rect::new(
                area.x + (i % 3) as f32 * (bw + gap),
                area.y + 26.0 + (i / 3) as f32 * (bh + gap),
                bw,
                bh,
            );
            let hot = self.hover == Some(hit_preset(i));
            cv.round_rect(r, 9.0, if hot { th.surface_hi } else { th.surface });
            cv.text(
                r,
                preset_label(m),
                TextStyle::label().align(Align::Center),
                th.text,
            );
            cv.hit(r, hit_preset(i), CursorKind::Hand);
        }
        let top = area.y + 26.0 + 2.0 * (bh + gap) + 6.0;
        if self.timers.is_empty() {
            cv.text(
                Rect::new(area.x, top + 8.0, area.w, 16.0),
                "Pick a length to start a timer.",
                TextStyle::caption(),
                th.text_faint,
            );
        }
        for (i, t) in self.timers.items().iter().enumerate() {
            let r = Rect::new(area.x, top + i as f32 * 32.0, area.w, 28.0);
            let hot = self.hover == Some(hit_cancel(i));
            cv.round_rect(r, 9.0, th.surface);
            cv.text(
                Rect::new(r.x + 10.0, r.y, 62.0, r.h),
                Text::from(t.label.clone()),
                TextStyle::body(),
                th.text_dim,
            );
            cv.text(
                Rect::new(r.x + 70.0, r.y, r.w - 70.0 - 30.0, r.h),
                format_clock(t.end - env.unix),
                TextStyle::new(14.0, Weight::SemiBold)
                    .align(Align::End)
                    .tabular(),
                th.accent,
            );
            let x = Rect::new(r.right() - 24.0, r.y + 4.0, 20.0, 20.0);
            cv.icon_button(
                x,
                Icon::Close,
                hit_cancel(i),
                th.text_dim,
                hot.then_some(th.surface_hi),
            );
        }
    }

    fn draw_activity(&self, cv: &mut Canvas, area: Rect) {
        let th = *cv.theme;
        cv.text(
            Rect::new(area.x, area.y, area.w, 20.0),
            "Activity",
            TextStyle::title(),
            th.text,
        );
        let rows = self.rows();
        if rows.is_empty() {
            cv.text(
                Rect::new(area.x, area.y + 40.0, area.w, 18.0),
                "All quiet",
                TextStyle::body().align(Align::Center),
                th.text_dim,
            );
            cv.text(
                Rect::new(area.x, area.y + 60.0, area.w, 30.0),
                "Microphone and camera use, and downloads, show up here.",
                TextStyle {
                    lines: 2,
                    ..TextStyle::caption().align(Align::Center)
                },
                th.text_faint,
            );
            return;
        }
        for (i, row) in rows.iter().take(MAX_ACTIVITY_ROWS).enumerate() {
            let r = Rect::new(
                area.x,
                area.y + 26.0 + i as f32 * ROW_H,
                area.w,
                ROW_H - 4.0,
            );
            let tile = Rect::new(r.x, r.y + 2.0, 28.0, 28.0);
            let mute = row
                .mic
                .then(|| self.mic_muted())
                .flatten()
                .filter(|_| self.mic_mute);
            let right = r.right() - if mute.is_some() { 34.0 } else { 0.0 };
            if let Some(muted) = mute {
                let b = Rect::new(r.right() - 30.0, r.y + 3.0, 26.0, 26.0);
                let hot = self.hover == Some(HIT_MIC);
                cv.icon_button(
                    b,
                    if muted { Icon::MicMuted } else { Icon::Mic },
                    HIT_MIC,
                    if muted { th.warn } else { th.text_dim },
                    hot.then_some(th.surface_hi),
                );
            }
            cv.squircle(tile, 8.0, th.surface_hi);
            cv.icon(
                row.icon,
                tile.inset(6.0),
                if row.warn { th.warn } else { th.accent },
            );
            cv.text(
                Rect::new(tile.right() + 8.0, r.y, right - tile.right() - 8.0, 17.0),
                Text::from(row.title.clone()),
                TextStyle::new(13.0, Weight::Medium),
                th.text,
            );
            cv.text(
                Rect::new(
                    tile.right() + 8.0,
                    r.y + 16.0,
                    right - tile.right() - 8.0,
                    15.0,
                ),
                Text::from(row.sub.clone()),
                TextStyle::caption(),
                th.text_dim,
            );
        }
        if rows.len() > MAX_ACTIVITY_ROWS {
            cv.text(
                Rect::new(area.x, area.bottom() - 14.0, area.w, 14.0),
                format!("+{} more", rows.len() - MAX_ACTIVITY_ROWS),
                TextStyle::caption().align(Align::Center),
                th.text_faint,
            );
        }
    }

    fn peek_icon_and_colors(th: &Theme, n: &Notice) -> (Icon, crate::color::Color) {
        match n {
            Notice::Timer { .. } => (Icon::Timer, th.accent),
            Notice::Download(_) => (Icon::Download, th.ok),
        }
    }
}

impl Module for Live {
    fn id(&self) -> ModuleId {
        "live"
    }

    fn title(&self) -> &'static str {
        "Live"
    }

    fn icon(&self) -> Icon {
        Icon::Timer
    }

    fn subscriptions(&self) -> EventMask {
        EventMask::of(&[
            Kind::Privacy,
            Kind::Downloads,
            Kind::DownloadDone,
            Kind::StoreLoaded,
            Kind::Control,
        ])
    }

    /// Keeps the countdowns fresh while the page is open. (Honoured only while expanded.)
    fn poll_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(1))
    }

    fn expanded_size(&self) -> Size {
        Size::new(424.0, 236.0)
    }

    fn peek_size(&self) -> Option<Size> {
        Some(Size::new(354.0, 76.0))
    }

    fn chip_width(&self) -> Option<f32> {
        self.chip_kind().map(|k| match k {
            ChipKind::Privacy => 108.0,
            ChipKind::Timer => 64.0,
            ChipKind::Download => 96.0,
        })
    }

    fn chip_priority(&self) -> i32 {
        match self.chip_kind() {
            Some(ChipKind::Privacy) => 90,
            Some(ChipKind::Timer) => 35,
            _ => 20,
        }
    }

    fn on_start(&mut self, cx: &mut Cx) {
        cx.command(Command::Store(StoreCmd::Load(KEY)));
    }

    fn next_wake(&self, now: f64, env: &Env) -> Option<f64> {
        let t = self.timers.soonest()?;
        let rem = t.end - env.unix;
        if rem <= 0 {
            return Some(now);
        }
        Some(now + secs_to_chip_change(rem).min(rem) as f64)
    }

    fn on_tick(&mut self, cx: &mut Cx) {
        let done = self.timers.take_finished(cx.env.unix);
        if let Some(first) = done.first() {
            self.announce(
                Notice::Timer {
                    label: first.label.clone(),
                    count: done.len(),
                },
                cx,
            );
            self.save(cx);
        }
        cx.request_redraw();
    }

    fn on_event(&mut self, ev: &Event, cx: &mut Cx) {
        match &ev.kind {
            EventKind::Privacy(p) => {
                self.privacy = p.clone();
                cx.request_redraw();
            }
            EventKind::Control(s) => {
                // A reading in flight must not undo a toggle made a moment ago.
                if cx.now >= self.mic_hold_until && self.mic != s.mic_muted {
                    self.mic = s.mic_muted;
                    cx.request_redraw();
                }
            }
            EventKind::Downloads(d) => {
                self.downloads = d.clone();
                cx.request_redraw();
            }
            EventKind::DownloadDone(done) if self.cfg.downloads => {
                self.announce(Notice::Download(done.clone()), cx);
            }
            EventKind::StoreLoaded(item) if &*item.key == KEY => {
                if let Some(data) = &item.data
                    && let Ok(t) = serde_json::from_str::<Timers>(data)
                {
                    let now = cx.env.unix;
                    let mut t = t.sanitized(now);
                    // What ended while the app was closed is old news.
                    let _ = t.take_finished(now);
                    self.timers = t;
                    self.last_saved = data.to_string();
                    cx.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn on_config(&mut self, cfg: &Config, cx: &mut Cx) {
        self.cfg = cfg.live.clone();
        self.mic_mute = cfg.control.mic_mute;
        cx.request_redraw();
    }

    fn on_visibility(&mut self, v: Visibility, cx: &mut Cx) {
        if v == Visibility::Expanded && self.wants_mic_state() {
            cx.command(Command::Control(ControlCmd::Refresh));
        }
        if v != Visibility::Expanded {
            self.hover = None;
        }
    }

    fn on_suspend(&mut self, _cx: &mut Cx) {
        self.suspended = true;
        self.hover = None;
    }

    fn on_resume(&mut self, cx: &mut Cx) {
        self.suspended = false;
        if let Some(n) = self.unseen.take() {
            self.announce(n, cx);
        }
    }

    fn on_poll(&mut self, cx: &mut Cx) {
        if self.wants_mic_state() {
            cx.command(Command::Control(ControlCmd::Refresh));
        }
        cx.request_redraw();
    }

    fn on_input(&mut self, hit: Option<HitId>, input: &Input, cx: &mut Cx) -> bool {
        match input {
            Input::Move(_) => {
                if hit != self.hover {
                    self.hover = hit;
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
            Input::Click(_) => {
                let Some(h) = hit else { return false };
                match h.0 {
                    100..=105 => {
                        let i = (h.0 - 100) as usize;
                        if let Some(&m) = self.cfg.timer_presets.get(i) {
                            self.timers.add(m, cx.env.unix);
                            self.save(cx);
                        }
                    }
                    300 => {
                        if let Some(m) = self.mic_muted() {
                            self.mic = Some(!m);
                            self.mic_hold_until = cx.now + 1.2;
                            cx.command(Command::Control(ControlCmd::ToggleMicMute));
                        }
                    }
                    200..=202 => {
                        let i = (h.0 - 200) as usize;
                        if let Some(id) = self.timers.items().get(i).map(|t| t.id) {
                            self.timers.cancel(id);
                            self.save(cx);
                        }
                    }
                    _ => return false,
                }
                cx.request_redraw();
                true
            }
            _ => false,
        }
    }

    fn on_peek_input(&mut self, hit: Option<HitId>, input: &Input, cx: &mut Cx) -> bool {
        if matches!(input, Input::Click(_))
            && hit == Some(HIT_SHOW)
            && let Some(Notice::Download(d)) = &self.notice
        {
            cx.command(Command::Reveal(d.path.clone()));
            return true;
        }
        false
    }

    fn draw_chip(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let th = *cv.theme;
        let Some(kind) = self.chip_kind() else { return };
        let icon_rect = |x: f32| Rect::new(x, area.center().y - 7.0, 14.0, 14.0);
        match kind {
            ChipKind::Privacy => {
                let mut x = area.x;
                if !self.privacy.mic.is_empty() {
                    cv.icon(Icon::Mic, icon_rect(x), th.warn);
                    x += 16.0;
                }
                if !self.privacy.camera.is_empty() {
                    cv.icon(Icon::Video, icon_rect(x), th.warn);
                    x += 16.0;
                }
                let name = self
                    .privacy
                    .mic
                    .first()
                    .or(self.privacy.camera.first())
                    .cloned()
                    .unwrap_or_else(|| "".into());
                cv.text(
                    Rect::new(x + 2.0, area.y, (area.right() - x - 2.0).max(0.0), area.h),
                    Text::from(name),
                    TextStyle::new(11.5, Weight::SemiBold),
                    th.text,
                );
            }
            ChipKind::Timer => {
                let rem = self
                    .timers
                    .soonest()
                    .map_or(0, |t| (t.end - dx.env.unix).max(0));
                cv.icon(Icon::Timer, icon_rect(area.x), th.accent);
                cv.text(
                    Rect::new(area.x + 18.0, area.y, (area.w - 18.0).max(0.0), area.h),
                    format!("{}m", chip_minutes(rem)),
                    TextStyle::new(12.0, Weight::SemiBold).tabular(),
                    th.text,
                );
            }
            ChipKind::Download => {
                let (bytes, speed) = self
                    .downloads
                    .first()
                    .map_or((0, 0), |d| (d.bytes, d.speed_bps));
                cv.icon(Icon::Download, icon_rect(area.x), th.accent);
                cv.text(
                    Rect::new(area.x + 18.0, area.y, (area.w - 18.0).max(0.0), area.h),
                    if speed > 0 {
                        fmt_speed(speed as f64)
                    } else {
                        fmt_bytes(bytes)
                    },
                    TextStyle::new(11.5, Weight::SemiBold).tabular(),
                    th.text,
                );
            }
        }
    }

    fn draw_peek(&mut self, cv: &mut Canvas, area: Rect, _dx: &DrawCx) {
        let th = *cv.theme;
        let Some(n) = self.notice.clone() else { return };
        let (icon, color) = Self::peek_icon_and_colors(&th, &n);
        let tile = Rect::new(area.x, area.y, area.h, area.h);
        cv.squircle(tile, 8.0, th.surface_hi);
        cv.icon(icon, tile.inset(tile.w * 0.22), color);
        let show = matches!(n, Notice::Download(_));
        let btn_w = 58.0;
        let x = tile.right() + 12.0;
        let w = (area.right() - x - if show { btn_w + 8.0 } else { 0.0 }).max(0.0);
        let line = |y: f32, h: f32| Rect::new(x, area.y + y, w, h);
        let (cap, title, sub): (&str, Text, String) = match &n {
            Notice::Timer { label, count } => (
                "Timer",
                Text::Static(if *count > 1 {
                    "Timers finished"
                } else {
                    "Timer finished"
                }),
                if *count > 1 {
                    format!("{count} timers ran out")
                } else {
                    label.clone()
                },
            ),
            Notice::Download(d) => (
                "Download complete",
                Text::from(d.name.clone()),
                fmt_bytes(d.bytes),
            ),
        };
        cv.text(line(0.0, 15.0), cap, TextStyle::caption(), th.accent);
        cv.text(
            line(15.0, 18.0),
            title,
            TextStyle::new(13.0, Weight::SemiBold),
            th.text,
        );
        cv.text(line(33.0, 15.0), sub, TextStyle::caption(), th.text_dim);
        if show {
            let b = Rect::new(
                area.right() - btn_w,
                area.y + (area.h - 26.0) * 0.5,
                btn_w,
                26.0,
            );
            cv.capsule(b, th.surface_hi);
            cv.text(
                b,
                "Show",
                TextStyle::new(12.0, Weight::SemiBold).align(Align::Center),
                th.text,
            );
            cv.hit(b, HIT_SHOW, CursorKind::Hand);
        }
    }

    fn draw_expanded(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let left = Rect::new(area.x, area.y, 176.0, area.h);
        let right = Rect::new(area.x + 196.0, area.y, (area.w - 196.0).max(0.0), area.h);
        self.draw_timers(cv, left, dx.env);
        self.draw_activity(cv, right);
    }
}

/// `label_for` is re-exported for the platform's self-test.
pub fn timer_label(minutes: u32) -> String {
    label_for(minutes)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::civil::{LocalTime, unix_from_civil};
    use crate::draw::{DrawCmd, DrawList};
    use crate::module::{Out, ShellRequest};

    fn env_at(secs: i64) -> Env {
        let base = unix_from_civil(2026, 10, 6, 14, 0, 0);
        let (y, mo, d, h, mi, s) = crate::civil::civil_from_unix(base + secs);
        Env {
            local: LocalTime::new(y, mo, d, h, mi, s),
            unix: base + secs,
            system_24h: true,
            ..Env::default()
        }
    }

    macro_rules! cx {
        ($t:expr) => {
            Cx::for_test($t.now, &$t.env, &$t.theme, &$t.cfg, &mut $t.out)
        };
    }

    struct T {
        m: Live,
        theme: Theme,
        cfg: Config,
        env: Env,
        out: Out,
        now: f64,
    }

    impl T {
        fn new() -> T {
            T {
                m: Live::new(LiveCfg::default()),
                theme: Theme::default(),
                cfg: Config::default(),
                env: env_at(0),
                out: Out::default(),
                now: 100.0,
            }
        }
        fn at(&mut self, secs: i64) {
            self.now += (secs - (self.env.unix - env_at(0).unix)) as f64;
            self.env = env_at(secs);
        }
        fn send(&mut self, kind: EventKind) {
            let ev = Event::new(crate::events::Source::Local, kind);
            let mut cx = cx!(self);
            self.m.on_event(&ev, &mut cx);
        }
        fn input(&mut self, hit: Option<HitId>, i: Input) -> bool {
            let mut cx = cx!(self);
            self.m.on_input(hit, &i, &mut cx)
        }
        fn peek_input(&mut self, hit: Option<HitId>, i: Input) -> bool {
            let mut cx = cx!(self);
            self.m.on_peek_input(hit, &i, &mut cx)
        }
        fn click(&mut self, id: HitId) -> bool {
            self.input(Some(id), Input::Click(crate::geom::Vec2::ZERO))
        }
        fn tick(&mut self) {
            let mut cx = cx!(self);
            self.m.on_tick(&mut cx);
        }
        fn mic(&mut self, apps: &[&str], cams: &[&str]) {
            self.send(EventKind::Privacy(Arc::new(PrivacyState {
                mic: apps.iter().map(|a| Arc::from(*a)).collect(),
                camera: cams.iter().map(|a| Arc::from(*a)).collect(),
            })));
        }
        fn download(&mut self, name: &str, bytes: u64, speed: u64) {
            self.send(EventKind::Downloads(Arc::new(vec![ActiveDownload {
                name: name.into(),
                bytes,
                speed_bps: speed,
            }])));
        }
        fn draw(&mut self) -> DrawList {
            let mut list = DrawList::new();
            let mut cv = Canvas::new(&mut list, &self.theme);
            let dx = DrawCx {
                now: self.now,
                env: &self.env,
                config: &self.cfg,
            };
            self.m
                .draw_expanded(&mut cv, Rect::new(0.0, 0.0, 364.0, 196.0), &dx);
            list
        }
        fn chip(&mut self) -> Vec<String> {
            let mut list = DrawList::new();
            let mut cv = Canvas::new(&mut list, &self.theme);
            let dx = DrawCx {
                now: self.now,
                env: &self.env,
                config: &self.cfg,
            };
            self.m
                .draw_chip(&mut cv, Rect::new(0.0, 0.0, 108.0, 24.0), &dx);
            texts(&list)
        }
        fn peek(&mut self) -> DrawList {
            let mut list = DrawList::new();
            let mut cv = Canvas::new(&mut list, &self.theme);
            let dx = DrawCx {
                now: self.now,
                env: &self.env,
                config: &self.cfg,
            };
            self.m
                .draw_peek(&mut cv, Rect::new(0.0, 0.0, 296.0, 48.0), &dx);
            list
        }
    }

    fn texts(l: &DrawList) -> Vec<String> {
        l.cmds
            .iter()
            .filter_map(|c| match c {
                DrawCmd::Text { text, .. } => Some(text.as_str().to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_idle_module_has_no_chip_and_no_wake_up() {
        let t = T::new();
        assert_eq!(t.m.chip_width(), None);
        assert_eq!(t.m.next_wake(t.now, &t.env), None);
    }

    #[test]
    fn a_program_using_the_microphone_gets_a_chip_with_its_name() {
        let mut t = T::new();
        t.mic(&["Zoom"], &[]);
        assert_eq!(t.m.chip_width(), Some(108.0));
        assert_eq!(t.m.chip_priority(), 90, "privacy wins the pill");
        assert_eq!(t.chip(), vec!["Zoom".to_string()]);
        t.mic(&["Zoom"], &["Zoom"]);
        assert_eq!(t.chip(), vec!["Zoom".to_string()], "both icons, one name");
        t.mic(&[], &[]);
        assert_eq!(t.m.chip_width(), None);
        // Switched off in the configuration: the page still works but the pill stays quiet.
        t.m.cfg.privacy = false;
        t.mic(&["Zoom"], &[]);
        assert_eq!(t.m.chip_width(), None);
        assert!(
            t.draw()
                .cmds
                .iter()
                .all(|c| !matches!(c, DrawCmd::Text { text, .. } if text.as_str() == "Zoom"))
        );
    }

    #[test]
    fn the_pill_shows_the_most_important_activity_only() {
        let mut t = T::new();
        t.download("big.iso", 5_000_000, 2_000_000);
        assert_eq!(t.m.chip_width(), Some(96.0));
        assert_eq!(t.chip(), vec!["1.9 MB/s".to_string()]);
        t.click(hit_preset(1)); // 5 minute timer
        assert_eq!(t.m.chip_width(), Some(64.0), "a timer outranks a download");
        assert_eq!(t.chip(), vec!["5m".to_string()]);
        t.mic(&["Teams"], &[]);
        assert_eq!(t.m.chip_width(), Some(108.0), "and privacy outranks both");
        assert_eq!(t.m.chip_priority(), 90);
    }

    #[test]
    fn a_stalled_download_shows_its_size_instead_of_a_speed() {
        let mut t = T::new();
        t.download("a.zip", 3 * 1024 * 1024, 0);
        assert_eq!(t.chip(), vec!["3.0 MB".to_string()]);
    }

    #[test]
    fn timers_start_from_presets_count_down_and_can_be_cancelled() {
        let mut t = T::new();
        assert!(t.click(hit_preset(0)));
        assert!(t.click(hit_preset(2)));
        assert_eq!(t.m.timers.items().len(), 2);
        let tx = texts(&t.draw());
        assert!(
            tx.contains(&"01:00".to_string()) && tx.contains(&"10:00".to_string()),
            "{tx:?}"
        );
        assert!(tx.contains(&"1 min".to_string()) && tx.contains(&"10 min".to_string()));
        t.at(15);
        assert!(texts(&t.draw()).contains(&"00:45".to_string()));
        assert!(t.click(hit_cancel(0)));
        assert_eq!(t.m.timers.items().len(), 1);
        assert!(!t.click(HitId(555)), "unknown regions are not consumed");
        // A preset index beyond the configured list does nothing.
        t.m.cfg.timer_presets = vec![1];
        assert!(t.click(hit_preset(4)));
        assert_eq!(t.m.timers.items().len(), 1);
    }

    #[test]
    fn at_most_three_timers_run_at_once() {
        let mut t = T::new();
        for i in 0..5 {
            t.click(hit_preset(i % 3));
        }
        assert_eq!(t.m.timers.items().len(), crate::timers::MAX_TIMERS);
    }

    #[test]
    fn the_wake_up_is_the_timer_end_or_the_chip_minute() {
        let mut t = T::new();
        t.click(hit_preset(1)); // 5 min
        let wake = t.m.next_wake(t.now, &t.env).unwrap();
        assert!(
            (wake - t.now - 60.0).abs() < 1e-6,
            "the minute count turns over first"
        );
        t.at(240); // 60 s left
        let wake = t.m.next_wake(t.now, &t.env).unwrap();
        assert!(
            (wake - t.now - 60.0).abs() < 1e-6,
            "1m holds to the very end"
        );
        t.at(299);
        let wake = t.m.next_wake(t.now, &t.env).unwrap();
        assert!((wake - t.now - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_finished_timer_chimes_and_announces_once() {
        let mut t = T::new();
        t.click(hit_preset(0)); // 1 min
        t.out = Out::default();
        t.at(60);
        t.tick();
        assert!(t.m.timers.is_empty());
        assert!(t.out.commands.contains(&Command::Chime));
        assert!(matches!(
            &t.out.shell[..],
            [ShellRequest::Peek { module: "test", .. }]
        ));
        let tx = texts(&t.peek());
        assert!(
            tx.contains(&"Timer finished".to_string()) && tx.contains(&"1 min".to_string()),
            "{tx:?}"
        );
        // The save reflects the now-empty list.
        assert!(
            t.out
                .commands
                .iter()
                .any(|c| matches!(c, Command::Store(StoreCmd::Save { key: "timers", .. })))
        );
        t.out = Out::default();
        t.tick();
        assert!(t.out.shell.is_empty(), "not announced twice");
    }

    #[test]
    fn several_timers_ending_together_are_one_banner() {
        let mut t = T::new();
        t.click(hit_preset(0));
        t.click(hit_preset(0));
        t.at(60);
        t.out = Out::default();
        t.tick();
        assert_eq!(t.out.shell.len(), 1);
        assert!(texts(&t.peek()).contains(&"Timers finished".to_string()));
    }

    #[test]
    fn nothing_chimes_or_shows_while_a_game_is_in_front_and_it_is_announced_after() {
        let mut t = T::new();
        t.click(hit_preset(0));
        {
            let mut cx = cx!(t);
            t.m.on_suspend(&mut cx);
        }
        t.out = Out::default();
        t.at(60);
        t.tick();
        assert!(t.out.shell.is_empty() && !t.out.commands.contains(&Command::Chime));
        {
            let mut cx = cx!(t);
            t.m.on_resume(&mut cx);
        }
        assert_eq!(t.out.shell.len(), 1);
    }

    #[test]
    fn a_finished_download_offers_show_in_folder_and_never_opens_the_file() {
        let mut t = T::new();
        t.send(EventKind::DownloadDone(DownloadDone {
            name: "setup.exe".into(),
            path: r"C:\Users\me\Downloads\setup.exe".into(),
            bytes: 42 * 1024 * 1024,
        }));
        assert_eq!(t.out.shell.len(), 1);
        assert!(
            !t.out.commands.contains(&Command::Chime),
            "downloads do not chime"
        );
        let list = t.peek();
        let tx = texts(&list);
        assert!(
            tx.contains(&"setup.exe".to_string()) && tx.contains(&"42 MB".to_string()),
            "{tx:?}"
        );
        assert!(tx.contains(&"Show".to_string()));
        assert!(list.hits.iter().any(|h| h.id == HIT_SHOW));
        assert!(t.peek_input(Some(HIT_SHOW), Input::Click(crate::geom::Vec2::ZERO)));
        assert_eq!(
            t.out.commands.last(),
            Some(&Command::Reveal(r"C:\Users\me\Downloads\setup.exe".into())),
            "only a reveal command exists for downloads: nothing opens or runs the file"
        );
        assert!(!t.peek_input(None, Input::Click(crate::geom::Vec2::ZERO)));
    }

    #[test]
    fn download_banners_follow_the_downloads_setting() {
        let mut t = T::new();
        t.m.cfg.downloads = false;
        t.send(EventKind::DownloadDone(DownloadDone {
            name: "a".into(),
            path: "C:\\a".into(),
            bytes: 1,
        }));
        assert!(t.out.shell.is_empty());
        t.download("x.zip", 10, 5);
        assert_eq!(t.m.chip_width(), None, "and no chip either");
    }

    #[test]
    fn the_activity_list_names_programs_and_downloads() {
        let mut t = T::new();
        t.mic(&["Zoom"], &["Teams"]);
        t.download("movie.mkv", 120 * 1024 * 1024, 3 * 1024 * 1024);
        let tx = texts(&t.draw());
        for want in [
            "Zoom",
            "Using the microphone",
            "Teams",
            "Using the camera",
            "movie.mkv",
            "120 MB · 3.0 MB/s",
        ] {
            assert!(tx.contains(&want.to_string()), "{want}: {tx:?}");
        }
        let quiet = T::new().draw();
        assert!(texts(&quiet).contains(&"All quiet".to_string()));
    }

    #[test]
    fn a_long_activity_list_is_summarised() {
        let mut t = T::new();
        let names = ["a", "b", "c", "d", "e", "f"];
        t.mic(&names, &[]);
        assert!(texts(&t.draw()).contains(&"+2 more".to_string()));
    }

    #[test]
    fn timers_are_saved_restored_and_expired_ones_are_dropped_silently() {
        let mut t = T::new();
        t.click(hit_preset(2)); // 10 min
        let json = t
            .out
            .commands
            .iter()
            .rev()
            .find_map(|c| match c {
                Command::Store(StoreCmd::Save { data, .. }) => Some(data.to_string()),
                _ => None,
            })
            .unwrap();
        // Restored 2 minutes later: still running with 8 minutes left.
        let mut u = T::new();
        u.at(120);
        u.send(EventKind::StoreLoaded(crate::events::StoreItem {
            key: "timers".into(),
            data: Some(json.clone().into()),
        }));
        assert_eq!(u.m.timers.items().len(), 1);
        assert_eq!(u.m.timers.items()[0].end - u.env.unix, 480);
        // Restored after it expired: gone, and no banner.
        let mut v = T::new();
        v.at(3600);
        v.send(EventKind::StoreLoaded(crate::events::StoreItem {
            key: "timers".into(),
            data: Some(json.into()),
        }));
        assert!(v.m.timers.is_empty() && v.out.shell.is_empty());
        // Garbage and other modules' keys are ignored.
        v.send(EventKind::StoreLoaded(crate::events::StoreItem {
            key: "timers".into(),
            data: Some("nonsense".into()),
        }));
        v.send(EventKind::StoreLoaded(crate::events::StoreItem {
            key: "other".into(),
            data: Some("{}".into()),
        }));
        assert!(v.m.timers.is_empty());
    }

    #[test]
    fn it_asks_for_its_saved_timers_on_start() {
        let mut t = T::new();
        let mut cx = cx!(t);
        t.m.on_start(&mut cx);
        assert_eq!(
            t.out.commands,
            vec![Command::Store(StoreCmd::Load("timers"))]
        );
    }

    #[test]
    fn preset_labels() {
        assert_eq!(preset_label(5), "5m");
        assert_eq!(preset_label(60), "1h");
        assert_eq!(preset_label(120), "2h");
        assert_eq!(timer_label(90), "1 h 30 min");
    }

    #[test]
    fn it_works_inside_the_host_with_a_chip() {
        use crate::module::ModuleHost;
        let mut cfg = Config::default();
        cfg.modules.order = vec!["live".into()];
        let mut host = ModuleHost::new(
            vec![crate::module::Factory { id: "live", create }],
            Arc::new(cfg),
            Theme::default(),
        );
        host.set_context(10.0, env_at(0));
        host.start_new();
        assert_eq!(host.chips_width(), 0.0);
        host.dispatch(vec![Event::new(
            crate::events::Source::Local,
            EventKind::Privacy(Arc::new(PrivacyState {
                mic: vec!["Zoom".into()],
                camera: vec![],
            })),
        )]);
        assert!(host.chips_width() > 0.0);
        assert_eq!(host.pages().len(), 1);
    }

    fn control_event(t: &mut T, muted: Option<bool>) {
        t.send(EventKind::Control(Arc::new(crate::events::ControlState {
            volume: None,
            brightness: None,
            wifi: crate::events::Radio::Unavailable,
            bluetooth: crate::events::Radio::Unavailable,
            dnd: None,
            mic_muted: muted,
        })));
    }

    #[test]
    fn a_program_using_the_microphone_gets_a_mute_button() {
        let mut t = T::new();
        // Nothing to mute, or no reading yet: no button.
        assert!(!t.draw().hits.iter().any(|h| h.id == HIT_MIC));
        t.mic(&["Zoom"], &[]);
        assert!(!t.draw().hits.iter().any(|h| h.id == HIT_MIC), "no reading");
        control_event(&mut t, Some(false));
        assert!(t.draw().hits.iter().any(|h| h.id == HIT_MIC));
        assert!(t.click(HIT_MIC));
        assert_eq!(
            std::mem::take(&mut t.out.commands),
            vec![Command::Control(ControlCmd::ToggleMicMute)]
        );
        // A reading that was already in flight does not undo the click...
        control_event(&mut t, Some(false));
        assert_eq!(t.m.mic_muted(), Some(true));
        // ...but later ones are believed again.
        t.now += 2.0;
        control_event(&mut t, Some(false));
        assert_eq!(t.m.mic_muted(), Some(false));
        // The switch removes the button.
        t.m.mic_mute = false;
        assert!(!t.draw().hits.iter().any(|h| h.id == HIT_MIC));
    }

    #[test]
    fn the_mute_state_is_only_asked_for_while_the_microphone_is_in_use() {
        let mut t = T::new();
        let ask = |t: &mut T| {
            let mut cx = cx!(t);
            t.m.on_visibility(Visibility::Expanded, &mut cx);
            t.m.on_poll(&mut cx);
            let n = t
                .out
                .commands
                .iter()
                .filter(|c| **c == Command::Control(ControlCmd::Refresh))
                .count();
            t.out.commands.clear();
            n
        };
        assert_eq!(ask(&mut t), 0);
        t.mic(&["Zoom"], &[]);
        assert_eq!(ask(&mut t), 2);
        t.m.mic_mute = false;
        assert_eq!(ask(&mut t), 0);
    }
}
