//! The built-in modules. Each lives in its own file and is registered here; the host only
//! instantiates the ones the configuration enables.

use crate::module::Factory;

pub mod calendar;
pub mod clipboard;
pub mod clock;
pub mod live;
pub mod media;
pub mod notifications;
pub mod pomodoro;
pub mod shelf;
pub mod stats;

/// Every module this build knows about, in no particular order (page order comes from the config).
pub fn registry() -> Vec<Factory> {
    vec![
        Factory {
            id: "media",
            create: media::create,
        },
        Factory {
            id: "clipboard",
            create: clipboard::create,
        },
        Factory {
            id: "shelf",
            create: shelf::create,
        },
        Factory {
            id: "notifications",
            create: notifications::create,
        },
        Factory {
            id: "calendar",
            create: calendar::create,
        },
        Factory {
            id: "pomodoro",
            create: pomodoro::create,
        },
        Factory {
            id: "live",
            create: live::create,
        },
        Factory {
            id: "stats",
            create: stats::create,
        },
        Factory {
            id: "clock",
            create: clock::create,
        },
    ]
}
