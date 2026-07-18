// SPDX-License-Identifier: GPL-3.0-or-later
//! Bridge between the tokio IPC handler thread and the GTK main loop.
//!
//! GTK widgets are `!Send`, so anything that touches the widget tree
//! must run on the main thread. The IPC server, state store, and
//! desktop notifier all run on tokio. We connect them with an async
//! channel: tokio side sends [`GtkCommand`] values, the GTK side reads
//! them via `glib::MainContext::spawn_local` and dispatches into the
//! window controller.

use flowmux_core::{
    AgentStatus, NotificationId, NotificationLevel, PaneId, PaneSurface, PlacementStrategy,
    SplitDirection, SurfaceId, WorkspaceId,
};
use flowmux_ipc::protocol::{BrowserWaitCondition, NotificationSummary};
use std::path::PathBuf;
use tokio::sync::oneshot;

/// Result of a successful `BrowserOpenSplit`. The dispatcher reports
/// both the new browser pane's id and how it was placed (cmux's
/// `placement_strategy`) so the IPC layer can forward both to the agent.
#[derive(Debug, Clone, Copy)]
pub struct BrowserOpenOutcome {
    pub pane: PaneId,
    pub placement_strategy: PlacementStrategy,
}

/// Cmux-style scriptable browser controller verb. One variant per
/// public method on `flowmux_browser::BrowserController`. Bundled into
/// a single bridge variant so the dispatcher only has to handle one
/// "browser" arm regardless of how many verbs there are.
#[derive(Debug, Clone)]
pub enum BrowserOp {
    /// Take a non-mutating page snapshot. Runs `scripts::SNAPSHOT_JS`,
    /// repopulates the pane's `RefStore` from the returned `refs` map,
    /// and returns the snapshot JSON. The live DOM is never stamped.
    Snapshot,
    Navigate {
        url: String,
    },
    Back,
    Forward,
    Reload,
    Url,
    Title,
    Click {
        target: String,
    },
    Fill {
        target: String,
        value: String,
    },
    Select {
        target: String,
        value: String,
    },
    Scroll {
        target: String,
        x: i32,
        y: i32,
    },
    Type {
        text: String,
    },
    Press {
        key: String,
    },
    Text {
        target: String,
    },
    Value {
        target: String,
    },
    Attr {
        target: String,
        name: String,
    },
    Wait {
        condition: BrowserWaitCondition,
        timeout_ms: u64,
        poll_ms: u64,
    },
    Screenshot {
        path: PathBuf,
    },

    // ---- Phase 5 P0 action gap ----
    DblClick {
        target: String,
    },
    Hover {
        target: String,
    },
    Focus {
        target: String,
    },
    Blur {
        target: String,
    },
    Check {
        target: String,
    },
    Uncheck {
        target: String,
    },
    IsVisible {
        target: String,
    },
    IsEnabled {
        target: String,
    },
    IsChecked {
        target: String,
    },
    Count {
        selector: String,
    },
}

