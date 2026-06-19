// SPDX-License-Identifier: GPL-3.0-or-later
//! Keyboard shortcut installation.
//!
//! Defaults live in [`flowmux_config::keybindings`] and follow two
//! conventions:
//!
//! * **Pane-tree bindings** — split / focus-direction / close-surface,
//!   chosen to match the keys the project owner already uses
//!   (Ctrl+Shift+PageUp/Down for splits, Alt+arrows for directional
//!   focus, Alt+W to close).
//! * **Common terminal conventions** — Ctrl+Shift+C/V for copy/paste,
//!   Ctrl+Shift+T for a new surface. Universal across modern terminal
//!   emulators (GNOME Terminal, Tilix, kitty, foot, etc.).
//!
//! Users override either layer through the `keybindings` field of
//! `options.json`. Resolution is a partial overlay: actions absent
//! from the user map keep their defaults, an empty accel array marks
//! an action as explicitly unbound. Accelerator strings follow GTK's
//! `gtk_accelerator_parse` syntax; the install routine validates each
//! one and skips invalid entries with a warning.

use crate::bridge::{Bridge, FocusDir, GtkCommand, WsNav};
use crate::ui::terminal_pane::{TerminalPane, ALT_ENTER_BYTES};
use crate::ui::window::ClipboardToast;
use adw::prelude::*;
use flowmux_config::keybindings::{ActionId, KeybindingOverrides};
use flowmux_config::options::Options;
use flowmux_core::SplitDirection;
use gtk::glib;
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use tokio::sync::oneshot;

/// Tracks which pane currently has keyboard focus so split / close /
/// focus-direction shortcuts know where to operate.
pub type FocusedPane = Rc<Cell<Option<flowmux_core::PaneId>>>;

/// Group prefix for every flowmux window action. `set_accels_for_action`
/// and `add_action_entries` both need the namespaced name (`win.copy`)
/// but `flowmux_config::keybindings::ActionId::as_str` returns the bare
/// form (`copy`), so callers prepend this constant.
const ACTION_GROUP: &str = "win.";
const INSERT_NEWLINE_ACTION: &str = "insert-newline";
const INSERT_NEWLINE_FULL_ACTION: &str = "win.insert-newline";
const INSERT_NEWLINE_ACCELS: &[&str] = &["<Shift>Return", "<Shift>KP_Enter", "<Shift>ISO_Enter"];

/// Build the namespaced action name (`win.<bare>`) GTK uses for accel
/// registration.
fn full_action_name(action: ActionId) -> String {
    format!("{ACTION_GROUP}{}", action.as_str())
}

/// Install accelerators on the application, layering the user overrides
/// in `options.keybindings` on top of the built-in defaults.
///
/// Each accel string is validated through `gtk::accelerator_parse`.
/// Invalid entries are logged and skipped — the rest of the action's
/// accels still install. Duplicate accels across actions are logged
/// (GTK keeps the last writer); the function does not refuse to install
/// either side so the user can recover by editing `options.json` again.
pub fn install_accels(app: &adw::Application, options: &Options) {
    let overrides = &options.keybindings;
    for unknown in overrides.unknown_keys() {
        tracing::warn!(action = %unknown, "unknown keybinding action key — ignoring");
    }
    for non_editable in overrides.non_editable_keys() {
        tracing::warn!(
            action = %non_editable,
            "keybinding action is not user-editable — ignoring override"
        );
    }
    let resolved = overrides.resolve();

    // accel -> first action that claimed it, for conflict warnings.
    let mut owner: HashMap<String, &'static str> = HashMap::new();

    for (action, accels) in &resolved {
        let valid: Vec<String> = accels
            .iter()
            .filter_map(|accel| {
                if accel.is_empty() {
                    tracing::warn!(action = action.as_str(), "empty accel string — skipping");
                    return None;
                }
                if gtk::accelerator_parse(accel).is_none() {
                    tracing::warn!(
                        action = action.as_str(),
                        accel = %accel,
                        "invalid accel — skipping"
                    );
                    return None;
                }
                if let Some(prev) = owner.get(accel) {
                    tracing::warn!(
                        accel = %accel,
                        first = %prev,
                        second = action.as_str(),
                        "accel bound to multiple actions — GTK will keep the last writer"
                    );
                } else {
                    owner.insert(accel.clone(), action.as_str());
                }
                Some(accel.clone())
            })
            .collect();

        let accel_refs: Vec<&str> = valid.iter().map(|s| s.as_str()).collect();
        app.set_accels_for_action(&full_action_name(*action), &accel_refs);
    }
    app.set_accels_for_action(INSERT_NEWLINE_FULL_ACTION, INSERT_NEWLINE_ACCELS);
}

