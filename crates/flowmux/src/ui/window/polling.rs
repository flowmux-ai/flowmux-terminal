// SPDX-License-Identifier: GPL-3.0-or-later
//! Background polling: terminal cwd, agent processes, launcher badge.
//!
//! Split out of `window.rs` (pure move; behavior unchanged).

use super::*;

fn agent_poll_delay(has_hook_presence: bool) -> Duration {
    if has_hook_presence {
        Duration::from_secs(10)
    } else {
        Duration::from_secs(2)
    }
}

fn schedule_agent_process_poll(controller: WindowController, delay: Duration) {
    glib::timeout_add_local_once(delay, move || {
        glib::MainContext::default().spawn_local(async move {
            controller.poll_agent_processes().await;
            let delay = agent_poll_delay(controller.store.has_hook_agent_presence().await);
            schedule_agent_process_poll(controller, delay);
        });
    });
}

impl WindowController {
    pub(super) async fn update_terminal_cwd(
        &self,
        pane: PaneId,
        surface: SurfaceId,
        cwd: std::path::PathBuf,
    ) -> Option<WorkspaceId> {
        let ws_id = self.store.update_surface_cwd(pane, surface, cwd).await?;
        if let Some(title) = self.store.surface_title(pane, surface).await {
            self.pane_registry
                .borrow()
                .set_surface_title(surface, &title);
        }
        Some(ws_id)
    }
    pub(super) fn flush_terminal_cwds_blocking(&self) {
        let cwd_entries = self.pane_registry.borrow().terminal_cwds();
        for (pane, surface, cwd) in cwd_entries {
            let _ = self.store.update_surface_cwd_blocking(pane, surface, cwd);
        }
    }

    pub(super) fn flush_terminal_scrollback_blocking(&self) {
        if !self.options.borrow().restore_terminal_scrollback {
            return;
        }
        let snapshots = self.pane_registry.borrow().terminal_scrollback_snapshots();
        for (pane, surface, text) in snapshots {
            let _ = self
                .store
                .update_surface_scrollback_blocking(pane, surface, text);
        }
    }

    pub(super) fn flush_editor_sessions_blocking(&self) {
        let snapshots = self.pane_registry.borrow().editor_session_snapshots();
        for (pane, surface, session) in snapshots {
            let _ = self
                .store
                .update_editor_session_blocking(pane, surface, session);
        }
    }

