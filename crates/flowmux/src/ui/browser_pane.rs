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
//!
//! Options model: upstream cmux uses a single engine (WKWebView) and only
//! separates `WKWebsiteDataStore` by profile (`Sources/Panels/BrowserPanel.swift:443`).
//! flowmux follows the same model: every tab renders with the single WebKitGTK
//! 6.0 engine, while the option labels (WebKit / Chrome / Firefox / Custom)
//! map to [`BrowserProfile`] values that isolate cookies, localStorage, and
//! IndexedDB directories.

use crate::ui::terminal_pane::PaneCallbacks;
use flowmux_browser::{BrowserProfile, RefScope, RefStore};
use flowmux_config::options::BrowserEngine;
use flowmux_core::{PaneId, SurfaceId};
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use webkit6::prelude::*;

#[derive(Clone)]
pub struct BrowserPane {
    pub id: PaneId,
    pub root: gtk::Box,
    pub web_view: webkit6::WebView,
    pub address_bar: gtk::Entry,
    /// cmux-style server-side ref store. Each snapshot clears + repopulates
    /// the entry for this pane; subsequent click/fill/etc. resolve their
    /// `eN` ref through this map to a CSS selector before injecting JS.
    pub refs: Rc<RefCell<RefStore>>,
    /// Scope key — derived from the surface id so multiple browser
    /// surfaces in the same pane keep their refs separate.
    pub ref_scope: RefScope,
}

/// Build a [`RefScope`] from a [`SurfaceId`]. The scope is just the
/// surface uuid as u128 — opaque to the store, stable across calls.
pub fn ref_scope_for_surface(surface_id: SurfaceId) -> RefScope {
    RefScope::from_u128(surface_id.0.as_u128())
}

