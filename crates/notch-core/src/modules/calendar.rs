//! Calendar: a month grid with the day's agenda beside it, a countdown chip in the collapsed pill
//! for the next meeting, and a banner with a **Join** button shortly before it starts.
//!
//! The data comes from ICS feeds read by the platform (`CalendarData` events); this module only
//! *looks* at it. It needs the clock even while hidden (the banner has to appear on time), so it
//! asks the host for wake-ups (`next_wake`) at exactly the moments something changes: the chip
//! appears, the banner is due, the event starts, the chip's minute count turns over. Between those
//! moments nothing runs.

use std::collections::HashSet;
use std::sync::Arc;

use crate::civil::{MONTHS, WEEKDAYS_SHORT, civil_from_days, days_from_civil};
use crate::color::Color;
use crate::config::{CalendarCfg, Config};
use crate::draw::{Align, Canvas, CursorKind, HitId, Text, TextStyle, Weight};
use crate::events::{CalEvent, CalendarData, Event, EventKind, EventMask, Kind};
use crate::geom::{Rect, Size, Vec2};
use crate::icons::Icon;
use crate::input::Input;
use crate::module::{CalCmd, Command, Cx, DrawCx, Env, Module, ModuleId, Visibility};
use crate::pomodoro::secs_to_chip_change;
use crate::theme::Theme;

const DAY: i64 = 86_400;
/// How long the chip lingers ("now") after a meeting has started.
const CHIP_LINGER: i64 = 120;
/// How long the "starting now" banner may still be raised after the start.
const START_WINDOW: i64 = 60;
const CHIP_W: f32 = 64.0;
/// Data older than this is fetched again when the page opens.
const STALE_SECS: i64 = 600;

const CELL_W: f32 = 24.0;
const CELL_H: f32 = 24.0;
const GRID_W: f32 = CELL_W * 7.0;
const ROW_H: f32 = 38.0;
const AGENDA_ROWS: usize = 4;

const HIT_PREV: HitId = HitId(1);
const HIT_NEXT: HitId = HitId(2);
const HIT_TODAY: HitId = HitId(3);
const HIT_CONFIG: HitId = HitId(4);
const HIT_REFRESH: HitId = HitId(5);
const HIT_JOIN_PEEK: HitId = HitId(300);

fn hit_cell(i: usize) -> HitId {
    HitId(100 + i as u32)
}

fn cell_of(h: HitId) -> Option<usize> {
    (100..142).contains(&h.0).then(|| (h.0 - 100) as usize)
}

fn hit_join(i: usize) -> HitId {
    HitId(200 + i as u32)
}

fn join_of(h: HitId) -> Option<usize> {
    (200..210).contains(&h.0).then(|| (h.0 - 200) as usize)
}

pub fn create(cfg: &Config) -> Option<Box<dyn Module>> {
    cfg.calendar
        .enabled
        .then(|| Box::new(Calendar::new(cfg.calendar.clone())) as Box<dyn Module>)
}

// ----- pure helpers -------------------------------------------------------------------------------------

/// The 42 days (days since the epoch) shown for a month, six rows of seven.
pub fn month_grid(year: i32, month: u32, monday_first: bool) -> [i64; 42] {
    let first = days_from_civil(year, month, 1);
    let wd = crate::civil::weekday_from_days(first);
    let offset = if monday_first { wd } else { (wd + 1) % 7 };
    let start = first - i64::from(offset);
    std::array::from_fn(|i| start + i as i64)
}

/// `(year, month)` shifted by `delta` months.
pub fn shift_month(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let idx = year * 12 + month as i32 - 1 + delta;
    (idx.div_euclid(12), (idx.rem_euclid(12) + 1) as u32)
}

/// "13:05" or "1:05 PM" for a time given as seconds since midnight.
pub fn fmt_clock(secs_of_day: i64, hour24: bool) -> String {
    let s = secs_of_day.rem_euclid(DAY);
    let (h, m) = (s / 3600, s / 60 % 60);
    if hour24 {
        format!("{h:02}:{m:02}")
    } else {
        let h12 = match h % 12 {
            0 => 12,
            x => x,
        };
        format!("{h12}:{m:02} {}", if h < 12 { "AM" } else { "PM" })
    }
}

/// "in 12 min", "in 1 h 05 min", "now".
pub fn countdown(secs: i64) -> String {
    if secs <= 0 {
        return "now".into();
    }
    let mins = (secs + 59) / 60;
    match mins {
        0..=59 => format!("in {mins} min"),
        60..=1439 => {
            let (h, m) = (mins / 60, mins % 60);
            if m == 0 {
                format!("in {h} h")
            } else {
                format!("in {h} h {m:02} min")
            }
        }
        _ => format!("in {} d", mins / 1440),
    }
}

/// An event title as drawable text (a placeholder when it has none).
fn title_text(title: &Arc<str>) -> Text {
    if title.is_empty() {
        Text::Static("(no title)")
    } else {
        Text::Shared(title.clone())
    }
}

/// A small stable hash of a title, to tell two events at the same minute apart.
fn title_hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum AlertKind {
    /// "in 5 min"
    Lead,
    /// "starting now"
    Start,
}

#[derive(Clone, Debug)]
struct Alert {
    title: Arc<str>,
    start_utc: i64,
    start_local: i64,
    join: Option<Arc<str>>,
    kind: AlertKind,
}

fn feed_color(th: &Theme, feed: u8) -> Color {
    [th.accent, th.ok, th.warn, th.danger][feed as usize % 4]
}

// ----- the module -----------------------------------------------------------------------------------------

pub struct Calendar {
    cfg: CalendarCfg,
    data: Arc<CalendarData>,
    /// Month shown in the grid; `None` = the current month.
    view: Option<(i32, u32)>,
    /// Selected day (days since the epoch); `None` = today.
    selected: Option<i64>,
    hover: Option<HitId>,
    /// Banners already raised, so each event is announced once per kind.
    alerted: HashSet<(i64, u64, AlertKind)>,
    alert: Option<Alert>,
    chip_on: bool,
    suspended: bool,
    /// Monotonic time a refresh was last requested.
    last_refresh_req: f64,
}

impl Calendar {
    pub fn new(cfg: CalendarCfg) -> Calendar {
        Calendar {
            cfg,
            data: Arc::new(CalendarData::default()),
            view: None,
            selected: None,
            hover: None,
            alerted: HashSet::new(),
            alert: None,
            chip_on: false,
            suspended: false,
            last_refresh_req: f64::NEG_INFINITY,
        }
    }

    fn today(env: &Env) -> i64 {
        env.local.days()
    }

    fn selected_day(&self, env: &Env) -> i64 {
        self.selected.unwrap_or_else(|| Self::today(env))
    }

    fn monday_first(&self) -> bool {
        self.cfg.week_starts_monday
    }

