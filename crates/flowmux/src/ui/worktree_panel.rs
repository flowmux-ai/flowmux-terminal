// SPDX-License-Identifier: GPL-3.0-or-later

//! Right-side panel for inspecting and acting on Git worktrees.

use crate::bridge::FocusDir;
use adw::prelude::*;
use flowmux_vcs::worktree::WorktreeInfo;
use gtk::{gdk, glib};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

#[derive(Clone, Debug)]
pub struct WorktreeRowView {
    pub info: WorktreeInfo,
    pub remove_block_reason: Option<String>,
    pub operation_in_progress: bool,
}

impl WorktreeRowView {
    #[cfg(all(test, not(target_os = "macos")))]
    pub fn available(info: WorktreeInfo) -> Self {
        Self {
            info,
            remove_block_reason: None,
            operation_in_progress: false,
        }
    }
}

type PathCallback = Rc<RefCell<Option<Box<dyn Fn(PathBuf)>>>>;

#[derive(Clone)]
pub struct WorktreePanel {
    root: gtk::Box,
    repository_label: gtk::Label,
    content: gtk::Stack,
    list: gtk::ListBox,
    scroll: gtk::ScrolledWindow,
    status: adw::StatusPage,
    spinner: gtk::Spinner,
    close_button: gtk::Button,
    refresh_button: gtk::Button,
    rows: Rc<RefCell<Vec<WorktreeRowView>>>,
    open: Rc<Cell<bool>>,
    on_info: PathCallback,
    on_remove: PathCallback,
    on_refresh: Rc<RefCell<Option<Box<dyn Fn()>>>>,
    on_close: Rc<RefCell<Option<Box<dyn Fn()>>>>,
    on_focus_out: Rc<RefCell<Option<Box<dyn Fn(FocusDir)>>>>,
    on_focus_changed: Rc<RefCell<Option<Box<dyn Fn(bool)>>>>,
}

impl WorktreePanel {
    pub fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("flowmux-worktree-panel");
        root.set_size_request(300, -1);
        root.set_hexpand(false);
        root.set_vexpand(true);
        root.set_focusable(true);
        root.set_visible(false);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.add_css_class("flowmux-worktree-panel-header");

        let title_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        title_box.set_hexpand(true);
        let title = gtk::Label::new(Some("Worktrees"));
        title.add_css_class("heading");
        title.set_xalign(0.0);
        let repository_label = gtk::Label::new(None);
        repository_label.add_css_class("dim-label");
        repository_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        repository_label.set_xalign(0.0);
        title_box.append(&title);
        title_box.append(&repository_label);

        let refresh_button = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh_button.add_css_class("flat");
        refresh_button.set_focus_on_click(false);
        refresh_button.set_tooltip_text(Some("Refresh worktrees"));
        refresh_button.update_property(&[gtk::accessible::Property::Label("Refresh worktrees")]);

        let close_button = gtk::Button::from_icon_name("window-close-symbolic");
        close_button.add_css_class("flat");
        close_button.set_focus_on_click(false);
        close_button.set_tooltip_text(Some("Close worktree panel"));
        close_button.update_property(&[gtk::accessible::Property::Label("Close worktree panel")]);

        header.append(&title_box);
        header.append(&refresh_button);
        header.append(&close_button);
        root.append(&header);

        let spinner = gtk::Spinner::new();
        spinner.set_size_request(32, 32);
        let loading_label = gtk::Label::new(Some("Loading worktrees…"));
        loading_label.add_css_class("dim-label");
        let loading = gtk::Box::new(gtk::Orientation::Vertical, 12);
        loading.set_halign(gtk::Align::Center);
        loading.set_valign(gtk::Align::Center);
        loading.append(&spinner);
        loading.append(&loading_label);

        let retry_button = gtk::Button::with_label("Retry");
        retry_button.add_css_class("suggested-action");
        retry_button.set_halign(gtk::Align::Center);
        retry_button.set_visible(false);
        let status = adw::StatusPage::builder().child(&retry_button).build();

        let list = gtk::ListBox::new();
        list.add_css_class("flowmux-worktree-list");
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.set_activate_on_single_click(false);
        let scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .vexpand(true)
            .build();

        let content = gtk::Stack::new();
        content.set_hexpand(true);
        content.set_vexpand(true);
        content.add_named(&loading, Some("loading"));
        content.add_named(&status, Some("status"));
        content.add_named(&scroll, Some("list"));
        root.append(&content);

        let panel = Self {
            root,
            repository_label,
            content,
            list,
            scroll,
            status,
            spinner,
            close_button,
            refresh_button,
            rows: Rc::new(RefCell::new(Vec::new())),
            open: Rc::new(Cell::new(false)),
            on_info: Rc::new(RefCell::new(None)),
            on_remove: Rc::new(RefCell::new(None)),
            on_refresh: Rc::new(RefCell::new(None)),
            on_close: Rc::new(RefCell::new(None)),
            on_focus_out: Rc::new(RefCell::new(None)),
            on_focus_changed: Rc::new(RefCell::new(None)),
        };

