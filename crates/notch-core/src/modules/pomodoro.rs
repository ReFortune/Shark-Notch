//! Pomodoro: a focus timer with a to-do list beside it. The task you pick is the one the next
//! finished focus session is credited to.
//!
//! * The timer runs on the wall clock (`Env::unix`), so it survives restarts and sleep; it is saved
//!   with the tasks through the store service. Between its changes nothing runs: the module asks
//!   the host for a wake-up when the phase ends and when the pill's minute count turns over.
//! * While running, a chip with the minutes left sits in the collapsed pill; when a phase ends a
//!   banner says so (and offers to start the next one), with the system chime if enabled. Nothing
//!   is shown or played while a fullscreen app is in front: it is announced when the shell is back.
//! * Typing a task needs the keyboard, which the notch never takes by itself: clicking "Add task"
//!   asks for it, and it is given back as soon as the entry is finished.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{Config, PomodoroCfg};
use crate::draw::{Align, Canvas, CursorKind, DrawCmd, HitId, Text, TextStyle, Weight};
use crate::events::{Event, EventKind, EventMask, Kind};
use crate::geom::{Rect, Size, Vec2};
use crate::icons::Icon;
use crate::input::{Input, Key};
use crate::module::{Command, Cx, DrawCx, Env, Module, ModuleId, StoreCmd, Visibility};
use crate::pomodoro::{
    Finished, Phase, SavedTimer, Settings, Timer, chip_minutes, format_clock, secs_to_chip_change,
};
use crate::theme::Theme;
use crate::todos::{MAX_TITLE, TodoList};

const KEY: &str = "pomodoro";
const CHIP_W: f32 = 64.0;
const ROW_H: f32 = 30.0;
const VISIBLE_ROWS: usize = 5;
/// Characters of a long entry shown in the text field (its tail, where the typing is).
const FIELD_CHARS: usize = 24;

const HIT_PLAY: HitId = HitId(1);
const HIT_SKIP: HitId = HitId(2);
const HIT_RESET: HitId = HitId(3);
const HIT_ADD: HitId = HitId(4);
const HIT_CLEAR_DONE: HitId = HitId(5);
const HIT_START_NEXT: HitId = HitId(900);

fn hit_check(i: usize) -> HitId {
    HitId(100 + i as u32)
}
fn hit_row(i: usize) -> HitId {
    HitId(200 + i as u32)
}
fn hit_del(i: usize) -> HitId {
    HitId(400 + i as u32)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Check(usize),
    Row(usize),
    Del(usize),
}

fn part_of(h: HitId) -> Option<Part> {
    let i = |base: u32| (h.0 - base) as usize;
    match h.0 {
        100..=199 => Some(Part::Check(i(100))),
        200..=399 => Some(Part::Row(i(200))),
        400..=499 => Some(Part::Del(i(400))),
        _ => None,
    }
}

pub fn create(cfg: &Config) -> Option<Box<dyn Module>> {
    cfg.pomodoro
        .enabled
        .then(|| Box::new(Pomodoro::new(cfg.pomodoro.clone())) as Box<dyn Module>)
}

/// The timer settings the configuration describes.
pub fn settings_from(cfg: &PomodoroCfg) -> Settings {
    let secs = |m: f32| (f64::from(m) * 60.0).round().max(1.0) as i64;
    Settings {
        focus: secs(cfg.focus_minutes),
        short_break: secs(cfg.short_break_minutes),
        long_break: secs(cfg.long_break_minutes),
        long_every: cfg.long_break_every.max(1),
        auto_breaks: cfg.auto_start_breaks,
        auto_focus: cfg.auto_start_focus,
    }
}

/// What is saved between runs.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    timer: Option<SavedTimer>,
    todos: TodoList,
    /// Local day (days since the epoch) and the focus sessions finished on it.
    day: i64,
    sessions: u32,
}

fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        s.to_string()
    } else {
        format!("…{}", s.chars().skip(count - n + 1).collect::<String>())
    }
}

pub struct Pomodoro {
    cfg: PomodoroCfg,
    timer: Timer,
    todos: TodoList,
    /// Local day and the number of focus sessions finished on it.
    today: (i64, u32),
    hover: Option<HitId>,
    /// The task being typed (`Some` while the keyboard is ours).
    editing: Option<String>,
    scroll: usize,
    suspended: bool,
    /// What the banner announces, and a phase that ended while a game was in front.
    notice: Option<Finished>,
    unseen: Option<Finished>,
    last_saved: String,
}

impl Pomodoro {
    pub fn new(cfg: PomodoroCfg) -> Pomodoro {
        Pomodoro {
            timer: Timer::new(settings_from(&cfg)),
            cfg,
            todos: TodoList::default(),
            today: (0, 0),
            hover: None,
            editing: None,
            scroll: 0,
            suspended: false,
            notice: None,
            unseen: None,
            last_saved: String::new(),
        }
    }

    pub fn timer(&self) -> &Timer {
        &self.timer
    }

    pub fn todos(&self) -> &TodoList {
        &self.todos
    }

    pub fn sessions_today(&self, env: &Env) -> u32 {
        if self.today.0 == env.local.days() {
            self.today.1
        } else {
            0
        }
    }

