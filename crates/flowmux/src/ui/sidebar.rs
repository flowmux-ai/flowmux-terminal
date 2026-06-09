// SPDX-License-Identifier: GPL-3.0-or-later
//! Workspace sidebar (flowmux's vertical-tabs left panel).
//!
//! Layout:
//!
//! ```text
//! +----------------+
//! | [+] [bell]     |  toolbar
//! +----------------+
//! | • workspace 1  |
//! | • workspace 2  |  scrollable workspace list
//! | • workspace 3  |
//! +----------------+
//! ```
//!
//! The toolbar's `+` adds a workspace (Ctrl+N equivalent) and
//! the bell shows an in-process notification transcript. The list
//! rows expose hover-X close, color bar, right-click menu (rename /
//! recolor / close).

use crate::bridge::{Bridge, GtkCommand};
use crate::notifications::{NotificationEntry, NotificationStore};
use flowmux_core::{AgentActivity, NotificationLevel, PrState, Workspace, WorkspaceId};
use gtk::prelude::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

type RowsCell = Rc<RefCell<Vec<(WorkspaceId, gtk::ListBoxRow)>>>;

#[derive(Clone)]
pub struct Sidebar {
    pub root: gtk::Box,
    pub list: gtk::ListBox,
    rows: Rc<RefCell<Vec<(WorkspaceId, gtk::ListBoxRow)>>>,
    on_close: Rc<dyn Fn(WorkspaceId)>,
    bell_button: gtk::MenuButton,
    bell_popover: gtk::Popover,
    notifications: NotificationStore,
    attentions: Rc<RefCell<HashSet<WorkspaceId>>>,
    /// Workspaces with an agent currently `Running`; their row carries
    /// the `flowmux-agent-running` class so the color bar breathes. The
    /// class lives on the reused `ListBoxRow`, so it survives a row
    /// rebuild; this set just keeps the membership idempotent.
    agent_running: Rc<RefCell<HashSet<WorkspaceId>>>,
    bridge: Bridge,
    /// Last computed subtitle lines per workspace, capped at 3 lines.
    /// Kept so paths that do not know subtitle data, such as rename or color
    /// changes, can redraw a row without losing its subtitles. WindowController
    /// updates this via [`Sidebar::upsert_with_subtitles`] after
    /// sync_workspace_label.
    subtitle_cache: Rc<RefCell<HashMap<WorkspaceId, Vec<String>>>>,
}

