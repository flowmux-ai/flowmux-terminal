// SPDX-License-Identifier: GPL-3.0-or-later

use gtk::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

#[derive(Clone, Debug, PartialEq, Eq)]
enum DownloadPhase {
    InProgress,
    Cancelling,
    Complete,
    Cancelled,
    Failed(String),
}

impl DownloadPhase {
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Complete | Self::Cancelled | Self::Failed(_))
    }
}

#[derive(Clone, Debug)]
struct DownloadLifecycle {
    phase: DownloadPhase,
}

impl Default for DownloadLifecycle {
    fn default() -> Self {
        Self {
            phase: DownloadPhase::InProgress,
        }
    }
}

impl DownloadLifecycle {
    fn phase(&self) -> &DownloadPhase {
        &self.phase
    }

    fn request_cancel(&mut self) -> bool {
        if self.phase != DownloadPhase::InProgress {
            return false;
        }
        self.phase = DownloadPhase::Cancelling;
        true
    }

    fn finish(&mut self) -> bool {
        if self.phase.is_terminal() {
            return false;
        }
        self.phase = if self.phase == DownloadPhase::Cancelling {
            DownloadPhase::Cancelled
        } else {
            DownloadPhase::Complete
        };
        true
    }

    fn fail(&mut self, error: String) -> bool {
        if self.phase.is_terminal() {
            return false;
        }
        self.phase = if self.phase == DownloadPhase::Cancelling {
            DownloadPhase::Cancelled
        } else {
            DownloadPhase::Failed(error)
        };
        true
    }
}

#[derive(Default)]
struct DownloadCollection {
    next_id: u64,
    active_count: usize,
    entries: HashMap<u64, DownloadLifecycle>,
}

impl DownloadCollection {
    fn insert(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.active_count += 1;
        self.entries.insert(id, DownloadLifecycle::default());
        id
    }

    fn request_cancel(&mut self, id: u64) -> bool {
        self.entries
            .get_mut(&id)
            .is_some_and(DownloadLifecycle::request_cancel)
    }

    fn finish(&mut self, id: u64) -> bool {
        let transitioned = self
            .entries
            .get_mut(&id)
            .is_some_and(DownloadLifecycle::finish);
        if transitioned {
            self.active_count -= 1;
        }
        transitioned
    }

    fn fail(&mut self, id: u64, error: String) -> bool {
        let transitioned = self
            .entries
            .get_mut(&id)
            .is_some_and(|entry| entry.fail(error));
        if transitioned {
            self.active_count -= 1;
        }
        transitioned
    }

    fn remove_terminal(&mut self, id: u64) -> bool {
        if !self
            .entries
            .get(&id)
            .is_some_and(|entry| entry.phase().is_terminal())
        {
            return false;
        }
        self.entries.remove(&id);
        true
    }

    fn clear_terminal(&mut self) -> Vec<u64> {
        let mut terminal: Vec<_> = self
            .entries
            .iter()
            .filter_map(|(id, entry)| entry.phase().is_terminal().then_some(*id))
            .collect();
        terminal.sort_unstable();
        for id in &terminal {
            self.entries.remove(id);
        }
        terminal
    }

