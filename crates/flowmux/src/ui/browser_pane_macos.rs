// SPDX-License-Identifier: GPL-3.0-or-later
//! macOS in-app browser pane backed by the system WebKit `WKWebView`.

use crate::ui::pane_terminal::PaneCallbacks;
use flowmux_browser::{BrowserProfile, RefScope, RefStore};
use flowmux_config::options::BrowserEngine;
use flowmux_core::{PaneId, SurfaceId};
use gtk::glib::{self, translate::ToGlibPtr};
use gtk::prelude::*;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{
    define_class, msg_send, AnyThread, ClassType, MainThreadMarker, MainThreadOnly, Message,
};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSResponder, NSView, NSWindow};
use objc2_foundation::{
    NSDictionary, NSError, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURLRequest,
    NSURL, NSUUID,
};
use objc2_web_kit::{
    WKAudiovisualMediaTypes, WKNavigationAction, WKPreferences, WKSnapshotConfiguration,
    WKUIDelegate, WKWebView, WKWebViewConfiguration, WKWebsiteDataStore, WKWindowFeatures,
};
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

thread_local! {
    static NATIVE_BROWSER_VIEW_SUSPEND_COUNT: Cell<u32> = const { Cell::new(0) };
}

unsafe extern "C" {
    fn gdk_macos_surface_get_native_window(surface: *mut gtk::gdk::ffi::GdkSurface) -> *mut c_void;
}

#[derive(Clone)]
pub struct BrowserPane {
    pane_id: Rc<Cell<PaneId>>,
    pub root: gtk::Box,
    pub web_view: gtk::Widget,
    native: Rc<NativeBrowserView>,
    address: gtk::Entry,
    pub refs: Rc<RefCell<RefStore>>,
    pub ref_scope: RefScope,
}

struct NativeBrowserView {
    web_view: Retained<WKWebView>,
    _ui_delegate: Retained<BrowserUIDelegate>,
    last_url: RefCell<String>,
    last_title: RefCell<String>,
    zoom: Cell<f64>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. The delegate is used
    // only on the main thread as required by WKUIDelegate.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    struct BrowserUIDelegate;

    unsafe impl NSObjectProtocol for BrowserUIDelegate {}

    unsafe impl WKUIDelegate for BrowserUIDelegate {
        #[unsafe(method_id(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:))]
        #[allow(non_snake_case)]
        fn webView_createWebViewWithConfiguration_forNavigationAction_windowFeatures(
            &self,
            web_view: &WKWebView,
            _configuration: &WKWebViewConfiguration,
            navigation_action: &WKNavigationAction,
            _window_features: &WKWindowFeatures,
        ) -> Option<Retained<WKWebView>> {
            let request = unsafe { navigation_action.request() };
            unsafe {
                web_view.loadRequest(&request);
            }
            None
        }
    }
);

impl Drop for NativeBrowserView {
    fn drop(&mut self) {
        let view = self.web_view.as_super();
        view.removeFromSuperview();
    }
}

pub struct NativeBrowserViewsSuspend {
    views: Vec<(Retained<NSView>, bool)>,
}

impl Drop for NativeBrowserViewsSuspend {
    fn drop(&mut self) {
        let still_suspended = pop_native_browser_view_suspend();
        for (view, was_hidden) in self.views.drain(..) {
            view.setHidden(if still_suspended { true } else { was_hidden });
        }
    }
}

pub fn suspend_native_browser_views_for_window(window: &gtk::Window) -> NativeBrowserViewsSuspend {
    push_native_browser_view_suspend();
    let Some(content_view) = native_content_view(window) else {
        return NativeBrowserViewsSuspend { views: Vec::new() };
    };

    let mut views = Vec::new();
    collect_web_views(&content_view, &mut views);
    for (view, _) in &views {
        view.setHidden(true);
    }
    NativeBrowserViewsSuspend { views }
}

fn push_native_browser_view_suspend() {
    NATIVE_BROWSER_VIEW_SUSPEND_COUNT.with(|count| {
        count.set(count.get().saturating_add(1));
    });
}

fn pop_native_browser_view_suspend() -> bool {
    NATIVE_BROWSER_VIEW_SUSPEND_COUNT.with(|count| {
        let next = count.get().saturating_sub(1);
        count.set(next);
        next > 0
    })
}

