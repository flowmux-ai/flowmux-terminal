// SPDX-License-Identifier: GPL-3.0-or-later
//! Browser pane placeholder for targets without WebKitGTK.

use crate::ui::terminal_pane::PaneCallbacks;
use flowmux_browser::{RefScope, RefStore};
use flowmux_config::options::BrowserEngine;
use flowmux_core::{PaneId, SurfaceId};
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

const UNSUPPORTED: &str =
    "browser tabs require WebKitGTK and are currently available only on Linux builds";

#[derive(Clone)]
pub struct BrowserPane {
    pub root: gtk::Box,
    pub web_view: gtk::Widget,
    pub refs: Rc<RefCell<RefStore>>,
    pub ref_scope: RefScope,
}

pub fn ref_scope_for_surface(surface_id: SurfaceId) -> RefScope {
    RefScope::from_u128(surface_id.0.as_u128())
}

pub struct NativeBrowserViewsSuspend;

pub fn suspend_native_browser_views_for_window(_window: &gtk::Window) -> NativeBrowserViewsSuspend {
    NativeBrowserViewsSuspend
}

impl BrowserPane {
    pub fn new(
        _id: PaneId,
        surface_id: SurfaceId,
        initial_url: Option<&str>,
        callbacks: PaneCallbacks,
        _engine: BrowserEngine,
        _persist_session: bool,
    ) -> Self {
        let _ = callbacks.on_browser_uri_changed.clone();
        let _ = callbacks.on_browser_title_changed.clone();

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);

        let chrome = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        chrome.set_margin_top(4);
        chrome.set_margin_bottom(4);
        chrome.set_margin_start(4);
        chrome.set_margin_end(4);

        let address = gtk::Entry::new();
        address.set_hexpand(true);
        address.set_sensitive(false);
        address.set_text(initial_url.unwrap_or("about:blank"));
        chrome.append(&address);

        let message = gtk::Label::new(Some(UNSUPPORTED));
        message.set_wrap(true);
        message.set_halign(gtk::Align::Center);
        message.set_valign(gtk::Align::Center);
        message.set_hexpand(true);
        message.set_vexpand(true);
        message.add_css_class("dim-label");

        root.append(&chrome);
        root.append(&message);

        Self {
            root,
            web_view: message.upcast::<gtk::Widget>(),
            refs: Rc::new(RefCell::new(RefStore::new())),
            ref_scope: ref_scope_for_surface(surface_id),
        }
    }

    pub fn current_url(&self) -> String {
        String::new()
    }

    pub fn current_title(&self) -> String {
        String::new()
    }

    pub fn load_uri(&self, _url: &str) {}

    pub fn go_back(&self) -> bool {
        false
    }

    pub fn go_forward(&self) -> bool {
        false
    }

    pub fn reload(&self) {}

    pub fn stop_loading(&self) {}

    pub fn grab_focus(&self) {
        self.web_view.grab_focus();
    }

    pub fn focus_widget(&self) -> gtk::Widget {
        self.web_view.clone()
    }

    pub fn set_zoom_level(&self, _zoom: f64) {}

    pub fn snapshot_to_png<F: FnOnce(Result<String, String>) + 'static>(
        &self,
        _path: std::path::PathBuf,
        on_done: F,
    ) {
        on_done(Err(UNSUPPORTED.into()));
    }

    pub fn evaluate_js<F: FnOnce(Result<String, String>) + 'static>(
        &self,
        _source: &str,
        on_done: F,
    ) {
        on_done(Err(UNSUPPORTED.into()));
    }
}
