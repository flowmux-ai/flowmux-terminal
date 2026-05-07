// SPDX-License-Identifier: GPL-3.0-or-later
//! WebKitGTK 6.0 in-app browser pane.
//!
//! Replaces the macOS WKWebView path. Each pane owns:
//!
//! * a [`webkit::WebView`] for rendering;
//! * a small chrome row (back / forward / reload / address bar);
//! * a scriptable API entry point — `evaluate_javascript_async` is
//!   already exposed by webkit6, so the Task 15 work mostly involves
//!   wrapping it in a stable IPC verb shape, not new widgets.

use flowmux_core::PaneId;
use gtk::prelude::*;
use webkit6::prelude::*;

#[derive(Clone)]
pub struct BrowserPane {
    pub id: PaneId,
    pub root: gtk::Box,
    pub web_view: webkit6::WebView,
    pub address_bar: gtk::Entry,
}

impl BrowserPane {
    pub fn new(id: PaneId, initial_url: Option<&str>) -> Self {
        let web_view = webkit6::WebView::new();
        web_view.set_hexpand(true);
        web_view.set_vexpand(true);

        let back = gtk::Button::from_icon_name("go-previous-symbolic");
        let forward = gtk::Button::from_icon_name("go-next-symbolic");
        let reload = gtk::Button::from_icon_name("view-refresh-symbolic");
        let address = gtk::Entry::new();
        address.set_hexpand(true);
        address.set_placeholder_text(Some("Enter URL — e.g. http://localhost:3000"));

        let chrome = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        chrome.set_margin_top(4);
        chrome.set_margin_bottom(4);
        chrome.set_margin_start(4);
        chrome.set_margin_end(4);
        chrome.append(&back);
        chrome.append(&forward);
        chrome.append(&reload);
        chrome.append(&address);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.append(&chrome);
        root.append(&web_view);

        // Wire chrome buttons.
        {
            let v = web_view.clone();
            back.connect_clicked(move |_| {
                if v.can_go_back() {
                    v.go_back();
                }
            });
        }
        {
            let v = web_view.clone();
            forward.connect_clicked(move |_| {
                if v.can_go_forward() {
                    v.go_forward();
                }
            });
        }
        {
            let v = web_view.clone();
            reload.connect_clicked(move |_| v.reload());
        }
        {
            let v = web_view.clone();
            let a = address.clone();
            address.connect_activate(move |_| {
                let raw = a.text().to_string();
                let uri = normalize_uri(&raw);
                v.load_uri(&uri);
            });
        }

        // Reflect navigation in the address bar.
        {
            let a = address.clone();
            web_view.connect_uri_notify(move |w| {
                if let Some(uri) = w.uri() {
                    a.set_text(uri.as_str());
                }
            });
        }

        if let Some(url) = initial_url {
            let normalized = normalize_uri(url);
            address.set_text(&normalized);
            web_view.load_uri(&normalized);
        } else {
            web_view.load_uri("about:blank");
        }

        Self {
            id,
            root,
            web_view,
            address_bar: address,
        }
    }

    pub fn navigate(&self, url: &str) {
        self.web_view.load_uri(url);
    }

    /// Move backwards in session history. Returns false if there's
    /// nothing to go back to.
    pub fn back(&self) -> bool {
        if self.web_view.can_go_back() {
            self.web_view.go_back();
            true
        } else {
            false
        }
    }

    pub fn forward(&self) -> bool {
        if self.web_view.can_go_forward() {
            self.web_view.go_forward();
            true
        } else {
            false
        }
    }

    pub fn reload(&self) {
        self.web_view.reload();
    }

    pub fn current_url(&self) -> String {
        self.web_view
            .uri()
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    pub fn current_title(&self) -> String {
        self.web_view
            .title()
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    /// Run JS and call `on_done` with the JS result string. The
    /// scriptable API wraps this with a oneshot channel that the IPC
    /// handler awaits.
    pub fn evaluate_js<F: FnOnce(Result<String, String>) + 'static>(
        &self,
        source: &str,
        on_done: F,
    ) {
        self.web_view.evaluate_javascript(
            source,
            None,
            None,
            gtk::gio::Cancellable::NONE,
            move |result| {
                let r = match result {
                    Ok(value) => Ok(value.to_str().to_string()),
                    Err(e) => Err(e.to_string()),
                };
                on_done(r);
            },
        );
    }
}

fn normalize_uri(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return "about:blank".into();
    }
    if raw.starts_with("http://")
        || raw.starts_with("https://")
        || raw.starts_with("about:")
        || raw.starts_with("file://")
    {
        return raw.to_string();
    }
    if raw.starts_with("localhost") || raw.starts_with("127.") || raw.starts_with("[::1]") {
        return format!("http://{raw}");
    }
    if raw.contains('.') && !raw.contains(' ') {
        return format!("https://{raw}");
    }
    format!("https://duckduckgo.com/?q={}", urlencode(raw))
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".into(),
            other => format!("%{:02X}", other),
        })
        .collect()
}
