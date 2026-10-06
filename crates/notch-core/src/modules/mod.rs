//! The built-in modules. Each lives in its own file and is registered here; the host only
//! instantiates the ones the configuration enables.

use crate::module::Factory;

pub mod clipboard;
pub mod clock;
pub mod media;
pub mod notifications;
pub mod shelf;

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
            id: "clock",
            create: clock::create,
        },
    ]
}