    /// Indices of the events that overlap a local day.
    fn events_on(&self, day: i64) -> Vec<usize> {
        let (lo, hi) = (day * DAY, (day + 1) * DAY);
        self.data
            .events
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                if e.end_local > e.start_local {
                    e.end_local > lo && e.start_local < hi
                } else {
                    e.start_local >= lo && e.start_local < hi
                }
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Per-cell marker counts (0..=3) for the grid.
    fn marks(&self, grid: &[i64; 42]) -> [u8; 42] {
        let mut out = [0u8; 42];
        let (lo, hi) = (grid[0] * DAY, (grid[41] + 1) * DAY);
        for e in &self.data.events {
            let end = e.end_local.max(e.start_local + 1);
            if end <= lo || e.start_local >= hi {
                continue;
            }
            let first = (e.start_local.div_euclid(DAY) - grid[0]).max(0);
            let last = ((end - 1).div_euclid(DAY) - grid[0]).min(41);
            for c in first..=last {
                let c = c as usize;
                out[c] = (out[c] + 1).min(3);
            }
        }
        out
    }

    /// The event that owns the chip: the next timed event within `chip_minutes`, until a couple of
    /// minutes after it starts.
    fn chip_event(&self, unix: i64) -> Option<&CalEvent> {
        let ahead = i64::from(self.cfg.chip_minutes) * 60;
        if ahead == 0 {
            return None;
        }
        self.data
            .events
            .iter()
            .filter(|e| {
                !e.all_day && e.start_utc > unix - CHIP_LINGER && e.start_utc - unix <= ahead
            })
            .min_by_key(|e| e.start_utc)
    }

    /// The next (or current) timed event, for the agenda's highlight.
    fn next_event(&self, unix: i64) -> Option<usize> {
        self.data
            .events
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.all_day && e.end_utc.max(e.start_utc + 60) > unix)
            .min_by_key(|(_, e)| e.start_utc)
            .map(|(i, _)| i)
    }

    /// Recompute whether the chip shows; returns whether that changed.
    fn refresh_chip(&mut self, unix: i64) -> bool {
        let on = self.chip_event(unix).is_some();
        std::mem::replace(&mut self.chip_on, on) != on
    }

    /// Raise a banner for an event that is due one (not while a game is in front).
    fn raise_alerts(&mut self, cx: &mut Cx) {
        let unix = cx.env.unix;
        let lead = i64::from(self.cfg.alert_minutes) * 60;
        if lead == 0 || self.suspended {
            return;
        }
        let mut due: Option<(i64, usize, AlertKind)> = None;
        for (i, e) in self.data.events.iter().enumerate() {
            if e.all_day {
                continue;
            }
            let until = e.start_utc - unix;
            let kind = if until > 0 && until <= lead {
                AlertKind::Lead
            } else if until <= 0 && until > -START_WINDOW {
                AlertKind::Start
            } else {
                continue;
            };
            if self
                .alerted
                .contains(&(e.start_utc, title_hash(&e.title), kind))
            {
                continue;
            }
            if due.is_none_or(|(s, _, _)| e.start_utc < s) {
                due = Some((e.start_utc, i, kind));
            }
        }
        let Some((_, i, kind)) = due else { return };
        let e = &self.data.events[i];
        self.alerted
            .insert((e.start_utc, title_hash(&e.title), kind));
        // Announcing the start makes the earlier "in 5 min" banner moot.
        if kind == AlertKind::Start {
            self.alerted
                .insert((e.start_utc, title_hash(&e.title), AlertKind::Lead));
        }
        self.alert = Some(Alert {
            title: e.title.clone(),
            start_utc: e.start_utc,
            start_local: e.start_local,
            join: e.join_url.clone(),
            kind,
        });
        cx.peek(f64::from(self.cfg.peek_secs));
    }

    /// Forget bookkeeping about events that are long past.
    fn prune(&mut self, unix: i64) {
        self.alerted.retain(|(start, _, _)| *start > unix - DAY);
    }

    fn view_month(&self, env: &Env) -> (i32, u32) {
        self.view.unwrap_or((env.local.year, env.local.month))
    }

    fn view_today(&mut self) {
        self.view = None;
        self.selected = None;
    }

    fn day_label(&self, day: i64, today: i64) -> String {
        let (_, m, d) = civil_from_days(day);
        let wd = crate::civil::weekday_from_days(day) as usize;
        match day - today {
            0 => "Today".into(),
            1 => "Tomorrow".into(),
            -1 => "Yesterday".into(),
            _ => format!(
                "{} {} {}",
                WEEKDAYS_SHORT[wd],
                d,
                crate::civil::MONTHS_SHORT[m as usize - 1]
            ),
        }
    }

    // ----- drawing ---------------------------------------------------------------------------------

    fn hour24(env: &Env) -> bool {
        env.system_24h
    }

    fn draw_header(&self, cv: &mut Canvas, header: Rect, env: &Env) {
        let th = *cv.theme;
        let (y, m) = self.view_month(env);
        cv.text(
            Rect::new(header.x, header.y, 170.0, header.h),
            format!("{} {y}", MONTHS[m as usize - 1]),
            TextStyle::title(),
            th.text,
        );
        let b = 22.0;
        let top = header.y + (header.h - b) * 0.5;
        let next = Rect::new(header.right() - b, top, b, b);
        let prev = Rect::new(next.x - b - 2.0, top, b, b);
        let today = Rect::new(prev.x - 48.0, top, 44.0, b);
        for (r, icon, id) in [
            (prev, Icon::ChevronLeft, HIT_PREV),
            (next, Icon::ChevronRight, HIT_NEXT),
        ] {
            let bg = (self.hover == Some(id)).then_some(th.surface);
            cv.icon_button(r, icon, id, th.text_dim, bg);
        }
        if self.hover == Some(HIT_TODAY) {
            cv.capsule(today, th.surface);
        }
        cv.text(
            today,
            "Today",
            TextStyle::label().align(Align::Center),
            th.text_dim,
        );
        cv.hit(today, HIT_TODAY, CursorKind::Hand);
    }

