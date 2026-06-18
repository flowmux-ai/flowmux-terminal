// SPDX-License-Identifier: GPL-3.0-or-later

//! Right-side Finder-style file browser for the focused pane's cwd.

use crate::bridge::FocusDir;
use crate::ui::popover_pos;
use crate::ui::show_in_folder;
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

#[derive(Clone)]
pub struct FileBrowserPanel {
    root: gtk::Box,
    path_label: gtk::Label,
    list: gtk::ListBox,
    scroll: gtk::ScrolledWindow,
    status: gtk::Label,
    close_button: gtk::Button,
    model: Rc<RefCell<FileBrowserModel>>,
    on_focus_out: Rc<RefCell<Option<Box<dyn Fn(FocusDir)>>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileBrowserRow {
    path: PathBuf,
    is_dir: bool,
    depth: usize,
    expanded: bool,
    focused: bool,
    cut: bool,
}

#[derive(Debug, Clone)]
struct FsEntry {
    path: PathBuf,
    name: String,
    is_dir: bool,
}

#[derive(Debug, Default)]
struct FileBrowserModel {
    root: Option<PathBuf>,
    expanded: HashSet<PathBuf>,
    focused: Option<PathBuf>,
    clipboard: Option<FileClipboard>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileBrowserActivation {
    None,
    Refresh,
    Open(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardOperation {
    Copy,
    Cut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileClipboard {
    operation: ClipboardOperation,
    paths: Vec<PathBuf>,
}

impl FileBrowserPanel {
    pub fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("flowmux-file-browser");
        root.set_size_request(300, -1);
        root.set_hexpand(false);
        root.set_vexpand(true);
        root.set_focusable(true);
        root.set_visible(false);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.add_css_class("flowmux-file-browser-header");

        let title_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        title_box.set_hexpand(true);
        let title = gtk::Label::new(Some("Files"));
        title.add_css_class("heading");
        title.set_xalign(0.0);
        let path_label = gtk::Label::new(None);
        path_label.add_css_class("dim-label");
        path_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        path_label.set_xalign(0.0);
        title_box.append(&title);
        title_box.append(&path_label);

        let close_button = gtk::Button::from_icon_name("window-close-symbolic");
        close_button.add_css_class("flat");
        close_button.set_tooltip_text(Some("Close file browser"));
        close_button.set_focus_on_click(false);

        header.append(&title_box);
        header.append(&close_button);
        root.append(&header);

        let list = gtk::ListBox::new();
        list.add_css_class("flowmux-file-browser-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        list.set_activate_on_single_click(false);

        let status = gtk::Label::new(Some("No focused directory"));
        status.add_css_class("dim-label");
        status.set_margin_top(16);
        status.set_margin_start(16);
        status.set_margin_end(16);
        status.set_wrap(true);

        let scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .hexpand(true)
            .vexpand(true)
            .build();
        root.append(&scroll);
        root.append(&status);

        let panel = Self {
            root,
            path_label,
            list,
            scroll,
            status,
            close_button,
            model: Rc::new(RefCell::new(FileBrowserModel::default())),
            on_focus_out: Rc::new(RefCell::new(None)),
        };

        panel.install_focus_style();
        panel.install_pointer_focus();
        panel.install_keyboard();

        panel
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn connect_close<F: Fn() + 'static>(&self, f: F) {
        self.close_button.connect_clicked(move |_| f());
    }

    pub fn connect_focus_out<F: Fn(FocusDir) + 'static>(&self, f: F) {
        *self.on_focus_out.borrow_mut() = Some(Box::new(f));
    }

    pub fn show_for_root(&self, root: PathBuf) {
        self.model.borrow_mut().set_root(root);
        self.root.set_visible(true);
        self.refresh();
    }

    pub fn hide(&self) {
        self.root.set_visible(false);
    }

    pub fn grab_focus(&self) {
        self.root.grab_focus();
    }

    pub fn refresh(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }

        let root = {
            let model = self.model.borrow();
            model.root.clone()
        };

        let Some(root) = root else {
            self.path_label.set_text("");
            self.status.set_text("No focused directory");
            self.status.set_visible(true);
            return;
        };

        self.path_label.set_text(&root.to_string_lossy());
        self.path_label
            .set_tooltip_text(Some(root.to_string_lossy().as_ref()));

        if !root.is_dir() {
            self.status.set_text("Focused path is not a directory");
            self.status.set_visible(true);
            return;
        }

        let rows = {
            let mut model = self.model.borrow_mut();
            model.sync_focus();
            model.rows()
        };

        if rows.is_empty() {
            self.status.set_text("Directory is empty");
            self.status.set_visible(true);
            return;
        }

        self.status.set_visible(false);
        for row in rows {
            self.list.append(&self.build_row(&row));
        }
        self.scroll_focused_row_into_view();
    }

    fn install_focus_style(&self) {
        let focus = gtk::EventControllerFocus::new();
        let root = self.root.clone();
        focus.connect_enter(move |_| root.add_css_class("focused"));
        let root = self.root.clone();
        focus.connect_leave(move |_| root.remove_css_class("focused"));
        self.root.add_controller(focus);
    }

    fn install_pointer_focus(&self) {
        let click = gtk::GestureClick::new();
        let root = self.root.clone();
        click.connect_pressed(move |_, _, _, _| {
            root.grab_focus();
        });
        self.root.add_controller(click);
    }

    fn install_keyboard(&self) {
        let key = gtk::EventControllerKey::new();
        let panel = self.clone();
        key.connect_key_pressed(move |_, keyval, _, state| {
            if state.contains(gdk::ModifierType::ALT_MASK) {
                if let Some(dir) = key_to_focus_dir(keyval) {
                    if let Some(cb) = panel.on_focus_out.borrow().as_ref() {
                        cb(dir);
                    }
                    return glib::Propagation::Stop;
                }
            }

            if state.contains(gdk::ModifierType::CONTROL_MASK)
                && !state.intersects(gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK)
            {
                match keyval.to_unicode().map(|ch| ch.to_ascii_lowercase()) {
                    Some('c') => {
                        panel.copy_focused();
                        return glib::Propagation::Stop;
                    }
                    Some('x') => {
                        panel.cut_focused();
                        return glib::Propagation::Stop;
                    }
                    Some('v') => {
                        panel.paste_from_clipboard();
                        return glib::Propagation::Stop;
                    }
                    _ => {}
                }
            }

            if state.intersects(
                gdk::ModifierType::ALT_MASK
                    | gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::SUPER_MASK,
            ) {
                return glib::Propagation::Proceed;
            }

            if keyval == gdk::Key::Up {
                panel.move_focus(-1);
                return glib::Propagation::Stop;
            }
            if keyval == gdk::Key::Down {
                panel.move_focus(1);
                return glib::Propagation::Stop;
            }
            if keyval == gdk::Key::Left {
                panel.collapse_focused();
                return glib::Propagation::Stop;
            }
            if keyval == gdk::Key::Right {
                panel.expand_focused();
                return glib::Propagation::Stop;
            }
            if keyval == gdk::Key::Return || keyval == gdk::Key::KP_Enter {
                panel.activate_focused();
                return glib::Propagation::Stop;
            }
            if keyval == gdk::Key::F2 {
                panel.show_rename_dialog();
                return glib::Propagation::Stop;
            }

            glib::Propagation::Proceed
        });
        self.root.add_controller(key);
    }

    fn move_focus(&self, delta: isize) {
        if self.model.borrow_mut().move_focus(delta) {
            self.sync_focus_classes();
            self.scroll_focused_row_into_view();
        }
    }

    fn expand_focused(&self) {
        if self.model.borrow_mut().expand_focused() {
            self.refresh();
        }
    }

    fn collapse_focused(&self) {
        if self.model.borrow_mut().collapse_focused() {
            self.refresh();
        }
    }

    fn activate_focused(&self) {
        match self.model.borrow_mut().activate_focused() {
            FileBrowserActivation::None => {}
            FileBrowserActivation::Refresh => self.refresh(),
            FileBrowserActivation::Open(path) => open_file(&path),
        }
    }

    fn focus_path(&self, path: PathBuf) {
        if self.model.borrow_mut().focus_path(&path) {
            self.root.grab_focus();
            self.sync_focus_classes();
            self.scroll_focused_row_into_view();
        }
    }

    fn activate_path(&self, path: PathBuf) {
        if self.model.borrow_mut().focus_path(&path) {
            self.root.grab_focus();
        }
        self.activate_focused();
    }

    fn copy_focused(&self) {
        if self.model.borrow_mut().copy_focused() {
            self.sync_focus_classes();
        }
    }

    fn cut_focused(&self) {
        if self.model.borrow_mut().cut_focused() {
            self.sync_focus_classes();
        }
    }

    fn paste_from_clipboard(&self) {
        let result = { self.model.borrow_mut().paste_from_clipboard() };
        match result {
            Ok(true) => self.refresh(),
            Ok(false) => {}
            Err(err) => self.show_status(&format!("Paste failed: {err}")),
        }
    }

    fn show_rename_dialog(&self) {
        let Some(path) = self.model.borrow_mut().focused_path() else {
            return;
        };

        let popup = gtk::Window::builder()
            .modal(true)
            .title("Rename")
            .default_width(360)
            .resizable(false)
            .build();
        if let Some(window) = self
            .root
            .root()
            .and_then(|root| root.downcast::<gtk::Window>().ok())
        {
            popup.set_transient_for(Some(&window));
        }

        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);

        let label = gtk::Label::new(Some("Name"));
        label.set_xalign(0.0);
        let entry = gtk::Entry::new();
        entry.set_text(&display_name(&path));
        entry.select_region(0, -1);
        let error = gtk::Label::new(None);
        error.add_css_class("error");
        error.set_xalign(0.0);
        error.set_wrap(true);
        error.set_visible(false);
        content.append(&label);
        content.append(&entry);
        content.append(&error);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        buttons.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let rename = gtk::Button::with_label("Rename");
        rename.add_css_class("suggested-action");
        buttons.append(&cancel);
        buttons.append(&rename);
        content.append(&buttons);
        popup.set_child(Some(&content));

        let popup_for_cancel = popup.clone();
        cancel.connect_clicked(move |_| popup_for_cancel.close());
        let panel = self.clone();
        let entry_for_rename = entry.clone();
        let error_for_rename = error.clone();
        let popup_for_rename = popup.clone();
        rename.connect_clicked(move |_| {
            let new_name = entry_for_rename.text().to_string();
            match panel.rename_focused_entry(&new_name) {
                Ok(()) => popup_for_rename.close(),
                Err(err) => {
                    error_for_rename.set_text(&format!("{err}"));
                    error_for_rename.set_visible(true);
                }
            }
        });

        let rename_for_entry = rename.clone();
        entry.connect_activate(move |_| rename_for_entry.emit_clicked());

        popup.present();
        entry.grab_focus();
    }

    fn rename_focused_entry(&self, new_name: &str) -> io::Result<()> {
        let result = { self.model.borrow_mut().rename_focused(new_name) };
        result.map(|_| self.refresh())
    }

    fn show_status(&self, message: &str) {
        self.status.set_text(message);
        self.status.set_visible(true);
    }

    fn sync_focus_classes(&self) {
        let rows = self.model.borrow().rows();
        let mut child = self.list.first_child();
        let mut idx = 0usize;
        while let Some(widget) = child {
            child = widget.next_sibling();
            let Ok(row_widget) = widget.downcast::<gtk::ListBoxRow>() else {
                continue;
            };
            if rows.get(idx).map(|row| row.focused).unwrap_or(false) {
                row_widget.add_css_class("focused");
            } else {
                row_widget.remove_css_class("focused");
            }
            if rows.get(idx).map(|row| row.cut).unwrap_or(false) {
                row_widget.add_css_class("cut");
            } else {
                row_widget.remove_css_class("cut");
            }
            idx += 1;
        }
    }

    fn scroll_focused_row_into_view(&self) {
        let Some(index) = self.model.borrow().focused_index() else {
            return;
        };
        let Some(row) = self.list.row_at_index(index as i32) else {
            return;
        };
        let Some(bounds) = row.compute_bounds(&self.list) else {
            return;
        };
        let adjustment = self.scroll.vadjustment();
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

    fn build_row(&self, row: &FileBrowserRow) -> gtk::ListBoxRow {
        let list_row = gtk::ListBoxRow::new();
        list_row.add_css_class("flowmux-file-browser-row");
        if row.focused {
            list_row.add_css_class("focused");
        }
        if row.cut {
            list_row.add_css_class("cut");
        }
        list_row.set_selectable(false);
        list_row.set_activatable(true);
        list_row.set_tooltip_text(Some(row.path.to_string_lossy().as_ref()));

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        content.set_margin_start(8 + (row.depth as i32 * 14));
        content.set_margin_end(8);
        content.set_margin_top(2);
        content.set_margin_bottom(2);

        let disclosure = if row.is_dir {
            let icon = if row.expanded {
                "pan-down-symbolic"
            } else {
                "pan-end-symbolic"
            };
            gtk::Image::from_icon_name(icon)
        } else {
            gtk::Image::new()
        };
        disclosure.set_pixel_size(12);
        disclosure.set_size_request(14, 14);

        let icon = if row.is_dir {
            gtk::Image::from_icon_name("folder-symbolic")
        } else {
            gtk::Image::from_icon_name("text-x-generic-symbolic")
        };
        icon.set_pixel_size(16);

        let label = gtk::Label::new(Some(&display_name(&row.path)));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);

        content.append(&disclosure);
        content.append(&icon);
        content.append(&label);
        list_row.set_child(Some(&content));

        let click = gtk::GestureClick::new();
        click.set_button(0);
        let panel = self.clone();
        let path = row.path.clone();
        let row_for_menu = list_row.clone();
        click.connect_pressed(
            move |gesture, n_press, x, y| match gesture.current_button() {
                gdk::BUTTON_PRIMARY => {
                    if n_press >= 2 {
                        panel.activate_path(path.clone());
                    } else {
                        panel.focus_path(path.clone());
                    }
                }
                gdk::BUTTON_SECONDARY => {
                    panel.focus_path(path.clone());
                    show_context_menu(&row_for_menu, &path, x, y);
                }
                _ => {}
            },
        );
        list_row.add_controller(click);

        list_row
    }
}

impl FileBrowserModel {
    fn set_root(&mut self, root: PathBuf) {
        let root = normalize_root(root);
        if self.root.as_ref() != Some(&root) {
            self.expanded.clear();
            self.focused = None;
        }
        self.root = Some(root);
    }

