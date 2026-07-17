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

use crate::ui::browser_bookmarks::BookmarkMenu;
use crate::ui::browser_downloads::DownloadManager;
use crate::ui::pane_terminal::PaneCallbacks;
use adw::prelude::*;
use flowmux_browser::{BrowserProfile, RefScope, RefStore};
use flowmux_config::options::BrowserEngine;
use flowmux_core::{PaneId, SurfaceId};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use webkit6::prelude::*;

#[derive(Clone)]
pub struct BrowserPane {
    pane_id: Rc<Cell<PaneId>>,
    pub root: gtk::Box,
    pub web_view: webkit6::WebView,
    zoom: Rc<Cell<f64>>,
    zoom_label: gtk::Button,
    find_entry: gtk::SearchEntry,
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

pub struct NativeBrowserViewsSuspend;

pub fn suspend_native_browser_views_for_window(_window: &gtk::Window) -> NativeBrowserViewsSuspend {
    NativeBrowserViewsSuspend
}

impl BrowserPane {
    pub fn new(
        id: PaneId,
        surface_id: SurfaceId,
        initial_url: Option<&str>,
        callbacks: PaneCallbacks,
        engine: BrowserEngine,
        persist_session: bool,
    ) -> Self {
        let pane_id = Rc::new(Cell::new(id));
        // BrowserEngine labels affect only WebsiteDataStore isolation, matching
        // upstream cmux. Every tab renders through the same WebKitGTK engine.
        // Map them 1:1 to flowmux-browser::BrowserProfile to split data dirs.
        let profile = engine_to_profile(&engine);
        tracing::debug!(
            engine = ?engine,
            profile = ?profile,
            persist_session,
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
        let network_session = build_network_session(&profile, persist_session);
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
            settings.set_javascript_can_open_windows_automatically(true);
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
            //
            // Escape hatch: on hosts where WebKit's web process aborts with
            // `Could not create default EGL display: EGL_BAD_PARAMETER`
            // (e.g. Ubuntu 22.04 + flatpak: host Mesa is too old for the
            // newer Mesa shipped in org.freedesktop.Platform.GL//24.08),
            // setting FLOWMUX_WEBKIT_HW_ACCEL=never makes WebKit skip the
            // EGL setup entirely and CPU-rasterise. Pages render; video
            // accel is lost.
            let hw_accel_policy = match std::env::var("FLOWMUX_WEBKIT_HW_ACCEL")
                .as_deref()
                .map(str::trim)
            {
                Ok("never") | Ok("Never") | Ok("NEVER") => {
                    tracing::info!(
                        "FLOWMUX_WEBKIT_HW_ACCEL=never set; disabling WebKit GPU acceleration"
                    );
                    webkit6::HardwareAccelerationPolicy::Never
                }
                _ => webkit6::HardwareAccelerationPolicy::Always,
            };
            settings.set_hardware_acceleration_policy(hw_accel_policy);
        } else {
            tracing::warn!("WebView::settings() returned None — media options skipped");
        }

        {
            let pane_id = pane_id.clone();
            let open_url = callbacks.on_open_url.clone();
            web_view.connect_create(move |parent, navigation_action| {
                let url = navigation_action
                    .clone()
                    .request()
                    .and_then(|request| request.uri())
                    .map(|uri| uri.to_string());
                if let Some(url) = url {
                    (open_url.borrow_mut())(pane_id.get(), url);
                }

                // WebKit requires a related WebView return value before it
                // completes window.open(). The real destination is routed to
                // a FlowMux browser tab above, so keep this contract-only view
                // hidden and block its duplicate navigation.
                //
                // Tearing the view down on the next main-loop turn is NOT
                // safe: disposing a WebKitWebViewBase whose web process is
                // still handshaking nulls its AcceleratedBackingStore while
                // an accelerated-compositing update is in flight, and the
                // unguarded update() call segfaults the whole GUI process
                // (WebKitGTK 2.52, UIProcess/gtk — release-build ASSERT).
                // Instead, ask the view to close only once WebKit reports it
                // ready (or after a grace period), and drop the last strong
                // ref only from the `close` signal so dispose always runs on
                // a page that finished the close handshake.
                let placeholder = webkit6::WebView::builder().related_view(parent).build();
                placeholder.connect_decide_policy(|_, decision, _| {
                    decision.ignore();
                    true
                });
                let keep_alive = Rc::new(RefCell::new(Some(placeholder.clone())));
                {
                    let keep_alive = keep_alive.clone();
                    placeholder.connect_close(move |_| {
                        keep_alive.borrow_mut().take();
                    });
                }
                placeholder.connect_ready_to_show(|view| view.try_close());
                let weak = placeholder.downgrade();
                gtk::glib::timeout_add_local_once(std::time::Duration::from_secs(10), move || {
                    if let Some(view) = weak.upgrade() {
                        view.try_close();
                    }
                });
                placeholder.upcast()
            });
        }
        web_view.connect_permission_request(move |web_view, request| {
            let Some(parent) = web_view.root() else {
                request.deny();
                return true;
            };
            let permission = permission_name(request);
            let site = web_view
                .uri()
                .and_then(|uri| gtk::glib::Uri::parse(&uri, gtk::glib::UriFlags::NONE).ok())
                .and_then(|uri| uri.host().map(|host| host.to_string()))
                .unwrap_or_else(|| "This page".into());
            let dialog = adw::AlertDialog::new(
                Some(&format!("Allow {permission}?")),
                Some(&format!("{site} is requesting access.")),
            );
            dialog.add_response("deny", "Deny");
            dialog.add_response("allow", "Allow");
            dialog.set_default_response(Some("deny"));
            dialog.set_close_response("deny");
            dialog.set_response_appearance("allow", adw::ResponseAppearance::Suggested);

            let request = request.clone();
            dialog.connect_response(None, move |dialog, response| {
                if response == "allow" {
                    request.allow();
                } else {
                    request.deny();
                }
                dialog.close();
            });
            dialog.present(Some(&parent));
            true
        });

        let back = gtk::Button::from_icon_name("go-previous-symbolic");
        let forward = gtk::Button::from_icon_name("go-next-symbolic");
        let reload = gtk::Button::from_icon_name("view-refresh-symbolic");
        let find = gtk::Button::from_icon_name("edit-find-symbolic");
        find.set_tooltip_text(Some("Find in page"));
        let zoom_out = gtk::Button::from_icon_name("zoom-out-symbolic");
        zoom_out.set_tooltip_text(Some("Zoom out"));
        let zoom_reset = gtk::Button::with_label("100%");
        zoom_reset.set_tooltip_text(Some("Reset page zoom"));
        let zoom_in = gtk::Button::from_icon_name("zoom-in-symbolic");
        zoom_in.set_tooltip_text(Some("Zoom in"));
        let zoom = Rc::new(Cell::new(1.0));
        let zoom_label = zoom_reset.clone();
        let address = gtk::Entry::new();
        address.set_hexpand(true);
        address.set_placeholder_text(Some("Enter URL — e.g. http://localhost:3000"));
        // Tool icon on the right side of the URL bar opens the WebKit
        // Web Inspector as a detached (separate-window) popup.
        // applications-utilities-symbolic ships with Adwaita and reads
        // as a "toolbox / dev tools" glyph in both light and dark.
        let inspector = gtk::Button::from_icon_name("applications-utilities-symbolic");
        inspector.add_css_class("flat");
        inspector.set_tooltip_text(Some("Open Web Inspector"));
        let downloads = DownloadManager::new();
        let bookmarks = BookmarkMenu::new(
            &profile,
            {
                let web_view = web_view.downgrade();
                Rc::new(move || {
                    let web_view = web_view.upgrade()?;
                    let url = web_view.uri()?.to_string();
                    let title = web_view
                        .title()
                        .map(|title| title.to_string())
                        .filter(|title| !title.trim().is_empty())
                        .unwrap_or_else(|| url.clone());
                    Some(flowmux_browser::Bookmark { title, url })
                })
            },
            {
                let web_view = web_view.downgrade();
                Rc::new(move |url| {
                    if let Some(web_view) = web_view.upgrade() {
                        web_view.load_uri(url);
                    }
                })
            },
        );
        bookmarks.button.set_tooltip_text(Some(&format!(
            "Bookmarks\nProfile: {}\n{}",
            profile.display_name(),
            if persist_session {
                "Cookies and site data are saved in this browser profile"
            } else {
                "Cookies and site data are discarded when flowmux exits"
            }
        )));

        let chrome = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        chrome.set_margin_top(4);
        chrome.set_margin_bottom(4);
        chrome.set_margin_start(4);
        chrome.set_margin_end(4);
        chrome.append(&back);
        chrome.append(&forward);
        chrome.append(&reload);
        chrome.append(&address);
        chrome.append(&bookmarks.button);
        chrome.append(&find);
        chrome.append(&zoom_out);
        chrome.append(&zoom_reset);
        chrome.append(&zoom_in);
        chrome.append(&downloads.button());
        chrome.append(&inspector);

        {
            let downloads = downloads.clone();
            let download_directory = download_directory();
            network_session.connect_download_started(move |_, download| {
                let native_for_cancel = download.clone();
                let item = downloads.add(move || native_for_cancel.cancel());
                {
                    let directory = download_directory.clone();
                    let item = item.clone();
                    download.connect_decide_destination(move |download, suggested| {
                        if let Err(error) = std::fs::create_dir_all(&directory) {
                            item.fail(error.to_string());
                            return false;
                        }
                        let destination = available_download_path(&directory, suggested);
                        item.set_destination(&destination);
                        download.set_destination(&destination.to_string_lossy());
                        true
                    });
                }
                {
                    let item = item.clone();
                    download.connect_estimated_progress_notify(move |download| {
                        item.set_progress(download.estimated_progress());
                    });
                }
                {
                    let item = item.clone();
                    download.connect_finished(move |_| {
                        item.finish();
                    });
                }
                {
                    let item = item.clone();
                    download.connect_failed(move |_, error| {
                        item.fail(error.to_string());
                    });
                }
            });
        }

        let find_entry = gtk::SearchEntry::builder()
            .placeholder_text("Find in page…")
            .visible(false)
            .build();
        find_entry.set_margin_start(4);
        find_entry.set_margin_end(4);
        find_entry.set_margin_bottom(4);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.append(&chrome);
        root.append(&find_entry);
        root.append(&web_view);

        // Wire chrome buttons.
        {
            let v = web_view.downgrade();
            back.connect_clicked(move |_| {
                let Some(v) = v.upgrade() else {
                    return;
                };
                if v.can_go_back() {
                    v.go_back();
                }
            });
        }
        {
            let v = web_view.downgrade();
            forward.connect_clicked(move |_| {
                let Some(v) = v.upgrade() else {
                    return;
                };
                if v.can_go_forward() {
                    v.go_forward();
                }
            });
        }
        {
            let v = web_view.downgrade();
            reload.connect_clicked(move |_| {
                if let Some(v) = v.upgrade() {
                    v.reload();
                }
            });
        }
        {
            let entry = find_entry.downgrade();
            find.connect_clicked(move |_| {
                let Some(entry) = entry.upgrade() else {
                    return;
                };
                let visible = !entry.is_visible();
                entry.set_visible(visible);
                if visible {
                    entry.grab_focus();
                }
            });
        }
        {
            let v = web_view.downgrade();
            let zoom = zoom.clone();
            let label = zoom_label.downgrade();
            zoom_out.connect_clicked(move |_| {
                let (Some(v), Some(label)) = (v.upgrade(), label.upgrade()) else {
                    return;
                };
                set_webkit_zoom(&v, &zoom, &label, zoom.get() - 0.1);
            });
        }
        {
            let v = web_view.downgrade();
            let zoom = zoom.clone();
            zoom_reset.connect_clicked(move |label| {
                let Some(v) = v.upgrade() else {
                    return;
                };
                set_webkit_zoom(&v, &zoom, label, 1.0);
            });
        }
        {
            let v = web_view.downgrade();
            let zoom = zoom.clone();
            let label = zoom_label.downgrade();
            zoom_in.connect_clicked(move |_| {
                let (Some(v), Some(label)) = (v.upgrade(), label.upgrade()) else {
                    return;
                };
                set_webkit_zoom(&v, &zoom, &label, zoom.get() + 0.1);
            });
        }
        {
            let v = web_view.downgrade();
            find_entry.connect_search_changed(move |entry| {
                let Some(v) = v.upgrade() else {
                    return;
                };
                let Some(controller) = v.find_controller() else {
                    return;
                };
                let text = entry.text();
                if text.is_empty() {
                    controller.search_finish();
                } else {
                    controller.search(
                        &text,
                        (webkit6::FindOptions::CASE_INSENSITIVE
                            | webkit6::FindOptions::WRAP_AROUND)
                            .bits(),
                        u32::MAX,
                    );
                }
            });
        }
        {
            let v = web_view.downgrade();
            find_entry.connect_activate(move |_| {
                let Some(v) = v.upgrade() else {
                    return;
                };
                if let Some(controller) = v.find_controller() {
                    controller.search_next();
                }
            });
        }
        {
            let v = web_view.downgrade();
            find_entry.connect_stop_search(move |entry| {
                if let Some(v) = v.upgrade() {
                    if let Some(controller) = v.find_controller() {
                        controller.search_finish();
                    }
                    v.grab_focus();
                }
                entry.set_visible(false);
            });
        }
        {
            let v = web_view.downgrade();
            inspector.connect_clicked(move |_| {
                let Some(v) = v.upgrade() else {
                    return;
                };
                if let Some(insp) = v.inspector() {
                    insp.show();
                    insp.detach();
                } else {
                    tracing::warn!("WebKit Web Inspector not available on this build");
                }
            });
        }
        {
            let v = web_view.downgrade();
            address.connect_activate(move |address| {
                let Some(v) = v.upgrade() else {
                    return;
                };
                let uri = normalize_uri(&address.text());
                v.load_uri(&uri);
            });
        }

        // Reflect navigation in the address bar AND mirror the new URL
        // back to the daemon so state can restore the last page on next launch.
        {
            let address = address.downgrade();
            let uri_cb = callbacks.on_browser_uri_changed.clone();
            let pane_id = pane_id.clone();
            web_view.connect_uri_notify(move |web_view| {
                if let Some(uri) = web_view.uri() {
                    let uri = uri.to_string();
                    if let Some(address) = address.upgrade() {
                        address.set_text(&uri);
                    }
                    (uri_cb.borrow_mut())(pane_id.get(), surface_id, uri);
                }
            });
        }

        // Report browser page title changes so the surface tab name can update.
        // The daemon ignores them for title-locked user-renamed surfaces.
        {
            let title_cb = callbacks.on_browser_title_changed.clone();
            let pane_id = pane_id.clone();
            web_view.connect_title_notify(move |w| {
                let title = w.title().map(|t| t.to_string()).unwrap_or_default();
                if !title.trim().is_empty() {
                    (title_cb.borrow_mut())(pane_id.get(), surface_id, title);
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
            pane_id,
            root,
            web_view,
            zoom,
            zoom_label,
            find_entry,
            refs: Rc::new(RefCell::new(RefStore::new())),
            ref_scope: ref_scope_for_surface(surface_id),
        }
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

    pub fn load_uri(&self, url: &str) {
        self.web_view.load_uri(url);
    }

    pub fn go_back(&self) -> bool {
        let moved = self.web_view.can_go_back();
        if moved {
            self.web_view.go_back();
        }
        moved
    }

    pub fn go_forward(&self) -> bool {
        let moved = self.web_view.can_go_forward();
        if moved {
            self.web_view.go_forward();
        }
        moved
    }

    pub fn reload(&self) {
        self.web_view.reload();
    }

    pub fn stop_loading(&self) {
        self.web_view.stop_loading();
    }

    pub fn prepare_for_close(&self) {
        self.stop_loading();
        self.web_view.terminate_web_process();
    }

    pub fn grab_focus(&self) {
        self.web_view.grab_focus();
    }

    pub fn show_search(&self) {
        self.find_entry.set_visible(true);
        self.find_entry.grab_focus();
        self.find_entry.select_region(0, -1);
    }

    pub fn pane_id_handle(&self) -> Rc<Cell<PaneId>> {
        self.pane_id.clone()
    }

    pub fn set_pane_id(&self, id: PaneId) {
        self.pane_id.set(id);
    }

    pub fn focus_widget(&self) -> gtk::Widget {
        self.web_view.clone().upcast::<gtk::Widget>()
    }

    pub fn set_zoom_level(&self, zoom: f64) {
        set_webkit_zoom(&self.web_view, &self.zoom, &self.zoom_label, zoom);
    }

    pub fn snapshot_to_png<F: FnOnce(Result<String, String>) + 'static>(
        &self,
        path: std::path::PathBuf,
        on_done: F,
    ) {
        self.web_view.snapshot(
            webkit6::SnapshotRegion::Visible,
            webkit6::SnapshotOptions::NONE,
            gtk::gio::Cancellable::NONE,
            move |result| {
                let mapped = match result {
                    Ok(texture) => texture
                        .save_to_png(&path)
                        .map(|_| path.display().to_string())
                        .map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                on_done(mapped);
            },
        );
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

fn set_webkit_zoom(
    web_view: &webkit6::WebView,
    zoom: &Cell<f64>,
    label: &gtk::Button,
    requested: f64,
) {
    let level = requested.clamp(0.5, 2.0);
    zoom.set(level);
    label.set_label(&format!("{:.0}%", level * 100.0));
    web_view.set_zoom_level(level);
}

fn download_directory() -> std::path::PathBuf {
    gtk::glib::user_special_dir(gtk::glib::UserDirectory::Downloads)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join("Downloads"))
        })
        .unwrap_or_else(std::env::temp_dir)
}

fn permission_name(request: &webkit6::PermissionRequest) -> String {
    if let Some(media) = request.downcast_ref::<webkit6::UserMediaPermissionRequest>() {
        let mut devices = Vec::new();
        if webkit6::functions::user_media_permission_is_for_audio_device(media) {
            devices.push("microphone");
        }
        if webkit6::functions::user_media_permission_is_for_video_device(media) {
            devices.push("camera");
        }
        if webkit6::functions::user_media_permission_is_for_display_device(media) {
            devices.push("screen sharing");
        }
        return if devices.is_empty() {
            "media devices".into()
        } else {
            devices.join(" and ")
        };
    }
    if request.type_().name() == "WebKitClipboardPermissionRequest" {
        "clipboard".into()
    } else if request.is::<webkit6::GeolocationPermissionRequest>() {
        "location".into()
    } else if request.is::<webkit6::NotificationPermissionRequest>() {
        "notifications".into()
    } else if request.is::<webkit6::PointerLockPermissionRequest>() {
        "pointer lock".into()
    } else if request.is::<webkit6::DeviceInfoPermissionRequest>() {
        "device information".into()
    } else if request.is::<webkit6::WebsiteDataAccessPermissionRequest>() {
        "cross-site data".into()
    } else if request.is::<webkit6::MediaKeySystemPermissionRequest>() {
        "protected media".into()
    } else {
        "site permission".into()
    }
}

fn available_download_path(directory: &std::path::Path, suggested: &str) -> std::path::PathBuf {
    let file_name = std::path::Path::new(suggested)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("download");
    let candidate = directory.join(file_name);
    if !candidate.exists() {
        return candidate;
    }

    let path = std::path::Path::new(file_name);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    let extension = path.extension().and_then(|value| value.to_str());
    for number in 1.. {
        let numbered = match extension {
            Some(extension) => format!("{stem} ({number}).{extension}"),
            None => format!("{stem} ({number})"),
        };
        let candidate = directory.join(numbered);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
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
/// `persist_session` controls whether site state survives a flowmux restart:
///
/// * `true` — every profile (including [`BrowserProfile::Default`]) writes
///   into `$XDG_DATA_HOME/flowmux/browser/<slug>/` and the session's
///   `CookieManager` is wired to a sqlite file under that directory via
///   [`set_cookie_persistent_storage`]. WebKitGTK does not enable cookie
///   persistence for a fresh session by default, so the explicit call is
///   what makes login cookies survive a quit/relaunch — without it, only
///   localStorage / IndexedDB persisted, which is why most logins were
///   forgotten.
/// * `false` — return [`webkit6::NetworkSession::new_ephemeral`] regardless
///   of profile, so cookies, localStorage, IndexedDB, and cache all live
///   in memory and are dropped on quit.
///
/// If a persistent profile's data dir cannot be created, fall back to the
/// global default session so pages still load (with a warning).
fn build_network_session(
    profile: &BrowserProfile,
    persist_session: bool,
) -> webkit6::NetworkSession {
    if !persist_session {
        tracing::debug!(
            profile = ?profile,
            "browser session persistence disabled — using ephemeral NetworkSession"
        );
        return webkit6::NetworkSession::new_ephemeral();
    }

    match profile.data_dir() {
        Ok(dir) => {
            let dir_str = dir.to_string_lossy().into_owned();
            let session = webkit6::NetworkSession::new(Some(&dir_str), Some(&dir_str));
            set_cookie_persistent_storage(&session, &dir);
            session
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
    }
}

/// Wire the session's [`webkit6::CookieManager`] to a sqlite file at
/// [`cookies_sqlite_path`]. WebKitGTK's [`CookieManager`] keeps cookies
/// in memory until this is called, which is the root cause of the "I had
/// to log in again after restarting flowmux" report — cookies were the only
/// piece of site state not persisted by [`webkit6::NetworkSession::new`].
///
/// Logs a warning and leaves cookies in-memory if the manager is missing
/// (should never happen for a freshly created persistent session).
fn set_cookie_persistent_storage(session: &webkit6::NetworkSession, data_dir: &std::path::Path) {
    match session.cookie_manager() {
        Some(cm) => {
            let cookies_path = cookies_sqlite_path(data_dir);
            let cookies_str = cookies_path.to_string_lossy().into_owned();
            cm.set_persistent_storage(&cookies_str, webkit6::CookiePersistentStorage::Sqlite);
        }
        None => {
            tracing::warn!(
                data_dir = %data_dir.display(),
                "NetworkSession has no CookieManager — cookies will not persist"
            );
        }
    }
}

/// `<data_dir>/cookies.sqlite` — the file flowmux hands to
/// [`webkit6::CookieManager::set_persistent_storage`] when session
/// persistence is enabled. Kept as a pure helper so the path layout can be
/// asserted without spinning up WebKit.
pub(crate) fn cookies_sqlite_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("cookies.sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_browser_pane_releases_root_and_web_view() {
        // GTK binds to its first test thread. The focused test below performs
        // the real check; a full suite may have initialized GTK elsewhere.
        if gtk::is_initialized() && !gtk::is_initialized_main_thread() {
            return;
        }
        if !gtk::is_initialized() {
            gtk::init().expect("GTK must initialize for the browser lifetime test");
        }

        let pane = BrowserPane::new(
            PaneId::new(),
            SurfaceId::new(),
            None,
            PaneCallbacks::noop_for_test(),
            BrowserEngine::Webkit,
            false,
        );
        let root = pane.root.downgrade();
        let web_view = pane.web_view.downgrade();

        drop(pane);

        assert!(
            root.upgrade().is_none(),
            "closed browser tab retained its GTK root"
        );
        assert!(
            web_view.upgrade().is_none(),
            "closed browser tab retained its WebView"
        );
    }

    #[test]
    fn engine_to_profile_maps_each_builtin_to_its_data_slot() {
        // The Webkit label uses the shared default slot so every flowmux
        // tab sees the same cookies; the import labels each get their own
        // slot so credentials from a Firefox/Chrome import don't bleed into
        // the default profile.
        assert_eq!(
            engine_to_profile(&BrowserEngine::Webkit),
            BrowserProfile::Default
        );
        assert_eq!(
            engine_to_profile(&BrowserEngine::Chrome),
            BrowserProfile::ChromeImport
        );
        assert_eq!(
            engine_to_profile(&BrowserEngine::Firefox),
            BrowserProfile::FirefoxImport
        );
        assert_eq!(
            engine_to_profile(&BrowserEngine::Custom {
                name: "Brave".into()
            }),
            BrowserProfile::Custom {
                name: "Brave".into()
            }
        );
    }

    #[test]
    fn cookies_sqlite_path_is_under_profile_data_dir() {
        // The file must sit directly inside the profile data dir so it stays
        // colocated with the rest of the WebKit storage WebKit places there.
        // If this file ever moves, an existing user's persisted cookies stop
        // being picked up — assert the layout explicitly.
        let dir = std::path::PathBuf::from("/tmp/flowmux-test-profile");
        let path = cookies_sqlite_path(&dir);
        assert_eq!(path, dir.join("cookies.sqlite"));
        assert!(path.starts_with(&dir));
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("cookies.sqlite")
        );
    }

    #[test]
    fn normalize_uri_blank_input_returns_about_blank() {
        assert_eq!(normalize_uri(""), "about:blank");
        assert_eq!(normalize_uri("   "), "about:blank");
    }

    #[test]
    fn normalize_uri_preserves_explicit_schemes() {
        for raw in [
            "http://example.com",
            "https://example.com/path",
            "about:blank",
            "file:///tmp/x.html",
        ] {
            assert_eq!(normalize_uri(raw), raw);
        }
    }

    #[test]
    fn normalize_uri_promotes_localhost_to_http() {
        assert_eq!(normalize_uri("localhost:3000"), "http://localhost:3000");
        assert_eq!(normalize_uri("127.0.0.1:8080"), "http://127.0.0.1:8080");
    }

    #[test]
    fn normalize_uri_promotes_dotted_host_to_https() {
        assert_eq!(normalize_uri("example.com"), "https://example.com");
        assert_eq!(
            normalize_uri("docs.gtk.org/gtk4/"),
            "https://docs.gtk.org/gtk4/"
        );
    }

    #[test]
    fn normalize_uri_falls_back_to_search() {
        assert_eq!(
            normalize_uri("hello world"),
            "https://duckduckgo.com/?q=hello+world"
        );
    }

    #[test]
    fn download_path_stays_in_directory_and_avoids_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("report.txt"), "existing").unwrap();

        assert_eq!(
            available_download_path(directory.path(), "../report.txt"),
            directory.path().join("report (1).txt")
        );
        assert_eq!(
            available_download_path(directory.path(), "../../"),
            directory.path().join("download")
        );
    }
}