fn native_browser_views_are_suspended() -> bool {
    NATIVE_BROWSER_VIEW_SUSPEND_COUNT.with(|count| count.get() > 0)
}

fn collect_web_views(view: &NSView, views: &mut Vec<(Retained<NSView>, bool)>) {
    if view.isKindOfClass(WKWebView::class()) {
        views.push((view.retain(), view.isHidden()));
    }

    let subviews = view.subviews();
    for index in 0..subviews.count() {
        let subview = subviews.objectAtIndex(index);
        collect_web_views(&subview, views);
    }
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
        persist_session: bool,
    ) -> Self {
        let pane_id = Rc::new(Cell::new(id));
        let profile = engine_to_profile(&engine);
        tracing::debug!(
            engine = ?engine,
            profile = ?profile,
            persist_session,
            "creating browser pane (macOS WKWebView + profile-isolated WebsiteDataStore)"
        );

        let mtm = MainThreadMarker::new().expect("WKWebView must be created on the main thread");
        let web_view = create_web_view(mtm, &profile, persist_session);

        let back = gtk::Button::from_icon_name("go-previous-symbolic");
        let forward = gtk::Button::from_icon_name("go-next-symbolic");
        let reload = gtk::Button::from_icon_name("view-refresh-symbolic");
        let address = gtk::Entry::new();
        address.set_hexpand(true);
        address.set_placeholder_text(Some("Enter URL — e.g. http://localhost:3000"));
        let inspector = gtk::Button::from_icon_name("applications-utilities-symbolic");
        inspector.add_css_class("flat");
        inspector.set_tooltip_text(Some(
            "Web Inspector is available from Safari's Develop menu",
        ));

        let chrome = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        chrome.set_margin_top(4);
        chrome.set_margin_bottom(4);
        chrome.set_margin_start(4);
        chrome.set_margin_end(4);
        chrome.append(&back);
        chrome.append(&forward);
        chrome.append(&reload);
        chrome.append(&address);
        chrome.append(&inspector);

        let viewport = gtk::DrawingArea::new();
        viewport.set_hexpand(true);
        viewport.set_vexpand(true);
        viewport.set_focusable(true);
        let web_widget = viewport.clone().upcast::<gtk::Widget>();

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.append(&chrome);
        root.append(&viewport);

        let native = Rc::new(NativeBrowserView {
            web_view: web_view.clone(),
            _ui_delegate: install_ui_delegate(&web_view),
            last_url: RefCell::new(String::new()),
            last_title: RefCell::new(String::new()),
            zoom: Cell::new(1.0),
        });

        {
            let native = native.clone();
            back.connect_clicked(move |_| {
                if unsafe { native.web_view.canGoBack() } {
                    unsafe {
                        native.web_view.goBack();
                    }
                }
            });
        }
        {
            let native = native.clone();
            forward.connect_clicked(move |_| {
                if unsafe { native.web_view.canGoForward() } {
                    unsafe {
                        native.web_view.goForward();
                    }
                }
            });
        }
        {
            let native = native.clone();
            reload.connect_clicked(move |_| unsafe {
                native.web_view.reload();
            });
        }
        inspector.connect_clicked(|_| {
            tracing::info!("macOS WKWebView inspector is exposed through Safari's Develop menu");
        });
        {
            let native = native.clone();
            let address = address.clone();
            address.clone().connect_activate(move |_| {
                let uri = normalize_uri(&address.text());
                load_uri_native(&native.web_view, &uri);
            });
        }

        {
            let native = native.clone();
            let web_widget = web_widget.clone();
            root.connect_map(move |_| sync_native_view_frame(&native.web_view, &web_widget));
        }
        {
            let native = native.clone();
            root.connect_unmap(move |_| native.web_view.as_super().setHidden(true));
        }
        {
            let native = native.clone();
            root.connect_destroy(move |_| native.web_view.as_super().removeFromSuperview());
        }
        {
            let native = Rc::downgrade(&native);
            let web_widget = web_widget.downgrade();
            gtk::glib::timeout_add_local(Duration::from_millis(50), move || {
                let Some(native) = native.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let Some(web_widget) = web_widget.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                sync_native_view_frame(&native.web_view, &web_widget);
                glib::ControlFlow::Continue
            });
        }
        {
            let native = Rc::downgrade(&native);
            let address = address.downgrade();
            let uri_cb = callbacks.on_browser_uri_changed.clone();
            let title_cb = callbacks.on_browser_title_changed.clone();
            let pane_id = pane_id.clone();
            gtk::glib::timeout_add_local(Duration::from_millis(250), move || {
                let Some(native) = native.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let Some(address) = address.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                sync_browser_state(
                    &native,
                    &address,
                    pane_id.get(),
                    surface_id,
                    &uri_cb,
                    &title_cb,
                );
                glib::ControlFlow::Continue
            });
        }

        let initial = initial_url
            .map(normalize_uri)
            .unwrap_or_else(|| "about:blank".into());
        address.set_text(&initial);
        load_uri_native(&native.web_view, &initial);

        Self {
            pane_id,
            root,
            web_view: web_widget,
            native,
            address,
            refs: Rc::new(RefCell::new(RefStore::new())),
            ref_scope: ref_scope_for_surface(surface_id),
        }
    }

    pub fn current_url(&self) -> String {
        current_url_native(&self.native.web_view)
    }

    pub fn current_title(&self) -> String {
        current_title_native(&self.native.web_view)
    }

    pub fn load_uri(&self, url: &str) {
        let normalized = normalize_uri(url);
        self.address.set_text(&normalized);
        load_uri_native(&self.native.web_view, &normalized);
    }

    pub fn go_back(&self) -> bool {
        let moved = unsafe { self.native.web_view.canGoBack() };
        if moved {
            unsafe {
                self.native.web_view.goBack();
            }
        }
        moved
    }

    pub fn go_forward(&self) -> bool {
        let moved = unsafe { self.native.web_view.canGoForward() };
        if moved {
            unsafe {
                self.native.web_view.goForward();
            }
        }
        moved
    }

    pub fn reload(&self) {
        unsafe {
            self.native.web_view.reload();
        }
    }

    pub fn stop_loading(&self) {
        unsafe {
            self.native.web_view.stopLoading();
        }
    }

    pub fn grab_focus(&self) {
        focus_native_view(&self.native.web_view);
    }

    pub fn pane_id_handle(&self) -> Rc<Cell<PaneId>> {
        self.pane_id.clone()
    }

    pub fn set_pane_id(&self, id: PaneId) {
        self.pane_id.set(id);
    }

    pub fn focus_widget(&self) -> gtk::Widget {
        self.web_view.clone()
    }

    pub fn set_zoom_level(&self, zoom: f64) {
        self.native.zoom.set(zoom);
        unsafe {
            self.native.web_view.setPageZoom(zoom);
        }
    }

    pub fn snapshot_to_png<F: FnOnce(Result<String, String>) + 'static>(
        &self,
        path: PathBuf,
        on_done: F,
    ) {
        let callback = Rc::new(RefCell::new(Some(on_done)));
        let path_for_block = path.clone();
        let block = block2::RcBlock::new(
            move |image: *mut objc2_app_kit::NSImage, error: *mut NSError| {
                let result = if !error.is_null() {
                    Err(error_description(error))
                } else if image.is_null() {
                    Err("WKWebView snapshot returned no image".into())
                } else {
                    unsafe { save_nsimage_to_png(&*image, &path_for_block) }
                };
                if let Some(callback) = callback.borrow_mut().take() {
                    callback(result);
                }
            },
        );
        unsafe {
            self.native
                .web_view
                .takeSnapshotWithConfiguration_completionHandler(
                    Option::<&WKSnapshotConfiguration>::None,
                    &block,
                );
        }
    }

    /// Run JS and call `on_done` with the JS result string. The
    /// scriptable API wraps this with a oneshot channel that the IPC
    /// handler awaits.
    pub fn evaluate_js<F: FnOnce(Result<String, String>) + 'static>(
        &self,
        source: &str,
        on_done: F,
    ) {
        let script = NSString::from_str(source);
        let callback = Rc::new(RefCell::new(Some(on_done)));
        let block = block2::RcBlock::new(move |value: *mut AnyObject, error: *mut NSError| {
            let result = if !error.is_null() {
                Err(error_description(error))
            } else {
                Ok(object_description(value))
            };
            if let Some(callback) = callback.borrow_mut().take() {
                callback(result);
            }
        });
        unsafe {
            self.native
                .web_view
                .evaluateJavaScript_completionHandler(&script, Some(&block));
        }
    }
}

