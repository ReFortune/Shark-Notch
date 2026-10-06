//! The iPhone page: whether the link is listening and where, what the phone last said (its battery
//! and Focus), what it last sent, and the two buttons that manage the pairing token.
//!
//! The listener runs only when `[phone] listen = true`; until then this page is the explanation of
//! what the link is and how to switch it on. The page and the chip are views over events: the
//! listener (a service) produces `PhoneLink`, `Battery` and `Focus` events with the phone as their
//! source, exactly as the clipboard and notification modules receive phone items.

use std::sync::Arc;

use crate::config::{Config, PhoneCfg};
use crate::draw::{Align, Canvas, CursorKind, HitId, Text, TextStyle, Weight};
use crate::events::{BatteryInfo, Event, EventKind, EventMask, FocusInfo, Kind, PhoneLink, Source};
use crate::geom::{Rect, Size};
use crate::icons::Icon;
use crate::input::Input;
use crate::module::{Command, Cx, DrawCx, Module, ModuleId, PhoneCmd, Visibility};

const HIT_COPY: HitId = HitId(1);
const HIT_NEW: HitId = HitId(2);
const HIT_CONFIG: HitId = HitId(3);

/// How long "Copied" stays, and how long the second tap on "New token" has to come.
const FLASH_SECS: f64 = 1.6;
const CONFIRM_SECS: f64 = 3.0;
const CHIP_W: f32 = 84.0;

pub fn create(cfg: &Config) -> Option<Box<dyn Module>> {
    cfg.phone
        .enabled
        .then(|| Box::new(Phone::new(cfg.phone.clone())) as Box<dyn Module>)
}

pub struct Phone {
    cfg: PhoneCfg,
    link: Arc<PhoneLink>,
    battery: Option<BatteryInfo>,
    focus: Option<FocusInfo>,
    hover: Option<HitId>,
    copied_until: f64,
    confirm_until: f64,
}

/// "just now", "2 min ago", "3 h ago", "5 days ago".
pub fn fmt_ago(secs: i64) -> String {
    match secs {
        s if s < 45 => "just now".to_string(),
        s if s < 3600 => format!("{} min ago", ((s + 30) / 60).max(1)),
        s if s < 86_400 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86_400),
    }
}

impl Phone {
    pub fn new(cfg: PhoneCfg) -> Phone {
        Phone {
            cfg,
            link: Arc::new(PhoneLink::default()),
            battery: None,
            focus: None,
            hover: None,
            copied_until: 0.0,
            confirm_until: 0.0,
        }
    }

    fn button(&self, cv: &mut Canvas, r: Rect, id: HitId, label: Text, accent: bool) {
        let th = *cv.theme;
        let hot = self.hover == Some(id);
        let fill = if accent {
            th.accent.with_alpha(if hot { 0.5 } else { 0.34 })
        } else if hot {
            th.surface_hi
        } else {
            th.surface
        };
        cv.round_rect(r, 9.0, fill);
        cv.text(
            r,
            label,
            TextStyle::new(12.0, Weight::SemiBold).align(Align::Center),
            th.text,
        );
        cv.hit(r, id, CursorKind::Hand);
    }

    fn draw_setup(&self, cv: &mut Canvas, area: Rect) {
        let th = *cv.theme;
        cv.text(
            Rect::new(area.x, area.y, area.w, 20.0),
            "iPhone",
            TextStyle::title(),
            th.text,
        );
        cv.text(
            Rect::new(area.x, area.y + 28.0, area.w, 18.0),
            "The link is off",
            TextStyle::new(14.0, Weight::SemiBold),
            th.text,
        );
        cv.text(
            Rect::new(area.x, area.y + 50.0, area.w, 60.0),
            "It runs a small server on your own network so Shortcuts on your iPhone can send clipboard text, files, battery and Focus here. It starts only when you switch it on.",
            TextStyle {
                lines: 4,
                ..TextStyle::caption()
            },
            th.text_dim,
        );
        cv.text(
            Rect::new(area.x, area.y + 114.0, area.w, 16.0),
            "Set listen = true under [phone] in the settings, then follow docs/IPHONE_SHORTCUTS.md.",
            TextStyle::caption(),
            th.text_faint,
        );
        self.button(
            cv,
            Rect::new(area.x, area.bottom() - 30.0, 128.0, 28.0),
            HIT_CONFIG,
            Text::Static("Open settings"),
            true,
        );
    }
}

