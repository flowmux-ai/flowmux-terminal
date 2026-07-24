// SPDX-License-Identifier: GPL-3.0-or-later
//! Terminal pane type + the shared per-pane callback bundle.
//!
//! flowmux renders terminals with the VTE-backed
//! [`crate::ui::ghostty_pane::GhosttyPane`], so `PaneTerminal` is an alias for
//! it. (Historically this was an enum over multiple backends.) The pane registry stores
//! `PaneTerminal`; spawn-time wiring lives in `workspace_view.rs`.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use flowmux_core::{PaneId, PaneSurface, SplitDirection, SurfaceId, WorkspaceId};
use tokio::sync::oneshot;

use crate::ui::ghostty_pane::GhosttyPane;

/// The terminal pane type used throughout the GUI.
pub type PaneTerminal = GhosttyPane;

/// Shift+Enter input sequence: insert a literal newline at the prompt without
/// submitting, after committing any in-progress IME text.
pub use crate::ui::ghostty_pane::INSERT_NEWLINE_BYTES;

#[derive(Debug, Clone)]
pub enum TabDropCommand {
    MoveToPane {
        src_pane: PaneId,
        surface: SurfaceId,
        surface_model: Option<PaneSurface>,
        dst_pane: PaneId,
        target_index: usize,
    },
    SplitIntoPane {
        src_pane: PaneId,
        surface: SurfaceId,
        surface_model: Option<PaneSurface>,
        dst_pane: PaneId,
        direction: SplitDirection,
    },
    Reorder {
        pane: PaneId,
        surface: SurfaceId,
        target_index: usize,
    },
}

/// Per-pane callbacks the surface backends invoke to drive the window
/// controller (focus, tab/pane menu actions, title changes, …). Shared by
/// the terminal and browser panes.
#[derive(Clone)]
pub struct PaneCallbacks {
    pub on_child_exited: Rc<RefCell<dyn FnMut(PaneId, i32)>>,
    pub on_focus: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// The embedded editor keeps ordinary keys inside Monaco and reports only
    /// plain Alt+arrow so the window can move focus through the pane tree.
    pub on_editor_focus_direction:
        Rc<RefCell<dyn FnMut(PaneId, flowmux_editor::EditorFocusDirection)>>,
    /// Terminal-body right-click menu 'Close Pane'.
    pub on_close_pane: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Right-click menu 'Split Right'.
    pub on_split_right: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Right-click menu 'Split Down'.
    pub on_split_down: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Pane-local surface tab activation.
    pub on_activate_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// Pane-local new terminal tab.
    pub on_new_surface: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Pane-local new browser tab.
    pub on_new_browser_surface: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Pane-local close tab.
    pub on_close_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// Pane-local rename tab.
    pub on_rename_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// Tab right-click "Show in folder" → open file manager at the
    /// terminal surface's current working directory. Only invoked from
    /// terminal tab popovers; browser tabs skip the menu entirely.
    pub on_show_surface_folder: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// Per-surface "Copy path" / "Copy URL" handler. The dispatcher
    /// reads the surface kind and copies cwd or URL accordingly, so
    /// the same callback is reused by both terminal and browser
    /// right-click menus.
    pub on_copy_surface_text: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// A tab drag ended without landing on another tab drop target. The caller
    /// moves that live surface into a new top-level window and removes it from
    /// the source pane.
    pub on_tab_drag_to_new_window: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// Move a tab to the last position of the first pane of `dst_workspace`.
    /// Backs the right-click "Move" menu and drops onto a side-panel workspace.
    pub on_move_surface_to_workspace: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, WorkspaceId)>>,
    /// Split `dst_pane` in the given direction and move the dragged tab into the
    /// new sibling, preserving its live state. Backs dropping on the right /
    /// bottom region of a pane body.
    pub on_split_surface_into_pane:
        Rc<RefCell<dyn FnMut(PaneId, SurfaceId, Option<PaneSurface>, PaneId, SplitDirection)>>,
    /// Dispatch a DnD mutation and return its controller acknowledgement.
    pub dispatch_tab_drop:
        Rc<dyn Fn(TabDropCommand) -> Option<oneshot::Receiver<Result<(), String>>>>,
    /// Snapshot of the current workspaces (id + display name) at call time, used
    /// to populate the right-click "Move" submenu so it reflects live state.
    pub list_workspaces: Rc<dyn Fn() -> Vec<(WorkspaceId, String)>>,
    /// The workspace a given pane currently lives in, queried synchronously so
    /// the "Move" submenu can exclude the tab's own workspace at click time.
    pub workspace_of_pane: Rc<dyn Fn(PaneId) -> Option<WorkspaceId>>,
    /// Shared across all surface tabs in one window for the duration of a drag.
    /// The source tab uses this to distinguish a true no-target drag from a
    /// rejected drop on a known tab (self/cross-pane/invalid payload).
    pub tab_drag_drop_seen: Rc<Cell<bool>>,
    /// Set once a same-window tab drop callback has started. This keeps a
    /// post-drop leave event from making the source treat the move as remote.
    pub tab_drag_drop_committed: Rc<Cell<bool>>,
    /// Last split preview shown while dragging a tab over a pane body. GTK can
    /// report the final drop as having no target near zone edges; in that case
    /// drag end commits the previewed split instead of tearing off.
    pub tab_drag_split_candidate: Rc<RefCell<Option<(PaneId, SplitDirection)>>>,
    /// The terminal reported that a surface changed its cwd (OSC 7). The
    /// controller refreshes the window title / VCS sidebar and records the
    /// cwd so a new tab in the pane inherits it.
    pub on_terminal_cwd_changed: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, PathBuf)>>,
    /// WebKit reported that a browser pane navigated to a new URL.
    pub on_browser_uri_changed: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, String)>>,
    /// WebKit reported that a browser pane's page title changed.
    pub on_browser_title_changed: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, String)>>,
    /// The terminal reported an OSC 0/2 window title, often emitted by programs
    /// such as vi, claude, codex, or tmux inside the shell. Empty titles are
    /// ignored by the caller.
    pub on_terminal_title_changed: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, String)>>,
    /// Return the current user options. Used when creating a new BrowserPane to
    /// choose the engine and apply zoom immediately after widget creation. This
    /// cheaply clones the `Rc<RefCell<Options>>` held by WindowController, so
    /// dialog updates are visible on the next call.
    pub read_options: Rc<dyn Fn() -> flowmux_config::options::Options>,
    /// Return the surface's current 0-based index within the same pane. Tab DnD
    /// uses PaneRegistry::surface_tabs to compute final_index from the source
    /// and target relative positions.
    pub position_of_surface_in_pane: Rc<dyn Fn(PaneId, SurfaceId) -> Option<usize>>,
    /// Resolve a point in the window root to the rendered pane under it and
    /// normalized coordinates inside that pane. macOS uses this when native
    /// WebViews prevent GDK from delivering a normal drop event.
    #[cfg(target_os = "macos")]
    pub pane_at_root_point: Rc<dyn Fn(&gtk::Widget, f64, f64) -> Option<(PaneId, f64, f64)>>,
    /// Resolve a point in the window root to a surface tab, including its
    /// current index and whether the point is on the tab's trailing half.
    #[cfg(target_os = "macos")]
    pub tab_at_root_point:
        Rc<dyn Fn(&gtk::Widget, f64, f64) -> Option<(PaneId, SurfaceId, usize, bool)>>,
    /// Called when Ctrl+click selects a URL inside the terminal. The caller
    /// opens that URL in a new browser tab in the same pane
    /// (GtkCommand::OpenUrlInBrowserTab). The URL arrives with trailing
    /// punctuation already trimmed.
    pub on_open_url: Rc<RefCell<dyn FnMut(PaneId, String)>>,
    /// Called when Ctrl+click selects an absolute image path inside the
    /// terminal. The caller opens it in a dedicated image viewer window.
    pub on_open_image: Rc<RefCell<dyn FnMut(PaneId, PathBuf)>>,
    /// Called when Ctrl+click selects a Markdown path inside the terminal.
    /// The caller opens it in the Markdown viewer binary.
    pub on_open_markdown: Rc<RefCell<dyn FnMut(PaneId, PathBuf)>>,
}

