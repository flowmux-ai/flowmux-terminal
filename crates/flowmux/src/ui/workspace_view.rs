// SPDX-License-Identifier: GPL-3.0-or-later
//! Render a workspace's pane tree as recursive GTK widgets.
//!
//! A split node becomes `gtk::Paned`; a leaf pane becomes a framed
//! cmux-style pane with a surface tab bar and a stack of terminal or
//! browser panels.

use crate::theme::ResolvedTheme;
use crate::ui::browser_pane::BrowserPane;
use crate::ui::editor_pane::EditorPane;
use crate::ui::ghostty_pane::{is_flatpak_sandbox, GhosttyPane};
use crate::ui::pane_terminal::{PaneCallbacks, PaneTerminal, TabDropCommand};
use flowmux_core::{
    terminal_tab_title_for_cwd, Pane, PaneContent, PaneId, PaneSurface, SplitDirection, Surface,
    SurfaceId, SurfaceKind, Workspace, WorkspaceId,
};
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) const TAB_DND_MIME: &str = "application/x-flowmux-tab";
pub(crate) const TAB_DND_PAYLOAD_MAX: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct TabDndPayload {
    pub src_pane: PaneId,
    pub src_surface: SurfaceId,
    pub surface: Option<PaneSurface>,
}

pub(crate) fn tab_dnd_content_formats() -> gtk::gdk::ContentFormats {
    gtk::gdk::ContentFormats::builder()
        .add_mime_type(TAB_DND_MIME)
        .add_type(gtk::glib::types::Type::STRING)
        .build()
}

pub(crate) fn tab_dnd_formats_accept_payload(formats: &gtk::gdk::ContentFormats) -> bool {
    formats.contain_mime_type(TAB_DND_MIME) || formats.contains_type(gtk::glib::types::Type::STRING)
}

fn tab_dnd_content_provider(payload: String) -> gtk::gdk::ContentProvider {
    let bytes = gtk::glib::Bytes::from_owned(payload.clone().into_bytes());
    let mime_provider = gtk::gdk::ContentProvider::for_bytes(TAB_DND_MIME, &bytes);
    let value_provider = gtk::gdk::ContentProvider::for_value(&payload.to_value());
    gtk::gdk::ContentProvider::new_union(&[mime_provider, value_provider])
}

pub(crate) async fn read_tab_dnd_payload_from_drop(
    drop: &gtk::gdk::Drop,
) -> Result<TabDndPayload, String> {
    let payload = read_tab_dnd_payload_text(drop).await?;
    parse_tab_dnd_payload(&payload).map_err(|error| format!("payload invalid: {error}"))
}

async fn read_tab_dnd_payload_text(drop: &gtk::gdk::Drop) -> Result<String, String> {
    let mut mime_error = None;
    if drop.formats().contain_mime_type(TAB_DND_MIME) {
        match drop
            .read_future(&[TAB_DND_MIME], gtk::glib::Priority::default())
            .await
        {
            Ok((stream, mime_type)) if mime_type.as_str() == TAB_DND_MIME => {
                let bytes = stream
                    .read_bytes_future(TAB_DND_PAYLOAD_MAX, gtk::glib::Priority::default())
                    .await
                    .map_err(|error| format!("failed to read MIME payload bytes: {error}"))?;
                return std::str::from_utf8(bytes.as_ref())
                    .map(str::to_string)
                    .map_err(|error| format!("MIME payload was not UTF-8: {error}"));
            }
            Ok((_stream, mime_type)) => {
                mime_error = Some(format!("unexpected MIME payload type: {mime_type}"));
            }
            Err(error) => {
                mime_error = Some(format!("failed to read MIME payload: {error}"));
            }
        }
    }

    if drop.formats().contains_type(gtk::glib::types::Type::STRING) {
        let value = drop
            .read_value_future(
                gtk::glib::types::Type::STRING,
                gtk::glib::Priority::default(),
            )
            .await
            .map_err(|error| format!("failed to read String payload: {error}"))?;
        return value
            .get::<String>()
            .map_err(|error| format!("String payload was not a string: {error}"));
    }

    Err(mime_error.unwrap_or_else(|| "drop had no tab payload format".to_string()))
}

fn tab_dnd_payload_string(pane: PaneId, surface: SurfaceId, surface_model: &PaneSurface) -> String {
    match serde_json::to_string(surface_model) {
        Ok(surface_json) => format!("{pane}|{surface}|{surface_json}"),
        Err(error) => {
            tracing::warn!(%surface, %error, "tab drag: failed to serialize surface model");
            format!("{pane}|{surface}")
        }
    }
}

/// Return the leaf pane id when the workspace is "solo" — the first
/// (rendered) surface's root is a single leaf holding a single tab.
/// Used to suppress the focus border in a trivial 1-pane/1-tab
/// workspace. `build_workspace_widget` already only renders
/// `ws.surfaces.first()`, so we mirror that scope here.
pub fn solo_workspace_pane(ws: &Workspace) -> Option<PaneId> {
    match &ws.surfaces.first()?.root_pane {
        Pane::Leaf { id, content } if content.surface_count() == 1 => Some(*id),
        _ => None,
    }
}

#[derive(Default)]
pub struct PaneRegistry {
    pub terminals: HashMap<SurfaceId, PaneTerminal>,
    pub browsers: HashMap<SurfaceId, BrowserPane>,
    pub editors: HashMap<SurfaceId, EditorPane>,
    active_terminal_by_pane: HashMap<PaneId, SurfaceId>,
    active_browser_by_pane: HashMap<PaneId, SurfaceId>,
    active_editor_by_pane: HashMap<PaneId, SurfaceId>,
    pane_frames: HashMap<PaneId, gtk::Widget>,
    surface_stacks: HashMap<PaneId, gtk::Stack>,
    pub surface_tabs: HashMap<PaneId, Vec<(SurfaceId, gtk::Widget)>>,
    /// Tab-bar `gtk::Box` so incremental tab additions can `append`
    /// into the same row instead of rebuilding the whole pane.
    pane_tab_containers: HashMap<PaneId, gtk::Box>,
    pane_zoom_badges: HashMap<PaneId, gtk::Label>,
    surface_tab_labels: HashMap<SurfaceId, gtk::Label>,
    pane_workspace: HashMap<PaneId, WorkspaceId>,
    surface_workspace: HashMap<SurfaceId, WorkspaceId>,
    /// PaneId of a Pane::Split node -> the `gtk::Paned` widget representing it.
    /// On exit, compute the ratio from paned.position()/width/height, persist it
    /// in the store, and restore the same ratio on next launch.
    split_paneds: HashMap<PaneId, gtk::Paned>,
    split_workspace: HashMap<PaneId, WorkspaceId>,
}

pub struct TornOffSurface {
    pub pane: PaneId,
    pub surface: SurfaceId,
    pub title: String,
    pub kind: SurfaceKind,
    pub content: gtk::Widget,
    pub focus: gtk::Widget,
}

impl PaneRegistry {
    pub fn pane_for_surface(&self, surface: SurfaceId) -> Option<PaneId> {
        self.surface_tabs.iter().find_map(|(pane, tabs)| {
            tabs.iter()
                .any(|(candidate, _)| *candidate == surface)
                .then_some(*pane)
        })
    }

    pub fn active_terminal(&self, pane: PaneId) -> Option<&PaneTerminal> {
        self.active_terminal_by_pane
            .get(&pane)
            .and_then(|surface| self.terminals.get(surface))
    }

    pub fn active_browser(&self, pane: PaneId) -> Option<&BrowserPane> {
        self.active_browser_by_pane
            .get(&pane)
            .and_then(|surface| self.browsers.get(surface))
    }

    pub fn active_editor(&self, pane: PaneId) -> Option<&EditorPane> {
        self.active_editor_by_pane
            .get(&pane)
            .and_then(|surface| self.editors.get(surface))
    }

    pub fn pane_frame(&self, pane: PaneId) -> Option<gtk::Widget> {
        self.pane_frames.get(&pane).cloned()
    }

    pub fn set_pane_zoomed(&mut self, pane: PaneId, zoomed: bool) {
        let Some(frame) = self.pane_frames.get(&pane) else {
            return;
        };
        if zoomed {
            frame.add_css_class("flowmux-pane-zoomed");
            if self.pane_zoom_badges.contains_key(&pane) {
                return;
            }
            let Some(tabs) = self.pane_tab_containers.get(&pane) else {
                return;
            };
            let badge = gtk::Label::new(Some("Zoomed"));
            badge.add_css_class("flowmux-pane-zoom-badge");
            badge.set_tooltip_text(Some("This pane is temporarily maximized"));
            tabs.append(&badge);
            self.pane_zoom_badges.insert(pane, badge);
        } else {
            frame.remove_css_class("flowmux-pane-zoomed");
            if let Some(badge) = self.pane_zoom_badges.remove(&pane) {
                badge.unparent();
            }
        }
    }

    pub fn mark_focused_pane(&self, focused: PaneId) {
        if !self.pane_frames.contains_key(&focused) {
            return;
        }
        for (pane, frame) in &self.pane_frames {
            if *pane == focused {
                if !frame.has_css_class("focused") {
                    frame.add_css_class("focused");
                }
            } else {
                frame.remove_css_class("focused");
            }
        }
    }

    /// Mirror unread notification ownership onto pane frames. The theme uses
    /// the same top-edge focus treatment for this class, so no second renderer
    /// or notification-specific color path is needed.
    pub fn set_notification_panes(&self, panes: &HashSet<PaneId>) {
        for (pane, frame) in &self.pane_frames {
            if panes.contains(pane) {
                frame.add_css_class("flowmux-notification");
            } else {
                frame.remove_css_class("flowmux-notification");
            }
        }
    }

    /// Workspace id containing each `pane`. Used by focus_in_direction to keep
    /// Alt+arrow movement inside the same workspace. Inactive workspaces can
    /// overlap at the same GtkStack coordinates, so this prevents focus from
    /// leaking to another workspace when compute_bounds is misleading.
    pub fn workspace_of_pane(&self, pane: PaneId) -> Option<WorkspaceId> {
        self.pane_workspace.get(&pane).copied()
    }

