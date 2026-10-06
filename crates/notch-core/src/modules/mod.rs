//! The built-in modules. Each lives in its own file and is registered here; the host only
//! instantiates the ones the configuration enables.

use crate::module::Factory;

pub mod clock;

/// Every module this build knows about, in no particular order (page order comes from the config).
pub fn registry() -> Vec<Factory> {
    vec![Factory {
        id: "clock",
        create: clock::create,
    }]
}