    fn save(&mut self, cx: &mut Cx) {
        let saved = Saved {
            timer: Some(self.timer.save()),
            todos: self.todos.clone(),
            day: self.today.0,
            sessions: self.today.1,
        };
        let Ok(json) = serde_json::to_string(&saved) else {
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

    fn load(&mut self, json: &str, cx: &mut Cx) {
        let Ok(saved) = serde_json::from_str::<Saved>(json) else {
            return;
        };
        self.todos = saved.todos.sanitized();
        self.today = (saved.day, saved.sessions.min(999));
        if let Some(t) = &saved.timer {
            self.timer.restore(t, cx.env.unix);
        }
        self.last_saved = json.to_string();
        self.scroll = self.scroll.min(self.max_scroll());
        cx.request_redraw();
    }

    /// A phase ended (by the clock or by skipping): credit it, announce it, save.
    fn on_finished(&mut self, f: Finished, announce: bool, cx: &mut Cx) {
        if f.focus_done {
            self.todos.credit_session();
            let day = cx.env.local.days();
            if self.today.0 != day {
                self.today = (day, 0);
            }
            self.today.1 += 1;
        }
        if announce {
            if self.suspended {
                self.unseen = Some(f);
            } else {
                self.notice = Some(f);
                cx.peek(f64::from(self.cfg.peek_secs));
                if self.cfg.sound {
                    cx.command(Command::Chime);
                }
            }
        }
        self.save(cx);
        cx.request_redraw();
    }

    fn max_scroll(&self) -> usize {
        self.todos.items().len().saturating_sub(VISIBLE_ROWS)
    }

    fn finish_editing(&mut self, commit: bool, cx: &mut Cx) {
        let Some(text) = self.editing.take() else {
            return;
        };
        cx.request_keyboard(false);
        if commit && self.todos.add(&text).is_some() {
            self.scroll = self.max_scroll();
            self.save(cx);
        }
        cx.request_redraw();
    }

    fn unix(env: &Env) -> i64 {
        env.unix
    }

    // ----- drawing -------------------------------------------------------------------------------

    fn phase_color(th: &Theme, p: Phase) -> crate::color::Color {
        if p.is_break() { th.ok } else { th.accent }
    }

    fn draw_timer(&self, cv: &mut Canvas, area: Rect, env: &Env) {
        let th = *cv.theme;
        let now = Self::unix(env);
        let phase = self.timer.phase();
        let color = Self::phase_color(&th, phase);
        let centre = Vec2::new(area.x + 70.0, area.y + 64.0);
        let radius = 54.0;
        cv.push(DrawCmd::Ring {
            center: centre,
            radius,
            width: 6.0,
            start_deg: 0.0,
            sweep_deg: 360.0,
            color: th.surface_hi,
        });
        let left = self.timer.fraction_left(now);
        if left > 0.002 {
            cv.push(DrawCmd::Ring {
                center: centre,
                radius,
                width: 6.0,
                start_deg: 0.0,
                sweep_deg: 360.0 * left,
                color,
            });
        }
        cv.text(
            Rect::new(centre.x - 52.0, centre.y - 22.0, 104.0, 34.0),
            format_clock(self.timer.remaining(now)),
            TextStyle::big().align(Align::Center).tabular(),
            th.text,
        );
        cv.text(
            Rect::new(centre.x - 52.0, centre.y + 12.0, 104.0, 16.0),
            if self.timer.paused() {
                format!("{} · paused", phase.label())
            } else {
                phase.label().to_string()
            },
            TextStyle::caption().align(Align::Center),
            th.text_dim,
        );
        // Controls under the ring.
        let y = area.y + 132.0;
        let play = Rect::new(centre.x - 18.0, y, 36.0, 36.0);
        let (reset, skip) = (
            Rect::new(play.x - 38.0, y + 4.0, 28.0, 28.0),
            Rect::new(play.right() + 10.0, y + 4.0, 28.0, 28.0),
        );
        let hot = |id| self.hover == Some(id);
        cv.icon_button(
            reset,
            Icon::Reset,
            HIT_RESET,
            th.text_dim,
            hot(HIT_RESET).then_some(th.surface),
        );
        cv.icon_button(
            play,
            if self.timer.running() {
                Icon::Pause
            } else {
                Icon::Play
            },
            HIT_PLAY,
            th.on_accent,
            Some(if hot(HIT_PLAY) {
                color.with_alpha(0.85)
            } else {
                color
            }),
        );
        cv.icon_button(
            skip,
            Icon::Next,
            HIT_SKIP,
            th.text_dim,
            hot(HIT_SKIP).then_some(th.surface),
        );
        // Sessions in this cycle, and today's total.
        let every = self.timer.settings().long_every as usize;
        let dots_w = every as f32 * 10.0;
        let dy = area.y + 180.0;
        for k in 0..every {
            let c = Vec2::new(centre.x - dots_w * 0.5 + 5.0 + k as f32 * 10.0, dy);
            let filled = k < self.timer.cycle() as usize;
            cv.circle(c, 3.0, if filled { th.accent } else { th.surface_hi });
        }
        let done = self.sessions_today(env);
        if done > 0 {
            cv.text(
                Rect::new(area.x, dy + 6.0, 140.0, 14.0),
                format!("{done} session{} today", if done == 1 { "" } else { "s" }),
                TextStyle::caption().align(Align::Center),
                th.text_faint,
            );
        }
    }

    fn draw_tasks(&self, cv: &mut Canvas, area: Rect) {
        let th = *cv.theme;
        let open = self.todos.open_count();
        cv.text(
            Rect::new(area.x, area.y, 100.0, 20.0),
            "Tasks",
            TextStyle::title(),
            th.text,
        );
        let mut right = area.right();
        if self.todos.done_count() > 0 {
            let b = Rect::new(right - 70.0, area.y + 2.0, 70.0, 18.0);
            if self.hover == Some(HIT_CLEAR_DONE) {
                cv.capsule(b, th.surface);
            }
            cv.text(
                b,
                "Clear done",
                TextStyle::label().align(Align::Center),
                th.text_dim,
            );
            cv.hit(b, HIT_CLEAR_DONE, CursorKind::Hand);
            right -= 76.0;
        }
        cv.text(
            Rect::new(right - 40.0, area.y + 4.0, 40.0, 14.0),
            format!("{open} open"),
            TextStyle::caption().align(Align::End),
            th.text_faint,
        );
        let list_top = area.y + 26.0;
        let order = self.todos.display_order();
        if order.is_empty() && self.editing.is_none() {
            cv.text(
                Rect::new(area.x, list_top + 24.0, area.w, 18.0),
                "No tasks yet",
                TextStyle::body().align(Align::Center),
                th.text_faint,
            );
            cv.text(
                Rect::new(area.x, list_top + 42.0, area.w, 14.0),
                "Pick one and it earns your focus sessions.",
                TextStyle::caption().align(Align::Center),
                th.text_faint,
            );
        }
        let rows = if self.editing.is_some() {
            VISIBLE_ROWS - 1
        } else {
            VISIBLE_ROWS
        };
        let scroll = self.scroll.min(order.len().saturating_sub(rows));
        for (vi, &idx) in order.iter().skip(scroll).take(rows).enumerate() {
            let t = &self.todos.items()[idx];
            let r = Rect::new(area.x, list_top + vi as f32 * ROW_H, area.w, ROW_H - 2.0);
            let current = self.todos.current_id() == Some(t.id);
            let hovered = matches!(
                self.hover.and_then(part_of),
                Some(Part::Check(i) | Part::Row(i) | Part::Del(i)) if i == vi
            );
            if current {
                cv.round_rect(r, 9.0, th.surface);
                cv.round_rect(
                    Rect::new(r.x + 1.0, r.y + 6.0, 3.0, r.h - 12.0),
                    1.5,
                    th.accent,
                );
            } else if hovered {
                cv.round_rect(r, 9.0, th.surface.with_alpha(th.surface.a * 0.6));
            }
            // Checkbox.
            let c = Vec2::new(r.x + 17.0, r.center().y);
            if t.done {
                cv.circle(c, 8.0, th.ok);
                cv.icon(
                    Icon::Check,
                    Rect::new(c.x - 6.0, c.y - 6.0, 12.0, 12.0),
                    th.on_accent,
                );
            } else {
                cv.push(DrawCmd::Ring {
                    center: c,
                    radius: 7.0,
                    width: 1.6,
                    start_deg: 0.0,
                    sweep_deg: 360.0,
                    color: if hovered { th.text_dim } else { th.text_faint },
                });
            }
            cv.hit(
                Rect::new(r.x + 4.0, r.y, 28.0, r.h),
                hit_check(vi),
                CursorKind::Hand,
            );
            // Title, session count, delete.
            let mut tr = r.right() - 8.0;
            if hovered {
                let b = Rect::new(tr - 20.0, r.center().y - 10.0, 20.0, 20.0);
                cv.icon_button(b, Icon::Close, hit_del(vi), th.text_dim, None);
                tr -= 24.0;
            }
            if t.sessions > 0 {
                cv.circle(Vec2::new(tr - 4.0, r.center().y), 3.0, th.accent);
                cv.text(
                    Rect::new(tr - 30.0, r.y, 22.0, r.h),
                    t.sessions.to_string(),
                    TextStyle::caption().align(Align::End).tabular(),
                    th.text_dim,
                );
                tr -= 34.0;
            }
            cv.text(
                Rect::new(r.x + 32.0, r.y, (tr - r.x - 32.0).max(0.0), r.h),
                Text::from(t.title.clone()),
                TextStyle::body(),
                if t.done { th.text_faint } else { th.text },
            );
            cv.hit(
                Rect::new(r.x + 32.0, r.y, (tr - r.x - 32.0).max(0.0), r.h),
                hit_row(vi),
                CursorKind::Hand,
            );
        }
        // The entry row at the bottom.
        let shown = order.len().saturating_sub(scroll).min(rows);
        let y = list_top + shown as f32 * ROW_H + 2.0;
        let field = Rect::new(area.x, y, area.w, 26.0);
        if let Some(buf) = &self.editing {
            cv.round_rect(field, 9.0, th.surface_hi);
            cv.text(
                Rect::new(field.x + 10.0, field.y, field.w - 16.0, field.h),
                format!("{}|", tail_chars(buf, FIELD_CHARS)),
                TextStyle::body(),
                th.text,
            );
        } else if order.len() < crate::todos::MAX_TODOS {
            let hot = self.hover == Some(HIT_ADD);
            cv.round_rect(field, 9.0, if hot { th.surface_hi } else { th.surface });
            cv.icon(
                Icon::Plus,
                Rect::new(field.x + 8.0, field.y + 5.0, 16.0, 16.0),
                th.text_dim,
            );
            cv.text(
                Rect::new(field.x + 28.0, field.y, field.w - 34.0, field.h),
                "Add task",
                TextStyle::body(),
                th.text_dim,
            );
            cv.hit(field, HIT_ADD, CursorKind::Hand);
        }
    }
}

impl Module for Pomodoro {
    fn id(&self) -> ModuleId {
        "pomodoro"
    }

    fn title(&self) -> &'static str {
        "Focus"
    }

    fn icon(&self) -> Icon {
        Icon::Timer
    }

    fn subscriptions(&self) -> EventMask {
        EventMask::of(&[Kind::StoreLoaded])
    }

    /// Keeps the countdown fresh while the page is open. (Honoured only while expanded.)
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
        self.timer.running().then_some(CHIP_W)
    }

