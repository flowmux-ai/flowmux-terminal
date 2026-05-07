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
//! The toolbar's `+` adds a workspace (Ctrl+Shift+T equivalent) and
//! the bell shows an in-process notification transcript. The list
//! rows expose hover-X close, color bar, right-click menu (rename /
//! recolor / close).

use crate::bridge::{Bridge, GtkCommand};
use crate::notifications::{NotificationEntry, NotificationLog};
use flowmux_core::{NotificationLevel, PrState, Workspace, WorkspaceId};
use gtk::glib::variant::ToVariant;
use gtk::prelude::*;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

#[derive(Clone)]
pub struct Sidebar {
    pub root: gtk::Box,
    pub list: gtk::ListBox,
    rows: Rc<RefCell<Vec<(WorkspaceId, gtk::ListBoxRow)>>>,
    on_close: Rc<dyn Fn(WorkspaceId)>,
    bell_button: gtk::MenuButton,
    bell_popover: gtk::Popover,
    notification_log: NotificationLog,
    attentions: Rc<RefCell<HashSet<WorkspaceId>>>,
    bridge: Bridge,
}

impl Sidebar {
    pub fn new<S, C>(
        on_select: S,
        on_close: C,
        bridge: Bridge,
        notification_log: NotificationLog,
    ) -> Self
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

        let attentions: Rc<RefCell<HashSet<WorkspaceId>>> = Rc::new(RefCell::new(HashSet::new()));
        let rows_for_cb = rows.clone();
        let attentions_for_cb = attentions.clone();
        list.connect_row_selected(move |_, selected| {
            if let Some(row) = selected {
                if let Some((id, list_row)) =
                    rows_for_cb.borrow().iter().find(|(_, r)| r == row).cloned()
                {
                    // Selecting a row clears any attention indicator
                    // it had been carrying.
                    if attentions_for_cb.borrow_mut().remove(&id) {
                        list_row.remove_css_class("flowmux-attention");
                    }
                    on_select(id);
                }
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
        new_btn.set_tooltip_text(Some("New tab (Ctrl+Shift+T)"));
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

        let log_for_show = notification_log.clone();
        let popover_for_show = bell_popover.clone();
        let bell_for_show = bell_button.clone();
        bell_popover.connect_show(move |_| {
            // Render fresh contents on every open and mark all entries
            // seen so subsequent opens dim the existing ones.
            popover_for_show.set_child(Some(&render_notification_list(&log_for_show)));
            for entry in log_for_show.borrow_mut().iter_mut() {
                entry.seen = true;
            }
            bell_for_show.remove_css_class("accent");
        });
        toolbar.append(&bell_button);

        // ---- Outer vbox: toolbar + list ----
        let root_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root_box.append(&toolbar);
        root_box.append(&scroll);

        Self {
            root: root_box,
            list,
            rows,
            on_close,
            bell_button,
            bell_popover,
            notification_log,
            attentions,
            bridge,
        }
    }

    pub fn upsert(&self, ws: &Workspace) {
        let mut rows = self.rows.borrow_mut();
        if let Some((_, row)) = rows.iter().find(|(id, _)| *id == ws.id).cloned() {
            row.set_child(Some(&row_widget(
                ws,
                self.on_close.clone(),
                self.bridge.clone(),
            )));
            return;
        }
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&row_widget(
            ws,
            self.on_close.clone(),
            self.bridge.clone(),
        )));
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

    /// Tint a workspace row to flag that an agent finished there.
    /// Cleared automatically when the user selects the row.
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

    /// Indicate a fresh notification by tinting the bell button.
    /// Cleared next time the popover opens (which marks all seen).
    pub fn bump_notification_badge(&self) {
        if !self.bell_button.has_css_class("accent") {
            self.bell_button.add_css_class("accent");
        }
        // Refresh the popover content if it happens to be visible so
        // the new entry shows immediately.
        if self.bell_popover.is_visible() {
            self.bell_popover
                .set_child(Some(&render_notification_list(&self.notification_log)));
        }
    }
}

fn render_notification_list(log: &NotificationLog) -> gtk::Widget {
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
    scroll.set_min_content_height(160);
    scroll.set_max_content_height(420);
    scroll.set_propagate_natural_height(true);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");

    let entries = log.borrow();
    if entries.is_empty() {
        let empty = gtk::Label::new(Some("No notifications yet."));
        empty.set_margin_top(20);
        empty.set_margin_bottom(20);
        empty.add_css_class("dim-label");
        scroll.set_child(Some(&empty));
        return scroll.upcast();
    }

    // Newest at top.
    for entry in entries.iter().rev() {
        list.append(&notification_row(entry));
    }
    scroll.set_child(Some(&list));
    scroll.upcast()
}

fn notification_row(entry: &NotificationEntry) -> gtk::Widget {
    let v = gtk::Box::new(gtk::Orientation::Vertical, 2);
    v.set_margin_top(8);
    v.set_margin_bottom(8);
    v.set_margin_start(10);
    v.set_margin_end(10);

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

    if entry.seen {
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
    v.upcast()
}

fn format_time(ts: &chrono::DateTime<chrono::Utc>) -> String {
    let local: chrono::DateTime<chrono::Local> = (*ts).into();
    local.format("%H:%M:%S").to_string()
}

fn row_widget(ws: &Workspace, on_close: Rc<dyn Fn(WorkspaceId)>, bridge: Bridge) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.set_margin_top(6);
    row.set_margin_bottom(6);
    row.set_margin_start(4);
    row.set_margin_end(6);

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

        popover.set_child(Some(&v));
        popover.set_parent(&row_for_click);
        popover.set_has_arrow(false);
        crate::ui::popover_pos::anchor_at_click(&popover, &row_for_click, x, y);
        popover.connect_closed(|p| p.unparent());
        popover.popup();
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    row.add_controller(click);

    row.upcast()
}

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

fn build_meta_column(ws: &Workspace) -> gtk::Box {
    // Two-line layout:
    //   line 1: workspace name (bold heading)
    //   line 2: last folder name [+ " / branch" if a git repo]  (dim caption)
    // Optional 3rd line: linked PR badge / listening ports if present.
    let v = gtk::Box::new(gtk::Orientation::Vertical, 1);

    let title = gtk::Label::new(Some(&ws.name));
    title.set_halign(gtk::Align::Start);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_xalign(0.0);
    title.add_css_class("heading");
    v.append(&title);

    let subtitle_text = subtitle_for(ws);
    let subtitle = gtk::Label::new(Some(&subtitle_text));
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_xalign(0.0);
    subtitle.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    subtitle.add_css_class("caption");
    subtitle.add_css_class("dim-label");
    v.append(&subtitle);

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

/// Build the second line: "<last-folder>" or "<last-folder> / <branch>".
fn subtitle_for(ws: &Workspace) -> String {
    let last_folder = ws
        .root_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_else(|| ws.root_dir.to_str().unwrap_or(""));
    match ws.git.as_ref() {
        Some(g) => format!("{last_folder} / {}", g.branch),
        None => last_folder.to_string(),
    }
}
