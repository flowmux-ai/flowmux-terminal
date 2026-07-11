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
use crate::ui::file_browser::{FileBrowserPaneState, FileBrowserPanel};
use crate::ui::pane_terminal::PaneCallbacks;
use crate::ui::sidebar::{Sidebar, WorkspaceRowAgentBlock, WorkspaceRowDetails};
use crate::ui::workspace_view::{
    attach_surface_to_pane, build_surface, build_surface_tab_widget, solo_workspace_pane,
    split_pane_incremental, IncrementalSplitOutcome, MovingSurface, PaneRegistry, TornOffSurface,
};
use adw::prelude::*;
use flowmux_config::cmux_json::{CmuxJson, CommandTarget, CustomCommand};
use flowmux_core::{
    AgentNotificationVisualFlags, AgentStatus, Pane, PaneContent, PaneId, PaneSurface,
    PlacementStrategy, SplitDirection, Surface, SurfaceId, SurfaceKind, Workspace,
    WorkspaceAgentBlock, WorkspaceId,
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
            SurfaceKind::Browser { .. } => None,
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
pub struct WindowController {
    pub window: adw::ApplicationWindow,
    pub focused_pane: FocusedPane,
    sidebar: Sidebar,
    /// Outermost `gtk::Paned` separating the side panel and content area.
    /// Its position is saved to the store on exit and restored on next launch.
    sidebar_split: gtk::Paned,
    file_browser: FileBrowserState,
    stack: gtk::Stack,
    agent_bar: AgentBarState,
    surfaces: Rc<RefCell<HashMap<WorkspaceId, gtk::Widget>>>,
    pane_registry: Rc<RefCell<PaneRegistry>>,
    callbacks: PaneCallbacks,
    store: StateStore,
    bridge: Bridge,
    theme: Arc<ResolvedTheme>,
    notifications: NotificationStore,
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
    /// `org.gtk.Notifications` client used to withdraw toasts when the
    /// user acknowledges them in flowmux. Defaults to a fresh empty
    /// cell; `main.rs` swaps in the same cell `DaemonHandler` uses to
    /// send `AddNotification` (via [`Self::use_shared_notifier`]) so
    /// both sides run through the same `Connection::session()`.
    /// gnome-shell keys entries by `(sender bus name, app_id)`, so a
    /// withdraw from a second connection silently fails to match — the
    /// dock badge and message-tray entry stay until the user clears
    /// them by hand. Uses a `tokio::sync::Mutex` so the same handle
    /// can be shared with the tokio-side daemon handler.
    notifier: Arc<tokio::sync::Mutex<Option<flowmux_notify::DesktopNotifier>>>,
    /// Set while a launcher-badge publish task is in flight on the main
    /// context. Combined with `badge_dirty` it serializes overlapping
    /// `refresh_launcher_badge` calls so the *last* state of
    /// `NotificationStore` always wins — without it two concurrent
    /// `spawn_local` tasks could publish their counts out of order and
    /// leave the dock badge stuck on a stale value.
    badge_publisher_busy: Rc<Cell<bool>>,
    /// Set when a refresh request arrived while another publish task was
    /// already running. The in-flight task drains it after each publish
    /// and republishes the latest `unread_count()` until the flag stays
    /// false — guaranteeing the dock badge converges to the freshest
    /// state regardless of `spawn_local` scheduling order.
    badge_dirty: Rc<Cell<bool>>,
    /// Handle to the Tokio runtime so D-Bus calls dispatched from
    /// `glib::spawn_local` can enter the runtime before they `await`.
    /// `zbus` (with the `tokio` feature) calls `Handle::current()` to
    /// pick a reactor; without an active runtime context every
    /// `update_launcher_count`, `close`, and `send` panics, the panic
    /// is swallowed by GLib's task wrapper, and the dock badge / FDO
    /// toast never updates. Captured from `tokio::runtime::Handle` at
    /// controller construction so every D-Bus path can `enter()` it.
    tokio_handle: Option<tokio::runtime::Handle>,
}

fn wsl_resize_handles_enabled() -> bool {
    !crate::platform::env_flag_enabled("FLOWMUX_NO_WSL_RESIZE_HANDLES")
        && (crate::platform::running_under_wsl()
            || crate::platform::env_flag_enabled("FLOWMUX_WSL_RESIZE_HANDLES"))
}

fn set_window_content(window: &adw::ApplicationWindow, toolbar: &adw::ToolbarView) {
    if wsl_resize_handles_enabled() {
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(toolbar));
        window.set_content(Some(&overlay));
        install_window_resize_handles(window, &overlay);
    } else {
        window.set_content(Some(toolbar));
    }
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
mod command_palette;
mod file_browser;
mod polling;
mod surface_ops;

impl WindowController {
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
                    .send(GtkCommand::RemoveWorkspace { id, ack: tx })
                    .await;
                let _ = rx.await;
            });
        };
        let sidebar = Sidebar::new(on_select, on_close, bridge.clone(), notifications.clone());
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
        let callbacks = make_callbacks(
            focused_pane.clone(),
            bridge.clone(),
            options.clone(),
            pane_registry.clone(),
            sidebar.workspace_titles(),
            tab_drag_drop_seen,
            tab_drag_drop_committed,
        );

        let content_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content_box.set_hexpand(true);
        content_box.set_vexpand(true);
        content_box.append(&stack);
        content_box.append(&agent_bar.root);

        // gtk::Paned lets the user drag the divider between the
        // sidebar and the content stack — replaces the fixed-width
        // adw::OverlaySplitView so people can hide / widen the tab
        // list to taste.
        sidebar.root.set_size_request(160, -1);
        // Restore a saved sidebar position, otherwise use default 260.
        let stored_sidebar_pos = store.sidebar_position_blocking().unwrap_or(260);
        let split = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&sidebar.root)
            .end_child(&content_box)
            .resize_start_child(false)
            .resize_end_child(true)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .position(stored_sidebar_pos)
            .build();

        let file_browser = FileBrowserPanel::new();
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

        file_browser.widget().set_vexpand(true);

        let file_browser_split = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&split)
            .end_child(file_browser.widget())
            .resize_start_child(true)
            .resize_end_child(false)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .position(980)
            .build();

        let content_overlay = gtk::Overlay::new();
        content_overlay.set_child(Some(&file_browser_split));
        let clipboard_toast = ClipboardToast::new();
        content_overlay.add_overlay(clipboard_toast.widget());

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&content_overlay));

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
        set_window_content(&window, &toolbar);
        if was_maximized {
            window.maximize();
        }

        register_workspace_actions(&window, &store, &bridge);

        let controller = Self {
            window,
            focused_pane,
            sidebar,
            sidebar_split: split,
            file_browser: FileBrowserState {
                source_pane: file_browser_source_pane,
                active: file_browser_active,
                pane_states: file_browser_pane_states,
                split: file_browser_split,
                panel: file_browser,
            },
            stack,
            agent_bar: AgentBarState {
                bar: agent_bar,
                attentions: agent_bar_attentions,
            },
            surfaces,
            pane_registry,
            callbacks,
            store,
            bridge,
            theme,
            notifications,
            options,
            css_provider,
            clipboard_toast,
            focus_mru: Rc::new(RefCell::new(HashMap::new())),
            notifier: Arc::new(tokio::sync::Mutex::new(None)),
            badge_publisher_busy: Rc::new(Cell::new(false)),
            badge_dirty: Rc::new(Cell::new(false)),
            tokio_handle,
        };
        controller.install_state_flush_on_close();
        controller.install_cwd_polling_fallback();
        controller.install_agent_process_polling();
        controller
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
        self.notifier = handle;
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
        self.sidebar.upsert(ws);
        let name = ws.id.to_string();
        self.pane_registry.borrow_mut().clear_workspace(ws.id);
        let new_widget = self.build_workspace_widget(ws);
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

    fn install_state_flush_on_close(&self) {
        let controller = self.clone();
        self.window.connect_close_request(move |_| {
            controller.flush_terminal_cwds_blocking();
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
        // file_browser_active now means the browser actually holds keyboard focus
        // (driven by connect_focus_changed), so it cleanly disambiguates the two
        // jobs of this command:
        //   * browser focused  -> Alt+arrow escapes OUT to a pane.
        //   * a pane focused    -> Alt+arrow moves between panes / INTO the browser.
        // `from` (from the keybinding's FocusedPane tracker) equals the source pane
        // in BOTH cases because focusing the browser does not update that tracker, so
        // gating on the focus flag is what makes the first Alt+arrow do the right
        // thing instead of taking two presses.
        if self.file_browser.active.get()
            && self.file_browser.source_pane.get().is_some()
            && (from.is_none() || from == self.file_browser.source_pane.get())
        {
            self.focus_out_of_file_browser(dir);
            return;
        }

        match from {
            Some(pane) => self.focus_direction_or_file_browser(pane, dir),
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
                self.theme.clone(),
            ),
            None => gtk::Label::new(Some("(empty workspace)")).upcast(),
        }
    }

    pub async fn dispatch(&self, cmd: GtkCommand) {
        match cmd {
            GtkCommand::ShowOptionsDialog => {
                let current = self.options.borrow().clone();
                let options_cell = self.options.clone();
                let registry = self.pane_registry.clone();
                let css_provider = self.css_provider.clone();
                let theme = self.theme.clone();
                let window = self.window.clone();
                let controller = self.clone();
                let default_font_family = theme.font_family();
                let default_font_size = theme.font_size();
                crate::ui::options_dialog::present(
                    &self.window,
                    current,
                    default_font_family,
                    default_font_size,
                    move |opts| {
                        if let Err(e) = flowmux_config::options::save(&opts) {
                            tracing::warn!(error = %e, "options save failed");
                            return;
                        }
                        *options_cell.borrow_mut() = opts.clone();
                        // Build the effective terminal font once (theme font with
                        // the user's family / size overrides layered on) and apply
                        // it live alongside the global zoom scale.
                        let font =
                            theme.font_with_overrides(opts.font_family.as_deref(), opts.font_size);
                        let registry = registry.borrow();
                        for terminal in registry.terminals.values() {
                            terminal.set_font(&font);
                            terminal.set_font_scale(opts.zoom_factor());
                            terminal
                                .set_cursor_blink(opts.cursor_blink, opts.cursor_blink_interval_ms);
                        }
                        for browser in registry.browsers.values() {
                            browser.set_zoom_level(opts.zoom_factor());
                        }
                        // Focus border color/opacity apply by reloading one CSS string
                        // into the same CssProvider instance, so all widgets update automatically.
                        css_provider.load_from_string(&theme.css(
                            opts.focus_border_color_or_default(),
                            opts.focus_border_alpha(),
                        ));
                        // Re-install keybindings so the user does not have to
                        // restart for shortcut edits to take effect.
                        // set_accels_for_action overwrites the same keys so a
                        // second pass on the live ApplicationWindow's app is safe.
                        if let Some(app) = window
                            .application()
                            .and_then(|a| a.downcast::<adw::Application>().ok())
                        {
                            crate::keybindings::install_accels(&app, &opts);
                        } else {
                            tracing::warn!(
                            "options applied without keybinding re-install — window had no Application; restart to pick up shortcut changes"
                        );
                        }
                        tracing::info!(
                            zoom_percent = opts.zoom_percent,
                            engine = ?opts.default_browser_engine,
                            focus_border_color = %opts.focus_border_color,
                            focus_border_opacity = opts.focus_border_opacity,
                            agent_bar_enabled = opts.agent_bar_enabled,
                            keybindings_overrides = opts.keybindings.len(),
                            "options applied"
                        );
                        let controller = controller.clone();
                        glib::MainContext::default().spawn_local(async move {
                            controller.refresh_agent_bar().await;
                        });
                    },
                );
            }
            GtkCommand::ShowCommandPalette => {
                self.show_command_palette();
            }

            GtkCommand::WorkspaceCreated {
                id,
                name: _,
                root: _,
                ack,
            } => {
                // Pull the authoritative workspace (with the store's
                // pane ids) instead of fabricating new ones — otherwise
                // `focused_pane` gets a UUID that doesn't exist in the
                // store and split / close shortcuts no-op.
                if let Some(ws) = self.store.get_workspace(id).await {
                    self.render_workspace(&ws);
                }
                let _ = ack.send(());
            }
            GtkCommand::PaneSplitApplied {
                id,
                pane,
                new_pane,
                direction,
                ack,
            } => {
                self.apply_split_incremental_or_rerender(id, pane, new_pane, direction)
                    .await;
                let _ = ack.send(());
            }
            GtkCommand::SplitFocused {
                pane,
                direction,
                ack,
            } => {
                match self.store.split_pane(pane, direction).await {
                    Some((ws_id, new_pane)) => {
                        self.apply_split_incremental_or_rerender(ws_id, pane, new_pane, direction)
                            .await;
                        // Move keyboard focus to the new pane for both the
                        // incremental path and rerender fallback. Also handle
                        // browser splits from BrowserOpenSplit so web_view receives focus.
                        let registry = self.pane_registry.clone();
                        glib::idle_add_local_once(move || {
                            let r = registry.borrow();
                            if let Some(term) = r.active_terminal(new_pane) {
                                term.grab_focus();
                            } else if let Some(browser) = r.active_browser(new_pane) {
                                browser.grab_focus();
                            }
                        });
                        let _ = ack.send(Ok(new_pane));
                    }
                    None => {
                        let _ = ack.send(Err(format!("pane not found: {pane}")));
                    }
                }
            }
            GtkCommand::CloseFocused { pane, ack } => {
                // Peek before mutating: if this is the only pane in
                // the workspace, closing it would also drop the
                // workspace. Confirm with the user first.
                if let Some((ws_id, count)) = self.store.workspace_pane_count_for(pane).await {
                    if count == 1 && !self.confirm_close_workspace(ws_id).await {
                        let _ = ack.send(Ok(()));
                        return;
                    }
                }
                match self.store.close_pane(pane).await {
                    None => {
                        let _ = ack.send(Err(format!("pane not found: {pane}")));
                    }
                    Some(flowmux_daemon::CloseOutcome::PaneRemoved { workspace }) => {
                        // Incremental collapse: keep every other pane's
                        // widget instance (and therefore every running
                        // PTY shell + browser nav state) intact. This
                        // path replaces the prior `rerender_workspace`
                        // that destroyed claude/codex sessions on close.
                        self.apply_close_pane_incremental_or_rerender(workspace, pane)
                            .await;
                        self.focus_after_close(workspace, pane).await;
                        self.sync_workspace_agent_status_from_store(workspace).await;
                        let _ = ack.send(Ok(()));
                    }
                    Some(flowmux_daemon::CloseOutcome::SurfaceRemoved { workspace }) => {
                        // close_pane removed the entire surface (workspace-
                        // level tab) but the workspace still has at least
                        // one other surface. Drop the registry pane entry
                        // and rerender — surface switching is rare and
                        // not in the user's reset complaint scope.
                        if let Some(ws) = self.store.get_workspace(workspace).await {
                            self.rerender_workspace(&ws);
                            self.sync_workspace_agent_status_from_store(workspace).await;
                        }
                        let _ = ack.send(Ok(()));
                    }
                    Some(flowmux_daemon::CloseOutcome::WorkspaceRemoved { workspace }) => {
                        self.drop_workspace(workspace);
                        self.activate_active_or_show_empty().await;
                        let _ = ack.send(Ok(()));
                    }
                }
            }
            GtkCommand::FocusDirection { from, dir } => {
                self.focus_direction_from_command(from, dir).await;
            }
            GtkCommand::FileBrowserFocusOut { dir } => {
                self.focus_out_of_file_browser(dir);
            }
            GtkCommand::FileBrowserCloseAndRestoreFocus => {
                self.close_file_browser_and_restore_focus();
            }
            GtkCommand::NewSurface { pane } => {
                let cwd = {
                    let r = self.pane_registry.borrow();
                    r.active_terminal(pane).and_then(|term| term.current_dir())
                }
                .or_else(|| std::env::current_dir().ok());
                if let Some((ws_id, surface_id)) =
                    self.store.add_terminal_surface_to_pane(pane, cwd).await
                {
                    self.attach_or_rerender_surface(ws_id, pane, surface_id)
                        .await;
                }
            }
            GtkCommand::NewBrowserSurface { pane } => {
                if let Some((ws_id, surface_id)) = self
                    .store
                    .add_browser_surface_to_pane(pane, "about:blank".into())
                    .await
                {
                    self.attach_or_rerender_surface(ws_id, pane, surface_id)
                        .await;
                }
            }
            GtkCommand::ToggleFileBrowser { pane } => {
                // `None` comes from the side-panel footer button / Ctrl+Alt+F,
                // which have no pane context — target the focused pane.
                // Already visible → close; otherwise open for that pane.
                if self.file_browser.panel.widget().is_visible() {
                    self.close_file_browser_and_restore_focus();
                } else if let Some(pane) = pane.or_else(|| self.focused_pane.get()) {
                    self.show_file_browser_for_pane(pane).await;
                }
            }
            GtkCommand::OpenUrlInBrowserTab { pane, url } => {
                // Open a Ctrl-clicked terminal URL in a new browser tab in the
                // same pane. BrowserPane::build receives the URL as initial_url
                // and immediately load_uri's it, so no extra navigate command is
                // needed. If surface creation fails, for example because the pane
                // disappeared right after the click, ignore it quietly.
                if let Some((ws_id, surface_id)) =
                    self.store.add_browser_surface_to_pane(pane, url).await
                {
                    self.attach_or_rerender_surface(ws_id, pane, surface_id)
                        .await;
                }
            }
            GtkCommand::OpenImageViewer { pane, path } => {
                tracing::info!(%pane, path = %path.display(), "opening terminal image path");
                crate::ui::image_viewer::open_image_viewer(&self.window, path);
            }
            GtkCommand::OpenMarkdownViewer { pane, path } => {
                tracing::info!(%pane, path = %path.display(), "opening terminal markdown path");
                if let Err(err) = crate::ui::file_browser::launch_markdown_viewer(&path) {
                    tracing::warn!(error = %err, path = %path.display(), "failed to open Markdown viewer from terminal path");
                }
            }
            GtkCommand::ActivateSurface { pane, surface } => {
                let ws_id = self.store.set_active_surface(pane, surface).await;
                self.pane_registry
                    .borrow_mut()
                    .activate_surface(pane, surface);
                self.refresh_window_title().await;
                if let Some(ws_id) = ws_id {
                    // Tab activation changes the active surface used for the
                    // side-panel name and subtitles.
                    self.sync_workspace_agent_status_from_store(ws_id).await;
                }
                self.refresh_agent_screen_status(surface, None).await;
                // After a surface is activated through click, IPC, or another
                // path, move keyboard focus to the
                // newly active widget: the terminal's gtk::DrawingArea or the
                // browser's WebView. That lets typing go to the new tab's shell
                // or page and keeps Tab as shell completion instead of tab-bar
                // traversal. Defer one frame because the widget was just added
                // to the stack.
                let registry = self.pane_registry.clone();
                glib::idle_add_local_once(move || {
                    let r = registry.borrow();
                    if let Some(term) = r.terminals.get(&surface) {
                        term.grab_focus();
                    } else if let Some(browser) = r.browsers.get(&surface) {
                        browser.grab_focus();
                    }
                });
            }
            GtkCommand::CloseSurface { pane, surface, ack } => {
                // A successful in-process MOVE can rehome the surface before
                // DragSource::drag-end emits its fallback close for the old
                // pane. Reject that stale close before considering whether the
                // old pane is the workspace's last one; otherwise we present a
                // spurious workspace-close dialog and block the GTK dispatcher.
                if self.store.surface_title(pane, surface).await.is_none() {
                    let _ = ack.send(Err(format!("surface not found: {surface}")));
                    return;
                }
                // Closing the only tab in a leaf falls through to
                // close_pane(pane) inside the store; if that pane is
                // also the workspace's only pane, the workspace dies.
                // Confirm in that exact case so an accidental Ctrl+W
                // on the last tab does not nuke the workspace.
                let tabs = self.store.tab_count_in_pane(pane).await;
                let panes = self.store.workspace_pane_count_for(pane).await;
                if tabs == Some(1) {
                    if let Some((ws_id, count)) = panes {
                        if count == 1 && !self.confirm_close_workspace(ws_id).await {
                            let _ = ack.send(Ok(()));
                            return;
                        }
                    }
                }
                match self.store.close_surface(pane, surface).await {
                    None => {
                        let _ = ack.send(Err(format!("surface not found: {surface}")));
                    }
                    Some(flowmux_daemon::CloseOutcome::WorkspaceRemoved { workspace }) => {
                        self.drop_workspace(workspace);
                        self.activate_active_or_show_empty().await;
                        let _ = ack.send(Ok(()));
                    }
                    Some(flowmux_daemon::CloseOutcome::SurfaceRemoved { workspace }) => {
                        // Only one surface in the same pane disappeared. Full
                        // workspace rerender would lose other panes' shell and
                        // browser state, so detach incrementally. If the store
                        // moved active to a new surface, activate it to sync the
                        // stack and tab highlight.
                        self.pane_registry
                            .borrow_mut()
                            .detach_surface_widget(pane, surface);
                        if let Some(ws) = self.store.get_workspace(workspace).await {
                            if let Some(active) = ws
                                .surfaces
                                .iter()
                                .find_map(|s| s.root_pane.active_surface_id(pane))
                            {
                                self.pane_registry
                                    .borrow_mut()
                                    .activate_surface(pane, active);
                            }
                            self.refresh_workspace_solo(&ws);
                        }
                        self.refresh_window_title().await;
                        self.sync_workspace_agent_status_from_store(workspace).await;
                        let _ = ack.send(Ok(()));
                    }
                    Some(flowmux_daemon::CloseOutcome::PaneRemoved { workspace }) => {
                        // Incremental collapse — see
                        // `apply_close_pane_incremental_or_rerender`
                        // for the details. Keeps every other pane's
                        // widget alive across the close.
                        self.apply_close_pane_incremental_or_rerender(workspace, pane)
                            .await;
                        // Alt+W on a single-tab pane lands here. Without this
                        // call focus stays on the dead pane id, so arrow keys
                        // / typing go nowhere until the user clicks a sibling.
                        // CloseFocused already calls focus_after_close in the
                        // same situation; keep the two close paths in sync.
                        self.focus_after_close(workspace, pane).await;
                        self.sync_workspace_agent_status_from_store(workspace).await;
                        let _ = ack.send(Ok(()));
                    }
                }
            }
            GtkCommand::RenameSurface {
                pane,
                surface,
                title,
                ack,
            } => match self
                .store
                .rename_surface(pane, surface, title.clone())
                .await
            {
                None => {
                    let _ = ack.send(Err(format!("surface not found: {surface}")));
                }
                Some(_) => {
                    self.pane_registry
                        .borrow()
                        .set_surface_title(surface, &title);
                    self.refresh_window_title().await;
                    let _ = ack.send(Ok(()));
                }
            },
            GtkCommand::ShowRenameSurfaceDialog { pane, surface } => {
                if let Some(title) = self.store.surface_title(pane, surface).await {
                    show_rename_surface_dialog(
                        &self.window,
                        pane,
                        surface,
                        &title,
                        self.bridge.clone(),
                    );
                }
            }
            GtkCommand::ReorderSurface {
                pane,
                surface,
                target_index,
                ack,
            } => {
                tracing::info!(%pane, %surface, target_index, "ReorderSurface dispatch start");
                // If store-side reorder returns no change (None), leave GTK
                // widgets unchanged. Widget reorder must update both the tab-bar
                // gtk::Box and surface_tabs indexes held by main-thread PaneRegistry.
                let store_result = self
                    .store
                    .reorder_surface_in_pane(pane, surface, target_index)
                    .await;
                if store_result.is_some() {
                    self.pane_registry.borrow_mut().reorder_surface_widget(
                        pane,
                        surface,
                        target_index,
                    );
                    tracing::info!(%pane, %surface, target_index, "ReorderSurface applied");
                } else {
                    tracing::warn!(
                        %pane,
                        %surface,
                        target_index,
                        "ReorderSurface store update returned None (no-op or unknown surface)"
                    );
                }
                let _ = ack.send(Ok(()));
            }
            GtkCommand::TearOffSurface { pane, surface } => {
                self.tear_off_surface(pane, surface).await;
            }
            GtkCommand::MoveSurfaceToPane {
                src_pane,
                surface,
                surface_model,
                dst_pane,
                target_index,
                ack,
            } => {
                let res = self
                    .move_surface(src_pane, surface, surface_model, dst_pane, target_index)
                    .await;
                let _ = ack.send(res);
            }
            GtkCommand::MoveSurfaceToWorkspace {
                src_pane,
                surface,
                dst_workspace,
                ack,
            } => {
                let res = self
                    .move_surface_to_workspace(src_pane, surface, dst_workspace)
                    .await;
                let _ = ack.send(res);
            }
            GtkCommand::SplitSurfaceIntoPane {
                src_pane,
                surface,
                surface_model,
                dst_pane,
                direction,
                ack,
            } => {
                let res = self
                    .split_surface_into_pane(src_pane, surface, surface_model, dst_pane, direction)
                    .await;
                let _ = ack.send(res);
            }
            GtkCommand::TerminalCwdChanged { pane, surface, cwd } => {
                let ws_id = self.update_terminal_cwd(pane, surface, cwd).await;
                self.refresh_window_title().await;
                if let Some(ws_id) = ws_id {
                    self.sync_workspace_label(ws_id).await;
                }
                self.refresh_file_browser_from_focus().await;
            }
            GtkCommand::BrowserUriChanged { pane, surface, url } => {
                let _ = self.store.update_browser_url(pane, surface, url).await;
            }
            GtkCommand::BrowserTitleChanged {
                pane,
                surface,
                title,
            } => {
                if let Some(ws_id) = self
                    .store
                    .update_surface_auto_title(pane, surface, title)
                    .await
                {
                    if let Some(latest) = self.store.surface_title(pane, surface).await {
                        self.pane_registry
                            .borrow()
                            .set_surface_title(surface, &latest);
                    }
                    self.refresh_window_title().await;
                    self.sync_workspace_label(ws_id).await;
                }
            }
            GtkCommand::TerminalTitleChanged {
                pane,
                surface,
                title,
            } => {
                // VTE parsed an OSC 0/2 window title. Prompt-shaped shell
                // titles such as "user@host:~/path" duplicate cwd-driven labels,
                // and trim-empty or whitespace-only values are ignored. Everything
                // else follows BrowserTitleChanged semantics, respecting title_locked.
                if title.trim().is_empty() {
                    return;
                }
                let signal_title = title.clone();
                if let Some(ws_id) = self
                    .store
                    .update_surface_auto_title(pane, surface, title)
                    .await
                {
                    if let Some(latest) = self.store.surface_title(pane, surface).await {
                        self.pane_registry
                            .borrow()
                            .set_surface_title(surface, &latest);
                    }
                    self.refresh_window_title().await;
                    self.sync_workspace_label(ws_id).await;
                }
                self.refresh_agent_screen_status(surface, Some(signal_title))
                    .await;
            }
            GtkCommand::RefreshWindowTitle => {
                self.refresh_window_title().await;
            }
            GtkCommand::PaneFocused { pane } => {
                self.on_pane_focused(pane).await;
                self.refresh_file_browser_from_focus().await;
            }
            GtkCommand::NewWorkspace { root } => {
                // Prefer the focused pane's cwd so a new tab opens
                // where the user was working, falling back to the
                // root the caller suggested (typically the daemon's
                // own current_dir) and finally to "/".
                let resolved = self
                    .focused_pane
                    .get()
                    .and_then(|id| {
                        let r = self.pane_registry.borrow();
                        r.active_terminal(id).cloned()
                    })
                    .and_then(|p| p.current_dir())
                    .unwrap_or(root);
                let id = self.store.create_workspace(None, resolved).await;
                if let Some(ws) = self.store.get_workspace(id).await {
                    self.render_workspace(&ws);
                }
            }
            GtkCommand::RemoveWorkspace { id, ack } => {
                flowmux_config::notify_debug!("gui/dispatch", "RemoveWorkspace id={id}");
                if !self.confirm_close_workspace(id).await {
                    let _ = ack.send(());
                    return;
                }
                if self.store.remove_workspace(id).await {
                    self.drop_workspace(id);
                    self.activate_active_or_show_empty().await;
                }
                let _ = ack.send(());
            }
            GtkCommand::RemoveAllWorkspaces { ack } => {
                flowmux_config::notify_debug!("gui/dispatch", "RemoveAllWorkspaces");
                if !self.confirm_close_all_workspaces().await {
                    let _ = ack.send(());
                    return;
                }
                for id in self.store.remove_all_workspaces().await {
                    self.drop_workspace(id);
                }
                self.activate_active_or_show_empty().await;
                let _ = ack.send(());
            }
            GtkCommand::RenameWorkspace { id, name, ack } => {
                self.store.rename_workspace(id, name).await;
                if let Some(ws) = self.store.get_workspace(id).await {
                    self.sidebar.upsert(&ws);
                }
                let _ = ack.send(());
            }
            GtkCommand::SetWorkspaceColor { id, color, ack } => {
                self.store.set_workspace_color(id, color).await;
                if let Some(ws) = self.store.get_workspace(id).await {
                    self.sidebar.upsert(&ws);
                }
                self.refresh_agent_bar().await;
                let _ = ack.send(());
            }
            GtkCommand::ReorderWorkspace { id, target_index } => {
                tracing::info!(workspace = %id, target_index, "ReorderWorkspace dispatch start");
                let store_result = self.store.reorder_workspace(id, target_index).await;
                if store_result {
                    self.sidebar.reorder(id, target_index);
                    tracing::info!(workspace = %id, target_index, "ReorderWorkspace applied");
                } else {
                    tracing::warn!(
                        workspace = %id,
                        target_index,
                        "ReorderWorkspace store update returned false (no-op or unknown id)"
                    );
                }
            }
            GtkCommand::ShowRenameDialog { id } => {
                flowmux_config::notify_debug!("gui/dispatch", "ShowRenameDialog id={id}");
                if let Some(ws) = self.store.get_workspace(id).await {
                    // Match cmux prefill behavior: start from custom_title when
                    // present so the user can edit it, otherwise show the current
                    // automatic name (`name`).
                    let prefill = ws.custom_title.as_deref().unwrap_or(&ws.name).to_string();
                    show_rename_dialog(&self.window, id, &prefill, self.bridge.clone());
                }
            }
            GtkCommand::ShowColorDialog { id } => {
                flowmux_config::notify_debug!("gui/dispatch", "ShowColorDialog id={id}");
                let current = self.store.get_workspace(id).await.and_then(|w| w.color);
                show_color_dialog(&self.window, id, current.as_deref(), self.bridge.clone());
            }
            GtkCommand::FocusWorkspaceDir { dir } => {
                let snap = self.store.snapshot().await;
                if snap.workspace_order.is_empty() {
                    return;
                }
                let active = self.sidebar.selected_workspace().or(snap
                    .active_workspace
                    .or_else(|| snap.workspace_order.first().copied()));
                let Some(active) = active else { return };
                let cur = snap
                    .workspace_order
                    .iter()
                    .position(|x| *x == active)
                    .unwrap_or(0);
                let len = snap.workspace_order.len();
                let next = match dir {
                    WsNav::Next => (cur + 1) % len,
                    WsNav::Prev => (cur + len - 1) % len,
                };
                let target = snap.workspace_order[next];
                self.activate_workspace(target).await;
            }
            GtkCommand::AddNotification {
                pane,
                surface,
                workspace,
                title,
                body,
                level,
                ack,
            } => {
                // Suppress when the user is already on the source
                // pane+surface AND the app window is focused. Mirrors
                // cmux's `shouldSuppressExternalDelivery` policy: don't
                // toast or grow the bell list for an event the user is
                // literally watching.
                //
                // Exception: NeedsInput (agent paused, waiting for
                // the user) and Error notifications always pierce the
                // suppression. "Same pane focused" is not the same as
                // "user is reading right now" — they may have scrolled
                // past the prompt, be on a different monitor, or have
                // their eyes off the screen entirely. Silencing the
                // bell for the only event class that exists to say
                // "stop typing and look here" defeats its purpose.
                let window_active = self.window.is_active();
                let focused = self.focused_pane.get();
                let pierces_focus = matches!(
                    level,
                    flowmux_core::NotificationLevel::NeedsInput
                        | flowmux_core::NotificationLevel::Error
                );
                let suppress =
                    should_suppress_notification(level, self.is_source_focused(pane, surface));
                tracing::info!(
                    ?pane,
                    ?surface,
                    ?workspace,
                    ?level,
                    ?focused,
                    window_active,
                    pierces_focus,
                    suppress,
                    "AddNotification: suppress decision"
                );
                flowmux_config::notify_debug!(
                    "gui/add",
                    "AddNotification pane={pane:?} surface={surface:?} workspace={workspace:?} level={level:?} focused={focused:?} window_active={window_active} pierces_focus={pierces_focus} suppress={suppress}"
                );
                if suppress {
                    flowmux_config::notify_debug!(
                        "gui/add",
                        "SUPPRESSED — sending ack=None (skips both in-app push and desktop toast)"
                    );
                    let _ = ack.send(None);
                    return;
                }
                let Some(entry_id) = self
                    .notifications
                    .push(title, body, level, pane, surface, workspace)
                else {
                    // Near-duplicate of an entry pushed within
                    // `DUP_WINDOW`: the OSC path and the lifecycle
                    // hook both fired for the same Stop event. Ack
                    // with None so the IPC handler also skips the
                    // desktop toast — one row, one toast per event.
                    tracing::info!(
                        ?pane,
                        ?surface,
                        ?level,
                        "AddNotification: deduplicated against recent entry — skipping both in-app and desktop"
                    );
                    flowmux_config::notify_debug!(
                        "gui/add",
                        "DEDUP HIT — pane={pane:?} surface={surface:?} same source within DUP_WINDOW, ack=None"
                    );
                    let _ = ack.send(None);
                    return;
                };
                self.sidebar.bump_notification_badge();
                let mut marked_attention = false;
                if matches!(level, flowmux_core::NotificationLevel::NeedsInput) {
                    let flags = AgentNotificationVisualFlags::for_unread(
                        self.options.borrow().agent_notification_target,
                        false,
                    );
                    if flags.agent_bar {
                        if let Some(surface_id) = surface {
                            self.mark_agent_bar_attention(surface_id);
                            marked_attention = true;
                        }
                    }
                    if flags.workspace {
                        if let Some(ws_id) = workspace {
                            self.sidebar.mark_attention(ws_id);
                            marked_attention = true;
                        }
                    }
                }
                tracing::info!(
                    ?entry_id,
                    marked_attention,
                    workspace_known = workspace.is_some(),
                    "AddNotification: in-app entry stored, badges updated"
                );
                flowmux_config::notify_debug!(
                    "gui/add",
                    "PUSHED entry_id={entry_id:?} marked_attention={marked_attention} workspace_known={} — ack=Some, daemon will now fire desktop toast",
                    workspace.is_some()
                );
                self.refresh_launcher_badge();
                // System-notification toggle: when disabled, the in-app bell
                // entry above stays (and badges update), but ack=None tells the
                // IPC handler to skip the desktop toast so nothing reaches the
                // system notification service.
                let system_notifications_enabled =
                    self.options.borrow().system_notifications_enabled;
                if !system_notifications_enabled {
                    flowmux_config::notify_debug!(
                        "gui/add",
                        "system notifications disabled — kept in-app entry={entry_id:?}, ack=None (no desktop toast)"
                    );
                    let _ = ack.send(None);
                    return;
                }
                let _ = ack.send(Some(entry_id));
            }
            GtkCommand::SetNotificationDesktopId { id, desktop_id } => {
                // The daemon's `Notify` reply may race the user's read
                // gesture: by the time the desktop_id arrives, the user
                // may already have opened the bell popover or activated
                // the source workspace, in which case the previous
                // sweep had nothing to close. Detect that here and fire
                // a one-off close so the FDO toast does not linger and
                // the dock badge stays in sync.
                match self.notifications.set_desktop_id(id, desktop_id.clone()) {
                    SetDesktopIdResult::Stale => {
                        self.close_desktop_notifications(vec![desktop_id]);
                        self.refresh_launcher_badge();
                    }
                    SetDesktopIdResult::Stored | SetDesktopIdResult::Unknown => {}
                }
            }
            GtkCommand::CloseDesktopNotifications { desktop_ids } => {
                self.close_desktop_notifications(desktop_ids);
                self.refresh_launcher_badge();
            }
            GtkCommand::RefreshLauncherBadge => {
                self.refresh_launcher_badge();
            }
            GtkCommand::OpenNotification { id } => {
                let Some(entry) = self.notifications.find(id) else {
                    tracing::debug!(%id, "open notification: id not found");
                    return;
                };
                let did = entry.desktop_id.clone();
                if self.notifications.mark_read(id) {
                    if let Some(did) = did {
                        self.close_desktop_notifications(vec![did]);
                    }
                    self.clear_notification_attention_for_entry(&entry);
                    self.refresh_launcher_badge();
                }
                if let Some(ws_id) = entry.workspace {
                    self.activate_workspace(ws_id).await;
                }
                if let Some(pane) = entry.pane {
                    // Switch to the source tab first when the entry
                    // points at a non-active surface inside its pane,
                    // then grab focus. The activate-surface dispatch
                    // is awaited so the focus_pane idle below sees the
                    // newly-active terminal/browser widget.
                    if let Some(source_surface) = entry.surface {
                        let active = self.pane_registry.borrow().active_surface(pane);
                        if active != Some(source_surface) {
                            self.activate_surface_now(pane, source_surface).await;
                        }
                    }
                    self.focus_pane(pane);
                }
                // Mirrors cmux's `bringToFront(window)` so the click on
                // a desktop toast / popover row brings flowmux up even
                // if it was minimized or behind another window.
                self.window.present();
            }
            GtkCommand::ListNotifications { unread_only, ack } => {
                let entries = self
                    .notifications
                    .entries()
                    .into_iter()
                    .filter(|entry| !unread_only || !entry.read)
                    .map(notification_summary)
                    .collect();
                let _ = ack.send((entries, self.notifications.unread_count()));
            }

            GtkCommand::OpenNotificationWithAck { id, ack } => {
                let changed = self.open_notification_id(id).await;
                let _ = ack.send(changed);
            }

            GtkCommand::OpenOldestUnreadNotification { ack } => {
                let id = self
                    .notifications
                    .entries()
                    .into_iter()
                    .find(|entry| !entry.read)
                    .map(|entry| entry.id);
                let changed = match id {
                    Some(id) => self.open_notification_id(id).await,
                    None => false,
                };
                let _ = ack.send(changed);
            }

            GtkCommand::MarkNotificationRead { id, ack } => {
                let entry = self.notifications.find(id);
                let desktop_id = entry.as_ref().and_then(|entry| entry.desktop_id.clone());
                let changed = self.notifications.mark_read(id);
                if changed {
                    if let Some(desktop_id) = desktop_id {
                        self.close_desktop_notifications(vec![desktop_id]);
                    }
                    if let Some(entry) = &entry {
                        self.clear_notification_attention_for_entry(entry);
                    }
                    self.refresh_launcher_badge();
                }
                let _ = ack.send(changed);
            }

            GtkCommand::ClearNotifications { ack } => {
                let entries = self.notifications.entries();
                let had_entries = !entries.is_empty();
                let desktop_ids = self.notifications.clear_all();
                self.clear_all_notification_attention_for_entries(&entries);
                if !desktop_ids.is_empty() {
                    self.close_desktop_notifications(desktop_ids);
                }
                self.refresh_launcher_badge();
                let _ = ack.send(had_entries);
            }

            GtkCommand::DeleteNotification { id } => {
                // Trash button on the bell-popover row. Drop the entry,
                // close any live FDO toast (so the system notification
                // center shrinks in lockstep), re-publish the dock
                // badge if the unread count changed, and re-render the
                // popover so the deleted row vanishes immediately.
                let entry = self.notifications.find(id);
                match self.notifications.remove(id) {
                    RemoveOutcome::Unknown => {
                        tracing::debug!(%id, "delete notification: id not found");
                    }
                    RemoveOutcome::RemovedRead => {
                        // Read-only delete: unread count unchanged, no
                        // FDO toast was outstanding for this entry.
                        if let Some(entry) = &entry {
                            self.clear_notification_attention_for_entry(entry);
                        }
                        self.sidebar.refresh_notification_popover();
                    }
                    RemoveOutcome::RemovedUnread { desktop_id } => {
                        if let Some(did) = desktop_id {
                            self.close_desktop_notifications(vec![did]);
                        }
                        if let Some(entry) = &entry {
                            self.clear_notification_attention_for_entry(entry);
                        }
                        self.refresh_launcher_badge();
                        self.sidebar.refresh_notification_popover();
                    }
                }
            }
            GtkCommand::ClearAllNotifications => {
                let entries = self.notifications.entries();
                let desktop_ids = self.notifications.clear_all();
                self.clear_all_notification_attention_for_entries(&entries);
                if !desktop_ids.is_empty() {
                    self.close_desktop_notifications(desktop_ids);
                }
                self.refresh_launcher_badge();
                self.sidebar.refresh_notification_popover();
            }
            GtkCommand::SetAgentStatus { workspace, status } => {
                self.sync_workspace_agent_status(workspace, status).await;
            }
            GtkCommand::QueryAgentSurfaceVisible { surface, ack } => {
                let _ = ack.send(self.is_agent_surface_visible(surface));
            }
            GtkCommand::FocusWorkspaceAt { idx } => {
                let snap = self.store.snapshot().await;
                let target_idx = (idx as usize).saturating_sub(1);
                if let Some(id) = snap.workspace_order.get(target_idx).copied() {
                    self.activate_workspace(id).await;
                }
            }
            GtkCommand::ActivateWorkspace { id } => {
                self.activate_workspace(id).await;
            }
            GtkCommand::OpenAgentBarItem {
                workspace,
                pane,
                surface,
            } => {
                self.open_agent_bar_item(workspace, pane, surface).await;
            }
            GtkCommand::PaneSendKeys { pane, keys, ack } => {
                let registry = self.pane_registry.borrow();
                let res = match registry.active_terminal(pane) {
                    Some(p) => p.write_input(keys.as_bytes()).map_err(|e| e.to_string()),
                    None => Err(format!("pane not found: {pane}")),
                };
                let _ = ack.send(res);
            }
            GtkCommand::PaneReadScreen { pane, ack } => {
                let registry = self.pane_registry.borrow();
                let res = match registry.active_terminal(pane) {
                    Some(p) => Ok(p.screen_text()),
                    None => Err(format!("pane not found: {pane}")),
                };
                let _ = ack.send(res);
            }
            GtkCommand::FocusPane { pane, ack } => {
                // Existence check up front so a bad id is reported rather
                // than silently no-op'd by focus_pane. Reuses the same
                // grab-focus primitive the notification-click path uses.
                let known = self.pane_registry.borrow().pane_frame(pane).is_some();
                if known {
                    self.focus_pane(pane);
                    self.on_pane_focused(pane).await;
                    let _ = ack.send(Ok(()));
                    self.refresh_file_browser_from_focus().await;
                } else {
                    let _ = ack.send(Err(format!("pane not found: {pane}")));
                }
            }
            GtkCommand::ResizePane { pane, ratio, ack } => {
                let res = self.resize_pane_ratio(pane, ratio).await;
                let _ = ack.send(res);
            }
            GtkCommand::NotificationOnPane { pane, title, body } => {
                tracing::info!(%pane, %title, %body, "pane notification");
                // TODO: paint blue ring + sidebar badge.
            }
            GtkCommand::InjectCookies { cookies, ack } => {
                let result = inject_cookies_into_webkit(&cookies);
                let _ = ack.send(result);
            }
            GtkCommand::ShowSurfaceFolder { pane, surface } => {
                let cwd = self
                    .pane_registry
                    .borrow()
                    .terminals
                    .get(&surface)
                    .and_then(|t| t.current_dir());
                let workspace_for_pane = self.pane_registry.borrow().workspace_of_pane(pane);
                let stored = match workspace_for_pane {
                    Some(workspace) => self
                        .store
                        .get_workspace(workspace)
                        .await
                        .and_then(|ws| stored_terminal_cwd_from_workspace(&ws, pane, surface)),
                    None => None,
                };
                match cwd.or(stored) {
                    Some(p) => crate::ui::show_in_folder::open_directory(&p),
                    None => {
                        tracing::info!(%pane, %surface, "show-in-folder: surface has no resolvable cwd");
                    }
                }
            }
            GtkCommand::CopySurfaceText { pane, surface } => {
                let text = self.copyable_surface_text(pane, surface).await;
                match text {
                    Some(text) => {
                        self.window.clipboard().set_text(&text.value);
                        self.clipboard_toast
                            .show_with_message(&format!("Copied {}: {}", text.kind, text.value));
                    }
                    None => {
                        tracing::info!(%pane, %surface, "copy-surface-text: no path/url");
                        self.clipboard_toast.show_with_message("Nothing to copy");
                    }
                }
            }
            GtkCommand::CopyFocusedPaneText { workspace } => {
                flowmux_config::notify_debug!("gui/dispatch", "CopyFocusedPaneText ws={workspace}");
                let ws = self.store.get_workspace(workspace).await;
                let Some(ws) = ws else {
                    tracing::info!(%workspace, "copy-focused-pane-text: workspace not found");
                    return;
                };
                let focused = self.focused_pane.get().filter(|p| {
                    self.pane_registry.borrow().workspace_of_pane(*p) == Some(workspace)
                });
                let candidate_panes: Vec<PaneId> = focused
                    .into_iter()
                    .chain(
                        ws.surfaces
                            .first()
                            .and_then(|s| s.root_pane.first_leaf_id()),
                    )
                    .collect();
                let mut resolved = None;
                for pane in candidate_panes {
                    let surface = self.pane_registry.borrow().active_surface(pane);
                    let Some(surface) = surface else {
                        continue;
                    };
                    if let Some(text) = self.copyable_surface_text(pane, surface).await {
                        resolved = Some(text);
                        break;
                    }
                }
                let text = resolved.unwrap_or_else(|| CopyableText::stored_path(ws.root_dir));
                self.window.clipboard().set_text(&text.value);
                self.clipboard_toast
                    .show_with_message(&format!("Copied {}: {}", text.kind, text.value));
            }
            GtkCommand::ShowFocusedPaneFolder { workspace } => {
                flowmux_config::notify_debug!(
                    "gui/dispatch",
                    "ShowFocusedPaneFolder ws={workspace}"
                );
                // Resolution order:
                //   1. Globally focused pane, if it belongs to this workspace —
                //      its active terminal's cwd.
                //   2. Workspace's first leaf pane — its active terminal's cwd.
                //   3. Workspace's stored `root_dir` — guarantees we open
                //      *something* for a workspace whose panes are all browsers
                //      or whose terminals haven't reported a cwd yet.
                let ws = self.store.get_workspace(workspace).await;
                let Some(ws) = ws else {
                    tracing::info!(%workspace, "show-in-folder: workspace not found");
                    return;
                };
                let focused = self.focused_pane.get().filter(|p| {
                    self.pane_registry.borrow().workspace_of_pane(*p) == Some(workspace)
                });
                let candidate_panes: Vec<PaneId> = focused
                    .into_iter()
                    .chain(
                        ws.surfaces
                            .first()
                            .and_then(|s| s.root_pane.first_leaf_id()),
                    )
                    .collect();
                let path = {
                    let r = self.pane_registry.borrow();
                    candidate_panes.iter().find_map(|pane| {
                        let surface = r
                            .active_surface(*pane)
                            .or_else(|| active_surface_from_workspace(&ws, *pane))?;
                        r.active_terminal(*pane)
                            .and_then(|t| t.current_dir())
                            .or_else(|| stored_terminal_cwd_from_workspace(&ws, *pane, surface))
                    })
                }
                .unwrap_or_else(|| ws.root_dir.clone());
                crate::ui::show_in_folder::open_directory(&path);
            }
            GtkCommand::BrowserEval { pane, source, ack } => {
                let registry = self.pane_registry.borrow();
                match registry.active_browser(pane) {
                    None => {
                        let _ = ack.send(Err(format!("browser pane not found: {pane}")));
                    }
                    Some(browser) => {
                        // evaluate_js is callback-style; bridge it to the ack.
                        let cell = std::cell::Cell::new(Some(ack));
                        browser.evaluate_js(&source, move |result| {
                            if let Some(ack) = cell.take() {
                                let _ = ack.send(result);
                            }
                        });
                    }
                }
            }
            GtkCommand::BrowserAction { pane, op, ack } => {
                let browser = self.pane_registry.borrow().active_browser(pane).cloned();
                let Some(browser) = browser else {
                    let _ = ack.send(Err(format!("browser pane not found: {pane}")));
                    return;
                };
                match op {
                    BrowserOp::Snapshot => {
                        // Run the non-mutating snapshot script, then mirror
                        // its `refs` map into the pane's server-side
                        // `RefStore` so subsequent `click e3` / `text e3`
                        // calls resolve. The page DOM is never stamped.
                        let browser2 = browser.clone();
                        let cell = std::cell::Cell::new(Some(ack));
                        browser.evaluate_js(flowmux_browser::scripts::SNAPSHOT_JS, move |result| {
                            if let Some(ack) = cell.take() {
                                let mapped = match result {
                                    Ok(json) => {
                                        if let Ok(snap) =
                                            serde_json::from_str::<flowmux_browser::DomSnapshot>(
                                                &json,
                                            )
                                        {
                                            browser2
                                                .refs
                                                .borrow_mut()
                                                .populate_from_snapshot(browser2.ref_scope, &snap);
                                        }
                                        Ok(BrowserActionResult::String(json))
                                    }
                                    Err(e) => Err(e),
                                };
                                let _ = ack.send(mapped);
                            }
                        });
                    }
                    BrowserOp::Navigate { url } => {
                        // Any navigation invalidates the current snapshot's
                        // refs — drop them so a stale `eN` can't resolve to
                        // an element on the old page.
                        browser.refs.borrow_mut().clear(browser.ref_scope);
                        browser.load_uri(&url);
                        let _ = ack.send(Ok(BrowserActionResult::Ok));
                    }
                    BrowserOp::Back => {
                        let moved = browser.go_back();
                        if moved {
                            browser.refs.borrow_mut().clear(browser.ref_scope);
                        }
                        let _ = ack.send(Ok(BrowserActionResult::Bool(moved)));
                    }
                    BrowserOp::Forward => {
                        let moved = browser.go_forward();
                        if moved {
                            browser.refs.borrow_mut().clear(browser.ref_scope);
                        }
                        let _ = ack.send(Ok(BrowserActionResult::Bool(moved)));
                    }
                    BrowserOp::Reload => {
                        browser.refs.borrow_mut().clear(browser.ref_scope);
                        browser.reload();
                        let _ = ack.send(Ok(BrowserActionResult::Ok));
                    }
                    BrowserOp::Url => {
                        let value = browser.current_url();
                        let _ = ack.send(Ok(BrowserActionResult::String(value)));
                    }
                    BrowserOp::Title => {
                        let value = browser.current_title();
                        let _ = ack.send(Ok(BrowserActionResult::String(value)));
                    }
                    BrowserOp::Click { target } => match resolve_ref(&browser, &target) {
                        Ok(sel) => run_browser_js(
                            &browser,
                            &flowmux_browser::scripts::click_by_selector(&sel),
                            ack,
                            true,
                        ),
                        Err(e) => {
                            let _ = ack.send(Err(e));
                        }
                    },
                    BrowserOp::Fill { target, value } => match resolve_ref(&browser, &target) {
                        Ok(sel) => run_browser_js(
                            &browser,
                            &flowmux_browser::scripts::fill_by_selector(&sel, &value),
                            ack,
                            true,
                        ),
                        Err(e) => {
                            let _ = ack.send(Err(e));
                        }
                    },
                    BrowserOp::Select { target, value } => match resolve_ref(&browser, &target) {
                        Ok(sel) => run_browser_js(
                            &browser,
                            &flowmux_browser::scripts::select_option_by_selector(&sel, &value),
                            ack,
                            true,
                        ),
                        Err(e) => {
                            let _ = ack.send(Err(e));
                        }
                    },
                    BrowserOp::Scroll { target, x, y } => match resolve_ref(&browser, &target) {
                        Ok(sel) => run_browser_js(
                            &browser,
                            &flowmux_browser::scripts::scroll_by_selector(&sel, x, y),
                            ack,
                            true,
                        ),
                        Err(e) => {
                            let _ = ack.send(Err(e));
                        }
                    },
                    BrowserOp::Type { text } => {
                        let js = flowmux_browser::scripts::type_keys(&text);
                        run_browser_js(&browser, &js, ack, true);
                    }
                    BrowserOp::Press { key } => {
                        let js = flowmux_browser::scripts::press_key(&key);
                        run_browser_js(&browser, &js, ack, true);
                    }
                    BrowserOp::Text { target } => match resolve_ref(&browser, &target) {
                        Ok(sel) => run_browser_js(
                            &browser,
                            &flowmux_browser::scripts::text_of_selector(&sel),
                            ack,
                            false,
                        ),
                        Err(e) => {
                            let _ = ack.send(Err(e));
                        }
                    },
                    BrowserOp::Value { target } => match resolve_ref(&browser, &target) {
                        Ok(sel) => run_browser_js(
                            &browser,
                            &flowmux_browser::scripts::value_of_selector(&sel),
                            ack,
                            false,
                        ),
                        Err(e) => {
                            let _ = ack.send(Err(e));
                        }
                    },
                    BrowserOp::Attr { target, name } => match resolve_ref(&browser, &target) {
                        Ok(sel) => run_browser_js(
                            &browser,
                            &flowmux_browser::scripts::attr_of_selector(&sel, &name),
                            ack,
                            false,
                        ),
                        Err(e) => {
                            let _ = ack.send(Err(e));
                        }
                    },

                    // ---- Phase 5 P0 action gap ------------------------
                    BrowserOp::Wait {
                        condition,
                        timeout_ms,
                        poll_ms,
                    } => run_browser_wait(browser.clone(), condition, timeout_ms, poll_ms, ack),
                    BrowserOp::Screenshot { path } => run_browser_screenshot(&browser, path, ack),
                    BrowserOp::DblClick { target } => match resolve_ref(&browser, &target) {
                        Ok(sel) => run_browser_js(
                            &browser,
                            &flowmux_browser::scripts::dblclick_by_selector(&sel),
                            ack,
                            true,
                        ),
                        Err(e) => {
                            let _ = ack.send(Err(e));
                        }
                    },
                    BrowserOp::Hover { target } => match resolve_ref(&browser, &target) {
                        Ok(sel) => run_browser_js(
                            &browser,
                            &flowmux_browser::scripts::hover_by_selector(&sel),
                            ack,
                            true,
                        ),
                        Err(e) => {
                            let _ = ack.send(Err(e));
                        }
                    },
                    BrowserOp::Focus { target } => match resolve_ref(&browser, &target) {
                        Ok(sel) => run_browser_js(
                            &browser,
                            &flowmux_browser::scripts::focus_by_selector(&sel),
                            ack,
                            true,
                        ),
                        Err(e) => {
                            let _ = ack.send(Err(e));
                        }
                    },
                    BrowserOp::Blur { target } => match resolve_ref(&browser, &target) {
                        Ok(sel) => run_browser_js(
                            &browser,
                            &flowmux_browser::scripts::blur_by_selector(&sel),
                            ack,
                            true,
                        ),
                        Err(e) => {
                            let _ = ack.send(Err(e));
                        }
                    },
                    BrowserOp::Check { target } => match resolve_ref(&browser, &target) {
                        Ok(sel) => run_browser_js(
                            &browser,
                            &flowmux_browser::scripts::check_by_selector(&sel),
                            ack,
                            true,
                        ),
                        Err(e) => {
                            let _ = ack.send(Err(e));
                        }
                    },
                    BrowserOp::Uncheck { target } => match resolve_ref(&browser, &target) {
                        Ok(sel) => run_browser_js(
                            &browser,
                            &flowmux_browser::scripts::uncheck_by_selector(&sel),
                            ack,
                            true,
                        ),
                        Err(e) => {
                            let _ = ack.send(Err(e));
                        }
                    },
                    BrowserOp::IsVisible { target } => match resolve_ref(&browser, &target) {
                        Ok(sel) => run_browser_js_bool(
                            &browser,
                            &flowmux_browser::scripts::is_visible_selector(&sel),
                            ack,
                        ),
                        Err(e) => {
                            let _ = ack.send(Err(e));
                        }
                    },
                    BrowserOp::IsEnabled { target } => match resolve_ref(&browser, &target) {
                        Ok(sel) => run_browser_js_bool(
                            &browser,
                            &flowmux_browser::scripts::is_enabled_selector(&sel),
                            ack,
                        ),
                        Err(e) => {
                            let _ = ack.send(Err(e));
                        }
                    },
                    BrowserOp::IsChecked { target } => match resolve_ref(&browser, &target) {
                        Ok(sel) => run_browser_js_bool(
                            &browser,
                            &flowmux_browser::scripts::is_checked_selector(&sel),
                            ack,
                        ),
                        Err(e) => {
                            let _ = ack.send(Err(e));
                        }
                    },
                    BrowserOp::Count { selector } => {
                        // Count takes a raw selector (not a ref) — the
                        // agent might want to know how many `.row`
                        // elements exist before navigating into them.
                        run_browser_js(
                            &browser,
                            &flowmux_browser::scripts::count_selector(&selector),
                            ack,
                            false,
                        );
                    }
                }
            }
            GtkCommand::BrowserOpenSplit {
                target_pane,
                url,
                direction,
                ack,
            } => {
                let Some(target) = target_pane.or_else(|| self.focused_pane.get()) else {
                    let _ = ack.send(Err("no target pane focused".into()));
                    return;
                };

                // cmux preferredBrowserTargetPane policy: if the source
                // pane already has a browser leaf on its right side,
                // append a new tab there instead of creating a new
                // split. Falls back to a fresh vertical split when no
                // such right sibling exists.
                if let Some(reuse_target) = self.store.find_right_sibling_browser_leaf(target).await
                {
                    match self
                        .store
                        .add_browser_surface_to_pane(reuse_target, url.clone())
                        .await
                    {
                        Some((workspace, surface_id)) => {
                            // Incremental attach: only the right-sibling
                            // browser pane gets a new tab. Other panes —
                            // including the terminal that called us — keep
                            // their PTY child and browser navigation
                            // state. Falling back to rerender_workspace
                            // here would kill claude/codex running in the
                            // caller's terminal (regression #pane-reset).
                            self.attach_or_rerender_surface(workspace, reuse_target, surface_id)
                                .await;
                            let _ = ack.send(Ok(BrowserOpenOutcome {
                                pane: reuse_target,
                                placement_strategy: PlacementStrategy::ReuseRightSibling,
                            }));
                            return;
                        }
                        None => {
                            // The right-sibling pane disappeared between
                            // discovery and update — fall through to the
                            // split path so the agent still gets a pane.
                            tracing::debug!(
                                %reuse_target,
                                "right-sibling browser leaf disappeared; falling back to split"
                            );
                        }
                    }
                }

                match self
                    .store
                    .split_pane_with_browser(target, direction, url)
                    .await
                {
                    None => {
                        let _ = ack.send(Err(format!("pane not found: {target}")));
                    }
                    Some((workspace, new_pane)) => {
                        // Incremental split: reparent the source pane's
                        // existing frame into a fresh Paned and put a new
                        // BrowserPane in the sibling slot. Other panes
                        // (including the terminal we are called from)
                        // keep their state. Same regression as above
                        // applied to the split path.
                        self.apply_split_incremental_or_rerender(
                            workspace, target, new_pane, direction,
                        )
                        .await;
                        let _ = ack.send(Ok(BrowserOpenOutcome {
                            pane: new_pane,
                            placement_strategy: PlacementStrategy::SplitRight,
                        }));
                    }
                }
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
            let has_active_surface =
                registry.active_terminal(id).is_some() || registry.active_browser(id).is_some();
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

fn make_callbacks(
    focused: FocusedPane,
    bridge: Bridge,
    options: Rc<RefCell<flowmux_config::options::Options>>,
    pane_registry: Rc<RefCell<PaneRegistry>>,
    workspace_titles: Rc<RefCell<Vec<(WorkspaceId, String)>>>,
    tab_drag_drop_seen: Rc<Cell<bool>>,
    tab_drag_drop_committed: Rc<Cell<bool>>,
) -> PaneCallbacks {
    use std::cell::RefCell;
    use std::rc::Rc;
    PaneCallbacks {
        on_child_exited: Rc::new(RefCell::new(|pane, status| {
            tracing::info!(%pane, status, "child exited");
        })),
        on_close_pane: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let (tx, _rx) = oneshot::channel();
                    let _ = bridge
                        .tx
                        .send(GtkCommand::CloseFocused { pane, ack: tx })
                        .await;
                });
            }))
        },
        on_focus: {
            let bridge = bridge.clone();
            let focused = focused.clone();
            Rc::new(RefCell::new(move |pane| {
                tracing::debug!(%pane, "pane focused");
                focused.set(Some(pane));
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge.tx.send(GtkCommand::PaneFocused { pane }).await;
                    let _ = bridge.tx.send(GtkCommand::RefreshWindowTitle).await;
                });
            }))
        },
        on_split_right: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let (tx, _rx) = oneshot::channel();
                    let _ = bridge
                        .tx
                        .send(GtkCommand::SplitFocused {
                            pane,
                            direction: flowmux_core::SplitDirection::Vertical,
                            ack: tx,
                        })
                        .await;
                });
            }))
        },
        on_split_down: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let (tx, _rx) = oneshot::channel();
                    let _ = bridge
                        .tx
                        .send(GtkCommand::SplitFocused {
                            pane,
                            direction: flowmux_core::SplitDirection::Horizontal,
                            ack: tx,
                        })
                        .await;
                });
            }))
        },
        on_activate_surface: {
            let bridge = bridge.clone();
            let focused = focused.clone();
            Rc::new(RefCell::new(move |pane, surface| {
                focused.set(Some(pane));
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge
                        .tx
                        .send(GtkCommand::ActivateSurface { pane, surface })
                        .await;
                });
            }))
        },
        on_new_surface: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge.tx.send(GtkCommand::NewSurface { pane }).await;
                });
            }))
        },
        on_new_browser_surface: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge.tx.send(GtkCommand::NewBrowserSurface { pane }).await;
                });
            }))
        },
        on_close_surface: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane, surface| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let (tx, _rx) = oneshot::channel();
                    let _ = bridge
                        .tx
                        .send(GtkCommand::CloseSurface {
                            pane,
                            surface,
                            ack: tx,
                        })
                        .await;
                });
            }))
        },
        on_rename_surface: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane, surface| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge
                        .tx
                        .send(GtkCommand::ShowRenameSurfaceDialog { pane, surface })
                        .await;
                });
            }))
        },
        on_show_surface_folder: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane, surface| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge
                        .tx
                        .send(GtkCommand::ShowSurfaceFolder { pane, surface })
                        .await;
                });
            }))
        },
        on_copy_surface_text: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane, surface| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge
                        .tx
                        .send(GtkCommand::CopySurfaceText { pane, surface })
                        .await;
                });
            }))
        },
        on_reorder_surface: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane, surface, target_index| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let (tx, _rx) = oneshot::channel();
                    let _ = bridge
                        .tx
                        .send(GtkCommand::ReorderSurface {
                            pane,
                            surface,
                            target_index,
                            ack: tx,
                        })
                        .await;
                });
            }))
        },
        on_tab_drag_to_new_window: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane, surface| {
                tracing::debug!(%pane, %surface, "tab drag requested tear-off window");
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge
                        .tx
                        .send(GtkCommand::TearOffSurface { pane, surface })
                        .await;
                });
            }))
        },
        on_move_surface_to_pane: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(
                move |src_pane, surface, surface_model, dst_pane, target_index| {
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let (ack_tx, ack_rx) = oneshot::channel();
                        let _ = bridge
                            .tx
                            .send(GtkCommand::MoveSurfaceToPane {
                                src_pane,
                                surface,
                                surface_model,
                                dst_pane,
                                target_index,
                                ack: ack_tx,
                            })
                            .await;
                        let _ = ack_rx.await;
                    });
                },
            ))
        },
        on_move_surface_to_workspace: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |src_pane, surface, dst_workspace| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let (ack_tx, ack_rx) = oneshot::channel();
                    let _ = bridge
                        .tx
                        .send(GtkCommand::MoveSurfaceToWorkspace {
                            src_pane,
                            surface,
                            dst_workspace,
                            ack: ack_tx,
                        })
                        .await;
                    let _ = ack_rx.await;
                });
            }))
        },
        list_workspaces: {
            let workspace_titles = workspace_titles.clone();
            Rc::new(move || workspace_titles.borrow().clone())
        },
        workspace_of_pane: {
            let pane_registry = pane_registry.clone();
            Rc::new(move |pane| pane_registry.borrow().workspace_of_pane(pane))
        },
        on_split_surface_into_pane: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(
                move |src_pane, surface, surface_model, dst_pane, direction| {
                    let bridge = bridge.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let (ack_tx, ack_rx) = oneshot::channel();
                        let _ = bridge
                            .tx
                            .send(GtkCommand::SplitSurfaceIntoPane {
                                src_pane,
                                surface,
                                surface_model,
                                dst_pane,
                                direction,
                                ack: ack_tx,
                            })
                            .await;
                        let _ = ack_rx.await;
                    });
                },
            ))
        },
        tab_drag_drop_seen,
        tab_drag_drop_committed,
        tab_drag_split_candidate: Rc::new(RefCell::new(None)),
        on_terminal_cwd_changed: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane, surface, cwd| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge
                        .tx
                        .send(GtkCommand::TerminalCwdChanged { pane, surface, cwd })
                        .await;
                });
            }))
        },
        on_browser_uri_changed: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane, surface, url| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge
                        .tx
                        .send(GtkCommand::BrowserUriChanged { pane, surface, url })
                        .await;
                });
            }))
        },
        on_browser_title_changed: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane, surface, title| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge
                        .tx
                        .send(GtkCommand::BrowserTitleChanged {
                            pane,
                            surface,
                            title,
                        })
                        .await;
                });
            }))
        },
        on_terminal_title_changed: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane, surface, title: String| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge
                        .tx
                        .send(GtkCommand::TerminalTitleChanged {
                            pane,
                            surface,
                            title,
                        })
                        .await;
                });
            }))
        },
        read_options: {
            let options = options.clone();
            Rc::new(move || options.borrow().clone())
        },
        position_of_surface_in_pane: {
            let registry = pane_registry.clone();
            Rc::new(move |pane, surface| {
                let r = registry.borrow();
                r.surface_tabs
                    .get(&pane)?
                    .iter()
                    .position(|(id, _)| *id == surface)
            })
        },
        on_open_url: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane, url| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge
                        .tx
                        .send(GtkCommand::OpenUrlInBrowserTab { pane, url })
                        .await;
                });
            }))
        },
        on_open_image: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane, path| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge
                        .tx
                        .send(GtkCommand::OpenImageViewer { pane, path })
                        .await;
                });
            }))
        },
        on_open_markdown: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane, path| {
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge
                        .tx
                        .send(GtkCommand::OpenMarkdownViewer { pane, path })
                        .await;
                });
            }))
        },
    }
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
                    .send(GtkCommand::RemoveWorkspace { id, ack: tx })
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
        self.label.set_text(message);
        let current = self.generation.get().wrapping_add(1);
        self.generation.set(current);
        self.revealer.set_reveal_child(true);

        let revealer = self.revealer.clone();
        let generation = self.generation.clone();
        glib::timeout_add_local_once(Duration::from_millis(1400), move || {
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

    fn agent_bar_visible(controller: &WindowController) -> bool {
        controller.agent_bar.bar.root.property::<bool>("visible")
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
            .dispatch(GtkCommand::SetAgentStatus {
                workspace: ws_id,
                status: Some(AgentStatus::Working),
            })
            .await;
        assert!(
            agent_bar_visible(&controller),
            "SetAgentStatus must refresh Agent Bar after a live agent appears"
        );

        controller.store.set_agent_activity(surface, None).await;
        controller
            .dispatch(GtkCommand::SetAgentStatus {
                workspace: ws_id,
                status: None,
            })
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
            .dispatch(GtkCommand::SetAgentStatus {
                workspace: ws_id,
                status: Some(AgentStatus::Working),
            })
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

    /// Push 200 notifications (the MAX_RETAINED cap) interleaved with
    /// SetNotificationDesktopId and periodic workspace-activation
    /// sweeps. After the final activation the badge must be at 0 and
    /// every entry whose desktop_id was attached before the sweep must
    /// be marked read.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    async fn stress_many_notifications_with_periodic_sweeps_drains_to_zero() {
        let (controller, ws_id, pane) =
            build_single_workspace_controller("com.flowmux.App.UiTest.StressManyNotif").await;
        const TOTAL: usize = 200;
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

        // The total entry count is capped at MAX_RETAINED (200). The
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
