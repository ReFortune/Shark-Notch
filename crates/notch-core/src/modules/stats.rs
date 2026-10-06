//! System stats: processor, memory, GPU and network with a minute of history each, and the battery.
//!
//! This page is the only thing that causes any measuring. The host polls a module only while its
//! page is on screen, and each poll just *asks* the platform for a reading (`StatsCmd::Sample`);
//! with the page closed nothing runs, and the history is dropped so that reopening starts clean
//! instead of showing a stale minute with a hole in it.

use std::sync::Arc;
use std::time::Duration;

use crate::color::Color;
use crate::config::{Config, StatsCfg};
use crate::draw::{Align, Canvas, Text, TextStyle, Weight};
use crate::events::{Event, EventKind, EventMask, Kind, PowerStatus, StatsSnapshot};
use crate::geom::{Rect, Size};
use crate::icons::Icon;
use crate::module::{Command, Cx, DrawCx, Module, ModuleId, StatsCmd, Visibility};
use crate::stats::{History, fmt_memory, fmt_rate, fmt_remaining};

/// Samples shown per chart (a minute at the default one reading per second).
const SLOTS: usize = 60;
const GAP: f32 = 8.0;
const BATTERY_H: f32 = 30.0;
/// The network chart never zooms in on background noise: its scale is at least this (bytes/s).
const NET_FLOOR: f32 = 128.0 * 1024.0;

pub fn create(cfg: &Config) -> Option<Box<dyn Module>> {
    cfg.stats
        .enabled
        .then(|| Box::new(Stats::new(cfg.stats.clone())) as Box<dyn Module>)
}

pub struct Stats {
    cfg: StatsCfg,
    cpu: History,
    mem: History,
    gpu: History,
    down: History,
    up: History,
    latest: Option<Arc<StatsSnapshot>>,
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
        EventMask::of(&[Kind::Stats])
    }

    /// Honoured only while the page is on screen: this is the only thing that ever measures.
    fn poll_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs_f32(
            self.cfg.interval_secs.clamp(0.5, 5.0),
        ))
    }

    fn expanded_size(&self) -> Size {
        Size::new(424.0, 232.0)
    }

    fn on_poll(&mut self, cx: &mut Cx) {
        cx.command(Command::Stats(StatsCmd::Sample { gpu: self.cfg.gpu }));
    }

    fn on_event(&mut self, ev: &Event, cx: &mut Cx) {
        if let EventKind::Stats(s) = &ev.kind {
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
    }

    fn on_visibility(&mut self, _v: Visibility, _cx: &mut Cx) {
        // Opening starts a fresh minute; closing drops the old one.
        self.forget();
    }

    fn on_config(&mut self, cfg: &Config, cx: &mut Cx) {
        self.cfg = cfg.stats.clone();
        cx.request_redraw();
    }

    fn draw_expanded(&mut self, cv: &mut Canvas, area: Rect, _dx: &DrawCx) {
        let th = *cv.theme;
        let tile_w = (area.w - GAP) * 0.5;
        let tile_h = ((area.h - BATTERY_H - 2.0 * GAP) * 0.5).max(40.0);
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

        // Battery
        let strip = Rect::new(area.x, area.bottom() - BATTERY_H, area.w, BATTERY_H);
        self.draw_battery(
            cv,
            strip,
            snap.as_ref().and_then(|s| s.power.as_ref()),
            snap.is_some(),
        );
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
                .draw_expanded(&mut cv, Rect::new(0.0, 0.0, 372.0, 188.0), &dx);
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
                .draw_expanded(&mut cv, Rect::new(0.0, 0.0, 372.0, 188.0), &dx);
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
            vec![Command::Stats(StatsCmd::Sample { gpu: true })]
        );
        t.m.cfg.gpu = false;
        t.out = Out::default();
        {
            let mut cx = cx!(t);
            t.m.on_poll(&mut cx);
        }
        assert_eq!(
            t.out.commands,
            vec![Command::Stats(StatsCmd::Sample { gpu: false })]
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
}
