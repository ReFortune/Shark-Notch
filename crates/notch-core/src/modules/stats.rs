//! System stats: processor, memory, GPU and network with a minute of history each, and the battery.
//!
//! This page is the only thing that causes any measuring. The host polls a module only while its
//! page is on screen, and each poll just *asks* the platform for a reading (`StatsCmd::Sample`);
//! with the page closed nothing runs, and the history is dropped so that reopening starts clean
//! instead of showing a stale minute with a hole in it.

use std::sync::Arc;
use std::time::Duration;

use crate::aiusage::Totals;
use crate::claudelimits::{LimitsState, Window, fmt_until, fmt_until_short};
use crate::color::Color;
use crate::config::{Config, StatsCfg};
use crate::draw::{Align, Canvas, Text, TextStyle, Weight};
use crate::events::{BtDevice, Event, EventKind, EventMask, Kind, PowerStatus, StatsSnapshot};
use crate::geom::{Rect, Size};
use crate::icons::Icon;
use crate::module::{Command, Cx, DrawCx, Module, ModuleId, StatsCmd, Visibility};
use crate::stats::{History, fmt_memory, fmt_rate, fmt_remaining};

/// Samples shown per chart (a minute at the default one reading per second).
const SLOTS: usize = 60;
const GAP: f32 = 8.0;
const BATTERY_H: f32 = 30.0;
/// Bluetooth devices named in the strip (the rest are left out).
const MAX_DEVICES: usize = 3;
/// The network chart never zooms in on background noise: its scale is at least this (bytes/s).
const NET_FLOOR: f32 = 128.0 * 1024.0;
/// How long the plug-in / full-charge banner stays.
const BANNER_SECS: f64 = 3.0;

pub fn create(cfg: &Config) -> Option<Box<dyn Module>> {
    cfg.stats
        .enabled
        .then(|| Box::new(Stats::new(cfg.stats.clone())) as Box<dyn Module>)
}

/// What the short banner announces.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Banner {
    Plugged(PowerStatus),
    Unplugged(PowerStatus),
    Full(PowerStatus),
    /// A Claude limit is nearly used up: `weekly` or the 5-hour one, how much, when it starts over.
    Limit {
        weekly: bool,
        used: f32,
        resets_at: i64,
    },
}

pub struct Stats {
    cfg: StatsCfg,
    cpu: History,
    mem: History,
    gpu: History,
    down: History,
    up: History,
    latest: Option<Arc<StatsSnapshot>>,
    /// The last power reading Windows announced (the baseline for the plug-in / full banners).
    power: Option<PowerStatus>,
    banner: Option<Banner>,
    /// Claude Code's token use today, as last reported, and whether it is working right now.
    ai: Option<Totals>,
    /// Claude's plan limits, as last read (opt-in; asked for when Claude Code starts working).
    limits: Option<Arc<LimitsState>>,
    /// Per window (5 hours, week): the level already announced (0, 80 %, 95 %) and its reset time.
    warned: [(u8, i64); 2],
    /// Polls since the limits were last asked for (while this page is open).
    limits_polls: u32,
    suspended: bool,
}

/// What a tile shows besides its chart.
struct Tile<'a> {
    label: &'static str,
    /// Small line under the label.
    sub: Text,
    /// Large value at the right ("37%").
    value: Text,
    color: Color,
    /// Value-less tiles ("not available") draw no chart.
    chart: Option<(&'a [f32], (f32, f32))>,
}

impl Stats {
    pub fn new(cfg: StatsCfg) -> Stats {
        Stats {
            cfg,
            cpu: History::new(SLOTS),
            mem: History::new(SLOTS),
            gpu: History::new(SLOTS),
            down: History::new(SLOTS),
            up: History::new(SLOTS),
            latest: None,
            power: None,
            banner: None,
            ai: None,
            limits: None,
            warned: [(0, 0); 2],
            limits_polls: 0,
            suspended: false,
        }
    }

    /// Windows announced a new power source or level: say so when the charger came or went, or the
    /// battery just reached full. The first announcement (Windows sends the current state when the
    /// notification is registered) is only the baseline.
    fn on_power(&mut self, p: PowerStatus, cx: &mut Cx) {
        let before = self.power.replace(p);
        let Some(before) = before.filter(|_| self.cfg.battery_hud && !self.suspended) else {
            return;
        };
        let banner = if p.plugged != before.plugged {
            Some(if p.plugged {
                Banner::Plugged(p)
            } else {
                Banner::Unplugged(p)
            })
        } else if p.plugged && p.battery.percent >= 100 && before.battery.percent < 100 {
            Some(Banner::Full(p))
        } else {
            None
        };
        if let Some(b) = banner {
            self.banner = Some(b);
            cx.peek(BANNER_SECS);
            cx.request_redraw();
        }
    }

    /// Is Claude Code working (its logs grew within the last minute and a half)?
    fn ai_working(&self, unix: i64) -> bool {
        self.ai
            .is_some_and(|t| t.last_at > 0 && unix - t.last_at < Self::AI_ACTIVE_SECS)
    }

    /// The limits just arrived: say so when one has crossed 80 % or 95 % since it was last looked
    /// at. Each level is announced once per window; a window that starts over begins again.
    fn check_limits(&mut self, cx: &mut Cx) {
        let Some(LimitsState::Known(l)) = self.limits.as_deref().cloned() else {
            return;
        };
        let mut best: Option<Banner> = None;
        for (i, win) in [l.five_hour, l.seven_day].into_iter().enumerate() {
            let Some(w) = win else { continue };
            let level = if w.used >= 95.0 {
                2
            } else {
                u8::from(w.used >= 80.0)
            };
            let (seen, seen_reset) = self.warned[i];
            let fresh_window = seen_reset != 0 && w.resets_at != 0 && w.resets_at != seen_reset;
            let before = if fresh_window { 0 } else { seen };
            self.warned[i] = (level, w.resets_at);
            if level > before {
                let b = Banner::Limit {
                    weekly: i == 1,
                    used: w.used,
                    resets_at: w.resets_at,
                };
                if best.is_none_or(|x| matches!(x, Banner::Limit { used, .. } if used < w.used)) {
                    best = Some(b);
                }
            }
        }
        if let Some(b) = best.filter(|_| !self.suspended) {
            self.banner = Some(b);
            cx.peek(BANNER_SECS + 1.5);
            cx.request_redraw();
        }
    }

