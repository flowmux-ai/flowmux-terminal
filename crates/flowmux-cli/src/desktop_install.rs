// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-user install of the freedesktop launcher entry + hicolor icons.
//!
//! Why this lives inside flowmuxctl: a `cargo install --path crates/flowmux`
//! drops only the binary into `$CARGO_HOME/bin`. The freedesktop spec
//! requires a `.desktop` file under `$XDG_DATA_HOME/applications` and
//! matching icons under `$XDG_DATA_HOME/icons/hicolor/<size>/apps/` for
//! the app to show up in the launcher / dock. The .deb packaging
//! installs these system-wide, but per-user `cargo install` cannot.
//! Embedding both as `include_bytes!` and writing them out from
//! `flowmux fix` keeps the install story to "two `cargo install`s plus
//! one `flowmux fix`" with no extra script.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Freedesktop application id — must match the launcher Exec target and
/// `StartupWMClass` so libwayland / mutter associate the running window
/// with the entry. Mirrors the .deb assets layout.
pub const APP_ID: &str = "com.flowmux.App";

/// `.desktop` text embedded at compile time.
pub const DESKTOP_ENTRY: &str = include_str!("../../../resources/desktop/com.flowmux.App.desktop");

/// One icon size we ship. Hicolor expects per-resolution PNGs plus an
/// optional scalable SVG. Sizes mirror the .deb assets list.
struct IconAsset {
    /// Edge length in pixels, used to build `<size>x<size>/apps/<APP_ID>.png`.
    size: u32,
    bytes: &'static [u8],
}

const ICONS: &[IconAsset] = &[
    IconAsset {
        size: 16,
        bytes: include_bytes!("../../../resources/icons/flowmux-16.png"),
    },
    IconAsset {
        size: 24,
        bytes: include_bytes!("../../../resources/icons/flowmux-24.png"),
    },
    IconAsset {
        size: 32,
        bytes: include_bytes!("../../../resources/icons/flowmux-32.png"),
    },
    IconAsset {
        size: 48,
        bytes: include_bytes!("../../../resources/icons/flowmux-48.png"),
    },
    IconAsset {
        size: 64,
        bytes: include_bytes!("../../../resources/icons/flowmux-64.png"),
    },
    IconAsset {
        size: 96,
        bytes: include_bytes!("../../../resources/icons/flowmux-96.png"),
    },
    IconAsset {
        size: 128,
        bytes: include_bytes!("../../../resources/icons/flowmux-128.png"),
    },
    IconAsset {
        size: 256,
        bytes: include_bytes!("../../../resources/icons/flowmux-256.png"),
    },
    IconAsset {
        size: 512,
        bytes: include_bytes!("../../../resources/icons/flowmux-512.png"),
    },
];

const SCALABLE_SVG: &[u8] = include_bytes!("../../../resources/icons/flowmux.svg");

/// Where the desktop entry / icons end up. Tests can construct one
/// against a tempdir; production callers go through [`resolve`].
#[derive(Debug, Clone)]
pub struct DesktopLayout {
    /// `$XDG_DATA_HOME/applications` (or `$HOME/.local/share/applications`).
    pub apps_dir: PathBuf,
    /// `$XDG_DATA_HOME/icons/hicolor`.
    pub icons_root: PathBuf,
}

impl DesktopLayout {
    pub fn from_data_home(data_home: &Path) -> Self {
        Self {
            apps_dir: data_home.join("applications"),
            icons_root: data_home.join("icons").join("hicolor"),
        }
    }

    pub fn desktop_path(&self) -> PathBuf {
        self.apps_dir.join(format!("{APP_ID}.desktop"))
    }

    fn icon_png_path(&self, size: u32) -> PathBuf {
        self.icons_root
            .join(format!("{size}x{size}"))
            .join("apps")
            .join(format!("{APP_ID}.png"))
    }

    fn icon_svg_path(&self) -> PathBuf {
        self.icons_root
            .join("scalable")
            .join("apps")
            .join(format!("{APP_ID}.svg"))
    }
}

/// Resolve the real `$XDG_DATA_HOME` (or fallback). `dirs::data_dir()`
/// already implements the fallback chain.
pub fn resolve() -> Result<DesktopLayout> {
    let data_home = dirs::data_dir().ok_or_else(|| {
        anyhow::anyhow!("$XDG_DATA_HOME / $HOME unset; cannot place desktop entry")
    })?;
    Ok(DesktopLayout::from_data_home(&data_home))
}

/// Per-asset doctor status. Same shape as `agent::DoctorStatus` but
/// kept local so the icon byte-compare path doesn't drag UTF-8 reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetStatus {
    /// Present and bytes match the embedded payload.
    Ok,
    /// Present but bytes differ — `flowmux fix` overwrites.
    Drift,
    /// Not on disk — `flowmux fix` writes it.
    Missing,
    /// I/O error while inspecting (permissions etc.).
    Error(String),
}

/// Snapshot of the desktop entry + every icon size in one struct so
/// doctor can render one row per asset family.
#[derive(Debug, Clone)]
pub struct DesktopDoctor {
    pub desktop: AssetStatus,
    pub icons: Vec<(u32, AssetStatus)>,
    pub svg: AssetStatus,
}

impl DesktopDoctor {
    /// True when at least one asset is missing or drifted. Maps to
    /// `Status::NeedsFix` in the outer doctor report.
    #[allow(dead_code)]
    pub fn needs_fix(&self) -> bool {
        let dirty = |s: &AssetStatus| matches!(s, AssetStatus::Missing | AssetStatus::Drift);
        dirty(&self.desktop) || dirty(&self.svg) || self.icons.iter().any(|(_, s)| dirty(s))
    }

