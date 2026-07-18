// SPDX-License-Identifier: GPL-3.0-or-later
//! macOS editor surface backed by the system WKWebView.

use super::{
    handle_bridge_message, is_allowed_editor_navigation, EditorBridgeState, EditorHostState,
};
use flowmux_core::{PaneId, SurfaceId};
use flowmux_editor::{EditorAssetServer, HostMessage, ProtocolError};
use gtk::glib::{self, translate::ToGlibPtr};
use gtk::prelude::*;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, ProtocolObject};
use objc2::{define_class, msg_send, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSResponder, NSView, NSWindow};
use objc2_foundation::{
    NSError, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURLRequest, NSURL,
};
use objc2_web_kit::{
    WKNavigation, WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate,
    WKNavigationResponse, WKNavigationResponsePolicy, WKPreferences, WKScriptMessage,
    WKScriptMessageHandler, WKUserContentController, WKWebView, WKWebViewConfiguration,
    WKWebsiteDataStore,
};
use std::cell::Cell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

const MESSAGE_HANDLER_NAME: &str = "flowmuxEditor";

unsafe extern "C" {
    fn gdk_macos_surface_get_native_window(surface: *mut gtk::gdk::ffi::GdkSurface) -> *mut c_void;
}

#[derive(Clone)]
pub struct EditorPane {
    pane_id: Rc<Cell<PaneId>>,
    workspace_root: PathBuf,
    pub root: gtk::Box,
    web_widget: gtk::Widget,
    native: Rc<NativeEditorView>,
    bridge: Rc<EditorBridgeState>,
    host: Rc<EditorHostState>,
    _asset_server: Rc<EditorAssetServer>,
}

struct NativeEditorView {
    web_view: Retained<WKWebView>,
    attached: Cell<bool>,
    user_content_controller: Retained<WKUserContentController>,
    _script_message_handler: Retained<EditorScriptMessageHandler>,
    _navigation_delegate: Retained<EditorNavigationDelegate>,
}

struct EditorScriptMessageHandlerIvars {
    bridge: Rc<EditorBridgeState>,
    host: Rc<EditorHostState>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. WebKit invokes the
    // script handler on the main thread.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = EditorScriptMessageHandlerIvars]
    struct EditorScriptMessageHandler;

    unsafe impl NSObjectProtocol for EditorScriptMessageHandler {}

    unsafe impl WKScriptMessageHandler for EditorScriptMessageHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        #[allow(non_snake_case)]
        unsafe fn userContentController_didReceiveScriptMessage(
            &self,
            _user_content_controller: &WKUserContentController,
            message: &WKScriptMessage,
        ) {
            let body = unsafe { message.body() };
            let Some(body) = body.downcast_ref::<NSString>() else {
                tracing::warn!("editor WKWebView sent a non-string bridge message");
                return;
            };
            let scripts =
                handle_bridge_message(&self.ivars().bridge, &self.ivars().host, &body.to_string());
            let Some(web_view) = (unsafe { message.webView() }) else {
                return;
            };
            for script in scripts {
                evaluate_script(&web_view, &script);
            }
        }
    }
);

struct EditorNavigationDelegateIvars {
    allowed_prefix: String,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. WebKit invokes the
    // navigation delegate on the main thread.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = EditorNavigationDelegateIvars]
    struct EditorNavigationDelegate;

    unsafe impl NSObjectProtocol for EditorNavigationDelegate {}