    /// Return all pane ids belonging to `workspace` for focus_in_direction
    /// candidate filtering.
    pub fn pane_ids_in_workspace(
        &self,
        workspace: WorkspaceId,
    ) -> impl Iterator<Item = PaneId> + '_ {
        self.pane_workspace
            .iter()
            .filter_map(move |(pane, ws)| (*ws == workspace).then_some(*pane))
    }

    pub fn surface_ids_in_workspace(&self, workspace: WorkspaceId) -> Vec<SurfaceId> {
        self.surface_workspace
            .iter()
            .filter_map(|(surface, owner)| (*owner == workspace).then_some(*surface))
            .collect()
    }

    pub fn active_surface(&self, pane: PaneId) -> Option<SurfaceId> {
        self.active_terminal_by_pane
            .get(&pane)
            .or_else(|| self.active_browser_by_pane.get(&pane))
            .or_else(|| self.active_editor_by_pane.get(&pane))
            .copied()
    }

    pub fn current_dir_for_pane(&self, pane: PaneId) -> Option<std::path::PathBuf> {
        if let Some(surface) = self.active_terminal_by_pane.get(&pane) {
            if let Some(dir) = self
                .terminals
                .get(surface)
                .and_then(|term| term.current_dir())
            {
                return Some(dir);
            }
        }

        self.surface_tabs.get(&pane).and_then(|tabs| {
            tabs.iter().find_map(|(surface, _)| {
                self.terminals
                    .get(surface)
                    .and_then(|term| term.current_dir())
            })
        })
    }

    pub fn next_surface(&self, pane: PaneId) -> Option<SurfaceId> {
        self.adjacent_surface(pane, 1)
    }

    pub fn previous_surface(&self, pane: PaneId) -> Option<SurfaceId> {
        self.adjacent_surface(pane, -1)
    }

    fn adjacent_surface(&self, pane: PaneId, offset: isize) -> Option<SurfaceId> {
        let tabs = self.surface_tabs.get(&pane)?;
        if tabs.len() < 2 {
            return None;
        }
        let active = self.active_surface(pane);
        let active_idx = active
            .and_then(|active| tabs.iter().position(|(id, _)| *id == active))
            .unwrap_or(0);
        let len = tabs.len() as isize;
        let next_idx = (active_idx as isize + offset).rem_euclid(len) as usize;
        Some(tabs[next_idx].0)
    }

    pub fn terminal_cwds(&self) -> Vec<(PaneId, SurfaceId, std::path::PathBuf)> {
        self.terminals
            .iter()
            .filter_map(|(surface, terminal)| {
                terminal
                    .poll_cwd_if_changed()
                    .map(|cwd| (terminal.id(), *surface, cwd))
            })
            .collect()
    }

    pub fn terminal_scrollback_snapshots(&self) -> Vec<(PaneId, SurfaceId, String)> {
        self.terminals
            .iter()
            .filter_map(|(surface, terminal)| {
                terminal.screen_text().map(|text| {
                    (
                        terminal.id(),
                        *surface,
                        normalize_scrollback_snapshot(&text),
                    )
                })
            })
            .collect()
    }

    pub fn dirty_terminal_scrollback_snapshots(&self) -> Vec<(PaneId, SurfaceId, String)> {
        self.terminals
            .iter()
            .filter_map(|(surface, terminal)| {
                terminal.dirty_screen_text().map(|text| {
                    (
                        terminal.id(),
                        *surface,
                        normalize_scrollback_snapshot(&text),
                    )
                })
            })
            .collect()
    }

    pub fn terminal_cwd_poll_inputs(
        &self,
    ) -> Vec<(PaneId, SurfaceId, Option<std::path::PathBuf>, Option<i32>)> {
        #[cfg(not(target_os = "linux"))]
        {
            Vec::new()
        }

        #[cfg(target_os = "linux")]
        {
            self.terminals
                .iter()
                .filter_map(|(surface, terminal)| {
                    terminal
                        .announced_current_dir()
                        .is_none()
                        .then(|| (terminal.id(), *surface, None, terminal.pid.get()))
                })
                .collect()
        }
    }

    pub fn apply_terminal_cwd_poll_results(
        &self,
        results: Vec<(PaneId, SurfaceId, Option<std::path::PathBuf>)>,
    ) -> Vec<(PaneId, SurfaceId, std::path::PathBuf)> {
        results
            .into_iter()
            .filter_map(|(pane, surface, cwd)| {
                let terminal = self.terminals.get(&surface)?;
                (terminal.id() == pane)
                    .then(|| terminal.record_polled_cwd(cwd))
                    .flatten()
                    .map(|cwd| (pane, surface, cwd))
            })
            .collect()
    }

    /// (surface, shell child PID) for every live terminal. The PID is the
    /// pty-tee/shell wrapper spawned for the pane; the agent process (if any)
    /// is a descendant. Feeds the Agent Bar's process-truth detection sweep.
    pub fn terminal_agent_pids(&self) -> Vec<(SurfaceId, u32)> {
        self.terminals
            .iter()
            .filter_map(|(surface, terminal)| {
                let pid = terminal.pid.get()?;
                (pid > 0).then_some((*surface, pid as u32))
            })
            .collect()
    }

    pub fn set_surface_title(&self, surface: SurfaceId, title: &str) {
        if let Some(label) = self.surface_tab_labels.get(&surface) {
            label.set_text(title);
            label.set_tooltip_text(Some(title));
        }
    }

    pub fn surface_title_text(&self, surface: SurfaceId) -> Option<String> {
        self.surface_tab_labels
            .get(&surface)
            .map(|label| label.text().to_string())
    }

    pub fn clear_workspace(&mut self, workspace: WorkspaceId) {
        let panes: Vec<PaneId> = self
            .pane_workspace
            .iter()
            .filter_map(|(pane, owner)| (*owner == workspace).then_some(*pane))
            .collect();
        for pane in panes {
            self.active_terminal_by_pane.remove(&pane);
            self.active_browser_by_pane.remove(&pane);
            self.active_editor_by_pane.remove(&pane);
            self.pane_frames.remove(&pane);
            self.surface_stacks.remove(&pane);
            self.surface_tabs.remove(&pane);
            self.pane_tab_containers.remove(&pane);
            self.pane_zoom_badges.remove(&pane);
            self.pane_workspace.remove(&pane);
        }

        let split_ids: Vec<PaneId> = self
            .split_workspace
            .iter()
            .filter_map(|(split, owner)| (*owner == workspace).then_some(*split))
            .collect();
        for split in split_ids {
            self.split_paneds.remove(&split);
            self.split_workspace.remove(&split);
        }

        let surfaces: Vec<SurfaceId> = self
            .surface_workspace
            .iter()
            .filter_map(|(surface, owner)| (*owner == workspace).then_some(*surface))
            .collect();
        for surface in surfaces {
            if let Some(terminal) = self.terminals.remove(&surface) {
                terminal.close_pty();
            }
            if let Some(browser) = self.browsers.remove(&surface) {
                browser.prepare_for_close();
            }
            if let Some(editor) = self.editors.remove(&surface) {
                editor.prepare_for_close();
            }
            self.surface_tab_labels.remove(&surface);
            self.surface_workspace.remove(&surface);
        }
    }

    /// Move the tab identified by `surface` within the same pane to
    /// `target_index`. Called only after store-side reorder succeeds; it keeps
    /// the tab bar `gtk::Box` and `surface_tabs` vector in sync. Out-of-range
    /// or same-position targets are no-ops.
    pub fn reorder_surface_widget(
        &mut self,
        pane: PaneId,
        surface: SurfaceId,
        target_index: usize,
    ) {
        let Some(tabs) = self.surface_tabs.get_mut(&pane) else {
            return;
        };
        let Some(current) = tabs.iter().position(|(id, _)| *id == surface) else {
            return;
        };
        let len = tabs.len();
        if len == 0 {
            return;
        }
        let new_index = target_index.min(len - 1);
        if current == new_index {
            return;
        }
        let entry = tabs.remove(current);
        tabs.insert(new_index, entry);
        let order: Vec<gtk::Widget> = tabs.iter().map(|(_, w)| w.clone()).collect();
        if let Some(container) = self.pane_tab_containers.get(&pane).cloned() {
            // GtkBox has no direct reorder API, so the safest path is to detach
            // all children and append them in the new order. Widgets are reused,
            // preserving handlers and state.
            let mut child = container.first_child();
            while let Some(c) = child {
                let next = c.next_sibling();
                container.remove(&c);
                child = next;
            }
            for w in &order {
                container.append(w);
            }
        }
    }

    /// Return current (split_id, ratio) pairs for all registered split paned
    /// widgets. Ratio is paned.position() / total length. Skip unrealized paned
    /// widgets or those with zero width/height because their ratio is meaningless.
    pub fn split_ratios(&self) -> Vec<(PaneId, f32)> {
        let mut out = Vec::new();
        for (split_id, paned) in &self.split_paneds {
            let total = match paned.orientation() {
                gtk::Orientation::Horizontal => paned.width(),
                _ => paned.height(),
            };
            if total <= 0 {
                continue;
            }
            let pos = paned.position();
            if pos <= 0 {
                continue;
            }
            let ratio = (pos as f32) / (total as f32);
            out.push((*split_id, ratio));
        }
        out
    }

    /// Apply a split ratio to a registered `gtk::Paned` immediately or once
    /// the widget has a meaningful size.
    pub fn apply_split_ratio(&self, split_id: PaneId, ratio: f32) -> bool {
        let Some(paned) = self.split_paneds.get(&split_id) else {
            return false;
        };
        apply_ratio_when_sized(paned, ratio.clamp(0.05, 0.95));
        true
    }

    /// Register one split paned widget. If the same split_id already exists,
    /// update only the widget and keep the workspace mapping.
    pub fn register_split(&mut self, split_id: PaneId, workspace: WorkspaceId, paned: gtk::Paned) {
        self.split_paneds.insert(split_id, paned);
        self.split_workspace.insert(split_id, workspace);
    }

    pub fn activate_surface(&mut self, pane: PaneId, surface: SurfaceId) {
        if let Some(stack) = self.surface_stacks.get(&pane) {
            stack.set_visible_child_name(&surface.to_string());
        }
        if self.terminals.contains_key(&surface) {
            self.active_terminal_by_pane.insert(pane, surface);
            self.active_browser_by_pane.remove(&pane);
            self.active_editor_by_pane.remove(&pane);
        } else if self.browsers.contains_key(&surface) {
            self.active_browser_by_pane.insert(pane, surface);
            self.active_terminal_by_pane.remove(&pane);
            self.active_editor_by_pane.remove(&pane);
        } else if self.editors.contains_key(&surface) {
            self.active_editor_by_pane.insert(pane, surface);
            self.active_terminal_by_pane.remove(&pane);
            self.active_browser_by_pane.remove(&pane);
        }
        if let Some(tabs) = self.surface_tabs.get(&pane) {
            for (id, tab) in tabs {
                if *id == surface {
                    tab.add_css_class("active");
                } else {
                    tab.remove_css_class("active");
                }
            }
        }
    }

    /// Toggle the `has-multi-tabs` class on a pane's tab row so the active
    /// tab grows a 2px top stripe only when the pane has ≥2 tabs.
    /// Whether `pane` is currently rendered (its surface stack exists).
    pub fn has_pane(&self, pane: PaneId) -> bool {
        self.surface_stacks.contains_key(&pane)
    }

    pub fn refresh_tab_multi_class(&self, pane: PaneId) {
        let Some(tabs_box) = self.pane_tab_containers.get(&pane) else {
            return;
        };
        let count = self.surface_tabs.get(&pane).map(|v| v.len()).unwrap_or(0);
        if count >= 2 {
            tabs_box.add_css_class("has-multi-tabs");
        } else {
            tabs_box.remove_css_class("has-multi-tabs");
        }
    }

    /// Stamp the `flowmux-solo` class on the single pane that owns the
    /// whole workspace when it also has exactly one tab; clear it from any
    /// other pane in the same workspace. Used so the focus border is
    /// suppressed for trivial 1-pane/1-tab workspaces.
    pub fn set_workspace_solo(&self, workspace: WorkspaceId, solo_pane: Option<PaneId>) {
        for (pane_id, frame) in &self.pane_frames {
            if self.pane_workspace.get(pane_id) != Some(&workspace) {
                continue;
            }
            if solo_pane == Some(*pane_id) {
                frame.add_css_class("flowmux-solo");
            } else {
                frame.remove_css_class("flowmux-solo");
            }
        }
    }

    /// Drop every registry entry that belongs to `pane` and the
    /// surfaces inside it. Used by the incremental `close_pane` path
    /// — the GTK widgets themselves are re-parented (or unparented)
    /// by the caller, so this method only cleans up the in-memory
    /// indexes and never touches widgets directly.
    pub fn forget_pane(&mut self, pane: PaneId) {
        let surfaces: Vec<SurfaceId> = self
            .surface_tabs
            .get(&pane)
            .map(|tabs| tabs.iter().map(|(id, _)| *id).collect())
            .unwrap_or_default();
        for s in surfaces {
            if let Some(terminal) = self.terminals.remove(&s) {
                terminal.close_pty();
            }
            if let Some(browser) = self.browsers.remove(&s) {
                browser.prepare_for_close();
            }
            if let Some(editor) = self.editors.remove(&s) {
                editor.prepare_for_close();
            }
            self.surface_tab_labels.remove(&s);
            self.surface_workspace.remove(&s);
        }
        self.surface_tabs.remove(&pane);
        self.surface_stacks.remove(&pane);
        self.pane_frames.remove(&pane);
        self.pane_tab_containers.remove(&pane);
        self.pane_zoom_badges.remove(&pane);
        self.active_terminal_by_pane.remove(&pane);
        self.active_browser_by_pane.remove(&pane);
        self.active_editor_by_pane.remove(&pane);
        // pane_workspace is keyed by this same leaf PaneId; dropping it here
        // keeps the map from growing without bound across split/close churn in
        // a long-lived workspace (clear_workspace only fires on full teardown).
        self.pane_workspace.remove(&pane);
    }

    /// Drop the split-index entries for a `gtk::Paned` that just collapsed on
    /// the incremental close path. `split_paneds` / `split_workspace` are keyed
    /// by the split node's id, which the caller no longer has once the store
    /// tree collapsed, so match by the widget value instead. Without this the
    /// two maps leak one entry per closed split until the workspace is cleared.
    pub fn forget_split_paned(&mut self, paned: &gtk::Paned) {
        let stale: Vec<PaneId> = self
            .split_paneds
            .iter()
            .filter_map(|(id, p)| (p == paned).then_some(*id))
            .collect();
        for id in stale {
            self.split_paneds.remove(&id);
            self.split_workspace.remove(&id);
        }
    }

    /// Detach only one surface tab/panel from the same pane's widget tree.
    /// Other panes in the same workspace are untouched, preserving shell
    /// sessions and browser navigation state. Only call after close_surface
    /// returned `SurfaceRemoved`; removing an entire pane needs a separate path
    /// because the split tree changes.
    pub fn detach_surface_widget(&mut self, pane: PaneId, surface: SurfaceId) {
        // Unparent the tab widget from the tab bar.
        if let Some(tabs) = self.surface_tabs.get_mut(&pane) {
            if let Some(idx) = tabs.iter().position(|(id, _)| *id == surface) {
                let (_, tab_widget) = tabs.remove(idx);
                if let Some(parent) = tab_widget.parent() {
                    if let Some(b) = parent.downcast_ref::<gtk::Box>() {
                        b.remove(&tab_widget);
                    } else {
                        tab_widget.unparent();
                    }
                }
            }
        }
        // Remove the surface panel from the same pane's stack.
        if let Some(stack) = self.surface_stacks.get(&pane) {
            if let Some(child) = stack.child_by_name(&surface.to_string()) {
                stack.remove(&child);
            }
        }
        // Clean PaneRegistry indexes.
        if let Some(terminal) = self.terminals.remove(&surface) {
            terminal.close_pty();
        }
        if let Some(browser) = self.browsers.remove(&surface) {
            browser.prepare_for_close();
        }
        if let Some(editor) = self.editors.remove(&surface) {
            editor.prepare_for_close();
        }
        self.surface_tab_labels.remove(&surface);
        self.surface_workspace.remove(&surface);
        if self.active_terminal_by_pane.get(&pane) == Some(&surface) {
            self.active_terminal_by_pane.remove(&pane);
        }
        if self.active_browser_by_pane.get(&pane) == Some(&surface) {
            self.active_browser_by_pane.remove(&pane);
        }
        if self.active_editor_by_pane.get(&pane) == Some(&surface) {
            self.active_editor_by_pane.remove(&pane);
        }
        self.refresh_tab_multi_class(pane);
    }

    /// Remove one surface from its pane and return the live widget so it can be
    /// re-parented into a standalone window. Unlike detach_surface_widget, this
    /// preserves the surface panel instead of dropping it.
    pub fn take_surface_for_tearoff(
        &mut self,
        pane: PaneId,
        surface: SurfaceId,
        fallback_title: &str,
    ) -> Option<TornOffSurface> {
        let stack = self.surface_stacks.get(&pane)?.clone();
        let content = stack.child_by_name(&surface.to_string())?;
        let title = self
            .surface_tab_labels
            .get(&surface)
            .map(|label| label.text().to_string())
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| fallback_title.to_string());

        if let Some(tabs) = self.surface_tabs.get_mut(&pane) {
            if let Some(idx) = tabs.iter().position(|(id, _)| *id == surface) {
                let (_, tab_widget) = tabs.remove(idx);
                if let Some(parent) = tab_widget.parent() {
                    if let Some(b) = parent.downcast_ref::<gtk::Box>() {
                        b.remove(&tab_widget);
                    } else {
                        tab_widget.unparent();
                    }
                }
            }
        }
        stack.remove(&content);

        let (focus, kind) = if let Some(terminal) = self.terminals.remove(&surface) {
            (
                terminal.root_widget(),
                SurfaceKind::Terminal {
                    shell: None,
                    cwd: terminal.current_dir(),
                },
            )
        } else if let Some(browser) = self.browsers.remove(&surface) {
            (
                browser.focus_widget(),
                SurfaceKind::Browser { initial_url: None },
            )
        } else if let Some(editor) = self.editors.remove(&surface) {
            (
                editor.focus_widget(),
                SurfaceKind::Editor {
                    workspace_root: editor.workspace_root().to_path_buf(),
                    session: flowmux_core::EditorSessionState::default(),
                },
            )
        } else {
            (
                content.clone(),
                SurfaceKind::Terminal {
                    shell: None,
                    cwd: None,
                },
            )
        };
        self.surface_tab_labels.remove(&surface);
        self.surface_workspace.remove(&surface);
        if self.active_terminal_by_pane.get(&pane) == Some(&surface) {
            self.active_terminal_by_pane.remove(&pane);
        }
        if self.active_browser_by_pane.get(&pane) == Some(&surface) {
            self.active_browser_by_pane.remove(&pane);
        }
        if self.active_editor_by_pane.get(&pane) == Some(&surface) {
            self.active_editor_by_pane.remove(&pane);
        }
        self.refresh_tab_multi_class(pane);

        Some(TornOffSurface {
            pane,
            surface,
            title,
            kind,
            content,
            focus,
        })
    }

    /// Detach a surface from `pane` for an **in-app move**, keeping the live
    /// backend handle (terminal PTY / browser WebView) intact so it can be
    /// re-homed into another pane without restarting. Unlike
    /// [`Self::take_surface_for_tearoff`], which drops the registry handle (a
    /// torn-off window only needs the widget), this returns the handle inside
    /// [`MovingSurface`] so [`Self::attach_moved_surface`] can re-register it.
    /// Returns `None` if the surface is not rendered in `pane`.
    pub fn detach_surface_for_move(
        &mut self,
        pane: PaneId,
        surface: SurfaceId,
    ) -> Option<MovingSurface> {
        let stack = self.surface_stacks.get(&pane)?.clone();
        let content = stack.child_by_name(&surface.to_string())?;

        // Pull the live handle out first; bail (leaving the widget tree
        // untouched) if neither map owns this surface.
        let handle = if let Some(terminal) = self.terminals.remove(&surface) {
            MovingHandle::Terminal(terminal)
        } else if let Some(browser) = self.browsers.remove(&surface) {
            MovingHandle::Browser(browser)
        } else if let Some(editor) = self.editors.remove(&surface) {
            MovingHandle::Editor(editor)
        } else {
            return None;
        };

        if let Some(tabs) = self.surface_tabs.get_mut(&pane) {
            if let Some(idx) = tabs.iter().position(|(id, _)| *id == surface) {
                let (_, tab_widget) = tabs.remove(idx);
                if let Some(parent) = tab_widget.parent() {
                    if let Some(b) = parent.downcast_ref::<gtk::Box>() {
                        b.remove(&tab_widget);
                    } else {
                        tab_widget.unparent();
                    }
                }
            }
        }
        stack.remove(&content);
        self.surface_tab_labels.remove(&surface);
        self.surface_workspace.remove(&surface);
        if self.active_terminal_by_pane.get(&pane) == Some(&surface) {
            self.active_terminal_by_pane.remove(&pane);
        }
        if self.active_browser_by_pane.get(&pane) == Some(&surface) {
            self.active_browser_by_pane.remove(&pane);
        }
        if self.active_editor_by_pane.get(&pane) == Some(&surface) {
            self.active_editor_by_pane.remove(&pane);
        }
        self.refresh_tab_multi_class(pane);

        Some(MovingSurface {
            surface,
            content,
            handle,
        })
    }

    /// Mount a [`MovingSurface`] (produced by [`Self::detach_surface_for_move`])
    /// into `dst_pane`, appending its tab to the bar and re-registering the live
    /// handle under `dst_workspace`. The moved tab becomes active. Returns
    /// `false` if `dst_pane` is not currently rendered (caller should ensure the
    /// destination workspace is built first).
    pub fn attach_moved_surface(
        &mut self,
        dst_pane: PaneId,
        dst_workspace: WorkspaceId,
        moving: MovingSurface,
        tab: gtk::Widget,
        label: gtk::Label,
    ) -> bool {
        let Some(stack) = self.surface_stacks.get(&dst_pane).cloned() else {
            return false;
        };
        let Some(tabs_box) = self.pane_tab_containers.get(&dst_pane).cloned() else {
            return false;
        };
        let surface = moving.surface;
        stack.add_named(&moving.content, Some(&surface.to_string()));
        tabs_box.append(&tab);
        match moving.handle {
            MovingHandle::Terminal(terminal) => {
                terminal.set_pane_id(dst_pane);
                self.terminals.insert(surface, terminal);
            }
            MovingHandle::Browser(browser) => {
                browser.set_pane_id(dst_pane);
                self.browsers.insert(surface, browser);
            }
            MovingHandle::Editor(editor) => {
                editor.set_pane_id(dst_pane);
                self.editors.insert(surface, editor);
            }
        }
        self.surface_tab_labels.insert(surface, label);
        self.surface_workspace.insert(surface, dst_workspace);
        self.surface_tabs
            .entry(dst_pane)
            .or_default()
            .push((surface, tab));
        self.activate_surface(dst_pane, surface);
        self.refresh_tab_multi_class(dst_pane);
        true
    }
}