fn create_web_view(
    mtm: MainThreadMarker,
    profile: &BrowserProfile,
    persist_session: bool,
) -> Retained<WKWebView> {
    let config = unsafe { WKWebViewConfiguration::new(mtm) };
    let preferences = unsafe { WKPreferences::new(mtm) };
    unsafe {
        preferences.setJavaScriptCanOpenWindowsAutomatically(true);
        preferences.setElementFullscreenEnabled(true);
        config.setPreferences(&preferences);
        config.setMediaTypesRequiringUserActionForPlayback(WKAudiovisualMediaTypes::None);
        config.setWebsiteDataStore(&website_data_store(mtm, profile, persist_session));
    }

    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
    let web_view =
        unsafe { WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &config) };
    unsafe {
        web_view.setAllowsBackForwardNavigationGestures(true);
        web_view.setPageZoom(1.0);
    }
    web_view
}

fn install_ui_delegate(web_view: &WKWebView) -> Retained<BrowserUIDelegate> {
    let delegate: Retained<BrowserUIDelegate> =
        unsafe { msg_send![BrowserUIDelegate::class(), new] };
    unsafe {
        web_view.setUIDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    }
    delegate
}

fn website_data_store(
    mtm: MainThreadMarker,
    profile: &BrowserProfile,
    persist_session: bool,
) -> Retained<WKWebsiteDataStore> {
    if !persist_session {
        return unsafe { WKWebsiteDataStore::nonPersistentDataStore(mtm) };
    }

    let slug = profile.slug();
    let uuid = NSString::from_str(&uuid_for_profile_slug(&slug));
    match NSUUID::from_string(&uuid) {
        Some(identifier) => unsafe { WKWebsiteDataStore::dataStoreForIdentifier(&identifier, mtm) },
        None => unsafe { WKWebsiteDataStore::defaultDataStore(mtm) },
    }
}

