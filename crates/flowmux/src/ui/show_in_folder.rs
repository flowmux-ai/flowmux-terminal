// SPDX-License-Identifier: GPL-3.0-or-later
//! Open the system file viewer at a directory path.
//!
//! Used by the sidebar workspace context menu and the pane tab context
//! menu ("Show in folder"). The viewer is whatever `xdg-open` resolves
//! `inode/directory` to (Nautilus on a default Ubuntu/GNOME install).
//!
//! Two execution paths, picked at runtime:
//!
//! - WSL/WSLg: spawn `explorer.exe <windows-dir>` after translating the
//!   Linux path through `wslpath -w`.
//! - Native build: spawn `xdg-open <dir>` directly.
//! - Flatpak sandbox: spawn `flatpak-spawn --host xdg-open <dir>` so the
//!   file manager runs on the host, not inside the sandbox where it
//!   isn't installed. Detected by the presence of `/.flatpak-info`,
//!   the same marker the rest of the codebase uses for its
//!   `flatpak-spawn --host` shell wrapping. The sandbox already has
//!   `--talk-name=org.freedesktop.Flatpak` per the manifest, so the
//!   spawn call works without portal plumbing.
//!
//! Spawning goes through `gio::Subprocess` so the GLib main loop reaps
//! the child via its built-in child-watch source. A bare
//! `std::process::Command` would leak a zombie until the GUI exits.

use gtk::gio;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

/// Open the user's file manager at `dir`. Logs on failure but does not
/// surface an error to the caller — the menu item is best-effort.
pub fn open_directory(dir: &Path) {
    if !dir.is_dir() {
        tracing::warn!(path = %dir.display(), "show-in-folder: path is not a directory");
        return;
    }
    let Some(argv) = viewer_argv(dir) else {
        return;
    };
    let program = argv[0].to_string_lossy().into_owned();
    let argv_refs: Vec<&OsStr> = argv.iter().map(OsString::as_os_str).collect();
    match gio::Subprocess::newv(
        &argv_refs,
        gio::SubprocessFlags::NONE,
    ) {
        Ok(_child) => {
            tracing::info!(path = %dir.display(), "show-in-folder: spawned {}", program);
        }
        Err(e) => {
            tracing::warn!(
                path = %dir.display(),
                error = %e,
                "show-in-folder: failed to spawn {}",
                program,
            );
        }
    }
}

fn viewer_argv(dir: &Path) -> Option<Vec<OsString>> {
    if crate::platform::running_under_wsl()
        && !crate::platform::env_flag_enabled("FLOWMUX_NO_WSL_EXPLORER")
    {
        let Some(windows_path) = wsl_windows_path(dir) else {
            tracing::warn!(
                path = %dir.display(),
                "show-in-folder: failed to convert WSL path for Explorer",
            );
            return None;
        };
        return Some(vec![wsl_explorer_program(), windows_path]);
    }

    if in_flatpak_sandbox() {
        Some(vec![
            OsString::from("flatpak-spawn"),
            OsString::from("--host"),
            OsString::from("xdg-open"),
            dir.as_os_str().to_os_string(),
        ])
    } else {
        Some(vec![OsString::from("xdg-open"), dir.as_os_str().to_os_string()])
    }
}

fn wsl_windows_path(dir: &Path) -> Option<OsString> {
    let output = Command::new("wslpath").arg("-w").arg(dir).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim_end_matches(['\r', '\n']);
    if path.is_empty() {
        None
    } else {
        Some(OsString::from(path))
    }
}

fn wsl_explorer_program() -> OsString {
    let path = Path::new("/mnt/c/Windows/explorer.exe");
    if path.is_file() {
        path.as_os_str().to_os_string()
    } else {
        OsString::from("explorer.exe")
    }
}

/// True when this process is running inside a Flatpak sandbox. Matches
/// the detection that `flowmux-cli` and the terminal pane already use.
fn in_flatpak_sandbox() -> bool {
    Path::new("/.flatpak-info").exists() || std::env::var_os("FLATPAK_ID").is_some()
}
