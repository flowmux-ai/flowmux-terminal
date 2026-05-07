// SPDX-License-Identifier: GPL-3.0-or-later
//! Workspace sidebar (cmux's vertical-tabs left panel).
//!
//! Each row shows: workspace name, current branch, linked PR badge
//! (if any), listening ports, and the latest unread notification body.
//! Hovering the row reveals a small X button on the right that closes
//! the workspace.

use flowmux_core::{PrState, Workspace, WorkspaceId};
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Clone)]
pub struct Sidebar {
    pub root: gtk::ScrolledWindow,
    pub list: gtk::ListBox,
    rows: Rc<RefCell<Vec<(WorkspaceId, gtk::ListBoxRow)>>>,
    on_close: Rc<dyn Fn(WorkspaceId)>,
}

impl Sidebar {
    pub fn new<S, C>(on_select: S, on_close: C) -> Self
    where
        S: Fn(WorkspaceId) + 'static,
        C: Fn(WorkspaceId) + 'static,
    {
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.add_css_class("navigation-sidebar");

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroll.set_vexpand(true);
        scroll.set_child(Some(&list));

        let rows: Rc<RefCell<Vec<(WorkspaceId, gtk::ListBoxRow)>>> =
            Rc::new(RefCell::new(Vec::new()));

        let rows_for_cb = rows.clone();
        list.connect_row_selected(move |_, selected| {
            if let Some(row) = selected {
                if let Some((id, _)) = rows_for_cb
                    .borrow()
                    .iter()
                    .find(|(_, r)| r == row)
                    .cloned()
                {
                    on_select(id);
                }
            }
        });

        let on_close: Rc<dyn Fn(WorkspaceId)> = Rc::new(on_close);
        Self { root: scroll, list, rows, on_close }
    }

    pub fn upsert(&self, ws: &Workspace) {
        let mut rows = self.rows.borrow_mut();
        if let Some((_, row)) = rows.iter().find(|(id, _)| *id == ws.id).cloned() {
            row.set_child(Some(&row_widget(ws, self.on_close.clone())));
            return;
        }
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&row_widget(ws, self.on_close.clone())));
        self.list.append(&row);
        rows.push((ws.id, row));
    }

    pub fn remove(&self, id: WorkspaceId) {
        let mut rows = self.rows.borrow_mut();
        if let Some(idx) = rows.iter().position(|(wid, _)| *wid == id) {
            self.list.remove(&rows[idx].1);
            rows.swap_remove(idx);
        }
    }
}

fn row_widget(ws: &Workspace, on_close: Rc<dyn Fn(WorkspaceId)>) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.set_margin_top(6);
    row.set_margin_bottom(6);
    row.set_margin_start(4);
    row.set_margin_end(6);

    // Left-edge color bar — distinct hue per workspace so multiple
    // tabs stay visually separable at a glance.
    if let Some(color) = ws.color.as_deref() {
        row.append(&color_bar(color));
    }

    let meta = build_meta_column(ws);
    meta.set_hexpand(true);
    meta.set_margin_start(6);
    row.append(&meta);

    let close_btn = gtk::Button::from_icon_name("window-close-symbolic");
    close_btn.add_css_class("flat");
    close_btn.add_css_class("circular");
    close_btn.set_tooltip_text(Some("Close tab"));
    close_btn.set_valign(gtk::Align::Center);
    // Hidden by default — opacity 0 keeps the row layout stable when
    // the button shows/hides on hover. `can-target = false` blocks
    // accidental clicks while invisible.
    close_btn.set_opacity(0.0);
    close_btn.set_can_target(false);
    let id = ws.id;
    let on_close_for_click = on_close.clone();
    close_btn.connect_clicked(move |_| on_close_for_click(id));
    row.append(&close_btn);

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

    row.upcast()
}

/// 4-pixel rounded vertical color strip drawn with Cairo so the
/// color is fully data-driven (no per-row CSS provider).
fn color_bar(color: &str) -> gtk::Widget {
    let bar = gtk::DrawingArea::new();
    bar.set_size_request(4, -1);
    bar.set_vexpand(true);
    bar.set_valign(gtk::Align::Fill);
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
        // Rounded corners at top/bottom.
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
        cr.arc(r, h - r, r, 0.5 * std::f64::consts::PI, std::f64::consts::PI);
        cr.close_path();
        let _ = cr.fill();
    });
    bar.upcast()
}

fn build_meta_column(ws: &Workspace) -> gtk::Box {
    let v = gtk::Box::new(gtk::Orientation::Vertical, 2);

    let title = gtk::Label::new(Some(&ws.name));
    title.set_halign(gtk::Align::Start);
    title.add_css_class("heading");
    v.append(&title);

    if let Some(git) = &ws.git {
        let h = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let branch = gtk::Label::new(Some(&format!("⎇ {}", git.branch)));
        branch.set_halign(gtk::Align::Start);
        branch.add_css_class("dim-label");
        branch.add_css_class("caption");
        h.append(&branch);
        if let Some(pr) = &git.linked_pr {
            let badge = gtk::Label::new(Some(&format!("#{}", pr.number)));
            badge.add_css_class("caption");
            badge.add_css_class(match pr.state {
                PrState::Open => "success",
                PrState::Merged => "accent",
                PrState::Closed => "warning",
                PrState::Draft => "dim-label",
            });
            h.append(&badge);
        }
        v.append(&h);
    }

    if !ws.listening_ports.is_empty() {
        let ports = ws
            .listening_ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let p = gtk::Label::new(Some(&format!(":: {ports}")));
        p.set_halign(gtk::Align::Start);
        p.add_css_class("caption");
        p.add_css_class("dim-label");
        v.append(&p);
    }

    v
}
