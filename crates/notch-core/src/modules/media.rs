//! The media page: album art, title and artist, a seekable progress bar, transport controls and a
//! level-reactive visualizer, driven by the system's current media session.
//!
//! * **Event driven.** Everything arrives as `MediaChanged` events from the platform's media
//!   service; nothing here polls the OS. The playback position is *extrapolated* from the last
//!   snapshot while playing, so a progress bar needs no timeline queries.
//! * **Cheap when idle.** No session means no page (it is not in the ring) and no chip. The
//!   visualizer asks for continuous frames only while it is playing **and** the page is open.
//! * **Honest visualizer.** The platform reports the loudness of the output device (a peak meter,
//!   not a spectrum); each bar follows that level with its own phase, so it reacts to the music
//!   without the cost of capturing and transforming audio.

use std::sync::Arc;
use std::time::Duration;

use crate::color::Color;
use crate::config::{Config, MediaCfg};
use crate::draw::{Align, Canvas, CursorKind, DrawCmd, HitId, ImageId, TextStyle, Weight};
use crate::events::{Event, EventKind, EventMask, Kind, MediaSnapshot, Source};
use crate::geom::{Rect, Size, Vec2};
use crate::icons::Icon;
use crate::input::Input;
use crate::module::{Audio, Command, Cx, DrawCx, MediaCmd, Module, ModuleId, Visibility};
use crate::theme::Theme;

const HIT_PREV: HitId = HitId(1);
const HIT_PLAY: HitId = HitId(2);
const HIT_NEXT: HitId = HitId(3);
const HIT_SEEK: HitId = HitId(4);

/// Number of visualizer bars.
pub const BARS: usize = 5;

/// How long an optimistic play/pause or a requested seek is shown before the real state wins.
const OPTIMISTIC_SECS: f64 = 1.5;
const SEEK_HOLD_SECS: f64 = 2.0;

pub fn create(cfg: &Config) -> Option<Box<dyn Module>> {
    cfg.media
        .enabled
        .then(|| Box::new(Media::new(cfg.media.clone())) as Box<dyn Module>)
}

/// `"Spotify.exe"` → `"Spotify"`, `"Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"` →
/// `"Media Player"`. Works on the app user model id Windows reports for the session.
pub fn friendly_app_name(aumid: &str) -> String {
    let id = aumid.trim();
    let lower = id.to_ascii_lowercase();
    let known: &[(&str, &str)] = &[
        ("spotify", "Spotify"),
        ("zunemusic", "Media Player"),
        ("zunevideo", "Films & TV"),
        ("applemusic", "Apple Music"),
        ("itunes", "iTunes"),
        ("msedge", "Edge"),
        ("chrome", "Chrome"),
        ("firefox", "Firefox"),
        ("brave", "Brave"),
        ("opera", "Opera"),
        ("vivaldi", "Vivaldi"),
        ("vlc", "VLC"),
        ("foobar", "foobar2000"),
        ("musicbee", "MusicBee"),
        ("tidal", "TIDAL"),
        ("amazonmusic", "Amazon Music"),
        ("deezer", "Deezer"),
        ("youtubemusic", "YouTube Music"),
        ("youtube", "YouTube"),
        ("netflix", "Netflix"),
        ("pandora", "Pandora"),
        ("soundcloud", "SoundCloud"),
        ("mpv", "mpv"),
        ("potplayer", "PotPlayer"),
        ("winamp", "Winamp"),
    ];
    if let Some((_, name)) = known.iter().find(|(k, _)| lower.contains(k)) {
        return (*name).to_string();
    }
    // Fall back to the last meaningful segment: "Vendor.App_hash!App" -> "App".
    let seg = id.rsplit(['!', '\\', '/']).next().unwrap_or(id);
    let seg = seg.split('_').next().unwrap_or(seg);
    let seg = seg.strip_suffix(".exe").unwrap_or(seg);
    let seg = seg.rsplit('.').next().unwrap_or(seg);
    let mut chars = seg.chars();
    match chars.next() {
        Some(c) if !seg.is_empty() => c.to_uppercase().collect::<String>() + chars.as_str(),
        _ => String::new(),
    }
}