    fn sync_focus(&mut self) {
        let rows = self.visible_rows();
        if rows.is_empty() {
            self.focused = None;
            return;
        }

        if self
            .focused
            .as_ref()
            .is_none_or(|focused| !rows.iter().any(|row| row.path == *focused))
        {
            self.focused = rows.first().map(|row| row.path.clone());
        }
    }

    fn rows(&self) -> Vec<FileBrowserRow> {
        let focused = self.focused.as_ref();
        let cut_paths = self.cut_paths();
        self.visible_rows()
            .into_iter()
            .map(|mut row| {
                row.focused = focused == Some(&row.path);
                row.cut = cut_paths.contains(&row.path);
                row
            })
            .collect()
    }

    fn visible_rows(&self) -> Vec<FileBrowserRow> {
        let Some(root) = self.root.as_ref() else {
            return Vec::new();
        };

        let mut rows = Vec::new();
        self.collect_rows(root, 0, &mut rows);
        rows
    }

    fn collect_rows(&self, dir: &Path, depth: usize, rows: &mut Vec<FileBrowserRow>) {
        let Ok(entries) = read_dir_entries(dir) else {
            return;
        };

        for entry in entries {
            let expanded = entry.is_dir && self.expanded.contains(&entry.path);
            rows.push(FileBrowserRow {
                path: entry.path.clone(),
                is_dir: entry.is_dir,
                depth,
                expanded,
                focused: false,
                cut: false,
            });

            if expanded {
                self.collect_rows(&entry.path, depth + 1, rows);
            }
        }
    }

