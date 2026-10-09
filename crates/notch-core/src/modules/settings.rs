//! The settings page: one switch per module. A click writes `enabled` of that module's section into
//! `config.toml` (`Command::SetBool`); the file reload then adds or removes the module, so the page
//! shows what the file says, never a copy of its own.

use crate::config::Config;
use crate::draw::{Canvas, CursorKind, HitId, TextStyle};
use crate::geom::{Rect, Size, Vec2};
use crate::icons::Icon;
use crate::input::Input;
use crate::module::{Command, Cx, DrawCx, Module, ModuleId};

pub fn create(cfg: &Config) -> Option<Box<dyn Module>> {
    cfg.settings
        .enabled
        .then(|| Box::new(Settings) as Box<dyn Module>)
}

/// `(section and module id, label)` of every switch. The settings page itself has none: switching
/// it off from itself would leave no way back except the file.
pub const SWITCHES: [(&str, &str); 9] = [
    ("media", "Media"),
    ("clipboard", "Clipboard"),
    ("shelf", "Shelf"),
    ("notifications", "Notifications"),
    ("calendar", "Calendar"),
    ("pomodoro", "Focus"),
    ("live", "Live"),
    ("stats", "Stats"),
    ("control", "Controls"),
];
const CLOCK: (&str, &str) = ("clock", "Clock");

const COLS: usize = 2;
const ROW_H: f32 = 28.0;
const HEADER_H: f32 = 24.0;

pub struct Settings;

/// Every row, in draw order.
fn rows() -> impl Iterator<Item = (&'static str, &'static str)> {
    SWITCHES.into_iter().chain([CLOCK])
}

fn row_rect(area: Rect, k: usize) -> Rect {
    let w = area.w / COLS as f32;
    Rect::new(
        area.x + (k % COLS) as f32 * w,
        area.y + HEADER_H + (k / COLS) as f32 * ROW_H,
        w - 8.0,
        ROW_H - 4.0,
    )
}

impl Module for Settings {
    fn id(&self) -> ModuleId {
        "settings"
    }

    fn title(&self) -> &'static str {
        "Settings"
    }

    fn icon(&self) -> Icon {
        Icon::Gear
    }

    fn expanded_size(&self) -> Size {
        Size::new(400.0, 14.0 + HEADER_H + 6.0 * ROW_H + 26.0)
    }

    fn on_input(&mut self, hit: Option<HitId>, input: &Input, cx: &mut Cx) -> bool {
        let (Input::Click(_), Some(HitId(k))) = (input, hit) else {
            return false;
        };
        let Some((id, _)) = rows().nth(k as usize) else {
            return false;
        };
        cx.command(Command::SetBool {
            section: id,
            key: "enabled",
            value: !cx.config.module_enabled(id),
        });
        true
    }

    fn draw_expanded(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let th = *cv.theme;
        cv.text(
            Rect::new(area.x, area.y, area.w, HEADER_H),
            "Modules",
            TextStyle::label(),
            th.text_dim,
        );
        for (k, (id, label)) in rows().enumerate() {
            let r = row_rect(area, k);
            let on = dx.config.module_enabled(id);
            cv.round_rect(r, 8.0, th.surface);
            cv.text(
                Rect::new(r.x + 10.0, r.y, r.w - 56.0, r.h),
                label,
                TextStyle::body(),
                th.text,
            );
            let track = Rect::new(r.right() - 40.0, r.center().y - 8.0, 30.0, 16.0);
            cv.capsule(track, if on { th.accent } else { th.surface_hi });
            let kx = if on {
                track.right() - 8.0
            } else {
                track.x + 8.0
            };
            cv.circle(Vec2::new(kx, track.center().y), 6.0, th.text);
            cv.hit(r, HitId(k as u32), CursorKind::Hand);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::DrawList;
    use crate::module::{Env, Out};
    use crate::theme::Theme;

    #[test]
    fn a_click_asks_to_flip_that_modules_flag() {
        let (env, th) = (Env::default(), Theme::default());
        let mut cfg = Config::default();
        cfg.stats.enabled = false;
        let mut out = Out::default();
        let mut cx = Cx::for_test(0.0, &env, &th, &cfg, &mut out);
        let stats = rows().position(|(id, _)| id == "stats").unwrap() as u32;
        assert!(Settings.on_input(
            Some(HitId(stats)),
            &Input::Click(Default::default()),
            &mut cx
        ));
        let media = rows().position(|(id, _)| id == "media").unwrap() as u32;
        Settings.on_input(
            Some(HitId(media)),
            &Input::Click(Default::default()),
            &mut cx,
        );
        assert_eq!(
            out.commands,
            vec![
                Command::SetBool {
                    section: "stats",
                    key: "enabled",
                    value: true
                },
                Command::SetBool {
                    section: "media",
                    key: "enabled",
                    value: false
                },
            ]
        );
    }

    #[test]
    fn every_switch_is_a_section_with_an_enabled_flag_and_a_hit_region() {
        let th = Theme::default();
        let cfg = Config::default();
        let env = Env::default();
        let mut list = DrawList::new();
        let area = Rect::new(0.0, 0.0, 400.0, 200.0);
        Settings.draw_expanded(
            &mut Canvas::new(&mut list, &th),
            area,
            &DrawCx {
                now: 0.0,
                env: &env,
                config: &cfg,
            },
        );
        assert_eq!(list.hits.len(), SWITCHES.len() + 1);
        // The section named by each switch exists and parses with `enabled`.
        let mut text = String::new();
        for (id, _) in rows() {
            text += &format!("[{id}]\nenabled = false\n");
        }
        let c = Config::parse(&text).unwrap();
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        for (id, _) in rows() {
            assert!(!c.config.module_enabled(id), "{id}");
        }
        assert!(rows().all(|(id, _)| cfg.module_enabled(id)));
    }
}
