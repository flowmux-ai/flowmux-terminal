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
    /// 사이드 패널 좌하단 옵션 버튼이 눌리면 GTK 측에서 모달
    /// 옵션 다이얼로그를 띄운다. 다이얼로그 자체가 OK / 취소
    /// 결과를 보유 — bridge에는 결과를 돌려주지 않는다.
    ShowOptionsDialog,
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
    /// `from = None` (현재 포커스된 pane이 없을 때) 인 경우 활성 워크스페이스의
    /// 첫 leaf pane에 포커스를 잡아 — 사이드 패널에서 워크스페이스만 클릭한 직후
    /// Alt+화살표를 처음 누른 케이스를 자연스럽게 처리한다.
    FocusDirection {
        from: Option<PaneId>,
        dir: FocusDir,
    },
    /// Open a brand-new terminal surface in the active workspace.
    /// (Reserved for the planned horizontal surface-tab bar; currently
    /// unused since the sidebar shows workspaces, not surfaces.)
    NewSurface { pane: PaneId },
    /// 같은 pane에 빈(about:blank) 탭브라우저를 새 탭으로 추가한다.
    NewBrowserSurface { pane: PaneId },
    /// 터미널 안의 URL을 Ctrl-클릭했을 때 같은 pane에 새 탭브라우저로
    /// URL을 로드해 연다. `pane`은 클릭이 발생한 터미널의 pane id이고
    /// `url`은 trailing punctuation을 정리한 후의 URL 문자열이다.
    OpenUrlInBrowserTab { pane: PaneId, url: String },
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
    /// 같은 pane 안에서 탭(터미널/탭브라우저)을 드래그 앤 드랍으로 좌우
    /// reorder. `target_index`는 이동 후의 최종 위치이며 길이를 넘으면
    /// 끝으로 클램프된다.
    ReorderSurface {
        pane: PaneId,
        surface: SurfaceId,
        target_index: usize,
        ack: oneshot::Sender<Result<(), String>>,
    },
    /// VTE reported a cwd change for a terminal surface.
    TerminalCwdChanged {
        pane: PaneId,
        surface: SurfaceId,
        cwd: PathBuf,
    },
    /// WebKit reported that a browser pane navigated to a new URL.
    /// 다음 실행 시 같은 페이지로 복원되도록 state에 반영한다.
    BrowserUriChanged {
        pane: PaneId,
        surface: SurfaceId,
        url: String,
    },
    /// WebKit reported a page title change. surface의 title_locked가
    /// false인 경우에 한해 탭 라벨과 윈도우 제목을 갱신한다.
    BrowserTitleChanged {
        pane: PaneId,
        surface: SurfaceId,
        title: String,
    },
    /// VTE가 OSC 0/2 (window title)로 받은 타이틀 변화를 알린다.
    /// vi/claude/codex/tmux 같은 프로그램이 셸 안에서 실행되면
    /// 보내며, 사용자가 직접 rename한 surface는 daemon 쪽에서
    /// 무시한다. 빈 문자열은 무시 (셸이 종료되면서 OSC를 비울 때).
    TerminalTitleChanged {
        pane: PaneId,
        surface: SurfaceId,
        title: String,
    },
    /// 윈도우 타이틀을 "flowmux - {focused tab name}"으로 다시 계산.
    /// 포커스 변경 / 탭 활성화 / 탭 라벨 변경 직후에 보낸다.
    RefreshWindowTitle,
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
    /// 사이드 패널 안에서 워크스페이스를 드래그 앤 드랍해 순서를 재배치할 때
    /// 발행한다. `target_index`는 이동 후의 최종 위치이며, 길이를 넘으면
    /// 데몬에서 마지막 슬롯으로 클램프된다.
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
