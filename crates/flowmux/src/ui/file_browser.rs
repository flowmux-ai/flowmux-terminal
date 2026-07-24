// SPDX-License-Identifier: GPL-3.0-or-later

//! Right-side Finder-style file browser for the focused pane's cwd.

use crate::bridge::FocusDir;
use crate::ui::popover_pos;
use crate::ui::show_in_folder;
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Instant;

type DeleteHandler = Arc<dyn Fn(&Path, &FileOperationControl) -> io::Result<()> + Send + Sync>;
type OpenFileHandler = Rc<RefCell<Box<dyn Fn(PathBuf)>>>;
type KeyboardInputGuard = Rc<RefCell<Box<dyn Fn() -> bool>>>;
const ROW_BATCH_SIZE: usize = 500;

#[cfg(any(target_os = "macos", test))]
pub(crate) fn file_browser_accepts_keyboard_input(editor_has_native_focus: bool) -> bool {
    !editor_has_native_focus
}

#[derive(Clone)]
pub struct FileBrowserPanel {
    root: gtk::Box,
    path_label: gtk::Label,
    list: gtk::ListBox,
    scroll: gtk::ScrolledWindow,
    status: gtk::Label,
    load_more_button: gtk::Button,
    cancel_operation_button: gtk::Button,
    close_button: gtk::Button,
    model: Rc<RefCell<FileBrowserModel>>,
    delete_handler: Rc<RefCell<DeleteHandler>>,
    path_clipboard_writer: Rc<RefCell<Box<dyn Fn(&str)>>>,
    on_open_file: OpenFileHandler,
    on_focus_out: Rc<RefCell<Option<Box<dyn Fn(FocusDir)>>>>,
    on_escape: Rc<RefCell<Option<Box<dyn Fn()>>>>,
    on_focus_changed: Rc<RefCell<Option<Box<dyn Fn(bool)>>>>,
    keyboard_input_guard: KeyboardInputGuard,
    open: Rc<Cell<bool>>,
    file_operation_in_progress: Rc<Cell<bool>>,
    current_file_operation_cancel: Rc<RefCell<Option<Arc<AtomicBool>>>>,
    #[cfg_attr(test, allow(dead_code))]
    directory_generation: Rc<Cell<u64>>,
    directory_error: Rc<RefCell<Option<String>>>,
    directory_warnings: Rc<RefCell<HashMap<PathBuf, String>>>,
    directory_monitors: Rc<RefCell<Vec<gio::FileMonitor>>>,
    monitored_directories: Rc<RefCell<HashSet<PathBuf>>>,
    monitor_epoch: Rc<Cell<u64>>,
    #[cfg_attr(test, allow(dead_code))]
    monitor_refresh_scheduled: Rc<Cell<bool>>,
    #[cfg_attr(test, allow(dead_code))]
    monitor_reload_in_progress: Rc<Cell<bool>>,
    pending_monitor_refreshes: Rc<RefCell<HashSet<PathBuf>>>,
    visible_row_limit: Rc<Cell<usize>>,
    rendered_rows: Rc<RefCell<Vec<(FileBrowserRow, gtk::ListBoxRow)>>>,
    #[cfg(all(test, not(target_os = "macos")))]
    rebuild_count: Rc<Cell<usize>>,
    #[cfg(all(test, not(target_os = "macos")))]
    row_build_count: Rc<Cell<usize>>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct FileBrowserPaneState {
    pub(crate) root: Option<PathBuf>,
    pub(crate) expanded: HashSet<PathBuf>,
    pub(crate) focused: Option<PathBuf>,
    pub(crate) selected: HashSet<PathBuf>,
    pub(crate) selection_anchor: Option<PathBuf>,
    pub(crate) scroll_value: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileBrowserRow {
    path: PathBuf,
    is_dir: bool,
    icon_name: Option<String>,
    depth: usize,
    expanded: bool,
    focused: bool,
    selected: bool,
    cut: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum RowReconcileAction {
    Move { from: usize, to: usize },
    Insert { at: usize, row: FileBrowserRow },
    Remove { at: usize },
}

fn row_reconciliation_actions(
    current: &[FileBrowserRow],
    desired: &[FileBrowserRow],
) -> Vec<RowReconcileAction> {
    let mut working = current.to_vec();
    let mut actions = Vec::new();
    for (index, desired_row) in desired.iter().enumerate() {
        if working.get(index) == Some(desired_row) {
            continue;
        }
        if let Some(existing) = working
            .iter()
            .enumerate()
            .skip(index + 1)
            .find_map(|(position, current)| (current == desired_row).then_some(position))
        {
            let row = working.remove(existing);
            working.insert(index, row);
            actions.push(RowReconcileAction::Move {
                from: existing,
                to: index,
            });
        } else {
            working.insert(index, desired_row.clone());
            actions.push(RowReconcileAction::Insert {
                at: index,
                row: desired_row.clone(),
            });
        }
    }
    while working.len() > desired.len() {
        let at = working.len() - 1;
        working.pop();
        actions.push(RowReconcileAction::Remove { at });
    }
    actions
}

#[derive(Debug, Clone)]
struct FsEntry {
    path: PathBuf,
    name: String,
    is_dir: bool,
    icon_name: Option<String>,
}

#[derive(Debug)]
struct DirectorySnapshot {
    entries_by_dir: HashMap<PathBuf, Vec<FsEntry>>,
    warnings: HashMap<PathBuf, String>,
}

#[derive(Debug, Default)]
struct FileBrowserModel {
    root: Option<PathBuf>,
    expanded: HashSet<PathBuf>,
    focused: Option<PathBuf>,
    selected: HashSet<PathBuf>,
    selection_anchor: Option<PathBuf>,
    clipboard: Option<FileClipboard>,
    entries_by_dir: HashMap<PathBuf, Vec<FsEntry>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileBrowserActivation {
    None,
    Refresh,
    Open(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileOpenTarget {
    Editor,
    ImageViewer,
    Binary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrimaryClickAction {
    Focus,
    Activate,
    ToggleSelection,
    ExtendSelection,
}

fn primary_click_action(n_press: i32, state: gdk::ModifierType) -> PrimaryClickAction {
    if state.contains(gdk::ModifierType::CONTROL_MASK) {
        PrimaryClickAction::ToggleSelection
    } else if state.contains(gdk::ModifierType::SHIFT_MASK) {
        PrimaryClickAction::ExtendSelection
    } else if n_press >= 2 {
        PrimaryClickAction::Activate
    } else {
        PrimaryClickAction::Focus
    }
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

struct PasteRequest {
    clipboard: FileClipboard,
    target_dir: PathBuf,
}

#[derive(Debug)]
struct PasteOutcome {
    pasted: Vec<PathBuf>,
    moved: Vec<(PathBuf, PathBuf)>,
    clipboard_after: Option<FileClipboard>,
    error: Option<io::Error>,
    cancelled: bool,
}

struct DeleteOutcome {
    deleted: Vec<PathBuf>,
    failed: Vec<(PathBuf, io::Error)>,
    cancelled: bool,
}

#[derive(Clone)]
struct FileOperationControl {
    cancelled: Arc<AtomicBool>,
    completed: Arc<AtomicUsize>,
    total: usize,
}

impl FileOperationControl {
    fn new(total: usize) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            completed: Arc::new(AtomicUsize::new(0)),
            total,
        }
    }

    fn check_cancelled(&self) -> io::Result<()> {
        if self.cancelled.load(AtomicOrdering::Relaxed) {
            Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
        } else {
            Ok(())
        }
    }

    fn mark_completed(&self) {
        self.completed.fetch_add(1, AtomicOrdering::Relaxed);
    }
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

        let cancel_operation_button = gtk::Button::with_label("Cancel");
        cancel_operation_button.add_css_class("flat");
        cancel_operation_button.set_visible(false);
        cancel_operation_button.set_tooltip_text(Some("Cancel file operation"));

        header.append(&title_box);
        header.append(&cancel_operation_button);
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

        let load_more_button = gtk::Button::with_label("Show 500 more");
        load_more_button.add_css_class("flat");
        load_more_button.set_margin_top(6);
        load_more_button.set_margin_bottom(6);
        load_more_button.set_margin_start(16);
        load_more_button.set_margin_end(16);
        load_more_button.set_visible(false);

        let scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .hexpand(true)
            .vexpand(true)
            .build();
        root.append(&scroll);
        root.append(&load_more_button);
        root.append(&status);

        let clipboard_root = root.clone();
        let panel = Self {
            root,
            path_label,
            list,
            scroll,
            status,
            load_more_button,
            cancel_operation_button,
            close_button,
            model: Rc::new(RefCell::new(FileBrowserModel::default())),
            delete_handler: Rc::new(RefCell::new(Arc::new(move_to_trash))),
            path_clipboard_writer: Rc::new(RefCell::new(Box::new(move |path| {
                clipboard_root.clipboard().set_text(path);
            }))),
            on_open_file: Rc::new(RefCell::new(Box::new(|path| open_file(&path)))),
            on_focus_out: Rc::new(RefCell::new(None)),
            on_escape: Rc::new(RefCell::new(None)),
            on_focus_changed: Rc::new(RefCell::new(None)),
            keyboard_input_guard: Rc::new(RefCell::new(Box::new(|| true))),
            open: Rc::new(Cell::new(false)),
            file_operation_in_progress: Rc::new(Cell::new(false)),
            current_file_operation_cancel: Rc::new(RefCell::new(None)),
            directory_generation: Rc::new(Cell::new(0)),
            directory_error: Rc::new(RefCell::new(None)),
            directory_warnings: Rc::new(RefCell::new(HashMap::new())),
            directory_monitors: Rc::new(RefCell::new(Vec::new())),
            monitored_directories: Rc::new(RefCell::new(HashSet::new())),
            monitor_epoch: Rc::new(Cell::new(0)),
            monitor_refresh_scheduled: Rc::new(Cell::new(false)),
            monitor_reload_in_progress: Rc::new(Cell::new(false)),
            pending_monitor_refreshes: Rc::new(RefCell::new(HashSet::new())),
            visible_row_limit: Rc::new(Cell::new(ROW_BATCH_SIZE)),
            rendered_rows: Rc::new(RefCell::new(Vec::new())),
            #[cfg(all(test, not(target_os = "macos")))]
            rebuild_count: Rc::new(Cell::new(0)),
            #[cfg(all(test, not(target_os = "macos")))]
            row_build_count: Rc::new(Cell::new(0)),
        };

        panel.install_focus_style();
        panel.install_pointer_focus();
        panel.install_keyboard();
        let panel_for_more = panel.clone();
        panel.load_more_button.connect_clicked(move |_| {
            panel_for_more.show_more_rows();
        });
        let cancel = panel.current_file_operation_cancel.clone();
        let status_for_cancel = panel.status.clone();
        panel.cancel_operation_button.connect_clicked(move |_| {
            if let Some(cancel) = cancel.borrow().as_ref() {
                cancel.store(true, AtomicOrdering::Relaxed);
                status_for_cancel.set_text("Cancelling…");
            }
        });

        panel
    }

    fn begin_file_operation(
        &self,
        label: &'static str,
        total: usize,
    ) -> Option<FileOperationControl> {
        if self.file_operation_in_progress.replace(true) {
            return None;
        }
        let control = FileOperationControl::new(total);
        *self.current_file_operation_cancel.borrow_mut() = Some(control.cancelled.clone());
        self.cancel_operation_button.set_visible(true);
        self.status.set_text(&format!("{label}… 0/{total}"));
        self.status.set_visible(true);

        let panel = self.clone();
        let progress = control.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            if !panel.file_operation_in_progress.get() {
                return glib::ControlFlow::Break;
            }
            if !progress.cancelled.load(AtomicOrdering::Relaxed) {
                panel.status.set_text(&format!(
                    "{label}… {}/{}",
                    progress.completed.load(AtomicOrdering::Relaxed),
                    progress.total
                ));
            }
            glib::ControlFlow::Continue
        });
        Some(control)
    }

    fn finish_file_operation(&self) {
        self.file_operation_in_progress.set(false);
        self.current_file_operation_cancel.borrow_mut().take();
        self.cancel_operation_button.set_visible(false);
    }

    fn cancel_current_file_operation(&self) {
        if let Some(cancel) = self.current_file_operation_cancel.borrow().as_ref() {
            cancel.store(true, AtomicOrdering::Relaxed);
        }
    }

    fn operation_context_is_active(&self, root: &Path) -> bool {
        self.open.get() && self.model.borrow().root.as_deref() == Some(root)
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

    pub fn connect_escape<F: Fn() + 'static>(&self, f: F) {
        *self.on_escape.borrow_mut() = Some(Box::new(f));
    }

    /// Notified with `true` when keyboard focus enters the file browser (or any
    /// descendant) and `false` when it leaves. Drives the controller's notion of
    /// whether the browser actually holds focus, which disambiguates Alt+arrow.
    pub fn connect_focus_changed<F: Fn(bool) + 'static>(&self, f: F) {
        *self.on_focus_changed.borrow_mut() = Some(Box::new(f));
    }

    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn set_keyboard_input_guard<F: Fn() -> bool + 'static>(&self, guard: F) {
        *self.keyboard_input_guard.borrow_mut() = Box::new(guard);
    }

    pub fn connect_open_file<F: Fn(PathBuf) + 'static>(&self, f: F) {
        *self.on_open_file.borrow_mut() = Box::new(f);
    }

