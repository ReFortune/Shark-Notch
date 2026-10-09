//! OS-backed event producers ("transports"): everything that watches the machine and talks to the
//! bus on worker threads. The module host never sees any of this; it only sees bus events, and
//! asks for things through `Command`s that [`Services::command`] routes here.
//!
//! A service exists only while its module is active in the configuration (`Config::module_active`),
//! so a disabled module costs nothing: no thread, no subscription, no loaded WinRT.

use std::sync::{Arc, Mutex};

use notch_core::bus::BusSender;
use notch_core::config::Config;
use notch_core::image::ImageCache;
use notch_core::module::{Command, ControlCmd, PhoneCmd};

use crate::win::dragdrop::ShelfSlot;

pub mod audio;
pub mod calendar;
pub mod clipboard;
pub mod control;
pub mod downloads;
pub mod media;
pub mod notifications;
pub mod phone;
pub mod privacy;
pub mod shelf;
pub mod stats;
pub mod store;

pub struct Services {
    bus: BusSender,
    images: Arc<ImageCache>,
    pub audio: audio::AudioMeter,
    media: Option<media::MediaService>,
    clipboard: Option<clipboard::ClipboardService>,
    shelf: Option<shelf::ShelfService>,
    notifications: Option<notifications::NotificationsService>,
    store: Option<store::StoreService>,
    calendar: Option<calendar::CalendarService>,
    privacy: Option<privacy::PrivacyService>,
    downloads: Option<downloads::DownloadsService>,
    stats: Option<stats::StatsService>,
    /// The iPhone link's listener: exists only while `[phone] listen` is on.
    phone: Option<phone::PhoneService>,
    /// Started the first time the command-centre page asks for something.
    control: Option<control::ControlService>,
    /// Whether the configuration wants the privacy watcher (it is stopped while suspended).
    want_privacy: bool,
    /// The shelf worker's inbox, shared with the OLE drop target (empty while the shelf is off).
    shelf_slot: ShelfSlot,
    suspended: bool,
}

impl Services {
    pub fn new(bus: BusSender, images: Arc<ImageCache>) -> Services {
        Services {
            bus,
            images,
            audio: audio::AudioMeter::new(),
            media: None,
            clipboard: None,
            shelf: None,
            notifications: None,
            store: None,
            calendar: None,
            privacy: None,
            downloads: None,
            stats: None,
            phone: None,
            control: None,
            want_privacy: false,
            shelf_slot: Arc::new(Mutex::new(None)),
            suspended: false,
        }
    }