/// Helper retained for tests and other callers that just want the
/// resolved accels for a single action without the GTK install path.
pub fn resolved_accels(overrides: &KeybindingOverrides, action: ActionId) -> Vec<String> {
    overrides
        .resolve()
        .into_iter()
        .find(|(a, _)| *a == action)
        .map(|(_, accels)| accels)
        .unwrap_or_default()
}

/// Pane registry handle so copy/paste can call into the focused
/// terminal on the GTK main thread.
pub type TerminalRegistry = Rc<std::cell::RefCell<crate::ui::workspace_view::PaneRegistry>>;

/// Register the action handlers on a window.
pub fn install_actions(
    window: &adw::ApplicationWindow,
    bridge: Bridge,
    focused: FocusedPane,
    registry: TerminalRegistry,
    clipboard_toast: ClipboardToast,
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
    let quit_app = make_quit_app_action(window.clone());

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
    let new_window = make_new_window_action();
    let command_palette = {
        let bridge = bridge.clone();
        gtk::gio::ActionEntry::builder("command-palette")
            .activate(move |_, _, _| {
                tracing::debug!(action = "command-palette", "key action fired");
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge.tx.send(GtkCommand::ShowCommandPalette).await;
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

    let copy = make_copy_action(focused.clone(), registry.clone(), clipboard_toast.clone());
    let paste = make_paste_action(focused.clone(), registry.clone());
    let insert_newline =
        make_insert_newline_action(window.clone(), focused.clone(), registry.clone());

    // Single-chord copy: pressing the configured accel (default
    // `Ctrl+Shift+K`) writes the focused pane's cwd to the clipboard
    // and surfaces a toast. No follower key; GTK action map handles
    // case/IM/layout normalisation for the Ctrl-modified leader itself.
    let copy_pane_path = {
        let window = window.clone();
        let focused = focused.clone();
        let registry = registry.clone();
        let toast = clipboard_toast.clone();
        gtk::gio::ActionEntry::builder("copy-pane-path")
            .activate(move |_, _, _| {
                copy_focused_pane_path(&window, &focused, &registry, &toast);
            })
            .build()
    };

    let [w1, w2, w3, w4, w5, w6, w7, w8] = ws_jumps;
    window.add_action_entries([
        split_right,
        split_down,
        focus_left,
        focus_right,
        focus_up,
        focus_down,
        close_surface,
        quit_app,
        new_surface,
        new_browser_surface,
        next_surface,
        prev_surface,
        new_workspace,
        new_window,
        command_palette,
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
        insert_newline,
        copy_pane_path,
    ]);
}

/// Resolve the focused pane's terminal cwd, write it to the system
/// clipboard, and surface a toast. Surfaces an error toast instead when
/// no terminal cwd is available so the user knows the chord fired but
/// had nothing useful to copy.
fn copy_focused_pane_path(
    window: &adw::ApplicationWindow,
    focused: &FocusedPane,
    registry: &TerminalRegistry,
    clipboard_toast: &ClipboardToast,
) {
    let Some(pane) = focused.get() else {
        tracing::info!("copy-pane-path: no focused pane");
        clipboard_toast.show_with_message("No focused pane");
        return;
    };
    let cwd = {
        let r = registry.borrow();
        r.active_terminal(pane).and_then(|t| t.current_dir())
    };
    let Some(cwd) = cwd else {
        tracing::info!(%pane, "copy-pane-path: no terminal cwd");
        clipboard_toast.show_with_message("No pane path to copy");
        return;
    };
    let path_str = cwd.display().to_string();
    window.clipboard().set_text(&path_str);
    tracing::info!(%pane, path = %path_str, "copy-pane-path: copied");
    clipboard_toast.show_with_message(&format!("Copied path: {path_str}"));
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

/// Action for `win.new-window` (Ctrl+Shift+N). Spawns a fresh `flowmux`
/// process so the user gets a second top-level window. `NON_UNIQUE` on the
/// GApplication is what makes this give a real window rather than an
/// activation hand-off; see `build_application` in `main.rs` for the why.
///
/// We re-exec `current_exe()` (not just `"flowmux"`) so a build run out of
/// `target/release/` opens another copy of the same binary rather than
/// whatever happens to be on `PATH`.
fn make_new_window_action() -> gtk::gio::ActionEntry<adw::ApplicationWindow> {
    gtk::gio::ActionEntry::builder("new-window")
        .activate(move |_, _, _| {
            tracing::debug!(action = "new-window", "key action fired");
            let exe = match std::env::current_exe() {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "new-window: could not resolve current_exe()");
                    return;
                }
            };
            match std::process::Command::new(&exe).spawn() {
                Ok(child) => {
                    tracing::info!(pid = child.id(), exe = %exe.display(), "spawned new flowmux window");
                }
                Err(e) => {
                    tracing::warn!(error = %e, exe = %exe.display(), "new-window: failed to spawn");
                }
            }
        })
        .build()
}

/// Action for `win.quit-app` (Ctrl+Shift+W). Asks the user to confirm
/// before tearing the entire flowmux window down. We close the window
/// rather than calling `application.quit()` so the existing
/// `connect_close_request` handler (state flush, save, deferred destroy)
/// runs exactly the same way as a window-manager close. With NON_UNIQUE
/// GApplication, closing the only window ends the process — i.e.
/// "quit the whole app" — without a separate quit path.
fn make_quit_app_action(
    window: adw::ApplicationWindow,
) -> gtk::gio::ActionEntry<adw::ApplicationWindow> {
    gtk::gio::ActionEntry::builder("quit-app")
        .activate(move |_, _, _| {
            tracing::debug!(action = "quit-app", "key action fired");
            let window = window.clone();
            glib::MainContext::default().spawn_local(async move {
                if confirm_quit_app(&window).await {
                    window.close();
                }
            });
        })
        .build()
}

/// Modal "Quit flowmux?" confirmation dialog. Returns true when the
/// user picks "Quit", false when they cancel or dismiss. Mirrors the
/// shape of `confirm_close_workspace` in `ui::window` (same response
/// names, default = cancel, destructive styling on the confirm button)
/// so the two dialogs behave identically from the user's side.
async fn confirm_quit_app(window: &adw::ApplicationWindow) -> bool {
    let (dialog, rx) = build_quit_dialog();
    dialog.present(Some(window));
    rx.await.unwrap_or(false)
}

/// Construct the quit-confirmation dialog plus a oneshot receiver that
/// resolves to `true` for "quit" / `false` for cancel-or-dismiss.
/// Split out from [`confirm_quit_app`] so tests can present the dialog,
/// fire a response programmatically (`dialog.response("quit")`), and
/// observe the receiver value without driving an actual GUI session.
fn build_quit_dialog() -> (adw::AlertDialog, oneshot::Receiver<bool>) {
    let dialog = adw::AlertDialog::new(
        Some("Quit flowmux?"),
        Some("This closes the flowmux window and stops every running tab."),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("quit", "Quit");
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("quit", adw::ResponseAppearance::Destructive);

    let (tx, rx) = oneshot::channel::<bool>();
    let tx_cell: Rc<Cell<Option<oneshot::Sender<bool>>>> = Rc::new(Cell::new(Some(tx)));
    let tx_for_resp = tx_cell.clone();
    dialog.connect_response(None, move |dialog, response| {
        if let Some(tx) = tx_for_resp.take() {
            let _ = tx.send(response == "quit");
        }
        dialog.close();
    });
    (dialog, rx)
}

fn make_copy_action(
    focused: FocusedPane,
    registry: TerminalRegistry,
    clipboard_toast: ClipboardToast,
) -> gtk::gio::ActionEntry<adw::ApplicationWindow> {
    gtk::gio::ActionEntry::builder("copy")
        .activate(move |_, _, _| {
            let Some(pane) = focused.get() else { return };
            let r = registry.borrow();
            let Some(term) = r.active_terminal(pane) else {
                return;
            };
            // No selection means there is nothing to copy, and we want
            // to leave whatever is already on the clipboard untouched
            // (e.g. text the user copied from another app).
            if !term.has_selection() {
                return;
            }
            term.copy_selection_to_clipboard();
            clipboard_toast.show();
        })
        .build()
}

fn make_insert_newline_action(
    window: adw::ApplicationWindow,
    focused: FocusedPane,
    registry: TerminalRegistry,
) -> gtk::gio::ActionEntry<adw::ApplicationWindow> {
    gtk::gio::ActionEntry::builder(INSERT_NEWLINE_ACTION)
        .activate(move |_, _, _| {
            let Some(pane) = focused.get() else { return };
            let r = registry.borrow();
            let Some(term) = r.active_terminal(pane) else {
                return;
            };
            if !window_focus_is_terminal(&window, term) {
                return;
            }
            term.feed_after_preedit_commit(ALT_ENTER_BYTES);
        })
        .build()
}

fn window_focus_is_terminal(window: &adw::ApplicationWindow, term: &TerminalPane) -> bool {
    let Some(focus) = gtk::prelude::GtkWindowExt::focus(window) else {
        return false;
    };
    let terminal_widget = term.widget.clone().upcast::<gtk::Widget>();
    focus == terminal_widget || focus.is_ancestor(&terminal_widget)
}

fn make_paste_action(
    focused: FocusedPane,
    registry: TerminalRegistry,
) -> gtk::gio::ActionEntry<adw::ApplicationWindow> {
    gtk::gio::ActionEntry::builder("paste")
        .activate(move |_, _, _| {
            let Some(pane) = focused.get() else { return };
            let r = registry.borrow();
            let Some(term) = r.active_terminal(pane) else {
                return;
            };
            term.paste_clipboard();
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flowmux_config::keybindings::default_accels;

    fn default_for(action: ActionId) -> Vec<&'static str> {
        default_accels(action).to_vec()
    }

    #[test]
    fn shift_tab_is_reserved_for_terminal_and_agents() {
        // No default cycle binding may claim Shift+Tab, so it always reaches
        // the focused terminal / agent. NextSurface keeps its own binding
        // (<Ctrl><Shift>Right) — the requirement is only that it is not Shift+Tab.
        let steals_shift_tab = |action| {
            default_for(action)
                .iter()
                .any(|accel| *accel == "<Shift>Tab" || *accel == "<Shift>ISO_Left_Tab")
        };
        assert!(!steals_shift_tab(ActionId::NextSurface));
        assert!(!steals_shift_tab(ActionId::NextWorkspace));
    }

    #[test]
    fn ctrl_tab_cycles_workspace_list() {
        assert_eq!(default_for(ActionId::NextWorkspace), vec!["<Ctrl>Tab"]);
        assert!(default_for(ActionId::PrevWorkspace)
            .iter()
            .any(|accel| accel.contains("<Ctrl><Shift>Tab")));
    }

    #[test]
    fn ctrl_shift_b_opens_new_browser_surface_distinct_from_terminal_tab() {
        assert_eq!(default_for(ActionId::NewSurface), vec!["<Ctrl><Shift>t"]);
        assert_eq!(
            default_for(ActionId::NewBrowserSurface),
            vec!["<Ctrl><Shift>b"]
        );
    }

    #[gtk::test]
    fn insert_newline_accels_cover_enter_variants() {
        adw::init().expect("libadwaita should initialize in GTK test");
        assert_eq!(
            INSERT_NEWLINE_ACCELS,
            &["<Shift>Return", "<Shift>KP_Enter", "<Shift>ISO_Enter"]
        );
        for accel in INSERT_NEWLINE_ACCELS {
            assert!(
                gtk::accelerator_parse(*accel).is_some(),
                "{accel} must be valid GTK accelerator syntax"
            );
        }
    }

    #[test]
    fn user_override_replaces_default_via_resolved_accels() {
        let mut overrides = KeybindingOverrides::new();
        overrides.set(ActionId::SplitRight, vec!["<Ctrl><Alt>r".into()]);
        assert_eq!(
            resolved_accels(&overrides, ActionId::SplitRight),
            vec!["<Ctrl><Alt>r".to_string()]
        );
        // Untouched action keeps its default.
        assert_eq!(
            resolved_accels(&overrides, ActionId::SplitDown),
            vec!["<Ctrl><Shift>Page_Down".to_string()]
        );
    }

    #[test]
    fn empty_override_unbinds_action_in_resolved_accels() {
        let mut overrides = KeybindingOverrides::new();
        overrides.set(ActionId::SplitRight, vec![]);
        assert!(resolved_accels(&overrides, ActionId::SplitRight).is_empty());
    }

    /// Ctrl+N must create a workspace inside the current window and
    /// Ctrl+Shift+N must launch a brand-new flowmux window. These were
    /// swapped intentionally — the unshifted form is the cheap/local
    /// action, the shifted form is the heavier "open another app window"
    /// action. Pinning both so a future shortcut shuffle doesn't quietly
    /// flip them back.
    #[test]
    fn ctrl_n_opens_new_workspace_and_ctrl_shift_n_opens_new_window() {
        assert_eq!(default_for(ActionId::NewWorkspace), vec!["<Ctrl>n"]);
        assert_eq!(default_for(ActionId::NewWindow), vec!["<Ctrl><Shift>n"]);
        // The two actions must not share an accelerator — sharing would
        // collapse "new workspace in this window" and "new window" onto
        // the same key.
        let ws: std::collections::HashSet<_> = default_for(ActionId::NewWorkspace)
            .iter()
            .copied()
            .collect();
        for accel in default_for(ActionId::NewWindow) {
            assert!(
                !ws.contains(accel),
                "new-window must not share a default accel with new-workspace"
            );
        }
    }

    /// The new-window action must be registered on the ApplicationWindow
    /// under the `win` action group so the Ctrl+Shift+N accelerator
    /// actually routes somewhere. Catches a regression where the entry
    /// is built but never reaches `add_action_entries`.
    #[gtk::test]
    async fn new_window_action_is_registered_on_application_window_under_win_namespace() {
        use gtk::gio::prelude::ActionGroupExt;

        adw::init().expect("libadwaita should initialize in GTK test");
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.NewWindowActionRegistered")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let window = adw::ApplicationWindow::builder()
            .application(&app)
            .default_width(320)
            .default_height(240)
            .build();
        window.add_action_entries([make_new_window_action()]);

        assert!(
            window.has_action("new-window"),
            "new-window action must be registered on the window so Ctrl+Shift+N routes to it"
        );
    }

    /// Ctrl+Shift+W must trigger the whole-window quit confirmation,
    /// distinct from Alt+W (single-pane close). Pinning both bindings
    /// here so a future shortcut shuffle doesn't silently collapse them
    /// onto the same key.
    #[test]
    fn ctrl_shift_w_quits_app_distinct_from_alt_w_close_surface() {
        assert_eq!(default_for(ActionId::QuitApp), vec!["<Ctrl><Shift>w"]);
        assert_eq!(default_for(ActionId::CloseSurface), vec!["<Alt>w"]);
        // The two bindings must not share an accelerator — sharing
        // would mean one key both closes a tab and asks to quit the
        // whole app, which is exactly the regression the user hit on
        // Alt+W and the reason quit-app got its own modifier combo.
        let close_set: std::collections::HashSet<_> = default_for(ActionId::CloseSurface)
            .iter()
            .copied()
            .collect();
        for accel in default_for(ActionId::QuitApp) {
            assert!(
                !close_set.contains(accel),
                "quit-app must not share a default accel with close-surface"
            );
        }
    }

    /// Quit dialog has Cancel/Quit responses, defaults to Cancel, treats
    /// dismissal as Cancel, and styles Quit as destructive — same shape
    /// as `confirm_close_workspace` so the two confirmation dialogs feel
    /// identical from the user's side.
    #[gtk::test]
    async fn quit_dialog_has_cancel_default_and_destructive_quit_response() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let (dialog, _rx) = build_quit_dialog();
        assert_eq!(dialog.default_response().as_deref(), Some("cancel"));
        assert_eq!(dialog.close_response(), "cancel");
        assert_eq!(
            dialog.response_appearance("quit"),
            adw::ResponseAppearance::Destructive
        );
        // Both responses are enabled and exposed by id so the action
        // handler can compare against the literal "quit"/"cancel" keys.
        assert!(dialog.is_response_enabled("cancel"));
        assert!(dialog.is_response_enabled("quit"));
    }

    /// Scenario: user presses Ctrl+Shift+W, the dialog appears,
    /// and picking "Quit" closes the window (NON_UNIQUE GApplication
    /// → process exit). Picking "Cancel" leaves the window alone.
    /// We exercise the dialog directly via `build_quit_dialog` +
    /// `dialog.response(...)` so the test does not depend on
    /// keyboard-accel routing or widget-tree introspection.
    #[gtk::test]
    async fn quit_dialog_quit_response_resolves_true_cancel_resolves_false() {
        adw::init().expect("libadwaita should initialize in GTK test");

        let (dialog, rx) = build_quit_dialog();
        dialog.emit_by_name::<()>("response", &[&"quit"]);
        let result = rx.await.expect("dialog must signal a response");
        assert!(
            result,
            "picking Quit must resolve to true so the action proceeds with window.close()"
        );

        let (dialog2, rx2) = build_quit_dialog();
        dialog2.emit_by_name::<()>("response", &[&"cancel"]);
        let result2 = rx2.await.expect("dialog must signal a response");
        assert!(
            !result2,
            "picking Cancel must resolve to false so the action leaves the window alone"
        );
    }

    /// Scenario: install the quit-app action on a real ApplicationWindow,
    /// register it, and verify that the GAction name is actually exposed
    /// on the window — i.e. a real `Ctrl+Shift+W` press routed through
    /// `gtk::Application::activate_action` will find a handler. Catches
    /// regressions where the entry is added but mis-spelled or never
    /// reaches `add_action_entries`.
    #[gtk::test]
    async fn quit_app_action_is_registered_on_application_window_under_win_namespace() {
        use gtk::gio::prelude::ActionGroupExt;

        adw::init().expect("libadwaita should initialize in GTK test");
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.QuitAppActionRegistered")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let window = adw::ApplicationWindow::builder()
            .application(&app)
            .default_width(320)
            .default_height(240)
            .build();
        let entry = make_quit_app_action(window.clone());
        window.add_action_entries([entry]);

        // The accelerator binds to "win.quit-app", which means the
        // action lives in the ApplicationWindow's "win" action group
        // under the bare name "quit-app".
        assert!(
            window.has_action("quit-app"),
            "quit-app action must be registered on the window so Ctrl+Shift+W routes to it"
        );
    }

    /// Scenario: dispatching the quit-app action through GAction
    /// activation (the same path the keyboard accelerator takes) must
    /// schedule the confirmation dialog rather than closing the window
    /// outright. We pin "no synchronous close" so a future refactor
    /// that drops the confirm step is caught here, not in production.
    #[gtk::test]
    async fn activating_quit_app_action_does_not_close_window_synchronously() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.QuitAppActionNoSyncClose")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let window = adw::ApplicationWindow::builder()
            .application(&app)
            .default_width(320)
            .default_height(240)
            .build();
        let entry = make_quit_app_action(window.clone());
        window.add_action_entries([entry]);

        let close_count = Rc::new(Cell::new(0u32));
        let close_count_for_handler = close_count.clone();
        window.connect_close_request(move |_| {
            close_count_for_handler.set(close_count_for_handler.get() + 1);
            // Stop so the window survives test teardown.
            glib::Propagation::Stop
        });

        gtk::prelude::WidgetExt::activate_action(&window, "win.quit-app", None)
            .expect("win.quit-app action should be registered on the window");

        // Pump one idle cycle so any synchronous-close bug would have
        // already surfaced. The intended behaviour is that the action
        // pops a confirm dialog and waits for the user's response, so
        // close_count must remain zero here.
        let (idle_tx, idle_rx) = oneshot::channel();
        glib::idle_add_local_once(move || {
            let _ = idle_tx.send(());
        });
        let _ = idle_rx.await;

        assert_eq!(
            close_count.get(),
            0,
            "Ctrl+Shift+W must wait for the confirm dialog response — it must not close the window before the user picks Quit"
        );
    }

    // ---- copy-pane-path leader chord ----

    #[test]
    fn copy_pane_path_default_is_ctrl_shift_k() {
        assert_eq!(default_for(ActionId::CopyPanePath), vec!["<Ctrl><Shift>k"]);
    }

    #[test]
    fn copy_pane_path_is_user_editable_and_round_trips() {
        assert!(ActionId::CopyPanePath.is_user_editable());
        assert_eq!(
            ActionId::from_wire(ActionId::CopyPanePath.as_str()),
            Some(ActionId::CopyPanePath)
        );
    }

    #[test]
    fn copy_pane_path_user_override_replaces_default() {
        use flowmux_config::keybindings::KeybindingOverrides;
        let mut overrides = KeybindingOverrides::default();
        overrides.set(ActionId::CopyPanePath, vec!["<Ctrl><Alt>p".into()]);
        assert_eq!(
            resolved_accels(&overrides, ActionId::CopyPanePath),
            vec!["<Ctrl><Alt>p".to_string()]
        );
    }

    #[test]
    fn copy_pane_path_empty_override_unbinds_chord() {
        use flowmux_config::keybindings::KeybindingOverrides;
        let mut overrides = KeybindingOverrides::default();
        overrides.set(ActionId::CopyPanePath, vec![]);
        assert!(resolved_accels(&overrides, ActionId::CopyPanePath).is_empty());
    }

    /// Action installation smoke test: a fresh window with the
    /// `copy-pane-path` action registered must accept activation via
    /// the `win.copy-pane-path` name without panicking. The action's
    /// real side effect (clipboard write + toast) needs a focused
    /// terminal pane that the bare unit-test rig does not have, so
    /// this is intentionally a registration check.
    #[gtk::test]
    fn copy_pane_path_action_is_registered_on_application_window() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.CopyPanePathRegistered")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let window = adw::ApplicationWindow::builder()
            .application(&app)
            .default_width(320)
            .default_height(240)
            .build();

        let fired = Rc::new(Cell::new(false));
        let fired_for_action = fired.clone();
        let entry = gtk::gio::ActionEntry::builder("copy-pane-path")
            .activate(move |_, _, _| {
                fired_for_action.set(true);
            })
            .build();
        window.add_action_entries([entry]);

        gtk::prelude::WidgetExt::activate_action(&window, "win.copy-pane-path", None)
            .expect("win.copy-pane-path action should be registered on the window");
        assert!(
            fired.get(),
            "activating win.copy-pane-path must call its handler exactly once"
        );
    }

    #[gtk::test]
    fn insert_newline_action_is_registered_on_application_window() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.InsertNewlineRegistered")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let window = adw::ApplicationWindow::builder()
            .application(&app)
            .default_width(320)
            .default_height(240)
            .build();

        let fired = Rc::new(Cell::new(false));
        let fired_for_action = fired.clone();
        let entry = gtk::gio::ActionEntry::builder(INSERT_NEWLINE_ACTION)
            .activate(move |_, _, _| {
                fired_for_action.set(true);
            })
            .build();
        window.add_action_entries([entry]);

        gtk::prelude::WidgetExt::activate_action(&window, INSERT_NEWLINE_FULL_ACTION, None)
            .expect("win.insert-newline action should be registered on the window");
        assert!(
            fired.get(),
            "activating win.insert-newline must call its handler exactly once"
        );
    }

    #[gtk::test]
    fn clipboard_toast_show_with_message_updates_label() {
        use crate::ui::window::ClipboardToast;
        adw::init().expect("libadwaita should initialize in GTK test");
        let toast = ClipboardToast::new();
        assert_eq!(toast.current_message(), ClipboardToast::DEFAULT_MESSAGE);
        toast.show_with_message("Copied path: /tmp/foo");
        assert_eq!(toast.current_message(), "Copied path: /tmp/foo");
    }
}