    #[cfg(all(test, not(target_os = "macos")))]
    fn set_delete_handler<F: Fn(&Path) -> io::Result<()> + Send + Sync + 'static>(&self, f: F) {
        *self.delete_handler.borrow_mut() = Arc::new(move |path, _| f(path));
    }

    #[cfg(all(test, not(target_os = "macos")))]
    fn set_path_clipboard_writer<F: Fn(&str) + 'static>(&self, f: F) {
        *self.path_clipboard_writer.borrow_mut() = Box::new(f);
    }

    #[cfg(all(test, not(target_os = "macos")))]
    fn show_for_root(&self, root: PathBuf) {
        if !self.is_showing_root(&root) {
            self.cancel_current_file_operation();
        }
        self.model.borrow_mut().set_root(root.clone());
        self.open.set(true);
        self.root.set_visible(true);
        self.refresh_reset_scroll();
    }

    pub(crate) fn show_for_root_with_state(
        &self,
        root: PathBuf,
        state: Option<FileBrowserPaneState>,
    ) {
        if !self.is_showing_root(&root) {
            self.cancel_current_file_operation();
            self.clear_directory_monitors();
            self.visible_row_limit.set(ROW_BATCH_SIZE);
        }
        let scroll_value = self.model.borrow_mut().set_root_with_state(root, state);
        self.open.set(true);
        self.root.set_visible(true);
        self.refresh_with_scroll_value(scroll_value);
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open.get()
    }

    pub(crate) fn is_showing_root(&self, root: &Path) -> bool {
        let root = normalize_root(root.to_path_buf());
        self.model.borrow().root.as_ref() == Some(&root)
    }

    #[cfg(all(test, not(target_os = "macos")))]
    pub(crate) fn rebuild_count(&self) -> usize {
        self.rebuild_count.get()
    }

    pub(crate) fn pane_state(&self) -> FileBrowserPaneState {
        let model = self.model.borrow();
        FileBrowserPaneState {
            root: model.root.clone(),
            expanded: model.expanded.clone(),
            focused: model.focused.clone(),
            selected: model.selected.clone(),
            selection_anchor: model.selection_anchor.clone(),
            scroll_value: self.scroll.vadjustment().value(),
        }
    }

    pub fn hide(&self) {
        self.cancel_current_file_operation();
        self.open.set(false);
        self.directory_generation
            .set(self.directory_generation.get().wrapping_add(1));
        self.clear_directory_monitors();
        self.root.set_visible(false);
    }

    pub fn grab_focus(&self) {
        self.root.grab_focus();
    }

    pub fn refresh(&self) {
        let scroll_value = self.scroll.vadjustment().value();
        self.refresh_with_scroll_value(scroll_value);
    }

    fn refresh_with_scroll_value(&self, scroll_value: f64) {
        #[cfg(test)]
        {
            let result = self.model.borrow_mut().reload_entries_sync();
            *self.directory_error.borrow_mut() = result.err().map(|error| error.to_string());
            self.rebuild_rows();
            self.restore_scroll_value(scroll_value);
        }
        #[cfg(not(test))]
        self.reload_directories(scroll_value);
    }

    #[cfg(not(test))]
    fn reload_directories(&self, scroll_value: f64) {
        let (root, expanded) = {
            let model = self.model.borrow();
            (model.root.clone(), model.expanded.clone())
        };
        let Some(root) = root else {
            self.rebuild_rows();
            return;
        };
        let generation = self.directory_generation.get().wrapping_add(1);
        self.directory_generation.set(generation);
        self.directory_error.borrow_mut().take();
        self.status.set_text("Loading…");
        self.status.set_visible(true);

        let panel = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let started = Instant::now();
            let root_for_worker = root.clone();
            let loaded =
                gio::spawn_blocking(move || load_directory_snapshot(&root_for_worker, &expanded))
                    .await;
            if panel.directory_generation.get() != generation
                || panel.model.borrow().root.as_ref() != Some(&root)
            {
                return;
            }

            match loaded {
                Ok(Ok(snapshot)) => {
                    let directory_count = snapshot.entries_by_dir.len();
                    let entry_count = snapshot
                        .entries_by_dir
                        .values()
                        .map(Vec::len)
                        .sum::<usize>();
                    let monitored_directories =
                        snapshot.entries_by_dir.keys().cloned().collect::<Vec<_>>();
                    panel.model.borrow_mut().entries_by_dir = snapshot.entries_by_dir;
                    *panel.directory_warnings.borrow_mut() = snapshot.warnings;
                    panel.directory_error.borrow_mut().take();
                    panel.install_directory_monitors(monitored_directories);
                    tracing::info!(
                        operation = "file_browser_read_dir",
                        directory_count,
                        entry_count,
                        elapsed_ms = started.elapsed().as_millis(),
                        "file browser directory snapshot loaded"
                    );
                }
                Ok(Err(error)) => {
                    tracing::warn!(
                        operation = "file_browser_read_dir",
                        %error,
                        elapsed_ms = started.elapsed().as_millis(),
                        "file browser directory snapshot failed"
                    );
                    panel.model.borrow_mut().entries_by_dir.clear();
                    panel.directory_warnings.borrow_mut().clear();
                    panel.clear_directory_monitors();
                    *panel.directory_error.borrow_mut() = Some(error.to_string());
                }
                Err(_) => {
                    panel.model.borrow_mut().entries_by_dir.clear();
                    panel.directory_warnings.borrow_mut().clear();
                    panel.clear_directory_monitors();
                    *panel.directory_error.borrow_mut() =
                        Some("Directory worker panicked".to_string());
                }
            }
            panel.rebuild_rows();
            panel.restore_scroll_value(scroll_value);
        });
    }

    #[cfg_attr(test, allow(dead_code))]
    fn install_directory_monitors(&self, directories: Vec<PathBuf>) {
        let directories = directories.into_iter().collect::<HashSet<_>>();
        if *self.monitored_directories.borrow() == directories {
            return;
        }
        self.clear_directory_monitors();
        let mut monitors = Vec::new();
        for directory in &directories {
            let file = gio::File::for_path(directory);
            let Ok(monitor) =
                file.monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
            else {
                tracing::debug!(directory = %directory.display(), "file browser monitor unavailable");
                continue;
            };
            let panel = self.clone();
            let directory = directory.clone();
            monitor.connect_changed(move |_, _, _, event| {
                if !matches!(
                    event,
                    gio::FileMonitorEvent::Created
                        | gio::FileMonitorEvent::Deleted
                        | gio::FileMonitorEvent::Moved
                        | gio::FileMonitorEvent::Renamed
                        | gio::FileMonitorEvent::MovedIn
                        | gio::FileMonitorEvent::MovedOut
                ) {
                    return;
                }
                panel.queue_monitored_directory_refresh(directory.clone());
            });
            monitors.push(monitor);
        }
        *self.directory_monitors.borrow_mut() = monitors;
        *self.monitored_directories.borrow_mut() = directories;
    }

    #[cfg_attr(test, allow(dead_code))]
    fn queue_monitored_directory_refresh(&self, directory: PathBuf) {
        if !self.open.get() {
            return;
        }
        self.pending_monitor_refreshes
            .borrow_mut()
            .insert(directory);
        self.schedule_pending_monitor_refresh();
    }

    #[cfg_attr(test, allow(dead_code))]
    fn schedule_pending_monitor_refresh(&self) {
        if self.pending_monitor_refreshes.borrow().is_empty()
            || self.monitor_reload_in_progress.get()
            || self.monitor_refresh_scheduled.replace(true)
        {
            return;
        }
        let epoch = self.monitor_epoch.get();
        let panel = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
            if panel.monitor_epoch.get() != epoch {
                return;
            }
            panel.monitor_refresh_scheduled.set(false);
            if panel.open.get() {
                panel.reload_monitored_directories();
            }
        });
    }

    #[cfg_attr(test, allow(dead_code))]
    fn reload_monitored_directories(&self) {
        if self.monitor_reload_in_progress.replace(true) {
            return;
        }
        let directories = self
            .pending_monitor_refreshes
            .borrow_mut()
            .drain()
            .collect::<Vec<_>>();
        let root = self.model.borrow().root.clone();
        let generation = self.directory_generation.get();
        let epoch = self.monitor_epoch.get();
        let scroll_value = self.scroll.vadjustment().value();
        let panel = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let loaded = gio::spawn_blocking(move || {
                directories
                    .into_iter()
                    .map(|directory| {
                        let result = read_dir_entries(&directory);
                        (directory, result)
                    })
                    .collect::<Vec<_>>()
            })
            .await;

            if let Ok(results) = loaded {
                if panel.open.get()
                    && panel.directory_generation.get() == generation
                    && panel.monitor_epoch.get() == epoch
                    && panel.model.borrow().root == root
                {
                    let root = root.as_deref();
                    let mut model = panel.model.borrow_mut();
                    let mut warnings = panel.directory_warnings.borrow_mut();
                    for (directory, result) in results {
                        match result {
                            Ok(entries) => {
                                model.entries_by_dir.insert(directory.clone(), entries);
                                warnings.remove(&directory);
                                if root == Some(directory.as_path()) {
                                    panel.directory_error.borrow_mut().take();
                                }
                            }
                            Err(error) if root == Some(directory.as_path()) => {
                                model.entries_by_dir.remove(&directory);
                                *panel.directory_error.borrow_mut() = Some(error.to_string());
                            }
                            Err(error) => {
                                model.entries_by_dir.insert(directory.clone(), Vec::new());
                                warnings.insert(directory, error.to_string());
                            }
                        }
                    }
                    drop(warnings);
                    drop(model);
                    panel.rebuild_rows();
                    panel.restore_scroll_value(scroll_value);
                }
            }

            panel.monitor_reload_in_progress.set(false);
            panel.schedule_pending_monitor_refresh();
        });
    }

    fn clear_directory_monitors(&self) {
        self.monitor_epoch
            .set(self.monitor_epoch.get().wrapping_add(1));
        self.monitor_refresh_scheduled.set(false);
        self.pending_monitor_refreshes.borrow_mut().clear();
        self.directory_monitors.borrow_mut().clear();
        self.monitored_directories.borrow_mut().clear();
    }

    #[cfg(all(test, not(target_os = "macos")))]
    fn refresh_reset_scroll(&self) {
        self.rebuild_rows();
        self.restore_scroll_value(0.0);
    }

    fn rebuild_rows(&self) {
        #[cfg(all(test, not(target_os = "macos")))]
        self.rebuild_count.set(self.rebuild_count.get() + 1);

        self.load_more_button.set_visible(false);

        let root = {
            let model = self.model.borrow();
            model.root.clone()
        };

        let Some(root) = root else {
            self.reconcile_rows(Vec::new());
            self.path_label.set_text("");
            self.status.set_text("No focused directory");
            self.status.set_visible(true);
            return;
        };

        self.path_label.set_text(&root.to_string_lossy());
        self.path_label
            .set_tooltip_text(Some(root.to_string_lossy().as_ref()));

        if let Some(error) = self.directory_error.borrow().as_deref() {
            self.reconcile_rows(Vec::new());
            self.status.set_text(error);
            self.status.set_visible(true);
            return;
        }

        if !self.model.borrow().entries_by_dir.contains_key(&root) {
            self.reconcile_rows(Vec::new());
            self.status.set_text("Loading…");
            self.status.set_visible(true);
            return;
        }

        let rows = {
            let mut model = self.model.borrow_mut();
            model.sync_focus();
            model.rows()
        };
        let warning_count = self.directory_warnings.borrow().len();

        if rows.is_empty() {
            self.reconcile_rows(Vec::new());
            if warning_count == 0 {
                self.status.set_text("Directory is empty");
            } else {
                self.status.set_text(&format!(
                    "Unable to read {warning_count} expanded folder(s)"
                ));
            }
            self.status.set_visible(true);
            return;
        }

        let row_count = rows.len();
        let visible_count = row_count.min(self.visible_row_limit.get());
        self.update_list_status(row_count, visible_count, warning_count);
        self.reconcile_rows(rows.into_iter().take(visible_count).collect());
    }

    fn show_more_rows(&self) {
        let new_limit = self.visible_row_limit.get().saturating_add(ROW_BATCH_SIZE);
        self.visible_row_limit.set(new_limit);
        self.rebuild_rows();
    }

    fn reconcile_rows(&self, desired: Vec<FileBrowserRow>) {
        let mut rendered = self.rendered_rows.borrow_mut();
        let current = rendered
            .iter()
            .map(|(row, _)| row.clone())
            .collect::<Vec<_>>();
        for action in row_reconciliation_actions(&current, &desired) {
            match action {
                RowReconcileAction::Move { from, to } => {
                    let entry = rendered.remove(from);
                    self.list.remove(&entry.1);
                    self.list.insert(&entry.1, to as i32);
                    rendered.insert(to, entry);
                }
                RowReconcileAction::Insert { at, row } => {
                    let list_row = self.build_row(&row);
                    self.list.insert(&list_row, at as i32);
                    rendered.insert(at, (row, list_row));
                }
                RowReconcileAction::Remove { at } => {
                    let (_, row) = rendered.remove(at);
                    self.list.remove(&row);
                }
            }
        }
    }

    fn update_list_status(&self, row_count: usize, visible_count: usize, warning_count: usize) {
        self.load_more_button.set_visible(visible_count < row_count);
        if visible_count < row_count {
            let mut message = format!("Showing {visible_count} of {row_count} items");
            if warning_count > 0 {
                message.push_str(&format!("; {warning_count} folder(s) unavailable"));
            }
            self.status.set_text(&message);
            self.status.set_visible(true);
        } else if warning_count > 0 {
            self.status.set_text(&format!(
                "Unable to read {warning_count} expanded folder(s)"
            ));
            self.status.set_visible(true);
        } else {
            self.status.set_visible(false);
        }
    }

    fn install_focus_style(&self) {
        let focus = gtk::EventControllerFocus::new();
        let root = self.root.clone();
        let on_changed = self.on_focus_changed.clone();
        focus.connect_enter(move |_| {
            root.add_css_class("focused");
            if let Some(cb) = on_changed.borrow().as_ref() {
                cb(true);
            }
        });
        let root = self.root.clone();
        let on_changed = self.on_focus_changed.clone();
        focus.connect_leave(move |_| {
            root.remove_css_class("focused");
            if let Some(cb) = on_changed.borrow().as_ref() {
                cb(false);
            }
        });
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
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        let panel = self.clone();
        key.connect_key_pressed(move |_, keyval, _, state| panel.handle_key(keyval, state));
        self.root.add_controller(key);
    }

    fn handle_key(&self, keyval: gdk::Key, state: gdk::ModifierType) -> glib::Propagation {
        if !(self.keyboard_input_guard.borrow())() {
            return glib::Propagation::Proceed;
        }

        if keyval == gdk::Key::Escape {
            if let Some(cb) = self.on_escape.borrow().as_ref() {
                cb();
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
            if let Some(dir) = key_to_focus_dir(keyval) {
                if let Some(cb) = self.on_focus_out.borrow().as_ref() {
                    cb(dir);
                }
                return glib::Propagation::Stop;
            }
        }

        if state.contains(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK)
            && !state.intersects(gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK)
            && keyval
                .to_unicode()
                .is_some_and(|ch| ch.eq_ignore_ascii_case(&'k'))
        {
            self.copy_focused_path_to_clipboard();
            return glib::Propagation::Stop;
        }

        if state.contains(gdk::ModifierType::CONTROL_MASK)
            && !state.intersects(gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK)
        {
            match keyval.to_unicode().map(|ch| ch.to_ascii_lowercase()) {
                Some('c') => {
                    self.copy_focused();
                    return glib::Propagation::Stop;
                }
                Some('x') => {
                    self.cut_focused();
                    return glib::Propagation::Stop;
                }
                Some('v') => {
                    self.paste_from_clipboard();
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

        if state.contains(gdk::ModifierType::SHIFT_MASK) {
            if keyval == gdk::Key::Up {
                self.extend_selection_focus(-1);
                return glib::Propagation::Stop;
            }
            if keyval == gdk::Key::Down {
                self.extend_selection_focus(1);
                return glib::Propagation::Stop;
            }
        }

        if keyval == gdk::Key::Up {
            self.move_focus(-1);
            return glib::Propagation::Stop;
        }
        if keyval == gdk::Key::Down {
            self.move_focus(1);
            return glib::Propagation::Stop;
        }
        if keyval == gdk::Key::Left {
            self.collapse_focused();
            return glib::Propagation::Stop;
        }
        if keyval == gdk::Key::Right {
            self.expand_focused();
            return glib::Propagation::Stop;
        }
        if keyval == gdk::Key::Return || keyval == gdk::Key::KP_Enter {
            self.activate_focused();
            return glib::Propagation::Stop;
        }
        if keyval == gdk::Key::F2 {
            self.show_rename_dialog();
            return glib::Propagation::Stop;
        }

        if keyval == gdk::Key::Delete {
            if state.contains(gdk::ModifierType::SHIFT_MASK) {
                self.show_delete_confirmation();
            } else {
                self.delete_focused_to_trash();
            }
            return glib::Propagation::Stop;
        }

        glib::Propagation::Proceed
    }

    fn move_focus(&self, delta: isize) {
        if self.model.borrow_mut().move_focus(delta) {
            self.ensure_focused_row_rendered();
            self.sync_focus_classes();
            self.scroll_focused_row_into_view();
        }
    }

    fn extend_selection_focus(&self, delta: isize) {
        if self.model.borrow_mut().extend_selection_focus(delta) {
            self.ensure_focused_row_rendered();
            self.sync_focus_classes();
            self.scroll_focused_row_into_view();
        }
    }

    fn ensure_focused_row_rendered(&self) {
        let Some(index) = self.model.borrow().focused_index() else {
            return;
        };
        if index < self.visible_row_limit.get() {
            return;
        }
        let required = ((index / ROW_BATCH_SIZE) + 1) * ROW_BATCH_SIZE;
        self.visible_row_limit.set(required);
        self.rebuild_rows();
    }

    fn expand_focused(&self) {
        let changed = { self.model.borrow_mut().expand_focused() };
        if changed {
            self.refresh();
        }
    }

    fn collapse_focused(&self) {
        let changed = { self.model.borrow_mut().collapse_focused() };
        if changed {
            self.refresh();
        }
    }

    fn activate_focused(&self) {
        let activation = { self.model.borrow_mut().activate_focused() };
        match activation {
            FileBrowserActivation::None => {}
            FileBrowserActivation::Refresh => self.refresh(),
            FileBrowserActivation::Open(path) => (self.on_open_file.borrow())(path),
        }
    }

    fn focus_path(&self, path: PathBuf) {
        if self.model.borrow_mut().focus_path(&path) {
            self.root.grab_focus();
            self.sync_focus_classes();
            self.scroll_focused_row_into_view();
        }
    }

    fn toggle_path_selection(&self, path: PathBuf) {
        if self.model.borrow_mut().toggle_path_selection(&path) {
            self.root.grab_focus();
            self.sync_focus_classes();
        }
    }

    fn extend_selection_to_path(&self, path: PathBuf) {
        if self.model.borrow_mut().extend_selection_to_path(&path) {
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

    fn handle_primary_click(&self, path: PathBuf, n_press: i32, state: gdk::ModifierType) {
        match primary_click_action(n_press, state) {
            PrimaryClickAction::Focus => self.focus_path(path),
            PrimaryClickAction::Activate => self.activate_path(path),
            PrimaryClickAction::ToggleSelection => self.toggle_path_selection(path),
            PrimaryClickAction::ExtendSelection => self.extend_selection_to_path(path),
        }
    }

    fn copy_focused(&self) {
        if self.model.borrow_mut().copy_focused() {
            self.sync_focus_classes();
        }
    }

    fn copy_focused_path_to_clipboard(&self) {
        let Some(path) = self.model.borrow_mut().focused_path() else {
            return;
        };
        let path = path.display().to_string();
        (self.path_clipboard_writer.borrow())(&path);
        self.show_status(&format!("Copied path: {path}"));
    }

    fn cut_focused(&self) {
        if self.model.borrow_mut().cut_focused() {
            self.sync_focus_classes();
        }
    }

    fn paste_from_clipboard(&self) {
        let Some(request) = self.model.borrow_mut().paste_request() else {
            return;
        };
        let Some(operation_root) = self.model.borrow().root.clone() else {
            return;
        };
        let Some(control) = self.begin_file_operation("Pasting", request.clipboard.paths.len())
        else {
            return;
        };
        let scroll_value = self.scroll.vadjustment().value();
        let panel = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let started = Instant::now();
            let worker_control = control.clone();
            let result =
                gio::spawn_blocking(move || execute_paste_with_control(request, &worker_control))
                    .await;
            match result {
                Ok(outcome) => {
                    let pasted = outcome.pasted.len();
                    let error = outcome.error.as_ref().map(ToString::to_string);
                    let cancelled = outcome.cancelled;
                    if panel.operation_context_is_active(&operation_root) {
                        if panel.model.borrow_mut().apply_paste_outcome(outcome) {
                            panel.refresh_with_scroll_value(scroll_value);
                        } else if error.is_none() && !cancelled {
                            panel.show_status("Nothing to paste");
                        }
                        if cancelled {
                            panel.show_status("Paste cancelled");
                        } else if let Some(error) = error {
                            panel.show_status(&format!("Paste failed: {error}"));
                        }
                    } else {
                        panel.model.borrow_mut().clipboard = outcome.clipboard_after;
                    }
                    tracing::info!(
                        operation = "paste",
                        pasted,
                        elapsed_ms = started.elapsed().as_millis(),
                        "file browser operation completed"
                    );
                }
                Err(_) if panel.operation_context_is_active(&operation_root) => {
                    panel.show_status("Paste failed: worker panicked");
                }
                Err(_) => {}
            }
            panel.finish_file_operation();
        });
    }

    fn delete_focused_to_trash(&self) {
        let paths = self.model.borrow_mut().deletion_targets();
        self.delete_paths_with(paths, self.delete_handler.borrow().clone(), "trash");
    }

    fn show_delete_confirmation(&self) {
        let paths = self.model.borrow_mut().deletion_targets();
        if paths.is_empty() {
            return;
        }

        let popup = gtk::Window::builder()
            .modal(true)
            .title("Delete permanently?")
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

        let label_text = if paths.len() == 1 {
            format!("Really delete \"{}\" permanently?", display_name(&paths[0]))
        } else {
            format!("Really delete {} items permanently?", paths.len())
        };
        let label = gtk::Label::new(Some(&label_text));
        label.set_xalign(0.0);
        label.set_wrap(true);
        content.append(&label);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        buttons.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let yes = gtk::Button::with_label("Yes");
        yes.add_css_class("destructive-action");
        buttons.append(&cancel);
        buttons.append(&yes);
        content.append(&buttons);
        popup.set_child(Some(&content));

        let popup_for_cancel = popup.clone();
        cancel.connect_clicked(move |_| popup_for_cancel.close());

        let panel = self.clone();
        let popup_for_yes = popup.clone();
        yes.connect_clicked(move |_| {
            panel.delete_paths_permanently(paths.clone());
            popup_for_yes.close();
        });

        popup.present();
    }

    fn delete_paths_permanently(&self, paths: Vec<PathBuf>) {
        self.delete_paths_with(
            paths,
            Arc::new(permanently_delete_path_with_control),
            "delete",
        );
    }

    fn delete_paths_with(
        &self,
        paths: Vec<PathBuf>,
        delete: DeleteHandler,
        operation: &'static str,
    ) {
        let paths = compact_removed_paths(paths);
        if paths.is_empty() {
            return;
        }
        let Some(operation_root) = self.model.borrow().root.clone() else {
            return;
        };
        let label = if operation == "trash" {
            "Moving to Trash"
        } else {
            "Deleting"
        };
        let Some(control) = self.begin_file_operation(label, paths.len()) else {
            return;
        };

        let scroll_value = self.scroll.vadjustment().value();
        let next_focus = self
            .model
            .borrow()
            .focus_candidate_after_removed_paths(&paths);
        let panel = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let started = Instant::now();
            let worker_control = control.clone();
            match gio::spawn_blocking(move || {
                execute_delete_with_control(paths, delete, &worker_control)
            })
            .await
            {
                Ok(outcome) => {
                    let deleted_count = outcome.deleted.len();
                    let failed_count = outcome.failed.len();
                    let next_focus = outcome
                        .failed
                        .first()
                        .map(|(path, _)| path.clone())
                        .or(next_focus);
                    let context_is_active = panel.operation_context_is_active(&operation_root);
                    if context_is_active {
                        if !outcome.deleted.is_empty() {
                            panel
                                .model
                                .borrow_mut()
                                .forget_removed_paths(&outcome.deleted, next_focus);
                        }
                        if !outcome.deleted.is_empty() || outcome.cancelled {
                            panel.refresh_with_scroll_value(scroll_value);
                        }
                    }
                    if context_is_active {
                        if let Some((_, err)) = outcome.failed.first() {
                            if failed_count == 1 {
                                panel.show_status(&format!("Delete failed: {err}"));
                            } else {
                                panel.show_status(&format!(
                                    "Delete failed for {failed_count} items: {err}"
                                ));
                            }
                        } else if outcome.cancelled {
                            panel.show_status("Delete cancelled");
                        }
                    }
                    tracing::info!(
                        operation,
                        deleted = deleted_count,
                        failed = failed_count,
                        cancelled = outcome.cancelled,
                        elapsed_ms = started.elapsed().as_millis(),
                        "file browser operation completed"
                    );
                }
                Err(_) if panel.operation_context_is_active(&operation_root) => {
                    panel.show_status("Delete failed: worker panicked");
                }
                Err(_) => {}
            }
            panel.finish_file_operation();
        });
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
        install_rename_popup_escape(&popup);
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
            if rows.get(idx).map(|row| row.selected).unwrap_or(false) {
                row_widget.add_css_class("selected");
            } else {
                row_widget.remove_css_class("selected");
            }
            if rows.get(idx).map(|row| row.cut).unwrap_or(false) {
                row_widget.add_css_class("cut");
            } else {
                row_widget.remove_css_class("cut");
            }
            idx += 1;
        }
    }

    fn restore_scroll_value(&self, value: f64) {
        let adjustment = self.scroll.vadjustment();
        set_adjustment_value(&adjustment, value);
        glib::idle_add_local_once(move || set_adjustment_value(&adjustment, value));
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
        #[cfg(all(test, not(target_os = "macos")))]
        self.row_build_count.set(self.row_build_count.get() + 1);

        let list_row = gtk::ListBoxRow::new();
        list_row.add_css_class("flowmux-file-browser-row");
        if row.focused {
            list_row.add_css_class("focused");
        }
        if row.selected {
            list_row.add_css_class("selected");
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

        let icon = row.icon_name.as_deref().map_or_else(
            || gtk::Image::from_icon_name(fallback_icon_name(row.is_dir)),
            gtk::Image::from_icon_name,
        );
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
                    panel.handle_primary_click(
                        path.clone(),
                        n_press,
                        gesture.current_event_state(),
                    );
                }
                gdk::BUTTON_SECONDARY => {
                    panel.focus_path(path.clone());
                    show_context_menu(&row_for_menu, &path, x, y, panel.on_open_file.clone());
                }
                _ => {}
            },
        );
        list_row.add_controller(click);

        list_row
    }
}