impl Sidebar {
    pub fn new<S, C>(
        on_select: S,
        on_close: C,
        bridge: Bridge,
        notifications: NotificationStore,
    ) -> Self
    where
        S: Fn(WorkspaceId) + 'static,
        C: Fn(WorkspaceId) + 'static,
    {
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        // A single click activates a row (= switches workspace). With
        // this on we listen on `row-activated` instead of
        // `row-selected`; the latter also fires when GTK moves focus
        // by Tab traversal, which made plain Tab unintentionally
        // jump between workspaces.
        list.set_activate_on_single_click(true);
        list.add_css_class("navigation-sidebar");

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroll.set_vexpand(true);
        scroll.set_child(Some(&list));

        let rows: Rc<RefCell<Vec<(WorkspaceId, gtk::ListBoxRow)>>> =
            Rc::new(RefCell::new(Vec::new()));

        let attentions: Rc<RefCell<HashSet<WorkspaceId>>> = Rc::new(RefCell::new(HashSet::new()));
        let rows_for_cb = rows.clone();
        let attentions_for_cb = attentions.clone();
        // `row-activated` fires only on click / Enter, never on Tab
        // focus traversal. That keeps Tab usable inside the terminal
        // and stops focus from silently flipping workspaces.
        list.connect_row_activated(move |_, row| {
            if let Some((id, list_row)) =
                rows_for_cb.borrow().iter().find(|(_, r)| r == row).cloned()
            {
                if attentions_for_cb.borrow_mut().remove(&id) {
                    list_row.remove_css_class("flowmux-attention");
                }
                on_select(id);
            }
        });

        let on_close: Rc<dyn Fn(WorkspaceId)> = Rc::new(on_close);

        // ---- Top toolbar ----
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        toolbar.set_margin_top(6);
        toolbar.set_margin_bottom(6);
        toolbar.set_margin_start(8);
        toolbar.set_margin_end(8);

        let new_btn = gtk::Button::from_icon_name("list-add-symbolic");
        new_btn.add_css_class("flat");
        new_btn.set_tooltip_text(Some("New workspace (Ctrl+N)"));
        let bridge_for_new = bridge.clone();
        new_btn.connect_clicked(move |_| {
            let bridge = bridge_for_new.clone();
            gtk::glib::MainContext::default().spawn_local(async move {
                let root =
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
                let _ = bridge.tx.send(GtkCommand::NewWorkspace { root }).await;
            });
        });
        toolbar.append(&new_btn);

        let bell_button = gtk::MenuButton::new();
        bell_button.set_icon_name("notifications-symbolic");
        bell_button.add_css_class("flat");
        bell_button.set_tooltip_text(Some("Notifications"));
        bell_button.set_hexpand(true);
        bell_button.set_halign(gtk::Align::End);
        let bell_popover = gtk::Popover::new();
        bell_popover.set_size_request(320, -1);
        bell_button.set_popover(Some(&bell_popover));

        let store_for_show = notifications.clone();
        let bridge_for_rows = bridge.clone();
        let bridge_for_close = bridge.clone();
        let popover_for_show = bell_popover.clone();
        let bell_for_show = bell_button.clone();
        bell_popover.connect_show(move |_| {
            // Render BEFORE the ack sweep so unread entries still look
            // unread (full opacity, accent on AttentionNeeded titles)
            // the moment the popover appears. The previous order
            // marked everything read first, so the user saw every row
            // dimmed even on first open — the exact symptom this
            // guards against. On the *next* open, those entries are
            // legitimately read and dim correctly.
            popover_for_show.set_child(Some(&render_notification_list(
                &store_for_show,
                bridge_for_rows.clone(),
                popover_for_show.clone(),
            )));
            bell_for_show.remove_css_class("accent");

            // Now ack: flip the store, withdraw the matching desktop
            // toasts so the system tray / dock badge converge.
            // `desktop_ids` may be empty when no toast had a desktop
            // id yet (IPC race) — we still dispatch the refresh so
            // any pending badge state catches up.
            let desktop_ids = store_for_show.mark_all_unread_read();
            let bridge = bridge_for_close.clone();
            gtk::glib::MainContext::default().spawn_local(async move {
                if !desktop_ids.is_empty() {
                    let _ = bridge
                        .tx
                        .send(GtkCommand::CloseDesktopNotifications { desktop_ids })
                        .await;
                }
                let _ = bridge.tx.send(GtkCommand::RefreshLauncherBadge).await;
            });
        });
        toolbar.append(&bell_button);

        // ---- Bottom footer: small left options button ----
        // Click dispatches ShowOptionsDialog through the bridge so the window
        // dispatcher can present the modal dialog.
        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        footer.set_margin_top(2);
        footer.set_margin_bottom(4);
        footer.set_margin_start(4);
        footer.set_margin_end(4);
        let options_btn = gtk::Button::from_icon_name("emblem-system-symbolic");
        options_btn.add_css_class("flat");
        options_btn.set_tooltip_text(Some("Options"));
        options_btn.set_focus_on_click(false);
        // Keep it small and dimmed instead of matching the sidebar's more
        // prominent buttons.
        options_btn.add_css_class("flowmux-sidebar-options");
        options_btn.set_halign(gtk::Align::Start);
        let bridge_for_options = bridge.clone();
        options_btn.connect_clicked(move |_| {
            let bridge = bridge_for_options.clone();
            gtk::glib::MainContext::default().spawn_local(async move {
                let _ = bridge.tx.send(GtkCommand::ShowOptionsDialog).await;
            });
        });
        footer.append(&options_btn);

        // ---- Outer vbox: toolbar + list + footer ----
        let root_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root_box.append(&toolbar);
        root_box.append(&scroll);
        root_box.append(&footer);

        Self {
            root: root_box,
            list,
            rows,
            on_close,
            bell_button,
            bell_popover,
            notifications,
            attentions,
            agent_running: Rc::new(RefCell::new(HashSet::new())),
            bridge,
            subtitle_cache: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    /// Add or redraw a workspace row using cached subtitles. Used by paths
    /// that do not know subtitle data, such as rename or color changes; the
    /// subtitles stay at the last value supplied to [`Self::upsert_with_subtitles`].
    pub fn upsert(&self, ws: &Workspace) {
        let cached = self.subtitle_cache.borrow().get(&ws.id).cloned();
        let lines = cached.unwrap_or_default();
        self.upsert_inner(ws, &lines);
    }

    /// Add or update a workspace row while supplying subtitle lines. Lines are
    /// capped at 3, and an empty list removes the subtitle area. Repeated calls
    /// for the same workspace replace the row content with the new subtitles.
    pub fn upsert_with_subtitles(&self, ws: &Workspace, lines: &[String]) {
        let trimmed: Vec<String> = lines.iter().take(3).cloned().collect();
        self.subtitle_cache
            .borrow_mut()
            .insert(ws.id, trimmed.clone());
        self.upsert_inner(ws, &trimmed);
    }

    fn upsert_inner(&self, ws: &Workspace, subtitles: &[String]) {
        let mut rows = self.rows.borrow_mut();
        if let Some((_, row)) = rows.iter().find(|(id, _)| *id == ws.id).cloned() {
            row.set_child(Some(&row_widget(
                ws,
                subtitles,
                self.on_close.clone(),
                self.bridge.clone(),
            )));
            return;
        }
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&row_widget(
            ws,
            subtitles,
            self.on_close.clone(),
            self.bridge.clone(),
        )));
        attach_dnd_handlers(&row, ws.id, self.bridge.clone(), self.rows.clone());
        self.list.append(&row);
        rows.push((ws.id, row));
    }