    fn focus_path(&mut self, path: &Path) -> bool {
        if self.visible_rows().iter().any(|row| row.path == path) {
            let changed = self.focused.as_deref() != Some(path);
            self.focused = Some(path.to_path_buf());
            changed
        } else {
            false
        }
    }

    fn move_focus(&mut self, delta: isize) -> bool {
        self.sync_focus();
        let rows = self.visible_rows();
        if rows.is_empty() {
            return false;
        }

        let current = self
            .focused
            .as_ref()
            .and_then(|focused| rows.iter().position(|row| row.path == *focused))
            .unwrap_or(0);
        let last = rows.len() - 1;
        let next = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(last)
        };

        let changed = current != next;
        self.focused = Some(rows[next].path.clone());
        changed
    }

    fn expand_focused(&mut self) -> bool {
        let Some(row) = self.focused_row() else {
            return false;
        };
        if !row.is_dir || row.expanded {
            return false;
        }
        self.expanded.insert(row.path);
        true
    }

    fn collapse_focused(&mut self) -> bool {
        let Some(row) = self.focused_row() else {
            return false;
        };
        if !row.is_dir || !row.expanded {
            return false;
        }
        self.expanded.remove(&row.path);
        true
    }

    fn activate_focused(&mut self) -> FileBrowserActivation {
        let Some(row) = self.focused_row() else {
            return FileBrowserActivation::None;
        };
        if !row.is_dir {
            return FileBrowserActivation::Open(row.path);
        }

        if row.expanded {
            self.expanded.remove(&row.path);
        } else {
            self.expanded.insert(row.path);
        }
        FileBrowserActivation::Refresh
    }