    fn forget(&mut self) {
        for h in [
            &mut self.cpu,
            &mut self.mem,
            &mut self.gpu,
            &mut self.down,
            &mut self.up,
        ] {
            h.clear();
        }
        self.latest = None;
    }

    fn mem_percent(s: &StatsSnapshot) -> Option<f32> {
        (s.mem_total > 0).then(|| (s.mem_used as f64 / s.mem_total as f64 * 100.0) as f32)
    }

    fn draw_tile(&self, cv: &mut Canvas, r: Rect, t: Tile, secondary: Option<(&[f32], Color)>) {
        let th = *cv.theme;
        let pad = 10.0;
        cv.squircle(r, 12.0, th.surface);
        let inner = r.w - 2.0 * pad;
        cv.text(
            Rect::new(r.x + pad, r.y + 8.0, inner * 0.5, 15.0),
            t.label,
            TextStyle::new(11.5, Weight::SemiBold),
            th.text_dim,
        );
        cv.text(
            Rect::new(r.x + pad, r.y + 23.0, inner, 14.0),
            t.sub,
            TextStyle::caption(),
            th.text_faint,
        );
        cv.text(
            Rect::new(r.x + pad, r.y + 5.0, inner, 24.0),
            t.value,
            TextStyle::new(18.0, Weight::SemiBold)
                .align(Align::End)
                .tabular(),
            th.text,
        );
        let chart = Rect::new(r.x + pad, r.y + 41.0, inner, (r.h - 41.0 - 9.0).max(0.0));
        if let Some((values, scale)) = t.chart
            && chart.h > 4.0
        {
            if let Some((other, c)) = secondary {
                cv.sparkline(chart, other, SLOTS, scale, c, 1.4, None);
            }
            cv.sparkline(
                chart,
                values,
                SLOTS,
                scale,
                t.color,
                1.6,
                Some(t.color.with_alpha(0.18)),
            );
        }
    }

    /// Height the optional strips (Bluetooth devices, Claude Code) add to the page.
    fn extra_height(&self) -> f32 {
        (BATTERY_H + GAP) * (usize::from(self.cfg.devices) + usize::from(self.cfg.ai_limits)) as f32
    }

    /// How long Claude Code counts as "working" after its logs last grew.
    const AI_ACTIVE_SECS: i64 = 90;

    /// "Claude 5-hour limit / 92% used · resets in 1 h 20 min" with a bar.
    fn draw_limit(
        &self,
        cv: &mut Canvas,
        area: Rect,
        weekly: bool,
        used: f32,
        resets_at: i64,
        unix: i64,
    ) {
        let th = *cv.theme;
        let color = if used >= 95.0 { th.danger } else { th.warn };
        let tile = Rect::new(area.x, area.y, area.h, area.h);
        cv.squircle(tile, 8.0, th.surface_hi);
        cv.icon(Icon::Pulse, tile.inset(tile.w * 0.22), color);
        let x = tile.right() + 12.0;
        let w = (area.right() - x).max(0.0);
        cv.text(
            Rect::new(x, area.y + 2.0, w, 18.0),
            if weekly {
                "Claude weekly limit"
            } else {
                "Claude 5-hour limit"
            },
            TextStyle::new(13.5, Weight::SemiBold),
            th.text,
        );
        let mut sub = format!("{used:.0}% used");
        if resets_at > 0 {
            sub.push_str(&format!(" · resets in {}", fmt_until(resets_at, unix)));
        }
        cv.text(
            Rect::new(x, area.y + 21.0, w, 15.0),
            sub,
            TextStyle::caption(),
            th.text_dim,
        );
        cv.bar(
            Rect::new(x, area.bottom() - 8.0, w, 5.0),
            used / 100.0,
            th.surface_hi,
            color,
        );
    }

