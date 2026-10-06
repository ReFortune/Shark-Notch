//! A deliberately tiny file logger (no dependencies): timestamped lines, size-capped with one
//! rotation, level filter that can be changed at runtime when the config reloads.

use std::fmt::Arguments;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    pub fn parse(s: &str) -> Level {
        match s {
            "error" => Level::Error,
            "warn" => Level::Warn,
            "debug" => Level::Debug,
            _ => Level::Info,
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
        }
    }
}

struct Logger {
    level: Level,
    file: Option<File>,
    written: u64,
    echo: bool,
    last_lines: std::collections::VecDeque<String>,
}

static LOGGER: OnceLock<Mutex<Logger>> = OnceLock::new();
const MAX_BYTES: u64 = 512 * 1024;
const KEEP_LINES: usize = 64;

/// Open the log file (rotating it if it is large). `echo` also prints to stderr/stdout.
pub fn init(path: &Path, level: Level, echo: bool) {
    let mut file = None;
    let mut written = 0;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() > MAX_BYTES
    {
        let mut old: PathBuf = path.to_path_buf();
        old.set_extension("log.old");
        let _ = std::fs::rename(path, old);
    }
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(path) {
        written = f.metadata().map(|m| m.len()).unwrap_or(0);
        file = Some(f);
    }
    let _ = LOGGER.set(Mutex::new(Logger {
        level,
        file,
        written,
        echo,
        last_lines: Default::default(),
    }));
}

pub fn set_level(level: Level) {
    if let Some(l) = LOGGER.get()
        && let Ok(mut l) = l.lock()
    {
        l.level = level;
    }
}

/// The most recent lines, for the tray "Diagnostics" item.
pub fn recent() -> String {
    LOGGER
        .get()
        .and_then(|l| {
            l.lock()
                .ok()
                .map(|l| l.last_lines.iter().cloned().collect::<Vec<_>>().join("\n"))
        })
        .unwrap_or_default()
}

pub fn write(level: Level, args: Arguments<'_>) {
    let Some(logger) = LOGGER.get() else { return };
    let Ok(mut l) = logger.lock() else { return };
    if level > l.level {
        return;
    }
    let line = format!(
        "{} {} {}\n",
        crate::win::util::local_time_string(),
        level.tag(),
        args
    );
    let under_cap = l.written < MAX_BYTES * 2;
    if under_cap
        && let Some(f) = l.file.as_mut()
        && f.write_all(line.as_bytes()).is_ok()
    {
        l.written += line.len() as u64;
    }
    if l.echo {
        print!("{line}");
    }
    if l.last_lines.len() == KEEP_LINES {
        l.last_lines.pop_front();
    }
    l.last_lines.push_back(line.trim_end().to_string());
}

#[macro_export]
macro_rules! error {
    ($($t:tt)*) => { $crate::log::write($crate::log::Level::Error, format_args!($($t)*)) };
}
#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { $crate::log::write($crate::log::Level::Warn, format_args!($($t)*)) };
}
#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { $crate::log::write($crate::log::Level::Info, format_args!($($t)*)) };
}
#[macro_export]
macro_rules! debug {
    ($($t:tt)*) => { $crate::log::write($crate::log::Level::Debug, format_args!($($t)*)) };
}