/// A surface detached for an in-app move: the live content widget plus the
/// backend handle that keeps its PTY / WebView running.
pub struct MovingSurface {
    pub surface: SurfaceId,
    pub content: gtk::Widget,
    pub handle: MovingHandle,
}

/// The live backend handle carried by a [`MovingSurface`].
pub enum MovingHandle {
    Terminal(PaneTerminal),
    Browser(BrowserPane),
    Editor(EditorPane),
}

/// Apply a `gtk::Paned` ratio immediately after its first allocation.
///
/// At `connect_realize` time the widget is not allocated yet, so paned.width()
/// or height() is 0 and `set_position` would store a meaningless position for
/// the next launch. Defer one frame with `idle_add_local`, retry while total is
/// 0, and give up after 60 tries, about one second, to avoid infinite loops for
/// inactive workspaces that never map.
fn apply_ratio_when_sized(paned: &gtk::Paned, ratio: f32) {
    let weak = paned.downgrade();
    let mut attempts: u32 = 0;
    gtk::glib::idle_add_local(move || {
        let Some(p) = weak.upgrade() else {
            return gtk::glib::ControlFlow::Break;
        };
        let total = match p.orientation() {
            gtk::Orientation::Horizontal => p.width(),
            _ => p.height(),
        };
        if total > 0 {
            p.set_position((total as f32 * ratio) as i32);
            return gtk::glib::ControlFlow::Break;
        }
        attempts += 1;
        if attempts > 60 {
            return gtk::glib::ControlFlow::Break;
        }
        gtk::glib::ControlFlow::Continue
    });
}

/// Result of [`split_pane_incremental`].
pub enum IncrementalSplitOutcome {
    /// Success. The target was already inside another split, so the stack child
    /// is unchanged and the caller does not need to update the surfaces map.
    SucceededNested,
    /// Success. The target was a direct child of the workspace stack, so that
    /// child was replaced by the new `gtk::Paned`. The caller must update the
    /// surfaces map to this new widget for later rerender / drop_workspace paths.
    SucceededRoot { new_root: gtk::Widget },
    /// Incremental path failed. The caller should safely fall back to
    /// rerender_workspace, usually because the target is missing from the
    /// registry or its parent container is unexpected.
    Failed,
}

/// Wrap only `target_pane` in a new split while preserving other panes in the
/// same workspace. Semantics match flowmux-core::Pane::split_leaf: `target_pane`
/// keeps its PaneId as the first child of the new split, and the sibling
/// identified by `new_pane_id` is added as the second child.
///
/// The key to this incremental path is reusing the target pane's `gtk::Frame`,
/// so other panes' terminal shell sessions and browser navigation state survive
/// without rerender. The daemon-side split_pane must already have run before
/// this call so the tree shape is decided.
///
/// `parent_stack_name` is supplied by the caller so a target frame that was a
/// direct stack child can be re-added with the same name, the workspace id.
#[allow(clippy::too_many_arguments)]
pub fn split_pane_incremental(
    workspace: WorkspaceId,
    target_pane: PaneId,
    new_pane_id: PaneId,
    new_split_id: PaneId,
    direction: SplitDirection,
    ratio: f32,
    new_content: PaneContent,
    new_cwd: Option<std::path::PathBuf>,
    parent_stack_name: &str,
    callbacks: &PaneCallbacks,
    registry: Rc<RefCell<PaneRegistry>>,
    theme: Arc<ResolvedTheme>,
    // Build the new sibling empty (no tab / terminal) so a dragged live tab can
    // be mounted into it afterwards.
    empty_sibling: bool,
) -> IncrementalSplitOutcome {
    let Some(target_frame) = registry.borrow().pane_frame(target_pane) else {
        return IncrementalSplitOutcome::Failed;
    };
    let Some(parent) = target_frame.parent() else {
        return IncrementalSplitOutcome::Failed;
    };

    // Record the parent container type and target slot before detach.
    enum Slot {
        PanedStart(gtk::Paned),
        PanedEnd(gtk::Paned),
        Stack(gtk::Stack),
    }
    let slot = if let Some(p) = parent.downcast_ref::<gtk::Paned>() {
        if p.start_child().as_ref() == Some(&target_frame) {
            Slot::PanedStart(p.clone())
        } else if p.end_child().as_ref() == Some(&target_frame) {
            Slot::PanedEnd(p.clone())
        } else {
            return IncrementalSplitOutcome::Failed;
        }
    } else if let Some(s) = parent.downcast_ref::<gtk::Stack>() {
        Slot::Stack(s.clone())
    } else {
        return IncrementalSplitOutcome::Failed;
    };

    // Detach the target frame from its parent. set_*_child(None) automatically
    // unparents the previous child.
    match &slot {
        Slot::PanedStart(p) => p.set_start_child(gtk::Widget::NONE),
        Slot::PanedEnd(p) => p.set_end_child(gtk::Widget::NONE),
        Slot::Stack(s) => s.remove(&target_frame),
    }

    // Build the new sibling pane widget. cwd / argv belong only to the sibling;
    // the target reuses its already-built frame.
    let new_sibling = build_leaf_pane(
        workspace,
        new_pane_id,
        &new_content,
        Vec::new(),
        new_cwd,
        callbacks,
        registry.clone(),
        theme.clone(),
        empty_sibling,
    );

    let orient = match direction {
        SplitDirection::Horizontal => gtk::Orientation::Vertical,
        SplitDirection::Vertical => gtk::Orientation::Horizontal,
    };
    let paned = gtk::Paned::new(orient);
    paned.set_hexpand(true);
    paned.set_vexpand(true);
    paned.set_start_child(Some(&target_frame));
    paned.set_end_child(Some(&new_sibling));
    paned.set_resize_start_child(true);
    paned.set_resize_end_child(true);
    paned.set_shrink_start_child(false);
    paned.set_shrink_end_child(false);
    {
        let p = paned.clone();
        paned.connect_realize(move |_| apply_ratio_when_sized(&p, ratio));
    }

    registry
        .borrow_mut()
        .register_split(new_split_id, workspace, paned.clone());

    let paned_widget: gtk::Widget = paned.upcast();

    // Insert the new Paned back into the vacated slot.
    match slot {
        Slot::PanedStart(p) => {
            p.set_start_child(Some(&paned_widget));
            IncrementalSplitOutcome::SucceededNested
        }
        Slot::PanedEnd(p) => {
            p.set_end_child(Some(&paned_widget));
            IncrementalSplitOutcome::SucceededNested
        }
        Slot::Stack(s) => {
            s.add_named(&paned_widget, Some(parent_stack_name));
            s.set_visible_child_name(parent_stack_name);
            IncrementalSplitOutcome::SucceededRoot {
                new_root: paned_widget,
            }
        }
    }
}

