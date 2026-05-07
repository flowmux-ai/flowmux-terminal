// SPDX-License-Identifier: GPL-3.0-or-later
//! Main application window. Composes header bar + sidebar + content
//! stack and exposes a [`WindowController`] that routes [`GtkCommand`]
//! values from the bridge to widget operations.

use crate::bridge::{Bridge, BrowserActionResult, BrowserOp, FocusDir, GtkCommand, WsNav};
use crate::keybindings::FocusedPane;
use crate::notifications::{NotificationEntry, NotificationLog};
use crate::theme::ResolvedTheme;
use crate::ui::sidebar::Sidebar;
use crate::ui::terminal_pane::PaneCallbacks;
use crate::ui::workspace_view::{attach_surface_to_pane, build_surface, PaneRegistry};
use adw::prelude::*;
use flowmux_core::{PaneId, SurfaceId, Workspace, WorkspaceId};
use flowmux_daemon::StateStore;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::oneshot;
use vte::prelude::*;
use webkit6::prelude::*;

#[derive(Clone)]
pub struct WindowController {
    pub window: adw::ApplicationWindow,
    pub focused_pane: FocusedPane,
    sidebar: Sidebar,
    stack: gtk::Stack,
    surfaces: Rc<RefCell<HashMap<WorkspaceId, gtk::Widget>>>,
    pane_registry: Rc<RefCell<PaneRegistry>>,
    callbacks: PaneCallbacks,
    store: StateStore,
    bridge: Bridge,
    theme: Arc<ResolvedTheme>,
    notification_log: NotificationLog,
    options: Rc<RefCell<flowmux_config::options::Options>>,
    /// 글로벌 CssProvider — 옵션 다이얼로그에서 포커스 테두리 색이
    /// 바뀌면 같은 인스턴스의 CSS를 다시 로드해 모든 pane에 즉시
    /// 반영한다.
    css_provider: gtk::CssProvider,
}

impl WindowController {
    pub fn new(
        app: &adw::Application,
        store: StateStore,
        theme: Arc<ResolvedTheme>,
        bridge: Bridge,
        css_provider: gtk::CssProvider,
    ) -> Self {
        let focused_pane: FocusedPane = Rc::new(Cell::new(None));
        let notification_log = crate::notifications::new_log();
        let stack = gtk::Stack::new();
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.set_hexpand(true);
        stack.set_vexpand(true);

        let surfaces: Rc<RefCell<HashMap<WorkspaceId, gtk::Widget>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let surfaces_for_select = surfaces.clone();
        let stack_for_select = stack.clone();
        let store_for_select = store.clone();

        let on_select = move |id: WorkspaceId| {
            if surfaces_for_select.borrow().contains_key(&id) {
                stack_for_select.set_visible_child_name(&id.to_string());
            }
            let store = store_for_select.clone();
            glib::MainContext::default().spawn_local(async move {
                store.set_active_workspace(Some(id)).await;
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
        let sidebar = Sidebar::new(
            on_select,
            on_close,
            bridge.clone(),
            notification_log.clone(),
        );

        let pane_registry: Rc<RefCell<PaneRegistry>> =
            Rc::new(RefCell::new(PaneRegistry::default()));
        let initial_options = flowmux_config::options::load();
        tracing::info!(
            zoom_percent = initial_options.zoom_percent,
            engine = ?initial_options.default_browser_engine,
            "options loaded"
        );
        let options = Rc::new(RefCell::new(initial_options));
        let callbacks =
            make_callbacks(focused_pane.clone(), bridge.clone(), options.clone());

        // gtk::Paned lets the user drag the divider between the
        // sidebar and the content stack — replaces the fixed-width
        // adw::OverlaySplitView so people can hide / widen the tab
        // list to taste.
        sidebar.root.set_size_request(160, -1);
        let split = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&sidebar.root)
            .end_child(&stack)
            .resize_start_child(false)
            .resize_end_child(true)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .position(260)
            .build();

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&split));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(1280)
            .default_height(800)
            .title("flowmux")
            .build();
        window.set_content(Some(&toolbar));

        register_workspace_actions(&window, &store, &bridge);

        let controller = Self {
            window,
            focused_pane,
            sidebar,
            stack,
            surfaces,
            pane_registry,
            callbacks,
            store,
            bridge,
            theme,
            notification_log,
            options,
            css_provider,
        };
        controller.install_state_flush_on_close();
        controller
    }