impl FileBrowserModel {
    #[cfg(test)]
    fn set_root(&mut self, root: PathBuf) {
        self.set_root_with_state(root, None);
        let _ = self.reload_entries_sync();
    }

    fn set_root_with_state(&mut self, root: PathBuf, state: Option<FileBrowserPaneState>) -> f64 {
        let root = normalize_root(root);
        if let Some(state) = state.filter(|state| state.root.as_ref() == Some(&root)) {
            self.root = Some(root);
            self.expanded = state.expanded;
            self.focused = state.focused;
            self.selected = state.selected;
            self.selection_anchor = state.selection_anchor;
            return state.scroll_value;
        }

        if self.root.as_ref() != Some(&root) {
            self.expanded.clear();
            self.focused = None;
            self.selected.clear();
            self.selection_anchor = None;
            self.entries_by_dir.clear();
        }
        self.root = Some(root);
        0.0
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
        let selected = &self.selected;
        let cut_paths = self.cut_paths();
        self.visible_rows()
            .into_iter()
            .map(|mut row| {
                row.focused = focused == Some(&row.path);
                row.selected = selected.contains(&row.path);
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
        let Some(entries) = self.entries_by_dir.get(dir) else {
            return;
        };

        for entry in entries {
            let expanded = entry.is_dir && self.expanded.contains(&entry.path);
            rows.push(FileBrowserRow {
                path: entry.path.clone(),
                is_dir: entry.is_dir,
                icon_name: entry.icon_name.clone(),
                depth,
                expanded,
                focused: false,
                selected: false,
                cut: false,
            });

            if expanded {
                self.collect_rows(&entry.path, depth + 1, rows);
            }
        }
    }

    #[cfg(test)]
    fn reload_entries_sync(&mut self) -> io::Result<()> {
        let Some(root) = self.root.clone() else {
            self.entries_by_dir.clear();
            return Ok(());
        };
        self.entries_by_dir = load_directory_snapshot(&root, &self.expanded)?.entries_by_dir;
        Ok(())
    }

    fn focus_path(&mut self, path: &Path) -> bool {
        if self.visible_rows().iter().any(|row| row.path == path) {
            let changed = self.focused.as_deref() != Some(path);
            self.select_only_path(path.to_path_buf());
            changed
        } else {
            false
        }
    }

    fn toggle_path_selection(&mut self, path: &Path) -> bool {
        if !self.visible_rows().iter().any(|row| row.path == path) {
            return false;
        }
        self.focused = Some(path.to_path_buf());
        self.selection_anchor = Some(path.to_path_buf());
        if !self.selected.remove(path) {
            self.selected.insert(path.to_path_buf());
        }
        true
    }

    fn extend_selection_to_path(&mut self, path: &Path) -> bool {
        let rows = self.visible_rows();
        if !rows.iter().any(|row| row.path == path) {
            return false;
        }
        let anchor = self
            .selection_anchor
            .clone()
            .or_else(|| self.focused.clone())
            .unwrap_or_else(|| path.to_path_buf());
        self.focused = Some(path.to_path_buf());
        self.select_range(&rows, &anchor, path);
        true
    }

    fn select_only_path(&mut self, path: PathBuf) {
        self.focused = Some(path.clone());
        self.selected.clear();
        self.selected.insert(path.clone());
        self.selection_anchor = Some(path);
    }

    fn select_range(&mut self, rows: &[FileBrowserRow], anchor: &Path, target: &Path) {
        let Some(anchor_idx) = rows.iter().position(|row| row.path == anchor) else {
            self.select_only_path(target.to_path_buf());
            return;
        };
        let Some(target_idx) = rows.iter().position(|row| row.path == target) else {
            return;
        };
        let (start, end) = if anchor_idx <= target_idx {
            (anchor_idx, target_idx)
        } else {
            (target_idx, anchor_idx)
        };
        self.selected.clear();
        self.selected
            .extend(rows[start..=end].iter().map(|row| row.path.clone()));
        self.selection_anchor = Some(anchor.to_path_buf());
    }

    fn deletion_targets(&self) -> Vec<PathBuf> {
        let rows = self.visible_rows();
        let mut paths: Vec<PathBuf> = rows
            .iter()
            .filter(|row| self.selected.contains(&row.path))
            .map(|row| row.path.clone())
            .collect();
        if paths.is_empty() {
            if let Some(focused) = self.focused.clone() {
                paths.push(focused);
            }
        }
        compact_removed_paths(paths)
    }

    fn focus_candidate_after_removed_paths(&self, removed: &[PathBuf]) -> Option<PathBuf> {
        let rows = self.visible_rows();
        let first_removed = rows
            .iter()
            .position(|row| path_is_under_any(&row.path, removed))?;
        rows.iter()
            .skip(first_removed)
            .find(|row| !path_is_under_any(&row.path, removed))
            .or_else(|| {
                rows[..first_removed]
                    .iter()
                    .rev()
                    .find(|row| !path_is_under_any(&row.path, removed))
            })
            .map(|row| row.path.clone())
    }

    fn forget_removed_paths(&mut self, removed: &[PathBuf], next_focus: Option<PathBuf>) {
        self.expanded
            .retain(|path| !path_is_under_any(path, removed));
        self.selected
            .retain(|path| !path_is_under_any(path, removed));
        if self
            .selection_anchor
            .as_ref()
            .is_some_and(|path| path_is_under_any(path, removed))
        {
            self.selection_anchor = None;
        }
        if let Some(clipboard) = self.clipboard.as_mut() {
            clipboard
                .paths
                .retain(|path| !path_is_under_any(path, removed));
        }
        if self
            .clipboard
            .as_ref()
            .is_some_and(|clipboard| clipboard.paths.is_empty())
        {
            self.clipboard = None;
        }
        self.focused = next_focus;
        if self.selected.is_empty() {
            if let Some(focused) = self.focused.clone() {
                self.selected.insert(focused.clone());
                self.selection_anchor = Some(focused);
            }
        }
    }

    fn move_focus(&mut self, delta: isize) -> bool {
        self.move_focus_with_selection(delta, false)
    }

    fn extend_selection_focus(&mut self, delta: isize) -> bool {
        self.move_focus_with_selection(delta, true)
    }

    fn move_focus_with_selection(&mut self, delta: isize, extend_selection: bool) -> bool {
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
        if extend_selection {
            let anchor = self
                .selection_anchor
                .clone()
                .unwrap_or_else(|| rows[current].path.clone());
            self.focused = Some(rows[next].path.clone());
            let target = rows[next].path.clone();
            self.select_range(&rows, &anchor, &target);
        } else {
            self.select_only_path(rows[next].path.clone());
        }
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
        self.selected = self
            .selected
            .iter()
            .map(|path| rewrite_path_prefix(path, old_path, new_path))
            .collect();
        self.selection_anchor = self
            .selection_anchor
            .as_ref()
            .map(|path| rewrite_path_prefix(path, old_path, new_path));
    }

    fn copy_focused(&mut self) -> bool {
        let paths = self.clipboard_targets();
        if paths.is_empty() {
            return false;
        }
        self.clipboard = Some(FileClipboard {
            operation: ClipboardOperation::Copy,
            paths,
        });
        true
    }

    fn cut_focused(&mut self) -> bool {
        let paths = self.clipboard_targets();
        if paths.is_empty() {
            return false;
        }
        self.clipboard = Some(FileClipboard {
            operation: ClipboardOperation::Cut,
            paths,
        });
        true
    }

    fn clipboard_targets(&mut self) -> Vec<PathBuf> {
        if self.selected.is_empty() {
            return self.focused_path().into_iter().collect();
        }
        self.visible_rows()
            .into_iter()
            .filter(|row| self.selected.contains(&row.path))
            .map(|row| row.path)
            .collect()
    }

    #[cfg(test)]
    fn paste_from_clipboard(&mut self) -> io::Result<bool> {
        let Some(request) = self.paste_request() else {
            return Ok(false);
        };
        let mut outcome = execute_paste(request);
        let error = outcome.error.take();
        let changed = self.apply_paste_outcome(outcome);
        match error {
            Some(error) => Err(error),
            None => Ok(changed),
        }
    }

    fn paste_request(&mut self) -> Option<PasteRequest> {
        Some(PasteRequest {
            clipboard: self.clipboard.clone()?,
            target_dir: self.paste_target_dir()?,
        })
    }

    fn apply_paste_outcome(&mut self, outcome: PasteOutcome) -> bool {
        for (source, destination) in outcome.moved {
            self.rewrite_tracked_paths(&source, &destination);
        }
        self.clipboard = outcome.clipboard_after;
        self.sync_focus();
        !outcome.pasted.is_empty()
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

#[cfg(test)]
fn execute_paste(request: PasteRequest) -> PasteOutcome {
    let control = FileOperationControl::new(request.clipboard.paths.len());
    execute_paste_with_control(request, &control)
}

fn execute_paste_with_control(
    request: PasteRequest,
    control: &FileOperationControl,
) -> PasteOutcome {
    let PasteRequest {
        clipboard,
        target_dir,
    } = request;
    let clipboard_after_copy = clipboard.clone();
    let mut pasted = Vec::new();
    let mut moved = Vec::new();
    let mut remaining = Vec::new();
    let mut error = None;
    let mut cancelled = false;
    for (index, source) in clipboard.paths.iter().enumerate() {
        if let Err(cancel_error) = control.check_cancelled() {
            cancelled = true;
            error = Some(cancel_error);
            remaining.extend_from_slice(&clipboard.paths[index..]);
            break;
        }
        if !source.exists() {
            control.mark_completed();
            continue;
        }
        if source.is_dir() && target_dir.starts_with(source) {
            error = Some(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot paste a directory into itself",
            ));
            remaining.extend_from_slice(&clipboard.paths[index..]);
            break;
        }

        if clipboard.operation == ClipboardOperation::Cut
            && source.parent() == Some(target_dir.as_path())
        {
            pasted.push(source.clone());
            control.mark_completed();
            continue;
        }

        let destination = match unique_destination(&target_dir, source) {
            Ok(destination) => destination,
            Err(err) => {
                error = Some(err);
                remaining.extend_from_slice(&clipboard.paths[index..]);
                break;
            }
        };
        let result = match clipboard.operation {
            ClipboardOperation::Copy => copy_path_with_control(source, &destination, control),
            ClipboardOperation::Cut => move_path_with_control(source, &destination, control),
        };
        if let Err(operation_error) = result {
            let _ = permanently_delete_path(&destination);
            cancelled = operation_error.kind() == io::ErrorKind::Interrupted;
            error = Some(operation_error);
            remaining.extend_from_slice(&clipboard.paths[index..]);
            break;
        }
        if clipboard.operation == ClipboardOperation::Cut {
            moved.push((source.clone(), destination.clone()));
        }
        pasted.push(destination);
        control.mark_completed();
    }

    let clipboard_after = match clipboard.operation {
        ClipboardOperation::Copy => Some(clipboard_after_copy),
        ClipboardOperation::Cut if remaining.is_empty() => None,
        ClipboardOperation::Cut => Some(FileClipboard {
            operation: ClipboardOperation::Cut,
            paths: remaining,
        }),
    };
    PasteOutcome {
        pasted,
        moved,
        clipboard_after,
        error,
        cancelled,
    }
}

fn execute_delete_with_control(
    paths: Vec<PathBuf>,
    delete: DeleteHandler,
    control: &FileOperationControl,
) -> DeleteOutcome {
    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    let mut cancelled = false;
    for path in paths {
        if control.check_cancelled().is_err() {
            cancelled = true;
            break;
        }
        match delete(&path, control) {
            Ok(()) => deleted.push(path),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                cancelled = true;
                break;
            }
            Err(err) => failed.push((path, err)),
        }
        control.mark_completed();
    }
    DeleteOutcome {
        deleted,
        failed,
        cancelled,
    }
}

fn read_dir_entries(dir: &Path) -> std::io::Result<Vec<FsEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false);
        let icon_name = system_file_icon_name(&path);
        entries.push(FsEntry {
            path,
            name,
            is_dir,
            icon_name,
        });
    }

    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    Ok(entries)
}

