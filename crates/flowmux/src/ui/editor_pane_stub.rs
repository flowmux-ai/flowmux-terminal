// SPDX-License-Identifier: GPL-3.0-or-later

use flowmux_core::{EditorSessionState, PaneId, SurfaceId};
use flowmux_editor::{HostMessage, ProtocolError};
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
    pub fn new(
        pane_id: PaneId,
        _surface_id: SurfaceId,
        workspace_root: PathBuf,
        _restored: EditorSessionState,
    ) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        let label = gtk::Label::new(Some("The embedded editor is unavailable on this platform."));
        root.append(&label);
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

    pub fn session_state(&self) -> EditorSessionState {
        EditorSessionState::default()
    }

    pub fn focus_widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    pub fn grab_focus(&self) {
        self.root.grab_focus();
    }

    pub fn open_file(&self, _path: &Path) -> Result<(), String> {
        Err("the embedded editor is unavailable on this platform".into())
    }

    pub fn dirty_document_paths(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    pub fn save_all_dirty(&self) -> Result<(), String> {
        Ok(())
    }

    pub fn discard_all_dirty(&self) {}

    pub fn send(&self, message: HostMessage) -> Result<(), ProtocolError> {
        flowmux_editor::serialize_host_message("unavailable", &message).map(|_| ())
    }

    pub fn prepare_for_close(&self) {}
}
