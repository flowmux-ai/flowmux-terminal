// SPDX-License-Identifier: GPL-3.0-or-later
//! Main application window. Composes header bar + sidebar + content
//! stack and exposes a [`WindowController`] that routes [`GtkCommand`]
//! values from the bridge to widget operations.

use crate::bridge::{
    Bridge, BrowserActionResult, BrowserOp, BrowserOpenOutcome, FocusDir, GtkCommand, WsNav,
};
use crate::keybindings::FocusedPane;
use crate::notifications::{
    NotificationEntry, NotificationStore, RemoveOutcome, SetDesktopIdResult,
};
use crate::theme::ResolvedTheme;
use crate::ui::agent_bar::AgentBar;
use crate::ui::editor_pane::EditorPane;
use crate::ui::file_browser::{FileBrowserPaneState, FileBrowserPanel};
use crate::ui::pane_terminal::PaneCallbacks;
use crate::ui::sidebar::{Sidebar, WorkspaceRowAgentBlock, WorkspaceRowDetails};
use crate::ui::workspace_view::{
    attach_surface_to_pane, build_surface, build_surface_tab_widget, solo_workspace_pane,
    split_pane_incremental, IncrementalSplitOutcome, MovingSurface, PaneRegistry, TornOffSurface,
};
use crate::ui::worktree_panel::WorktreePanel;
use adw::prelude::*;
use flowmux_config::cmux_json::{CmuxJson, CommandTarget, CustomCommand};
use flowmux_core::{
    AgentNotificationVisualFlags, Pane, PaneContent, PaneId, PaneSurface, PlacementStrategy,
    SplitDirection, Surface, SurfaceId, SurfaceKind, Workspace, WorkspaceAgentBlock, WorkspaceId,
};
use flowmux_daemon::StateStore;
use flowmux_ipc::protocol::{BrowserWaitCondition, NotificationSummary};
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

fn notification_summary(entry: NotificationEntry) -> NotificationSummary {
    NotificationSummary {
        id: entry.id,
        title: entry.title,
        body: entry.body,
        level: entry.level,
        created_at: entry.created_at,
        read: entry.read,
        pane: entry.pane,
        surface: entry.surface,
        workspace: entry.workspace,
    }
}

fn should_suppress_notification(
    level: flowmux_core::NotificationLevel,
    source_focused: bool,
) -> bool {
    source_focused
        && !matches!(
            level,
            flowmux_core::NotificationLevel::NeedsInput | flowmux_core::NotificationLevel::Error
        )
}

fn agent_surface_is_visible(
    window_active: bool,
    focused_pane: Option<PaneId>,
    source_pane: Option<PaneId>,
    active_surface: Option<SurfaceId>,
    source_surface: SurfaceId,
) -> bool {
    window_active
        && focused_pane.is_some()
        && focused_pane == source_pane
        && active_surface == Some(source_surface)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandPaletteCommand {
    OpenBrowser,
    RenameTab,
    ReloadConfig,
    OpenUnread,
    Keybinding(flowmux_config::keybindings::ActionId),
    ActivateWorkspace(WorkspaceId),
    FocusPane {
        workspace: WorkspaceId,
        pane: PaneId,
    },
    ActivateSurface {
        workspace: WorkspaceId,
        pane: PaneId,
        surface: SurfaceId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CopyableText {
    kind: &'static str,
    value: String,
    live_terminal_cwd: Option<PathBuf>,
}

impl CopyableText {
    fn live_path(cwd: PathBuf) -> Self {
        Self {
            kind: "path",
            value: cwd.display().to_string(),
            live_terminal_cwd: Some(cwd),
        }
    }

    fn stored_path(cwd: PathBuf) -> Self {
        Self {
            kind: "path",
            value: cwd.display().to_string(),
            live_terminal_cwd: None,
        }
    }

    fn url(url: String) -> Option<Self> {
        (!url.is_empty()).then_some(Self {
            kind: "URL",
            value: url,
            live_terminal_cwd: None,
        })
    }
}

fn command_palette_commands() -> &'static [CommandPaletteCommand] {
    &[
        CommandPaletteCommand::OpenBrowser,
        CommandPaletteCommand::RenameTab,
        CommandPaletteCommand::ReloadConfig,
        CommandPaletteCommand::OpenUnread,
    ]
}

fn command_palette_label(command: CommandPaletteCommand) -> &'static str {
    match command {
        CommandPaletteCommand::OpenBrowser => "Open browser",
        CommandPaletteCommand::RenameTab => "Rename tab",
        CommandPaletteCommand::ReloadConfig => "Reload config",
        CommandPaletteCommand::OpenUnread => "Open unread notification",
        CommandPaletteCommand::Keybinding(action) => action.label(),
        CommandPaletteCommand::ActivateWorkspace(_) => "Go to workspace",
        CommandPaletteCommand::FocusPane { .. } => "Focus pane",
        CommandPaletteCommand::ActivateSurface { .. } => "Go to tab",
    }
}

fn stored_surface_copy_text_from_workspace(
    ws: &Workspace,
    pane: PaneId,
    surface: SurfaceId,
) -> Option<CopyableText> {
    ws.surfaces
        .iter()
        .find_map(|surface_root| surface_root.root_pane.find_surface(pane, surface))
        .and_then(|pane_surface| match pane_surface.kind {
            SurfaceKind::Terminal { cwd: Some(cwd), .. } => Some(CopyableText::stored_path(cwd)),
            SurfaceKind::Terminal { cwd: None, .. } => None,
            SurfaceKind::Browser { initial_url } => initial_url.and_then(CopyableText::url),
            SurfaceKind::Editor { workspace_root, .. } => {
                Some(CopyableText::stored_path(workspace_root))
            }
        })
}

fn stored_terminal_cwd_from_workspace(
    ws: &Workspace,
    pane: PaneId,
    surface: SurfaceId,
) -> Option<PathBuf> {
    ws.surfaces
        .iter()
        .find_map(|surface_root| surface_root.root_pane.find_surface(pane, surface))
        .and_then(|pane_surface| match pane_surface.kind {
            SurfaceKind::Terminal { cwd, .. } => cwd,
            SurfaceKind::Browser { .. } | SurfaceKind::Editor { .. } => None,
        })
}

fn active_surface_from_workspace(ws: &Workspace, pane: PaneId) -> Option<SurfaceId> {
    ws.surfaces
        .iter()
        .find_map(|surface_root| surface_root.root_pane.active_surface_id(pane))
}

fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'/' | b':' | b'='))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

fn forget_saved_agent_sessions(surfaces: &[SurfaceId]) {
    let Some(store) = flowmux_state::default_agent_session_store() else {
        return;
    };
    for surface in surfaces {
        if let Err(error) = store.forget_surface(*surface) {
            tracing::warn!(%surface, %error, "failed to forget closed agent session");
        }
    }
}

fn custom_command_cwd(base_dir: &std::path::Path, command: &CustomCommand) -> std::path::PathBuf {
    match command.cwd.as_deref() {
        Some(cwd) => {
            let path = std::path::PathBuf::from(cwd);
            if path.is_absolute() {
                path
            } else {
                base_dir.join(path)
            }
        }
        None => base_dir.to_path_buf(),
    }
}

fn custom_command_shell_line(
    base_dir: &std::path::Path,
    env: &std::collections::BTreeMap<String, String>,
    command: &CustomCommand,
) -> Option<String> {
    if command.run.is_empty() {
        return None;
    }

    let mut parts = vec![
        "cd".to_string(),
        shell_quote(&custom_command_cwd(base_dir, command).to_string_lossy()),
        "&&".to_string(),
    ];
    if !env.is_empty() {
        parts.push("env".to_string());
        for (key, value) in env {
            parts.push(shell_quote(&format!("{key}={value}")));
        }
    }
    parts.extend(command.run.iter().map(|arg| shell_quote(arg)));
    Some(parts.join(" "))
}

/// Agent bar state: the live-agent overview widget plus the set of surfaces
/// currently flagged for attention. Grouped out of two flat `WindowController`
/// fields.
#[derive(Clone)]
struct AgentBarState {
    /// The agent bar widget shown above the content area.
    bar: AgentBar,
    /// Surfaces flagged for attention (e.g. an agent awaiting input).
    attentions: Rc<RefCell<HashSet<SurfaceId>>>,
}

/// Cohesive file-browser state, grouped out of five flat `WindowController`
/// fields. Every terminal pane can reveal an in-pane file browser; this holds
/// the shared panel plus the per-pane and focus-restore bookkeeping.
#[derive(Clone)]
struct FileBrowserState {
    /// Pane that had focus when the browser opened, so focus can be restored on close.
    source_pane: FocusedPane,
    /// Whether the file browser currently owns keyboard focus.
    active: Rc<Cell<bool>>,
    /// Per-pane saved browser state (expanded dirs, selection) keyed by pane.
    pane_states: Rc<RefCell<HashMap<PaneId, FileBrowserPaneState>>>,
    /// Horizontal `gtk::Paned` splitting the content area and the browser panel.
    split: gtk::Paned,
    /// The shared file-browser widget.
    panel: FileBrowserPanel,
}

#[derive(Clone)]
struct WorktreePanelState {
    source_pane: FocusedPane,
    source_directory: Rc<RefCell<Option<PathBuf>>>,
    loading: Rc<Cell<bool>>,
    active: Rc<Cell<bool>>,
    generation: Rc<Cell<u64>>,
    repository_root: Rc<RefCell<Option<PathBuf>>>,
    removals_in_progress: Rc<RefCell<HashSet<PathBuf>>>,
    tokio_handle: Option<tokio::runtime::Handle>,
    split: gtk::Paned,
    panel: WorktreePanel,
}

const PANE_ZOOM_PAGE: &str = "__pane_zoom";

struct ActivePaneZoom {
    pane: PaneId,
    workspace: WorkspaceId,
    frame: gtk::Widget,
    origin: PaneZoomOrigin,
}

enum PaneZoomOrigin {
    PanedStart { paned: gtk::Paned, position: i32 },
    PanedEnd { paned: gtk::Paned, position: i32 },
    WorkspaceRoot,
}

#[derive(Clone, Default)]
struct PaneZoomState {
    active: Rc<RefCell<Option<ActivePaneZoom>>>,
}

#[derive(Clone, Default)]
struct WindowCloseState {
    approved: Rc<Cell<bool>>,
    prompting: Rc<Cell<bool>>,
}

fn dirty_editor_labels(editors: &[EditorPane]) -> Vec<String> {
    let mut labels = Vec::new();
    let mut seen = HashSet::new();
    for editor in editors {
        for path in editor.dirty_document_paths() {
            let label = path
                .strip_prefix(editor.workspace_root())
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            if seen.insert(label.clone()) {
                labels.push(label);
            }
        }
    }
    labels
}

fn dirty_editor_dialog_body(labels: &[String]) -> String {
    if labels.len() == 1 {
        return format!("“{}” has unsaved changes.", labels[0]);
    }
    let mut body = format!("{} files have unsaved changes.\n\n", labels.len());
    for label in labels.iter().take(8) {
        body.push_str(&format!("• {label}\n"));
    }
    if labels.len() > 8 {
        body.push_str(&format!("• … and {} more\n", labels.len() - 8));
    }
    body.pop();
    body
}

async fn show_editor_save_error(parent: &adw::ApplicationWindow, error: &str) {
    let dialog = adw::AlertDialog::new(Some("Could not save changes"), Some(error));
    dialog.add_response("ok", "OK");
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("ok");

    let (tx, rx) = oneshot::channel::<()>();
    let tx_cell: Rc<Cell<Option<oneshot::Sender<()>>>> = Rc::new(Cell::new(Some(tx)));
    dialog.connect_response(None, move |dialog, _| {
        if let Some(tx) = tx_cell.take() {
            let _ = tx.send(());
        }
        dialog.close();
    });
    let _native_view_suspend =
        crate::ui::browser_pane::suspend_native_browser_views_for_window(parent.upcast_ref());
    dialog.present(Some(parent));
    let _ = rx.await;
}

fn build_dirty_editor_dialog(labels: &[String]) -> (adw::AlertDialog, oneshot::Receiver<String>) {
    let dialog = adw::AlertDialog::new(
        Some("Save changes before closing?"),
        Some(&dirty_editor_dialog_body(labels)),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("discard", "Discard");
    dialog.add_response("save", "Save");
    dialog.set_default_response(Some("save"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);

    let (tx, rx) = oneshot::channel::<String>();
    let tx_cell: Rc<Cell<Option<oneshot::Sender<String>>>> = Rc::new(Cell::new(Some(tx)));
    dialog.connect_response(None, move |dialog, response| {
        if let Some(tx) = tx_cell.take() {
            let _ = tx.send(response.to_string());
        }
        dialog.close();
    });
    (dialog, rx)
}

async fn confirm_dirty_editor_close(
    parent: &adw::ApplicationWindow,
    editors: Vec<EditorPane>,
) -> bool {
    let labels = dirty_editor_labels(&editors);
    if labels.is_empty() {
        return true;
    }

    let (dialog, rx) = build_dirty_editor_dialog(&labels);
    let _native_view_suspend =
        crate::ui::browser_pane::suspend_native_browser_views_for_window(parent.upcast_ref());
    dialog.present(Some(parent));

    match rx.await.as_deref() {
        Ok("discard") => {
            for editor in editors {
                editor.discard_all_dirty();
            }
            true
        }
        Ok("save") => {
            for editor in editors {
                if let Err(error) = editor.save_all_dirty() {
                    show_editor_save_error(parent, &error).await;
                    return false;
                }
            }
            true
        }
        _ => false,
    }
}

#[derive(Clone)]
pub struct WindowController {
    pub window: adw::ApplicationWindow,
    pub focused_pane: FocusedPane,
    workspace_presenter: workspace_presenter::WorkspacePresenter,
    /// Outermost `gtk::Paned` separating the side panel and content area.
    /// Its position is saved to the store on exit and restored on next launch.
    sidebar_split: gtk::Paned,
    worktrees: WorktreePanelState,
    file_browser: FileBrowserState,
    agent_bar: AgentBarState,
    pane_zoom: PaneZoomState,
    callbacks: PaneCallbacks,
    bridge: Bridge,
    /// Currently applied visual theme. Swapped in place by
    /// [`WindowController::apply_runtime_theme`] when the Theme tab picks
    /// a preset; new panes read the current value at creation time.
    theme: Rc<RefCell<Arc<ResolvedTheme>>>,
    notifications: notification_coordinator::NotificationCoordinator,
    options: Rc<RefCell<flowmux_config::options::Options>>,
    /// Global CssProvider. When the options dialog changes focus border color
    /// or opacity, reload CSS into this same instance so every pane updates immediately.
    css_provider: gtk::CssProvider,
    /// Small in-window toast shown when terminal text is copied.
    clipboard_toast: ClipboardToast,
    /// MRU pane list per workspace, with the front as most recently focused and
    /// capped at 3 panes. The side-panel label comes from the head pane's active
    /// surface title, and subtitles come from the active terminal cwd paths for
    /// the head through third panes, shortened to the last 3 folders with a
    /// "..." prefix. Updated on focus moves within a workspace.
    focus_mru: Rc<RefCell<HashMap<WorkspaceId, std::collections::VecDeque<PaneId>>>>,
    /// Command-palette entry keys ordered most-recent-first for this window.
    /// Kept in memory only; project and workspace entries can change between launches.
    palette_mru: Rc<RefCell<std::collections::VecDeque<String>>>,
    /// Monotonic id for process-tree sweeps. A slower older worker result is
    /// discarded instead of overwriting a newer agent observation.
    agent_poll_generation: Rc<Cell<u64>>,
    cwd_poll_generation: Rc<Cell<u64>>,
    window_close: WindowCloseState,
}

fn wsl_resize_handles_enabled() -> bool {
    !crate::platform::env_flag_enabled("FLOWMUX_NO_WSL_RESIZE_HANDLES")
        && (crate::platform::running_under_wsl()
            || crate::platform::env_flag_enabled("FLOWMUX_WSL_RESIZE_HANDLES"))
}

fn restore_paned_position(paned: gtk::Paned, position: i32) {
    paned.set_position(position);
    glib::idle_add_local_once(move || paned.set_position(position));
}

fn detach_pane_for_zoom(frame: &gtk::Widget, stack: &gtk::Stack) -> Option<PaneZoomOrigin> {
    let parent = frame.parent()?;
    let origin = if let Ok(paned) = parent.clone().downcast::<gtk::Paned>() {
        let position = paned.position();
        if paned.start_child().as_ref() == Some(frame) {
            paned.set_start_child(gtk::Widget::NONE);
            PaneZoomOrigin::PanedStart { paned, position }
        } else if paned.end_child().as_ref() == Some(frame) {
            paned.set_end_child(gtk::Widget::NONE);
            PaneZoomOrigin::PanedEnd { paned, position }
        } else {
            return None;
        }
    } else if parent == stack.clone().upcast::<gtk::Widget>() {
        return Some(PaneZoomOrigin::WorkspaceRoot);
    } else {
        return None;
    };
    stack.add_named(frame, Some(PANE_ZOOM_PAGE));
    stack.set_visible_child_name(PANE_ZOOM_PAGE);
    Some(origin)
}

fn restore_pane_from_zoom(frame: &gtk::Widget, stack: &gtk::Stack, origin: PaneZoomOrigin) {
    match origin {
        PaneZoomOrigin::PanedStart { paned, position } => {
            stack.remove(frame);
            paned.set_start_child(Some(frame));
            restore_paned_position(paned, position);
        }
        PaneZoomOrigin::PanedEnd { paned, position } => {
            stack.remove(frame);
            paned.set_end_child(Some(frame));
            restore_paned_position(paned, position);
        }
        PaneZoomOrigin::WorkspaceRoot => {}
    }
}

fn set_window_content(window: &adw::ApplicationWindow, content: &impl IsA<gtk::Widget>) {
    if wsl_resize_handles_enabled() {
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(content));
        window.set_content(Some(&overlay));
        install_window_resize_handles(window, &overlay);
    } else {
        window.set_content(Some(content));
    }
}

/// Build the desktop workbench shell as two aligned libadwaita header bars.
/// Window controls stay native while the sidebar actions occupy the space that
/// used to be a second custom toolbar below an otherwise empty title bar.
fn build_split_window_shell(
    sidebar: &Sidebar,
    content: &impl IsA<gtk::Widget>,
    sidebar_position: i32,
) -> gtk::Paned {
    let sidebar_view = adw::ToolbarView::new();
    sidebar_view.add_css_class("flowmux-sidebar-shell");
    sidebar_view.add_top_bar(&sidebar.header);
    sidebar_view.set_content(Some(&sidebar.root));
    sidebar_view.set_size_request(160, -1);

    let content_header = adw::HeaderBar::new();
    content_header.set_show_start_title_buttons(false);
    content_header.set_show_end_title_buttons(true);
    let content_view = adw::ToolbarView::new();
    content_view.add_top_bar(&content_header);
    content_view.set_content(Some(content));

    let split = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&sidebar_view)
        .end_child(&content_view)
        .resize_start_child(false)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(sidebar_position)
        .build();
    split.add_css_class("flowmux-window-split");
    split
}

fn install_window_resize_handles(window: &adw::ApplicationWindow, overlay: &gtk::Overlay) {
    const EDGE: i32 = 14;
    const CORNER: i32 = 30;

    add_resize_handle(
        window,
        overlay,
        gtk::gdk::SurfaceEdge::South,
        "s-resize",
        gtk::Align::Fill,
        gtk::Align::End,
        -1,
        EDGE,
    );
    add_resize_handle(
        window,
        overlay,
        gtk::gdk::SurfaceEdge::West,
        "w-resize",
        gtk::Align::Start,
        gtk::Align::Fill,
        EDGE,
        -1,
    );
    add_resize_handle(
        window,
        overlay,
        gtk::gdk::SurfaceEdge::East,
        "e-resize",
        gtk::Align::End,
        gtk::Align::Fill,
        EDGE,
        -1,
    );

    // Keep the CSD header bar clear: WSLg can miss libadwaita's resize
    // hit-test, but covering the top edge also steals the window controls.
    // Bottom corners still provide diagonal resize without blocking Close.
    add_resize_handle(
        window,
        overlay,
        gtk::gdk::SurfaceEdge::SouthWest,
        "sw-resize",
        gtk::Align::Start,
        gtk::Align::End,
        CORNER,
        CORNER,
    );
    add_resize_handle(
        window,
        overlay,
        gtk::gdk::SurfaceEdge::SouthEast,
        "se-resize",
        gtk::Align::End,
        gtk::Align::End,
        CORNER,
        CORNER,
    );
}

