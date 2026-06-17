// SPDX-License-Identifier: GPL-3.0-or-later

//! Right-side Finder-style file browser for the focused pane's cwd.

use crate::ui::popover_pos;
use crate::ui::show_in_folder;
use gtk::gio;
use gtk::prelude::*;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

#[derive(Clone)]
pub struct FileBrowserPanel {
    root: gtk::Box,
    path_label: gtk::Label,
    list: gtk::ListBox,
    status: gtk::Label,
    close_button: gtk::Button,
    root_dir: Rc<RefCell<Option<PathBuf>>>,
    expanded: Rc<RefCell<HashSet<PathBuf>>>,
    selected: Rc<RefCell<Option<PathBuf>>>,
    rows: Rc<RefCell<Vec<FileBrowserRow>>>,
}

#[derive(Clone)]
struct FileBrowserRow {
    path: PathBuf,
    is_dir: bool,
    depth: usize,
    expanded: bool,
}

#[derive(Clone)]
struct FsEntry {
    path: PathBuf,
    name: String,
    is_dir: bool,
}

impl FileBrowserPanel {
    pub fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("flowmux-file-browser");
        root.set_size_request(300, -1);
        root.set_hexpand(false);
        root.set_vexpand(true);
        root.set_visible(false);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.add_css_class("flowmux-file-browser-header");

        let title = gtk::Label::new(Some("Files"));
        title.add_css_class("heading");
        title.set_xalign(0.0);

        let path_label = gtk::Label::new(None);
        path_label.add_css_class("dim-label");
        path_label.set_xalign(0.0);
        path_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        path_label.set_hexpand(true);

        let title_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        title_box.set_hexpand(true);
        title_box.append(&title);
        title_box.append(&path_label);

        let close_button = gtk::Button::from_icon_name("window-close-symbolic");
        close_button.add_css_class("flat");
        close_button.set_tooltip_text(Some("Close FileBrowser"));
        close_button.set_focus_on_click(false);

        header.append(&title_box);
        header.append(&close_button);

        let list = gtk::ListBox::new();
        list.add_css_class("flowmux-file-browser-list");
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.set_activate_on_single_click(false);

        let status = gtk::Label::new(Some("No focused directory"));
        status.add_css_class("dim-label");
        status.set_margin_top(16);
        status.set_margin_start(12);
        status.set_margin_end(12);
        status.set_wrap(true);
        status.set_xalign(0.0);

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hexpand(true);
        scroll.set_vexpand(true);
        scroll.set_child(Some(&list));

        root.append(&header);
        root.append(&status);
        root.append(&scroll);

        let panel = Self {
            root,
            path_label,
            list,
            status,
            close_button,
            root_dir: Rc::new(RefCell::new(None)),
            expanded: Rc::new(RefCell::new(HashSet::new())),
            selected: Rc::new(RefCell::new(None)),
            rows: Rc::new(RefCell::new(Vec::new())),
        };

        {
            let panel = panel.clone();
            let list = panel.list.clone();
            list.connect_row_selected(move |_, row| {
                let Some(row) = row else {
                    *panel.selected.borrow_mut() = None;
                    return;
                };
                let index = row.index();
                let selected = panel
                    .rows
                    .borrow()
                    .get(index.max(0) as usize)
                    .map(|row| row.path.clone());
                *panel.selected.borrow_mut() = selected;
            });
        }

        {
            let panel = panel.clone();
            let list = panel.list.clone();
            list.connect_row_activated(move |_, row| {
                panel.activate_row(row.index());
            });
        }

        panel
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn connect_close<F: Fn() + 'static>(&self, f: F) {
        self.close_button.connect_clicked(move |_| f());
    }

    pub fn show_for_root(&self, root: PathBuf) {
        let root = normalize_root(root);
        let changed = self.root_dir.borrow().as_ref() != Some(&root);
        if changed {
            self.expanded.borrow_mut().clear();
            *self.selected.borrow_mut() = None;
        }
        *self.root_dir.borrow_mut() = Some(root);
        self.root.set_visible(true);
        self.refresh();
    }

    pub fn hide(&self) {
        self.root.set_visible(false);
    }

    pub fn refresh(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        self.rows.borrow_mut().clear();

        let Some(root) = self.root_dir.borrow().clone() else {
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

        let mut rows = Vec::new();
        self.collect_rows(&root, 0, &mut rows);

        if rows.is_empty() {
            self.status.set_text("Directory is empty");
            self.status.set_visible(true);
            return;
        }

        self.status.set_visible(false);
        *self.rows.borrow_mut() = rows.clone();

        for row in rows {
            self.list.append(&self.build_row(&row));
        }
    }

    fn activate_row(&self, index: i32) {
        let Some(row) = self.rows.borrow().get(index.max(0) as usize).cloned() else {
            return;
        };

        if row.is_dir {
            let mut expanded = self.expanded.borrow_mut();
            if expanded.contains(&row.path) {
                expanded.remove(&row.path);
            } else {
                expanded.insert(row.path);
            }
            drop(expanded);
            self.refresh();
        } else {
            open_file(&row.path);
        }
    }

    fn collect_rows(&self, dir: &Path, depth: usize, rows: &mut Vec<FileBrowserRow>) {
        let Ok(entries) = read_dir_entries(dir) else {
            return;
        };

        for entry in entries {
            let is_expanded = entry.is_dir && self.expanded.borrow().contains(&entry.path);
            rows.push(FileBrowserRow {
                path: entry.path.clone(),
                is_dir: entry.is_dir,
                depth,
                expanded: is_expanded,
            });

            if is_expanded {
                self.collect_rows(&entry.path, depth + 1, rows);
            }
        }
    }

    fn build_row(&self, row: &FileBrowserRow) -> gtk::ListBoxRow {
        let list_row = gtk::ListBoxRow::new();
        list_row.add_css_class("flowmux-file-browser-row");
        list_row.set_selectable(true);
        list_row.set_activatable(true);
        list_row.set_tooltip_text(Some(row.path.to_string_lossy().as_ref()));

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        content.set_margin_top(2);
        content.set_margin_bottom(2);
        content.set_margin_start(8 + (row.depth as i32 * 14));
        content.set_margin_end(8);

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
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_hexpand(true);

        content.append(&disclosure);
        content.append(&icon);
        content.append(&label);
        list_row.set_child(Some(&content));

        let click = gtk::GestureClick::new();
        click.set_button(gtk::gdk::BUTTON_SECONDARY);
        let path = row.path.clone();
        let row_for_menu = list_row.clone();
        click.connect_pressed(move |gesture, _n_press, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            show_context_menu(&row_for_menu, &path, x, y);
        });
        list_row.add_controller(click);

        list_row
    }
}

fn read_dir_entries(dir: &Path) -> std::io::Result<Vec<FsEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
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

fn show_context_menu(parent: &gtk::ListBoxRow, path: &Path, x: f64, y: f64) {
    let popover = gtk::Popover::new();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.set_margin_top(4);
    content.set_margin_bottom(4);

    let show = gtk::Button::with_label("Show in folder");
    show.add_css_class("flat");
    show.set_halign(gtk::Align::Fill);
    show.set_hexpand(true);
    if let Some(label) = show.child().and_then(|w| w.downcast::<gtk::Label>().ok()) {
        label.set_xalign(0.0);
    }

    let target = path.to_path_buf();
    let pop = popover.clone();
    show.connect_clicked(move |_| {
        pop.popdown();
        show_path_in_folder(&target);
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
