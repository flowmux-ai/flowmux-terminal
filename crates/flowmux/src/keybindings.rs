// SPDX-License-Identifier: GPL-3.0-or-later
//! Default keyboard shortcuts.
//!
//! Two layers:
//!
//! * **Pane-tree bindings** — split / focus-direction / close-surface,
//!   chosen to match the keys the project owner already uses
//!   (Ctrl+Shift+PageUp/Down for splits, Alt+arrows for directional
//!   focus, Alt+W to close). These are baked in as flowmux's own
//!   defaults rather than read from any external terminal's config.
//! * **Common terminal conventions** — Ctrl+Shift+C/V for copy/paste,
//!   Ctrl+Shift+T for a new surface. Universal across modern terminal
//!   emulators (GNOME Terminal, Tilix, kitty, foot, etc.) so they
//!   feel native to anyone coming from those.
//!
//! Accelerator format follows GTK's `gtk_accelerator_parse` syntax.

use crate::bridge::{Bridge, FocusDir, GtkCommand, WsNav};
use flowmux_core::SplitDirection;
use gtk::gio::prelude::*;
use gtk::glib;
use gtk::prelude::*;
use std::cell::Cell;
use std::rc::Rc;
use tokio::sync::oneshot;
use vte::prelude::*;

/// Tracks which pane currently has keyboard focus so split / close /
/// focus-direction shortcuts know where to operate.
pub type FocusedPane = Rc<Cell<Option<flowmux_core::PaneId>>>;

/// One action can have multiple accelerators (e.g. Ctrl+Shift+Tab and
/// Ctrl+ISO_Left_Tab both move to the previous workspace).
pub const BINDINGS: &[(&str, &[&str])] = &[
    // Pane tree
    ("win.split-right", &["<Ctrl><Shift>Page_Up"]),
    ("win.split-down", &["<Ctrl><Shift>Page_Down"]),
    ("win.focus-left", &["<Alt>Left"]),
    ("win.focus-right", &["<Alt>Right"]),
    ("win.focus-up", &["<Alt>Up"]),
    ("win.focus-down", &["<Alt>Down"]),
    ("win.close-surface", &["<Alt>w"]),
    // Tab navigation. The bare Tab key is reserved for the terminal
    // (shell completion etc.). Shift+Tab cycles pane-local terminal /
    // browser tabs; Ctrl+Tab cycles the left workspace list.
    ("win.next-surface", &["<Shift>Tab", "<Shift>ISO_Left_Tab"]),
    ("win.prev-surface", &[]),
    ("win.next-workspace", &["<Ctrl>Tab"]),
    (
        "win.prev-workspace",
        &[
            "<Ctrl><Shift>Tab",
            "<Ctrl><Shift>ISO_Left_Tab",
            "<Ctrl>ISO_Left_Tab",
        ],
    ),
    ("win.workspace-1", &["<Alt>1"]),
    ("win.workspace-2", &["<Alt>2"]),
    ("win.workspace-3", &["<Alt>3"]),
    ("win.workspace-4", &["<Alt>4"]),
    ("win.workspace-5", &["<Alt>5"]),
    ("win.workspace-6", &["<Alt>6"]),
    ("win.workspace-7", &["<Alt>7"]),
    ("win.workspace-8", &["<Alt>8"]),
    // Common terminal conventions
    ("win.copy", &["<Ctrl><Shift>c"]),
    ("win.paste", &["<Ctrl><Shift>v"]),
    ("win.new-surface", &["<Ctrl><Shift>t"]),
    // Ctrl+Shift+B adds a new browser tab to the same pane. It mirrors the
    // browser-tab add button on the right side of the tab bar and pairs with
    // Ctrl+Shift+T for terminal tabs.
    ("win.new-browser-surface", &["<Ctrl><Shift>b"]),
    ("win.new-workspace", &["<Ctrl><Shift>n"]),
];

/// Install accelerators on the application.
pub fn install_accels(app: &adw::Application) {
    for (action, accels) in BINDINGS {
        app.set_accels_for_action(action, accels);
    }
}

/// Pane registry handle so copy/paste can call into the focused VTE
/// terminal on the GTK main thread.
pub type TerminalRegistry = Rc<std::cell::RefCell<crate::ui::workspace_view::PaneRegistry>>;