pub fn build_surface(
    workspace: WorkspaceId,
    surface: &Surface,
    callbacks: &PaneCallbacks,
    registry: Rc<RefCell<PaneRegistry>>,
    theme: Arc<ResolvedTheme>,
) -> gtk::Widget {
    let (argv, cwd) = match &surface.kind {
        SurfaceKind::Terminal { cwd, shell } => (
            shell.clone().map(|s| vec![s]).unwrap_or_default(),
            cwd.clone(),
        ),
        SurfaceKind::Browser { .. } => (Vec::new(), None),
        SurfaceKind::Editor { .. } => (Vec::new(), None),
    };
    build_pane(
        workspace,
        &surface.root_pane,
        argv,
        cwd,
        callbacks,
        registry,
        theme,
    )
}

fn build_pane(
    workspace: WorkspaceId,
    pane: &Pane,
    argv: Vec<String>,
    cwd: Option<std::path::PathBuf>,
    callbacks: &PaneCallbacks,
    registry: Rc<RefCell<PaneRegistry>>,
    theme: Arc<ResolvedTheme>,
) -> gtk::Widget {
    match pane {
        Pane::Leaf { id, content } => build_leaf_pane(
            workspace, *id, content, argv, cwd, callbacks, registry, theme, false,
        ),
        Pane::Split {
            id: split_id,
            direction,
            ratio,
            first,
            second,
        } => {
            let orient = match direction {
                SplitDirection::Horizontal => gtk::Orientation::Vertical,
                SplitDirection::Vertical => gtk::Orientation::Horizontal,
            };
            let paned = gtk::Paned::new(orient);
            paned.set_hexpand(true);
            paned.set_vexpand(true);
            let left = build_pane(
                workspace,
                first,
                argv.clone(),
                cwd.clone(),
                callbacks,
                registry.clone(),
                theme.clone(),
            );
            let right = build_pane(
                workspace,
                second,
                argv,
                cwd,
                callbacks,
                registry.clone(),
                theme,
            );
            paned.set_start_child(Some(&left));
            paned.set_end_child(Some(&right));
            paned.set_resize_start_child(true);
            paned.set_resize_end_child(true);
            paned.set_shrink_start_child(false);
            paned.set_shrink_end_child(false);
            let r = *ratio;
            {
                let p = paned.clone();
                paned.connect_realize(move |_| apply_ratio_when_sized(&p, r));
            }
            registry
                .borrow_mut()
                .register_split(*split_id, workspace, paned.clone());
            paned.upcast()
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_leaf_pane(
    workspace: WorkspaceId,
    pane_id: PaneId,
    content: &PaneContent,
    argv: Vec<String>,
    cwd: Option<std::path::PathBuf>,
    callbacks: &PaneCallbacks,
    registry: Rc<RefCell<PaneRegistry>>,
    theme: Arc<ResolvedTheme>,
    // Build a tab-less, terminal-less shell pane. Used as the sibling of a
    // drag-split: the moved live tab is mounted into it right afterwards via
    // `attach_moved_surface`, so spawning a throwaway terminal here would be
    // wasteful and would briefly flash an extra shell.
    empty: bool,
) -> gtk::Widget {
    let surfaces = if empty {
        Vec::new()
    } else {
        materialize_surfaces(content, cwd)
    };
    // `active` is unused when `surfaces` is empty (a pane built empty for a
    // drag-split, whose live tab is mounted right after via
    // `attach_moved_surface`); fall back to a throwaway id instead of indexing.
    let active = match content {
        PaneContent::Tabs { active, .. }
            if surfaces.iter().any(|surface| surface.id == *active) =>
        {
            *active
        }
        _ => surfaces
            .first()
            .map(|s| s.id)
            .unwrap_or_else(SurfaceId::new),
    };

    let frame = gtk::Frame::new(None);
    frame.add_css_class("flowmux-pane");
    frame.set_hexpand(true);
    frame.set_vexpand(true);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.set_hexpand(true);
    root.set_vexpand(true);

    let tabbar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    tabbar.add_css_class("flowmux-pane-tabbar");

    let tabs = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    tabs.add_css_class("flowmux-pane-tabs");
    tabs.set_hexpand(false);

    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);

    let tools = gtk::Box::new(gtk::Orientation::Horizontal, 1);
    tools.add_css_class("flowmux-pane-tools");

    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);

    let mut tab_widgets = Vec::new();
    for surface in &surfaces {
        let (tab, label) =
            build_surface_tab_widget(pane_id, surface, surface.id == active, callbacks);
        tabs.append(&tab);
        tab_widgets.push((surface.id, tab.clone().upcast::<gtk::Widget>()));
        registry
            .borrow_mut()
            .surface_tab_labels
            .insert(surface.id, label);

        let widget = build_panel(
            pane_id,
            workspace,
            surface,
            argv.clone(),
            callbacks,
            registry.clone(),
            theme.clone(),
            frame.clone(),
        );
        stack.add_named(&widget, Some(&surface.id.to_string()));
    }

    let split_right = pane_tool_button("go-next-symbolic", "Split right");
    {
        let cb = callbacks.on_split_right.clone();
        split_right.connect_clicked(move |_| (cb.borrow_mut())(pane_id));
    }
    tools.append(&split_right);

    let split_down = pane_tool_button("go-down-symbolic", "Split down");
    {
        let cb = callbacks.on_split_down.clone();
        split_down.connect_clicked(move |_| (cb.borrow_mut())(pane_id));
    }
    tools.append(&split_down);

    let add = pane_tool_button("tab-new-symbolic", "Add tab");
    {
        let cb = callbacks.on_new_surface.clone();
        add.connect_clicked(move |_| (cb.borrow_mut())(pane_id));
    }
    tools.append(&add);

    let add_browser = pane_tool_button("web-browser-symbolic", "Add browser tab");
    {
        let cb = callbacks.on_new_browser_surface.clone();
        add_browser.connect_clicked(move |_| (cb.borrow_mut())(pane_id));
    }
    tools.append(&add_browser);

    let pane_menu = pane_menu_button(pane_id, callbacks);
    tools.append(&pane_menu);

    // The File browser toggle lives in the side-panel footer (next to the
    // Options button), not in the per-pane tool row.

    stack.set_visible_child_name(&active.to_string());
    tabbar.append(&tabs);
    tabbar.append(&spacer);
    tabbar.append(&tools);
    root.append(&tabbar);
    root.append(&stack);

    // Overlay a translucent preview on top of the pane body so a drag can show
    // whether dropping will split (right / bottom half highlighted) or add a tab
    // (no highlight). The drawing area is click-through.
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&root));
    let drop_zone = Rc::new(Cell::new(PaneDropZone::None));
    let preview = gtk::DrawingArea::new();
    preview.set_can_target(false);
    {
        let drop_zone = drop_zone.clone();
        preview.set_draw_func(move |_, cr, w, h| {
            let (w, h) = (w as f64, h as f64);
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.10);
            match drop_zone.get() {
                PaneDropZone::SplitRight => {
                    cr.rectangle(w / 2.0, 0.0, w / 2.0, h);
                    let _ = cr.fill();
                }
                PaneDropZone::SplitDown => {
                    cr.rectangle(0.0, h / 2.0, w, h / 2.0);
                    let _ = cr.fill();
                }
                PaneDropZone::Tab | PaneDropZone::None => {}
            }
        });
    }
    overlay.add_overlay(&preview);
    frame.set_child(Some(&overlay));

    // Dropping a tab on the pane body splits or appends depending on the region;
    // see `attach_pane_body_dnd`.
    attach_pane_body_dnd(&frame, pane_id, drop_zone, preview, callbacks);

    {
        let frame_widget = frame.clone().upcast::<gtk::Widget>();
        let mut r = registry.borrow_mut();
        r.pane_frames.insert(pane_id, frame_widget);
        r.surface_stacks.insert(pane_id, stack);
        r.surface_tabs.insert(pane_id, tab_widgets);
        r.pane_tab_containers.insert(pane_id, tabs);
        r.pane_workspace.insert(pane_id, workspace);
        r.activate_surface(pane_id, active);
        r.refresh_tab_multi_class(pane_id);
    }

    frame.upcast()
}

fn materialize_surfaces(
    content: &PaneContent,
    fallback_cwd: Option<std::path::PathBuf>,
) -> Vec<PaneSurface> {
    match content {
        PaneContent::Tabs { surfaces, .. } if !surfaces.is_empty() => surfaces.clone(),
        PaneContent::Terminal { .. } => vec![PaneSurface::terminal(
            terminal_tab_title_for_cwd(fallback_cwd.as_deref()),
            fallback_cwd,
        )],
        PaneContent::Browser { url } => vec![PaneSurface::browser("Browser", url.clone())],
        PaneContent::Tabs { .. } => vec![PaneSurface::terminal(
            terminal_tab_title_for_cwd(fallback_cwd.as_deref()),
            fallback_cwd,
        )],
    }
}

/// Build a single surface tab + wire its click / double-click (rename)
/// / close handlers. Shared between the initial pane render and the
/// incremental [`attach_surface_to_pane`] path so a click on either
/// behaves identically.
pub(crate) fn build_surface_tab_widget(
    pane_id: PaneId,
    surface: &PaneSurface,
    active: bool,
    callbacks: &PaneCallbacks,
) -> (gtk::Box, gtk::Label) {
    let (tab, label) = surface_tab(surface, active);
    let button = tab
        .first_child()
        .and_downcast::<gtk::Button>()
        .expect("surface tab starts with button");
    {
        let activate_cb = callbacks.on_activate_surface.clone();
        let rename_cb = callbacks.on_rename_surface.clone();
        let last_click = Rc::new(Cell::new(None::<Instant>));
        let surface_id = surface.id;
        button.connect_clicked(move |_| {
            let now = Instant::now();
            let double_clicked = last_click
                .get()
                .is_some_and(|last| now.duration_since(last) <= Duration::from_millis(500));
            if double_clicked {
                last_click.set(None);
                (rename_cb.borrow_mut())(pane_id, surface_id);
            } else {
                last_click.set(Some(now));
                (activate_cb.borrow_mut())(pane_id, surface_id);
            }
        });
    }
    let close = tab
        .last_child()
        .and_downcast::<gtk::Button>()
        .expect("surface tab ends with close button");
    {
        let cb = callbacks.on_close_surface.clone();
        let surface_id = surface.id;
        close.connect_clicked(move |_| (cb.borrow_mut())(pane_id, surface_id));
    }
    // Right-click on a tab opens a small context menu. Terminal tabs
    // get "Show in folder" + "Copy path"; browser tabs get just
    // "Copy URL". Browser tabs have no cwd so "Show in folder" is
    // intentionally absent rather than disabled.
    attach_tab_context_menu(&tab, pane_id, surface, callbacks);
    attach_tab_dnd_handlers(&tab, pane_id, surface, callbacks);
    (tab, label)
}

