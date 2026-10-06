//! OS-backed event producers ("transports"): everything that watches the machine and talks to the
//! bus on worker threads. The module host never sees any of this; it only sees bus events, and
//! asks for things through `Command`s that [`Services::command`] routes here.
//!
//! A service exists only while its module is active in the configuration (`Config::module_active`),
//! so a disabled module costs nothing: no thread, no subscription, no loaded WinRT.

use std::sync::Arc;

use notch_core::bus::BusSender;
use notch_core::config::Config;
use notch_core::image::ImageCache;
use notch_core::module::Command;

pub mod audio;
pub mod media;

pub struct Services {
    bus: BusSender,
    images: Arc<ImageCache>,
    pub audio: audio::AudioMeter,
    media: Option<media::MediaService>,
    suspended: bool,
}

impl Services {
    pub fn new(bus: BusSender, images: Arc<ImageCache>) -> Services {
        Services {
            bus,
            images,
            audio: audio::AudioMeter::new(),
            media: None,
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
            Command::OpenUrl(_) => false,
        }
    }

    /// The shell stepped aside (fullscreen app, pause, lock): stop doing background work.
    pub fn suspend(&mut self, on: bool) {
        self.suspended = on;
        if let Some(m) = &self.media {
            m.suspend(on);
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
    }
}

impl Drop for Services {
    fn drop(&mut self) {
        self.shutdown();
    }
}