    /// Apply a drag-and-drop result to the side panel by moving the visual row
    /// to a new index. Missing `id` is a no-op, and `target_index` is clamped
    /// to the last slot when it exceeds the length.
    pub fn reorder(&self, id: WorkspaceId, target_index: usize) {
        let mut rows = self.rows.borrow_mut();
        let Some(current) = rows.iter().position(|(rid, _)| *rid == id) else {
            return;
        };
        let len = rows.len();
        if len == 0 {
            return;
        }
        let target = target_index.min(len - 1);
        if current == target {
            return;
        }

        let (rid, row) = rows.remove(current);
        // Detach the row widget from ListBox and insert the same widget at the
        // new position. `gtk::ListBox::insert(_, position)` appends when
        // position is -1 or beyond the length.
        self.list.remove(&row);
        self.list.insert(&row, target as i32);
        rows.insert(target, (rid, row));
    }

    pub fn remove(&self, id: WorkspaceId) {
        let mut rows = self.rows.borrow_mut();
        if let Some(idx) = rows.iter().position(|(wid, _)| *wid == id) {
            self.list.remove(&rows[idx].1);
            rows.swap_remove(idx);
        }
    }

    pub fn select_workspace(&self, id: WorkspaceId) {
        if let Some((_, row)) = self.rows.borrow().iter().find(|(wid, _)| *wid == id) {
            self.list.select_row(Some(row));
        }
    }

    pub fn selected_workspace(&self) -> Option<WorkspaceId> {
        let selected = self.list.selected_row()?;
        self.rows
            .borrow()
            .iter()
            .find_map(|(id, row)| (row == &selected).then_some(*id))
    }

    /// Tint a workspace row to flag that an agent finished there.
    /// Cleared automatically when the user selects the row, and also
    /// when [`Self::clear_attention`] is called from a programmatic
    /// activation path (notification click, Alt+number, etc.).
    pub fn mark_attention(&self, id: WorkspaceId) {
        if self.attentions.borrow_mut().insert(id) {
            if let Some((_, row)) = self
                .rows
                .borrow()
                .iter()
                .find(|(wid, _)| *wid == id)
                .cloned()
            {
                row.add_css_class("flowmux-attention");
            }
        }
    }

    /// Drop the attention tint on `id` if present. Programmatic
    /// activation paths (notification click, Alt+number, focus
    /// shortcuts) call this so the row stops glowing once the user
    /// has been brought to the workspace, even when they did not
    /// click the side-panel row themselves.
    pub fn clear_attention(&self, id: WorkspaceId) {
        if self.attentions.borrow_mut().remove(&id) {
            if let Some((_, row)) = self
                .rows
                .borrow()
                .iter()
                .find(|(wid, _)| *wid == id)
                .cloned()
            {
                row.remove_css_class("flowmux-attention");
            }
        }
    }

    /// Reflect an AI agent's live activity on the workspace row. Only
    /// [`AgentActivity::Running`] breathes the color bar (via the
    /// `flowmux-agent-running` class); `NeedsInput`/`Idle`/`None` clear
    /// it (a waiting agent still shows the separate attention tint from
    /// its notification). The class sits on the reused `ListBoxRow`, so
    /// it persists across row rebuilds.
    pub fn set_agent_activity(&self, id: WorkspaceId, activity: Option<AgentActivity>) {
        let running = matches!(activity, Some(AgentActivity::Running));
        let changed = if running {
            self.agent_running.borrow_mut().insert(id)
        } else {
            self.agent_running.borrow_mut().remove(&id)
        };
        if !changed {
            return;
        }
        if let Some((_, row)) = self
            .rows
            .borrow()
            .iter()
            .find(|(wid, _)| *wid == id)
            .cloned()
        {
            if running {
                row.add_css_class("flowmux-agent-running");
            } else {
                row.remove_css_class("flowmux-agent-running");
            }
        }
    }

    /// Expose a cache copy so scenario tests can verify subtitle lines passed
    /// to [`Self::upsert_with_subtitles`]. The side-panel row widget tree is a
    /// GTK object and awkward to read directly, so the cache is the source of truth.
    #[cfg(test)]
    pub(crate) fn cached_subtitles(&self, id: WorkspaceId) -> Option<Vec<String>> {
        self.subtitle_cache.borrow().get(&id).cloned()
    }

    /// Indicate a fresh notification by tinting the bell button.
    /// Cleared next time the popover opens (which marks all seen).
    pub fn bump_notification_badge(&self) {
        if !self.bell_button.has_css_class("accent") {
            self.bell_button.add_css_class("accent");
        }
        // Refresh the popover content if it happens to be visible so
        // the new entry shows immediately.
        self.refresh_notification_popover();
    }

    /// Re-render the bell popover when it is currently shown. Called
    /// after a per-row trash-button delete so the removed entry
    /// disappears immediately instead of after the next open. No-op
    /// when the popover is hidden — the next `connect_show` will pull
    /// fresh entries from `NotificationStore` anyway.
    pub fn refresh_notification_popover(&self) {
        if self.bell_popover.is_visible() {
            self.bell_popover.set_child(Some(&render_notification_list(
                &self.notifications,
                self.bridge.clone(),
                self.bell_popover.clone(),
            )));
        }
    }
}