    pub(super) fn install_editor_session_persistence(&self) {
        let controller = self.clone();
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let snapshots = controller.pane_registry.borrow().editor_session_snapshots();
            let controller = controller.clone();
            glib::MainContext::default().spawn_local(async move {
                let mut updated = false;
                for (pane, surface, session) in snapshots {
                    if controller
                        .store
                        .update_editor_session(pane, surface, session)
                        .await
                        .is_some()
                    {
                        updated = true;
                        if let Some(title) = controller.store.surface_title(pane, surface).await {
                            controller
                                .pane_registry
                                .borrow()
                                .set_surface_title(surface, &title);
                        }
                    }
                }
                if updated {
                    controller.refresh_window_title().await;
                }
            });
            glib::ControlFlow::Continue
        });
    }

    /// Capture terminal history periodically so a crash or power loss loses at
    /// most one short interval. StateStore de-duplicates identical snapshots
    /// and debounces disk writes.
    pub(super) fn install_scrollback_persistence(&self) {
        let controller = self.clone();
        glib::timeout_add_local(Duration::from_secs(15), move || {
            if controller.options.borrow().restore_terminal_scrollback {
                let snapshots = controller
                    .pane_registry
                    .borrow()
                    .dirty_terminal_scrollback_snapshots();
                let controller = controller.clone();
                glib::MainContext::default().spawn_local(async move {
                    for (pane, surface, text) in snapshots {
                        let _ = controller
                            .store
                            .update_surface_scrollback(pane, surface, text)
                            .await;
                    }
                });
            }
            glib::ControlFlow::Continue
        });
    }
    /// Relying only on VTE OSC 7 (`current-directory-uri::notify`) misses shells
    /// without OSC 7 integration, such as Ubuntu's default bash spawned by
    /// flowmux; after `cd`, no notify ever arrives and the tab name stays stale.
    /// Poll once per second to reuse TerminalPane::current_dir()'s
    /// `/proc/<pid>/cwd` fallback. The OSC 7 event path remains immediate, and
    /// polling is a safety net for OSC-7-naive shells.
    pub(super) fn install_cwd_polling_fallback(&self) {
        let controller = self.clone();
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let controller = controller.clone();
            glib::MainContext::default().spawn_local(async move {
                controller.poll_terminal_cwds().await;
            });
            glib::ControlFlow::Continue
        });
    }
    /// Agent Bar presence is driven primarily by process truth. Poll every 2s
    /// when process detection is the only signal, then relax to 10s while a
    /// lifecycle-hook presence is healthy.
    pub(super) fn install_agent_process_polling(&self) {
        schedule_agent_process_poll(self.clone(), Duration::from_secs(2));
    }
    pub(super) async fn poll_agent_processes(&self) {
        let pids = self.pane_registry.borrow().terminal_agent_pids();
        if pids.is_empty() {
            return;
        }
        let generation = self.agent_poll_generation.get().wrapping_add(1);
        self.agent_poll_generation.set(generation);
        let started = Instant::now();
        let detected = match gtk::gio::spawn_blocking(move || {
            pids.into_iter()
                .map(|(surface, pid)| (surface, flowmux_procmon::agent_name_in_tree(pid)))
                .collect::<Vec<_>>()
        })
        .await
        {
            Ok(detected) => detected,
            Err(_) => {
                tracing::warn!(generation, "agent process poll worker panicked");
                return;
            }
        };
        if self.agent_poll_generation.get() != generation {
            tracing::debug!(generation, "discarding stale agent process poll");
            return;
        }
        tracing::debug!(
            generation,
            surfaces = detected.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "agent process poll completed"
        );
        let changed = self.store.reconcile_process_agents(&detected).await;
        for (workspace, _) in changed {
            self.sync_workspace_agent_status(workspace).await;
        }
    }
    pub(super) async fn poll_terminal_cwds(&self) {
        let inputs = self.pane_registry.borrow().terminal_cwd_poll_inputs();
        if inputs.is_empty() {
            return;
        }
        let generation = self.cwd_poll_generation.get().wrapping_add(1);
        self.cwd_poll_generation.set(generation);
        let started = Instant::now();
        let results = match gtk::gio::spawn_blocking(move || {
            inputs
                .into_iter()
                .map(|(pane, surface, announced, pid)| {
                    let cwd = announced.or_else(|| {
                        pid.and_then(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok())
                    });
                    (pane, surface, cwd)
                })
                .collect::<Vec<_>>()
        })
        .await
        {
            Ok(results) => results,
            Err(_) => {
                tracing::warn!(generation, "terminal cwd poll worker panicked");
                return;
            }
        };
        if self.cwd_poll_generation.get() != generation {
            tracing::debug!(generation, "discarding stale terminal cwd poll");
            return;
        }
        let cwd_entries = self
            .pane_registry
            .borrow()
            .apply_terminal_cwd_poll_results(results);
        tracing::debug!(
            generation,
            changed = cwd_entries.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "terminal cwd poll completed"
        );
        let mut changed_workspaces: std::collections::HashSet<WorkspaceId> =
            std::collections::HashSet::new();
        for (pane, surface, cwd) in cwd_entries {
            // terminal_cwds() already diffs against each pane's last polled cwd,
            // so this list holds only panes whose cwd actually changed — idle
            // terminals never reach the daemon state mutex below.
            // When the folder name changes, update the store and tab label immediately.
            if let Some(ws_id) = self.store.update_surface_cwd(pane, surface, cwd).await {
                if let Some(title) = self.store.surface_title(pane, surface).await {
                    self.pane_registry
                        .borrow()
                        .set_surface_title(surface, &title);
                }
                changed_workspaces.insert(ws_id);
            }
        }
        if !changed_workspaces.is_empty() {
            self.refresh_window_title().await;
            // For shells without OSC 7, this polling is the only cwd-change
            // signal. Side-panel workspace names/subtitles are updated only via
            // sync_workspace_label, so polling must use the same path to follow cd.
            for ws_id in changed_workspaces {
                self.sync_workspace_label(ws_id).await;
            }
            // OSC-7-less shells reach the file browser only through polling, so
            // mirror the OSC 7 path (TerminalCwdChanged) and re-root the panel on
            // the focused pane's new cwd.
            self.refresh_file_browser_from_focus().await;
        }
    }
    /// Republish the unread-notification count to the dock via the
    /// Unity LauncherEntry signal. `org.gtk.Notifications.RemoveNotification`
    /// clears the GNOME message-tray dot, but Ubuntu Dock's *number
    /// circle* on the launcher icon is driven exclusively by this
    /// Unity-vintage signal — without re-emitting after each
    /// mark-read sweep, the circle stays pinned at the last published
    /// value. An in-flight publish task acts as the single publisher;
    /// further refreshes set `badge_dirty`, and the task re-reads
    /// `unread_count()` after each `await` so bursty pushes/sweeps
    /// always converge to the freshest value.
    pub(super) fn refresh_launcher_badge(&self) {
        let unread = self
            .notifications
            .entries()
            .into_iter()
            .filter(|entry| !entry.read)
            .collect::<Vec<_>>();
        let workspaces = unread
            .iter()
            .filter_map(|entry| entry.workspace)
            .collect::<HashSet<_>>();
        let panes = unread
            .iter()
            .filter_map(|entry| entry.pane)
            .collect::<HashSet<_>>();
        self.sidebar.set_notification_workspaces(&workspaces);
        self.pane_registry.borrow().set_notification_panes(&panes);
        self.notifications.refresh_launcher_badge();
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_polling_relaxes_for_hook_backed_sessions() {
        assert_eq!(agent_poll_delay(false), Duration::from_secs(2));
        assert_eq!(agent_poll_delay(true), Duration::from_secs(10));
    }
}