    /// The Claude line on this page: the 5-hour and weekly limits as two bars, or why not.
    fn draw_ai(&self, cv: &mut Canvas, r: Rect, unix: i64) {
        let th = *cv.theme;
        cv.squircle(r, 10.0, th.surface);
        cv.text(
            Rect::new(r.x + 12.0, r.y, 86.0, r.h),
            "Claude Code",
            TextStyle::new(11.5, Weight::SemiBold),
            th.text_dim,
        );
        let body = Rect::new(r.x + 100.0, r.y, r.w - 110.0, r.h);
        let note = |cv: &mut Canvas, text: &'static str| {
            cv.text(body, text, TextStyle::caption(), th.text_faint);
        };
        match self.limits.as_deref() {
            Some(LimitsState::Known(l)) => {
                let w = body.w * 0.5;
                for (i, (label, win)) in [("5 h", l.five_hour), ("Week", l.seven_day)]
                    .into_iter()
                    .enumerate()
                {
                    self.draw_window(
                        cv,
                        Rect::new(body.x + i as f32 * w, r.y, w - 8.0, r.h),
                        label,
                        win,
                        unix,
                    );
                }
            }
            Some(LimitsState::SignedOut) => note(cv, "Sign in to Claude Code to see limits"),
            Some(LimitsState::Expired) => note(cv, "Login expired: use Claude Code once"),
            Some(LimitsState::Failed) => note(cv, "Limits unavailable right now"),
            None => note(cv, "…"),
        }
    }

    /// One limit: "5 h 42% · 2h10m" with a bar under it.
    fn draw_window(&self, cv: &mut Canvas, r: Rect, label: &str, win: Option<Window>, unix: i64) {
        let th = *cv.theme;
        let Some(w) = win else {
            cv.text(
                Rect::new(r.x, r.y + 2.0, r.w, 16.0),
                format!("{label} —"),
                TextStyle::caption(),
                th.text_faint,
            );
            return;
        };
        let color = if w.used >= 90.0 {
            th.danger
        } else if w.used >= 70.0 {
            th.warn
        } else {
            th.accent
        };
        let mut text = format!("{label} {:.0}%", w.used);
        if w.resets_at > 0 {
            text.push_str(&format!(" · {}", fmt_until_short(w.resets_at, unix)));
        }
        cv.text(
            Rect::new(r.x, r.y + 3.0, r.w, 15.0),
            text,
            TextStyle::caption().tabular(),
            th.text,
        );
        cv.bar(
            Rect::new(r.x, r.bottom() - 8.0, r.w, 4.0),
            w.used / 100.0,
            th.surface_hi,
            color,
        );
    }

    fn draw_devices(&self, cv: &mut Canvas, r: Rect, devices: Option<&[BtDevice]>) {
        let th = *cv.theme;
        cv.squircle(r, 10.0, th.surface);
        cv.text(
            Rect::new(r.x + 12.0, r.y, 58.0, r.h),
            "Devices",
            TextStyle::new(11.5, Weight::SemiBold),
            th.text_dim,
        );
        let list = devices.unwrap_or_default();
        if list.is_empty() {
            let note = if devices.is_some() {
                "No Bluetooth device connected"
            } else {
                "…"
            };
            cv.text(
                Rect::new(r.x + 70.0, r.y, r.w - 82.0, r.h),
                note,
                TextStyle::caption(),
                th.text_faint,
            );
            return;
        }
        let shown = &list[..list.len().min(MAX_DEVICES)];
        let w = (r.w - 70.0 - 8.0) / shown.len() as f32;
        for (i, d) in shown.iter().enumerate() {
            let x = r.x + 70.0 + i as f32 * w;
            let name: String = if d.name.chars().count() > 13 {
                d.name.chars().take(12).chain(['…']).collect()
            } else {
                d.name.to_string()
            };
            let text = match d.battery {
                Some(p) => format!("{name} {p}%"),
                None => name,
            };
            let low = d.battery.is_some_and(|p| p <= 20);
            cv.text(
                Rect::new(x, r.y, w - 4.0, r.h),
                text,
                TextStyle::caption(),
                if low { th.warn } else { th.text_dim },
            );
        }
    }

    fn draw_battery(&self, cv: &mut Canvas, r: Rect, power: Option<&PowerStatus>, have_data: bool) {
        let th = *cv.theme;
        cv.squircle(r, 10.0, th.surface);
        let mid = r.center().y;
        let Some(p) = power else {
            let note = if have_data {
                "No battery: this PC runs on mains power"
            } else {
                "Battery"
            };
            cv.text(
                Rect::new(r.x + 12.0, r.y, r.w - 24.0, r.h),
                note,
                TextStyle::caption(),
                th.text_faint,
            );
            return;
        };
        let color = if p.battery.charging || p.plugged {
            th.ok
        } else if p.battery.percent <= 10 {
            th.danger
        } else if p.battery.percent <= 20 {
            th.warn
        } else {
            th.text
        };
        cv.text(
            Rect::new(r.x + 12.0, r.y, 58.0, r.h),
            "Battery",
            TextStyle::new(11.5, Weight::SemiBold),
            th.text_dim,
        );
        cv.text(
            Rect::new(r.x + 70.0, r.y, 40.0, r.h),
            format!("{}%", p.battery.percent),
            TextStyle::new(14.0, Weight::SemiBold).tabular(),
            th.text,
        );
        let bar = Rect::new(r.x + 116.0, mid - 4.0, 96.0, 8.0);
        cv.bar(
            bar,
            f32::from(p.battery.percent) / 100.0,
            th.surface_hi,
            color,
        );
        let status = if p.battery.charging {
            "Charging".to_string()
        } else if p.plugged {
            "Plugged in".to_string()
        } else if let Some(s) = p.secs_left {
            format!("{} left", fmt_remaining(s))
        } else {
            "On battery".to_string()
        };
        let status = if p.saver && !p.plugged {
            format!("{status} · saver on")
        } else {
            status
        };
        let text_x = bar.right() + 10.0;
        let bolt = p.battery.charging;
        let w = (r.right() - text_x - 10.0 - if bolt { 16.0 } else { 0.0 }).max(0.0);
        if bolt {
            cv.icon(Icon::Bolt, Rect::new(text_x, mid - 6.0, 12.0, 12.0), th.ok);
        }
        cv.text(
            Rect::new(text_x + if bolt { 16.0 } else { 0.0 }, r.y, w, r.h),
            status,
            TextStyle::caption(),
            th.text_dim,
        );
    }
}

impl Module for Stats {
    fn id(&self) -> ModuleId {
        "stats"
    }

