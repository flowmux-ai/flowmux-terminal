// SPDX-License-Identifier: GPL-3.0-or-later
//! Workspace lifecycle command dispatch on the GTK main thread.

use super::*;

impl WindowController {
    pub(super) async fn dispatch_workspace_command(&self, cmd: GtkCommand) {
        match cmd {
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
                let closing_surfaces = self.pane_registry.borrow().surface_ids_in_workspace(id);
                if self.store.remove_workspace(id).await {
                    forget_saved_agent_sessions(&closing_surfaces);
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
            other => {
                unreachable!("workspace router got a non-workspace command: {other:?}")
            }
        }
    }
}
