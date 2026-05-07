// SPDX-License-Identifier: GPL-3.0-or-later
//! XDG paths for flowmux. cmux on macOS uses
//! `~/Library/Application Support/cmux`; on Linux we follow XDG.

use std::path::PathBuf;

pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("flowmux"))
}

pub fn data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("flowmux"))
}

pub fn state_dir() -> Option<PathBuf> {
    dirs::state_dir().map(|d| d.join("flowmux"))
}

pub fn runtime_socket() -> PathBuf {
    if let Some(rt) = dirs::runtime_dir() {
        rt.join("flowmux.sock")
    } else {
        std::env::temp_dir().join(format!("flowmux-{}.sock", whoami()))
    }
}

pub fn ghostty_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("ghostty").join("config"))
}

fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "anon".into())
}
