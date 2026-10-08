//! Where things live on disk.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Where `data_dir()` points instead of `%LOCALAPPDATA%\SharkNotch` (the self-test must never touch
/// a real user's saved data).
static DATA_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

pub fn set_data_dir(dir: PathBuf) {
    let _ = DATA_DIR_OVERRIDE.set(dir);
}

fn env_dir(var: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// `%APPDATA%\SharkNotch` — user-editable configuration.
pub fn config_dir() -> PathBuf {
    env_dir("APPDATA").join("SharkNotch")
}

/// `%LOCALAPPDATA%\SharkNotch` — logs and persisted data (pinned clip text, tasks and timers, the iPhone token and inbox).
pub fn data_dir() -> PathBuf {
    match DATA_DIR_OVERRIDE.get() {
        Some(d) => d.clone(),
        None => env_dir("LOCALAPPDATA").join("SharkNotch"),
    }
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn log_path() -> PathBuf {
    data_dir().join("notch.log")
}
