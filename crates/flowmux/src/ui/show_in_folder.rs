// SPDX-License-Identifier: GPL-3.0-or-later
//! Open the system file viewer at a directory path.
//!
//! Used by the sidebar workspace context menu and the pane tab context
//! menu ("Show in folder"). The viewer is the platform file manager:
//! Finder on macOS, Explorer under WSL, and whatever `xdg-open` resolves
//! `inode/directory` to on other native builds.
//!
//! Two execution paths, picked at runtime:
//!
//! - WSL/WSLg: spawn `explorer.exe <windows-dir>` after translating the
//!   Linux path through `wslpath -w`.
//! - Native macOS build: spawn `/usr/bin/open -R <dir>` directly.
//! - Other native builds: spawn `xdg-open <dir>` directly.
//! - Flatpak sandbox: spawn `flatpak-spawn --host xdg-open <dir>` so the
//!   file manager runs on the host, not inside the sandbox where it
//!   isn't installed. Detected by the presence of `/.flatpak-info`,
//!   the same marker the rest of the codebase uses for its
//!   `flatpak-spawn --host` shell wrapping. The sandbox already has
//!   `--talk-name=org.freedesktop.Flatpak` per the manifest, so the
//!   spawn call works without portal plumbing.
//!
//! Non-macOS spawning goes through `gio::Subprocess` so the GLib main
//! loop reaps the child via its built-in child-watch source. macOS
//! uses `/usr/bin/open` directly and waits for the short-lived helper
//! on a background thread.

#[cfg(not(target_os = "macos"))]
use gtk::gio;
#[cfg(not(target_os = "macos"))]
use std::ffi::OsStr;
use std::ffi::OsString;
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
    match spawn_viewer(&argv) {
        Ok(()) => {
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

#[cfg(target_os = "macos")]
fn spawn_viewer(argv: &[OsString]) -> Result<(), String> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .spawn()
        .map_err(|err| err.to_string())?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn spawn_viewer(argv: &[OsString]) -> Result<(), String> {
    let argv_refs: Vec<&OsStr> = argv.iter().map(OsString::as_os_str).collect();
    gio::Subprocess::newv(&argv_refs, gio::SubprocessFlags::NONE)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

fn viewer_argv(dir: &Path) -> Option<Vec<OsString>> {
    let running_under_wsl = crate::platform::running_under_wsl();
    let wsl_disabled = crate::platform::env_flag_enabled("FLOWMUX_NO_WSL_EXPLORER");
    let windows_path = (running_under_wsl && !wsl_disabled).then(|| wsl_windows_path(dir));
    let argv = viewer_argv_for_platform(
        dir,
        running_under_wsl,
        wsl_disabled,
        windows_path.flatten(),
        wsl_explorer_program(),
        in_flatpak_sandbox(),
    );
    if argv.is_none() && running_under_wsl && !wsl_disabled {
        tracing::warn!(
            path = %dir.display(),
            "show-in-folder: failed to convert WSL path for Explorer",
        );
    }
    argv
}

fn viewer_argv_for_platform(
    dir: &Path,
    running_under_wsl: bool,
    wsl_disabled: bool,
    windows_path: Option<OsString>,
    explorer_program: OsString,
    in_flatpak: bool,
) -> Option<Vec<OsString>> {
    if running_under_wsl && !wsl_disabled {
        return windows_path.map(|path| vec![explorer_program, path]);
    }

    if in_flatpak {
        Some(vec![
            OsString::from("flatpak-spawn"),
            OsString::from("--host"),
            OsString::from("xdg-open"),
            dir.as_os_str().to_os_string(),
        ])
    } else {
        Some(native_viewer_argv(dir))
    }
}

#[cfg(target_os = "macos")]
fn native_viewer_argv(dir: &Path) -> Vec<OsString> {
    vec![
        OsString::from("/usr/bin/open"),
        OsString::from("-R"),
        dir.as_os_str().to_os_string(),
    ]
}

#[cfg(not(target_os = "macos"))]
fn native_viewer_argv(dir: &Path) -> Vec<OsString> {
    vec![OsString::from("xdg-open"), dir.as_os_str().to_os_string()]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(argv: Vec<OsString>) -> Vec<String> {
        argv.into_iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn wsl_prefers_explorer_with_translated_windows_path() {
        let argv = viewer_argv_for_platform(
            Path::new("/home/junsu/project"),
            true,
            false,
            Some(OsString::from(r"C:\Users\junsu\project")),
            OsString::from("explorer.exe"),
            false,
        )
        .unwrap();
        assert_eq!(
            strings(argv),
            vec!["explorer.exe", r"C:\Users\junsu\project"]
        );
    }

    #[test]
    fn wsl_returns_none_when_path_translation_fails() {
        assert!(viewer_argv_for_platform(
            Path::new("/home/junsu/project"),
            true,
            false,
            None,
            OsString::from("explorer.exe"),
            false,
        )
        .is_none());
    }

    #[test]
    fn wsl_disable_env_falls_back_to_native_viewer() {
        let argv = viewer_argv_for_platform(
            Path::new("/home/junsu/project"),
            true,
            true,
            Some(OsString::from(r"C:\Users\junsu\project")),
            OsString::from("explorer.exe"),
            false,
        )
        .unwrap();
        #[cfg(target_os = "macos")]
        assert_eq!(
            strings(argv),
            vec!["/usr/bin/open", "-R", "/home/junsu/project"]
        );

        #[cfg(not(target_os = "macos"))]
        assert_eq!(strings(argv), vec!["xdg-open", "/home/junsu/project"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_macos_uses_finder_open() {
        let argv = viewer_argv_for_platform(
            Path::new("/Users/junsu/project"),
            false,
            false,
            None,
            OsString::from("explorer.exe"),
            false,
        )
        .unwrap();
        assert_eq!(
            strings(argv),
            vec!["/usr/bin/open", "-R", "/Users/junsu/project"]
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn native_non_macos_uses_xdg_open() {
        let argv = viewer_argv_for_platform(
            Path::new("/home/junsu/project"),
            false,
            false,
            None,
            OsString::from("explorer.exe"),
            false,
        )
        .unwrap();
        assert_eq!(strings(argv), vec!["xdg-open", "/home/junsu/project"]);
    }

    #[test]
    fn flatpak_uses_host_xdg_open_outside_wsl() {
        let argv = viewer_argv_for_platform(
            Path::new("/home/junsu/project"),
            false,
            false,
            None,
            OsString::from("explorer.exe"),
            true,
        )
        .unwrap();
        assert_eq!(
            strings(argv),
            vec!["flatpak-spawn", "--host", "xdg-open", "/home/junsu/project"]
        );
    }
}