    /// True if any read returned an I/O error.
    pub fn has_error(&self) -> bool {
        let err = |s: &AssetStatus| matches!(s, AssetStatus::Error(_));
        err(&self.desktop) || err(&self.svg) || self.icons.iter().any(|(_, s)| err(s))
    }
}

fn check_bytes(path: &Path, expected: &[u8]) -> AssetStatus {
    if !path.exists() {
        return AssetStatus::Missing;
    }
    match fs::read(path) {
        Ok(bytes) if bytes == expected => AssetStatus::Ok,
        Ok(_) => AssetStatus::Drift,
        Err(e) => AssetStatus::Error(e.to_string()),
    }
}

fn check_text(path: &Path, expected: &str) -> AssetStatus {
    if !path.exists() {
        return AssetStatus::Missing;
    }
    match fs::read_to_string(path) {
        Ok(s) if s == expected => AssetStatus::Ok,
        Ok(_) => AssetStatus::Drift,
        Err(e) => AssetStatus::Error(e.to_string()),
    }
}

/// Inspect the on-disk desktop entry + icons without writing.
pub fn doctor(layout: &DesktopLayout) -> DesktopDoctor {
    let desktop = check_text(&layout.desktop_path(), DESKTOP_ENTRY);
    let svg = check_bytes(&layout.icon_svg_path(), SCALABLE_SVG);
    let icons = ICONS
        .iter()
        .map(|asset| {
            (
                asset.size,
                check_bytes(&layout.icon_png_path(asset.size), asset.bytes),
            )
        })
        .collect();
    DesktopDoctor {
        desktop,
        icons,
        svg,
    }
}

/// Outcome of a single `install` run.
#[derive(Debug, Clone, Default)]
pub struct InstallOutcome {
    /// Files actually written. Empty when everything was already
    /// up-to-date.
    pub written: Vec<PathBuf>,
    /// Files that were already correct.
    pub already_ok: Vec<PathBuf>,
}

impl InstallOutcome {
    pub fn touched_count(&self) -> usize {
        self.written.len()
    }
}

fn write_if_changed_bytes(path: &Path, payload: &[u8], outcome: &mut InstallOutcome) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    if path.exists() {
        if let Ok(existing) = fs::read(path) {
            if existing == payload {
                outcome.already_ok.push(path.to_path_buf());
                return Ok(());
            }
        }
    }
    fs::write(path, payload).with_context(|| format!("writing {}", path.display()))?;
    outcome.written.push(path.to_path_buf());
    Ok(())
}

fn write_if_changed_text(path: &Path, payload: &str, outcome: &mut InstallOutcome) -> Result<()> {
    write_if_changed_bytes(path, payload.as_bytes(), outcome)
}

/// Idempotent install of the desktop entry + every embedded icon.
pub fn install(layout: &DesktopLayout) -> Result<InstallOutcome> {
    let mut outcome = InstallOutcome::default();
    write_if_changed_text(&layout.desktop_path(), DESKTOP_ENTRY, &mut outcome)?;
    for asset in ICONS {
        write_if_changed_bytes(&layout.icon_png_path(asset.size), asset.bytes, &mut outcome)?;
    }
    write_if_changed_bytes(&layout.icon_svg_path(), SCALABLE_SVG, &mut outcome)?;
    Ok(outcome)
}

/// Refresh the freedesktop caches so the launcher picks up a newly
/// installed entry without a logout. Both tools are best-effort; a
/// failure is logged but never returned as an error because the user
/// can refresh by re-logging in.
pub fn refresh_caches(layout: &DesktopLayout) {
    use std::process::Command;
    let _ = Command::new("update-desktop-database")
        .arg(&layout.apps_dir)
        .status();
    let _ = Command::new("gtk-update-icon-cache")
        .arg("--quiet")
        .arg(&layout.icons_root)
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_layout() -> (tempfile::TempDir, DesktopLayout) {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = DesktopLayout::from_data_home(dir.path());
        (dir, layout)
    }

    #[test]
    fn install_writes_every_asset_then_is_idempotent() {
        let (_dir, layout) = tmp_layout();
        let first = install(&layout).expect("first install");
        assert!(!first.written.is_empty());
        assert!(first.already_ok.is_empty());

        let second = install(&layout).expect("second install");
        assert!(second.written.is_empty());
        assert_eq!(second.already_ok.len(), first.written.len());
    }

    #[test]
    fn doctor_flags_missing_then_clears_after_install() {
        let (_dir, layout) = tmp_layout();
        let pre = doctor(&layout);
        assert!(pre.needs_fix());
        assert!(matches!(pre.desktop, AssetStatus::Missing));

        install(&layout).expect("install");
        let post = doctor(&layout);
        assert!(!post.needs_fix());
        assert!(matches!(post.desktop, AssetStatus::Ok));
        for (_size, status) in &post.icons {
            assert!(matches!(status, AssetStatus::Ok));
        }
    }

    #[test]
    fn doctor_flags_drift_when_file_modified() {
        let (_dir, layout) = tmp_layout();
        install(&layout).expect("install");
        fs::write(layout.desktop_path(), "garbage").expect("overwrite");
        let report = doctor(&layout);
        assert!(matches!(report.desktop, AssetStatus::Drift));
        assert!(report.needs_fix());
    }
}