    fn focused_row(&mut self) -> Option<FileBrowserRow> {
        self.sync_focus();
        let focused = self.focused.as_ref()?;
        self.visible_rows()
            .into_iter()
            .find(|row| row.path == *focused)
    }

    fn focused_path(&mut self) -> Option<PathBuf> {
        self.focused_row().map(|row| row.path)
    }

    fn focused_index(&self) -> Option<usize> {
        let focused = self.focused.as_ref()?;
        self.visible_rows()
            .iter()
            .position(|row| row.path == *focused)
    }

    fn rename_focused(&mut self, new_name: &str) -> io::Result<PathBuf> {
        let old_path = self.focused_path().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no focused file browser row")
        })?;
        let name = valid_file_name(new_name)?;
        let parent = old_path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "focused path has no parent")
        })?;
        let new_path = parent.join(name);

        if old_path == new_path {
            return Ok(new_path);
        }
        if new_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", new_path.display()),
            ));
        }

        fs::rename(&old_path, &new_path)?;
        self.rewrite_tracked_paths(&old_path, &new_path);
        Ok(new_path)
    }

    fn rewrite_tracked_paths(&mut self, old_path: &Path, new_path: &Path) {
        self.focused = self
            .focused
            .as_ref()
            .map(|path| rewrite_path_prefix(path, old_path, new_path));
        self.expanded = self
            .expanded
            .iter()
            .map(|path| rewrite_path_prefix(path, old_path, new_path))
            .collect();
    }

    fn copy_focused(&mut self) -> bool {
        let Some(path) = self.focused_path() else {
            return false;
        };
        self.clipboard = Some(FileClipboard {
            operation: ClipboardOperation::Copy,
            paths: vec![path],
        });
        true
    }

    fn cut_focused(&mut self) -> bool {
        let Some(path) = self.focused_path() else {
            return false;
        };
        self.clipboard = Some(FileClipboard {
            operation: ClipboardOperation::Cut,
            paths: vec![path],
        });
        true
    }

    fn paste_from_clipboard(&mut self) -> io::Result<bool> {
        let Some(clipboard) = self.clipboard.clone() else {
            return Ok(false);
        };
        let Some(target_dir) = self.paste_target_dir() else {
            return Ok(false);
        };

        let mut pasted = Vec::new();
        for source in &clipboard.paths {
            if !source.exists() {
                continue;
            }
            if source.is_dir() && target_dir.starts_with(source) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cannot paste a directory into itself",
                ));
            }

            if clipboard.operation == ClipboardOperation::Cut
                && source.parent() == Some(target_dir.as_path())
            {
                pasted.push(source.clone());
                continue;
            }

            let destination = unique_destination(&target_dir, source)?;
            match clipboard.operation {
                ClipboardOperation::Copy => copy_path(source, &destination)?,
                ClipboardOperation::Cut => {
                    move_path(source, &destination)?;
                    self.rewrite_tracked_paths(source, &destination);
                }
            }
            pasted.push(destination);
        }

        if clipboard.operation == ClipboardOperation::Cut {
            self.clipboard = None;
        }
        if let Some(last) = pasted.last() {
            self.focused = Some(last.clone());
        }
        self.sync_focus();
        Ok(!pasted.is_empty())
    }

    fn paste_target_dir(&mut self) -> Option<PathBuf> {
        let row = self.focused_row()?;
        if row.is_dir {
            Some(row.path)
        } else {
            row.path.parent().map(Path::to_path_buf)
        }
    }

    fn cut_paths(&self) -> HashSet<PathBuf> {
        match &self.clipboard {
            Some(FileClipboard {
                operation: ClipboardOperation::Cut,
                paths,
            }) => paths.iter().cloned().collect(),
            _ => HashSet::new(),
        }
    }
}

