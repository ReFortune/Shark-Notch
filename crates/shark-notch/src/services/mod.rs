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
use notch_core::module::Command;

use crate::win::dragdrop::ShelfSlot;

pub mod audio;
pub mod calendar;
pub mod clipboard;
pub mod media;
pub mod notifications;
pub mod shelf;
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
        let want_store = cfg.module_active("pomodoro");
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
    }

    pub fn shelf_slot(&self) -> ShelfSlot {
        self.shelf_slot.clone()
    }

    pub fn shelf(&self) -> Option<&shelf::ShelfService> {
        self.shelf.as_ref()
    }

    /// The clipboard service, if its module is active (the iPhone listener adds items through it).
    #[allow(dead_code)] // used by the iPhone listener (phase 11)
    pub fn clipboard(&self) -> Option<&clipboard::ClipboardService> {
        self.clipboard.as_ref()
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
            // Handled by the app itself (they need the UI thread or the config path).
            Command::OpenUrl(_) | Command::OpenConfig | Command::Chime => false,
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