/// Register the action handlers on a window.
pub fn install_actions(
    window: &adw::ApplicationWindow,
    bridge: Bridge,
    focused: FocusedPane,
    registry: TerminalRegistry,
) {
    let split_right = make_pane_action(
        "split-right",
        focused.clone(),
        move_split(bridge.clone(), SplitDirection::Vertical),
    );
    let split_down = make_pane_action(
        "split-down",
        focused.clone(),
        move_split(bridge.clone(), SplitDirection::Horizontal),
    );
    let focus_left = make_focus_direction_action(
        "focus-left",
        focused.clone(),
        bridge.clone(),
        FocusDir::Left,
    );
    let focus_right = make_focus_direction_action(
        "focus-right",
        focused.clone(),
        bridge.clone(),
        FocusDir::Right,
    );
    let focus_up =
        make_focus_direction_action("focus-up", focused.clone(), bridge.clone(), FocusDir::Up);
    let focus_down = make_focus_direction_action(
        "focus-down",
        focused.clone(),
        bridge.clone(),
        FocusDir::Down,
    );
    let close_surface = make_close_surface_action(
        "close-surface",
        focused.clone(),
        bridge.clone(),
        registry.clone(),
    );

    let new_workspace = {
        let bridge = bridge.clone();
        gtk::gio::ActionEntry::builder("new-workspace")
            .activate(move |_, _, _| {
                tracing::debug!(action = "new-workspace", "key action fired");
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let root =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
                    let _ = bridge.tx.send(GtkCommand::NewWorkspace { root }).await;
                });
            })
            .build()
    };
    let new_surface = make_pane_action(
        "new-surface",
        focused.clone(),
        Box::new({
            let bridge = bridge.clone();
            move |pane| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge.tx.send(GtkCommand::NewSurface { pane }).await;
                });
            }
        }),
    );
    let new_browser_surface = make_pane_action(
        "new-browser-surface",
        focused.clone(),
        Box::new({
            let bridge = bridge.clone();
            move |pane| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge.tx.send(GtkCommand::NewBrowserSurface { pane }).await;
                });
            }
        }),
    );
    let next_surface = make_surface_nav_action(
        "next-surface",
        focused.clone(),
        bridge.clone(),
        registry.clone(),
        SurfaceNav::Next,
    );
    let prev_surface = make_surface_nav_action(
        "prev-surface",
        focused.clone(),
        bridge.clone(),
        registry.clone(),
        SurfaceNav::Prev,
    );

    let next_workspace = make_ws_nav_action("next-workspace", WsNav::Next, bridge.clone());
    let prev_workspace = make_ws_nav_action("prev-workspace", WsNav::Prev, bridge.clone());
    // Eight individual one-shot actions for Alt+1..Alt+8 — simpler
    // than a parametrised "jump-to-workspace(uint8)" because GTK
    // accelerators can target any of these by name with no extra
    // detailed-action plumbing.
    let ws_jumps = [1u8, 2, 3, 4, 5, 6, 7, 8].map(|i| {
        let bridge = bridge.clone();
        let name = match i {
            1 => "workspace-1",
            2 => "workspace-2",
            3 => "workspace-3",
            4 => "workspace-4",
            5 => "workspace-5",
            6 => "workspace-6",
            7 => "workspace-7",
            8 => "workspace-8",
            _ => unreachable!(),
        };
        gtk::gio::ActionEntry::builder(name)
            .activate(move |_, _, _| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge
                        .tx
                        .send(GtkCommand::FocusWorkspaceAt { idx: i })
                        .await;
                });
            })
            .build()
    });

    let copy = make_clipboard_action("copy", focused.clone(), registry.clone(), ClipboardOp::Copy);
    let paste = make_clipboard_action("paste", focused.clone(), registry, ClipboardOp::Paste);

    let [w1, w2, w3, w4, w5, w6, w7, w8] = ws_jumps;
    window.add_action_entries([
        split_right,
        split_down,
        focus_left,
        focus_right,
        focus_up,
        focus_down,
        close_surface,
        new_surface,
        new_browser_surface,
        next_surface,
        prev_surface,
        new_workspace,
        next_workspace,
        prev_workspace,
        w1,
        w2,
        w3,
        w4,
        w5,
        w6,
        w7,
        w8,
        copy,
        paste,
    ]);
}

#[derive(Clone, Copy)]
enum SurfaceNav {
    Next,
    Prev,
}

fn make_surface_nav_action(
    name: &'static str,
    focused: FocusedPane,
    bridge: Bridge,
    registry: TerminalRegistry,
    dir: SurfaceNav,
) -> gtk::gio::ActionEntry<adw::ApplicationWindow> {
    gtk::gio::ActionEntry::builder(name)
        .activate(move |_, _, _| {
            let pane = match focused.get() {
                Some(pane) => pane,
                None => {
                    tracing::info!(action = name, "no pane focused — ignoring");
                    return;
                }
            };
            let surface = {
                let registry = registry.borrow();
                match dir {
                    SurfaceNav::Next => registry.next_surface(pane),
                    SurfaceNav::Prev => registry.previous_surface(pane),
                }
            };
            let Some(surface) = surface else {
                tracing::info!(action = name, %pane, "no pane-local surface — ignoring");
                return;
            };
            let bridge = bridge.clone();
            glib::MainContext::default().spawn_local(async move {
                let _ = bridge
                    .tx
                    .send(GtkCommand::ActivateSurface { pane, surface })
                    .await;
            });
        })
        .build()
}

fn make_ws_nav_action(
    name: &'static str,
    dir: WsNav,
    bridge: Bridge,
) -> gtk::gio::ActionEntry<adw::ApplicationWindow> {
    gtk::gio::ActionEntry::builder(name)
        .activate(move |_, _, _| {
            let bridge = bridge.clone();
            glib::MainContext::default().spawn_local(async move {
                let _ = bridge.tx.send(GtkCommand::FocusWorkspaceDir { dir }).await;
            });
        })
        .build()
}

