// SPDX-License-Identifier: GPL-3.0-or-later
//! Flowmux editor surface host.
//!
//! The initial implementation owns the pane identity and stable GTK root used
//! by surface lifecycle operations. The Monaco WebView replaces the placeholder
//! child without changing the registry contract.

use flowmux_core::PaneId;
use gtk::prelude::*;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

#[derive(Clone)]
pub struct EditorPane {
    pane_id: Rc<Cell<PaneId>>,
    workspace_root: PathBuf,
    pub root: gtk::Box,
}

impl EditorPane {
    pub fn new(pane_id: PaneId, workspace_root: PathBuf) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.set_focusable(true);
        root.set_halign(gtk::Align::Fill);
        root.set_valign(gtk::Align::Fill);

        let title = gtk::Label::new(Some("Editor"));
        title.add_css_class("title-2");
        let path = gtk::Label::new(Some(&workspace_root.display().to_string()));
        path.add_css_class("dim-label");
        path.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        root.append(&title);
        root.append(&path);

        Self {
            pane_id: Rc::new(Cell::new(pane_id)),
            workspace_root,
            root,
        }
    }

    pub fn pane_id(&self) -> PaneId {
        self.pane_id.get()
    }

    pub fn set_pane_id(&self, pane_id: PaneId) {
        self.pane_id.set(pane_id);
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn focus_widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    pub fn grab_focus(&self) {
        self.root.grab_focus();
    }

    pub fn prepare_for_close(&self) {}
}
