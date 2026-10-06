//! Where things live on disk.

use std::path::PathBuf;

fn env_dir(var: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// `%APPDATA%\SharkNotch` — user-editable configuration.
pub fn config_dir() -> PathBuf {
    env_dir("APPDATA").join("SharkNotch")
}

/// `%LOCALAPPDATA%\SharkNotch` — logs and persisted data (clips, shelf index, to-dos).
pub fn data_dir() -> PathBuf {
    env_dir("LOCALAPPDATA").join("SharkNotch")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn log_path() -> PathBuf {
    data_dir().join("notch.log")
}