type PaneAction = Box<dyn Fn(flowmux_core::PaneId)>;

fn make_pane_action(
    name: &'static str,
    focused: FocusedPane,
    action: PaneAction,
) -> gtk::gio::ActionEntry<adw::ApplicationWindow> {
    gtk::gio::ActionEntry::builder(name)
        .activate(move |_, _, _| {
            let pane = match focused.get() {
                Some(p) => p,
                None => {
                    tracing::info!(action = name, "no pane focused — ignoring");
                    return;
                }
            };
            tracing::debug!(action = name, %pane, "key action fired");
            action(pane);
        })
        .build()
}

fn move_split(bridge: Bridge, direction: SplitDirection) -> PaneAction {
    Box::new(move |pane| {
        let bridge = bridge.clone();
        glib::MainContext::default().spawn_local(async move {
            let (tx, rx) = oneshot::channel();
            let _ = bridge
                .tx
                .send(GtkCommand::SplitFocused {
                    pane,
                    direction,
                    ack: tx,
                })
                .await;
            let _ = rx.await;
        });
    })
}

/// Action builder for focus-{left,right,up,down}.
///
/// Unlike `make_pane_action`, this does not ignore the action when no pane
/// is currently focused. It dispatches `from = None` so the dispatcher can
/// focus the active workspace's first leaf pane after a side-panel-only
/// workspace click.
fn make_focus_direction_action(
    name: &'static str,
    focused: FocusedPane,
    bridge: Bridge,
    dir: FocusDir,
) -> gtk::gio::ActionEntry<adw::ApplicationWindow> {
    gtk::gio::ActionEntry::builder(name)
        .activate(move |_, _, _| {
            let from = focused.get();
            tracing::debug!(action = name, ?from, ?dir, "focus direction action fired");
            let bridge = bridge.clone();
            glib::MainContext::default().spawn_local(async move {
                let _ = bridge
                    .tx
                    .send(GtkCommand::FocusDirection { from, dir })
                    .await;
            });
        })
        .build()
}

#[derive(Clone, Copy)]
enum ClipboardOp {
    Copy,
    Paste,
}

fn make_close_surface_action(
    name: &'static str,
    focused: FocusedPane,
    bridge: Bridge,
    registry: TerminalRegistry,
) -> gtk::gio::ActionEntry<adw::ApplicationWindow> {
    gtk::gio::ActionEntry::builder(name)
        .activate(move |_, _, _| {
            let pane = match focused.get() {
                Some(p) => p,
                None => {
                    tracing::info!(action = name, "no pane focused — ignoring");
                    return;
                }
            };
            let surface = match registry.borrow().active_surface(pane) {
                Some(surface) => surface,
                None => {
                    tracing::info!(action = name, %pane, "no active surface — ignoring");
                    return;
                }
            };
            let bridge = bridge.clone();
            glib::MainContext::default().spawn_local(async move {
                let (tx, rx) = oneshot::channel();
                let _ = bridge
                    .tx
                    .send(GtkCommand::CloseSurface {
                        pane,
                        surface,
                        ack: tx,
                    })
                    .await;
                let _ = rx.await;
            });
        })
        .build()
}

fn make_clipboard_action(
    name: &'static str,
    focused: FocusedPane,
    registry: TerminalRegistry,
    op: ClipboardOp,
) -> gtk::gio::ActionEntry<adw::ApplicationWindow> {
    gtk::gio::ActionEntry::builder(name)
        .activate(move |_, _, _| {
            let pane = match focused.get() {
                Some(p) => p,
                None => return,
            };
            let r = registry.borrow();
            let Some(term) = r.active_terminal(pane) else {
                return;
            };
            match op {
                ClipboardOp::Copy => {
                    term.widget.copy_clipboard_format(vte::Format::Text);
                }
                ClipboardOp::Paste => {
                    term.widget.paste_clipboard();
                }
            }
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accels(action: &str) -> &'static [&'static str] {
        BINDINGS
            .iter()
            .find_map(|(name, accels)| (*name == action).then_some(*accels))
            .expect("binding should exist")
    }

    #[test]
    fn shift_tab_cycles_pane_local_surface_not_workspace() {
        assert_eq!(
            accels("win.next-surface"),
            &["<Shift>Tab", "<Shift>ISO_Left_Tab"]
        );
        assert!(!accels("win.next-workspace")
            .iter()
            .any(|accel| accel.contains("<Shift>Tab") || accel.contains("<Shift>ISO_Left_Tab")));
    }

    #[test]
    fn ctrl_tab_cycles_workspace_list() {
        assert_eq!(accels("win.next-workspace"), &["<Ctrl>Tab"]);
        assert!(accels("win.prev-workspace")
            .iter()
            .any(|accel| accel.contains("<Ctrl><Shift>Tab")));
    }

    #[test]
    fn ctrl_shift_b_opens_new_browser_surface_distinct_from_terminal_tab() {
        assert_eq!(accels("win.new-surface"), &["<Ctrl><Shift>t"]);
        assert_eq!(accels("win.new-browser-surface"), &["<Ctrl><Shift>b"]);
    }
}