    fn chip_priority(&self) -> i32 {
        40
    }

    fn on_start(&mut self, cx: &mut Cx) {
        cx.command(Command::Store(StoreCmd::Load(KEY)));
    }

    fn next_wake(&self, now: f64, env: &Env) -> Option<f64> {
        let end = self.timer.next_change()?;
        let rem = end - env.unix;
        if rem <= 0 {
            return Some(now);
        }
        // The phase ends, or the chip's minute count turns over - whichever is first.
        Some(now + secs_to_chip_change(rem).min(rem) as f64)
    }

    fn on_tick(&mut self, cx: &mut Cx) {
        if let Some(f) = self.timer.tick(cx.env.unix) {
            self.on_finished(f, true, cx);
        }
        cx.request_redraw();
    }

    fn on_event(&mut self, ev: &Event, cx: &mut Cx) {
        if let EventKind::StoreLoaded(item) = &ev.kind
            && &*item.key == KEY
            && let Some(data) = &item.data
        {
            self.load(data, cx);
        }
    }

    fn on_config(&mut self, cfg: &Config, cx: &mut Cx) {
        self.cfg = cfg.pomodoro.clone();
        self.timer
            .set_settings(settings_from(&self.cfg), cx.env.unix);
        cx.request_redraw();
    }

    fn on_visibility(&mut self, v: Visibility, cx: &mut Cx) {
        if v != Visibility::Expanded {
            self.hover = None;
            // Leaving the page ends any typing (keeping what was typed).
            self.finish_editing(true, cx);
        }
    }

