// SPDX-License-Identifier: GPL-3.0-or-later
//! Linux editor surface backed by an isolated WebKitGTK WebView.

use super::{is_allowed_editor_navigation, EditorBridgeState};
use flowmux_core::{PaneId, SurfaceId};
use flowmux_editor::{EditorAssetServer, HostMessage, ProtocolError};
use gtk::prelude::*;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
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
    _asset_server: Rc<EditorAssetServer>,
    _network_session: webkit6::NetworkSession,
    closed: Rc<Cell<bool>>,
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
            let web_view = web_view.downgrade();
            user_content_manager.connect_script_message_received(
                Some(MESSAGE_HANDLER_NAME),
                move |_, value| {
                    let scripts = bridge.receive(&value.to_str());
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
            _asset_server: asset_server,
            _network_session: network_session,
            closed: Rc::new(Cell::new(false)),
        };
        if let Err(error) = pane.send(HostMessage::InitializeEditor {
            workspace_name: workspace_name(pane.workspace_root()),
            documents: Vec::new(),
            active_document_id: None,
        }) {
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
        self.web_view.clone().upcast()
    }

    pub fn grab_focus(&self) {
        self.web_view.grab_focus();
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