fn load_directory_snapshot(
    root: &Path,
    expanded: &HashSet<PathBuf>,
) -> io::Result<DirectorySnapshot> {
    load_directory_snapshot_with(root, expanded, &read_dir_entries)
}

fn load_directory_snapshot_with(
    root: &Path,
    expanded: &HashSet<PathBuf>,
    reader: &dyn Fn(&Path) -> io::Result<Vec<FsEntry>>,
) -> io::Result<DirectorySnapshot> {
    fn collect(
        directory: &Path,
        expanded: &HashSet<PathBuf>,
        snapshot: &mut HashMap<PathBuf, Vec<FsEntry>>,
        warnings: &mut HashMap<PathBuf, String>,
        is_root: bool,
        reader: &dyn Fn(&Path) -> io::Result<Vec<FsEntry>>,
    ) -> io::Result<()> {
        let entries = match reader(directory) {
            Ok(entries) => entries,
            Err(error) if is_root => return Err(error),
            Err(error) => {
                warnings.insert(directory.to_path_buf(), error.to_string());
                snapshot.insert(directory.to_path_buf(), Vec::new());
                return Ok(());
            }
        };
        let children = entries
            .iter()
            .filter(|entry| entry.is_dir && expanded.contains(&entry.path))
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        snapshot.insert(directory.to_path_buf(), entries);
        for child in children {
            collect(&child, expanded, snapshot, warnings, false, reader)?;
        }
        Ok(())
    }

    let mut entries_by_dir = HashMap::new();
    let mut warnings = HashMap::new();
    collect(
        root,
        expanded,
        &mut entries_by_dir,
        &mut warnings,
        true,
        reader,
    )?;
    Ok(DirectorySnapshot {
        entries_by_dir,
        warnings,
    })
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

fn copy_path_with_control(
    source: &Path,
    destination: &Path,
    control: &FileOperationControl,
) -> io::Result<()> {
    control.check_cancelled()?;
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(source)?;
        std::os::unix::fs::symlink(target, destination)
    } else if metadata.is_dir() {
        fs::create_dir(destination)?;
        for entry in fs::read_dir(source)? {
            control.check_cancelled()?;
            let entry = entry?;
            let child_source = entry.path();
            let child_destination = destination.join(entry.file_name());
            copy_path_with_control(&child_source, &child_destination, control)?;
        }
        fs::set_permissions(destination, metadata.permissions())
    } else {
        copy_file_with_control(source, destination, &metadata, control)
    }
}