#[cfg(test)]
impl PaneCallbacks {
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn noop_for_test() -> Self {
        Self {
            on_child_exited: Rc::new(RefCell::new(|_, _| {})),
            on_focus: Rc::new(RefCell::new(|_| {})),
            on_editor_focus_direction: Rc::new(RefCell::new(|_, _| {})),
            on_close_pane: Rc::new(RefCell::new(|_| {})),
            on_split_right: Rc::new(RefCell::new(|_| {})),
            on_split_down: Rc::new(RefCell::new(|_| {})),
            on_activate_surface: Rc::new(RefCell::new(|_, _| {})),
            on_new_surface: Rc::new(RefCell::new(|_| {})),
            on_new_browser_surface: Rc::new(RefCell::new(|_| {})),
            on_close_surface: Rc::new(RefCell::new(|_, _| {})),
            on_rename_surface: Rc::new(RefCell::new(|_, _| {})),
            on_show_surface_folder: Rc::new(RefCell::new(|_, _| {})),
            on_copy_surface_text: Rc::new(RefCell::new(|_, _| {})),
            on_tab_drag_to_new_window: Rc::new(RefCell::new(|_, _| {})),
            on_move_surface_to_workspace: Rc::new(RefCell::new(|_, _, _| {})),
            on_split_surface_into_pane: Rc::new(RefCell::new(|_, _, _, _, _| {})),
            dispatch_tab_drop: Rc::new(|_| None),
            list_workspaces: Rc::new(Vec::new),
            workspace_of_pane: Rc::new(|_| None),
            tab_drag_drop_seen: Rc::new(Cell::new(false)),
            tab_drag_drop_committed: Rc::new(Cell::new(false)),
            tab_drag_split_candidate: Rc::new(RefCell::new(None)),
            on_terminal_cwd_changed: Rc::new(RefCell::new(|_, _, _| {})),
            on_browser_uri_changed: Rc::new(RefCell::new(|_, _, _| {})),
            on_browser_title_changed: Rc::new(RefCell::new(|_, _, _| {})),
            on_terminal_title_changed: Rc::new(RefCell::new(|_, _, _| {})),
            read_options: Rc::new(flowmux_config::options::Options::default),
            position_of_surface_in_pane: Rc::new(|_, _| None),
            #[cfg(target_os = "macos")]
            pane_at_root_point: Rc::new(|_, _, _| None),
            #[cfg(target_os = "macos")]
            tab_at_root_point: Rc::new(|_, _, _| None),
            on_open_url: Rc::new(RefCell::new(|_, _| {})),
            on_open_image: Rc::new(RefCell::new(|_, _| {})),
            on_open_markdown: Rc::new(RefCell::new(|_, _| {})),
        }
    }
}