    fn active_count(&self) -> usize {
        self.active_count
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn phase(&self, id: u64) -> Option<&DownloadPhase> {
        self.entries.get(&id).map(DownloadLifecycle::phase)
    }

    fn has_terminal(&self) -> bool {
        self.entries
            .values()
            .any(|entry| entry.phase().is_terminal())
    }
}

#[derive(Clone)]
pub(crate) struct DownloadManager {
    inner: Rc<DownloadManagerInner>,
}

struct DownloadManagerInner {
    button: gtk::MenuButton,
    #[cfg_attr(not(test), allow(dead_code))]
    scroll: gtk::ScrolledWindow,
    list: gtk::Box,
    empty: gtk::Label,
    clear: gtk::Button,
    collection: RefCell<DownloadCollection>,
    rows: RefCell<HashMap<u64, DownloadRow>>,
}

struct DownloadRow {
    root: gtk::Box,
    name: gtk::Label,
    progress: gtk::ProgressBar,
    open: gtk::Button,
    folder: gtk::Button,
    cancel: gtk::Button,
    remove: gtk::Button,
    destination: Rc<RefCell<Option<PathBuf>>>,
    cancel_action: Rc<dyn Fn()>,
}

#[derive(Clone)]
pub(crate) struct DownloadItem {
    inner: Weak<DownloadManagerInner>,
    id: u64,
}

impl DownloadManager {
    pub(crate) fn new() -> Self {
        let button = gtk::MenuButton::builder()
            .icon_name("folder-download-symbolic")
            .tooltip_text("Downloads")
            .build();

        let clear = gtk::Button::with_label("Clear all");
        clear.add_css_class("flat");
        clear.set_halign(gtk::Align::End);
        clear.set_sensitive(false);
        clear.set_tooltip_text(Some("Clear completed, cancelled, and failed downloads"));

        let list = gtk::Box::new(gtk::Orientation::Vertical, 8);
        list.set_margin_top(8);
        list.set_margin_bottom(8);
        list.set_margin_start(8);
        list.set_margin_end(8);
        let empty = gtk::Label::new(Some("No downloads yet"));
        empty.add_css_class("dim-label");
        list.append(&empty);

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroll.set_max_content_height(420);
        scroll.set_propagate_natural_height(true);
        scroll.set_child(Some(&list));

        let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
        root.append(&clear);
        root.append(&scroll);
        let popover = gtk::Popover::new();
        popover.set_child(Some(&root));
        button.set_popover(Some(&popover));

        let inner = Rc::new(DownloadManagerInner {
            button,
            scroll,
            list,
            empty,
            clear,
            collection: RefCell::new(DownloadCollection::default()),
            rows: RefCell::new(HashMap::new()),
        });
        let weak_inner = Rc::downgrade(&inner);
        inner.clear.connect_clicked(move |_| {
            if let Some(inner) = weak_inner.upgrade() {
                inner.clear_terminal();
            }
        });
        Self { inner }
    }

    pub(crate) fn button(&self) -> gtk::MenuButton {
        self.inner.button.clone()
    }

    pub(crate) fn add<F>(&self, cancel_action: F) -> DownloadItem
    where
        F: Fn() + 'static,
    {
        let id = self.inner.collection.borrow_mut().insert();
        let name = gtk::Label::new(Some("Preparing download…"));
        name.set_halign(gtk::Align::Start);
        name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        let progress = gtk::ProgressBar::new();
        progress.set_hexpand(true);

        let details = gtk::Box::new(gtk::Orientation::Vertical, 4);
        details.append(&name);
        details.append(&progress);
        let open = gtk::Button::new();
        open.set_hexpand(true);
        open.set_halign(gtk::Align::Fill);
        open.set_sensitive(false);
        open.set_child(Some(&details));

        let folder = gtk::Button::from_icon_name("folder-open-symbolic");
        folder.set_tooltip_text(Some("Show in folder"));
        folder.set_sensitive(false);
        let cancel = gtk::Button::from_icon_name("process-stop-symbolic");
        cancel.set_tooltip_text(Some("Cancel download"));
        let remove = gtk::Button::from_icon_name("edit-delete-symbolic");
        remove.set_tooltip_text(Some("Remove from list"));
        remove.set_visible(false);

        let root = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        root.append(&open);
        root.append(&folder);
        root.append(&cancel);
        root.append(&remove);
        self.inner.list.append(&root);

        let destination = Rc::new(RefCell::new(None));
        let cancel_action: Rc<dyn Fn()> = Rc::new(cancel_action);
        self.inner.rows.borrow_mut().insert(
            id,
            DownloadRow {
                root,
                name,
                progress,
                open: open.clone(),
                folder: folder.clone(),
                cancel: cancel.clone(),
                remove: remove.clone(),
                destination,
                cancel_action,
            },
        );

        {
            let inner = Rc::downgrade(&self.inner);
            cancel.connect_clicked(move |_| {
                if let Some(inner) = inner.upgrade() {
                    inner.request_cancel(id);
                }
            });
        }
        {
            let inner = Rc::downgrade(&self.inner);
            remove.connect_clicked(move |_| {
                if let Some(inner) = inner.upgrade() {
                    inner.remove_terminal(id);
                }
            });
        }
        {
            let inner = Rc::downgrade(&self.inner);
            open.connect_clicked(move |_| {
                if let Some(inner) = inner.upgrade() {
                    inner.open_file(id);
                }
            });
        }
        {
            let inner = Rc::downgrade(&self.inner);
            folder.connect_clicked(move |_| {
                if let Some(inner) = inner.upgrade() {
                    inner.show_in_folder(id);
                }
            });
        }

        self.inner.refresh_summary();
        DownloadItem {
            inner: Rc::downgrade(&self.inner),
            id,
        }
    }