        panel.install_focus_style();
        panel.install_pointer_focus();
        panel.install_keyboard();

        let on_refresh = panel.on_refresh.clone();
        panel.refresh_button.connect_clicked(move |_| {
            if let Some(callback) = on_refresh.borrow().as_ref() {
                callback();
            }
        });
        let on_refresh = panel.on_refresh.clone();
        retry_button.connect_clicked(move |_| {
            if let Some(callback) = on_refresh.borrow().as_ref() {
                callback();
            }
        });
        let on_close = panel.on_close.clone();
        panel.close_button.connect_clicked(move |_| {
            if let Some(callback) = on_close.borrow().as_ref() {
                callback();
            }
        });
        panel
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn show_loading(&self) {
        self.open.set(true);
        self.root.set_visible(true);
        self.refresh_button.set_sensitive(false);
        self.spinner.start();
        self.content.set_visible_child_name("loading");
    }

    pub fn show_not_repository(&self) {
        self.clear_repository_context();
        self.show_status(
            "Not a Git repository",
            "The focused pane is not in a Git repository",
            false,
        );
    }

    pub fn show_error(&self, message: &str) {
        self.clear_repository_context();
        self.show_status("Unable to load worktrees", message, true);
    }

    pub fn set_rows(&self, repository_name: &str, rows: Vec<WorktreeRowView>) {
        let selected_path = self.selected_path();
        self.refresh_button.set_sensitive(true);
        self.spinner.stop();
        self.repository_label.set_text(repository_name);
        self.repository_label
            .set_tooltip_text(Some(repository_name));

        self.clear_rows();
        *self.rows.borrow_mut() = rows;

        for row in self.rows.borrow().iter() {
            self.list.append(&self.build_row(row));
        }

        if self.rows.borrow().is_empty() {
            self.show_status(
                "No worktrees found",
                "This repository has no worktrees",
                false,
            );
            return;
        }

        if let Some(button) = self.retry_button() {
            button.set_visible(false);
        }
        self.content.set_visible_child_name("list");
        let selected_index = selected_path
            .as_ref()
            .and_then(|path| {
                self.rows
                    .borrow()
                    .iter()
                    .position(|row| row.info.path == *path)
            })
            .unwrap_or(0);
        self.select_index_internal(selected_index);
    }

    pub fn hide(&self) {
        self.open.set(false);
        self.root.set_visible(false);
    }

    pub fn is_open(&self) -> bool {
        self.open.get()
    }

    pub fn is_showing_rows(&self) -> bool {
        self.content.visible_child_name().as_deref() == Some("list")
    }

    pub fn grab_focus(&self) {
        self.root.grab_focus();
    }

    pub fn selected_path(&self) -> Option<PathBuf> {
        self.list
            .selected_row()
            .and_then(|row| self.path_at_index(row.index()))
    }

    pub fn row_for_path(&self, path: &Path) -> Option<WorktreeRowView> {
        self.rows
            .borrow()
            .iter()
            .find(|row| row.info.path.as_path() == path)
            .cloned()
    }

    pub fn set_operation_in_progress(&self, path: &Path, in_progress: bool) -> bool {
        let mut rows = self.rows.borrow_mut();
        let Some(row) = rows.iter_mut().find(|row| row.info.path.as_path() == path) else {
            return false;
        };
        row.operation_in_progress = in_progress;
        let updated_rows = rows.clone();
        drop(rows);
        if self.is_showing_rows() {
            let repository_name = self.repository_label.text();
            self.set_rows(&repository_name, updated_rows);
        }
        true
    }

