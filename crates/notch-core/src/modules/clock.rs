//! The clock page: big time, the date, and the ISO week. It exists mostly to prove the module
//! system end to end (config toggle -> lazy instantiation -> poll only while visible -> redraw).

use std::time::Duration;

use crate::civil::{LocalTime, MONTHS, MONTHS_SHORT, WEEKDAYS, WEEKDAYS_SHORT};
use crate::config::{ClockCfg, Config, HourFormat};
use crate::draw::{Align, Canvas, TextStyle, Weight};
use crate::geom::{Rect, Size};
use crate::icons::Icon;
#[cfg(test)]
use crate::module::Audio;
use crate::module::{Cx, DrawCx, Env, Module, ModuleId};

pub fn create(cfg: &Config) -> Option<Box<dyn Module>> {
    cfg.clock
        .enabled
        .then(|| Box::new(Clock::new(cfg.clock.clone())) as Box<dyn Module>)
}

pub struct Clock {
    cfg: ClockCfg,
    /// What the last frame showed; `on_poll` redraws only when this would change.
    drawn: String,
}

/// `(main, seconds, suffix)`: `("2:05", Some(":09"), Some("PM"))`.
pub fn format_time(
    t: &LocalTime,
    hour24: bool,
    seconds: bool,
) -> (String, Option<String>, Option<&'static str>) {
    let sec = seconds.then(|| format!(":{:02}", t.second));
    if hour24 {
        (format!("{:02}:{:02}", t.hour, t.minute), sec, None)
    } else {
        let h = match t.hour % 12 {
            0 => 12,
            h => h,
        };
        (
            format!("{h}:{:02}", t.minute),
            sec,
            Some(if t.hour < 12 { "AM" } else { "PM" }),
        )
    }
}

impl Clock {
    pub fn new(cfg: ClockCfg) -> Clock {
        Clock {
            cfg,
            drawn: String::new(),
        }
    }

    fn hour24(&self, env: &Env) -> bool {
        match self.cfg.hour_format {
            HourFormat::System => env.system_24h,
            HourFormat::H12 => false,
            HourFormat::H24 => true,
        }
    }

    fn key(&self, env: &Env) -> String {
        let (m, s, x) = format_time(&env.local, self.hour24(env), self.cfg.show_seconds);
        format!(
            "{m}{}{}{}-{}",
            s.unwrap_or_default(),
            x.unwrap_or_default(),
            env.local.day,
            env.local.month
        )
    }
}

impl Module for Clock {
    fn id(&self) -> ModuleId {
        "clock"
    }