fn add_resize_handle(
    window: &adw::ApplicationWindow,
    overlay: &gtk::Overlay,
    edge: gtk::gdk::SurfaceEdge,
    cursor: &str,
    halign: gtk::Align,
    valign: gtk::Align,
    width: i32,
    height: i32,
) {
    const TITLEBAR_CONTROL_SAFE_TOP: i32 = 48;

    let handle = gtk::Box::new(gtk::Orientation::Vertical, 0);
    handle.set_halign(halign);
    handle.set_valign(valign);
    handle.set_can_focus(false);
    handle.set_can_target(true);
    handle.set_cursor_from_name(Some(cursor));
    if matches!(
        edge,
        gtk::gdk::SurfaceEdge::East | gtk::gdk::SurfaceEdge::West
    ) {
        handle.set_margin_top(TITLEBAR_CONTROL_SAFE_TOP);
    }
    if width > 0 {
        handle.set_width_request(width);
    } else {
        handle.set_hexpand(true);
    }
    if height > 0 {
        handle.set_height_request(height);
    } else {
        handle.set_vexpand(true);
    }

    let gesture = gtk::GestureClick::new();
    gesture.set_button(gtk::gdk::BUTTON_PRIMARY);
    let window_weak = window.downgrade();
    let handle_for_gesture = handle.clone();
    gesture.connect_pressed(move |gesture, _n_press, x, y| {
        let Some(window) = window_weak.upgrade() else {
            return;
        };
        let Some(surface) = window.surface() else {
            return;
        };
        let Ok(toplevel) = surface.downcast::<gtk::gdk::Toplevel>() else {
            return;
        };
        let event = gesture.current_event();
        let device = event.as_ref().and_then(|event| event.device());
        let surface_point = handle_for_gesture
            .compute_point(&window, &gtk::graphene::Point::new(x as f32, y as f32))
            .unwrap_or_else(|| gtk::graphene::Point::new(x as f32, y as f32));
        let timestamp = event
            .as_ref()
            .map(|event| event.time())
            .unwrap_or_else(|| gesture.current_event_time());

        toplevel.begin_resize(
            edge,
            device.as_ref(),
            gtk::gdk::BUTTON_PRIMARY as i32,
            surface_point.x() as f64,
            surface_point.y() as f64,
            timestamp,
        );
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    handle.add_controller(gesture);

    overlay.add_overlay(&handle);
}

mod agent_bar;
mod browser_commands;
mod command_palette;
mod file_browser;
mod notification_commands;
mod notification_coordinator;
mod pane_callbacks;
mod pane_commands;
mod polling;
mod surface_ops;
mod window_chrome_commands;
mod workspace_commands;
mod workspace_presenter;
mod worktrees;

impl std::ops::Deref for WindowController {
    type Target = workspace_presenter::WorkspacePresenter;

    fn deref(&self) -> &Self::Target {
        &self.workspace_presenter
    }
}

impl WindowController {
    /// Snapshot of the currently applied theme.
    pub(super) fn current_theme(&self) -> Arc<ResolvedTheme> {
        self.theme.borrow().clone()
    }

    fn zoomed_pane(&self) -> Option<PaneId> {
        self.pane_zoom
            .active
            .borrow()
            .as_ref()
            .map(|zoom| zoom.pane)
    }

    fn toggle_pane_zoom(&self, pane: PaneId) {
        if self.zoomed_pane() == Some(pane) {
            self.clear_pane_zoom();
            self.focus_pane(pane);
            return;
        }
        self.clear_pane_zoom();

        let (frame, workspace) = {
            let registry = self.pane_registry.borrow();
            let Some(frame) = registry.pane_frame(pane) else {
                return;
            };
            let Some(workspace) = registry.workspace_of_pane(pane) else {
                return;
            };
            (frame, workspace)
        };
        let Some(origin) = detach_pane_for_zoom(&frame, &self.stack) else {
            tracing::warn!(%pane, "pane zoom: unsupported parent");
            return;
        };
        self.pane_registry.borrow_mut().set_pane_zoomed(pane, true);
        *self.pane_zoom.active.borrow_mut() = Some(ActivePaneZoom {
            pane,
            workspace,
            frame,
            origin,
        });
        self.focus_pane(pane);
    }

    fn clear_pane_zoom(&self) -> Option<PaneId> {
        let active = self.pane_zoom.active.borrow_mut().take()?;
        self.pane_registry
            .borrow_mut()
            .set_pane_zoomed(active.pane, false);

        restore_pane_from_zoom(&active.frame, &self.stack, active.origin);
        if self.surfaces.borrow().contains_key(&active.workspace) {
            self.stack
                .set_visible_child_name(&active.workspace.to_string());
        }
        Some(active.pane)
    }

    /// Re-resolve the theme from `opts` and repaint everything that
    /// depends on it: every open terminal (colors + font), the global CSS
    /// provider, and libadwaita's dark/light color scheme. New panes pick
    /// up the swapped theme automatically. Used both for the Theme tab's
    /// live preview and for the final apply on OK.
    pub(super) fn apply_runtime_theme(&self, opts: &flowmux_config::options::Options) {
        let resolved = Arc::new(ResolvedTheme::resolve(opts));
        *self.theme.borrow_mut() = resolved.clone();

        let style = adw::StyleManager::default();
        style.set_color_scheme(if resolved.is_dark() {
            adw::ColorScheme::ForceDark
        } else {
            adw::ColorScheme::ForceLight
        });

        let font = resolved.font_with_overrides(opts.font_family.as_deref(), opts.font_size);
        let registry = self.pane_registry.borrow();
        for terminal in registry.terminals.values() {
            resolved.apply_to_ghostty(terminal);
            terminal.set_font(&font);
        }
        let editor_appearance = resolved.editor_appearance(opts);
        for editor in registry.editors.values() {
            editor.apply_appearance(editor_appearance.clone());
        }
        drop(registry);

        self.css_provider.load_from_string(&resolved.css(
            opts.focus_border_color_or_default(),
            opts.focus_border_alpha(),
        ));
    }

    pub fn new(
        app: &adw::Application,
        store: StateStore,
        theme: Arc<ResolvedTheme>,
        bridge: Bridge,
        css_provider: gtk::CssProvider,
        tokio_handle: Option<tokio::runtime::Handle>,
    ) -> Self {
        let focused_pane: FocusedPane = Rc::new(Cell::new(None));
        let file_browser_source_pane: FocusedPane = Rc::new(Cell::new(None));
        let file_browser_active = Rc::new(Cell::new(false));
        let file_browser_pane_states = Rc::new(RefCell::new(HashMap::new()));
        let worktree_source_pane: FocusedPane = Rc::new(Cell::new(None));
        let worktree_source_directory = Rc::new(RefCell::new(None));
        let worktree_loading = Rc::new(Cell::new(false));
        let worktree_active = Rc::new(Cell::new(false));
        let worktree_generation = Rc::new(Cell::new(0));
        let worktree_repository_root = Rc::new(RefCell::new(None));
        let worktree_removals_in_progress = Rc::new(RefCell::new(HashSet::new()));
        let worktree_tokio_handle = tokio_handle.clone();
        let notifications = NotificationStore::new();
        let stack = gtk::Stack::new();
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.set_hexpand(true);
        stack.set_vexpand(true);

        let surfaces: Rc<RefCell<HashMap<WorkspaceId, gtk::Widget>>> =
            Rc::new(RefCell::new(HashMap::new()));

        // Side-panel row click handler. Delegate through bridge::ActivateWorkspace
        // so clicks use the same activate_workspace path as Alt+number/Ctrl+Tab.
        // This prevents focused_pane from still pointing at the previous
        // workspace and leaking Alt+arrow focus to another workspace. The
        // dispatcher handles GtkStack visibility, active workspace state, and
        // first-leaf grab_focus in one flow.
        let bridge_for_select = bridge.clone();
        let on_select = move |id: WorkspaceId| {
            let bridge = bridge_for_select.clone();
            glib::MainContext::default().spawn_local(async move {
                let _ = bridge.tx.send(GtkCommand::ActivateWorkspace { id }).await;
            });
        };
        let bridge_for_close = bridge.clone();
        let on_close = move |id: WorkspaceId| {
            let bridge = bridge_for_close.clone();
            glib::MainContext::default().spawn_local(async move {
                let (tx, rx) = oneshot::channel();
                let _ = bridge
                    .tx
                    .send(GtkCommand::RemoveWorkspace {
                        id,
                        confirm: true,
                        ack: tx,
                    })
                    .await;
                let _ = rx.await;
            });
        };
        let sidebar = Sidebar::new(
            on_select,
            on_close,
            bridge.clone(),
            notifications.clone(),
            tokio_handle.clone(),
        );
        let agent_bar = AgentBar::new(bridge.clone());
        let agent_bar_attentions = Rc::new(RefCell::new(HashSet::new()));

        let pane_registry: Rc<RefCell<PaneRegistry>> =
            Rc::new(RefCell::new(PaneRegistry::default()));
        let initial_options = flowmux_config::options::load();
        tracing::info!(
            zoom_percent = initial_options.zoom_percent,
            engine = ?initial_options.default_browser_engine,
            "options loaded"
        );
        let options = Rc::new(RefCell::new(initial_options));
        let (tab_drag_drop_seen, tab_drag_drop_committed) = sidebar.tab_drag_drop_state();
        let callbacks = pane_callbacks::PaneCallbackRouter::new(
            focused_pane.clone(),
            bridge.clone(),
            options.clone(),
            pane_registry.clone(),
            sidebar.workspace_titles(),
            tab_drag_drop_seen,
            tab_drag_drop_committed,
        )
        .into_callbacks();

        let content_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content_box.set_hexpand(true);
        content_box.set_vexpand(true);
        content_box.append(&stack);
        content_box.append(&agent_bar.root);

        let file_browser = FileBrowserPanel::new();
        #[cfg(target_os = "macos")]
        {
            let focused_pane = focused_pane.clone();
            let pane_registry = pane_registry.clone();
            file_browser.set_keyboard_input_guard(move || {
                let Some(pane) = focused_pane.get() else {
                    return true;
                };
                let registry = pane_registry.borrow();
                let editor_has_native_focus = registry
                    .active_editor(pane)
                    .is_some_and(|editor| editor.has_native_focus());
                crate::ui::file_browser::file_browser_accepts_keyboard_input(
                    editor_has_native_focus,
                )
            });
        }
        let file_browser_for_close = file_browser.clone();
        let file_browser_active_for_close = file_browser_active.clone();
        let file_browser_source_for_close = file_browser_source_pane.clone();
        let file_browser_states_for_close = file_browser_pane_states.clone();
        file_browser.connect_close(move || {
            if let Some(pane) = file_browser_source_for_close.get() {
                file_browser_states_for_close
                    .borrow_mut()
                    .insert(pane, file_browser_for_close.pane_state());
            }
            file_browser_active_for_close.set(false);
            file_browser_for_close.hide();
        });
        let file_browser_active_for_focus = file_browser_active.clone();
        file_browser.connect_focus_changed(move |focused| {
            // file_browser_active tracks whether the browser actually holds keyboard
            // focus (not merely whether the panel is open), so Alt+arrow can tell
            // "escape the browser" from "enter the browser".
            file_browser_active_for_focus.set(focused);
        });
        let file_browser_bridge = bridge.clone();
        file_browser.connect_focus_out(move |dir| {
            let bridge = file_browser_bridge.clone();
            glib::MainContext::default().spawn_local(async move {
                let _ = bridge
                    .tx
                    .send(GtkCommand::FileBrowserFocusOut { dir })
                    .await;
            });
        });
        let file_browser_bridge = bridge.clone();
        file_browser.connect_escape(move || {
            let bridge = file_browser_bridge.clone();
            glib::MainContext::default().spawn_local(async move {
                let _ = bridge
                    .tx
                    .send(GtkCommand::FileBrowserCloseAndRestoreFocus)
                    .await;
            });
        });
        let file_browser_bridge = bridge.clone();
        let file_browser_source_for_open = file_browser_source_pane.clone();
        file_browser.connect_open_file(move |path| {
            let bridge = file_browser_bridge.clone();
            let source_pane = file_browser_source_for_open.get();
            glib::MainContext::default().spawn_local(async move {
                let _ = bridge
                    .tx
                    .send(GtkCommand::OpenFileInEditor { path, source_pane })
                    .await;
            });
        });

        file_browser.widget().set_vexpand(true);

        let worktree_panel = WorktreePanel::new();
        let worktree_active_for_focus = worktree_active.clone();
        worktree_panel.connect_focus_changed(move |focused| {
            worktree_active_for_focus.set(focused);
        });
        let worktree_bridge = bridge.clone();
        worktree_panel.connect_focus_out(move |dir| {
            let bridge = worktree_bridge.clone();
            glib::MainContext::default().spawn_local(async move {
                let _ = bridge
                    .tx
                    .send(GtkCommand::WorktreePanelFocusOut { dir })
                    .await;
            });
        });
        let worktree_bridge = bridge.clone();
        worktree_panel.connect_close(move || {
            let bridge = worktree_bridge.clone();
            glib::MainContext::default().spawn_local(async move {
                let _ = bridge
                    .tx
                    .send(GtkCommand::WorktreePanelCloseAndRestoreFocus)
                    .await;
            });
        });
        let worktree_bridge = bridge.clone();
        worktree_panel.connect_refresh(move || {
            let bridge = worktree_bridge.clone();
            glib::MainContext::default().spawn_local(async move {
                let _ = bridge.tx.send(GtkCommand::RefreshWorktrees).await;
            });
        });
        let worktree_bridge = bridge.clone();
        worktree_panel.connect_info(move |path| {
            let bridge = worktree_bridge.clone();
            glib::MainContext::default().spawn_local(async move {
                let _ = bridge.tx.send(GtkCommand::ShowWorktreeInfo { path }).await;
            });
        });
        let worktree_bridge = bridge.clone();
        worktree_panel.connect_remove(move |path| {
            let bridge = worktree_bridge.clone();
            glib::MainContext::default().spawn_local(async move {
                let _ = bridge.tx.send(GtkCommand::RemoveWorktree { path }).await;
            });
        });
        worktree_panel.widget().set_vexpand(true);

        let worktree_split = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&content_box)
            .end_child(worktree_panel.widget())
            .resize_start_child(true)
            .resize_end_child(false)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .position(680)
            .build();

        let file_browser_split = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&worktree_split)
            .end_child(file_browser.widget())
            .resize_start_child(true)
            .resize_end_child(false)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .position(720)
            .build();

        // Keep the resizable sidebar, but make both sides native toolbar
        // surfaces so their headers form one aligned Ubuntu/GNOME title row.
        let stored_sidebar_pos = store.sidebar_position_blocking().unwrap_or(260);
        let split = build_split_window_shell(&sidebar, &file_browser_split, stored_sidebar_pos);

        let content_overlay = gtk::Overlay::new();
        content_overlay.set_child(Some(&split));
        let clipboard_toast = ClipboardToast::new();
        content_overlay.add_overlay(clipboard_toast.widget());

        // Restore saved window size/maximized state, otherwise default to 1280x800.
        let stored_window = store.window_layout_blocking();
        let (default_w, default_h, was_maximized) = match &stored_window {
            Some(layout) => (
                layout.width.max(320),
                layout.height.max(240),
                layout.maximized,
            ),
            None => (1280, 800, false),
        };
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(default_w)
            .default_height(default_h)
            .icon_name(crate::APP_ID)
            .title("flowmux")
            .build();
        set_window_content(&window, &content_overlay);
        if was_maximized {
            window.maximize();
        }

        register_workspace_actions(&window, &store, &bridge);

        let controller = Self {
            window,
            focused_pane,
            workspace_presenter: workspace_presenter::WorkspacePresenter::new(
                store,
                sidebar,
                stack,
                surfaces,
                pane_registry,
            ),
            sidebar_split: split,
            worktrees: WorktreePanelState {
                source_pane: worktree_source_pane,
                source_directory: worktree_source_directory,
                loading: worktree_loading,
                active: worktree_active,
                generation: worktree_generation,
                repository_root: worktree_repository_root,
                removals_in_progress: worktree_removals_in_progress,
                tokio_handle: worktree_tokio_handle,
                split: worktree_split,
                panel: worktree_panel,
            },
            file_browser: FileBrowserState {
                source_pane: file_browser_source_pane,
                active: file_browser_active,
                pane_states: file_browser_pane_states,
                split: file_browser_split,
                panel: file_browser,
            },
            agent_bar: AgentBarState {
                bar: agent_bar,
                attentions: agent_bar_attentions,
            },
            pane_zoom: PaneZoomState::default(),
            callbacks,
            bridge,
            theme: Rc::new(RefCell::new(theme)),
            notifications: notification_coordinator::NotificationCoordinator::new(
                notifications,
                tokio_handle,
            ),
            options,
            css_provider,
            clipboard_toast,
            focus_mru: Rc::new(RefCell::new(HashMap::new())),
            palette_mru: Rc::new(RefCell::new(std::collections::VecDeque::new())),
            agent_poll_generation: Rc::new(Cell::new(0)),
            cwd_poll_generation: Rc::new(Cell::new(0)),
            window_close: WindowCloseState::default(),
        };
        controller.install_state_flush_on_close();
        controller.install_cwd_polling_fallback();
        controller.install_editor_session_persistence();
        controller.install_scrollback_persistence();
        controller.install_agent_process_polling();
        controller
    }

    #[cfg(test)]
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    fn right_tool_order_for_test(&self) -> [&'static str; 3] {
        let sidebar_view = self
            .sidebar_split
            .start_child()
            .expect("sidebar shell missing")
            .downcast::<adw::ToolbarView>()
            .expect("sidebar must use a native toolbar shell");
        let content_view = self
            .sidebar_split
            .end_child()
            .expect("content shell missing")
            .downcast::<adw::ToolbarView>()
            .expect("content must use a native toolbar shell");
        assert!(sidebar_view.content().is_some());
        assert_eq!(
            content_view.content(),
            Some(self.file_browser.split.clone().upcast())
        );
        assert_eq!(
            self.file_browser.split.start_child(),
            Some(self.worktrees.split.clone().upcast())
        );
        assert_eq!(
            self.file_browser.split.end_child(),
            Some(self.file_browser.panel.widget().clone().upcast())
        );
        assert!(self.worktrees.split.start_child().is_some());
        assert_eq!(
            self.worktrees.split.end_child(),
            Some(self.worktrees.panel.widget().clone().upcast())
        );
        ["content", "worktrees", "files"]
    }

    /// Replace the lazily-initialized notifier cell with one shared
    /// with `DaemonHandler`. Must be called before the controller is
    /// cloned (clones capture the current `Arc`), otherwise the GUI
    /// keeps issuing `RemoveNotification` on a different
    /// `Connection::session()` than the `AddNotification` came from
    /// and gnome-shell — which keys by `(sender, app_id)` — never
    /// drops the matching entry.
    pub fn use_shared_notifier(
        &mut self,
        handle: Arc<tokio::sync::Mutex<Option<flowmux_notify::DesktopNotifier>>>,
    ) {
        self.notifications.use_shared_notifier(handle);
    }

    pub fn show_status_when_empty(&self) {
        if self.surfaces.borrow().is_empty() {
            if self.stack.child_by_name("__empty").is_none() {
                let status = adw::StatusPage::builder()
                    .icon_name("utilities-terminal-symbolic")
                    .title("FlowMux")
                    .description("No workspaces yet")
                    .build();
                self.stack.add_named(&status, Some("__empty"));
            }
            self.stack.set_visible_child_name("__empty");
            self.focused_pane.set(None);
        }
    }

    pub fn render_workspace(&self, ws: &Workspace) {
        self.render_workspace_with_activation(ws, true);
    }

    fn render_workspace_with_activation(&self, ws: &Workspace, activate: bool) {
        if activate {
            self.clear_pane_zoom();
        }
        self.sidebar.upsert(ws);
        let mut surfaces = self.surfaces.borrow_mut();
        if surfaces.contains_key(&ws.id) {
            return;
        }
        let widget = self.build_workspace_widget(ws);
        let name = ws.id.to_string();
        self.stack.add_named(&widget, Some(&name));
        surfaces.insert(ws.id, widget);
        if activate {
            self.stack.set_visible_child_name(&name);
        }
        drop(surfaces);
        self.refresh_workspace_solo(ws);
        if activate {
            self.sidebar.select_workspace(ws.id);
            self.focus_first_leaf_of(ws);
        }
    }

    pub fn rerender_workspace(&self, ws: &Workspace) {
        self.clear_pane_zoom();
        self.sidebar.upsert(ws);
        let name = ws.id.to_string();
        {
            // Keep live editors across the rebuild: destroying one would turn
            // its unsaved buffer into a crash-recovery prompt.
            let mut registry = self.pane_registry.borrow_mut();
            registry.detach_workspace_editors(ws.id);
            registry.clear_workspace(ws.id);
        }
        let new_widget = self.build_workspace_widget(ws);
        self.pane_registry
            .borrow_mut()
            .discard_unused_detached_editors();
        let mut surfaces = self.surfaces.borrow_mut();
        if let Some(old) = surfaces.remove(&ws.id) {
            self.stack.remove(&old);
        }
        self.stack.add_named(&new_widget, Some(&name));
        surfaces.insert(ws.id, new_widget);
        self.stack.set_visible_child_name(&name);
        drop(surfaces);
        self.refresh_workspace_solo(ws);
        self.sidebar.select_workspace(ws.id);
        self.focus_first_leaf_of(ws);
    }

    /// Stamp the `flowmux-solo` class on the single pane/tab workspace's
    /// frame and clear it elsewhere. Must run after any layout change
    /// (rerender, split, close pane, add tab, close tab) so the focus
    /// border stays hidden only while the workspace is trivially small.
    fn refresh_workspace_solo(&self, ws: &Workspace) {
        let solo = solo_workspace_pane(ws);
        self.pane_registry.borrow().set_workspace_solo(ws.id, solo);
    }

    /// Shared pane registry — exposed so the keybindings module can
    /// reach into terminal widgets for copy/paste actions on the GTK
    /// main thread without going through the bridge.
    pub fn pane_registry(&self) -> Rc<RefCell<PaneRegistry>> {
        self.pane_registry.clone()
    }

    /// Toast handle used by copy actions. Exposed to keybindings so the
    /// action can remain synchronous on the GTK main thread.
    pub fn clipboard_toast(&self) -> ClipboardToast {
        self.clipboard_toast.clone()
    }

    /// AI usage menu button used by the keyboard action so the shortcut and
    /// side-panel footer button always toggle the same popover instance.
    pub fn usage_button(&self) -> gtk::MenuButton {
        self.sidebar.usage_button()
    }

    fn editors_for_surfaces(&self, surfaces: &[SurfaceId]) -> Vec<EditorPane> {
        let registry = self.pane_registry.borrow();
        surfaces
            .iter()
            .filter_map(|surface| registry.editors.get(surface).cloned())
            .collect()
    }

    async fn confirm_dirty_surfaces(&self, surfaces: &[SurfaceId]) -> bool {
        confirm_dirty_editor_close(&self.window, self.editors_for_surfaces(surfaces)).await
    }

    fn install_state_flush_on_close(&self) {
        let controller = self.clone();
        self.window.connect_close_request(move |_| {
            if !controller.window_close.approved.get() {
                if controller.window_close.prompting.get() {
                    return glib::Propagation::Stop;
                }
                let editors = controller
                    .pane_registry
                    .borrow()
                    .editors
                    .values()
                    .cloned()
                    .collect::<Vec<_>>();
                if !dirty_editor_labels(&editors).is_empty() {
                    controller.window_close.prompting.set(true);
                    let pending = controller.clone();
                    glib::spawn_future_local(async move {
                        let approved = confirm_dirty_editor_close(&pending.window, editors).await;
                        pending.window_close.prompting.set(false);
                        if approved {
                            pending.window_close.approved.set(true);
                            pending.window.close();
                        }
                    });
                    return glib::Propagation::Stop;
                }
                controller.window_close.approved.set(true);
            }
            controller.flush_terminal_cwds_blocking();
            controller.flush_terminal_scrollback_blocking();
            controller.flush_editor_sessions_blocking();
            controller.flush_layout_blocking();
            if let Err(e) = controller.store.save_now_blocking() {
                tracing::warn!(error = %e, "state save on close failed");
            }
            // Cancel all in-flight WebView loads with stop_loading only.
            // The earlier `load_uri("about:blank")` attempt started a new load,
            // which was then internally cancelled during destroy and printed two
            // `internallyFailedLoadTimerFired` ERROR lines. `try_close()` can
            // trigger beforeunload and the same race, so skip it too.
            //
            // Defer destroy by two idle cycles: first let GTK unrealize WebView
            // widgets, then drop the window on the second idle. Avoid timeout to
            // keep the polling-timer regression guard intact.
            for browser in controller.pane_registry.borrow().browsers.values() {
                browser.stop_loading();
            }
            let window = controller.window.clone();
            glib::idle_add_local_once(move || {
                let window = window.clone();
                glib::idle_add_local_once(move || window.destroy());
            });
            glib::Propagation::Stop
        });
    }

    /// Recompute the window title as "flowmux - {focused tab name}".
    /// Fall back to "flowmux" when no pane is focused or the focused pane has no
    /// active surface.
    async fn refresh_window_title(&self) {
        let focused = self.focused_pane.get();
        let title = match focused {
            None => None,
            Some(pane) => {
                let active = self.pane_registry.borrow().active_surface(pane);
                match active {
                    Some(surface) => self.store.surface_title(pane, surface).await,
                    None => None,
                }
            }
        };
        let next = match title.as_deref().map(str::trim) {
            Some(t) if !t.is_empty() => format!("flowmux - {t}"),
            _ => "flowmux".to_string(),
        };
        tracing::debug!(
            focused = ?focused,
            label = ?title,
            next = %next,
            "refresh_window_title"
        );
        self.window.set_title(Some(&next));
    }

    fn live_surface_copy_text(&self, surface: SurfaceId) -> Option<CopyableText> {
        let registry = self.pane_registry.borrow();
        if let Some(term) = registry.terminals.get(&surface) {
            return term.current_dir().map(CopyableText::live_path);
        }
        if let Some(editor) = registry.editors.get(&surface) {
            return Some(CopyableText::live_path(
                editor.workspace_root().to_path_buf(),
            ));
        }
        registry
            .browsers
            .get(&surface)
            .and_then(|browser| CopyableText::url(browser.current_url()))
    }

    async fn stored_surface_copy_text(
        &self,
        pane: PaneId,
        surface: SurfaceId,
    ) -> Option<CopyableText> {
        let ws_id = self.pane_registry.borrow().workspace_of_pane(pane)?;
        let ws = self.store.get_workspace(ws_id).await?;
        stored_surface_copy_text_from_workspace(&ws, pane, surface)
    }

    async fn copyable_surface_text(
        &self,
        pane: PaneId,
        surface: SurfaceId,
    ) -> Option<CopyableText> {
        if let Some(text) = self.live_surface_copy_text(surface) {
            if let Some(cwd) = text.live_terminal_cwd.clone() {
                if let Some(ws_id) = self.update_terminal_cwd(pane, surface, cwd).await {
                    self.refresh_window_title().await;
                    self.sync_workspace_label(ws_id).await;
                    self.refresh_file_browser_from_focus().await;
                }
            }
            return Some(text);
        }

        self.stored_surface_copy_text(pane, surface).await
    }

    async fn focus_direction_from_command(&self, from: Option<PaneId>, dir: FocusDir) {
        if self.file_browser.active.get()
            && self.file_browser.source_pane.get().is_some()
            && (from.is_none() || from == self.file_browser.source_pane.get())
        {
            self.focus_out_of_file_browser(dir);
            return;
        }

        if self.worktrees.active.get()
            && self.worktrees.source_pane.get().is_some()
            && (from.is_none() || from == self.worktrees.source_pane.get())
        {
            self.focus_out_of_worktree_panel(dir);
            return;
        }

        match from {
            Some(pane) => self.focus_direction_or_right_tools(pane, dir),
            None => self.focus_first_leaf_of_active_workspace().await,
        }
    }

    async fn sync_workspace_label(&self, ws_id: WorkspaceId) {
        let Some(ws) = self.store.get_workspace(ws_id).await else {
            return;
        };

        // Determine the ws.name update candidate from the MRU head's active surface.
        let mru: Vec<PaneId> = self
            .focus_mru
            .borrow()
            .get(&ws_id)
            .map(|q| q.iter().copied().collect())
            .unwrap_or_default();
        // If MRU is empty, fall back to the workspace's first leaf. This happens
        // during initial render before anything has focus.
        let head_pane = mru.first().copied().or_else(|| {
            ws.surfaces
                .first()
                .and_then(|s| s.root_pane.first_leaf_id())
        });

        if let Some(head_pane) = head_pane {
            if let Some(new_name) = focused_surface_full_title(&ws, head_pane) {
                self.store.set_workspace_name(ws_id, new_name).await;
            }
        }

        // Re-read the updated workspace from store before drawing the sidebar;
        // the local ws is stale after set_workspace_name.
        if let Some(ws) = self.store.get_workspace(ws_id).await {
            let details = workspace_row_details(&ws, &mru);
            self.sidebar.upsert_with_details(&ws, details);
        }
        self.refresh_agent_bar().await;
    }

    /// Handle a pane focus event, update MRU, and sync label/subtitles. Focusing
    /// the same pane again moves it to the MRU head, though the label itself may
    /// not change because set_workspace_name is idempotent.
    async fn on_pane_focused(&self, pane: PaneId) {
        let Some(ws_id) = self.store.workspace_for_pane(pane).await else {
            return;
        };
        self.focused_pane.set(Some(pane));
        self.pane_registry.borrow().mark_focused_pane(pane);
        {
            let mut mru = self.focus_mru.borrow_mut();
            let queue = mru.entry(ws_id).or_default();
            queue.retain(|p| *p != pane);
            queue.push_front(pane);
            while queue.len() > 3 {
                queue.pop_back();
            }
        }
        self.sync_workspace_label(ws_id).await;
        let active_surface = self.pane_registry.borrow().active_surface(pane);
        self.acknowledge_source_notifications(Some(ws_id), Some(pane), active_surface);
        if let Some(surface) = active_surface {
            self.refresh_agent_screen_status(surface, None).await;
        }
    }

    /// Called right before exit. Record window size, maximized state, sidebar
    /// divider, and every split paned ratio in the store so the next launch can
    /// restore the same layout.
    fn flush_layout_blocking(&self) {
        // Window size. While maximized, default_size keeps the size from before
        // maximizing, which is the natural expanded size for next launch.
        let (w, h) = self.window.default_size();
        let layout = flowmux_state::WindowLayout {
            width: w,
            height: h,
            maximized: self.window.is_maximized(),
        };
        self.store.set_window_layout_blocking(layout);

        // Side-panel divider position.
        let pos = self.sidebar_split.position();
        if pos > 0 {
            self.store.set_sidebar_position_blocking(pos);
        }

        // All pane split ratios.
        let ratios = self.pane_registry.borrow().split_ratios();
        for (split_id, ratio) in ratios {
            self.store.set_pane_split_ratio_blocking(split_id, ratio);
        }
    }

    /// Drop the workspace's stack page entirely (used when its last
    /// surface is closed).
    pub fn drop_workspace(&self, id: WorkspaceId) {
        let dropping_zoomed_workspace = self
            .pane_zoom
            .active
            .borrow()
            .as_ref()
            .is_some_and(|zoom| zoom.workspace == id);
        if dropping_zoomed_workspace {
            self.clear_pane_zoom();
        }
        self.sidebar.remove(id);
        self.pane_registry.borrow_mut().clear_workspace(id);
        let mut surfaces = self.surfaces.borrow_mut();
        if let Some(old) = surfaces.remove(&id) {
            self.stack.remove(&old);
        }
    }

    /// Show a modal "Are you sure you want to close this workspace?"
    /// dialog and resolve to the user's choice. Used by every path that
    /// can drop a workspace (sidebar X click, last-pane Ctrl+W,
    /// last-tab close) so the user always confirms an irreversible
    /// teardown of the workspace's running PTYs and browser state.
    async fn confirm_close_workspace(&self, id: WorkspaceId) -> bool {
        let title = match self.store.get_workspace(id).await {
            Some(ws) => ws.display_title().to_string(),
            None => return true, // Already gone — nothing to confirm.
        };
        let dialog = adw::AlertDialog::new(
            Some("Close workspace?"),
            Some(&format!("This will close “{title}” and stop its tabs.")),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("close", "Close");
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.set_response_appearance("close", adw::ResponseAppearance::Destructive);

        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        let tx_cell: Rc<Cell<Option<tokio::sync::oneshot::Sender<bool>>>> =
            Rc::new(Cell::new(Some(tx)));
        let tx_for_resp = tx_cell.clone();
        dialog.connect_response(None, move |dialog, response| {
            if let Some(tx) = tx_for_resp.take() {
                let _ = tx.send(response == "close");
            }
            dialog.close();
        });
        let _native_browser_suspend =
            crate::ui::browser_pane::suspend_native_browser_views_for_window(
                self.window.upcast_ref(),
            );
        dialog.present(Some(&self.window));
        rx.await.unwrap_or(false)
    }

    /// Show a single modal confirmation before closing every open
    /// workspace via the sidebar's "Close all tabs" item. Resolves to
    /// `true` if the user confirms, `false` on cancel or when there is
    /// nothing to close.
    async fn confirm_close_all_workspaces(&self) -> bool {
        let count = self.store.list_workspaces().await.len();
        if count == 0 {
            return false;
        }
        let dialog = adw::AlertDialog::new(
            Some("Close all tabs?"),
            Some(&format!(
                "This will close all {count} workspaces and stop their tabs."
            )),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("close", "Close all");
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.set_response_appearance("close", adw::ResponseAppearance::Destructive);

        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        let tx_cell: Rc<Cell<Option<tokio::sync::oneshot::Sender<bool>>>> =
            Rc::new(Cell::new(Some(tx)));
        let tx_for_resp = tx_cell.clone();
        dialog.connect_response(None, move |dialog, response| {
            if let Some(tx) = tx_for_resp.take() {
                let _ = tx.send(response == "close");
            }
            dialog.close();
        });
        let _native_browser_suspend =
            crate::ui::browser_pane::suspend_native_browser_views_for_window(
                self.window.upcast_ref(),
            );
        dialog.present(Some(&self.window));
        rx.await.unwrap_or(false)
    }

    async fn activate_active_or_show_empty(&self) {
        if let Some(id) = self.store.active_or_first().await {
            if self.surfaces.borrow().contains_key(&id) {
                self.activate_workspace(id).await;
                return;
            }
        }
        self.show_status_when_empty();
        self.refresh_agent_bar().await;
    }

    /// Inline copy of the `GtkCommand::ActivateSurface` arm — used by
    /// the notification click router so we can `await` the surface
    /// switch before grabbing focus. Idempotent when the surface is
    /// already active.
    async fn activate_surface_now(&self, pane: PaneId, surface: SurfaceId) {
        let ws_id = self.store.set_active_surface(pane, surface).await;
        self.pane_registry
            .borrow_mut()
            .activate_surface(pane, surface);
        self.refresh_window_title().await;
        if let Some(ws_id) = ws_id {
            self.sync_workspace_agent_status_from_store(ws_id).await;
        }
        self.refresh_agent_screen_status(surface, None).await;
        self.refresh_file_browser_from_focus().await;
    }

    /// True when the GUI is the foreground window AND the user is
    /// currently looking at exactly the pane+surface that the
    /// notification came from. Used to suppress redundant toasts /
    /// bell-popover entries (cmux's
    /// `shouldSuppressExternalDelivery`).
    fn is_source_focused(
        &self,
        source_pane: Option<PaneId>,
        source_surface: Option<SurfaceId>,
    ) -> bool {
        let Some(pane) = source_pane else {
            return false;
        };
        if !self.window.is_active() {
            return false;
        }
        if self.focused_pane.get() != Some(pane) {
            return false;
        }
        match source_surface {
            // Notification carries no surface — same-pane is enough.
            None => true,
            // Compare to the currently active surface inside the pane;
            // a non-active tab still gets a notification.
            Some(s) => self.pane_registry.borrow().active_surface(pane) == Some(s),
        }
    }

    fn is_agent_surface_visible(&self, surface: SurfaceId) -> bool {
        let focused_pane = self.focused_pane.get();
        let active_surface =
            focused_pane.and_then(|pane| self.pane_registry.borrow().active_surface(pane));
        let source_pane = self.pane_registry.borrow().pane_for_surface(surface);
        agent_surface_is_visible(
            self.window.is_active(),
            focused_pane,
            source_pane,
            active_surface,
            surface,
        )
    }

    /// Grab keyboard focus on `pane`. Deferred to the next idle so that
    /// any in-flight workspace activation has finished swapping the
    /// stack child before we ask GTK to focus a specific terminal /
    /// browser leaf. No-op when `pane` is unknown to the registry —
    /// e.g. when the source workspace was closed between the
    /// notification firing and the user clicking it.
    fn focus_pane(&self, pane: PaneId) {
        let registry = self.pane_registry.clone();
        if registry.borrow().pane_frame(pane).is_none() {
            tracing::debug!(%pane, "focus_pane: pane is not registered");
            return;
        }
        // Keep pane-local shortcuts and the focus border in sync even if the
        // backend widget does not emit a focus-enter event after a drag split.
        self.focused_pane.set(Some(pane));
        registry.borrow().mark_focused_pane(pane);
        let bridge = self.bridge.clone();
        glib::MainContext::default().spawn_local(async move {
            let _ = bridge.tx.send(GtkCommand::PaneFocused { pane }).await;
            let _ = bridge.tx.send(GtkCommand::RefreshWindowTitle).await;
        });
        glib::idle_add_local_once(move || {
            let r = registry.borrow();
            if let Some(term) = r.active_terminal(pane) {
                term.grab_focus();
            } else if let Some(browser) = r.active_browser(pane) {
                browser.grab_focus();
            } else if let Some(editor) = r.active_editor(pane) {
                editor.grab_focus();
            } else {
                tracing::debug!(%pane, "focus_pane: no surface registered for pane");
            }
        });
    }

    async fn resize_pane_ratio(&self, pane: PaneId, ratio: f32) -> Result<(), String> {
        if !ratio.is_finite() || !(0.0..1.0).contains(&ratio) {
            return Err("ratio must be a finite value between 0 and 1".into());
        }

        let split_id = if self.store.set_pane_split_ratio(pane, ratio).await {
            pane
        } else {
            let Some(split_id) = self.store.parent_split_for_pane(pane).await else {
                return Err(format!("pane not found: {pane}"));
            };
            let _ = self.store.set_pane_split_ratio(split_id, ratio).await;
            split_id
        };

        let _ = self
            .pane_registry
            .borrow()
            .apply_split_ratio(split_id, ratio);
        Ok(())
    }

    /// Find the first leaf in this workspace's first surface and
    /// grab keyboard focus on it. Deferred to the next idle so the
    /// widget tree is realized first.
    fn focus_first_leaf_of(&self, ws: &Workspace) {
        let leaf = ws
            .surfaces
            .first()
            .and_then(|s| s.root_pane.first_leaf_id());
        let Some(leaf_id) = leaf else { return };
        let registry = self.pane_registry.clone();
        glib::idle_add_local_once(move || {
            let r = registry.borrow();
            if let Some(term) = r.active_terminal(leaf_id) {
                term.grab_focus();
            } else if let Some(browser) = r.active_browser(leaf_id) {
                browser.grab_focus();
            } else if let Some(editor) = r.active_editor(leaf_id) {
                editor.grab_focus();
            }
        });
    }

    /// Focus the active workspace's first leaf pane. Used as a fallback when the
    /// user has only clicked a workspace in the side panel, focused_pane is None,
    /// and any Alt+arrow direction is pressed.
    async fn focus_first_leaf_of_active_workspace(&self) {
        let Some(active) = self.store.active_or_first().await else {
            return;
        };
        if let Some(ws) = self.store.get_workspace(active).await {
            self.focus_first_leaf_of(&ws);
        }
    }

    fn build_workspace_widget(&self, ws: &Workspace) -> gtk::Widget {
        match ws.surfaces.first() {
            Some(s) => build_surface(
                ws.id,
                s,
                &self.callbacks,
                self.pane_registry.clone(),
                self.current_theme(),
            ),
            None => gtk::Label::new(Some("(empty workspace)")).upcast(),
        }
    }

    pub async fn dispatch(&self, cmd: GtkCommand) {
        if matches!(&cmd, GtkCommand::BrowserOpenSplit { .. }) {
            self.clear_pane_zoom();
        }
        match cmd {
            command @ (GtkCommand::BrowserEval { .. }
            | GtkCommand::BrowserAction { .. }
            | GtkCommand::BrowserOpenSplit { .. }
            | GtkCommand::OpenUrlInBrowserTab { .. }
            | GtkCommand::InjectCookies { .. }) => {
                self.dispatch_browser_command(command).await;
            }
            command @ (GtkCommand::AddNotification { .. }
            | GtkCommand::SetNotificationDesktopId { .. }
            | GtkCommand::CloseDesktopNotifications { .. }
            | GtkCommand::RefreshLauncherBadge
            | GtkCommand::OpenNotification { .. }
            | GtkCommand::ListNotifications { .. }
            | GtkCommand::OpenNotificationWithAck { .. }
            | GtkCommand::OpenOldestUnreadNotification { .. }
            | GtkCommand::MarkNotificationRead { .. }
            | GtkCommand::ClearNotifications { .. }
            | GtkCommand::DeleteNotification { .. }
            | GtkCommand::ClearAllNotifications
            | GtkCommand::SetAgentStatus { .. }
            | GtkCommand::QueryAgentSurfaceVisible { .. }
            | GtkCommand::OpenAgentBarItem { .. }) => {
                self.dispatch_notification_command(command).await;
            }
            command @ (GtkCommand::WorkspaceCreated { .. }
            | GtkCommand::NewWorkspace { .. }
            | GtkCommand::RemoveWorkspace { .. }
            | GtkCommand::RemoveAllWorkspaces { .. }
            | GtkCommand::RenameWorkspace { .. }
            | GtkCommand::SetWorkspaceColor { .. }
            | GtkCommand::ReorderWorkspace { .. }
            | GtkCommand::ShowRenameDialog { .. }
            | GtkCommand::ShowColorDialog { .. }
            | GtkCommand::FocusWorkspaceDir { .. }
            | GtkCommand::FocusWorkspaceAt { .. }
            | GtkCommand::ActivateWorkspace { .. }) => {
                self.dispatch_workspace_command(command).await;
            }
            command @ (GtkCommand::PaneSplitApplied { .. }
            | GtkCommand::SplitFocused { .. }
            | GtkCommand::CloseFocused { .. }
            | GtkCommand::FocusDirection { .. }
            | GtkCommand::NewSurface { .. }
            | GtkCommand::OpenTig { .. }
            | GtkCommand::CreateSurface { .. }
            | GtkCommand::NewBrowserSurface { .. }
            | GtkCommand::ActivateSurface { .. }
            | GtkCommand::CloseSurface { .. }
            | GtkCommand::RenameSurface { .. }
            | GtkCommand::ShowRenameSurfaceDialog { .. }
            | GtkCommand::ReorderSurface { .. }
            | GtkCommand::TearOffSurface { .. }
            | GtkCommand::MoveSurfaceToPane { .. }
            | GtkCommand::MoveSurfaceToWorkspace { .. }
            | GtkCommand::SplitSurfaceIntoPane { .. }
            | GtkCommand::TerminalCwdChanged { .. }
            | GtkCommand::BrowserUriChanged { .. }
            | GtkCommand::BrowserTitleChanged { .. }
            | GtkCommand::TerminalTitleChanged { .. }
            | GtkCommand::RefreshWindowTitle
            | GtkCommand::PaneFocused { .. }
            | GtkCommand::PaneSendKeys { .. }
            | GtkCommand::PaneReadScreen { .. }
            | GtkCommand::FocusPane { .. }
            | GtkCommand::TogglePaneZoom { .. }
            | GtkCommand::ResizePane { .. }) => {
                self.dispatch_pane_command(command).await;
            }
            command @ (GtkCommand::ShowOptionsDialog
            | GtkCommand::ShowCommandPalette
            | GtkCommand::FileBrowserFocusOut { .. }
            | GtkCommand::FileBrowserCloseAndRestoreFocus
            | GtkCommand::OpenFileInEditor { .. }
            | GtkCommand::ToggleWorktreePanel { .. }
            | GtkCommand::RefreshWorktrees
            | GtkCommand::WorktreesLoaded { .. }
            | GtkCommand::ShowWorktreeInfo { .. }
            | GtkCommand::RemoveWorktree { .. }
            | GtkCommand::WorktreeRemovalFinished { .. }
            | GtkCommand::WorktreePanelFocusOut { .. }
            | GtkCommand::WorktreePanelCloseAndRestoreFocus
            | GtkCommand::ToggleFileBrowser { .. }
            | GtkCommand::OpenImageViewer { .. }
            | GtkCommand::OpenMarkdownViewer { .. }
            | GtkCommand::ShowSurfaceFolder { .. }
            | GtkCommand::CopySurfaceText { .. }
            | GtkCommand::CopyFocusedPaneText { .. }
            | GtkCommand::ShowFocusedPaneFolder { .. }) => {
                self.dispatch_window_chrome_command(command).await;
            }
        }
    }

    /// Move keyboard focus to the nearest pane in `dir` relative to
    /// the pane currently identified by `from`. Bbox computation is
    /// in the stack's coordinate space so split orientation doesn't
    /// matter.
    fn focus_in_direction(&self, from: PaneId, dir: FocusDir) -> Option<PaneId> {
        use gtk::graphene::Rect;

        let registry = self.pane_registry.borrow();
        let from_widget = registry.pane_frame(from)?;
        // Alt+arrow moves only within the same workspace. GtkStack can keep
        // inactive workspace widgets overlapping at the same coordinates, where
        // compute_bounds may return non-zero values; without the workspace
        // filter, focus could leak into another workspace.
        let workspace = registry.workspace_of_pane(from)?;
        let stack = &self.stack;
        let from_bbox = from_widget.compute_bounds(stack)?;
        let from_center = (
            from_bbox.x() + from_bbox.width() / 2.0,
            from_bbox.y() + from_bbox.height() / 2.0,
        );

        let mut best: Option<(PaneId, f32)> = None;
        for id in registry.pane_ids_in_workspace(workspace) {
            if id == from {
                continue;
            }
            let Some(pane) = registry.pane_frame(id) else {
                continue;
            };
            let Some(bbox) = pane.compute_bounds(stack) else {
                continue;
            };
            let center = (
                bbox.x() + bbox.width() / 2.0,
                bbox.y() + bbox.height() / 2.0,
            );
            let (dx, dy) = (center.0 - from_center.0, center.1 - from_center.1);
            // Direction predicate + axis-aligned distance preference.
            let in_direction = match dir {
                FocusDir::Left => dx < -1.0 && dy.abs() < bbox.height().max(from_bbox.height()),
                FocusDir::Right => dx > 1.0 && dy.abs() < bbox.height().max(from_bbox.height()),
                FocusDir::Up => dy < -1.0 && dx.abs() < bbox.width().max(from_bbox.width()),
                FocusDir::Down => dy > 1.0 && dx.abs() < bbox.width().max(from_bbox.width()),
            };
            if !in_direction {
                continue;
            }
            let dist = dx * dx + dy * dy;
            if best.map(|(_, d)| dist < d).unwrap_or(true) {
                best = Some((id, dist));
            }
        }
        let _ = Rect::new(0.0, 0.0, 0.0, 0.0); // ensure import used in non-tests path
        if let Some((id, _)) = best {
            let has_active_surface = registry.active_terminal(id).is_some()
                || registry.active_browser(id).is_some()
                || registry.active_editor(id).is_some();
            drop(registry);

            if has_active_surface {
                self.focus_pane(id);
                Some(id)
            } else {
                tracing::debug!(target_pane = %id, "no active surface to focus");
                None
            }
        } else {
            tracing::debug!(?dir, "no pane in that direction");
            None
        }
    }

    /// Bring `id`'s workspace to the foreground, persist it as the
    /// active workspace, and grab focus on its first leaf so keyboard
    /// shortcuts work immediately.
    async fn activate_workspace(&self, id: WorkspaceId) {
        self.clear_pane_zoom();
        if self.surfaces.borrow().contains_key(&id) {
            self.stack.set_visible_child_name(&id.to_string());
        }
        self.sidebar.select_workspace(id);
        // Programmatic activation paths (notification click, Alt+
        // number, focus shortcut) bypass the row-activated callback
        // that would otherwise drop the attention tint, so we clear it
        // here too.
        self.acknowledge_workspace_notifications(id);
        self.store.set_active_workspace(Some(id)).await;
        self.sync_workspace_agent_status_from_store(id).await;
        if let Some(ws) = self.store.get_workspace(id).await {
            // Selecting a workspace lands on the last tab of its first pane.
            if let Some(leaf) = ws
                .surfaces
                .first()
                .and_then(|s| s.root_pane.first_leaf_id())
            {
                let last =
                    ws.surfaces
                        .first()
                        .and_then(|s| match s.root_pane.find_leaf_content(leaf) {
                            Some(flowmux_core::PaneContent::Tabs { surfaces, .. }) => {
                                surfaces.last().map(|surface| surface.id)
                            }
                            _ => None,
                        });
                if let Some(last) = last {
                    self.store.set_active_surface(leaf, last).await;
                    self.pane_registry.borrow_mut().activate_surface(leaf, last);
                    self.sync_workspace_agent_status_from_store(id).await;
                    self.refresh_agent_screen_status(last, None).await;
                }
            }
            self.focus_first_leaf_of(&ws);
        }
    }

    pub async fn restore_from_store(&self) {
        let snap = self.store.snapshot().await;
        let mut rendered = HashSet::new();
        for ws_id in &snap.workspace_order {
            if let Some(ws) = snap.workspaces.iter().find(|ws| ws.id == *ws_id) {
                if rendered.insert(ws.id) {
                    self.render_workspace_with_activation(ws, false);
                }
            }
        }
        // StateStore normalizes workspace_order into a complete permutation,
        // but retain a defensive fallback for snapshots produced by older
        // state files or tests that construct State directly.
        for ws in &snap.workspaces {
            if rendered.insert(ws.id) {
                self.render_workspace_with_activation(ws, false);
            }
        }
        // First-render the side-panel rows had no MRU yet, so their
        // subtitle area was blank. Now that every workspace's pane
        // tree is in the store, fill subtitles from the first leaf
        // of each workspace (and refresh ws.name from that leaf's
        // active surface). The user sees populated paths under each
        // workspace name on launch instead of empty rows.
        for ws_id in &snap.workspace_order {
            self.sync_workspace_label(*ws_id).await;
        }
        let active = snap
            .active_workspace
            .or_else(|| snap.workspace_order.first().copied());
        if let Some(active) = active {
            self.activate_workspace(active).await;
        }
        self.refresh_all_agent_screen_statuses().await;
    }
}

/// Lazily connect to `org.gtk.Notifications` and return a clone of
/// the cached [`flowmux_notify::DesktopNotifier`].
///
/// The cell is an `Arc<tokio::sync::Mutex<…>>` shared with the
/// daemon-side handler so that the first connection wins the lazy
/// init race and both the `AddNotification` (tokio side) and
/// `RemoveNotification` (GTK side) paths reuse the same unique bus
/// name. gnome-shell keys entries by `(sender, app_id)`, so swapping
/// connections mid-flight is exactly what leaves the dock badge
/// pinned after the user acks.
async fn ensure_desktop_notifier(
    cell: &Arc<tokio::sync::Mutex<Option<flowmux_notify::DesktopNotifier>>>,
) -> Option<flowmux_notify::DesktopNotifier> {
    let mut guard = cell.lock().await;
    if let Some(n) = guard.as_ref() {
        return Some(n.clone());
    }
    match flowmux_notify::DesktopNotifier::connect().await {
        Ok(n) => {
            *guard = Some(n.clone());
            Some(n)
        }
        Err(e) => {
            tracing::debug!(error = %e, "could not connect to org.gtk.Notifications");
            None
        }
    }
}

/// Return the active surface title for `focused_pane` at original length for
/// the side-panel label. User-renamed labels or OSC 0/2 labels already use
/// surface.title as the original value. Otherwise, for terminals, extract the
/// cwd folder name at full length because surface.title may be truncated to 15
/// characters for tab display.
fn focused_surface_full_title(
    ws: &flowmux_core::Workspace,
    focused_pane: PaneId,
) -> Option<String> {
    use flowmux_core::SurfaceKind;
    let active = ws
        .surfaces
        .first()
        .and_then(|s| s.root_pane.active_surface_id(focused_pane))?;
    let surface = ws
        .surfaces
        .first()
        .and_then(|s| s.root_pane.find_surface(focused_pane, active))?;
    if surface.title_locked {
        return Some(surface.title.clone());
    }
    if let SurfaceKind::Terminal { cwd: Some(cwd), .. } = &surface.kind {
        let derived = flowmux_core::terminal_tab_title_for_cwd(Some(cwd));
        if surface.title == derived {
            // surface.title is the truncated cwd folder name. Rebuild the full
            // length value so the side panel can ellipsize it to available width.
            if let Some(folder) = cwd.file_name().and_then(|n| n.to_str()) {
                if !folder.is_empty() {
                    return Some(folder.to_string());
                }
            }
        }
    }
    Some(surface.title.clone())
}

/// Build one subtitle line from each active surface in `pane_ids` MRU order.
///   * Active terminal surfaces with cwd use [`shorten_cwd_path`].
///   * Active terminal surfaces without cwd are skipped to avoid caching the
///     first-spawn race as a subtitle.
///   * Active browser tabs use `Browser-{tab name}`.
///
/// Result length never exceeds `cap`. If MRU is empty or short, DFS over tree
/// leaves left-first to keep side-panel subtitles populated.
const SIDEBAR_AGENT_BLOCK_LIMIT: usize = 4;
const SIDEBAR_PATH_LINE_LIMIT: usize = 3;

fn workspace_row_details(ws: &Workspace, mru: &[PaneId]) -> WorkspaceRowDetails {
    let agent_blocks = ws.collect_agent_blocks(mru);
    if agent_blocks.is_empty() {
        return WorkspaceRowDetails::path_only(&collect_subtitle_lines(
            ws,
            mru,
            SIDEBAR_PATH_LINE_LIMIT,
        ));
    }

    let visible_count = agent_blocks.len().min(SIDEBAR_AGENT_BLOCK_LIMIT);
    let overflow_count = agent_blocks.len().saturating_sub(visible_count);
    let visible_agent_panes: HashSet<PaneId> = agent_blocks
        .iter()
        .take(visible_count)
        .map(|block| block.pane)
        .collect();
    let mut row_agent_blocks: Vec<WorkspaceRowAgentBlock> = agent_blocks
        .iter()
        .take(visible_count)
        .map(workspace_row_agent_block)
        .collect();
    if overflow_count > 0 {
        if let Some(last) = row_agent_blocks.last_mut() {
            last.overflow_count = overflow_count;
        }
    }
    let path_lines =
        collect_subtitle_lines_excluding(ws, mru, SIDEBAR_PATH_LINE_LIMIT, &visible_agent_panes);

    WorkspaceRowDetails {
        agent_blocks: row_agent_blocks,
        path_lines,
    }
}

fn workspace_row_agent_block(block: &WorkspaceAgentBlock) -> WorkspaceRowAgentBlock {
    WorkspaceRowAgentBlock {
        agent_name: block.agent_name.clone(),
        status: block.status,
        seen: block.seen,
        status_text: block
            .status_text
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| block.status.as_str().to_string()),
        path: block
            .cwd
            .as_deref()
            .map(|cwd| shorten_cwd_path(std::path::Path::new(cwd))),
        overflow_count: 0,
    }
}

fn collect_subtitle_lines(ws: &flowmux_core::Workspace, mru: &[PaneId], cap: usize) -> Vec<String> {
    collect_subtitle_lines_excluding(ws, mru, cap, &HashSet::new())
}

fn collect_subtitle_lines_excluding(
    ws: &flowmux_core::Workspace,
    mru: &[PaneId],
    cap: usize,
    excluded_panes: &HashSet<PaneId>,
) -> Vec<String> {
    use flowmux_core::SurfaceKind;
    let Some(root) = ws.surfaces.first().map(|s| &s.root_pane) else {
        return Vec::new();
    };
    let line_for = |pane_id: PaneId| -> Option<String> {
        let active = root.active_surface_id(pane_id)?;
        let surface = root.find_surface(pane_id, active)?;
        match &surface.kind {
            SurfaceKind::Terminal { cwd: Some(cwd), .. } => Some(shorten_cwd_path(cwd)),
            SurfaceKind::Terminal { cwd: None, .. } => None,
            SurfaceKind::Browser { .. } => Some(format!("Browser-{}", surface.title)),
            SurfaceKind::Editor { .. } => Some(format!("Editor-{}", surface.title)),
        }
    };

    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<PaneId> = HashSet::new();
    for pane in mru {
        if excluded_panes.contains(pane) {
            continue;
        }
        if seen.contains(pane) {
            continue;
        }
        if let Some(line) = line_for(*pane) {
            out.push(line);
            seen.insert(*pane);
            if out.len() >= cap {
                return out;
            }
        }
    }
    // MRU is empty or short, so fill from tree leaves by left-first DFS.
    let mut all_leaves: Vec<PaneId> = Vec::new();
    root.for_each_leaf(|id| all_leaves.push(id));
    for pane in all_leaves {
        if out.len() >= cap {
            break;
        }
        if excluded_panes.contains(&pane) {
            continue;
        }
        if seen.contains(&pane) {
            continue;
        }
        if let Some(line) = line_for(pane) {
            out.push(line);
            seen.insert(pane);
        }
    }
    out
}

/// Keep only the last 3 normal path components and shorten the prefix to "...".
/// Paths with 3 or fewer components are shown unchanged. Examples:
///   * `/home/junsu/dev/os/flowmux` → `.../dev/os/flowmux`
///   * `/home/junsu`               → `/home/junsu`
///   * `/`                          → `/`
pub(crate) fn shorten_cwd_path(path: &std::path::Path) -> String {
    use std::path::Component;
    let names: Vec<&str> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    if names.len() <= 3 {
        return path.display().to_string();
    }
    let last3 = &names[names.len() - 3..];
    format!(".../{}", last3.join("/"))
}

/// Inject cookies into the default WebKit network session.
///
/// Real injection goes through `WebKit.NetworkSession.cookie_manager()`
/// → `CookieManager.add_cookie(&soup::Cookie, ...)`. The `soup::Cookie`
/// type is only re-exported from webkit6 when the `soup3` feature is
/// enabled (which in turn pulls in libsoup-3). To keep the default
/// build minimal we record the cookies that *would* be injected and
/// return the count; flipping `flowmux/Cargo.toml` to
/// `webkit6 = { version = "0.4", features = ["soup3"] }` and replacing
/// the body below with `manager.add_cookie(...)` calls is the only
/// change needed when we ship cookie import to users.
fn inject_cookies_into_webkit(cookies: &[flowmux_cookies::Cookie]) -> Result<usize, String> {
    let mut count = 0;
    for c in cookies {
        tracing::debug!(host = %c.host, name = %c.name, "would inject cookie");
        count += 1;
    }
    Ok(count)
}

/// Per-workspace context-menu actions. These accept a workspace UUID
/// string as their target value so a single action handler serves
/// every sidebar row's context menu.
fn register_workspace_actions(
    window: &adw::ApplicationWindow,
    store: &StateStore,
    bridge: &Bridge,
) {
    use gtk::gio;

    // win.rename-workspace(<uuid>) — opens an adw::AlertDialog with an
    // Entry for the new name and OK/Cancel responses.
    let store_for_rename = store.clone();
    let bridge_for_rename = bridge.clone();
    let window_weak = window.downgrade();
    let rename = gio::ActionEntry::builder("rename-workspace")
        .parameter_type(Some(gtk::glib::VariantTy::STRING))
        .activate(move |_, _, param| {
            let Some(id_str) = param.and_then(|p| p.str().map(String::from)) else {
                return;
            };
            let Ok(id) = id_str.parse::<WorkspaceId>() else {
                return;
            };
            let store = store_for_rename.clone();
            let bridge = bridge_for_rename.clone();
            let window_weak = window_weak.clone();
            glib::MainContext::default().spawn_local(async move {
                let Some(ws) = store.get_workspace(id).await else {
                    return;
                };
                let Some(window) = window_weak.upgrade() else {
                    return;
                };
                let prefill = ws.custom_title.as_deref().unwrap_or(&ws.name).to_string();
                show_rename_dialog(&window, id, &prefill, bridge);
            });
        })
        .build();

    // win.recolor-workspace(<uuid>) — opens a gtk::ColorDialog seeded
    // with the current color and writes the picked one back.
    let store_for_color = store.clone();
    let bridge_for_color = bridge.clone();
    let window_weak2 = window.downgrade();
    let recolor = gio::ActionEntry::builder("recolor-workspace")
        .parameter_type(Some(gtk::glib::VariantTy::STRING))
        .activate(move |_, _, param| {
            let Some(id_str) = param.and_then(|p| p.str().map(String::from)) else {
                return;
            };
            let Ok(id) = id_str.parse::<WorkspaceId>() else {
                return;
            };
            let store = store_for_color.clone();
            let bridge = bridge_for_color.clone();
            let window_weak = window_weak2.clone();
            glib::MainContext::default().spawn_local(async move {
                let current = store.get_workspace(id).await.and_then(|w| w.color);
                let Some(window) = window_weak.upgrade() else {
                    return;
                };
                show_color_dialog(&window, id, current.as_deref(), bridge);
            });
        })
        .build();

    // win.close-tab(<uuid>) — same effect as the hover X button, but
    // routed through the right-click menu so the close path is
    // discoverable.
    let bridge_for_close = bridge.clone();
    let close_tab = gio::ActionEntry::builder("close-tab")
        .parameter_type(Some(gtk::glib::VariantTy::STRING))
        .activate(move |_, _, param| {
            let Some(id_str) = param.and_then(|p| p.str().map(String::from)) else {
                return;
            };
            let Ok(id) = id_str.parse::<WorkspaceId>() else {
                return;
            };
            let bridge = bridge_for_close.clone();
            glib::MainContext::default().spawn_local(async move {
                let (tx, rx) = oneshot::channel();
                let _ = bridge
                    .tx
                    .send(GtkCommand::RemoveWorkspace {
                        id,
                        confirm: true,
                        ack: tx,
                    })
                    .await;
                let _ = rx.await;
            });
        })
        .build();

    window.add_action_entries([rename, recolor, close_tab]);
}

fn show_rename_dialog(
    window: &adw::ApplicationWindow,
    id: WorkspaceId,
    current_name: &str,
    bridge: Bridge,
) {
    let dialog = adw::AlertDialog::new(
        Some("Rename Tab"),
        Some("Leave empty and click OK to return to automatic mode."),
    );
    let entry = gtk::Entry::new();
    entry.set_text(current_name);
    entry.set_activates_default(true);
    entry.set_hexpand(true);
    dialog.set_extra_child(Some(&entry));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", "OK");
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);

    let entry_for_resp = entry.clone();
    dialog.connect_response(None, move |dialog, response| {
        if response == "ok" {
            // Match cmux: pass empty or whitespace-only input through to the
            // daemon as the signal to reset custom_title to None. The daemon
            // trims and interprets meaningless input as returning to automatic mode.
            let new_name = entry_for_resp.text().to_string();
            let bridge = bridge.clone();
            glib::MainContext::default().spawn_local(async move {
                let (tx, _rx) = oneshot::channel();
                let _ = bridge
                    .tx
                    .send(GtkCommand::RenameWorkspace {
                        id,
                        name: new_name,
                        ack: tx,
                    })
                    .await;
            });
        }
        dialog.close();
    });
    dialog.present(Some(window));
}