    fn on_suspend(&mut self, cx: &mut Cx) {
        self.suspended = true;
        self.hover = None;
        self.finish_editing(true, cx);
    }

    fn on_resume(&mut self, cx: &mut Cx) {
        self.suspended = false;
        if let Some(f) = self.unseen.take() {
            // A session ended while a game was in front: say so now, quietly.
            self.notice = Some(f);
            cx.peek(f64::from(self.cfg.peek_secs));
            cx.request_redraw();
        }
    }

    fn on_poll(&mut self, cx: &mut Cx) {
        cx.request_redraw();
    }

    fn on_input(&mut self, hit: Option<HitId>, input: &Input, cx: &mut Cx) -> bool {
        // Typing first: while the keyboard is ours these are the only inputs that matter.
        if self.editing.is_some() {
            match input {
                Input::Char(c) => {
                    if let Some(buf) = self.editing.as_mut()
                        && !c.is_control()
                        && buf.chars().count() < MAX_TITLE
                    {
                        buf.push(*c);
                        cx.request_redraw();
                    }
                    return true;
                }
                Input::Text(s) => {
                    if let Some(buf) = self.editing.as_mut() {
                        for c in s.chars() {
                            // Line breaks and tabs in pasted text become spaces; other controls go.
                            let c = if c.is_whitespace() { ' ' } else { c };
                            if c.is_control() {
                                continue;
                            }
                            if buf.chars().count() >= MAX_TITLE {
                                break;
                            }
                            buf.push(c);
                        }
                        cx.request_redraw();
                    }
                    return true;
                }
                Input::Key(Key::Backspace) => {
                    if let Some(buf) = self.editing.as_mut() {
                        buf.pop();
                        cx.request_redraw();
                    }
                    return true;
                }
                Input::Key(Key::Enter) => {
                    self.finish_editing(true, cx);
                    return true;
                }
                Input::Key(Key::Escape) => {
                    self.finish_editing(false, cx);
                    return true;
                }
                Input::FocusLost => {
                    self.finish_editing(true, cx);
                    return true;
                }
                _ => {}
            }
        }
        let now = Self::unix(cx.env);
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
            Input::Wheel { dy, .. } => {
                let over_list = hit.and_then(part_of).is_some() || hit == Some(HIT_ADD);
                if self.max_scroll() == 0 || !over_list {
                    return false;
                }
                let next = if *dy > 0.0 {
                    self.scroll.saturating_sub(1)
                } else {
                    (self.scroll + 1).min(self.max_scroll())
                };
                if next != self.scroll {
                    self.scroll = next;
                    cx.request_redraw();
                }
                true
            }
            Input::Click(_) => {
                let Some(h) = hit else { return false };
                match h {
                    HIT_PLAY => {
                        self.timer.toggle(now);
                        self.save(cx);
                    }
                    HIT_SKIP => {
                        let f = self.timer.skip(now);
                        self.on_finished(f, false, cx);
                    }
                    HIT_RESET => {
                        self.timer.reset();
                        self.save(cx);
                    }
                    HIT_ADD => {
                        self.editing = Some(String::new());
                        cx.request_keyboard(true);
                    }
                    HIT_CLEAR_DONE => {
                        self.todos.clear_done();
                        self.scroll = self.scroll.min(self.max_scroll());
                        self.save(cx);
                    }
                    _ => {
                        let order = self.todos.display_order();
                        let rows = VISIBLE_ROWS;
                        let scroll = self.scroll.min(order.len().saturating_sub(rows));
                        let id_at =
                            |vi: usize| order.get(scroll + vi).map(|&i| self.todos.items()[i].id);
                        match part_of(h) {
                            Some(Part::Check(vi)) => {
                                if let Some(id) = id_at(vi) {
                                    self.todos.toggle(id);
                                    self.save(cx);
                                }
                            }
                            Some(Part::Row(vi)) => {
                                if let Some(id) = id_at(vi) {
                                    self.todos.select(id);
                                    self.save(cx);
                                }
                            }
                            Some(Part::Del(vi)) => {
                                if let Some(id) = id_at(vi) {
                                    self.todos.remove(id);
                                    self.scroll = self.scroll.min(self.max_scroll());
                                    self.save(cx);
                                }
                            }
                            None => return false,
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
        if matches!(input, Input::Click(_)) && hit == Some(HIT_START_NEXT) {
            self.timer.start(Self::unix(cx.env));
            self.notice = None;
            self.save(cx);
            cx.request_redraw();
            return true;
        }
        false
    }

    fn draw_chip(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let th = *cv.theme;
        let rem = self.timer.remaining(Self::unix(dx.env));
        let color = Self::phase_color(&th, self.timer.phase());
        let icon = Rect::new(area.x, area.center().y - 8.0, 16.0, 16.0);
        cv.icon(Icon::Timer, icon, color);
        cv.text(
            Rect::new(
                icon.right() + 3.0,
                area.y,
                area.right() - icon.right() - 3.0,
                area.h,
            ),
            format!("{}m", chip_minutes(rem)),
            TextStyle::new(12.0, Weight::SemiBold).tabular(),
            th.text,
        );
    }

    fn draw_peek(&mut self, cv: &mut Canvas, area: Rect, _dx: &DrawCx) {
        let th = *cv.theme;
        let Some(f) = self.notice else { return };
        let tile = Rect::new(area.x, area.y, area.h, area.h);
        cv.squircle(tile, 8.0, th.surface_hi);
        cv.icon(
            if f.phase.is_break() {
                Icon::Timer
            } else {
                Icon::Check
            },
            tile.inset(tile.w * 0.22),
            Self::phase_color(&th, f.phase),
        );
        let start_btn = !f.started_next;
        let btn_w = 62.0;
        let x = tile.right() + 12.0;
        let w = (area.right() - x - if start_btn { btn_w + 8.0 } else { 0.0 }).max(0.0);
        let line = |y: f32, h: f32| Rect::new(x, area.y + y, w, h);
        let title = match f.phase {
            Phase::Focus => "Focus session complete",
            Phase::ShortBreak | Phase::LongBreak => "Break's over",
        };
        let mins = self.timer.total() / 60;
        let next = match f.next {
            Phase::Focus => format!("Back to focus · {mins} min"),
            p => format!("Time for a {mins} min {}", p.label().to_lowercase()),
        };
        cv.text(line(0.0, 15.0), "Pomodoro", TextStyle::caption(), th.accent);
        cv.text(
            line(15.0, 18.0),
            title,
            TextStyle::new(13.0, Weight::SemiBold),
            th.text,
        );
        cv.text(line(33.0, 15.0), next, TextStyle::caption(), th.text_dim);
        if start_btn {
            let b = Rect::new(
                area.right() - btn_w,
                area.y + (area.h - 26.0) * 0.5,
                btn_w,
                26.0,
            );
            cv.capsule(b, th.accent);
            cv.text(
                b,
                "Start",
                TextStyle::new(12.0, Weight::SemiBold).align(Align::Center),
                th.on_accent,
            );
            cv.hit(b, HIT_START_NEXT, CursorKind::Hand);
        }
    }

    fn draw_expanded(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let (left, right) = (
            Rect::new(area.x, area.y, 140.0, area.h),
            Rect::new(area.x + 160.0, area.y, (area.w - 160.0).max(0.0), area.h),
        );
        self.draw_timer(cv, left, dx.env);
        self.draw_tasks(cv, right);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::civil::{LocalTime, unix_from_civil};
    use crate::draw::DrawList;
    use crate::module::{Out, ShellRequest};

    fn env_at(unix_offset: i64) -> Env {
        let base = unix_from_civil(2026, 10, 6, 14, 0, 0);
        let (y, mo, d, h, mi, s) = crate::civil::civil_from_unix(base + unix_offset);
        Env {
            local: LocalTime::new(y, mo, d, h, mi, s),
            unix: base + unix_offset,
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
        m: Pomodoro,
        theme: Theme,
        cfg: Config,
        env: Env,
        out: Out,
        now: f64,
    }

    impl T {
        fn new() -> T {
            let cfg = PomodoroCfg {
                focus_minutes: 1.0,
                short_break_minutes: 0.5,
                long_break_minutes: 1.0,
                long_break_every: 2,
                auto_start_breaks: false,
                ..PomodoroCfg::default()
            };
            T {
                m: Pomodoro::new(cfg.clone()),
                theme: Theme::default(),
                cfg: Config {
                    pomodoro: cfg,
                    ..Config::default()
                },
                env: env_at(0),
                out: Out::default(),
                now: 100.0,
            }
        }
        fn at(&mut self, secs: i64) {
            self.now += (secs - (self.env.unix - env_at(0).unix)) as f64;
            self.env = env_at(secs);
        }
        fn click(&mut self, hit: HitId) -> bool {
            self.input(Some(hit), Input::Click(Vec2::ZERO))
        }
        fn input(&mut self, hit: Option<HitId>, i: Input) -> bool {
            let mut cx = cx!(self);
            self.m.on_input(hit, &i, &mut cx)
        }
        fn tick(&mut self) {
            let mut cx = cx!(self);
            self.m.on_tick(&mut cx);
        }
        fn peek_input(&mut self, hit: Option<HitId>, i: Input) -> bool {
            let mut cx = cx!(self);
            self.m.on_peek_input(hit, &i, &mut cx)
        }
        fn type_text(&mut self, s: &str) {
            for c in s.chars() {
                self.input(None, Input::Char(c));
            }
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
        fn saved_json(&self) -> Option<String> {
            self.out.commands.iter().rev().find_map(|c| match c {
                Command::Store(StoreCmd::Save { data, .. }) => Some(data.to_string()),
                _ => None,
            })
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
    fn settings_come_from_the_configuration() {
        let s = settings_from(&PomodoroCfg::default());
        assert_eq!(
            (s.focus, s.short_break, s.long_break, s.long_every),
            (1500, 300, 900, 4)
        );
        let s = settings_from(&PomodoroCfg {
            focus_minutes: 0.05,
            ..PomodoroCfg::default()
        });
        assert_eq!(
            s.focus, 3,
            "fractions of a minute work (the self-test uses them)"
        );
    }

    #[test]
    fn starting_shows_a_chip_and_schedules_the_end() {
        let mut t = T::new();
        assert_eq!(t.m.chip_width(), None, "an idle timer has no chip");
        assert_eq!(t.m.next_wake(t.now, &t.env), None);
        assert!(t.click(HIT_PLAY));
        assert_eq!(t.m.chip_width(), Some(CHIP_W));
        // 60 s focus: the chip's minute count (1m) holds until the end.
        let wake = t.m.next_wake(t.now, &t.env).unwrap();
        assert!((wake - t.now - 60.0).abs() < 1e-6);
        t.at(10);
        let wake = t.m.next_wake(t.now, &t.env).unwrap();
        assert!((wake - t.now - 50.0).abs() < 1e-6);
        // Pause: the chip and the wake-up go away and the state is saved.
        assert!(t.click(HIT_PLAY));
        assert_eq!(t.m.chip_width(), None);
        assert_eq!(t.m.next_wake(t.now, &t.env), None);
        assert!(t.saved_json().is_some());
    }

    #[test]
    fn a_long_focus_session_wakes_each_minute_for_the_chip() {
        let mut t = T::new();
        t.m.timer.set_settings(
            Settings {
                focus: 1500,
                ..*t.m.timer.settings()
            },
            0,
        );
        t.click(HIT_PLAY);
        let wake = t.m.next_wake(t.now, &t.env).unwrap();
        assert!((wake - t.now - 60.0).abs() < 1e-6, "{}", wake - t.now);
        t.at(60);
        let wake = t.m.next_wake(t.now, &t.env).unwrap();
        assert!((wake - t.now - 60.0).abs() < 1e-6);
        let chip = {
            let mut list = DrawList::new();
            let mut cv = Canvas::new(&mut list, &t.theme);
            let dx = DrawCx {
                now: t.now,
                env: &t.env,
                config: &t.cfg,
            };
            t.m.draw_chip(&mut cv, Rect::new(0.0, 0.0, CHIP_W, 24.0), &dx);
            texts(&list)
        };
        assert_eq!(chip, vec!["24m".to_string()]);
    }

    #[test]
    fn when_the_session_ends_it_chimes_announces_credits_the_task_and_saves() {
        let mut t = T::new();
        t.click(HIT_ADD);
        t.type_text("write the docs");
        t.input(None, Input::Key(Key::Enter));
        let id = t.m.todos().items()[0].id;
        // Select the task via its row (visible index 0).
        assert!(t.click(hit_row(0)));
        assert_eq!(t.m.todos().current_id(), Some(id));
        t.click(HIT_PLAY);
        t.out = Out::default();
        t.at(60);
        t.tick();
        assert_eq!(t.m.timer().phase(), Phase::ShortBreak);
        assert!(
            !t.m.timer().running(),
            "auto_start_breaks is off in this fixture"
        );
        assert!(t.out.commands.contains(&Command::Chime));
        assert!(matches!(
            &t.out.shell[..],
            [ShellRequest::Peek { module: "test", .. }]
        ));
        assert_eq!(
            t.m.todos().items()[0].sessions,
            1,
            "credited to the current task"
        );
        assert_eq!(t.m.sessions_today(&t.env), 1);
        let json = t.saved_json().unwrap();
        assert!(json.contains("\"sessions\":1"), "{json}");
        // Nothing more happens on the next tick.
        t.out = Out::default();
        t.tick();
        assert!(t.out.commands.is_empty() && t.out.shell.is_empty());
    }

    #[test]
    fn the_banner_offers_to_start_the_next_phase_and_the_button_does() {
        let mut t = T::new();
        t.click(HIT_PLAY);
        t.at(60);
        t.tick();
        let mut list = DrawList::new();
        {
            let mut cv = Canvas::new(&mut list, &t.theme);
            let dx = DrawCx {
                now: t.now,
                env: &t.env,
                config: &t.cfg,
            };
            t.m.draw_peek(&mut cv, Rect::new(0.0, 0.0, 296.0, 48.0), &dx);
        }
        let tx = texts(&list);
        assert!(tx.contains(&"Focus session complete".to_string()), "{tx:?}");
        assert!(tx.iter().any(|s| s.starts_with("Time for a 0 min short break") || s.contains("short break")), "{tx:?}");
        assert!(tx.contains(&"Start".to_string()));
        assert!(list.hits.iter().any(|h| h.id == HIT_START_NEXT));
        assert!(
            !t.peek_input(None, Input::Click(Vec2::ZERO)),
            "elsewhere: the shell opens the page"
        );
        assert!(t.peek_input(Some(HIT_START_NEXT), Input::Click(Vec2::ZERO)));
        assert!(t.m.timer().running());
    }

    #[test]
    fn auto_started_breaks_run_by_themselves_and_the_banner_has_no_button() {
        let mut t = T::new();
        t.m.cfg.auto_start_breaks = true;
        t.m.timer.set_settings(settings_from(&t.m.cfg), 0);
        t.click(HIT_PLAY);
        t.at(60);
        t.tick();
        assert!(t.m.timer().running());
        assert_eq!(t.m.timer().phase(), Phase::ShortBreak);
        let mut list = DrawList::new();
        let mut cv = Canvas::new(&mut list, &t.theme);
        let dx = DrawCx {
            now: t.now,
            env: &t.env,
            config: &t.cfg,
        };
        t.m.draw_peek(&mut cv, Rect::new(0.0, 0.0, 296.0, 48.0), &dx);
        assert!(list.hits.is_empty(), "nothing to click: it already started");
    }

    #[test]
    fn nothing_chimes_or_pops_up_while_a_game_is_in_front_and_it_is_announced_after() {
        let mut t = T::new();
        t.click(HIT_PLAY);
        {
            let mut cx = cx!(t);
            t.m.on_suspend(&mut cx);
        }
        t.out = Out::default();
        t.at(60);
        t.tick();
        assert!(t.out.shell.is_empty());
        assert!(
            !t.out.commands.contains(&Command::Chime),
            "no sound over a game"
        );
        assert_eq!(
            t.m.timer().phase(),
            Phase::ShortBreak,
            "the timer itself kept time"
        );
        {
            let mut cx = cx!(t);
            t.m.on_resume(&mut cx);
        }
        assert_eq!(t.out.shell.len(), 1, "announced once the shell is back");
    }

    #[test]
    fn skipping_does_not_credit_or_chime() {
        let mut t = T::new();
        t.click(HIT_ADD);
        t.type_text("a");
        t.input(None, Input::Key(Key::Enter));
        t.click(hit_row(0));
        t.click(HIT_PLAY);
        t.out = Out::default();
        t.click(HIT_SKIP);
        assert_eq!(t.m.todos().items()[0].sessions, 0);
        assert!(!t.out.commands.contains(&Command::Chime));
        assert!(t.out.shell.is_empty());
        assert_eq!(t.m.timer().phase(), Phase::ShortBreak);
    }

    #[test]
    fn reset_restarts_the_phase_without_running() {
        let mut t = T::new();
        t.click(HIT_PLAY);
        t.at(30);
        t.click(HIT_RESET);
        assert!(!t.m.timer().running());
        assert_eq!(t.m.timer().remaining(t.env.unix), 60);
    }

    #[test]
    fn adding_a_task_takes_and_gives_back_the_keyboard() {
        let mut t = T::new();
        assert!(t.click(HIT_ADD));
        assert_eq!(t.out.keyboard, Some(("test", true)));
        t.out = Out::default();
        t.type_text("  buy milkk");
        let tx = texts(&t.draw());
        assert!(
            tx.iter().any(|s| s == "  buy milkk|"),
            "the typed text with a caret: {tx:?}"
        );
        t.input(None, Input::Key(Key::Backspace));
        assert!(t.input(None, Input::Key(Key::Enter)));
        assert_eq!(t.out.keyboard, Some(("test", false)));
        assert_eq!(t.m.todos().items().len(), 1);
        assert_eq!(t.m.todos().items()[0].title, "buy milk");
        assert!(t.saved_json().unwrap().contains("buy milk"));
        // Escape discards.
        t.click(HIT_ADD);
        t.type_text("nope");
        t.input(None, Input::Key(Key::Escape));
        assert_eq!(t.m.todos().items().len(), 1);
        assert_eq!(t.out.keyboard, Some(("test", false)));
    }

    #[test]
    fn typing_is_bounded_and_pasting_is_cleaned() {
        let mut t = T::new();
        t.click(HIT_ADD);
        t.input(None, Input::Text("line one\nline two\u{7}".into()));
        t.type_text(&"x".repeat(500));
        t.input(None, Input::Key(Key::Enter));
        let title = t.m.todos().items()[0].title.clone();
        assert!(title.starts_with("line one line two"), "{title}");
        assert!(title.chars().count() <= MAX_TITLE);
        // Control characters typed directly are ignored too.
        t.click(HIT_ADD);
        t.input(None, Input::Char('\u{8}'));
        t.input(None, Input::Key(Key::Enter));
        assert_eq!(t.m.todos().items().len(), 1, "an empty entry adds nothing");
    }

    #[test]
    fn losing_focus_or_leaving_the_page_keeps_what_was_typed() {
        let mut t = T::new();
        t.click(HIT_ADD);
        t.type_text("kept");
        t.input(None, Input::FocusLost);
        assert_eq!(t.m.todos().items()[0].title, "kept");
        assert_eq!(t.out.keyboard, Some(("test", false)));
        t.click(HIT_ADD);
        t.type_text("also kept");
        let mut cx = cx!(t);
        t.m.on_visibility(Visibility::Hidden, &mut cx);
        assert_eq!(t.m.todos().items().len(), 2);
    }

    #[test]
    fn tasks_can_be_ticked_selected_and_deleted_from_the_list() {
        let mut t = T::new();
        for name in ["one", "two", "three"] {
            t.click(HIT_ADD);
            t.type_text(name);
            t.input(None, Input::Key(Key::Enter));
        }
        let list = t.draw();
        assert!(list.hits.iter().any(|h| h.id == hit_check(2)));
        t.click(hit_check(0)); // tick "one": it moves to the bottom
        let order: Vec<String> =
            t.m.todos()
                .display_order()
                .into_iter()
                .map(|i| t.m.todos().items()[i].title.clone())
                .collect();
        assert_eq!(order, vec!["two", "three", "one"]);
        t.click(hit_row(0)); // select "two"
        assert_eq!(t.m.todos().current().map(|x| x.title.as_str()), Some("two"));
        t.click(hit_del(0)); // delete "two"
        assert_eq!(t.m.todos().items().len(), 2);
        assert_eq!(t.m.todos().current_id(), None);
        t.click(HIT_CLEAR_DONE);
        assert_eq!(t.m.todos().items().len(), 1);
    }

    #[test]
    fn the_page_shows_time_phase_tasks_and_counters() {
        let mut t = T::new();
        t.click(HIT_ADD);
        t.type_text("write");
        t.input(None, Input::Key(Key::Enter));
        t.click(hit_row(0));
        t.click(HIT_PLAY);
        t.at(15);
        let tx = texts(&t.draw());
        assert!(tx.contains(&"00:45".to_string()), "{tx:?}");
        assert!(tx.contains(&"Focus".to_string()));
        assert!(tx.contains(&"Tasks".to_string()));
        assert!(tx.contains(&"1 open".to_string()));
        assert!(tx.contains(&"write".to_string()));
        assert!(tx.contains(&"Add task".to_string()));
        t.click(HIT_PLAY);
        assert!(texts(&t.draw()).contains(&"Focus · paused".to_string()));
        let mut t2 = T::new();
        assert!(texts(&t2.draw()).contains(&"No tasks yet".to_string()));
    }

    #[test]
    fn the_task_list_scrolls_when_it_is_long() {
        let mut t = T::new();
        for i in 0..8 {
            t.click(HIT_ADD);
            t.type_text(&format!("task {i}"));
            t.input(None, Input::Key(Key::Enter));
        }
        let first = |t: &mut T| {
            texts(&t.draw())
                .into_iter()
                .find(|s| s.starts_with("task "))
                .unwrap()
        };
        // Adding scrolls to the newest; wheel up reveals earlier ones.
        assert_eq!(t.m.scroll, 3);
        assert_eq!(first(&mut t), "task 3");
        t.input(
            Some(hit_row(0)),
            Input::Wheel {
                pos: Vec2::ZERO,
                dx: 0.0,
                dy: 120.0,
            },
        );
        assert_eq!(first(&mut t), "task 2");
        for _ in 0..10 {
            t.input(
                Some(hit_row(0)),
                Input::Wheel {
                    pos: Vec2::ZERO,
                    dx: 0.0,
                    dy: 120.0,
                },
            );
        }
        assert_eq!(first(&mut t), "task 0");
        assert!(
            !t.input(
                None,
                Input::Wheel {
                    pos: Vec2::ZERO,
                    dx: 0.0,
                    dy: 120.0
                }
            ),
            "elsewhere the wheel switches pages"
        );
    }

    #[test]
    fn state_round_trips_through_the_store_and_a_running_timer_resumes() {
        let mut t = T::new();
        t.click(HIT_ADD);
        t.type_text("persist me");
        t.input(None, Input::Key(Key::Enter));
        t.click(hit_row(0));
        t.click(HIT_PLAY);
        t.at(20);
        t.click(HIT_PLAY); // pause with 40 s left
        t.click(HIT_PLAY); // resume
        let json = t.saved_json().unwrap();

        let mut u = T::new();
        u.at(30);
        let ev = Event::new(
            crate::events::Source::Local,
            EventKind::StoreLoaded(crate::events::StoreItem {
                key: KEY.into(),
                data: Some(json.into()),
            }),
        );
        let mut cx = cx!(u);
        u.m.on_event(&ev, &mut cx);
        assert_eq!(u.m.todos().items()[0].title, "persist me");
        assert_eq!(
            u.m.todos().current().map(|x| x.title.as_str()),
            Some("persist me")
        );
        assert!(u.m.timer().running(), "it was running when saved");
        assert_eq!(
            u.m.timer().remaining(u.env.unix),
            30,
            "resumed at t=20 with 40 s left: 30 s remain at t=30"
        );
    }

    #[test]
    fn a_missing_or_damaged_save_is_ignored() {
        let mut t = T::new();
        for data in [None, Some("not json"), Some("{}"), Some("{\"todos\":5}")] {
            let ev = Event::new(
                crate::events::Source::Local,
                EventKind::StoreLoaded(crate::events::StoreItem {
                    key: KEY.into(),
                    data: data.map(Into::into),
                }),
            );
            let mut cx = cx!(t);
            t.m.on_event(&ev, &mut cx);
        }
        assert!(t.m.todos().items().is_empty());
        // Another module's key is none of our business.
        let ev = Event::new(
            crate::events::Source::Local,
            EventKind::StoreLoaded(crate::events::StoreItem {
                key: "other".into(),
                data: Some("{\"todos\":{\"items\":[{\"id\":1,\"title\":\"x\",\"done\":false,\"sessions\":0}],\"next_id\":2,\"current\":null}}".into()),
            }),
        );
        let mut cx = cx!(t);
        t.m.on_event(&ev, &mut cx);
        assert!(t.m.todos().items().is_empty());
    }

    #[test]
    fn it_asks_for_its_saved_state_on_start() {
        let mut t = T::new();
        let mut cx = cx!(t);
        t.m.on_start(&mut cx);
        assert_eq!(
            t.out.commands,
            vec![Command::Store(StoreCmd::Load("pomodoro"))]
        );
    }

    #[test]
    fn identical_state_is_not_saved_twice() {
        let mut t = T::new();
        t.click(HIT_RESET);
        let first = t.out.commands.len();
        t.click(HIT_RESET);
        assert_eq!(t.out.commands.len(), first, "no change, no write");
    }

    #[test]
    fn changing_the_configuration_updates_idle_timers() {
        let mut t = T::new();
        let mut cfg = Config::default();
        cfg.pomodoro.focus_minutes = 50.0;
        let mut cx = cx!(t);
        t.m.on_config(&cfg, &mut cx);
        assert_eq!(t.m.timer().remaining(t.env.unix), 3000);
    }

    #[test]
    fn it_works_inside_the_host() {
        use crate::module::ModuleHost;
        let mut cfg = Config::default();
        cfg.modules.order = vec!["pomodoro".into()];
        let mut host = ModuleHost::new(
            vec![crate::module::Factory {
                id: "pomodoro",
                create,
            }],
            Arc::new(cfg),
            Theme::default(),
        );
        host.set_context(10.0, env_at(0));
        host.start_new();
        let out = host.take_out();
        assert!(
            out.commands
                .contains(&Command::Store(StoreCmd::Load("pomodoro")))
        );
        assert_eq!(host.pages().len(), 1);
        assert_eq!(host.chips_width(), 0.0);
        assert_eq!(
            host.next_deadline(),
            None,
            "an idle timer needs no wake-ups"
        );
    }
}
