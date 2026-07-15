// SPDX-License-Identifier: GPL-3.0-or-later
//! Built-in fallback icons for GTK symbolic names used by flowmux.

use std::path::{Path, PathBuf};

struct SymbolicIcon {
    name: &'static str,
    path: &'static str,
}

const INDEX_THEME: &str = r#"[Icon Theme]
Name=FlowMux Builtin
Comment=Built-in fallback icons for flowmux
Directories=scalable/actions,scalable/apps

[scalable/actions]
Context=Actions
Type=Scalable
Size=16
MinSize=8
MaxSize=64

[scalable/apps]
Context=Applications
Type=Scalable
Size=16
MinSize=8
MaxSize=512
"#;

const APP_ICON_SVG: &str = include_str!("../../../resources/icons/flowmux.svg");

const SYMBOLIC_ICONS: &[SymbolicIcon] = &[
    SymbolicIcon {
        name: "applications-utilities-symbolic",
        path: "M10.8 1.5a3.4 3.4 0 0 0-3.2 4.6L2.4 11.3a1.8 1.8 0 0 0 2.5 2.5l5.2-5.2a3.4 3.4 0 0 0 4.4-4.3l-2.3 2.3-1.8-1.8 2.3-2.3a3.4 3.4 0 0 0-1.9-1z",
    },
    SymbolicIcon {
        name: "emblem-system-symbolic",
        path: GEAR_PATH,
    },
    SymbolicIcon {
        name: "folder-symbolic",
        path: "M1.5 4A1.5 1.5 0 0 1 3 2.5h3L7.5 4H13a1.5 1.5 0 0 1 1.5 1.5v6A1.5 1.5 0 0 1 13 13H3a1.5 1.5 0 0 1-1.5-1.5zM3 5.5v6h10v-6z",
    },
    SymbolicIcon {
        name: "go-down-symbolic",
        path: "M8.8 2.5v8.1l3-3 1.2 1.2-5 5-5-5 1.2-1.2 3 3V2.5z",
    },
    SymbolicIcon {
        name: "go-next-symbolic",
        path: "M8.8 3 13.8 8l-5 5-1.2-1.2 3-3H2.5V7.2h8.1l-3-3z",
    },
    SymbolicIcon {
        name: "go-previous-symbolic",
        path: "M7.2 3 2.2 8l5 5 1.2-1.2-3-3h8.1V7.2H5.4l3-3z",
    },
    SymbolicIcon {
        name: "input-keyboard-symbolic",
        path: "M1.5 4h13v8h-13zM3 5.5v5h10v-5zM4 6.5h1v1H4zm2 0h1v1H6zm2 0h1v1H8zm2 0h1v1h-1zM4 8.5h1v1H4zm2 0h4v1H6zm5 0h1v1h-1z",
    },
    SymbolicIcon {
        name: "list-add-symbolic",
        path: ADD_PATH,
    },
    SymbolicIcon {
        name: "notifications-symbolic",
        path: "M8 14a2 2 0 0 0 1.8-1.2H6.2A2 2 0 0 0 8 14zM4 11.5h8L11 10V7a3 3 0 0 0-2.2-2.9V3a.8.8 0 0 0-1.6 0v1.1A3 3 0 0 0 5 7v3z",
    },
    SymbolicIcon {
        name: "pan-down-symbolic",
        path: "M3 5.2 4.2 4 8 7.8 11.8 4 13 5.2l-5 5z",
    },
    SymbolicIcon {
        name: "pan-end-symbolic",
        path: "M5.2 3 10.2 8l-5 5L4 11.8 7.8 8 4 4.2z",
    },
    SymbolicIcon {
        name: "preferences-system-symbolic",
        path: GEAR_PATH,
    },
    SymbolicIcon {
        name: "tab-new-symbolic",
        path: "M2 3h8l2 2h2v8H2zM3.5 4.5v7h9v-5H9.4l-1.8-2zM7.2 6.2h1.6V8h1.8v1.6H8.8v1.8H7.2V9.6H5.4V8h1.8z",
    },
    SymbolicIcon {
        name: "text-x-generic-symbolic",
        path: "M4 1.5h5l3 3V14H4zM9 2.8V5h2.2zM5.5 7h5v1h-5zm0 2h5v1h-5zm0 2h3v1h-3z",
    },
    SymbolicIcon {
        name: "user-trash-symbolic",
        path: "M5.5 2 6 1h4l.5 1H13v1.5H3V2zm-1 3h7l-.5 8H5zM6 6v5h1V6zm3 0v5h1V6z",
    },
    SymbolicIcon {
        name: "utilities-terminal-symbolic",
        path: "M2 3h12v10H2zM3.5 4.5v7h9v-7zM4.5 6l1.8 2-1.8 2h1.8L8 8 6.3 6zM8 9.5h3v1H8z",
    },
    SymbolicIcon {
        name: "vcs-branch-symbolic",
        path: "M4 2a2 2 0 1 0 1.5 3.3v4.4A2 2 0 1 0 7 11.6V9.8c0-.8.6-1.4 1.4-1.4h1.1A2 2 0 1 0 9.5 7H8.4A2.9 2.9 0 0 0 7 7.4V5.3A2 2 0 0 0 4 2zm0 1.3a.7.7 0 1 1 0 1.4.7.7 0 0 1 0-1.4zm7 0a.7.7 0 1 1 0 1.4.7.7 0 0 1 0-1.4zm-5 8a.7.7 0 1 1 0 1.4.7.7 0 0 1 0-1.4z",
    },
    SymbolicIcon {
        name: "view-refresh-symbolic",
        path: "M12.7 5.2A5.2 5.2 0 1 0 13 8h-1.5a3.7 3.7 0 1 1-.9-2.4L8.5 5.6V7h5V2h-1.4z",
    },
    SymbolicIcon {
        name: "web-browser-symbolic",
        path: "M8 1.5a6.5 6.5 0 1 0 0 13 6.5 6.5 0 0 0 0-13zM8 3c.7.8 1.1 1.7 1.3 2.5H6.7C6.9 4.7 7.3 3.8 8 3zM3.8 7h2.6a7 7 0 0 0 0 2H3.8a5 5 0 0 1 0-2zm.8 3.5h2.1c.2.9.6 1.8 1.3 2.5a5 5 0 0 1-3.4-2.5zM8 13c-.7-.8-1.1-1.7-1.3-2.5h2.6C9.1 11.3 8.7 12.2 8 13zm1.6-4H6.4a7 7 0 0 1 0-2h3.2a7 7 0 0 1 0 2zM9.3 5.5A8 8 0 0 0 8 3a5 5 0 0 1 3.4 2.5zm0 5h2.1A5 5 0 0 1 8 13c.7-.7 1.1-1.6 1.3-2.5zM9.6 9a7 7 0 0 0 0-2h2.6a5 5 0 0 1 0 2z",
    },
    SymbolicIcon {
        name: "window-close-symbolic",
        path: "M3.2 2.1 8 6.9l4.8-4.8 1.1 1.1L9.1 8l4.8 4.8-1.1 1.1L8 9.1l-4.8 4.8-1.1-1.1L6.9 8 2.1 3.2z",
    },
];

