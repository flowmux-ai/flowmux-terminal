// SPDX-License-Identifier: GPL-3.0-or-later
//! Agent bar + notification attention tracking and rendering.
//!
//! Split out of `window.rs` (pure move; behavior unchanged).

use super::*;

impl WindowController {
    pub(super) async fn refresh_agent_bar(&self) {
        if !self.options.borrow().agent_bar_enabled {
            self.agent_bar.render(
                &flowmux_core::AgentBarModel {
                    visible: false,
                    items: Vec::new(),
                },
                &HashSet::new(),
                None,
            );
            return;
        }

        let snap = self.store.snapshot().await;
        let mut ordered = Vec::new();
        for ws_id in &snap.workspace_order {
            if let Some(ws) = snap.workspaces.iter().find(|ws| ws.id == *ws_id) {
                ordered.push(ws);
            }
        }
        for ws in &snap.workspaces {
            if !snap.workspace_order.contains(&ws.id) {
                ordered.push(ws);
            }
        }

        let model = flowmux_core::collect_agent_bar_model(ordered);
        let live_surfaces: HashSet<SurfaceId> =
            model.items.iter().map(|item| item.surface).collect();
        let focused_surface = self
            .focused_pane
            .get()
            .and_then(|pane| self.pane_registry.borrow().active_surface(pane))
            .filter(|surface| live_surfaces.contains(surface));
        let attentions = {
            let mut attentions = self.agent_bar_attentions.borrow_mut();
            attentions.retain(|surface| live_surfaces.contains(surface));
            attentions.clone()
        };
        self.agent_bar.render(&model, &attentions, focused_surface);
    }
    pub(super) async fn sync_workspace_agent_status(
        &self,
        workspace: WorkspaceId,
        status: Option<AgentStatus>,
    ) {
        self.sidebar.set_agent_status(workspace, status);
        self.sync_workspace_label(workspace).await;
        self.refresh_agent_bar().await;
    }
    pub(super) async fn sync_workspace_agent_status_from_store(&self, workspace: WorkspaceId) {
        let status = self.store.workspace_agent_status(workspace).await;
        self.sync_workspace_agent_status(workspace, status).await;
    }
    pub(super) fn mark_agent_bar_attention(&self, surface: SurfaceId) {
        if self.agent_bar_attentions.borrow_mut().insert(surface) {
            self.agent_bar.mark_attention(surface);
        }
    }
    pub(super) fn clear_agent_bar_attention(&self, surface: SurfaceId) {
        if self.agent_bar_attentions.borrow_mut().remove(&surface) {
            self.agent_bar.clear_attention(surface);
        }
    }
    pub(super) fn clear_agent_bar_attentions<I>(&self, surfaces: I)
    where
        I: IntoIterator<Item = SurfaceId>,
    {
        for surface in surfaces {
            self.clear_agent_bar_attention(surface);
        }
    }
    pub(super) fn acknowledge_workspace_notifications(&self, workspace: WorkspaceId) {
        let surfaces = self.notifications.unread_surfaces_for_workspace(workspace);
        let to_close = self.notifications.mark_workspace_read(workspace);
        self.clear_agent_bar_attentions(surfaces);
        self.sidebar.clear_attention(workspace);
        if !to_close.is_empty() {
            self.close_desktop_notifications(to_close);
        }
        self.refresh_launcher_badge();
    }
    pub(super) fn acknowledge_source_notifications(
        &self,
        workspace: Option<WorkspaceId>,
        pane: Option<PaneId>,
        surface: Option<SurfaceId>,
    ) {
        let unread_before = self.notifications.unread_count();
        let to_close = self
            .notifications
            .mark_source_read(workspace, pane, surface);
        let changed = self.notifications.unread_count() != unread_before;
        if let Some(surface) = surface {
            self.clear_agent_bar_attention(surface);
        }
        if let Some(workspace) = workspace {
            if !self.notifications.has_unread_workspace(workspace) {
                self.sidebar.clear_attention(workspace);
            }
        }
        if !to_close.is_empty() {
            self.close_desktop_notifications(to_close);
        }
        if changed {
            self.refresh_launcher_badge();
        }
    }
    pub(super) fn clear_notification_attention_for_entry(&self, entry: &NotificationEntry) {
        if let Some(surface) = entry.surface {
            if !self.notifications.has_unread_surface(surface) {
                self.clear_agent_bar_attention(surface);
            }
        }
        if let Some(workspace) = entry.workspace {
            if !self.notifications.has_unread_workspace(workspace) {
                self.sidebar.clear_attention(workspace);
            }
        }
    }
    pub(super) fn clear_all_notification_attention_for_entries(&self, entries: &[NotificationEntry]) {
        for entry in entries {
            if let Some(surface) = entry.surface {
                self.clear_agent_bar_attention(surface);
            }
            if let Some(workspace) = entry.workspace {
                self.sidebar.clear_attention(workspace);
            }
        }
    }
    pub(super) async fn refresh_agent_screen_status(&self, surface: SurfaceId, title: Option<String>) {
        let (screen, title) = {
            let registry = self.pane_registry.borrow();
            let screen = registry
                .terminals
                .get(&surface)
                .and_then(|terminal| terminal.screen_text());
            let title = title.or_else(|| registry.surface_title_text(surface));
            (screen, title)
        };
        if screen.is_none() && title.is_none() {
            return;
        }
        if let Some((ws_id, status)) = self
            .store
            .report_agent_screen_signals(surface, screen.as_deref(), title.as_deref())
            .await
        {
            self.sync_workspace_agent_status(ws_id, status).await;
        }
    }
    pub(super) async fn refresh_all_agent_screen_statuses(&self) {
        let surfaces: Vec<SurfaceId> = self
            .pane_registry
            .borrow()
            .terminals
            .keys()
            .copied()
            .collect();
        for surface in surfaces {
            self.refresh_agent_screen_status(surface, None).await;
        }
        self.refresh_agent_bar().await;
    }
    pub(super) async fn open_agent_bar_item(&self, workspace: WorkspaceId, pane: PaneId, surface: SurfaceId) {
        self.activate_workspace(workspace).await;
        if self.pane_registry.borrow().active_surface(pane) != Some(surface) {
            self.activate_surface_now(pane, surface).await;
        }
        self.acknowledge_source_notifications(Some(workspace), Some(pane), Some(surface));
        self.window.present();
        self.focus_pane(pane);
    }
    pub(super) async fn open_notification_id(&self, id: flowmux_core::NotificationId) -> bool {
        let Some(entry) = self.notifications.find(id) else {
            tracing::debug!(%id, "open notification: id not found");
            return false;
        };

        let desktop_id = entry.desktop_id.clone();
        let changed = self.notifications.mark_read(id);
        if changed {
            if let Some(desktop_id) = desktop_id {
                self.close_desktop_notifications(vec![desktop_id]);
            }
            self.clear_notification_attention_for_entry(&entry);
            self.refresh_launcher_badge();
        }

        if let Some(ws_id) = entry.workspace {
            self.activate_workspace(ws_id).await;
        }

        if let Some(pane) = entry.pane {
            if let Some(source_surface) = entry.surface {
                let active = self.pane_registry.borrow().active_surface(pane);
                if active != Some(source_surface) {
                    self.activate_surface_now(pane, source_surface).await;
                }
            }

            self.focus_pane(pane);
        }

        self.window.present();
        true
    }
    pub(super) fn close_desktop_notifications(&self, desktop_ids: Vec<String>) {
        if desktop_ids.is_empty() {
            return;
        }
        let notifier_cell = self.notifier.clone();
        let handle = self.tokio_handle.clone();
        glib::MainContext::default().spawn_local(async move {
            // `zbus` (tokio feature) needs an active Tokio runtime
            // context for every `await`. The GTK main thread is not a
            // Tokio worker, so without this guard the first `.await`
            // panics with "no reactor running"; the panic is swallowed
            // by GLib's task wrapper and the toast never closes,
            // leaving the message tray inflated and the dock badge
            // stuck. The guard lives for the entire async block,
            // covering connect + every `close().await`.
            let _enter = handle.as_ref().map(|h| h.enter());
            let Some(notifier) = ensure_desktop_notifier(&notifier_cell).await else {
                return;
            };
            for did in desktop_ids {
                if let Err(e) = notifier.close(&did).await {
                    tracing::debug!(error = %e, %did, "close notification failed");
                }
            }
        });
    }
}