    unsafe impl WKNavigationDelegate for EditorNavigationDelegate {
        #[unsafe(method(webView:didFinishNavigation:))]
        #[allow(non_snake_case)]
        unsafe fn webView_didFinishNavigation(
            &self,
            _web_view: &WKWebView,
            _navigation: Option<&WKNavigation>,
        ) {
            tracing::debug!("editor WKWebView navigation finished");
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        #[allow(non_snake_case)]
        unsafe fn webView_didFailNavigation_withError(
            &self,
            _web_view: &WKWebView,
            _navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            tracing::warn!(
                error = %error.localizedDescription(),
                "editor WKWebView navigation failed"
            );
        }

        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        #[allow(non_snake_case)]
        fn webView_decidePolicyForNavigationAction_decisionHandler(
            &self,
            _web_view: &WKWebView,
            navigation_action: &WKNavigationAction,
            decision_handler: &block2::DynBlock<dyn Fn(WKNavigationActionPolicy)>,
        ) {
            let request = unsafe { navigation_action.request() };
            let url = request
                .URL()
                .and_then(|url| url.absoluteString())
                .map(|url| url.to_string());
            let allowed = url
                .as_deref()
                .is_some_and(|url| is_allowed_editor_navigation(url, &self.ivars().allowed_prefix));
            if !allowed {
                tracing::warn!(?url, "blocked navigation from editor WKWebView");
            }
            decision_handler.call((if allowed {
                WKNavigationActionPolicy::Allow
            } else {
                WKNavigationActionPolicy::Cancel
            },));
        }

        #[unsafe(method(webView:decidePolicyForNavigationResponse:decisionHandler:))]
        #[allow(non_snake_case)]
        fn webView_decidePolicyForNavigationResponse_decisionHandler(
            &self,
            _web_view: &WKWebView,
            navigation_response: &WKNavigationResponse,
            decision_handler: &block2::DynBlock<dyn Fn(WKNavigationResponsePolicy)>,
        ) {
            decision_handler.call((if unsafe { navigation_response.canShowMIMEType() } {
                WKNavigationResponsePolicy::Allow
            } else {
                WKNavigationResponsePolicy::Cancel
            },));
        }
    }
);

impl Drop for NativeEditorView {
    fn drop(&mut self) {
        let name = NSString::from_str(MESSAGE_HANDLER_NAME);
        unsafe {
            self.web_view.stopLoading();
            self.user_content_controller
                .removeScriptMessageHandlerForName(&name);
        }
        self.web_view.as_super().removeFromSuperview();
    }
}

impl EditorPane {
    pub fn new(pane_id: PaneId, surface_id: SurfaceId, workspace_root: PathBuf) -> Self {
        let asset_server = Rc::new(
            EditorAssetServer::start()
                .expect("the editor asset server must bind to the IPv4 loopback interface"),
        );
        let editor_url = asset_server
            .editor_url(&surface_id.0.to_string())
            .expect("surface UUIDs are valid editor URL identifiers");
        let allowed_prefix = editor_url
            .strip_suffix(&format!("index.html?surface={}", surface_id.0))
            .expect("editor URLs end with the generated entry point")
            .to_string();
        let bridge = Rc::new(EditorBridgeState::new(surface_id));
        let host = Rc::new(EditorHostState::new(&workspace_root));
        let mtm = MainThreadMarker::new().expect("WKWebView must be created on the main thread");
        let native = Rc::new(create_native_editor_view(
            mtm,
            bridge.clone(),
            host.clone(),
            &allowed_prefix,
        ));

        let viewport = gtk::DrawingArea::new();
        viewport.set_hexpand(true);
        viewport.set_vexpand(true);
        viewport.set_focusable(true);
        let web_widget = viewport.clone().upcast::<gtk::Widget>();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.append(&viewport);

        {
            let native = native.clone();
            let web_widget = web_widget.clone();
            root.connect_map(move |_| sync_native_view_frame(&native, &web_widget));
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
            glib::timeout_add_local(Duration::from_millis(50), move || {
                let Some(native) = native.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let Some(web_widget) = web_widget.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                sync_native_view_frame(&native, &web_widget);
                glib::ControlFlow::Continue
            });
        }

        load_uri_native(&native.web_view, &editor_url);
        let pane = Self {
            pane_id: Rc::new(Cell::new(pane_id)),
            workspace_root,
            root,
            web_widget,
            native,
            bridge,
            host,
            _asset_server: asset_server,
        };
        if let Err(error) = pane.send(
            pane.host
                .initialize_message(workspace_name(pane.workspace_root())),
        ) {
            tracing::error!(%error, "failed to queue editor initialization");
        }
        pane
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
        self.web_widget.clone()
    }