    pub fn connect_info<F: Fn(PathBuf) + 'static>(&self, callback: F) {
        *self.on_info.borrow_mut() = Some(Box::new(callback));
    }

    pub fn connect_remove<F: Fn(PathBuf) + 'static>(&self, callback: F) {
        *self.on_remove.borrow_mut() = Some(Box::new(callback));
    }

    pub fn connect_refresh<F: Fn() + 'static>(&self, callback: F) {
        *self.on_refresh.borrow_mut() = Some(Box::new(callback));
    }

    pub fn connect_close<F: Fn() + 'static>(&self, callback: F) {
        *self.on_close.borrow_mut() = Some(Box::new(callback));
    }

    pub fn connect_focus_out<F: Fn(FocusDir) + 'static>(&self, callback: F) {
        *self.on_focus_out.borrow_mut() = Some(Box::new(callback));
    }

    pub fn connect_focus_changed<F: Fn(bool) + 'static>(&self, callback: F) {
        *self.on_focus_changed.borrow_mut() = Some(Box::new(callback));
    }

    fn show_status(&self, title: &str, description: &str, retry: bool) {
        self.refresh_button.set_sensitive(true);
        self.spinner.stop();
        self.status.set_title(title);
        self.status.set_description(Some(description));
        if let Some(button) = self.retry_button() {
            button.set_visible(retry);
        }
        self.content.set_visible_child_name("status");
    }

    fn clear_repository_context(&self) {
        self.repository_label.set_text("");
        self.repository_label.set_tooltip_text(None);
        self.clear_rows();
        self.rows.borrow_mut().clear();
    }

    fn clear_rows(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
    }

    fn retry_button(&self) -> Option<gtk::Button> {
        self.status.child()?.downcast().ok()
    }

    fn build_row(&self, row: &WorktreeRowView) -> gtk::ListBoxRow {
        let list_row = gtk::ListBoxRow::new();
        list_row.add_css_class("flowmux-worktree-row");
        list_row.set_selectable(true);
        list_row.set_activatable(false);
        list_row.set_tooltip_text(Some(row.info.path.to_string_lossy().as_ref()));

        let content = gtk::Box::new(gtk::Orientation::Vertical, 5);
        content.set_margin_top(8);
        content.set_margin_bottom(8);
        content.set_margin_start(10);
        content.set_margin_end(10);

        let branch = gtk::Label::new(Some(&branch_label(&row.info)));
        branch.add_css_class("heading");
        branch.set_xalign(0.0);
        branch.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let description = gtk::Label::new(Some(
            row.info
                .commit_subject
                .as_deref()
                .unwrap_or("Description unavailable"),
        ));
        description.add_css_class("caption");
        description.set_xalign(0.0);
        description.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let path = gtk::Label::new(Some(row.info.path.to_string_lossy().as_ref()));
        path.add_css_class("dim-label");
        path.set_xalign(0.0);
        path.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        path.set_tooltip_text(Some(row.info.path.to_string_lossy().as_ref()));

        let badges = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        for text in badge_labels(&row.info) {
            let badge = gtk::Label::new(Some(&text));
            badge.add_css_class("caption");
            badge.add_css_class("dim-label");
            badges.append(&badge);
        }

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.set_halign(gtk::Align::End);
        let info = gtk::Button::with_label("Info");
        let remove = gtk::Button::with_label("Remove");

        info.set_tooltip_text(Some("Show worktree information"));

        let remove_block_reason = row.remove_block_reason.as_deref().or(row
            .operation_in_progress
            .then_some("A worktree operation is in progress"));
        remove.set_sensitive(remove_block_reason.is_none());
        remove.set_tooltip_text(Some(remove_block_reason.unwrap_or("Remove worktree")));
        if let Some(reason) = remove_block_reason {
            remove.update_property(&[gtk::accessible::Property::Description(reason)]);
        }

        let path_for_info = row.info.path.clone();
        let on_info = self.on_info.clone();
        info.connect_clicked(move |_| {
            if let Some(callback) = on_info.borrow().as_ref() {
                callback(path_for_info.clone());
            }
        });
        let path_for_remove = row.info.path.clone();
        let on_remove = self.on_remove.clone();
        remove.connect_clicked(move |_| {
            if let Some(callback) = on_remove.borrow().as_ref() {
                callback(path_for_remove.clone());
            }
        });

        actions.append(&info);
        actions.append(&remove);
        content.append(&branch);
        content.append(&description);
        content.append(&path);
        content.append(&badges);
        content.append(&actions);
        list_row.set_child(Some(&content));
        list_row
    }

    fn install_focus_style(&self) {
        let focus = gtk::EventControllerFocus::new();
        let root = self.root.downgrade();
        let on_focus_changed = self.on_focus_changed.clone();
        focus.connect_enter(move |_| {
            let Some(root) = root.upgrade() else {
                return;
            };
            root.add_css_class("focused");
            if let Some(callback) = on_focus_changed.borrow().as_ref() {
                callback(true);
            }
        });
        let root = self.root.downgrade();
        let on_focus_changed = self.on_focus_changed.clone();
        focus.connect_leave(move |_| {
            let Some(root) = root.upgrade() else {
                return;
            };
            root.remove_css_class("focused");
            if let Some(callback) = on_focus_changed.borrow().as_ref() {
                callback(false);
            }
        });
        self.root.add_controller(focus);
    }

    fn install_pointer_focus(&self) {
        let click = gtk::GestureClick::new();
        let root = self.root.downgrade();
        click.connect_pressed(move |_, _, _, _| {
            if let Some(root) = root.upgrade() {
                root.grab_focus();
            }
        });
        self.root.add_controller(click);
    }

    fn install_keyboard(&self) {
        let key = gtk::EventControllerKey::new();
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        let list = self.list.downgrade();
        let scroll = self.scroll.downgrade();
        let rows = Rc::downgrade(&self.rows);
        let on_close = Rc::downgrade(&self.on_close);
        let on_focus_out = Rc::downgrade(&self.on_focus_out);
        key.connect_key_pressed(move |_, key, _, state| {
            let (Some(list), Some(scroll), Some(rows), Some(on_close), Some(on_focus_out)) = (
                list.upgrade(),
                scroll.upgrade(),
                rows.upgrade(),
                on_close.upgrade(),
                on_focus_out.upgrade(),
            ) else {
                return glib::Propagation::Proceed;
            };
            dispatch_key(&list, &scroll, &rows, &on_close, &on_focus_out, key, state)
        });
        self.root.add_controller(key);
    }

    #[cfg(all(test, not(target_os = "macos")))]
    fn handle_key(&self, key: gdk::Key, state: gdk::ModifierType) -> glib::Propagation {
        dispatch_key(
            &self.list,
            &self.scroll,
            &self.rows,
            &self.on_close,
            &self.on_focus_out,
            key,
            state,
        )
    }

    #[cfg(all(test, not(target_os = "macos")))]
    fn handle_navigation_key(&self, key: gdk::Key) -> glib::Propagation {
        dispatch_navigation_key(&self.list, &self.scroll, &self.rows, key)
    }

    fn select_index_internal(&self, index: usize) {
        select_index_and_scroll(&self.list, &self.scroll, index);
    }

    fn path_at_index(&self, index: i32) -> Option<PathBuf> {
        usize::try_from(index).ok().and_then(|index| {
            self.rows
                .borrow()
                .get(index)
                .map(|row| row.info.path.clone())
        })
    }

    #[cfg(all(test, not(target_os = "macos")))]
    fn row_count(&self) -> usize {
        self.rows.borrow().len()
    }

    #[cfg(all(test, not(target_os = "macos")))]
    fn action_labels(&self, index: i32) -> Vec<String> {
        let Some(row) = self.list.row_at_index(index) else {
            return Vec::new();
        };
        descendant_buttons(row.upcast_ref())
            .into_iter()
            .filter_map(|button| button.label().map(Into::into))
            .collect()
    }

    #[cfg(all(test, not(target_os = "macos")))]
    fn select_index(&self, index: i32) {
        if let Ok(index) = usize::try_from(index) {
            self.select_index_internal(index);
        }
    }

    #[cfg(all(test, not(target_os = "macos")))]
    pub(crate) fn repository_name(&self) -> Option<String> {
        let name = self.repository_label.text();
        (!name.is_empty()).then(|| name.into())
    }
}