fn read_dir_entries(dir: &Path) -> std::io::Result<Vec<FsEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false);
        entries.push(FsEntry { path, name, is_dir });
    }

    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    Ok(entries)
}

fn unique_destination(target_dir: &Path, source: &Path) -> io::Result<PathBuf> {
    let file_name = source.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no file name", source.display()),
        )
    })?;
    let candidate = target_dir.join(file_name);
    if !candidate.exists() {
        return Ok(candidate);
    }

    let stem = source
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| file_name.to_string_lossy().into_owned());
    let extension = source
        .extension()
        .map(|ext| ext.to_string_lossy().into_owned());

    for idx in 1.. {
        let suffix = if idx == 1 {
            " copy".to_string()
        } else {
            format!(" copy {idx}")
        };
        let name = match &extension {
            Some(ext) if source.is_file() => format!("{stem}{suffix}.{ext}"),
            _ => format!("{stem}{suffix}"),
        };
        let candidate = target_dir.join(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    unreachable!("unbounded suffix search must return before exhausting usize")
}

fn copy_path(source: &Path, destination: &Path) -> io::Result<()> {
    if source.is_dir() {
        fs::create_dir(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let child_source = entry.path();
            let child_destination = destination.join(entry.file_name());
            copy_path(&child_source, &child_destination)?;
        }
        Ok(())
    } else {
        fs::copy(source, destination).map(|_| ())
    }
}

fn move_path(source: &Path, destination: &Path) -> io::Result<()> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            copy_path(source, destination)?;
            let remove_result = if source.is_dir() {
                fs::remove_dir_all(source)
            } else {
                fs::remove_file(source)
            };
            remove_result.map_err(|remove_err| {
                io::Error::new(
                    remove_err.kind(),
                    format!(
                        "renamed by copy fallback but failed to remove {} after rename error {rename_err}: {remove_err}",
                        source.display()
                    ),
                )
            })
        }
    }
}