    pub fn grab_focus(&self) {
        focus_native_view(&self.native.web_view);
    }

    pub fn send(&self, message: HostMessage) -> Result<(), ProtocolError> {
        if let Some(script) = self.bridge.queue(message)? {
            evaluate_script(&self.native.web_view, &script);
        }
        Ok(())
    }

    pub fn prepare_for_close(&self) {
        unsafe {
            self.native.web_view.stopLoading();
        }
    }
}

fn create_native_editor_view(
    mtm: MainThreadMarker,
    bridge: Rc<EditorBridgeState>,
    host: Rc<EditorHostState>,
    allowed_prefix: &str,
) -> NativeEditorView {
    let configuration = unsafe { WKWebViewConfiguration::new(mtm) };
    let preferences = unsafe { WKPreferences::new(mtm) };
    let user_content_controller = unsafe { WKUserContentController::new(mtm) };
    let script_message_handler = EditorScriptMessageHandler::alloc(mtm)
        .set_ivars(EditorScriptMessageHandlerIvars { bridge, host });
    let script_message_handler: Retained<EditorScriptMessageHandler> =
        unsafe { msg_send![super(script_message_handler), init] };
    let handler_name = NSString::from_str(MESSAGE_HANDLER_NAME);
    unsafe {
        preferences.setJavaScriptCanOpenWindowsAutomatically(false);
        configuration.setPreferences(&preferences);
        configuration.setWebsiteDataStore(&WKWebsiteDataStore::nonPersistentDataStore(mtm));
        user_content_controller.addScriptMessageHandler_name(
            ProtocolObject::from_ref(&*script_message_handler),
            &handler_name,
        );
        configuration.setUserContentController(&user_content_controller);
    }

    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
    let web_view = unsafe {
        WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &configuration)
    };
    unsafe {
        web_view.setAllowsBackForwardNavigationGestures(false);
        web_view.setPageZoom(1.0);
    }
    let navigation_delegate =
        EditorNavigationDelegate::alloc(mtm).set_ivars(EditorNavigationDelegateIvars {
            allowed_prefix: allowed_prefix.to_string(),
        });
    let navigation_delegate: Retained<EditorNavigationDelegate> =
        unsafe { msg_send![super(navigation_delegate), init] };
    unsafe {
        web_view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*navigation_delegate)));
    }

    NativeEditorView {
        web_view,
        attached: Cell::new(false),
        user_content_controller,
        _script_message_handler: script_message_handler,
        _navigation_delegate: navigation_delegate,
    }
}

fn evaluate_script(web_view: &WKWebView, source: &str) {
    let source = NSString::from_str(source);
    let block = block2::RcBlock::new(
        move |_value: *mut objc2::runtime::AnyObject, error: *mut objc2_foundation::NSError| {
            if !error.is_null() {
                let error = unsafe { (&*error).localizedDescription().to_string() };
                tracing::warn!(%error, "failed to deliver message to editor WKWebView");
            }
        },
    );
    unsafe {
        web_view.evaluateJavaScript_completionHandler(&source, Some(&block));
    }
}

fn load_uri_native(web_view: &WKWebView, url: &str) {
    let url = NSString::from_str(url);
    let Some(url) = NSURL::URLWithString(&url) else {
        tracing::error!("WKWebView rejected the generated editor URL");
        return;
    };
    let request = NSURLRequest::requestWithURL(&url);
    unsafe {
        web_view.loadRequest(&request);
    }
}

fn sync_native_view_frame(native: &NativeEditorView, placeholder: &gtk::Widget) {
    let web_view = &native.web_view;
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
        if !native.attached.replace(true) {
            tracing::debug!(width, height, "editor WKWebView attached to native window");
        }
    }
    if crate::ui::browser_pane::native_browser_views_are_suspended() {
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

fn workspace_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}
