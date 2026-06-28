// SPDX-License-Identifier: GPL-3.0-or-later
//! Terminal pane type + the shared per-pane callback bundle.
//!
//! flowmux renders terminals with the libghostty-vt backend
//! ([`crate::ui::ghostty_pane::GhosttyPane`]), so `PaneTerminal` is an alias for
//! it. (Historically this was an enum over a VTE backend and the libghostty
//! backend; VTE has since been removed.) The pane registry stores
//! `PaneTerminal`; spawn-time wiring lives in `workspace_view.rs`.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use flowmux_core::{PaneId, SurfaceId, WorkspaceId};

use crate::ui::ghostty_pane::GhosttyPane;

/// The terminal pane type used throughout the GUI.
pub type PaneTerminal = GhosttyPane;

/// Shift+Enter input sequence: insert a literal newline at the prompt without
/// submitting, after committing any in-progress IME text.
pub use crate::ui::ghostty_pane::INSERT_NEWLINE_BYTES;

/// Per-pane callbacks the surface backends invoke to drive the window
/// controller (focus, tab/pane menu actions, title changes, …). Shared by
/// the terminal and browser panes.
#[derive(Clone)]
pub struct PaneCallbacks {
    pub on_child_exited: Rc<RefCell<dyn FnMut(PaneId, i32)>>,
    pub on_focus: Rc<RefCell<dyn FnMut(PaneId)>>,
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
    /// Reorder a tab within the same pane by drag and drop. The third argument
    /// is the final 0-based index after the move, clamped if it exceeds length.
    pub on_reorder_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, usize)>>,
    /// A tab drag ended without landing on another tab drop target. The caller
    /// moves that live surface into a new top-level window and removes it from
    /// the source pane.
    pub on_tab_drag_to_new_window: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// Move a tab into another pane (possibly in another workspace) by drag and
    /// drop, preserving its live state. Args: source pane, surface, destination
    /// pane, final 0-based index in the destination (clamped to the end).
    pub on_move_surface_to_pane: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, PaneId, usize)>>,
    /// Move a tab to the last position of the first pane of `dst_workspace`.
    /// Backs the right-click "Move" menu and drops onto a side-panel workspace.
    pub on_move_surface_to_workspace: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, WorkspaceId)>>,
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
    /// Called when Ctrl+click selects a URL inside the terminal. The caller
    /// opens that URL in a new browser tab in the same pane
    /// (GtkCommand::OpenUrlInBrowserTab). The URL arrives with trailing
    /// punctuation already trimmed.
    pub on_open_url: Rc<RefCell<dyn FnMut(PaneId, String)>>,
}