fn render_notification_list(
    store: &NotificationStore,
    bridge: Bridge,
    popover: gtk::Popover,
) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
    root.set_margin_top(6);
    root.set_margin_bottom(6);
    root.set_margin_start(6);
    root.set_margin_end(6);

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
    scroll.set_min_content_height(160);
    scroll.set_max_content_height(420);
    scroll.set_propagate_natural_height(true);

    let entries = store.entries();
    if entries.is_empty() {
        let empty = gtk::Label::new(Some("No notifications yet."));
        empty.set_margin_top(20);
        empty.set_margin_bottom(20);
        empty.add_css_class("dim-label");
        scroll.set_child(Some(&empty));
        root.append(&scroll);
        return root.upcast();
    }

    // "All Clear" header: clears the in-process transcript and
    // withdraws every still-open desktop toast in one sweep. Sits
    // above the list so the user can drop the whole stack without
    // tapping each row's trash button.
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    header.set_halign(gtk::Align::End);
    let clear_btn = gtk::Button::with_label("All Clear");
    clear_btn.add_css_class("flat");
    clear_btn.set_tooltip_text(Some("Clear every notification"));
    let bridge_for_clear = bridge.clone();
    let popover_for_clear = popover.clone();
    clear_btn.connect_clicked(move |_| {
        popover_for_clear.popdown();
        let bridge = bridge_for_clear.clone();
        gtk::glib::MainContext::default().spawn_local(async move {
            let _ = bridge.tx.send(GtkCommand::ClearAllNotifications).await;
        });
    });
    header.append(&clear_btn);
    root.append(&header);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");

    // Newest at top.
    for entry in entries.iter().rev() {
        list.append(&notification_row(entry, bridge.clone(), popover.clone()));
    }
    scroll.set_child(Some(&list));
    root.append(&scroll);
    root.upcast()
}

