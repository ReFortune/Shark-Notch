//! Notifications: the banners an iPhone shortcut sends, shown as a short pop-up. There is no page.
//!
//! * **Banner, then gone.** A fresh notification asks the shell for a peek of a few seconds; nothing
//!   stays on screen and nothing needs a click.
//! * **Never while a game is in front.** While the shell is suspended (fullscreen app, lock, pause)
//!   notifications are *counted*, not shown; when it comes back one short "while you were away"
//!   banner says how many arrived.
//! * **Only what you send.** Windows lets only a process with package identity read other apps'
//!   notifications, which this app does not have, so nothing from Windows appears here.

use std::sync::Arc;

use crate::config::{Config, NotificationsCfg};
use crate::draw::{Canvas, ImageId, TextStyle, Weight};
use crate::events::{Event, EventKind, EventMask, Kind, Source};
use crate::geom::{Rect, Size};
use crate::icons::Icon;
use crate::module::{Cx, DrawCx, Module, ModuleId};

/// How long the "while you were away" banner stays.
const SUMMARY_SECS: f64 = 3.2;

pub fn create(cfg: &Config) -> Option<Box<dyn Module>> {
    cfg.notifications.enabled.then(|| {
        Box::new(Notifications::new(
            cfg.notifications.clone(),
            cfg.fullscreen.show_missed_indicator,
        )) as Box<dyn Module>
    })
}

/// The newest notification, for the banner.
#[derive(Clone, Debug, PartialEq)]
struct Latest {
    app: Arc<str>,
    title: Arc<str>,
    body: Arc<str>,
    icon: u64,
    source: Source,
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
    latest: Option<Latest>,
    suspended: bool,
    /// Notifications that arrived while the shell was suspended.
    missed: u32,
    peek_mode: PeekMode,
}

impl Notifications {
    pub fn new(cfg: NotificationsCfg, show_missed: bool) -> Notifications {
        Notifications {
            cfg,
            show_missed,
            latest: None,
            suspended: false,
            missed: 0,
            peek_mode: PeekMode::Latest,
        }
    }

    fn ignored(&self, app: &str) -> bool {
        self.cfg
            .ignore_apps
            .iter()
            .any(|a| a.trim().eq_ignore_ascii_case(app.trim()))
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
        EventMask::of(&[Kind::Notification, Kind::Suspended])
    }

    /// Banner only: this module has no page in the ring.
    fn page_visible(&self) -> bool {
        false
    }

    fn expanded_size(&self) -> Size {
        Size::new(0.0, 0.0)
    }

    fn peek_size(&self) -> Option<Size> {
        Some(Size::new(354.0, 76.0))
    }

