// SPDX-License-Identifier: GPL-3.0-or-later
//! Pane and surface lifecycle command dispatch on the GTK main thread.

use super::*;

const OPEN_TIG_SHELL_COMMAND: &str = "tig\r";

impl WindowController {
    pub(super) async fn dispatch_pane_command(&self, cmd: GtkCommand) {
        let zoomed = self.zoomed_pane();
        let clears_zoom = match &cmd {
            GtkCommand::PaneSplitApplied { .. }
            | GtkCommand::SplitFocused { .. }
            | GtkCommand::CloseFocused { .. }
            | GtkCommand::FocusDirection { .. }
            | GtkCommand::CloseSurface { .. }
            | GtkCommand::TearOffSurface { .. }
            | GtkCommand::MoveSurfaceToPane { .. }
            | GtkCommand::MoveSurfaceToWorkspace { .. }
            | GtkCommand::SplitSurfaceIntoPane { .. }
            | GtkCommand::ResizePane { .. } => zoomed.is_some(),
            GtkCommand::FocusPane { pane, .. } => zoomed.is_some_and(|zoom| zoom != *pane),
            _ => false,
        };
        if clears_zoom {
            self.clear_pane_zoom();
        }

        match cmd {
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
                            } else if let Some(editor) = r.active_editor(new_pane) {
                                editor.grab_focus();
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
                let closing_surfaces = self
                    .pane_registry
                    .borrow()
                    .surface_tabs
                    .get(&pane)
                    .map(|tabs| tabs.iter().map(|(surface, _)| *surface).collect::<Vec<_>>())
                    .unwrap_or_default();
                if !self.confirm_dirty_surfaces(&closing_surfaces).await {
                    let _ = ack.send(Ok(()));
                    return;
                }
                let outcome = self.store.close_pane(pane).await;
                if outcome.is_some() {
                    forget_saved_agent_sessions(&closing_surfaces);
                }
                match outcome {
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
            GtkCommand::OpenTig { pane } => {
                let cwd = {
                    let registry = self.pane_registry.borrow();
                    registry
                        .active_terminal(pane)
                        .and_then(|terminal| terminal.current_dir())
                };
                if let Some((workspace, surface)) =
                    self.store.add_terminal_surface_to_pane(pane, cwd).await
                {
                    self.attach_or_rerender_surface(workspace, pane, surface)
                        .await;
                    let terminal = self.pane_registry.borrow().terminals.get(&surface).cloned();
                    if let Some(terminal) = terminal {
                        glib::timeout_add_local_once(Duration::from_millis(250), move || {
                            if let Err(error) =
                                terminal.write_input(OPEN_TIG_SHELL_COMMAND.as_bytes())
                            {
                                tracing::warn!(%pane, %surface, %error, "failed to start tig");
                            }
                        });
                    } else {
                        tracing::warn!(%pane, %surface, "tig tab was not attached");
                    }
                }
            }
            GtkCommand::CreateSurface {
                workspace,
                cwd,
                shell,
                ack,
            } => {
                let focused = self.focused_pane.get();
                let pane = match focused {
                    Some(pane) if self.store.workspace_for_pane(pane).await == Some(workspace) => {
                        Some(pane)
                    }
                    _ => self
                        .store
                        .get_workspace(workspace)
                        .await
                        .and_then(|workspace| {
                            workspace.surfaces.first()?.root_pane.first_leaf_id()
                        }),
                };
                let result = match pane {
                    Some(pane) => match self
                        .store
                        .add_terminal_surface_to_pane_with_shell(pane, cwd, shell)
                        .await
                    {
                        Some((ws_id, surface)) => {
                            self.attach_or_rerender_surface(ws_id, pane, surface).await;
                            Ok((pane, surface))
                        }
                        None => Err(format!("workspace has no tab-capable pane: {workspace}")),
                    },
                    None => Err(format!("workspace not found: {workspace}")),
                };
                let _ = ack.send(result);
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
                self.refresh_file_browser_from_focus().await;
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
                    } else if let Some(editor) = r.editors.get(&surface) {
                        editor.grab_focus();
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
                if !self.confirm_dirty_surfaces(&[surface]).await {
                    let _ = ack.send(Ok(()));
                    return;
                }
                let outcome = self.store.close_surface(pane, surface).await;
                if outcome.is_some() {
                    forget_saved_agent_sessions(&[surface]);
                }
                match outcome {
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
                self.refresh_worktrees_from_focus().await;
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
                self.refresh_worktrees_from_focus().await;
            }
            GtkCommand::PaneSendKeys { pane, keys, ack } => {
                let registry = self.pane_registry.borrow();
                let res = match registry.active_terminal(pane) {
                    Some(p) => p.write_input(keys.as_bytes()).map_err(|e| e.to_string()),
                    None => Err(format!("pane not found: {pane}")),
                };
                let _ = ack.send(res);
            }
            GtkCommand::TerminalTimelineMark { pane, surface, ack } => {
                let registry = self.pane_registry.borrow();
                let terminal = surface
                    .and_then(|id| registry.terminals.get(&id))
                    .or_else(|| pane.and_then(|id| registry.active_terminal(id)));
                let res = match terminal {
                    Some(terminal) => {
                        terminal.capture_timeline_mark();
                        Ok(())
                    }
                    None => Err(format!(
                        "terminal surface not found: pane={pane:?} surface={surface:?}"
                    )),
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
                    self.refresh_worktrees_from_focus().await;
                } else {
                    let _ = ack.send(Err(format!("pane not found: {pane}")));
                }
            }
            GtkCommand::TogglePaneZoom { pane } => {
                self.toggle_pane_zoom(pane);
            }
            GtkCommand::ResizePane { pane, ratio, ack } => {
                let res = self.resize_pane_ratio(pane, ratio).await;
                let _ = ack.send(res);
            }
            other => unreachable!("non-pane command routed to pane dispatcher: {other:?}"),
        }
    }
}