fn sync_native_view_frame(web_view: &WKWebView, placeholder: &gtk::Widget) {
    if !placeholder.is_mapped() {
        web_view.as_super().setHidden(true);
        return;
    }

    let Some(root) = placeholder.root() else {
        web_view.as_super().setHidden(true);
        return;
    };
    let Ok(window) = root.downcast::<gtk::Window>() else {
        web_view.as_super().setHidden(true);
        return;
    };
    let Some(content_view) = native_content_view(&window) else {
        web_view.as_super().setHidden(true);
        return;
    };
    let Some(bounds) = placeholder.compute_bounds(&window) else {
        web_view.as_super().setHidden(true);
        return;
    };

    let width = f64::from(bounds.width()).max(1.0);
    let height = f64::from(bounds.height()).max(1.0);
    let content_bounds = content_view.bounds();
    let x = f64::from(bounds.x());
    let y = if content_view.isFlipped() {
        f64::from(bounds.y())
    } else {
        content_bounds.size.height - f64::from(bounds.y()) - height
    };
    let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(width, height));
    let view = web_view.as_super();
    view.setFrame(frame);
    if unsafe { view.superview() }.is_none() {
        content_view.addSubview(view);
        view.setAutoresizingMask(
            objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
                | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable,
        );
    }
    if native_browser_views_are_suspended() {
        view.setHidden(true);
        return;
    }
    view.setHidden(false);
}

fn native_content_view(window: &gtk::Window) -> Option<Retained<NSView>> {
    let surface = window.surface()?;
    let surface_ptr = surface.to_glib_none().0;
    let ns_window = unsafe { gdk_macos_surface_get_native_window(surface_ptr) };
    if ns_window.is_null() {
        return None;
    }
    unsafe { (&*(ns_window as *mut NSWindow)).contentView() }
}

fn focus_native_view(web_view: &WKWebView) {
    let view = web_view.as_super();
    if let Some(window) = view.window() {
        let responder: &NSResponder = view.as_super();
        window.makeFirstResponder(Some(responder));
    }
}