fn show_rename_surface_dialog(
    window: &adw::ApplicationWindow,
    pane: PaneId,
    surface: SurfaceId,
    current_title: &str,
    bridge: Bridge,
) {
    let dialog = adw::AlertDialog::new(Some("Rename Pane Tab"), None);
    let entry = gtk::Entry::new();
    entry.set_text(current_title);
    entry.set_activates_default(true);
    entry.set_hexpand(true);
    dialog.set_extra_child(Some(&entry));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", "OK");
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);

    let entry_for_resp = entry.clone();
    dialog.connect_response(None, move |dialog, response| {
        if response == "ok" {
            let new_title = entry_for_resp.text().trim().to_string();
            if !new_title.is_empty() {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let (tx, _rx) = oneshot::channel();
                    let _ = bridge
                        .tx
                        .send(GtkCommand::RenameSurface {
                            pane,
                            surface,
                            title: new_title,
                            ack: tx,
                        })
                        .await;
                });
            }
        }
        dialog.close();
    });
    dialog.present(Some(window));
}

fn show_color_dialog(
    window: &adw::ApplicationWindow,
    id: WorkspaceId,
    current: Option<&str>,
    bridge: Bridge,
) {
    let dialog = gtk::ColorDialog::builder()
        .title("Tab Color")
        .modal(true)
        .with_alpha(false)
        .build();
    let initial = current
        .and_then(|s| gtk::gdk::RGBA::parse(s).ok())
        .unwrap_or_else(|| gtk::gdk::RGBA::new(0.5, 0.5, 0.5, 1.0));
    dialog.choose_rgba(
        Some(window),
        Some(&initial),
        gtk::gio::Cancellable::NONE,
        move |result| {
            let Ok(rgba) = result else { return };
            let hex = format!(
                "#{:02x}{:02x}{:02x}",
                (rgba.red() * 255.0).clamp(0.0, 255.0) as u8,
                (rgba.green() * 255.0).clamp(0.0, 255.0) as u8,
                (rgba.blue() * 255.0).clamp(0.0, 255.0) as u8,
            );
            let bridge = bridge.clone();
            glib::MainContext::default().spawn_local(async move {
                let (tx, _rx) = oneshot::channel();
                let _ = bridge
                    .tx
                    .send(GtkCommand::SetWorkspaceColor {
                        id,
                        color: hex,
                        ack: tx,
                    })
                    .await;
            });
        },
    );
}

/// Top-of-window "Copied to clipboard" toast. The generation counter
/// keeps repeated copies safe: each new copy bumps `generation`, so the
/// pending hide-timer from the previous copy notices it is stale and
/// leaves the new toast visible for its own full duration.
#[derive(Clone)]
pub struct ClipboardToast {
    revealer: gtk::Revealer,
    label: gtk::Label,
    generation: Rc<Cell<u64>>,
}

impl ClipboardToast {
    pub const DEFAULT_MESSAGE: &'static str = "Copied to clipboard";

    pub fn new() -> Self {
        let label = gtk::Label::new(Some(Self::DEFAULT_MESSAGE));
        label.set_xalign(0.5);

        let toast = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        toast.add_css_class("flowmux-clipboard-toast");
        toast.append(&label);

        let revealer = gtk::Revealer::builder()
            .transition_duration(140)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(false)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Start)
            .build();
        revealer.set_margin_top(10);
        revealer.set_can_target(false);
        revealer.set_child(Some(&toast));

        Self {
            revealer,
            label,
            generation: Rc::new(Cell::new(0)),
        }
    }

    pub fn widget(&self) -> &gtk::Revealer {
        &self.revealer
    }

    /// Show the toast with the default "Copied to clipboard" message.
    pub fn show(&self) {
        self.show_with_message(Self::DEFAULT_MESSAGE);
    }

    /// Show the toast with a caller-supplied message. Used by the
    /// "copy pane path" chord so the user sees what was copied.
    pub fn show_with_message(&self, message: &str) {
        self.show_with_message_for(message, Duration::from_millis(1400));
    }

    pub fn show_with_message_for(&self, message: &str, duration: Duration) {
        self.label.set_text(message);
        let current = self.generation.get().wrapping_add(1);
        self.generation.set(current);
        self.revealer.set_reveal_child(true);

        let revealer = self.revealer.clone();
        let generation = self.generation.clone();
        glib::timeout_add_local_once(duration, move || {
            if generation.get() == current {
                revealer.set_reveal_child(false);
            }
        });
    }

    #[cfg(all(test, not(target_os = "macos")))]
    pub fn current_message(&self) -> String {
        self.label.text().to_string()
    }
}

/// Evaluate a script on `browser`'s WebView and forward the result
/// through `ack`. When `ok_string_required` is true, the script's
/// returned string must be exactly `"ok"` for the ack to resolve to
/// `BrowserActionResult::Ok` — anything else (including the
/// `"error: …"` strings flowmux_browser scripts use) becomes an Err.
/// When false, the raw string is forwarded so the caller can parse
/// JSON (Snapshot) or read a value back (Text / Value / Attr).
/// Resolve an agent-supplied ref token (e.g. `e3` or `@e3`) to a CSS
/// selector via the browser pane's [`flowmux_browser::RefStore`].
/// Returns a friendly error string when the ref isn't bound — the
/// agent then knows to take a fresh `snapshot --interactive` first.
fn resolve_ref(
    browser: &crate::ui::browser_pane::BrowserPane,
    ref_id: &str,
) -> Result<String, String> {
    let refs = browser.refs.borrow();
    refs.resolve(browser.ref_scope, ref_id)
        .map(|s| s.to_string())
        .ok_or_else(|| {
            format!(
                "ref `{ref_id}` not found in this pane's snapshot — \
                 take a fresh `flowmux browser snapshot` first"
            )
        })
}

/// Like [`run_browser_js`] but expects the page to evaluate to
/// the literal `"true"` or `"false"`, mapping them to
/// `BrowserActionResult::Bool`. Anything else surfaces as an error
/// (e.g. `"error: not found"` keeps its message).
fn run_browser_js_bool(
    browser: &crate::ui::browser_pane::BrowserPane,
    js: &str,
    ack: tokio::sync::oneshot::Sender<Result<BrowserActionResult, String>>,
) {
    let cell = std::cell::Cell::new(Some(ack));
    browser.evaluate_js(js, move |result| {
        if let Some(ack) = cell.take() {
            let mapped = match result {
                Ok(s) if s == "true" => Ok(BrowserActionResult::Bool(true)),
                Ok(s) if s == "false" => Ok(BrowserActionResult::Bool(false)),
                Ok(other) => Err(other),
                Err(e) => Err(e),
            };
            let _ = ack.send(mapped);
        }
    });
}

fn run_browser_js(
    browser: &crate::ui::browser_pane::BrowserPane,
    js: &str,
    ack: tokio::sync::oneshot::Sender<Result<BrowserActionResult, String>>,
    ok_string_required: bool,
) {
    let cell = std::cell::Cell::new(Some(ack));
    browser.evaluate_js(js, move |result| {
        if let Some(ack) = cell.take() {
            let mapped = match result {
                Ok(s) => {
                    if ok_string_required {
                        if s == "ok" {
                            Ok(BrowserActionResult::Ok)
                        } else {
                            Err(s)
                        }
                    } else {
                        Ok(BrowserActionResult::String(s))
                    }
                }
                Err(e) => Err(e),
            };
            let _ = ack.send(mapped);
        }
    });
}

/// Spawn the GTK-side dispatch loop. Lives on the main context.
fn run_browser_wait(
    browser: crate::ui::browser_pane::BrowserPane,
    condition: BrowserWaitCondition,
    timeout_ms: u64,
    poll_ms: u64,
    ack: tokio::sync::oneshot::Sender<Result<BrowserActionResult, String>>,
) {
    let js = Rc::<str>::from(browser_wait_js(&condition));
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let interval = Duration::from_millis(poll_ms.max(1));
    let ack = Rc::new(RefCell::new(Some(ack)));
    poll_browser_wait(browser, js, deadline, interval, ack);
}

fn poll_browser_wait(
    browser: crate::ui::browser_pane::BrowserPane,
    js: Rc<str>,
    deadline: Instant,
    interval: Duration,
    ack: Rc<RefCell<Option<tokio::sync::oneshot::Sender<Result<BrowserActionResult, String>>>>>,
) {
    if Instant::now() >= deadline {
        if let Some(ack) = ack.borrow_mut().take() {
            let _ = ack.send(Ok(BrowserActionResult::Bool(false)));
        }
        return;
    }

    let next_browser = browser.clone();
    let next_js = js.clone();
    let next_ack = ack.clone();
    browser.evaluate_js(&js, move |result| match result {
        Ok(value) if browser_js_truthy(&value) => {
            if let Some(ack) = ack.borrow_mut().take() {
                let _ = ack.send(Ok(BrowserActionResult::Bool(true)));
            }
        }
        Ok(_) => {
            if Instant::now() >= deadline {
                if let Some(ack) = ack.borrow_mut().take() {
                    let _ = ack.send(Ok(BrowserActionResult::Bool(false)));
                }
                return;
            }
            glib::timeout_add_local_once(interval, move || {
                poll_browser_wait(next_browser, next_js, deadline, interval, next_ack);
            });
        }
        Err(e) => {
            if let Some(ack) = ack.borrow_mut().take() {
                let _ = ack.send(Err(e));
            }
        }
    });
}

fn browser_wait_js(condition: &BrowserWaitCondition) -> String {
    let literal = |value: &str| serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into());
    match condition {
        BrowserWaitCondition::Selector(selector) => {
            format!("Boolean(document.querySelector({}))", literal(selector))
        }
        BrowserWaitCondition::Text(text) => format!(
            "Boolean(document.body && document.body.innerText.includes({}))",
            literal(text)
        ),
        BrowserWaitCondition::Url(url) => {
            format!("Boolean(location.href.includes({}))", literal(url))
        }
        BrowserWaitCondition::ReadyState(state) => {
            format!("document.readyState === {}", literal(state))
        }
        BrowserWaitCondition::Js(source) => format!(
            r#"
(() => {{
  const source = {};
  try {{
    const value = Function("return (" + source + ")")();
    return Boolean(typeof value === "function" ? value() : value);
  }} catch (_) {{
    return Boolean(Function(source)());
  }}
}})()
"#,
            literal(source)
        ),
    }
}

fn browser_js_truthy(value: &str) -> bool {
    !matches!(value.trim(), "" | "false" | "0" | "null" | "undefined")
}

fn run_browser_screenshot(
    browser: &crate::ui::browser_pane::BrowserPane,
    path: PathBuf,
    ack: tokio::sync::oneshot::Sender<Result<BrowserActionResult, String>>,
) {
    let cell = std::cell::Cell::new(Some(ack));
    browser.snapshot_to_png(path, move |result| {
        if let Some(ack) = cell.take() {
            let mapped = result.map(BrowserActionResult::String);
            let _ = ack.send(mapped);
        }
    });
}

pub fn spawn_dispatch_loop(rx: async_channel::Receiver<GtkCommand>, controller: WindowController) {
    glib::MainContext::default().spawn_local(async move {
        while let Ok(cmd) = rx.recv().await {
            controller.dispatch(cmd).await;
        }
    });
}

fn file_browser_return_pane(focused: Option<PaneId>, source: Option<PaneId>) -> Option<PaneId> {
    source.or(focused)
}

#[cfg(test)]
mod tests {
    #![cfg_attr(target_os = "macos", allow(dead_code, unused_imports))]

    use super::*;
    use flowmux_core::PaneContent;
    use flowmux_state::State;
    use flowmux_vcs::worktree::{WorktreeChanges, WorktreeInfo, WorktreeList};

    fn agent_bar_visible(controller: &WindowController) -> bool {
        controller.agent_bar.bar.root.property::<bool>("visible")
    }

