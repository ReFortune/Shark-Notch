//! TOML configuration: typed, defaulted, validated, hot-reloadable.
//!
//! Policy: a *parse* error (bad TOML, wrong type, unknown enum value) keeps the previous config and
//! reports the message; an *out-of-range* value is clamped with a warning; an *unknown key* is a
//! warning (typos should be visible, but must not break newer/older config files).

use serde::{Deserialize, Serialize};

use crate::hotkey;
use crate::theme::ThemeMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReduceMotion {
    /// Follow Windows' "Animation effects" setting.
    #[default]
    System,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FullscreenScope {
    /// Any fullscreen app on any monitor suspends the notch.
    #[default]
    Any,
    /// Only a fullscreen app on the notch's own monitor does.
    SameMonitor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// Flush with the top edge, with concave "ears" like a hardware notch.
    #[default]
    Notch,
    /// Floating pill with a small gap above it.
    Island,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    pub autostart: bool,
    pub tray_icon: bool,
    /// `"primary"`, a 1-based monitor number (`"2"`), or a device name (`"\\\\.\\DISPLAY2"`).
    pub monitor: String,
    /// Hide the notch from screenshots, screen sharing and recordings.
    pub exclude_from_capture: bool,
    /// `"error" | "warn" | "info" | "debug"`.
    pub log_level: String,
}

impl Default for General {
    fn default() -> Self {
        Self {
            autostart: false,
            tray_icon: true,
            monitor: "primary".into(),
            exclude_from_capture: true,
            log_level: "info".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    /// Toggle the expanded notch.
    pub toggle: String,
    /// Optional: show a brief peek (also over fullscreen apps if `fullscreen.peek_over_fullscreen`). Empty = off.
    pub peek: String,
    pub next_page: String,
    pub prev_page: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            toggle: "Ctrl+Alt+N".into(),
            peek: String::new(),
            next_page: String::new(),
            prev_page: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hover {
    pub enabled: bool,
    /// Width of the trigger zone at the top-centre of the screen (DIPs).
    pub zone_width: f32,
    /// Height of the trigger zone measured from the top edge (DIPs).
    pub zone_height: f32,
    pub dwell_ms: u32,
    pub leave_grace_ms: u32,
    /// Hover is ignored for this long after a keystroke.
    pub typing_suppress_ms: u32,
    /// Cursor sampling rate while nothing is near the zone.
    pub poll_hz: u32,
    pub approach_margin: f32,
}

impl Default for Hover {
    fn default() -> Self {
        Self {
            enabled: true,
            zone_width: 200.0,
            zone_height: 6.0,
            dwell_ms: 150,
            leave_grace_ms: 280,
            typing_suppress_ms: 600,
            poll_hz: 10,
            approach_margin: 140.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme: ThemeMode,
    /// `"system"` or `"#RRGGBB"`.
    pub accent: String,
    pub style: Style,
    /// Draw the tiny idle pill. If false, the notch only appears when something happens.
    pub idle_visible: bool,
    pub pill_width: f32,
    pub pill_height: f32,
    pub corner_radius: f32,
    /// 0 = circular corners, 1 = fully continuous (squircle).
    pub corner_smoothing: f32,
    /// Concave "ears" where the notch meets the screen edge (Notch style only).
    pub ears: bool,
    pub ear_size: f32,
    /// Clickable page icons along the bottom (false: plain dots).
    pub page_icons: bool,
    /// Extra UI scale on top of Windows' DPI scaling.
    pub scale: f32,
    /// The window (and its swap chain) is sized once for this panel size, so pages never force a
    /// rebuild. A module page larger than this is clipped.
    pub max_panel_width: f32,
    pub max_panel_height: f32,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            accent: "system".into(),
            style: Style::Notch,
            idle_visible: true,
            pill_width: 112.0,
            pill_height: 6.0,
            corner_radius: 26.0,
            corner_smoothing: 0.6,
            ears: true,
            ear_size: 12.0,
            page_icons: true,
            scale: 1.0,
            max_panel_width: 460.0,
            max_panel_height: 340.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Animation {
    pub reduce_motion: ReduceMotion,
    /// Global speed multiplier for every spring.
    pub speed: f32,
    /// 0 = no overshoot, 1 = very bouncy.
    pub bounciness: f32,
    pub stagger_in_ms: u32,
    pub stagger_out_ms: u32,
    /// Swap scroll direction for page switching.
    pub reverse_scroll: bool,
}

impl Default for Animation {
    fn default() -> Self {
        Self {
            reduce_motion: ReduceMotion::System,
            speed: 1.0,
            bounciness: 0.5,
            stagger_in_ms: 70,
            stagger_out_ms: 40,
            reverse_scroll: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Performance {
    /// Release the GPU stack after the notch has been idle this long (0 = keep it warm forever).
    pub gpu_idle_release_secs: u32,
    /// Warm the GPU stack when the cursor approaches the zone.
    pub prewarm_on_approach: bool,
    /// Log a frame-time report after every animation burst.
    pub frame_report: bool,
}

impl Default for Performance {
    fn default() -> Self {
        Self {
            gpu_idle_release_secs: 300,
            prewarm_on_approach: true,
            frame_report: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Fullscreen {
    pub enabled: bool,
    pub scope: FullscreenScope,
    /// Show a small badge (a bell with a count) after a fullscreen session if notifications were missed.
    pub show_missed_indicator: bool,
    /// Allow the `hotkeys.peek` key to show the notch over borderless-fullscreen apps.
    pub peek_over_fullscreen: bool,
}

impl Default for Fullscreen {
    fn default() -> Self {
        Self {
            enabled: true,
            scope: FullscreenScope::Any,
            show_missed_indicator: true,
            peek_over_fullscreen: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum HourFormat {
    /// Follow the Windows regional setting.
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "12")]
    H12,
    #[serde(rename = "24")]
    H24,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SettingsCfg {
    /// The settings page: one switch per module.
    pub enabled: bool,
}

impl Default for SettingsCfg {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClockCfg {
    pub enabled: bool,
    /// `"system"`, `"12"` or `"24"`.
    pub hour_format: HourFormat,
    pub show_seconds: bool,
    pub show_week: bool,
}

impl Default for ClockCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            hour_format: HourFormat::System,
            show_seconds: false,
            show_week: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaCfg {
    pub enabled: bool,
    /// Briefly show the new track when the song changes while the notch is collapsed.
    pub peek_on_change: bool,
    /// How long that peek stays, in seconds.
    pub peek_secs: f32,
    /// Level-reactive bars on the page — animated only while music is playing *and* the page is open.
    pub visualizer: bool,
    /// Show the line being sung under the transport controls. **Off by default**: it asks
    /// lrclib.net for each track's lyrics (the artist, title, album and length go over the
    /// network) while the page is open, and LRCLIB publishes no terms about its lyrics.
    pub lyrics: bool,
}

impl Default for MediaCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            peek_on_change: true,
            peek_secs: 2.8,
            visualizer: true,
            lyrics: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardCfg {
    pub enabled: bool,
    /// Unpinned entries kept (pinned ones are on top of this).
    pub max_items: u32,
    /// Of those, at most this many may be images (they are stored on disk while the app runs).
    pub max_images: u32,
    /// Capture images at all.
    pub capture_images: bool,
    /// Text longer than this many KiB is ignored.
    pub max_text_kib: u32,
    /// Keep pinned text across restarts (`pins.json` next to the log; history itself is never saved).
    pub persist_pins: bool,
    /// Briefly show items that arrive from the iPhone.
    pub peek_phone_items: bool,
}

impl Default for ClipboardCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            max_items: 30,
            max_images: 8,
            capture_images: true,
            max_text_kib: 256,
            persist_pins: true,
            peek_phone_items: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShelfCfg {
    pub enabled: bool,
    /// Open the shelf by itself when a drag (of a file) heads for the top of the screen.
    pub open_on_drag: bool,
    /// Items kept on the shelf (older ones fall off; the files themselves are never touched).
    pub max_items: u32,
}

impl Default for ShelfCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            open_on_drag: true,
            max_items: 40,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationsCfg {
    pub enabled: bool,
    /// Briefly show new notifications while the notch is collapsed.
    pub peek: bool,
    /// How long that peek stays, in seconds.
    pub peek_secs: f32,
    /// Notifications kept in the list.
    pub max_items: u32,
    /// Apps (display names, case-insensitive) whose notifications are never shown here.
    pub ignore_apps: Vec<String>,
}

impl Default for NotificationsCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            peek: true,
            peek_secs: 4.0,
            max_items: 20,
            ignore_apps: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CalendarCfg {
    pub enabled: bool,
    /// ICS calendar feeds: `https://` / `webcal://` links or paths of `.ics` files.
    pub feeds: Vec<String>,
    /// How often the feeds are read again, in minutes.
    pub refresh_minutes: u32,
    pub week_starts_monday: bool,
    /// A banner (with a Join button if the event has a call link) this many minutes before an event
    /// starts, and again as it starts. 0 = no banner.
    pub alert_minutes: u32,
    /// The collapsed pill shows a countdown for an event starting within this many minutes. 0 = never.
    pub chip_minutes: u32,
    /// How long such a banner stays, in seconds.
    pub peek_secs: f32,
}

impl Default for CalendarCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            feeds: Vec::new(),
            refresh_minutes: 30,
            week_starts_monday: true,
            alert_minutes: 5,
            chip_minutes: 15,
            peek_secs: 8.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PomodoroCfg {
    pub enabled: bool,
    pub focus_minutes: f32,
    pub short_break_minutes: f32,
    pub long_break_minutes: f32,
    /// A long break after this many focus sessions.
    pub long_break_every: u32,
    pub auto_start_breaks: bool,
    pub auto_start_focus: bool,
    /// Play the system chime when a session or break ends (never while a fullscreen app is in front).
    pub sound: bool,
    pub peek_secs: f32,
    /// A stopwatch under the timer on the Focus page.
    pub stopwatch: bool,
}

impl Default for PomodoroCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            focus_minutes: 25.0,
            short_break_minutes: 5.0,
            long_break_minutes: 15.0,
            long_break_every: 4,
            auto_start_breaks: true,
            auto_start_focus: false,
            sound: true,
            peek_secs: 6.0,
            stopwatch: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveCfg {
    pub enabled: bool,
    /// Show a chip while a program is using the microphone or the camera.
    pub privacy: bool,
    /// Show browser downloads in progress (and a banner when one finishes).
    pub downloads: bool,
    /// The folder watched for downloads; empty = your Downloads folder.
    pub download_dir: String,
    /// Quick-timer buttons, in minutes (at most six).
    pub timer_presets: Vec<u32>,
    /// The system chime when a timer ends (never over a fullscreen app).
    pub sound: bool,
    pub peek_secs: f32,
}

impl Default for LiveCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            privacy: true,
            downloads: true,
            download_dir: String::new(),
            timer_presets: vec![1, 5, 10, 15, 30, 60],
            sound: true,
            peek_secs: 6.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatsCfg {
    pub enabled: bool,
    /// Seconds between readings while the page is on screen (nothing is read while it is not).
    pub interval_secs: f32,
    /// Read the GPU's utilisation counters too (the most expensive of the readings).
    pub gpu: bool,
    /// Show network speeds in bits per second (Mbps) instead of bytes (MB/s).
    pub net_bits: bool,
    /// A short banner when the charger is plugged in or removed and when the battery is full.
    pub battery_hud: bool,
    /// A strip with the connected Bluetooth devices and their batteries (read every few seconds
    /// while the page is on screen).
    pub devices: bool,
    /// Claude Code's token use today (read from its own session logs, nothing is sent anywhere):
    /// a line on this page and a chip on the pill while it is working.
    pub ai_usage: bool,
}

impl Default for StatsCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 1.0,
            gpu: true,
            net_bits: false,
            battery_hud: true,
            devices: true,
            ai_usage: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlCfg {
    pub enabled: bool,
    /// Seconds between readings of the controls while the page is on screen (so a volume key
    /// pressed elsewhere shows up); nothing is read while it is not.
    pub interval_secs: f32,
    /// A "Keep awake" tile (and a chip while it is on) that stops Windows sleeping and the screen
    /// turning off. It is off at every start.
    pub keep_awake: bool,
    /// A microphone mute tile, and a mute button next to a program that is using the microphone.
    pub mic_mute: bool,
}

impl Default for ControlCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 1.0,
            keep_awake: true,
            mic_mute: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PhoneCfg {
    /// The link may be switched on at all (`listen` does that).
    pub enabled: bool,
    /// The link itself: a small server on your network. Off by default: a listener is something
    /// you switch on, knowing what it is.
    pub listen: bool,
    /// TCP port the PC listens on. 0 picks a free one.
    pub port: u16,
    /// Only accept phones on the same network as one of this PC's own addresses (not merely any
    /// private address, which would include a network reached through a router or VPN).
    pub same_network_only: bool,
    /// Largest file accepted, in MiB.
    pub max_file_mib: u32,
    /// Received files older than this many days are deleted from the inbox folder.
    pub keep_days: u32,
}

impl Default for PhoneCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: false,
            port: 8765,
            same_network_only: true,
            max_file_mib: 100,
            keep_days: 14,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Modules {
    /// Page order. Unknown ids are ignored; modules that are disabled are skipped.
    pub order: Vec<String>,
}

impl Default for Modules {
    fn default() -> Self {
        Self {
            order: vec![
                "media".into(),
                "clipboard".into(),
                "shelf".into(),
                "notifications".into(),
                "calendar".into(),
                "pomodoro".into(),
                "live".into(),
                "stats".into(),
                "control".into(),
                "clock".into(),
                "settings".into(),
            ],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub hotkeys: Hotkeys,
    pub hover: Hover,
    pub appearance: Appearance,
    pub animation: Animation,
    pub performance: Performance,
    pub fullscreen: Fullscreen,
    pub modules: Modules,
    pub media: MediaCfg,
    pub clipboard: ClipboardCfg,
    pub shelf: ShelfCfg,
    pub notifications: NotificationsCfg,
    pub calendar: CalendarCfg,
    pub pomodoro: PomodoroCfg,
    pub live: LiveCfg,
    pub stats: StatsCfg,
    pub control: ControlCfg,
    pub phone: PhoneCfg,
    pub clock: ClockCfg,
    pub settings: SettingsCfg,
}

/// Result of loading a config text.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub warnings: Vec<String>,
}

impl Config {
    /// Parse and validate. `Err` carries a human-readable parse error (keep the old config).
    pub fn parse(text: &str) -> Result<Loaded, String> {
        let value: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut config: Config = value
            .clone()
            .try_into()
            .map_err(|e: toml::de::Error| e.to_string())?;
        let mut warnings = Vec::new();
        if let Ok(schema) = toml::Value::try_from(Config::default()) {
            unknown_keys(&value, &schema, "", &mut warnings);
        }
        config.validate(&mut warnings);
        Ok(Loaded { config, warnings })
    }

    /// Clamp out-of-range values and blank invalid hotkeys, recording a warning for each.
    pub fn validate(&mut self, warnings: &mut Vec<String>) {
        fn clamp_f(v: &mut f32, lo: f32, hi: f32, name: &str, w: &mut Vec<String>) {
            if !v.is_finite() {
                w.push(format!("{name}: not a finite number, using {lo}"));
                *v = lo;
            } else if *v < lo || *v > hi {
                let c = v.clamp(lo, hi);
                w.push(format!(
                    "{name}: {v} is out of range {lo}..={hi}, using {c}"
                ));
                *v = c;
            }
        }
        fn clamp_u(v: &mut u32, lo: u32, hi: u32, name: &str, w: &mut Vec<String>) {
            if *v < lo || *v > hi {
                let c = (*v).clamp(lo, hi);
                w.push(format!(
                    "{name}: {v} is out of range {lo}..={hi}, using {c}"
                ));
                *v = c;
            }
        }
        let w = warnings;
        clamp_f(
            &mut self.hover.zone_width,
            40.0,
            800.0,
            "hover.zone_width",
            w,
        );
        clamp_f(
            &mut self.hover.zone_height,
            1.0,
            40.0,
            "hover.zone_height",
            w,
        );
        clamp_f(
            &mut self.hover.approach_margin,
            0.0,
            600.0,
            "hover.approach_margin",
            w,
        );
        clamp_u(&mut self.hover.dwell_ms, 0, 2000, "hover.dwell_ms", w);
        clamp_u(
            &mut self.hover.leave_grace_ms,
            0,
            3000,
            "hover.leave_grace_ms",
            w,
        );
        clamp_u(
            &mut self.hover.typing_suppress_ms,
            0,
            5000,
            "hover.typing_suppress_ms",
            w,
        );
        clamp_u(&mut self.hover.poll_hz, 2, 60, "hover.poll_hz", w);
        clamp_f(
            &mut self.appearance.pill_width,
            40.0,
            400.0,
            "appearance.pill_width",
            w,
        );
        clamp_f(
            &mut self.appearance.pill_height,
            2.0,
            40.0,
            "appearance.pill_height",
            w,
        );
        clamp_f(
            &mut self.appearance.corner_radius,
            0.0,
            60.0,
            "appearance.corner_radius",
            w,
        );
        clamp_f(
            &mut self.appearance.corner_smoothing,
            0.0,
            1.0,
            "appearance.corner_smoothing",
            w,
        );
        clamp_f(
            &mut self.appearance.ear_size,
            0.0,
            40.0,
            "appearance.ear_size",
            w,
        );
        clamp_f(&mut self.appearance.scale, 0.5, 2.0, "appearance.scale", w);
        clamp_f(
            &mut self.appearance.max_panel_width,
            200.0,
            1200.0,
            "appearance.max_panel_width",
            w,
        );
        clamp_f(
            &mut self.appearance.max_panel_height,
            100.0,
            900.0,
            "appearance.max_panel_height",
            w,
        );
        clamp_f(&mut self.animation.speed, 0.25, 4.0, "animation.speed", w);
        clamp_f(
            &mut self.animation.bounciness,
            0.0,
            1.0,
            "animation.bounciness",
            w,
        );
        clamp_u(
            &mut self.animation.stagger_in_ms,
            0,
            400,
            "animation.stagger_in_ms",
            w,
        );
        clamp_u(
            &mut self.animation.stagger_out_ms,
            0,
            400,
            "animation.stagger_out_ms",
            w,
        );
        clamp_f(&mut self.media.peek_secs, 0.5, 10.0, "media.peek_secs", w);
        clamp_u(&mut self.shelf.max_items, 1, 200, "shelf.max_items", w);
        clamp_f(
            &mut self.notifications.peek_secs,
            1.0,
            15.0,
            "notifications.peek_secs",
            w,
        );
        clamp_u(
            &mut self.notifications.max_items,
            1,
            100,
            "notifications.max_items",
            w,
        );
        clamp_u(
            &mut self.calendar.refresh_minutes,
            5,
            1440,
            "calendar.refresh_minutes",
            w,
        );
        clamp_u(
            &mut self.calendar.alert_minutes,
            0,
            120,
            "calendar.alert_minutes",
            w,
        );
        clamp_u(
            &mut self.calendar.chip_minutes,
            0,
            240,
            "calendar.chip_minutes",
            w,
        );
        clamp_f(
            &mut self.calendar.peek_secs,
            2.0,
            30.0,
            "calendar.peek_secs",
            w,
        );
        self.calendar.feeds.retain(|f| !f.trim().is_empty());
        self.calendar.feeds.truncate(8);
        for (v, name) in [
            (&mut self.pomodoro.focus_minutes, "pomodoro.focus_minutes"),
            (
                &mut self.pomodoro.short_break_minutes,
                "pomodoro.short_break_minutes",
            ),
            (
                &mut self.pomodoro.long_break_minutes,
                "pomodoro.long_break_minutes",
            ),
        ] {
            clamp_f(v, 0.05, 600.0, name, w);
        }
        clamp_u(
            &mut self.pomodoro.long_break_every,
            1,
            12,
            "pomodoro.long_break_every",
            w,
        );
        clamp_f(
            &mut self.pomodoro.peek_secs,
            2.0,
            30.0,
            "pomodoro.peek_secs",
            w,
        );
        clamp_f(&mut self.live.peek_secs, 2.0, 30.0, "live.peek_secs", w);
        clamp_f(
            &mut self.stats.interval_secs,
            0.5,
            5.0,
            "stats.interval_secs",
            w,
        );
        clamp_u(
            &mut self.phone.max_file_mib,
            1,
            4096,
            "phone.max_file_mib",
            w,
        );
        clamp_u(&mut self.phone.keep_days, 1, 3650, "phone.keep_days", w);
        clamp_f(
            &mut self.control.interval_secs,
            0.5,
            5.0,
            "control.interval_secs",
            w,
        );
        // Quick timers: 1 minute to 10 hours, at most six buttons.
        let presets = std::mem::take(&mut self.live.timer_presets);
        let before = presets.len();
        self.live.timer_presets = presets
            .into_iter()
            .filter(|m| (1..=crate::timers::MAX_MINUTES).contains(m))
            .take(6)
            .collect();
        if self.live.timer_presets.len() != before {
            w.push("live.timer_presets: only 1..=600 minutes, at most six, are kept".to_string());
        }
        if self.live.timer_presets.is_empty() {
            w.push("live.timer_presets: empty, using the defaults".to_string());
            self.live.timer_presets = LiveCfg::default().timer_presets;
        }
        clamp_u(
            &mut self.clipboard.max_items,
            5,
            200,
            "clipboard.max_items",
            w,
        );
        clamp_u(
            &mut self.clipboard.max_images,
            0,
            30,
            "clipboard.max_images",
            w,
        );
        clamp_u(
            &mut self.clipboard.max_text_kib,
            1,
            4096,
            "clipboard.max_text_kib",
            w,
        );
        clamp_u(
            &mut self.performance.gpu_idle_release_secs,
            0,
            86_400,
            "performance.gpu_idle_release_secs",
            w,
        );

        if self.appearance.accent != "system"
            && crate::color::Color::from_hex(&self.appearance.accent).is_none()
        {
            w.push(format!(
                "appearance.accent: '{}' is not 'system' or #RRGGBB, using 'system'",
                self.appearance.accent
            ));
            self.appearance.accent = "system".into();
        }
        if !matches!(
            self.general.log_level.as_str(),
            "error" | "warn" | "info" | "debug"
        ) {
            w.push(format!(
                "general.log_level: '{}' is not error/warn/info/debug, using 'info'",
                self.general.log_level
            ));
            self.general.log_level = "info".into();
        }
        for (name, key) in [
            ("hotkeys.toggle", &mut self.hotkeys.toggle),
            ("hotkeys.peek", &mut self.hotkeys.peek),
            ("hotkeys.next_page", &mut self.hotkeys.next_page),
            ("hotkeys.prev_page", &mut self.hotkeys.prev_page),
        ] {
            if let Err(e) = hotkey::parse(key) {
                w.push(format!("{name}: '{key}': {e}; hotkey disabled"));
                key.clear();
            }
        }
    }

    /// Is module `id` enabled in its own section (whether or not `modules.order` lists it)?
    pub fn module_enabled(&self, id: &str) -> bool {
        match id {
            "clock" => self.clock.enabled,
            "media" => self.media.enabled,
            "clipboard" => self.clipboard.enabled,
            "shelf" => self.shelf.enabled,
            "notifications" => self.notifications.enabled,
            "calendar" => self.calendar.enabled,
            "pomodoro" => self.pomodoro.enabled,
            "live" => self.live.enabled,
            "stats" => self.stats.enabled,
            "control" => self.control.enabled,
            "settings" => self.settings.enabled,
            _ => false,
        }
    }

    /// Is module `id` both listed in `modules.order` and enabled in its own section? The module host
    /// and the platform services (which own the OS-side producers) use the same answer, so a
    /// disabled module costs nothing: no object, no thread, no subscription.
    pub fn module_active(&self, id: &str) -> bool {
        self.module_enabled(id) && self.modules.order.iter().any(|m| m == id)
    }

    /// Spring-time parameters etc. that the shell derives from the config.
    pub fn dwell_secs(&self) -> f64 {
        self.hover.dwell_ms as f64 / 1000.0
    }
}

/// Recursively collect keys in `user` that `schema` does not know (tables only; arrays and the
/// contents of free-form tables are not inspected).
fn unknown_keys(user: &toml::Value, schema: &toml::Value, path: &str, out: &mut Vec<String>) {
    if let (toml::Value::Table(u), toml::Value::Table(s)) = (user, schema) {
        for (k, v) in u {
            let full = if path.is_empty() {
                k.clone()
            } else {
                format!("{path}.{k}")
            };
            match s.get(k) {
                Some(sv) => unknown_keys(v, sv, &full, out),
                None => out.push(format!("unknown setting '{full}' (ignored)")),
            }
        }
    }
}

/// Set the boolean `key` in `[section]` of a config file's text and change nothing else: comments,
/// order, spacing (the `#` of a trailing comment keeps its column) and line endings stay as they are.
/// A key that is missing is added right under its section header, a section that is missing at the
/// end. The tray's "Start with Windows" uses it, so that the choice is written where the next launch
/// looks for it (the file is the source of truth for `autostart`).
pub fn with_bool(text: &str, section: &str, key: &str, value: bool) -> String {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let new = if value { "true" } else { "false" };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();

    let (mut header, mut target, mut in_section) = (None, None, false);
    for (i, raw) in lines.iter().enumerate() {
        let t = raw.trim();
        if t.starts_with('[') {
            let name = t.trim_start_matches('[').split(']').next().map(str::trim);
            in_section = !t.starts_with("[[") && name == Some(section);
            if in_section && header.is_none() {
                header = Some(i);
            }
        } else if in_section
            && target.is_none()
            && !t.starts_with('#')
            && t.split_once('=').is_some_and(|(k, _)| k.trim() == key)
        {
            target = Some(i);
        }
    }

    let mut out = String::with_capacity(text.len() + 32);
    match (header, target) {
        (_, Some(i)) => {
            for (n, raw) in lines.iter().enumerate() {
                if n == i {
                    out.push_str(&replace_value(raw, new));
                } else {
                    out.push_str(raw);
                }
            }
        }
        (Some(h), None) => {
            for (n, raw) in lines.iter().enumerate() {
                out.push_str(raw);
                if n == h {
                    if !raw.ends_with('\n') {
                        out.push_str(nl);
                    }
                    out.push_str(&format!("{key} = {new}{nl}"));
                }
            }
        }
        (None, None) => {
            out.push_str(text);
            if !text.is_empty() && !text.ends_with('\n') {
                out.push_str(nl);
            }
            out.push_str(&format!("{nl}[{section}]{nl}{key} = {new}{nl}"));
        }
    }
    out
}

/// `line` (`key = old   # comment`) with the value replaced by `new`, the comment kept in its column.
fn replace_value(line: &str, new: &str) -> String {
    let Some(eq) = line.find('=') else {
        return line.to_string();
    };
    let after = &line[eq + 1..];
    let rest = after.trim_start_matches([' ', '\t']);
    let lead = &after[..after.len() - rest.len()];
    let old_len = rest
        .find(|c: char| c.is_whitespace() || c == '#')
        .unwrap_or(rest.len());
    let mut tail = rest[old_len..].to_string();
    if tail.starts_with(' ') {
        let spaces = tail.len() - tail.trim_start_matches(' ').len();
        if new.len() > old_len {
            // `false` -> `true` is one character shorter and the other way round one longer:
            // take the difference from (or give it to) the gap in front of the comment.
            let d = (new.len() - old_len).min(spaces.saturating_sub(1));
            tail.drain(..d);
        } else {
            tail.insert_str(0, &" ".repeat(old_len - new.len()));
        }
    }
    format!("{}{lead}{new}{tail}", &line[..eq + 1])
}

/// The commented file written on first run. A test keeps it in sync with `Config::default()`.
pub const DEFAULT_TOML: &str = r##"# Shark Notch configuration. Saved changes are applied immediately; a mistake keeps the previous
# settings and shows the error in the tray tooltip and the log.

[general]
autostart = false              # start with Windows (the tray menu's switch writes this line too)
tray_icon = true               # if false, run the executable again to bring the icon back
monitor = "primary"            # "primary", a number like "2", or a device name like "\\\\.\\DISPLAY2"
exclude_from_capture = true    # hide the notch from screenshots / screen sharing / recordings
log_level = "info"             # error | warn | info | debug

[hotkeys]
toggle = "Ctrl+Alt+N"          # open/close the notch
peek = ""                      # brief peek (optional, off when empty)
next_page = ""
prev_page = ""

[hover]
enabled = true
zone_width = 200.0             # DIPs, centred at the top edge
zone_height = 6.0              # DIPs below the top edge
dwell_ms = 150                 # how long the cursor must rest in the zone
leave_grace_ms = 280           # delay before closing after the cursor leaves
typing_suppress_ms = 600       # ignore hover for this long after a keystroke
poll_hz = 10                   # cursor sampling rate while nothing is near the zone
approach_margin = 140.0

[appearance]
theme = "system"               # system | dark | light
accent = "system"              # "system" or "#RRGGBB"
style = "notch"                # notch (flush with the edge) | island (floating)
idle_visible = true            # draw the tiny idle pill
pill_width = 112.0
pill_height = 6.0
corner_radius = 26.0
corner_smoothing = 0.6         # 0 = circular, 1 = fully continuous
ears = true                    # concave fillets where the notch meets the screen edge
ear_size = 12.0
page_icons = true              # clickable page icons along the bottom (false: plain dots)
scale = 1.0                    # extra UI scale on top of Windows' DPI scaling
max_panel_width = 460.0        # the window is sized once for this; larger module pages are clipped
max_panel_height = 340.0

[animation]
reduce_motion = "system"       # system | on | off
speed = 1.0
bounciness = 0.5               # 0 = none, 1 = very bouncy
stagger_in_ms = 70             # content appears this long after the shape starts moving
stagger_out_ms = 40
reverse_scroll = false

[performance]
gpu_idle_release_secs = 300    # free the GPU stack after this much inactivity (0 = never)
prewarm_on_approach = true
frame_report = false           # log a frame-time report after every animation

[fullscreen]
enabled = true                 # step aside while a game / fullscreen app is in front
scope = "any"                  # any | same_monitor
show_missed_indicator = true
peek_over_fullscreen = false

[modules]
order = ["media", "clipboard", "shelf", "notifications", "calendar", "pomodoro", "live", "stats", "control", "clock", "settings"]   # page order; a module that is disabled in its own section is skipped

[media]
enabled = true                 # follows whatever Windows considers the current media session
peek_on_change = true          # briefly show the new track while the notch is collapsed
peek_secs = 2.8
visualizer = true              # level bars; frames are only drawn while playing AND the page is open
lyrics = false                # opt-in: the line being sung, from lrclib.net (artist, title, album and length are sent to it while the page is open)

[clipboard]
enabled = true
max_items = 30                 # unpinned entries kept (history lives in memory only)
max_images = 8                 # of those, at most this many images (stored as temp files while running)
capture_images = true
max_text_kib = 256             # longer text is ignored
persist_pins = true            # pinned *text* survives restarts (pins.json); nothing else is ever saved
peek_phone_items = true        # briefly show items sent from the iPhone

[shelf]
enabled = true
open_on_drag = true            # a file drag heading for the top of the screen opens the shelf
max_items = 40                 # the shelf only holds references; your files are never moved or deleted

[notifications]
enabled = true                 # the page and pop-ups for banners your iPhone shortcuts send (Windows notifications are not read)
peek = true                    # briefly show a new notification while the notch is collapsed, then tuck away
peek_secs = 4.0
max_items = 20
ignore_apps = []               # e.g. ["Spotify", "Steam"]

[calendar]
enabled = true
feeds = []                     # ICS feeds: "https://…/basic.ics", "webcal://…", or a path to an .ics file. The link is a secret: keep this file private.
refresh_minutes = 30           # the feeds are read again this often (and when the page opens while stale)
week_starts_monday = true
alert_minutes = 5              # a banner (with Join, if the event has a call link) this long before a start, and at the start; 0 = off
chip_minutes = 15              # a countdown in the collapsed pill for an event starting within this long; 0 = off
peek_secs = 8.0

[pomodoro]
enabled = true
focus_minutes = 25.0
short_break_minutes = 5.0
long_break_minutes = 15.0
long_break_every = 4
auto_start_breaks = true
auto_start_focus = false
sound = true                   # the system chime when a session or break ends (never over a fullscreen app)
peek_secs = 6.0
stopwatch = true                # a stopwatch under the timer on the Focus page

[live]
enabled = true
privacy = true                 # a chip while a program is using the microphone or camera (read from Windows' own usage records)
downloads = true               # browser downloads in progress, and a banner when one finishes
download_dir = ""              # empty = your Downloads folder
timer_presets = [1, 5, 10, 15, 30, 60]   # quick timers, in minutes (at most six)
sound = true                   # the system chime when a timer ends (never over a fullscreen app)
peek_secs = 6.0

[stats]
enabled = true
interval_secs = 1.0            # between readings while the page is on screen; nothing is read while it is not
gpu = true                     # read the GPU utilisation counters too (the dearest reading); off hides the GPU tile
net_bits = false               # network speeds in Mbps instead of MB/s
battery_hud = true              # a short banner when the charger is plugged in or removed, and when the battery is full
devices = true                  # a strip with connected Bluetooth devices and their batteries (read while the page is open)
ai_usage = true                # Claude Code's token use today, read from ~/.claude/projects (never sent anywhere): a line here and a chip while it works

[control]
enabled = true                 # volume, brightness, Wi-Fi, Bluetooth, a snip button and Focus; see docs/CONTROL.md
interval_secs = 1.0            # between readings while the page is on screen; nothing is read while it is not
keep_awake = true              # a Keep awake tile; while it is on the PC does not sleep and the screen stays on (off at every start)
mic_mute = true                # a microphone mute tile, and a mute button on the Live page while a program uses the microphone

[phone]
enabled = true                 # false: the link can never run, whatever `listen` says
listen = false                 # the link itself: a small server on your own network that Shortcuts on the phone can send to. Read docs/IPHONE_SHORTCUTS.md first
port = 8765                    # 0 = pick a free port
same_network_only = true       # only phones on the same network as this PC (not any private address)
max_file_mib = 100             # largest file accepted
keep_days = 14                 # received files older than this are deleted

[clock]
enabled = true
hour_format = "system"         # system | 12 | 24
show_seconds = false           # also makes the clock page refresh every second while it is open
show_week = true

[settings]
enabled = true                 # the page with a switch for every module
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_a_boolean_keeps_the_comment_where_it_was() {
        let text =
            "[general]\nautostart = false              # start with Windows\ntray_icon = true\n";
        let on = with_bool(text, "general", "autostart", true);
        assert_eq!(
            on,
            "[general]\nautostart = true               # start with Windows\ntray_icon = true\n"
        );
        assert_eq!(with_bool(&on, "general", "autostart", false), text);
        // Setting what is already set changes nothing.
        assert_eq!(with_bool(text, "general", "autostart", false), text);
    }

    #[test]
    fn the_shipped_file_survives_a_round_trip_through_the_tray_toggle() {
        let on = with_bool(DEFAULT_TOML, "general", "autostart", true);
        let l = Config::parse(&on).expect("still parses");
        assert!(l.config.general.autostart);
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
        let mut expected = Config::default();
        expected.general.autostart = true;
        assert_eq!(l.config, expected, "only autostart changed");
        assert_eq!(with_bool(&on, "general", "autostart", false), DEFAULT_TOML);
    }

    #[test]
    fn only_the_named_section_and_an_uncommented_key_are_touched() {
        let text = "# autostart = true\n[other]\nautostart = false\n[general]\n# autostart = false\nautostart = false\n";
        let out = with_bool(text, "general", "autostart", true);
        assert_eq!(
            out,
            "# autostart = true\n[other]\nautostart = false\n[general]\n# autostart = false\nautostart = true\n"
        );
    }

    #[test]
    fn a_missing_key_or_section_is_added_and_line_endings_are_kept() {
        let out = with_bool(
            "[general]\r\ntray_icon = true\r\n",
            "general",
            "autostart",
            true,
        );
        assert_eq!(out, "[general]\r\nautostart = true\r\ntray_icon = true\r\n");
        let out = with_bool("[hover]\ndwell_ms = 150", "general", "autostart", true);
        assert_eq!(
            out,
            "[hover]\ndwell_ms = 150\n\n[general]\nautostart = true\n"
        );
        let out = with_bool("", "general", "autostart", false);
        assert_eq!(out, "\n[general]\nautostart = false\n");
        assert!(Config::parse(&out).is_ok());
    }

    #[test]
    fn shipped_default_file_equals_default_config() {
        let l = Config::parse(DEFAULT_TOML).expect("default file parses");
        assert_eq!(
            l.config,
            Config::default(),
            "DEFAULT_TOML drifted from Config::default()"
        );
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
    }

    #[test]
    fn quick_timer_presets_and_the_stats_interval_are_bounded() {
        let l = Config::parse(
            "[live]\ntimer_presets = [0, 5, 9999, 10, 15, 20, 25, 30]\n[stats]\ninterval_secs = 0.01\n",
        )
        .unwrap();
        assert_eq!(l.config.live.timer_presets, vec![5, 10, 15, 20, 25, 30]);
        assert_eq!(l.config.stats.interval_secs, 0.5);
        assert!(l.warnings.iter().any(|w| w.contains("live.timer_presets")));
        assert!(l.warnings.iter().any(|w| w.contains("stats.interval_secs")));
        let l = Config::parse("[live]\ntimer_presets = []\n").unwrap();
        assert_eq!(l.config.live.timer_presets, vec![1, 5, 10, 15, 30, 60]);
        assert_eq!(l.warnings.len(), 1);
    }

    #[test]
    fn empty_file_gives_defaults() {
        let l = Config::parse("").unwrap();
        assert_eq!(l.config, Config::default());
    }

    #[test]
    fn partial_file_overrides_only_what_it_names() {
        let l =
            Config::parse("[hover]\ndwell_ms = 300\n[appearance]\ntheme = \"light\"\n").unwrap();
        assert_eq!(l.config.hover.dwell_ms, 300);
        assert_eq!(
            l.config.hover.zone_height, 6.0,
            "sibling keys keep defaults"
        );
        assert_eq!(l.config.appearance.theme, ThemeMode::Light);
        assert_eq!(l.config.general, General::default());
    }

    #[test]
    fn out_of_range_values_are_clamped_with_warnings() {
        let l =
            Config::parse("[hover]\ndwell_ms = 99999\npoll_hz = 0\n[animation]\nspeed = 100.0\n")
                .unwrap();
        assert_eq!(l.config.hover.dwell_ms, 2000);
        assert_eq!(l.config.hover.poll_hz, 2);
        assert_eq!(l.config.animation.speed, 4.0);
        assert_eq!(l.warnings.len(), 3, "{:?}", l.warnings);
        assert!(l.warnings.iter().any(|w| w.contains("hover.dwell_ms")));
    }

    #[test]
    fn unknown_keys_warn_but_do_not_fail() {
        let l = Config::parse("[hover]\ndwel_ms = 10\n[nonsense]\nx = 1\n").unwrap();
        assert!(
            l.warnings.iter().any(|w| w.contains("hover.dwel_ms")),
            "{:?}",
            l.warnings
        );
        assert!(l.warnings.iter().any(|w| w.contains("nonsense")));
        assert_eq!(l.config.hover.dwell_ms, 150, "typo did not change anything");
    }

    #[test]
    fn syntax_and_type_errors_are_errors() {
        assert!(Config::parse("[hover\n").is_err());
        let e = Config::parse("[appearance]\ntheme = \"drak\"\n").unwrap_err();
        assert!(e.contains("drak") || e.contains("variant"), "{e}");
        assert!(Config::parse("[hover]\ndwell_ms = \"fast\"\n").is_err());
        assert!(
            Config::parse("[hover]\ndwell_ms = -5\n").is_err(),
            "negative into u32"
        );
    }

    #[test]
    fn bad_hotkeys_and_colours_are_disabled_not_fatal() {
        let l = Config::parse("[hotkeys]\ntoggle = \"Ctrl+Banana\"\npeek = \"Ctrl+Alt+P\"\n[appearance]\naccent = \"blue\"\n").unwrap();
        assert_eq!(l.config.hotkeys.toggle, "");
        assert_eq!(l.config.hotkeys.peek, "Ctrl+Alt+P");
        assert_eq!(l.config.appearance.accent, "system");
        assert_eq!(l.warnings.len(), 2, "{:?}", l.warnings);
    }

    #[test]
    fn nan_and_infinity_are_rejected() {
        let l = Config::parse("[appearance]\nscale = nan\n[hover]\nzone_width = inf\n").unwrap();
        assert!(l.config.appearance.scale.is_finite() && l.config.hover.zone_width.is_finite());
        assert_eq!(l.warnings.len(), 2);
    }

    #[test]
    fn valid_accent_hex_is_accepted() {
        let l = Config::parse("[appearance]\naccent = \"#ff8800\"\n").unwrap();
        assert_eq!(l.config.appearance.accent, "#ff8800");
        assert!(l.warnings.is_empty());
    }
}
