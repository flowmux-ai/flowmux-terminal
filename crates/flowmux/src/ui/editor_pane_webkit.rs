// SPDX-License-Identifier: GPL-3.0-or-later
//! Linux editor surface backed by an isolated WebKitGTK WebView.

use super::{
    handle_bridge_message, is_allowed_editor_navigation, queue_host_messages,
    should_poll_editor_documents, EditorBridgeState, EditorHostState,
};
use flowmux_core::{EditorSessionState, PaneId, SurfaceId};
use flowmux_editor::{EditorAppearance, EditorAssetServer, HostMessage, ProtocolError};
use gtk::gio;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use webkit6::prelude::*;

const MESSAGE_HANDLER_NAME: &str = "flowmuxEditor";

#[derive(Clone)]
pub struct EditorPane {
    pane_id: Rc<Cell<PaneId>>,
    workspace_root: PathBuf,
    pub root: gtk::Box,
    web_view: webkit6::WebView,
    user_content_manager: webkit6::UserContentManager,
    bridge: Rc<EditorBridgeState>,
    host: Rc<EditorHostState>,
    _asset_server: Rc<EditorAssetServer>,
    _network_session: webkit6::NetworkSession,
    closed: Rc<Cell<bool>>,
    appearance: Rc<RefCell<EditorAppearance>>,
    file_monitors: Rc<RefCell<HashMap<PathBuf, gio::FileMonitor>>>,
    file_monitor_generation: Rc<Cell<u64>>,
}