fn notification_row(
    entry: &NotificationEntry,
    bridge: Bridge,
    popover: gtk::Popover,
) -> gtk::Widget {
    let row = gtk::ListBoxRow::new();
    row.set_activatable(true);
    row.set_selectable(false);

    // Horizontal split: text column on the left grows to fill, trash
    // button pinned to the right. The button gets its own click handler
    // so a "delete" tap can fire `DeleteNotification` without the row's
    // gesture also dispatching `OpenNotification` for the same entry.
    let h = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    h.set_margin_top(8);
    h.set_margin_bottom(8);
    h.set_margin_start(10);
    h.set_margin_end(10);

    let v = gtk::Box::new(gtk::Orientation::Vertical, 2);
    v.set_hexpand(true);

    let title = gtk::Label::new(Some(&entry.title));
    title.set_halign(gtk::Align::Start);
    title.add_css_class("heading");
    let body = gtk::Label::new(Some(&entry.body));
    body.set_halign(gtk::Align::Start);
    body.set_wrap(true);
    body.set_max_width_chars(40);
    let when = gtk::Label::new(Some(&format_time(&entry.created_at)));
    when.set_halign(gtk::Align::Start);
    when.add_css_class("caption");
    when.add_css_class("dim-label");

    if entry.read {
        v.set_opacity(0.55);
    }
    if matches!(
        entry.level,
        NotificationLevel::AttentionNeeded | NotificationLevel::Error
    ) {
        title.add_css_class("accent");
    }

    v.append(&title);
    v.append(&body);
    v.append(&when);
    h.append(&v);

    let entry_id = entry.id;

    let delete_btn = gtk::Button::from_icon_name("user-trash-symbolic");
    delete_btn.set_tooltip_text(Some("Delete notification"));
    delete_btn.add_css_class("flat");
    // Center vertically next to the title row so the icon doesn't sit
    // awkwardly aligned with the body wrap.
    delete_btn.set_valign(gtk::Align::Center);
    let bridge_for_delete = bridge.clone();
    delete_btn.connect_clicked(move |_| {
        tracing::debug!(%entry_id, "notification trash button clicked");
        let bridge = bridge_for_delete.clone();
        gtk::glib::MainContext::default().spawn_local(async move {
            if let Err(e) = bridge
                .tx
                .send(GtkCommand::DeleteNotification { id: entry_id })
                .await
            {
                tracing::warn!(error = %e, "DeleteNotification dispatch failed");
            }
        });
    });
    h.append(&delete_btn);

    row.set_child(Some(&h));

    // A GestureClick on the row's primary button is the only path that
    // fires regardless of the ListBox's SelectionMode. With
    // `SelectionMode::None` the ListBox's `row-activated` and the row's
    // `activate` signals are suppressed, so a previous `connect_activate`
    // handler never ran and clicks looked dead.
    //
    // The trash button consumes its own click before this gesture sees
    // it (gtk::Button is its own widget with its own controller), so
    // pressing the icon dispatches Delete without also firing Open.
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_PRIMARY);
    let bridge_for_click = bridge.clone();
    let popover_for_click = popover.clone();
    click.connect_released(move |gesture, _n_press, _x, _y| {
        tracing::debug!(%entry_id, "notification row clicked");
        let bridge = bridge_for_click.clone();
        let popover = popover_for_click.clone();
        gtk::glib::MainContext::default().spawn_local(async move {
            if let Err(e) = bridge
                .tx
                .send(GtkCommand::OpenNotification { id: entry_id })
                .await
            {
                tracing::warn!(error = %e, "OpenNotification dispatch failed");
            }
        });
        popover.popdown();
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    row.add_controller(click);

    // Keyboard parity: Space/Enter on a focused row also routes.
    let bridge_for_key = bridge.clone();
    let popover_for_key = popover.clone();
    row.connect_activate(move |_| {
        tracing::debug!(%entry_id, "notification row activated by keyboard");
        let bridge = bridge_for_key.clone();
        let popover = popover_for_key.clone();
        gtk::glib::MainContext::default().spawn_local(async move {
            let _ = bridge
                .tx
                .send(GtkCommand::OpenNotification { id: entry_id })
                .await;
        });
        popover.popdown();
    });

    row.upcast()
}

fn format_time(ts: &chrono::DateTime<chrono::Utc>) -> String {
    let local: chrono::DateTime<chrono::Local> = (*ts).into();
    local.format("%H:%M:%S").to_string()
}

/// Attach drag-and-drop controllers to one side-panel workspace row.
///
/// - `DragSource`: serializes the workspace ID as a UUID string in the
///   ContentProvider and dims the source row during drag.
/// - `DropTarget`: when dropped on another row, uses the drop y position to
///   choose above or below and sends [`GtkCommand::ReorderWorkspace`].
fn attach_dnd_handlers(row: &gtk::ListBoxRow, id: WorkspaceId, bridge: Bridge, rows: RowsCell) {
    let drag_source = gtk::DragSource::new();
    drag_source.set_actions(gtk::gdk::DragAction::MOVE);
    let id_for_prepare = id;
    drag_source.connect_prepare(move |_, _, _| {
        tracing::debug!(workspace = %id_for_prepare, "sidebar drag prepare");
        // ContentProvider::for_value + DropTarget::new(String) gives the most
        // reliable type match. for_bytes(mime, bytes) creates MIME-specific
        // content that did not match DropTarget::new(Bytes::static_type()), so
        // motion/drop signals never fired.
        let payload = id_for_prepare.to_string();
        Some(gtk::gdk::ContentProvider::for_value(&payload.to_value()))
    });
    let row_for_begin = row.clone();
    drag_source.connect_drag_begin(move |_, _| {
        row_for_begin.set_opacity(0.4);
        row_for_begin.add_css_class("flowmux-dragging");
    });
    let row_for_end = row.clone();
    drag_source.connect_drag_end(move |_, _, _| {
        row_for_end.set_opacity(1.0);
        row_for_end.remove_css_class("flowmux-dragging");
    });
    let row_for_cancel = row.clone();
    drag_source.connect_drag_cancel(move |_, _, _| {
        row_for_cancel.set_opacity(1.0);
        row_for_cancel.remove_css_class("flowmux-dragging");
        false
    });
    row.add_controller(drag_source);

    let drop_target =
        gtk::DropTarget::new(gtk::glib::types::Type::STRING, gtk::gdk::DragAction::MOVE);
    // Use motion y to choose the upper or lower half of the row and place the
    // indicator. Drop logic uses the same y basis for new_index, so the blue
    // line marks the actual drop position. Hovering the upper half of the first
    // row signals "move to the top".
    let target_id_for_motion = id;
    let row_for_motion = row.clone();
    drop_target.connect_motion(move |_, _x, y| {
        tracing::trace!(target = %target_id_for_motion, y, "sidebar drop motion");
        let height = row_for_motion.height();
        let above = if height > 0 {
            y < (height as f64) / 2.0
        } else {
            true
        };
        if above {
            row_for_motion.remove_css_class("flowmux-drop-below");
            row_for_motion.add_css_class("flowmux-drop-above");
        } else {
            row_for_motion.remove_css_class("flowmux-drop-above");
            row_for_motion.add_css_class("flowmux-drop-below");
        }
        gtk::gdk::DragAction::MOVE
    });
    let row_for_leave = row.clone();
    drop_target.connect_leave(move |_| {
        row_for_leave.remove_css_class("flowmux-drop-above");
        row_for_leave.remove_css_class("flowmux-drop-below");
    });
    let row_for_drop = row.clone();
    let target_id = id;
    drop_target.connect_drop(move |_, value, _x, y| {
        tracing::debug!(target = %target_id, "sidebar drop fired");
        row_for_drop.remove_css_class("flowmux-drop-above");
        row_for_drop.remove_css_class("flowmux-drop-below");
        let Ok(payload) = value.get::<String>() else {
            tracing::warn!(value = ?value, "sidebar drop: payload was not String — DropTarget type mismatch");
            return false;
        };
        let Ok(source_id) = payload.parse::<WorkspaceId>() else {
            tracing::warn!(payload = %payload, "sidebar drop: payload not a WorkspaceId");
            return false;
        };
        if source_id == target_id {
            tracing::debug!(workspace = %source_id, "sidebar drop: target == source, ignoring");
            return false;
        }

        let rows_snapshot: Vec<WorkspaceId> = rows
            .borrow()
            .iter()
            .map(|(rid, _)| *rid)
            .collect();
        let Some(source_idx) = rows_snapshot.iter().position(|rid| *rid == source_id) else {
            return false;
        };
        let Some(target_idx) = rows_snapshot.iter().position(|rid| *rid == target_id) else {
            return false;
        };

        // Drop above the target if y is in the upper half, otherwise below it.
        let row_height = row_for_drop.height();
        let above = if row_height > 0 {
            y < (row_height as f64) / 2.0
        } else {
            true
        };

        // Compute the final index. reorder_workspace means "remove, then insert
        // at target", so target index shifts down by one when the source was
        // before the target.
        let new_index = match (above, source_idx < target_idx) {
            (true, true) => target_idx.saturating_sub(1),
            (true, false) => target_idx,
            (false, true) => target_idx,
            (false, false) => target_idx + 1,
        };

        if new_index == source_idx {
            tracing::debug!(
                workspace = %source_id,
                new_index,
                "sidebar drop: index unchanged after computation"
            );
            return false;
        }

        tracing::info!(
            source = %source_id,
            target = %target_id,
            source_idx,
            target_idx,
            new_index,
            above,
            "sidebar reorder: sending ReorderWorkspace"
        );
        let tx = bridge.tx.clone();
        gtk::glib::MainContext::default().spawn_local(async move {
            if let Err(e) = tx
                .send(GtkCommand::ReorderWorkspace {
                    id: source_id,
                    target_index: new_index,
                })
                .await
            {
                tracing::warn!(error = %e, "sidebar reorder: bridge send failed");
            }
        });
        true
    });
    row.add_controller(drop_target);
}

fn row_widget(
    ws: &Workspace,
    subtitles: &[String],
    on_close: Rc<dyn Fn(WorkspaceId)>,
    bridge: Bridge,
) -> gtk::Widget {
    // Row content (color bar + text column) lives in a horizontal Box;
    // the close button is layered on top via a `gtk::Overlay` rather than
    // taking a slot in that Box. Keeping it out of the linear layout lets
    // the text column claim the row's full width — otherwise the always-
    // present (just transparent) button reserved a strip on the right
    // that read as dead blank space whenever it was hidden. On hover the
    // button fades in and overlaps the tail of the text.
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    // User-requested vertical margin reduction from 6 to 3px; keep start/end.
    content.set_margin_top(3);
    content.set_margin_bottom(3);
    content.set_margin_start(4);
    content.set_margin_end(6);

    if let Some(color) = ws.color.as_deref() {
        content.append(&color_bar(color));
    }

    let meta = build_meta_column(ws, subtitles);
    meta.set_hexpand(true);
    meta.set_margin_start(6);
    content.append(&meta);

    let row = gtk::Overlay::new();
    row.set_child(Some(&content));

    let close_btn = gtk::Button::from_icon_name("window-close-symbolic");
    close_btn.add_css_class("flat");
    close_btn.add_css_class("circular");
    close_btn.add_css_class("flowmux-sidebar-close");
    close_btn.set_tooltip_text(Some("Close tab"));
    close_btn.set_halign(gtk::Align::End);
    close_btn.set_valign(gtk::Align::Center);
    close_btn.set_margin_end(6);
    close_btn.set_opacity(0.0);
    close_btn.set_can_target(false);
    let id = ws.id;
    let on_close_for_click = on_close.clone();
    close_btn.connect_clicked(move |_| on_close_for_click(id));
    row.add_overlay(&close_btn);

    let motion = gtk::EventControllerMotion::new();
    let btn_enter = close_btn.clone();
    motion.connect_enter(move |_, _, _| {
        btn_enter.set_opacity(1.0);
        btn_enter.set_can_target(true);
    });
    let btn_leave = close_btn.clone();
    motion.connect_leave(move |_| {
        btn_leave.set_opacity(0.0);
        btn_leave.set_can_target(false);
    });
    row.add_controller(motion);

    // Right-click context menu — plain Popover + Button rows whose
    // click closures send the right GtkCommand directly through the
    // bridge. We deliberately avoid PopoverMenu+win.* actions because
    // the action lookup chain has been observed to drop through some
    // GTK versions, leaving the menu items inert.
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_SECONDARY);
    let row_for_click = row.clone();
    let on_close_for_menu = on_close.clone();
    click.connect_pressed(move |gesture, _n_press, x, y| {
        // Claim the sequence up front so the row's primary-click gesture
        // and the ListBox don't also act on this press.
        gesture.set_state(gtk::EventSequenceState::Claimed);
        let popover = gtk::Popover::new();
        let v = gtk::Box::new(gtk::Orientation::Vertical, 0);
        v.set_margin_top(4);
        v.set_margin_bottom(4);

        let mk = |label: &str| -> gtk::Button {
            let b = gtk::Button::with_label(label);
            b.add_css_class("flat");
            b.set_halign(gtk::Align::Fill);
            b.set_hexpand(true);
            if let Some(label) = b.child().and_downcast::<gtk::Label>() {
                label.set_xalign(0.0);
            }
            b
        };

        let rename_btn = mk("Change tab name");
        let bridge_for_rename = bridge.clone();
        let pop = popover.clone();
        rename_btn.connect_clicked(move |_| {
            pop.popdown();
            let bridge = bridge_for_rename.clone();
            gtk::glib::MainContext::default().spawn_local(async move {
                let _ = bridge.tx.send(GtkCommand::ShowRenameDialog { id }).await;
            });
        });
        v.append(&rename_btn);

        let color_btn = mk("Change color…");
        let bridge_for_color = bridge.clone();
        let pop = popover.clone();
        color_btn.connect_clicked(move |_| {
            pop.popdown();
            let bridge = bridge_for_color.clone();
            gtk::glib::MainContext::default().spawn_local(async move {
                let _ = bridge.tx.send(GtkCommand::ShowColorDialog { id }).await;
            });
        });
        v.append(&color_btn);

        v.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let close_btn = mk("Close tab");
        let on_close_clone = on_close_for_menu.clone();
        let pop = popover.clone();
        close_btn.connect_clicked(move |_| {
            pop.popdown();
            on_close_clone(id);
        });
        v.append(&close_btn);

        // Close every open workspace at once. The dispatcher shows a
        // single confirmation before tearing them all down.
        let close_all_btn = mk("Close all tabs");
        let bridge_for_close_all = bridge.clone();
        let pop = popover.clone();
        close_all_btn.connect_clicked(move |_| {
            pop.popdown();
            let bridge = bridge_for_close_all.clone();
            gtk::glib::MainContext::default().spawn_local(async move {
                let (ack, rx) = tokio::sync::oneshot::channel();
                let _ = bridge
                    .tx
                    .send(GtkCommand::RemoveAllWorkspaces { ack })
                    .await;
                let _ = rx.await;
            });
        });
        v.append(&close_all_btn);

        v.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        // Open the focused pane's cwd in the system file manager
        // (Nautilus on a default Ubuntu/GNOME install). The dispatcher
        // resolves "focused pane" inside this workspace and falls back
        // to its first leaf pane.
        let show_btn = mk("Show in folder");
        let bridge_for_show = bridge.clone();
        let pop = popover.clone();
        show_btn.connect_clicked(move |_| {
            pop.popdown();
            let bridge = bridge_for_show.clone();
            gtk::glib::MainContext::default().spawn_local(async move {
                let _ = bridge
                    .tx
                    .send(GtkCommand::ShowFocusedPaneFolder { workspace: id })
                    .await;
            });
        });
        v.append(&show_btn);

        // Copy the focused-pane text identifier — cwd for terminal,
        // URL for browser — to the clipboard. The dispatcher routes
        // based on the active surface kind, so one item covers both
        // cases without forcing the user to pick.
        let copy_btn = mk("Copy path");
        let bridge_for_copy = bridge.clone();
        let pop = popover.clone();
        copy_btn.connect_clicked(move |_| {
            pop.popdown();
            let bridge = bridge_for_copy.clone();
            gtk::glib::MainContext::default().spawn_local(async move {
                let _ = bridge
                    .tx
                    .send(GtkCommand::CopyFocusedPaneText { workspace: id })
                    .await;
            });
        });
        v.append(&copy_btn);

        popover.set_child(Some(&v));
        popover.set_parent(&row_for_click);
        popover.set_has_arrow(false);
        crate::ui::popover_pos::anchor_at_click(&popover, &row_for_click, x, y);
        popover.connect_closed(|p| p.unparent());
        // Pop up after the right-button press event fully settles. On
        // GTK 4.6 (Ubuntu 22.04), calling popup() synchronously inside
        // the press handler — while the button is still physically down —
        // left the autohide popover's input grab unstable, so the first
        // clicks on its menu items were dropped intermittently (the bug
        // looked timing-dependent: it "worked sometimes"). Deferring to
        // an idle callback lets the press resolve and the grab settle
        // before the popover takes its own grab.
        gtk::glib::idle_add_local_once(move || popover.popup());
    });
    row.add_controller(click);

    row.upcast()
}

