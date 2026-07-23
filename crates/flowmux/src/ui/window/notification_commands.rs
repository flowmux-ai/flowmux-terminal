// SPDX-License-Identifier: GPL-3.0-or-later
//! Notification and agent-status command dispatch on the GTK main thread.

use super::*;

impl WindowController {
    pub(super) async fn dispatch_notification_command(&self, cmd: GtkCommand) {
        match cmd {
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
            GtkCommand::OpenAgentBarItem {
                workspace,
                pane,
                surface,
            } => {
                self.open_agent_bar_item(workspace, pane, surface).await;
            }
            other => {
                unreachable!("notification router got a non-notification command: {other:?}")
            }
        }
    }
}