    /// Start the services the configuration wants and stop the ones it no longer does.
    pub fn sync(&mut self, cfg: &Config) {
        let want_media = cfg.module_active("media");
        match (want_media, self.media.is_some()) {
            (true, false) => {
                self.media = media::MediaService::start(self.bus.clone(), self.images.clone());
                if self.suspended
                    && let Some(m) = &self.media
                {
                    m.suspend(true);
                }
            }
            (false, true) => {
                if let Some(m) = self.media.take() {
                    m.stop();
                }
            }
            _ => {}
        }

        let want_clip = cfg.module_active("clipboard");
        match (want_clip, self.clipboard.is_some()) {
            (true, false) => {
                self.clipboard = clipboard::ClipboardService::start(
                    cfg.clipboard.clone(),
                    self.bus.clone(),
                    self.images.clone(),
                );
                if self.suspended
                    && let Some(c) = &self.clipboard
                {
                    c.suspend(true);
                }
            }
            (false, true) => {
                if let Some(c) = self.clipboard.take() {
                    c.stop();
                }
            }
            (true, true) => {
                if let Some(c) = &self.clipboard {
                    c.configure(cfg.clipboard.clone());
                }
            }
            (false, false) => {}
        }

        let want_shelf = cfg.module_active("shelf");
        match (want_shelf, self.shelf.is_some()) {
            (true, false) => {
                self.shelf = shelf::ShelfService::start(
                    self.bus.clone(),
                    self.images.clone(),
                    &self.shelf_slot,
                );
            }
            (false, true) => {
                if let Some(s) = self.shelf.take() {
                    s.stop(&self.shelf_slot);
                }
            }
            _ => {}
        }

        let want_notif = cfg.module_active("notifications");
        match (want_notif, self.notifications.is_some()) {
            (true, false) => {
                self.notifications = notifications::NotificationsService::start(
                    cfg.notifications.clone(),
                    self.bus.clone(),
                    self.images.clone(),
                );
            }
            (false, true) => {
                if let Some(n) = self.notifications.take() {
                    n.stop();
                }
            }
            (true, true) => {
                if let Some(n) = &self.notifications {
                    n.configure(cfg.notifications.clone());
                }
            }
            (false, false) => {}
        }

        // The store serves the modules that keep data between runs.
        let want_store = cfg.module_active("pomodoro") || cfg.module_active("live");
        match (want_store, self.store.is_some()) {
            (true, false) => self.store = store::StoreService::start(self.bus.clone()),
            (false, true) => {
                if let Some(s) = self.store.take() {
                    s.stop();
                }
            }
            _ => {}
        }

        // Calendar feeds are only fetched when the module is on and a feed is configured.
        let want_cal = cfg.module_active("calendar") && !cfg.calendar.feeds.is_empty();
        match (want_cal, self.calendar.is_some()) {
            (true, false) => {
                self.calendar =
                    calendar::CalendarService::start(cfg.calendar.clone(), self.bus.clone());
                if self.suspended
                    && let Some(c) = &self.calendar
                {
                    c.suspend(true);
                }
            }
            (false, true) => {
                if let Some(c) = self.calendar.take() {
                    c.stop();
                }
            }
            (true, true) => {
                if let Some(c) = &self.calendar {
                    c.configure(cfg.calendar.clone());
                }
            }
            (false, false) => {}
        }

        // The microphone / camera watcher sleeps on a registry notification; it has no timer.
        self.want_privacy = cfg.module_active("live") && cfg.live.privacy;
        self.sync_privacy();

        // The stats sampler sleeps until the stats page asks for a reading.
        let want_stats = cfg.module_active("stats");
        match (want_stats, self.stats.is_some()) {
            (true, false) => self.stats = stats::StatsService::start(self.bus.clone()),
            (false, true) => {
                if let Some(s) = self.stats.take() {
                    s.stop();
                }
            }
            _ => {}
        }

        // The iPhone listener: a port is open only while the user has switched the link on. A new
        // port means a restart; the other settings apply to the running listener.
        let want_phone = cfg.phone.enabled && cfg.phone.listen;
        let features = phone::Features::of(cfg);
        if self
            .phone
            .as_ref()
            .is_some_and(|p| !want_phone || p.is_dead() || p.setting_port() != cfg.phone.port)
            && let Some(p) = self.phone.take()
        {
            p.stop();
        }
        if want_phone {
            if let Some(p) = &self.phone {
                p.configure(&cfg.phone, features);
            } else {
                self.phone = phone::PhoneService::start(&cfg.phone, features, self.bus.clone());
            }
        }

        // The command centre's worker is created by its first request (see `command`); here it is
        // only stopped when the module goes away.
        if !cfg.module_active("control")
            && let Some(c) = self.control.take()
        {
            c.stop();
        }

        // The Downloads-folder watcher: restarted if the watched folder was changed.
        let want_downloads = cfg.module_active("live") && cfg.live.downloads;
        let running_dir = self.downloads.as_ref().map(|d| d.setting().to_string());
        let changed = running_dir
            .as_deref()
            .is_some_and(|d| d != cfg.live.download_dir);
        if (!want_downloads || changed)
            && let Some(d) = self.downloads.take()
        {
            d.stop();
        }
        if want_downloads && self.downloads.is_none() {
            self.downloads =
                downloads::DownloadsService::start(&cfg.live.download_dir, self.bus.clone());
        }
    }

    /// Start or stop the watcher to match the configuration and the suspended state.
    fn sync_privacy(&mut self) {
        match (self.want_privacy && !self.suspended, self.privacy.is_some()) {
            (true, false) => self.privacy = privacy::PrivacyService::start(self.bus.clone()),
            (false, true) => {
                if let Some(p) = self.privacy.take() {
                    p.stop();
                }
            }
            _ => {}
        }
    }

    /// Has the command centre's worker been started? (It starts with the page's first request.)
    pub fn control_running(&self) -> bool {
        self.control.is_some()
    }

    pub fn shelf_slot(&self) -> ShelfSlot {
        self.shelf_slot.clone()
    }

    pub fn shelf(&self) -> Option<&shelf::ShelfService> {
        self.shelf.as_ref()
    }