fn color_bar(color: &str) -> gtk::Widget {
    let bar = gtk::DrawingArea::new();
    bar.set_size_request(4, -1);
    bar.set_vexpand(true);
    bar.set_valign(gtk::Align::Fill);
    // Targeted by the `.flowmux-agent-running .flowmux-color-bar` CSS
    // rule so the bar's opacity "breathes" while an agent is working.
    bar.add_css_class("flowmux-color-bar");
    let color_owned = color.to_string();
    bar.set_draw_func(move |_, cr, w, h| {
        let rgba = gtk::gdk::RGBA::parse(&color_owned)
            .unwrap_or_else(|_| gtk::gdk::RGBA::new(0.5, 0.5, 0.5, 1.0));
        cr.set_source_rgba(
            rgba.red() as f64,
            rgba.green() as f64,
            rgba.blue() as f64,
            rgba.alpha() as f64,
        );
        let r = 2.0;
        let w = w as f64;
        let h = h as f64;
        cr.new_path();
        cr.arc(r, r, r, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
        cr.line_to(w - r, 0.0);
        cr.arc(w - r, r, r, 1.5 * std::f64::consts::PI, 0.0);
        cr.line_to(w, h - r);
        cr.arc(w - r, h - r, r, 0.0, 0.5 * std::f64::consts::PI);
        cr.line_to(r, h);
        cr.arc(
            r,
            h - r,
            r,
            0.5 * std::f64::consts::PI,
            std::f64::consts::PI,
        );
        cr.close_path();
        let _ = cr.fill();
    });
    bar.upcast()
}

fn build_meta_column(ws: &Workspace, subtitles: &[String]) -> gtk::Box {
    // Layout:
    //   line 1: workspace display title, custom_title if present, otherwise name
    //   line 2..4: up to 3 subtitle lines supplied by the caller, usually
    //              shortened MRU pane cwd paths in .../A/B/C form.
    //   optional aux: linked PR badge / listening ports.
    let v = gtk::Box::new(gtk::Orientation::Vertical, 1);

    let title = gtk::Label::new(Some(ws.display_title()));
    title.set_halign(gtk::Align::Start);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_xalign(0.0);
    title.add_css_class("heading");
    v.append(&title);

    for (i, line) in subtitles.iter().take(3).enumerate() {
        let label = gtk::Label::new(Some(line.as_str()));
        label.set_halign(gtk::Align::Start);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        label.add_css_class("caption");
        label.add_css_class("dim-label");
        // Make the first subtitle, the MRU head, slightly stronger to indicate
        // the terminal the user is currently viewing.
        if i == 0 {
            label.add_css_class("flowmux-sidebar-subtitle-primary");
        }
        v.append(&label);
    }

    // Auxiliary line: PR badge + listening ports (kept compact).
    let aux = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let mut has_aux = false;
    if let Some(git) = &ws.git {
        if let Some(pr) = &git.linked_pr {
            let badge = gtk::Label::new(Some(&format!("#{}", pr.number)));
            badge.add_css_class("caption");
            badge.add_css_class(match pr.state {
                PrState::Open => "success",
                PrState::Merged => "accent",
                PrState::Closed => "warning",
                PrState::Draft => "dim-label",
            });
            aux.append(&badge);
            has_aux = true;
        }
    }
    if !ws.listening_ports.is_empty() {
        let ports = ws
            .listening_ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let p = gtk::Label::new(Some(&format!(":{ports}")));
        p.add_css_class("caption");
        p.add_css_class("dim-label");
        aux.append(&p);
        has_aux = true;
    }
    if has_aux {
        v.append(&aux);
    }

    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use flowmux_core::{Pane, PaneContent, PaneId, PaneSurface, Surface, SurfaceId, SurfaceKind};
    use std::path::PathBuf;

    fn ws_with_active_terminal_cwd(cwd: Option<PathBuf>) -> Workspace {
        let surface = PaneSurface::terminal("auto", cwd.clone());
        let surface_id = surface.id;
        Workspace {
            id: WorkspaceId::new(),
            name: "auto".into(),
            custom_title: None,
            root_dir: PathBuf::from("/tmp/origin"),
            git: None,
            listening_ports: vec![],
            surfaces: vec![Surface {
                id: SurfaceId::new(),
                kind: SurfaceKind::Terminal {
                    shell: None,
                    cwd: cwd.clone(),
                },
                title: "main".into(),
                root_pane: Pane::Leaf {
                    id: PaneId::new(),
                    content: PaneContent::Tabs {
                        active: surface_id,
                        surfaces: vec![surface],
                    },
                },
            }],
            color: None,
        }
    }

    /// Smoke test that row_widget can build a stable widget tree with a name
    /// and subtitle lines. Requires GTK init, so headless environments skip it.
    #[gtk::test]
    fn row_widget_builds_with_one_to_three_subtitle_lines() {
        if gtk::init().is_err() {
            return;
        }
        let ws = ws_with_active_terminal_cwd(Some(PathBuf::from("/home/u/dev/os/flowmux")));
        let bridge = crate::bridge::Bridge::new().0;
        let on_close: Rc<dyn Fn(WorkspaceId)> = Rc::new(|_| {});

        for n in 0..=3 {
            let lines: Vec<String> = (0..n).map(|i| format!(".../line{i}")).collect();
            let _ = row_widget(&ws, &lines, on_close.clone(), bridge.clone());
        }
        // Even with 4 lines, build_meta_column truncates to 3 via take(3).
        let four = vec!["a".into(), "b".into(), "c".into(), "d-overflow".into()];
        let _ = row_widget(&ws, &four, on_close, bridge);
    }
}