fn copy_file_with_control(
    source: &Path,
    destination: &Path,
    metadata: &fs::Metadata,
    control: &FileOperationControl,
) -> io::Result<()> {
    let mut source_file = File::open(source)?;
    let mut destination_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        control.check_cancelled()?;
        let read = source_file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        destination_file.write_all(&buffer[..read])?;
    }
    destination_file.flush()?;
    fs::set_permissions(destination, metadata.permissions())
}

fn move_path_with_control(
    source: &Path,
    destination: &Path,
    control: &FileOperationControl,
) -> io::Result<()> {
    control.check_cancelled()?;
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            if !is_cross_device_error(&rename_err) {
                return Err(rename_err);
            }
            copy_path_with_control(source, destination, control)?;
            control.check_cancelled()?;
            let remove_result = permanently_delete_path_with_control(source, control);
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

fn is_cross_device_error(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::EXDEV)
}

fn show_context_menu(
    parent: &impl IsA<gtk::Widget>,
    path: &Path,
    x: f64,
    y: f64,
    on_open_file: OpenFileHandler,
) {
    let popover = gtk::Popover::new();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.set_margin_top(4);
    content.set_margin_bottom(4);

    let open = gtk::Button::with_label("Open");
    open.add_css_class("flat");
    open.set_halign(gtk::Align::Fill);
    open.set_hexpand(true);
    let target = path.to_path_buf();
    let pop = popover.clone();
    open.connect_clicked(move |_| {
        (on_open_file.borrow())(target.clone());
        pop.popdown();
    });
    content.append(&open);

    let open_externally = gtk::Button::with_label("Open Externally");
    open_externally.add_css_class("flat");
    open_externally.set_halign(gtk::Align::Fill);
    open_externally.set_hexpand(true);
    let target = path.to_path_buf();
    let pop = popover.clone();
    open_externally.connect_clicked(move |_| {
        open_file(&target);
        pop.popdown();
    });
    content.append(&open_externally);

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

fn install_rename_popup_escape(popup: &gtk::Window) {
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    let popup_for_key = popup.clone();
    key.connect_key_pressed(move |_, keyval, _, _| handle_rename_popup_key(&popup_for_key, keyval));
    popup.add_controller(key);
}

fn handle_rename_popup_key(popup: &gtk::Window, keyval: gdk::Key) -> glib::Propagation {
    if keyval == gdk::Key::Escape {
        popup.close();
        return glib::Propagation::Stop;
    }

    glib::Propagation::Proceed
}

fn show_path_in_folder(path: &Path) {
    let dir = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    show_in_folder::open_directory(dir);
}

pub(crate) fn open_file(path: &Path) {
    if is_markdown_file(path) {
        match launch_markdown_viewer(path) {
            Ok(()) => return,
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "failed to launch Markdown viewer; falling back to default app"
                );
            }
        }
    }

    let file = gio::File::for_path(path);
    let uri = file.uri();
    if let Err(err) = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
        tracing::warn!(path = %path.display(), error = %err, "failed to open file");
    }
}

pub(crate) fn file_open_target(path: &Path) -> FileOpenTarget {
    if crate::ui::image_viewer::supports_path(path) {
        return FileOpenTarget::ImageViewer;
    }

    let Some(extension) = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
    else {
        return if flowmux_editor::is_editable_text_file(path) {
            FileOpenTarget::Editor
        } else {
            FileOpenTarget::Binary
        };
    };

    let known_binary_format = matches!(
        extension.as_str(),
        "pdf"
            | "mp3"
            | "wav"
            | "flac"
            | "ogg"
            | "m4a"
            | "mp4"
            | "mov"
            | "mkv"
            | "webm"
            | "avi"
            | "zip"
            | "gz"
            | "bz2"
            | "xz"
            | "7z"
            | "rar"
            | "tar"
            | "dmg"
            | "iso"
            | "exe"
            | "dll"
            | "so"
            | "dylib"
            | "a"
            | "o"
            | "class"
            | "jar"
            | "wasm"
    );
    if known_binary_format || !flowmux_editor::is_editable_text_file(path) {
        FileOpenTarget::Binary
    } else {
        FileOpenTarget::Editor
    }
}

pub(crate) fn open_binary(path: &Path) {
    // Opening a file from the viewer must never execute it merely because its
    // Unix mode has an executable bit. Let the desktop's registered handler
    // decide how to display non-editable content.
    open_file(path);
}

fn is_markdown_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase),
        Some(ext) if matches!(ext.as_str(), "md" | "markdown" | "mdown" | "mkd" | "mkdn")
    )
}

pub(crate) fn launch_markdown_viewer(path: &Path) -> io::Result<()> {
    Command::new(markdown_viewer_binary())
        .arg(path)
        .spawn()
        .map(|_| ())
}

fn markdown_viewer_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("FLOWMUX_MD_VIEWER") {
        return PathBuf::from(path);
    }

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(dir) = current_exe.parent() {
            let candidate = dir.join("flowmux-md-viewer");
            if candidate.exists() {
                return candidate;
            }
        }
    }

    PathBuf::from("flowmux-md-viewer")
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

fn path_is_or_under(path: &Path, parent: &Path) -> bool {
    path == parent || path.starts_with(parent)
}

fn path_is_under_any(path: &Path, parents: &[PathBuf]) -> bool {
    parents.iter().any(|parent| path_is_or_under(path, parent))
}

fn compact_removed_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut compacted: Vec<PathBuf> = Vec::new();
    for path in paths {
        if compacted
            .iter()
            .any(|parent| path_is_or_under(&path, parent))
        {
            continue;
        }
        compacted.retain(|child| !path_is_or_under(child, &path));
        compacted.push(path);
    }
    compacted
}

fn set_adjustment_value(adjustment: &gtk::Adjustment, value: f64) {
    let lower = adjustment.lower();
    let upper = (adjustment.upper() - adjustment.page_size()).max(lower);
    adjustment.set_value(value.clamp(lower, upper));
}

fn system_file_icon(path: &Path) -> Option<gio::Icon> {
    gio::File::for_path(path)
        .query_info(
            "standard::icon",
            gio::FileQueryInfoFlags::NONE,
            None::<&gio::Cancellable>,
        )
        .ok()
        .and_then(|info| info.icon())
}

fn system_file_icon_name(path: &Path) -> Option<String> {
    system_file_icon(path)
        .and_then(|icon| icon.downcast::<gio::ThemedIcon>().ok())
        .and_then(|icon| icon.names().first().map(ToString::to_string))
}

fn fallback_icon_name(is_dir: bool) -> &'static str {
    if is_dir {
        "folder-symbolic"
    } else {
        "text-x-generic-symbolic"
    }
}

fn move_to_trash(path: &Path, control: &FileOperationControl) -> io::Result<()> {
    control.check_cancelled()?;
    gio::File::for_path(path)
        .trash(None::<&gio::Cancellable>)
        .map(|_| ())
        .map_err(|err| io::Error::other(err.to_string()))
}

fn permanently_delete_path(path: &Path) -> io::Result<()> {
    permanently_delete_path_with_control(path, &FileOperationControl::new(1))
}