fn dispatch_key(
    list: &gtk::ListBox,
    scroll: &gtk::ScrolledWindow,
    rows: &RefCell<Vec<WorktreeRowView>>,
    on_close: &RefCell<Option<Box<dyn Fn()>>>,
    on_focus_out: &RefCell<Option<Box<dyn Fn(FocusDir)>>>,
    key: gdk::Key,
    state: gdk::ModifierType,
) -> glib::Propagation {
    if key == gdk::Key::Escape {
        if let Some(callback) = on_close.borrow().as_ref() {
            callback();
        }
        return glib::Propagation::Stop;
    }
    let plain_alt = state.contains(gdk::ModifierType::ALT_MASK)
        && !state.intersects(
            gdk::ModifierType::CONTROL_MASK
                | gdk::ModifierType::SHIFT_MASK
                | gdk::ModifierType::SUPER_MASK,
        );
    if plain_alt {
        if let Some(direction) = key_to_focus_dir(key) {
            if let Some(callback) = on_focus_out.borrow().as_ref() {
                callback(direction);
            }
            return glib::Propagation::Stop;
        }
    }
    if state.intersects(
        gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::SUPER_MASK,
    ) {
        return glib::Propagation::Proceed;
    }
    dispatch_navigation_key(list, scroll, rows, key)
}

fn dispatch_navigation_key(
    list: &gtk::ListBox,
    scroll: &gtk::ScrolledWindow,
    rows: &RefCell<Vec<WorktreeRowView>>,
    key: gdk::Key,
) -> glib::Propagation {
    let row_count = rows.borrow().len();
    if row_count == 0 {
        return glib::Propagation::Proceed;
    }

    let selected = list.selected_row().map(|row| row.index() as usize);
    let next = if key == gdk::Key::Up {
        Some(selected.map_or(row_count - 1, |index| {
            if index == 0 {
                row_count - 1
            } else {
                index - 1
            }
        }))
    } else if key == gdk::Key::Down {
        Some(selected.map_or(0, |index| (index + 1) % row_count))
    } else if key == gdk::Key::Home {
        Some(0)
    } else if key == gdk::Key::End {
        Some(row_count - 1)
    } else {
        None
    };
    if let Some(index) = next {
        select_index_and_scroll(list, scroll, index);
        return glib::Propagation::Stop;
    }

    glib::Propagation::Proceed
}