    fn draw_grid(&self, cv: &mut Canvas, area: Rect, env: &Env) {
        let th = *cv.theme;
        let view = self.view_month(env);
        let grid = month_grid(view.0, view.1, self.monday_first());
        let marks = self.marks(&grid);
        let today = Self::today(env);
        let selected = self.selected_day(env);
        for c in 0..7usize {
            let wd = if self.monday_first() { c } else { (c + 6) % 7 };
            let letter = &WEEKDAYS_SHORT[wd][..1];
            cv.text(
                Rect::new(area.x + c as f32 * CELL_W, area.y, CELL_W, 14.0),
                letter,
                TextStyle::new(10.0, Weight::Medium).align(Align::Center),
                th.text_faint,
            );
        }
        let top = area.y + 16.0;
        for (i, &day) in grid.iter().enumerate() {
            let (col, row) = (i % 7, i / 7);
            let r = Rect::new(
                area.x + col as f32 * CELL_W,
                top + row as f32 * CELL_H,
                CELL_W,
                CELL_H,
            );
            let (_, m, d) = civil_from_days(day);
            let in_month = m == view.1;
            let is_today = day == today;
            let is_sel = day == selected;
            let hot = self.hover == Some(hit_cell(i));
            let disc = Rect::new(r.x + 1.5, r.y + 1.0, r.w - 3.0, r.h - 3.0);
            if is_today {
                cv.circle(disc.center(), disc.w * 0.5, th.accent);
            } else if is_sel {
                cv.circle(disc.center(), disc.w * 0.5, th.surface_hi);
            } else if hot {
                cv.circle(disc.center(), disc.w * 0.5, th.surface);
            }
            let color = if is_today {
                th.on_accent
            } else if in_month {
                th.text
            } else {
                th.text_faint
            };
            cv.text(
                Rect::new(r.x, r.y + 3.0, r.w, 14.0),
                d.to_string(),
                TextStyle::new(
                    11.5,
                    if is_today {
                        Weight::SemiBold
                    } else {
                        Weight::Regular
                    },
                )
                .align(Align::Center)
                .tabular(),
                color,
            );
            // Event dots under the number.
            let n = marks[i] as usize;
            let dot = if is_today { th.on_accent } else { th.accent };
            for k in 0..n {
                let x = r.center().x + (k as f32 - (n as f32 - 1.0) * 0.5) * 4.0;
                cv.circle(Vec2::new(x, r.bottom() - 3.4), 1.3, dot);
            }
            cv.hit(r, hit_cell(i), CursorKind::Hand);
        }
    }

    fn draw_agenda(&self, cv: &mut Canvas, area: Rect, env: &Env) {
        let th = *cv.theme;
        let today = Self::today(env);
        let day = self.selected_day(env);
        let unix = env.unix;
        let h24 = Self::hour24(env);
        let idx = self.events_on(day);
        cv.text(
            Rect::new(area.x, area.y, area.w - 60.0, 18.0),
            self.day_label(day, today),
            TextStyle::new(13.0, Weight::SemiBold),
            th.text,
        );
        if !idx.is_empty() {
            cv.text(
                Rect::new(area.right() - 60.0, area.y + 2.0, 60.0, 14.0),
                format!(
                    "{} event{}",
                    idx.len(),
                    if idx.len() == 1 { "" } else { "s" }
                ),
                TextStyle::caption().align(Align::End),
                th.text_faint,
            );
        }
        if idx.is_empty() {
            cv.text(
                Rect::new(area.x, area.y + 40.0, area.w, 18.0),
                "Nothing scheduled",
                TextStyle::body().align(Align::Center),
                th.text_faint,
            );
            return;
        }
        // All-day events first, then by start.
        let mut order = idx;
        order.sort_by_key(|&i| {
            let e = &self.data.events[i];
            (!e.all_day, e.start_utc)
        });
        let next = self.next_event(unix);
        let mut y = area.y + 24.0;
        let shown = order.len().min(AGENDA_ROWS);
        for (k, &i) in order.iter().take(shown).enumerate() {
            let e = &self.data.events[i];
            let row = Rect::new(area.x, y, area.w, ROW_H - 4.0);
            let is_next = Some(i) == next && day == today;
            let ended = day == today && !e.all_day && e.end_utc.max(e.start_utc + 60) <= unix;
            if is_next {
                cv.round_rect(row, 9.0, th.surface);
            }
            let bar = feed_color(&th, e.feed);
            cv.round_rect(
                Rect::new(row.x + 4.0, row.y + 5.0, 3.0, row.h - 10.0),
                1.5,
                if ended { th.text_faint } else { bar },
            );
            let join = e.join_url.is_some() && !ended;
            let text_w = (row.w - 16.0 - if join { 52.0 } else { 0.0 }).max(0.0);
            let when = if e.all_day {
                "All day".to_string()
            } else if is_next {
                let delta = e.start_utc - unix;
                if delta > 0 {
                    format!("{} · {}", countdown(delta), fmt_clock(e.start_local, h24))
                } else {
                    format!("now · until {}", fmt_clock(e.end_local, h24))
                }
            } else {
                format!(
                    "{} – {}",
                    fmt_clock(e.start_local, h24),
                    fmt_clock(e.end_local, h24)
                )
            };
            cv.text(
                Rect::new(row.x + 12.0, row.y + 3.0, text_w, 14.0),
                when,
                TextStyle::caption().tabular(),
                if is_next { th.accent } else { th.text_faint },
            );
            cv.text(
                Rect::new(row.x + 12.0, row.y + 17.0, text_w, 16.0),
                title_text(&e.title),
                TextStyle::body(),
                if ended { th.text_faint } else { th.text },
            );
            if join {
                let b = Rect::new(row.right() - 48.0, row.y + (row.h - 22.0) * 0.5, 46.0, 22.0);
                let hot = self.hover == Some(hit_join(k));
                cv.capsule(
                    b,
                    if hot {
                        th.accent.with_alpha(0.85)
                    } else {
                        th.accent
                    },
                );
                cv.icon(
                    Icon::Video,
                    Rect::new(b.x + 7.0, b.y + 5.0, 12.0, 12.0),
                    th.on_accent,
                );
                cv.text(
                    Rect::new(b.x + 20.0, b.y, b.w - 22.0, b.h),
                    "Join",
                    TextStyle::new(11.5, Weight::SemiBold),
                    th.on_accent,
                );
                cv.hit(b, hit_join(k), CursorKind::Hand);
            }
            y += ROW_H;
        }
        if order.len() > shown {
            cv.text(
                Rect::new(area.x, y - 2.0, area.w, 14.0),
                format!("+{} more", order.len() - shown),
                TextStyle::caption().align(Align::Center),
                th.text_faint,
            );
        }
    }

    fn draw_setup(&self, cv: &mut Canvas, area: Rect) {
        let th = *cv.theme;
        let c = area.centered(area.w, 110.0);
        cv.icon(
            Icon::Calendar,
            Rect::new(c.center().x - 14.0, c.y, 28.0, 28.0),
            th.text_faint,
        );
        cv.text(
            Rect::new(c.x, c.y + 36.0, c.w, 20.0),
            "No calendar yet",
            TextStyle::body().align(Align::Center),
            th.text_dim,
        );
        cv.text(
            Rect::new(c.x, c.y + 56.0, c.w, 16.0),
            "Add an ICS feed under [calendar] in config.toml",
            TextStyle::caption().align(Align::Center),
            th.text_faint,
        );
        let b = Rect::new(c.center().x - 55.0, c.y + 82.0, 110.0, 24.0);
        let hot = self.hover == Some(HIT_CONFIG);
        cv.capsule(b, if hot { th.surface_hi } else { th.surface });
        cv.text(
            b,
            "Open config",
            TextStyle::label().align(Align::Center),
            th.text,
        );
        cv.hit(b, HIT_CONFIG, CursorKind::Hand);
    }
}

impl Module for Calendar {
    fn id(&self) -> ModuleId {
        "calendar"
    }