impl Module for Phone {
    fn id(&self) -> ModuleId {
        "phone"
    }

    fn title(&self) -> &'static str {
        "iPhone"
    }

    fn icon(&self) -> Icon {
        Icon::Bolt
    }

    fn subscriptions(&self) -> EventMask {
        EventMask::of(&[Kind::PhoneLink, Kind::Battery, Kind::FocusChanged])
    }

    /// While the page is on screen the addresses are looked at again now and then (a phone joining
    /// a different Wi-Fi changes them); nothing runs while it is not.
    fn poll_interval(&self) -> Option<std::time::Duration> {
        self.cfg.listen.then_some(std::time::Duration::from_secs(5))
    }

    fn on_poll(&mut self, cx: &mut Cx) {
        cx.command(Command::Phone(PhoneCmd::Refresh));
    }

    fn expanded_size(&self) -> Size {
        Size::new(424.0, 206.0)
    }

    fn chip_width(&self) -> Option<f32> {
        self.focus.as_ref().filter(|f| f.active).map(|_| CHIP_W)
    }

    fn chip_priority(&self) -> i32 {
        25
    }

    fn on_event(&mut self, ev: &Event, cx: &mut Cx) {
        match &ev.kind {
            EventKind::PhoneLink(l) => self.link = l.clone(),
            // Only what the phone reports: no other source describes the iPhone.
            EventKind::Battery(b) if ev.source == Source::Phone => self.battery = Some(*b),
            EventKind::FocusChanged(f) if ev.source == Source::Phone => {
                self.focus = Some(f.clone())
            }
            _ => return,
        }
        cx.request_redraw();
    }

    fn on_config(&mut self, cfg: &Config, cx: &mut Cx) {
        self.cfg = cfg.phone.clone();
        if !self.cfg.listen {
            // The link was switched off: what it last said is no longer current.
            self.link = Arc::new(PhoneLink::default());
            self.battery = None;
            self.focus = None;
        }
        cx.request_redraw();
    }

    fn on_visibility(&mut self, v: Visibility, _cx: &mut Cx) {
        if v != Visibility::Expanded {
            self.hover = None;
            self.copied_until = 0.0;
            self.confirm_until = 0.0;
        }
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
            Input::Click(_) => match hit {
                Some(HIT_CONFIG) => {
                    cx.command(Command::OpenConfig);
                    true
                }
                Some(HIT_COPY) => {
                    cx.command(Command::Phone(PhoneCmd::CopyToken));
                    self.copied_until = cx.now + FLASH_SECS;
                    cx.redraw_at(self.copied_until);
                    cx.request_redraw();
                    true
                }
                Some(HIT_NEW) => {
                    // A new token breaks every Shortcut that has the old one: ask twice.
                    if cx.now < self.confirm_until {
                        self.confirm_until = 0.0;
                        cx.command(Command::Phone(PhoneCmd::NewToken));
                    } else {
                        self.confirm_until = cx.now + CONFIRM_SECS;
                        cx.redraw_at(self.confirm_until);
                    }
                    cx.request_redraw();
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }

    fn draw_expanded(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        if !self.cfg.listen {
            self.draw_setup(cv, area);
            return;
        }
        let th = *cv.theme;
        let now = dx.env.unix;
        cv.text(
            Rect::new(area.x, area.y, area.w * 0.5, 20.0),
            "iPhone",
            TextStyle::title(),
            th.text,
        );
        let (status, color) = match (&self.link.error, self.link.addrs.is_empty()) {
            (Some(_), _) => ("Not listening", th.warn),
            (None, true) => ("Starting…", th.text_faint),
            (None, false) => ("Listening", th.ok),
        };
        cv.circle(
            crate::geom::Vec2::new(area.right() - 80.0, area.y + 10.0),
            3.5,
            color,
        );
        cv.text(
            Rect::new(area.right() - 72.0, area.y, 72.0, 20.0),
            status,
            TextStyle::caption(),
            th.text_dim,
        );

        // Where to send.
        if let Some(err) = &self.link.error {
            cv.text(
                Rect::new(area.x, area.y + 28.0, area.w, 34.0),
                Text::from(err.clone()),
                TextStyle {
                    lines: 2,
                    ..TextStyle::caption()
                },
                th.warn,
            );
        } else if let Some(first) = self.link.addrs.first() {
            cv.text(
                Rect::new(area.x, area.y + 26.0, area.w, 18.0),
                format!("http://{first}"),
                TextStyle::new(14.0, Weight::SemiBold).tabular(),
                th.text,
            );
            let more = self.link.addrs.len() - 1;
            cv.text(
                Rect::new(area.x, area.y + 45.0, area.w, 15.0),
                if more > 0 {
                    format!(
                        "and {more} more address{}",
                        if more == 1 { "" } else { "es" }
                    )
                } else {
                    "Send from Shortcuts with the token as a Bearer header".to_string()
                },
                TextStyle::caption(),
                th.text_faint,
            );
        }

        // What the phone said.
        let row = area.y + 72.0;
        cv.squircle(Rect::new(area.x, row, area.w, 40.0), 10.0, th.surface);
        match &self.battery {
            Some(b) => {
                let low = b.percent <= 20 && !b.charging;
                let bar = Rect::new(area.x + 14.0, row + 16.0, 70.0, 8.0);
                cv.bar(
                    bar,
                    f32::from(b.percent) / 100.0,
                    th.surface_hi,
                    if low {
                        th.warn
                    } else if b.charging {
                        th.ok
                    } else {
                        th.text
                    },
                );
                cv.text(
                    Rect::new(bar.right() + 8.0, row, 44.0, 40.0),
                    format!("{}%", b.percent),
                    TextStyle::new(13.0, Weight::SemiBold).tabular(),
                    th.text,
                );
                if b.charging {
                    cv.icon(
                        Icon::Bolt,
                        Rect::new(bar.right() + 50.0, row + 13.0, 14.0, 14.0),
                        th.ok,
                    );
                }
            }
            None => cv.text(
                Rect::new(area.x + 14.0, row, 150.0, 40.0),
                "No battery report yet",
                TextStyle::caption(),
                th.text_faint,
            ),
        }
        let (focus_text, focus_color) = match &self.focus {
            Some(f) if f.active => (Text::from(f.name.clone()), th.accent),
            Some(_) => (Text::Static("Focus off"), th.text_dim),
            None => (Text::Static("No Focus report yet"), th.text_faint),
        };
        cv.icon(
            Icon::Moon,
            Rect::new(area.x + area.w * 0.55, row + 11.0, 18.0, 18.0),
            focus_color,
        );
        cv.text(
            Rect::new(
                area.x + area.w * 0.55 + 24.0,
                row,
                area.w * 0.45 - 30.0,
                40.0,
            ),
            focus_text,
            TextStyle::new(12.5, Weight::SemiBold),
            focus_color,
        );

        // What it last sent.
        let activity = match &self.link.last {
            Some((t, what)) => format!(
                "Last: {what} · {} · {} accepted, {} refused",
                fmt_ago((now - t).max(0)),
                self.link.accepted,
                self.link.refused
            ),
            None => format!("Nothing received yet · {} refused", self.link.refused),
        };
        cv.text(
            Rect::new(area.x, row + 46.0, area.w, 15.0),
            activity,
            TextStyle::caption(),
            th.text_dim,
        );

        // The token.
        let y = area.bottom() - 30.0;
        let copied = dx.now < self.copied_until;
        self.button(
            cv,
            Rect::new(area.x, y, 120.0, 28.0),
            HIT_COPY,
            Text::Static(if copied { "Copied" } else { "Copy token" }),
            copied,
        );
        let confirming = dx.now < self.confirm_until;
        self.button(
            cv,
            Rect::new(area.x + 128.0, y, 150.0, 28.0),
            HIT_NEW,
            Text::Static(if confirming {
                "Sure? Tap again"
            } else {
                "New token"
            }),
            confirming,
        );
    }

    fn draw_chip(&mut self, cv: &mut Canvas, area: Rect, _dx: &DrawCx) {
        let th = *cv.theme;
        let Some(f) = self.focus.as_ref().filter(|f| f.active) else {
            return;
        };
        cv.icon(
            Icon::Moon,
            Rect::new(area.x, area.center().y - 7.0, 14.0, 14.0),
            th.accent,
        );
        cv.text(
            Rect::new(area.x + 18.0, area.y, (area.w - 18.0).max(0.0), area.h),
            Text::from(f.name.clone()),
            TextStyle::new(11.5, Weight::SemiBold),
            th.text,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::{DrawCmd, DrawList};
    use crate::module::{Env, Out};
    use crate::theme::Theme;

    macro_rules! cx {
        ($t:expr) => {
            Cx::for_test($t.now, &$t.env, &$t.theme, &$t.cfg, &mut $t.out)
        };
    }

    struct T {
        m: Phone,
        theme: Theme,
        cfg: Config,
        env: Env,
        out: Out,
        now: f64,
    }

    fn listening() -> PhoneCfg {
        PhoneCfg {
            listen: true,
            ..PhoneCfg::default()
        }
    }

    impl T {
        fn new(cfg: PhoneCfg) -> T {
            T {
                m: Phone::new(cfg),
                theme: Theme::default(),
                cfg: Config::default(),
                env: Env {
                    unix: 1_000_000,
                    ..Env::default()
                },
                out: Out::default(),
                now: 10.0,
            }
        }

        fn send(&mut self, source: Source, kind: EventKind) {
            let ev = Event::new(source, kind);
            let mut cx = cx!(self);
            self.m.on_event(&ev, &mut cx);
        }

        fn link(&mut self, l: PhoneLink) {
            self.send(Source::Local, EventKind::PhoneLink(Arc::new(l)));
        }

        fn list(&mut self) -> DrawList {
            let mut list = DrawList::new();
            let mut cv = Canvas::new(&mut list, &self.theme);
            let dx = DrawCx {
                now: self.now,
                env: &self.env,
                config: &self.cfg,
            };
            self.m
                .draw_expanded(&mut cv, Rect::new(0.0, 0.0, 372.0, 160.0), &dx);
            assert!(list.is_balanced());
            list
        }

        fn texts(&mut self) -> Vec<String> {
            self.list()
                .cmds
                .iter()
                .filter_map(|c| match c {
                    DrawCmd::Text { text, .. } => Some(text.as_str().to_string()),
                    _ => None,
                })
                .collect()
        }

        fn click(&mut self, id: HitId) -> bool {
            let mut cx = cx!(self);
            self.m
                .on_input(Some(id), &Input::Click(crate::geom::Vec2::ZERO), &mut cx)
        }

        fn commands(&mut self) -> Vec<Command> {
            std::mem::take(&mut self.out.commands)
        }
    }

    #[test]
    fn until_it_is_switched_on_the_page_explains_what_it_is_and_how() {
        let mut t = T::new(PhoneCfg::default());
        let tx = t.texts();
        assert!(tx.contains(&"The link is off".to_string()), "{tx:?}");
        assert!(tx.iter().any(|s| s.contains("docs/IPHONE_SHORTCUTS.md")));
        let list = t.list();
        assert!(list.hits.iter().any(|h| h.id == HIT_CONFIG));
        assert!(
            !list
                .hits
                .iter()
                .any(|h| h.id == HIT_COPY || h.id == HIT_NEW),
            "no token buttons while there is no link"
        );
        assert!(t.click(HIT_CONFIG));
        assert_eq!(t.commands(), vec![Command::OpenConfig]);
    }

    #[test]
    fn a_listening_link_shows_where_to_send_and_what_was_last_received() {
        let mut t = T::new(listening());
        assert!(t.texts().contains(&"Starting…".to_string()));
        t.link(PhoneLink {
            addrs: vec!["192.168.1.23:8765".into(), "10.0.0.5:8765".into()],
            port: 8765,
            last: Some((1_000_000 - 130, "clipboard".into())),
            accepted: 14,
            refused: 2,
            ..PhoneLink::default()
        });
        let tx = t.texts();
        for want in [
            "Listening",
            "http://192.168.1.23:8765",
            "and 1 more address",
            "Last: clipboard · 2 min ago · 14 accepted, 2 refused",
            "No battery report yet",
            "No Focus report yet",
            "Copy token",
            "New token",
        ] {
            assert!(tx.contains(&want.to_string()), "{want}: {tx:?}");
        }
        t.link(PhoneLink {
            addrs: vec!["192.168.1.23:8765".into()],
            ..PhoneLink::default()
        });
        assert!(
            t.texts()
                .contains(&"Nothing received yet · 0 refused".to_string())
        );
    }

    #[test]
    fn a_link_that_cannot_listen_says_why() {
        let mut t = T::new(listening());
        t.link(PhoneLink {
            error: Some("port 8765 is already in use by another program".into()),
            ..PhoneLink::default()
        });
        let tx = t.texts();
        assert!(tx.contains(&"Not listening".to_string()));
        assert!(tx.iter().any(|s| s.contains("already in use")));
    }

    #[test]
    fn the_phones_battery_and_focus_are_shown_and_only_the_phones() {
        let mut t = T::new(listening());
        t.send(
            Source::Phone,
            EventKind::Battery(BatteryInfo {
                percent: 82,
                charging: true,
            }),
        );
        t.send(
            Source::Phone,
            EventKind::FocusChanged(FocusInfo {
                name: "Work".into(),
                active: true,
            }),
        );
        let tx = t.texts();
        assert!(
            tx.contains(&"82%".to_string()) && tx.contains(&"Work".to_string()),
            "{tx:?}"
        );
        // A battery event that is not from the phone does not describe the iPhone.
        t.send(
            Source::Local,
            EventKind::Battery(BatteryInfo {
                percent: 5,
                charging: false,
            }),
        );
        assert!(t.texts().contains(&"82%".to_string()));
        t.send(
            Source::Phone,
            EventKind::FocusChanged(FocusInfo {
                name: "Work".into(),
                active: false,
            }),
        );
        assert!(t.texts().contains(&"Focus off".to_string()));
    }

    #[test]
    fn the_phones_focus_is_a_chip_only_while_it_is_on() {
        let mut t = T::new(listening());
        assert_eq!(t.m.chip_width(), None);
        t.send(
            Source::Phone,
            EventKind::FocusChanged(FocusInfo {
                name: "Sleep".into(),
                active: true,
            }),
        );
        assert_eq!(t.m.chip_width(), Some(CHIP_W));
        let mut list = DrawList::new();
        {
            let mut cv = Canvas::new(&mut list, &t.theme);
            let dx = DrawCx {
                now: t.now,
                env: &t.env,
                config: &t.cfg,
            };
            t.m.draw_chip(&mut cv, Rect::new(0.0, 0.0, CHIP_W, 24.0), &dx);
        }
        assert!(
            list.cmds
                .iter()
                .any(|c| matches!(c, DrawCmd::Text { text, .. } if text.as_str() == "Sleep"))
        );
        t.send(
            Source::Phone,
            EventKind::FocusChanged(FocusInfo {
                name: "Sleep".into(),
                active: false,
            }),
        );
        assert_eq!(t.m.chip_width(), None);
    }

    #[test]
    fn copying_the_token_is_confirmed_and_a_new_one_needs_two_taps() {
        let mut t = T::new(listening());
        t.link(PhoneLink {
            addrs: vec!["192.168.1.23:8765".into()],
            ..PhoneLink::default()
        });
        assert!(t.click(HIT_COPY));
        assert_eq!(t.commands(), vec![Command::Phone(PhoneCmd::CopyToken)]);
        assert!(t.texts().contains(&"Copied".to_string()));
        t.now += FLASH_SECS + 0.1;
        assert!(t.texts().contains(&"Copy token".to_string()));

        // First tap only asks; the second within a few seconds does it.
        assert!(t.click(HIT_NEW));
        assert!(t.commands().is_empty());
        assert!(t.texts().contains(&"Sure? Tap again".to_string()));
        assert!(t.click(HIT_NEW));
        assert_eq!(t.commands(), vec![Command::Phone(PhoneCmd::NewToken)]);
        // Too slow a second tap starts over.
        assert!(t.click(HIT_NEW));
        t.now += CONFIRM_SECS + 0.5;
        assert!(t.click(HIT_NEW));
        assert!(t.commands().is_empty(), "the confirmation had lapsed");
    }

    #[test]
    fn switching_the_link_off_forgets_what_the_phone_said() {
        let mut t = T::new(listening());
        t.send(
            Source::Phone,
            EventKind::FocusChanged(FocusInfo {
                name: "Work".into(),
                active: true,
            }),
        );
        assert!(t.m.chip_width().is_some());
        let cfg = Config::default(); // listen = false
        {
            let mut cx = cx!(t);
            t.m.on_config(&cfg, &mut cx);
        }
        assert_eq!(t.m.chip_width(), None);
        assert!(t.texts().contains(&"The link is off".to_string()));
    }

    #[test]
    fn the_addresses_are_refreshed_only_by_the_hosts_poll_and_only_while_listening() {
        let t = T::new(PhoneCfg::default());
        assert_eq!(
            t.m.poll_interval(),
            None,
            "nothing to refresh while the link is off"
        );
        let mut t = T::new(listening());
        assert_eq!(t.m.poll_interval(), Some(std::time::Duration::from_secs(5)));
        {
            let mut cx = cx!(t);
            t.m.on_poll(&mut cx);
        }
        assert_eq!(t.commands(), vec![Command::Phone(PhoneCmd::Refresh)]);
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(fmt_ago(0), "just now");
        assert_eq!(fmt_ago(44), "just now");
        assert_eq!(fmt_ago(50), "1 min ago");
        assert_eq!(fmt_ago(130), "2 min ago");
        assert_eq!(fmt_ago(3599), "60 min ago");
        assert_eq!(fmt_ago(7300), "2 h ago");
        assert_eq!(fmt_ago(3 * 86_400 + 5), "3 days ago");
    }

    #[test]
    fn it_works_inside_the_host_and_can_be_switched_off() {
        let mut cfg = Config::default();
        cfg.phone.enabled = false;
        assert!(create(&cfg).is_none());

        use crate::module::ModuleHost;
        let mut cfg = Config::default();
        cfg.modules.order = vec!["phone".into()];
        let mut host = ModuleHost::new(
            vec![crate::module::Factory {
                id: "phone",
                create,
            }],
            Arc::new(cfg),
            Theme::default(),
        );
        host.set_context(10.0, Env::default());
        host.start_new();
        assert_eq!(host.pages().len(), 1);
        assert_eq!(host.chips_width(), 0.0);
        assert_eq!(
            host.next_deadline(),
            None,
            "nothing is scheduled while idle"
        );
    }
}