fn select_index_and_scroll(list: &gtk::ListBox, scroll: &gtk::ScrolledWindow, index: usize) {
    let Some(row) = list.row_at_index(index as i32) else {
        return;
    };
    list.select_row(Some(&row));
    scroll_row_into_view(list, scroll, &row);
}

fn scroll_row_into_view(list: &gtk::ListBox, scroll: &gtk::ScrolledWindow, row: &gtk::ListBoxRow) {
    let Some(bounds) = row.compute_bounds(list) else {
        return;
    };
    let adjustment = scroll.vadjustment();
    let row_top = bounds.y() as f64;
    let row_bottom = row_top + bounds.height() as f64;
    let view_top = adjustment.value();
    let view_bottom = view_top + adjustment.page_size();

    if row_top < view_top {
        adjustment.set_value(row_top);
    } else if row_bottom > view_bottom {
        adjustment.set_value((row_bottom - adjustment.page_size()).max(0.0));
    }
}

fn branch_label(info: &WorktreeInfo) -> String {
    info.branch.clone().unwrap_or_else(|| {
        let short_head: String = info.head.chars().take(8).collect();
        format!("Detached at {short_head}")
    })
}

fn badge_labels(info: &WorktreeInfo) -> Vec<String> {
    let mut labels = Vec::new();
    if info.is_current {
        labels.push("Activated".into());
    }
    if info.lock_reason.is_some() {
        labels.push("Locked".into());
    }
    match &info.changes {
        Some(changes) if changes.is_clean() => labels.push("Clean".into()),
        Some(changes) => {
            let modified = changes.staged + changes.unstaged;
            if modified > 0 {
                labels.push(format!("Modified {modified}"));
            }
            if changes.untracked > 0 {
                labels.push(format!("Untracked {}", changes.untracked));
            }
        }
        None => labels.push("Status unavailable".into()),
    }
    labels
}

fn key_to_focus_dir(key: gdk::Key) -> Option<FocusDir> {
    if key == gdk::Key::Left || key == gdk::Key::Back {
        Some(FocusDir::Left)
    } else if key == gdk::Key::Right || key == gdk::Key::Forward {
        Some(FocusDir::Right)
    } else if key == gdk::Key::Up {
        Some(FocusDir::Up)
    } else if key == gdk::Key::Down {
        Some(FocusDir::Down)
    } else {
        None
    }
}

#[cfg(all(test, not(target_os = "macos")))]
fn descendant_buttons(widget: &gtk::Widget) -> Vec<gtk::Button> {
    let mut buttons = Vec::new();
    let mut child = widget.first_child();
    while let Some(widget) = child {
        if let Ok(button) = widget.clone().downcast::<gtk::Button>() {
            buttons.push(button);
        }
        buttons.extend(descendant_buttons(&widget));
        child = widget.next_sibling();
    }
    buttons
}

#[cfg(all(test, not(target_os = "macos")))]
mod tests {
    use super::*;
    use crate::bridge::FocusDir;
    use flowmux_vcs::worktree::{WorktreeChanges, WorktreeInfo};
    use gtk::{gdk, glib};
    use std::cell::{Cell, RefCell};
    use std::ffi::{CStr, CString};
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::Duration;

    fn info(path: &str, branch: &str) -> WorktreeInfo {
        WorktreeInfo {
            path: path.into(),
            branch: Some(branch.into()),
            head: "1234567890abcdef".into(),
            commit_subject: Some(format!("commit for {branch}")),
            commit_time: Some(1_700_000_000),
            changes: Some(WorktreeChanges::default()),
            is_main: branch == "main",
            is_current: branch == "main",
            is_bare: false,
            lock_reason: None,
            prunable_reason: None,
        }
    }

    fn action_buttons(panel: &WorktreePanel, index: i32) -> Vec<gtk::Button> {
        let row = panel.list.row_at_index(index).expect("worktree row");
        descendant_buttons(row.upcast_ref())
    }

    fn descendant_labels(widget: &gtk::Widget) -> Vec<gtk::Label> {
        fn collect(widget: &gtk::Widget, labels: &mut Vec<gtk::Label>) {
            let mut child = widget.first_child();
            while let Some(widget) = child {
                if let Ok(label) = widget.clone().downcast::<gtk::Label>() {
                    labels.push(label);
                }
                collect(&widget, labels);
                child = widget.next_sibling();
            }
        }

        let mut labels = Vec::new();
        collect(widget, &mut labels);
        labels
    }

