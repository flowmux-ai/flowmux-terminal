// SPDX-License-Identifier: GPL-3.0-or-later
//! Platform editor WebView and its versioned bridge state.

use flowmux_core::{EditorFileState, EditorSessionState, SurfaceId};
use flowmux_editor::{
    javascript_for_host_message, parse_editor_message, EditorFileSessionState, EditorMessage,
    EditorSession, EditorSessionSnapshot, EditorViewState, HostMessage, ProtocolError,
    RecoveryOperation, RecoveryStore,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

const RECOVERY_DEBOUNCE: Duration = Duration::from_millis(350);

#[derive(Default)]
pub(super) struct EditorBridgeReceive {
    scripts: Vec<String>,
    message: Option<EditorMessage>,
}

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

    pub(super) fn receive(&self, raw: &str) -> EditorBridgeReceive {
        let (surface_id, message) = match parse_editor_message(raw) {
            Ok(message) => message,
            Err(error) => {
                tracing::warn!(%error, "editor WebView sent an invalid bridge message");
                return EditorBridgeReceive::default();
            }
        };
        if surface_id != self.surface_id {
            tracing::warn!(
                expected = %self.surface_id,
                actual = %surface_id,
                "editor WebView bridge surface mismatch"
            );
            return EditorBridgeReceive::default();
        }

        let became_ready = matches!(&message, EditorMessage::EditorReady);
        if became_ready {
            self.ready.set(true);
            tracing::debug!(surface_id = %self.surface_id, "editor WebView bridge ready");
        }
        if !became_ready {
            return EditorBridgeReceive {
                scripts: Vec::new(),
                message: Some(message),
            };
        }

        let scripts = self
            .pending
            .borrow_mut()
            .drain(..)
            .filter_map(|message| {
                javascript_for_host_message(&self.surface_id, &message)
                    .map_err(|error| {
                        tracing::error!(%error, "failed to encode queued editor message");
                    })
                    .ok()
            })
            .collect();
        EditorBridgeReceive {
            scripts,
            message: None,
        }
    }
}

pub(super) struct EditorHostState {
    session: RefCell<Result<EditorSession, String>>,
    startup_messages: RefCell<Vec<HostMessage>>,
    recovery_sender: Option<Sender<RecoveryOperation>>,
    pending_recovery: RefCell<HashMap<PathBuf, RecoveryOperation>>,
    recovery_generation: Cell<u64>,
}

impl EditorHostState {
    pub(super) fn new(workspace_root: &Path, restored: EditorSessionState) -> Self {
        let recovery_store = flowmux_config::paths::state_dir().and_then(|state_root| {
            match RecoveryStore::new(state_root, workspace_root) {
                Ok(store) => Some(store),
                Err(error) => {
                    tracing::warn!(%error, "editor recovery store is unavailable");
                    None
                }
            }
        });
        let mut session = match &recovery_store {
            Some(store) => EditorSession::with_recovery_store(workspace_root, store.clone()),
            None => EditorSession::new(workspace_root),
        }
        .map_err(|error| {
            let error = error.to_string();
            tracing::error!(%error, "failed to initialize editor document session");
            error
        });
        let mut startup_messages = Vec::new();
        if let Ok(session) = &mut session {
            for file in restored.open_files {
                match session.restore_document(
                    &file.path,
                    EditorViewState {
                        cursor_line: file.cursor_line,
                        cursor_column: file.cursor_column,
                        scroll_top: file.scroll_top,
                    },
                ) {
                    Ok(messages) => {
                        startup_messages.extend(messages.into_iter().filter(|message| {
                            matches!(message, HostMessage::RecoveryAvailable { .. })
                        }))
                    }
                    Err(error) => {
                        tracing::warn!(path = %file.path.display(), %error, "skipping unavailable restored editor document");
                    }
                }
            }
            if let Some(active_file) = restored.active_file {
                session.activate_path(active_file);
            }
        }
        let recovery_sender = recovery_store.and_then(start_recovery_worker);
        let host = Self {
            session: RefCell::new(session),
            startup_messages: RefCell::new(startup_messages),
            recovery_sender,
            pending_recovery: RefCell::new(HashMap::new()),
            recovery_generation: Cell::new(0),
        };
        host.stage_recovery_operations();
        host
    }