impl BrowserOp {
    /// Stable public verb name used by capability reporting and diagnostics.
    /// Keep this mapping next to the operation enum so adding an operation
    /// cannot silently omit its agent-facing name.
    pub fn capability_name(&self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::Navigate { .. } => "navigate",
            Self::Back => "back",
            Self::Forward => "forward",
            Self::Reload => "reload",
            Self::Url => "url",
            Self::Title => "title",
            Self::Click { .. } => "click",
            Self::Fill { .. } => "fill",
            Self::Select { .. } => "select",
            Self::Scroll { .. } => "scroll",
            Self::Type { .. } => "type",
            Self::Press { .. } => "press",
            Self::Text { .. } => "text",
            Self::Value { .. } => "value",
            Self::Attr { .. } => "attr",
            Self::Wait { .. } => "wait",
            Self::Screenshot { .. } => "screenshot",
            Self::DblClick { .. } => "dblclick",
            Self::Hover { .. } => "hover",
            Self::Focus { .. } => "focus",
            Self::Blur { .. } => "blur",
            Self::Check { .. } => "check",
            Self::Uncheck { .. } => "uncheck",
            Self::IsVisible { .. } => "is-visible",
            Self::IsEnabled { .. } => "is-enabled",
            Self::IsChecked { .. } => "is-checked",
            Self::Count { .. } => "count",
        }
    }

    /// Whether this operation primarily reads browser state. `wait` is a
    /// query because it observes a condition without mutating the page.
    pub fn is_query(&self) -> bool {
        matches!(
            self,
            Self::Snapshot
                | Self::Url
                | Self::Title
                | Self::Text { .. }
                | Self::Value { .. }
                | Self::Attr { .. }
                | Self::Wait { .. }
                | Self::IsVisible { .. }
                | Self::IsEnabled { .. }
                | Self::IsChecked { .. }
                | Self::Count { .. }
        )
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Show the modal options dialog from the GTK side.
    /// The dialog owns OK / cancel handling and returns nothing through
    /// the bridge.
    ShowOptionsDialog,
    /// Show the command palette from the GTK side.
    ShowCommandPalette,
    /// Render a freshly-created workspace in the sidebar + open its first pane.
    WorkspaceCreated {
        id: WorkspaceId,
        #[allow(dead_code)]
        name: String,
        #[allow(dead_code)]
        root: PathBuf,
        ack: oneshot::Sender<()>,
    },
    /// Apply a split that was already committed to the store.
    PaneSplitApplied {
        id: WorkspaceId,
        pane: PaneId,
        new_pane: PaneId,
        direction: SplitDirection,
        ack: oneshot::Sender<()>,
    },
    /// Send keystrokes to a pane.
    PaneSendKeys {
        pane: PaneId,
        keys: String,
        ack: oneshot::Sender<Result<(), String>>,
    },
    /// Read the terminal buffer text of a pane (`flowmux read-screen`).
    /// `Ok(None)` means the pane has no readable terminal surface (e.g. a
    /// browser tab); `Err` means the pane id did not resolve to a pane.
    PaneReadScreen {
        pane: PaneId,
        ack: oneshot::Sender<Result<Option<String>, String>>,
    },
    /// Grab keyboard focus for a pane by id (`flowmux focus-pane`).
    /// `Err` when the id is not a live pane. Non-destructive.
    FocusPane {
        pane: PaneId,
        ack: oneshot::Sender<Result<(), String>>,
    },
    /// Temporarily expand one pane without changing the persisted split tree.
    TogglePaneZoom { pane: PaneId },
    ResizePane {
        pane: PaneId,
        ratio: f32,
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
    /// If `from = None`, focus the active workspace's first leaf pane.
    /// This handles pressing Alt+arrow immediately after selecting only
    /// a workspace row in the side panel.
    FocusDirection { from: Option<PaneId>, dir: FocusDir },
    /// Open a brand-new terminal surface in the active workspace.
    /// (Reserved for the planned horizontal surface-tab bar; currently
    /// unused since the sidebar shows workspaces, not surfaces.)
    NewSurface { pane: PaneId },
    /// Open a new tab in the target pane and run tig in its shell.
    OpenTig { pane: PaneId },
    /// Open a terminal tab in the target workspace for `flowmux new-tab`.
    CreateSurface {
        workspace: WorkspaceId,
        cwd: Option<PathBuf>,
        shell: Option<String>,
        ack: oneshot::Sender<Result<(PaneId, SurfaceId), String>>,
    },
    /// Add an empty about:blank browser tab to the same pane.
    NewBrowserSurface { pane: PaneId },
    /// Toggle the right-side FileBrowser rooted at `pane`'s current directory:
    /// open it if hidden, close it if already showing. `None` targets the
    /// currently focused pane (used by the side-panel footer button and the
    /// Ctrl+Alt+F keybinding, neither of which has a pane context of its own).
    ToggleFileBrowser { pane: Option<PaneId> },
    /// Toggle the right-side Git worktree panel for the focused pane.
    ToggleWorktreePanel { pane: Option<PaneId> },
    /// Reload worktrees for the panel's current source pane.
    RefreshWorktrees,
    /// Deliver a background worktree listing to the GTK main thread.
    WorktreesLoaded {
        generation: u64,
        result:
            Result<flowmux_vcs::worktree::WorktreeList, flowmux_vcs::worktree::WorktreeListError>,
    },
    /// Open a worktree in a new workspace or activate its existing workspace.
    OpenWorktree { path: PathBuf },
    /// Apply background Git enrichment to a workspace and its side-panel row.
    WorkspaceGitInfoLoaded {
        workspace: WorkspaceId,
        info: flowmux_core::GitInfo,
    },
    /// Show cached Git and workspace details for a worktree.
    ShowWorktreeInfo { path: PathBuf },
    /// Confirm and remove a worktree checkout while retaining its branch.
    RemoveWorktree { path: PathBuf },
    /// Deliver a completed background worktree removal to the GTK main thread.
    WorktreeRemovalFinished {
        path: PathBuf,
        force: bool,
        result: Result<(), flowmux_vcs::worktree::RemoveWorktreeError>,
    },
    /// Move keyboard focus out of the right-side Git worktree panel.
    WorktreePanelFocusOut { dir: FocusDir },
    /// Close the worktree panel and restore focus to its source pane.
    WorktreePanelCloseAndRestoreFocus,
    /// Move keyboard focus out of the right-side FileBrowser.
    FileBrowserFocusOut { dir: FocusDir },
    /// Close the right-side FileBrowser and restore focus to the source pane.
    FileBrowserCloseAndRestoreFocus,
    /// Open a file in an editor surface selected from the source workspace.
    OpenFileInEditor {
        path: PathBuf,
        source_pane: Option<PaneId>,
    },
    /// Open a Ctrl-clicked terminal URL in a new browser tab in the same
    /// pane. `pane` is the source terminal pane and `url` has already had
    /// trailing punctuation trimmed.
    OpenUrlInBrowserTab { pane: PaneId, url: String },
    /// Open a Ctrl-clicked terminal image path in a dedicated viewer window.
    OpenImageViewer { pane: PaneId, path: PathBuf },
    /// Open a Ctrl-clicked terminal Markdown path in the Markdown viewer binary.
    OpenMarkdownViewer { pane: PaneId, path: PathBuf },
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
    /// Reorder a terminal or browser tab within the same pane by drag and
    /// drop. `target_index` is the final position after the move and is
    /// clamped to the end if it exceeds the length.
    ReorderSurface {
        pane: PaneId,
        surface: SurfaceId,
        target_index: usize,
        ack: oneshot::Sender<Result<(), String>>,
    },
    /// Tear a pane-local surface tab out into its own top-level window. The
    /// existing GTK widget is moved, so the running terminal/browser state is
    /// preserved and the source pane loses that tab.
    TearOffSurface { pane: PaneId, surface: SurfaceId },
    /// Move a pane-local surface tab into another pane (possibly in another
    /// workspace) by drag and drop. The existing GTK widget is moved, so the
    /// running terminal/browser state is preserved. `target_index` is the final
    /// position in the destination pane, clamped to the end.
    MoveSurfaceToPane {
        src_pane: PaneId,
        surface: SurfaceId,
        surface_model: Option<PaneSurface>,
        dst_pane: PaneId,
        target_index: usize,
        ack: oneshot::Sender<Result<(), String>>,
    },
    /// Move a pane-local surface tab to the last position of the first pane of
    /// `dst_workspace`. Backs the right-click "Move" menu and a drop directly
    /// onto a workspace row in the side panel.
    MoveSurfaceToWorkspace {
        src_pane: PaneId,
        surface: SurfaceId,
        dst_workspace: WorkspaceId,
        ack: oneshot::Sender<Result<(), String>>,
    },
    /// Split `dst_pane` in `direction` and move the dragged tab (with its live
    /// state) into the freshly created sibling pane. Backs dropping a tab on the
    /// right / bottom region of a pane body.
    SplitSurfaceIntoPane {
        src_pane: PaneId,
        surface: SurfaceId,
        surface_model: Option<PaneSurface>,
        dst_pane: PaneId,
        direction: SplitDirection,
        ack: oneshot::Sender<Result<(), String>>,
    },
    /// terminal pane reported a cwd change for a terminal surface.
    TerminalCwdChanged {
        pane: PaneId,
        surface: SurfaceId,
        cwd: PathBuf,
    },
    /// WebKit reported that a browser pane navigated to a new URL.
    /// Store it in state so the next launch can restore the same page.
    BrowserUriChanged {
        pane: PaneId,
        surface: SurfaceId,
        url: String,
    },
    /// WebKit reported a page title change. Update the tab label and
    /// window title only when the surface is not title-locked.
    BrowserTitleChanged {
        pane: PaneId,
        surface: SurfaceId,
        title: String,
    },
    /// terminal pane reported an OSC 0/2 window-title change. Programs such as
    /// vi, claude, codex, or tmux can emit these from inside the shell.
    /// The daemon ignores user-renamed surfaces and empty reset titles.
    TerminalTitleChanged {
        pane: PaneId,
        surface: SurfaceId,
        title: String,
    },
    /// Recompute the window title as "flowmux - {focused tab name}".
    /// Sent after focus changes, tab activation, or tab label changes.
    RefreshWindowTitle,
    /// Emitted when a pane receives keyboard focus. The workspace side-panel
    /// label and subtitles are based on the MRU focused pane's active
    /// surface, so they need recomputation on focus moves.
    PaneFocused { pane: PaneId },
    /// Create a brand-new workspace and add it to the sidebar.
    NewWorkspace { root: std::path::PathBuf },
    /// Remove a workspace entirely (sidebar row + stack page + state).
    /// Triggered by the hover X button on a sidebar row, and by
    /// tmux-compat teardown (kill-pane of the last pane / kill-server).
    /// `confirm: false` skips the modal dialog — agent-driven paths
    /// must never block on user input.
    RemoveWorkspace {
        id: WorkspaceId,
        confirm: bool,
        ack: oneshot::Sender<()>,
    },
    /// Remove every open workspace. Triggered by the sidebar context
    /// menu's "Close all tabs" item.
    RemoveAllWorkspaces { ack: oneshot::Sender<()> },
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
    /// Emitted when a workspace row is reordered by drag and drop in the
    /// side panel. `target_index` is the final position after the move and
    /// is clamped to the last slot by the daemon if it exceeds the length.
    ReorderWorkspace {
        id: WorkspaceId,
        target_index: usize,
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
    ///
    /// `workspace` is resolved by the IPC handler from `pane` so the
    /// later [`Self::OpenNotification`] route knows which side-panel
    /// row to bring to the foreground without a second store lookup.
    /// `surface` is the specific tab inside `pane` so suppression can
    /// compare and the click router can switch tabs.
    /// `ack` reports back the [`NotificationId`] of the freshly-pushed
    /// entry, so the IPC handler can later attach the FDO desktop
    /// notification id via [`Self::SetNotificationDesktopId`]. Returns
    /// `None` when the GUI suppressed the notification (source pane +
    /// surface already focused) — the IPC handler interprets this as
    /// "do not fire the desktop toast either".
    AddNotification {
        pane: Option<PaneId>,
        surface: Option<SurfaceId>,
        workspace: Option<WorkspaceId>,
        title: String,
        body: String,
        level: NotificationLevel,
        ack: oneshot::Sender<Option<NotificationId>>,
    },
    /// Tell the GUI store which `org.gtk.Notifications` id was
    /// assigned to a previously-added entry. Used by the bell popover
    /// to ask the daemon to withdraw the toast (and shrink the dock
    /// badge) once the user reads it inside flowmux.
    SetNotificationDesktopId {
        id: NotificationId,
        desktop_id: String,
    },
    /// Ask the daemon to withdraw the supplied `desktop_id`s through
    /// `org.gtk.Notifications.RemoveNotification`. On GNOME this drops
    /// the entry from the message tray (Super+V) **and** decrements
    /// Ubuntu Dock's per-app notification counter, so the dock badge
    /// shrinks in lockstep. The dispatcher coalesces this with the
    /// store-level `mark_*_read` sweep that produced the ids.
    CloseDesktopNotifications { desktop_ids: Vec<String> },
    /// Historic no-op. Earlier flowmux drove the dock badge directly
    /// via `com.canonical.Unity.LauncherEntry::Update`; the badge
    /// counter is now derived by the dock from
    /// `org.gtk.Notifications` per-app entries, so we don't have to
    /// publish anything for it to converge. The variant is kept so
    /// existing dispatch sites compile while the rewrite settles —
    /// remove together with its handler in a follow-up.
    RefreshLauncherBadge,
    /// User clicked a row in the bell popover. Mark the entry read,
    /// activate its workspace (if known), and grab focus on the source
    /// pane (if known). Mirrors cmux's `openNotification → focusTab`.
    OpenNotification { id: NotificationId },
    /// CLI notification management: read the in-process transcript.
    ListNotifications {
        unread_only: bool,
        ack: oneshot::Sender<(Vec<NotificationSummary>, usize)>,
    },
    /// CLI notification management: open one entry and report whether it
    /// existed.
    OpenNotificationWithAck {
        id: NotificationId,
        ack: oneshot::Sender<bool>,
    },
    /// CLI notification management: open the oldest unread entry.
    OpenOldestUnreadNotification { ack: oneshot::Sender<bool> },
    /// CLI notification management: mark one entry as read.
    MarkNotificationRead {
        id: NotificationId,
        ack: oneshot::Sender<bool>,
    },
    /// CLI notification management: clear the transcript.
    ClearNotifications { ack: oneshot::Sender<bool> },
    /// User clicked the trash button on a bell-popover row. Drop the
    /// entry from the in-process transcript, withdraw its FDO toast
    /// (when one exists) and re-publish the dock badge unread count.
    /// Refreshes the popover so the row vanishes immediately.
    DeleteNotification { id: NotificationId },
    /// Cycle to the previous / next workspace in sidebar order.
    FocusWorkspaceDir { dir: WsNav },
    /// Jump straight to the N-th workspace (1-indexed; clamped to
    /// what currently exists).
    FocusWorkspaceAt { idx: u8 },
    /// A side-panel workspace row was clicked or row-activated. The
    /// dispatcher routes it through activate_workspace so GtkStack
    /// visibility, store active_workspace, and first-leaf grab_focus happen
    /// in one flow shared by clicks, Alt+number, and Ctrl+Tab.
    ActivateWorkspace { id: WorkspaceId },
    /// A bottom agent-bar item was clicked. The dispatcher activates the
    /// workspace, switches to that pane-local tab, focuses the pane, and
    /// acknowledges matching notifications through the normal read path.
    OpenAgentBarItem {
        workspace: WorkspaceId,
        pane: PaneId,
        surface: SurfaceId,
    },
    /// A notification was raised on a pane (from terminal OSC signal). Update
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
    /// Open a browser surface "next to" the source pane. cmux's policy
    /// (mirrored here): if the source pane already has a browser leaf
    /// to its right, append a new tab to that pane instead of creating
    /// a new split. Otherwise split the source pane in the requested
    /// direction (typically Vertical = right) and put a fresh browser
    /// pane in the new sibling. `target_pane = None` means "use the
    /// focused pane"; the dispatcher resolves it on the GTK side.
    BrowserOpenSplit {
        target_pane: Option<PaneId>,
        url: String,
        direction: SplitDirection,
        ack: oneshot::Sender<Result<BrowserOpenOutcome, String>>,
    },
    /// Inject a list of cookies into the WebKit cookie manager.
    InjectCookies {
        cookies: Vec<flowmux_cookies::Cookie>,
        ack: oneshot::Sender<Result<usize, String>>,
    },
    /// Open the system file manager at the cwd of the workspace's
    /// currently-focused pane (or, if no pane in `workspace` is focused,
    /// the workspace's first leaf pane's active terminal). Fired by the
    /// sidebar workspace right-click "Show in folder" item.
    ShowFocusedPaneFolder { workspace: WorkspaceId },
    /// Open the system file manager at a specific surface's cwd. Fired
    /// by the pane tab right-click "Show in folder" item. Only meaningful
    /// for terminal surfaces; the caller skips browser tabs before sending.
    ShowSurfaceFolder {
        #[allow(dead_code)]
        pane: PaneId,
        surface: SurfaceId,
    },
    /// Copy a single surface's text identifier to the clipboard — the
    /// cwd for terminal surfaces, the current URL for browser surfaces.
    /// Fired by per-pane and per-tab "Copy path" / "Copy URL"
    /// right-click items. The dispatcher resolves which one based on
    /// the surface kind so the caller does not need to branch.
    CopySurfaceText { pane: PaneId, surface: SurfaceId },
    /// Copy the workspace's currently-focused pane's text identifier
    /// (terminal cwd or browser URL) to the clipboard. Used by the
    /// sidebar workspace right-click "Copy path" item; uses the same
    /// focused-pane → first-leaf-pane → workspace.root_dir resolution
    /// as ShowFocusedPaneFolder.
    CopyFocusedPaneText { workspace: WorkspaceId },
    /// Drop every notification from the in-process transcript and
    /// withdraw their matching desktop toasts in one sweep. Fired by
    /// the "All Clear" button at the top of the bell popover.
    ClearAllNotifications,
    /// An AI agent's live activity in `workspace` changed. Drives the
    /// breathing opacity of the workspace's left color bar.
    /// `activity: None` clears the presence (session end / dead PID).
    /// Fire-and-forget — the sidebar render has no reply.
    SetAgentStatus {
        workspace: WorkspaceId,
        status: Option<AgentStatus>,
    },
    /// Ask the GTK thread whether an agent surface is genuinely visible:
    /// active app window, focused containing pane, and active pane tab.
    QueryAgentSurfaceVisible {
        surface: SurfaceId,
        ack: oneshot::Sender<bool>,
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
