//! Command centre: volume, brightness, Wi-Fi, Bluetooth, a snip button and Windows' Focus state.
//!
//! The page is a view over what the platform reports (`Control` events) plus optimistic feedback:
//! a slider follows the pointer at once and a toggle flips at once, and the platform's next reading
//! only takes over again after a short hold, so a reading that was already in flight cannot snap
//! the control back under the user's finger.
//!
//! Like the stats page, it asks for a reading only from the host's poll, which runs only while the
//! page is on screen. Controls a PC does not have say so instead of offering something that does
//! nothing, and the **Focus** tile only opens Windows' settings: Windows has no supported way for
//! another program to turn do-not-disturb on or off, and this does not use unsupported ones.

use std::sync::Arc;
use std::time::Duration;

use crate::color::Color;
use crate::config::{Config, ControlCfg};
use crate::draw::{Align, Canvas, CursorKind, HitId, Text, TextStyle, Weight};
use crate::events::{ControlState, Event, EventKind, EventMask, Kind, Radio};
use crate::geom::{Rect, Size, Vec2};
use crate::icons::Icon;
use crate::input::Input;
use crate::module::{Command, ControlCmd, Cx, DrawCx, Module, ModuleId, RadioKind, Visibility};

const HIT_WIFI: HitId = HitId(1);
const HIT_BLUETOOTH: HitId = HitId(2);
const HIT_FOCUS: HitId = HitId(3);
const HIT_SNIP: HitId = HitId(4);
const HIT_MUTE: HitId = HitId(5);
const HIT_VOLUME: HitId = HitId(6);
const HIT_BRIGHTNESS: HitId = HitId(7);

const GAP: f32 = 8.0;
const TILE_H: f32 = 64.0;
const ROW_H: f32 = 40.0;
/// While a slider is dragged its value is sent at most this often (seconds).
const SEND_EVERY: f64 = 0.05;
/// After a slider is released, or a toggle clicked, readings do not override it for this long.
const HOLD_SECS: f64 = 1.2;
/// A radio takes a moment to switch.
const RADIO_HOLD_SECS: f64 = 2.5;

