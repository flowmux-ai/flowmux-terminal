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
//! The toolbar's `+` adds a workspace (Ctrl+Shift+N equivalent) and
//! the bell shows an in-process notification transcript. The list
//! rows expose hover-X close, color bar, right-click menu (rename /
//! recolor / close).

use crate::bridge::{Bridge, GtkCommand};
use crate::notifications::{NotificationEntry, NotificationLog};
use flowmux_core::{NotificationLevel, PrState, Workspace, WorkspaceId};
use gtk::prelude::*;
use std::cell::RefCell;
use std::collections::HashSet;
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
        new_btn.set_tooltip_text(Some("New workspace (Ctrl+Shift+N)"));
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

        // ---- Bottom footer: 좌측 작은 옵션 버튼 ----
        // 클릭하면 bridge로 ShowOptionsDialog 보내 윈도우 dispatch에서
        // 모달 다이얼로그를 띄운다.
        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        footer.set_margin_top(2);
        footer.set_margin_bottom(4);
        footer.set_margin_start(4);
        footer.set_margin_end(4);
        let options_btn = gtk::Button::from_icon_name("emblem-system-symbolic");
        options_btn.add_css_class("flat");
        options_btn.set_tooltip_text(Some("옵션"));
        options_btn.set_focus_on_click(false);
        // 작은 크기 — 사이드바의 다른 버튼과 동일한 high-contrast가
        // 아니라 dimmed 톤으로 두어 시각적으로 잠잠하게.
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
        attach_dnd_handlers(&row, ws.id, self.bridge.clone(), self.rows.clone());
        self.list.append(&row);
        rows.push((ws.id, row));
    }

    /// 드래그 앤 드랍 결과를 사이드 패널에 반영해 워크스페이스 행의 시각적
    /// 위치를 새 인덱스로 옮긴다. `id`가 없으면 no-op이며,
    /// `target_index`가 길이를 넘으면 마지막 슬롯으로 클램프된다.
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
        // ListBox에서 위젯을 떼고 같은 위젯을 새 위치에 끼워 넣는다.
        // `gtk::ListBox::insert(_, position)`는 position이 -1이거나
        // 길이를 넘으면 끝에 추가한다.
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

/// 드래그 앤 드랍 데이터에 사용하는 mime-type. WorkspaceId의 UUID
/// 문자열을 바이트로 담는다. 같은 mime을 받는 DropTarget만 매칭되어
/// 외부 앱과의 충돌이 없다.
const DND_MIME: &str = "application/x-flowmux-workspace-id";

/// 사이드 패널의 한 워크스페이스 행에 드래그 앤 드랍 컨트롤러를 연결한다.
///
/// - `DragSource`: 행을 잡으면 워크스페이스 ID(UUID 문자열)를 ContentProvider에
///   담아 드래그를 시작한다. 드래그 동안 원본 행은 살짝 흐려진다.
/// - `DropTarget`: 다른 행 위에 드랍되면 드랍 위치 y로 행의 위/아래를
///   결정해 [`GtkCommand::ReorderWorkspace`]를 보낸다.
fn attach_dnd_handlers(row: &gtk::ListBoxRow, id: WorkspaceId, bridge: Bridge, rows: RowsCell) {
    let drag_source = gtk::DragSource::new();
    drag_source.set_actions(gtk::gdk::DragAction::MOVE);
    let id_for_prepare = id;
    drag_source.connect_prepare(move |_, _, _| {
        tracing::debug!(workspace = %id_for_prepare, "sidebar drag prepare");
        let payload = id_for_prepare.to_string();
        let bytes = gtk::glib::Bytes::from_owned(payload.into_bytes());
        Some(gtk::gdk::ContentProvider::for_bytes(DND_MIME, &bytes))
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
        gtk::DropTarget::new(gtk::glib::Bytes::static_type(), gtk::gdk::DragAction::MOVE);
    let row_for_motion = row.clone();
    drop_target.connect_motion(move |_, _, _| {
        row_for_motion.add_css_class("flowmux-drop-hover");
        gtk::gdk::DragAction::MOVE
    });
    let row_for_leave = row.clone();
    drop_target.connect_leave(move |_| {
        row_for_leave.remove_css_class("flowmux-drop-hover");
    });
    let row_for_drop = row.clone();
    let target_id = id;
    drop_target.connect_drop(move |_, value, _x, y| {
        row_for_drop.remove_css_class("flowmux-drop-hover");
        let Ok(bytes) = value.get::<gtk::glib::Bytes>() else {
            tracing::warn!("sidebar drop: payload was not Bytes — DropTarget type mismatch");
            return false;
        };
        let Ok(payload) = std::str::from_utf8(&bytes) else {
            tracing::warn!("sidebar drop: payload not UTF-8");
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

        // 드랍 y가 행의 절반보다 위면 target 앞, 아래면 뒤에 둔다.
        let row_height = row_for_drop.height();
        let above = if row_height > 0 {
            y < (row_height as f64) / 2.0
        } else {
            true
        };

        // 최종 인덱스 계산. reorder_workspace는 "remove 후 insert(target)"
        // 의미이므로, source가 target보다 앞에 있을 때 target index가 1 줄어든다.
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