    fn assert_accessible_description(button: &gtk::Button, expected: &str) {
        let expected = CString::new(expected).expect("accessible description");
        let mismatch = unsafe {
            gtk::ffi::gtk_test_accessible_check_property(
                button.as_ptr().cast(),
                gtk::ffi::GTK_ACCESSIBLE_PROPERTY_DESCRIPTION,
                expected.as_ptr(),
            )
        };
        if mismatch.is_null() {
            return;
        }

        let mismatch_text = unsafe { CStr::from_ptr(mismatch) }
            .to_string_lossy()
            .into_owned();
        unsafe { glib::ffi::g_free(mismatch.cast()) };
        panic!("accessible description mismatch: {mismatch_text}");
    }

    #[gtk::test]
    fn dropping_panel_releases_root_and_children() {
        let panel = WorktreePanel::new();
        let root = panel.root.downgrade();
        let list = panel.list.downgrade();

        drop(panel);

        assert!(
            root.upgrade().is_none(),
            "root was retained by a controller"
        );
        assert!(
            list.upgrade().is_none(),
            "child was retained by a controller"
        );
    }

    #[gtk::test]
    fn panel_state_transitions_show_loading_status_rows_and_hide() {
        let panel = WorktreePanel::new();
        assert!(!panel.is_open());
        assert!(!panel.widget().is_visible());

        panel.show_loading();
        assert!(panel.is_open());
        assert!(panel.widget().is_visible());
        assert!(!panel.refresh_button.is_sensitive());
        assert!(panel.spinner.is_spinning());
        assert_eq!(
            panel.content.visible_child_name().as_deref(),
            Some("loading")
        );

        panel.show_not_repository();
        assert!(!panel.spinner.is_spinning());
        assert!(panel.refresh_button.is_sensitive());
        assert_eq!(
            panel.content.visible_child_name().as_deref(),
            Some("status")
        );
        assert_eq!(
            panel.status.description().as_deref(),
            Some("The focused pane is not in a Git repository")
        );

        panel.show_error("Git is not available");
        assert_eq!(
            panel.status.description().as_deref(),
            Some("Git is not available")
        );

        panel.set_rows("repo", Vec::new());
        assert_eq!(panel.repository_name().as_deref(), Some("repo"));
        assert_eq!(panel.status.title().as_str(), "No worktrees found");

        panel.set_rows(
            "repo",
            vec![WorktreeRowView::available(info("/repo/main", "main"))],
        );
        assert_eq!(panel.content.visible_child_name().as_deref(), Some("list"));
        assert_eq!(panel.selected_path(), Some(PathBuf::from("/repo/main")));

        panel.show_not_repository();
        assert_eq!(panel.repository_name(), None);
        assert!(panel.row_for_path(Path::new("/repo/main")).is_none());
        assert!(panel.list.first_child().is_none());

        panel.hide();
        assert!(!panel.is_open());
        assert!(!panel.widget().is_visible());
    }

    #[gtk::test]
    fn rows_preserve_selection_and_render_action_labels() {
        let panel = WorktreePanel::new();
        panel.set_rows(
            "repo",
            vec![
                WorktreeRowView::available(info("/repo/main", "main")),
                WorktreeRowView::available(info("/repo/feature", "feature")),
            ],
        );
        panel.select_index(1);
        assert_eq!(panel.selected_path(), Some(PathBuf::from("/repo/feature")));
        assert_eq!(panel.row_count(), 2);
        assert_eq!(panel.action_labels(1), vec!["Info", "Remove"]);

        panel.set_rows(
            "repo",
            vec![
                WorktreeRowView::available(info("/repo/new", "new")),
                WorktreeRowView::available(info("/repo/feature", "feature")),
            ],
        );
        assert_eq!(panel.selected_path(), Some(PathBuf::from("/repo/feature")));

        panel.set_rows(
            "repo",
            vec![WorktreeRowView::available(info("/repo/new", "new"))],
        );
        assert_eq!(panel.selected_path(), Some(PathBuf::from("/repo/new")));
    }

    #[gtk::test]
    fn row_lookup_is_exact_and_rendering_handles_missing_description() {
        let panel = WorktreePanel::new();
        let mut row = WorktreeRowView::available(info("/repo/feature", "feature"));
        row.info.commit_subject = None;
        panel.set_rows("repo", vec![row]);

        assert!(panel.row_for_path(Path::new("/repo/feature")).is_some());
        assert!(panel.row_for_path(Path::new("/repo/feature/..")).is_none());

        let list_row = panel.list.row_at_index(0).expect("worktree row");
        let labels = descendant_labels(list_row.upcast_ref());
        assert!(labels
            .iter()
            .any(|label| label.text().as_str() == "Description unavailable"));
        let path = labels
            .iter()
            .find(|label| label.text().as_str() == "/repo/feature")
            .expect("path label");
        assert_eq!(path.ellipsize(), gtk::pango::EllipsizeMode::Middle);
    }

