// SPDX-License-Identifier: GPL-3.0-or-later
//! Render a workspace's pane tree as recursive GTK widgets.
//!
//! A split node becomes `gtk::Paned`; a leaf pane becomes a framed
//! cmux-style pane with a surface tab bar and a stack of terminal or
//! browser panels.

use crate::theme::ResolvedTheme;
use crate::ui::browser_pane::BrowserPane;
use crate::ui::terminal_pane::{PaneCallbacks, TerminalPane};
use flowmux_core::{
    terminal_tab_title_for_cwd, Pane, PaneContent, PaneId, PaneSurface, SplitDirection, Surface,
    SurfaceId, SurfaceKind, WorkspaceId,
};
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use webkit6::prelude::*;

const TAB_DND_MIME: &str = "application/x-flowmux-tab";
const TAB_DND_PAYLOAD_MAX: usize = 128;

#[derive(Default)]
pub struct PaneRegistry {
    pub terminals: HashMap<SurfaceId, TerminalPane>,
    pub browsers: HashMap<SurfaceId, BrowserPane>,
    active_terminal_by_pane: HashMap<PaneId, SurfaceId>,
    active_browser_by_pane: HashMap<PaneId, SurfaceId>,
    pane_frames: HashMap<PaneId, gtk::Widget>,
    surface_stacks: HashMap<PaneId, gtk::Stack>,
    pub surface_tabs: HashMap<PaneId, Vec<(SurfaceId, gtk::Widget)>>,
    /// Tab-bar `gtk::Box` so incremental tab additions can `append`
    /// into the same row instead of rebuilding the whole pane.
    pane_tab_containers: HashMap<PaneId, gtk::Box>,
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
    pub content: gtk::Widget,
    pub focus: gtk::Widget,
}

impl PaneRegistry {
    pub fn active_terminal(&self, pane: PaneId) -> Option<&TerminalPane> {
        self.active_terminal_by_pane
            .get(&pane)
            .and_then(|surface| self.terminals.get(surface))
    }

    pub fn active_browser(&self, pane: PaneId) -> Option<&BrowserPane> {
        self.active_browser_by_pane
            .get(&pane)
            .and_then(|surface| self.browsers.get(surface))
    }

    pub fn pane_frame(&self, pane: PaneId) -> Option<gtk::Widget> {
        self.pane_frames.get(&pane).cloned()
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

    pub fn active_surface(&self, pane: PaneId) -> Option<SurfaceId> {
        self.active_terminal_by_pane
            .get(&pane)
            .or_else(|| self.active_browser_by_pane.get(&pane))
            .copied()
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
                    .current_dir()
                    .map(|cwd| (terminal.id, *surface, cwd))
            })
            .collect()
    }

    pub fn set_surface_title(&self, surface: SurfaceId, title: &str) {
        if let Some(label) = self.surface_tab_labels.get(&surface) {
            label.set_text(title);
            label.set_tooltip_text(Some(title));
        }
    }

    #[cfg(test)]
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
            self.pane_frames.remove(&pane);
            self.surface_stacks.remove(&pane);
            self.surface_tabs.remove(&pane);
            self.pane_tab_containers.remove(&pane);
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
            self.terminals.remove(&surface);
            self.browsers.remove(&surface);
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
        } else if self.browsers.contains_key(&surface) {
            self.active_browser_by_pane.insert(pane, surface);
            self.active_terminal_by_pane.remove(&pane);
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
            self.terminals.remove(&s);
            self.browsers.remove(&s);
            self.surface_tab_labels.remove(&s);
            self.surface_workspace.remove(&s);
        }
        self.surface_tabs.remove(&pane);
        self.surface_stacks.remove(&pane);
        self.pane_frames.remove(&pane);
        self.pane_tab_containers.remove(&pane);
        self.active_terminal_by_pane.remove(&pane);
        self.active_browser_by_pane.remove(&pane);
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
        self.terminals.remove(&surface);
        self.browsers.remove(&surface);
        self.surface_tab_labels.remove(&surface);
        self.surface_workspace.remove(&surface);
        if self.active_terminal_by_pane.get(&pane) == Some(&surface) {
            self.active_terminal_by_pane.remove(&pane);
        }
        if self.active_browser_by_pane.get(&pane) == Some(&surface) {
            self.active_browser_by_pane.remove(&pane);
        }
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

        let focus = if let Some(terminal) = self.terminals.remove(&surface) {
            terminal.root_widget()
        } else if let Some(browser) = self.browsers.remove(&surface) {
            browser.web_view.clone().upcast::<gtk::Widget>()
        } else {
            content.clone()
        };
        self.surface_tab_labels.remove(&surface);
        self.surface_workspace.remove(&surface);
        if self.active_terminal_by_pane.get(&pane) == Some(&surface) {
            self.active_terminal_by_pane.remove(&pane);
        }
        if self.active_browser_by_pane.get(&pane) == Some(&surface) {
            self.active_browser_by_pane.remove(&pane);
        }

        Some(TornOffSurface {
            pane,
            surface,
            title,
            content,
            focus,
        })
    }
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
/// so other panes' VTE shell sessions and browser navigation state survive
/// without rerender. The daemon-side split_pane must already have run before
/// this call so the tree shape is decided.
///
/// `parent_stack_name` is supplied by the caller so a target frame that was a
/// direct stack child can be re-added with the same name, the workspace id.
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
            workspace, *id, content, argv, cwd, callbacks, registry, theme,
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