/// `83_000` → `"1:23"`, `3_725_000` → `"1:02:05"`.
pub fn fmt_clock(ms: u64) -> String {
    let s = ms / 1000;
    let (h, m, s) = (s / 3600, s % 3600 / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Advance the visualizer bars one frame. `level` is the device's peak (0..=1), `t` the frame time
/// and `dt` the time since the previous frame. Attack is fast and release slow, both expressed as
/// exponential rates so the motion looks the same at 60, 120 or 144 Hz.
pub fn step_bars(bars: &mut [f32; BARS], level: f32, t: f64, dt: f32) {
    let level = level.clamp(0.0, 1.0);
    for (i, b) in bars.iter_mut().enumerate() {
        let wob = 0.5 + 0.5 * ((t * (2.3 + 0.7 * i as f64) + i as f64 * 1.9).sin() as f32);
        let target = (level.powf(0.6) * (0.35 + 0.65 * wob)).clamp(0.0, 1.0);
        let rate = if target > *b { 38.0 } else { 7.5 };
        *b += (target - *b) * (1.0 - (-rate * dt.clamp(0.0, 0.25)).exp());
    }
}

/// Geometry of the expanded page, computed from its content rect so input and drawing agree.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Layout {
    pub art: Rect,
    pub app: Rect,
    pub bars: Rect,
    pub title: Rect,
    pub artist: Rect,
    pub seek: Rect,
    pub seek_hit: Rect,
    pub time_left: Rect,
    pub time_right: Rect,
    pub prev: Rect,
    pub play: Rect,
    pub next: Rect,
}

impl Layout {
    pub fn new(area: Rect) -> Layout {
        let art_s = 92.0f32.min(area.h);
        let art = Rect::new(area.x, area.y, art_s, art_s);
        let x0 = art.right() + 16.0;
        let w = (area.right() - x0).max(0.0);
        let bars = Rect::new((area.right() - 29.0).max(x0), area.y + 1.0, 29.0, 12.0);
        let seek = Rect::new(x0, area.y + 63.0, w, 4.0);
        let cx = x0 + w * 0.5;
        let cy = area.y + 96.0;
        let btn = |c: f32, s: f32| Rect::new(cx + c - s * 0.5, cy - s * 0.5, s, s);
        Layout {
            art,
            app: Rect::new(x0, area.y, (w - 34.0).max(0.0), 14.0),
            bars,
            title: Rect::new(x0, area.y + 14.0, w, 22.0),
            artist: Rect::new(x0, area.y + 36.0, w, 19.0),
            seek,
            seek_hit: seek.inflate(0.0, 8.0),
            time_left: Rect::new(x0, area.y + 69.0, w * 0.5, 14.0),
            time_right: Rect::new(x0 + w * 0.5, area.y + 69.0, w * 0.5, 14.0),
            prev: btn(-50.0, 30.0),
            play: btn(0.0, 36.0),
            next: btn(50.0, 30.0),
        }
    }

    /// Fraction along the seek bar (clamped to 0..=1) for a pointer x coordinate.
    pub fn seek_frac(&self, x: f32) -> f32 {
        if self.seek.w <= 0.0 {
            return 0.0;
        }
        ((x - self.seek.x) / self.seek.w).clamp(0.0, 1.0)
    }
}

pub struct Media {
    cfg: MediaCfg,
    snap: Arc<MediaSnapshot>,
    /// `cx.now` when `snap` arrived: the origin of the position extrapolation.
    received_at: f64,
    /// The first snapshot after start-up describes what was already playing: it never peeks.
    seen_any: bool,
    hover: Option<HitId>,
    /// Fraction under the pointer while the seek bar is being dragged.
    scrub: Option<f32>,
    /// A seek we asked for: `(target ms, show until)`.
    pending_seek: Option<(u64, f64)>,
    /// A play/pause we toggled locally: `(playing, show until)`.
    optimistic: Option<(bool, f64)>,
    lay: Layout,
    bars: [f32; BARS],
    last_frame: f64,
    /// Cleared once the platform reports that no audio device can be metered.
    meter_ok: bool,
    polls: u32,
}

impl Media {
    pub fn new(cfg: MediaCfg) -> Media {
        Media {
            cfg,
            snap: Arc::new(MediaSnapshot::default()),
            received_at: 0.0,
            seen_any: false,
            hover: None,
            scrub: None,
            pending_seek: None,
            optimistic: None,
            lay: Layout::default(),
            bars: [0.0; BARS],
            last_frame: 0.0,
            meter_ok: true,
            polls: 0,
        }
    }

    pub fn snapshot(&self) -> &MediaSnapshot {
        &self.snap
    }

    fn playing(&self, now: f64) -> bool {
        match self.optimistic {
            Some((p, until)) if now < until => p,
            _ => self.snap.playing,
        }
    }

    /// Playback position now: the held seek target, else the snapshot position advanced by the time
    /// since it arrived while playing, never past the end.
    pub fn position_ms(&self, now: f64) -> u64 {
        if let Some((target, until)) = self.pending_seek
            && now < until
        {
            return target;
        }
        let mut p = self.snap.position_ms as f64;
        if self.snap.playing {
            p += (now - self.received_at).max(0.0) * 1000.0;
        }
        if self.snap.duration_ms > 0 {
            p = p.min(self.snap.duration_ms as f64);
        }
        p as u64
    }

    fn can_seek(&self) -> bool {
        self.snap.can_seek && self.snap.duration_ms > 0
    }

    fn accent(&self, th: &Theme) -> Color {
        match self.snap.accent {
            Some([r, g, b]) => Color::rgb8(r, g, b).ensure_contrast(th.bg, 3.0),
            None => th.accent,
        }
    }

    fn artist_line(&self) -> String {
        match (self.snap.artist.is_empty(), self.snap.album.is_empty()) {
            (false, false) => format!("{} — {}", self.snap.artist, self.snap.album),
            (false, true) => self.snap.artist.to_string(),
            (true, false) => self.snap.album.to_string(),
            (true, true) => String::new(),
        }
    }

    fn draw_art(&self, cv: &mut Canvas, rect: Rect, radius: f32, th: &Theme) {
        if self.snap.art != 0 {
            cv.image(ImageId(self.snap.art), rect, radius);
        } else {
            cv.squircle(rect, radius, th.surface_hi);
            cv.icon(
                Icon::Note,
                rect.centered(rect.w * 0.42, rect.h * 0.42),
                th.text_faint,
            );
        }
    }

    fn draw_bars(&self, cv: &mut Canvas, rect: Rect, color: Color, live: bool) {
        let (bw, gap) = (3.0, 3.5);
        let total = BARS as f32 * bw + (BARS as f32 - 1.0) * gap;
        let x0 = rect.right() - total;
        for (i, &v) in self.bars.iter().enumerate() {
            // Paused (or not yet sampled): a calm row of dots rather than frozen mid-motion bars.
            let v = if live { v } else { 0.0 };
            let h = 3.0 + v * (rect.h - 3.0);
            let x = x0 + i as f32 * (bw + gap);
            cv.capsule(Rect::new(x, rect.bottom() - h, bw, h), color);
        }
    }

    fn button(
        &self,
        cv: &mut Canvas,
        rect: Rect,
        icon: Icon,
        id: HitId,
        enabled: bool,
        th: &Theme,
    ) {
        if enabled && self.hover == Some(id) {
            cv.capsule(rect, th.surface);
        }
        let color = if enabled { th.text } else { th.text_faint };
        cv.icon(icon, rect.inset(rect.w * 0.2), color);
        if enabled {
            cv.hit(rect, id, CursorKind::Hand);
        }
    }
}

impl Module for Media {
    fn id(&self) -> ModuleId {
        "media"
    }

    fn title(&self) -> &'static str {
        "Media"
    }

    fn icon(&self) -> Icon {
        Icon::Note
    }

    fn subscriptions(&self) -> EventMask {
        EventMask::of(&[Kind::MediaChanged])
    }

    /// 1 Hz while the page is open: advance the clock labels when nothing else is drawing, and resync
    /// the position with the player every few seconds. (Honoured only while expanded.)
    fn poll_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(1))
    }

    /// Continuous frames for the visualizer: only while playing, enabled, and a device can be metered.
    fn wants_frames(&self) -> bool {
        self.cfg.visualizer && self.meter_ok && self.snap.playing && self.snap.is_active()
    }

    fn page_visible(&self) -> bool {
        self.snap.is_active()
    }

    fn expanded_size(&self) -> Size {
        Size::new(424.0, 152.0)
    }

    fn peek_size(&self) -> Option<Size> {
        Some(Size::new(354.0, 64.0))
    }

    fn on_event(&mut self, ev: &Event, cx: &mut Cx) {
        let EventKind::MediaChanged(new) = &ev.kind else {
            return;
        };
        if ev.source != Source::Local {
            return;
        }
        let changed_track = new.is_active() && !self.snap.same_track(new);
        let first = !self.seen_any;
        self.seen_any = true;
        // The real state replaces whatever we guessed.
        self.optimistic = None;
        if let Some((target, _)) = self.pending_seek
            && (changed_track || new.position_ms.abs_diff(target) < 2500)
        {
            self.pending_seek = None;
        }
        self.snap = new.clone();
        self.received_at = cx.now;
        cx.request_redraw();
        if changed_track && !first && new.playing && self.cfg.peek_on_change {
            cx.peek(f64::from(self.cfg.peek_secs));
        }
    }

    fn on_config(&mut self, cfg: &Config, cx: &mut Cx) {
        self.cfg = cfg.media.clone();
        cx.request_redraw();
    }

    fn on_visibility(&mut self, v: Visibility, cx: &mut Cx) {
        match v {
            Visibility::Expanded => {
                self.polls = 0;
                // Correct any drift and pick up a thumbnail that arrived after the track did.
                cx.command(Command::Media(MediaCmd::Refresh));
            }
            _ => {
                self.hover = None;
                self.scrub = None;
                self.bars = [0.0; BARS];
            }
        }
    }

    fn on_suspend(&mut self, _cx: &mut Cx) {
        self.hover = None;
        self.scrub = None;
        self.bars = [0.0; BARS];
    }

    fn on_poll(&mut self, cx: &mut Cx) {
        self.polls += 1;
        if self.snap.playing && self.scrub.is_none() && !self.wants_frames() {
            cx.request_redraw();
        }
        if self.polls.is_multiple_of(8) {
            cx.command(Command::Media(MediaCmd::Refresh));
        }
    }

    fn on_input(&mut self, hit: Option<HitId>, input: &Input, cx: &mut Cx) -> bool {
        if !self.snap.is_active() {
            return false;
        }
        match input {
            Input::Move(p) | Input::Drag { pos: p, .. } => {
                if self.scrub.is_some() {
                    self.scrub = Some(self.lay.seek_frac(p.x));
                    cx.request_redraw();
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
                if hit == Some(HIT_SEEK) && self.can_seek() {
                    self.scrub = Some(self.lay.seek_frac(p.x));
                    cx.request_redraw();
                    return true;
                }
                false
            }
            Input::Up(p) => {
                if self.scrub.take().is_some() {
                    let ms =
                        (f64::from(self.lay.seek_frac(p.x)) * self.snap.duration_ms as f64) as u64;
                    self.pending_seek = Some((ms, cx.now + SEEK_HOLD_SECS));
                    cx.command(Command::Media(MediaCmd::SeekTo(ms)));
                    cx.request_redraw();
                    return true;
                }
                false
            }
            Input::Click(_) => {
                let s = self.snap.clone();
                match hit {
                    Some(HIT_PLAY) if s.can_play_pause => {
                        let now_playing = self.playing(cx.now);
                        self.optimistic = Some((!now_playing, cx.now + OPTIMISTIC_SECS));
                        cx.command(Command::Media(MediaCmd::PlayPause));
                        cx.request_redraw();
                        true
                    }
                    Some(HIT_NEXT) if s.can_next => {
                        cx.command(Command::Media(MediaCmd::Next));
                        true
                    }
                    Some(HIT_PREV) if s.can_prev => {
                        // Like every player: a second or two in, "previous" restarts the track.
                        cx.command(Command::Media(MediaCmd::Previous));
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }

    fn draw_peek(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let th = *cv.theme;
        let accent = self.accent(&th);
        let art = Rect::new(area.x, area.y, area.h, area.h);
        self.draw_art(cv, art, 8.0, &th);
        let x0 = art.right() + 12.0;
        let w = (area.right() - x0 - 26.0).max(0.0);
        let line = self.artist_line();
        let (title_rect, artist_rect) = if line.is_empty() {
            (Rect::new(x0, area.y, w, area.h), Rect::default())
        } else {
            (
                Rect::new(x0, area.y, w, area.h * 0.55),
                Rect::new(x0, area.y + area.h * 0.55, w, area.h * 0.45),
            )
        };
        let title = if self.snap.title.is_empty() {
            self.snap.app.clone()
        } else {
            self.snap.title.clone()
        };
        let style = TextStyle::new(13.5, Weight::SemiBold);
        cv.text(
            title_rect,
            &title,
            if line.is_empty() {
                style.align(Align::Start)
            } else {
                style
            },
            th.text,
        );
        if !line.is_empty() {
            cv.text(artist_rect, line, TextStyle::caption(), th.text_dim);
        }
        // Three still bars: "this is playing" without an animation loop.
        let rect = Rect::new(area.right() - 20.0, area.center().y - 7.0, 20.0, 14.0);
        let heights = [0.45, 0.9, 0.6];
        for (i, k) in heights.iter().enumerate() {
            let h = 3.0 + k * 11.0;
            cv.capsule(
                Rect::new(rect.x + i as f32 * 7.0, rect.bottom() - h, 3.0, h),
                accent,
            );
        }
        let _ = dx;
    }

    fn draw_expanded(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let th = *cv.theme;
        let now = dx.now;
        let lay = Layout::new(area);
        self.lay = lay;
        let accent = self.accent(&th);
        let playing = self.playing(now);

        // Visualizer state advances here, per frame, from the platform's sampled level.
        let dt = (now - self.last_frame) as f32;
        self.last_frame = now;
        let mut live = false;
        match dx.env.audio {
            Audio::Level(level) if playing => {
                step_bars(&mut self.bars, level, now, dt);
                live = self.cfg.visualizer;
            }
            Audio::Unavailable => self.meter_ok = false,
            _ => {}
        }

        if self.snap.art != 0 && th.dark {
            cv.push(DrawCmd::Glow {
                center: lay.art.center(),
                radius: lay.art.w * 0.95,
                color: accent.with_alpha(0.22),
            });
        }
        self.draw_art(cv, lay.art, 12.0, &th);

        let app = if self.snap.app.is_empty() {
            "Now playing".into()
        } else {
            self.snap.app.clone()
        };
        cv.text(lay.app, &app, TextStyle::caption(), th.text_faint);
        if self.cfg.visualizer {
            self.draw_bars(cv, lay.bars, accent, live);
        }

        let title = if self.snap.title.is_empty() {
            Arc::<str>::from("Unknown title")
        } else {
            self.snap.title.clone()
        };
        cv.text(lay.title, &title, TextStyle::title(), th.text);
        let line = self.artist_line();
        if !line.is_empty() {
            cv.text(lay.artist, line, TextStyle::body(), th.text_dim);
        }

        // Progress.
        let dur = self.snap.duration_ms;
        let pos = self.position_ms(now);
        if dur > 0 {
            let frac = self.scrub.unwrap_or(pos as f32 / dur as f32);
            cv.bar(lay.seek, frac, th.surface_hi, accent);
            if self.scrub.is_some() || self.hover == Some(HIT_SEEK) {
                cv.circle(
                    Vec2::new(
                        lay.seek.x + lay.seek.w * frac.clamp(0.0, 1.0),
                        lay.seek.center().y,
                    ),
                    5.5,
                    th.text,
                );
            }
            let shown = self
                .scrub
                .map_or(pos, |f| (f64::from(f) * dur as f64) as u64);
            cv.text(
                lay.time_left,
                fmt_clock(shown),
                TextStyle::caption().tabular(),
                th.text_dim,
            );
            cv.text(
                lay.time_right,
                fmt_clock(dur),
                TextStyle::caption().tabular().align(Align::End),
                th.text_faint,
            );
            if self.can_seek() {
                cv.hit(lay.seek_hit, HIT_SEEK, CursorKind::Hand);
            }
        } else {
            cv.bar(lay.seek, 0.0, th.surface, accent);
            cv.text(lay.time_left, "LIVE", TextStyle::caption(), th.text_faint);
        }

        // Transport controls.
        self.button(cv, lay.prev, Icon::Prev, HIT_PREV, self.snap.can_prev, &th);
        let can_toggle = self.snap.can_play_pause;
        let fill = if self.hover == Some(HIT_PLAY) && can_toggle {
            th.text.mul_alpha(0.86)
        } else {
            th.text
        };
        cv.capsule(lay.play, if can_toggle { fill } else { th.surface_hi });
        cv.icon(
            if playing { Icon::Pause } else { Icon::Play },
            lay.play.inset(lay.play.w * 0.24),
            if can_toggle { th.bg } else { th.text_faint },
        );
        if can_toggle {
            cv.hit(lay.play, HIT_PLAY, CursorKind::Hand);
        }
        self.button(cv, lay.next, Icon::Next, HIT_NEXT, self.snap.can_next, &th);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::{DrawList, Text};
    use crate::module::{Env, ModuleHost, Out};

    fn snap(title: &str, playing: bool) -> Arc<MediaSnapshot> {
        Arc::new(MediaSnapshot {
            app: "Spotify".into(),
            title: title.into(),
            artist: "Artist".into(),
            album: "Album".into(),
            playing,
            position_ms: 30_000,
            duration_ms: 200_000,
            art: 0,
            accent: None,
            can_play_pause: true,
            can_next: true,
            can_prev: true,
            can_seek: true,
        })
    }

    struct T {
        media: Media,
        theme: Theme,
        cfg: Config,
        env: Env,
        out: Out,
    }

    impl T {
        fn new() -> T {
            T {
                media: Media::new(MediaCfg::default()),
                theme: Theme::default(),
                cfg: Config::default(),
                env: Env::default(),
                out: Out::default(),
            }
        }
        fn send(&mut self, now: f64, s: Arc<MediaSnapshot>) {
            let ev = Event::new(Source::Local, EventKind::MediaChanged(s));
            let mut cx = Cx::for_test(now, &self.env, &self.theme, &self.cfg, &mut self.out);
            self.media.on_event(&ev, &mut cx);
        }
        fn input(&mut self, now: f64, hit: Option<HitId>, i: Input) -> bool {
            let mut cx = Cx::for_test(now, &self.env, &self.theme, &self.cfg, &mut self.out);
            self.media.on_input(hit, &i, &mut cx)
        }
        fn draw(&mut self, now: f64, audio: Audio) -> DrawList {
            let mut list = DrawList::new();
            let mut cv = Canvas::new(&mut list, &self.theme);
            let env = Env { audio, ..self.env };
            self.media.draw_expanded(
                &mut cv,
                Rect::new(0.0, 0.0, 364.0, 112.0),
                &DrawCx {
                    now,
                    env: &env,
                    config: &self.cfg,
                },
            );
            list
        }
        fn commands(&mut self) -> Vec<Command> {
            std::mem::take(&mut self.out.commands)
        }
    }

    fn texts(list: &DrawList) -> Vec<String> {
        list.cmds
            .iter()
            .filter_map(|c| match c {
                DrawCmd::Text { text, .. } => Some(text.as_str().to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn app_names_are_made_friendly() {
        assert_eq!(friendly_app_name("Spotify.exe"), "Spotify");
        assert_eq!(
            friendly_app_name("SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify"),
            "Spotify"
        );
        assert_eq!(
            friendly_app_name("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"),
            "Media Player"
        );
        assert_eq!(friendly_app_name("chrome.exe"), "Chrome");
        assert_eq!(friendly_app_name("MSEdge"), "Edge");
        assert_eq!(
            friendly_app_name("AppleInc.AppleMusicWin_nzyj5cx40ttqa!App"),
            "Apple Music"
        );
        assert_eq!(
            friendly_app_name("C:\\Tools\\superplayer.exe"),
            "Superplayer",
            "unknown apps are tidied, not dropped"
        );
        assert_eq!(friendly_app_name("Vendor.Thing_abc!Thing"), "Thing");
        assert_eq!(friendly_app_name(""), "");
    }

    #[test]
    fn clock_labels() {
        assert_eq!(fmt_clock(0), "0:00");
        assert_eq!(fmt_clock(83_999), "1:23");
        assert_eq!(fmt_clock(600_000), "10:00");
        assert_eq!(fmt_clock(3_725_000), "1:02:05");
    }

    #[test]
    fn position_is_extrapolated_while_playing_and_frozen_when_paused() {
        let mut t = T::new();
        t.send(100.0, snap("Song", true));
        assert_eq!(t.media.position_ms(100.0), 30_000);
        assert_eq!(t.media.position_ms(110.0), 40_000, "ten seconds later");
        assert_eq!(t.media.position_ms(1_000.0), 200_000, "never past the end");
        t.send(200.0, snap("Song", false));
        assert_eq!(t.media.position_ms(230.0), 30_000, "paused: frozen");
    }

    #[test]
    fn the_page_exists_only_while_there_is_a_session() {
        let mut t = T::new();
        assert!(!t.media.page_visible());
        t.send(0.0, snap("Song", true));
        assert!(t.media.page_visible());
        t.send(1.0, Arc::new(MediaSnapshot::default()));
        assert!(
            !t.media.page_visible(),
            "session closed: the page leaves the ring"
        );
        assert!(!t.media.wants_frames());
    }

    #[test]
    fn a_new_track_peeks_but_the_first_snapshot_and_repeats_do_not() {
        let mut t = T::new();
        t.send(0.0, snap("Already playing at start-up", true));
        assert!(
            t.out.shell.is_empty(),
            "first snapshot is the state we woke up to"
        );
        t.send(1.0, snap("Already playing at start-up", false));
        t.send(2.0, snap("Already playing at start-up", true));
        assert!(
            t.out.shell.is_empty(),
            "same track: pause/play does not peek"
        );
        t.send(3.0, snap("Next song", true));
        assert_eq!(t.out.shell.len(), 1, "track changed");
        t.out.shell.clear();
        t.send(4.0, snap("Third", false));
        assert!(
            t.out.shell.is_empty(),
            "a track that is not playing does not announce itself"
        );
    }

    #[test]
    fn peeking_can_be_switched_off() {
        let mut t = T::new();
        t.media = Media::new(MediaCfg {
            peek_on_change: false,
            ..Default::default()
        });
        t.send(0.0, snap("A", true));
        t.send(1.0, snap("B", true));
        assert!(t.out.shell.is_empty());
    }

    #[test]
    fn events_from_the_phone_are_not_mistaken_for_local_playback() {
        let mut t = T::new();
        let ev = Event::new(
            Source::Phone,
            EventKind::MediaChanged(snap("Phone song", true)),
        );
        let mut cx = Cx::for_test(0.0, &t.env, &t.theme, &t.cfg, &mut t.out);
        t.media.on_event(&ev, &mut cx);
        assert!(!t.media.page_visible());
    }

    #[test]
    fn the_page_shows_title_artist_times_and_controls() {
        let mut t = T::new();
        t.send(0.0, snap("Midnight City", true));
        let list = t.draw(0.0, Audio::Idle);
        let tx = texts(&list);
        assert!(tx.contains(&"Midnight City".to_string()), "{tx:?}");
        assert!(tx.contains(&"Artist — Album".to_string()), "{tx:?}");
        assert!(tx.contains(&"Spotify".to_string()));
        assert!(tx.contains(&"0:30".to_string()) && tx.contains(&"3:20".to_string()));
        assert!(list.is_balanced());
        let ids: Vec<u32> = list.hits.iter().map(|h| h.id.0).collect();
        assert!(
            ids.contains(&1) && ids.contains(&2) && ids.contains(&3) && ids.contains(&4),
            "{ids:?}"
        );
        let icons: Vec<Icon> = list
            .cmds
            .iter()
            .filter_map(|c| {
                if let DrawCmd::Icon { icon, .. } = c {
                    Some(*icon)
                } else {
                    None
                }
            })
            .collect();
        assert!(
            icons.contains(&Icon::Pause),
            "playing shows the pause glyph: {icons:?}"
        );
        assert!(icons.contains(&Icon::Note), "no art yet: placeholder");
    }

    #[test]
    fn paused_shows_play_and_art_replaces_the_placeholder() {
        let mut t = T::new();
        let mut s = (*snap("Song", false)).clone();
        s.art = 7;
        t.send(0.0, Arc::new(s));
        let list = t.draw(0.0, Audio::Idle);
        let icons: Vec<Icon> = list
            .cmds
            .iter()
            .filter_map(|c| {
                if let DrawCmd::Icon { icon, .. } = c {
                    Some(*icon)
                } else {
                    None
                }
            })
            .collect();
        assert!(icons.contains(&Icon::Play) && !icons.contains(&Icon::Note));
        assert!(
            list.cmds
                .iter()
                .any(|c| matches!(c, DrawCmd::Image { id, .. } if id.0 == 7))
        );
    }

    #[test]
    fn controls_the_player_cannot_honour_are_not_interactive() {
        let mut t = T::new();
        let mut s = (*snap("Song", true)).clone();
        s.can_next = false;
        s.can_seek = false;
        t.send(0.0, Arc::new(s));
        let list = t.draw(0.0, Audio::Idle);
        let ids: Vec<u32> = list.hits.iter().map(|h| h.id.0).collect();
        assert!(!ids.contains(&3), "next disabled");
        assert!(!ids.contains(&4), "seeking disabled");
        assert!(ids.contains(&2));
        assert!(!t.input(1.0, Some(HIT_NEXT), Input::Click(Vec2::ZERO)));
        assert!(t.commands().is_empty());
    }

    #[test]
    fn clicking_the_transport_buttons_sends_commands() {
        let mut t = T::new();
        t.send(0.0, snap("Song", true));
        t.draw(0.0, Audio::Idle);
        assert!(t.input(1.0, Some(HIT_PLAY), Input::Click(Vec2::ZERO)));
        assert!(t.input(1.0, Some(HIT_NEXT), Input::Click(Vec2::ZERO)));
        assert!(t.input(1.0, Some(HIT_PREV), Input::Click(Vec2::ZERO)));
        assert!(
            !t.input(1.0, None, Input::Click(Vec2::ZERO)),
            "a click on nothing is not consumed"
        );
        assert_eq!(
            t.commands(),
            vec![
                Command::Media(MediaCmd::PlayPause),
                Command::Media(MediaCmd::Next),
                Command::Media(MediaCmd::Previous)
            ]
        );
    }

    #[test]
    fn play_pause_flips_at_once_and_the_real_state_wins_when_it_arrives() {
        let mut t = T::new();
        t.send(0.0, snap("Song", true));
        t.input(1.0, Some(HIT_PLAY), Input::Click(Vec2::ZERO));
        let paused_look = t.draw(1.1, Audio::Idle);
        assert!(
            paused_look.cmds.iter().any(|c| matches!(
                c,
                DrawCmd::Icon {
                    icon: Icon::Play,
                    ..
                }
            )),
            "optimistic: shows 'play' immediately"
        );
        // The player answers: it is indeed paused.
        t.send(1.2, snap("Song", false));
        assert!(t.media.optimistic.is_none());
        // If the player never answers the guess expires.
        let mut t2 = T::new();
        t2.send(0.0, snap("Song", true));
        t2.input(1.0, Some(HIT_PLAY), Input::Click(Vec2::ZERO));
        assert!(!t2.media.playing(1.2));
        assert!(
            t2.media.playing(1.0 + OPTIMISTIC_SECS + 0.1),
            "guess expired: back to the truth"
        );
    }

    #[test]
    fn dragging_the_seek_bar_scrubs_and_releasing_seeks() {
        let mut t = T::new();
        t.send(0.0, snap("Song", false)); // 200 s long
        t.draw(0.0, Audio::Idle);
        let seek = t.media.lay.seek;
        let at = |f: f32| Vec2::new(seek.x + seek.w * f, seek.center().y);
        assert!(t.input(1.0, Some(HIT_SEEK), Input::Down(at(0.25))));
        assert_eq!(t.media.scrub, Some(0.25));
        assert!(
            t.input(1.1, None, Input::Move(at(0.5))),
            "moves are consumed while scrubbing, even outside the bar"
        );
        let shown = texts(&t.draw(1.1, Audio::Idle));
        assert!(
            shown.contains(&"1:40".to_string()),
            "label follows the thumb: {shown:?}"
        );
        assert!(t.input(1.2, None, Input::Up(at(0.75))));
        assert_eq!(
            t.commands(),
            vec![Command::Media(MediaCmd::SeekTo(150_000))]
        );
        assert_eq!(
            t.media.position_ms(1.5),
            150_000,
            "held at the target until the player confirms"
        );
        t.send(2.0, {
            let mut s = (*snap("Song", false)).clone();
            s.position_ms = 150_400;
            Arc::new(s)
        });
        assert!(
            t.media.pending_seek.is_none(),
            "confirmed by the next snapshot"
        );
        assert_eq!(t.media.position_ms(2.0), 150_400);
    }

    #[test]
    fn dragging_past_either_end_clamps() {
        let mut t = T::new();
        t.send(0.0, snap("Song", false));
        t.draw(0.0, Audio::Idle);
        let seek = t.media.lay.seek;
        t.input(
            0.0,
            Some(HIT_SEEK),
            Input::Down(Vec2::new(seek.x - 50.0, 0.0)),
        );
        assert_eq!(t.media.scrub, Some(0.0));
        t.input(0.1, None, Input::Move(Vec2::new(seek.right() + 500.0, 0.0)));
        assert_eq!(t.media.scrub, Some(1.0));
        t.input(0.2, None, Input::Up(Vec2::new(seek.right() + 500.0, 0.0)));
        assert_eq!(
            t.commands(),
            vec![Command::Media(MediaCmd::SeekTo(200_000))]
        );
    }

    #[test]
    fn live_streams_have_no_seek_bar() {
        let mut t = T::new();
        let mut s = (*snap("Radio", true)).clone();
        s.duration_ms = 0;
        t.send(0.0, Arc::new(s));
        let list = t.draw(0.0, Audio::Idle);
        assert!(texts(&list).contains(&"LIVE".to_string()));
        assert!(!list.hits.iter().any(|h| h.id == HIT_SEEK));
        assert!(!t.input(0.0, Some(HIT_SEEK), Input::Down(Vec2::ZERO)));
    }

    #[test]
    fn hover_redraws_only_when_the_hovered_control_changes() {
        let mut t = T::new();
        t.send(0.0, snap("Song", true));
        t.out.redraw = false;
        t.input(1.0, Some(HIT_PLAY), Input::Move(Vec2::ZERO));
        assert!(t.out.redraw);
        t.out.redraw = false;
        t.input(1.1, Some(HIT_PLAY), Input::Move(Vec2::ZERO));
        assert!(!t.out.redraw, "same control: nothing to redraw");
        t.input(1.2, None, Input::Leave);
        assert!(t.out.redraw);
    }

    #[test]
    fn the_visualizer_wants_frames_only_while_playing_and_possible() {
        let mut t = T::new();
        t.send(0.0, snap("Song", true));
        assert!(t.media.wants_frames());
        t.send(1.0, snap("Song", false));
        assert!(!t.media.wants_frames(), "paused: no frames");
        t.send(2.0, snap("Song", true));
        t.draw(2.0, Audio::Unavailable);
        assert!(!t.media.wants_frames(), "no device to meter: stop asking");
        let mut off = T::new();
        off.media = Media::new(MediaCfg {
            visualizer: false,
            ..Default::default()
        });
        off.send(0.0, snap("Song", true));
        assert!(!off.media.wants_frames(), "disabled in the config");
    }

    #[test]
    fn bars_follow_the_level_and_fall_back_when_it_stops() {
        let mut bars = [0.0; BARS];
        let mut t = 0.0;
        for _ in 0..30 {
            t += 1.0 / 60.0;
            step_bars(&mut bars, 0.9, t, 1.0 / 60.0);
        }
        assert!(bars.iter().all(|b| *b > 0.1), "{bars:?}");
        assert!(bars.iter().cloned().fold(0.0f32, f32::max) > 0.5);
        let spread = bars.iter().cloned().fold(0.0f32, f32::max)
            - bars.iter().cloned().fold(1.0f32, f32::min);
        assert!(spread > 0.05, "bars are not all identical: {bars:?}");
        for _ in 0..120 {
            t += 1.0 / 60.0;
            step_bars(&mut bars, 0.0, t, 1.0 / 60.0);
        }
        assert!(bars.iter().all(|b| *b < 0.02), "released: {bars:?}");
    }

    #[test]
    fn bar_motion_does_not_depend_on_the_refresh_rate() {
        let run = |hz: f32| {
            let mut bars = [0.0; BARS];
            let steps = (hz * 0.25) as usize;
            for i in 0..steps {
                step_bars(&mut bars, 1.0, (i as f64 + 1.0) / f64::from(hz), 1.0 / hz);
            }
            bars.iter().sum::<f32>() / BARS as f32
        };
        let (a, b) = (run(60.0), run(144.0));
        assert!((a - b).abs() < 0.08, "60 Hz {a} vs 144 Hz {b}");
    }

    #[test]
    fn a_frame_hitch_cannot_make_the_bars_jump() {
        let mut bars = [0.0; BARS];
        step_bars(&mut bars, 1.0, 10.0, 5.0); // a 5 s stall
        assert!(bars.iter().all(|b| *b <= 1.0 && *b >= 0.0));
    }

    #[test]
    fn layout_is_inside_the_area_and_input_agrees_with_it() {
        let area = Rect::new(10.0, 20.0, 364.0, 112.0);
        let l = Layout::new(area);
        for r in [
            l.art,
            l.app,
            l.bars,
            l.title,
            l.artist,
            l.seek,
            l.time_left,
            l.time_right,
        ] {
            assert!(
                r.x >= area.x - 0.01 && r.right() <= area.right() + 0.01 && r.y >= area.y - 0.01,
                "{r:?}"
            );
        }
        assert!(
            l.seek_hit.contains(l.seek.center()) && l.seek_hit.h > l.seek.h,
            "easy to grab"
        );
        assert!(
            l.prev.right() < l.play.x && l.play.right() < l.next.x,
            "controls do not overlap"
        );
        assert_eq!(l.seek_frac(l.seek.x), 0.0);
        assert_eq!(l.seek_frac(l.seek.right()), 1.0);
        assert!((l.seek_frac(l.seek.center().x) - 0.5).abs() < 1e-4);
        assert_eq!(
            Layout::default().seek_frac(5.0),
            0.0,
            "degenerate layout cannot divide by zero"
        );
    }

    #[test]
    fn refresh_is_requested_when_the_page_opens_and_periodically() {
        let mut t = T::new();
        t.send(0.0, snap("Song", true));
        {
            let mut cx = Cx::for_test(1.0, &t.env, &t.theme, &t.cfg, &mut t.out);
            t.media.on_visibility(Visibility::Expanded, &mut cx);
        }
        assert_eq!(t.commands(), vec![Command::Media(MediaCmd::Refresh)]);
        let mut refreshes = 0;
        for i in 0..16 {
            let mut cx = Cx::for_test(2.0 + f64::from(i), &t.env, &t.theme, &t.cfg, &mut t.out);
            t.media.on_poll(&mut cx);
        }
        refreshes += t
            .commands()
            .iter()
            .filter(|c| **c == Command::Media(MediaCmd::Refresh))
            .count();
        assert_eq!(refreshes, 2, "every 8 s while open");
    }

    #[test]
    fn the_peek_shows_the_track_and_never_panics_on_missing_fields() {
        let mut t = T::new();
        t.send(0.0, snap("Song", true));
        let mut list = DrawList::new();
        {
            let mut cv = Canvas::new(&mut list, &t.theme);
            t.media.draw_peek(
                &mut cv,
                Rect::new(0.0, 0.0, 294.0, 36.0),
                &DrawCx {
                    now: 0.0,
                    env: &t.env,
                    config: &t.cfg,
                },
            );
        }
        let tx = texts(&list);
        assert!(
            tx.contains(&"Song".to_string()) && tx.contains(&"Artist — Album".to_string()),
            "{tx:?}"
        );
        // Title only, nothing else.
        let mut bare = T::new();
        bare.send(
            0.0,
            Arc::new(MediaSnapshot {
                title: "Only a title".into(),
                playing: true,
                ..Default::default()
            }),
        );
        let mut list = DrawList::new();
        {
            let mut cv = Canvas::new(&mut list, &bare.theme);
            bare.media.draw_peek(
                &mut cv,
                Rect::new(0.0, 0.0, 294.0, 36.0),
                &DrawCx {
                    now: 0.0,
                    env: &bare.env,
                    config: &bare.cfg,
                },
            );
        }
        assert!(texts(&list).contains(&"Only a title".to_string()));
        assert!(list.is_balanced());
    }

    #[test]
    fn works_inside_the_host_and_follows_the_config() {
        let mut base = Config::default();
        base.modules.order = vec!["media".into(), "clock".into()]; // media + clock only
        let mut host = ModuleHost::new(
            crate::modules::registry(),
            Arc::new(base.clone()),
            Theme::default(),
        );
        assert_eq!(
            host.page_ids(),
            vec!["clock"],
            "no session: only the clock page"
        );
        host.dispatch(vec![Event::new(
            Source::Local,
            EventKind::MediaChanged(snap("Song", true)),
        )]);
        assert_eq!(
            host.page_ids(),
            vec!["media", "clock"],
            "media joins the ring, first by default order"
        );
        let out = host.take_out();
        assert!(out.shell.is_empty(), "first snapshot: no peek");
        host.dispatch(vec![Event::new(
            Source::Local,
            EventKind::MediaChanged(snap("Another", true)),
        )]);
        assert_eq!(
            host.take_out().shell.len(),
            1,
            "a track change peeks through the host"
        );
        let mut cfg = base;
        cfg.media.enabled = false;
        host.apply_config(Arc::new(cfg));
        assert_eq!(
            host.page_ids(),
            vec!["clock"],
            "disabled: the module is dropped entirely"
        );
    }

    #[test]
    fn disabled_in_config_means_never_instantiated() {
        let mut cfg = Config::default();
        assert!(create(&cfg).is_some());
        cfg.media.enabled = false;
        assert!(create(&cfg).is_none());
        assert!(!cfg.module_active("media"));
    }

    #[test]
    fn text_static_and_shared_are_both_drawn() {
        // Guards the `&Arc<str>` -> Text conversions used for the title lines.
        let t: Text = (&Arc::<str>::from("x")).into();
        assert_eq!(t.as_str(), "x");
    }
}