    #[gtk::test]
    fn action_sensitivity_and_tooltips_explain_disabled_buttons() {
        let panel = WorktreePanel::new();
        let mut blocked = WorktreeRowView::available(info("/repo/blocked", "blocked"));
        blocked.remove_block_reason = Some("Close the open workspace first".into());
        let mut busy = WorktreeRowView::available(info("/repo/busy", "busy"));
        busy.operation_in_progress = true;
        panel.set_rows("repo", vec![blocked, busy]);

        let blocked_buttons = action_buttons(&panel, 0);
        assert!(blocked_buttons[0].is_sensitive());
        assert!(!blocked_buttons[1].is_sensitive());
        assert_eq!(
            blocked_buttons[1].tooltip_text().as_deref(),
            Some("Close the open workspace first")
        );

        let busy_buttons = action_buttons(&panel, 1);
        assert!(busy_buttons[0].is_sensitive());
        assert!(!busy_buttons[1].is_sensitive());
        assert!(busy_buttons[1].tooltip_text().is_some());
    }

    #[gtk::test]
    fn operation_progress_updates_only_the_target_row() {
        let panel = WorktreePanel::new();
        panel.set_rows(
            "repo",
            vec![
                WorktreeRowView::available(info("/repo/a", "a")),
                WorktreeRowView::available(info("/repo/b", "b")),
            ],
        );

        assert!(panel.set_operation_in_progress(Path::new("/repo/b"), true));
        assert!(
            !panel
                .row_for_path(Path::new("/repo/a"))
                .unwrap()
                .operation_in_progress
        );
        assert!(
            panel
                .row_for_path(Path::new("/repo/b"))
                .unwrap()
                .operation_in_progress
        );
        assert!(action_buttons(&panel, 0)[1].is_sensitive());
        assert!(!action_buttons(&panel, 1)[1].is_sensitive());
    }

    #[gtk::test]
    fn disabled_actions_expose_reasons_to_assistive_technology() {
        let panel = WorktreePanel::new();
        let mut blocked = WorktreeRowView::available(info("/repo/blocked", "blocked"));
        blocked.remove_block_reason = Some("Close the open workspace first".into());
        panel.set_rows("repo", vec![blocked]);

        let blocked_buttons = action_buttons(&panel, 0);
        assert_accessible_description(&blocked_buttons[1], "Close the open workspace first");
    }

    #[gtk::test]
    fn badges_follow_worktree_status_policy() {
        let mut row = info("/repo/main", "main");
        row.lock_reason = Some("maintenance".into());
        row.changes = Some(WorktreeChanges {
            staged: 2,
            unstaged: 3,
            untracked: 4,
        });
        assert_eq!(
            badge_labels(&row),
            ["Activated", "Locked", "Modified 5", "Untracked 4"]
        );

        row.is_current = false;
        row.lock_reason = None;
        row.changes = None;
        assert_eq!(badge_labels(&row), ["Status unavailable"]);
    }

    #[gtk::test]
    fn row_and_header_buttons_emit_callbacks_for_the_expected_path() {
        let panel = WorktreePanel::new();
        panel.set_rows(
            "repo",
            vec![WorktreeRowView::available(info("/repo/feature", "feature"))],
        );
        let inspected = Rc::new(RefCell::new(Vec::new()));
        let removed = Rc::new(RefCell::new(Vec::new()));
        let refreshed = Rc::new(Cell::new(0));
        let closed = Rc::new(Cell::new(0));

        {
            let inspected = inspected.clone();
            panel.connect_info(move |path| inspected.borrow_mut().push(path));
        }
        {
            let removed = removed.clone();
            panel.connect_remove(move |path| removed.borrow_mut().push(path));
        }
        {
            let refreshed = refreshed.clone();
            panel.connect_refresh(move || refreshed.set(refreshed.get() + 1));
        }
        {
            let closed = closed.clone();
            panel.connect_close(move || closed.set(closed.get() + 1));
        }

        let buttons = action_buttons(&panel, 0);
        buttons[0].emit_clicked();
        buttons[1].emit_clicked();
        panel.refresh_button.emit_clicked();
        panel.close_button.emit_clicked();

        let expected = vec![PathBuf::from("/repo/feature")];
        assert_eq!(*inspected.borrow(), expected);
        assert_eq!(*removed.borrow(), expected);
        assert_eq!(refreshed.get(), 1);
        assert_eq!(closed.get(), 1);
    }

    #[gtk::test]
    fn up_down_wrap_and_home_end_select_boundaries() {
        let panel = WorktreePanel::new();
        panel.set_rows(
            "repo",
            vec![
                WorktreeRowView::available(info("/repo/a", "a")),
                WorktreeRowView::available(info("/repo/b", "b")),
            ],
        );
        panel.select_index(0);
        assert_eq!(
            panel.handle_navigation_key(gdk::Key::Up),
            glib::Propagation::Stop
        );
        assert_eq!(panel.selected_path(), Some(PathBuf::from("/repo/b")));
        panel.handle_navigation_key(gdk::Key::Down);
        assert_eq!(panel.selected_path(), Some(PathBuf::from("/repo/a")));
        panel.handle_navigation_key(gdk::Key::End);
        assert_eq!(panel.selected_path(), Some(PathBuf::from("/repo/b")));
        panel.handle_navigation_key(gdk::Key::Home);
        assert_eq!(panel.selected_path(), Some(PathBuf::from("/repo/a")));
    }