fn show_context_menu(parent: &impl IsA<gtk::Widget>, path: &Path, x: f64, y: f64) {
    let popover = gtk::Popover::new();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.set_margin_top(4);
    content.set_margin_bottom(4);

    let show = gtk::Button::with_label("Show in folder");
    show.add_css_class("flat");
    show.set_halign(gtk::Align::Fill);
    show.set_hexpand(true);
    let target = path.to_path_buf();
    let pop = popover.clone();
    show.connect_clicked(move |_| {
        show_path_in_folder(&target);
        pop.popdown();
    });
    content.append(&show);

    popover.set_child(Some(&content));
    popover.set_parent(parent);
    popover_pos::anchor_at_click(&popover, parent, x, y);
    popover.popup();
}

fn show_path_in_folder(path: &Path) {
    let dir = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    show_in_folder::open_directory(dir);
}

fn open_file(path: &Path) {
    let file = gio::File::for_path(path);
    let uri = file.uri();
    if let Err(err) = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
        tracing::warn!(path = %path.display(), error = %err, "failed to open file");
    }
}

fn key_to_focus_dir(key: gdk::Key) -> Option<FocusDir> {
    if key == gdk::Key::Left {
        Some(FocusDir::Left)
    } else if key == gdk::Key::Right {
        Some(FocusDir::Right)
    } else if key == gdk::Key::Up {
        Some(FocusDir::Up)
    } else if key == gdk::Key::Down {
        Some(FocusDir::Down)
    } else {
        None
    }
}

fn valid_file_name(name: &str) -> io::Result<&str> {
    use std::path::Component;

    let name = name.trim();
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(name),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "name must be a single file name",
        )),
    }
}