fn sync_browser_state(
    native: &NativeBrowserView,
    address: &gtk::Entry,
    id: PaneId,
    surface_id: SurfaceId,
    uri_cb: &Rc<RefCell<dyn FnMut(PaneId, SurfaceId, String)>>,
    title_cb: &Rc<RefCell<dyn FnMut(PaneId, SurfaceId, String)>>,
) {
    let url = current_url_native(&native.web_view);
    if !url.is_empty() && *native.last_url.borrow() != url {
        address.set_text(&url);
        *native.last_url.borrow_mut() = url.clone();
        (uri_cb.borrow_mut())(id, surface_id, url);
    }

    let title = current_title_native(&native.web_view);
    if !title.trim().is_empty() && *native.last_title.borrow() != title {
        *native.last_title.borrow_mut() = title.clone();
        (title_cb.borrow_mut())(id, surface_id, title);
    }
}

fn current_url_native(web_view: &WKWebView) -> String {
    unsafe {
        web_view
            .URL()
            .and_then(|url| url.absoluteString())
            .map(|url| url.to_string())
    }
    .unwrap_or_default()
}

fn current_title_native(web_view: &WKWebView) -> String {
    unsafe { web_view.title().map(|title| title.to_string()) }.unwrap_or_default()
}

fn load_uri_native(web_view: &WKWebView, url: &str) {
    if url == "about:blank" {
        let html = NSString::from_str("<!doctype html><meta charset=\"utf-8\"><title></title>");
        unsafe {
            web_view.loadHTMLString_baseURL(&html, None);
        }
        return;
    }

    let ns_url = NSString::from_str(url);
    let Some(url) = NSURL::URLWithString(&ns_url) else {
        tracing::warn!(url, "WKWebView rejected URL");
        return;
    };
    let request = NSURLRequest::requestWithURL(&url);
    unsafe {
        web_view.loadRequest(&request);
    }
}

unsafe fn save_nsimage_to_png(
    image: &objc2_app_kit::NSImage,
    path: &Path,
) -> Result<String, String> {
    let tiff = image
        .TIFFRepresentation()
        .ok_or_else(|| "snapshot image had no TIFF representation".to_string())?;
    let bitmap = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &tiff)
        .ok_or_else(|| "snapshot image could not be converted to bitmap".to_string())?;
    let properties = NSDictionary::<objc2_app_kit::NSBitmapImageRepPropertyKey, AnyObject>::new();
    let png = bitmap
        .representationUsingType_properties(NSBitmapImageFileType::PNG, &properties)
        .ok_or_else(|| "snapshot bitmap could not be encoded as PNG".to_string())?;
    let path_string = NSString::from_str(&path.to_string_lossy());
    if png.writeToFile_atomically(&path_string, true) {
        Ok(path.display().to_string())
    } else {
        Err(format!("failed to write snapshot PNG: {}", path.display()))
    }
}

fn object_description(value: *mut AnyObject) -> String {
    if value.is_null() {
        return String::new();
    }
    autoreleasepool(|_| unsafe {
        let value = &*value;
        let description: Retained<NSString> = msg_send![value, description];
        description.to_string()
    })
}

fn error_description(error: *mut NSError) -> String {
    if error.is_null() {
        return "unknown WebKit error".into();
    }
    unsafe { (&*error).localizedDescription().to_string() }
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

fn engine_to_profile(engine: &BrowserEngine) -> BrowserProfile {
    match engine {
        BrowserEngine::Webkit => BrowserProfile::Default,
        BrowserEngine::Chrome => BrowserProfile::ChromeImport,
        BrowserEngine::Firefox => BrowserProfile::FirefoxImport,
        BrowserEngine::Custom { name } => BrowserProfile::Custom { name: name.clone() },
    }
}

fn uuid_for_profile_slug(slug: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in b"flowmux-browser-profile:" {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    for byte in slug.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&0x666c6f776d757800u64.to_be_bytes());
    bytes[8..].copy_from_slice(&hash.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_uri_matches_webkit_backend_rules() {
        assert_eq!(normalize_uri(""), "about:blank");
        assert_eq!(normalize_uri("localhost:3000"), "http://localhost:3000");
        assert_eq!(normalize_uri("example.com"), "https://example.com");
        assert_eq!(
            normalize_uri("rust webkit"),
            "https://duckduckgo.com/?q=rust+webkit"
        );
    }

    #[test]
    fn profile_uuid_is_stable_and_uuid_shaped() {
        let first = uuid_for_profile_slug("default");
        let second = uuid_for_profile_slug("default");
        assert_eq!(first, second);
        assert_eq!(first.len(), 36);
        assert_eq!(&first[14..15], "5");
    }
}