/// Build the secondary-click popover used by surface tabs. Mirrors the
/// plain `Popover` and `Button` row pattern used by `ghostty_pane.rs`
/// and `sidebar.rs`. The `connect_clicked` closures route directly to the
/// per-pane callbacks.
///
/// PopoverMenu + `win.*` actions have been observed to drop in some GTK
/// versions.
fn attach_tab_context_menu(
    tab: &gtk::Box,
    pane_id: PaneId,
    surface: &PaneSurface,
    callbacks: &PaneCallbacks,
) {
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_SECONDARY);
    let tab_for_click = tab.clone();
    let on_show = callbacks.on_show_surface_folder.clone();
    let on_copy = callbacks.on_copy_surface_text.clone();
    let list_workspaces = callbacks.list_workspaces.clone();
    let workspace_of_pane = callbacks.workspace_of_pane.clone();
    let on_move_to_workspace = callbacks.on_move_surface_to_workspace.clone();
    let surface_id = surface.id;
    let is_terminal = matches!(surface.kind, SurfaceKind::Terminal { .. });
    click.connect_pressed(move |gesture, _n_press, x, y| {
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

        if is_terminal {
            let show_btn = mk("Show in folder");
            let pop = popover.clone();
            let cb = on_show.clone();
            show_btn.connect_clicked(move |_| {
                pop.popdown();
                (cb.borrow_mut())(pane_id, surface_id);
            });
            v.append(&show_btn);

            let copy_btn = mk("Copy path");
            let pop = popover.clone();
            let cb = on_copy.clone();
            copy_btn.connect_clicked(move |_| {
                pop.popdown();
                (cb.borrow_mut())(pane_id, surface_id);
            });
            v.append(&copy_btn);
        } else {
            let copy_btn = mk("Copy URL");
            let pop = popover.clone();
            let cb = on_copy.clone();
            copy_btn.connect_clicked(move |_| {
                pop.popdown();
                (cb.borrow_mut())(pane_id, surface_id);
            });
            v.append(&copy_btn);
        }

        // "Move" item: target workspaces queried live (so names/order reflect
        // the click moment), excluding the tab's own workspace. Disabled when
        // there is nowhere else to move to.
        let current_ws = (workspace_of_pane)(pane_id);
        // Keep each workspace's 1-based side-panel position as its number, then
        // drop the tab's own workspace — so for 3 workspaces moving #2's tab the
        // menu shows "1." and "3.", not a renumbered "1." "2.".
        let movable: Vec<(usize, WorkspaceId, String)> = (list_workspaces)()
            .into_iter()
            .enumerate()
            .map(|(i, (id, name))| (i + 1, id, name))
            .filter(|(_, id, _)| Some(*id) != current_ws)
            .collect();

        let move_btn = gtk::Button::new();
        move_btn.add_css_class("flat");
        let move_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let move_label = gtk::Label::new(Some("Move"));
        move_label.set_xalign(0.0);
        move_label.set_hexpand(true);
        move_label.set_halign(gtk::Align::Fill);
        move_row.append(&move_label);
        if !movable.is_empty() {
            // A right-pointing chevron signals the submenu.
            move_row.append(&gtk::Image::from_icon_name("go-next-symbolic"));
        }
        move_btn.set_child(Some(&move_row));
        move_btn.set_sensitive(!movable.is_empty());

        if !movable.is_empty() {
            let submenu = gtk::Popover::new();
            submenu.set_has_arrow(false);
            submenu.set_position(gtk::PositionType::Right);
            submenu.set_parent(&move_btn);
            let sub_v = gtk::Box::new(gtk::Orientation::Vertical, 0);
            sub_v.set_margin_top(4);
            sub_v.set_margin_bottom(4);
            for (number, ws_id, name) in movable.into_iter() {
                // Numbered by side-panel position so the order is visible.
                let item = mk(&format!("{}. {}", number, name));
                let outer = popover.clone();
                let sub = submenu.clone();
                let cb = on_move_to_workspace.clone();
                item.connect_clicked(move |_| {
                    sub.popdown();
                    outer.popdown();
                    (cb.borrow_mut())(pane_id, surface_id, ws_id);
                });
                sub_v.append(&item);
            }
            submenu.set_child(Some(&sub_v));
            let sub_for_btn = submenu.clone();
            move_btn.connect_clicked(move |_| {
                sub_for_btn.popup();
            });
        }
        v.append(&move_btn);

        popover.set_child(Some(&v));
        popover.set_parent(&tab_for_click);
        popover.set_has_arrow(false);
        crate::ui::popover_pos::anchor_at_click(&popover, &tab_for_click, x, y);
        popover.connect_closed(|p| p.unparent());
        popover.popup();
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    tab.add_controller(click);
}

pub(crate) fn parse_tab_dnd_payload(payload: &str) -> Result<TabDndPayload, &'static str> {
    let mut parts = payload.splitn(3, '|');
    let Some(src_pane_str) = parts.next() else {
        return Err("missing separator");
    };
    let Some(src_surface_str) = parts.next() else {
        return Err("missing separator");
    };
    let src_pane = src_pane_str
        .parse::<PaneId>()
        .map_err(|_| "invalid pane id")?;
    let src_surface = src_surface_str
        .parse::<SurfaceId>()
        .map_err(|_| "invalid surface id")?;
    let surface = parts
        .next()
        .map(serde_json::from_str::<PaneSurface>)
        .transpose()
        .map_err(|_| "invalid surface model")?;
    Ok(TabDndPayload {
        src_pane,
        src_surface,
        surface,
    })
}

#[cfg(test)]
mod tab_dnd_tests {
    use super::*;

    #[test]
    fn parse_tab_dnd_payload_round_trips() {
        let pane = PaneId::new();
        let surface = SurfaceId::new();
        let payload = format!("{pane}|{surface}");

        let parsed = parse_tab_dnd_payload(&payload).expect("payload parses");
        assert_eq!(parsed.src_pane, pane);
        assert_eq!(parsed.src_surface, surface);
        assert!(parsed.surface.is_none());
    }

    #[test]
    fn parse_tab_dnd_payload_accepts_surface_model() {
        let pane = PaneId::new();
        let surface = PaneSurface::browser("Docs", "https://example.test".into());
        let payload = format!(
            "{pane}|{}|{}",
            surface.id,
            serde_json::to_string(&surface).unwrap()
        );

        let parsed = parse_tab_dnd_payload(&payload).expect("payload parses");
        assert_eq!(parsed.src_pane, pane);
        assert_eq!(parsed.src_surface, surface.id);
        let parsed_surface = parsed.surface.expect("surface model");
        assert_eq!(parsed_surface.title, "Docs");
    }

    #[test]
    fn tab_dnd_payload_string_includes_surface_model_for_fallback_drop_targets() {
        let pane = PaneId::new();
        let surface = PaneSurface::browser("Docs", "https://example.test".into());
        let payload = tab_dnd_payload_string(pane, surface.id, &surface);

        let parsed = parse_tab_dnd_payload(&payload).expect("payload parses");
        assert_eq!(parsed.src_pane, pane);
        assert_eq!(parsed.src_surface, surface.id);
        assert!(parsed.surface.is_some());
    }

    #[test]
    fn parse_tab_dnd_payload_rejects_plain_text() {
        assert!(parse_tab_dnd_payload("not a tab drag").is_err());
    }

    #[test]
    fn drop_zone_partitions_pane_into_tab_right_and_down() {
        // top-left → tab
        assert!(matches!(drop_zone(0.2, 0.2), PaneDropZone::Tab));
        // top-right → split right
        assert!(matches!(drop_zone(0.8, 0.2), PaneDropZone::SplitRight));
        // bottom (either side) → split down
        assert!(matches!(drop_zone(0.2, 0.8), PaneDropZone::SplitDown));
        assert!(matches!(drop_zone(0.8, 0.8), PaneDropZone::SplitDown));
        // exact center counts as the lower half (split down)
        assert!(matches!(drop_zone(0.5, 0.5), PaneDropZone::SplitDown));
    }

    #[test]
    fn landed_drop_zone_prefers_visible_split_preview() {
        assert!(matches!(
            landed_drop_zone(PaneDropZone::SplitRight, Some(0.2), Some(0.2)),
            PaneDropZone::SplitRight
        ));
        assert!(matches!(
            landed_drop_zone(PaneDropZone::SplitDown, None, None),
            PaneDropZone::SplitDown
        ));
        assert!(matches!(
            landed_drop_zone(PaneDropZone::Tab, Some(0.8), Some(0.2)),
            PaneDropZone::SplitRight
        ));
    }

