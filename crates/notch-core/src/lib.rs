//! Portable core of Shark Notch. Everything here is plain Rust with no OS calls so that it can be
//! unit-tested on any host; the Windows binary (`shark-notch`) only supplies OS glue and pixels.

pub mod color;
pub mod compose;
pub mod config;
pub mod demo;
pub mod draw;
pub mod frame;
pub mod fullscreen;
pub mod geom;
pub mod hotkey;
pub mod hover;
pub mod icons;
pub mod input;
pub mod path;
pub mod raster;
pub mod sched;
pub mod shell;
pub mod spring;
pub mod theme;