    pub(super) fn initialize_message(&self, workspace_name: String) -> HostMessage {
        match &*self.session.borrow() {
            Ok(session) => session.initialize_message(workspace_name),
            Err(_) => HostMessage::InitializeEditor {
                workspace_name,
                documents: Vec::new(),
                active_document_id: None,
            },
        }
    }

    pub(super) fn take_startup_messages(&self) -> Vec<HostMessage> {
        std::mem::take(&mut *self.startup_messages.borrow_mut())
    }

    pub(super) fn session_state(&self) -> EditorSessionState {
        match &*self.session.borrow() {
            Ok(session) => core_session_state(session.session_snapshot()),
            Err(_) => EditorSessionState::default(),
        }
    }

    pub(super) fn open_document(&self, path: &Path) -> Result<Vec<HostMessage>, String> {
        let result = match &mut *self.session.borrow_mut() {
            Ok(session) => session
                .open_document(path)
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.clone()),
        };
        self.stage_recovery_operations();
        result
    }

    pub(super) fn dirty_document_paths(&self) -> Vec<PathBuf> {
        match &*self.session.borrow() {
            Ok(session) => session.dirty_document_paths(),
            Err(_) => Vec::new(),
        }
    }

    pub(super) fn save_all_dirty(&self) -> (Vec<HostMessage>, Result<(), String>) {
        let result = match &mut *self.session.borrow_mut() {
            Ok(session) => {
                let (messages, result) = session.save_all_dirty();
                (messages, result.map_err(|error| error.to_string()))
            }
            Err(error) => (Vec::new(), Err(error.clone())),
        };
        self.stage_recovery_operations();
        result
    }

    pub(super) fn discard_all_dirty(&self) {
        if let Ok(session) = &mut *self.session.borrow_mut() {
            session.discard_all_dirty();
        }
        self.stage_recovery_operations();
    }

    fn handle(&self, message: EditorMessage) -> Vec<HostMessage> {
        let result = match &mut *self.session.borrow_mut() {
            Ok(session) => session.handle_editor_message(message),
            Err(error) => {
                tracing::warn!(%error, "editor document session is unavailable");
                return Vec::new();
            }
        };
        match result {
            Ok(messages) => messages,
            Err(error) => {
                tracing::warn!(%error, "editor document message was rejected");
                Vec::new()
            }
        }
    }

    fn stage_recovery_operations(&self) -> bool {
        let operations = match &mut *self.session.borrow_mut() {
            Ok(session) => session.take_recovery_operations(),
            Err(_) => Vec::new(),
        };
        if operations.is_empty() {
            return false;
        }

        let Some(sender) = &self.recovery_sender else {
            return false;
        };
        let mut pending = self.pending_recovery.borrow_mut();
        let mut has_write = false;
        for operation in operations {
            let path = operation.identity_path().to_path_buf();
            match operation {
                RecoveryOperation::Write(_) => {
                    pending.insert(path, operation);
                    has_write = true;
                }
                RecoveryOperation::Remove(_) => {
                    pending.remove(&path);
                    if sender.send(operation).is_err() {
                        tracing::warn!("editor recovery worker stopped unexpectedly");
                    }
                }
            }
        }
        if has_write {
            self.recovery_generation
                .set(self.recovery_generation.get().wrapping_add(1));
        }
        has_write
    }

    fn flush_recovery(&self) {
        let Some(sender) = &self.recovery_sender else {
            return;
        };
        for operation in self
            .pending_recovery
            .borrow_mut()
            .drain()
            .map(|(_, value)| value)
        {
            if sender.send(operation).is_err() {
                tracing::warn!("editor recovery worker stopped unexpectedly");
                break;
            }
        }
    }

    pub(super) fn poll_external_changes(&self) -> Vec<HostMessage> {
        let result = match &mut *self.session.borrow_mut() {
            Ok(session) => session.poll_external_changes(),
            Err(error) => {
                tracing::warn!(%error, "editor document session is unavailable");
                return Vec::new();
            }
        };
        match result {
            Ok(messages) => messages,
            Err(error) => {
                tracing::warn!(%error, "failed to inspect open editor documents");
                Vec::new()
            }
        }
    }
}