    #[test]
    fn tab_drag_split_direction_from_delta_uses_right_or_down_drag() {
        assert!(tab_drag_split_direction_from_delta(20.0, 20.0).is_none());
        assert!(matches!(
            tab_drag_split_direction_from_delta(120.0, 20.0),
            Some(SplitDirection::Vertical)
        ));
        assert!(matches!(
            tab_drag_split_direction_from_delta(20.0, 120.0),
            Some(SplitDirection::Horizontal)
        ));
        assert!(matches!(
            tab_drag_split_direction_from_delta(120.0, 160.0),
            Some(SplitDirection::Horizontal)
        ));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn take_surface_for_tearoff_moves_widget_out_of_source_pane() {
        let pane = PaneId::new();
        let surface = SurfaceId::new();
        let workspace = WorkspaceId::new();
        let stack = gtk::Stack::new();
        let content = gtk::Label::new(Some("live content")).upcast::<gtk::Widget>();
        stack.add_named(&content, Some(&surface.to_string()));

        let tab_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let tab_widget = gtk::Button::new().upcast::<gtk::Widget>();
        tab_bar.append(&tab_widget);
        let label = gtk::Label::new(Some("dragged tab"));

        let mut registry = PaneRegistry::default();
        registry.surface_stacks.insert(pane, stack.clone());
        registry
            .surface_tabs
            .insert(pane, vec![(surface, tab_widget.clone())]);
        registry.pane_tab_containers.insert(pane, tab_bar);
        registry.surface_tab_labels.insert(surface, label);
        registry.surface_workspace.insert(surface, workspace);
        registry.active_terminal_by_pane.insert(pane, surface);

        let torn = registry
            .take_surface_for_tearoff(pane, surface, "fallback")
            .expect("surface widget should be detached for tear-off");

        assert_eq!(torn.pane, pane);
        assert_eq!(torn.surface, surface);
        assert_eq!(torn.title, "dragged tab");
        assert!(torn.content.parent().is_none());
        assert!(torn.focus.parent().is_none());
        assert!(stack.child_by_name(&surface.to_string()).is_none());
        assert!(tab_widget.parent().is_none());
        assert!(registry.surface_tabs.get(&pane).unwrap().is_empty());
        assert!(!registry.surface_tab_labels.contains_key(&surface));
        assert!(!registry.surface_workspace.contains_key(&surface));
        assert!(registry.active_surface(pane).is_none());
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn editor_surface_moves_between_panes_with_live_handle() {
        let src = PaneId::new();
        let dst = PaneId::new();
        let surface = SurfaceId::new();
        let src_workspace = WorkspaceId::new();
        let dst_workspace = WorkspaceId::new();
        let editor = EditorPane::new(src, "/tmp/다국어-プロジェクト".into());
        let content = editor.root.clone().upcast::<gtk::Widget>();

        let src_stack = gtk::Stack::new();
        src_stack.add_named(&content, Some(&surface.to_string()));
        let src_tabs = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let src_tab = gtk::Button::new().upcast::<gtk::Widget>();
        src_tabs.append(&src_tab);

        let mut registry = PaneRegistry::default();
        registry.surface_stacks.insert(src, src_stack);
        registry.surface_tabs.insert(src, vec![(surface, src_tab)]);
        registry.pane_tab_containers.insert(src, src_tabs);
        registry
            .surface_tab_labels
            .insert(surface, gtk::Label::new(Some("Editor")));
        registry.surface_workspace.insert(surface, src_workspace);
        registry.editors.insert(surface, editor);
        registry.activate_surface(src, surface);

        assert_eq!(registry.active_surface(src), Some(surface));
        assert_eq!(
            registry.active_editor(src).map(EditorPane::pane_id),
            Some(src)
        );

        let moving = registry
            .detach_surface_for_move(src, surface)
            .expect("editor surface should detach with its live handle");
        assert!(matches!(&moving.handle, MovingHandle::Editor(_)));
        assert!(registry.active_surface(src).is_none());
        assert!(!registry.editors.contains_key(&surface));

        registry.surface_stacks.insert(dst, gtk::Stack::new());
        registry
            .pane_tab_containers
            .insert(dst, gtk::Box::new(gtk::Orientation::Horizontal, 0));
        let dst_tab = gtk::Button::new().upcast::<gtk::Widget>();
        let dst_label = gtk::Label::new(Some("Editor"));
        assert!(registry.attach_moved_surface(dst, dst_workspace, moving, dst_tab, dst_label,));

        let moved = registry.active_editor(dst).expect("moved editor is active");
        assert_eq!(moved.pane_id(), dst);
        assert_eq!(
            moved.workspace_root(),
            std::path::Path::new("/tmp/다국어-プロジェクト")
        );
        assert_eq!(registry.active_surface(dst), Some(surface));
    }
}

/// Attach controllers that reorder terminal or browser tabs left/right within
/// the same pane by drag and drop, and open a new window when a tab drag ends
/// without landing on another tab.
///
/// - `DragSource`: serializes the (PaneId, SurfaceId) pair as a flowmux-only
///   MIME payload. PaneId is included so DropTarget can reject cross-pane moves.
/// - `DropTargetAsync`: dropping on another tab in the same pane uses x position to
///   choose before/after and calls the reorder callback. Cross-pane drops are rejected.
fn attach_tab_dnd_handlers(
    tab: &gtk::Box,
    pane_id: PaneId,
    surface: &PaneSurface,
    callbacks: &PaneCallbacks,
) {
    let surface_id = surface.id;
    let drag_widget = tab
        .first_child()
        .map(|child| child.upcast::<gtk::Widget>())
        .unwrap_or_else(|| tab.clone().upcast::<gtk::Widget>());
    let saw_tab_drop_target = callbacks.tab_drag_drop_seen.clone();
    let committed_tab_drop = callbacks.tab_drag_drop_committed.clone();
    let split_candidate = callbacks.tab_drag_split_candidate.clone();
    let opened_new_window = Rc::new(Cell::new(false));

    let drag_source = gtk::DragSource::new();
    drag_source.set_actions(gtk::gdk::DragAction::MOVE);
    let surface_for_payload = surface.clone();
    drag_source.connect_prepare(move |_, _, _| {
        tracing::debug!(%pane_id, %surface_id, "tab drag prepare");
        Some(tab_dnd_content_provider(tab_dnd_payload_string(
            pane_id,
            surface_id,
            &surface_for_payload,
        )))
    });
    let tab_for_begin = tab.clone();
    let saw_target_for_begin = saw_tab_drop_target.clone();
    let committed_for_begin = committed_tab_drop.clone();
    let split_candidate_for_begin = split_candidate.clone();
    let opened_for_begin = opened_new_window.clone();
    drag_source.connect_drag_begin(move |_, _| {
        saw_target_for_begin.set(false);
        committed_for_begin.set(false);
        split_candidate_for_begin.borrow_mut().take();
        opened_for_begin.set(false);
        tab_for_begin.set_opacity(0.4);
        tab_for_begin.add_css_class("flowmux-pane-tab-dragging");
    });
    let tab_for_end = tab.clone();
    let saw_target_for_end = saw_tab_drop_target.clone();
    let committed_for_end = committed_tab_drop.clone();
    let opened_for_end = opened_new_window.clone();
    let new_window_cb_for_end = callbacks.on_tab_drag_to_new_window.clone();
    let close_cb_for_remote_move = callbacks.on_close_surface.clone();
    let split_candidate_for_end = split_candidate.clone();
    let split_cb_for_end = callbacks.on_split_surface_into_pane.clone();
    let surface_for_end = surface.clone();
    drag_source.connect_drag_end(move |_, drag, delete_data| {
        let selected_action = drag.selected_action();
        tracing::debug!(
            %pane_id,
            %surface_id,
            delete_data,
            selected_action = ?selected_action,
            saw_tab_drop_target = saw_target_for_end.get(),
            "tab drag end"
        );
        if !committed_for_end.get() && !opened_for_end.get() {
            if let Some((target_pane, direction)) = split_candidate_for_end.borrow_mut().take() {
                saw_target_for_end.set(true);
                committed_for_end.set(true);
                opened_for_end.set(true);
                tracing::info!(
                    %pane_id,
                    %surface_id,
                    %target_pane,
                    ?direction,
                    "tab drag ended without a drop target; committing previewed split"
                );
                (split_cb_for_end.borrow_mut())(
                    pane_id,
                    surface_id,
                    Some(surface_for_end.clone()),
                    target_pane,
                    direction,
                );
            }
        }
        if selected_action.contains(gtk::gdk::DragAction::MOVE)
            && delete_data
            && !saw_target_for_end.get()
            && !committed_for_end.get()
        {
            tracing::info!(
                %pane_id,
                %surface_id,
                "tab drag moved to another window; closing source tab"
            );
            (close_cb_for_remote_move.borrow_mut())(pane_id, surface_id);
        } else if selected_action.is_empty()
            && !delete_data
            && !saw_target_for_end.get()
            && !opened_for_end.get()
        {
            opened_for_end.set(true);
            tracing::info!(
                %pane_id,
                %surface_id,
                "tab drag ended outside tab targets; opening new window"
            );
            (new_window_cb_for_end.borrow_mut())(pane_id, surface_id);
        }
        split_candidate_for_end.borrow_mut().take();
        saw_target_for_end.set(false);
        tab_for_end.set_opacity(1.0);
        tab_for_end.remove_css_class("flowmux-pane-tab-dragging");
    });
    let tab_for_cancel = tab.clone();
    let saw_target_for_cancel = saw_tab_drop_target.clone();
    let committed_for_cancel = committed_tab_drop.clone();
    let new_window_cb = callbacks.on_tab_drag_to_new_window.clone();
    let opened_for_cancel = opened_new_window.clone();
    let split_candidate_for_cancel = split_candidate.clone();
    let split_cb_for_cancel = callbacks.on_split_surface_into_pane.clone();
    let surface_for_cancel = surface.clone();
    drag_source.connect_drag_cancel(move |_, drag, reason| {
        let selected_action = drag.selected_action();
        tab_for_cancel.set_opacity(1.0);
        tab_for_cancel.remove_css_class("flowmux-pane-tab-dragging");
        tracing::debug!(
            %pane_id,
            %surface_id,
            ?reason,
            selected_action = ?selected_action,
            saw_tab_drop_target = saw_target_for_cancel.get(),
            "tab drag cancel"
        );
        if !committed_for_cancel.get() && !opened_for_cancel.get() {
            if let Some((target_pane, direction)) = split_candidate_for_cancel.borrow_mut().take() {
                saw_target_for_cancel.set(true);
                committed_for_cancel.set(true);
                opened_for_cancel.set(true);
                tracing::info!(
                    %pane_id,
                    %surface_id,
                    %target_pane,
                    ?direction,
                    "tab drag canceled without a drop target; committing previewed split"
                );
                (split_cb_for_cancel.borrow_mut())(
                    pane_id,
                    surface_id,
                    Some(surface_for_cancel.clone()),
                    target_pane,
                    direction,
                );
            }
        }
        if matches!(
            reason,
            gtk::gdk::DragCancelReason::NoTarget | gtk::gdk::DragCancelReason::Error
        ) && selected_action.is_empty()
            && !saw_target_for_cancel.get()
            && !opened_for_cancel.get()
        {
            opened_for_cancel.set(true);
            tracing::info!(
                %pane_id,
                %surface_id,
                "tab drag ended without a drop target; opening new window"
            );
            (new_window_cb.borrow_mut())(pane_id, surface_id);
        }
        split_candidate_for_cancel.borrow_mut().take();
        saw_target_for_cancel.set(false);
        false
    });
    drag_widget.add_controller(drag_source);

    #[cfg(target_os = "macos")]
    install_macos_tab_split_drag_fallback(
        &drag_widget,
        tab,
        pane_id,
        surface,
        callbacks,
        saw_tab_drop_target.clone(),
        committed_tab_drop.clone(),
        opened_new_window.clone(),
    );

    let drop_target =
        gtk::DropTargetAsync::new(Some(tab_dnd_content_formats()), gtk::gdk::DragAction::MOVE);
    drop_target.connect_accept(|target, drop| {
        if tab_dnd_formats_accept_payload(&drop.formats()) {
            true
        } else {
            target.reject_drop(drop);
            false
        }
    });
    // Use motion x to choose the left or right half of the tab and place the
    // indicator. Drop logic uses the same x basis for final_index, so the blue
    // line marks the actual drop position. Hovering the left half of the first
    // tab signals "move to the front".
    let tab_for_motion = tab.clone();
    drop_target.connect_drag_motion(move |_, _drop, x, _y| {
        let width = tab_for_motion.width();
        let after = if width > 0 {
            x > (width as f64) / 2.0
        } else {
            false
        };
        if after {
            tab_for_motion.remove_css_class("flowmux-pane-tab-drop-before");
            tab_for_motion.add_css_class("flowmux-pane-tab-drop-after");
        } else {
            tab_for_motion.remove_css_class("flowmux-pane-tab-drop-after");
            tab_for_motion.add_css_class("flowmux-pane-tab-drop-before");
        }
        gtk::gdk::DragAction::MOVE
    });
    let tab_for_leave = tab.clone();
    drop_target.connect_drag_leave(move |_, _drop| {
        tab_for_leave.remove_css_class("flowmux-pane-tab-drop-before");
        tab_for_leave.remove_css_class("flowmux-pane-tab-drop-after");
    });
    let target_pane = pane_id;
    let target_surface = surface_id;
    let tab_for_drop = tab.clone();
    let dispatch_tab_drop = callbacks.dispatch_tab_drop.clone();
    let position_of_surface_cb = callbacks.position_of_surface_in_pane.clone();
    let saw_target_for_drop = saw_tab_drop_target.clone();
    let committed_for_drop = committed_tab_drop.clone();
    let split_candidate_for_drop = split_candidate.clone();
    drop_target.connect_drop(move |_, drop, x, _y| {
        saw_target_for_drop.set(true);
        committed_for_drop.set(true);
        split_candidate_for_drop.borrow_mut().take();
        tracing::debug!(%target_pane, %target_surface, "tab drop fired");
        tab_for_drop.remove_css_class("flowmux-pane-tab-drop-before");
        tab_for_drop.remove_css_class("flowmux-pane-tab-drop-after");
        let drop = drop.clone();
        let tab_for_drop = tab_for_drop.clone();
        let dispatch_tab_drop = dispatch_tab_drop.clone();
        let position_of_surface_cb = position_of_surface_cb.clone();
        gtk::glib::MainContext::default().spawn_local(async move {
            let payload = match read_tab_dnd_payload_from_drop(&drop).await {
                Ok(payload) => payload,
                Err(error) => {
                    tracing::warn!(error = %error, "tab drop: failed to read payload");
                    drop.finish(gtk::gdk::DragAction::empty());
                    return;
                }
            };
            let src_pane = payload.src_pane;
            let src_surface = payload.src_surface;
            let src_surface_model = payload.surface;
            // Dropping a tab onto itself within the same pane is a no-op.
            if src_pane == target_pane && src_surface == target_surface {
                tracing::debug!(%src_surface, "tab drop: dropped onto self, ignoring");
                drop.finish(gtk::gdk::DragAction::empty());
                return;
            }

            // If drop x is left of half the tab width, insert before the target;
            // otherwise insert after it. Since target_index is the final index,
            // count sibling positions in the parent GtkBox to find the target's
            // current index.
            let Some(parent) = tab_for_drop.parent() else {
                drop.finish(gtk::gdk::DragAction::empty());
                return;
            };
            let mut target_index: usize = 0;
            let mut child = parent.first_child();
            while let Some(c) = child {
                if c == tab_for_drop.clone().upcast::<gtk::Widget>() {
                    break;
                }
                target_index += 1;
                child = c.next_sibling();
            }

            let tab_width = tab_for_drop.width();
            let after = if tab_width > 0 {
                x > (tab_width as f64) / 2.0
            } else {
                false
            };

            // In the same box:
            // - When the source is left of the target (src_idx < target_index),
            //   "before target" means target_index - 1, and "after target" means target_index.
            // - When the source is right of the target (src_idx > target_index),
            //   "before target" means target_index, and "after target" means target_index + 1.
            // The final_index is computed after removing the source and inserting
            // next to target, using the source position exposed from PaneRegistry::surface_tabs.
            let src_index = (position_of_surface_cb)(target_pane, src_surface);
            let final_index = match (after, src_index) {
                (false, Some(s)) if s < target_index => target_index.saturating_sub(1),
                (false, _) => target_index,
                (true, Some(s)) if s < target_index => target_index,
                (true, _) => target_index.saturating_add(1),
            };

            tracing::info!(
                %target_pane,
                %src_surface,
                %target_surface,
                target_index,
                src_index = ?src_index,
                final_index,
                after,
                "tab drop: dispatching reorder callback"
            );
            let command = if src_pane == target_pane {
                TabDropCommand::Reorder {
                    pane: target_pane,
                    surface: src_surface,
                    target_index: final_index,
                }
            } else {
                TabDropCommand::MoveToPane {
                    src_pane,
                    surface: src_surface,
                    surface_model: src_surface_model,
                    dst_pane: target_pane,
                    target_index: final_index,
                }
            };
            let Some(ack) = dispatch_tab_drop(command) else {
                tracing::warn!("tab drop: command bridge is unavailable");
                drop.finish(gtk::gdk::DragAction::empty());
                return;
            };
            match ack.await {
                Ok(Ok(())) => drop.finish(gtk::gdk::DragAction::MOVE),
                Ok(Err(error)) => {
                    tracing::warn!(%error, "tab drop: move rejected");
                    drop.finish(gtk::gdk::DragAction::empty());
                }
                Err(error) => {
                    tracing::warn!(%error, "tab drop: move acknowledgement dropped");
                    drop.finish(gtk::gdk::DragAction::empty());
                }
            }
        });
        true
    });
    tab.add_controller(drop_target);
}

#[cfg(target_os = "macos")]
fn install_macos_tab_split_drag_fallback(
    drag_widget: &gtk::Widget,
    tab: &gtk::Box,
    pane_id: PaneId,
    surface: &PaneSurface,
    callbacks: &PaneCallbacks,
    saw_tab_drop_target: Rc<Cell<bool>>,
    committed_tab_drop: Rc<Cell<bool>>,
    opened_new_window: Rc<Cell<bool>>,
) {
    let gesture = gtk::GestureDrag::new();
    gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
    gesture.set_button(gtk::gdk::BUTTON_PRIMARY);

    let tab_for_begin = tab.clone();
    let saw_target_for_begin = saw_tab_drop_target.clone();
    let committed_for_begin = committed_tab_drop.clone();
    let opened_for_begin = opened_new_window.clone();
    gesture.connect_drag_begin(move |_, _, _| {
        saw_target_for_begin.set(false);
        committed_for_begin.set(false);
        opened_for_begin.set(false);
        tab_for_begin.set_opacity(0.4);
        tab_for_begin.add_css_class("flowmux-pane-tab-dragging");
    });

    let tab_for_end = tab.clone();
    let split_cb = callbacks.on_split_surface_into_pane.clone();
    let surface_model = surface.clone();
    let surface_id = surface.id;
    gesture.connect_drag_end(move |_, dx, dy| {
        tab_for_end.set_opacity(1.0);
        tab_for_end.remove_css_class("flowmux-pane-tab-dragging");
        if committed_tab_drop.get() || opened_new_window.get() {
            return;
        }
        let Some(direction) = tab_drag_split_direction_from_delta(dx, dy) else {
            return;
        };
        saw_tab_drop_target.set(true);
        committed_tab_drop.set(true);
        opened_new_window.set(true);
        tracing::info!(
            %pane_id,
            %surface_id,
            ?direction,
            dx,
            dy,
            "macOS tab drag fallback committing split"
        );
        (split_cb.borrow_mut())(
            pane_id,
            surface_id,
            Some(surface_model.clone()),
            pane_id,
            direction,
        );
    });

    drag_widget.add_controller(gesture);
}

#[cfg(any(target_os = "macos", test))]
fn tab_drag_split_direction_from_delta(dx: f64, dy: f64) -> Option<SplitDirection> {
    const MIN_DRAG_DISTANCE: f64 = 80.0;
    let right = dx >= MIN_DRAG_DISTANCE;
    let down = dy >= MIN_DRAG_DISTANCE;
    match (right, down) {
        (false, false) => None,
        (true, false) => Some(SplitDirection::Vertical),
        (false, true) => Some(SplitDirection::Horizontal),
        (true, true) if dy > dx => Some(SplitDirection::Horizontal),
        (true, true) => Some(SplitDirection::Vertical),
    }
}

/// Which pane-body region a tab drag is over, deciding the drop outcome and the
/// translucent preview shown on the pane.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PaneDropZone {
    /// Not hovering / no preview.
    None,
    /// Add as a tab at the end (top-left region).
    Tab,
    /// Split side-by-side, new pane on the right (right region).
    SplitRight,
    /// Split stacked, new pane below (bottom region).
    SplitDown,
}

impl PaneDropZone {
    fn split_direction(self) -> Option<SplitDirection> {
        match self {
            PaneDropZone::SplitRight => Some(SplitDirection::Vertical),
            PaneDropZone::SplitDown => Some(SplitDirection::Horizontal),
            PaneDropZone::None | PaneDropZone::Tab => None,
        }
    }
}

/// Map normalized pointer coordinates inside a pane to a drop zone: the bottom
/// half splits down, the upper-right splits right, the upper-left adds a tab.
fn drop_zone(fx: f64, fy: f64) -> PaneDropZone {
    if fy >= 0.5 {
        PaneDropZone::SplitDown
    } else if fx >= 0.5 {
        PaneDropZone::SplitRight
    } else {
        PaneDropZone::Tab
    }
}

fn landed_drop_zone(previewed: PaneDropZone, fx: Option<f64>, fy: Option<f64>) -> PaneDropZone {
    if previewed.split_direction().is_some() {
        return previewed;
    }

    match (fx, fy) {
        (Some(fx), Some(fy)) => drop_zone(fx, fy),
        _ => PaneDropZone::Tab,
    }
}

/// Make the body of a pane a drop target for tab drags. The region decides the
/// outcome: right → split side-by-side, bottom → split stacked, top-left → add a
/// tab at the end. A translucent overlay previews the split while dragging.
fn attach_pane_body_dnd(
    widget: &impl IsA<gtk::Widget>,
    pane_id: PaneId,
    zone: Rc<Cell<PaneDropZone>>,
    preview: gtk::DrawingArea,
    callbacks: &PaneCallbacks,
) {
    let frame: gtk::Widget = widget.clone().upcast();

    // Preview: track the pointer during the drag and repaint the overlay.
    let motion = gtk::DropControllerMotion::new();
    motion.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let frame = frame.clone();
        let zone = zone.clone();
        let preview = preview.clone();
        let saw_drop_target = callbacks.tab_drag_drop_seen.clone();
        let split_candidate = callbacks.tab_drag_split_candidate.clone();
        motion.connect_motion(move |motion, x, y| {
            if motion
                .drop()
                .is_some_and(|drop| tab_dnd_formats_accept_payload(&drop.formats()))
            {
                saw_drop_target.set(true);
            }
            let (w, h) = (frame.width() as f64, frame.height() as f64);
            let new = if w > 0.0 && h > 0.0 {
                drop_zone(x / w, y / h)
            } else {
                PaneDropZone::None
            };
            if let Some(direction) = new.split_direction() {
                *split_candidate.borrow_mut() = Some((pane_id, direction));
            } else {
                split_candidate.borrow_mut().take();
            }
            if zone.get() != new {
                zone.set(new);
                preview.queue_draw();
            }
        });
    }
    {
        let zone = zone.clone();
        let preview = preview.clone();
        let saw_drop_target = callbacks.tab_drag_drop_seen.clone();
        let committed_drop = callbacks.tab_drag_drop_committed.clone();
        let split_candidate = callbacks.tab_drag_split_candidate.clone();
        motion.connect_leave(move |_| {
            if !committed_drop.get() {
                if zone.get().split_direction().is_some() {
                    saw_drop_target.set(true);
                } else {
                    saw_drop_target.set(false);
                    split_candidate.borrow_mut().take();
                }
            }
            if zone.get() != PaneDropZone::None {
                zone.set(PaneDropZone::None);
                preview.queue_draw();
            }
        });
    }
    widget.add_controller(motion);