    fn title(&self) -> &'static str {
        "Calendar"
    }

    fn icon(&self) -> Icon {
        Icon::Calendar
    }

    fn subscriptions(&self) -> EventMask {
        EventMask::of(&[Kind::Calendar])
    }

    /// Keeps "in 12 min" fresh while the page is open. (Honoured only while expanded.)
    fn poll_interval(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_secs(30))
    }

    fn expanded_size(&self) -> Size {
        Size::new(424.0, 236.0)
    }

    fn peek_size(&self) -> Option<Size> {
        Some(Size::new(354.0, 76.0))
    }

    fn chip_width(&self) -> Option<f32> {
        self.chip_on.then_some(CHIP_W)
    }

    fn chip_priority(&self) -> i32 {
        30
    }

    fn on_start(&mut self, cx: &mut Cx) {
        if !self.cfg.feeds.is_empty() {
            self.last_refresh_req = cx.now;
        }
    }

    fn next_wake(&self, now: f64, env: &Env) -> Option<f64> {
        let unix = env.unix;
        let (chip, alert) = (
            i64::from(self.cfg.chip_minutes) * 60,
            i64::from(self.cfg.alert_minutes) * 60,
        );
        let mut best: Option<i64> = None;
        let mut take = |t: i64| {
            if t > unix {
                best = Some(best.map_or(t, |b| b.min(t)));
            }
        };
        for e in self.data.events.iter().filter(|e| !e.all_day) {
            let s = e.start_utc;
            if s > unix + 2 * DAY {
                continue;
            }
            if s + CHIP_LINGER <= unix {
                continue;
            }
            for t in [s - chip, s - alert, s, s + CHIP_LINGER] {
                take(t);
            }
        }
        // While the chip shows, its minute count turns over on a minute boundary.
        if self.chip_on
            && let Some(e) = self.chip_event(unix)
            && e.start_utc > unix
        {
            take(unix + secs_to_chip_change(e.start_utc - unix));
        }
        best.map(|t| now + (t - unix) as f64)
    }

    fn on_tick(&mut self, cx: &mut Cx) {
        self.prune(cx.env.unix);
        self.raise_alerts(cx);
        // The chip's text (or its presence) changed with time.
        self.refresh_chip(cx.env.unix);
        cx.request_redraw();
    }

    fn on_event(&mut self, ev: &Event, cx: &mut Cx) {
        if let EventKind::Calendar(d) = &ev.kind {
            self.data = d.clone();
            self.refresh_chip(cx.env.unix);
            self.raise_alerts(cx);
            cx.request_redraw();
        }
    }

    fn on_config(&mut self, cfg: &Config, cx: &mut Cx) {
        self.cfg = cfg.calendar.clone();
        self.refresh_chip(cx.env.unix);
        cx.request_redraw();
    }

    fn on_visibility(&mut self, v: Visibility, cx: &mut Cx) {
        match v {
            Visibility::Expanded => {
                self.view_today();
                let stale = cx.env.unix - self.data.fetched_unix > STALE_SECS;
                if !self.cfg.feeds.is_empty() && stale && cx.now - self.last_refresh_req > 30.0 {
                    self.last_refresh_req = cx.now;
                    cx.command(Command::Calendar(CalCmd::Refresh));
                }
            }
            _ => self.hover = None,
        }
    }

    fn on_suspend(&mut self, _cx: &mut Cx) {
        self.suspended = true;
        self.hover = None;
    }

    fn on_resume(&mut self, cx: &mut Cx) {
        self.suspended = false;
        // A meeting that is still ahead (or just started) is announced now.
        self.raise_alerts(cx);
        self.refresh_chip(cx.env.unix);
    }

    fn on_poll(&mut self, cx: &mut Cx) {
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
                match h {
                    HIT_PREV | HIT_NEXT => {
                        let (y, m) = self.view_month(cx.env);
                        self.view = Some(shift_month(y, m, if h == HIT_NEXT { 1 } else { -1 }));
                    }
                    HIT_TODAY => self.view_today(),
                    HIT_CONFIG => cx.command(Command::OpenConfig),
                    HIT_REFRESH => cx.command(Command::Calendar(CalCmd::Refresh)),
                    _ => {
                        if let Some(c) = cell_of(h) {
                            let (vy, vm) = self.view_month(cx.env);
                            let day = month_grid(vy, vm, self.monday_first())[c];
                            self.selected = Some(day);
                            let (y, m, _) = civil_from_days(day);
                            self.view = Some((y, m));
                        } else if let Some(k) = join_of(h) {
                            let day = self.selected_day(cx.env);
                            let mut order = self.events_on(day);
                            order.sort_by_key(|&i| {
                                let e = &self.data.events[i];
                                (!e.all_day, e.start_utc)
                            });
                            if let Some(url) = order
                                .get(k)
                                .and_then(|&i| self.data.events[i].join_url.clone())
                            {
                                cx.command(Command::OpenUrl(url));
                            }
                        } else {
                            return false;
                        }
                    }
                }
                cx.request_redraw();
                true
            }
            _ => false,
        }
    }

    fn on_peek_input(&mut self, hit: Option<HitId>, input: &Input, cx: &mut Cx) -> bool {
        if matches!(input, Input::Click(_))
            && hit == Some(HIT_JOIN_PEEK)
            && let Some(url) = self.alert.as_ref().and_then(|a| a.join.clone())
        {
            cx.command(Command::OpenUrl(url));
            return true;
        }
        false
    }

    fn draw_chip(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let th = *cv.theme;
        let Some(e) = self.chip_event(dx.env.unix) else {
            return;
        };
        let icon = Rect::new(area.x, area.center().y - 8.0, 16.0, 16.0);
        cv.icon(Icon::Calendar, icon, th.accent);
        let until = e.start_utc - dx.env.unix;
        let text = if until <= 0 {
            "now".to_string()
        } else {
            format!("{}m", crate::pomodoro::chip_minutes(until))
        };
        cv.text(
            Rect::new(
                icon.right() + 3.0,
                area.y,
                area.right() - icon.right() - 3.0,
                area.h,
            ),
            text,
            TextStyle::new(12.0, Weight::SemiBold).tabular(),
            th.text,
        );
    }

    fn draw_peek(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let th = *cv.theme;
        let Some(a) = self.alert.clone() else { return };
        let tile = Rect::new(area.x, area.y, area.h, area.h);
        cv.squircle(tile, 8.0, th.surface_hi);
        cv.icon(Icon::Calendar, tile.inset(tile.w * 0.22), th.accent);
        let join = a.join.is_some();
        let btn_w = 62.0;
        let x = tile.right() + 12.0;
        let w = (area.right() - x - if join { btn_w + 8.0 } else { 0.0 }).max(0.0);
        let line = |y: f32, h: f32| Rect::new(x, area.y + y, w, h);
        let until = a.start_utc - dx.env.unix;
        let when = match a.kind {
            AlertKind::Start => "Starting now".to_string(),
            AlertKind::Lead => format!(
                "{} · {}",
                countdown(until),
                fmt_clock(a.start_local, Self::hour24(dx.env))
            ),
        };
        cv.text(line(0.0, 15.0), "Calendar", TextStyle::caption(), th.accent);
        cv.text(
            line(15.0, 18.0),
            title_text(&a.title),
            TextStyle::new(13.0, Weight::SemiBold),
            th.text,
        );
        cv.text(line(33.0, 15.0), when, TextStyle::caption(), th.text_dim);
        if join {
            let b = Rect::new(
                area.right() - btn_w,
                area.y + (area.h - 26.0) * 0.5,
                btn_w,
                26.0,
            );
            cv.capsule(b, th.accent);
            cv.icon(
                Icon::Video,
                Rect::new(b.x + 9.0, b.y + 6.0, 14.0, 14.0),
                th.on_accent,
            );
            cv.text(
                Rect::new(b.x + 25.0, b.y, b.w - 27.0, b.h),
                "Join",
                TextStyle::new(12.0, Weight::SemiBold),
                th.on_accent,
            );
            cv.hit(b, HIT_JOIN_PEEK, CursorKind::Hand);
        }
    }

    fn draw_expanded(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let th = *cv.theme;
        if self.cfg.feeds.is_empty() {
            self.draw_setup(cv, area);
            return;
        }
        let (header, body) = area.split_top(26.0);
        self.draw_header(cv, header, dx.env);
        let grid_area = Rect::new(body.x, body.y + 4.0, GRID_W, body.h - 4.0);
        self.draw_grid(cv, grid_area, dx.env);
        let agenda = Rect::new(
            body.x + GRID_W + 16.0,
            body.y + 4.0,
            (body.w - GRID_W - 16.0).max(0.0),
            body.h - 4.0,
        );
        self.draw_agenda(cv, agenda, dx.env);
        if self.data.failed > 0 {
            let msg = match &self.data.error {
                Some(e) => format!("Could not read the feed: {e}"),
                None => format!("Could not read {} feed(s)", self.data.failed),
            };
            let r = Rect::new(agenda.x, area.bottom() - 14.0, agenda.w - 24.0, 14.0);
            cv.text(r, msg, TextStyle::caption(), th.warn);
            let b = Rect::new(agenda.right() - 20.0, area.bottom() - 17.0, 20.0, 20.0);
            cv.icon_button(b, Icon::Reset, HIT_REFRESH, th.text_dim, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::civil::{LocalTime, unix_from_civil};
    use crate::draw::{DrawCmd, DrawList};
    use crate::module::{Out, ShellRequest};

    const NOON: i64 = 12 * 3600;

    fn env_at(h: u32, mi: u32, s: u32) -> Env {
        Env {
            local: LocalTime::new(2026, 10, 6, h, mi, s),
            unix: unix_from_civil(2026, 10, 6, h, mi, s),
            system_24h: true,
            ..Env::default()
        }
    }

    fn ev(title: &str, start: i64, mins: i64, join: Option<&str>) -> CalEvent {
        CalEvent {
            title: title.into(),
            location: "".into(),
            start_utc: start,
            end_utc: start + mins * 60,
            start_local: start,
            end_local: start + mins * 60,
            all_day: false,
            join_url: join.map(Into::into),
            feed: 0,
        }
    }

    fn feeds_cfg() -> CalendarCfg {
        CalendarCfg {
            feeds: vec!["https://example.com/a.ics".into()],
            ..CalendarCfg::default()
        }
    }

    /// A context borrowing only the fixture's plain fields, so `t.m` stays usable beside it.
    macro_rules! cx {
        ($t:expr) => {
            Cx::for_test($t.now, &$t.env, &$t.theme, &$t.cfg, &mut $t.out)
        };
    }

    struct T {
        m: Calendar,
        theme: Theme,
        cfg: Config,
        env: Env,
        out: Out,
        now: f64,
    }

    impl T {
        fn new() -> T {
            T {
                m: Calendar::new(feeds_cfg()),
                theme: Theme::default(),
                cfg: Config {
                    calendar: feeds_cfg(),
                    ..Config::default()
                },
                env: env_at(14, 5, 9),
                out: Out::default(),
                now: 100.0,
            }
        }
        fn set_time(&mut self, h: u32, mi: u32, s: u32) {
            let dt = unix_from_civil(2026, 10, 6, h, mi, s) - self.env.unix;
            self.env = env_at(h, mi, s);
            self.now += dt as f64;
        }
        fn feed(&mut self, events: Vec<CalEvent>) {
            let d = Arc::new(CalendarData {
                events,
                fetched_unix: self.env.unix,
                feeds: 1,
                ..Default::default()
            });
            let e = Event::new(crate::events::Source::Local, EventKind::Calendar(d));
            let mut cx = cx!(self);
            self.m.on_event(&e, &mut cx);
        }
        fn tick(&mut self) {
            let mut cx = cx!(self);
            self.m.on_tick(&mut cx);
        }
        fn input(&mut self, hit: Option<HitId>, i: Input) -> bool {
            let mut cx = cx!(self);
            self.m.on_input(hit, &i, &mut cx)
        }
        fn peek_input(&mut self, hit: Option<HitId>, i: Input) -> bool {
            let mut cx = cx!(self);
            self.m.on_peek_input(hit, &i, &mut cx)
        }
        fn visibility(&mut self, v: Visibility) {
            let mut cx = cx!(self);
            self.m.on_visibility(v, &mut cx);
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
        fn draw_peek(&mut self) -> DrawList {
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
        fn draw_chip(&mut self) -> DrawList {
            let mut list = DrawList::new();
            let mut cv = Canvas::new(&mut list, &self.theme);
            let dx = DrawCx {
                now: self.now,
                env: &self.env,
                config: &self.cfg,
            };
            self.m
                .draw_chip(&mut cv, Rect::new(0.0, 0.0, CHIP_W, 24.0), &dx);
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

    fn at(h: u32, mi: u32) -> i64 {
        unix_from_civil(2026, 10, 6, h, mi, 0)
    }

    // ----- helpers -----

    #[test]
    fn the_month_grid_starts_on_the_right_weekday() {
        // October 2026 starts on a Thursday.
        let g = month_grid(2026, 10, true);
        assert_eq!(
            civil_from_days(g[0]),
            (2026, 9, 28),
            "Monday-first: the grid starts 28 Sep"
        );
        assert_eq!(civil_from_days(g[3]), (2026, 10, 1));
        let g = month_grid(2026, 10, false);
        assert_eq!(civil_from_days(g[0]), (2026, 9, 27), "Sunday-first");
        assert_eq!(civil_from_days(g[41]), (2026, 11, 7));
        // February 2026 starts on a Sunday: Monday-first leaves six leading days.
        let g = month_grid(2026, 2, true);
        assert_eq!(civil_from_days(g[6]), (2026, 2, 1));
    }

    #[test]
    fn months_shift_across_year_ends() {
        assert_eq!(shift_month(2026, 12, 1), (2027, 1));
        assert_eq!(shift_month(2026, 1, -1), (2025, 12));
        assert_eq!(shift_month(2026, 10, 0), (2026, 10));
        assert_eq!(shift_month(2026, 3, -15), (2024, 12));
    }

    #[test]
    fn clock_and_countdown_text() {
        assert_eq!(fmt_clock(13 * 3600 + 5 * 60, true), "13:05");
        assert_eq!(fmt_clock(13 * 3600 + 5 * 60, false), "1:05 PM");
        assert_eq!(fmt_clock(0, false), "12:00 AM");
        assert_eq!(fmt_clock(12 * 3600, false), "12:00 PM");
        assert_eq!(countdown(0), "now");
        assert_eq!(countdown(-30), "now");
        assert_eq!(countdown(1), "in 1 min");
        assert_eq!(countdown(61), "in 2 min");
        assert_eq!(countdown(59 * 60), "in 59 min");
        assert_eq!(countdown(3600), "in 1 h");
        assert_eq!(countdown(3600 + 5 * 60), "in 1 h 05 min");
        assert_eq!(countdown(3 * DAY), "in 3 d");
    }

    // ----- state -----

    #[test]
    fn events_are_found_on_their_local_days_including_multi_day_and_all_day() {
        let mut t = T::new();
        let day = |d: u32| days_from_civil(2026, 10, d);
        let mut trip = ev("Trip", day(7) * DAY, 0, None);
        trip.all_day = true;
        trip.end_utc = day(9) * DAY;
        trip.end_local = day(9) * DAY;
        t.feed(vec![
            ev("Standup", at(9, 0), 15, None),
            ev("Late", unix_from_civil(2026, 10, 6, 23, 30, 0), 60, None), // ends after midnight
            trip,
        ]);
        let titles = |d: i64| -> Vec<String> {
            t.m.events_on(d)
                .into_iter()
                .map(|i| t.m.data.events[i].title.to_string())
                .collect()
        };
        assert_eq!(titles(day(6)), vec!["Standup", "Late"]);
        assert_eq!(titles(day(7)), vec!["Late", "Trip"]);
        assert_eq!(titles(day(8)), vec!["Trip"]);
        assert!(titles(day(9)).is_empty(), "the all-day end is exclusive");
    }

    #[test]
    fn the_grid_marks_days_with_events() {
        let mut t = T::new();
        t.feed(vec![
            ev("a", at(9, 0), 30, None),
            ev("b", at(10, 0), 30, None),
            ev("c", at(11, 0), 30, None),
            ev("d", at(12, 0), 30, None),
            ev("far", at(9, 0) + 10 * DAY, 30, None),
        ]);
        let grid = month_grid(2026, 10, true);
        let m = t.m.marks(&grid);
        let cell = |d: u32| (days_from_civil(2026, 10, d) - grid[0]) as usize;
        assert_eq!(m[cell(6)], 3, "capped at three dots");
        assert_eq!(m[cell(16)], 1);
        assert_eq!(m[cell(7)], 0);
    }

    #[test]
    fn navigation_moves_the_month_selects_days_and_returns_to_today() {
        let mut t = T::new();
        assert_eq!(t.m.view_month(&t.env), (2026, 10));
        assert!(t.input(Some(HIT_NEXT), Input::Click(Vec2::ZERO)));
        assert_eq!(t.m.view_month(&t.env), (2026, 11));
        assert!(t.input(Some(HIT_PREV), Input::Click(Vec2::ZERO)));
        assert!(t.input(Some(HIT_PREV), Input::Click(Vec2::ZERO)));
        assert_eq!(t.m.view_month(&t.env), (2026, 9));
        // Click the cell for 3 Oct (in the next month's view it is a leading day of October).
        assert!(t.input(Some(HIT_NEXT), Input::Click(Vec2::ZERO)));
        let grid = month_grid(2026, 10, true);
        let cell = (days_from_civil(2026, 10, 15) - grid[0]) as usize;
        assert!(t.input(Some(hit_cell(cell)), Input::Click(Vec2::ZERO)));
        assert_eq!(t.m.selected, Some(days_from_civil(2026, 10, 15)));
        // Clicking a trailing day of the next month moves the view there.
        let trailing = (days_from_civil(2026, 11, 2) - grid[0]) as usize;
        assert!(t.input(Some(hit_cell(trailing)), Input::Click(Vec2::ZERO)));
        assert_eq!(t.m.view_month(&t.env), (2026, 11));
        assert!(t.input(Some(HIT_TODAY), Input::Click(Vec2::ZERO)));
        assert_eq!((t.m.view_month(&t.env), t.m.selected), ((2026, 10), None));
        assert!(
            !t.input(Some(HitId(999)), Input::Click(Vec2::ZERO)),
            "unknown regions are not consumed"
        );
    }

    #[test]
    fn opening_the_page_shows_today_and_refreshes_stale_data() {
        let mut t = T::new();
        t.m.view = Some((2020, 1));
        t.m.selected = Some(5);
        t.visibility(Visibility::Expanded);
        assert_eq!((t.m.view_month(&t.env), t.m.selected), ((2026, 10), None));
        assert!(
            t.out.commands.contains(&Command::Calendar(CalCmd::Refresh)),
            "never fetched: ask for data"
        );
        // Fresh data and a recent request: no new request.
        t.out.commands.clear();
        t.feed(vec![]);
        t.visibility(Visibility::Hidden);
        t.visibility(Visibility::Expanded);
        assert!(t.out.commands.is_empty());
        // Stale again, but asked too recently.
        t.env.unix += 2 * STALE_SECS;
        t.visibility(Visibility::Hidden);
        t.visibility(Visibility::Expanded);
        assert!(t.out.commands.is_empty(), "rate-limited");
        t.now += 60.0;
        t.visibility(Visibility::Hidden);
        t.visibility(Visibility::Expanded);
        assert_eq!(t.out.commands, vec![Command::Calendar(CalCmd::Refresh)]);
    }

    #[test]
    fn with_no_feeds_the_page_says_how_to_add_one_and_nothing_is_fetched() {
        let mut t = T::new();
        t.m = Calendar::new(CalendarCfg::default());
        t.visibility(Visibility::Expanded);
        assert!(t.out.commands.is_empty(), "no feeds, no fetch");
        let tx = texts(&t.draw());
        assert!(tx.contains(&"No calendar yet".to_string()), "{tx:?}");
        assert!(tx.iter().any(|s| s.contains("config.toml")));
        assert!(t.input(Some(HIT_CONFIG), Input::Click(Vec2::ZERO)));
        assert_eq!(t.out.commands, vec![Command::OpenConfig]);
    }

    // ----- the chip -----

    #[test]
    fn the_chip_appears_within_the_window_and_counts_down_by_minutes() {
        let mut t = T::new();
        t.feed(vec![ev(
            "Review",
            at(14, 30),
            30,
            Some("https://meet.google.com/x"),
        )]);
        assert_eq!(
            t.m.chip_width(),
            None,
            "25 minutes away: beyond the 15 minute window"
        );
        t.set_time(14, 15, 0);
        t.tick();
        assert_eq!(t.m.chip_width(), Some(CHIP_W));
        assert!(texts(&t.draw_chip()).contains(&"15m".to_string()));
        t.set_time(14, 29, 30);
        t.tick();
        assert!(texts(&t.draw_chip()).contains(&"1m".to_string()));
        t.set_time(14, 30, 5);
        t.tick();
        assert!(texts(&t.draw_chip()).contains(&"now".to_string()));
        t.set_time(14, 33, 0);
        t.tick();
        assert_eq!(
            t.m.chip_width(),
            None,
            "gone a couple of minutes after the start"
        );
    }

    #[test]
    fn all_day_events_and_a_zero_window_never_make_a_chip() {
        let mut t = T::new();
        let mut all_day = ev("Holiday", at(14, 10), 60, None);
        all_day.all_day = true;
        t.feed(vec![all_day]);
        assert_eq!(t.m.chip_width(), None);
        t.m.cfg.chip_minutes = 0;
        t.feed(vec![ev("Soon", at(14, 10), 30, None)]);
        assert_eq!(t.m.chip_width(), None);
    }

    // ----- wake-ups -----

    #[test]
    fn the_module_asks_to_be_woken_exactly_when_something_changes() {
        let mut t = T::new();
        t.feed(vec![ev("Review", at(15, 0), 30, None)]);
        // Now 14:05:09; the chip appears 15 minutes before 15:00 -> 14:45.
        let wake = t.m.next_wake(t.now, &t.env).unwrap();
        assert!((wake - t.now - (at(14, 45) - t.env.unix) as f64).abs() < 1e-6);
        // At 14:45 the chip shows; the next change is the alert (5 min before), but the chip's own
        // minute count turns over first: 14:45:00 -> 15 min left, the next turnover 60 s later.
        t.set_time(14, 45, 0);
        t.tick();
        let wake = t.m.next_wake(t.now, &t.env).unwrap();
        assert!((wake - t.now - 60.0).abs() < 1e-6, "{}", wake - t.now);
        // Alert time: 14:55.
        t.set_time(14, 54, 30);
        t.tick();
        let wake = t.m.next_wake(t.now, &t.env).unwrap();
        assert!((wake - t.now - 30.0).abs() < 1e-6, "{}", wake - t.now);
    }

    #[test]
    fn nothing_is_scheduled_without_upcoming_events() {
        let mut t = T::new();
        assert_eq!(t.m.next_wake(t.now, &t.env), None);
        t.feed(vec![ev("Old", at(9, 0), 30, None)]);
        assert_eq!(
            t.m.next_wake(t.now, &t.env),
            None,
            "everything is in the past"
        );
        t.feed(vec![ev("Far", at(14, 0) + 10 * DAY, 30, None)]);
        assert_eq!(
            t.m.next_wake(t.now, &t.env),
            None,
            "days away: no wake-up needed yet"
        );
    }

    // ----- banners -----

    #[test]
    fn a_banner_with_join_is_raised_before_the_start_and_again_at_the_start_once_each() {
        let mut t = T::new();
        t.feed(vec![ev(
            "Review",
            at(15, 0),
            30,
            Some("https://meet.google.com/abc"),
        )]);
        t.out.shell.clear();
        t.set_time(14, 55, 0);
        t.tick();
        assert_eq!(t.out.shell.len(), 1, "5 minutes before");
        assert!(matches!(
            t.out.shell[0],
            ShellRequest::Peek { module: "test", .. }
        ));
        t.tick();
        assert_eq!(t.out.shell.len(), 1, "not twice for the same moment");
        let tx = texts(&t.draw_peek());
        assert!(tx.contains(&"Review".to_string()), "{tx:?}");
        assert!(tx.iter().any(|s| s.starts_with("in 5 min")), "{tx:?}");
        assert!(tx.contains(&"Join".to_string()));
        t.set_time(15, 0, 3);
        t.tick();
        assert_eq!(t.out.shell.len(), 2, "and at the start");
        assert!(texts(&t.draw_peek()).contains(&"Starting now".to_string()));
        t.set_time(15, 0, 40);
        t.tick();
        assert_eq!(t.out.shell.len(), 2);
        t.set_time(15, 5, 0);
        t.tick();
        assert_eq!(t.out.shell.len(), 2, "too late for a start banner");
    }

    #[test]
    fn clicking_join_in_the_banner_opens_the_call_and_other_clicks_are_left_to_the_shell() {
        let mut t = T::new();
        t.feed(vec![ev(
            "Review",
            at(14, 8),
            30,
            Some("https://zoom.us/j/42"),
        )]);
        t.set_time(14, 6, 0);
        t.tick();
        let list = t.draw_peek();
        assert!(
            list.hits.iter().any(|h| h.id == HIT_JOIN_PEEK),
            "the button is a hit region"
        );
        assert!(t.peek_input(Some(HIT_JOIN_PEEK), Input::Click(Vec2::ZERO)));
        assert_eq!(
            t.out.commands,
            vec![Command::OpenUrl("https://zoom.us/j/42".into())]
        );
        assert!(
            !t.peek_input(None, Input::Click(Vec2::ZERO)),
            "elsewhere: the shell opens the page"
        );
        assert!(!t.peek_input(Some(HIT_JOIN_PEEK), Input::Move(Vec2::ZERO)));
    }

    #[test]
    fn a_banner_without_a_call_link_has_no_join_button() {
        let mut t = T::new();
        t.feed(vec![ev("Lunch", at(14, 8), 30, None)]);
        t.set_time(14, 6, 0);
        t.tick();
        assert_eq!(t.out.shell.len(), 1);
        let list = t.draw_peek();
        assert!(!texts(&list).contains(&"Join".to_string()));
        assert!(list.hits.is_empty());
    }

    #[test]
    fn nothing_pops_up_while_a_game_is_in_front_and_the_banner_comes_after() {
        let mut t = T::new();
        t.feed(vec![ev("Review", at(14, 20), 30, None)]);
        {
            let mut cx = cx!(t);
            t.m.on_suspend(&mut cx);
        }
        t.set_time(14, 17, 0);
        t.tick();
        assert!(t.out.shell.is_empty(), "silent while suspended");
        assert!(t.m.chip_on, "the chip state still follows the clock");
        {
            let mut cx = cx!(t);
            t.m.on_resume(&mut cx);
        }
        assert_eq!(
            t.out.shell.len(),
            1,
            "announced when the game is closed, if still ahead"
        );
        // But a meeting that started long ago is not announced late.
        let mut t = T::new();
        t.feed(vec![ev("Past", at(13, 0), 30, None)]);
        let mut cx = cx!(t);
        t.m.on_resume(&mut cx);
        assert!(t.out.shell.is_empty());
    }

    #[test]
    fn banners_can_be_switched_off() {
        let mut t = T::new();
        t.m.cfg.alert_minutes = 0;
        t.feed(vec![ev("Review", at(14, 8), 30, None)]);
        t.set_time(14, 7, 0);
        t.tick();
        assert!(t.out.shell.is_empty());
    }

    #[test]
    fn two_events_at_the_same_time_are_announced_one_after_the_other() {
        let mut t = T::new();
        t.feed(vec![
            ev("Alpha", at(14, 8), 30, None),
            ev("Beta", at(14, 8), 30, None),
        ]);
        t.set_time(14, 6, 0);
        t.tick();
        t.tick();
        assert_eq!(t.out.shell.len(), 2, "each gets its banner");
        t.tick();
        assert_eq!(t.out.shell.len(), 2);
    }

    // ----- the page -----

    #[test]
    fn the_page_shows_the_month_the_day_and_its_events() {
        let mut t = T::new();
        t.feed(vec![
            ev("Standup", at(9, 0), 15, None),
            ev(
                "Design review",
                at(15, 0),
                60,
                Some("https://meet.google.com/abc"),
            ),
            ev("Retro", at(16, 30), 30, None),
        ]);
        let tx = texts(&t.draw());
        assert!(tx.contains(&"October 2026".to_string()), "{tx:?}");
        assert!(tx.contains(&"Today".to_string()));
        assert!(tx.contains(&"3 events".to_string()));
        assert!(tx.contains(&"Standup".to_string()) && tx.contains(&"Retro".to_string()));
        assert!(
            tx.contains(&"09:00 – 09:15".to_string()),
            "past events show their range: {tx:?}"
        );
        assert!(
            tx.iter().any(|s| s.starts_with("in 55 min")),
            "the next event shows its countdown: {tx:?}"
        );
        assert!(tx.contains(&"Join".to_string()));
        // 42 day cells, each a hit region, plus the arrows, Today and one Join.
        let list = t.draw();
        let cells = list.hits.iter().filter(|h| cell_of(h.id).is_some()).count();
        assert_eq!(cells, 42);
        assert!(list.hits.iter().any(|h| join_of(h.id).is_some()));
    }

    #[test]
    fn selecting_another_day_shows_that_days_events() {
        let mut t = T::new();
        t.feed(vec![
            ev("Today thing", at(9, 0), 15, None),
            ev("Friday thing", at(9, 0) + 3 * DAY, 15, None),
        ]);
        t.m.selected = Some(days_from_civil(2026, 10, 9));
        let tx = texts(&t.draw());
        assert!(tx.contains(&"Fri 9 Oct".to_string()), "{tx:?}");
        assert!(tx.contains(&"Friday thing".to_string()));
        assert!(!tx.contains(&"Today thing".to_string()));
        t.m.selected = Some(days_from_civil(2026, 10, 12));
        assert!(texts(&t.draw()).contains(&"Nothing scheduled".to_string()));
    }

    #[test]
    fn the_join_button_in_the_agenda_opens_the_right_events_link() {
        let mut t = T::new();
        t.feed(vec![
            ev("No call", at(15, 0), 30, None),
            ev(
                "Call",
                at(16, 0),
                30,
                Some("https://teams.microsoft.com/l/meetup-join/x"),
            ),
        ]);
        let list = t.draw();
        let join = list
            .hits
            .iter()
            .find(|h| join_of(h.id).is_some())
            .unwrap()
            .id;
        assert!(t.input(Some(join), Input::Click(Vec2::ZERO)));
        assert_eq!(
            t.out.commands,
            vec![Command::OpenUrl(
                "https://teams.microsoft.com/l/meetup-join/x".into()
            )]
        );
    }

    #[test]
    fn long_days_are_summarised_and_ended_events_lose_their_join_button() {
        let mut t = T::new();
        let events: Vec<CalEvent> = (0..7)
            .map(|i| ev(&format!("e{i}"), at(8 + i, 0), 30, None))
            .collect();
        t.feed(events);
        let tx = texts(&t.draw());
        assert!(tx.contains(&"+3 more".to_string()), "{tx:?}");
        let mut t = T::new();
        t.feed(vec![ev("Done", at(9, 0), 30, Some("https://zoom.us/j/1"))]);
        assert!(
            texts(&t.draw()).iter().all(|s| s != "Join"),
            "it already ended"
        );
    }

    #[test]
    fn a_failed_feed_is_reported_with_a_retry_button() {
        let mut t = T::new();
        let d = Arc::new(CalendarData {
            feeds: 1,
            failed: 1,
            error: Some("HTTP 404".into()),
            ..Default::default()
        });
        let e = Event::new(crate::events::Source::Local, EventKind::Calendar(d));
        let mut cx = cx!(t);
        t.m.on_event(&e, &mut cx);
        let list = t.draw();
        assert!(texts(&list).iter().any(|s| s.contains("HTTP 404")));
        assert!(t.input(Some(HIT_REFRESH), Input::Click(Vec2::ZERO)));
        assert_eq!(t.out.commands, vec![Command::Calendar(CalCmd::Refresh)]);
    }

    #[test]
    fn the_week_start_follows_the_setting() {
        let mut t = T::new();
        t.feed(vec![]);
        let first_letter = |l: &DrawList| {
            texts(l)
                .into_iter()
                .find(|s| s.len() == 1 && s.chars().all(char::is_alphabetic))
                .unwrap()
        };
        assert_eq!(first_letter(&t.draw()), "M");
        t.m.cfg.week_starts_monday = false;
        assert_eq!(first_letter(&t.draw()), "S");
    }

    #[test]
    fn noon_is_not_confused_with_midnight() {
        assert_eq!(fmt_clock(NOON, true), "12:00");
        assert_eq!(
            fmt_clock(-3600, true),
            "23:00",
            "negative seconds wrap into the previous day"
        );
    }

    #[test]
    fn bookkeeping_is_pruned() {
        let mut t = T::new();
        t.feed(vec![ev("Review", at(14, 8), 30, None)]);
        t.set_time(14, 6, 0);
        t.tick();
        assert!(!t.m.alerted.is_empty());
        t.env.unix += 2 * DAY;
        t.tick();
        assert!(t.m.alerted.is_empty());
    }

    #[test]
    fn it_works_inside_the_host_with_a_chip_and_a_peek() {
        use crate::module::ModuleHost;
        let mut cfg = Config::default();
        cfg.modules.order = vec!["calendar".into()];
        cfg.calendar = feeds_cfg();
        let mut host = ModuleHost::new(
            vec![crate::module::Factory {
                id: "calendar",
                create,
            }],
            Arc::new(cfg),
            Theme::default(),
        );
        host.set_context(10.0, env_at(14, 5, 9));
        host.start_new();
        let d = Arc::new(CalendarData {
            events: vec![ev("Review", at(14, 10), 30, Some("https://zoom.us/j/1"))],
            fetched_unix: at(14, 5),
            feeds: 1,
            ..Default::default()
        });
        host.dispatch(vec![Event::new(
            crate::events::Source::Local,
            EventKind::Calendar(d),
        )]);
        let out = host.take_out();
        assert!(host.chips_width() > 0.0, "the chip is on the pill");
        assert!(
            out.shell.iter().any(|r| matches!(
                r,
                ShellRequest::Peek {
                    module: "calendar",
                    ..
                }
            )),
            "and the banner is requested: {:?}",
            out.shell
        );
        assert!(
            host.next_deadline().is_some(),
            "the next change is scheduled"
        );
        assert!(host.peek_owner("calendar").is_some());
    }
}