const ADD_PATH: &str = "M7 2h2v5h5v2H9v5H7V9H2V7h5z";
const GEAR_PATH: &str = "M7 1.5h2l.4 1.6c.4.1.8.3 1.1.5l1.4-.8 1.3 1.7-1.1 1.2c.1.4.2.8.2 1.2s-.1.8-.2 1.2l1.1 1.2-1.3 1.7-1.4-.8c-.3.2-.7.4-1.1.5L9 14.5H7l-.4-1.6c-.4-.1-.8-.3-1.1-.5l-1.4.8-1.3-1.7 1.1-1.2c-.1-.4-.2-.8-.2-1.2s.1-.8.2-1.2L2.8 6.7 4.1 5l1.4.8c.3-.2.7-.4 1.1-.5zM8 6a2 2 0 1 0 0 4 2 2 0 0 0 0-4z";

pub fn install() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let icon_theme = gtk::IconTheme::for_display(&display);
    let missing_builtin = SYMBOLIC_ICONS
        .iter()
        .any(|icon| !icon_theme.has_icon(icon.name))
        || !icon_theme.has_icon(crate::APP_ID);
    if !missing_builtin {
        return;
    }

    for root in candidate_roots() {
        match write_icon_theme(&root) {
            Ok(()) => {
                icon_theme.add_search_path(&root);
                return;
            }
            Err(err) => {
                tracing::warn!(
                    path = %root.display(),
                    error = %err,
                    "could not install fallback icon theme"
                );
            }
        }
    }
}

fn candidate_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(data_dir) = flowmux_config::paths::data_dir() {
        roots.push(data_dir.join("icons"));
    }
    roots.push(std::env::temp_dir().join("flowmux-icons"));
    roots
}

fn write_icon_theme(root: &Path) -> std::io::Result<()> {
    let hicolor = root.join("hicolor");
    write_if_changed(&hicolor.join("index.theme"), INDEX_THEME.as_bytes())?;

    let actions = hicolor.join("scalable").join("actions");
    for icon in SYMBOLIC_ICONS {
        let svg = symbolic_svg(icon.path);
        write_if_changed(&actions.join(format!("{}.svg", icon.name)), svg.as_bytes())?;
    }

    write_if_changed(
        &hicolor
            .join("scalable")
            .join("apps")
            .join(format!("{}.svg", crate::APP_ID)),
        APP_ICON_SVG.as_bytes(),
    )?;

    Ok(())
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Ok(existing) = std::fs::read(path) {
        if existing == bytes {
            return Ok(());
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)
}

fn symbolic_svg(path: &str) -> String {
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16"><path fill="#2e3436" d="{path}"/></svg>"##
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn symbolic_icon_names_are_unique_and_named_symbolic() {
        let mut names = BTreeSet::new();
        for icon in SYMBOLIC_ICONS {
            assert!(icon.name.ends_with("-symbolic"));
            assert!(
                names.insert(icon.name),
                "duplicate icon name: {}",
                icon.name
            );
        }
    }

    #[test]
    fn write_icon_theme_creates_index_actions_and_app_icon() {
        let tmp = tempfile::tempdir().unwrap();
        write_icon_theme(tmp.path()).unwrap();

        assert!(tmp.path().join("hicolor/index.theme").exists());
        assert!(tmp
            .path()
            .join("hicolor/scalable/actions/window-close-symbolic.svg")
            .exists());
        assert!(tmp
            .path()
            .join("hicolor/scalable/actions/vcs-branch-symbolic.svg")
            .exists());
        assert!(tmp
            .path()
            .join(format!("hicolor/scalable/apps/{}.svg", crate::APP_ID))
            .exists());
    }
}