impl EditorPane {
    pub fn new(
        pane_id: PaneId,
        surface_id: SurfaceId,
        workspace_root: PathBuf,
        restored: EditorSessionState,
        appearance: EditorAppearance,
    ) -> Result<Self, String> {
        let asset_server = Rc::new(EditorAssetServer::start().map_err(|error| error.to_string())?);
        let editor_url = asset_server
            .editor_url(&surface_id.0.to_string())
            .map_err(|error| error.to_string())?;
        let allowed_prefix = editor_url
            .strip_suffix(&format!("index.html?surface={}", surface_id.0))
            .expect("editor URLs end with the generated entry point")
            .to_string();
        let bridge = Rc::new(EditorBridgeState::new(surface_id));
        let host = Rc::new(EditorHostState::new(&workspace_root, restored));
        let closed = Rc::new(Cell::new(false));
        let appearance = Rc::new(RefCell::new(appearance));
        let user_content_manager = webkit6::UserContentManager::new();
        assert!(
            user_content_manager.register_script_message_handler(MESSAGE_HANDLER_NAME, None),
            "editor script message handler must be unique"
        );
        let network_session = webkit6::NetworkSession::new_ephemeral();
        network_session.connect_download_started(|_, download| download.cancel());
        let web_view = webkit6::WebView::builder()
            .network_session(&network_session)
            .user_content_manager(&user_content_manager)
            .build();
        web_view.set_hexpand(true);
        web_view.set_vexpand(true);
        if let Some(settings) = webkit6::prelude::WebViewExt::settings(&web_view) {
            settings.set_enable_javascript(true);
            settings.set_javascript_can_open_windows_automatically(false);
            settings.set_enable_html5_database(false);
            settings.set_enable_html5_local_storage(false);
            settings.set_enable_fullscreen(false);
            settings.set_enable_developer_extras(cfg!(debug_assertions));
        }

        {
            let bridge = bridge.clone();
            let host = host.clone();
            let web_view = web_view.downgrade();
            user_content_manager.connect_script_message_received(
                Some(MESSAGE_HANDLER_NAME),
                move |_, value| {
                    let scripts = handle_bridge_message(&bridge, &host, &value.to_str());
                    if let Some(web_view) = web_view.upgrade() {
                        for script in scripts {
                            evaluate_script(&web_view, &script);
                        }
                    }
                },
            );
        }
        {
            let allowed_prefix = allowed_prefix.clone();
            web_view.connect_decide_policy(move |_, decision, decision_type| {
                if !matches!(
                    decision_type,
                    webkit6::PolicyDecisionType::NavigationAction
                        | webkit6::PolicyDecisionType::NewWindowAction
                ) {
                    return false;
                }
                let Some(navigation) = decision.downcast_ref::<webkit6::NavigationPolicyDecision>()
                else {
                    decision.ignore();
                    return true;
                };
                let uri = navigation
                    .navigation_action()
                    .and_then(|mut action| action.request())
                    .and_then(|request| request.uri())
                    .map(|uri| uri.to_string());
                if uri
                    .as_deref()
                    .is_some_and(|uri| is_allowed_editor_navigation(uri, &allowed_prefix))
                {
                    decision.use_();
                } else {
                    tracing::warn!(?uri, "blocked navigation from editor WebView");
                    decision.ignore();
                }
                true
            });
        }
        web_view.connect_permission_request(|_, request| {
            request.deny();
            true
        });
        {
            // A crashed web process (OOM on a large document, WebKit bug)
            // otherwise leaves a permanently blank pane: `ready` stays true so
            // every later message is fired into a dead page. Reset the bridge,
            // queue a full re-initialization, and reload the page.
            let bridge = bridge.clone();
            let host = Rc::downgrade(&host);
            let closed = closed.clone();
            let appearance = appearance.clone();
            let workspace_root = workspace_root.clone();
            web_view.connect_web_process_terminated(move |web_view, reason| {
                if closed.get() {
                    return;
                }
                let Some(host) = host.upgrade() else {
                    return;
                };
                tracing::warn!(?reason, "editor WebView web process terminated; reloading");
                bridge.reset();
                let mut messages = vec![HostMessage::SetAppearance {
                    appearance: appearance.borrow().clone(),
                }];
                messages.extend(host.reinitialize_messages(workspace_name(&workspace_root)));
                for message in messages {
                    if let Err(error) = bridge.queue(message) {
                        tracing::warn!(%error, "failed to queue editor reinitialization");
                    }
                }
                web_view.reload();
            });
        }

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.append(&web_view);
        web_view.load_uri(&editor_url);

        let pane = Self {
            pane_id: Rc::new(Cell::new(pane_id)),
            workspace_root,
            root,
            web_view,
            user_content_manager,
            bridge,
            host,
            _asset_server: asset_server,
            _network_session: network_session,
            closed,
            appearance,
            file_monitors: Rc::new(RefCell::new(HashMap::new())),
            file_monitor_generation: Rc::new(Cell::new(0)),
        };
        let initial_appearance = pane.appearance.borrow().clone();
        pane.apply_appearance(initial_appearance);
        if let Err(error) = pane.send(
            pane.host
                .initialize_message(workspace_name(pane.workspace_root())),
        ) {
            tracing::error!(%error, "failed to queue editor initialization");
        }
        for message in pane.host.take_startup_messages() {
            if let Err(error) = pane.send(message) {
                tracing::warn!(%error, "failed to queue restored editor state");
            }
        }
        {
            let bridge = pane.bridge.clone();
            let host = Rc::downgrade(&pane.host);
            let web_view = pane.web_view.downgrade();
            let tick = Rc::new(Cell::new(0_u8));
            gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                let Some(host) = host.upgrade() else {
                    return gtk::glib::ControlFlow::Break;
                };
                let Some(web_view) = web_view.upgrade() else {
                    return gtk::glib::ControlFlow::Break;
                };
                for script in queue_host_messages(&bridge, host.poll_search_messages()) {
                    evaluate_script(&web_view, &script);
                }
                let next_tick = tick.get().wrapping_add(1);
                tick.set(next_tick);
                if next_tick.is_multiple_of(10) {
                    for script in queue_host_messages(&bridge, host.poll_external_changes()) {
                        evaluate_script(&web_view, &script);
                    }
                }
                gtk::glib::ControlFlow::Continue
            });
        }
        Ok(pane)
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
        self.host.session_state()
    }

    pub fn focus_widget(&self) -> gtk::Widget {
        self.web_view.clone().upcast()
    }

    pub fn grab_focus(&self) {
        self.web_view.grab_focus();
    }

    pub fn apply_appearance(&self, appearance: EditorAppearance) {
        *self.appearance.borrow_mut() = appearance.clone();
        if let Err(error) = self.send(HostMessage::SetAppearance { appearance }) {
            tracing::warn!(%error, "failed to apply editor appearance");
        }
    }

    pub fn set_zoom_level(&self, zoom: f64) {
        self.web_view.set_zoom_level(zoom.clamp(0.1, 2.0));
    }

    /// Copy the editor selection when the app-level copy shortcut fires while
    /// this surface is focused (plain Ctrl+C reaches the WebView natively;
    /// this covers the global Ctrl+Shift+C accelerator).
    pub fn copy_selection(&self) {
        // WebKit's built-in editing command names ("Copy"/"Paste").
        self.web_view.execute_editing_command("Copy");
    }

    pub fn paste_clipboard(&self) {
        self.web_view.execute_editing_command("Paste");
    }

    pub fn show_workspace_search(&self) {
        if let Err(error) = self.send(HostMessage::ShowWorkspaceSearch) {
            tracing::warn!(%error, "failed to show editor workspace search");
        }
    }

    pub fn open_file(&self, path: &Path) -> Result<(), String> {
        let messages = self.host.open_document(path)?;
        self.install_file_monitor(path);
        for message in messages {
            self.send(message).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn dirty_document_paths(&self) -> Vec<PathBuf> {
        self.host.dirty_document_paths()
    }

    pub fn save_all_dirty(&self) -> Result<(), String> {
        let (messages, result) = self.host.save_all_dirty();
        for message in messages {
            if let Err(error) = self.send(message) {
                tracing::warn!(%error, "failed to resynchronize editor after close-guard save");
            }
        }
        result
    }

    pub fn discard_all_dirty(&self) {
        self.host.discard_all_dirty();
    }

    fn install_file_monitor(&self, path: &Path) {
        let Some(parent) = path.parent() else {
            return;
        };
        let directory = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        if self.file_monitors.borrow().contains_key(&directory) {
            return;
        }
        let file = gio::File::for_path(&directory);
        let Ok(monitor) =
            file.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        else {
            tracing::warn!(directory = %directory.display(), "editor file monitor unavailable");
            return;
        };
        let bridge = self.bridge.clone();
        let host = self.host.clone();
        let web_view = self.web_view.downgrade();
        let generation = self.file_monitor_generation.clone();
        monitor.connect_changed(move |_, _, _, event| {
            if !should_poll_editor_documents(event) {
                return;
            }
            let expected = generation.get().wrapping_add(1);
            generation.set(expected);
            let generation = generation.clone();
            let bridge = bridge.clone();
            let host = host.clone();
            let web_view = web_view.clone();
            gtk::glib::timeout_add_local_once(Duration::from_millis(120), move || {
                if generation.get() != expected {
                    return;
                }
                let Some(web_view) = web_view.upgrade() else {
                    return;
                };
                for script in queue_host_messages(&bridge, host.poll_external_changes()) {
                    evaluate_script(&web_view, &script);
                }
            });
        });
        self.file_monitors.borrow_mut().insert(directory, monitor);
    }

    pub fn send(&self, message: HostMessage) -> Result<(), ProtocolError> {
        if let Some(script) = self.bridge.queue(message)? {
            evaluate_script(&self.web_view, &script);
        }
        Ok(())
    }

    pub fn prepare_for_close(&self) {
        if self.closed.replace(true) {
            return;
        }
        for monitor in self
            .file_monitors
            .borrow_mut()
            .drain()
            .map(|(_, monitor)| monitor)
        {
            monitor.cancel();
        }
        self.user_content_manager
            .unregister_script_message_handler(MESSAGE_HANDLER_NAME, None);
        self.web_view.stop_loading();
        self.web_view.terminate_web_process();
    }
}

fn evaluate_script(web_view: &webkit6::WebView, script: &str) {
    web_view.evaluate_javascript(script, None, None, gtk::gio::Cancellable::NONE, |result| {
        if let Err(error) = result {
            tracing::warn!(%error, "failed to deliver message to editor WebView");
        }
    });
}

fn workspace_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}
