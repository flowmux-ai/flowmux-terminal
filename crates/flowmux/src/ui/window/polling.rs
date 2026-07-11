// SPDX-License-Identifier: GPL-3.0-or-later
//! Background polling: terminal cwd, agent processes, launcher badge.
//!
//! Split out of `window.rs` (pure move; behavior unchanged).

use super::*;

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
    /// Agent Bar presence is driven primarily by *process truth*: every 2s,
    /// resolve which AI agent (if any) is running in each terminal pane's
    /// process subtree and reconcile the store. This shows an agent the moment
    /// it launches — no wait for a hook, an OSC title, or recognizable TUI
    /// text — and drops it when it exits. Screen/title events still refine the
    /// working/idle status on top of this. Matches the daemon's 2s liveness
    /// sweep cadence.
    pub(super) fn install_agent_process_polling(&self) {
        let controller = self.clone();
        glib::timeout_add_local(Duration::from_secs(2), move || {
            let controller = controller.clone();
            glib::MainContext::default().spawn_local(async move {
                controller.poll_agent_processes().await;
            });
            glib::ControlFlow::Continue
        });
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
        for (workspace, status) in changed {
            self.sync_workspace_agent_status(workspace, status).await;
        }
    }
    pub(super) async fn poll_terminal_cwds(&self) {
        let inputs = self.pane_registry.borrow().terminal_cwd_poll_inputs();
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
        if self.badge_publisher_busy.get() {
            // Another spawn_local is already publishing. Just signal it
            // to republish once it finishes its current await — the
            // store will be re-read after the in-flight publish so the
            // latest count wins without us starting a racing task.
            self.badge_dirty.set(true);
            return;
        }
        self.badge_publisher_busy.set(true);
        self.badge_dirty.set(false);
        let notifier_cell = self.notifier.clone();
        let store = self.notifications.clone();
        let busy = self.badge_publisher_busy.clone();
        let dirty = self.badge_dirty.clone();
        let handle = self.tokio_handle.clone();
        glib::MainContext::default().spawn_local(async move {
            // zbus's tokio executor needs an active runtime context
            // across every `await`. Without it, `update_launcher_count`
            // panics inside `spawn_local`, GLib swallows the panic, and
            // the dock badge never updates.
            let _enter = handle.as_ref().map(|h| h.enter());
            let app_uri = format!(
                "application://{}.desktop",
                flowmux_notify::DESKTOP_FILE_BASENAME
            );
            loop {
                let Some(notifier) = ensure_desktop_notifier(&notifier_cell).await else {
                    dirty.set(false);
                    busy.set(false);
                    return;
                };
                let count = store.unread_count() as i64;
                if let Err(e) = notifier.update_launcher_count(&app_uri, count).await {
                    tracing::debug!(error = %e, count, "launcher entry update failed");
                }
                if !dirty.get() {
                    busy.set(false);
                    return;
                }
                dirty.set(false);
            }
        });
    }
}