fn rewrite_path_prefix(path: &Path, old_path: &Path, new_path: &Path) -> PathBuf {
    if path == old_path {
        return new_path.to_path_buf();
    }
    path.strip_prefix(old_path)
        .map(|suffix| new_path.join(suffix))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn normalize_root(root: PathBuf) -> PathBuf {
    if root.is_dir() {
        root
    } else {
        root.parent().map(Path::to_path_buf).unwrap_or(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "flowmux-file-browser-{name}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn dir(&self, rel: &str) -> PathBuf {
            let path = self.path.join(rel);
            fs::create_dir_all(&path).unwrap();
            path
        }

        fn file(&self, rel: &str) -> PathBuf {
            let path = self.path.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, rel).unwrap();
            path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn row_names(model: &FileBrowserModel) -> Vec<String> {
        model
            .rows()
            .iter()
            .map(|row| display_name(&row.path))
            .collect()
    }

    fn row_paths(model: &FileBrowserModel) -> Vec<PathBuf> {
        model.rows().iter().map(|row| row.path.clone()).collect()
    }

    #[test]
    fn rows_are_sorted_with_directories_first() {
        let tmp = TestDir::new("sort");
        tmp.file("z.txt");
        tmp.dir("b-dir");
        tmp.file("a.txt");
        tmp.dir("a-dir");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());
        model.sync_focus();

        assert_eq!(row_names(&model), ["a-dir", "b-dir", "a.txt", "z.txt"]);
        assert_eq!(
            model.focused.as_ref(),
            Some(&tmp.path.join("a-dir")),
            "initial focus follows the first visible row"
        );
    }

    #[test]
    fn up_down_move_internal_focus_without_wrapping() {
        let tmp = TestDir::new("move");
        tmp.file("a.txt");
        tmp.file("b.txt");
        tmp.file("c.txt");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());

        assert!(model.move_focus(1));
        assert_eq!(model.focused.as_ref(), Some(&tmp.path.join("b.txt")));
        assert!(model.move_focus(1));
        assert_eq!(model.focused.as_ref(), Some(&tmp.path.join("c.txt")));
        assert!(!model.move_focus(1));
        assert_eq!(model.focused.as_ref(), Some(&tmp.path.join("c.txt")));
        assert!(model.move_focus(-2));
        assert_eq!(model.focused.as_ref(), Some(&tmp.path.join("a.txt")));
        assert!(!model.move_focus(-1));
    }

    #[test]
    fn right_left_and_enter_expand_or_collapse_focused_folder() {
        let tmp = TestDir::new("expand");
        tmp.dir("src");
        tmp.file("src/main.rs");
        tmp.file("top.txt");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());
        model.sync_focus();

        assert_eq!(row_names(&model), ["src", "top.txt"]);
        assert!(model.expand_focused());
        assert_eq!(row_names(&model), ["src", "main.rs", "top.txt"]);
        assert!(!model.expand_focused());
        assert!(model.collapse_focused());
        assert_eq!(row_names(&model), ["src", "top.txt"]);
        assert_eq!(model.activate_focused(), FileBrowserActivation::Refresh);
        assert_eq!(row_names(&model), ["src", "main.rs", "top.txt"]);
        assert_eq!(model.activate_focused(), FileBrowserActivation::Refresh);
        assert_eq!(row_names(&model), ["src", "top.txt"]);
    }

    #[test]
    fn enter_on_file_returns_open_action() {
        let tmp = TestDir::new("open");
        let file = tmp.file("a.txt");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());

        assert_eq!(model.activate_focused(), FileBrowserActivation::Open(file));
    }

    #[test]
    fn rename_focused_file_updates_rows_and_focus() {
        let tmp = TestDir::new("rename-file");
        tmp.file("old.txt");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());

        let renamed = model.rename_focused("new.txt").unwrap();

        assert_eq!(renamed, tmp.path.join("new.txt"));
        assert!(!tmp.path.join("old.txt").exists());
        assert!(tmp.path.join("new.txt").exists());
        assert_eq!(model.focused.as_ref(), Some(&tmp.path.join("new.txt")));
        assert_eq!(row_names(&model), ["new.txt"]);
    }

    #[test]
    fn rename_expanded_folder_preserves_expansion() {
        let tmp = TestDir::new("rename-dir");
        tmp.dir("old");
        tmp.file("old/main.rs");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());
        assert!(model.expand_focused());

        let renamed = model.rename_focused("new").unwrap();

        assert_eq!(renamed, tmp.path.join("new"));
        assert_eq!(model.focused.as_ref(), Some(&tmp.path.join("new")));
        assert!(model.expanded.contains(&tmp.path.join("new")));
        assert_eq!(row_names(&model), ["new", "main.rs"]);
    }

    #[test]
    fn rename_rejects_empty_or_nested_names() {
        let tmp = TestDir::new("rename-invalid");
        tmp.file("old.txt");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());

        assert_eq!(
            model.rename_focused("").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            model.rename_focused("nested/name.txt").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(tmp.path.join("old.txt").exists());
        assert_eq!(row_names(&model), ["old.txt"]);
    }

    #[test]
    fn copy_paste_file_creates_unique_sibling_and_focuses_it() {
        let tmp = TestDir::new("copy-file");
        tmp.file("a.txt");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());

        assert!(model.copy_focused());
        assert!(model.paste_from_clipboard().unwrap());

        let copied = tmp.path.join("a copy.txt");
        assert!(tmp.path.join("a.txt").exists());
        assert!(copied.exists());
        assert_eq!(model.focused.as_ref(), Some(&copied));
        assert!(row_paths(&model).contains(&copied));
    }

    #[test]
    fn copy_paste_folder_recursively() {
        let tmp = TestDir::new("copy-dir");
        tmp.dir("src");
        tmp.file("src/main.rs");
        let target = tmp.file("target.txt");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());
        assert!(model.focus_path(&tmp.path.join("src")));
        assert!(model.copy_focused());
        assert!(model.focus_path(&target));

        assert!(model.paste_from_clipboard().unwrap());

        assert!(tmp.path.join("src/main.rs").exists());
        assert!(tmp.path.join("src copy/main.rs").exists());
        assert_eq!(model.focused.as_ref(), Some(&tmp.path.join("src copy")));
    }

    #[test]
    fn cut_marks_row_and_paste_moves_file() {
        let tmp = TestDir::new("cut-file");
        let file = tmp.file("a.txt");
        let dest = tmp.dir("dest");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());
        assert!(model.focus_path(&file));

        assert!(model.cut_focused());
        let rows = model.rows();
        assert!(rows.iter().any(|row| row.path == file && row.cut));

        assert!(model.focus_path(&dest));
        assert!(model.paste_from_clipboard().unwrap());

        assert!(!file.exists());
        assert!(dest.join("a.txt").exists());
        assert!(model.clipboard.is_none());
        assert!(model.rows().iter().all(|row| !row.cut));
    }

    #[test]
    fn paste_into_expanded_folder_updates_visible_rows() {
        let tmp = TestDir::new("paste-expanded");
        let dest = tmp.dir("dest");
        let file = tmp.file("a.txt");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());
        assert!(model.focus_path(&dest));
        assert!(model.expand_focused());
        assert!(model.focus_path(&file));
        assert!(model.copy_focused());
        assert!(model.focus_path(&dest));

        assert!(model.paste_from_clipboard().unwrap());

        let pasted = dest.join("a.txt");
        assert!(pasted.exists());
        assert!(row_paths(&model).contains(&pasted));
    }

    #[gtk::test]
    fn panel_rename_refreshes_after_model_borrow_drops() {
        let tmp = TestDir::new("panel-rename");
        tmp.file("old.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());

        panel.rename_focused_entry("new.txt").unwrap();

        assert!(!tmp.path.join("old.txt").exists());
        assert!(tmp.path.join("new.txt").exists());
        assert_eq!(
            panel.model.borrow().focused.as_ref(),
            Some(&tmp.path.join("new.txt"))
        );
    }

    #[gtk::test]
    fn panel_paste_refreshes_after_model_borrow_drops() {
        let tmp = TestDir::new("panel-paste");
        tmp.file("a.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.copy_focused();

        panel.paste_from_clipboard();

        assert!(tmp.path.join("a copy.txt").exists());
        assert_eq!(
            panel.model.borrow().focused.as_ref(),
            Some(&tmp.path.join("a copy.txt"))
        );
    }
}
