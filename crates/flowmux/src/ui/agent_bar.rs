// SPDX-License-Identifier: GPL-3.0-or-later
//! Bottom bar for live AI agents across all workspaces.

use crate::bridge::{Bridge, GtkCommand};
use flowmux_core::{
    AgentBarItem, AgentBarModel, AgentStatus, SurfaceId, AGENT_BAR_ITEM_MAX_WIDTH_PX,
};
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::str::FromStr;

#[derive(Clone)]
pub(crate) struct AgentBar {
    pub(crate) root: gtk::Box,
    items: gtk::Box,
    item_buttons: Rc<RefCell<HashMap<SurfaceId, gtk::Button>>>,
    item_order: Rc<RefCell<Vec<SurfaceId>>>,
    bridge: Bridge,
}

impl AgentBar {
    pub(crate) fn new(bridge: Bridge) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        root.add_css_class("flowmux-agent-bar");
        root.set_visible(false);

        let label = gtk::Label::new(Some("Agents"));
        label.add_css_class("caption-heading");
        label.set_halign(gtk::Align::Start);
        root.append(&label);

        let items = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        items.set_halign(gtk::Align::Start);

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hexpand(true);
        scroll.set_hscrollbar_policy(gtk::PolicyType::Automatic);
        scroll.set_vscrollbar_policy(gtk::PolicyType::Never);
        // Horizontal scrolling keeps item min widths intact; equal shrinking
        // would make multiple agent names/statuses unreadable before ellipsize.
        scroll.set_child(Some(&items));
        root.append(&scroll);

