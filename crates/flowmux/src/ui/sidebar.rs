// SPDX-License-Identifier: GPL-3.0-or-later
//! Workspace sidebar (flowmux's vertical-tabs left panel).
//!
//! Layout:
//!
//! ```text
//! +----------------+
//! | [+] Workspaces |  native split header
//! +----------------+
//! | • workspace 1  |
//! | • workspace 2  |  scrollable workspace list
//! | • workspace 3  |
//! +----------------+
//! ```
//!
//! The header's `+` adds a workspace (Ctrl+N equivalent) and
//! the bell shows an in-process notification transcript. The list
//! rows expose hover-X close, color bar, right-click menu (rename /
//! recolor / close).

use crate::bridge::{Bridge, GtkCommand};
use crate::notifications::{NotificationEntry, NotificationStore};
use crate::ui::update_banner::UpdateBanner;
use crate::ui::usage_popover::UsagePopover;
use crate::ui::workspace_view::{
    read_tab_dnd_payload_from_drop, tab_dnd_content_formats, tab_dnd_formats_accept_payload,
};
use adw::prelude::*;
use flowmux_core::{AgentStatus, NotificationLevel, PrState, Workspace, WorkspaceId};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use tokio::sync::oneshot;

type RowsCell = Rc<RefCell<Vec<(WorkspaceId, gtk::ListBoxRow)>>>;

const WORKSPACE_DND_MIME: &str = "application/x-flowmux-workspace";
const SIDEBAR_TREE_GUTTER_WIDTH: i32 = 14;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TreeBranch {
    Middle,
    Last,
}

impl TreeBranch {
    fn for_position(has_following_sibling: bool) -> Self {
        if has_following_sibling {
            Self::Middle
        } else {
            Self::Last
        }
    }
}

/// Connector columns drawn before one metadata row. Each boolean represents
/// an ancestor depth and is true while that ancestor still has a following
/// sibling. `branch` describes the row at the current (last) depth.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TreeLine {
    ancestor_continuations: Vec<bool>,
    branch: TreeBranch,
}

impl TreeLine {
    fn top_level(has_following_sibling: bool) -> Self {
        Self {
            ancestor_continuations: Vec::new(),
            branch: TreeBranch::for_position(has_following_sibling),
        }
    }