impl BrowserPane {
    pub fn new(
        id: PaneId,
        surface_id: SurfaceId,
        initial_url: Option<&str>,
        callbacks: PaneCallbacks,
        engine: BrowserEngine,
    ) -> Self {
        // BrowserEngine labels affect only WebsiteDataStore isolation, matching
        // upstream cmux. Every tab renders through the same WebKitGTK engine.
        // Map them 1:1 to flowmux-browser::BrowserProfile to split data dirs.
        let profile = engine_to_profile(&engine);
        tracing::debug!(
            engine = ?engine,
            profile = ?profile,
            "creating browser pane (WebKitGTK + profile-isolated NetworkSession)"
        );
        // Idempotent WebKit sandbox bypass. main.rs sets the same env var,
        // but unit tests can build BrowserPane through the lib path without
        // entering main.rs, so set it again here for consistent behavior.
        // See the matching set_var comment in main.rs for the background.
        if std::env::var_os("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS").is_none() {
            std::env::set_var("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "1");
        }

        // Build a per-profile NetworkSession. Default reuses WebKit's global
        // default session and persists in the standard system location. Other
        // profiles live under `$XDG_DATA_HOME/flowmux/browser/<slug>/` so cookies
        // and localStorage do not mix inside one flowmux instance.
        let network_session = build_network_session(&profile);
        let web_view = webkit6::WebView::builder()
            .network_session(&network_session)
            // Some environments inherited muted=true from GtkWindow and muted
            // video playback. Request false in the builder and enforce it once
            // more immediately after build.
            .is_muted(false)
            .build();
        webkit6::prelude::WebViewExt::set_is_muted(&web_view, false);
        web_view.set_hexpand(true);
        web_view.set_vexpand(true);

        // Map the core options from cmux's `configureWebViewConfiguration`
        // (BrowserPanel.swift:2586-) to WebKitGTK Settings:
        //   * mediaTypesRequiringUserActionForPlayback = []
        //         → media-playback-requires-user-gesture = false
        //         + media-playback-allows-inline = true
        //   * developerExtrasEnabled = true
        //         → enable-developer-extras = true
        //   * isElementFullscreenEnabled = true
        //         → enable-fullscreen = true
        //   * defaultWebpagePreferences.allowsContentJavaScript = true
        //         -> enable-javascript = true (WebKitGTK default)
        //
        // Also enable media-related options that WebKitGTK leaves disabled by
        // default. cmux's WKWebView only omits them because macOS WebKit has
        // them enabled by default, so set them explicitly for parity:
        //   * enable-mediasource (adaptive streaming such as HLS / DASH)
        //   * enable-encrypted-media (DRM, such as Netflix/Disney+)
        //   * enable-webaudio (audio contexts)
        //   * hardware-acceleration-policy = ALWAYS (GPU video decode)
        // A freshly created WebView should always have settings. Unwrap the
        // Option conservatively; if WebKit ever returns None, media options
        // fall back to system defaults.
        if let Some(settings) = webkit6::prelude::WebViewExt::settings(&web_view) {
            settings.set_media_playback_requires_user_gesture(false);
            settings.set_media_playback_allows_inline(true);
            settings.set_enable_developer_extras(true);
            settings.set_enable_fullscreen(true);
            settings.set_enable_javascript(true);
            // enable-media is the WebKitGTK 6.0 master switch for the
            // audio/video element pipeline. If false, the GStreamer audio sink
            // may never attach, leaving video visible but silent.
            settings.set_enable_media(true);
            settings.set_enable_mediasource(true);
            settings.set_enable_encrypted_media(true);
            settings.set_enable_webaudio(true);
            // HTML5 storage is true by default, but make it explicit for site compatibility.
            settings.set_enable_html5_local_storage(true);
            settings.set_enable_html5_database(true);
            // Keep GPU acceleration for all pages. The shutdown race around
            // missing `eglDestroySync` / `corrupted size vs. prev_size` is
            // blocked in main.rs by disabling the DMA-BUF renderer. webkit6
            // 0.4 exposes only Always / Never, not ON_DEMAND, and Never would
            // also lose video acceleration.
            settings.set_hardware_acceleration_policy(webkit6::HardwareAccelerationPolicy::Always);
        } else {
            tracing::warn!("WebView::settings() returned None — media options skipped");
        }

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

        // Reflect navigation in the address bar AND mirror the new URL
        // back to the daemon so state can restore the last page on next launch.
        {
            let a = address.clone();
            let uri_cb = callbacks.on_browser_uri_changed.clone();
            web_view.connect_uri_notify(move |w| {
                if let Some(uri) = w.uri() {
                    let uri_str = uri.to_string();
                    a.set_text(&uri_str);
                    (uri_cb.borrow_mut())(id, surface_id, uri_str);
                }
            });
        }

        // Report browser page title changes so the surface tab name can update.
        // The daemon ignores them for title-locked user-renamed surfaces.
        {
            let title_cb = callbacks.on_browser_title_changed.clone();
            web_view.connect_title_notify(move |w| {
                let title = w.title().map(|t| t.to_string()).unwrap_or_default();
                if !title.trim().is_empty() {
                    (title_cb.borrow_mut())(id, surface_id, title);
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
            refs: Rc::new(RefCell::new(RefStore::new())),
            ref_scope: ref_scope_for_surface(surface_id),
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

/// Map [`BrowserEngine`] labels saved by the options dialog to
/// flowmux-browser [`BrowserProfile`] values 1:1. The mapping is semantic;
/// every result still renders through the same WebKitGTK engine.
///
/// * `Webkit` -> `Default` (flowmux default data directory)
/// * `Chrome` -> `ChromeImport` (Chromium-family cookie import slot)
/// * `Firefox` -> `FirefoxImport` (Firefox cookie import slot)
/// * `Custom { name }` -> `Custom { name }` (user-defined isolation slot)
fn engine_to_profile(engine: &BrowserEngine) -> BrowserProfile {
    match engine {
        BrowserEngine::Webkit => BrowserProfile::Default,
        BrowserEngine::Chrome => BrowserProfile::ChromeImport,
        BrowserEngine::Firefox => BrowserProfile::FirefoxImport,
        BrowserEngine::Custom { name } => BrowserProfile::Custom { name: name.clone() },
    }
}

/// Build and return a profile-specific [`webkit6::NetworkSession`].
///
/// * [`BrowserProfile::Default`] reuses the system-managed global default
///   session, sharing the cookie pool with other running flowmux instances in
///   the same spirit as cmux's sharedProcessPool.
/// * Other profiles create persistent NetworkSession data + cache directories
///   under `$XDG_DATA_HOME/flowmux/browser/<slug>/`. If directory creation fails,
///   warn and fall back to the global default session so pages still load.
fn build_network_session(profile: &BrowserProfile) -> webkit6::NetworkSession {
    match profile {
        BrowserProfile::Default => webkit6::NetworkSession::default()
            .unwrap_or_else(|| webkit6::NetworkSession::new(None, None)),
        other => match other.data_dir() {
            Ok(dir) => {
                let dir_str = dir.to_string_lossy().into_owned();
                webkit6::NetworkSession::new(Some(&dir_str), Some(&dir_str))
            }
            Err(e) => {
                tracing::warn!(
                    profile = ?profile,
                    error = %e,
                    "browser profile data dir unavailable, falling back to default session"
                );
                webkit6::NetworkSession::default()
                    .unwrap_or_else(|| webkit6::NetworkSession::new(None, None))
            }
        },
    }
}