        Self {
            root,
            items,
            item_buttons: Rc::new(RefCell::new(HashMap::new())),
            item_order: Rc::new(RefCell::new(Vec::new())),
            bridge,
        }
    }

    pub(crate) fn render(
        &self,
        model: &AgentBarModel,
        attention_surfaces: &HashSet<SurfaceId>,
        focused_surface: Option<SurfaceId>,
    ) {
        while let Some(child) = self.items.first_child() {
            self.items.remove(&child);
        }
        self.item_buttons.borrow_mut().clear();

        if !model.visible {
            self.root.set_visible(false);
            return;
        }

        self.root.set_visible(true);
        let ordered_items = self.ordered_items(model);
        for item in ordered_items {
            let button = self.item_button(item);
            if attention_surfaces.contains(&item.surface) {
                button.add_css_class("flowmux-attention");
            }
            if focused_surface == Some(item.surface) {
                button.add_css_class("focused");
            }
            self.item_buttons
                .borrow_mut()
                .insert(item.surface, button.clone());
            self.items.append(&button);
        }
    }

    pub(crate) fn mark_attention(&self, surface: SurfaceId) {
        if let Some(button) = self.item_buttons.borrow().get(&surface) {
            button.add_css_class("flowmux-attention");
        }
    }

    pub(crate) fn clear_attention(&self, surface: SurfaceId) {
        if let Some(button) = self.item_buttons.borrow().get(&surface) {
            button.remove_css_class("flowmux-attention");
        }
    }

    fn item_button(&self, item: &AgentBarItem) -> gtk::Button {
        let button = gtk::Button::new();
        button.add_css_class("flat");
        button.add_css_class("flowmux-agent-bar-item");
        button.set_size_request(AGENT_BAR_ITEM_MAX_WIDTH_PX as i32, -1);
        button.set_hexpand(false);
        button.set_tooltip_text(Some(&format!("{}: {}", item.agent_name, item.status_text)));

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        row.set_valign(gtk::Align::Center);

        let stripe = color_stripe(&item.color);
        row.append(&stripe);

        let icon = gtk::Image::from_icon_name(agent_status_icon_name(item.status, item.seen));
        icon.set_pixel_size(12);
        icon.add_css_class(agent_status_css_class(item.status, item.seen));
        row.append(&icon);

        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_hexpand(true);

        let name = gtk::Label::new(Some(item.agent_name.as_str()));
        name.set_halign(gtk::Align::Start);
        name.set_xalign(0.0);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_single_line_mode(true);
        name.set_lines(1);
        name.set_max_width_chars((AGENT_BAR_ITEM_MAX_WIDTH_PX / 10) as i32);
        name.add_css_class("caption-heading");
        text.append(&name);

        let status = gtk::Label::new(Some(item.status_text.as_str()));
        status.set_halign(gtk::Align::Start);
        status.set_xalign(0.0);
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);
        status.set_single_line_mode(true);
        status.set_lines(1);
        status.set_max_width_chars((AGENT_BAR_ITEM_MAX_WIDTH_PX / 8) as i32);
        status.add_css_class("caption");
        status.add_css_class(agent_status_css_class(item.status, item.seen));
        text.append(&status);

        row.append(&text);
        button.set_child(Some(&row));

        let activated = Rc::new(Cell::new(false));
        let bridge = self.bridge.clone();
        let workspace = item.workspace;
        let pane = item.pane;
        let surface = item.surface;
        let activated_for_button = activated.clone();
        button.connect_clicked(move |_| {
            open_agent_bar_item(
                bridge.clone(),
                activated_for_button.clone(),
                workspace,
                pane,
                surface,
            );
        });

        let click = gtk::GestureClick::new();
        click.set_button(gtk::gdk::BUTTON_PRIMARY);
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let bridge = self.bridge.clone();
        click.connect_released(move |gesture, _n_press, _x, _y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            open_agent_bar_item(bridge.clone(), activated.clone(), workspace, pane, surface);
        });
        button.add_controller(click);

        self.attach_dnd_handlers(&button, item.surface);

        button
    }

    fn ordered_items<'a>(&self, model: &'a AgentBarModel) -> Vec<&'a AgentBarItem> {
        let live_surfaces: Vec<SurfaceId> = model.items.iter().map(|item| item.surface).collect();
        {
            let mut order = self.item_order.borrow_mut();
            order.retain(|surface| live_surfaces.contains(surface));
            for surface in &live_surfaces {
                if !order.contains(surface) {
                    order.push(*surface);
                }
            }
        }

        let order = self.item_order.borrow();
        order
            .iter()
            .filter_map(|surface| model.items.iter().find(|item| item.surface == *surface))
            .collect()
    }

    fn attach_dnd_handlers(&self, button: &gtk::Button, surface: SurfaceId) {
        let drag_source = gtk::DragSource::new();
        drag_source.set_actions(gtk::gdk::DragAction::MOVE);
        drag_source.connect_prepare(move |_, _, _| {
            Some(gtk::gdk::ContentProvider::for_value(
                &surface.to_string().to_value(),
            ))
        });
        let button_for_begin = button.clone();
        drag_source.connect_drag_begin(move |_, _| {
            button_for_begin.set_opacity(0.4);
            button_for_begin.add_css_class("flowmux-dragging");
        });
        let button_for_end = button.clone();
        drag_source.connect_drag_end(move |_, _, _| {
            button_for_end.set_opacity(1.0);
            button_for_end.remove_css_class("flowmux-dragging");
        });
        let button_for_cancel = button.clone();
        drag_source.connect_drag_cancel(move |_, _, _| {
            button_for_cancel.set_opacity(1.0);
            button_for_cancel.remove_css_class("flowmux-dragging");
            false
        });
        button.add_controller(drag_source);

        let drop_target =
            gtk::DropTarget::new(gtk::glib::types::Type::STRING, gtk::gdk::DragAction::MOVE);
        let button_for_motion = button.clone();
        drop_target.connect_motion(move |_, x, _y| {
            let width = button_for_motion.width();
            let before = if width > 0 {
                x < (width as f64) / 2.0
            } else {
                true
            };
            if before {
                button_for_motion.remove_css_class("flowmux-drop-after");
                button_for_motion.add_css_class("flowmux-drop-before");
            } else {
                button_for_motion.remove_css_class("flowmux-drop-before");
                button_for_motion.add_css_class("flowmux-drop-after");
            }
            gtk::gdk::DragAction::MOVE
        });
        let button_for_leave = button.clone();
        drop_target.connect_leave(move |_| {
            button_for_leave.remove_css_class("flowmux-drop-before");
            button_for_leave.remove_css_class("flowmux-drop-after");
        });
        let order_for_drop = self.item_order.clone();
        let items_for_drop = self.items.clone();
        let item_buttons_for_drop = self.item_buttons.clone();
        let button_for_drop = button.clone();
        drop_target.connect_drop(move |_, value, x, _y| {
            button_for_drop.remove_css_class("flowmux-drop-before");
            button_for_drop.remove_css_class("flowmux-drop-after");
            let Ok(payload) = value.get::<String>() else {
                return false;
            };
            let Ok(source) = SurfaceId::from_str(&payload) else {
                return false;
            };
            if source == surface {
                return false;
            }
            let mut order = order_for_drop.borrow_mut();
            let target_width = button_for_drop.width();
            let before = if target_width > 0 {
                x < (target_width as f64) / 2.0
            } else {
                true
            };
            let Some(next_order) = reordered_agent_bar_order(&order, source, surface, before)
            else {
                return false;
            };
            *order = next_order;
            reorder_item_widgets(&items_for_drop, &item_buttons_for_drop.borrow(), &order);
            true
        });
        button.add_controller(drop_target);
    }
}