    fn on_event(&mut self, ev: &Event, cx: &mut Cx) {
        match &ev.kind {
            EventKind::Notification(n) => {
                // Only a notification that is new (not an old one handed over) can interrupt.
                if self.ignored(&n.app) || !n.fresh {
                    return;
                }
                if self.suspended {
                    self.missed += 1;
                } else if self.cfg.peek && !n.quiet {
                    self.latest = Some(Latest {
                        app: n.app.clone(),
                        title: n.title.clone(),
                        body: n.body.clone(),
                        icon: n.icon,
                        source: ev.source,
                    });
                    self.peek_mode = PeekMode::Latest;
                    cx.peek(f64::from(self.cfg.peek_secs));
                    cx.request_redraw();
                }
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
        cx.request_redraw();
    }

    fn draw_expanded(&mut self, _cv: &mut Canvas, _area: Rect, _dx: &DrawCx) {}

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
                let Some(row) = self.latest.as_ref() else {
                    return;
                };
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::{DrawCmd, DrawList};
    use crate::events::Notification;
    use crate::module::{Env, ModuleHost, Out, ShellRequest};
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
    }

    impl T {
        fn new() -> T {
            T::with(NotificationsCfg::default(), true)
        }
        fn with(cfg: NotificationsCfg, show_missed: bool) -> T {
            T {
                m: Notifications::new(cfg, show_missed),
                theme: Theme::default(),
                cfg: Config::default(),
                env: Env::default(),
                out: Out::default(),
            }
        }
        fn send(&mut self, source: Source, kind: EventKind) {
            let ev = Event::new(source, kind);
            let mut cx = Cx::for_test(100.0, &self.env, &self.theme, &self.cfg, &mut self.out);
            self.m.on_event(&ev, &mut cx);
        }
        fn notify(&mut self, source: Source, n: Notification) {
            self.send(source, EventKind::Notification(n));
        }
        fn peeks(&self) -> usize {
            self.out.shell.len()
        }
        fn banner(&mut self) -> Vec<String> {
            let mut list = DrawList::new();
            let mut cv = Canvas::new(&mut list, &self.theme);
            self.m.draw_peek(
                &mut cv,
                Rect::new(0.0, 0.0, 294.0, 30.0),
                &DrawCx {
                    now: 0.0,
                    env: &self.env,
                    config: &self.cfg,
                },
            );
            assert!(list.is_balanced());
            list.cmds
                .iter()
                .filter_map(|c| match c {
                    DrawCmd::Text { text, .. } => Some(text.as_str().to_string()),
                    _ => None,
                })
                .collect()
        }
    }

    #[test]
    fn a_fresh_notification_shows_a_banner_naming_app_title_and_body() {
        let mut t = T::new();
        t.notify(
            Source::Local,
            note(1, "Mail", "Invoice due", "Pay by Friday", true),
        );
        assert_eq!(t.peeks(), 1);
        let tx = t.banner();
        for want in ["Mail", "Invoice due", "Pay by Friday"] {
            assert!(tx.contains(&want.to_string()), "{want}: {tx:?}");
        }
    }

    #[test]
    fn an_old_one_handed_over_or_a_quiet_one_never_interrupts() {
        let mut t = T::new();
        t.notify(Source::Local, note(1, "Mail", "Old news", "", false));
        let mut quiet = note(2, "Chat", "shh", "", true);
        quiet.quiet = true;
        t.notify(Source::Local, quiet);
        assert_eq!(t.peeks(), 0);
    }

    #[test]
    fn the_banner_can_be_switched_off_and_ignored_apps_never_show() {
        let mut off = T::with(
            NotificationsCfg {
                peek: false,
                ..Default::default()
            },
            true,
        );
        off.notify(Source::Local, note(1, "Mail", "Hello", "", true));
        assert_eq!(off.peeks(), 0);
        let mut t = T::with(
            NotificationsCfg {
                ignore_apps: vec!["  spotify ".into()],
                ..Default::default()
            },
            true,
        );
        t.notify(Source::Local, note(1, "Spotify", "Now playing", "", true));
        assert_eq!(t.peeks(), 0);
        t.notify(Source::Local, note(2, "Mail", "Hi", "", true));
        assert_eq!(t.peeks(), 1);
    }

    #[test]
    fn phone_notifications_look_the_same_with_a_device_tag() {
        let mut t = T::new();
        t.notify(
            Source::Phone,
            note((1 << 32) | 5, "Messages", "Mum", "Call me", true),
        );
        assert!(t.banner().contains(&"Messages · iPhone".to_string()));
    }

    #[test]
    fn while_suspended_they_are_counted_and_one_summary_follows_the_return() {
        let mut t = T::new();
        t.send(Source::Local, EventKind::Suspended(true));
        for i in 1..=3 {
            t.notify(Source::Local, note(i, "Chat", "gg", "", true));
        }
        assert_eq!(t.peeks(), 0, "nothing interrupts a game");
        t.send(Source::Local, EventKind::Suspended(false));
        assert_eq!(t.peeks(), 1, "one summary banner");
        let tx = t.banner();
        assert!(
            tx.contains(&"While you were away".to_string())
                && tx.contains(&"3 notifications".to_string()),
            "{tx:?}"
        );
    }

    #[test]
    fn no_summary_when_nothing_was_missed_or_when_it_is_switched_off() {
        let mut t = T::new();
        t.send(Source::Local, EventKind::Suspended(true));
        t.send(Source::Local, EventKind::Suspended(false));
        assert_eq!(t.peeks(), 0);
        let mut off = T::with(NotificationsCfg::default(), false);
        off.send(Source::Local, EventKind::Suspended(true));
        off.notify(Source::Local, note(1, "Chat", "a", "", true));
        off.send(Source::Local, EventKind::Suspended(false));
        assert_eq!(off.peeks(), 0);
    }

    #[test]
    fn it_has_no_page_but_still_pops_up_inside_the_host() {
        let mut base = Config::default();
        base.modules.order = vec!["notifications".into(), "clock".into()];
        let mut host = ModuleHost::new(
            crate::modules::registry(),
            Arc::new(base.clone()),
            Theme::default(),
        );
        assert_eq!(host.page_ids(), vec!["clock"], "no page in the ring");
        host.dispatch(vec![Event::new(
            Source::Phone,
            EventKind::Notification(note(1, "Chat", "a", "", true)),
        )]);
        let out = host.take_out();
        assert!(
            out.shell.iter().any(
                |r| matches!(r, ShellRequest::Peek { module, .. } if *module == "notifications")
            ),
            "{:?}",
            out.shell
        );
        assert!(host.peek_size("notifications").is_some());
        let mut cfg = base;
        cfg.notifications.enabled = false;
        host.apply_config(Arc::new(cfg));
        host.dispatch(vec![Event::new(
            Source::Phone,
            EventKind::Notification(note(2, "Chat", "b", "", true)),
        )]);
        assert!(host.take_out().shell.is_empty(), "switched off: silent");
    }
}