fn build_leaf_pane(
    workspace: WorkspaceId,
    pane_id: PaneId,
    content: &PaneContent,
    argv: Vec<String>,
    cwd: Option<std::path::PathBuf>,
    callbacks: &PaneCallbacks,
    registry: Rc<RefCell<PaneRegistry>>,
    theme: Arc<ResolvedTheme>,
) -> gtk::Widget {
    let surfaces = materialize_surfaces(content, cwd);
    let active = match content {
        PaneContent::Tabs { active, .. }
            if surfaces.iter().any(|surface| surface.id == *active) =>
        {
            *active
        }
        _ => surfaces[0].id,
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
        let pane_id = pane_id;
        split_right.connect_clicked(move |_| (cb.borrow_mut())(pane_id));
    }
    tools.append(&split_right);

    let split_down = pane_tool_button("go-down-symbolic", "Split down");
    {
        let cb = callbacks.on_split_down.clone();
        let pane_id = pane_id;
        split_down.connect_clicked(move |_| (cb.borrow_mut())(pane_id));
    }
    tools.append(&split_down);

    let add = pane_tool_button("tab-new-symbolic", "Add tab");
    {
        let cb = callbacks.on_new_surface.clone();
        let pane_id = pane_id;
        add.connect_clicked(move |_| (cb.borrow_mut())(pane_id));
    }
    tools.append(&add);

    let add_browser = pane_tool_button("web-browser-symbolic", "Add browser tab");
    {
        let cb = callbacks.on_new_browser_surface.clone();
        let pane_id = pane_id;
        add_browser.connect_clicked(move |_| (cb.borrow_mut())(pane_id));
    }
    tools.append(&add_browser);

    stack.set_visible_child_name(&active.to_string());
    tabbar.append(&tabs);
    tabbar.append(&spacer);
    tabbar.append(&tools);
    root.append(&tabbar);
    root.append(&stack);
    frame.set_child(Some(&root));

    {
        let frame_widget = frame.clone().upcast::<gtk::Widget>();
        let mut r = registry.borrow_mut();
        r.pane_frames.insert(pane_id, frame_widget);
        r.surface_stacks.insert(pane_id, stack);
        r.surface_tabs.insert(pane_id, tab_widgets);
        r.pane_tab_containers.insert(pane_id, tabs);
        r.pane_workspace.insert(pane_id, workspace);
        r.activate_surface(pane_id, active);
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
fn build_surface_tab_widget(
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
    attach_tab_dnd_handlers(&tab, pane_id, surface.id, callbacks);
    (tab, label)
}

fn parse_tab_dnd_payload(payload: &str) -> Result<(PaneId, SurfaceId), &'static str> {
    let Some((src_pane_str, src_surface_str)) = payload.split_once('|') else {
        return Err("missing separator");
    };
    let src_pane = src_pane_str
        .parse::<PaneId>()
        .map_err(|_| "invalid pane id")?;
    let src_surface = src_surface_str
        .parse::<SurfaceId>()
        .map_err(|_| "invalid surface id")?;
    Ok((src_pane, src_surface))
}

#[cfg(test)]
mod tab_dnd_tests {
    use super::*;

    #[test]
    fn parse_tab_dnd_payload_round_trips() {
        let pane = PaneId::new();
        let surface = SurfaceId::new();
        let payload = format!("{pane}|{surface}");

        assert_eq!(parse_tab_dnd_payload(&payload), Ok((pane, surface)));
    }

    #[test]
    fn parse_tab_dnd_payload_rejects_plain_text() {
        assert!(parse_tab_dnd_payload("not a tab drag").is_err());
    }

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
        assert!(registry.surface_tab_labels.get(&surface).is_none());
        assert!(registry.surface_workspace.get(&surface).is_none());
        assert!(registry.active_surface(pane).is_none());
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
    surface_id: SurfaceId,
    callbacks: &PaneCallbacks,
) {
    let saw_tab_drop_target = callbacks.tab_drag_drop_seen.clone();
    let opened_new_window = Rc::new(Cell::new(false));

    let drag_source = gtk::DragSource::new();
    drag_source.set_actions(gtk::gdk::DragAction::MOVE);
    drag_source.connect_prepare(move |_, _, _| {
        tracing::debug!(%pane_id, %surface_id, "tab drag prepare");
        let payload = format!("{pane_id}|{surface_id}");
        let bytes = gtk::glib::Bytes::from_owned(payload.into_bytes());
        Some(gtk::gdk::ContentProvider::for_bytes(TAB_DND_MIME, &bytes))
    });
    let tab_for_begin = tab.clone();
    let saw_target_for_begin = saw_tab_drop_target.clone();
    let opened_for_begin = opened_new_window.clone();
    drag_source.connect_drag_begin(move |_, _| {
        saw_target_for_begin.set(false);
        opened_for_begin.set(false);
        tab_for_begin.set_opacity(0.4);
        tab_for_begin.add_css_class("flowmux-pane-tab-dragging");
    });
    let tab_for_end = tab.clone();
    let saw_target_for_end = saw_tab_drop_target.clone();
    let opened_for_end = opened_new_window.clone();
    let new_window_cb_for_end = callbacks.on_tab_drag_to_new_window.clone();
    drag_source.connect_drag_end(move |_, drag, delete_data| {
        tracing::debug!(
            %pane_id,
            %surface_id,
            delete_data,
            selected_action = ?drag.selected_action(),
            saw_tab_drop_target = saw_target_for_end.get(),
            "tab drag end"
        );
        if !saw_target_for_end.get() && !opened_for_end.get() {
            opened_for_end.set(true);
            tracing::info!(
                %pane_id,
                %surface_id,
                "tab drag ended outside tab targets; opening new window"
            );
            (new_window_cb_for_end.borrow_mut())(pane_id, surface_id);
        }
        saw_target_for_end.set(false);
        tab_for_end.set_opacity(1.0);
        tab_for_end.remove_css_class("flowmux-pane-tab-dragging");
    });
    let tab_for_cancel = tab.clone();
    let saw_target_for_cancel = saw_tab_drop_target.clone();
    let new_window_cb = callbacks.on_tab_drag_to_new_window.clone();
    let opened_for_cancel = opened_new_window.clone();
    drag_source.connect_drag_cancel(move |_, drag, reason| {
        tab_for_cancel.set_opacity(1.0);
        tab_for_cancel.remove_css_class("flowmux-pane-tab-dragging");
        tracing::debug!(
            %pane_id,
            %surface_id,
            ?reason,
            selected_action = ?drag.selected_action(),
            saw_tab_drop_target = saw_target_for_cancel.get(),
            "tab drag cancel"
        );
        if matches!(
            reason,
            gtk::gdk::DragCancelReason::NoTarget | gtk::gdk::DragCancelReason::Error
        ) && !saw_target_for_cancel.get()
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
        saw_target_for_cancel.set(false);
        false
    });
    tab.add_controller(drag_source);

    let drop_target = gtk::DropTargetAsync::new(
        Some(gtk::gdk::ContentFormats::new(&[TAB_DND_MIME])),
        gtk::gdk::DragAction::MOVE,
    );
    drop_target.connect_accept(|target, drop| {
        if drop.formats().contain_mime_type(TAB_DND_MIME) {
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
    let reorder_cb = callbacks.on_reorder_surface.clone();
    let position_of_surface_cb = callbacks.position_of_surface_in_pane.clone();
    let saw_target_for_drop = saw_tab_drop_target.clone();
    drop_target.connect_drop(move |_, drop, x, _y| {
        saw_target_for_drop.set(true);
        tracing::debug!(%target_pane, %target_surface, "tab drop fired");
        tab_for_drop.remove_css_class("flowmux-pane-tab-drop-before");
        tab_for_drop.remove_css_class("flowmux-pane-tab-drop-after");
        let drop = drop.clone();
        let tab_for_drop = tab_for_drop.clone();
        let reorder_cb = reorder_cb.clone();
        let position_of_surface_cb = position_of_surface_cb.clone();
        gtk::glib::MainContext::default().spawn_local(async move {
            let (stream, mime_type) = match drop
                .read_future(&[TAB_DND_MIME], gtk::glib::Priority::default())
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "tab drop: failed to read payload");
                    drop.finish(gtk::gdk::DragAction::empty());
                    return;
                }
            };
            if mime_type.as_str() != TAB_DND_MIME {
                tracing::warn!(mime = %mime_type, "tab drop: unexpected payload mime type");
                drop.finish(gtk::gdk::DragAction::empty());
                return;
            }
            let bytes = match stream
                .read_bytes_future(TAB_DND_PAYLOAD_MAX, gtk::glib::Priority::default())
                .await
            {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!(error = %e, "tab drop: failed to read payload bytes");
                    drop.finish(gtk::gdk::DragAction::empty());
                    return;
                }
            };
            let payload = match std::str::from_utf8(bytes.as_ref()) {
                Ok(payload) => payload,
                Err(e) => {
                    tracing::warn!(error = %e, "tab drop: payload was not UTF-8");
                    drop.finish(gtk::gdk::DragAction::empty());
                    return;
                }
            };
            let Ok((src_pane, src_surface)) = parse_tab_dnd_payload(payload) else {
                tracing::warn!(payload = %payload, "tab drop: payload invalid");
                drop.finish(gtk::gdk::DragAction::empty());
                return;
            };
            // Cross-pane moves are unsupported; reorder only over another tab in the same pane.
            if src_pane != target_pane {
                tracing::debug!(%src_pane, %target_pane, "tab drop: cross-pane drop ignored");
                drop.finish(gtk::gdk::DragAction::empty());
                return;
            }
            if src_surface == target_surface {
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
            (reorder_cb.borrow_mut())(target_pane, src_surface, final_index);
            drop.finish(gtk::gdk::DragAction::MOVE);
        });
        true
    });
    tab.add_controller(drop_target);
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
            let argv = shell.clone().map(|s| vec![s]).unwrap_or(argv);
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
            let pane = TerminalPane::spawn(
                pane_id,
                surface.id,
                argv,
                cwd.clone(),
                extra_env,
                callbacks.clone(),
            );
            theme.apply_to_vte(&pane.widget);
            // Start the new terminal widget with the current zoom option.
            pane.set_font_scale((callbacks.read_options)().zoom_factor());

            {
                let cb = callbacks.on_terminal_cwd_changed.clone();
                let surface_id = surface.id;
                pane.connect_current_dir_notify(move |pane| {
                    if let Some(cwd) = pane.current_dir() {
                        (cb.borrow_mut())(pane_id, surface_id, cwd);
                    }
                });
            }

            // Apply OSC 0/2 window titles emitted by vi/claude/codex/tmux and
            // similar programs to the tab label and window title. VTE may send
            // empty resets, so the dispatcher ignores trim-empty values.
            {
                let cb = callbacks.on_terminal_title_changed.clone();
                let surface_id = surface.id;
                pane.connect_title_notify(move |_pane, title| {
                    tracing::debug!(
                        %pane_id,
                        %surface_id,
                        title = %title,
                        "terminal title notify"
                    );
                    (cb.borrow_mut())(pane_id, surface_id, title);
                });
            }

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
            pane.add_controller(focus);

            // The bare VTE widget is the pane's root — never wrap it in
            // a one-child layout container before inserting it into the
            // surface stack. See the doc comment on TerminalPane.widget
            // for why (Paned split sizing regression).
            let widget = pane.root_widget();
            let mut r = registry.borrow_mut();
            r.terminals.insert(surface.id, pane);
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
            pane.web_view.set_zoom_level(opts.zoom_factor());

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
    }
}