    #[cfg(test)]
    fn scroll_policy(&self) -> gtk::PolicyType {
        self.inner.scroll.hscrollbar_policy()
    }

    #[cfg(test)]
    fn max_content_height(&self) -> i32 {
        self.inner.scroll.max_content_height()
    }

    #[cfg(test)]
    fn propagates_natural_height(&self) -> bool {
        self.inner.scroll.propagates_natural_height()
    }

    #[cfg(test)]
    fn entry_count(&self) -> usize {
        self.inner.collection.borrow().len()
    }

    #[cfg(test)]
    fn active_count(&self) -> usize {
        self.inner.collection.borrow().active_count()
    }

    #[cfg(test)]
    fn tooltip_text(&self) -> String {
        self.inner
            .button
            .tooltip_text()
            .map(|text| text.to_string())
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn empty_visible(&self) -> bool {
        self.inner.empty.property("visible")
    }

    #[cfg(test)]
    fn clear_terminal(&self) {
        self.inner.clear_terminal();
    }
}

impl DownloadItem {
    pub(crate) fn set_destination(&self, destination: &std::path::Path) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let rows = inner.rows.borrow();
        let Some(row) = rows.get(&self.id) else {
            return;
        };
        row.destination.replace(Some(destination.to_path_buf()));
        row.name.set_text(
            destination
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("download"),
        );
        row.name
            .set_tooltip_text(Some(&destination.display().to_string()));
    }

    pub(crate) fn set_progress(&self, fraction: f64) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let rows = inner.rows.borrow();
        if let Some(row) = rows.get(&self.id) {
            row.progress.set_fraction(fraction.clamp(0.0, 1.0));
        }
    }