    #[test]
    fn dirty_editor_dialog_lists_multilingual_paths_and_limits_long_lists() {
        let labels = vec![
            "문서/한국어.txt".to_string(),
            "資料/日本語.txt".to_string(),
            "emoji/🙂.txt".to_string(),
            "four.txt".to_string(),
            "five.txt".to_string(),
            "six.txt".to_string(),
            "seven.txt".to_string(),
            "eight.txt".to_string(),
            "nine.txt".to_string(),
        ];

        let body = dirty_editor_dialog_body(&labels);

        assert!(body.starts_with("9 files have unsaved changes."));
        assert!(body.contains("• 문서/한국어.txt"));
        assert!(body.contains("• 資料/日本語.txt"));
        assert!(body.contains("• emoji/🙂.txt"));
        assert!(body.contains("• … and 1 more"));
        assert!(!body.contains("nine.txt"));
        assert_eq!(
            dirty_editor_dialog_body(&["한글.txt".into()]),
            "“한글.txt” has unsaved changes."
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn dirty_editor_dialog_exposes_save_discard_and_cancel_responses() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let labels = vec!["다국어/日本語🙂.txt".to_string()];
        let (dialog, _rx) = build_dirty_editor_dialog(&labels);

        assert_eq!(dialog.default_response().as_deref(), Some("save"));
        assert_eq!(dialog.close_response(), "cancel");
        assert_eq!(
            dialog.response_appearance("discard"),
            adw::ResponseAppearance::Destructive
        );
        assert!(dialog.is_response_enabled("save"));
        assert!(dialog.is_response_enabled("discard"));
        assert!(dialog.is_response_enabled("cancel"));

        let (save_dialog, save_rx) = build_dirty_editor_dialog(&labels);
        save_dialog.emit_by_name::<()>("response", &[&"save"]);
        assert_eq!(save_rx.await.unwrap(), "save");

        let (discard_dialog, discard_rx) = build_dirty_editor_dialog(&labels);
        discard_dialog.emit_by_name::<()>("response", &[&"discard"]);
        assert_eq!(discard_rx.await.unwrap(), "discard");

        let (cancel_dialog, cancel_rx) = build_dirty_editor_dialog(&labels);
        cancel_dialog.emit_by_name::<()>("response", &[&"cancel"]);
        assert_eq!(cancel_rx.await.unwrap(), "cancel");
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn pane_zoom_view_round_trip_preserves_split_position_and_model() {
        let pane = PaneId::new();
        let workspace = workspace_with_tabbed_surface(
            pane,
            PaneSurface::terminal("zoom", None),
            PathBuf::from("/tmp/flowmux-zoom"),
        );
        let model_before = serde_json::to_string(&workspace).unwrap();

        let zoomed = gtk::Box::new(gtk::Orientation::Vertical, 0).upcast::<gtk::Widget>();
        let sibling = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let paned = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&zoomed)
            .end_child(&sibling)
            .position(321)
            .build();
        let stack = gtk::Stack::new();
        stack.add_named(&paned, Some(&workspace.id.to_string()));

        let origin = detach_pane_for_zoom(&zoomed, &stack).expect("split pane can zoom");
        assert!(paned.start_child().is_none());
        assert_eq!(stack.visible_child_name().as_deref(), Some(PANE_ZOOM_PAGE));

        restore_pane_from_zoom(&zoomed, &stack, origin);
        assert_eq!(paned.start_child().as_ref(), Some(&zoomed));
        assert_eq!(paned.position(), 321);
        assert!(stack.child_by_name(PANE_ZOOM_PAGE).is_none());
        assert_eq!(serde_json::to_string(&workspace).unwrap(), model_before);
    }

    fn workspace_with_tabbed_surface(
        pane: PaneId,
        pane_surface: PaneSurface,
        root_dir: PathBuf,
    ) -> Workspace {
        let active = pane_surface.id;
        Workspace {
            id: WorkspaceId::new(),
            name: "copy-test".into(),
            custom_title: None,
            root_dir,
            git: None,
            listening_ports: vec![],
            color: None,
            surfaces: vec![Surface {
                id: SurfaceId::new(),
                title: "copy-test".into(),
                kind: pane_surface.kind.clone(),
                root_pane: Pane::Leaf {
                    id: pane,
                    content: PaneContent::Tabs {
                        active,
                        surfaces: vec![pane_surface],
                    },
                },
            }],
        }
    }

    #[test]
    fn stored_copy_text_uses_terminal_cwd_from_state() {
        let pane = PaneId::new();
        let cwd = PathBuf::from("/tmp/flowmux-copy-path");
        let pane_surface = PaneSurface::terminal("copy", Some(cwd.clone()));
        let surface = pane_surface.id;
        let ws = workspace_with_tabbed_surface(pane, pane_surface, PathBuf::from("/tmp/root"));

        assert_eq!(
            stored_surface_copy_text_from_workspace(&ws, pane, surface),
            Some(CopyableText::stored_path(cwd))
        );
    }

    #[test]
    fn stored_terminal_cwd_uses_terminal_cwd_from_state() {
        let pane = PaneId::new();
        let cwd = PathBuf::from("/tmp/flowmux-show-folder");
        let pane_surface = PaneSurface::terminal("show", Some(cwd.clone()));
        let surface = pane_surface.id;
        let ws = workspace_with_tabbed_surface(pane, pane_surface, PathBuf::from("/tmp/root"));

        assert_eq!(
            stored_terminal_cwd_from_workspace(&ws, pane, surface),
            Some(cwd)
        );
    }

    #[test]
    fn active_surface_falls_back_to_workspace_state() {
        let pane = PaneId::new();
        let pane_surface = PaneSurface::terminal("show", Some("/tmp/flowmux-show-folder".into()));
        let surface = pane_surface.id;
        let ws = workspace_with_tabbed_surface(pane, pane_surface, PathBuf::from("/tmp/root"));

        assert_eq!(active_surface_from_workspace(&ws, pane), Some(surface));
    }

    #[test]
    fn stored_copy_text_uses_browser_url_from_state() {
        let pane = PaneId::new();
        let pane_surface = PaneSurface::browser("docs", "https://example.test/docs".into());
        let surface = pane_surface.id;
        let ws = workspace_with_tabbed_surface(pane, pane_surface, PathBuf::from("/tmp/root"));

        assert_eq!(
            stored_surface_copy_text_from_workspace(&ws, pane, surface),
            CopyableText::url("https://example.test/docs".into())
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn restore_from_store_renders_sidebar_in_persisted_workspace_order() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let store = StateStore::new_lazy(State::default());
        let first = store
            .create_workspace(
                Some("first".into()),
                PathBuf::from("/tmp/flowmux-order-first"),
            )
            .await;
        let second = store
            .create_workspace(
                Some("second".into()),
                PathBuf::from("/tmp/flowmux-order-second"),
            )
            .await;
        let third = store
            .create_workspace(
                Some("third".into()),
                PathBuf::from("/tmp/flowmux-order-third"),
            )
            .await;
        assert!(store.reorder_workspace(third, 0).await);
        store.set_active_workspace(Some(second)).await;
        assert_eq!(
            store.snapshot().await.workspace_order,
            vec![third, first, second]
        );

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.RestoreWorkspaceOrder")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store,
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );

        controller.restore_from_store().await;

        let restored: Vec<WorkspaceId> = controller
            .sidebar
            .workspace_titles()
            .borrow()
            .iter()
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(restored, vec![third, first, second]);
        assert_eq!(controller.sidebar.selected_workspace(), Some(second));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn terminal_cwd_event_updates_rendered_tab_label_and_respects_manual_rename() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-cwd-one");
        let next = std::env::temp_dir().join("flowmux-ui-cwd-two");
        let fixed_next = std::env::temp_dir().join("flowmux-ui-cwd-three");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&next).unwrap();
        std::fs::create_dir_all(&fixed_next).unwrap();

        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        let surface = ws.surfaces[0].root_pane.active_surface_id(pane).unwrap();
        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );

        controller.render_workspace(&ws);
        assert_eq!(
            controller
                .pane_registry
                .borrow()
                .surface_title_text(surface)
                .as_deref(),
            Some("flowmux-ui-cwd-on...")
        );

        controller
            .dispatch(GtkCommand::TerminalCwdChanged {
                pane,
                surface,
                cwd: next.clone(),
            })
            .await;

        assert_eq!(
            store.surface_title(pane, surface).await.as_deref(),
            Some("flowmux-ui-cwd-tw...")
        );
        assert_eq!(
            controller
                .pane_registry
                .borrow()
                .surface_title_text(surface)
                .as_deref(),
            Some("flowmux-ui-cwd-tw...")
        );

        assert_eq!(
            store.rename_surface(pane, surface, "fixed".into()).await,
            Some(ws_id)
        );
        controller
            .pane_registry
            .borrow()
            .set_surface_title(surface, "fixed");

        controller
            .dispatch(GtkCommand::TerminalCwdChanged {
                pane,
                surface,
                cwd: fixed_next.clone(),
            })
            .await;

        assert_eq!(
            store.surface_title(pane, surface).await.as_deref(),
            Some("fixed")
        );
        assert_eq!(
            controller
                .pane_registry
                .borrow()
                .surface_title_text(surface)
                .as_deref(),
            Some("fixed")
        );