    fn title(&self) -> &'static str {
        "Clock"
    }

    fn icon(&self) -> Icon {
        Icon::Clock
    }

    fn poll_interval(&self) -> Option<Duration> {
        // Only honoured while the page is open. Seconds need 1 Hz; otherwise a few seconds of slack
        // on the minute rollover is invisible.
        Some(Duration::from_secs(if self.cfg.show_seconds {
            1
        } else {
            5
        }))
    }

    fn expanded_size(&self) -> Size {
        Size::new(344.0, 112.0)
    }

    fn peek_size(&self) -> Option<Size> {
        Some(Size::new(274.0, 54.0))
    }

    fn on_poll(&mut self, cx: &mut Cx) {
        if self.key(cx.env) != self.drawn {
            cx.request_redraw();
        }
    }

    fn on_config(&mut self, cfg: &Config, cx: &mut Cx) {
        self.cfg = cfg.clock.clone();
        cx.request_redraw();
    }

    fn draw_peek(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let th = *cv.theme;
        let t = &dx.env.local;
        let (main, _, suffix) = format_time(t, self.hour24(dx.env), false);
        let time = match suffix {
            Some(x) => format!("{main} {x}"),
            None => main,
        };
        let (left, right) = area.split_left(area.w * 0.46);
        cv.text(
            left,
            time,
            TextStyle::new(22.0, Weight::SemiBold).tabular(),
            th.text,
        );
        let date = format!(
            "{} {} {}",
            WEEKDAYS_SHORT[t.weekday() as usize],
            t.day,
            MONTHS_SHORT[t.month as usize - 1]
        );
        cv.text(
            right,
            date,
            TextStyle::label().align(Align::End),
            th.text_dim,
        );
    }

    fn draw_expanded(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let th = *cv.theme;
        let t = &dx.env.local;
        self.drawn = self.key(dx.env);
        let (main, sec, suffix) = format_time(t, self.hour24(dx.env), self.cfg.show_seconds);

        let left = Rect::new(area.x, area.y, 150.0, area.h);
        cv.text(
            Rect::new(left.x, left.y, left.w, 46.0),
            main,
            TextStyle::new(40.0, Weight::SemiBold).tabular(),
            th.text,
        );
        let sub = [suffix.map(str::to_string), sec]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("  ");
        if !sub.is_empty() {
            cv.text(
                Rect::new(left.x + 3.0, left.y + 46.0, left.w, 20.0),
                sub,
                TextStyle::label().tabular(),
                th.accent,
            );
        }

        let rw = (area.w - 150.0).max(0.0);
        let right = Rect::new(area.right() - rw, area.y, rw, area.h);
        let end = |s: TextStyle| s.align(Align::End);
        cv.text(
            Rect::new(right.x, right.y + 2.0, right.w, 18.0),
            WEEKDAYS[t.weekday() as usize],
            end(TextStyle::label()),
            th.text_dim,
        );
        cv.text(
            Rect::new(right.x, right.y + 20.0, right.w, 24.0),
            format!("{} {}", t.day, MONTHS[t.month as usize - 1]),
            end(TextStyle::title()),
            th.text,
        );
        let year_line = if self.cfg.show_week {
            format!("{} · Week {}", t.year, t.iso_week().1)
        } else {
            t.year.to_string()
        };
        cv.text(
            Rect::new(right.x, right.y + 46.0, right.w, 18.0),
            year_line,
            end(TextStyle::caption()),
            th.text_faint,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::{DrawCmd, DrawList};
    use crate::events::Source;
    use crate::module::{Out, Visibility};
    use crate::theme::Theme;

    fn at(h: u32, mi: u32, s: u32) -> Env {
        Env {
            local: LocalTime::new(2026, 10, 6, h, mi, s),
            unix: 0,
            system_24h: true,
            audio: Audio::Idle,
        }
    }

    fn texts(list: &DrawList) -> Vec<String> {
        list.cmds
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

    fn draw(clock: &mut Clock, env: &Env) -> Vec<String> {
        let theme = Theme::default();
        let mut list = DrawList::new();
        let cfg = Config::default();
        let mut cv = Canvas::new(&mut list, &theme);
        clock.draw_expanded(
            &mut cv,
            Rect::new(0.0, 0.0, 284.0, 72.0),
            &DrawCx {
                now: 0.0,
                env,
                config: &cfg,
            },
        );
        texts(&list)
    }

    #[test]
    fn twenty_four_and_twelve_hour_formats() {
        let t = |h| LocalTime::new(2026, 10, 6, h, 5, 9);
        assert_eq!(
            format_time(&t(14), true, false),
            ("14:05".into(), None, None)
        );
        assert_eq!(format_time(&t(0), true, false).0, "00:05");
        assert_eq!(
            format_time(&t(14), false, false),
            ("2:05".into(), None, Some("PM"))
        );
        assert_eq!(
            format_time(&t(0), false, false),
            ("12:05".into(), None, Some("AM")),
            "midnight is 12 AM"
        );
        assert_eq!(
            format_time(&t(12), false, false),
            ("12:05".into(), None, Some("PM")),
            "noon is 12 PM"
        );
        assert_eq!(
            format_time(&t(9), false, true),
            ("9:05".into(), Some(":09".into()), Some("AM"))
        );
    }

    #[test]
    fn draws_time_date_and_week() {
        let mut c = Clock::new(ClockCfg {
            hour_format: HourFormat::H24,
            ..Default::default()
        });
        let t = draw(&mut c, &at(14, 32, 7));
        assert_eq!(t, vec!["14:32", "Tuesday", "6 October", "2026 · Week 41"]);
    }

    #[test]
    fn peek_is_a_compact_time_and_date() {
        let mut c = Clock::new(ClockCfg {
            hour_format: HourFormat::H24,
            ..Default::default()
        });
        let theme = Theme::default();
        let cfg = Config::default();
        let mut list = DrawList::new();
        let env = at(14, 32, 7);
        {
            let mut cv = Canvas::new(&mut list, &theme);
            c.draw_peek(
                &mut cv,
                Rect::new(0.0, 0.0, 214.0, 26.0),
                &DrawCx {
                    now: 0.0,
                    env: &env,
                    config: &cfg,
                },
            );
        }
        assert_eq!(texts(&list), vec!["14:32", "Tue 6 Oct"]);
        assert!(c.peek_size().is_some());
    }

    #[test]
    fn hour_format_follows_the_system_unless_overridden() {
        let mut env = at(15, 0, 0);
        env.system_24h = false;
        let mut sys = Clock::new(ClockCfg::default());
        assert!(
            draw(&mut sys, &env).contains(&"3:00".to_string()),
            "system says 12h"
        );
        assert!(draw(&mut sys, &env).contains(&"PM".to_string()));
        let mut forced = Clock::new(ClockCfg {
            hour_format: HourFormat::H24,
            ..Default::default()
        });
        assert!(draw(&mut forced, &env).contains(&"15:00".to_string()));
    }

    #[test]
    fn seconds_and_week_are_optional() {
        let mut c = Clock::new(ClockCfg {
            show_seconds: true,
            show_week: false,
            hour_format: HourFormat::H24,
            ..Default::default()
        });
        let t = draw(&mut c, &at(8, 1, 2));
        assert!(t.contains(&":02".to_string()));
        assert!(t.contains(&"2026".to_string()), "year only, no week");
        assert_eq!(c.poll_interval(), Some(Duration::from_secs(1)));
        assert_eq!(
            Clock::new(ClockCfg::default()).poll_interval(),
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn poll_asks_for_a_redraw_only_when_the_display_would_change() {
        let mut c = Clock::new(ClockCfg {
            hour_format: HourFormat::H24,
            ..Default::default()
        });
        draw(&mut c, &at(10, 15, 0));
        let theme = Theme::default();
        let cfg = Config::default();
        let mut out = Out::default();
        let mut poll = |c: &mut Clock, env: &Env| {
            let mut cx = Cx::for_test(1.0, env, &theme, &cfg, &mut out);
            c.on_poll(&mut cx);
            std::mem::take(&mut out.redraw)
        };
        assert!(
            !poll(&mut c, &at(10, 15, 40)),
            "same minute: nothing to redraw"
        );
        assert!(poll(&mut c, &at(10, 16, 0)), "minute rolled over");
        draw(&mut c, &at(10, 16, 0));
        assert!(!poll(&mut c, &at(10, 16, 30)));
        assert!(
            poll(
                &mut c,
                &Env {
                    local: LocalTime::new(2026, 10, 7, 10, 16, 30),
                    unix: 0,
                    system_24h: true,
                    audio: Audio::Idle
                }
            ),
            "date change also redraws"
        );
    }

    #[test]
    fn disabled_in_config_means_never_instantiated() {
        let mut cfg = Config::default();
        assert!(create(&cfg).is_some());
        cfg.clock.enabled = false;
        assert!(create(&cfg).is_none());
    }

    #[test]
    fn works_inside_the_host_end_to_end() {
        use crate::module::ModuleHost;
        use std::sync::Arc;
        // Only the clock: the other built-in pages are switched off for this test.
        let mut only_clock = Config::default();
        only_clock.modules.order = vec!["clock".into()];
        let host_cfg = Arc::new(only_clock);
        let mut h = ModuleHost::new(crate::modules::registry(), host_cfg, Theme::default());
        assert_eq!(h.page_ids(), vec!["clock"]);
        assert_eq!(h.pages(), vec![Size::new(344.0, 112.0)]);
        h.set_context(100.0, at(10, 0, 0));
        h.set_view(None);
        assert_eq!(
            h.next_deadline(),
            None,
            "collapsed: the clock polls nothing"
        );
        h.set_view(Some(0));
        assert_eq!(h.next_deadline(), Some(100.0));
        h.tick();
        h.set_context(106.0, at(10, 1, 0));
        h.tick();
        let out = h.take_out();
        assert!(out.redraw, "minute change while open requests a redraw");
        let _ = (Source::Local, Visibility::Hidden);
    }
}
