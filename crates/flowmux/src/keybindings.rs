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
use crate::ui::window::ClipboardToast;
use adw::prelude::*;
use flowmux_core::SplitDirection;
use gtk::glib;
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
    // Ctrl+Shift+W asks the user to confirm, then closes the entire
    // flowmux window (which under NON_UNIQUE GApplication is the whole
    // app process). Distinct from Alt+W (single tab/pane only) so an
    // accidental modifier slip can't nuke the whole session.
    ("win.quit-app", &["<Ctrl><Shift>w"]),
    // Tab navigation. Bare Tab and Shift+Tab are reserved for the terminal
    // (shell completion, agent shortcuts, etc.). Ctrl+Tab cycles the left
    // workspace list.
    ("win.next-surface", &[]),
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
    // Ctrl+N opens a new workspace inside this window; Ctrl+Shift+N launches
    // a brand-new flowmux window (a separate OS process under NON_UNIQUE).
    // The two are deliberately split so the unshifted form stays cheap and
    // local while the shifted form is the heavier "open another app window".
    ("win.new-workspace", &["<Ctrl>n"]),
    ("win.new-window", &["<Ctrl><Shift>n"]),
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

    let copy = make_copy_action(focused.clone(), registry.clone(), clipboard_toast);
    let paste = make_paste_action(focused.clone(), registry);

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
    let (dialog, rx) = build_quit_dialog(Some(window));
    dialog.present();
    rx.await.unwrap_or(false)
}

/// Construct the quit-confirmation dialog plus a oneshot receiver that
/// resolves to `true` for "quit" / `false` for cancel-or-dismiss.
/// Split out from [`confirm_quit_app`] so tests can present the dialog,
/// fire a response programmatically (`dialog.response(...)`), and
/// observe the receiver value without driving an actual GUI session.
fn build_quit_dialog(
    parent: Option<&adw::ApplicationWindow>,
) -> (gtk::MessageDialog, oneshot::Receiver<bool>) {
    let dialog = gtk::MessageDialog::builder()
        .modal(true)
        .message_type(gtk::MessageType::Question)
        .text("Quit flowmux?")
        .secondary_text("This closes the flowmux window and stops every running tab.")
        .build();
    dialog.set_transient_for(parent);
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    let quit_button = dialog.add_button("Quit", gtk::ResponseType::Accept);
    quit_button.add_css_class("destructive-action");
    dialog.set_default_response(gtk::ResponseType::Cancel);

    let (tx, rx) = oneshot::channel::<bool>();
    let tx_cell: Rc<Cell<Option<oneshot::Sender<bool>>>> = Rc::new(Cell::new(Some(tx)));
    let tx_for_resp = tx_cell.clone();
    dialog.connect_response(move |dialog, response| {
        if let Some(tx) = tx_for_resp.take() {
            let _ = tx.send(response == gtk::ResponseType::Accept);
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
            if !term.widget.has_selection() {
                return;
            }
            term.widget.copy_clipboard_format(vte::Format::Text);
            clipboard_toast.show();
        })
        .build()
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
            term.widget.paste_clipboard();
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
    fn shift_tab_is_reserved_for_terminal_and_agents() {
        assert!(accels("win.next-surface").is_empty());
        assert!(!accels("win.next-workspace")
            .iter()
            .any(|accel| *accel == "<Shift>Tab" || *accel == "<Shift>ISO_Left_Tab"));
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

    /// Ctrl+N must create a workspace inside the current window and
    /// Ctrl+Shift+N must launch a brand-new flowmux window. These were
    /// swapped intentionally — the unshifted form is the cheap/local
    /// action, the shifted form is the heavier "open another app window"
    /// action. Pinning both so a future shortcut shuffle doesn't quietly
    /// flip them back.
    #[test]
    fn ctrl_n_opens_new_workspace_and_ctrl_shift_n_opens_new_window() {
        assert_eq!(accels("win.new-workspace"), &["<Ctrl>n"]);
        assert_eq!(accels("win.new-window"), &["<Ctrl><Shift>n"]);
        // The two actions must not share an accelerator — sharing would
        // collapse "new workspace in this window" and "new window" onto
        // the same key.
        let ws: std::collections::HashSet<_> =
            accels("win.new-workspace").iter().copied().collect();
        for accel in accels("win.new-window") {
            assert!(
                !ws.contains(accel),
                "win.new-window must not share an accelerator with win.new-workspace"
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
        assert_eq!(accels("win.quit-app"), &["<Ctrl><Shift>w"]);
        assert_eq!(accels("win.close-surface"), &["<Alt>w"]);
        // The two bindings must not share an accelerator — sharing
        // would mean one key both closes a tab and asks to quit the
        // whole app, which is exactly the regression the user hit on
        // Alt+W and the reason quit-app got its own modifier combo.
        let close_set: std::collections::HashSet<_> =
            accels("win.close-surface").iter().copied().collect();
        for accel in accels("win.quit-app") {
            assert!(
                !close_set.contains(accel),
                "win.quit-app must not share an accelerator with win.close-surface"
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
        let (dialog, _rx) = build_quit_dialog(None);
        // Both responses are enabled and exposed by id so the action
        // handler can compare against stable GTK response values.
        assert!(dialog
            .widget_for_response(gtk::ResponseType::Cancel)
            .is_some());
        let quit = dialog
            .widget_for_response(gtk::ResponseType::Accept)
            .expect("quit response button");
        assert!(quit.has_css_class("destructive-action"));
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

        let (dialog, rx) = build_quit_dialog(None);
        dialog.response(gtk::ResponseType::Accept);
        let result = rx.await.expect("dialog must signal a response");
        assert!(
            result,
            "picking Quit must resolve to true so the action proceeds with window.close()"
        );

        let (dialog2, rx2) = build_quit_dialog(None);
        dialog2.response(gtk::ResponseType::Cancel);
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
}