        let refreshed = store.get_workspace(ws_id).await.unwrap();
        let PaneContent::Tabs { surfaces, .. } = (match &refreshed.surfaces[0].root_pane {
            flowmux_core::Pane::Leaf { content, .. } => content,
            flowmux_core::Pane::Split { .. } => panic!("expected single leaf"),
        }) else {
            panic!("expected tabs")
        };
        assert!(matches!(
            &surfaces[0].kind,
            flowmux_core::SurfaceKind::Terminal { cwd: Some(cwd), .. } if cwd == &fixed_next
        ));
        assert!(surfaces[0].title_locked);
    }

    /// Verify that focus changes, tab activation, and RefreshWindowTitle update
    /// adw::ApplicationWindow.title to "flowmux - {focused tab name}". With no
    /// focus, it falls back to plain "flowmux".
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn refresh_window_title_uses_focused_pane_active_surface() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-window-title");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        let surface = ws.surfaces[0].root_pane.active_surface_id(pane).unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.WindowTitle")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);

        // Initial state with no focus falls back to plain "flowmux".
        controller.focused_pane.set(None);
        controller.dispatch(GtkCommand::RefreshWindowTitle).await;
        assert_eq!(
            controller.window.title().map(|s| s.to_string()).as_deref(),
            Some("flowmux")
        );

        // With focus, the title becomes "flowmux - {tab name}".
        let expected_tab_name = store.surface_title(pane, surface).await.unwrap();
        controller.focused_pane.set(Some(pane));
        controller.dispatch(GtkCommand::RefreshWindowTitle).await;
        assert_eq!(
            controller.window.title().map(|s| s.to_string()),
            Some(format!("flowmux - {expected_tab_name}"))
        );

        // After RenameSurface dispatch, the window title follows the new name.
        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::RenameSurface {
                pane,
                surface,
                title: "Custom".into(),
                ack: ack_tx,
            })
            .await;
        let _ = ack_rx.await;
        assert_eq!(
            controller.window.title().map(|s| s.to_string()).as_deref(),
            Some("flowmux - Custom")
        );
    }

    /// Verify that `BrowserUriChanged` dispatch stores the last navigated URL.
    /// To test only store interaction without launching webkit::WebView, create
    /// a browser surface in state with add_browser_surface_to_pane first.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn browser_uri_changed_dispatch_persists_url_in_state() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-browser-url");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        // Add the browser surface directly to avoid WebKit init cost.
        let (_, browser) = store
            .add_browser_surface_to_pane(pane, "https://before.test".into())
            .await
            .unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.BrowserUri")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );

        controller
            .dispatch(GtkCommand::BrowserUriChanged {
                pane,
                surface: browser,
                url: "https://after.test/page".into(),
            })
            .await;

        let updated = store.get_workspace(ws_id).await.unwrap();
        let s = updated.surfaces[0]
            .root_pane
            .find_surface(pane, browser)
            .unwrap();
        assert!(matches!(
            &s.kind,
            flowmux_core::SurfaceKind::Browser { initial_url: Some(u) } if u == "https://after.test/page"
        ));
    }

    /// `BrowserTitleChanged` dispatch updates both store and tab label, while
    /// user-renamed surfaces remain protected from automatic updates.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn browser_title_changed_dispatch_updates_state_but_skips_locked() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-browser-title");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        let (_, browser_a) = store
            .add_browser_surface_to_pane(pane, "https://a.test".into())
            .await
            .unwrap();
        let (_, browser_b) = store
            .add_browser_surface_to_pane(pane, "https://b.test".into())
            .await
            .unwrap();
        store
            .rename_surface(pane, browser_b, "Pinned".into())
            .await
            .unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.BrowserTitle")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );

        // A: title_locked=false -> updated.
        controller
            .dispatch(GtkCommand::BrowserTitleChanged {
                pane,
                surface: browser_a,
                title: "Hello — Page A".into(),
            })
            .await;
        assert_eq!(
            store.surface_title(pane, browser_a).await.as_deref(),
            Some("Hello — Page A")
        );

        // B: title_locked=true -> stays "Pinned".
        controller
            .dispatch(GtkCommand::BrowserTitleChanged {
                pane,
                surface: browser_b,
                title: "Should not stick".into(),
            })
            .await;
        assert_eq!(
            store.surface_title(pane, browser_b).await.as_deref(),
            Some("Pinned")
        );
    }

    /// `TerminalTitleChanged` dispatch updates the tab label from OSC 0/2
    /// titles. Empty strings are ignored, and title_locked=true surfaces are
    /// protected because user rename wins.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn terminal_title_changed_dispatch_updates_tab_and_skips_locked() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-terminal-title");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        // Reuse the terminal surface automatically created with the first workspace.
        let surface_a = match &ws.surfaces[0].root_pane {
            flowmux_core::Pane::Leaf {
                content: flowmux_core::PaneContent::Tabs { surfaces, .. },
                ..
            } => surfaces[0].id,
            _ => unreachable!("default workspace pane is a tabbed leaf"),
        };
        let (_, surface_locked) = store
            .add_terminal_surface_to_pane(pane, Some(root.clone()))
            .await
            .unwrap();
        store
            .rename_surface(pane, surface_locked, "Pinned-shell".into())
            .await
            .unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.TerminalTitle")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );

        // 1. Normal OSC 2 -> tab label updates.
        controller
            .dispatch(GtkCommand::TerminalTitleChanged {
                pane,
                surface: surface_a,
                title: "vi src/main.rs".into(),
            })
            .await;
        assert_eq!(
            store.surface_title(pane, surface_a).await.as_deref(),
            Some("vi src/main.rs")
        );

        // 2. Empty string -> ignored for shell exit / OSC reset.
        controller
            .dispatch(GtkCommand::TerminalTitleChanged {
                pane,
                surface: surface_a,
                title: "".into(),
            })
            .await;
        assert_eq!(
            store.surface_title(pane, surface_a).await.as_deref(),
            Some("vi src/main.rs"),
            "empty OSC title should not erase the active label"
        );

        // 3. Whitespace-only title -> ignored.
        controller
            .dispatch(GtkCommand::TerminalTitleChanged {
                pane,
                surface: surface_a,
                title: "   \t".into(),
            })
            .await;
        assert_eq!(
            store.surface_title(pane, surface_a).await.as_deref(),
            Some("vi src/main.rs")
        );

        // 4. title_locked=true -> ignored.
        controller
            .dispatch(GtkCommand::TerminalTitleChanged {
                pane,
                surface: surface_locked,
                title: "Should not stick".into(),
            })
            .await;
        assert_eq!(
            store.surface_title(pane, surface_locked).await.as_deref(),
            Some("Pinned-shell")
        );
    }

    /// Regression test: bash emits OSC 7 (cwd) and OSC 0/2 prompt-shaped window
    /// titles like `user@host: /path` on each prompt. OSC 0/2 must not overwrite
    /// cwd-driven folder labels. Instead, cwd changes should update both tab and
    /// window title to the folder name, while external program titles such as
    /// vi/codex should still pass through.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn terminal_cwd_change_updates_tab_and_window_title_and_ignores_shell_ps1_echo() {
        adw::init().expect("libadwaita should initialize in GTK test");
        // Use absolute paths under /tmp to avoid $HOME effects. Absolute-path
        // PS1 matching is sufficient to verify this flow.
        let initial = std::env::temp_dir().join("flowmux-ui-cwd-flow-one");
        let next = std::env::temp_dir().join("flowmux-ui-cwd-flow-two");
        std::fs::create_dir_all(&initial).unwrap();
        std::fs::create_dir_all(&next).unwrap();

        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), initial.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        let surface = ws.surfaces[0].root_pane.active_surface_id(pane).unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.CwdFlow")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);
        controller.focused_pane.set(Some(pane));
        controller.dispatch(GtkCommand::RefreshWindowTitle).await;

        // Initial: tab/window title is the workspace root folder name.
        let initial_title = store.surface_title(pane, surface).await.unwrap();
        assert_eq!(initial_title, "flowmux-ui-cwd-fl...");
        assert_eq!(
            controller.window.title().map(|s| s.to_string()),
            Some(format!("flowmux - {initial_title}"))
        );

        // User runs `cd flowmux-ui-cwd-flow-two`. In bash, PROMPT_COMMAND
        // (OSC 7 cwd) emits before PS1 expansion (OSC 0/2 window title), so
        // dispatch in the same order.
        controller
            .dispatch(GtkCommand::TerminalCwdChanged {
                pane,
                surface,
                cwd: next.clone(),
            })
            .await;
        assert_eq!(
            store.surface_title(pane, surface).await.as_deref(),
            Some("flowmux-ui-cwd-fl..."),
            "OSC 7 cwd changes update the tab label to the new folder name",
        );
        assert_eq!(
            controller.window.title().map(|s| s.to_string()),
            Some("flowmux - flowmux-ui-cwd-fl...".into()),
            "the window title follows the new tab name",
        );

        // bash then emits OSC 0/2 (`user@host: /new/path`) for the same prompt.
        // Since cwd is already updated, this must be recognized as a PS1 echo
        // and ignored. The regression would freeze the label in PS1 form here.
        controller
            .dispatch(GtkCommand::TerminalTitleChanged {
                pane,
                surface,
                title: format!("junsu@host: {}", next.display()),
            })
            .await;
        assert_eq!(
            store.surface_title(pane, surface).await.as_deref(),
            Some("flowmux-ui-cwd-fl..."),
            "shell PS1 OSC 0/2 must not overwrite cwd-driven labels",
        );
        assert_eq!(
            controller.window.title().map(|s| s.to_string()),
            Some("flowmux - flowmux-ui-cwd-fl...".into()),
        );

        // If bash draws another prompt without cd, such as after an empty Enter,
        // the same PS1 echo with debian_chroot prefix and no-space variant must
        // also be ignored.
        controller
            .dispatch(GtkCommand::TerminalTitleChanged {
                pane,
                surface,
                title: format!("(jammy)junsu@host:{}", next.display()),
            })
            .await;
        assert_eq!(
            store.surface_title(pane, surface).await.as_deref(),
            Some("flowmux-ui-cwd-fl...")
        );

        // External program titles, such as vi, still apply to both tab and window.
        controller
            .dispatch(GtkCommand::TerminalTitleChanged {
                pane,
                surface,
                title: "vim src/main.rs".into(),
            })
            .await;
        assert_eq!(
            store.surface_title(pane, surface).await.as_deref(),
            Some("vim src/main.rs"),
        );
        assert_eq!(
            controller.window.title().map(|s| s.to_string()).as_deref(),
            Some("flowmux - vim src/main.rs"),
        );
    }

    /// When a new tab is added (NewSurface), that tab becomes active and the
    /// window title updates to the new tab name instead of keeping the previous
    /// active tab name.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn new_surface_dispatch_updates_window_title_to_new_tab() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-window-title-newtab");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.WindowTitleNewTab")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);
        controller.focused_pane.set(Some(pane));
        controller.dispatch(GtkCommand::RefreshWindowTitle).await;
        let initial = controller.window.title().map(|s| s.to_string());

        // dispatch creates a new terminal surface itself, attaches it, and then
        // calls refresh_window_title.
        controller.dispatch(GtkCommand::NewSurface { pane }).await;

        let title_now = controller.window.title().map(|s| s.to_string());
        assert!(title_now.is_some());
        assert!(
            title_now.as_deref().unwrap().starts_with("flowmux - "),
            "title should keep the flowmux prefix, got {title_now:?}"
        );
        // If the new tab is active, that surface title is the window title source.
        let active = controller
            .pane_registry
            .borrow()
            .active_surface(pane)
            .expect("active surface must be tracked");
        let expected = store.surface_title(pane, active).await.unwrap();
        assert_eq!(
            title_now,
            Some(format!("flowmux - {expected}")),
            "window title should follow the newly-active tab — initial was {initial:?}"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn open_tig_dispatch_adds_active_tab_to_target_pane_with_its_cwd() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-open-tig");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let workspace = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let initial = store.get_workspace(workspace).await.unwrap();
        let pane = initial.surfaces[0].root_pane.first_leaf_id().unwrap();
        let initial_surface = initial.surfaces[0]
            .root_pane
            .active_surface_id(pane)
            .unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.OpenTig")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&initial);
        controller.focused_pane.set(Some(pane));

        controller.dispatch(GtkCommand::OpenTig { pane }).await;

        let updated = store.get_workspace(workspace).await.unwrap();
        let content = updated.surfaces[0]
            .root_pane
            .find_leaf_content(pane)
            .expect("target pane should remain present");
        let PaneContent::Tabs { active, surfaces } = content else {
            panic!("target pane should contain tabs");
        };
        assert_eq!(surfaces.len(), 2);
        assert_ne!(active, initial_surface);
        let tig_surface = surfaces
            .iter()
            .find(|surface| surface.id == active)
            .expect("new tig tab should be active");
        assert!(matches!(
            &tig_surface.kind,
            SurfaceKind::Terminal { cwd: Some(cwd), .. } if cwd == &root
        ));
        assert!(controller
            .pane_registry
            .borrow()
            .terminals
            .contains_key(&active));
    }

    /// ActivateSurface dispatch alone recomputes the window title from the active tab.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn activate_surface_dispatch_refreshes_window_title() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-window-title-activate");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        let original_surface = ws.surfaces[0].root_pane.active_surface_id(pane).unwrap();
        let (_, browser) = store
            .add_browser_surface_to_pane(pane, "https://docs.test".into())
            .await
            .unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.WindowTitleActivate")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        let ws = store.get_workspace(ws_id).await.unwrap();
        controller.render_workspace(&ws);
        controller.focused_pane.set(Some(pane));

        controller
            .dispatch(GtkCommand::ActivateSurface {
                pane,
                surface: original_surface,
            })
            .await;
        let term_title = store.surface_title(pane, original_surface).await.unwrap();
        assert_eq!(
            controller.window.title().map(|s| s.to_string()),
            Some(format!("flowmux - {term_title}"))
        );

        controller
            .dispatch(GtkCommand::ActivateSurface {
                pane,
                surface: browser,
            })
            .await;
        // add_browser_surface_to_pane stores browser surfaces as "Browser".
        assert_eq!(
            controller.window.title().map(|s| s.to_string()).as_deref(),
            Some("flowmux - Browser")
        );
    }

    /// Verify that ReorderSurface dispatch updates both store and PaneRegistry,
    /// preserves the active tab, treats same-position moves as no-ops, and does
    /// not affect tabs in other panes.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn reorder_surface_dispatch_updates_store_and_widget_order() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-reorder-surface");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        let first = ws.surfaces[0].root_pane.active_surface_id(pane).unwrap();
        let (_, second) = store
            .add_terminal_surface_to_pane(pane, None)
            .await
            .unwrap();
        let (_, browser) = store
            .add_browser_surface_to_pane(pane, "https://three.test".into())
            .await
            .unwrap();
        // Restore the active tab to first; the browser was added last and became active.
        store.set_active_surface(pane, first).await;

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.ReorderSurface")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        let ws = store.get_workspace(ws_id).await.unwrap();
        controller.render_workspace(&ws);

        // first (index 0) -> last (index 2).
        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::ReorderSurface {
                pane,
                surface: first,
                target_index: 2,
                ack: ack_tx,
            })
            .await;
        ack_rx.await.unwrap().unwrap();

        // Check store-side order.
        let snap = store.get_workspace(ws_id).await.unwrap();
        let flowmux_core::Pane::Leaf {
            content: flowmux_core::PaneContent::Tabs { active, surfaces },
            ..
        } = &snap.surfaces[0].root_pane
        else {
            panic!("expected tabs")
        };
        assert_eq!(
            surfaces.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![second, browser, first],
            "store reorder failed"
        );
        // The active tab remains first.
        assert_eq!(*active, first);

        // Dispatch same-position again -> store returns None, so widgets stay unchanged.
        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::ReorderSurface {
                pane,
                surface: first,
                target_index: 2,
                ack: ack_tx,
            })
            .await;
        ack_rx.await.unwrap().unwrap();
        let snap = store.get_workspace(ws_id).await.unwrap();
        let flowmux_core::Pane::Leaf {
            content: flowmux_core::PaneContent::Tabs { surfaces, .. },
            ..
        } = &snap.surfaces[0].root_pane
        else {
            panic!("expected tabs")
        };
        assert_eq!(
            surfaces.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![second, browser, first]
        );

        // Out-of-range index clamps to the end, which is its current position, so no-op.
        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::ReorderSurface {
                pane,
                surface: first,
                target_index: 999,
                ack: ack_tx,
            })
            .await;
        ack_rx.await.unwrap().unwrap();

        // Move browser, currently in the middle, to the front.
        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::ReorderSurface {
                pane,
                surface: browser,
                target_index: 0,
                ack: ack_tx,
            })
            .await;
        ack_rx.await.unwrap().unwrap();
        let snap = store.get_workspace(ws_id).await.unwrap();
        let flowmux_core::Pane::Leaf {
            content: flowmux_core::PaneContent::Tabs { active, surfaces },
            ..
        } = &snap.surfaces[0].root_pane
        else {
            panic!("expected tabs")
        };
        assert_eq!(
            surfaces.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![browser, second, first]
        );
        // Active remains first.
        assert_eq!(*active, first);
    }

    /// `poll_terminal_cwds` is the safety net for shells without OSC 7. Directly
    /// calling it should update store/tab labels for registered (pane, surface,
    /// cwd) entries and no-op when nothing changed. This is the body called by
    /// the one-second timer handler (`install_cwd_polling_fallback`) each tick.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn poll_terminal_cwds_picks_up_changes_without_osc7_event() {
        adw::init().expect("libadwaita should initialize in GTK test");
        // Folder names must still differ after 15-character truncation, so make
        // their prefixes different enough for assert_ne! to be meaningful.
        let initial = std::env::temp_dir().join("alpha-poll-cwd");
        std::fs::create_dir_all(&initial).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), initial.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        let surface = ws.surfaces[0].root_pane.active_surface_id(pane).unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.CwdPoll")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);
        controller.focused_pane.set(Some(pane));

        // With no change, poll is a no-op because store update_surface_cwd returns
        // false. Dispatch the same cwd again to confirm the flow stays intact.
        controller
            .dispatch(GtkCommand::TerminalCwdChanged {
                pane,
                surface,
                cwd: initial.clone(),
            })
            .await;
        let stable = store.surface_title(pane, surface).await;

        // Simulate poll discovering a new cwd by dispatching TerminalCwdChanged.
        // It goes through the same update_terminal_cwd path as the poll body.
        let next = std::env::temp_dir().join("bravo-poll-cwd");
        std::fs::create_dir_all(&next).unwrap();
        controller
            .dispatch(GtkCommand::TerminalCwdChanged {
                pane,
                surface,
                cwd: next.clone(),
            })
            .await;
        let updated = store.surface_title(pane, surface).await;
        assert_ne!(stable, updated, "tab label updates to the new folder name");
        assert_eq!(updated.as_deref(), Some("bravo-poll-cwd"));
        assert_eq!(
            controller.window.title().map(|s| s.to_string()).as_deref(),
            Some("flowmux - bravo-poll-cwd")
        );

        // poll_terminal_cwds is the OSC-7-less safety net: it reads each pane's
        // real cwd (the shell spawned in `initial` and emitted no OSC 7, so
        // /proc/<pid>/cwd reports `initial`). Polling reconciles the label from
        // the simulated event back to the shell's actual directory.
        controller.poll_terminal_cwds().await;
        let polled = store.surface_title(pane, surface).await;
        assert_eq!(
            polled.as_deref(),
            Some("alpha-poll-cwd"),
            "poll reflects the shell's real cwd via /proc when no OSC 7 was emitted"
        );

        // A second poll with no real cwd change is a no-op (store reports no
        // change, so the label and window title stay put).
        controller.poll_terminal_cwds().await;
        assert_eq!(
            store.surface_title(pane, surface).await,
            polled,
            "a second poll with no real change leaves the label unchanged"
        );
    }

    /// Regression guard: OSC 0/2 titles from external programs such as vi or
    /// claude must not be reverted to the folder name by the one-second cwd
    /// polling fallback. poll_terminal_cwds passes the same cwd each tick, so
    /// that path must never touch `surface.title`.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn program_title_persists_across_cwd_polling() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let cwd = std::env::temp_dir().join("flowmux-program-title-poll");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store.create_workspace(Some("ui".into()), cwd.clone()).await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        let surface = ws.surfaces[0].root_pane.active_surface_id(pane).unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.ProgramTitlePoll")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);
        controller.focused_pane.set(Some(pane));
        controller.dispatch(GtkCommand::RefreshWindowTitle).await;

        // Enter an external program (claude): OSC 2 emits "Claude Code".
        controller
            .dispatch(GtkCommand::TerminalTitleChanged {
                pane,
                surface,
                title: "Claude Code".into(),
            })
            .await;
        assert_eq!(
            store.surface_title(pane, surface).await.as_deref(),
            Some("Claude Code")
        );
        assert_eq!(
            controller.window.title().map(|s| s.to_string()).as_deref(),
            Some("flowmux - Claude Code")
        );

        // Polling fires now and sees the same cwd because the program did not cd.
        // Running polling twice in a row must not disturb the title.
        controller.poll_terminal_cwds().await;
        controller.poll_terminal_cwds().await;
        assert_eq!(
            store.surface_title(pane, surface).await.as_deref(),
            Some("Claude Code"),
            "program titles must not be reverted to folder names by cwd polling",
        );
        assert_eq!(
            controller.window.title().map(|s| s.to_string()).as_deref(),
            Some("flowmux - Claude Code")
        );

        // After claude exits, moving to another folder naturally restores a folder label.
        let next = std::env::temp_dir().join("flowmux-program-title-after");
        std::fs::create_dir_all(&next).unwrap();
        controller
            .dispatch(GtkCommand::TerminalCwdChanged {
                pane,
                surface,
                cwd: next.clone(),
            })
            .await;
        assert_eq!(
            store.surface_title(pane, surface).await.as_deref(),
            Some("flowmux-program-t...")
        );
    }

    /// Regression guard: the Alt+arrow candidate filter must only consider panes
    /// inside the same workspace. Verify PaneRegistry::pane_ids_in_workspace
    /// filters out panes from other workspaces, because focus_in_direction builds
    /// its candidate list through this function.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn pane_ids_in_workspace_isolates_other_workspaces() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root_a = std::env::temp_dir().join("flowmux-ws-iso-a");
        let root_b = std::env::temp_dir().join("flowmux-ws-iso-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_a_id = store.create_workspace(Some("a".into()), root_a).await;
        let ws_b_id = store.create_workspace(Some("b".into()), root_b).await;
        let ws_a = store.get_workspace(ws_a_id).await.unwrap();
        let ws_b = store.get_workspace(ws_b_id).await.unwrap();
        let pane_a = ws_a.surfaces[0].root_pane.first_leaf_id().unwrap();
        let pane_b = ws_b.surfaces[0].root_pane.first_leaf_id().unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.WsIsolation")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws_a);
        controller.render_workspace(&ws_b);

        let r = controller.pane_registry.borrow();
        let in_a: std::collections::HashSet<_> = r.pane_ids_in_workspace(ws_a_id).collect();
        let in_b: std::collections::HashSet<_> = r.pane_ids_in_workspace(ws_b_id).collect();

        assert!(in_a.contains(&pane_a));
        assert!(
            !in_a.contains(&pane_b),
            "ws_a candidates must not include ws_b panes"
        );
        assert!(in_b.contains(&pane_b));
        assert!(
            !in_b.contains(&pane_a),
            "ws_b candidates must not include ws_a panes"
        );
        assert_eq!(r.workspace_of_pane(pane_a), Some(ws_a_id));
        assert_eq!(r.workspace_of_pane(pane_b), Some(ws_b_id));
    }

    /// If the user clicks only a workspace in the side panel and focused_pane is
    /// None, pressing any Alt+arrow direction should make the dispatcher focus
    /// the active workspace's first leaf pane.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn focus_direction_from_none_falls_back_to_first_leaf_of_active_workspace() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-focus-fallback");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store.create_workspace(Some("ui".into()), root).await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let first_leaf = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        store.set_active_workspace(Some(ws_id)).await;

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.FocusFallback")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);
        // Reproduce a state where the user clicked a workspace but no pane is focused yet.
        controller.focused_pane.set(None);

        controller
            .dispatch(GtkCommand::FocusDirection {
                from: None,
                dir: FocusDir::Left,
            })
            .await;
        // grab_focus runs inside the idle_add_local_once queued by
        // focus_first_leaf_of. The idle queue is FIFO, so queueing another idle
        // and waiting on a oneshot means the earlier grab_focus has already run,
        // without manually iterating the main loop in an async GTK test.
        let (idle_tx, idle_rx) = oneshot::channel();
        glib::idle_add_local_once(move || {
            let _ = idle_tx.send(());
        });
        let _ = idle_rx.await;

        assert_eq!(
            controller.focused_pane.get(),
            Some(first_leaf),
            "focused_pane should become the active workspace's first leaf",
        );
    }

    /// Regression guard: with multiple workspaces, clicking the second workspace
    /// in the side panel and pressing Alt+arrow used to focus a pane in the
    /// first workspace and start movement from there.
    ///
    /// Side-panel clicks dispatch GtkCommand::ActivateWorkspace, and the
    /// dispatcher calls activate_workspace. Inside it, focus_first_leaf_of queues
    /// grab_focus on the new workspace's first leaf; that grab_focus updates
    /// focused_pane through on_focus. If this flow breaks, focused_pane still
    /// points at the previous workspace and Alt+arrow starts from the wrong pane.
    #[test]
    fn file_browser_return_pane_prefers_saved_source_when_no_pane_is_focused() {
        let focused = PaneId::new();
        let source = PaneId::new();

        assert_eq!(file_browser_return_pane(None, Some(source)), Some(source));
        assert_eq!(
            file_browser_return_pane(Some(focused), Some(source)),
            Some(source)
        );
        assert_eq!(file_browser_return_pane(Some(focused), None), Some(focused));
    }

    fn sample_worktree_list(root: &str) -> WorktreeList {
        let root = PathBuf::from(root);
        WorktreeList {
            repository_root: root.clone(),
            current_worktree: root.clone(),
            items: vec![WorktreeInfo {
                path: root,
                branch: Some("main".into()),
                head: "1234567890abcdef".into(),
                commit_subject: Some("sample commit".into()),
                commit_time: Some(1_700_000_000),
                changes: Some(WorktreeChanges::default()),
                is_main: true,
                is_current: true,
                is_bare: false,
                lock_reason: None,
                prunable_reason: None,
            }],
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn stale_worktree_result_cannot_replace_newer_repository() {
        let (controller, _workspace, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.WorktreeStale").await;
        controller.worktrees.panel.show_loading();
        controller.worktrees.source_pane.set(Some(pane));
        controller.worktrees.generation.set(2);

        controller
            .apply_worktrees_loaded(1, Ok(sample_worktree_list("/old")))
            .await;
        assert_ne!(
            controller.worktrees.panel.repository_name(),
            Some("old".into())
        );

        controller
            .apply_worktrees_loaded(2, Ok(sample_worktree_list("/new")))
            .await;
        assert_eq!(
            controller.worktrees.panel.repository_name(),
            Some("new".into())
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn duplicate_worktree_refresh_is_coalesced_while_loading() {
        let (controller, _workspace, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.WorktreeRefreshCoalesce")
                .await;
        let start = controller.file_browser_root_for_pane(pane).await.unwrap();
        let start = std::fs::canonicalize(&start).unwrap_or(start);
        controller.worktrees.panel.show_loading();
        controller.worktrees.source_pane.set(Some(pane));
        *controller.worktrees.source_directory.borrow_mut() = Some(start);
        controller.worktrees.loading.set(true);
        controller.worktrees.generation.set(7);

        controller.refresh_worktrees(true).await;

        assert_eq!(controller.worktrees.generation.get(), 7);

        controller.worktrees.loading.set(false);
        controller.refresh_worktrees(true).await;
        assert_eq!(controller.worktrees.generation.get(), 8);
        assert!(!controller.worktrees.loading.get());
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn refreshed_worktree_rows_preserve_inflight_removal() {
        let (controller, _workspace, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.WorktreeRemovalRefresh")
                .await;
        let list = sample_worktree_list("/repo");
        let path = list.items[0].path.clone();
        controller
            .worktrees
            .removals_in_progress
            .borrow_mut()
            .insert(path.clone());
        controller.worktrees.panel.show_loading();
        controller.worktrees.source_pane.set(Some(pane));
        controller.worktrees.generation.set(1);

        controller.apply_worktrees_loaded(1, Ok(list)).await;

        assert!(
            controller
                .worktrees
                .panel
                .row_for_path(&path)
                .unwrap()
                .operation_in_progress
        );
        drop(controller);
        glib::timeout_future(std::time::Duration::from_millis(50)).await;
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn right_tool_layout_places_worktrees_before_file_browser() {
        let (controller, _workspace, _pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.WorktreeLayout").await;
        assert_eq!(
            controller.right_tool_order_for_test(),
            ["content", "worktrees", "files"]
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn alt_right_enters_worktrees_then_files_and_alt_left_reverses() {
        let (controller, _workspace, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.WorktreeFocusOrder").await;
        controller.worktrees.panel.widget().set_visible(true);
        controller.file_browser.panel.widget().set_visible(true);
        controller.window.set_default_size(900, 600);
        controller.window.present();
        glib::timeout_future(std::time::Duration::from_millis(50)).await;
        controller.focused_pane.set(Some(pane));
        controller.worktrees.source_pane.set(Some(pane));
        controller.file_browser.source_pane.set(Some(pane));

        controller
            .focus_direction_from_command(Some(pane), FocusDir::Right)
            .await;
        assert!(controller.worktrees.active.get());
        assert!(!controller.file_browser.active.get());

        controller
            .focus_direction_from_command(Some(pane), FocusDir::Right)
            .await;
        assert!(!controller.worktrees.active.get());
        assert!(controller.file_browser.active.get());

        controller.focus_out_of_file_browser(FocusDir::Left);
        assert!(controller.worktrees.active.get());
        assert!(!controller.file_browser.active.get());

        controller.focus_out_of_worktree_panel(FocusDir::Left);
        assert!(!controller.worktrees.active.get());
        assert_eq!(controller.focused_pane.get(), Some(pane));
        controller.window.close();
        glib::timeout_future(std::time::Duration::from_millis(50)).await;
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn closed_worktree_panel_is_skipped() {
        let (controller, _workspace, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.WorktreeFocusSkip").await;
        controller.worktrees.panel.widget().set_visible(false);
        controller.file_browser.panel.widget().set_visible(true);
        controller.window.set_default_size(900, 600);
        controller.window.present();
        glib::timeout_future(std::time::Duration::from_millis(50)).await;
        controller.focused_pane.set(Some(pane));
        controller
            .focus_direction_from_command(Some(pane), FocusDir::Right)
            .await;
        assert!(controller.file_browser.active.get());
        controller.window.close();
        glib::timeout_future(std::time::Duration::from_millis(50)).await;
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn toggle_worktree_panel_command_shows_and_hides_for_focused_pane() {
        let (controller, _workspace, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.WorktreeToggle").await;
        controller.window.present();
        glib::timeout_future(std::time::Duration::from_millis(50)).await;
        controller.focused_pane.set(Some(pane));

        controller
            .dispatch(GtkCommand::ToggleWorktreePanel { pane: None })
            .await;
        assert_eq!(
            controller.worktrees.source_pane.get(),
            Some(pane),
            "toggle command did not record its focused source pane"
        );
        assert!(
            controller.worktrees.panel.is_open(),
            "toggle command did not enter the panel's open state"
        );
        assert!(
            controller.worktrees.panel.widget().is_visible(),
            "toggle command recorded its source but did not reveal the panel"
        );

        controller
            .dispatch(GtkCommand::ToggleWorktreePanel { pane: None })
            .await;
        assert!(!controller.worktrees.panel.widget().is_visible());
        assert_eq!(controller.focused_pane.get(), Some(pane));
        controller.window.close();
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn file_browser_focus_out_left_restores_saved_source_focus() {
        let (controller, _ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.FileBrowserFocusOut").await;

        controller.file_browser.source_pane.set(Some(pane));
        controller.focused_pane.set(None);

        controller.focus_out_of_file_browser(FocusDir::Left);

        assert_eq!(controller.focused_pane.get(), Some(pane));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn file_browser_focus_direction_command_uses_file_browser_focus_out() {
        let (controller, _ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.FileBrowserFocusCommand")
                .await;

        controller.file_browser.panel.widget().set_visible(true);
        controller.file_browser.source_pane.set(Some(pane));

        // No pane focused (e.g. after a side-panel click): the global FocusDirection
        // command is the only way out of the browser, so it must focus-out.
        controller.file_browser.active.set(true);
        controller.focused_pane.set(None);
        controller
            .dispatch(GtkCommand::FocusDirection {
                from: None,
                dir: FocusDir::Left,
            })
            .await;

        assert_eq!(controller.focused_pane.get(), Some(pane));
        assert!(!controller.file_browser.active.get());
    }

    /// Regression: with a pane focused and the browser merely open, the first
    /// Alt+Right must move INTO the browser. The old guard ran a no-op focus-out
    /// when `from == source_pane`, which swallowed the first press (double-press bug).
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn file_browser_alt_right_enters_browser_on_first_press() {
        let (controller, _ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.FileBrowserAltRightIn").await;

        controller.window.set_default_size(900, 600);
        controller.window.present();
        glib::timeout_future(std::time::Duration::from_millis(50)).await;

        controller.file_browser.panel.widget().set_visible(true);
        controller.file_browser.source_pane.set(Some(pane));
        // Panel open but the terminal pane holds focus: active is false.
        controller.file_browser.active.set(false);
        controller.focused_pane.set(Some(pane));

        controller
            .dispatch(GtkCommand::FocusDirection {
                from: Some(pane),
                dir: FocusDir::Right,
            })
            .await;

        // First Alt+Right enters the browser: focus_file_browser sets active true.
        // The old buggy guard ran a no-op focus-out instead, needing a second press.
        assert!(controller.file_browser.active.get());
        assert_eq!(controller.file_browser.source_pane.get(), Some(pane));
    }

    /// With two side-by-side panes and the browser docked on the right, Alt+Left
    /// out of the browser lands on the adjacent (source) pane — the one touching
    /// the browser — not its far-side neighbour.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn file_browser_alt_left_focuses_adjacent_source_pane() {
        let (controller, _ws_id, pane_a) =
            build_single_workspace_controller("com.flowmux.App.UiTest.FileBrowserAltLeft").await;
        let (split_ack, split_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::SplitFocused {
                pane: pane_a,
                direction: SplitDirection::Vertical,
                ack: split_ack,
            })
            .await;
        let pane_b = split_rx
            .await
            .expect("split ack should be sent")
            .expect("split should succeed");

        controller.window.set_default_size(900, 600);
        controller.window.present();
        glib::timeout_future(std::time::Duration::from_millis(50)).await;

        // Browser opened from pane_b (the right pane, adjacent to the browser).
        controller.file_browser.source_pane.set(Some(pane_b));
        controller.file_browser.active.set(true);
        controller.focused_pane.set(Some(pane_b));

        controller.focus_out_of_file_browser(FocusDir::Left);

        // Lands on the adjacent pane_b, not its left neighbour pane_a.
        assert_eq!(controller.focused_pane.get(), Some(pane_b));
        assert!(!controller.file_browser.active.get());
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn file_browser_state_is_scoped_per_pane() {
        let (controller, ws_id, pane_a) =
            build_single_workspace_controller("com.flowmux.App.UiTest.FileBrowserPaneState").await;
        let (split_ack, split_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::SplitFocused {
                pane: pane_a,
                direction: SplitDirection::Vertical,
                ack: split_ack,
            })
            .await;
        let pane_b = split_rx
            .await
            .expect("split ack should be sent")
            .expect("split should create a second pane");
        let ws = controller.store.get_workspace(ws_id).await.unwrap();
        let surface_a = ws.surfaces[0].root_pane.active_surface_id(pane_a).unwrap();
        let surface_b = ws.surfaces[0].root_pane.active_surface_id(pane_b).unwrap();

        let root_a = std::env::temp_dir().join("flowmux-file-browser-pane-state-a");
        let root_b = std::env::temp_dir().join("flowmux-file-browser-pane-state-b");
        std::fs::create_dir_all(root_a.join("expanded-a")).unwrap();
        std::fs::write(root_a.join("expanded-a/child.txt"), "child").unwrap();
        std::fs::write(root_a.join("a.txt"), "a").unwrap();
        std::fs::create_dir_all(root_b.join("expanded-b")).unwrap();
        std::fs::write(root_b.join("expanded-b/child.txt"), "child").unwrap();
        std::fs::write(root_b.join("b.txt"), "b").unwrap();
        controller
            .store
            .update_surface_cwd(pane_a, surface_a, root_a.clone())
            .await;
        controller
            .store
            .update_surface_cwd(pane_b, surface_b, root_b.clone())
            .await;
        let ws = controller.store.get_workspace(ws_id).await.unwrap();
        controller.render_workspace(&ws);
        controller
            .dispatch(GtkCommand::TerminalCwdChanged {
                pane: pane_a,
                surface: surface_a,
                cwd: root_a.clone(),
            })
            .await;
        controller
            .dispatch(GtkCommand::TerminalCwdChanged {
                pane: pane_b,
                surface: surface_b,
                cwd: root_b.clone(),
            })
            .await;
        let registry_surface_a = controller
            .pane_registry
            .borrow()
            .active_surface(pane_a)
            .unwrap();
        let registry_surface_b = controller
            .pane_registry
            .borrow()
            .active_surface(pane_b)
            .unwrap();
        controller
            .pane_registry
            .borrow_mut()
            .terminals
            .remove(&registry_surface_a);
        controller
            .pane_registry
            .borrow_mut()
            .terminals
            .remove(&registry_surface_b);

        controller.file_browser.pane_states.borrow_mut().insert(
            pane_a,
            FileBrowserPaneState {
                root: Some(root_a.clone()),
                expanded: std::collections::HashSet::from([root_a.join("expanded-a")]),
                focused: Some(root_a.join("expanded-a")),
                selected: std::collections::HashSet::from([root_a.join("expanded-a")]),
                selection_anchor: Some(root_a.join("expanded-a")),
                scroll_value: 0.0,
            },
        );
        controller.file_browser.pane_states.borrow_mut().insert(
            pane_b,
            FileBrowserPaneState {
                root: Some(root_b.clone()),
                expanded: std::collections::HashSet::new(),
                focused: Some(root_b.join("b.txt")),
                selected: std::collections::HashSet::from([root_b.join("b.txt")]),
                selection_anchor: Some(root_b.join("b.txt")),
                scroll_value: 0.0,
            },
        );

        controller.show_file_browser_for_pane(pane_a).await;
        let state_a = controller.file_browser.panel.pane_state();
        assert_eq!(state_a.root, Some(root_a.clone()));
        assert_eq!(state_a.focused, Some(root_a.join("expanded-a")));
        assert!(state_a.expanded.contains(&root_a.join("expanded-a")));

        controller.show_file_browser_for_pane(pane_b).await;
        let state_b = controller.file_browser.panel.pane_state();
        assert_eq!(state_b.root, Some(root_b.clone()));
        assert_eq!(state_b.focused, Some(root_b.join("b.txt")));
        assert!(!state_b.expanded.contains(&root_a.join("expanded-a")));
        assert!(!state_b.expanded.contains(&root_b.join("expanded-b")));

        controller.show_file_browser_for_pane(pane_a).await;
        let state_a_again = controller.file_browser.panel.pane_state();
        assert_eq!(state_a_again.focused, Some(root_a.join("expanded-a")));
        assert!(state_a_again.expanded.contains(&root_a.join("expanded-a")));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn file_browser_uses_active_editor_root_over_inactive_terminal_cwd() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let workspace_root = std::env::temp_dir().join("flowmux-editor-browser-root");
        let terminal_cwd = std::env::temp_dir().join("flowmux-editor-browser-terminal-cwd");
        std::fs::create_dir_all(&workspace_root).unwrap();
        std::fs::create_dir_all(&terminal_cwd).unwrap();

        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("editor-root".into()), workspace_root.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        let terminal = ws.surfaces[0].root_pane.active_surface_id(pane).unwrap();
        store
            .update_surface_cwd(pane, terminal, terminal_cwd.clone())
            .await;
        store
            .add_editor_surface_to_pane(pane, workspace_root.clone())
            .await
            .expect("editor surface should be added");

        let ws = store.get_workspace(ws_id).await.unwrap();
        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.EditorFileBrowserRoot")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store,
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);

        assert_eq!(
            controller.pane_registry.borrow().current_dir_for_pane(pane),
            Some(terminal_cwd),
            "the inactive terminal must reproduce the competing cwd"
        );
        assert_eq!(
            controller.file_browser_root_for_pane(pane).await,
            Some(workspace_root)
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn file_browser_same_pane_root_refresh_does_not_rebuild_rows() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.FileBrowserRefreshNoop")
                .await;
        let ws = controller.store.get_workspace(ws_id).await.unwrap();
        let surface = ws.surfaces[0].root_pane.active_surface_id(pane).unwrap();
        let root = std::env::temp_dir().join("flowmux-file-browser-refresh-noop");
        let next_root = std::env::temp_dir().join("flowmux-file-browser-refresh-noop-next");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&next_root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&next_root).unwrap();
        for index in 0..256 {
            std::fs::write(root.join(format!("file-{index}.txt")), "file").unwrap();
        }
        std::fs::write(next_root.join("next.txt"), "next").unwrap();

        controller
            .store
            .update_surface_cwd(pane, surface, root.clone())
            .await;
        controller
            .pane_registry
            .borrow_mut()
            .terminals
            .remove(&surface);

        controller.show_file_browser_for_pane(pane).await;
        let rebuild_count = controller.file_browser.panel.rebuild_count();
        assert!(rebuild_count > 0);

        controller.show_file_browser_for_pane(pane).await;
        assert_eq!(controller.file_browser.panel.rebuild_count(), rebuild_count);

        controller
            .store
            .update_surface_cwd(pane, surface, next_root.clone())
            .await;
        controller.show_file_browser_for_pane(pane).await;
        assert!(controller.file_browser.panel.rebuild_count() > rebuild_count);
    }

    /// The file browser re-roots on the focused pane's new cwd. Both cwd-change
    /// paths (OSC 7 TerminalCwdChanged and the poll fallback) funnel through
    /// refresh_file_browser_from_focus, so exercising it directly covers both.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn file_browser_follows_focused_pane_cwd_change() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.FileBrowserFollowsCwd").await;
        let ws = controller.store.get_workspace(ws_id).await.unwrap();
        let surface = ws.surfaces[0].root_pane.active_surface_id(pane).unwrap();
        let root = std::env::temp_dir().join("flowmux-file-browser-follows-cwd");
        let next_root = std::env::temp_dir().join("flowmux-file-browser-follows-cwd-next");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&next_root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&next_root).unwrap();
        std::fs::write(root.join("here.txt"), "here").unwrap();
        std::fs::write(next_root.join("there.txt"), "there").unwrap();

        controller
            .store
            .update_surface_cwd(pane, surface, root.clone())
            .await;
        // Drop the live terminal so the root resolves from the stored cwd.
        controller
            .pane_registry
            .borrow_mut()
            .terminals
            .remove(&surface);

        controller.window.set_default_size(900, 600);
        controller.window.present();
        glib::timeout_future(std::time::Duration::from_millis(50)).await;

        controller.focused_pane.set(Some(pane));
        controller.show_file_browser_for_pane(pane).await;
        assert!(controller.file_browser.panel.is_showing_root(&root));

        // The focused pane cd's: a cwd-change refresh must move the panel with it.
        controller
            .store
            .update_surface_cwd(pane, surface, next_root.clone())
            .await;
        controller.refresh_file_browser_from_focus().await;
        assert!(controller.file_browser.panel.is_showing_root(&next_root));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn file_browser_follows_editor_tab_activation_in_same_pane() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let workspace = tempfile::tempdir().unwrap();
        let first_dir = workspace.path().join("first");
        let second_dir = workspace.path().join("second");
        std::fs::create_dir_all(&first_dir).unwrap();
        std::fs::create_dir_all(&second_dir).unwrap();
        let first_file = first_dir.join("first.txt");
        let second_file = second_dir.join("second.txt");
        std::fs::write(&first_file, "first\n").unwrap();
        std::fs::write(&second_file, "second\n").unwrap();

        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("editor-tabs".into()), workspace.path().to_path_buf())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.EditorTabFileBrowser")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);
        controller.focused_pane.set(Some(pane));
        controller.window.set_default_size(900, 600);
        controller.window.present();
        glib::timeout_future(std::time::Duration::from_millis(50)).await;

        controller.open_file_in_editor(first_file, Some(pane)).await;
        let first_surface = store.get_workspace(ws_id).await.unwrap().surfaces[0]
            .root_pane
            .active_surface_id(pane)
            .unwrap();
        controller
            .open_file_in_editor(second_file, Some(pane))
            .await;
        let second_surface = store.get_workspace(ws_id).await.unwrap().surfaces[0]
            .root_pane
            .active_surface_id(pane)
            .unwrap();
        assert_ne!(first_surface, second_surface);

        controller.show_file_browser_for_pane(pane).await;
        assert!(controller.file_browser.panel.is_showing_root(&second_dir));

        controller
            .dispatch(GtkCommand::ActivateSurface {
                pane,
                surface: first_surface,
            })
            .await;
        assert!(controller.file_browser.panel.is_showing_root(&first_dir));

        controller
            .dispatch(GtkCommand::ActivateSurface {
                pane,
                surface: second_surface,
            })
            .await;
        assert!(controller.file_browser.panel.is_showing_root(&second_dir));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn file_browser_opens_a_file_outside_the_static_workspace_root() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let workspace = tempfile::tempdir().unwrap();
        let current_dir = tempfile::tempdir().unwrap();
        let file = current_dir.path().join("current.txt");
        std::fs::write(&file, "current\n").unwrap();

        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("cwd-moved".into()), workspace.path().to_path_buf())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.EditorOutsideStaticRoot")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);

        controller
            .open_file_in_editor(file.clone(), Some(pane))
            .await;

        assert_eq!(store.tab_count_in_pane(pane).await, Some(2));
        let surface = store.get_workspace(ws_id).await.unwrap().surfaces[0]
            .root_pane
            .active_surface_id(pane)
            .unwrap();
        let registry = controller.pane_registry.borrow();
        let editor = registry.editors.get(&surface).unwrap();
        assert_eq!(editor.workspace_root(), current_dir.path());
        assert_eq!(
            editor.session_state().active_file.as_deref(),
            Some(file.as_path())
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn file_browser_escape_hides_panel_and_restores_saved_source_focus() {
        let (controller, _ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.FileBrowserEscape").await;

        controller.file_browser.source_pane.set(Some(pane));
        controller.focused_pane.set(None);
        controller.file_browser.panel.widget().set_visible(true);

        controller.close_file_browser_and_restore_focus();

        assert!(!controller.file_browser.panel.widget().is_visible());
        assert_eq!(controller.focused_pane.get(), Some(pane));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn clicking_second_workspace_moves_focus_into_it_so_alt_arrow_stays_there() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root_a = std::env::temp_dir().join("flowmux-ws-click-a");
        let root_b = std::env::temp_dir().join("flowmux-ws-click-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_a_id = store.create_workspace(Some("a".into()), root_a).await;
        let ws_b_id = store.create_workspace(Some("b".into()), root_b).await;
        let ws_a = store.get_workspace(ws_a_id).await.unwrap();
        let ws_b = store.get_workspace(ws_b_id).await.unwrap();
        let pane_a = ws_a.surfaces[0].root_pane.first_leaf_id().unwrap();
        let pane_b = ws_b.surfaces[0].root_pane.first_leaf_id().unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.WsClickFocus")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws_a);
        controller.render_workspace(&ws_b);
        // User initially worked in ws_a, so focused_pane is ws_a's pane.
        controller.focused_pane.set(Some(pane_a));
        store.set_active_workspace(Some(ws_a_id)).await;

        // Clicking ws_b in the side panel dispatches GtkCommand::ActivateWorkspace.
        // Reproduce that same flow by dispatching directly.
        controller
            .dispatch(GtkCommand::ActivateWorkspace { id: ws_b_id })
            .await;
        // Pass through one idle via oneshot to flush the idle queued by
        // activate_workspace's focus_first_leaf_of.
        let (idle_tx, idle_rx) = oneshot::channel();
        glib::idle_add_local_once(move || {
            let _ = idle_tx.send(());
        });
        let _ = idle_rx.await;

        assert_eq!(
            controller.focused_pane.get(),
            Some(pane_b),
            "clicking ws_b in the side panel should focus ws_b's first leaf",
        );

        // At this point, even if Alt+arrow is pressed, focus_in_direction uses a
        // source pane from ws_b and the candidate filter stays within that
        // workspace. Confirm the source belongs to ws_b.
        let r = controller.pane_registry.borrow();
        assert_eq!(
            r.workspace_of_pane(controller.focused_pane.get().unwrap()),
            Some(ws_b_id),
            "Alt+arrow source pane should belong to ws_b",
        );
        let in_b: std::collections::HashSet<_> = r.pane_ids_in_workspace(ws_b_id).collect();
        assert!(in_b.contains(&pane_b));
        assert!(!in_b.contains(&pane_a));
    }

    // ===== Side-panel label/subtitle scenario =====

    #[test]
    fn shorten_cwd_path_keeps_last_three_components() {
        use std::path::Path;
        // 5 components -> last 3 with ... prefix.
        assert_eq!(
            shorten_cwd_path(Path::new("/home/junsu/dev/os/flowmux")),
            ".../dev/os/flowmux"
        );
        // Exactly 3 components -> unchanged.
        assert_eq!(
            shorten_cwd_path(Path::new("/dev/os/flowmux")),
            "/dev/os/flowmux"
        );
        // 2 components -> unchanged.
        assert_eq!(shorten_cwd_path(Path::new("/home/junsu")), "/home/junsu");
        // Single component / root.
        assert_eq!(shorten_cwd_path(Path::new("/tmp")), "/tmp");
        // Deeper paths still keep the last 3.
        assert_eq!(shorten_cwd_path(Path::new("/a/b/c/d/e/f/g")), ".../e/f/g");
    }

    #[test]
    fn focused_surface_full_title_reconstructs_full_folder_for_terminal() {
        // Even if surface.title is truncated for a long folder name, reconstruct
        // the full length from cwd.
        use flowmux_core::{PaneSurface, Surface, SurfaceId, SurfaceKind};
        let cwd = std::path::PathBuf::from("/tmp/DynamicGenerativeUI");
        let mut surface = PaneSurface::terminal(
            flowmux_core::terminal_tab_title_for_cwd(Some(&cwd)),
            Some(cwd.clone()),
        );
        // surface.title is truncated ("DynamicGenerati...").
        assert!(surface.title.ends_with("..."));
        let surface_id = surface.id;
        // mut not actually needed, kept for clarity.
        surface.title_locked = false;
        let pane_id = PaneId::new();
        let ws = flowmux_core::Workspace {
            id: WorkspaceId::new(),
            name: "any".into(),
            custom_title: None,
            root_dir: cwd.clone(),
            git: None,
            listening_ports: vec![],
            surfaces: vec![Surface {
                id: SurfaceId::new(),
                kind: SurfaceKind::Terminal {
                    shell: None,
                    cwd: Some(cwd.clone()),
                },
                title: "main".into(),
                root_pane: flowmux_core::Pane::Leaf {
                    id: pane_id,
                    content: flowmux_core::PaneContent::Tabs {
                        active: surface_id,
                        surfaces: vec![surface],
                    },
                },
            }],
            color: None,
        };

        assert_eq!(
            focused_surface_full_title(&ws, pane_id).as_deref(),
            Some("DynamicGenerativeUI")
        );
    }

    #[test]
    fn focused_surface_full_title_respects_locked_or_osc_titles() {
        use flowmux_core::{PaneSurface, Surface, SurfaceId, SurfaceKind};
        // User rename to "MyName" sets title_locked=true. Use that label as the
        // workspace name candidate and do not overwrite it with the cwd folder.
        let cwd = std::path::PathBuf::from("/tmp/some-folder");
        let mut surface = PaneSurface::terminal("MyName", Some(cwd.clone()));
        surface.title_locked = true;
        let surface_id = surface.id;
        let pane_id = PaneId::new();
        let ws = flowmux_core::Workspace {
            id: WorkspaceId::new(),
            name: "any".into(),
            custom_title: None,
            root_dir: cwd.clone(),
            git: None,
            listening_ports: vec![],
            surfaces: vec![Surface {
                id: SurfaceId::new(),
                kind: SurfaceKind::Terminal {
                    shell: None,
                    cwd: Some(cwd),
                },
                title: "main".into(),
                root_pane: flowmux_core::Pane::Leaf {
                    id: pane_id,
                    content: flowmux_core::PaneContent::Tabs {
                        active: surface_id,
                        surfaces: vec![surface],
                    },
                },
            }],
            color: None,
        };
        assert_eq!(
            focused_surface_full_title(&ws, pane_id).as_deref(),
            Some("MyName")
        );
    }

    #[test]
    fn collect_subtitle_lines_walks_mru_then_falls_back_to_tree() {
        // Three leaves in a split tree. When MRU contains only part of them, the
        // rest should be filled by tree DFS.
        use flowmux_core::{Pane, PaneContent, SplitDirection};
        let l_id = PaneId::new();
        let m_id = PaneId::new();
        let r_id = PaneId::new();
        let cwd_l = std::path::PathBuf::from("/tmp/L");
        let cwd_m = std::path::PathBuf::from("/tmp/M");
        let cwd_r = std::path::PathBuf::from("/tmp/R");
        let ws = flowmux_core::Workspace {
            id: WorkspaceId::new(),
            name: "any".into(),
            custom_title: None,
            root_dir: "/tmp".into(),
            git: None,
            listening_ports: vec![],
            surfaces: vec![flowmux_core::Surface {
                id: flowmux_core::SurfaceId::new(),
                kind: flowmux_core::SurfaceKind::Terminal {
                    shell: None,
                    cwd: None,
                },
                title: "main".into(),
                root_pane: Pane::Split {
                    id: PaneId::new(),
                    direction: SplitDirection::Vertical,
                    ratio: 0.5,
                    first: Box::new(Pane::Leaf {
                        id: l_id,
                        content: PaneContent::tabbed_terminal("L", Some(cwd_l.clone())),
                    }),
                    second: Box::new(Pane::Split {
                        id: PaneId::new(),
                        direction: SplitDirection::Horizontal,
                        ratio: 0.5,
                        first: Box::new(Pane::Leaf {
                            id: m_id,
                            content: PaneContent::tabbed_terminal("M", Some(cwd_m.clone())),
                        }),
                        second: Box::new(Pane::Leaf {
                            id: r_id,
                            content: PaneContent::tabbed_terminal("R", Some(cwd_r.clone())),
                        }),
                    }),
                },
            }],
            color: None,
        };

        // MRU only: when order is r -> m -> l, subtitles follow that order.
        let mru = vec![r_id, m_id, l_id];
        let lines = collect_subtitle_lines(&ws, &mru, 3);
        assert_eq!(
            lines,
            vec![
                shorten_cwd_path(&cwd_r),
                shorten_cwd_path(&cwd_m),
                shorten_cwd_path(&cwd_l),
            ]
        );

        // One MRU entry only -> fill the rest by left-first tree DFS.
        let mru = vec![r_id];
        let lines = collect_subtitle_lines(&ws, &mru, 3);
        assert_eq!(
            lines,
            vec![
                shorten_cwd_path(&cwd_r),
                shorten_cwd_path(&cwd_l),
                shorten_cwd_path(&cwd_m),
            ]
        );

        // Empty MRU -> all lines come from tree DFS.
        let lines = collect_subtitle_lines(&ws, &[], 3);
        assert_eq!(
            lines,
            vec![
                shorten_cwd_path(&cwd_l),
                shorten_cwd_path(&cwd_m),
                shorten_cwd_path(&cwd_r),
            ]
        );
    }

    /// A leaf whose active surface is a browser tab emits a `Browser-{tab name}`
    /// subtitle instead of cwd. Even when terminal and browser tabs share one
    /// leaf, only the currently active tab kind is considered.
    #[test]
    fn collect_subtitle_lines_uses_browser_prefix_for_browser_panes() {
        use flowmux_core::{Pane, PaneContent, PaneSurface, Surface, SurfaceId, SurfaceKind};
        let pane_id = PaneId::new();
        let term_surface = PaneSurface::terminal("dev", Some(std::path::PathBuf::from("/tmp/dev")));
        let browser_surface =
            PaneSurface::browser("DocsHome", "https://example.com/docs/home".into());
        let browser_active = browser_surface.id;
        let ws = flowmux_core::Workspace {
            id: WorkspaceId::new(),
            name: "any".into(),
            custom_title: None,
            root_dir: "/tmp".into(),
            git: None,
            listening_ports: vec![],
            surfaces: vec![Surface {
                id: SurfaceId::new(),
                kind: SurfaceKind::Terminal {
                    shell: None,
                    cwd: None,
                },
                title: "main".into(),
                root_pane: Pane::Leaf {
                    id: pane_id,
                    content: PaneContent::Tabs {
                        active: browser_active,
                        surfaces: vec![term_surface, browser_surface],
                    },
                },
            }],
            color: None,
        };

        // Active browser -> one "Browser-DocsHome" line.
        let lines = collect_subtitle_lines(&ws, &[pane_id], 3);
        assert_eq!(lines, vec!["Browser-DocsHome".to_string()]);
        // Same with empty MRU; tree DFS reaches the same leaf.
        let lines = collect_subtitle_lines(&ws, &[], 3);
        assert_eq!(lines, vec!["Browser-DocsHome".to_string()]);
    }

    fn test_workspace_with_leaves(leaves: Vec<(PaneId, PaneSurface)>) -> Workspace {
        use flowmux_core::{Pane, PaneContent, SplitDirection};

        fn leaf(pane_id: PaneId, surface: PaneSurface) -> Pane {
            Pane::Leaf {
                id: pane_id,
                content: PaneContent::Tabs {
                    active: surface.id,
                    surfaces: vec![surface],
                },
            }
        }

        let root_pane = leaves
            .into_iter()
            .map(|(pane_id, surface)| leaf(pane_id, surface))
            .reduce(|first, second| Pane::Split {
                id: PaneId::new(),
                direction: SplitDirection::Vertical,
                ratio: 0.5,
                first: Box::new(first),
                second: Box::new(second),
            })
            .expect("test workspace needs at least one leaf");

        Workspace {
            id: WorkspaceId::new(),
            name: "any".into(),
            custom_title: None,
            root_dir: "/tmp".into(),
            git: None,
            listening_ports: vec![],
            surfaces: vec![Surface {
                id: SurfaceId::new(),
                kind: SurfaceKind::Terminal {
                    shell: None,
                    cwd: None,
                },
                title: "main".into(),
                root_pane,
            }],
            color: None,
        }
    }

    fn test_terminal_surface(
        title: &str,
        cwd: &str,
        agent: Option<(&str, flowmux_core::AgentActivity, Option<&str>)>,
    ) -> PaneSurface {
        let mut surface = PaneSurface::terminal(title, Some(std::path::PathBuf::from(cwd)));
        if let Some((name, activity, message)) = agent {
            let mut presence = flowmux_core::AgentPresence::new(name, activity, None);
            presence.message = message.map(str::to_string);
            surface.agent = Some(presence);
        }
        surface
    }

    #[test]
    fn workspace_row_details_put_agent_blocks_before_deduped_mru_paths() {
        let agent_pane = PaneId::new();
        let shell_pane = PaneId::new();
        let ws = test_workspace_with_leaves(vec![
            (
                agent_pane,
                test_terminal_surface(
                    "agent",
                    "/home/u/dev/flowmux",
                    Some((
                        "codex",
                        flowmux_core::AgentActivity::Running,
                        Some("running tests"),
                    )),
                ),
            ),
            (
                shell_pane,
                test_terminal_surface("shell", "/home/u/dev/plain", None),
            ),
        ]);

        let details = workspace_row_details(&ws, &[agent_pane, shell_pane]);
        assert_eq!(details.agent_blocks.len(), 1);
        assert_eq!(details.agent_blocks[0].agent_name, "codex");
        assert_eq!(
            details.agent_blocks[0].status,
            flowmux_core::AgentStatus::Working
        );
        assert_eq!(details.agent_blocks[0].status_text, "running tests");
        assert_eq!(
            details.agent_blocks[0].path.as_deref(),
            Some(".../u/dev/flowmux")
        );
        assert_eq!(details.path_lines, vec![".../u/dev/plain".to_string()]);
    }

    #[test]
    fn workspace_row_details_limits_agent_blocks_and_marks_overflow() {
        let first = PaneId::new();
        let second = PaneId::new();
        let third = PaneId::new();
        let fourth = PaneId::new();
        let fifth = PaneId::new();
        let ws = test_workspace_with_leaves(vec![
            (
                first,
                test_terminal_surface(
                    "first",
                    "/tmp/first",
                    Some(("codex", flowmux_core::AgentActivity::Running, None)),
                ),
            ),
            (
                second,
                test_terminal_surface(
                    "second",
                    "/tmp/second",
                    Some(("claude", flowmux_core::AgentActivity::Running, None)),
                ),
            ),
            (
                third,
                test_terminal_surface(
                    "third",
                    "/tmp/third",
                    Some(("opencode", flowmux_core::AgentActivity::Running, None)),
                ),
            ),
            (
                fourth,
                test_terminal_surface(
                    "fourth",
                    "/tmp/fourth",
                    Some(("gemini", flowmux_core::AgentActivity::Running, None)),
                ),
            ),
            (
                fifth,
                test_terminal_surface(
                    "fifth",
                    "/tmp/fifth",
                    Some(("aider", flowmux_core::AgentActivity::Running, None)),
                ),
            ),
        ]);

        let details = workspace_row_details(&ws, &[fifth, fourth, third, second, first]);
        assert_eq!(details.agent_blocks.len(), 4);
        assert_eq!(details.agent_blocks[0].agent_name, "aider");
        assert_eq!(details.agent_blocks[0].status_text, "working");
        assert_eq!(details.agent_blocks[1].agent_name, "gemini");
        assert_eq!(details.agent_blocks[2].agent_name, "opencode");
        assert_eq!(details.agent_blocks[3].agent_name, "claude");
        assert_eq!(details.agent_blocks[3].overflow_count, 1);
    }

    #[test]
    fn workspace_row_details_without_agents_matches_existing_subtitles() {
        let first = PaneId::new();
        let second = PaneId::new();
        let ws = test_workspace_with_leaves(vec![
            (
                first,
                test_terminal_surface("first", "/home/u/dev/first", None),
            ),
            (
                second,
                test_terminal_surface("second", "/home/u/dev/second", None),
            ),
        ]);
        let mru = vec![second, first];

        let details = workspace_row_details(&ws, &mru);
        assert!(details.agent_blocks.is_empty());
        assert_eq!(details.path_lines, collect_subtitle_lines(&ws, &mru, 3));
    }

    fn run_async<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    async fn sync_workspace_label_state_only(
        store: &StateStore,
        ws_id: WorkspaceId,
        mru: &[PaneId],
    ) -> Vec<String> {
        let ws = store.get_workspace(ws_id).await.unwrap();
        let head_pane = mru.first().copied().or_else(|| {
            ws.surfaces
                .first()
                .and_then(|surface| surface.root_pane.first_leaf_id())
        });
        if let Some(head_pane) = head_pane {
            if let Some(new_name) = focused_surface_full_title(&ws, head_pane) {
                store.set_workspace_name(ws_id, new_name).await;
            }
        }

        let ws = store.get_workspace(ws_id).await.unwrap();
        collect_subtitle_lines(&ws, mru, 3)
    }

    #[test]
    fn scenario_workspace_name_and_subtitles_track_focused_terminals_state_only() {
        run_async(async {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "flowmux-scn-name-subtitles-state-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();

            let store = StateStore::new_lazy(State::default());
            let ws_id = store.create_workspace(None, root.clone()).await;
            let ws = store.get_workspace(ws_id).await.unwrap();
            let pane_a = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
            let surface_a = ws.surfaces[0].root_pane.active_surface_id(pane_a).unwrap();

            let mut mru = vec![pane_a];
            let subs = sync_workspace_label_state_only(&store, ws_id, &mru).await;
            let ws = store.get_workspace(ws_id).await.unwrap();
            assert_eq!(
                ws.display_title(),
                root.file_name().unwrap().to_string_lossy()
            );
            assert_eq!(subs, vec![shorten_cwd_path(&root)]);

            let project_a = std::path::PathBuf::from("/home/flowmux-scn/dev/projectA");
            store
                .update_surface_cwd(pane_a, surface_a, project_a.clone())
                .await;
            let subs = sync_workspace_label_state_only(&store, ws_id, &mru).await;
            let ws = store.get_workspace(ws_id).await.unwrap();
            assert_eq!(ws.name, "projectA");
            assert_eq!(ws.display_title(), "projectA");
            assert_eq!(subs, vec![".../flowmux-scn/dev/projectA"]);

            assert!(store.rename_workspace(ws_id, "MyName".into()).await);
            let project_b = std::path::PathBuf::from("/home/flowmux-scn/dev/projectB");
            store
                .update_surface_cwd(pane_a, surface_a, project_b.clone())
                .await;
            let subs = sync_workspace_label_state_only(&store, ws_id, &mru).await;
            let ws = store.get_workspace(ws_id).await.unwrap();
            assert_eq!(ws.name, "projectB");
            assert_eq!(ws.display_title(), "MyName");
            assert_eq!(subs, vec![".../flowmux-scn/dev/projectB"]);

            let (_, pane_b) = store
                .split_pane(pane_a, SplitDirection::Vertical)
                .await
                .expect("split should succeed");
            let ws = store.get_workspace(ws_id).await.unwrap();
            let surface_b = ws.surfaces[0].root_pane.active_surface_id(pane_b).unwrap();
            let project_c = std::path::PathBuf::from("/home/flowmux-scn/dev/projectC");
            store
                .update_surface_cwd(pane_b, surface_b, project_c.clone())
                .await;

            mru.retain(|pane| *pane != pane_b);
            mru.insert(0, pane_b);
            let subs = sync_workspace_label_state_only(&store, ws_id, &mru).await;
            let ws = store.get_workspace(ws_id).await.unwrap();
            assert_eq!(ws.name, "projectC");
            assert_eq!(ws.display_title(), "MyName");
            assert_eq!(
                subs,
                vec![
                    ".../flowmux-scn/dev/projectC",
                    ".../flowmux-scn/dev/projectB",
                ]
            );

            mru.retain(|pane| *pane != pane_a);
            mru.insert(0, pane_a);
            let subs = sync_workspace_label_state_only(&store, ws_id, &mru).await;
            assert_eq!(
                subs,
                vec![
                    ".../flowmux-scn/dev/projectB",
                    ".../flowmux-scn/dev/projectC",
                ]
            );

            let (_, pane_c) = store
                .split_pane(pane_b, SplitDirection::Horizontal)
                .await
                .expect("second split should succeed");
            let ws = store.get_workspace(ws_id).await.unwrap();
            let surface_c = ws.surfaces[0].root_pane.active_surface_id(pane_c).unwrap();
            let project_d = std::path::PathBuf::from("/home/flowmux-scn/dev/projectD");
            store
                .update_surface_cwd(pane_c, surface_c, project_d.clone())
                .await;

            mru.retain(|pane| *pane != pane_c);
            mru.insert(0, pane_c);
            let subs = sync_workspace_label_state_only(&store, ws_id, &mru).await;
            assert_eq!(
                subs,
                vec![
                    ".../flowmux-scn/dev/projectD",
                    ".../flowmux-scn/dev/projectB",
                    ".../flowmux-scn/dev/projectC",
                ]
            );

            mru.retain(|pane| *pane != pane_a);
            mru.insert(0, pane_a);
            let subs = sync_workspace_label_state_only(&store, ws_id, &mru).await;
            assert_eq!(
                subs,
                vec![
                    ".../flowmux-scn/dev/projectB",
                    ".../flowmux-scn/dev/projectD",
                    ".../flowmux-scn/dev/projectC",
                ]
            );

            let (_, browser_surface) = store
                .add_browser_surface_to_pane(pane_a, "https://example.com/docs".into())
                .await
                .expect("browser tab should be added");
            assert!(store
                .rename_surface(pane_a, browser_surface, "DocsHome".into())
                .await
                .is_some());
            mru.retain(|pane| *pane != pane_a);
            mru.insert(0, pane_a);
            let subs = sync_workspace_label_state_only(&store, ws_id, &mru).await;
            let ws = store.get_workspace(ws_id).await.unwrap();
            assert_eq!(ws.name, "DocsHome");
            assert_eq!(ws.display_title(), "MyName");
            assert_eq!(
                subs,
                vec![
                    "Browser-DocsHome",
                    ".../flowmux-scn/dev/projectD",
                    ".../flowmux-scn/dev/projectC",
                ]
            );

            let _ = std::fs::remove_dir_all(&root);
        });
    }

    /// Scenario test for the requested side-panel workspace row behavior:
    ///   1. On focus, ws.name = active surface label and subtitles = that cwd.
    ///   2. One cd immediately updates ws.name and subtitles to the new folder/path.
    ///   3. "Change name" locks display_title while ws.name keeps tracking cwd.
    ///   4. After split, MRU head pane decides ws.name and subtitles use MRU order.
    ///   5. Moving focus to another pane puts that pane's cwd on the first subtitle.
    ///   6. With three split panes, focusing each once produces three subtitles.
    ///   7. Refocusing an existing MRU pane keeps length 3 and only updates the head.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn scenario_workspace_name_and_subtitles_track_focused_terminals_end_to_end() {
        adw::init().expect("libadwaita should initialize in GTK test");
        // Only root_dir must exist because terminal spawn uses it. Other cwd
        // values are handled as strings by store / sync logic.
        let root = std::env::temp_dir().join("flowmux-scn-name-subtitles");
        std::fs::create_dir_all(&root).unwrap();

        let store = StateStore::new_lazy(State::default());
        let ws_id = store.create_workspace(None, root.clone()).await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane_a = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        let surface_a = ws.surfaces[0].root_pane.active_surface_id(pane_a).unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.Scenario.NameSubtitles")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);

        // 1. Single-pane focus: ws.name = root folder name, subtitles = root cwd.
        controller
            .dispatch(GtkCommand::PaneFocused { pane: pane_a })
            .await;
        {
            let ws = store.get_workspace(ws_id).await.unwrap();
            assert_eq!(
                ws.display_title(),
                "flowmux-scn-name-subtitles",
                "initial workspace name is the root_dir folder name",
            );
        }
        let subs = controller
            .sidebar
            .cached_subtitles(ws_id)
            .expect("PaneFocused should cache subtitles");
        assert_eq!(subs.len(), 1, "one leaf -> one subtitle");
        assert_eq!(
            subs[0],
            shorten_cwd_path(&root),
            "first subtitle is the focused pane cwd after shortening",
        );

        // 2. cd -> ws.name is the new folder, subtitles keep the last 3 folders.
        let project_a = std::path::PathBuf::from("/home/flowmux-scn/dev/projectA");
        controller
            .dispatch(GtkCommand::TerminalCwdChanged {
                pane: pane_a,
                surface: surface_a,
                cwd: project_a.clone(),
            })
            .await;
        {
            let ws = store.get_workspace(ws_id).await.unwrap();
            assert_eq!(
                ws.name, "projectA",
                "ws.name reflects the new folder after cd"
            );
            assert_eq!(
                ws.display_title(),
                "projectA",
                "without custom_title, the automatic value is displayed",
            );
        }
        let subs = controller.sidebar.cached_subtitles(ws_id).unwrap();
        assert_eq!(
            subs[0], ".../flowmux-scn/dev/projectA",
            "4 components -> last 3 with \"...\" prefix",
        );

        // 3. Change name -> display_title stays fixed while automatic ws.name keeps tracking.
        let (rename_ack, rename_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::RenameWorkspace {
                id: ws_id,
                name: "MyName".into(),
                ack: rename_ack,
            })
            .await;
        let _ = rename_rx.await;
        {
            let ws = store.get_workspace(ws_id).await.unwrap();
            assert_eq!(ws.display_title(), "MyName", "custom name takes priority");
        }

        let project_b = std::path::PathBuf::from("/home/flowmux-scn/dev/projectB");
        controller
            .dispatch(GtkCommand::TerminalCwdChanged {
                pane: pane_a,
                surface: surface_a,
                cwd: project_b.clone(),
            })
            .await;
        {
            let ws = store.get_workspace(ws_id).await.unwrap();
            assert_eq!(
                ws.display_title(),
                "MyName",
                "custom name stays visible after cd",
            );
            assert_eq!(
                ws.name, "projectB",
                "automatic name keeps tracking folder names"
            );
        }
        let subs = controller.sidebar.cached_subtitles(ws_id).unwrap();
        assert_eq!(
            subs[0], ".../flowmux-scn/dev/projectB",
            "subtitle updates immediately after the terminal event",
        );

        // 4. Split -> two leaves, focus second pane -> two subtitles, new pane as head.
        let (_, pane_b) = store
            .split_pane(pane_a, SplitDirection::Vertical)
            .await
            .expect("split should succeed");
        let ws_after = store.get_workspace(ws_id).await.unwrap();
        let surface_b = ws_after.surfaces[0]
            .root_pane
            .active_surface_id(pane_b)
            .unwrap();
        let project_c = std::path::PathBuf::from("/home/flowmux-scn/dev/projectC");
        store
            .update_surface_cwd(pane_b, surface_b, project_c.clone())
            .await;

        controller
            .dispatch(GtkCommand::PaneFocused { pane: pane_b })
            .await;
        {
            let ws = store.get_workspace(ws_id).await.unwrap();
            assert_eq!(
                ws.name, "projectC",
                "MRU head is pane_b, so ws.name follows that surface label",
            );
            assert_eq!(
                ws.display_title(),
                "MyName",
                "rename lock remains after split",
            );
        }
        let subs = controller.sidebar.cached_subtitles(ws_id).unwrap();
        assert_eq!(
            subs.len(),
            2,
            "two split panes with focus history -> two subtitles"
        );
        assert_eq!(
            subs[0], ".../flowmux-scn/dev/projectC",
            "MRU[0] = newly focused pane_b",
        );
        assert_eq!(
            subs[1], ".../flowmux-scn/dev/projectB",
            "MRU[1] = previously focused pane_a",
        );

        // 5. Focus pane_a again -> subtitles reorder to [A, B].
        controller
            .dispatch(GtkCommand::PaneFocused { pane: pane_a })
            .await;
        let subs = controller.sidebar.cached_subtitles(ws_id).unwrap();
        assert_eq!(
            subs[0], ".../flowmux-scn/dev/projectB",
            "focus move makes pane_a the MRU head, so first subtitle uses its cwd",
        );
        assert_eq!(subs[1], ".../flowmux-scn/dev/projectC");

        // 6. Third split -> three subtitles after each pane has focus once.
        let (_, pane_c) = store
            .split_pane(pane_b, SplitDirection::Horizontal)
            .await
            .expect("second split");
        let ws_after = store.get_workspace(ws_id).await.unwrap();
        let surface_c = ws_after.surfaces[0]
            .root_pane
            .active_surface_id(pane_c)
            .unwrap();
        let project_d = std::path::PathBuf::from("/home/flowmux-scn/dev/projectD");
        store
            .update_surface_cwd(pane_c, surface_c, project_d.clone())
            .await;
        controller
            .dispatch(GtkCommand::PaneFocused { pane: pane_c })
            .await;

        let subs = controller.sidebar.cached_subtitles(ws_id).unwrap();
        assert_eq!(
            subs.len(),
            3,
            "three split panes with each focused once -> three subtitles",
        );
        assert_eq!(
            subs[0], ".../flowmux-scn/dev/projectD",
            "MRU[0]=C just focused"
        );
        assert_eq!(subs[1], ".../flowmux-scn/dev/projectB", "MRU[1]=A");
        assert_eq!(subs[2], ".../flowmux-scn/dev/projectC", "MRU[2]=B");

        // 7. Refocus a pane already in MRU -> update only the head, keep length 3.
        controller
            .dispatch(GtkCommand::PaneFocused { pane: pane_a })
            .await;
        let subs = controller.sidebar.cached_subtitles(ws_id).unwrap();
        assert_eq!(
            subs.len(),
            3,
            "refocusing an existing MRU pane keeps length"
        );
        assert_eq!(subs[0], ".../flowmux-scn/dev/projectB", "MRU head = pane_a");
        assert_eq!(subs[1], ".../flowmux-scn/dev/projectD", "MRU[1] = pane_c");
        assert_eq!(subs[2], ".../flowmux-scn/dev/projectC", "MRU[2] = pane_b");

        // 8. Add a browser surface inside pane_a. The store makes it active, and
        // another PaneFocused sync turns that pane's subtitle into
        // "Browser-{tab name}".
        let (_, browser_surface) = store
            .add_browser_surface_to_pane(pane_a, "https://example.com/docs".into())
            .await
            .expect("add browser surface to pane_a");
        // Give the browser tab the visible tab name so it is easy to verify the
        // subtitle uses that label exactly.
        store
            .rename_surface(pane_a, browser_surface, "DocsHome".into())
            .await;
        // PaneFocused sees the active surface after add_browser_surface_to_pane,
        // which is browser_surface. The ActivateSurface dispatch path tries to
        // update PaneRegistry GTK widgets, so this test triggers
        // sync_workspace_label through PaneFocused only.
        controller
            .dispatch(GtkCommand::PaneFocused { pane: pane_a })
            .await;

        let subs = controller.sidebar.cached_subtitles(ws_id).unwrap();
        assert_eq!(
            subs[0], "Browser-DocsHome",
            "active browser tabs use Browser-{{tab name}} subtitles",
        );
        // The remaining two lines still come from other leaves' terminal cwd.
        assert_eq!(subs[1], ".../flowmux-scn/dev/projectD", "MRU[1] = pane_c");
        assert_eq!(subs[2], ".../flowmux-scn/dev/projectC", "MRU[2] = pane_b");
    }

    /// Regression: closing the right pane of a side-by-side split (the
    /// X-button on the right pane's tab) must collapse only that pane
    /// and leave the surviving left pane visible. The earlier
    /// implementation called `paned.unparent()` while the workspace
    /// stack still kept the paned's `GtkStackPage` registered to the
    /// workspace name, so the subsequent `add_named` of the sibling
    /// silently no-op'd and the workspace went blank — the user
    /// reported it as "every pane closed".
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn close_right_pane_in_side_by_side_split_keeps_left_pane_visible() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-close-right-pane");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let left = ws.surfaces[0].root_pane.first_leaf_id().unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.CloseRightPane")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.options.borrow_mut().agent_bar_enabled = true;
        controller.render_workspace(&ws);

        // Split the original pane to the right. The new pane is the
        // right one; `left` keeps its identity / widget instance.
        let (split_ws, right) = store
            .split_pane(left, SplitDirection::Vertical)
            .await
            .expect("split should succeed");
        assert_eq!(split_ws, ws_id);

        // Drive the GTK side through the same command path the
        // SplitFocused keybinding uses, so the workspace ends up with
        // a real `gtk::Paned` whose two children are the left and
        // right pane frames — exactly the shape the X-close needs to
        // collapse.
        controller
            .apply_split_incremental_or_rerender(ws_id, left, right, SplitDirection::Vertical)
            .await;
        let right_surface = store.get_workspace(ws_id).await.unwrap().surfaces[0]
            .root_pane
            .active_surface_id(right)
            .unwrap();
        store
            .report_agent_status(
                right_surface,
                flowmux_core::AgentStatusReport {
                    name: "codex".into(),
                    status: Some(flowmux_core::AgentStatus::Working),
                    activity: Some(flowmux_core::AgentActivity::Running),
                    pid: None,
                    source: Some("flowmux:hook".into()),
                    seq: Some(1),
                    message: None,
                    custom_status: None,
                    session_id: None,
                },
            )
            .await;
        controller
            .sync_workspace_agent_status_from_store(ws_id)
            .await;
        assert_eq!(
            controller
                .sidebar
                .cached_details(ws_id)
                .unwrap()
                .agent_blocks
                .len(),
            1,
            "precondition: closing pane starts with an agent block"
        );
        assert!(
            agent_bar_visible(&controller),
            "precondition: closing pane starts with an Agent Bar item"
        );

        {
            let r = controller.pane_registry.borrow();
            assert!(r.pane_frame(left).is_some(), "left pane registered");
            assert!(r.pane_frame(right).is_some(), "right pane registered");
        }

        // X-button on the right pane → CloseFocused dispatches
        // close_pane → PaneRemoved → apply_close_pane_incremental_or_rerender.
        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::CloseFocused {
                pane: right,
                ack: ack_tx,
            })
            .await;
        ack_rx.await.unwrap().unwrap();

        // The left pane must still be rendered and the workspace
        // stack must still have a visible child for this workspace —
        // the regression manifested as both being absent.
        {
            let r = controller.pane_registry.borrow();
            assert!(
                r.pane_frame(left).is_some(),
                "regression: left pane disappeared after closing the right pane"
            );
            assert!(
                r.pane_frame(right).is_none(),
                "right pane should have been forgotten by PaneRegistry"
            );
        }

        let visible = controller.stack.visible_child_name();
        assert_eq!(
            visible.map(|n| n.to_string()),
            Some(ws_id.to_string()),
            "regression: workspace stack lost its visible child after right-pane close"
        );

        // The workspace's top-level widget should now be the surviving
        // left pane's frame, not the old paned (the old paned was
        // unparented). `surfaces` map carries that pointer.
        {
            let surfaces = controller.surfaces.borrow();
            let top = surfaces
                .get(&ws_id)
                .expect("surfaces map has workspace widget");
            let left_frame = controller
                .pane_registry
                .borrow()
                .pane_frame(left)
                .expect("left frame still in registry");
            assert_eq!(
                top, &left_frame,
                "the workspace stack child should be the surviving left pane's frame",
            );
        }

        // And the daemon-side state agrees: the workspace tree is now
        // a single leaf rooted at `left`, with no split node above it.
        let ws_after = store.get_workspace(ws_id).await.unwrap();
        let leaf_count = {
            let mut leaves = Vec::new();
            ws_after.surfaces[0]
                .root_pane
                .for_each_leaf(|id| leaves.push(id));
            leaves
        };
        assert_eq!(
            leaf_count,
            vec![left],
            "store collapsed the split correctly"
        );
        assert!(
            controller
                .sidebar
                .cached_details(ws_id)
                .unwrap()
                .agent_blocks
                .is_empty(),
            "closing an agent pane must refresh sidebar details"
        );
        assert!(
            !agent_bar_visible(&controller),
            "closing an agent pane must refresh Agent Bar visibility"
        );
    }

    /// Regression: closing an agent tab inside a pane must also recalculate the
    /// side-panel details. Otherwise a stale agent block remains under the
    /// workspace even though the surface and its agent presence are gone.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn closing_agent_tab_refreshes_sidebar_details() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-close-agent-tab-details");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.CloseAgentTabDetails")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.options.borrow_mut().agent_bar_enabled = true;
        controller.render_workspace(&ws);

        let agent_dir = root.join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let (_, agent_surface) = store
            .add_terminal_surface_to_pane(pane, Some(agent_dir))
            .await
            .expect("agent tab should be added");
        controller
            .attach_or_rerender_surface(ws_id, pane, agent_surface)
            .await;
        store
            .report_agent_status(
                agent_surface,
                flowmux_core::AgentStatusReport {
                    name: "codex".into(),
                    status: Some(flowmux_core::AgentStatus::Working),
                    activity: Some(flowmux_core::AgentActivity::Running),
                    pid: None,
                    source: Some("flowmux:hook".into()),
                    seq: Some(1),
                    message: None,
                    custom_status: None,
                    session_id: None,
                },
            )
            .await;
        controller
            .sync_workspace_agent_status_from_store(ws_id)
            .await;
        assert_eq!(
            controller
                .sidebar
                .cached_details(ws_id)
                .unwrap()
                .agent_blocks
                .len(),
            1,
            "precondition: closing tab starts with an agent block"
        );
        assert!(
            agent_bar_visible(&controller),
            "precondition: closing tab starts with an Agent Bar item"
        );

        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::CloseSurface {
                pane,
                surface: agent_surface,
                ack: ack_tx,
            })
            .await;
        ack_rx.await.unwrap().unwrap();

        assert!(
            controller
                .sidebar
                .cached_details(ws_id)
                .unwrap()
                .agent_blocks
                .is_empty(),
            "closing an agent tab must refresh sidebar details"
        );
        assert!(
            !agent_bar_visible(&controller),
            "closing an agent tab must refresh Agent Bar visibility"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn stale_close_after_cross_workspace_move_does_not_prompt_or_close_source() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let store = StateStore::new_lazy(State::default());
        let src_workspace = store
            .create_workspace(
                Some("source".into()),
                PathBuf::from("/tmp/flowmux-stale-close-source"),
            )
            .await;
        let dst_workspace = store
            .create_workspace(
                Some("destination".into()),
                PathBuf::from("/tmp/flowmux-stale-close-destination"),
            )
            .await;
        let src_pane = store.get_workspace(src_workspace).await.unwrap().surfaces[0]
            .root_pane
            .first_leaf_id()
            .unwrap();
        let dst_pane = store.get_workspace(dst_workspace).await.unwrap().surfaces[0]
            .root_pane
            .first_leaf_id()
            .unwrap();
        let (_, moved_surface) = store
            .add_terminal_surface_to_pane(src_pane, None)
            .await
            .expect("second source tab should be added");
        store
            .move_surface_to_workspace(src_pane, moved_surface, dst_workspace)
            .await
            .expect("cross-workspace move should succeed");

        assert!(store.surface_title(src_pane, moved_surface).await.is_none());
        assert!(store.surface_title(dst_pane, moved_surface).await.is_some());
        assert_eq!(store.tab_count_in_pane(src_pane).await, Some(1));
        assert_eq!(
            store.workspace_pane_count_for(src_pane).await,
            Some((src_workspace, 1))
        );

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.StaleCloseAfterMove")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );

        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::CloseSurface {
                pane: src_pane,
                surface: moved_surface,
                ack: ack_tx,
            })
            .await;
        let error = ack_rx
            .await
            .expect("stale close must acknowledge")
            .expect_err("stale close must be rejected");

        assert!(error.contains("surface not found"));
        assert!(store.get_workspace(src_workspace).await.is_some());
        assert_eq!(store.tab_count_in_pane(src_pane).await, Some(1));
        assert!(store.surface_title(dst_pane, moved_surface).await.is_some());
    }

    /// Regression: closing the split sibling must keep the surviving pane's
    /// underlying terminal widget instance alive. Pane-level widgets (the
    /// `gtk::Frame` and the `gtk::DrawingArea` it wraps) own the live PTY child
    /// process, so any path that swaps them out kills running programs like
    /// claude / codex / shells. The earlier `rerender_workspace` fallback did
    /// exactly that. This test pins the contract for the incremental path:
    /// the same widget instance survives split, survives close-of-sibling,
    /// and the pane's terminal is reachable through the registry.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn closing_split_sibling_preserves_surviving_pane_terminal_widget_identity() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-close-sibling-terminal");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let original = ws.surfaces[0].root_pane.first_leaf_id().unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.CloseSiblingTerminal")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);

        // Snapshot the original pane's terminal widget + frame BEFORE the split so we
        // can compare object identity through every subsequent rebuild.
        let original_terminal_pre_split = {
            let r = controller.pane_registry.borrow();
            r.active_terminal(original)
                .expect("rendered workspace should expose a terminal for the only pane")
                .render_area()
                .clone()
        };
        let original_frame_pre_split = controller
            .pane_registry
            .borrow()
            .pane_frame(original)
            .expect("frame should be registered for the only pane");

        // 1. Split horizontally so the workspace becomes Paned(left=original, right=new).
        //    apply_split_incremental_or_rerender must reuse `original`'s frame
        //    inside the new gtk::Paned, not rebuild it.
        let (split_ws, sibling) = store
            .split_pane(original, SplitDirection::Vertical)
            .await
            .expect("split should succeed");
        assert_eq!(split_ws, ws_id);
        controller
            .apply_split_incremental_or_rerender(ws_id, original, sibling, SplitDirection::Vertical)
            .await;

        let original_terminal_after_split = controller
            .pane_registry
            .borrow()
            .active_terminal(original)
            .expect("original pane must still have an active terminal after split")
            .render_area()
            .clone();
        let original_frame_after_split = controller
            .pane_registry
            .borrow()
            .pane_frame(original)
            .expect("original pane frame must still be registered after split");

        assert!(
            original_terminal_pre_split == original_terminal_after_split,
            "split rebuilt the surviving pane's terminal widget — that would kill any running PTY child (claude/codex/shell)"
        );
        assert!(
            original_frame_pre_split == original_frame_after_split,
            "split rebuilt the surviving pane's gtk::Frame — incremental split must reuse the existing frame"
        );

        // 2. Close the new sibling. `apply_close_pane_incremental_or_rerender`
        //    must collapse the Paned in place, leaving the original pane's
        //    frame as the workspace's top-level child without rebuilding it.
        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::CloseFocused {
                pane: sibling,
                ack: ack_tx,
            })
            .await;
        ack_rx.await.unwrap().unwrap();

        let original_terminal_after_close = controller
            .pane_registry
            .borrow()
            .active_terminal(original)
            .expect(
                "regression: closing the split sibling dropped the surviving pane's terminal entry — \
                 a fresh terminal means the running shell / agent was killed",
            )
            .render_area()
            .clone();
        let original_frame_after_close = controller
            .pane_registry
            .borrow()
            .pane_frame(original)
            .expect("regression: surviving pane's frame should still be registered after close");

        assert!(
            original_terminal_pre_split == original_terminal_after_close,
            "regression: closing the split sibling rebuilt the surviving pane's terminal — the running PTY child was killed and the user sees a fresh empty terminal instead of their claude/codex session"
        );
        assert!(
            original_frame_pre_split == original_frame_after_close,
            "regression: closing the split sibling replaced the surviving pane's gtk::Frame instance"
        );

        // The sibling's registry entries must have been forgotten, and the
        // workspace stack must point at the surviving frame.
        assert!(
            controller
                .pane_registry
                .borrow()
                .pane_frame(sibling)
                .is_none(),
            "closed sibling should no longer be in the registry"
        );
        let surfaces = controller.surfaces.borrow();
        let top = surfaces
            .get(&ws_id)
            .expect("workspace stack must have a top-level widget after collapse");
        assert!(
            top == &original_frame_after_close,
            "workspace stack child should now be the surviving pane's frame, not the old paned"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn pane_split_applied_preserves_existing_terminal_widget_identity() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.PaneSplitApplied").await;
        let original_terminal = {
            let registry = controller.pane_registry.borrow();
            registry
                .active_terminal(pane)
                .expect("source pane should have an active terminal")
                .render_area()
                .clone()
        };

        let (split_ws, new_pane) = controller
            .store
            .split_pane(pane, SplitDirection::Vertical)
            .await
            .expect("split should succeed");
        assert_eq!(split_ws, ws_id);

        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::PaneSplitApplied {
                id: ws_id,
                pane,
                new_pane,
                direction: SplitDirection::Vertical,
                ack: ack_tx,
            })
            .await;
        ack_rx.await.unwrap();

        let registry = controller.pane_registry.borrow();
        let current_terminal = registry
            .active_terminal(pane)
            .expect("source pane terminal should survive the split");
        assert!(
            current_terminal.render_area().clone() == original_terminal,
            "CLI-applied split must reuse the existing terminal widget"
        );
        assert!(
            registry.active_terminal(new_pane).is_some(),
            "new split pane should get its own terminal"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn browser_open_split_preserves_source_terminal_widget_identity() {
        let (controller, _ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.BrowserOpenSplitPreserve")
                .await;
        let original_terminal = {
            let registry = controller.pane_registry.borrow();
            registry
                .active_terminal(pane)
                .expect("source pane should have an active terminal")
                .render_area()
                .clone()
        };

        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::BrowserOpenSplit {
                target_pane: Some(pane),
                url: "https://example.com".into(),
                direction: SplitDirection::Vertical,
                ack: ack_tx,
            })
            .await;
        let outcome = ack_rx
            .await
            .expect("browser open ack should be sent")
            .expect("browser open should succeed");

        let registry = controller.pane_registry.borrow();
        let current_terminal = registry
            .active_terminal(pane)
            .expect("source pane terminal should survive browser split");
        assert!(
            current_terminal.render_area().clone() == original_terminal,
            "CLI browser open split must reuse the source terminal widget"
        );
        assert!(
            registry.active_browser(outcome.pane).is_some(),
            "browser open split should create a browser pane"
        );
        assert_eq!(outcome.placement_strategy, PlacementStrategy::SplitRight);
    }

    /// Regression: same terminal-identity contract across nested splits — the
    /// scenario the user reported was a deeper split tree, not a flat
    /// side-by-side. Build Pane A (claude) → split A right to get B → focus B
    /// → split B down to get C, so the tree is Split{A, Split{B, C}} with two
    /// levels of `gtk::Paned`. Closing C must collapse only the inner paned
    /// and leave A and B's terminal widgets intact.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn closing_inner_pane_in_two_level_split_preserves_other_panes() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-close-nested");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane_a = ws.surfaces[0].root_pane.first_leaf_id().unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.CloseNested")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);

        let a_terminal_initial = controller
            .pane_registry
            .borrow()
            .active_terminal(pane_a)
            .expect("pane A terminal must be registered")
            .render_area()
            .clone();
        let a_frame_initial = controller
            .pane_registry
            .borrow()
            .pane_frame(pane_a)
            .expect("pane A frame must be registered");

        // First split: A | B (vertical split → side-by-side).
        let (_, pane_b) = store
            .split_pane(pane_a, SplitDirection::Vertical)
            .await
            .expect("first split should succeed");
        controller
            .apply_split_incremental_or_rerender(ws_id, pane_a, pane_b, SplitDirection::Vertical)
            .await;

        let b_terminal_initial = controller
            .pane_registry
            .borrow()
            .active_terminal(pane_b)
            .expect("pane B terminal must be registered after first split")
            .render_area()
            .clone();

        // Second split: split B horizontally → B (top) over C (bottom).
        let (_, pane_c) = store
            .split_pane(pane_b, SplitDirection::Horizontal)
            .await
            .expect("second split should succeed");
        controller
            .apply_split_incremental_or_rerender(ws_id, pane_b, pane_c, SplitDirection::Horizontal)
            .await;

        // Sanity: A and B widgets identity unchanged across both splits.
        let a_terminal_after_splits = controller
            .pane_registry
            .borrow()
            .active_terminal(pane_a)
            .expect("pane A terminal must survive both splits")
            .render_area()
            .clone();
        let b_terminal_after_splits = controller
            .pane_registry
            .borrow()
            .active_terminal(pane_b)
            .expect("pane B terminal must survive its own split")
            .render_area()
            .clone();
        assert!(
            a_terminal_initial == a_terminal_after_splits,
            "pane A's terminal must be identical across nested splits"
        );
        assert!(
            b_terminal_initial == b_terminal_after_splits,
            "pane B's terminal must survive its own split"
        );

        // Close C (the deepest, newest pane). Tree should collapse to Split{A, B}.
        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::CloseFocused {
                pane: pane_c,
                ack: ack_tx,
            })
            .await;
        ack_rx.await.unwrap().unwrap();

        let a_terminal_after_close = controller
            .pane_registry
            .borrow()
            .active_terminal(pane_a)
            .expect(
                "regression: pane A vanished from registry — the close fell back to a full \
                 rerender and any agent running in A is now dead",
            )
            .render_area()
            .clone();
        let b_terminal_after_close = controller
            .pane_registry
            .borrow()
            .active_terminal(pane_b)
            .expect("regression: pane B vanished from registry after closing inner sibling C")
            .render_area()
            .clone();
        let a_frame_after_close = controller
            .pane_registry
            .borrow()
            .pane_frame(pane_a)
            .expect("pane A's frame must still be registered after closing inner pane C");

        assert!(
            a_terminal_initial == a_terminal_after_close,
            "regression: closing inner pane C rebuilt pane A's terminal widget — claude/codex/shell killed"
        );
        assert!(
            b_terminal_initial == b_terminal_after_close,
            "regression: closing inner pane C rebuilt pane B's terminal widget"
        );
        assert!(
            a_frame_initial == a_frame_after_close,
            "regression: closing inner pane C rebuilt pane A's gtk::Frame"
        );
        assert!(
            controller
                .pane_registry
                .borrow()
                .pane_frame(pane_c)
                .is_none(),
            "pane C should be forgotten by the registry"
        );

        // Daemon-side state must agree: tree collapsed to two leaves [A, B].
        let ws_after = store.get_workspace(ws_id).await.unwrap();
        let mut leaves: std::collections::HashSet<PaneId> = std::collections::HashSet::new();
        ws_after.surfaces[0].root_pane.for_each_leaf(|id| {
            leaves.insert(id);
        });
        let expected: std::collections::HashSet<PaneId> = [pane_a, pane_b].into_iter().collect();
        assert_eq!(
            leaves, expected,
            "store should have collapsed the inner split to {{A, B}}"
        );
    }

    /// Regression: closing the currently focused pane (X-button on its tab,
    /// or Alt+W) used to leave `focused_pane` pointing at the now-removed
    /// PaneId, so the user lost keyboard focus entirely. Subsequent
    /// `Alt+arrow` calls then fell back to "no pane focused, focus the first
    /// leaf" instead of moving relative to where the user was.
    ///
    /// Expected behaviour: focus jumps to the most recently focused pane
    /// that still exists — i.e. the MRU head after we drop the closed pane.
    /// We exercise the nested-split shape the user actually reported
    /// (`A | (B / C)` — two levels of `gtk::Paned`) because in the flat
    /// side-by-side shape GTK's own "find a new focus child" pass on a
    /// `gtk::Stack` swap accidentally hides the bug; it is the
    /// grand-paned `set_*_child` slot replacement that does NOT auto-
    /// hand focus to the surviving sibling, so an explicit handoff is
    /// required there.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn closing_focused_pane_in_nested_split_moves_focus_to_previous_pane() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-close-focus-prev-nested");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane_a = ws.surfaces[0].root_pane.first_leaf_id().unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.CloseFocusedPrevNested")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);

        // First split: A | B (vertical → side-by-side at the workspace root).
        let (_, pane_b) = store
            .split_pane(pane_a, SplitDirection::Vertical)
            .await
            .expect("first split should succeed");
        controller
            .apply_split_incremental_or_rerender(ws_id, pane_a, pane_b, SplitDirection::Vertical)
            .await;

        // Second split: B / C (horizontal → top/bottom inside the right slot).
        // Tree shape: Split{ Leaf(A), Split{ Leaf(B), Leaf(C) } }. Closing C
        // collapses the inner gtk::Paned via grand_paned.set_end_child(B), which
        // is the path that did not auto-transfer focus.
        let (_, pane_c) = store
            .split_pane(pane_b, SplitDirection::Horizontal)
            .await
            .expect("second split should succeed");
        controller
            .apply_split_incremental_or_rerender(ws_id, pane_b, pane_c, SplitDirection::Horizontal)
            .await;

        // Reproduce the user's focus history so MRU = [C, B, A] (C is current,
        // B was previous). Closing the focused pane C should hand focus back to B.
        for p in [pane_a, pane_b, pane_c] {
            controller.focused_pane.set(Some(p));
            controller
                .dispatch(GtkCommand::PaneFocused { pane: p })
                .await;
        }
        assert_eq!(controller.focused_pane.get(), Some(pane_c));

        // Close the focused pane C (X-button on its tab, or Alt+W).
        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::CloseFocused {
                pane: pane_c,
                ack: ack_tx,
            })
            .await;
        ack_rx.await.unwrap().unwrap();

        // grab_focus from focus_after_close runs inside an idle handler. Pump
        // one idle via a oneshot so the on_focus callback (wired through
        // EventControllerFocus) has a chance to update focused_pane.
        let (idle_tx, idle_rx) = oneshot::channel();
        glib::idle_add_local_once(move || {
            let _ = idle_tx.send(());
        });
        let _ = idle_rx.await;

        assert_eq!(
            controller.focused_pane.get(),
            Some(pane_b),
            "regression: closing the focused pane in a nested split must hand focus to the previous MRU pane (B), not leave focused_pane stuck on the removed C"
        );
        assert!(
            controller
                .pane_registry
                .borrow()
                .pane_frame(pane_c)
                .is_none(),
            "closed pane C should be forgotten by the registry"
        );
    }

    /// Regression-companion: closing a *non-focused* pane must not steal
    /// focus from whichever pane the user is actually typing in. This pins
    /// the contract that `focus_after_close` is a no-op when the closed
    /// pane wasn't the focused one — without it, an over-eager "always
    /// pick MRU head" would hijack focus on every X-button click.
    ///
    /// We use the same nested-split shape as the focused-close test so the
    /// close path actually exercises grand_paned slot replacement (the only
    /// path where focus_after_close can disagree with GTK's own focus
    /// chain handling).
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn closing_unfocused_pane_in_nested_split_keeps_focus_where_it_was() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-close-unfocused-keep-nested");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane_a = ws.surfaces[0].root_pane.first_leaf_id().unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.CloseUnfocusedKeepNested")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);

        let (_, pane_b) = store
            .split_pane(pane_a, SplitDirection::Vertical)
            .await
            .expect("first split should succeed");
        controller
            .apply_split_incremental_or_rerender(ws_id, pane_a, pane_b, SplitDirection::Vertical)
            .await;
        let (_, pane_c) = store
            .split_pane(pane_b, SplitDirection::Horizontal)
            .await
            .expect("second split should succeed");
        controller
            .apply_split_incremental_or_rerender(ws_id, pane_b, pane_c, SplitDirection::Horizontal)
            .await;

        // Focus order: C → B → A, so MRU = [A, B, C] and the user is typing
        // in A. Closing the unfocused pane C must leave A still focused.
        for p in [pane_c, pane_b, pane_a] {
            controller.focused_pane.set(Some(p));
            controller
                .dispatch(GtkCommand::PaneFocused { pane: p })
                .await;
        }
        assert_eq!(controller.focused_pane.get(), Some(pane_a));

        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::CloseFocused {
                pane: pane_c,
                ack: ack_tx,
            })
            .await;
        ack_rx.await.unwrap().unwrap();

        let (idle_tx, idle_rx) = oneshot::channel();
        glib::idle_add_local_once(move || {
            let _ = idle_tx.send(());
        });
        let _ = idle_rx.await;

        assert_eq!(
            controller.focused_pane.get(),
            Some(pane_a),
            "closing an unfocused pane must not steal focus from the pane the user is typing in"
        );
    }

    /// Regression: Alt+W triggers `win.close-surface` (CloseSurface),
    /// not the X-button's CloseFocused. When the focused pane has only
    /// one tab CloseSurface falls through to the daemon's `PaneRemoved`
    /// outcome, but the GTK side originally only ran the incremental
    /// collapse and forgot to hand focus to a sibling — leaving
    /// `focused_pane` pointing at the dead pane id. The user reported
    /// this as "Alt+W on a multi-split pane stops focusing anything".
    ///
    /// Same nested-split shape as the CloseFocused regression so the
    /// grand-paned slot replacement is exercised; only the dispatched
    /// command differs.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn closing_focused_pane_via_alt_w_in_nested_split_focuses_sibling() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-close-surface-prev-nested");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane_a = ws.surfaces[0].root_pane.first_leaf_id().unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.CloseSurfacePrevNested")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);

        // Tree shape: Split{ Leaf(A), Split{ Leaf(B), Leaf(C) } }.
        let (_, pane_b) = store
            .split_pane(pane_a, SplitDirection::Vertical)
            .await
            .expect("first split should succeed");
        controller
            .apply_split_incremental_or_rerender(ws_id, pane_a, pane_b, SplitDirection::Vertical)
            .await;
        let (_, pane_c) = store
            .split_pane(pane_b, SplitDirection::Horizontal)
            .await
            .expect("second split should succeed");
        controller
            .apply_split_incremental_or_rerender(ws_id, pane_b, pane_c, SplitDirection::Horizontal)
            .await;

        for p in [pane_a, pane_b, pane_c] {
            controller.focused_pane.set(Some(p));
            controller
                .dispatch(GtkCommand::PaneFocused { pane: p })
                .await;
        }
        assert_eq!(controller.focused_pane.get(), Some(pane_c));

        // Alt+W path: keybindings.rs sends CloseSurface with the active
        // surface of the focused pane, not CloseFocused.
        let surface_c = controller
            .pane_registry
            .borrow()
            .active_surface(pane_c)
            .expect("focused pane must have an active surface");
        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::CloseSurface {
                pane: pane_c,
                surface: surface_c,
                ack: ack_tx,
            })
            .await;
        ack_rx.await.unwrap().unwrap();

        // grab_focus runs from an idle handler; pump one idle cycle so the
        // EventControllerFocus on_focus callback updates focused_pane.
        let (idle_tx, idle_rx) = oneshot::channel();
        glib::idle_add_local_once(move || {
            let _ = idle_tx.send(());
        });
        let _ = idle_rx.await;

        assert_eq!(
            controller.focused_pane.get(),
            Some(pane_b),
            "regression: Alt+W (CloseSurface) on the focused pane in a nested split must hand focus to the previous MRU sibling, not leave focused_pane stuck on the removed pane id"
        );
        assert!(
            controller
                .pane_registry
                .borrow()
                .pane_frame(pane_c)
                .is_none(),
            "closed pane C should be forgotten by the registry"
        );
    }

    // ===== Dock-badge / unread sweep scenarios =====
    //
    // The dock badge is driven by `NotificationStore::unread_count()` —
    // every dispatcher path that flips an entry to read must leave
    // `unread_count()` at the value the launcher should publish next.
    // The tests below exercise the full dispatch loop (`AddNotification`,
    // `ActivateWorkspace`, `SetNotificationDesktopId`,
    // `CloseDesktopNotifications`) for the scenarios that historically
    // left the dock badge stuck on a stale value: rapid push + activate
    // sequences, multi-workspace isolation, repeat activation, global
    // notifications, and late desktop-id races.

    /// Helper: build a controller with a single workspace and return
    /// `(controller, ws_id, pane_id)`.
    async fn build_single_workspace_controller(
        app_id: &str,
    ) -> (WindowController, WorkspaceId, PaneId) {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join(format!("flowmux-badge-{app_id}"));
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store.create_workspace(Some("ws".into()), root).await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder().application_id(app_id).build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        // `WindowController::new` loads the developer's real options.json, so
        // pin options that affect these assertions.
        {
            let mut options = controller.options.borrow_mut();
            options.system_notifications_enabled = true;
            options.agent_bar_enabled = true;
        }
        controller.render_workspace(&ws);
        store.set_active_workspace(Some(ws_id)).await;
        (controller, ws_id, pane)
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn set_agent_status_dispatch_refreshes_agent_bar_visibility() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.AgentBarSetStatus").await;
        let surface = controller
            .pane_registry
            .borrow()
            .active_surface(pane)
            .expect("single workspace pane should have an active surface");

        assert!(
            !agent_bar_visible(&controller),
            "Agent Bar should stay hidden while no live agents exist"
        );

        controller
            .store
            .set_agent_activity(
                surface,
                Some(flowmux_core::AgentPresence::new(
                    "codex",
                    flowmux_core::AgentActivity::Running,
                    Some(42),
                )),
            )
            .await;
        controller
            .dispatch(GtkCommand::SetAgentStatus { workspace: ws_id })
            .await;
        assert!(
            agent_bar_visible(&controller),
            "SetAgentStatus must refresh Agent Bar after a live agent appears"
        );

        controller.store.set_agent_activity(surface, None).await;
        controller
            .dispatch(GtkCommand::SetAgentStatus { workspace: ws_id })
            .await;
        assert!(
            !agent_bar_visible(&controller),
            "SetAgentStatus must hide Agent Bar after the last agent is cleared"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn agent_bar_option_hides_live_agent_bar() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.AgentBarOption").await;
        let surface = controller
            .pane_registry
            .borrow()
            .active_surface(pane)
            .expect("single workspace pane should have an active surface");

        controller.options.borrow_mut().agent_bar_enabled = false;
        controller
            .store
            .set_agent_activity(
                surface,
                Some(flowmux_core::AgentPresence::new(
                    "codex",
                    flowmux_core::AgentActivity::Running,
                    Some(42),
                )),
            )
            .await;
        controller
            .dispatch(GtkCommand::SetAgentStatus { workspace: ws_id })
            .await;
        assert!(
            !agent_bar_visible(&controller),
            "disabled Agent Bar option must hide live agent items"
        );

        controller.options.borrow_mut().agent_bar_enabled = true;
        controller.refresh_agent_bar().await;
        assert!(
            agent_bar_visible(&controller),
            "re-enabling Agent Bar should render existing live agents"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn open_agent_bar_item_activates_workspace_pane_and_surface() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root_a = std::env::temp_dir().join("flowmux-agent-bar-click-a");
        let root_b = std::env::temp_dir().join("flowmux-agent-bar-click-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_a = store
            .create_workspace(Some("a".into()), root_a.clone())
            .await;
        let ws_b = store
            .create_workspace(Some("b".into()), root_b.clone())
            .await;
        let ws_a_model = store.get_workspace(ws_a).await.unwrap();
        let ws_b_model = store.get_workspace(ws_b).await.unwrap();
        let pane_a = ws_a_model.surfaces[0].root_pane.first_leaf_id().unwrap();
        let pane_b = ws_b_model.surfaces[0].root_pane.first_leaf_id().unwrap();
        let surface_b = ws_b_model.surfaces[0]
            .root_pane
            .active_surface_id(pane_b)
            .unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.AgentBarItemClick")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws_a_model);
        controller.render_workspace(&ws_b_model);
        store.set_active_workspace(Some(ws_a)).await;
        controller.stack.set_visible_child_name(&ws_a.to_string());
        controller.focused_pane.set(Some(pane_a));
        controller.window.present();
        glib::timeout_future(std::time::Duration::from_millis(50)).await;

        store
            .set_agent_activity(
                surface_b,
                Some(flowmux_core::AgentPresence::new(
                    "cline",
                    flowmux_core::AgentActivity::Idle,
                    Some(42),
                )),
            )
            .await;
        controller
            .sync_workspace_agent_status_from_store(ws_b)
            .await;
        assert!(
            agent_bar_visible(&controller),
            "precondition: Agent Bar is visible before clicking an item"
        );

        controller
            .dispatch(GtkCommand::OpenAgentBarItem {
                workspace: ws_b,
                pane: pane_b,
                surface: surface_b,
            })
            .await;
        let (idle_tx, idle_rx) = oneshot::channel();
        glib::idle_add_local_once(move || {
            let _ = idle_tx.send(());
        });
        let _ = idle_rx.await;

        assert_eq!(store.snapshot().await.active_workspace, Some(ws_b));
        assert_eq!(controller.focused_pane.get(), Some(pane_b));
        assert_eq!(
            controller.pane_registry.borrow().active_surface(pane_b),
            Some(surface_b)
        );
        assert!(
            controller
                .pane_registry
                .borrow()
                .active_terminal(pane_b)
                .is_some_and(|terminal| terminal.widget.has_focus()),
            "Agent Bar activation should leave keyboard focus on the target terminal"
        );
        assert_eq!(
            controller
                .stack
                .visible_child_name()
                .map(|name| name.to_string()),
            Some(ws_b.to_string())
        );
    }

    /// Push a notification through the same `GtkCommand::AddNotification`
    /// path the IPC handler uses, returning the entry id (or `None` when
    /// the controller suppressed the toast).
    async fn push_notification(
        controller: &WindowController,
        pane: Option<PaneId>,
        workspace: Option<WorkspaceId>,
        title: &str,
    ) -> Option<flowmux_core::NotificationId> {
        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::AddNotification {
                pane,
                surface: None,
                workspace,
                title: title.into(),
                body: String::new(),
                level: flowmux_core::NotificationLevel::NeedsInput,
                ack: ack_tx,
            })
            .await;
        ack_rx.await.unwrap()
    }

    #[test]
    fn command_palette_exposes_roadmap_actions() {
        let labels: Vec<_> = command_palette_commands()
            .iter()
            .map(|command| command_palette_label(*command))
            .collect();
        assert_eq!(
            labels,
            vec![
                "Open browser",
                "Rename tab",
                "Reload config",
                "Open unread notification"
            ]
        );
    }

    #[test]
    fn workspace_template_preview_describes_all_panes() {
        let preview = command_palette::workspace_template_preview(
            &command_palette::development_workspace_template(),
        );
        assert_eq!(
            preview,
            "• agent: terminal\n• browser: browser — about:blank\n• tests: terminal"
        );
    }

    #[tokio::test]
    async fn workspace_template_materializes_three_panes() {
        let store = StateStore::new_lazy(State::default());
        let base_dir = std::path::PathBuf::from("/tmp/flowmux-template-project");
        let template = command_palette::development_workspace_template();

        let materialized = command_palette::materialize_workspace_template(
            &store,
            &base_dir,
            &std::collections::BTreeMap::new(),
            &template,
        )
        .await
        .unwrap();
        let workspace = store.get_workspace(materialized.workspace).await.unwrap();
        let mut panes = Vec::new();
        workspace.surfaces[0]
            .root_pane
            .for_each_leaf(|pane| panes.push(pane));
        let kinds: Vec<_> = panes
            .iter()
            .filter_map(|pane| {
                let PaneContent::Tabs { surfaces, .. } =
                    workspace.surfaces[0].root_pane.find_leaf_content(*pane)?
                else {
                    return None;
                };
                surfaces.first().map(|surface| match surface.kind {
                    SurfaceKind::Terminal { .. } => "terminal",
                    SurfaceKind::Browser { .. } => "browser",
                    SurfaceKind::Editor { .. } => "editor",
                })
            })
            .collect();

        assert_eq!(workspace.name, "Agent + tests + browser");
        assert_eq!(
            workspace.custom_title.as_deref(),
            Some("Agent + tests + browser")
        );
        assert_eq!(panes.len(), 3);
        assert_eq!(kinds.iter().filter(|kind| **kind == "terminal").count(), 2);
        assert_eq!(kinds.iter().filter(|kind| **kind == "browser").count(), 1);
        assert!(materialized.terminal_commands.is_empty());
    }

    #[test]
    fn focused_source_suppresses_completed_but_not_blocking_notifications() {
        assert!(should_suppress_notification(
            flowmux_core::NotificationLevel::TurnCompleted,
            true
        ));
        assert!(!should_suppress_notification(
            flowmux_core::NotificationLevel::TurnCompleted,
            false
        ));
        assert!(!should_suppress_notification(
            flowmux_core::NotificationLevel::NeedsInput,
            true
        ));
        assert!(!should_suppress_notification(
            flowmux_core::NotificationLevel::Error,
            true
        ));
    }

    #[test]
    fn agent_seen_requires_active_window_focused_pane_and_active_surface() {
        let pane = PaneId::new();
        let surface = SurfaceId::new();
        assert!(agent_surface_is_visible(
            true,
            Some(pane),
            Some(pane),
            Some(surface),
            surface
        ));
        assert!(!agent_surface_is_visible(
            false,
            Some(pane),
            Some(pane),
            Some(surface),
            surface
        ));
        assert!(!agent_surface_is_visible(
            true,
            Some(PaneId::new()),
            Some(pane),
            Some(surface),
            surface
        ));
        assert!(!agent_surface_is_visible(
            true,
            Some(pane),
            Some(pane),
            Some(SurfaceId::new()),
            surface
        ));
    }

    #[test]
    fn browser_wait_js_covers_supported_conditions() {
        assert!(
            browser_wait_js(&BrowserWaitCondition::Selector(".ready".into()))
                .contains("document.querySelector")
        );
        assert!(browser_wait_js(&BrowserWaitCondition::Text("done".into()))
            .contains("innerText.includes"));
        assert!(
            browser_wait_js(&BrowserWaitCondition::Url("/dashboard".into()))
                .contains("location.href")
        );
        assert!(
            browser_wait_js(&BrowserWaitCondition::ReadyState("complete".into()))
                .contains("document.readyState")
        );
        assert!(
            browser_wait_js(&BrowserWaitCondition::Js("document.body !== null".into()))
                .contains("Function")
        );
    }

    #[test]
    fn project_command_shell_line_applies_cwd_env_and_quoting() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("NODE_ENV".to_string(), "test env".to_string());
        let command = CustomCommand {
            id: "test".into(),
            label: "Run tests".into(),
            run: vec!["pnpm".into(), "test unit".into()],
            cwd: Some("app dir".into()),
            target: CommandTarget::NewSurface,
            confirm: true,
        };

        let line = custom_command_shell_line(std::path::Path::new("/tmp/project"), &env, &command)
            .unwrap();
        assert_eq!(
            line,
            "cd '/tmp/project/app dir' && env 'NODE_ENV=test env' pnpm 'test unit'"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn notification_management_dispatch_lists_marks_and_clears() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.NotificationCli").await;
        let first = push_notification(&controller, Some(pane), Some(ws_id), "first")
            .await
            .unwrap();
        let second = push_notification(&controller, Some(pane), Some(ws_id), "second")
            .await
            .unwrap();

        let (list_tx, list_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::ListNotifications {
                unread_only: false,
                ack: list_tx,
            })
            .await;
        let (entries, unread_count) = list_rx.await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(unread_count, 2);
        assert_eq!(entries[0].id, first);
        assert_eq!(entries[1].id, second);

        let (mark_tx, mark_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::MarkNotificationRead {
                id: first,
                ack: mark_tx,
            })
            .await;
        assert!(mark_rx.await.unwrap());

        let (unread_tx, unread_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::ListNotifications {
                unread_only: true,
                ack: unread_tx,
            })
            .await;
        let (unread_entries, unread_count) = unread_rx.await.unwrap();
        assert_eq!(unread_count, 1);
        assert_eq!(unread_entries.len(), 1);
        assert_eq!(unread_entries[0].id, second);

        let (clear_tx, clear_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::ClearNotifications { ack: clear_tx })
            .await;
        assert!(clear_rx.await.unwrap());
        assert!(controller.notifications.entries().is_empty());
        assert_eq!(controller.notifications.unread_count(), 0);
    }

    /// Two notifications arriving on a workspace, then the user clicks
    /// that workspace in the side panel. After the dispatch sequence
    /// the store must report `unread_count() == 0` — that is the value
    /// the dock receives, so this is the regression guard against the
    /// "badge stays on 2" symptom.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn workspace_activation_sweeps_all_unread_for_that_workspace() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.BadgeSweep").await;
        // Window is inactive in tests (no compositor focus), so
        // `is_source_focused` returns false and the toast is recorded.
        let id_a = push_notification(&controller, Some(pane), Some(ws_id), "a")
            .await
            .expect("first push must record an entry");
        let id_b = push_notification(&controller, Some(pane), Some(ws_id), "b")
            .await
            .expect("second push must record an entry");
        assert_eq!(
            controller.notifications.unread_count(),
            2,
            "two NeedsInput notifications must inflate unread_count to 2",
        );

        // Side-panel click goes through `GtkCommand::ActivateWorkspace`.
        controller
            .dispatch(GtkCommand::ActivateWorkspace { id: ws_id })
            .await;

        assert_eq!(
            controller.notifications.unread_count(),
            0,
            "activating the source workspace must drain unread_count to 0 — the dock badge would otherwise stay pinned on the old total",
        );
        assert!(
            controller.notifications.find(id_a).unwrap().read,
            "entry a should be marked read after workspace activation"
        );
        assert!(
            controller.notifications.find(id_b).unwrap().read,
            "entry b should be marked read after workspace activation"
        );
    }

    /// `FocusPane` (the `flowmux focus-pane` path) grabs focus for a
    /// known pane id and returns a clean error for an unknown one rather
    /// than silently no-op'ing.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn focus_pane_command_acks_known_pane_and_errors_unknown() {
        let (controller, _ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.FocusPane").await;

        let (tx, rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::FocusPane { pane, ack: tx })
            .await;
        assert!(rx.await.unwrap().is_ok(), "known pane should focus");

        let (tx, rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::FocusPane {
                pane: PaneId::new(),
                ack: tx,
            })
            .await;
        assert!(
            rx.await.unwrap().is_err(),
            "unknown pane id must return an error, not silently no-op"
        );
    }

    /// Reactivating an already-active workspace must be a safe no-op for
    /// the badge: nothing to sweep, `unread_count()` already at 0.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn repeat_workspace_activation_is_idempotent_for_unread_count() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.BadgeRepeat").await;
        push_notification(&controller, Some(pane), Some(ws_id), "a").await;
        controller
            .dispatch(GtkCommand::ActivateWorkspace { id: ws_id })
            .await;
        assert_eq!(controller.notifications.unread_count(), 0);
        // A second activation on the same workspace must not reintroduce
        // unread state, panic, or otherwise disturb the dock count.
        controller
            .dispatch(GtkCommand::ActivateWorkspace { id: ws_id })
            .await;
        assert_eq!(
            controller.notifications.unread_count(),
            0,
            "re-activating the same workspace must keep unread_count at 0",
        );
    }

    /// Two workspaces, alarms on each. Activating one must only sweep
    /// that workspace's entries — the other workspace's count stays.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn activating_one_workspace_does_not_sweep_other_workspaces() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root_a = std::env::temp_dir().join("flowmux-badge-iso-a");
        let root_b = std::env::temp_dir().join("flowmux-badge-iso-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_a_id = store.create_workspace(Some("a".into()), root_a).await;
        let ws_b_id = store.create_workspace(Some("b".into()), root_b).await;
        let ws_a = store.get_workspace(ws_a_id).await.unwrap();
        let ws_b = store.get_workspace(ws_b_id).await.unwrap();
        let pane_a = ws_a.surfaces[0].root_pane.first_leaf_id().unwrap();
        let pane_b = ws_b.surfaces[0].root_pane.first_leaf_id().unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.BadgeMultiWs")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws_a);
        controller.render_workspace(&ws_b);
        store.set_active_workspace(Some(ws_a_id)).await;

        push_notification(&controller, Some(pane_a), Some(ws_a_id), "a1").await;
        push_notification(&controller, Some(pane_a), Some(ws_a_id), "a2").await;
        push_notification(&controller, Some(pane_b), Some(ws_b_id), "b1").await;
        assert_eq!(controller.notifications.unread_count(), 3);

        controller
            .dispatch(GtkCommand::ActivateWorkspace { id: ws_a_id })
            .await;
        assert_eq!(
            controller.notifications.unread_count(),
            1,
            "activating ws_a must only sweep ws_a's two entries; ws_b's entry stays unread",
        );

        controller
            .dispatch(GtkCommand::ActivateWorkspace { id: ws_b_id })
            .await;
        assert_eq!(
            controller.notifications.unread_count(),
            0,
            "after activating both workspaces in turn, every unread entry should be drained",
        );
    }

    /// Global notifications (`workspace = None`) must not be swept by a
    /// workspace activation. This is by design — they are only cleared
    /// when the bell popover opens — so the test guards us against
    /// accidentally widening the sweep and silencing global toasts.
    /// `RefreshLauncherBadge` after the activation, however, must run so
    /// the dock count reflects the still-unread global entry.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn workspace_activation_leaves_global_notifications_untouched() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.BadgeGlobal").await;
        // Workspace-bound entry — should be swept.
        push_notification(&controller, Some(pane), Some(ws_id), "ws").await;
        // Global entry (no pane, no workspace).
        let global_id = push_notification(&controller, None, None, "global")
            .await
            .expect("global push must record an entry");
        assert_eq!(controller.notifications.unread_count(), 2);

        controller
            .dispatch(GtkCommand::ActivateWorkspace { id: ws_id })
            .await;

        assert_eq!(
            controller.notifications.unread_count(),
            1,
            "the global entry must remain unread after a workspace activation",
        );
        assert!(
            !controller.notifications.find(global_id).unwrap().read,
            "global entry must stay unread until the bell popover sweeps it",
        );
    }

    /// Late-arriving `SetNotificationDesktopId` (the daemon's `Notify`
    /// reply lands after the user already activated the source
    /// workspace) must (1) detect the staleness via `SetDesktopIdResult`
    /// and (2) leave `unread_count()` at the same value as before — the
    /// entry was already read, so the badge should not regress.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn late_set_desktop_id_after_workspace_sweep_keeps_unread_count_stable() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.BadgeLateRace").await;
        let id = push_notification(&controller, Some(pane), Some(ws_id), "a")
            .await
            .expect("push must record an entry");

        // User activates the workspace before the daemon's reply lands.
        controller
            .dispatch(GtkCommand::ActivateWorkspace { id: ws_id })
            .await;
        assert_eq!(controller.notifications.unread_count(), 0);

        // Late reply: the daemon hands us the desktop_id now. The store
        // must report Stale (already read) and the dispatcher must fire
        // the close + refresh — `unread_count` already 0 stays at 0.
        controller
            .dispatch(GtkCommand::SetNotificationDesktopId {
                id,
                desktop_id: "did-4242".into(),
            })
            .await;

        assert_eq!(
            controller.notifications.unread_count(),
            0,
            "late desktop_id arriving after a sweep must not re-inflate the badge",
        );
        assert_eq!(
            controller.notifications.find(id).unwrap().desktop_id.as_deref(),
            Some("did-4242"),
            "even though the entry is already read, the late desktop_id should still be recorded so any subsequent close path has it",
        );
    }

    /// Rapid sequence — push → push → activate — through the dispatcher,
    /// mirroring the user-visible bug ("two notifications arrive, I
    /// click the workspace, badge stays on 2"). The store is the source
    /// of truth for what the dock badge will publish, so after the
    /// dispatch sequence `unread_count()` must be 0. A follow-up
    /// activation must remain a no-op and not regress the count.
    ///
    /// Note: the publish task itself (`refresh_launcher_badge`) is
    /// scheduled via `glib::MainContext::default().spawn_local` and
    /// short-circuits in headless tests because the FDO daemon is not
    /// reachable; we deliberately do not assert on the
    /// `badge_publisher_busy` / `badge_dirty` internals here because
    /// they are timing-dependent on when the GLib main context schedules
    /// the spawned future. The user-visible invariant is the store
    /// state, which is what the dock would receive next.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn rapid_push_push_activate_sequence_drains_unread_to_zero() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.BadgeRapid").await;

        // Two AddNotification commands followed immediately by
        // ActivateWorkspace — the same dispatch order the IPC handler
        // and side-panel click handler produce in production.
        push_notification(&controller, Some(pane), Some(ws_id), "a").await;
        push_notification(&controller, Some(pane), Some(ws_id), "b").await;
        controller
            .dispatch(GtkCommand::ActivateWorkspace { id: ws_id })
            .await;

        assert_eq!(
            controller.notifications.unread_count(),
            0,
            "rapid push+push+activate must end with an empty unread set",
        );

        // Following no-op activation must keep things at 0 even though
        // the previous publisher task may still be running in the
        // background (no D-Bus in tests, so connect fails and the task
        // exits gracefully).
        controller
            .dispatch(GtkCommand::ActivateWorkspace { id: ws_id })
            .await;
        assert_eq!(controller.notifications.unread_count(), 0);
    }

    /// Notification with `pane = Some(...)` but `workspace = None` (the
    /// IPC handler couldn't resolve a workspace for the pane — e.g.
    /// pane closed between firing the toast and store lookup) must not
    /// be swept by a workspace activation. The bell popover sweep is
    /// still the only path that drains it. This guards against a
    /// regression where a pane-with-no-workspace entry would otherwise
    /// silently keep the dock badge inflated.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn workspace_activation_does_not_sweep_pane_entries_without_workspace() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.BadgeOrphanPane").await;
        // Orphan: pane is set but workspace was unresolved by the IPC
        // handler. mark_workspace_read keys off the workspace field, so
        // this entry remains stuck until the bell popover sweep.
        let orphan = push_notification(&controller, Some(pane), None, "orphan")
            .await
            .expect("push must record an entry");
        push_notification(&controller, Some(pane), Some(ws_id), "ws").await;

        controller
            .dispatch(GtkCommand::ActivateWorkspace { id: ws_id })
            .await;
        assert_eq!(
            controller.notifications.unread_count(),
            1,
            "the workspace-bound entry was swept but the orphan must remain",
        );
        assert!(
            !controller.notifications.find(orphan).unwrap().read,
            "orphan entry must stay unread after a workspace sweep",
        );
    }

    /// Trash button on a bell-popover row dispatches
    /// `GtkCommand::DeleteNotification`. After dispatch the entry must
    /// be gone from the in-process transcript, the unread count must
    /// drop by exactly one for an unread entry, and unrelated entries
    /// must be untouched. Pins the user-visible feature: clicking the
    /// trash icon next to a notification removes that notification
    /// from the popover list while leaving every other entry alone.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn delete_notification_dispatch_removes_only_targeted_unread_entry() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.NotifTrashRemovesUnread")
                .await;
        let id_a = push_notification(&controller, Some(pane), Some(ws_id), "a")
            .await
            .expect("first push must record an entry");
        let id_b = push_notification(&controller, Some(pane), Some(ws_id), "b")
            .await
            .expect("second push must record an entry");
        let id_c = push_notification(&controller, Some(pane), Some(ws_id), "c")
            .await
            .expect("third push must record an entry");
        assert_eq!(controller.notifications.unread_count(), 3);

        // Trash on the middle entry — same dispatch the bell-popover
        // row's trash button issues.
        controller
            .dispatch(GtkCommand::DeleteNotification { id: id_b })
            .await;

        assert!(
            controller.notifications.find(id_b).is_none(),
            "deleted entry must be gone from the in-memory store"
        );
        assert!(
            controller.notifications.find(id_a).is_some(),
            "deleting one entry must not touch unrelated entries"
        );
        assert!(
            controller.notifications.find(id_c).is_some(),
            "deleting a middle entry must leave later entries alone"
        );
        assert_eq!(
            controller.notifications.unread_count(),
            2,
            "deleting one unread entry must drop unread_count by exactly one — this is the value the dock badge republishes"
        );
        // Surviving entries must keep insertion order so the rendered
        // popover still shows newest-at-top correctly.
        let surviving: Vec<_> = controller
            .notifications
            .entries()
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(surviving, vec![id_a, id_c]);
    }

    /// Trash button on an entry the user already opened (read=true)
    /// must still drop the entry from the transcript without changing
    /// the unread count. Without this branch the dispatcher would
    /// republish the badge unnecessarily on every read-row delete,
    /// or — worse — skip the popover refresh and leave a ghost row.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn delete_notification_dispatch_on_read_entry_keeps_unread_count() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.NotifTrashRemovesRead").await;
        let id = push_notification(&controller, Some(pane), Some(ws_id), "old")
            .await
            .expect("push must record an entry");
        // Mark it read directly so we can isolate the read-branch
        // delete from the workspace-sweep path.
        controller.notifications.mark_read(id);
        assert_eq!(controller.notifications.unread_count(), 0);

        controller
            .dispatch(GtkCommand::DeleteNotification { id })
            .await;

        assert!(
            controller.notifications.find(id).is_none(),
            "deleting a read entry must still remove it from the transcript"
        );
        assert_eq!(
            controller.notifications.unread_count(),
            0,
            "deleting an already-read entry must not move the unread count"
        );
    }

    /// Trash button on an id the store no longer knows about (e.g. the
    /// entry already aged out under MAX_RETAINED, or two trash clicks
    /// raced) must be a safe no-op — no panic, no badge change, no
    /// FDO close roundtrip. This pins the dispatcher's `Unknown` arm
    /// so a future refactor that removes it surfaces here.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn delete_notification_dispatch_on_unknown_id_is_safe_noop() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.NotifTrashUnknownIsNoop")
                .await;
        push_notification(&controller, Some(pane), Some(ws_id), "kept").await;
        let unread_before = controller.notifications.unread_count();
        let count_before = controller.notifications.entries().len();

        controller
            .dispatch(GtkCommand::DeleteNotification {
                id: flowmux_core::NotificationId::new(),
            })
            .await;

        assert_eq!(
            controller.notifications.entries().len(),
            count_before,
            "Unknown id must not delete an unrelated entry"
        );
        assert_eq!(
            controller.notifications.unread_count(),
            unread_before,
            "Unknown id must not change unread_count"
        );
    }

    // ---------------------------------------------------------------
    // Scenario tests for the bell-popover open path
    // ---------------------------------------------------------------
    //
    // The Sidebar wires `bell_popover.connect_show` to:
    //
    //   1. `notifications.mark_all_unread_read()` — synchronous flip.
    //   2. dispatch `CloseDesktopNotifications { ids }` if non-empty.
    //   3. dispatch `RefreshLauncherBadge`.
    //
    // We exercise the same three-step sequence here so the dispatch arms
    // get covered without driving real GTK signals from a headless test.

    /// One NeedsInput notification arrives. The user opens the bell
    /// popover. The popover-open sequence (mark_all_unread_read +
    /// CloseDesktopNotifications + RefreshLauncherBadge) must drain
    /// `unread_count()` to 0 and surface the matching desktop_id so the
    /// dispatcher would close the FDO toast.
    ///
    /// This pins the user-visible regression: a single notification, the
    /// user taps the bell, the dock badge does not go down. The store
    /// is the source of truth for what we re-publish, so verifying it
    /// drains to 0 here is equivalent to verifying the next
    /// `update_launcher_count` call would carry `count = 0`.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn popover_open_sequence_drains_single_notification_to_zero() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.PopoverOpenSingle").await;
        let id = push_notification(&controller, Some(pane), Some(ws_id), "alarm")
            .await
            .expect("push must record an entry");
        // Daemon's Notify reply: attach the FDO id the same way the IPC
        // handler does in production.
        controller
            .dispatch(GtkCommand::SetNotificationDesktopId {
                id,
                desktop_id: "did-9001".into(),
            })
            .await;
        assert_eq!(controller.notifications.unread_count(), 1);

        // Replicate Sidebar::connect_show step (1): the synchronous
        // mark-read sweep that runs on the GTK thread when the user
        // pops the bell.
        let to_close = controller.notifications.mark_all_unread_read();
        assert_eq!(
            to_close,
            vec!["did-9001".to_string()],
            "the popover sweep must surface the desktop_id so the dispatcher \
             can withdraw the desktop toast in lockstep with marking the entry read",
        );
        // Step (2): the dispatcher closes the toast on the FDO daemon.
        controller
            .dispatch(GtkCommand::CloseDesktopNotifications {
                desktop_ids: to_close,
            })
            .await;
        // Step (3): refresh re-publishes the unread count to the dock.
        controller.dispatch(GtkCommand::RefreshLauncherBadge).await;

        assert_eq!(
            controller.notifications.unread_count(),
            0,
            "after the popover open sequence, unread_count must be 0 — \
             this is the value the next LauncherEntry signal carries to the dock",
        );
        assert!(
            controller.notifications.find(id).unwrap().read,
            "the entry must be marked read so a re-render of the popover \
             dims the row instead of leaving it bold",
        );
    }

    /// User opens the bell popover before the daemon's `Notify` reply
    /// has carried the desktop_id back. `mark_all_unread_read` returns
    /// an empty vec (no FDO ids known yet), but the entry is still
    /// flipped to read. Dispatching `RefreshLauncherBadge` alone must
    /// be enough to drain the badge — no `CloseDesktopNotifications`
    /// roundtrip happens because there is no id to close.
    ///
    /// Then the late `SetNotificationDesktopId` arrives. The store
    /// reports `Stale` and the dispatcher fires the close + refresh on
    /// its own. The ENTIRE story must end with `unread_count() = 0`.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn popover_open_then_late_desktop_id_still_drains_badge() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.PopoverLateRace").await;
        let id = push_notification(&controller, Some(pane), Some(ws_id), "alarm")
            .await
            .expect("push must record an entry");
        assert_eq!(controller.notifications.unread_count(), 1);

        // User pops the bell BEFORE the Notify reply lands → no
        // desktop_id available in the sweep.
        let to_close = controller.notifications.mark_all_unread_read();
        assert!(
            to_close.is_empty(),
            "no desktop_id was attached yet; the sweep must return an empty list \
             so the dispatcher does not send a CloseDesktopNotifications no-op",
        );
        controller.dispatch(GtkCommand::RefreshLauncherBadge).await;
        assert_eq!(controller.notifications.unread_count(), 0);

        // Daemon's reply arrives. set_desktop_id reports Stale; the
        // dispatcher closes the toast and refreshes the badge.
        controller
            .dispatch(GtkCommand::SetNotificationDesktopId {
                id,
                desktop_id: "did-4242".into(),
            })
            .await;
        assert_eq!(
            controller.notifications.unread_count(),
            0,
            "the late desktop_id must not re-inflate unread_count — \
             the entry was already read by the popover sweep",
        );
        assert_eq!(
            controller
                .notifications
                .find(id)
                .unwrap()
                .desktop_id
                .as_deref(),
            Some("did-4242"),
            "the late desktop_id must still be recorded so any subsequent close \
             path (e.g. an explicit DeleteNotification) has it",
        );
    }

    /// Bell popover open while three notifications are already
    /// unread — two with desktop_ids attached and one whose Notify
    /// reply is in-flight. The sweep must close the two known toasts
    /// and the badge must drain to 0; the in-flight third entry
    /// reaches the dispatcher later as `Stale`.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn popover_open_with_partial_desktop_ids_still_clears_badge() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.PopoverPartial").await;
        let a = push_notification(&controller, Some(pane), Some(ws_id), "a")
            .await
            .expect("push a");
        let b = push_notification(&controller, Some(pane), Some(ws_id), "b")
            .await
            .expect("push b");
        let c = push_notification(&controller, Some(pane), Some(ws_id), "c")
            .await
            .expect("push c");
        // Two of the three have already been mapped to FDO ids; c is
        // still waiting for the daemon's Notify reply.
        controller
            .dispatch(GtkCommand::SetNotificationDesktopId {
                id: a,
                desktop_id: "did-11".into(),
            })
            .await;
        controller
            .dispatch(GtkCommand::SetNotificationDesktopId {
                id: b,
                desktop_id: "did-22".into(),
            })
            .await;

        let mut to_close = controller.notifications.mark_all_unread_read();
        // Order is insertion order; sort defensively in case the
        // implementation later reorders so this test still pins the
        // contents rather than the ordering.
        to_close.sort();
        assert_eq!(to_close, vec!["did-11".to_string(), "did-22".to_string()]);
        controller
            .dispatch(GtkCommand::CloseDesktopNotifications {
                desktop_ids: to_close,
            })
            .await;
        controller.dispatch(GtkCommand::RefreshLauncherBadge).await;
        assert_eq!(controller.notifications.unread_count(), 0);

        // Late reply for c → Stale → dispatcher closes did-33 and refreshes.
        controller
            .dispatch(GtkCommand::SetNotificationDesktopId {
                id: c,
                desktop_id: "did-33".into(),
            })
            .await;
        assert_eq!(controller.notifications.unread_count(), 0);
    }

    // ---------------------------------------------------------------
    // Stress: hammer the dispatcher with many notifications and
    // overlapping ack gestures, then assert the final invariants.
    // ---------------------------------------------------------------
    //
    // The dispatcher is single-threaded (GTK main loop), so "stress" is
    // about depth of state transitions, not parallelism. We push enough
    // entries to exercise the MAX_RETAINED ring, interleave acks and
    // late desktop-id replies, and finally verify:
    //
    //   * `unread_count()` matches the manually-counted unread entries.
    //   * Every `read` entry is consistent with what the sweeps did.
    //   * No entry id is duplicated or lost.
    //   * The dispatcher coalesces overlapping `RefreshLauncherBadge`
    //     bursts without panicking and the store ends in a sane state.

    /// Push 50 notifications (the MAX_RETAINED cap) interleaved with
    /// SetNotificationDesktopId and periodic workspace-activation
    /// sweeps. After the final activation the badge must be at 0 and
    /// every entry whose desktop_id was attached before the sweep must
    /// be marked read.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn stress_many_notifications_with_periodic_sweeps_drains_to_zero() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.StressManyNotif").await;
        const TOTAL: usize = 50;
        let mut ids = Vec::with_capacity(TOTAL);
        for i in 0..TOTAL {
            let id = push_notification(&controller, Some(pane), Some(ws_id), &format!("evt-{i}"))
                .await
                .expect("push must record an entry");
            ids.push(id);
            // Simulate the daemon's Notify reply for a subset of pushes
            // (mimicking real-world timing where some replies overtake
            // others).
            if i % 3 == 0 {
                controller
                    .dispatch(GtkCommand::SetNotificationDesktopId {
                        id,
                        desktop_id: format!("did-{}", i + 1),
                    })
                    .await;
            }
            // Every 50 entries, sweep the workspace as if the user
            // checked it. The sweep must converge unread_count to the
            // count of entries pushed AFTER this sweep (none yet, since
            // we sweep on the boundary and nothing else has pushed).
            if (i + 1) % 50 == 0 {
                controller
                    .dispatch(GtkCommand::ActivateWorkspace { id: ws_id })
                    .await;
                assert_eq!(
                    controller.notifications.unread_count(),
                    0,
                    "after sweep at i={i}, unread_count must be 0 — every entry up to here \
                     is workspace-bound and ActivateWorkspace flips them all to read",
                );
            }
        }

        // Final state: every push has been ack'd through the periodic
        // ActivateWorkspace sweeps, but the very last sweep happened at
        // i = 199 (when (199+1) % 50 == 0), so unread_count is 0.
        assert_eq!(
            controller.notifications.unread_count(),
            0,
            "after the stress sequence ends on a sweep boundary, unread_count must be 0",
        );

        // The total entry count is capped at MAX_RETAINED (50). The
        // first push is at index 0, the last at TOTAL-1; with TOTAL ==
        // MAX_RETAINED we expect exactly TOTAL entries to survive.
        let entries = controller.notifications.entries();
        assert_eq!(
            entries.len(),
            TOTAL,
            "MAX_RETAINED == TOTAL here, so every entry survives the ring buffer; \
             a future change to MAX_RETAINED must update this test in lockstep",
        );
        // Every surviving entry must be marked read after the final
        // sweep.
        assert!(
            entries.iter().all(|e| e.read),
            "every entry was inside ws_id and got swept by an ActivateWorkspace, \
             so they must all be read",
        );
    }

    /// Stress the popover-open path: push a batch, open the popover,
    /// push another batch, open again, and so on. After every popover
    /// open the badge must be at 0 (because the sweep flipped every
    /// unread entry); pushes between opens must inflate it again. This
    /// pins the symptom "the bell popover sweep does not drop the
    /// badge" against batch arrival patterns.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn stress_popover_open_drains_badge_across_repeated_batches() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.StressPopoverBatches").await;
        const BATCHES: usize = 10;
        const PER_BATCH: usize = 20;
        for batch in 0..BATCHES {
            for i in 0..PER_BATCH {
                let id = push_notification(
                    &controller,
                    Some(pane),
                    Some(ws_id),
                    &format!("b{batch}-i{i}"),
                )
                .await
                .expect("push must record an entry");
                // Map every other entry's desktop_id to mimic the
                // partial-replies regime.
                if i % 2 == 0 {
                    controller
                        .dispatch(GtkCommand::SetNotificationDesktopId {
                            id,
                            desktop_id: format!("did-{}", batch * PER_BATCH + i + 1),
                        })
                        .await;
                }
            }
            assert_eq!(
                controller.notifications.unread_count(),
                PER_BATCH,
                "before the popover sweep at batch {batch}, every entry from this batch is unread",
            );
            // Simulate the popover-open sequence end-to-end.
            let to_close = controller.notifications.mark_all_unread_read();
            controller
                .dispatch(GtkCommand::CloseDesktopNotifications {
                    desktop_ids: to_close,
                })
                .await;
            controller.dispatch(GtkCommand::RefreshLauncherBadge).await;
            assert_eq!(
                controller.notifications.unread_count(),
                0,
                "after the popover sweep at batch {batch}, the badge must drain to 0 — \
                 even when half the entries had no desktop_id attached yet",
            );
        }
        // Final: every entry should be marked read.
        let entries = controller.notifications.entries();
        assert!(
            entries.iter().all(|e| e.read),
            "every push should have been swept by one of the popover opens",
        );
    }

    /// Adversarial scenario: mix every ack channel concurrently to
    /// surface any state drift between the in-store flip and the
    /// dispatcher's badge republish path. Pushes, workspace
    /// activations, popover sweeps, late `SetNotificationDesktopId`s,
    /// and trash-button deletes are all interleaved. The invariant
    /// after each step is that `unread_count()` equals the number of
    /// entries with `read == false` actually in the store — no entry
    /// can be "ghost unread" or "ghost read".
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn stress_mixed_ack_channels_keep_unread_count_in_sync_with_entries() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.StressMixedAcks").await;

        // The check we run after every dispatched command — the unread
        // count exposed to the dock must match the actual unread set.
        let assert_invariant = |label: &str| {
            let store_count = controller.notifications.unread_count();
            let actual_unread = controller
                .notifications
                .entries()
                .into_iter()
                .filter(|e| !e.read)
                .count();
            assert_eq!(
                store_count, actual_unread,
                "[{label}] unread_count() {store_count} drifted from the entries-with-read-false count {actual_unread}; \
                 the dock badge would publish the wrong number",
            );
        };

        // Sequence a deliberately interleaved set of dispatches.
        let mut pushed = Vec::new();
        for i in 0usize..30 {
            let id = push_notification(&controller, Some(pane), Some(ws_id), &format!("x{i}"))
                .await
                .expect("push");
            pushed.push(id);
            assert_invariant(&format!("after push {i}"));

            match i % 5 {
                0 => {
                    // Late desktop_id on an older entry.
                    if let Some(old) = pushed.get(i.saturating_sub(3)).copied() {
                        controller
                            .dispatch(GtkCommand::SetNotificationDesktopId {
                                id: old,
                                desktop_id: format!("did-{}", i * 100),
                            })
                            .await;
                        assert_invariant(&format!("after set_desktop_id at i={i}"));
                    }
                }
                1 => {
                    // ActivateWorkspace mid-stream — sweeps everything
                    // pushed so far that targets this workspace.
                    controller
                        .dispatch(GtkCommand::ActivateWorkspace { id: ws_id })
                        .await;
                    assert_eq!(
                        controller.notifications.unread_count(),
                        0,
                        "after ActivateWorkspace at i={i}, every workspace-bound entry up to here must be read",
                    );
                    assert_invariant(&format!("after activate at i={i}"));
                }
                2 => {
                    // Popover open sweep mid-stream.
                    let ids = controller.notifications.mark_all_unread_read();
                    controller
                        .dispatch(GtkCommand::CloseDesktopNotifications { desktop_ids: ids })
                        .await;
                    controller.dispatch(GtkCommand::RefreshLauncherBadge).await;
                    assert_invariant(&format!("after popover sweep at i={i}"));
                }
                3 => {
                    // Trash an existing entry (the per-row delete).
                    if let Some(victim) = pushed.first().copied() {
                        controller
                            .dispatch(GtkCommand::DeleteNotification { id: victim })
                            .await;
                        pushed.retain(|id| *id != victim);
                        assert_invariant(&format!("after delete at i={i}"));
                    }
                }
                _ => {
                    // Bare RefreshLauncherBadge — just exercise the
                    // coalescing path with no state change.
                    controller.dispatch(GtkCommand::RefreshLauncherBadge).await;
                    assert_invariant(&format!("after bare refresh at i={i}"));
                }
            }
        }

        // Final converge: explicit ack of everything that's left, via
        // the workspace sweep, then the popover sweep so global / orphan
        // entries (none here, but the call must be a safe no-op) drain.
        controller
            .dispatch(GtkCommand::ActivateWorkspace { id: ws_id })
            .await;
        let leftover = controller.notifications.mark_all_unread_read();
        controller
            .dispatch(GtkCommand::CloseDesktopNotifications {
                desktop_ids: leftover,
            })
            .await;
        controller.dispatch(GtkCommand::RefreshLauncherBadge).await;
        assert_eq!(
            controller.notifications.unread_count(),
            0,
            "after the final ack chain, every surviving entry must be read so \
             the dock badge reads 0 — this is the user-visible contract",
        );
        assert_invariant("at end");
    }

    /// Burst of `RefreshLauncherBadge` commands queued back-to-back
    /// must not panic, hang, or leave the busy/dirty serialization
    /// flags wedged. The publisher coalesces overlapping refreshes; if
    /// it ever loses track of `badge_dirty`, the dock would freeze on a
    /// stale count under bursty traffic. We can't observe the actual
    /// LauncherEntry signal in tests (no D-Bus) but we can pin that
    /// every dispatch returns cleanly and the in-store count never
    /// drifts from the computed unread set.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn stress_refresh_burst_is_safely_coalesced() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.StressRefreshBurst").await;
        push_notification(&controller, Some(pane), Some(ws_id), "x").await;
        // 100 back-to-back refresh commands. The publisher's internal
        // busy/dirty flag must coalesce these into "publish at most a
        // small fixed number of times" — but we don't peek at the
        // flags here; we only check the dispatcher itself stays sane.
        for _ in 0..100 {
            controller.dispatch(GtkCommand::RefreshLauncherBadge).await;
        }
        assert_eq!(
            controller.notifications.unread_count(),
            1,
            "no refresh command should ever mutate the store; the count must remain 1",
        );
        // Now ack and burst again — the publisher must not get stuck
        // on the previous batch.
        controller
            .dispatch(GtkCommand::ActivateWorkspace { id: ws_id })
            .await;
        for _ in 0..100 {
            controller.dispatch(GtkCommand::RefreshLauncherBadge).await;
        }
        assert_eq!(controller.notifications.unread_count(), 0);
    }

    fn leaf_surface_ids(ws: &Workspace, pane: PaneId) -> Vec<SurfaceId> {
        ws.surfaces
            .iter()
            .find_map(|s| match s.root_pane.find_leaf_content(pane) {
                Some(flowmux_core::PaneContent::Tabs { surfaces, .. }) => {
                    Some(surfaces.iter().map(|x| x.id).collect())
                }
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Moving a tab to another pane re-homes the *same* live terminal widget
    /// (state preserved) and updates the model on both ends.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn move_surface_to_pane_preserves_live_widget() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-move-pane");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let src = store.get_workspace(ws_id).await.unwrap().surfaces[0]
            .root_pane
            .first_leaf_id()
            .unwrap();
        let (_, dst) = store
            .split_pane(src, flowmux_core::SplitDirection::Vertical)
            .await
            .unwrap();
        // Add a second tab to src so moving it away does not collapse src.
        let (_, moved) = store.add_terminal_surface_to_pane(src, None).await.unwrap();
        let ws = store.get_workspace(ws_id).await.unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.MovePane")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);

        let before = controller
            .pane_registry
            .borrow()
            .terminals
            .get(&moved)
            .map(|t| t.root_widget());
        assert!(before.is_some(), "moved surface should be a live terminal");

        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::MoveSurfaceToPane {
                src_pane: src,
                surface: moved,
                surface_model: None,
                dst_pane: dst,
                target_index: usize::MAX,
                ack: ack_tx,
            })
            .await;
        ack_rx.await.unwrap().expect("move should succeed");

        let ws2 = store.get_workspace(ws_id).await.unwrap();
        assert!(!leaf_surface_ids(&ws2, src).contains(&moved));
        assert_eq!(leaf_surface_ids(&ws2, dst).last().copied(), Some(moved));

        // Same GhosttyPane widget instance => terminal state was not rebuilt.
        let after = controller
            .pane_registry
            .borrow()
            .terminals
            .get(&moved)
            .map(|t| t.root_widget());
        assert_eq!(before, after, "moved terminal must be the same live widget");
    }

    /// Moving the only tab out of a pane collapses that pane but keeps the
    /// workspace, and the moved widget survives.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn move_last_tab_collapses_source_pane() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-move-collapse");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let keep = store.get_workspace(ws_id).await.unwrap().surfaces[0]
            .root_pane
            .first_leaf_id()
            .unwrap();
        let (_, src) = store
            .split_pane(keep, flowmux_core::SplitDirection::Vertical)
            .await
            .unwrap();
        let moved = store.get_workspace(ws_id).await.unwrap().surfaces[0]
            .root_pane
            .active_surface_id(src)
            .unwrap();
        let ws = store.get_workspace(ws_id).await.unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.MoveCollapse")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);

        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::MoveSurfaceToPane {
                src_pane: src,
                surface: moved,
                surface_model: None,
                dst_pane: keep,
                target_index: usize::MAX,
                ack: ack_tx,
            })
            .await;
        ack_rx.await.unwrap().expect("move should succeed");

        let ws2 = store.get_workspace(ws_id).await.unwrap();
        // src pane is gone; workspace remains with keep holding both tabs.
        assert!(store.get_workspace(ws_id).await.is_some());
        assert_eq!(ws2.surfaces[0].root_pane.first_leaf_id(), Some(keep));
        assert_eq!(leaf_surface_ids(&ws2, keep).len(), 2);
        assert!(controller
            .pane_registry
            .borrow()
            .terminals
            .contains_key(&moved));
    }

    fn pane_of_surface(ws: &Workspace, surface: SurfaceId) -> Option<PaneId> {
        let mut found = None;
        for s in &ws.surfaces {
            s.root_pane.for_each_leaf(|pane| {
                if let Some(flowmux_core::PaneContent::Tabs { surfaces, .. }) =
                    s.root_pane.find_leaf_content(pane)
                {
                    if surfaces.iter().any(|x| x.id == surface) {
                        found = Some(pane);
                    }
                }
            });
        }
        found
    }

    /// A singleton tab cannot split its own pane without leaving an empty source
    /// leaf. The rejected drop must restore the same live terminal widget.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn splitting_singleton_surface_into_its_own_pane_restores_live_widget() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-split-singleton-self");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let ws = store.get_workspace(ws_id).await.unwrap();
        let pane = ws.surfaces[0].root_pane.first_leaf_id().unwrap();
        let surface = ws.surfaces[0].root_pane.active_surface_id(pane).unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.SplitSingletonSelf")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);

        let before = controller
            .pane_registry
            .borrow()
            .terminals
            .get(&surface)
            .map(|terminal| terminal.root_widget())
            .expect("singleton terminal should be rendered");

        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::SplitSurfaceIntoPane {
                src_pane: pane,
                surface,
                surface_model: None,
                dst_pane: pane,
                direction: flowmux_core::SplitDirection::Horizontal,
                ack: ack_tx,
            })
            .await;
        assert!(ack_rx.await.unwrap().is_err());

        let ws_after = store.get_workspace(ws_id).await.unwrap();
        assert_eq!(pane_of_surface(&ws_after, surface), Some(pane));
        assert!(ws_after.surfaces[0]
            .root_pane
            .parent_split_id(pane)
            .is_none());
        let registry = controller.pane_registry.borrow();
        let terminal = registry
            .terminals
            .get(&surface)
            .expect("rejected split must reattach the live terminal");
        assert_eq!(terminal.id(), pane);
        assert_eq!(terminal.root_widget(), before);
        assert!(registry.has_pane(pane));
    }

    /// Dropping a tab on another pane's split region creates a sibling pane
    /// holding the same live terminal widget.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn split_surface_into_pane_preserves_live_widget() {
        adw::init().expect("libadwaita should initialize in GTK test");
        let root = std::env::temp_dir().join("flowmux-ui-split-move");
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::new_lazy(State::default());
        let ws_id = store
            .create_workspace(Some("ui".into()), root.clone())
            .await;
        let dst = store.get_workspace(ws_id).await.unwrap().surfaces[0]
            .root_pane
            .first_leaf_id()
            .unwrap();
        let (_, src) = store
            .split_pane(dst, flowmux_core::SplitDirection::Vertical)
            .await
            .unwrap();
        let (_, moved) = store.add_terminal_surface_to_pane(src, None).await.unwrap();
        let ws = store.get_workspace(ws_id).await.unwrap();

        let (bridge, _rx) = Bridge::new();
        let app = adw::Application::builder()
            .application_id("com.flowmux.App.UiTest.SplitMove")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let controller = WindowController::new(
            &app,
            store.clone(),
            Arc::new(ResolvedTheme::load()),
            bridge,
            gtk::CssProvider::new(),
            None,
        );
        controller.render_workspace(&ws);

        let before = controller
            .pane_registry
            .borrow()
            .terminals
            .get(&moved)
            .map(|t| t.root_widget());
        assert!(before.is_some());

        let (ack_tx, ack_rx) = oneshot::channel();
        controller
            .dispatch(GtkCommand::SplitSurfaceIntoPane {
                src_pane: src,
                surface: moved,
                surface_model: None,
                dst_pane: dst,
                direction: flowmux_core::SplitDirection::Horizontal,
                ack: ack_tx,
            })
            .await;
        ack_rx.await.unwrap().expect("split-move should succeed");

        let ws2 = store.get_workspace(ws_id).await.unwrap();
        let new_pane = pane_of_surface(&ws2, moved).expect("moved surface has a pane");
        assert_ne!(new_pane, src);
        assert_ne!(new_pane, dst);
        assert_eq!(leaf_surface_ids(&ws2, new_pane), vec![moved]);
        assert_eq!(
            controller
                .pane_registry
                .borrow()
                .terminals
                .get(&moved)
                .expect("moved terminal is still registered")
                .id(),
            new_pane,
            "split-moved terminal must report its destination pane id"
        );

        let after = controller
            .pane_registry
            .borrow()
            .terminals
            .get(&moved)
            .map(|t| t.root_widget());
        assert_eq!(
            before, after,
            "split-moved terminal must be the same widget"
        );
        assert!(controller.pane_registry.borrow().has_pane(new_pane));
    }
}
