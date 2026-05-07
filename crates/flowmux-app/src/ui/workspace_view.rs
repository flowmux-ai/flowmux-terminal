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
    Pane, PaneContent, PaneId, PaneSurface, SplitDirection, Surface, SurfaceId, SurfaceKind,
    WorkspaceId,
};
use gtk::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Default)]
pub struct PaneRegistry {
    pub terminals: HashMap<SurfaceId, TerminalPane>,
    pub browsers: HashMap<SurfaceId, BrowserPane>,
    active_terminal_by_pane: HashMap<PaneId, SurfaceId>,
    active_browser_by_pane: HashMap<PaneId, SurfaceId>,
    pane_frames: HashMap<PaneId, gtk::Widget>,
    surface_stacks: HashMap<PaneId, gtk::Stack>,
    surface_buttons: HashMap<PaneId, Vec<(SurfaceId, gtk::Button)>>,
    pane_workspace: HashMap<PaneId, WorkspaceId>,
    surface_workspace: HashMap<SurfaceId, WorkspaceId>,
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

    pub fn pane_ids(&self) -> impl Iterator<Item = PaneId> + '_ {
        self.pane_frames.keys().copied()
    }

    pub fn active_surface(&self, pane: PaneId) -> Option<SurfaceId> {
        self.active_terminal_by_pane
            .get(&pane)
            .or_else(|| self.active_browser_by_pane.get(&pane))
            .copied()
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
            self.surface_buttons.remove(&pane);
            self.pane_workspace.remove(&pane);
        }

        let surfaces: Vec<SurfaceId> = self
            .surface_workspace
            .iter()
            .filter_map(|(surface, owner)| (*owner == workspace).then_some(*surface))
            .collect();
        for surface in surfaces {
            self.terminals.remove(&surface);
            self.browsers.remove(&surface);
            self.surface_workspace.remove(&surface);
        }
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
        if let Some(buttons) = self.surface_buttons.get(&pane) {
            for (id, button) in buttons {
                if *id == surface {
                    button.add_css_class("active");
                } else {
                    button.remove_css_class("active");
                }
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
            direction,
            ratio,
            first,
            second,
            ..
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
            let right = build_pane(workspace, second, argv, cwd, callbacks, registry, theme);
            paned.set_start_child(Some(&left));
            paned.set_end_child(Some(&right));
            paned.set_resize_start_child(true);
            paned.set_resize_end_child(true);
            paned.set_shrink_start_child(false);
            paned.set_shrink_end_child(false);
            let r = *ratio;
            paned.connect_realize(move |p| {
                let total = match p.orientation() {
                    gtk::Orientation::Horizontal => p.width(),
                    _ => p.height(),
                };
                if total > 0 {
                    p.set_position((total as f32 * r) as i32);
                }
            });
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

    let tabbar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    tabbar.add_css_class("flowmux-pane-tabbar");
    tabbar.set_margin_top(4);
    tabbar.set_margin_start(6);
    tabbar.set_margin_end(6);

    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);

    let mut tab_buttons = Vec::new();
    for surface in &surfaces {
        let tab = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        tab.add_css_class("flowmux-pane-tab-wrap");
        let button = surface_tab_button(surface, surface.id == active);
        {
            let cb = callbacks.on_activate_surface.clone();
            let pane_id = pane_id;
            let surface_id = surface.id;
            button.connect_clicked(move |_| (cb.borrow_mut())(pane_id, surface_id));
        }
        tab.append(&button);
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("flowmux-pane-tab-close");
        close.set_tooltip_text(Some("Close tab"));
        {
            let cb = callbacks.on_close_surface.clone();
            let pane_id = pane_id;
            let surface_id = surface.id;
            close.connect_clicked(move |_| (cb.borrow_mut())(pane_id, surface_id));
        }
        tab.append(&close);
        tabbar.append(&tab);
        tab_buttons.push((surface.id, button));

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

    let add = gtk::Button::from_icon_name("tab-new-symbolic");
    add.add_css_class("flat");
    add.set_tooltip_text(Some("New terminal tab"));
    {
        let cb = callbacks.on_new_surface.clone();
        let pane_id = pane_id;
        add.connect_clicked(move |_| (cb.borrow_mut())(pane_id));
    }
    tabbar.append(&add);

    stack.set_visible_child_name(&active.to_string());
    root.append(&tabbar);
    root.append(&stack);
    frame.set_child(Some(&root));

    {
        let frame_widget = frame.clone().upcast::<gtk::Widget>();
        let mut r = registry.borrow_mut();
        r.pane_frames.insert(pane_id, frame_widget);
        r.surface_stacks.insert(pane_id, stack);
        r.surface_buttons.insert(pane_id, tab_buttons);
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
        PaneContent::Terminal { .. } => {
            vec![PaneSurface::terminal("Terminal", fallback_cwd)]
        }
        PaneContent::Browser { url } => vec![PaneSurface::browser("Browser", url.clone())],
        PaneContent::Tabs { .. } => vec![PaneSurface::terminal("Terminal", fallback_cwd)],
    }
}

fn surface_tab_button(surface: &PaneSurface, active: bool) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("flowmux-pane-tab");
    if active {
        button.add_css_class("active");
    }
    button.set_tooltip_text(Some(&surface.title));

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let icon_name = match surface.kind {
        SurfaceKind::Terminal { .. } => "utilities-terminal-symbolic",
        SurfaceKind::Browser { .. } => "web-browser-symbolic",
    };
    row.append(&gtk::Image::from_icon_name(icon_name));
    let label = gtk::Label::new(Some(&surface.title));
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_max_width_chars(18);
    row.append(&label);
    button.set_child(Some(&row));
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
            let pane = TerminalPane::spawn(pane_id, argv, cwd.clone(), callbacks.clone());
            theme.apply_to_vte(&pane.widget);

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
            pane.widget.add_controller(focus);

            let widget = pane.root.clone();
            let mut r = registry.borrow_mut();
            r.terminals.insert(surface.id, pane);
            r.surface_workspace.insert(surface.id, workspace);
            widget
        }
        SurfaceKind::Browser { initial_url } => {
            let pane = BrowserPane::new(pane_id, initial_url.as_deref());
            let widget = pane.root.clone().upcast::<gtk::Widget>();
            let mut r = registry.borrow_mut();
            r.browsers.insert(surface.id, pane);
            r.surface_workspace.insert(surface.id, workspace);
            widget
        }
    }
}