pub(super) fn queue_host_messages(
    bridge: &EditorBridgeState,
    messages: Vec<HostMessage>,
) -> Vec<String> {
    messages
        .into_iter()
        .filter_map(|message| {
            bridge
                .queue(message)
                .map_err(|error| {
                    tracing::error!(%error, "failed to encode editor response");
                })
                .ok()
                .flatten()
        })
        .collect()
}

pub(super) fn handle_bridge_message(
    bridge: &EditorBridgeState,
    host: &Rc<EditorHostState>,
    raw: &str,
) -> Vec<String> {
    let received = bridge.receive(raw);
    let mut scripts = received.scripts;
    if let Some(message) = received.message {
        scripts.extend(queue_host_messages(bridge, host.handle(message)));
        if host.stage_recovery_operations() {
            schedule_recovery_flush(host);
        }
    }
    scripts
}

fn schedule_recovery_flush(host: &Rc<EditorHostState>) {
    let expected_generation = host.recovery_generation.get();
    let host = Rc::downgrade(host);
    gtk::glib::timeout_add_local_once(RECOVERY_DEBOUNCE, move || {
        let Some(host) = host.upgrade() else {
            return;
        };
        if host.recovery_generation.get() == expected_generation {
            host.flush_recovery();
        }
    });
}

fn start_recovery_worker(store: RecoveryStore) -> Option<Sender<RecoveryOperation>> {
    let (sender, receiver) = mpsc::channel::<RecoveryOperation>();
    let result = std::thread::Builder::new()
        .name("flowmux-editor-recovery".into())
        .spawn(move || {
            for operation in receiver {
                if let Err(error) = store.apply(&operation) {
                    tracing::warn!(%error, "failed to update editor recovery snapshot");
                }
            }
        });
    match result {
        Ok(_) => Some(sender),
        Err(error) => {
            tracing::warn!(%error, "failed to start editor recovery worker");
            None
        }
    }
}

fn core_session_state(snapshot: EditorSessionSnapshot) -> EditorSessionState {
    EditorSessionState {
        open_files: snapshot
            .open_files
            .into_iter()
            .map(
                |EditorFileSessionState {
                     path,
                     view:
                         EditorViewState {
                             cursor_line,
                             cursor_column,
                             scroll_top,
                         },
                 }| EditorFileState {
                    path,
                    cursor_line,
                    cursor_column,
                    scroll_top,
                },
            )
            .collect(),
        active_file: snapshot.active_file,
    }
}

pub(super) fn is_allowed_editor_navigation(url: &str, allowed_prefix: &str) -> bool {
    url.starts_with(allowed_prefix)
}

pub(super) fn should_poll_editor_documents(event: gtk::gio::FileMonitorEvent) -> bool {
    matches!(
        event,
        gtk::gio::FileMonitorEvent::Changed
            | gtk::gio::FileMonitorEvent::ChangesDoneHint
            | gtk::gio::FileMonitorEvent::AttributeChanged
            | gtk::gio::FileMonitorEvent::Created
            | gtk::gio::FileMonitorEvent::Deleted
            | gtk::gio::FileMonitorEvent::Moved
            | gtk::gio::FileMonitorEvent::Renamed
            | gtk::gio::FileMonitorEvent::MovedIn
            | gtk::gio::FileMonitorEvent::MovedOut
    )
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
            .scripts
            .is_empty());
        let ready = format!(
            r#"{{"protocolVersion":1,"surfaceId":"{}","type":"editor_ready"}}"#,
            surface_id.0
        );
        let received = bridge.receive(&ready);
        assert_eq!(received.scripts.len(), 1);
        assert!(received.scripts[0].contains("initialize_editor"));
    }

    #[test]
    fn file_monitor_filters_events_that_can_change_document_state() {
        assert!(should_poll_editor_documents(
            gtk::gio::FileMonitorEvent::ChangesDoneHint
        ));
        assert!(should_poll_editor_documents(
            gtk::gio::FileMonitorEvent::Deleted
        ));
        assert!(!should_poll_editor_documents(
            gtk::gio::FileMonitorEvent::PreUnmount
        ));
    }
}
