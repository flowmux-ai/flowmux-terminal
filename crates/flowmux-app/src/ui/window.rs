// SPDX-License-Identifier: GPL-3.0-or-later
//! Main application window. Composes header bar + sidebar + content
//! stack and exposes a [`WindowController`] that routes [`GtkCommand`]
//! values from the bridge to widget operations.

use crate::bridge::{Bridge, FocusDir, GtkCommand, WsNav};
use crate::keybindings::FocusedPane;
use tokio::sync::oneshot;
use crate::theme::ResolvedTheme;
use crate::ui::sidebar::Sidebar;
use crate::ui::terminal_pane::PaneCallbacks;
use crate::ui::workspace_view::{build_surface, PaneRegistry};
use adw::prelude::*;
use flowmux_core::{PaneId, Workspace, WorkspaceId};
use flowmux_daemon::StateStore;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

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
    theme: Arc<ResolvedTheme>,
}

impl WindowController {
    pub fn new(
        app: &adw::Application,
        store: StateStore,
        theme: Arc<ResolvedTheme>,
        bridge: Bridge,
    ) -> Self {
        let focused_pane: FocusedPane = Rc::new(Cell::new(None));
        let stack = gtk::Stack::new();
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.set_hexpand(true);
        stack.set_vexpand(true);

        let surfaces: Rc<RefCell<HashMap<WorkspaceId, gtk::Widget>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let surfaces_for_select = surfaces.clone();
        let stack_for_select = stack.clone();

        let on_select = move |id: WorkspaceId| {
            if surfaces_for_select.borrow().contains_key(&id) {
                stack_for_select.set_visible_child_name(&id.to_string());
            }
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
        let sidebar = Sidebar::new(on_select, on_close);

        let pane_registry: Rc<RefCell<PaneRegistry>> = Rc::new(RefCell::new(PaneRegistry::default()));
        let callbacks = make_callbacks(focused_pane.clone());

        let split = adw::OverlaySplitView::builder()
            .min_sidebar_width(240.0)
            .max_sidebar_width(360.0)
            .show_sidebar(true)
            .sidebar(&sidebar.root)
            .content(&stack)
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

        Self {
            window,
            focused_pane,
            sidebar,
            stack,
            surfaces,
            pane_registry,
            callbacks,
            store,
            theme,
        }
    }

    pub fn show_status_when_empty(&self) {
        if self.surfaces.borrow().is_empty() {
            let status = adw::StatusPage::builder()
                .icon_name("utilities-terminal-symbolic")
                .title("flowmux")
                .description(
                    "No workspaces yet — open one with: flowmux workspace new --root .",
                )
                .build();
            self.stack.add_named(&status, Some("__empty"));
            self.stack.set_visible_child_name("__empty");
        }
    }

    pub fn render_workspace(&self, ws: &Workspace) {
        self.sidebar.upsert(ws);
        let mut surfaces = self.surfaces.borrow_mut();
        if surfaces.contains_key(&ws.id) {
            return;
        }
        let widget = self.build_workspace_widget(ws);
        let name = ws.id.to_string();
        self.stack.add_named(&widget, Some(&name));
        surfaces.insert(ws.id, widget);
        self.stack.set_visible_child_name(&name);
        drop(surfaces);
        self.focus_first_leaf_of(ws);
    }

    pub fn rerender_workspace(&self, ws: &Workspace) {
        self.sidebar.upsert(ws);
        let name = ws.id.to_string();
        let new_widget = self.build_workspace_widget(ws);
        let mut surfaces = self.surfaces.borrow_mut();
        if let Some(old) = surfaces.remove(&ws.id) {
            self.stack.remove(&old);
        }
        self.stack.add_named(&new_widget, Some(&name));
        surfaces.insert(ws.id, new_widget);
        self.stack.set_visible_child_name(&name);
        drop(surfaces);
        self.focus_first_leaf_of(ws);
    }

    /// Shared pane registry — exposed so the keybindings module can
    /// reach into VTE widgets for copy/paste actions on the GTK
    /// main thread without going through the bridge.
    pub fn pane_registry(&self) -> Rc<RefCell<PaneRegistry>> {
        self.pane_registry.clone()
    }

    /// Drop the workspace's stack page entirely (used when its last
    /// surface is closed).
    pub fn drop_workspace(&self, id: WorkspaceId) {
        self.sidebar.remove(id);
        let mut surfaces = self.surfaces.borrow_mut();
        if let Some(old) = surfaces.remove(&id) {
            self.stack.remove(&old);
        }
    }

    /// Find the first leaf in this workspace's first surface and
    /// grab keyboard focus on it. Deferred to the next idle so the
    /// widget tree is realized first.
    fn focus_first_leaf_of(&self, ws: &Workspace) {
        let leaf = ws.surfaces.first().and_then(|s| s.root_pane.first_leaf_id());
        let Some(leaf_id) = leaf else { return };
        let registry = self.pane_registry.clone();
        glib::idle_add_local_once(move || {
            let r = registry.borrow();
            if let Some(pane) = r.terminals.get(&leaf_id) {
                pane.widget.grab_focus();
            }
        });
    }

    fn build_workspace_widget(&self, ws: &Workspace) -> gtk::Widget {
        match ws.surfaces.first() {
            Some(s) => build_surface(
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
            GtkCommand::WorkspaceCreated { id, name: _, root: _, ack } => {
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
            GtkCommand::SplitFocused { pane, direction, ack } => {
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
                            if let Some(pane) = r.terminals.get(&new_pane) {
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
            GtkCommand::CloseFocused { pane, ack } => {
                match self.store.close_pane(pane).await {
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
                        let _ = ack.send(Ok(()));
                    }
                }
            }
            GtkCommand::FocusDirection { from, dir } => {
                self.focus_in_direction(from, dir);
            }
            GtkCommand::NewSurface => {
                let Some(ws_id) = self.store.active_or_first().await else {
                    return;
                };
                if self
                    .store
                    .add_terminal_surface(ws_id, std::env::current_dir().ok())
                    .await
                    .is_some()
                {
                    if let Some(ws) = self.store.get_workspace(ws_id).await {
                        self.rerender_workspace(&ws);
                    }
                }
            }
            GtkCommand::NewWorkspace { root } => {
                let id = self.store.create_workspace(None, root).await;
                if let Some(ws) = self.store.get_workspace(id).await {
                    self.render_workspace(&ws);
                }
            }
            GtkCommand::RemoveWorkspace { id, ack } => {
                self.store.remove_workspace(id).await;
                self.drop_workspace(id);
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
            GtkCommand::FocusWorkspaceDir { dir } => {
                let snap = self.store.snapshot().await;
                if snap.workspace_order.is_empty() {
                    return;
                }
                let active = snap
                    .active_workspace
                    .or_else(|| snap.workspace_order.first().copied());
                let Some(active) = active else { return };
                let cur = snap.workspace_order.iter().position(|x| *x == active).unwrap_or(0);
                let len = snap.workspace_order.len();
                let next = match dir {
                    WsNav::Next => (cur + 1) % len,
                    WsNav::Prev => (cur + len - 1) % len,
                };
                let target = snap.workspace_order[next];
                self.activate_workspace(target).await;
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
                let res = match registry.terminals.get(&pane) {
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
                match registry.browsers.get(&pane) {
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
        }
    }

    /// Move keyboard focus to the nearest pane in `dir` relative to
    /// the pane currently identified by `from`. Bbox computation is
    /// in the stack's coordinate space so split orientation doesn't
    /// matter.
    fn focus_in_direction(&self, from: PaneId, dir: FocusDir) {
        use gtk::graphene::Rect;

        let registry = self.pane_registry.borrow();
        let from_widget = match registry.terminals.get(&from) {
            Some(p) => p.widget.clone(),
            None => return,
        };
        let stack = &self.stack;
        let Some(from_bbox) = from_widget.compute_bounds(stack) else { return };
        let from_center = (
            from_bbox.x() + from_bbox.width() / 2.0,
            from_bbox.y() + from_bbox.height() / 2.0,
        );

        let mut best: Option<(PaneId, f32)> = None;
        for (id, pane) in registry.terminals.iter() {
            if *id == from {
                continue;
            }
            let Some(bbox) = pane.widget.compute_bounds(stack) else { continue };
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
                best = Some((*id, dist));
            }
        }
        let _ = Rect::new(0.0, 0.0, 0.0, 0.0); // ensure import used in non-tests path
        if let Some((id, _)) = best {
            if let Some(pane) = registry.terminals.get(&id) {
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
        self.store.set_active_workspace(Some(id)).await;
        if let Some(ws) = self.store.get_workspace(id).await {
            self.focus_first_leaf_of(&ws);
        }
    }

    pub async fn restore_from_store(&self) {
        let snap = self.store.snapshot().await;
        for ws in &snap.workspaces {
            self.render_workspace(ws);
        }
        if let Some(active) = snap.active_workspace {
            if self.surfaces.borrow().contains_key(&active) {
                self.stack.set_visible_child_name(&active.to_string());
            }
        }
    }
}

fn make_callbacks(focused: FocusedPane) -> PaneCallbacks {
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
        on_focus: Rc::new(RefCell::new(move |pane| {
            tracing::debug!(%pane, "pane focused");
            focused.set(Some(pane));
        })),
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
fn inject_cookies_into_webkit(
    cookies: &[flowmux_cookies::Cookie],
) -> Result<usize, String> {
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
    use gtk::glib::variant::ToVariant;

    // win.rename-workspace(<uuid>) — opens an adw::AlertDialog with an
    // Entry for the new name and OK/Cancel responses.
    let store_for_rename = store.clone();
    let bridge_for_rename = bridge.clone();
    let window_weak = window.downgrade();
    let rename = gio::ActionEntry::builder("rename-workspace")
        .parameter_type(Some(gtk::glib::VariantTy::STRING))
        .activate(move |_, _, param| {
            let Some(id_str) = param.and_then(|p| p.str().map(String::from)) else { return };
            let Ok(id) = id_str.parse::<WorkspaceId>() else { return };
            let store = store_for_rename.clone();
            let bridge = bridge_for_rename.clone();
            let window_weak = window_weak.clone();
            glib::MainContext::default().spawn_local(async move {
                let Some(ws) = store.get_workspace(id).await else { return };
                let Some(window) = window_weak.upgrade() else { return };
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
            let Some(id_str) = param.and_then(|p| p.str().map(String::from)) else { return };
            let Ok(id) = id_str.parse::<WorkspaceId>() else { return };
            let store = store_for_color.clone();
            let bridge = bridge_for_color.clone();
            let window_weak = window_weak2.clone();
            glib::MainContext::default().spawn_local(async move {
                let current = store.get_workspace(id).await.and_then(|w| w.color);
                let Some(window) = window_weak.upgrade() else { return };
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
            let Some(id_str) = param.and_then(|p| p.str().map(String::from)) else { return };
            let Ok(id) = id_str.parse::<WorkspaceId>() else { return };
            let bridge = bridge_for_close.clone();
            glib::MainContext::default().spawn_local(async move {
                let (tx, rx) = oneshot::channel();
                let _ = bridge.tx.send(GtkCommand::RemoveWorkspace { id, ack: tx }).await;
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
                        .send(GtkCommand::RenameWorkspace { id, name: new_name, ack: tx })
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
                    .send(GtkCommand::SetWorkspaceColor { id, color: hex, ack: tx })
                    .await;
            });
        },
    );
}

/// Spawn the GTK-side dispatch loop. Lives on the main context.
pub fn spawn_dispatch_loop(
    rx: async_channel::Receiver<GtkCommand>,
    controller: WindowController,
) {
    glib::MainContext::default().spawn_local(async move {
        while let Ok(cmd) = rx.recv().await {
            controller.dispatch(cmd).await;
        }
    });
}