    fn title(&self) -> &'static str {
        "Stats"
    }

    fn icon(&self) -> Icon {
        Icon::Pulse
    }

    fn subscriptions(&self) -> EventMask {
        EventMask::of(&[
            Kind::Stats,
            Kind::Power,
            Kind::Suspended,
            Kind::AiUsage,
            Kind::AiLimits,
        ])
    }

    /// Honoured only while the page is on screen: this is the only thing that ever measures.
    fn poll_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs_f32(
            self.cfg.interval_secs.clamp(0.5, 5.0),
        ))
    }

    fn expanded_size(&self) -> Size {
        Size::new(424.0, 232.0 + self.extra_height())
    }

    fn peek_size(&self) -> Option<Size> {
        Some(Size::new(296.0, 64.0))
    }

    fn draw_peek(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let th = *cv.theme;
        let Some(b) = self.banner else { return };
        let (title, p) = match b {
            Banner::Limit {
                weekly,
                used,
                resets_at,
            } => return self.draw_limit(cv, area, weekly, used, resets_at, dx.env.unix),
            Banner::Plugged(p) => ("Charger connected", p),
            Banner::Unplugged(p) => ("On battery", p),
            Banner::Full(p) => ("Fully charged", p),
        };
        let color = match b {
            Banner::Unplugged(_) if p.battery.percent <= 20 => th.warn,
            _ => th.ok,
        };
        let tile = Rect::new(area.x, area.y, area.h, area.h);
        cv.squircle(tile, 8.0, th.surface_hi);
        cv.icon(Icon::Bolt, tile.inset(tile.w * 0.22), color);
        let x = tile.right() + 12.0;
        let w = (area.right() - x).max(0.0);
        cv.text(
            Rect::new(x, area.y + 2.0, w, 18.0),
            title,
            TextStyle::new(13.5, Weight::SemiBold),
            th.text,
        );
        let sub = match (b, p.secs_left) {
            (Banner::Unplugged(_), Some(s)) => {
                format!("{}% · {} left", p.battery.percent, fmt_remaining(s))
            }
            _ => format!("{}%", p.battery.percent),
        };
        cv.text(
            Rect::new(x, area.y + 21.0, w - 40.0, 15.0),
            sub,
            TextStyle::caption(),
            th.text_dim,
        );
        cv.bar(
            Rect::new(x, area.bottom() - 8.0, w, 5.0),
            f32::from(p.battery.percent) / 100.0,
            th.surface_hi,
            color,
        );
    }

    fn on_poll(&mut self, cx: &mut Cx) {
        // Claude's limits: asked for when the page opens and then about once a minute.
        if self.cfg.ai_limits {
            if self.limits_polls.is_multiple_of(60) {
                cx.command(Command::ClaudeLimits);
            }
            self.limits_polls += 1;
        }
        cx.command(Command::Stats(StatsCmd::Sample {
            gpu: self.cfg.gpu,
            devices: self.cfg.devices,
        }));
    }

    fn on_event(&mut self, ev: &Event, cx: &mut Cx) {
        match &ev.kind {
            EventKind::Stats(s) => {
                if let Some(c) = s.cpu {
                    self.cpu.push(c);
                }
                if let Some(m) = Self::mem_percent(s) {
                    self.mem.push(m);
                }
                if let Some(g) = s.gpu {
                    self.gpu.push(g);
                }
                if let Some((d, u)) = s.net {
                    self.down.push(d as f32);
                    self.up.push(u as f32);
                }
                self.latest = Some(s.clone());
                cx.request_redraw();
            }
            EventKind::AiLimits(s) => {
                self.limits = Some(s.clone());
                self.check_limits(cx);
            }
            EventKind::Power(p) => self.on_power(*p, cx),
            EventKind::Suspended(on) => self.suspended = *on,
            EventKind::AiUsage(t) => {
                self.ai = Some(*t);
                // Working now: ask for the limits (the service answers at most once a minute).
                if self.cfg.ai_limits && self.ai_working(cx.env.unix) {
                    cx.command(Command::ClaudeLimits);
                }
            }
            _ => {}
        }
    }

    fn on_visibility(&mut self, _v: Visibility, _cx: &mut Cx) {
        self.limits_polls = 0;
        // Opening starts a fresh minute; closing drops the old one.
        self.forget();
    }

    fn on_config(&mut self, cfg: &Config, cx: &mut Cx) {
        self.cfg = cfg.stats.clone();
        cx.request_redraw();
    }

    fn draw_expanded(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let th = *cv.theme;
        let tile_w = (area.w - GAP) * 0.5;
        let tile_h = ((area.h - BATTERY_H - self.extra_height() - 2.0 * GAP) * 0.5).max(40.0);
        let at = |col: f32, row: f32| {
            Rect::new(
                area.x + col * (tile_w + GAP),
                area.y + row * (tile_h + GAP),
                tile_w,
                tile_h,
            )
        };
        let snap = self.latest.clone();
        let dash = || Text::Static("—");

        // CPU
        let cpu_now = snap.as_ref().and_then(|s| s.cpu);
        self.draw_tile(
            cv,
            at(0.0, 0.0),
            Tile {
                label: "CPU",
                sub: Text::Static("Processor"),
                value: cpu_now.map_or_else(dash, |c| format!("{c:.0}%").into()),
                color: th.accent,
                chart: Some((self.cpu.values(), (0.0, 100.0))),
            },
            None,
        );

        // Memory
        let mem_text = snap.as_ref().filter(|s| s.mem_total > 0).map(|s| {
            (
                format!("{:.0}%", Self::mem_percent(s).unwrap_or(0.0)),
                format!("{} of {}", fmt_memory(s.mem_used), fmt_memory(s.mem_total)),
            )
        });
        self.draw_tile(
            cv,
            at(1.0, 0.0),
            Tile {
                label: "Memory",
                sub: mem_text
                    .as_ref()
                    .map_or(Text::Static(" "), |(_, sub)| sub.clone().into()),
                value: mem_text
                    .as_ref()
                    .map_or_else(dash, |(v, _)| v.clone().into()),
                color: th.ok,
                chart: Some((self.mem.values(), (0.0, 100.0))),
            },
            None,
        );

        // GPU
        let gpu_now = snap.as_ref().and_then(|s| s.gpu);
        let (gpu_sub, gpu_value): (Text, Text) = match (self.cfg.gpu, gpu_now, snap.is_some()) {
            (false, _, _) => (Text::Static("Off in settings"), dash()),
            (true, Some(g), _) => (Text::Static("Busiest engine"), format!("{g:.0}%").into()),
            (true, None, true) => (Text::Static("No GPU counters on this PC"), dash()),
            (true, None, false) => (Text::Static("Busiest engine"), dash()),
        };
        self.draw_tile(
            cv,
            at(0.0, 1.0),
            Tile {
                label: "GPU",
                sub: gpu_sub,
                value: gpu_value,
                color: th.warn,
                chart: (self.cfg.gpu && !self.gpu.is_empty())
                    .then(|| (self.gpu.values(), (0.0, 100.0))),
            },
            None,
        );

        // Network
        let net = snap.as_ref().and_then(|s| s.net);
        let scale = self.down.max().max(self.up.max()).max(NET_FLOOR);
        self.draw_tile(
            cv,
            at(1.0, 1.0),
            Tile {
                label: "Network",
                sub: net.map_or(Text::Static(" "), |(_, u)| {
                    format!("↑ {}", fmt_rate(u, self.cfg.net_bits)).into()
                }),
                value: net.map_or_else(dash, |(d, _)| {
                    format!("↓ {}", fmt_rate(d, self.cfg.net_bits)).into()
                }),
                color: th.accent,
                chart: Some((self.down.values(), (0.0, scale))),
            },
            Some((self.up.values(), th.ok)),
        );

        // The strips, bottom up: battery, then the optional ones.
        let mut y = area.bottom() - BATTERY_H;
        self.draw_battery(
            cv,
            Rect::new(area.x, y, area.w, BATTERY_H),
            snap.as_ref().and_then(|s| s.power.as_ref()),
            snap.is_some(),
        );
        if self.cfg.devices {
            y -= BATTERY_H + GAP;
            self.draw_devices(
                cv,
                Rect::new(area.x, y, area.w, BATTERY_H),
                snap.as_ref().and_then(|s| s.devices.as_deref()),
            );
        }
        if self.cfg.ai_limits {
            y -= BATTERY_H + GAP;
            self.draw_ai(cv, Rect::new(area.x, y, area.w, BATTERY_H), dx.env.unix);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::{DrawCmd, DrawList};
    use crate::events::BatteryInfo;
    use crate::module::{Env, Out};
    use crate::theme::Theme;

    macro_rules! cx {
        ($t:expr) => {
            Cx::for_test($t.now, &$t.env, &$t.theme, &$t.cfg, &mut $t.out)
        };
    }

    fn power(
        percent: u8,
        charging: bool,
        plugged: bool,
        secs_left: Option<u32>,
        saver: bool,
    ) -> PowerStatus {
        PowerStatus {
            battery: BatteryInfo { percent, charging },
            plugged,
            secs_left,
            saver,
        }
    }

    struct T {
        m: Stats,
        theme: Theme,
        cfg: Config,
        env: Env,
        out: Out,
        now: f64,
    }

    impl T {
        fn new() -> T {
            T {
                m: Stats::new(StatsCfg::default()),
                theme: Theme::default(),
                cfg: Config::default(),
                env: Env::default(),
                out: Out::default(),
                now: 10.0,
            }
        }

        fn snap(cpu: Option<f32>) -> StatsSnapshot {
            StatsSnapshot {
                cpu,
                mem_used: 11 * 1024 * 1024 * 1024,
                mem_total: 16 * 1024 * 1024 * 1024,
                gpu: Some(12.0),
                net: Some((4.2 * 1024.0 * 1024.0, 120.0 * 1024.0)),
                power: Some(power(82, false, false, Some(3 * 3600 + 12 * 60), false)),
                devices: Some(vec![
                    BtDevice {
                        name: "AirPods Pro".into(),
                        battery: Some(74),
                    },
                    BtDevice {
                        name: "Keyboard".into(),
                        battery: None,
                    },
                ]),
            }
        }

        fn feed(&mut self, s: StatsSnapshot) {
            let ev = Event::new(crate::events::Source::Local, EventKind::Stats(Arc::new(s)));
            let mut cx = cx!(self);
            self.m.on_event(&ev, &mut cx);
        }

        fn texts(&mut self) -> Vec<String> {
            let mut list = DrawList::new();
            let mut cv = Canvas::new(&mut list, &self.theme);
            let dx = DrawCx {
                now: self.now,
                env: &self.env,
                config: &self.cfg,
            };
            self.m
                .draw_expanded(&mut cv, Rect::new(0.0, 0.0, 372.0, 264.0), &dx);
            assert!(list.is_balanced());
            list.cmds
                .iter()
                .filter_map(|c| match c {
                    DrawCmd::Text { text, .. } => Some(text.as_str().to_string()),
                    _ => None,
                })
                .collect()
        }

        fn shapes(&mut self) -> usize {
            let mut list = DrawList::new();
            let mut cv = Canvas::new(&mut list, &self.theme);
            let dx = DrawCx {
                now: self.now,
                env: &self.env,
                config: &self.cfg,
            };
            self.m
                .draw_expanded(&mut cv, Rect::new(0.0, 0.0, 372.0, 264.0), &dx);
            list.cmds
                .iter()
                .filter(|c| matches!(c, DrawCmd::Shape { .. }))
                .count()
        }
    }

    #[test]
    fn it_polls_only_through_the_host_and_asks_for_a_reading_each_time() {
        let mut t = T::new();
        assert_eq!(t.m.poll_interval(), Some(Duration::from_secs(1)));
        {
            let mut cx = cx!(t);
            t.m.on_poll(&mut cx);
        }
        assert_eq!(
            t.out.commands,
            vec![Command::Stats(StatsCmd::Sample {
                gpu: true,
                devices: true
            })]
        );
        t.m.cfg.gpu = false;
        t.out = Out::default();
        {
            let mut cx = cx!(t);
            t.m.on_poll(&mut cx);
        }
        assert_eq!(
            t.out.commands,
            vec![Command::Stats(StatsCmd::Sample {
                gpu: false,
                devices: true
            })]
        );
        // The interval comes from the configuration and is kept sane.
        t.m.cfg.interval_secs = 2.5;
        assert_eq!(t.m.poll_interval(), Some(Duration::from_millis(2500)));
        t.m.cfg.interval_secs = 0.0;
        assert_eq!(t.m.poll_interval(), Some(Duration::from_millis(500)));
    }

    #[test]
    fn before_the_first_reading_the_tiles_show_dashes_and_no_charts() {
        let mut t = T::new();
        let tx = t.texts();
        for want in ["CPU", "Memory", "GPU", "Network", "Battery", "—"] {
            assert!(tx.contains(&want.to_string()), "{want}: {tx:?}");
        }
        assert_eq!(t.shapes(), 0, "no history, nothing to chart");
    }

    #[test]
    fn a_reading_fills_the_tiles_and_history_draws_the_charts() {
        let mut t = T::new();
        t.feed(T::snap(Some(37.4)));
        let tx = t.texts();
        for want in [
            "37%",
            "69%",
            "11.0 GB of 16.0 GB",
            "12%",
            "↓ 4.2 MB/s",
            "↑ 120 KB/s",
            "82%",
            "3 h 12 min left",
        ] {
            assert!(tx.contains(&want.to_string()), "{want}: {tx:?}");
        }
        assert_eq!(t.shapes(), 0, "one reading is a point, not a line yet");
        t.feed(T::snap(Some(50.0)));
        // Each of CPU, memory, GPU and network draws an area and a line; the network also draws
        // the upload line.
        assert_eq!(t.shapes(), 4 * 2 + 1);
        assert!(t.texts().contains(&"50%".to_string()));
    }

    #[test]
    fn history_is_bounded_and_dropped_when_the_page_goes_away() {
        let mut t = T::new();
        for i in 0..200 {
            t.feed(T::snap(Some((i % 100) as f32)));
        }
        assert_eq!(t.m.cpu.values().len(), SLOTS);
        {
            let mut cx = cx!(t);
            t.m.on_visibility(Visibility::Collapsed, &mut cx);
        }
        assert!(t.m.cpu.is_empty() && t.m.latest.is_none());
        assert!(t.texts().contains(&"—".to_string()), "back to dashes");
    }

    #[test]
    fn a_reading_without_a_cpu_figure_keeps_the_old_chart_and_shows_a_dash() {
        let mut t = T::new();
        t.feed(T::snap(Some(20.0)));
        t.feed(T::snap(None));
        assert_eq!(t.m.cpu.values(), &[20.0]);
        let tx = t.texts();
        assert!(tx.iter().filter(|s| *s == "—").count() >= 1, "{tx:?}");
    }

    #[test]
    fn the_gpu_tile_says_why_it_has_no_figure() {
        let mut t = T::new();
        let mut s = T::snap(Some(10.0));
        s.gpu = None;
        t.feed(s.clone());
        assert!(
            t.texts()
                .contains(&"No GPU counters on this PC".to_string())
        );
        t.m.cfg.gpu = false;
        t.feed(s);
        assert!(t.texts().contains(&"Off in settings".to_string()));
    }

    #[test]
    fn the_battery_strip_covers_charging_plugged_low_and_absent() {
        let mut t = T::new();
        let mut s = T::snap(Some(5.0));
        s.power = Some(power(64, true, true, None, false));
        t.feed(s.clone());
        assert!(t.texts().contains(&"Charging".to_string()));
        s.power = Some(power(100, false, true, None, false));
        t.feed(s.clone());
        assert!(t.texts().contains(&"Plugged in".to_string()));
        s.power = Some(power(9, false, false, None, true));
        t.feed(s.clone());
        let tx = t.texts();
        assert!(tx.contains(&"On battery · saver on".to_string()), "{tx:?}");
        s.power = None;
        t.feed(s);
        assert!(
            t.texts()
                .contains(&"No battery: this PC runs on mains power".to_string())
        );
    }

    #[test]
    fn rates_follow_the_bits_setting() {
        let mut t = T::new();
        t.m.cfg.net_bits = true;
        t.feed(T::snap(Some(1.0)));
        let tx = t.texts();
        assert!(
            tx.contains(&"↓ 35 Mbps".to_string()) && tx.contains(&"↑ 983 Kbps".to_string()),
            "{tx:?}"
        );
    }

    #[test]
    fn disabled_means_never_instantiated_and_it_works_inside_the_host() {
        let mut cfg = Config::default();
        cfg.stats.enabled = false;
        assert!(create(&cfg).is_none());

        use crate::module::ModuleHost;
        let mut cfg = Config::default();
        cfg.modules.order = vec!["stats".into()];
        let mut host = ModuleHost::new(
            vec![crate::module::Factory {
                id: "stats",
                create,
            }],
            Arc::new(cfg),
            Theme::default(),
        );
        host.set_context(10.0, Env::default());
        host.start_new();
        assert_eq!(host.pages().len(), 1);
        assert_eq!(
            host.chips_width(),
            0.0,
            "stats never takes room on the pill"
        );
        assert_eq!(host.next_deadline(), None, "hidden: nothing is scheduled");
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

    fn reading(percent: u8, plugged: bool) -> PowerStatus {
        PowerStatus {
            battery: BatteryInfo {
                percent,
                charging: plugged && percent < 100,
            },
            plugged,
            secs_left: (!plugged).then_some(5400),
            saver: false,
        }
    }

    /// Feed power readings; returns the peek requests each one made.
    fn feed_power(m: &mut Stats, readings: &[PowerStatus]) -> Vec<usize> {
        let (env, theme, cfg) = (Env::default(), Theme::default(), Config::default());
        let mut out = Out::default();
        readings
            .iter()
            .map(|p| {
                let mut cx = Cx::for_test(0.0, &env, &theme, &cfg, &mut out);
                m.on_event(
                    &Event::new(crate::events::Source::Local, EventKind::Power(*p)),
                    &mut cx,
                );
                std::mem::take(&mut out.shell).len()
            })
            .collect()
    }

    #[test]
    fn plugging_in_unplugging_and_reaching_full_each_show_one_banner() {
        let mut m = Stats::new(StatsCfg::default());
        let peeks = feed_power(
            &mut m,
            &[
                reading(80, false),  // the baseline Windows sends on registration
                reading(80, true),   // plugged in
                reading(81, true),   // just charging
                reading(100, true),  // full
                reading(100, true),  // nothing new
                reading(100, false), // unplugged
            ],
        );
        assert_eq!(peeks, vec![0, 1, 0, 1, 0, 1]);
    }

    #[test]
    fn the_banner_names_the_event_and_the_battery() {
        let mut m = Stats::new(StatsCfg::default());
        feed_power(&mut m, &[reading(40, false), reading(40, true)]);
        let th = Theme::default();
        let mut list = DrawList::new();
        let env = Env::default();
        let cfg = Config::default();
        m.draw_peek(
            &mut Canvas::new(&mut list, &th),
            Rect::new(0.0, 0.0, 296.0, 44.0),
            &DrawCx {
                now: 0.0,
                env: &env,
                config: &cfg,
            },
        );
        let tx = texts(&list);
        assert!(tx.contains(&"Charger connected".to_string()), "{tx:?}");
        assert!(tx.contains(&"40%".to_string()), "{tx:?}");
        feed_power(&mut m, &[reading(40, false)]);
        let mut list = DrawList::new();
        m.draw_peek(
            &mut Canvas::new(&mut list, &th),
            Rect::new(0.0, 0.0, 296.0, 44.0),
            &DrawCx {
                now: 0.0,
                env: &env,
                config: &cfg,
            },
        );
        assert!(texts(&list).contains(&"40% · 1 h 30 min left".to_string()));
    }

    #[test]
    fn no_banner_while_a_game_is_in_front_or_when_switched_off() {
        let (env, theme, cfg) = (Env::default(), Theme::default(), Config::default());
        let mut out = Out::default();
        let mut m = Stats::new(StatsCfg::default());
        let mut send = |m: &mut Stats, kind: EventKind| {
            let mut cx = Cx::for_test(0.0, &env, &theme, &cfg, &mut out);
            m.on_event(&Event::new(crate::events::Source::Local, kind), &mut cx);
            std::mem::take(&mut out.shell).len()
        };
        send(&mut m, EventKind::Power(reading(50, false)));
        send(&mut m, EventKind::Suspended(true));
        assert_eq!(send(&mut m, EventKind::Power(reading(50, true))), 0);
        send(&mut m, EventKind::Suspended(false));
        assert_eq!(send(&mut m, EventKind::Power(reading(50, false))), 1);
        m.cfg.battery_hud = false;
        assert_eq!(send(&mut m, EventKind::Power(reading(50, true))), 0);
    }

    #[test]
    fn the_devices_strip_lists_connected_devices_with_batteries_and_can_be_switched_off() {
        let mut t = T::new();
        t.feed(T::snap(Some(10.0)));
        let tx = t.texts();
        assert!(tx.contains(&"Devices".to_string()), "{tx:?}");
        assert!(tx.contains(&"AirPods Pro 74%".to_string()), "{tx:?}");
        assert!(
            tx.contains(&"Keyboard".to_string()),
            "no figure, just the name: {tx:?}"
        );
        // A read with nobody connected says so; no read yet shows a placeholder.
        let mut none = T::new();
        let mut s = T::snap(Some(10.0));
        s.devices = Some(Vec::new());
        none.feed(s);
        assert!(
            none.texts()
                .contains(&"No Bluetooth device connected".to_string())
        );
        // Switched off: no strip and a shorter page.
        let mut off = T::new();
        off.m.cfg.devices = false;
        off.m.cfg.ai_usage = false;
        off.feed(T::snap(Some(10.0)));
        assert!(!off.texts().contains(&"Devices".to_string()));
        assert_eq!(off.m.expanded_size().h, 232.0);
        t.m.cfg.ai_usage = false;
        assert_eq!(t.m.expanded_size().h, 232.0 + 38.0);
    }

    #[test]
    fn the_poll_asks_for_devices_only_when_the_strip_is_on() {
        let mut t = T::new();
        for on in [true, false] {
            t.m.cfg.devices = on;
            let mut cx = cx!(t);
            t.m.on_poll(&mut cx);
            let last = t.out.commands.pop();
            assert!(
                matches!(
                    last,
                    Some(Command::Stats(StatsCmd::Sample { devices, .. })) if devices == on
                ),
                "{last:?}"
            );
        }
    }

    fn ai(t: &mut T, messages: u32, out: u64, last_at: i64, now_unix: i64) {
        t.env.unix = now_unix;
        let ev = Event::new(
            crate::events::Source::Local,
            EventKind::AiUsage(Totals {
                input: 1_200_000,
                output: out,
                messages,
                last_at,
            }),
        );
        let mut cx = cx!(t);
        t.m.on_event(&ev, &mut cx);
    }

    fn limits_event(t: &mut T, state: LimitsState) {
        let ev = Event::new(
            crate::events::Source::Local,
            EventKind::AiLimits(Arc::new(state)),
        );
        let mut cx = cx!(t);
        t.m.on_event(&ev, &mut cx);
    }

    fn known(five: f32, week: f32, reset5: i64, reset7: i64) -> LimitsState {
        use crate::claudelimits::{Limits, Window};
        LimitsState::Known(Limits {
            five_hour: Some(Window {
                used: five,
                resets_at: reset5,
            }),
            seven_day: Some(Window {
                used: week,
                resets_at: reset7,
            }),
        })
    }

    /// Feed limits; returns whether a banner was asked for.
    fn banner_for(t: &mut T, state: LimitsState) -> bool {
        t.out.shell.clear();
        limits_event(t, state);
        !t.out.shell.is_empty()
    }

    #[test]
    fn a_limit_nearly_used_up_gets_one_banner_per_level_per_window() {
        let mut t = T::new();
        assert!(
            !banner_for(&mut t, known(10.0, 20.0, 5_000, 90_000)),
            "plenty left"
        );
        assert!(banner_for(&mut t, known(81.0, 20.0, 5_000, 90_000)), "80 %");
        assert!(
            !banner_for(&mut t, known(83.0, 20.0, 5_000, 90_000)),
            "same level: quiet"
        );
        assert!(banner_for(&mut t, known(96.0, 20.0, 5_000, 90_000)), "95 %");
        assert!(!banner_for(&mut t, known(99.0, 20.0, 5_000, 90_000)));
        // The window starts over (new reset time) and fills again: announced again.
        assert!(!banner_for(&mut t, known(5.0, 20.0, 23_000, 90_000)));
        assert!(banner_for(&mut t, known(85.0, 20.0, 23_000, 90_000)));
        // The other limit has its own levels.
        assert!(
            banner_for(&mut t, known(85.0, 82.0, 23_000, 90_000)),
            "the week"
        );
        // A reading that says nothing useful is not a banner.
        for state in [
            LimitsState::SignedOut,
            LimitsState::Expired,
            LimitsState::Failed,
        ] {
            assert!(!banner_for(&mut t, state));
        }
    }

    #[test]
    fn the_limit_banner_names_the_limit_the_use_and_the_reset() {
        let mut t = T::new();
        limits_event(&mut t, known(92.4, 10.0, 1_000 + 80 * 60, 0));
        t.env.unix = 1_000;
        let th = Theme::default();
        let mut list = DrawList::new();
        let (env, cfg) = (t.env, Config::default());
        t.m.draw_peek(
            &mut Canvas::new(&mut list, &th),
            Rect::new(0.0, 0.0, 296.0, 44.0),
            &DrawCx {
                now: 0.0,
                env: &env,
                config: &cfg,
            },
        );
        let tx = texts(&list);
        assert!(tx.contains(&"Claude 5-hour limit".to_string()), "{tx:?}");
        assert!(
            tx.contains(&"92% used · resets in 1 h 20 min".to_string()),
            "{tx:?}"
        );
        assert!(list.is_balanced());
    }

    #[test]
    fn nothing_is_shown_while_a_game_is_in_front_and_the_limits_are_asked_for_only_while_working() {
        let mut t = T::new();
        // Off (the default): never asked, whatever Claude Code does.
        assert!(!StatsCfg::default().ai_limits);
        ai(&mut t, 3, 84_000, 995, 1_000);
        assert!(!t.out.commands.contains(&Command::ClaudeLimits));
        // On: asked when the logs say it is working, not when it has been quiet.
        t.m.cfg.ai_limits = true;
        ai(&mut t, 3, 84_100, 995, 1_000);
        assert!(t.out.commands.contains(&Command::ClaudeLimits));
        t.out.commands.clear();
        ai(&mut t, 3, 84_100, 100, 1_000);
        assert!(!t.out.commands.contains(&Command::ClaudeLimits), "quiet");
        // A game in front: counted as seen, no banner.
        limits_event_suspended(&mut t);
        assert!(!banner_for(&mut t, known(99.0, 99.0, 5_000, 90_000)));
        assert!(t.m.chip_width().is_none(), "no pill chip any more");
    }

    fn limits_event_suspended(t: &mut T) {
        let ev = Event::new(crate::events::Source::Local, EventKind::Suspended(true));
        let mut cx = cx!(t);
        t.m.on_event(&ev, &mut cx);
    }

    #[test]
    fn the_claude_line_is_on_this_page_only_when_the_limits_are_switched_on() {
        let mut t = T::new();
        t.feed(T::snap(Some(10.0)));
        assert!(
            !t.texts().contains(&"Claude Code".to_string()),
            "off by default"
        );
        let base = t.m.expanded_size().h;
        t.m.cfg.ai_limits = true;
        t.m.cfg.devices = false;
        assert_eq!(t.m.expanded_size().h, 232.0 + 38.0);
        t.env.unix = 1_000;
        let tx0 = t.texts();
        assert!(tx0.contains(&"…".to_string()), "nothing read yet: {tx0:?}");
        limits_event(&mut t, known(42.4, 93.0, 1_000 + 2 * 3600 + 600, 0));
        let tx = t.texts();
        assert!(tx.contains(&"Claude Code".to_string()), "{tx:?}");
        assert!(tx.contains(&"5 h 42% · 2h10m".to_string()), "{tx:?}");
        assert!(tx.contains(&"Week 93%".to_string()), "{tx:?}");
        for (state, want) in [
            (LimitsState::SignedOut, "Sign in to Claude Code"),
            (LimitsState::Expired, "Login expired"),
            (LimitsState::Failed, "unavailable"),
        ] {
            limits_event(&mut t, state);
            assert!(t.texts().iter().any(|s| s.contains(want)), "{want}");
        }
        assert!(base >= 232.0);
    }

    #[test]
    fn the_limits_are_asked_for_when_the_page_opens_and_then_once_a_minute() {
        let mut t = T::new();
        t.m.cfg.ai_limits = true;
        let mut asks = 0;
        for i in 0..=121 {
            let mut cx = cx!(t);
            t.m.on_poll(&mut cx);
            asks += t
                .out
                .commands
                .iter()
                .filter(|c| **c == Command::ClaudeLimits)
                .count();
            t.out.commands.clear();
            if i == 60 {
                // Closing and reopening the page asks again at once.
                let mut cx = cx!(t);
                t.m.on_visibility(Visibility::Collapsed, &mut cx);
            }
        }
        assert_eq!(asks, 4, "polls 0 and 60, then 61 and 121");
        // Off: never.
        t.m.cfg.ai_limits = false;
        let mut cx = cx!(t);
        t.m.on_poll(&mut cx);
        assert!(!t.out.commands.contains(&Command::ClaudeLimits));
    }
}