fn reordered_agent_bar_order(
    order: &[SurfaceId],
    source: SurfaceId,
    target: SurfaceId,
    before: bool,
) -> Option<Vec<SurfaceId>> {
    if source == target {
        return None;
    }
    let source_idx = order.iter().position(|candidate| *candidate == source)?;
    let target_idx = order.iter().position(|candidate| *candidate == target)?;
    let new_index = match (before, source_idx < target_idx) {
        (true, true) => target_idx.saturating_sub(1),
        (true, false) => target_idx,
        (false, true) => target_idx,
        (false, false) => target_idx + 1,
    };
    if new_index == source_idx {
        return None;
    }
    let mut next = order.to_vec();
    let moved = next.remove(source_idx);
    next.insert(new_index.min(next.len()), moved);
    Some(next)
}

fn reorder_item_widgets(
    items: &gtk::Box,
    buttons: &HashMap<SurfaceId, gtk::Button>,
    order: &[SurfaceId],
) {
    let mut previous: Option<gtk::Widget> = None;
    for surface in order {
        if let Some(button) = buttons.get(surface) {
            items.reorder_child_after(button, previous.as_ref());
            previous = Some(button.clone().upcast());
        }
    }
}

fn open_agent_bar_item(
    bridge: Bridge,
    activated: Rc<Cell<bool>>,
    workspace: flowmux_core::WorkspaceId,
    pane: flowmux_core::PaneId,
    surface: SurfaceId,
) {
    if activated.replace(true) {
        return;
    }
    let reset = activated.clone();
    gtk::glib::idle_add_local_once(move || {
        reset.set(false);
    });
    gtk::glib::MainContext::default().spawn_local(async move {
        let _ = bridge
            .tx
            .send(GtkCommand::OpenAgentBarItem {
                workspace,
                pane,
                surface,
            })
            .await;
    });
}

fn color_stripe(color: &str) -> gtk::DrawingArea {
    let rgba =
        gtk::gdk::RGBA::parse(color).unwrap_or_else(|_| gtk::gdk::RGBA::new(0.45, 0.55, 0.65, 1.0));
    let stripe = gtk::DrawingArea::new();
    stripe.set_content_width(4);
    stripe.set_content_height(29);
    stripe.add_css_class("flowmux-agent-bar-color");
    stripe.set_draw_func(move |_, cr, width, height| {
        cr.set_source_rgba(
            rgba.red() as f64,
            rgba.green() as f64,
            rgba.blue() as f64,
            rgba.alpha() as f64,
        );
        cr.rectangle(0.0, 0.0, width as f64, height as f64);
        let _ = cr.fill();
    });
    stripe
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
        AgentStatus::Blocked => "flowmux-sidebar-agent-blocked",
        AgentStatus::Working => "flowmux-sidebar-agent-working",
        AgentStatus::Done if !seen => "flowmux-sidebar-agent-done",
        AgentStatus::Done | AgentStatus::Idle => "flowmux-sidebar-agent-idle",
        AgentStatus::Unknown => "flowmux-sidebar-agent-unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reordered_agent_bar_order_moves_items_before_or_after_target() {
        let a = SurfaceId::new();
        let b = SurfaceId::new();
        let c = SurfaceId::new();
        let order = vec![a, b, c];

        assert_eq!(
            reordered_agent_bar_order(&order, a, c, true),
            Some(vec![b, a, c])
        );
        assert_eq!(
            reordered_agent_bar_order(&order, a, c, false),
            Some(vec![b, c, a])
        );
        assert_eq!(
            reordered_agent_bar_order(&order, c, a, true),
            Some(vec![c, a, b])
        );
        assert_eq!(
            reordered_agent_bar_order(&order, c, a, false),
            Some(vec![a, c, b])
        );
        assert_eq!(reordered_agent_bar_order(&order, b, b, true), None);
    }
}