    let drop_target =
        gtk::DropTargetAsync::new(Some(tab_dnd_content_formats()), gtk::gdk::DragAction::MOVE);
    drop_target.set_propagation_phase(gtk::PropagationPhase::Capture);
    drop_target.connect_accept(|target, drop| {
        if tab_dnd_formats_accept_payload(&drop.formats()) {
            true
        } else {
            target.reject_drop(drop);
            false
        }
    });
    let dispatch_tab_drop = callbacks.dispatch_tab_drop.clone();
    let target_pane = pane_id;
    let saw_drop_target = callbacks.tab_drag_drop_seen.clone();
    let committed_drop = callbacks.tab_drag_drop_committed.clone();
    let split_candidate = callbacks.tab_drag_split_candidate.clone();
    drop_target.connect_drop(move |_, drop, x, y| {
        saw_drop_target.set(true);
        committed_drop.set(true);
        split_candidate.borrow_mut().take();
        let (w, h) = (frame.width() as f64, frame.height() as f64);
        let landed = landed_drop_zone(
            zone.get(),
            (w > 0.0).then_some(x / w),
            (h > 0.0).then_some(y / h),
        );
        // Clear the preview now that the drop is committing.
        zone.set(PaneDropZone::None);
        preview.queue_draw();

        let drop = drop.clone();
        let dispatch_tab_drop = dispatch_tab_drop.clone();
        gtk::glib::MainContext::default().spawn_local(async move {
            let payload = match read_tab_dnd_payload_from_drop(&drop).await {
                Ok(payload) => payload,
                Err(error) => {
                    tracing::warn!(error = %error, "pane-body tab drop: failed to read payload");
                    drop.finish(gtk::gdk::DragAction::empty());
                    return;
                }
            };
            let src_pane = payload.src_pane;
            let src_surface = payload.src_surface;
            let src_surface_model = payload.surface;
            let command = match landed {
                PaneDropZone::SplitRight => TabDropCommand::SplitIntoPane {
                    src_pane,
                    surface: src_surface,
                    surface_model: src_surface_model,
                    dst_pane: target_pane,
                    direction: SplitDirection::Vertical,
                },
                PaneDropZone::SplitDown => TabDropCommand::SplitIntoPane {
                    src_pane,
                    surface: src_surface,
                    surface_model: src_surface_model,
                    dst_pane: target_pane,
                    direction: SplitDirection::Horizontal,
                },
                PaneDropZone::Tab | PaneDropZone::None => TabDropCommand::MoveToPane {
                    src_pane,
                    surface: src_surface,
                    surface_model: src_surface_model,
                    dst_pane: target_pane,
                    target_index: usize::MAX,
                },
            };
            let Some(ack) = dispatch_tab_drop(command) else {
                tracing::warn!("pane-body tab drop: command bridge is unavailable");
                drop.finish(gtk::gdk::DragAction::empty());
                return;
            };
            match ack.await {
                Ok(Ok(())) => drop.finish(gtk::gdk::DragAction::MOVE),
                Ok(Err(error)) => {
                    tracing::warn!(%error, "pane-body tab drop: move rejected");
                    drop.finish(gtk::gdk::DragAction::empty());
                }
                Err(error) => {
                    tracing::warn!(%error, "pane-body tab drop: move acknowledgement dropped");
                    drop.finish(gtk::gdk::DragAction::empty());
                }
            }
        });
        true
    });
    widget.add_controller(drop_target);
}

/// Attach a single new surface to an already-rendered pane: appends a
/// tab to its tab bar, mounts the panel widget into the pane's stack,
/// records it in `registry`, and activates it. Returns `false` if the
/// pane has not been rendered yet (e.g. workspace not visible) — the
/// caller can fall back to a full re-render.
///
/// This path leaves existing tab/browser GTK widgets untouched, preserving
/// browser navigation state and terminal shell sessions in other panes.
pub fn attach_surface_to_pane(
    pane_id: PaneId,
    workspace: WorkspaceId,
    surface: &PaneSurface,
    callbacks: &PaneCallbacks,
    registry: Rc<RefCell<PaneRegistry>>,
    theme: Arc<ResolvedTheme>,
) -> bool {
    let (tabs, stack, frame) = {
        let r = registry.borrow();
        let Some(tabs) = r.pane_tab_containers.get(&pane_id).cloned() else {
            return false;
        };
        let Some(stack) = r.surface_stacks.get(&pane_id).cloned() else {
            return false;
        };
        let Some(frame) = r
            .pane_frames
            .get(&pane_id)
            .and_then(|w| w.downcast_ref::<gtk::Frame>().cloned())
        else {
            return false;
        };
        (tabs, stack, frame)
    };

    let (tab, label) = build_surface_tab_widget(pane_id, surface, true, callbacks);
    tabs.append(&tab);

    let widget = build_panel(
        pane_id,
        workspace,
        surface,
        Vec::new(),
        callbacks,
        registry.clone(),
        theme,
        frame,
    );
    stack.add_named(&widget, Some(&surface.id.to_string()));

    {
        let mut r = registry.borrow_mut();
        r.surface_tab_labels.insert(surface.id, label);
        r.surface_tabs
            .entry(pane_id)
            .or_default()
            .push((surface.id, tab.upcast::<gtk::Widget>()));
        r.activate_surface(pane_id, surface.id);
        r.refresh_tab_multi_class(pane_id);
    }
    true
}

fn surface_tab(surface: &PaneSurface, active: bool) -> (gtk::Box, gtk::Label) {
    let tab = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    tab.add_css_class("flowmux-pane-tab");
    if active {
        tab.add_css_class("active");
    }

    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("flowmux-pane-tab-main");
    button.set_tooltip_text(Some(&surface.title));
    button.set_focus_on_click(false);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let icon_name = match surface.kind {
        SurfaceKind::Terminal { .. } => "utilities-terminal-symbolic",
        SurfaceKind::Browser { .. } => "web-browser-symbolic",
        SurfaceKind::Editor { .. } => "text-x-generic-symbolic",
    };
    row.append(&gtk::Image::from_icon_name(icon_name));
    let label = gtk::Label::new(Some(&surface.title));
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_max_width_chars(18);
    label.set_tooltip_text(Some(&surface.title));
    row.append(&label);
    button.set_child(Some(&row));
    tab.append(&button);

    let close = gtk::Button::from_icon_name("window-close-symbolic");
    close.add_css_class("flat");
    close.add_css_class("flowmux-pane-tab-close");
    close.set_tooltip_text(Some("Close tab"));
    close.set_focus_on_click(false);
    tab.append(&close);

    (tab, label)
}

fn pane_tool_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon_name);
    button.add_css_class("flat");
    button.add_css_class("flowmux-pane-tool");
    button.set_tooltip_text(Some(tooltip));
    button.set_focus_on_click(false);
    button
}

fn pane_menu_button(pane_id: PaneId, callbacks: &PaneCallbacks) -> gtk::MenuButton {
    let button = gtk::MenuButton::new();
    button.set_icon_name("view-more-symbolic");
    button.add_css_class("flat");
    button.add_css_class("flowmux-pane-tool");
    button.set_tooltip_text(Some("Pane actions"));
    button.set_focus_on_click(false);

    let popover = gtk::Popover::new();
    popover.set_has_arrow(false);
    let items = gtk::Box::new(gtk::Orientation::Vertical, 0);
    items.set_margin_top(4);
    items.set_margin_bottom(4);

    let close = gtk::Button::with_label("Close Pane");
    close.add_css_class("flat");
    close.set_halign(gtk::Align::Fill);
    close.set_hexpand(true);
    if let Some(label) = close.child().and_downcast::<gtk::Label>() {
        label.set_xalign(0.0);
    }
    {
        let popover = popover.clone();
        let callback = callbacks.on_close_pane.clone();
        close.connect_clicked(move |_| {
            popover.popdown();
            (callback.borrow_mut())(pane_id);
        });
    }
    items.append(&close);
    popover.set_child(Some(&items));
    button.set_popover(Some(&popover));
    button
}

#[cfg(all(test, not(target_os = "macos")))]
mod pane_menu_tests {
    use super::*;

    #[gtk::test]
    fn pane_menu_close_item_dispatches_the_owning_pane() {
        let pane = PaneId::new();
        let closed = Rc::new(RefCell::new(Vec::new()));
        let mut callbacks = PaneCallbacks::noop_for_test();
        callbacks.on_close_pane = {
            let closed = closed.clone();
            Rc::new(RefCell::new(move |pane| closed.borrow_mut().push(pane)))
        };

        let menu = pane_menu_button(pane, &callbacks);
        assert_eq!(menu.icon_name().as_deref(), Some("view-more-symbolic"));
        assert_eq!(menu.tooltip_text().as_deref(), Some("Pane actions"));

        let popover = menu
            .popover()
            .and_downcast::<gtk::Popover>()
            .expect("pane menu owns a popover");
        let items = popover
            .child()
            .and_downcast::<gtk::Box>()
            .expect("pane menu popover owns an item list");
        let close = items
            .first_child()
            .and_downcast::<gtk::Button>()
            .expect("pane menu starts with the close action");
        assert_eq!(close.label().as_deref(), Some("Close Pane"));

        close.emit_clicked();
        assert_eq!(&*closed.borrow(), &[pane]);
    }
}