fn permanently_delete_path_with_control(
    path: &Path,
    control: &FileOperationControl,
) -> io::Result<()> {
    control.check_cancelled()?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() {
        for entry in fs::read_dir(path)? {
            control.check_cancelled()?;
            permanently_delete_path_with_control(&entry?.path(), control)?;
        }
        control.check_cancelled()?;
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
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
    #![cfg_attr(target_os = "macos", allow(dead_code, unused_imports))]

    use super::*;
    use std::cell::Cell;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn reconciliation_row(name: &str) -> FileBrowserRow {
        FileBrowserRow {
            path: PathBuf::from(name),
            is_dir: false,
            icon_name: None,
            depth: 0,
            expanded: false,
            focused: false,
            selected: false,
            cut: false,
        }
    }

    #[test]
    fn row_reconciliation_builds_only_inserted_or_changed_rows() {
        let a = reconciliation_row("a");
        let b = reconciliation_row("b");
        let inserted = reconciliation_row("new");
        assert_eq!(
            row_reconciliation_actions(&[a.clone(), b.clone()], &[inserted.clone(), a, b]),
            vec![RowReconcileAction::Insert {
                at: 0,
                row: inserted,
            }]
        );

        let unchanged = reconciliation_row("same");
        let mut changed = unchanged.clone();
        changed.focused = true;
        assert_eq!(
            row_reconciliation_actions(&[unchanged], &[changed.clone()]),
            vec![
                RowReconcileAction::Insert {
                    at: 0,
                    row: changed,
                },
                RowReconcileAction::Remove { at: 1 },
            ]
        );
    }

    #[cfg(not(target_os = "macos"))]
    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let context = glib::MainContext::default();
        loop {
            while context.pending() {
                context.iteration(false);
            }
            if predicate() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for async file operation"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn markdown_file_extensions_route_to_markdown_viewer() {
        for name in [
            "README.md",
            "notes.markdown",
            "draft.mdown",
            "post.mkd",
            "manual.mkdn",
            "UPPER.MD",
        ] {
            assert!(
                is_markdown_file(Path::new(name)),
                "{name} should be Markdown"
            );
        }

        for name in ["image.png", "archive.md.bak", "Makefile", "folder"] {
            assert!(
                !is_markdown_file(Path::new(name)),
                "{name} should not be Markdown"
            );
        }
    }

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

    fn selected_names(model: &FileBrowserModel) -> Vec<String> {
        model
            .rows()
            .into_iter()
            .filter(|row| row.selected)
            .map(|row| display_name(&row.path))
            .collect()
    }

    fn panel_row_names(panel: &FileBrowserPanel) -> Vec<String> {
        row_names(&panel.model.borrow())
    }

    fn rendered_row_count(panel: &FileBrowserPanel) -> usize {
        let mut count = 0;
        let mut child = panel.list.first_child();
        while let Some(row) = child {
            count += 1;
            child = row.next_sibling();
        }
        count
    }

    fn panel_focused_path(panel: &FileBrowserPanel) -> Option<PathBuf> {
        panel.model.borrow().focused.clone()
    }

    fn panel_selected_names(panel: &FileBrowserPanel) -> Vec<String> {
        let model = panel.model.borrow();
        model
            .rows()
            .into_iter()
            .filter(|row| row.selected)
            .map(|row| display_name(&row.path))
            .collect()
    }

    fn set_panel_scroll(panel: &FileBrowserPanel, value: f64) {
        let adjustment = panel.scroll.vadjustment();
        adjustment.set_lower(0.0);
        adjustment.set_upper(1000.0);
        adjustment.set_page_size(100.0);
        adjustment.set_value(value);
    }

    fn key(name: &str) -> gdk::Key {
        gdk::Key::from_name(name).unwrap_or_else(|| panic!("missing key: {name}"))
    }

    fn find_entry(widget: &gtk::Widget) -> Option<gtk::Entry> {
        if let Ok(entry) = widget.clone().downcast::<gtk::Entry>() {
            return Some(entry);
        }

        let mut child = widget.first_child();
        while let Some(widget) = child {
            if let Some(entry) = find_entry(&widget) {
                return Some(entry);
            }
            child = widget.next_sibling();
        }
        None
    }

    fn find_button(widget: &gtk::Widget, label: &str) -> Option<gtk::Button> {
        if let Ok(button) = widget.clone().downcast::<gtk::Button>() {
            if button.label().as_deref() == Some(label) {
                return Some(button);
            }
        }

        let mut child = widget.first_child();
        while let Some(widget) = child {
            if let Some(button) = find_button(&widget, label) {
                return Some(button);
            }
            child = widget.next_sibling();
        }

        None
    }

    fn close_rename_windows() {
        for widget in gtk::Window::list_toplevels() {
            if let Ok(window) = widget.downcast::<gtk::Window>() {
                if window.title().as_deref() == Some("Rename") {
                    window.close();
                }
            }
        }
    }

    fn close_delete_windows() {
        for widget in gtk::Window::list_toplevels() {
            if let Ok(window) = widget.downcast::<gtk::Window>() {
                if window.title().as_deref() == Some("Delete permanently?") {
                    window.close();
                }
            }
        }
    }

    #[test]
    fn file_icon_prefers_system_icon_for_existing_file() {
        let tmp = TestDir::new("icon-system");
        let file = tmp.file("document.txt");

        assert!(system_file_icon_name(&file).is_some());
    }

    #[test]
    fn file_icon_falls_back_when_system_icon_is_unavailable() {
        let tmp = TestDir::new("icon-fallback");
        let missing = tmp.path.join("missing.txt");

        assert!(system_file_icon_name(&missing).is_none());
        assert_eq!(fallback_icon_name(false), "text-x-generic-symbolic");
        assert_eq!(fallback_icon_name(true), "folder-symbolic");
    }

    #[test]
    fn file_open_routing_uses_content_and_supported_viewer_formats() {
        let tmp = TestDir::new("open-routing");
        for name in ["main.rs", "README.md", "설정.json", "Dockerfile"] {
            let path = tmp.file(name);
            assert_eq!(file_open_target(&path), FileOpenTarget::Editor, "{name}");
        }

        let image = tmp.file("image.png");
        assert_eq!(file_open_target(&image), FileOpenTarget::ImageViewer);

        let pdf = tmp.file("manual.pdf");
        assert_eq!(file_open_target(&pdf), FileOpenTarget::Binary);

        let binary = tmp.path.join("program");
        fs::write(&binary, b"text\0binary").unwrap();
        assert_eq!(file_open_target(&binary), FileOpenTarget::Binary);
    }

    #[test]
    fn plain_primary_click_requires_two_presses_to_activate() {
        assert_eq!(
            primary_click_action(1, gdk::ModifierType::empty()),
            PrimaryClickAction::Focus
        );
        assert_eq!(
            primary_click_action(2, gdk::ModifierType::empty()),
            PrimaryClickAction::Activate
        );
        assert_eq!(
            primary_click_action(1, gdk::ModifierType::CONTROL_MASK),
            PrimaryClickAction::ToggleSelection
        );
        assert_eq!(
            primary_click_action(1, gdk::ModifierType::SHIFT_MASK),
            PrimaryClickAction::ExtendSelection
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn panel_activation_uses_the_configured_open_handler() {
        let tmp = TestDir::new("panel-open-handler");
        let path = tmp.file("다국어.rs");
        let opened = Rc::new(RefCell::new(None));
        let panel = FileBrowserPanel::new();
        {
            let opened = opened.clone();
            panel.connect_open_file(move |path| *opened.borrow_mut() = Some(path));
        }
        panel.show_for_root(tmp.path.clone());

        panel.activate_focused();

        assert_eq!(*opened.borrow(), Some(path));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn primary_click_selects_and_double_click_opens() {
        let tmp = TestDir::new("double-click-open");
        let path = tmp.file("문서.rs");
        let opened = Rc::new(RefCell::new(None));
        let panel = FileBrowserPanel::new();
        {
            let opened = opened.clone();
            panel.connect_open_file(move |path| *opened.borrow_mut() = Some(path));
        }
        panel.show_for_root(tmp.path.clone());

        panel.handle_primary_click(path.clone(), 1, gdk::ModifierType::empty());
        assert_eq!(*opened.borrow(), None);
        assert_eq!(panel_focused_path(&panel), Some(path.clone()));

        panel.handle_primary_click(path.clone(), 2, gdk::ModifierType::empty());
        assert_eq!(*opened.borrow(), Some(path));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn pane_state_restores_focus_scroll_and_expanded_paths() {
        let tmp = TestDir::new("pane-state-a");
        let dir = tmp.dir("folder");
        tmp.file("folder/child.txt");
        tmp.file("other.txt");
        let other = TestDir::new("pane-state-b");
        other.file("b.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.focus_path(dir.clone());
        panel.expand_focused();
        set_panel_scroll(&panel, 240.0);
        let state = panel.pane_state();

        panel.show_for_root(other.path.clone());
        assert_eq!(panel_focused_path(&panel), Some(other.path.join("b.txt")));
        assert_eq!(panel.scroll.vadjustment().value(), 0.0);

        panel.show_for_root_with_state(tmp.path.clone(), Some(state));

        assert_eq!(panel_focused_path(&panel), Some(dir.clone()));
        assert!(panel.model.borrow().expanded.contains(&dir));
        assert_eq!(panel.scroll.vadjustment().value(), 240.0);
        assert_eq!(
            panel_row_names(&panel),
            vec!["folder", "child.txt", "other.txt"]
        );
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

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn large_directory_rows_are_loaded_in_batches() {
        let tmp = TestDir::new("row-batches");
        for index in 0..525 {
            tmp.file(&format!("file-{index:03}.txt"));
        }
        let panel = FileBrowserPanel::new();

        panel.show_for_root(tmp.path.clone());

        assert_eq!(rendered_row_count(&panel), ROW_BATCH_SIZE);
        assert_eq!(panel.row_build_count.get(), ROW_BATCH_SIZE);
        assert!(panel.load_more_button.is_visible());
        assert_eq!(panel.status.text(), "Showing 500 of 525 items");
        let first_row = panel.list.first_child().unwrap();

        panel.load_more_button.emit_clicked();

        assert_eq!(rendered_row_count(&panel), 525);
        assert_eq!(panel.row_build_count.get(), 525);
        assert_eq!(panel.list.first_child().as_ref(), Some(&first_row));
        assert!(!panel.load_more_button.is_visible());
        assert!(!panel.status.is_visible());
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn directory_monitor_refreshes_only_the_changed_directory() {
        let tmp = TestDir::new("directory-monitor");
        tmp.file("existing.txt");
        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        let existing_row = panel.list.first_child().unwrap();
        let initial_builds = panel.row_build_count.get();
        panel.install_directory_monitors(vec![tmp.path.clone()]);

        let created = tmp.file("created.txt");
        wait_until(|| panel_row_names(&panel).contains(&"created.txt".to_string()));

        assert!(created.exists());
        assert_eq!(panel_row_names(&panel), vec!["created.txt", "existing.txt"]);
        assert_eq!(panel.row_build_count.get(), initial_builds + 1);
        assert_eq!(panel.list.last_child().as_ref(), Some(&existing_row));
    }

    #[test]
    fn directory_snapshot_reads_only_expanded_subtrees() {
        let tmp = TestDir::new("snapshot-expanded");
        let expanded_dir = tmp.dir("expanded");
        let collapsed_dir = tmp.dir("collapsed");
        let nested_dir = tmp.dir("expanded/nested");
        tmp.file("expanded/visible.txt");
        tmp.file("expanded/nested/hidden.txt");
        tmp.file("collapsed/hidden.txt");
        let expanded = HashSet::from([expanded_dir.clone()]);

        let snapshot = load_directory_snapshot(&tmp.path, &expanded).unwrap();

        assert!(snapshot.entries_by_dir.contains_key(&tmp.path));
        assert!(snapshot.entries_by_dir.contains_key(&expanded_dir));
        assert!(!snapshot.entries_by_dir.contains_key(&collapsed_dir));
        assert!(!snapshot.entries_by_dir.contains_key(&nested_dir));
        assert!(snapshot.warnings.is_empty());
    }

    #[test]
    fn directory_snapshot_keeps_root_when_expanded_child_fails() {
        let tmp = TestDir::new("snapshot-child-error");
        let expanded_dir = tmp.dir("expanded");
        tmp.file("visible.txt");
        let expanded = HashSet::from([expanded_dir.clone()]);

        let snapshot = load_directory_snapshot_with(&tmp.path, &expanded, &|directory| {
            if directory == expanded_dir {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            } else {
                read_dir_entries(directory)
            }
        })
        .unwrap();

        assert!(snapshot.entries_by_dir.contains_key(&tmp.path));
        assert_eq!(snapshot.entries_by_dir[&expanded_dir].len(), 0);
        assert_eq!(snapshot.warnings[&expanded_dir], "denied");
    }

    #[test]
    fn cancelled_recursive_paste_stops_before_writing() {
        let tmp = TestDir::new("cancel-paste");
        let source = tmp.dir("source");
        tmp.file("source/a.txt");
        let target = tmp.dir("target");
        let control = FileOperationControl::new(1);
        control.cancelled.store(true, AtomicOrdering::Relaxed);
        let request = PasteRequest {
            clipboard: FileClipboard {
                operation: ClipboardOperation::Copy,
                paths: vec![source],
            },
            target_dir: target.clone(),
        };

        let outcome = execute_paste_with_control(request, &control);

        assert!(outcome.cancelled);
        assert_eq!(outcome.error.unwrap().kind(), io::ErrorKind::Interrupted);
        assert!(!target.join("source").exists());
        assert_eq!(control.completed.load(AtomicOrdering::Relaxed), 0);
    }

    #[test]
    fn cancelled_delete_stops_before_next_selected_item() {
        let tmp = TestDir::new("cancel-delete");
        let first = tmp.file("a.txt");
        let second = tmp.file("b.txt");
        let control = FileOperationControl::new(2);
        let cancel = control.cancelled.clone();
        let delete: DeleteHandler = Arc::new(move |path, _| {
            fs::remove_file(path)?;
            cancel.store(true, AtomicOrdering::Relaxed);
            Ok(())
        });

        let outcome =
            execute_delete_with_control(vec![first.clone(), second.clone()], delete, &control);

        assert!(outcome.cancelled);
        assert_eq!(outcome.deleted, vec![first]);
        assert!(second.exists());
        assert_eq!(control.completed.load(AtomicOrdering::Relaxed), 1);
    }

    #[test]
    fn interrupted_delete_is_reported_as_cancelled() {
        let tmp = TestDir::new("cancel-delete-current");
        let path = tmp.file("a.txt");
        let control = FileOperationControl::new(1);
        let delete: DeleteHandler =
            Arc::new(|_, _| Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")));

        let outcome = execute_delete_with_control(vec![path.clone()], delete, &control);

        assert!(outcome.cancelled);
        assert!(outcome.failed.is_empty());
        assert!(path.exists());
        assert_eq!(control.completed.load(AtomicOrdering::Relaxed), 0);
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn file_operation_cancel_button_controls_current_operation() {
        let panel = FileBrowserPanel::new();
        let control = panel.begin_file_operation("Copying", 3).unwrap();

        assert!(panel.cancel_operation_button.property::<bool>("visible"));
        assert_eq!(panel.status.text(), "Copying… 0/3");
        panel.cancel_operation_button.emit_clicked();
        assert!(control.cancelled.load(AtomicOrdering::Relaxed));
        assert_eq!(panel.status.text(), "Cancelling…");

        panel.finish_file_operation();
        assert!(!panel.cancel_operation_button.property::<bool>("visible"));
        assert!(!panel.file_operation_in_progress.get());
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn changing_root_cancels_the_previous_file_operation() {
        let first = TestDir::new("operation-root-a");
        first.file("a.txt");
        let second = TestDir::new("operation-root-b");
        second.file("b.txt");
        let panel = FileBrowserPanel::new();
        panel.show_for_root(first.path.clone());
        let control = panel.begin_file_operation("Copying", 1).unwrap();

        panel.show_for_root(second.path.clone());

        assert!(control.cancelled.load(AtomicOrdering::Relaxed));
        assert!(!panel.operation_context_is_active(&first.path));
        assert!(panel.operation_context_is_active(&second.path));
        panel.finish_file_operation();
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
        model.reload_entries_sync().unwrap();
        assert_eq!(row_names(&model), ["src", "main.rs", "top.txt"]);
        assert!(!model.expand_focused());
        assert!(model.collapse_focused());
        assert_eq!(row_names(&model), ["src", "top.txt"]);
        assert_eq!(model.activate_focused(), FileBrowserActivation::Refresh);
        model.reload_entries_sync().unwrap();
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
        model.reload_entries_sync().unwrap();

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
        model.reload_entries_sync().unwrap();

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
    fn copy_paste_file_creates_unique_sibling_and_keeps_focus() {
        let tmp = TestDir::new("copy-file");
        tmp.file("a.txt");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());

        assert!(model.copy_focused());
        assert!(model.paste_from_clipboard().unwrap());
        model.reload_entries_sync().unwrap();

        let copied = tmp.path.join("a copy.txt");
        assert!(tmp.path.join("a.txt").exists());
        assert!(copied.exists());
        assert_eq!(model.focused.as_ref(), Some(&tmp.path.join("a.txt")));
        assert!(row_paths(&model).contains(&copied));
    }

    #[test]
    fn copy_paste_folder_recursively_and_keeps_focus() {
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
        assert_eq!(model.focused.as_ref(), Some(&target));
    }

    #[test]
    fn recursive_copy_preserves_directory_symlinks() {
        let tmp = TestDir::new("copy-dir-symlink");
        let source = tmp.dir("src");
        let external = tmp.dir("external");
        tmp.file("external/outside.txt");
        std::os::unix::fs::symlink(&external, source.join("external-link")).unwrap();
        let target = tmp.file("target.txt");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());
        assert!(model.focus_path(&source));
        assert!(model.copy_focused());
        assert!(model.focus_path(&target));

        assert!(model.paste_from_clipboard().unwrap());

        let copied_link = tmp.path.join("src copy/external-link");
        assert!(fs::symlink_metadata(&copied_link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_link(copied_link).unwrap(), external);
    }

    #[test]
    fn copy_paste_uses_all_selected_entries() {
        let tmp = TestDir::new("copy-selection");
        let first = tmp.file("a.txt");
        let second = tmp.file("b.txt");
        let target = tmp.dir("target");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());
        assert!(model.toggle_path_selection(&first));
        assert!(model.toggle_path_selection(&second));
        assert!(model.copy_focused());
        assert_eq!(
            model.clipboard.as_ref().unwrap().paths,
            vec![first.clone(), second.clone()]
        );
        assert!(model.focus_path(&target));

        assert!(model.paste_from_clipboard().unwrap());

        assert!(target.join("a.txt").exists());
        assert!(target.join("b.txt").exists());
    }

    #[test]
    fn partial_cut_keeps_unprocessed_entries_in_clipboard() {
        let tmp = TestDir::new("partial-cut");
        let first = tmp.file("a.txt");
        let directory = tmp.dir("source");
        let target = tmp.dir("source/target");
        let control = FileOperationControl::new(2);
        let request = PasteRequest {
            clipboard: FileClipboard {
                operation: ClipboardOperation::Cut,
                paths: vec![first.clone(), directory.clone()],
            },
            target_dir: target.clone(),
        };

        let outcome = execute_paste_with_control(request, &control);

        assert_eq!(
            outcome.error.as_ref().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(!first.exists());
        assert!(target.join("a.txt").exists());
        assert_eq!(
            outcome.clipboard_after.as_ref().unwrap().paths,
            vec![directory]
        );
    }

    #[test]
    fn copy_fallback_is_limited_to_cross_device_rename_errors() {
        let cross_device = io::Error::from_raw_os_error(libc::EXDEV);
        let permission_denied = io::Error::from(io::ErrorKind::PermissionDenied);

        assert!(is_cross_device_error(&cross_device));
        assert!(!is_cross_device_error(&permission_denied));
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
        model.reload_entries_sync().unwrap();
        assert!(model.focus_path(&file));
        assert!(model.copy_focused());
        assert!(model.focus_path(&dest));

        assert!(model.paste_from_clipboard().unwrap());
        model.reload_entries_sync().unwrap();

        let pasted = dest.join("a.txt");
        assert!(pasted.exists());
        assert!(row_paths(&model).contains(&pasted));
    }

    #[test]
    fn behavior_model_focus_and_selection_follow_keyboard_semantics() {
        let tmp = TestDir::new("behavior-model-selection");
        tmp.file("a.txt");
        tmp.file("b.txt");
        tmp.file("c.txt");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());
        model.sync_focus();

        assert_eq!(model.focused.as_ref(), Some(&tmp.path.join("a.txt")));
        assert_eq!(selected_names(&model), Vec::<String>::new());

        assert!(model.move_focus(1));
        assert_eq!(model.focused.as_ref(), Some(&tmp.path.join("b.txt")));
        assert_eq!(selected_names(&model), vec!["b.txt"]);

        assert!(model.extend_selection_focus(1));
        assert_eq!(model.focused.as_ref(), Some(&tmp.path.join("c.txt")));
        assert_eq!(selected_names(&model), vec!["b.txt", "c.txt"]);

        assert!(model.move_focus(-1));
        assert_eq!(model.focused.as_ref(), Some(&tmp.path.join("b.txt")));
        assert_eq!(selected_names(&model), vec!["b.txt"]);
    }

    #[test]
    fn behavior_model_folder_activation_toggles_visible_rows() {
        let tmp = TestDir::new("behavior-model-toggle");
        let src = tmp.dir("src");
        tmp.file("src/main.rs");
        tmp.file("top.txt");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());
        model.sync_focus();

        assert_eq!(row_names(&model), ["src", "top.txt"]);
        assert_eq!(model.focused.as_ref(), Some(&src));

        assert_eq!(model.activate_focused(), FileBrowserActivation::Refresh);
        model.reload_entries_sync().unwrap();
        assert_eq!(row_names(&model), ["src", "main.rs", "top.txt"]);

        assert!(model.collapse_focused());
        assert_eq!(row_names(&model), ["src", "top.txt"]);
    }

    #[test]
    fn behavior_model_copy_cut_paste_and_rename_update_filesystem_state() {
        let tmp = TestDir::new("behavior-model-file-ops");
        let file = tmp.file("a.txt");
        let dest = tmp.dir("dest");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());
        assert!(model.focus_path(&file));
        assert!(model.copy_focused());
        assert!(model.paste_from_clipboard().unwrap());
        model.reload_entries_sync().unwrap();
        assert!(tmp.path.join("a copy.txt").exists());

        assert_eq!(model.focused.as_ref(), Some(&file));
        assert!(model.rename_focused("renamed.txt").is_ok());
        model.reload_entries_sync().unwrap();
        let renamed = tmp.path.join("renamed.txt");
        assert!(!file.exists());
        assert!(renamed.exists());
        assert_eq!(model.focused.as_ref(), Some(&renamed));

        assert!(model.cut_focused());
        assert!(model.focus_path(&dest));
        assert!(model.paste_from_clipboard().unwrap());
        assert!(!renamed.exists());
        assert!(dest.join("renamed.txt").exists());
        assert!(model.clipboard.is_none());
    }

    #[test]
    fn behavior_model_delete_selection_compacts_children_and_restores_focus() {
        let tmp = TestDir::new("behavior-model-delete");
        let folder = tmp.dir("folder");
        let child = tmp.file("folder/child.txt");
        let other = tmp.file("other.txt");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());
        assert!(model.focus_path(&folder));
        assert!(model.expand_focused());
        model.reload_entries_sync().unwrap();
        assert!(model.extend_selection_to_path(&child));

        let targets = model.deletion_targets();
        assert_eq!(targets, vec![folder.clone()]);
        let next_focus = model.focus_candidate_after_removed_paths(&targets);
        assert_eq!(next_focus, Some(other.clone()));

        fs::remove_dir_all(&folder).unwrap();
        model.forget_removed_paths(&targets, next_focus);

        assert!(!folder.exists());
        assert_eq!(selected_names(&model), vec!["other.txt"]);
        assert_eq!(model.focused.as_ref(), Some(&other));
        assert!(!model.expanded.contains(&folder));
    }

    #[test]
    fn behavior_model_failed_delete_keeps_failed_entry_selected() {
        let tmp = TestDir::new("behavior-model-delete-failure");
        let a = tmp.file("a.txt");
        let b = tmp.file("b.txt");
        let c = tmp.file("c.txt");
        let d = tmp.file("d.txt");

        let mut model = FileBrowserModel::default();
        model.set_root(tmp.path.clone());
        assert!(model.focus_path(&a));
        assert!(model.extend_selection_to_path(&c));
        assert_eq!(selected_names(&model), vec!["a.txt", "b.txt", "c.txt"]);

        fs::remove_file(&a).unwrap();
        fs::remove_file(&c).unwrap();
        model.forget_removed_paths(&[a, c], Some(b.clone()));

        assert!(b.exists());
        assert!(d.exists());
        assert_eq!(selected_names(&model), vec!["b.txt"]);
        assert_eq!(model.focused.as_ref(), Some(&b));
    }

    #[cfg(not(target_os = "macos"))]
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

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn panel_paste_refreshes_after_model_borrow_drops() {
        let tmp = TestDir::new("panel-paste");
        tmp.file("a.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.copy_focused();

        panel.paste_from_clipboard();

        wait_until(|| !panel.file_operation_in_progress.get());
        assert!(tmp.path.join("a copy.txt").exists());
        assert_eq!(
            panel.model.borrow().focused.as_ref(),
            Some(&tmp.path.join("a.txt"))
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_arrow_keys_move_custom_focus_without_listbox_selection() {
        let tmp = TestDir::new("behavior-arrows");
        let a = tmp.file("a.txt");
        let b = tmp.file("b.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());

        assert_eq!(panel_focused_path(&panel), Some(a.clone()));
        assert!(panel.list.selected_row().is_none());

        assert_eq!(
            panel.handle_key(gdk::Key::Down, gdk::ModifierType::empty()),
            glib::Propagation::Stop
        );
        assert_eq!(panel_focused_path(&panel), Some(b.clone()));
        assert!(panel.list.selected_row().is_none());

        assert_eq!(
            panel.handle_key(gdk::Key::Up, gdk::ModifierType::empty()),
            glib::Propagation::Stop
        );
        assert_eq!(panel_focused_path(&panel), Some(a));
        assert!(panel.list.selected_row().is_none());
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_left_right_and_enter_toggle_focused_folder() {
        let tmp = TestDir::new("behavior-toggle");
        let src = tmp.dir("src");
        tmp.file("src/main.rs");
        tmp.file("top.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        assert_eq!(panel_focused_path(&panel), Some(src));
        assert_eq!(panel_row_names(&panel), ["src", "top.txt"]);

        assert_eq!(
            panel.handle_key(gdk::Key::Right, gdk::ModifierType::empty()),
            glib::Propagation::Stop
        );
        assert_eq!(panel_row_names(&panel), ["src", "main.rs", "top.txt"]);

        assert_eq!(
            panel.handle_key(gdk::Key::Left, gdk::ModifierType::empty()),
            glib::Propagation::Stop
        );
        assert_eq!(panel_row_names(&panel), ["src", "top.txt"]);

        assert_eq!(
            panel.handle_key(gdk::Key::Return, gdk::ModifierType::empty()),
            glib::Propagation::Stop
        );
        assert_eq!(panel_row_names(&panel), ["src", "main.rs", "top.txt"]);
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_f2_opens_rename_popup_for_focused_entry() {
        let tmp = TestDir::new("behavior-f2");
        tmp.file("old.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());

        assert_eq!(
            panel.handle_key(gdk::Key::F2, gdk::ModifierType::empty()),
            glib::Propagation::Stop
        );

        let rename = gtk::Window::list_toplevels()
            .into_iter()
            .filter_map(|widget| widget.downcast::<gtk::Window>().ok())
            .find(|window| window.title().as_deref() == Some("Rename"))
            .expect("F2 should open a Rename popup");
        let entry = find_entry(rename.upcast_ref()).expect("Rename popup should include an entry");
        assert_eq!(entry.text().as_str(), "old.txt");
        assert_eq!(
            handle_rename_popup_key(&rename, gdk::Key::Escape),
            glib::Propagation::Stop
        );
        assert!(!rename.is_visible());
        close_rename_windows();
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_alt_arrows_and_escape_use_callbacks() {
        let panel = FileBrowserPanel::new();
        let focus_out = Rc::new(RefCell::new(None));
        let closed = Rc::new(Cell::new(false));

        {
            let focus_out = focus_out.clone();
            panel.connect_focus_out(move |dir| *focus_out.borrow_mut() = Some(dir));
        }
        {
            let closed = closed.clone();
            panel.connect_escape(move || closed.set(true));
        }

        assert_eq!(
            panel.handle_key(gdk::Key::Left, gdk::ModifierType::ALT_MASK),
            glib::Propagation::Stop
        );
        assert_eq!(*focus_out.borrow(), Some(FocusDir::Left));

        assert_eq!(
            panel.handle_key(gdk::Key::Back, gdk::ModifierType::ALT_MASK),
            glib::Propagation::Stop
        );
        assert_eq!(*focus_out.borrow(), Some(FocusDir::Left));

        assert_eq!(
            panel.handle_key(gdk::Key::Forward, gdk::ModifierType::ALT_MASK),
            glib::Propagation::Stop
        );
        assert_eq!(*focus_out.borrow(), Some(FocusDir::Right));

        *focus_out.borrow_mut() = None;
        assert_eq!(
            panel.handle_key(
                gdk::Key::Left,
                gdk::ModifierType::ALT_MASK | gdk::ModifierType::CONTROL_MASK
            ),
            glib::Propagation::Proceed
        );
        assert_eq!(*focus_out.borrow(), None);

        assert_eq!(
            panel.handle_key(gdk::Key::Escape, gdk::ModifierType::empty()),
            glib::Propagation::Stop
        );
        assert!(closed.get());
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_ctrl_shift_k_copies_focused_item_path() {
        let tmp = TestDir::new("behavior-copy-path");
        let file = tmp.file("focused.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.focus_path(file.clone());

        let copied = Rc::new(RefCell::new(None));
        let copied_for_writer = copied.clone();
        panel.set_path_clipboard_writer(move |path| {
            *copied_for_writer.borrow_mut() = Some(path.to_string());
        });

        assert_eq!(
            panel.handle_key(
                key("k"),
                gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK
            ),
            glib::Propagation::Stop
        );

        let file_path = file.display().to_string();
        let root_path = tmp.path.display().to_string();
        assert_eq!(copied.borrow().as_deref(), Some(file_path.as_str()));
        assert_ne!(copied.borrow().as_deref(), Some(root_path.as_str()));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_shift_arrows_extend_file_selection() {
        let tmp = TestDir::new("behavior-shift-select");
        tmp.file("a.txt");
        tmp.file("b.txt");
        tmp.file("c.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());

        assert_eq!(
            panel.handle_key(gdk::Key::Down, gdk::ModifierType::SHIFT_MASK),
            glib::Propagation::Stop
        );
        assert_eq!(panel_selected_names(&panel), vec!["a.txt", "b.txt"]);
        assert_eq!(panel_focused_path(&panel), Some(tmp.path.join("b.txt")));
        assert!(panel
            .list
            .row_at_index(0)
            .unwrap()
            .has_css_class("selected"));
        assert!(panel
            .list
            .row_at_index(1)
            .unwrap()
            .has_css_class("selected"));

        assert_eq!(
            panel.handle_key(gdk::Key::Down, gdk::ModifierType::SHIFT_MASK),
            glib::Propagation::Stop
        );
        assert_eq!(
            panel_selected_names(&panel),
            vec!["a.txt", "b.txt", "c.txt"]
        );
        assert_eq!(panel_focused_path(&panel), Some(tmp.path.join("c.txt")));

        assert_eq!(
            panel.handle_key(gdk::Key::Up, gdk::ModifierType::SHIFT_MASK),
            glib::Propagation::Stop
        );
        assert_eq!(panel_selected_names(&panel), vec!["a.txt", "b.txt"]);
        assert_eq!(panel_focused_path(&panel), Some(tmp.path.join("b.txt")));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_plain_arrow_replaces_multi_selection() {
        let tmp = TestDir::new("behavior-plain-select");
        tmp.file("a.txt");
        tmp.file("b.txt");
        tmp.file("c.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());

        panel.handle_key(gdk::Key::Down, gdk::ModifierType::SHIFT_MASK);
        assert_eq!(panel_selected_names(&panel), vec!["a.txt", "b.txt"]);

        assert_eq!(
            panel.handle_key(gdk::Key::Down, gdk::ModifierType::empty()),
            glib::Propagation::Stop
        );
        assert_eq!(panel_selected_names(&panel), vec!["c.txt"]);
        assert_eq!(panel_focused_path(&panel), Some(tmp.path.join("c.txt")));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_ctrl_click_toggles_multi_selection() {
        let tmp = TestDir::new("behavior-ctrl-click-select");
        let a = tmp.file("a.txt");
        let b = tmp.file("b.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.focus_path(a.clone());

        panel.toggle_path_selection(b.clone());
        assert_eq!(panel_selected_names(&panel), vec!["a.txt", "b.txt"]);
        assert!(panel
            .list
            .row_at_index(1)
            .unwrap()
            .has_css_class("selected"));

        panel.toggle_path_selection(a);
        assert_eq!(panel_selected_names(&panel), vec!["b.txt"]);
        assert_eq!(panel_focused_path(&panel), Some(tmp.path.join("a.txt")));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_shift_click_extends_selection_range() {
        let tmp = TestDir::new("behavior-shift-click-select");
        let a = tmp.file("a.txt");
        tmp.file("b.txt");
        let c = tmp.file("c.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.focus_path(a);
        panel.extend_selection_to_path(c);

        assert_eq!(
            panel_selected_names(&panel),
            vec!["a.txt", "b.txt", "c.txt"]
        );
        assert_eq!(panel_focused_path(&panel), Some(tmp.path.join("c.txt")));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_ctrl_copy_paste_refreshes_visible_rows() {
        let tmp = TestDir::new("behavior-copy");
        tmp.file("a.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());

        assert_eq!(
            panel.handle_key(key("c"), gdk::ModifierType::CONTROL_MASK),
            glib::Propagation::Stop
        );
        assert_eq!(
            panel.handle_key(key("v"), gdk::ModifierType::CONTROL_MASK),
            glib::Propagation::Stop
        );

        wait_until(|| !panel.file_operation_in_progress.get());
        assert!(tmp.path.join("a copy.txt").exists());
        assert_eq!(panel_row_names(&panel), ["a copy.txt", "a.txt"]);
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_ctrl_copy_paste_uses_the_selected_range() {
        let tmp = TestDir::new("behavior-copy-selection");
        let first = tmp.file("a.txt");
        let second = tmp.file("b.txt");
        let target = tmp.dir("target");
        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.focus_path(first);
        panel.extend_selection_to_path(second);

        assert_eq!(
            panel.handle_key(key("c"), gdk::ModifierType::CONTROL_MASK),
            glib::Propagation::Stop
        );
        panel.focus_path(target.clone());
        assert_eq!(
            panel.handle_key(key("v"), gdk::ModifierType::CONTROL_MASK),
            glib::Propagation::Stop
        );

        wait_until(|| !panel.file_operation_in_progress.get());
        assert!(target.join("a.txt").exists());
        assert!(target.join("b.txt").exists());
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_ctrl_cut_marks_row_then_paste_moves_and_clears_cut_state() {
        let tmp = TestDir::new("behavior-cut");
        let file = tmp.file("a.txt");
        let dest = tmp.dir("dest");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.focus_path(file.clone());

        assert_eq!(
            panel.handle_key(key("x"), gdk::ModifierType::CONTROL_MASK),
            glib::Propagation::Stop
        );
        assert!(panel.list.row_at_index(1).unwrap().has_css_class("cut"));

        panel.focus_path(dest.clone());
        assert_eq!(
            panel.handle_key(key("v"), gdk::ModifierType::CONTROL_MASK),
            glib::Propagation::Stop
        );

        wait_until(|| !panel.file_operation_in_progress.get());
        assert!(!file.exists());
        assert!(dest.join("a.txt").exists());
        assert!(panel.model.borrow().rows().iter().all(|row| !row.cut));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_rename_preserves_focus_and_scroll_level() {
        let tmp = TestDir::new("behavior-rename-preserve");
        let old = tmp.file("old.txt");
        tmp.file("other.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.focus_path(old);
        set_panel_scroll(&panel, 320.0);

        panel.rename_focused_entry("new.txt").unwrap();

        assert_eq!(panel_focused_path(&panel), Some(tmp.path.join("new.txt")));
        assert_eq!(panel.scroll.vadjustment().value(), 320.0);
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_paste_preserves_focus_and_scroll_level() {
        let tmp = TestDir::new("behavior-paste-preserve");
        let source = tmp.file("source.txt");
        let target = tmp.dir("target");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.focus_path(source);
        panel.copy_focused();
        panel.focus_path(target.clone());
        set_panel_scroll(&panel, 260.0);

        panel.paste_from_clipboard();

        wait_until(|| !panel.file_operation_in_progress.get());
        assert!(target.join("source.txt").exists());
        assert_eq!(panel_focused_path(&panel), Some(target));
        assert_eq!(panel.scroll.vadjustment().value(), 260.0);
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_delete_preserves_scroll_level_while_moving_focus_to_neighbor() {
        let tmp = TestDir::new("behavior-delete-preserve");
        let file = tmp.file("a.txt");
        tmp.file("b.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.focus_path(file.clone());
        set_panel_scroll(&panel, 180.0);
        panel.set_delete_handler(|path| fs::remove_file(path));

        panel.delete_focused_to_trash();

        wait_until(|| !panel.file_operation_in_progress.get());
        assert!(!file.exists());
        assert_eq!(panel_focused_path(&panel), Some(tmp.path.join("b.txt")));
        assert_eq!(panel.scroll.vadjustment().value(), 180.0);
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn keyboard_guard_leaves_navigation_and_delete_for_the_editor() {
        let tmp = TestDir::new("keyboard-guard-editor");
        let first = tmp.file("a.txt");
        tmp.file("b.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root_with_state(tmp.path.clone(), None);
        panel.focus_path(first.clone());
        panel.set_keyboard_input_guard(|| false);

        assert_eq!(
            panel.handle_key(gdk::Key::Down, gdk::ModifierType::empty()),
            glib::Propagation::Proceed
        );
        assert_eq!(panel_focused_path(&panel), Some(first.clone()));
        assert_eq!(
            panel.handle_key(gdk::Key::Delete, gdk::ModifierType::empty()),
            glib::Propagation::Proceed
        );
        assert!(first.exists());
    }

    #[test]
    fn native_editor_focus_disables_file_browser_keyboard_input() {
        assert!(!file_browser_accepts_keyboard_input(true));
        assert!(file_browser_accepts_keyboard_input(false));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_delete_moves_focused_entry_to_trash_and_refreshes_visible_rows() {
        let tmp = TestDir::new("behavior-delete");
        let file = tmp.file("a.txt");
        tmp.file("b.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.focus_path(file.clone());

        let deleted = Arc::new(std::sync::Mutex::new(None));
        let deleted_for_handler = deleted.clone();
        panel.set_delete_handler(move |path| {
            *deleted_for_handler.lock().unwrap() = Some(path.to_path_buf());
            fs::remove_file(path)
        });

        assert_eq!(
            panel.handle_key(gdk::Key::Delete, gdk::ModifierType::empty()),
            glib::Propagation::Stop
        );

        wait_until(|| !panel.file_operation_in_progress.get());
        assert_eq!(deleted.lock().unwrap().as_ref(), Some(&file));
        assert!(!file.exists());
        assert_eq!(panel_row_names(&panel), vec!["b.txt"]);
        assert_eq!(panel_focused_path(&panel), Some(tmp.path.join("b.txt")));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_delete_removes_all_ctrl_selected_entries_in_one_pass() {
        let tmp = TestDir::new("behavior-delete-ctrl-multi");
        let a = tmp.file("a.txt");
        let b = tmp.file("b.txt");
        let c = tmp.file("c.txt");
        let d = tmp.file("d.txt");
        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.focus_path(a.clone());
        panel.toggle_path_selection(c.clone());
        assert_eq!(panel_selected_names(&panel), vec!["a.txt", "c.txt"]);

        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let deleted_for_handler = deleted.clone();
        panel.set_delete_handler(move |path| {
            deleted_for_handler.lock().unwrap().push(display_name(path));
            fs::remove_file(path)
        });

        panel.delete_focused_to_trash();

        wait_until(|| !panel.file_operation_in_progress.get());
        assert_eq!(deleted.lock().unwrap().as_slice(), ["a.txt", "c.txt"]);
        assert!(!a.exists());
        assert!(b.exists());
        assert!(!c.exists());
        assert!(d.exists());
        assert_eq!(panel_selected_names(&panel), vec!["b.txt"]);
        assert_eq!(panel_focused_path(&panel), Some(b.clone()));

        panel.delete_focused_to_trash();

        wait_until(|| !panel.file_operation_in_progress.get());
        assert_eq!(
            deleted.lock().unwrap().as_slice(),
            ["a.txt", "c.txt", "b.txt"]
        );
        assert!(!b.exists());
        assert!(d.exists());
        assert_eq!(panel_selected_names(&panel), vec!["d.txt"]);
        assert_eq!(panel_focused_path(&panel), Some(d));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_delete_removes_shift_selected_range_in_visible_order() {
        let tmp = TestDir::new("behavior-delete-shift-range");
        let a = tmp.file("a.txt");
        let b = tmp.file("b.txt");
        let c = tmp.file("c.txt");
        let d = tmp.file("d.txt");
        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.focus_path(a.clone());
        assert_eq!(
            panel.handle_key(gdk::Key::Down, gdk::ModifierType::SHIFT_MASK),
            glib::Propagation::Stop
        );
        assert_eq!(
            panel.handle_key(gdk::Key::Down, gdk::ModifierType::SHIFT_MASK),
            glib::Propagation::Stop
        );
        assert_eq!(
            panel_selected_names(&panel),
            vec!["a.txt", "b.txt", "c.txt"]
        );

        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let deleted_for_handler = deleted.clone();
        panel.set_delete_handler(move |path| {
            deleted_for_handler.lock().unwrap().push(display_name(path));
            fs::remove_file(path)
        });

        panel.delete_focused_to_trash();

        wait_until(|| !panel.file_operation_in_progress.get());
        assert_eq!(
            deleted.lock().unwrap().as_slice(),
            ["a.txt", "b.txt", "c.txt"]
        );
        assert!(!a.exists());
        assert!(!b.exists());
        assert!(!c.exists());
        assert!(d.exists());
        assert_eq!(panel_selected_names(&panel), vec!["d.txt"]);
        assert_eq!(panel_focused_path(&panel), Some(d));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_delete_compacts_selected_parent_and_child() {
        let tmp = TestDir::new("behavior-delete-parent-child");
        let folder = tmp.dir("folder");
        let child = tmp.file("folder/child.txt");
        let other = tmp.file("other.txt");
        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.focus_path(folder.clone());
        panel.expand_focused();
        panel.extend_selection_to_path(child);
        assert_eq!(panel_selected_names(&panel), vec!["folder", "child.txt"]);

        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let deleted_for_handler = deleted.clone();
        panel.set_delete_handler(move |path| {
            deleted_for_handler.lock().unwrap().push(display_name(path));
            if path.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            }
        });

        panel.delete_focused_to_trash();

        wait_until(|| !panel.file_operation_in_progress.get());
        assert_eq!(deleted.lock().unwrap().as_slice(), ["folder"]);
        assert!(!folder.exists());
        assert!(other.exists());
        assert_eq!(panel_selected_names(&panel), vec!["other.txt"]);
        assert_eq!(panel_focused_path(&panel), Some(other));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_delete_keeps_failed_selected_entries_selected() {
        let tmp = TestDir::new("behavior-delete-partial-failure");
        let a = tmp.file("a.txt");
        let b = tmp.file("b.txt");
        let c = tmp.file("c.txt");
        let d = tmp.file("d.txt");
        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.focus_path(a.clone());
        panel.extend_selection_to_path(c.clone());
        assert_eq!(
            panel_selected_names(&panel),
            vec!["a.txt", "b.txt", "c.txt"]
        );

        let attempted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let attempted_for_handler = attempted.clone();
        let failed = b.clone();
        panel.set_delete_handler(move |path| {
            attempted_for_handler
                .lock()
                .unwrap()
                .push(display_name(path));
            if path == failed {
                Err(io::Error::other("blocked"))
            } else {
                fs::remove_file(path)
            }
        });

        panel.delete_focused_to_trash();

        wait_until(|| !panel.file_operation_in_progress.get());
        assert_eq!(
            attempted.lock().unwrap().as_slice(),
            ["a.txt", "b.txt", "c.txt"]
        );
        assert!(!a.exists());
        assert!(b.exists());
        assert!(!c.exists());
        assert!(d.exists());
        assert_eq!(panel_selected_names(&panel), vec!["b.txt"]);
        assert_eq!(panel_focused_path(&panel), Some(b));
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_shift_delete_requires_confirmation_before_permanent_delete() {
        let tmp = TestDir::new("behavior-shift-delete");
        let file = tmp.file("a.txt");
        tmp.file("b.txt");

        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.focus_path(file.clone());

        assert_eq!(
            panel.handle_key(gdk::Key::Delete, gdk::ModifierType::SHIFT_MASK),
            glib::Propagation::Stop
        );
        assert!(file.exists());

        let popup = gtk::Window::list_toplevels()
            .into_iter()
            .filter_map(|widget| widget.downcast::<gtk::Window>().ok())
            .find(|window| window.title().as_deref() == Some("Delete permanently?"))
            .expect("Shift+Delete should open a permanent delete confirmation popup");
        let yes = find_button(popup.upcast_ref(), "Yes")
            .expect("permanent delete confirmation should include a Yes button");

        yes.emit_clicked();

        wait_until(|| !panel.file_operation_in_progress.get());
        assert!(!file.exists());
        assert_eq!(panel_row_names(&panel), vec!["b.txt"]);
        assert_eq!(panel_focused_path(&panel), Some(tmp.path.join("b.txt")));
        close_delete_windows();
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn behavior_permanent_delete_removes_multi_selection() {
        let tmp = TestDir::new("behavior-permanent-delete-multi");
        let a = tmp.file("a.txt");
        let b = tmp.file("b.txt");
        let c = tmp.file("c.txt");
        let d = tmp.file("d.txt");
        let panel = FileBrowserPanel::new();
        panel.show_for_root(tmp.path.clone());
        panel.focus_path(a.clone());
        panel.toggle_path_selection(c.clone());
        assert_eq!(panel_selected_names(&panel), vec!["a.txt", "c.txt"]);

        let targets = panel.model.borrow().deletion_targets();
        panel.delete_paths_permanently(targets);

        wait_until(|| !panel.file_operation_in_progress.get());
        assert!(!a.exists());
        assert!(b.exists());
        assert!(!c.exists());
        assert!(d.exists());
        assert_eq!(panel_selected_names(&panel), vec!["b.txt"]);
        assert_eq!(panel_focused_path(&panel), Some(b));
    }

    #[test]
    fn permanently_delete_path_removes_files_and_directories() {
        let tmp = TestDir::new("permanent-delete");
        let file = tmp.file("file.txt");
        let dir = tmp.dir("dir");
        fs::write(dir.join("child.txt"), "child").unwrap();

        permanently_delete_path(&file).unwrap();
        permanently_delete_path(&dir).unwrap();

        assert!(!file.exists());
        assert!(!dir.exists());
    }
}
