// SPDX-License-Identifier: GPL-3.0-or-later
//! Bridge between the tokio IPC handler thread and the GTK main loop.
//!
//! GTK widgets are `!Send`, so anything that touches the widget tree
//! must run on the main thread. The IPC server, state store, and
//! desktop notifier all run on tokio. We connect them with an async
//! channel: tokio side sends [`GtkCommand`] values, the GTK side reads
//! them via `glib::MainContext::spawn_local` and dispatches into the
//! window controller.

use flowmux_core::{PaneId, SplitDirection, WorkspaceId};
use std::path::PathBuf;
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy)]
pub enum FocusDir { Left, Right, Up, Down }

/// Direction for tab-list cyclic navigation.
#[derive(Debug, Clone, Copy)]
pub enum WsNav { Next, Prev }

/// One-way commands from tokio → GTK main loop. Each variant carries a
/// `oneshot::Sender` for replies if the caller needs the result.
#[derive(Debug)]
pub enum GtkCommand {
    /// Render a freshly-created workspace in the sidebar + open its first pane.
    WorkspaceCreated {
        id: WorkspaceId,
        name: String,
        root: PathBuf,
        ack: oneshot::Sender<()>,
    },
    /// Re-render a workspace from the latest store snapshot.
    /// Used after structural mutations like split.
    WorkspaceRerender {
        id: WorkspaceId,
        ack: oneshot::Sender<()>,
    },
    /// Send keystrokes to a pane.
    PaneSendKeys {
        pane: PaneId,
        keys: String,
        ack: oneshot::Sender<Result<(), String>>,
    },
    /// Split the focused pane and re-render its workspace. Used by
    /// keyboard shortcuts (the IPC verb path goes through the daemon
    /// directly via `Request::PaneSplit`).
    SplitFocused {
        pane: PaneId,
        direction: SplitDirection,
        ack: oneshot::Sender<Result<PaneId, String>>,
    },
    /// Close the focused pane. The split tree collapses; if the surface
    /// or the workspace becomes empty, those go too.
    CloseFocused {
        pane: PaneId,
        ack: oneshot::Sender<Result<(), String>>,
    },
    /// Move keyboard focus to the nearest pane in `dir`.
    FocusDirection {
        from: PaneId,
        dir: FocusDir,
    },
    /// Open a brand-new terminal surface in the active workspace.
    /// (Reserved for the planned horizontal surface-tab bar; currently
    /// unused since the sidebar shows workspaces, not surfaces.)
    #[allow(dead_code)]
    NewSurface,
    /// Create a brand-new workspace and add it to the sidebar. This is
    /// what Ctrl+Shift+T binds to in our model — matching how ghostty's
    /// `cmd+t = new_tab` adds a visible top-level navigation entry.
    NewWorkspace {
        root: std::path::PathBuf,
    },
    /// Remove a workspace entirely (sidebar row + stack page + state).
    /// Triggered by the hover X button on a sidebar row.
    RemoveWorkspace {
        id: WorkspaceId,
        ack: oneshot::Sender<()>,
    },
    /// Rename a workspace and refresh its sidebar row.
    RenameWorkspace {
        id: WorkspaceId,
        name: String,
        ack: oneshot::Sender<()>,
    },
    /// Recolor a workspace and refresh its sidebar row.
    SetWorkspaceColor {
        id: WorkspaceId,
        color: String,
        ack: oneshot::Sender<()>,
    },
    /// Cycle to the previous / next workspace in sidebar order.
    FocusWorkspaceDir {
        dir: WsNav,
    },
    /// Jump straight to the N-th workspace (1-indexed; clamped to
    /// what currently exists).
    FocusWorkspaceAt {
        idx: u8,
    },
    /// A notification was raised on a pane (from VTE OSC signal). Update
    /// the pane border / sidebar badge.
    #[allow(dead_code)]
    NotificationOnPane {
        pane: PaneId,
        title: String,
        body: String,
    },
    /// Evaluate JavaScript in a browser pane.
    BrowserEval {
        pane: PaneId,
        source: String,
        ack: oneshot::Sender<Result<String, String>>,
    },
    /// Inject a list of cookies into the WebKit cookie manager.
    InjectCookies {
        cookies: Vec<flowmux_cookies::Cookie>,
        ack: oneshot::Sender<Result<usize, String>>,
    },
}

#[derive(Clone)]
pub struct Bridge {
    pub tx: async_channel::Sender<GtkCommand>,
}

impl Bridge {
    pub fn new() -> (Self, async_channel::Receiver<GtkCommand>) {
        let (tx, rx) = async_channel::unbounded();
        (Self { tx }, rx)
    }
}