    #[gtk::test]
    async fn navigation_scrolls_the_selected_row_into_view() {
        let panel = WorktreePanel::new();
        panel.show_loading();
        panel.set_rows(
            "repo",
            (0..30)
                .map(|index| {
                    WorktreeRowView::available(info(
                        &format!("/repo/worktree-{index}"),
                        &format!("branch-{index}"),
                    ))
                })
                .collect(),
        );
        let window = gtk::Window::new();
        window.set_default_size(320, 180);
        window.set_child(Some(panel.widget()));
        window.present();

        let scroll = panel.scroll.clone();
        let adjustment = scroll.vadjustment();
        let last_row = panel.list.row_at_index(29).expect("last worktree row");
        let started = std::time::Instant::now();
        let last_bounds = loop {
            if let Some(bounds) = last_row.compute_bounds(&panel.list) {
                if bounds.y() + bounds.height() > 100.0 {
                    break bounds;
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "timed out waiting for the last row to be allocated below the synthetic viewport"
            );
            glib::timeout_future(Duration::from_millis(10)).await;
        };
        panel.select_index(0);
        adjustment.set_lower(0.0);
        adjustment.set_upper(10_000.0);
        adjustment.set_page_size(100.0);
        adjustment.set_value(0.0);

        assert_eq!(
            panel.handle_navigation_key(gdk::Key::End),
            glib::Propagation::Stop
        );
        assert_eq!(
            panel.selected_path(),
            Some(PathBuf::from("/repo/worktree-29"))
        );
        assert!(
            adjustment.value() > 0.0,
            "selection did not scroll: value={}, page_size={}, upper={}, row_bottom={}",
            adjustment.value(),
            adjustment.page_size(),
            adjustment.upper(),
            last_bounds.y() + last_bounds.height()
        );

        window.close();
        glib::timeout_future(Duration::from_millis(50)).await;
    }

    #[gtk::test]
    fn enter_alt_arrows_escape_and_tab_follow_capture_phase_contract() {
        let panel = WorktreePanel::new();
        panel.set_rows(
            "repo",
            vec![WorktreeRowView::available(info("/repo/feature", "feature"))],
        );
        let focus_out = Rc::new(RefCell::new(None));
        let closed = Rc::new(Cell::new(false));
        {
            let focus_out = focus_out.clone();
            panel.connect_focus_out(move |direction| *focus_out.borrow_mut() = Some(direction));
        }
        {
            let closed = closed.clone();
            panel.connect_close(move || closed.set(true));
        }

        assert_eq!(
            panel.handle_key(gdk::Key::Return, gdk::ModifierType::empty()),
            glib::Propagation::Proceed
        );

        assert_eq!(
            dispatch_key(
                &panel.list,
                &panel.scroll,
                &panel.rows,
                &panel.on_close,
                &panel.on_focus_out,
                gdk::Key::Return,
                gdk::ModifierType::empty(),
            ),
            glib::Propagation::Proceed
        );

        for (key, direction) in [
            (gdk::Key::Left, FocusDir::Left),
            (gdk::Key::Right, FocusDir::Right),
            (gdk::Key::Up, FocusDir::Up),
            (gdk::Key::Down, FocusDir::Down),
        ] {
            *focus_out.borrow_mut() = None;
            assert_eq!(
                panel.handle_key(key, gdk::ModifierType::ALT_MASK),
                glib::Propagation::Stop
            );
            assert_eq!(*focus_out.borrow(), Some(direction));
        }

        *focus_out.borrow_mut() = None;
        assert_eq!(
            panel.handle_key(
                gdk::Key::Left,
                gdk::ModifierType::ALT_MASK | gdk::ModifierType::CONTROL_MASK,
            ),
            glib::Propagation::Proceed
        );
        assert_eq!(*focus_out.borrow(), None);
        assert_eq!(
            panel.handle_key(gdk::Key::Tab, gdk::ModifierType::empty()),
            glib::Propagation::Proceed
        );
        assert_eq!(
            panel.handle_key(gdk::Key::Tab, gdk::ModifierType::SHIFT_MASK),
            glib::Propagation::Proceed
        );
        assert_eq!(
            panel.handle_key(gdk::Key::Escape, gdk::ModifierType::empty()),
            glib::Propagation::Stop
        );
        assert!(closed.get());
    }
}
