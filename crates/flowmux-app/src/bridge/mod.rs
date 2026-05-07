// SPDX-License-Identifier: GPL-3.0-or-later
//! Bridge between the tokio IPC handler thread and the GTK main loop.
//!
//! GTK widgets are `!Send`, so anything that touches the widget tree
//! must run on the main thread. The IPC server, state store, and
//! desktop notifier all run on tokio. We connect them with an async
//! channel: tokio side sends [`GtkCommand`] values, the GTK side reads
//! them via `glib::MainContext::spawn_local` and dispatches into the
//! window controller.

use flowmux_core::{NotificationLevel, PaneId, SplitDirection, SurfaceId, WorkspaceId};
use std::path::PathBuf;
use tokio::sync::oneshot;

/// Cmux-style scriptable browser controller verb. One variant per
/// public method on `flowmux_browser::BrowserController`. Bundled into
/// a single bridge variant so the dispatcher only has to handle one
/// "browser" arm regardless of how many verbs there are.
#[derive(Debug, Clone)]
pub enum BrowserOp {
    Navigate { url: String },
    Back,
    Forward,
    Reload,
    Url,
    Title,
    Snapshot,
    Eval { source: String },
    Click { target: String },
    Fill { target: String, value: String },
    Select { target: String, value: String },
    Scroll { target: String, x: i32, y: i32 },
    Type { text: String },
    Press { key: String },
    Text { target: String },
    Value { target: String },
    Attr { target: String, name: String },
}

/// Result shape returned by [`BrowserOp`] dispatch.
#[derive(Debug, Clone)]
pub enum BrowserActionResult {
    /// Acknowledgement for verbs that don't read anything back.
    Ok,
    /// Boolean result (Back / Forward navigation success).
    Bool(bool),
    /// String result (URL, title, page text/value/attr, eval output,
    /// snapshot JSON).
    String(String),
}

#[derive(Debug, Clone, Copy)]
pub enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

/// Direction for tab-list cyclic navigation.
#[derive(Debug, Clone, Copy)]
pub enum WsNav {
    Next,
    Prev,
}

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
    FocusDirection { from: PaneId, dir: FocusDir },
    /// Open a brand-new terminal surface in the active workspace.
    /// (Reserved for the planned horizontal surface-tab bar; currently
    /// unused since the sidebar shows workspaces, not surfaces.)
    NewSurface { pane: PaneId },
    /// 같은 pane에 빈(about:blank) 탭브라우저를 새 탭으로 추가한다.
    NewBrowserSurface { pane: PaneId },
    /// Switch the active pane-local surface tab.
    ActivateSurface { pane: PaneId, surface: SurfaceId },
    /// Close a pane-local surface tab.
    CloseSurface {
        pane: PaneId,
        surface: SurfaceId,
        ack: oneshot::Sender<Result<(), String>>,
    },
    /// Rename a pane-local surface tab and refresh its workspace.
    RenameSurface {
        pane: PaneId,
        surface: SurfaceId,
        title: String,
        ack: oneshot::Sender<Result<(), String>>,
    },
    /// Open the rename dialog for a pane-local surface tab.
    ShowRenameSurfaceDialog { pane: PaneId, surface: SurfaceId },
    /// VTE reported a cwd change for a terminal surface.
    TerminalCwdChanged {
        pane: PaneId,
        surface: SurfaceId,
        cwd: PathBuf,
    },
    /// Create a brand-new workspace and add it to the sidebar.
    NewWorkspace { root: std::path::PathBuf },
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
    /// Open the 'Change tab name' dialog for `id`. Bridge-driven so
    /// the dialog runs in the window dispatch loop where the parent
    /// window reference is in scope.
    ShowRenameDialog { id: WorkspaceId },
    /// Open the color picker dialog for `id`.
    ShowColorDialog { id: WorkspaceId },
    /// Append a notification to the in-process log shown in the
    /// sidebar's bell popover. flowmux-notify still delivers the real
    /// desktop notification through D-Bus; this is the GUI tee.
    AddNotification {
        pane: Option<PaneId>,
        title: String,
        body: String,
        level: NotificationLevel,
    },
    /// Cycle to the previous / next workspace in sidebar order.
    FocusWorkspaceDir { dir: WsNav },
    /// Jump straight to the N-th workspace (1-indexed; clamped to
    /// what currently exists).
    FocusWorkspaceAt { idx: u8 },
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
    /// Run a [`BrowserOp`] against a specific browser pane. Used by
    /// the cmux-style scriptable verbs (navigate / click / fill /
    /// snapshot / …) the IPC layer exposes.
    BrowserAction {
        pane: PaneId,
        op: BrowserOp,
        ack: oneshot::Sender<Result<BrowserActionResult, String>>,
    },
    /// Split a target pane and put a brand-new browser pane in the
    /// new sibling. `target_pane = None` means "use the focused
    /// pane"; the dispatcher resolves it on the GTK side. Returns
    /// the new browser pane's id.
    BrowserOpenSplit {
        target_pane: Option<PaneId>,
        url: String,
        direction: SplitDirection,
        ack: oneshot::Sender<Result<PaneId, String>>,
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