    #[cfg(test)]
    fn request_cancel(&self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.request_cancel(self.id);
        }
    }

    pub(crate) fn finish(&self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.finish(self.id);
        }
    }

    pub(crate) fn fail(&self, error: String) {
        if let Some(inner) = self.inner.upgrade() {
            inner.fail(self.id, error);
        }
    }

    #[cfg(test)]
    fn status_text(&self) -> String {
        let Some(inner) = self.inner.upgrade() else {
            return String::new();
        };
        let rows = inner.rows.borrow();
        rows.get(&self.id)
            .and_then(|row| row.progress.text())
            .map(|text| text.to_string())
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn name_text(&self) -> String {
        let Some(inner) = self.inner.upgrade() else {
            return String::new();
        };
        let rows = inner.rows.borrow();
        rows.get(&self.id)
            .map(|row| row.name.text().to_string())
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn progress_fraction(&self) -> f64 {
        let Some(inner) = self.inner.upgrade() else {
            return 0.0;
        };
        let rows = inner.rows.borrow();
        rows.get(&self.id)
            .map(|row| row.progress.fraction())
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn file_actions_enabled(&self) -> bool {
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        let rows = inner.rows.borrow();
        rows.get(&self.id)
            .is_some_and(|row| row.open.is_sensitive() && row.folder.is_sensitive())
    }

    #[cfg(test)]
    fn remove_from_list(&self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.remove_terminal(self.id);
        }
    }

    #[cfg(test)]
    fn click_open(&self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let rows = inner.rows.borrow();
        if let Some(row) = rows.get(&self.id) {
            row.open.emit_clicked();
        }
    }

    #[cfg(test)]
    fn click_folder(&self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let rows = inner.rows.borrow();
        if let Some(row) = rows.get(&self.id) {
            row.folder.emit_clicked();
        }
    }
}

impl DownloadManagerInner {
    fn request_cancel(&self, id: u64) {
        if !self.collection.borrow_mut().request_cancel(id) {
            return;
        }
        if let Some(row) = self.rows.borrow().get(&id) {
            row.progress.set_text(Some("Cancelling…"));
            row.progress.set_show_text(true);
            row.cancel.set_sensitive(false);
            (row.cancel_action)();
        }
    }

    fn finish(&self, id: u64) {
        let phase = {
            let mut collection = self.collection.borrow_mut();
            if !collection.finish(id) {
                return;
            }
            collection.phase(id).cloned()
        };
        if let Some(phase) = phase {
            self.render_terminal(id, &phase);
        }
    }

    fn fail(&self, id: u64, error: String) {
        let phase = {
            let mut collection = self.collection.borrow_mut();
            if !collection.fail(id, error) {
                return;
            }
            collection.phase(id).cloned()
        };
        if let Some(phase) = phase {
            self.render_terminal(id, &phase);
        }
    }

    fn render_terminal(&self, id: u64, phase: &DownloadPhase) {
        let rows = self.rows.borrow();
        let Some(row) = rows.get(&id) else {
            return;
        };
        row.progress.set_show_text(true);
        match phase {
            DownloadPhase::Complete => {
                row.progress.set_fraction(1.0);
                row.progress.set_text(Some("Complete"));
                let has_destination = row.destination.borrow().is_some();
                row.open.set_sensitive(has_destination);
                row.folder.set_sensitive(has_destination);
            }
            DownloadPhase::Cancelled => row.progress.set_text(Some("Cancelled")),
            DownloadPhase::Failed(error) => {
                row.progress.set_text(Some(&format!("Failed: {error}")))
            }
            DownloadPhase::InProgress | DownloadPhase::Cancelling => return,
        }
        row.cancel.set_visible(false);
        row.remove.set_visible(true);
        self.refresh_summary();
    }

    fn remove_terminal(&self, id: u64) {
        if !self.collection.borrow_mut().remove_terminal(id) {
            return;
        }
        if let Some(row) = self.rows.borrow_mut().remove(&id) {
            self.list.remove(&row.root);
        }
        self.refresh_summary();
    }

    fn open_file(&self, id: u64) {
        if let Some(path) = self.destination_for_action(id) {
            crate::ui::file_browser::open_file(&path);
        }
    }

    fn show_in_folder(&self, id: u64) {
        let Some(path) = self.destination_for_action(id) else {
            return;
        };
        if let Some(parent) = path.parent() {
            crate::ui::show_in_folder::open_directory(parent);
        }
    }

    fn destination_for_action(&self, id: u64) -> Option<PathBuf> {
        let rows = self.rows.borrow();
        let row = rows.get(&id)?;
        let path = row.destination.borrow().clone()?;
        if path.is_file() {
            return Some(path);
        }
        tracing::warn!(path = %path.display(), "downloaded file no longer exists");
        row.progress.set_text(Some("File not found"));
        row.progress.set_show_text(true);
        row.open.set_sensitive(false);
        row.folder.set_sensitive(false);
        None
    }

    fn clear_terminal(&self) {
        let ids = self.collection.borrow_mut().clear_terminal();
        let mut rows = self.rows.borrow_mut();
        for id in ids {
            if let Some(row) = rows.remove(&id) {
                self.list.remove(&row.root);
            }
        }
        drop(rows);
        self.refresh_summary();
    }

    fn refresh_summary(&self) {
        let collection = self.collection.borrow();
        let active = collection.active_count();
        self.empty.set_visible(collection.len() == 0);
        self.clear.set_sensitive(collection.has_terminal());
        if active == 0 {
            self.button.set_tooltip_text(Some("Downloads"));
        } else {
            self.button
                .set_tooltip_text(Some(&format!("Downloads — {active} in progress")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn cancelled_finish_never_becomes_complete() {
        let mut lifecycle = DownloadLifecycle::default();
        assert!(lifecycle.request_cancel());
        assert!(lifecycle.finish());
        assert_eq!(lifecycle.phase(), &DownloadPhase::Cancelled);
    }

    #[test]
    fn cancelled_failure_is_reported_as_cancelled() {
        let mut lifecycle = DownloadLifecycle::default();
        lifecycle.request_cancel();
        assert!(lifecycle.fail("network stopped".into()));
        assert_eq!(lifecycle.phase(), &DownloadPhase::Cancelled);
    }

    #[test]
    fn failure_cannot_be_overwritten_by_finished() {
        let mut lifecycle = DownloadLifecycle::default();
        assert!(lifecycle.fail("connection reset".into()));
        assert!(!lifecycle.finish());
        assert_eq!(
            lifecycle.phase(),
            &DownloadPhase::Failed("connection reset".into())
        );
    }

    #[test]
    fn normal_finish_is_complete() {
        let mut lifecycle = DownloadLifecycle::default();
        assert!(lifecycle.finish());
        assert_eq!(lifecycle.phase(), &DownloadPhase::Complete);
    }

    #[test]
    fn overlapping_downloads_decrement_active_count_once() {
        let mut collection = DownloadCollection::default();
        let first = collection.insert();
        let second = collection.insert();
        assert_eq!(collection.active_count(), 2);
        assert!(collection.finish(first));
        assert_eq!(collection.active_count(), 1);
        assert!(!collection.finish(first));
        assert_eq!(collection.active_count(), 1);
        assert!(collection.fail(second, "offline".into()));
        assert_eq!(collection.active_count(), 0);
    }

    #[test]
    fn clear_terminal_keeps_active_entries() {
        let mut collection = DownloadCollection::default();
        let active = collection.insert();
        let finished = collection.insert();
        collection.finish(finished);
        assert_eq!(collection.clear_terminal(), vec![finished]);
        assert_eq!(collection.len(), 1);
        assert!(!collection.remove_terminal(active));
        assert_eq!(collection.active_count(), 1);
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn manager_uses_bounded_vertical_scroller() {
        if gtk::init().is_err() {
            return;
        }
        let manager = DownloadManager::new();
        assert_eq!(manager.scroll_policy(), gtk::PolicyType::Never);
        assert_eq!(manager.max_content_height(), 420);
        assert!(manager.propagates_natural_height());
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn clear_all_keeps_active_download_rows() {
        if gtk::init().is_err() {
            return;
        }
        let manager = DownloadManager::new();
        let active = manager.add(|| {});
        let complete = manager.add(|| {});
        complete.finish();
        assert_eq!(manager.entry_count(), 2);
        manager.clear_terminal();
        assert_eq!(manager.entry_count(), 1);
        assert_eq!(manager.active_count(), 1);
        active.fail("cleanup".into());
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn cancel_then_finished_renders_cancelled_and_runs_cancel_once() {
        if gtk::init().is_err() {
            return;
        }
        let cancelled = Rc::new(Cell::new(0));
        let cancelled_for_action = cancelled.clone();
        let manager = DownloadManager::new();
        let item = manager.add(move || cancelled_for_action.set(cancelled_for_action.get() + 1));

        item.request_cancel();
        item.request_cancel();
        item.finish();

        assert_eq!(cancelled.get(), 1);
        assert_eq!(item.status_text(), "Cancelled");
        assert_eq!(manager.active_count(), 0);
        assert_eq!(manager.tooltip_text(), "Downloads");
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn failed_then_finished_adapter_order_preserves_failure() {
        if gtk::init().is_err() {
            return;
        }
        let manager = DownloadManager::new();
        let item = manager.add(|| {});
        item.fail("connection reset".into());
        item.finish();

        assert_eq!(item.status_text(), "Failed: connection reset");
        assert_eq!(manager.active_count(), 0);
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn destination_and_progress_enable_file_actions_only_after_completion() {
        if gtk::init().is_err() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.txt");
        std::fs::write(&path, "downloaded").unwrap();
        let manager = DownloadManager::new();
        let item = manager.add(|| {});

        item.set_destination(&path);
        item.set_progress(0.5);
        assert_eq!(item.name_text(), "report.txt");
        assert_eq!(item.progress_fraction(), 0.5);
        assert!(!item.file_actions_enabled());

        item.finish();
        assert!(item.file_actions_enabled());
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn individual_removal_rejects_active_and_removes_terminal_row() {
        if gtk::init().is_err() {
            return;
        }
        let manager = DownloadManager::new();
        let item = manager.add(|| {});

        item.remove_from_list();
        assert_eq!(manager.entry_count(), 1);
        item.finish();
        item.remove_from_list();

        assert_eq!(manager.entry_count(), 0);
        assert!(manager.empty_visible());
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn opening_a_missing_completed_file_reports_missing() {
        if gtk::init().is_err() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.txt");
        let manager = DownloadManager::new();
        let item = manager.add(|| {});
        item.set_destination(&path);
        item.finish();

        item.click_open();

        assert_eq!(item.status_text(), "File not found");
        assert!(!item.file_actions_enabled());
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn revealing_a_missing_completed_file_reports_missing() {
        if gtk::init().is_err() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.txt");
        let manager = DownloadManager::new();
        let item = manager.add(|| {});
        item.set_destination(&path);
        item.finish();

        item.click_folder();

        assert_eq!(item.status_text(), "File not found");
        assert!(!item.file_actions_enabled());
    }
}