    fn nested(ancestor_continues: bool, has_following_sibling: bool) -> Self {
        Self {
            ancestor_continuations: vec![ancestor_continues],
            branch: TreeBranch::for_position(has_following_sibling),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetadataTreeRow {
    AgentHeader(usize),
    AgentStatus(usize),
    AgentPath(usize),
    Path(usize),
    Aux,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MetadataTreeRowLayout {
    row: MetadataTreeRow,
    line: TreeLine,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TreeSegment {
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceRowDetails {
    pub(crate) agent_blocks: Vec<WorkspaceRowAgentBlock>,
    pub(crate) path_lines: Vec<String>,
}

impl WorkspaceRowDetails {
    pub(crate) fn path_only(lines: &[String]) -> Self {
        Self {
            agent_blocks: Vec::new(),
            path_lines: lines.iter().take(3).cloned().collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceRowAgentBlock {
    pub(crate) agent_name: String,
    pub(crate) status: AgentStatus,
    pub(crate) seen: bool,
    pub(crate) status_text: String,
    pub(crate) path: Option<String>,
    pub(crate) overflow_count: usize,
}

#[derive(Clone)]
pub struct Sidebar {
    pub header: adw::HeaderBar,
    pub root: gtk::Box,
    pub list: gtk::ListBox,
    rows: Rc<RefCell<Vec<(WorkspaceId, gtk::ListBoxRow)>>>,
    on_close: Rc<dyn Fn(WorkspaceId)>,
    bell_button: gtk::MenuButton,
    bell_popover: gtk::Popover,
    usage_button: gtk::MenuButton,
    notifications: NotificationStore,
    attentions: Rc<RefCell<HashSet<WorkspaceId>>>,
    notification_workspaces: Rc<RefCell<HashSet<WorkspaceId>>>,
    notification_badges: Rc<RefCell<HashMap<WorkspaceId, gtk::Box>>>,
    /// Workspace-level AI agent status rollups. The classes live on reused
    /// rows, so this map keeps membership idempotent across redraws.
    agent_status: Rc<RefCell<HashMap<WorkspaceId, AgentStatus>>>,
    bridge: Bridge,
    /// Last computed row detail per workspace. Kept so paths that do not know
    /// subtitle data, such as rename or color changes, can redraw a row without
    /// losing its current path/agent details. WindowController updates this via
    /// [`Sidebar::upsert_with_details`] after sync_workspace_label.
    subtitle_cache: Rc<RefCell<HashMap<WorkspaceId, WorkspaceRowDetails>>>,
    /// Live, ordered (id, name) snapshot mirroring the visible rows. Read
    /// synchronously by the pane tab "Move" submenu so it reflects the current
    /// workspace set and names at click time. Kept in sync by `upsert_inner`,
    /// `reorder`, and `remove`.
    titles: Rc<RefCell<Vec<(WorkspaceId, String)>>>,
    tab_drag_drop_seen: Rc<Cell<bool>>,
    tab_drag_drop_committed: Rc<Cell<bool>>,
    update_banner: UpdateBanner,
}

impl Sidebar {
    pub fn new<S, C>(
        on_select: S,
        on_close: C,
        bridge: Bridge,
        notifications: NotificationStore,
        tokio_handle: Option<tokio::runtime::Handle>,
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

        // ---- Native sidebar header ----
        let new_btn = gtk::Button::from_icon_name("list-add-symbolic");
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
        let bell_button = gtk::MenuButton::new();
        bell_button.set_icon_name("notifications-symbolic");
        bell_button.set_tooltip_text(Some("Notifications"));
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
            // unread (full opacity, accent on NeedsInput titles)
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
        let header_title = gtk::Label::new(Some("Workspaces"));
        header_title.add_css_class("heading");
        let header = adw::HeaderBar::new();
        header.set_show_start_title_buttons(true);
        header.set_show_end_title_buttons(false);
        header.set_title_widget(Some(&header_title));
        header.pack_start(&new_btn);
        header.pack_end(&bell_button);

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

        let footer_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        footer_spacer.set_hexpand(true);
        footer.append(&footer_spacer);

        let usage = UsagePopover::new(tokio_handle.clone());
        usage
            .button()
            .set_tooltip_text(Some("AI usage (Ctrl+Alt+U)"));
        footer.append(usage.button());
        let usage_button = usage.button().clone();

        let worktree_btn = gtk::Button::from_icon_name("vcs-branch-symbolic");
        worktree_btn.add_css_class("flat");
        worktree_btn.set_tooltip_text(Some("Worktrees (Ctrl+Alt+W)"));
        worktree_btn.set_focus_on_click(false);
        worktree_btn.add_css_class("flowmux-sidebar-options");
        worktree_btn.set_halign(gtk::Align::End);
        worktree_btn.set_widget_name("flowmux-worktree-button");
        let bridge_for_worktrees = bridge.clone();
        worktree_btn.connect_clicked(move |_| {
            let bridge = bridge_for_worktrees.clone();
            gtk::glib::MainContext::default().spawn_local(async move {
                let _ = bridge
                    .tx
                    .send(GtkCommand::ToggleWorktreePanel { pane: None })
                    .await;
            });
        });
        footer.append(&worktree_btn);

        // File browser toggle, immediately right of the worktree button. Sends
        // `None` so the window dispatcher targets the focused pane (the footer
        // has no pane context). Same Ctrl+Alt+F path.
        let file_browser_btn = gtk::Button::from_icon_name("folder-symbolic");
        file_browser_btn.add_css_class("flat");
        file_browser_btn.set_tooltip_text(Some("File browser (Ctrl+Alt+F)"));
        file_browser_btn.set_focus_on_click(false);
        file_browser_btn.add_css_class("flowmux-sidebar-options");
        file_browser_btn.set_halign(gtk::Align::End);
        file_browser_btn.set_widget_name("flowmux-file-browser-button");
        let bridge_for_files = bridge.clone();
        file_browser_btn.connect_clicked(move |_| {
            let bridge = bridge_for_files.clone();
            gtk::glib::MainContext::default().spawn_local(async move {
                let _ = bridge
                    .tx
                    .send(GtkCommand::ToggleFileBrowser { pane: None })
                    .await;
            });
        });
        footer.append(&file_browser_btn);

        // Self-update banner. Hidden until the background release check
        // finds a newer tag; the banner owns its own check/install wiring.
        let update_banner = UpdateBanner::new(tokio_handle);

        // ---- Sidebar body; the header is mounted by the window shell ----
        let root_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root_box.append(&scroll);
        root_box.append(update_banner.widget());
        root_box.append(&footer);

        Self {
            header,
            root: root_box,
            list,
            rows,
            on_close,
            bell_button,
            bell_popover,
            usage_button,
            notifications,
            attentions,
            notification_workspaces: Rc::new(RefCell::new(HashSet::new())),
            notification_badges: Rc::new(RefCell::new(HashMap::new())),
            agent_status: Rc::new(RefCell::new(HashMap::new())),
            bridge,
            subtitle_cache: Rc::new(RefCell::new(HashMap::new())),
            titles: Rc::new(RefCell::new(Vec::new())),
            tab_drag_drop_seen: Rc::new(Cell::new(false)),
            tab_drag_drop_committed: Rc::new(Cell::new(false)),
            update_banner,
        }
    }

    pub(crate) fn update_banner(&self) -> UpdateBanner {
        self.update_banner.clone()
    }

    pub(crate) fn usage_button(&self) -> gtk::MenuButton {
        self.usage_button.clone()
    }

    /// Add or redraw a workspace row using cached subtitles. Used by paths
    /// that do not know subtitle data, such as rename or color changes; the
    /// subtitles stay at the last value supplied to [`Self::upsert_with_details`].
    pub fn upsert(&self, ws: &Workspace) {
        let cached = self.subtitle_cache.borrow().get(&ws.id).cloned();
        let details = cached.unwrap_or_default();
        self.upsert_inner(ws, &details);
    }

    pub(crate) fn upsert_with_details(&self, ws: &Workspace, details: WorkspaceRowDetails) {
        self.subtitle_cache
            .borrow_mut()
            .insert(ws.id, details.clone());
        self.upsert_inner(ws, &details);
    }

    /// Live, ordered (id, name) snapshot of the side-panel workspaces. Read by
    /// the pane tab "Move" submenu so it always reflects the current set.
    pub fn workspace_titles(&self) -> Rc<RefCell<Vec<(WorkspaceId, String)>>> {
        self.titles.clone()
    }

    pub(crate) fn tab_drag_drop_state(&self) -> (Rc<Cell<bool>>, Rc<Cell<bool>>) {
        (
            self.tab_drag_drop_seen.clone(),
            self.tab_drag_drop_committed.clone(),
        )
    }

    fn upsert_inner(&self, ws: &Workspace, details: &WorkspaceRowDetails) {
        {
            // Use the displayed title (custom name, else folder-derived name) so
            // the "Move" submenu matches what the side panel shows, including
            // after a rename or a cwd change.
            let title = ws.display_title().to_string();
            let mut titles = self.titles.borrow_mut();
            if let Some(entry) = titles.iter_mut().find(|(id, _)| *id == ws.id) {
                entry.1 = title;
            } else {
                titles.push((ws.id, title));
            }
        }
        let mut rows = self.rows.borrow_mut();
        if let Some((_, row)) = rows.iter().find(|(id, _)| *id == ws.id).cloned() {
            let row_widget = row_widget(ws, details, self.on_close.clone(), self.bridge.clone());
            row_widget
                .badge
                .set_visible(self.notification_workspaces.borrow().contains(&ws.id));
            self.notification_badges
                .borrow_mut()
                .insert(ws.id, row_widget.badge);
            row.set_child(Some(&row_widget.root));
            let status = self.agent_status.borrow().get(&ws.id).copied();
            apply_agent_status_class(&row, status);
            return;
        }
        let row = gtk::ListBoxRow::new();
        let row_widget = row_widget(ws, details, self.on_close.clone(), self.bridge.clone());
        row_widget
            .badge
            .set_visible(self.notification_workspaces.borrow().contains(&ws.id));
        self.notification_badges
            .borrow_mut()
            .insert(ws.id, row_widget.badge);
        row.set_child(Some(&row_widget.root));
        let status = self.agent_status.borrow().get(&ws.id).copied();
        apply_agent_status_class(&row, status);
        attach_dnd_handlers(&row, ws.id, self.bridge.clone(), self.rows.clone());
        attach_tab_drop_to_row(
            &row,
            ws.id,
            self.bridge.clone(),
            self.tab_drag_drop_seen.clone(),
            self.tab_drag_drop_committed.clone(),
        );
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
        let was_selected = self.list.selected_row().as_ref() == Some(&row);
        // Detach the row widget from ListBox and insert the same widget at the
        // new position. `gtk::ListBox::insert(_, position)` appends when
        // position is -1 or beyond the length.
        self.list.remove(&row);
        self.list.insert(&row, target as i32);
        if was_selected {
            // A removed ListBoxRow keeps its selected flag even though the
            // ListBox no longer reports it as selected. Clear that stale flag
            // before selecting the reinserted row so GTK records it again.
            self.list.unselect_all();
            self.list.select_row(Some(&row));
        }
        rows.insert(target, (rid, row));
        {
            let mut titles = self.titles.borrow_mut();
            if let Some(cur) = titles.iter().position(|(tid, _)| *tid == id) {
                let entry = titles.remove(cur);
                let at = target.min(titles.len());
                titles.insert(at, entry);
            }
        }
    }

    pub fn remove(&self, id: WorkspaceId) {
        let mut rows = self.rows.borrow_mut();
        if let Some(idx) = rows.iter().position(|(wid, _)| *wid == id) {
            self.list.remove(&rows[idx].1);
            rows.remove(idx);
        }
        self.attentions.borrow_mut().remove(&id);
        self.notification_workspaces.borrow_mut().remove(&id);
        self.notification_badges.borrow_mut().remove(&id);
        self.agent_status.borrow_mut().remove(&id);
        self.subtitle_cache.borrow_mut().remove(&id);
        self.titles.borrow_mut().retain(|(tid, _)| *tid != id);
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

    #[cfg(test)]
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn workspace_row_contains(&self, id: WorkspaceId, text: &str) -> bool {
        fn contains(widget: &gtk::Widget, text: &str) -> bool {
            if widget
                .downcast_ref::<gtk::Label>()
                .is_some_and(|label| label.text().as_str() == text)
            {
                return true;
            }
            let mut child = widget.first_child();
            while let Some(widget) = child {
                if contains(&widget, text) {
                    return true;
                }
                child = widget.next_sibling();
            }
            false
        }

        self.rows
            .borrow()
            .iter()
            .find(|(workspace, _)| *workspace == id)
            .and_then(|(_, row)| row.child())
            .is_some_and(|row| contains(&row, text))
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

    /// Show a compact unread dot on each workspace that currently owns an
    /// unread notification. Replacing the set also clears stale dots after a
    /// workspace/pane is focused or notifications are read elsewhere.
    pub fn set_notification_workspaces(&self, workspaces: &HashSet<WorkspaceId>) {
        *self.notification_workspaces.borrow_mut() = workspaces.clone();
        for (workspace, badge) in self.notification_badges.borrow().iter() {
            badge.set_visible(workspaces.contains(workspace));
        }
    }

    /// Reflect an AI agent's rolled-up status on the workspace row.
    /// `Blocked` and `Done` get stable row classes so CSS can distinguish them.
    pub fn set_agent_status(&self, id: WorkspaceId, status: Option<AgentStatus>) {
        let changed = match status {
            Some(status) => self.agent_status.borrow_mut().insert(id, status) != Some(status),
            None => self.agent_status.borrow_mut().remove(&id).is_some(),
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
            apply_agent_status_class(&row, status);
        }
    }

    /// Expose a cache copy so scenario tests can verify subtitle lines passed
    /// to [`Self::upsert_with_details`]. The side-panel row widget tree is a
    /// GTK object and awkward to read directly, so the cache is the source of truth.
    #[cfg(test)]
    #[cfg_attr(all(test, target_os = "macos"), allow(dead_code))]
    pub(crate) fn cached_subtitles(&self, id: WorkspaceId) -> Option<Vec<String>> {
        self.subtitle_cache
            .borrow()
            .get(&id)
            .map(|details| details.path_lines.clone())
    }

    #[cfg(test)]
    #[cfg_attr(all(test, target_os = "macos"), allow(dead_code))]
    pub(crate) fn cached_details(&self, id: WorkspaceId) -> Option<WorkspaceRowDetails> {
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
        NotificationLevel::NeedsInput | NotificationLevel::Error
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
        Some(workspace_dnd_content_provider(id_for_prepare))
    });
    let row_for_begin = row.downgrade();
    drag_source.connect_drag_begin(move |_, _| {
        if let Some(row) = row_for_begin.upgrade() {
            row.set_opacity(0.4);
            row.add_css_class("flowmux-dragging");
        }
    });
    let row_for_end = row.downgrade();
    drag_source.connect_drag_end(move |_, _, _| {
        if let Some(row) = row_for_end.upgrade() {
            row.set_opacity(1.0);
            row.remove_css_class("flowmux-dragging");
        }
    });
    let row_for_cancel = row.downgrade();
    drag_source.connect_drag_cancel(move |_, _, _| {
        if let Some(row) = row_for_cancel.upgrade() {
            row.set_opacity(1.0);
            row.remove_css_class("flowmux-dragging");
        }
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
    let row_for_motion = row.downgrade();
    drop_target.connect_motion(move |_, _x, y| {
        let Some(row) = row_for_motion.upgrade() else {
            return gtk::gdk::DragAction::empty();
        };
        tracing::trace!(target = %target_id_for_motion, y, "sidebar drop motion");
        let height = row.height();
        let above = if height > 0 {
            y < (height as f64) / 2.0
        } else {
            true
        };
        if above {
            row.remove_css_class("flowmux-drop-below");
            row.add_css_class("flowmux-drop-above");
        } else {
            row.remove_css_class("flowmux-drop-above");
            row.add_css_class("flowmux-drop-below");
        }
        gtk::gdk::DragAction::MOVE
    });
    let row_for_leave = row.downgrade();
    drop_target.connect_leave(move |_| {
        if let Some(row) = row_for_leave.upgrade() {
            row.remove_css_class("flowmux-drop-above");
            row.remove_css_class("flowmux-drop-below");
        }
    });
    let row_for_drop = row.downgrade();
    let target_id = id;
    drop_target.connect_drop(move |_, value, _x, y| {
        let Some(row) = row_for_drop.upgrade() else {
            return false;
        };
        tracing::debug!(target = %target_id, "sidebar drop fired");
        row.remove_css_class("flowmux-drop-above");
        row.remove_css_class("flowmux-drop-below");
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
        let row_height = row.height();
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

fn workspace_dnd_content_provider(id: WorkspaceId) -> gtk::gdk::ContentProvider {
    let payload = id.to_string();
    let bytes = gtk::glib::Bytes::from_owned(payload.clone().into_bytes());
    let mime_provider = gtk::gdk::ContentProvider::for_bytes(WORKSPACE_DND_MIME, &bytes);
    let value_provider = gtk::gdk::ContentProvider::for_value(&payload.to_value());
    gtk::gdk::ContentProvider::new_union(&[mime_provider, value_provider])
}

/// Make a side-panel workspace row a drop target for **pane tab** drags.
/// Hovering a tab over the row selects that workspace mid-drag (so the user can
/// keep dragging into one of its panes), and releasing on the row moves the tab
/// to the last position of the workspace's first pane.
fn attach_tab_drop_to_row(
    row: &gtk::ListBoxRow,
    ws_id: WorkspaceId,
    bridge: Bridge,
    tab_drag_drop_seen: Rc<Cell<bool>>,
    tab_drag_drop_committed: Rc<Cell<bool>>,
) {
    let drop_target =
        gtk::DropTargetAsync::new(Some(tab_dnd_content_formats()), gtk::gdk::DragAction::MOVE);
    let bridge_accept = bridge.clone();
    drop_target.connect_accept(move |target, drop| {
        if sidebar_tab_drop_accepts_formats(&drop.formats()) {
            let bridge = bridge_accept.clone();
            gtk::glib::MainContext::default().spawn_local(async move {
                let _ = bridge
                    .tx
                    .send(GtkCommand::ActivateWorkspace { id: ws_id })
                    .await;
            });
            true
        } else {
            target.reject_drop(drop);
            false
        }
    });
    let bridge_drop = bridge.clone();
    drop_target.connect_drop(move |_, drop, _x, _y| {
        tab_drag_drop_seen.set(true);
        tab_drag_drop_committed.set(true);
        let drop = drop.clone();
        let bridge = bridge_drop.clone();
        gtk::glib::MainContext::default().spawn_local(async move {
            let payload = match read_tab_dnd_payload_from_drop(&drop).await {
                Ok(payload) => payload,
                Err(error) => {
                    tracing::warn!(error = %error, "sidebar tab drop: failed to read payload");
                    drop.finish(gtk::gdk::DragAction::empty());
                    return;
                }
            };
            let src_pane = payload.src_pane;
            let surface = payload.src_surface;
            let (ack_tx, ack_rx) = oneshot::channel();
            if let Err(error) = bridge
                .tx
                .send(GtkCommand::MoveSurfaceToWorkspace {
                    src_pane,
                    surface,
                    dst_workspace: ws_id,
                    ack: ack_tx,
                })
                .await
            {
                tracing::warn!(%error, "sidebar tab drop: bridge send failed");
                drop.finish(gtk::gdk::DragAction::empty());
                return;
            }
            match ack_rx.await {
                Ok(Ok(())) => drop.finish(gtk::gdk::DragAction::MOVE),
                Ok(Err(error)) => {
                    tracing::warn!(%error, "sidebar tab drop: move rejected");
                    drop.finish(gtk::gdk::DragAction::empty());
                }
                Err(error) => {
                    tracing::warn!(%error, "sidebar tab drop: move acknowledgement dropped");
                    drop.finish(gtk::gdk::DragAction::empty());
                }
            }
        });
        true
    });
    row.add_controller(drop_target);
}

fn sidebar_tab_drop_accepts_formats(formats: &gtk::gdk::ContentFormats) -> bool {
    !formats.contain_mime_type(WORKSPACE_DND_MIME) && tab_dnd_formats_accept_payload(formats)
}

struct WorkspaceRowWidget {
    root: gtk::Widget,
    badge: gtk::Box,
}

fn row_widget(
    ws: &Workspace,
    details: &WorkspaceRowDetails,
    on_close: Rc<dyn Fn(WorkspaceId)>,
    bridge: Bridge,
) -> WorkspaceRowWidget {
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

    let meta = build_meta_column(ws, details);
    meta.set_hexpand(true);
    meta.set_margin_start(6);
    content.append(&meta);

    let row = gtk::Overlay::new();
    row.set_child(Some(&content));

    let notification_badge = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    notification_badge.add_css_class("flowmux-workspace-notification-dot");
    notification_badge.set_halign(gtk::Align::End);
    notification_badge.set_valign(gtk::Align::Start);
    notification_badge.set_margin_top(7);
    notification_badge.set_margin_end(7);
    notification_badge.set_can_target(false);
    notification_badge.set_visible(false);
    row.add_overlay(&notification_badge);

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

    // Right-click context menu. Not a Popover: popup-surface input
    // grabs proved unreliable on X11 hosts (items intermittently dead
    // within one process run on Ubuntu 22.04 Xorg, host GTK and
    // Flatpak runtime alike), so the menu is drawn inside the window's
    // content overlay instead — see `ui::overlay_menu`. Item closures
    // send the right GtkCommand directly through the bridge.
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_SECONDARY);
    let row_for_click = row.downgrade();
    let on_close_for_menu = on_close.clone();
    click.connect_pressed(move |gesture, _n_press, x, y| {
        let Some(row) = row_for_click.upgrade() else {
            return;
        };
        // Claim the sequence up front so the row's primary-click gesture
        // and the ListBox don't also act on this press.
        gesture.set_state(gtk::EventSequenceState::Claimed);
        flowmux_config::notify_debug!("sidebar/ctxmenu", "menu opened ws={id}");
        use crate::ui::overlay_menu::MenuItem;

        let bridge_for_rename = bridge.clone();
        let rename = MenuItem::Action {
            label: "Change tab name",
            activate: Box::new(move || {
                flowmux_config::notify_debug!("sidebar/ctxmenu", "click rename ws={id}");
                let bridge = bridge_for_rename.clone();
                gtk::glib::MainContext::default().spawn_local(async move {
                    let _ = bridge.tx.send(GtkCommand::ShowRenameDialog { id }).await;
                });
            }),
        };

        let bridge_for_color = bridge.clone();
        let color = MenuItem::Action {
            label: "Change color…",
            activate: Box::new(move || {
                flowmux_config::notify_debug!("sidebar/ctxmenu", "click color ws={id}");
                let bridge = bridge_for_color.clone();
                gtk::glib::MainContext::default().spawn_local(async move {
                    let _ = bridge.tx.send(GtkCommand::ShowColorDialog { id }).await;
                });
            }),
        };

        let on_close_clone = on_close_for_menu.clone();
        let close = MenuItem::Action {
            label: "Close tab",
            activate: Box::new(move || {
                flowmux_config::notify_debug!("sidebar/ctxmenu", "click close-tab ws={id}");
                on_close_clone(id);
            }),
        };

        // Close every open workspace at once. The dispatcher shows a
        // single confirmation before tearing them all down.
        let bridge_for_close_all = bridge.clone();
        let close_all = MenuItem::Action {
            label: "Close all tabs",
            activate: Box::new(move || {
                flowmux_config::notify_debug!("sidebar/ctxmenu", "click close-all ws={id}");
                let bridge = bridge_for_close_all.clone();
                gtk::glib::MainContext::default().spawn_local(async move {
                    let (ack, rx) = tokio::sync::oneshot::channel();
                    let _ = bridge
                        .tx
                        .send(GtkCommand::RemoveAllWorkspaces { ack })
                        .await;
                    let _ = rx.await;
                });
            }),
        };

        // Open the focused pane's cwd in the system file manager
        // (Nautilus on a default Ubuntu/GNOME install). The dispatcher
        // resolves "focused pane" inside this workspace and falls back
        // to its first leaf pane.
        let bridge_for_show = bridge.clone();
        let show_folder = MenuItem::Action {
            label: "Show in folder",
            activate: Box::new(move || {
                flowmux_config::notify_debug!("sidebar/ctxmenu", "click show-folder ws={id}");
                let bridge = bridge_for_show.clone();
                gtk::glib::MainContext::default().spawn_local(async move {
                    let _ = bridge
                        .tx
                        .send(GtkCommand::ShowFocusedPaneFolder { workspace: id })
                        .await;
                });
            }),
        };

        // Copy the focused-pane text identifier — cwd for terminal,
        // URL for browser — to the clipboard. The dispatcher routes
        // based on the active surface kind, so one item covers both
        // cases without forcing the user to pick.
        let bridge_for_copy = bridge.clone();
        let copy_path = MenuItem::Action {
            label: "Copy path",
            activate: Box::new(move || {
                flowmux_config::notify_debug!("sidebar/ctxmenu", "click copy-path ws={id}");
                let bridge = bridge_for_copy.clone();
                gtk::glib::MainContext::default().spawn_local(async move {
                    let _ = bridge
                        .tx
                        .send(GtkCommand::CopyFocusedPaneText { workspace: id })
                        .await;
                });
            }),
        };

        crate::ui::overlay_menu::show_at(
            &row,
            x,
            y,
            vec![
                rename,
                color,
                MenuItem::Separator,
                close,
                close_all,
                MenuItem::Separator,
                show_folder,
                copy_path,
            ],
        );
    });
    row.add_controller(click);

    WorkspaceRowWidget {
        root: row.upcast(),
        badge: notification_badge,
    }
}

fn color_bar(color: &str) -> gtk::Widget {
    let bar = gtk::DrawingArea::new();
    bar.set_size_request(4, -1);
    bar.set_vexpand(true);
    bar.set_valign(gtk::Align::Fill);
    // Stable workspace color indicator. Agent state colors are rendered
    // elsewhere on the row; the bar itself does not animate.
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

fn apply_agent_status_class(row: &gtk::ListBoxRow, status: Option<AgentStatus>) {
    row.remove_css_class("flowmux-agent-running");
    row.remove_css_class("flowmux-agent-blocked");
    row.remove_css_class("flowmux-agent-done");
    match status {
        Some(AgentStatus::Working) => row.add_css_class("flowmux-agent-running"),
        Some(AgentStatus::Blocked) => row.add_css_class("flowmux-agent-blocked"),
        Some(AgentStatus::Done) => row.add_css_class("flowmux-agent-done"),
        Some(AgentStatus::Idle | AgentStatus::Unknown) | None => {}
    }
}

fn build_meta_column(ws: &Workspace, details: &WorkspaceRowDetails) -> gtk::Box {
    // Layout:
    //   line 1: workspace display title, custom_title if present, otherwise name
    //   line 2..: optional agent blocks followed by up to 3 shortened MRU cwd
    //             paths in .../A/B/C form.
    //   optional aux: linked PR badge / listening ports.
    // Tree gutter segments live in each metadata row. A zero box spacing keeps
    // adjacent 1px vertical segments continuous; the labels' own line heights
    // still provide the same compact visual rhythm.
    let v = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let title = gtk::Label::new(Some(ws.display_title()));
    title.set_halign(gtk::Align::Start);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_xalign(0.0);
    title.add_css_class("heading");
    title.set_margin_bottom(1);
    v.append(&title);

    let aux = workspace_aux_row(ws);
    for layout in metadata_tree_layout(details, aux.is_some()) {
        let child: gtk::Widget = match layout.row {
            MetadataTreeRow::AgentHeader(index) => {
                agent_block_header(&details.agent_blocks[index]).upcast()
            }
            MetadataTreeRow::AgentStatus(index) => {
                let block = &details.agent_blocks[index];
                let status = gtk::Label::new(Some(block.status_text.as_str()));
                status.set_halign(gtk::Align::Start);
                status.set_xalign(0.0);
                status.set_ellipsize(gtk::pango::EllipsizeMode::End);
                status.set_single_line_mode(true);
                status.set_lines(1);
                status.set_wrap(false);
                status.add_css_class("caption");
                status.add_css_class("dim-label");
                status.upcast()
            }
            MetadataTreeRow::AgentPath(index) => sidebar_path_label(
                details.agent_blocks[index]
                    .path
                    .as_deref()
                    .expect("AgentPath layout requires a path"),
                false,
            )
            .upcast(),
            MetadataTreeRow::Path(index) => sidebar_path_label(
                details.path_lines[index].as_str(),
                index == 0 && details.agent_blocks.is_empty(),
            )
            .upcast(),
            MetadataTreeRow::Aux => aux
                .as_ref()
                .expect("Aux layout requires an auxiliary row")
                .clone()
                .upcast(),
        };
        v.append(&sidebar_tree_row(&child, layout.line));
    }

    v
}

fn workspace_aux_row(ws: &Workspace) -> Option<gtk::Box> {
    // Auxiliary metadata is a workspace-level child, alongside agent blocks
    // and cwd paths, so it receives the same top-level tree connector.
    let aux = gtk::Box::new(gtk::Orientation::Horizontal, 6);
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
    }

    aux.first_child().map(|_| aux)
}

fn metadata_tree_layout(
    details: &WorkspaceRowDetails,
    has_aux: bool,
) -> Vec<MetadataTreeRowLayout> {
    let path_count = details.path_lines.len().min(3);
    let top_level_count = details.agent_blocks.len() + path_count + usize::from(has_aux);
    let mut top_level_index = 0;
    let mut rows = Vec::new();

    for (agent_index, block) in details.agent_blocks.iter().enumerate() {
        let has_following_top_level = top_level_index + 1 < top_level_count;
        rows.push(MetadataTreeRowLayout {
            row: MetadataTreeRow::AgentHeader(agent_index),
            line: TreeLine::top_level(has_following_top_level),
        });
        rows.push(MetadataTreeRowLayout {
            row: MetadataTreeRow::AgentStatus(agent_index),
            line: TreeLine::nested(has_following_top_level, block.path.is_some()),
        });
        if block.path.is_some() {
            rows.push(MetadataTreeRowLayout {
                row: MetadataTreeRow::AgentPath(agent_index),
                line: TreeLine::nested(has_following_top_level, false),
            });
        }
        top_level_index += 1;
    }

    for path_index in 0..path_count {
        let has_following_top_level = top_level_index + 1 < top_level_count;
        rows.push(MetadataTreeRowLayout {
            row: MetadataTreeRow::Path(path_index),
            line: TreeLine::top_level(has_following_top_level),
        });
        top_level_index += 1;
    }

    if has_aux {
        rows.push(MetadataTreeRowLayout {
            row: MetadataTreeRow::Aux,
            line: TreeLine::top_level(false),
        });
    }

    rows
}

fn sidebar_tree_row(child: &impl IsA<gtk::Widget>, line: TreeLine) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.set_hexpand(true);
    row.add_css_class("flowmux-sidebar-tree-row");

    let gutter_width =
        (line.ancestor_continuations.len() as i32 + 1).saturating_mul(SIDEBAR_TREE_GUTTER_WIDTH);
    let gutter = gtk::DrawingArea::builder()
        .content_width(gutter_width)
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    gutter.set_widget_name("flowmux-sidebar-tree-gutter");
    gutter.add_css_class("flowmux-sidebar-tree-gutter");
    gutter.set_can_target(false);
    gutter.set_focusable(false);
    gutter.set_vexpand(true);
    gutter.set_draw_func(move |area, cr, width, height| {
        let color = area.color();
        cr.set_source_rgba(
            color.red() as f64,
            color.green() as f64,
            color.blue() as f64,
            color.alpha() as f64,
        );
        cr.set_line_width(1.0);
        cr.set_line_cap(gtk::cairo::LineCap::Butt);
        for segment in tree_segments(&line, width, height) {
            cr.move_to(segment.x1, segment.y1);
            cr.line_to(segment.x2, segment.y2);
        }
        let _ = cr.stroke();
    });
    row.append(&gutter);

    child.set_hexpand(true);
    row.append(child);
    row
}

fn tree_segments(line: &TreeLine, width: i32, height: i32) -> Vec<TreeSegment> {
    let width = width.max(1) as f64;
    let height = height.max(1) as f64;
    let mid_y = (height / 2.0).floor() + 0.5;
    let mut segments = Vec::with_capacity(line.ancestor_continuations.len() + 2);

    for (depth, continues) in line.ancestor_continuations.iter().copied().enumerate() {
        if continues {
            let x = depth as f64 * SIDEBAR_TREE_GUTTER_WIDTH as f64 + 6.5;
            segments.push(TreeSegment {
                x1: x,
                y1: 0.0,
                x2: x,
                y2: height,
            });
        }
    }

    let depth = line.ancestor_continuations.len();
    let x = depth as f64 * SIDEBAR_TREE_GUTTER_WIDTH as f64 + 6.5;
    let branch_end_y = match line.branch {
        TreeBranch::Middle => height,
        TreeBranch::Last => mid_y,
    };
    segments.push(TreeSegment {
        x1: x,
        y1: 0.0,
        x2: x,
        y2: branch_end_y,
    });
    segments.push(TreeSegment {
        x1: x,
        y1: mid_y,
        x2: width - 0.5,
        y2: mid_y,
    });

    segments
}

fn agent_block_header(block: &WorkspaceRowAgentBlock) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    row.set_halign(gtk::Align::Start);

    let icon = gtk::Image::from_icon_name(agent_status_icon_name(block.status, block.seen));
    icon.set_pixel_size(12);
    icon.add_css_class(agent_status_css_class(block.status, block.seen));
    row.append(&icon);

    let label_text = agent_block_label_text(block);
    let label = gtk::Label::new(Some(&label_text));
    label.set_halign(gtk::Align::Start);
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.add_css_class("caption");
    label.add_css_class(agent_status_css_class(block.status, block.seen));
    row.append(&label);

    row
}

fn agent_block_label_text(block: &WorkspaceRowAgentBlock) -> String {
    if block.overflow_count == 0 {
        return block.agent_name.clone();
    }
    let suffix = if block.overflow_count == 1 {
        "agent"
    } else {
        "agents"
    };
    format!("{} +{} {}", block.agent_name, block.overflow_count, suffix)
}

fn sidebar_path_label(line: &str, primary: bool) -> gtk::Label {
    let label = gtk::Label::new(Some(line));
    label.set_halign(gtk::Align::Start);
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    label.add_css_class("caption");
    label.add_css_class("dim-label");
    if primary {
        label.add_css_class("flowmux-sidebar-subtitle-primary");
    }
    label
}

fn agent_status_icon_name(status: AgentStatus, seen: bool) -> &'static str {
    match status {
        AgentStatus::Blocked => "dialog-warning-symbolic",
        AgentStatus::Working => "process-working-symbolic",
        AgentStatus::Done if !seen => "emblem-ok-symbolic",
        AgentStatus::Done | AgentStatus::Idle => "media-playback-pause-symbolic",
        AgentStatus::Unknown => "dialog-question-symbolic",
    }
}

fn agent_status_css_class(status: AgentStatus, seen: bool) -> &'static str {
    match status {
        AgentStatus::Blocked if !seen => "flowmux-sidebar-agent-blocked",
        AgentStatus::Blocked => "flowmux-sidebar-agent-idle",
        AgentStatus::Working => "flowmux-sidebar-agent-working",
        AgentStatus::Done if !seen => "flowmux-sidebar-agent-done",
        AgentStatus::Done | AgentStatus::Idle => "flowmux-sidebar-agent-idle",
        AgentStatus::Unknown => "flowmux-sidebar-agent-unknown",
    }
}

#[cfg(test)]
mod tests {
    #![cfg_attr(target_os = "macos", allow(dead_code, unused_imports))]

    use super::*;
    use flowmux_core::{Pane, PaneContent, PaneId, PaneSurface, Surface, SurfaceId, SurfaceKind};
    use std::path::PathBuf;

    fn agent_block(name: &str, path: Option<&str>) -> WorkspaceRowAgentBlock {
        WorkspaceRowAgentBlock {
            agent_name: name.into(),
            status: AgentStatus::Working,
            seen: true,
            status_text: "running tests".into(),
            path: path.map(str::to_owned),
            overflow_count: 0,
        }
    }

    fn layout(row: MetadataTreeRow, line: TreeLine) -> MetadataTreeRowLayout {
        MetadataTreeRowLayout { row, line }
    }

    #[cfg(not(target_os = "macos"))]
    fn tree_gutter_widths(meta: &gtk::Box) -> Vec<i32> {
        let mut widths = Vec::new();
        let mut child = meta.first_child();
        while let Some(widget) = child {
            if widget.has_css_class("flowmux-sidebar-tree-row") {
                let row = widget
                    .clone()
                    .downcast::<gtk::Box>()
                    .expect("tree row must be a box");
                let gutter = row
                    .first_child()
                    .expect("tree row must have a gutter")
                    .downcast::<gtk::DrawingArea>()
                    .expect("tree gutter must be a drawing area");
                widths.push(gutter.content_width());
            }
            child = widget.next_sibling();
        }
        widths
    }

    #[cfg(not(target_os = "macos"))]
    fn descendant_labels(widget: &impl IsA<gtk::Widget>) -> Vec<gtk::Label> {
        fn visit(widget: &gtk::Widget, labels: &mut Vec<gtk::Label>) {
            if let Ok(label) = widget.clone().downcast::<gtk::Label>() {
                labels.push(label);
            }
            let mut child = widget.first_child();
            while let Some(current) = child {
                visit(&current, labels);
                child = current.next_sibling();
            }
        }

        let mut labels = Vec::new();
        visit(widget.as_ref(), &mut labels);
        labels
    }

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

    #[test]
    fn metadata_tree_layout_is_empty_without_metadata() {
        assert!(metadata_tree_layout(&WorkspaceRowDetails::default(), false).is_empty());
    }

    #[test]
    fn metadata_tree_layout_marks_direct_path_siblings_and_last_child() {
        let one = WorkspaceRowDetails::path_only(&[".../one".into()]);
        assert_eq!(
            metadata_tree_layout(&one, false),
            vec![layout(MetadataTreeRow::Path(0), TreeLine::top_level(false),)]
        );

        let three = WorkspaceRowDetails::path_only(&[
            ".../one".into(),
            ".../two".into(),
            ".../three".into(),
        ]);
        assert_eq!(
            metadata_tree_layout(&three, false),
            vec![
                layout(MetadataTreeRow::Path(0), TreeLine::top_level(true),),
                layout(MetadataTreeRow::Path(1), TreeLine::top_level(true),),
                layout(MetadataTreeRow::Path(2), TreeLine::top_level(false),),
            ]
        );
    }

    #[test]
    fn metadata_tree_layout_nests_agent_status_and_optional_path() {
        let details = WorkspaceRowDetails {
            agent_blocks: vec![agent_block("codex", None)],
            path_lines: vec![],
        };
        assert_eq!(
            metadata_tree_layout(&details, false),
            vec![
                layout(MetadataTreeRow::AgentHeader(0), TreeLine::top_level(false),),
                layout(
                    MetadataTreeRow::AgentStatus(0),
                    TreeLine::nested(false, false),
                ),
            ]
        );

        let details = WorkspaceRowDetails {
            agent_blocks: vec![agent_block("codex", Some(".../agent/path"))],
            path_lines: vec![".../fallback/path".into()],
        };
        assert_eq!(
            metadata_tree_layout(&details, true),
            vec![
                layout(MetadataTreeRow::AgentHeader(0), TreeLine::top_level(true),),
                layout(
                    MetadataTreeRow::AgentStatus(0),
                    TreeLine::nested(true, true),
                ),
                layout(MetadataTreeRow::AgentPath(0), TreeLine::nested(true, false),),
                layout(MetadataTreeRow::Path(0), TreeLine::top_level(true),),
                layout(MetadataTreeRow::Aux, TreeLine::top_level(false)),
            ]
        );
    }

    #[test]
    fn metadata_tree_layout_keeps_first_agent_open_until_final_agent() {
        let details = WorkspaceRowDetails {
            agent_blocks: vec![
                agent_block("codex", Some(".../codex")),
                agent_block("claude", None),
            ],
            path_lines: vec![],
        };
        assert_eq!(
            metadata_tree_layout(&details, false),
            vec![
                layout(MetadataTreeRow::AgentHeader(0), TreeLine::top_level(true),),
                layout(
                    MetadataTreeRow::AgentStatus(0),
                    TreeLine::nested(true, true),
                ),
                layout(MetadataTreeRow::AgentPath(0), TreeLine::nested(true, false),),
                layout(MetadataTreeRow::AgentHeader(1), TreeLine::top_level(false),),
                layout(
                    MetadataTreeRow::AgentStatus(1),
                    TreeLine::nested(false, false),
                ),
            ]
        );
    }

    #[test]
    fn metadata_tree_layout_caps_paths_and_ignores_agent_label_overflow() {
        let mut details = WorkspaceRowDetails {
            agent_blocks: vec![agent_block("codex", None)],
            path_lines: vec!["one".into(), "two".into(), "three".into(), "four".into()],
        };
        let baseline = metadata_tree_layout(&details, false);
        assert_eq!(
            baseline
                .iter()
                .filter(|row| matches!(row.row, MetadataTreeRow::Path(_)))
                .count(),
            3
        );
        assert!(!baseline
            .iter()
            .any(|row| row.row == MetadataTreeRow::Path(3)));

        details.agent_blocks[0].overflow_count = 42;
        assert_eq!(metadata_tree_layout(&details, false), baseline);
    }

    #[test]
    fn tree_segments_draw_middle_last_and_nested_connectors_within_gutter() {
        let assert_within = |segments: &[TreeSegment], width: f64, height: f64| {
            for segment in segments {
                assert!((0.0..=width).contains(&segment.x1));
                assert!((0.0..=width).contains(&segment.x2));
                assert!((0.0..=height).contains(&segment.y1));
                assert!((0.0..=height).contains(&segment.y2));
            }
        };

        for height in [1, 14, 17, 32] {
            let middle = tree_segments(&TreeLine::top_level(true), 14, height);
            let last = tree_segments(&TreeLine::top_level(false), 14, height);
            assert_eq!(middle.len(), 2);
            assert_eq!(last.len(), 2);
            assert_eq!(middle[0].y2, height.max(1) as f64);
            assert_eq!(last[0].y2, (height.max(1) as f64 / 2.0).floor() + 0.5);
            assert_eq!(middle[1].x2, 13.5);

            let nested = tree_segments(&TreeLine::nested(true, false), 28, height);
            assert_eq!(nested.len(), 3);
            assert_eq!(nested[0].x1, 6.5);
            assert_eq!(nested[1].x1, 20.5);
            assert_eq!(nested[2].x2, 27.5);
            assert_within(&middle, 14.0, height.max(1) as f64);
            assert_within(&last, 14.0, height.max(1) as f64);
            assert_within(&nested, 28.0, height.max(1) as f64);
        }

        let no_outer_continuation = tree_segments(&TreeLine::nested(false, false), 28, 16);
        assert_eq!(no_outer_continuation.len(), 2);
        assert!(no_outer_continuation
            .iter()
            .all(|segment| segment.x1 != 6.5));
    }

    /// Smoke test that row_widget can build a stable widget tree with a name
    /// and subtitle lines. Requires GTK init, so headless environments skip it.
    #[cfg(not(target_os = "macos"))]
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
            let details = WorkspaceRowDetails::path_only(&lines);
            let _ = row_widget(&ws, &details, on_close.clone(), bridge.clone());
        }
        // Even with 4 lines, WorkspaceRowDetails truncates to 3.
        let four = vec!["a".into(), "b".into(), "c".into(), "d-overflow".into()];
        let details = WorkspaceRowDetails::path_only(&four);
        let _ = row_widget(&ws, &details, on_close, bridge);
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn row_widget_builds_with_agent_blocks_and_paths() {
        if gtk::init().is_err() {
            return;
        }
        let ws = ws_with_active_terminal_cwd(Some(PathBuf::from("/home/u/dev/os/flowmux")));
        let bridge = crate::bridge::Bridge::new().0;
        let on_close: Rc<dyn Fn(WorkspaceId)> = Rc::new(|_| {});
        let details = WorkspaceRowDetails {
            agent_blocks: vec![WorkspaceRowAgentBlock {
                agent_name: "codex".into(),
                status: AgentStatus::Working,
                seen: true,
                status_text: "running tests".into(),
                path: Some(".../dev/os/flowmux".into()),
                overflow_count: 1,
            }],
            path_lines: vec![".../fallback/path".into()],
        };
        let _ = row_widget(&ws, &details, on_close, bridge);
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn metadata_widgets_have_expected_tree_gutter_depths() {
        if gtk::init().is_err() {
            return;
        }
        let mut ws = ws_with_active_terminal_cwd(Some(PathBuf::from("/tmp/flowmux")));

        let empty = build_meta_column(&ws, &WorkspaceRowDetails::default());
        assert!(tree_gutter_widths(&empty).is_empty());

        let details = WorkspaceRowDetails {
            agent_blocks: vec![agent_block("codex", Some(".../dev/os/flowmux"))],
            path_lines: vec![".../fallback/path".into()],
        };
        let with_agent = build_meta_column(&ws, &details);
        assert_eq!(tree_gutter_widths(&with_agent), vec![14, 28, 28, 14]);

        ws.listening_ports = vec![8080];
        let aux_only = build_meta_column(&ws, &WorkspaceRowDetails::default());
        assert_eq!(tree_gutter_widths(&aux_only), vec![14]);
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn long_tree_metadata_remains_ellipsized_at_narrow_widths() {
        if gtk::init().is_err() {
            return;
        }
        let mut ws = ws_with_active_terminal_cwd(Some(PathBuf::from("/tmp/flowmux")));
        ws.name = "an-extremely-long-workspace-name-that-must-fit-a-narrow-panel".into();
        let details = WorkspaceRowDetails {
            agent_blocks: vec![agent_block(
                "an-extremely-long-agent-name-that-must-be-truncated",
                Some(".../a/very/long/agent/path/that/must/be/truncated"),
            )],
            path_lines: vec![".../another/very/long/fallback/path/to/truncate".into()],
        };
        let meta = build_meta_column(&ws, &details);
        meta.set_size_request(96, -1);

        let labels = descendant_labels(&meta);
        assert!(labels.iter().any(|label| {
            label.label().contains("long-agent-name")
                && label.ellipsize() == gtk::pango::EllipsizeMode::End
        }));
        assert_eq!(
            labels
                .iter()
                .filter(|label| label.label().contains("long/") && label.has_css_class("caption"))
                .count(),
            2
        );
        assert!(labels
            .iter()
            .filter(|label| label.label().contains("long/"))
            .all(|label| label.ellipsize() == gtk::pango::EllipsizeMode::Middle));
        assert_eq!(tree_gutter_widths(&meta), vec![14, 28, 28, 14]);
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn redrawing_and_removing_workspace_releases_old_widgets() {
        if gtk::init().is_err() {
            return;
        }
        let bridge = crate::bridge::Bridge::new().0;
        let sidebar = Sidebar::new(|_| {}, |_| {}, bridge, NotificationStore::new(), None);
        let mut ws = ws_with_active_terminal_cwd(Some(PathBuf::from("/tmp/worktree")));
        sidebar.upsert(&ws);

        let row = sidebar.rows.borrow()[0].1.clone();
        let old_child = row.child().expect("workspace row child").downgrade();
        let old_row = row.downgrade();
        drop(row);

        ws.name = "updated".into();
        sidebar.upsert(&ws);
        assert!(
            old_child.upgrade().is_none(),
            "redraw retained the replaced widget tree"
        );

        sidebar.remove(ws.id);
        assert!(
            old_row.upgrade().is_none(),
            "removal retained the detached list row"
        );
    }

    #[test]
    fn agent_block_label_text_marks_overflow_count_with_agent_word() {
        let mut block = WorkspaceRowAgentBlock {
            agent_name: "claude".into(),
            status: AgentStatus::Working,
            seen: true,
            status_text: "running tests".into(),
            path: None,
            overflow_count: 0,
        };

        assert_eq!(agent_block_label_text(&block), "claude");
        block.overflow_count = 1;
        assert_eq!(agent_block_label_text(&block), "claude +1 agent");
        block.overflow_count = 3;
        assert_eq!(agent_block_label_text(&block), "claude +3 agents");
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn footer_orders_usage_worktrees_and_file_browser_buttons() {
        if gtk::init().is_err() {
            return;
        }
        let bridge = crate::bridge::Bridge::new().0;
        let sidebar = Sidebar::new(|_| {}, |_| {}, bridge, NotificationStore::new(), None);
        let footer = sidebar
            .root
            .last_child()
            .unwrap()
            .downcast::<gtk::Box>()
            .unwrap();
        let mut names = Vec::new();
        let mut child = footer.first_child();
        while let Some(widget) = child {
            names.push(widget.widget_name().to_string());
            child = widget.next_sibling();
        }

        let usage = names
            .iter()
            .position(|name| name == "flowmux-usage-button")
            .expect("usage button must exist");
        assert_eq!(
            sidebar.usage_button().tooltip_text().as_deref(),
            Some("AI usage (Ctrl+Alt+U)")
        );
        let worktrees = names
            .iter()
            .position(|name| name == "flowmux-worktree-button")
            .expect("worktree button must exist");
        let folder = names
            .iter()
            .position(|name| name == "flowmux-file-browser-button")
            .expect("file browser button must exist");
        assert_eq!(usage + 1, worktrees);
        assert_eq!(worktrees + 1, folder);
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn workspace_titles_track_display_title_through_rename() {
        if gtk::init().is_err() {
            return;
        }
        let bridge = crate::bridge::Bridge::new().0;
        let sidebar = Sidebar::new(|_| {}, |_| {}, bridge, NotificationStore::new(), None);
        let titles = sidebar.workspace_titles();

        let mut ws = ws_with_active_terminal_cwd(Some(PathBuf::from("/home/u/dev/projA")));
        ws.name = "projA".into();
        sidebar.upsert(&ws);
        assert_eq!(
            titles
                .borrow()
                .iter()
                .find(|(id, _)| *id == ws.id)
                .map(|(_, n)| n.clone()),
            Some("projA".to_string()),
            "auto name shows when no custom title",
        );

        // A rename sets custom_title; the cache must follow display_title.
        ws.custom_title = Some("MyName".into());
        sidebar.upsert(&ws);
        assert_eq!(
            titles
                .borrow()
                .iter()
                .find(|(id, _)| *id == ws.id)
                .map(|(_, n)| n.clone()),
            Some("MyName".to_string()),
            "custom title is reflected after rename",
        );
    }

    #[test]
    fn workspace_drag_formats_are_rejected_by_sidebar_tab_target() {
        let workspace_provider = workspace_dnd_content_provider(WorkspaceId::new());
        let formats = workspace_provider.formats();

        assert!(formats.contain_mime_type(WORKSPACE_DND_MIME));
        assert!(formats.contains_type(gtk::glib::types::Type::STRING));
        assert!(!sidebar_tab_drop_accepts_formats(&formats));
        assert!(sidebar_tab_drop_accepts_formats(&tab_dnd_content_formats()));

        let legacy_tab_formats = gtk::gdk::ContentFormats::builder()
            .add_type(gtk::glib::types::Type::STRING)
            .build();
        assert!(sidebar_tab_drop_accepts_formats(&legacy_tab_formats));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn removing_workspace_preserves_survivor_order_for_followup_reorder() {
        if gtk::init().is_err() {
            return;
        }
        let bridge = crate::bridge::Bridge::new().0;
        let sidebar = Sidebar::new(|_| {}, |_| {}, bridge, NotificationStore::new(), None);

        let workspaces: Vec<Workspace> = (0..4)
            .map(|index| {
                let mut ws = ws_with_active_terminal_cwd(Some(PathBuf::from(format!(
                    "/tmp/flowmux-sidebar-order-{index}"
                ))));
                ws.name = format!("workspace-{index}");
                ws
            })
            .collect();
        for ws in &workspaces {
            sidebar.upsert(ws);
        }
        let ids: Vec<WorkspaceId> = workspaces.iter().map(|ws| ws.id).collect();

        sidebar.remove(ids[1]);

        let row_ids: Vec<WorkspaceId> = sidebar.rows.borrow().iter().map(|(id, _)| *id).collect();
        assert_eq!(row_ids, vec![ids[0], ids[2], ids[3]]);
        let row_positions: Vec<i32> = sidebar
            .rows
            .borrow()
            .iter()
            .map(|(_, row)| row.index())
            .collect();
        assert_eq!(row_positions, vec![0, 1, 2]);

        sidebar.select_workspace(ids[3]);
        assert_eq!(sidebar.selected_workspace(), Some(ids[3]));
        sidebar.reorder(ids[3], 0);

        let expected = vec![ids[3], ids[0], ids[2]];
        let reordered_rows: Vec<WorkspaceId> =
            sidebar.rows.borrow().iter().map(|(id, _)| *id).collect();
        let reordered_titles: Vec<WorkspaceId> = sidebar
            .workspace_titles()
            .borrow()
            .iter()
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(reordered_rows, expected);
        assert_eq!(reordered_titles, expected);
        assert_eq!(sidebar.selected_workspace(), Some(ids[3]));
        let reordered_positions: Vec<i32> = sidebar
            .rows
            .borrow()
            .iter()
            .map(|(_, row)| row.index())
            .collect();
        assert_eq!(reordered_positions, vec![0, 1, 2]);
    }
}