    pub fn show_status_when_empty(&self) {
        if self.surfaces.borrow().is_empty() {
            if self.stack.child_by_name("__empty").is_none() {
                let status = adw::StatusPage::builder()
                    .icon_name("utilities-terminal-symbolic")
                    .title("flowmux")
                    .description("No workspaces yet — open one with: flowmux workspace new --root .")
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
        self.sidebar.select_workspace(ws.id);
        self.focus_first_leaf_of(ws);
    }

    /// 새로 만들어진 surface를 가능한 한 incremental하게 붙인다.
    ///
    /// 기존 동작: rerender_workspace 호출 → 워크스페이스 전체 위젯 재생성 →
    /// 다른 pane의 탭브라우저 navigate 상태와 터미널 셸 세션이 모두
    /// 사라짐.
    ///
    /// 새 동작: 해당 pane의 tab bar / stack에만 위젯을 append한다.
    /// pane이 아직 화면에 렌더되지 않았거나 (예: 다른 워크스페이스가 보이고
    /// 있을 때) registry에서 핸들을 못 찾은 경우엔 안전하게 전체
    /// rerender로 폴백한다.
    async fn attach_or_rerender_surface(
        &self,
        ws_id: WorkspaceId,
        pane: PaneId,
        surface_id: SurfaceId,
    ) {
        let Some(ws) = self.store.get_workspace(ws_id).await else {
            return;
        };
        let surface = ws
            .surfaces
            .iter()
            .find_map(|s| s.root_pane.find_surface(pane, surface_id));
        if let Some(surface) = surface {
            let attached = attach_surface_to_pane(
                pane,
                ws.id,
                &surface,
                &self.callbacks,
                self.pane_registry.clone(),
                self.theme.clone(),
            );
            if attached {
                self.refresh_window_title().await;
                return;
            }
        }
        self.rerender_workspace(&ws);
        self.refresh_window_title().await;
    }

    /// Shared pane registry — exposed so the keybindings module can
    /// reach into VTE widgets for copy/paste actions on the GTK
    /// main thread without going through the bridge.
    pub fn pane_registry(&self) -> Rc<RefCell<PaneRegistry>> {
        self.pane_registry.clone()
    }

    fn install_state_flush_on_close(&self) {
        let controller = self.clone();
        self.window.connect_close_request(move |_| {
            controller.flush_terminal_cwds_blocking();
            if let Err(e) = controller.store.save_now_blocking() {
                tracing::warn!(error = %e, "state save on close failed");
            }
            glib::Propagation::Proceed
        });
    }

    /// 윈도우 제목을 "flowmux - {focused tab name}"으로 다시 계산한다.
    /// 포커스된 pane이 없거나 그 pane에 active surface가 없으면
    /// "flowmux"로 폴백한다.
    async fn refresh_window_title(&self) {
        let title = match self.focused_pane.get() {
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
        self.window.set_title(Some(&next));
    }

    async fn update_terminal_cwd(&self, pane: PaneId, surface: SurfaceId, cwd: std::path::PathBuf) {
        if self
            .store
            .update_surface_cwd(pane, surface, cwd)
            .await
            .is_some()
        {
            if let Some(title) = self.store.surface_title(pane, surface).await {
                self.pane_registry
                    .borrow()
                    .set_surface_title(surface, &title);
            }
        }
    }

    fn flush_terminal_cwds_blocking(&self) {
        let cwd_entries = self.pane_registry.borrow().terminal_cwds();
        for (pane, surface, cwd) in cwd_entries {
            let _ = self.store.update_surface_cwd_blocking(pane, surface, cwd);
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

    async fn activate_active_or_show_empty(&self) {
        if let Some(id) = self.store.active_or_first().await {
            if self.surfaces.borrow().contains_key(&id) {
                self.activate_workspace(id).await;
                return;
            }
        }
        self.show_status_when_empty();
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
            if let Some(pane) = r.active_terminal(leaf_id) {
                pane.widget.grab_focus();
            }
        });
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
                crate::ui::options_dialog::present(&self.window, current, move |opts| {
                    if let Err(e) = flowmux_config::options::save(&opts) {
                        tracing::warn!(error = %e, "options save failed");
                        return;
                    }
                    *options_cell.borrow_mut() = opts.clone();
                    // 줌은 모든 기존 위젯에 즉시 반영.
                    let registry = registry.borrow();
                    for terminal in registry.terminals.values() {
                        terminal.widget.set_font_scale(opts.zoom_factor());
                    }
                    for browser in registry.browsers.values() {
                        browser.web_view.set_zoom_level(opts.zoom_factor());
                    }
                    // 포커스 테두리 색은 CSS 한 줄을 다시 로드해 반영 —
                    // 같은 CssProvider 인스턴스라 새 변경이 모든 위젯에
                    // 자동으로 다시 적용된다.
                    css_provider.load_from_string(
                        &theme.css(opts.focus_border_color_or_default()),
                    );
                    tracing::info!(
                        zoom_percent = opts.zoom_percent,
                        engine = ?opts.default_browser_engine,
                        focus_border_color = %opts.focus_border_color,
                        "options applied"
                    );
                });
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
            GtkCommand::WorkspaceRerender { id, ack } => {
                if let Some(ws) = self.store.get_workspace(id).await {
                    self.rerender_workspace(&ws);
                }
                let _ = ack.send(());
            }
            GtkCommand::SplitFocused {
                pane,
                direction,
                ack,
            } => {
                match self.store.split_pane(pane, direction).await {
                    Some((ws_id, new_pane)) => {
                        if let Some(ws) = self.store.get_workspace(ws_id).await {
                            self.rerender_workspace(&ws);
                        }
                        // Focus the freshly-created pane, not whichever
                        // first-leaf rerender_workspace defaulted to.
                        let registry = self.pane_registry.clone();
                        glib::idle_add_local_once(move || {
                            let r = registry.borrow();
                            if let Some(pane) = r.active_terminal(new_pane) {
                                pane.widget.grab_focus();
                            }
                        });
                        let _ = ack.send(Ok(new_pane));
                    }
                    None => {
                        let _ = ack.send(Err(format!("pane not found: {pane}")));
                    }
                }
            }
            GtkCommand::CloseFocused { pane, ack } => match self.store.close_pane(pane).await {
                None => {
                    let _ = ack.send(Err(format!("pane not found: {pane}")));
                }
                Some(flowmux_daemon::CloseOutcome::PaneRemoved { workspace })
                | Some(flowmux_daemon::CloseOutcome::SurfaceRemoved { workspace }) => {
                    if let Some(ws) = self.store.get_workspace(workspace).await {
                        self.rerender_workspace(&ws);
                    }
                    let _ = ack.send(Ok(()));
                }
                Some(flowmux_daemon::CloseOutcome::WorkspaceRemoved { workspace }) => {
                    self.drop_workspace(workspace);
                    self.activate_active_or_show_empty().await;
                    let _ = ack.send(Ok(()));
                }
            },
            GtkCommand::FocusDirection { from, dir } => {
                self.focus_in_direction(from, dir);
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
            GtkCommand::ActivateSurface { pane, surface } => {
                self.store.set_active_surface(pane, surface).await;
                self.pane_registry
                    .borrow_mut()
                    .activate_surface(pane, surface);
                self.refresh_window_title().await;
                // 탭(클릭 / Shift+Tab 사이클 / IPC 등 모든 경로)으로
                // surface가 활성화된 직후, 키보드 포커스를 새로 활성된
                // 위젯(터미널의 vte::Terminal 또는 브라우저의 WebView)
                // 으로 옮긴다. 이렇게 해야 사용자가 그대로 타이핑하면
                // 새 탭의 셸/페이지로 키 입력이 들어가고, Tab 키도 셸의
                // 자동완성으로 처리된다 (탭바로 포커스 traversal 되지
                // 않음). 위젯이 stack에 추가된 직후라 idle_add로 한
                // 프레임 미룬다.
                let registry = self.pane_registry.clone();
                glib::idle_add_local_once(move || {
                    let r = registry.borrow();
                    if let Some(term) = r.terminals.get(&surface) {
                        term.widget.grab_focus();
                    } else if let Some(browser) = r.browsers.get(&surface) {
                        browser.web_view.grab_focus();
                    }
                });
            }
            GtkCommand::CloseSurface { pane, surface, ack } => {
                match self.store.close_surface(pane, surface).await {
                    None => {
                        let _ = ack.send(Err(format!("surface not found: {surface}")));
                    }
                    Some(flowmux_daemon::CloseOutcome::WorkspaceRemoved { workspace }) => {
                        self.drop_workspace(workspace);
                        self.activate_active_or_show_empty().await;
                        let _ = ack.send(Ok(()));
                    }
                    Some(flowmux_daemon::CloseOutcome::PaneRemoved { workspace })
                    | Some(flowmux_daemon::CloseOutcome::SurfaceRemoved { workspace }) => {
                        if let Some(ws) = self.store.get_workspace(workspace).await {
                            self.rerender_workspace(&ws);
                        }
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
                // store 측 reorder가 변화 없음(None)을 반환하면 GTK 위젯도
                // 그대로 둔다. 위젯 reorder는 메인 스레드의 PaneRegistry가
                // 갖고 있는 탭바 gtk::Box와 surface_tabs 인덱스를 모두
                // 동시에 업데이트해야 일관성이 깨지지 않는다.
                let store_result = self
                    .store
                    .reorder_surface_in_pane(pane, surface, target_index)
                    .await;
                if store_result.is_some() {
                    self.pane_registry
                        .borrow_mut()
                        .reorder_surface_widget(pane, surface, target_index);
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
            GtkCommand::TerminalCwdChanged { pane, surface, cwd } => {
                self.update_terminal_cwd(pane, surface, cwd).await;
                self.refresh_window_title().await;
            }
            GtkCommand::BrowserUriChanged { pane, surface, url } => {
                let _ = self.store.update_browser_url(pane, surface, url).await;
            }
            GtkCommand::BrowserTitleChanged {
                pane,
                surface,
                title,
            } => {
                if self
                    .store
                    .update_surface_auto_title(pane, surface, title)
                    .await
                    .is_some()
                {
                    if let Some(latest) = self.store.surface_title(pane, surface).await {
                        self.pane_registry
                            .borrow()
                            .set_surface_title(surface, &latest);
                    }
                    self.refresh_window_title().await;
                }
            }
            GtkCommand::TerminalTitleChanged {
                pane,
                surface,
                title,
            } => {
                // VTE가 OSC 0/2로 받은 윈도우 타이틀이다. 셸 자체가
                // 보내는 prompt 형태(예: "user@host:~/path")는 이미
                // cwd-driven 라벨과 중복되니 trim 후 빈/공백만 남는
                // 경우 무시. 그 외엔 BrowserTitleChanged와 동일
                // 처리(title_locked 존중) — store가 적용되면 탭
                // 라벨과 윈도우 제목을 갱신.
                if title.trim().is_empty() {
                    return;
                }
                if self
                    .store
                    .update_surface_auto_title(pane, surface, title)
                    .await
                    .is_some()
                {
                    if let Some(latest) = self.store.surface_title(pane, surface).await {
                        self.pane_registry
                            .borrow()
                            .set_surface_title(surface, &latest);
                    }
                    self.refresh_window_title().await;
                }
            }
            GtkCommand::RefreshWindowTitle => {
                self.refresh_window_title().await;
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
                if self.store.remove_workspace(id).await {
                    self.drop_workspace(id);
                    self.activate_active_or_show_empty().await;
                }
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
                if let Some(ws) = self.store.get_workspace(id).await {
                    show_rename_dialog(&self.window, id, &ws.name, self.bridge.clone());
                }
            }
            GtkCommand::ShowColorDialog { id } => {
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
                title,
                body,
                level,
            } => {
                self.notification_log.borrow_mut().push(NotificationEntry {
                    title,
                    body,
                    level,
                    created_at: chrono::Utc::now(),
                    seen: false,
                });
                self.sidebar.bump_notification_badge();
                if matches!(level, flowmux_core::NotificationLevel::AttentionNeeded) {
                    if let Some(pane) = pane {
                        if let Some(ws_id) = self.store.workspace_for_pane(pane).await {
                            self.sidebar.mark_attention(ws_id);
                        }
                    }
                }
            }
            GtkCommand::FocusWorkspaceAt { idx } => {
                let snap = self.store.snapshot().await;
                let target_idx = (idx as usize).saturating_sub(1);
                if let Some(id) = snap.workspace_order.get(target_idx).copied() {
                    self.activate_workspace(id).await;
                }
            }
            GtkCommand::PaneSendKeys { pane, keys, ack } => {
                let registry = self.pane_registry.borrow();
                let res = match registry.active_terminal(pane) {
                    Some(p) => {
                        p.feed(keys.as_bytes());
                        Ok(())
                    }
                    None => Err(format!("pane not found: {pane}")),
                };
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
                    BrowserOp::Navigate { url } => {
                        browser.web_view.load_uri(&url);
                        let _ = ack.send(Ok(BrowserActionResult::Ok));
                    }
                    BrowserOp::Back => {
                        let moved = browser.web_view.can_go_back();
                        if moved {
                            browser.web_view.go_back();
                        }
                        let _ = ack.send(Ok(BrowserActionResult::Bool(moved)));
                    }
                    BrowserOp::Forward => {
                        let moved = browser.web_view.can_go_forward();
                        if moved {
                            browser.web_view.go_forward();
                        }
                        let _ = ack.send(Ok(BrowserActionResult::Bool(moved)));
                    }
                    BrowserOp::Reload => {
                        browser.web_view.reload();
                        let _ = ack.send(Ok(BrowserActionResult::Ok));
                    }
                    BrowserOp::Url => {
                        let value = browser
                            .web_view
                            .uri()
                            .map(|uri| uri.to_string())
                            .unwrap_or_default();
                        let _ = ack.send(Ok(BrowserActionResult::String(value)));
                    }
                    BrowserOp::Title => {
                        let value = browser
                            .web_view
                            .title()
                            .map(|title| title.to_string())
                            .unwrap_or_default();
                        let _ = ack.send(Ok(BrowserActionResult::String(value)));
                    }
                    BrowserOp::Eval { source } => {
                        let cell = std::cell::Cell::new(Some(ack));
                        browser.evaluate_js(&source, move |result| {
                            if let Some(ack) = cell.take() {
                                let _ = ack.send(result.map(BrowserActionResult::String));
                            }
                        });
                    }
                    BrowserOp::Snapshot => {
                        run_browser_js(&browser, flowmux_browser::scripts::SNAPSHOT_JS, ack, false);
                    }
                    BrowserOp::Click { target } => {
                        let js = flowmux_browser::scripts::click_by_ref(&target);
                        run_browser_js(&browser, &js, ack, true);
                    }
                    BrowserOp::Fill { target, value } => {
                        let js = flowmux_browser::scripts::fill_by_ref(&target, &value);
                        run_browser_js(&browser, &js, ack, true);
                    }
                    BrowserOp::Select { target, value } => {
                        let js = flowmux_browser::scripts::select_option_by_ref(&target, &value);
                        run_browser_js(&browser, &js, ack, true);
                    }
                    BrowserOp::Scroll { target, x, y } => {
                        let js = flowmux_browser::scripts::scroll_by_ref(&target, x, y);
                        run_browser_js(&browser, &js, ack, true);
                    }
                    BrowserOp::Type { text } => {
                        let js = flowmux_browser::scripts::type_keys(&text);
                        run_browser_js(&browser, &js, ack, true);
                    }
                    BrowserOp::Press { key } => {
                        let js = flowmux_browser::scripts::press_key(&key);
                        run_browser_js(&browser, &js, ack, true);
                    }
                    BrowserOp::Text { target } => {
                        let js = flowmux_browser::scripts::text_of(&target);
                        run_browser_js(&browser, &js, ack, false);
                    }
                    BrowserOp::Value { target } => {
                        let js = flowmux_browser::scripts::value_of(&target);
                        run_browser_js(&browser, &js, ack, false);
                    }
                    BrowserOp::Attr { target, name } => {
                        let js = flowmux_browser::scripts::attr_of(&target, &name);
                        run_browser_js(&browser, &js, ack, false);
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
                match self
                    .store
                    .split_pane_with_browser(target, direction, url)
                    .await
                {
                    None => {
                        let _ = ack.send(Err(format!("pane not found: {target}")));
                    }
                    Some((workspace, new_pane)) => {
                        if let Some(ws) = self.store.get_workspace(workspace).await {
                            self.rerender_workspace(&ws);
                        }
                        let _ = ack.send(Ok(new_pane));
                    }
                }
            }
        }
    }

    /// Move keyboard focus to the nearest pane in `dir` relative to
    /// the pane currently identified by `from`. Bbox computation is
    /// in the stack's coordinate space so split orientation doesn't
    /// matter.
    fn focus_in_direction(&self, from: PaneId, dir: FocusDir) {
        use gtk::graphene::Rect;

        let registry = self.pane_registry.borrow();
        let from_widget = match registry.pane_frame(from) {
            Some(p) => p,
            None => return,
        };
        let stack = &self.stack;
        let Some(from_bbox) = from_widget.compute_bounds(stack) else {
            return;
        };
        let from_center = (
            from_bbox.x() + from_bbox.width() / 2.0,
            from_bbox.y() + from_bbox.height() / 2.0,
        );

        let mut best: Option<(PaneId, f32)> = None;
        for id in registry.pane_ids() {
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
            if let Some(pane) = registry.active_terminal(id) {
                pane.widget.grab_focus();
            }
        } else {
            tracing::debug!(?dir, "no pane in that direction");
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
        self.store.set_active_workspace(Some(id)).await;
        if let Some(ws) = self.store.get_workspace(id).await {
            self.focus_first_leaf_of(&ws);
        }
    }

    pub async fn restore_from_store(&self) {
        let snap = self.store.snapshot().await;
        for ws in &snap.workspaces {
            self.render_workspace_with_activation(ws, false);
        }
        let active = snap
            .active_workspace
            .or_else(|| snap.workspace_order.first().copied());
        if let Some(active) = active {
            self.activate_workspace(active).await;
        }
    }
}

fn make_callbacks(
    focused: FocusedPane,
    bridge: Bridge,
    options: Rc<RefCell<flowmux_config::options::Options>>,
) -> PaneCallbacks {
    use std::cell::RefCell;
    use std::rc::Rc;
    PaneCallbacks {
        on_notification: Rc::new(RefCell::new(|pane, title, body| {
            tracing::info!(%pane, %title, %body, "OSC 99 from pane");
        })),
        on_bell: Rc::new(RefCell::new(|pane| {
            tracing::debug!(%pane, "BEL");
        })),
        on_child_exited: Rc::new(RefCell::new(|pane, status| {
            tracing::info!(%pane, status, "child exited");
        })),
        on_focus: {
            let bridge = bridge.clone();
            Rc::new(RefCell::new(move |pane| {
                tracing::debug!(%pane, "pane focused");
                focused.set(Some(pane));
                let bridge = bridge.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = bridge.tx.send(GtkCommand::RefreshWindowTitle).await;
                });
            }))
        },
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
            Rc::new(RefCell::new(move |pane, surface| {
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
    }
}

/// Inject cookies into the default WebKit network session.
///
/// Real injection goes through `WebKit.NetworkSession.cookie_manager()`
/// → `CookieManager.add_cookie(&soup::Cookie, ...)`. The `soup::Cookie`
/// type is only re-exported from webkit6 when the `soup3` feature is
/// enabled (which in turn pulls in libsoup-3). To keep the default
/// build minimal we record the cookies that *would* be injected and
/// return the count; flipping `flowmux-app/Cargo.toml` to
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
                show_rename_dialog(&window, id, &ws.name, bridge);
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
    let dialog = adw::AlertDialog::new(Some("Rename Tab"), None);
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
            let new_name = entry_for_resp.text().to_string();
            if !new_name.trim().is_empty() {
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

/// Evaluate a script on `browser`'s WebView and forward the result
/// through `ack`. When `ok_string_required` is true, the script's
/// returned string must be exactly `"ok"` for the ack to resolve to
/// `BrowserActionResult::Ok` — anything else (including the
/// `"error: …"` strings flowmux_browser scripts use) becomes an Err.
/// When false, the raw string is forwarded so the caller can parse
/// JSON (Snapshot) or read a value back (Text / Value / Attr).
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
pub fn spawn_dispatch_loop(rx: async_channel::Receiver<GtkCommand>, controller: WindowController) {
    glib::MainContext::default().spawn_local(async move {
        while let Ok(cmd) = rx.recv().await {
            controller.dispatch(cmd).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use flowmux_core::PaneContent;
    use flowmux_state::State;

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
        let controller =
            WindowController::new(
                &app,
                store.clone(),
                Arc::new(ResolvedTheme::load()),
                bridge,
                gtk::CssProvider::new(),
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

    /// 포커스 변경 / 탭 활성화 / RefreshWindowTitle 명령에 따라
    /// adw::ApplicationWindow.title이 "flowmux - {focused tab name}"
    /// 형식으로 갱신되는지 검증한다. 포커스가 없을 때는 "flowmux"
    /// 단독.
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
        );
        controller.render_workspace(&ws);

        // 포커스가 없는 초기 상태에서는 단독 "flowmux"로 폴백.
        controller.focused_pane.set(None);
        controller
            .dispatch(GtkCommand::RefreshWindowTitle)
            .await;
        assert_eq!(
            controller.window.title().map(|s| s.to_string()).as_deref(),
            Some("flowmux")
        );

        // 포커스가 잡히면 "flowmux - {tab name}"으로 바뀐다.
        let expected_tab_name = store.surface_title(pane, surface).await.unwrap();
        controller.focused_pane.set(Some(pane));
        controller
            .dispatch(GtkCommand::RefreshWindowTitle)
            .await;
        assert_eq!(
            controller.window.title().map(|s| s.to_string()),
            Some(format!("flowmux - {expected_tab_name}"))
        );

        // RenameSurface 디스패치 후에도 윈도우 제목이 새 이름을 따라간다.
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

    /// `BrowserUriChanged` 디스패치가 store에 마지막 navigate URL을
    /// 반영하는지 검증한다. webkit::WebView를 띄우지 않고 store
    /// 상호작용만 검증하기 위해, 미리 add_browser_surface_to_pane으로
    /// state에 browser surface를 만들어 두고 dispatch한다.
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
        // browser surface를 직접 추가해서 webkit init 부담을 피한다.
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

    /// `BrowserTitleChanged` 디스패치가 store/탭 라벨 모두를 갱신.
    /// 사용자가 직접 rename한 surface는 자동 갱신되지 않음을 함께
    /// 검증.
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
        );

        // A: title_locked=false → 갱신.
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

        // B: title_locked=true → 그대로 "Pinned".
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

    /// `TerminalTitleChanged` 디스패치가 OSC 0/2 제목으로 탭 라벨을
    /// 갱신한다. 빈 문자열은 무시되고, title_locked=true인 surface는
    /// 보호된다 (사용자 rename 우선).
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
        // 첫 channel 생성 시 자동으로 만들어진 terminal surface 그대로 사용.
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
        );

        // 1. 정상적인 OSC 2 → 탭 라벨 갱신.
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

        // 2. 빈 문자열 → 무시 (셸 종료 / OSC reset 시).
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

        // 3. 공백만 있는 타이틀 → 무시.
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

        // 4. title_locked=true → 무시.
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

    /// 새 탭이 추가(NewSurface)되면 그 탭이 active로 잡혀 윈도우
    /// 제목도 새 탭 이름으로 갱신된다. 기존 활성 탭 이름을 유지하면
    /// 안 된다.
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
        );
        controller.render_workspace(&ws);
        controller.focused_pane.set(Some(pane));
        controller.dispatch(GtkCommand::RefreshWindowTitle).await;
        let initial = controller.window.title().map(|s| s.to_string());

        // dispatch가 자체적으로 새 terminal surface를 만들고, attach 후
        // refresh_window_title 까지 호출한다.
        controller
            .dispatch(GtkCommand::NewSurface { pane })
            .await;

        let title_now = controller.window.title().map(|s| s.to_string());
        assert!(title_now.is_some());
        assert!(
            title_now.as_deref().unwrap().starts_with("flowmux - "),
            "title should keep the flowmux prefix, got {title_now:?}"
        );
        // 새 탭이 active 라면 store에서 그 surface의 title이 곧 윈도우 제목.
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

    /// ActivateSurface 디스패치만으로도 윈도우 제목이 활성 탭 기준
    /// 으로 다시 계산된다.
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
        // browser surface는 add_browser_surface_to_pane 시 "Browser"로 저장.
        assert_eq!(
            controller.window.title().map(|s| s.to_string()).as_deref(),
            Some("flowmux - Browser")
        );
    }

    /// ReorderSurface 디스패치가 store와 PaneRegistry를 모두 갱신하고
    /// 활성 탭이 보존되는지, 같은 자리로 보내면 no-op인지, 다른 pane의
    /// 탭은 영향이 없는지 한 번에 검증한다.
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
        // 활성 탭을 첫 번째로 되돌려 둔다 — browser가 마지막에 추가돼서 active.
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
        );
        let ws = store.get_workspace(ws_id).await.unwrap();
        controller.render_workspace(&ws);

        // first(인덱스 0) → 마지막(인덱스 2)
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

        // store 측 순서 확인.
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
        // 활성 탭은 first로 그대로.
        assert_eq!(*active, first);

        // 같은 자리로 다시 디스패치 → store가 None을 돌려주므로 위젯도 그대로.
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

        // 길이를 넘는 인덱스 → 끝으로 클램프되어 자기 자리이므로 no-op.
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

        // browser(가운데)를 처음으로.
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
        // 활성은 여전히 first.
        assert_eq!(*active, first);
    }

    #[test]
    fn app_source_does_not_reintroduce_glib_polling_timers() {
        fn visit(path: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
            if path.is_dir() {
                for entry in std::fs::read_dir(path).unwrap() {
                    visit(&entry.unwrap().path(), files);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path.to_path_buf());
            }
        }

        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let needle_one = ["timeout", "_add_local"].concat();
        let needle_two = ["glib", "::", "timeout"].concat();
        let mut files = Vec::new();
        visit(&src, &mut files);
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            assert!(
                !text.contains(&needle_one) && !text.contains(&needle_two),
                "polling timer found in {}",
                file.display()
            );
        }
    }
}