    /// The clipboard service, if its module is active (text from the iPhone is added through it).
    pub fn clipboard(&self) -> Option<&clipboard::ClipboardService> {
        self.clipboard.as_ref()
    }

    /// The iPhone link's pairing token, once the listener has one.
    pub fn phone_token(&self) -> Option<String> {
        self.phone.as_ref().and_then(|p| p.token())
    }

    /// The port the iPhone listener is on (the configured one, or the one the system picked).
    pub fn phone_port(&self) -> Option<u16> {
        self.phone.as_ref().map(|p| p.port())
    }

    /// Route a module's request to the service that owns it. Returns whether it was handled.
    pub fn command(&mut self, cmd: &Command) -> bool {
        match cmd {
            Command::Media(c) => {
                if let Some(m) = &self.media {
                    m.command(*c);
                }
                true
            }
            Command::Clipboard(c) => {
                if let Some(s) = &self.clipboard {
                    s.command(*c);
                }
                true
            }
            Command::Shelf(c) => {
                if let Some(s) = &self.shelf {
                    s.command(c);
                }
                true
            }
            Command::Notifications(c) => {
                if let Some(n) = &self.notifications {
                    n.command(*c);
                }
                true
            }
            Command::Calendar(c) => {
                if let Some(s) = &self.calendar {
                    s.command(*c);
                }
                true
            }
            Command::Store(c) => {
                if let Some(s) = &self.store {
                    s.command(c);
                }
                true
            }
            Command::Stats(c) => {
                if let Some(s) = &self.stats {
                    s.command(*c);
                }
                true
            }
            Command::Control(c) => {
                // Opening Windows pages is the UI thread's job (`App::exec`); the rest is ours.
                if matches!(
                    c,
                    ControlCmd::Snip
                        | ControlCmd::OpenFocusSettings
                        | ControlCmd::OpenRadioSettings
                        | ControlCmd::OpenAirplaneSettings
                ) {
                    return false;
                }
                if self.control.is_none() {
                    self.control = control::ControlService::start(self.bus.clone());
                }
                if let Some(s) = &self.control {
                    s.command(*c);
                }
                true
            }
            Command::Phone(c) => {
                // Putting the token on the clipboard needs the app's window: `App::exec` does it.
                if *c == PhoneCmd::CopyToken {
                    return false;
                }
                if let Some(p) = &self.phone {
                    match c {
                        PhoneCmd::NewToken => {
                            p.new_token();
                        }
                        PhoneCmd::CopyToken => {}
                    }
                }
                true
            }
            // Handled by the app itself (they need the UI thread or the config path).
            Command::OpenUrl(_)
            | Command::OpenConfig
            | Command::SetBool { .. }
            | Command::Chime
            | Command::Reveal(_) => false,
        }
    }

    /// The shell stepped aside (fullscreen app, pause, lock): stop doing background work.
    pub fn suspend(&mut self, on: bool) {
        self.suspended = on;
        if let Some(m) = &self.media {
            m.suspend(on);
        }
        if let Some(c) = &self.clipboard {
            c.suspend(on);
        }
        if let Some(c) = &self.calendar {
            c.suspend(on);
        }
        self.sync_privacy();
        if on {
            self.audio.stop();
        }
    }

    pub fn shutdown(&mut self) {
        self.audio.stop();
        if let Some(m) = self.media.take() {
            m.stop();
        }
        if let Some(c) = self.clipboard.take() {
            c.stop();
        }
        if let Some(s) = self.shelf.take() {
            s.stop(&self.shelf_slot);
        }
        if let Some(n) = self.notifications.take() {
            n.stop();
        }
        if let Some(c) = self.calendar.take() {
            c.stop();
        }
        if let Some(p) = self.privacy.take() {
            p.stop();
        }
        if let Some(d) = self.downloads.take() {
            d.stop();
        }
        if let Some(s) = self.stats.take() {
            s.stop();
        }
        if let Some(c) = self.control.take() {
            c.stop();
        }
        if let Some(p) = self.phone.take() {
            p.stop();
        }
        // Last: it flushes whatever the modules saved a moment ago.
        if let Some(s) = self.store.take() {
            s.stop();
        }
    }
}

impl Drop for Services {
    fn drop(&mut self) {
        self.shutdown();
    }
}
