//! Thin, safe-ish wrappers over the Win32 surface the app uses. Everything `unsafe` in the crate
//! lives behind these modules or in `gfx`; the rest of the app (shell wiring, config) is safe Rust.

pub mod autostart;
pub mod cfgwatch;
pub mod clock;
pub mod fullscreen;
pub mod hotkeys;
pub mod inbox;
pub mod layout;
pub mod paths;
pub mod sampler;
pub mod session;
pub mod single;
pub mod sys;
pub mod textclip;
pub mod tray;
pub mod util;
pub mod window;