pub fn create(cfg: &Config) -> Option<Box<dyn Module>> {
    cfg.control
        .enabled
        .then(|| Box::new(Control::new(cfg.control.clone())) as Box<dyn Module>)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Which {
    Volume,
    Brightness,
}

impl Which {
    fn index(self) -> usize {
        match self {
            Which::Volume => 0,
            Which::Brightness => 1,
        }
    }
}

/// Where everything is, for a page area.
struct Layout {
    wifi: Rect,
    bluetooth: Rect,
    focus: Rect,
    snip: Rect,
    mute: Rect,
    volume: Rect,
    volume_label: Rect,
    sun: Rect,
    brightness: Rect,
    brightness_label: Rect,
}

impl Layout {
    fn new(area: Rect) -> Layout {
        let tw = (area.w - 3.0 * GAP) / 4.0;
        let tile = |i: f32| Rect::new(area.x + i * (tw + GAP), area.y, tw, TILE_H);
        let row = |i: f32| area.y + TILE_H + GAP + 4.0 + i * (ROW_H + GAP);
        let slider = |y: f32| {
            Rect::new(
                area.x + 44.0,
                y + ROW_H * 0.5 - 4.0,
                area.w - 44.0 - 52.0,
                8.0,
            )
        };
        let label = |y: f32| Rect::new(area.right() - 46.0, y, 46.0, ROW_H);
        let (vy, by) = (row(0.0), row(1.0));
        Layout {
            wifi: tile(0.0),
            bluetooth: tile(1.0),
            focus: tile(2.0),
            snip: tile(3.0),
            mute: Rect::new(area.x + 2.0, vy + (ROW_H - 32.0) * 0.5, 32.0, 32.0),
            volume: slider(vy),
            volume_label: label(vy),
            sun: Rect::new(area.x + 2.0, by + (ROW_H - 32.0) * 0.5, 32.0, 32.0),
            brightness: slider(by),
            brightness_label: label(by),
        }
    }

    /// The pointer's target for a slider: a taller region than the 8 px track.
    fn grab(r: Rect) -> Rect {
        Rect::new(r.x - 6.0, r.y - 12.0, r.w + 12.0, r.h + 24.0)
    }

    fn slider(&self, w: Which) -> Rect {
        match w {
            Which::Volume => self.volume,
            Which::Brightness => self.brightness,
        }
    }
}

pub struct Control {
    cfg: ControlCfg,
    state: Option<Arc<ControlState>>,
    hover: Option<HitId>,
    /// The slider being dragged.
    drag: Option<Which>,
    /// What a slider shows while it is dragged and for a moment after (volume, brightness).
    shown: [Option<f32>; 2],
    /// Readings do not override a slider before this time.
    hold_until: [f64; 2],
    last_sent: [f64; 2],
    /// Optimistic mute and radio states, each with the time its hold ends.
    mute: Option<(bool, f64)>,
    wifi: Option<(bool, f64)>,
    bluetooth: Option<(bool, f64)>,
    lay: Option<Rect>,
}

impl Control {
    pub fn new(cfg: ControlCfg) -> Control {
        Control {
            cfg,
            state: None,
            hover: None,
            drag: None,
            shown: [None, None],
            hold_until: [0.0, 0.0],
            last_sent: [f64::NEG_INFINITY; 2],
            mute: None,
            wifi: None,
            bluetooth: None,
            lay: None,
        }
    }

    fn layout(&self) -> Option<Layout> {
        self.lay.map(Layout::new)
    }

    /// The level to show for a slider: the dragged / just-released value, else the reading.
    fn level(&self, w: Which, now: f64) -> Option<f32> {
        let reading = self.state.as_ref().and_then(|s| match w {
            Which::Volume => s.volume.map(|(v, _)| v),
            Which::Brightness => s.brightness,
        });
        reading?; // no such control: nothing to show, whatever was dragged
        if self.drag == Some(w) || now < self.hold_until[w.index()] {
            self.shown[w.index()].or(reading)
        } else {
            reading
        }
    }

    fn muted(&self, now: f64) -> bool {
        match self.mute {
            Some((m, until)) if now < until => m,
            _ => self
                .state
                .as_ref()
                .and_then(|s| s.volume)
                .is_some_and(|(_, m)| m),
        }
    }

    /// The effective state of a radio tile, including a toggle that was just clicked.
    fn radio(&self, kind: RadioKind, now: f64) -> Radio {
        let reading = self
            .state
            .as_ref()
            .map_or(Radio::Unavailable, |s| match kind {
                RadioKind::Wifi => s.wifi,
                RadioKind::Bluetooth => s.bluetooth,
            });
        let pending = match kind {
            RadioKind::Wifi => self.wifi,
            RadioKind::Bluetooth => self.bluetooth,
        };
        match (reading, pending) {
            (Radio::On | Radio::Off, Some((on, until))) if now < until => {
                if on {
                    Radio::On
                } else {
                    Radio::Off
                }
            }
            (r, _) => r,
        }
    }

    fn set_level(&mut self, w: Which, level: f32, cx: &mut Cx, force: bool) {
        let level = level.clamp(0.0, 1.0);
        self.shown[w.index()] = Some(level);
        if force || cx.now - self.last_sent[w.index()] >= SEND_EVERY {
            self.last_sent[w.index()] = cx.now;
            cx.command(Command::Control(match w {
                Which::Volume => ControlCmd::SetVolume(level),
                Which::Brightness => ControlCmd::SetBrightness(level),
            }));
        }
        cx.request_redraw();
    }

    fn click_radio(&mut self, kind: RadioKind, cx: &mut Cx) -> bool {
        match self.radio(kind, cx.now) {
            Radio::Unavailable => false,
            Radio::Denied => {
                cx.command(Command::Control(ControlCmd::OpenRadioSettings));
                true
            }
            Radio::Disabled => {
                cx.command(Command::Control(ControlCmd::OpenAirplaneSettings));
                true
            }
            current @ (Radio::On | Radio::Off) => {
                let on = current == Radio::Off;
                let slot = match kind {
                    RadioKind::Wifi => &mut self.wifi,
                    RadioKind::Bluetooth => &mut self.bluetooth,
                };
                *slot = Some((on, cx.now + RADIO_HOLD_SECS));
                cx.command(Command::Control(ControlCmd::SetRadio { kind, on }));
                cx.request_redraw();
                true
            }
        }
    }

    // ----- drawing --------------------------------------------------------------------------

    fn draw_tile(&self, cv: &mut Canvas, r: Rect, spec: TileSpec) {
        let TileSpec {
            id,
            icon,
            label,
            sub,
            look,
        } = spec;
        let th = *cv.theme;
        let hot = self.hover == Some(id) && look != TileLook::Unavailable;
        let (fill, icon_color, sub_color) = match look {
            TileLook::On => (
                th.accent.with_alpha(if hot { 0.34 } else { 0.24 }),
                th.accent,
                th.text_dim,
            ),
            TileLook::Off => (
                if hot { th.surface_hi } else { th.surface },
                th.text_dim,
                th.text_faint,
            ),
            TileLook::Warn => (
                if hot { th.surface_hi } else { th.surface },
                th.warn,
                th.warn,
            ),
            TileLook::Unavailable => (th.surface, th.text_faint, th.text_faint),
        };
        cv.squircle(r, 12.0, fill);
        cv.icon(
            icon,
            Rect::new(r.x + 10.0, r.y + 10.0, 22.0, 22.0),
            icon_color,
        );
        cv.text(
            Rect::new(r.x + 10.0, r.y + 36.0, r.w - 14.0, 15.0),
            label,
            TextStyle::new(12.0, Weight::SemiBold),
            if look == TileLook::Unavailable {
                th.text_faint
            } else {
                th.text
            },
        );
        cv.text(
            Rect::new(r.x + 10.0, r.y + 49.0, r.w - 14.0, 13.0),
            sub,
            TextStyle::new(10.5, Weight::Regular),
            sub_color,
        );
        if look != TileLook::Unavailable {
            cv.hit(r, id, CursorKind::Hand);
        }
    }

    fn draw_radio(&self, cv: &mut Canvas, r: Rect, id: HitId, kind: RadioKind, now: f64) {
        let (icon, label, none) = match kind {
            RadioKind::Wifi => (Icon::Wifi, "Wi-Fi", "No adapter"),
            RadioKind::Bluetooth => (Icon::Bluetooth, "Bluetooth", "No adapter"),
        };
        let (sub, look) = match (self.state.is_some(), self.radio(kind, now)) {
            (false, _) => ("…", TileLook::Unavailable),
            (true, Radio::On) => ("On", TileLook::On),
            (true, Radio::Off) => ("Off", TileLook::Off),
            (true, Radio::Denied) => ("Not allowed", TileLook::Warn),
            (true, Radio::Disabled) => ("Disabled", TileLook::Warn),
            (true, Radio::Unavailable) => (none, TileLook::Unavailable),
        };
        self.draw_tile(
            cv,
            r,
            TileSpec {
                id,
                icon,
                label,
                sub: Text::Static(sub),
                look,
            },
        );
    }

    fn draw_slider_row(&self, cv: &mut Canvas, row: SliderRow) {
        let SliderRow {
            icon_rect,
            icon,
            icon_hit,
            track,
            grab_id,
            label_rect,
            level,
            color,
            label,
        } = row;
        let th = *cv.theme;
        let active = level.is_some();
        let hot = icon_hit.is_some() && self.hover == icon_hit;
        if hot {
            cv.capsule(icon_rect, th.surface_hi);
        }
        cv.icon(
            icon,
            icon_rect.inset(6.0),
            if active { th.text } else { th.text_faint },
        );
        if let Some(id) = icon_hit.filter(|_| active) {
            cv.hit(icon_rect, id, CursorKind::Hand);
        }
        let f = level.unwrap_or(0.0);
        cv.bar(
            track,
            f,
            th.surface_hi,
            if active { color } else { th.surface },
        );
        if active {
            let dragging_here = matches!(
                (self.drag, grab_id),
                (Some(Which::Volume), HIT_VOLUME) | (Some(Which::Brightness), HIT_BRIGHTNESS)
            );
            if dragging_here || self.hover == Some(grab_id) {
                cv.circle(
                    Vec2::new(track.x + track.w * f, track.center().y),
                    7.0,
                    th.text,
                );
            }
            cv.hit(Layout::grab(track), grab_id, CursorKind::Hand);
        }
        cv.text(
            label_rect,
            label,
            TextStyle::new(12.5, Weight::SemiBold)
                .align(Align::End)
                .tabular(),
            if active { th.text } else { th.text_faint },
        );
    }
}

/// What a toggle tile shows.
struct TileSpec {
    id: HitId,
    icon: Icon,
    label: &'static str,
    sub: Text,
    look: TileLook,
}

/// What a slider row shows.
struct SliderRow {
    icon_rect: Rect,
    icon: Icon,
    /// The icon is also a button (the speaker mutes).
    icon_hit: Option<HitId>,
    track: Rect,
    grab_id: HitId,
    label_rect: Rect,
    /// `None`: this PC has no such control.
    level: Option<f32>,
    color: Color,
    label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TileLook {
    On,
    Off,
    /// Needs the user's attention (a privacy setting blocks it); still clickable.
    Warn,
    /// Nothing to do on this PC, or nothing known yet.
    Unavailable,
}

impl Module for Control {
    fn id(&self) -> ModuleId {
        "control"
    }

    fn title(&self) -> &'static str {
        "Controls"
    }

    fn icon(&self) -> Icon {
        Icon::Speaker
    }

    fn subscriptions(&self) -> EventMask {
        EventMask::of(&[Kind::Control])
    }

    /// Honoured only while the page is on screen.
    fn poll_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs_f32(
            self.cfg.interval_secs.clamp(0.5, 5.0),
        ))
    }

    fn expanded_size(&self) -> Size {
        Size::new(424.0, 198.0)
    }

    fn on_poll(&mut self, cx: &mut Cx) {
        // Not while a slider is being dragged: the user is the source of truth then.
        if self.drag.is_none() {
            cx.command(Command::Control(ControlCmd::Refresh));
        }
    }

    fn on_event(&mut self, ev: &Event, cx: &mut Cx) {
        if let EventKind::Control(s) = &ev.kind {
            self.state = Some(s.clone());
            cx.request_redraw();
        }
    }

    fn on_visibility(&mut self, _v: Visibility, _cx: &mut Cx) {
        self.state = None;
        self.hover = None;
        self.drag = None;
        self.shown = [None, None];
        self.hold_until = [0.0, 0.0];
        self.mute = None;
        self.wifi = None;
        self.bluetooth = None;
    }

    fn on_config(&mut self, cfg: &Config, cx: &mut Cx) {
        self.cfg = cfg.control.clone();
        cx.request_redraw();
    }

    fn on_input(&mut self, hit: Option<HitId>, input: &Input, cx: &mut Cx) -> bool {
        let Some(lay) = self.layout() else {
            return false;
        };
        match input {
            Input::Move(p) | Input::Drag { pos: p, .. } => {
                if let Some(w) = self.drag {
                    let r = lay.slider(w);
                    let level = ((p.x - r.x) / r.w).clamp(0.0, 1.0);
                    self.set_level(w, level, cx, false);
                    return true;
                }
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
            Input::Down(p) => {
                let w = match hit {
                    Some(HIT_VOLUME) => Which::Volume,
                    Some(HIT_BRIGHTNESS) => Which::Brightness,
                    _ => return false,
                };
                if self.level(w, cx.now).is_none() {
                    return false;
                }
                let r = lay.slider(w);
                self.drag = Some(w);
                self.set_level(w, ((p.x - r.x) / r.w).clamp(0.0, 1.0), cx, true);
                true
            }
            Input::Up(p) => {
                let Some(w) = self.drag.take() else {
                    return false;
                };
                let r = lay.slider(w);
                self.set_level(w, ((p.x - r.x) / r.w).clamp(0.0, 1.0), cx, true);
                self.hold_until[w.index()] = cx.now + HOLD_SECS;
                true
            }
            Input::Click(_) => {
                match hit {
                    Some(HIT_WIFI) => self.click_radio(RadioKind::Wifi, cx),
                    Some(HIT_BLUETOOTH) => self.click_radio(RadioKind::Bluetooth, cx),
                    Some(HIT_FOCUS) => {
                        cx.command(Command::Control(ControlCmd::OpenFocusSettings));
                        true
                    }
                    Some(HIT_SNIP) => {
                        cx.command(Command::Control(ControlCmd::Snip));
                        true
                    }
                    Some(HIT_MUTE) => {
                        let now_muted = self.muted(cx.now);
                        self.mute = Some((!now_muted, cx.now + HOLD_SECS));
                        cx.command(Command::Control(ControlCmd::ToggleMute));
                        cx.request_redraw();
                        true
                    }
                    // A click that ends a slider drag is consumed by `Up`; a plain click on the
                    // track is the same as pressing it, which `Down` already handled.
                    Some(HIT_VOLUME | HIT_BRIGHTNESS) => true,
                    _ => false,
                }
            }
            _ => false,
        }
    }

    fn draw_expanded(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let th = *cv.theme;
        self.lay = Some(area);
        let lay = Layout::new(area);
        let now = dx.now;
        let known = self.state.is_some();

        self.draw_radio(cv, lay.wifi, HIT_WIFI, RadioKind::Wifi, now);
        self.draw_radio(cv, lay.bluetooth, HIT_BLUETOOTH, RadioKind::Bluetooth, now);

        let (focus_sub, focus_look) = match self.state.as_ref().and_then(|s| s.dnd) {
            Some(true) => ("On · settings", TileLook::On),
            Some(false) => ("Off · settings", TileLook::Off),
            None if known => ("Settings", TileLook::Off),
            None => ("…", TileLook::Unavailable),
        };
        self.draw_tile(
            cv,
            lay.focus,
            TileSpec {
                id: HIT_FOCUS,
                icon: Icon::Moon,
                label: "Focus",
                sub: Text::Static(focus_sub),
                look: focus_look,
            },
        );
        self.draw_tile(
            cv,
            lay.snip,
            TileSpec {
                id: HIT_SNIP,
                icon: Icon::Snip,
                label: "Snip",
                sub: Text::Static("Screenshot"),
                look: TileLook::Off,
            },
        );

        // Volume
        let muted = self.muted(now);
        let vol = self.level(Which::Volume, now);
        let (vol_icon, vol_color) = if muted {
            (Icon::SpeakerMuted, th.text_faint)
        } else {
            (Icon::Speaker, th.accent)
        };
        let vol_label = match (known, vol) {
            (false, _) => String::new(),
            (true, None) => "—".to_string(),
            (true, Some(_)) if muted => "Muted".to_string(),
            (true, Some(v)) => format!("{:.0}%", v * 100.0),
        };
        self.draw_slider_row(
            cv,
            SliderRow {
                icon_rect: lay.mute,
                icon: vol_icon,
                icon_hit: Some(HIT_MUTE),
                track: lay.volume,
                grab_id: HIT_VOLUME,
                label_rect: lay.volume_label,
                level: vol,
                color: vol_color,
                label: vol_label,
            },
        );
        if known && vol.is_none() {
            cv.text(
                Rect::new(lay.volume.x, lay.volume.y - 16.0, lay.volume.w + 40.0, 14.0),
                "No audio output device",
                TextStyle::caption(),
                th.text_faint,
            );
        }

        // Brightness
        let bri = self.level(Which::Brightness, now);
        let bri_label = match (known, bri) {
            (false, _) => String::new(),
            (true, None) => "—".to_string(),
            (true, Some(b)) => format!("{:.0}%", b * 100.0),
        };
        self.draw_slider_row(
            cv,
            SliderRow {
                icon_rect: lay.sun,
                icon: Icon::Sun,
                icon_hit: None,
                track: lay.brightness,
                grab_id: HIT_BRIGHTNESS,
                label_rect: lay.brightness_label,
                level: bri,
                color: th.warn,
                label: bri_label,
            },
        );
        if known && bri.is_none() {
            cv.text(
                Rect::new(
                    lay.brightness.x,
                    lay.brightness.y - 16.0,
                    lay.brightness.w + 40.0,
                    14.0,
                ),
                "Brightness: not available on this display",
                TextStyle::caption(),
                th.text_faint,
            );
        }
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

    const AREA: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 372.0,
        h: 174.0,
    };

    struct T {
        m: Control,
        theme: Theme,
        cfg: Config,
        env: Env,
        out: Out,
        now: f64,
    }

    fn state() -> ControlState {
        ControlState {
            volume: Some((0.4, false)),
            brightness: Some(0.7),
            wifi: Radio::On,
            bluetooth: Radio::Off,
            dnd: Some(false),
        }
    }

    impl T {
        fn new() -> T {
            let mut t = T {
                m: Control::new(ControlCfg::default()),
                theme: Theme::default(),
                cfg: Config::default(),
                env: Env::default(),
                out: Out::default(),
                now: 10.0,
            };
            t.draw(); // lays the page out, as the first frame does
            t
        }

        fn feed(&mut self, s: ControlState) {
            let ev = Event::new(
                crate::events::Source::Local,
                EventKind::Control(Arc::new(s)),
            );
            let mut cx = cx!(self);
            self.m.on_event(&ev, &mut cx);
        }

        fn draw(&mut self) -> DrawList {
            let mut list = DrawList::new();
            let mut cv = Canvas::new(&mut list, &self.theme);
            let dx = DrawCx {
                now: self.now,
                env: &self.env,
                config: &self.cfg,
            };
            self.m.draw_expanded(&mut cv, AREA, &dx);
            assert!(list.is_balanced());
            list
        }

        fn texts(&mut self) -> Vec<String> {
            self.draw()
                .cmds
                .iter()
                .filter_map(|c| match c {
                    DrawCmd::Text { text, .. } => Some(text.as_str().to_string()),
                    _ => None,
                })
                .collect()
        }

        fn input(&mut self, hit: Option<HitId>, i: Input) -> bool {
            let mut cx = cx!(self);
            self.m.on_input(hit, &i, &mut cx)
        }

        fn click(&mut self, id: HitId) -> bool {
            self.input(Some(id), Input::Click(Vec2::ZERO))
        }

        fn commands(&mut self) -> Vec<Command> {
            std::mem::take(&mut self.out.commands)
        }

        /// The level of the one volume / brightness command in `cmds`.
        fn level_sent(cmds: &[Command]) -> f32 {
            match cmds {
                [Command::Control(ControlCmd::SetVolume(v) | ControlCmd::SetBrightness(v))] => *v,
                other => panic!("expected one level command, got {other:?}"),
            }
        }

        fn at_x(&self, w: Which, f: f32) -> Vec2 {
            let r = Layout::new(AREA).slider(w);
            Vec2::new(r.x + r.w * f, r.center().y)
        }
    }

    #[test]
    fn it_asks_for_a_reading_only_from_the_hosts_poll() {
        let mut t = T::new();
        assert_eq!(t.m.poll_interval(), Some(Duration::from_secs(1)));
        {
            let mut cx = cx!(t);
            t.m.on_poll(&mut cx);
        }
        assert_eq!(t.commands(), vec![Command::Control(ControlCmd::Refresh)]);
        t.m.cfg.interval_secs = 99.0;
        assert_eq!(t.m.poll_interval(), Some(Duration::from_secs(5)));
        // Not while a slider is being dragged.
        t.feed(state());
        t.input(Some(HIT_VOLUME), Input::Down(t.at_x(Which::Volume, 0.5)));
        t.commands();
        {
            let mut cx = cx!(t);
            t.m.on_poll(&mut cx);
        }
        assert!(t.commands().is_empty());
    }

    #[test]
    fn before_a_reading_nothing_is_offered() {
        let mut t = T::new();
        let list = t.draw();
        assert!(
            !list
                .hits
                .iter()
                .any(|h| matches!(h.id, HIT_WIFI | HIT_BLUETOOTH | HIT_FOCUS)
                    || h.id == HIT_VOLUME
                    || h.id == HIT_BRIGHTNESS),
            "no control is offered before anything is known"
        );
        let tx = t.texts();
        assert!(tx.contains(&"Wi-Fi".to_string()) && tx.contains(&"Bluetooth".to_string()));
    }

    #[test]
    fn a_reading_fills_the_page() {
        let mut t = T::new();
        t.feed(state());
        let tx = t.texts();
        for want in [
            "Wi-Fi",
            "On",
            "Bluetooth",
            "Off",
            "Focus",
            "Off · settings",
            "Snip",
            "Screenshot",
            "40%",
            "70%",
        ] {
            assert!(tx.contains(&want.to_string()), "{want}: {tx:?}");
        }
        let list = t.draw();
        for id in [
            HIT_WIFI,
            HIT_BLUETOOTH,
            HIT_FOCUS,
            HIT_SNIP,
            HIT_MUTE,
            HIT_VOLUME,
            HIT_BRIGHTNESS,
        ] {
            assert!(list.hits.iter().any(|h| h.id == id), "{id:?} has a region");
        }
    }

    #[test]
    fn controls_this_pc_does_not_have_say_so_and_are_not_clickable() {
        let mut t = T::new();
        t.feed(ControlState {
            volume: None,
            brightness: None,
            wifi: Radio::Unavailable,
            bluetooth: Radio::Unavailable,
            dnd: None,
        });
        let tx = t.texts();
        for want in [
            "No adapter",
            "No audio output device",
            "Brightness: not available on this display",
        ] {
            assert!(tx.contains(&want.to_string()), "{want}: {tx:?}");
        }
        let list = t.draw();
        for id in [
            HIT_WIFI,
            HIT_BLUETOOTH,
            HIT_VOLUME,
            HIT_BRIGHTNESS,
            HIT_MUTE,
        ] {
            assert!(
                !list.hits.iter().any(|h| h.id == id),
                "{id:?} must not be offered"
            );
        }
        assert!(!t.click(HIT_WIFI));
        assert!(t.commands().is_empty());
    }

    #[test]
    fn a_radio_toggle_flips_at_once_and_a_late_reading_does_not_flip_it_back() {
        let mut t = T::new();
        t.feed(state()); // Wi-Fi on
        assert!(t.click(HIT_WIFI));
        assert_eq!(
            t.commands(),
            vec![Command::Control(ControlCmd::SetRadio {
                kind: RadioKind::Wifi,
                on: false
            })]
        );
        assert!(
            t.texts().iter().filter(|s| *s == "Off").count() >= 2,
            "optimistic: Off now"
        );
        // A reading that was already in flight still says On: the tile holds.
        t.feed(state());
        assert!(t.texts().iter().filter(|s| *s == "Off").count() >= 2);
        // Once the hold is over the reading rules again.
        t.now += RADIO_HOLD_SECS + 0.1;
        let tx = t.texts();
        assert_eq!(tx.iter().filter(|s| *s == "On").count(), 1, "{tx:?}");
        // Bluetooth turns on the same way.
        assert!(t.click(HIT_BLUETOOTH));
        assert_eq!(
            t.commands(),
            vec![Command::Control(ControlCmd::SetRadio {
                kind: RadioKind::Bluetooth,
                on: true
            })]
        );
    }

    #[test]
    fn a_radio_refused_by_a_privacy_setting_opens_that_setting() {
        let mut t = T::new();
        let mut s = state();
        s.wifi = Radio::Denied;
        t.feed(s);
        assert!(t.texts().contains(&"Not allowed".to_string()));
        assert!(t.click(HIT_WIFI));
        assert_eq!(
            t.commands(),
            vec![Command::Control(ControlCmd::OpenRadioSettings)]
        );
    }

    #[test]
    fn a_radio_that_is_switched_off_beyond_the_apps_reach_says_so_and_opens_the_setting() {
        let mut t = T::new();
        let mut s = state();
        s.bluetooth = Radio::Disabled;
        t.feed(s);
        assert!(t.texts().contains(&"Disabled".to_string()));
        assert!(t.click(HIT_BLUETOOTH));
        assert_eq!(
            t.commands(),
            vec![Command::Control(ControlCmd::OpenAirplaneSettings)]
        );
    }

    #[test]
    fn dragging_the_volume_follows_the_pointer_and_sends_the_final_value() {
        let mut t = T::new();
        t.feed(state());
        // Press at 25 %: shown and sent at once.
        assert!(t.input(Some(HIT_VOLUME), Input::Down(t.at_x(Which::Volume, 0.25))));
        assert_eq!(
            t.commands(),
            vec![Command::Control(ControlCmd::SetVolume(0.25))]
        );
        assert!(t.texts().contains(&"25%".to_string()));
        // Moves inside the throttle window are shown but not all sent.
        t.input(
            None,
            Input::Drag {
                start: Vec2::ZERO,
                pos: t.at_x(Which::Volume, 0.5),
            },
        );
        assert!(t.commands().is_empty(), "throttled");
        assert!(t.texts().contains(&"50%".to_string()));
        t.now += SEND_EVERY + 0.01;
        t.input(
            None,
            Input::Drag {
                start: Vec2::ZERO,
                pos: t.at_x(Which::Volume, 0.6),
            },
        );
        assert_eq!(
            t.commands(),
            vec![Command::Control(ControlCmd::SetVolume(0.6))]
        );
        // Past the ends it clamps; release always sends the final value.
        assert!(t.input(None, Input::Up(Vec2::new(10_000.0, 0.0))));
        assert_eq!(
            t.commands(),
            vec![Command::Control(ControlCmd::SetVolume(1.0))]
        );
        // A reading in flight with the old value does not snap it back…
        t.feed(state());
        assert!(t.texts().contains(&"100%".to_string()));
        // …until the hold is over.
        t.now += HOLD_SECS + 0.1;
        assert!(t.texts().contains(&"40%".to_string()));
    }

    #[test]
    fn the_pointer_outside_a_drag_does_nothing_and_a_missing_control_cannot_be_dragged() {
        let mut t = T::new();
        t.feed(state());
        assert!(
            !t.input(None, Input::Up(Vec2::ZERO)),
            "no drag, nothing to finish"
        );
        assert!(!t.input(Some(HitId(99)), Input::Down(Vec2::ZERO)));
        let mut s = state();
        s.brightness = None;
        t.feed(s);
        assert!(!t.input(Some(HIT_BRIGHTNESS), Input::Down(Vec2::ZERO)));
        assert!(t.commands().is_empty());
    }

    #[test]
    fn brightness_works_like_volume() {
        let mut t = T::new();
        t.feed(state());
        assert!(t.input(
            Some(HIT_BRIGHTNESS),
            Input::Down(t.at_x(Which::Brightness, 0.1))
        ));
        let cmds = t.commands();
        assert!(matches!(
            cmds[..],
            [Command::Control(ControlCmd::SetBrightness(_))]
        ));
        assert!((T::level_sent(&cmds) - 0.1).abs() < 1e-4);
        assert!(t.input(None, Input::Up(t.at_x(Which::Brightness, 0.3))));
        let cmds = t.commands();
        assert!(matches!(
            cmds[..],
            [Command::Control(ControlCmd::SetBrightness(_))]
        ));
        assert!((T::level_sent(&cmds) - 0.3).abs() < 1e-4);
        assert!(t.texts().contains(&"30%".to_string()));
    }

    #[test]
    fn the_speaker_button_mutes_and_shows_it_at_once() {
        let mut t = T::new();
        t.feed(state());
        assert!(t.click(HIT_MUTE));
        assert_eq!(t.commands(), vec![Command::Control(ControlCmd::ToggleMute)]);
        assert!(t.texts().contains(&"Muted".to_string()));
        // A stale reading (unmuted) does not undo it during the hold; later it is the truth.
        t.feed(state());
        assert!(t.texts().contains(&"Muted".to_string()));
        t.now += HOLD_SECS + 0.1;
        assert!(t.texts().contains(&"40%".to_string()));
        // A muted reading shows Muted and a click unmutes.
        let mut s = state();
        s.volume = Some((0.4, true));
        t.feed(s);
        assert!(t.texts().contains(&"Muted".to_string()));
        assert!(t.click(HIT_MUTE));
        assert!(t.texts().contains(&"40%".to_string()), "optimistic unmute");
    }

    #[test]
    fn focus_and_snip_are_buttons_and_focus_only_opens_settings() {
        let mut t = T::new();
        let mut s = state();
        s.dnd = Some(true);
        t.feed(s);
        assert!(t.texts().contains(&"On · settings".to_string()));
        assert!(t.click(HIT_FOCUS));
        assert!(t.click(HIT_SNIP));
        assert_eq!(
            t.commands(),
            vec![
                Command::Control(ControlCmd::OpenFocusSettings),
                Command::Control(ControlCmd::Snip)
            ],
            "Focus is never *changed*: there is no command for that"
        );
        let mut s = state();
        s.dnd = None;
        t.feed(s);
        assert!(t.texts().contains(&"Settings".to_string()));
    }

    #[test]
    fn leaving_the_page_forgets_everything_so_reopening_starts_clean() {
        let mut t = T::new();
        t.feed(state());
        t.input(Some(HIT_VOLUME), Input::Down(t.at_x(Which::Volume, 0.5)));
        {
            let mut cx = cx!(t);
            t.m.on_visibility(Visibility::Collapsed, &mut cx);
        }
        assert!(t.m.state.is_none() && t.m.drag.is_none() && t.m.shown == [None, None]);
        assert!(!t.draw().hits.iter().any(|h| h.id == HIT_VOLUME));
    }

    #[test]
    fn disabled_means_never_instantiated_and_it_works_inside_the_host() {
        let mut cfg = Config::default();
        cfg.control.enabled = false;
        assert!(create(&cfg).is_none());

        use crate::module::ModuleHost;
        let mut cfg = Config::default();
        cfg.modules.order = vec!["control".into()];
        let mut host = ModuleHost::new(
            vec![crate::module::Factory {
                id: "control",
                create,
            }],
            Arc::new(cfg),
            Theme::default(),
        );
        host.set_context(10.0, Env::default());
        host.start_new();
        assert_eq!(host.pages().len(), 1);
        assert_eq!(host.chips_width(), 0.0);
        assert_eq!(host.next_deadline(), None, "hidden: nothing is scheduled");
    }
}
