// SPDX-License-Identifier: GPL-3.0-or-later
//! Platform editor WebView and its versioned bridge state.

use flowmux_core::SurfaceId;
use flowmux_editor::{
    javascript_for_host_message, parse_editor_message, EditorMessage, HostMessage, ProtocolError,
};
use std::cell::{Cell, RefCell};

pub(super) struct EditorBridgeState {
    surface_id: String,
    ready: Cell<bool>,
    pending: RefCell<Vec<HostMessage>>,
}

impl EditorBridgeState {
    fn new(surface_id: SurfaceId) -> Self {
        Self {
            surface_id: surface_id.0.to_string(),
            ready: Cell::new(false),
            pending: RefCell::new(Vec::new()),
        }
    }

    pub(super) fn queue(&self, message: HostMessage) -> Result<Option<String>, ProtocolError> {
        let script = javascript_for_host_message(&self.surface_id, &message)?;
        if self.ready.get() {
            Ok(Some(script))
        } else {
            self.pending.borrow_mut().push(message);
            Ok(None)
        }
    }

    pub(super) fn receive(&self, raw: &str) -> Vec<String> {
        let (surface_id, message) = match parse_editor_message(raw) {
            Ok(message) => message,
            Err(error) => {
                tracing::warn!(%error, "editor WebView sent an invalid bridge message");
                return Vec::new();
            }
        };
        if surface_id != self.surface_id {
            tracing::warn!(
                expected = %self.surface_id,
                actual = %surface_id,
                "editor WebView bridge surface mismatch"
            );
            return Vec::new();
        }

        let became_ready = matches!(&message, EditorMessage::EditorReady);
        if became_ready {
            self.ready.set(true);
            tracing::debug!(surface_id = %self.surface_id, "editor WebView bridge ready");
        }
        if !became_ready {
            return Vec::new();
        }

        self.pending
            .borrow_mut()
            .drain(..)
            .filter_map(|message| {
                javascript_for_host_message(&self.surface_id, &message)
                    .map_err(|error| {
                        tracing::error!(%error, "failed to encode queued editor message");
                    })
                    .ok()
            })
            .collect()
    }
}

pub(super) fn is_allowed_editor_navigation(url: &str, allowed_prefix: &str) -> bool {
    url.starts_with(allowed_prefix)
}

#[cfg(target_os = "linux")]
#[path = "editor_pane_webkit.rs"]
mod imp;

#[cfg(target_os = "macos")]
#[path = "editor_pane_macos.rs"]
mod imp;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[path = "editor_pane_stub.rs"]
mod imp;

pub use imp::*;

#[cfg(test)]
mod tests {
    use super::*;
    use flowmux_core::SurfaceId;

    #[test]
    fn navigation_is_limited_to_exact_token_prefix() {
        let allowed = "http://127.0.0.1:43125/token/";
        assert!(is_allowed_editor_navigation(
            "http://127.0.0.1:43125/token/index.html?surface=abc",
            allowed
        ));
        assert!(!is_allowed_editor_navigation(
            "http://127.0.0.1:43125/other/index.html",
            allowed
        ));
        assert!(!is_allowed_editor_navigation(
            "http://127.0.0.1:43125.evil/token/index.html",
            allowed
        ));
        assert!(!is_allowed_editor_navigation(
            "https://example.com",
            allowed
        ));
    }

    #[test]
    fn bridge_queues_until_matching_editor_ready() {
        let surface_id = SurfaceId::new();
        let bridge = EditorBridgeState::new(surface_id);
        assert!(bridge
            .queue(HostMessage::InitializeEditor {
                workspace_name: "다국어".into(),
                documents: Vec::new(),
                active_document_id: None,
            })
            .unwrap()
            .is_none());

        assert!(bridge
            .receive(r#"{"protocolVersion":1,"surfaceId":"wrong","type":"editor_ready"}"#)
            .is_empty());
        let ready = format!(
            r#"{{"protocolVersion":1,"surfaceId":"{}","type":"editor_ready"}}"#,
            surface_id.0
        );
        let scripts = bridge.receive(&ready);
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].contains("initialize_editor"));
    }
}