fn build_panel(
    pane_id: PaneId,
    workspace: WorkspaceId,
    surface: &PaneSurface,
    argv: Vec<String>,
    callbacks: &PaneCallbacks,
    registry: Rc<RefCell<PaneRegistry>>,
    theme: Arc<ResolvedTheme>,
    frame: gtk::Frame,
) -> gtk::Widget {
    match &surface.kind {
        SurfaceKind::Terminal { cwd, shell } => {
            let opts = (callbacks.read_options)();
            let inherited_shell = argv.first().map(String::as_str);
            let requested_shell = preferred_shell(
                shell.as_deref().or(inherited_shell),
                opts.default_shell.as_deref(),
            )
            .map(str::to_string);
            let mut argv = argv;
            let mut resolved_shell = None;
            let mut shell_warning = None;
            if let Some(requested_shell) = requested_shell.as_deref() {
                match flowmux_terminal::validate_shell_command(requested_shell) {
                    Ok(()) => {
                        resolved_shell = Some(requested_shell.to_string());
                        argv = vec![requested_shell.to_string()];
                    }
                    Err(error) => {
                        tracing::warn!(shell = requested_shell, %error, "configured shell is unavailable");
                        argv.clear();
                        shell_warning = Some(format!(
                            "flowmux: cannot start shell {requested_shell:?}: {error}\r\n\
                             Falling back to $SHELL.\r\n"
                        ));
                    }
                }
            }
            // Match the per-PID socket that `flowmux::main` binds, so
            // PTYs inside this GUI window route their notifications
            // back to the SAME GUI even when multiple flowmux windows
            // are running. Same process ⇒ same path.
            let socket = flowmux_config::paths::runtime_socket_for_pid(std::process::id());
            let bundled_cli = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("flowmux")))
                .filter(|p| p.exists());
            let extra_env = flowmux_terminal::agent_pty_env(
                pane_id,
                surface.id,
                workspace,
                &socket,
                bundled_cli.as_deref(),
            );
            // Start the new terminal widget with the current font + zoom
            // options so a freshly spawned tab matches the live ones.
            let font = theme.font_with_overrides(opts.font_family.as_deref(), opts.font_size);
            let resume_command = take_restored_agent_shell_command(
                surface.id,
                opts.auto_resume_agent_sessions,
                flowmux_state::default_agent_session_store(),
            );
            let is_resuming_agent = resume_command.is_some();
            let mut resume_input = None;
            if let Some(command) = resume_command {
                if is_flatpak_sandbox() {
                    // Flatpak's host-shell bridge needs to own the controlling
                    // terminal, so keep its normal spawn path and feed there.
                    argv = shell.clone().map(|s| vec![s]).unwrap_or_default();
                    resume_input = Some(format!("{command}\n"));
                } else {
                    let shell = resolved_shell
                        .clone()
                        .or_else(|| argv.first().cloned())
                        .or_else(|| std::env::var("SHELL").ok())
                        .unwrap_or_else(|| "/bin/bash".into());
                    argv = resumed_agent_shell_argv(&shell, &command);
                }
            }

            // VTE is the only terminal backend. GhosttyPane owns the
            // PTY + render; title/cwd changes are forwarded from inside it.
            let pane: PaneTerminal = GhosttyPane::spawn(
                pane_id,
                surface.id,
                argv,
                cwd.clone(),
                extra_env,
                opts.scrollback_lines_or_default(),
                callbacks.clone(),
            );
            theme.apply_to_ghostty(&pane);
            pane.set_font(&font);
            pane.set_font_scale(opts.zoom_factor());
            pane.set_cursor_blink(opts.cursor_blink, opts.cursor_blink_interval_ms);
            if let Some(scrollback) = scrollback_to_restore(
                opts.restore_terminal_scrollback,
                is_resuming_agent,
                surface.scrollback.as_deref(),
            ) {
                pane.restore_scrollback(&scrollback);
            }
            if let Some(message) = shell_warning {
                pane.show_message(&message);
            }
            if let Some(command) = resume_input {
                let terminal = pane.clone();
                gtk::glib::idle_add_local_once(move || {
                    if let Err(error) = terminal.write_input(command.as_bytes()) {
                        tracing::warn!(%error, "failed to start restored agent session");
                    }
                });
            }
            let pane_terminal: PaneTerminal = pane;

            // Toggle the .focused class on frame focus enter/leave. theme.rs
            // CSS draws a 1px border for the focused pane using the focus
            // border options.
            let frame_in = frame.clone();
            let frame_out = frame.clone();
            let focus = gtk::EventControllerFocus::new();
            focus.connect_enter(move |_| {
                if !frame_in.has_css_class("focused") {
                    frame_in.add_css_class("focused");
                }
            });
            focus.connect_leave(move |_| {
                frame_out.remove_css_class("focused");
            });
            pane_terminal.add_controller(focus);

            // The terminal widget is the pane's root; keeping the same root
            // instance alive preserves the running PTY child across split
            // tree changes.
            let widget = pane_terminal.root_widget();
            let mut r = registry.borrow_mut();
            r.terminals.insert(surface.id, pane_terminal);
            r.surface_workspace.insert(surface.id, workspace);
            widget
        }
        SurfaceKind::Browser { initial_url } => {
            let opts = (callbacks.read_options)();
            let pane = BrowserPane::new(
                pane_id,
                surface.id,
                initial_url.as_deref(),
                callbacks.clone(),
                opts.default_browser_engine.clone(),
                opts.persist_browser_session,
            );
            // Apply the zoom option to the new browser tab immediately so
            // widgets created before apply_zoom still start in sync.
            pane.set_zoom_level(opts.zoom_factor());
            let browser_pane_id = pane.pane_id_handle();

            // Browser tabs use the same focus marker and on_focus callback.
            // on_focus updates WindowController.focused_pane, then
            // RefreshWindowTitle recomputes the window title from the new
            // active surface label.
            //
            // Attach the controller to BrowserPane.root, not web_view. A
            // BrowserPane contains the chrome row plus web_view; if only web_view
            // owns the controller, clicking the address bar makes web_view leave,
            // clears the .focused border, and never calls on_focus for the chrome
            // row. On root, GTK4 EventControllerFocus emits enter/leave for the
            // widget plus descendants, so focus moves between the chrome row and
            // web_view are ignored and leave fires only when focus exits the pane.
            let frame_in = frame.clone();
            let frame_out = frame.clone();
            let on_focus = callbacks.on_focus.clone();
            let focus = gtk::EventControllerFocus::new();
            focus.connect_enter(move |_| {
                let pane_id = browser_pane_id.get();
                tracing::debug!(%pane_id, "browser pane focus enter");
                if !frame_in.has_css_class("focused") {
                    frame_in.add_css_class("focused");
                }
                (on_focus.borrow_mut())(pane_id);
            });
            focus.connect_leave(move |_| {
                frame_out.remove_css_class("focused");
            });
            pane.root.add_controller(focus);

            let widget = pane.root.clone().upcast::<gtk::Widget>();
            let mut r = registry.borrow_mut();
            r.browsers.insert(surface.id, pane);
            r.surface_workspace.insert(surface.id, workspace);
            widget
        }
        SurfaceKind::Editor { workspace_root, .. } => {
            let editor = EditorPane::new(pane_id, surface.id, workspace_root.clone());

            let frame_in = frame.clone();
            let frame_out = frame.clone();
            let on_focus = callbacks.on_focus.clone();
            let editor_pane_id = editor.clone();
            let focus = gtk::EventControllerFocus::new();
            focus.connect_enter(move |_| {
                if !frame_in.has_css_class("focused") {
                    frame_in.add_css_class("focused");
                }
                (on_focus.borrow_mut())(editor_pane_id.pane_id());
            });
            focus.connect_leave(move |_| {
                frame_out.remove_css_class("focused");
            });
            editor.root.add_controller(focus);

            let widget = editor.root.clone().upcast::<gtk::Widget>();
            let mut registry = registry.borrow_mut();
            registry.editors.insert(surface.id, editor);
            registry.surface_workspace.insert(surface.id, workspace);
            widget
        }
    }
}

fn preferred_shell<'a>(per_tab: Option<&'a str>, default: Option<&'a str>) -> Option<&'a str> {
    per_tab
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| default.map(str::trim).filter(|value| !value.is_empty()))
}

fn take_restored_agent_shell_command(
    surface: SurfaceId,
    enabled: bool,
    store: Option<flowmux_state::AgentSessionStore>,
) -> Option<String> {
    enabled
        .then_some(store)
        .flatten()
        .and_then(|store| store.take_surface(surface).ok().flatten())
        .and_then(|session| session.shell_command())
}

fn resumed_agent_shell_argv(shell: &str, command: &str) -> Vec<String> {
    let shell_command = format!("{command}; exec {} -l", shell_quote(shell));
    vec![shell.into(), "-lc".into(), shell_command]
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn scrollback_to_restore(
    enabled: bool,
    is_resuming_agent: bool,
    scrollback: Option<&str>,
) -> Option<String> {
    if !enabled || is_resuming_agent {
        return None;
    }
    scrollback
        .map(normalize_scrollback_snapshot)
        .filter(|text| !text.is_empty())
}

fn normalize_scrollback_snapshot(text: &str) -> String {
    let lines: Vec<_> = text.lines().collect();
    let Some(first) = lines.iter().position(|line| !line.trim().is_empty()) else {
        return String::new();
    };
    let last = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .unwrap_or(first);
    let meaningful = &lines[first..=last];
    if meaningful
        .iter()
        .filter(|line| !line.trim().is_empty())
        .count()
        <= 1
    {
        return String::new();
    }
    meaningful.join("\n")
}

#[cfg(test)]
mod resume_tests {
    use super::*;

    #[test]
    fn per_tab_shell_overrides_default_and_empty_values_are_ignored() {
        assert_eq!(
            preferred_shell(Some(" /bin/dash "), Some("/bin/zsh")),
            Some("/bin/dash")
        );
        assert_eq!(preferred_shell(None, Some(" /bin/zsh ")), Some("/bin/zsh"));
        assert_eq!(
            preferred_shell(Some("  "), Some("/bin/sh")),
            Some("/bin/sh")
        );
        assert_eq!(preferred_shell(Some("  "), Some("  ")), None);
    }

    #[test]
    fn restored_agent_command_respects_setting_and_surface_binding() {
        let dir = tempfile::tempdir().unwrap();
        let store = flowmux_state::AgentSessionStore::new(dir.path().to_path_buf());
        let surface = SurfaceId::new();
        store.record("claude", surface, "session-1").unwrap();

        assert!(take_restored_agent_shell_command(surface, false, Some(store.clone())).is_none());
        assert!(store.lookup_surface(surface).is_some());
        let command =
            take_restored_agent_shell_command(surface, true, Some(store.clone())).unwrap();
        assert!(command.contains("'claude' '--resume' 'session-1'"));
        assert!(store.lookup_surface(surface).is_none());
        assert!(take_restored_agent_shell_command(SurfaceId::new(), true, None).is_none());
    }

    #[test]
    fn resumed_agent_starts_as_hidden_shell_command_then_returns_to_login_shell() {
        let argv = resumed_agent_shell_argv("/bin/zsh", "resume-command");
        assert_eq!(argv[0], "/bin/zsh");
        assert_eq!(argv[1], "-lc");
        assert_eq!(
            argv[2], "resume-command; exec '/bin/zsh' -l",
            "resume text must be a shell argv, never terminal input"
        );
    }

    #[test]
    fn agent_resume_does_not_replay_scrollback_after_the_clear_sequence() {
        let scrollback = Some("old prompt\n\n\nold cursor");
        assert_eq!(
            scrollback_to_restore(true, false, scrollback),
            Some("old prompt\n\n\nold cursor".into())
        );
        assert_eq!(scrollback_to_restore(true, true, scrollback), None);
        assert_eq!(scrollback_to_restore(false, false, scrollback), None);
        assert_eq!(
            scrollback_to_restore(true, false, Some("\n\n➜  work \n")),
            None,
            "legacy idle viewport padding must not move the new cursor"
        );
    }

    #[test]
    fn empty_shell_viewport_is_not_persisted_as_scrollback() {
        assert_eq!(
            normalize_scrollback_snapshot("\n\n\n➜  work \n"),
            "",
            "viewport padding plus one idle prompt is not history"
        );
        assert_eq!(
            normalize_scrollback_snapshot("➜  work \n\n\n"),
            "",
            "trailing viewport padding is not history"
        );
    }

    #[test]
    fn meaningful_scrollback_drops_only_viewport_padding() {
        assert_eq!(
            normalize_scrollback_snapshot("\n\ncommand output\n\n➜  work \n\n"),
            "command output\n\n➜  work "
        );
    }
}
