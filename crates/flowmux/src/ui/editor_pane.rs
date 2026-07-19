// SPDX-License-Identifier: GPL-3.0-or-later
//! Platform editor WebView and its versioned bridge state.

use flowmux_core::{EditorFileState, EditorSessionState, PaneId, SurfaceId};
use flowmux_editor::{
    diff_base_content, index_workspace_files, javascript_for_host_message, parse_editor_message,
    search_workspace, EditorFileSessionState, EditorFocusDirection, EditorMessage,
    EditorNativeEditAction, EditorSession, EditorSessionSnapshot, EditorViewState, HostMessage,
    ProtocolError, RecoveryOperation, RecoveryStore, SearchCancellation, SearchOptions,
    WorkspaceSearchResult,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;

const RECOVERY_DEBOUNCE: Duration = Duration::from_millis(350);
const QUICK_OPEN_LIMIT: usize = 2_000;

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditorNavigationKey {
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
}

#[cfg(target_os = "macos")]
impl EditorNavigationKey {
    pub(crate) fn monaco_action(self, extend_selection: bool) -> &'static str {
        match (self, extend_selection) {
            (Self::Left, false) => "cursorLeft",
            (Self::Right, false) => "cursorRight",
            (Self::Up, false) => "cursorUp",
            (Self::Down, false) => "cursorDown",
            (Self::PageUp, false) => "cursorPageUp",
            (Self::PageDown, false) => "cursorPageDown",
            (Self::Home, false) => "cursorHome",
            (Self::End, false) => "cursorEnd",
            (Self::Left, true) => "cursorLeftSelect",
            (Self::Right, true) => "cursorRightSelect",
            (Self::Up, true) => "cursorUpSelect",
            (Self::Down, true) => "cursorDownSelect",
            (Self::PageUp, true) => "cursorPageUpSelect",
            (Self::PageDown, true) => "cursorPageDownSelect",
            (Self::Home, true) => "cursorHomeSelect",
            (Self::End, true) => "cursorEndSelect",
        }
    }
}

pub(super) type EditorFocusDirectionCallback =
    Rc<RefCell<Option<Box<dyn FnMut(PaneId, EditorFocusDirection)>>>>;

enum SearchWorkerMessage {
    QuickOpen {
        request_id: String,
        paths: Vec<String>,
        truncated: bool,
    },
    WorkspaceSearch {
        request_id: String,
        result: WorkspaceSearchResult,
        error: Option<String>,
    },
}

#[derive(Clone, Copy)]
enum SearchWorkerKind {
    QuickOpen,
    WorkspaceSearch,
}

struct SearchWorker {
    request_id: String,
    kind: SearchWorkerKind,
    cancellation: SearchCancellation,
    receiver: Receiver<SearchWorkerMessage>,
}

/// Fetches the diff base (`git` + disk read) off the UI thread; the result is
/// picked up by the same tick that polls search workers.
struct DiffWorker {
    receiver: Receiver<HostMessage>,
}

#[derive(Default)]
pub(super) struct EditorBridgeReceive {
    scripts: Vec<String>,
    message: Option<EditorMessage>,
}

pub(super) struct EditorBridgeDispatch {
    pub scripts: Vec<String>,
    pub focus_direction: Option<EditorFocusDirection>,
    pub native_edit_action: Option<EditorNativeEditAction>,
    pub native_edit_text: Option<String>,
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

    /// Forget readiness and queued messages after the WebView's web process
    /// dies: the reloaded page starts from scratch and reports `editor_ready`
    /// again, which drains whatever is queued after this reset.
    pub(super) fn reset(&self) {
        self.ready.set(false);
        self.pending.borrow_mut().clear();
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
    workspace_root: PathBuf,
    session: RefCell<Result<EditorSession, String>>,
    startup_messages: RefCell<Vec<HostMessage>>,
    recovery_sender: Option<Sender<RecoveryOperation>>,
    pending_recovery: RefCell<HashMap<PathBuf, RecoveryOperation>>,
    recovery_flush_pending: Cell<bool>,
    search_worker: RefCell<Option<SearchWorker>>,
    diff_worker: RefCell<Option<DiffWorker>>,
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
            workspace_root: workspace_root.to_path_buf(),
            session: RefCell::new(session),
            startup_messages: RefCell::new(startup_messages),
            recovery_sender,
            pending_recovery: RefCell::new(HashMap::new()),
            recovery_flush_pending: Cell::new(false),
            search_worker: RefCell::new(None),
            diff_worker: RefCell::new(None),
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

    /// Everything a freshly reloaded page needs after a web-process crash:
    /// the full document set plus any still-undecided recovery proposals.
    pub(super) fn reinitialize_messages(&self, workspace_name: String) -> Vec<HostMessage> {
        let mut messages = vec![self.initialize_message(workspace_name)];
        if let Ok(session) = &*self.session.borrow() {
            messages.extend(session.pending_recovery_messages());
        }
        messages
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
        match message {
            EditorMessage::QuickOpenRequested { request_id } => self.start_quick_open(request_id),
            EditorMessage::WorkspaceSearchRequested {
                request_id,
                query,
                options,
            } => self.start_workspace_search(request_id, query, options),
            EditorMessage::SearchCancelled { request_id } => {
                self.cancel_search(&request_id);
                Vec::new()
            }
            EditorMessage::SearchResultOpenRequested {
                path,
                line,
                column,
                length,
            } => self.open_search_result(path, line, column, length),
            EditorMessage::DiffRequested {
                document_id,
                document_version,
            } => self.start_diff(document_id, document_version),
            message => self.handle_session_message(message),
        }
    }

    fn handle_session_message(&self, message: EditorMessage) -> Vec<HostMessage> {
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

    fn start_quick_open(&self, request_id: String) -> Vec<HostMessage> {
        self.cancel_current_search();
        let root = self.workspace_root.clone();
        let cancellation = SearchCancellation::default();
        let worker_cancellation = cancellation.clone();
        let (sender, receiver) = mpsc::channel();
        let worker_request_id = request_id.clone();
        let spawned = std::thread::Builder::new()
            .name("flowmux-editor-quick-open".into())
            .spawn(move || {
                let mut paths = index_workspace_files(
                    &root,
                    QUICK_OPEN_LIMIT.saturating_add(1),
                    &worker_cancellation,
                );
                let truncated = paths.len() > QUICK_OPEN_LIMIT;
                paths.truncate(QUICK_OPEN_LIMIT);
                let paths = paths
                    .into_iter()
                    .filter_map(|path| path.strip_prefix(&root).ok()?.to_str().map(str::to_string))
                    .collect();
                let _ = sender.send(SearchWorkerMessage::QuickOpen {
                    request_id: worker_request_id,
                    paths,
                    truncated,
                });
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "failed to start editor quick open worker");
            return vec![HostMessage::QuickOpenCompleted {
                request_id,
                paths: Vec::new(),
                truncated: false,
            }];
        }
        *self.search_worker.borrow_mut() = Some(SearchWorker {
            request_id,
            kind: SearchWorkerKind::QuickOpen,
            cancellation,
            receiver,
        });
        Vec::new()
    }

    fn start_workspace_search(
        &self,
        request_id: String,
        query: String,
        options: SearchOptions,
    ) -> Vec<HostMessage> {
        self.cancel_current_search();
        let documents = match &*self.session.borrow() {
            Ok(session) => session.search_documents(),
            Err(error) => {
                return vec![HostMessage::WorkspaceSearchCompleted {
                    request_id,
                    result: WorkspaceSearchResult::default(),
                    error: Some(error.clone()),
                }];
            }
        };
        let root = self.workspace_root.clone();
        let cancellation = SearchCancellation::default();
        let worker_cancellation = cancellation.clone();
        let (sender, receiver) = mpsc::channel();
        let worker_request_id = request_id.clone();
        let spawned = std::thread::Builder::new()
            .name("flowmux-editor-search".into())
            .spawn(move || {
                let (result, error) = match search_workspace(
                    &root,
                    &query,
                    &options,
                    &documents,
                    &worker_cancellation,
                ) {
                    Ok(result) => (result, None),
                    Err(error) => (WorkspaceSearchResult::default(), Some(error.to_string())),
                };
                let _ = sender.send(SearchWorkerMessage::WorkspaceSearch {
                    request_id: worker_request_id,
                    result,
                    error,
                });
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "failed to start editor workspace search worker");
            return vec![HostMessage::WorkspaceSearchCompleted {
                request_id,
                result: WorkspaceSearchResult::default(),
                error: Some("Workspace search could not be started.".into()),
            }];
        }
        *self.search_worker.borrow_mut() = Some(SearchWorker {
            request_id,
            kind: SearchWorkerKind::WorkspaceSearch,
            cancellation,
            receiver,
        });
        Vec::new()
    }

    fn start_diff(&self, document_id: String, document_version: u64) -> Vec<HostMessage> {
        let target = match &*self.session.borrow() {
            Ok(session) => session.diff_target(&document_id, document_version),
            Err(error) => {
                tracing::warn!(%error, "editor document session is unavailable");
                return Vec::new();
            }
        };
        let target = match target {
            Ok(target) => target,
            Err(error) => {
                tracing::warn!(%error, "editor diff request was rejected");
                return Vec::new();
            }
        };
        let root = self.workspace_root.clone();
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("flowmux-editor-diff".into())
            .spawn(move || {
                let disk_content = diff_base_content(&root, &target);
                let _ = sender.send(HostMessage::ShowDiff {
                    document_id,
                    document_version,
                    disk_content,
                });
            });
        match spawned {
            // A newer request replaces the pending worker; its stale result
            // would be rejected by the WebView's version check anyway.
            Ok(_) => *self.diff_worker.borrow_mut() = Some(DiffWorker { receiver }),
            Err(error) => tracing::warn!(%error, "failed to start editor diff worker"),
        }
        Vec::new()
    }

    fn poll_diff_messages(&self) -> Vec<HostMessage> {
        let received = {
            let worker = self.diff_worker.borrow();
            let Some(worker) = worker.as_ref() else {
                return Vec::new();
            };
            match worker.receiver.try_recv() {
                Ok(message) => Some(message),
                Err(TryRecvError::Empty) => return Vec::new(),
                Err(TryRecvError::Disconnected) => None,
            }
        };
        self.diff_worker.borrow_mut().take();
        match received {
            Some(message) => vec![message],
            None => {
                tracing::warn!("editor diff worker stopped unexpectedly");
                Vec::new()
            }
        }
    }

    fn cancel_search(&self, request_id: &str) {
        let matches = self
            .search_worker
            .borrow()
            .as_ref()
            .is_some_and(|worker| worker.request_id == request_id);
        if matches {
            self.cancel_current_search();
        }
    }

    fn cancel_current_search(&self) {
        if let Some(worker) = self.search_worker.borrow_mut().take() {
            worker.cancellation.cancel();
        }
    }

    fn open_search_result(
        &self,
        relative_path: String,
        line: u32,
        column: u32,
        length: u32,
    ) -> Vec<HostMessage> {
        let path = self.workspace_root.join(relative_path);
        let result = match &mut *self.session.borrow_mut() {
            Ok(session) => session.open_search_result(path, line, column, length),
            Err(error) => {
                tracing::warn!(%error, "editor document session is unavailable");
                return Vec::new();
            }
        };
        match result {
            Ok(messages) => messages,
            Err(error) => {
                tracing::warn!(%error, "failed to open editor workspace search result");
                Vec::new()
            }
        }
    }

    pub(super) fn poll_search_messages(&self) -> Vec<HostMessage> {
        let mut messages = self.poll_diff_messages();
        messages.extend(self.poll_search_worker_messages());
        messages
    }

    fn poll_search_worker_messages(&self) -> Vec<HostMessage> {
        let received = {
            let workers = self.search_worker.borrow();
            let Some(worker) = workers.as_ref() else {
                return Vec::new();
            };
            match worker.receiver.try_recv() {
                Ok(message) => Ok(message),
                Err(TryRecvError::Empty) => return Vec::new(),
                Err(TryRecvError::Disconnected) => Err((worker.request_id.clone(), worker.kind)),
            }
        };
        self.search_worker.borrow_mut().take();
        match received {
            Ok(SearchWorkerMessage::QuickOpen {
                request_id,
                paths,
                truncated,
            }) => vec![HostMessage::QuickOpenCompleted {
                request_id,
                paths,
                truncated,
            }],
            Ok(SearchWorkerMessage::WorkspaceSearch {
                request_id,
                result,
                error,
            }) => vec![HostMessage::WorkspaceSearchCompleted {
                request_id,
                result,
                error,
            }],
            Err((request_id, SearchWorkerKind::QuickOpen)) => {
                tracing::warn!("editor quick open worker stopped unexpectedly");
                vec![HostMessage::QuickOpenCompleted {
                    request_id,
                    paths: Vec::new(),
                    truncated: false,
                }]
            }
            Err((request_id, SearchWorkerKind::WorkspaceSearch)) => {
                tracing::warn!("editor workspace search worker stopped unexpectedly");
                vec![HostMessage::WorkspaceSearchCompleted {
                    request_id,
                    result: WorkspaceSearchResult::default(),
                    error: Some("Workspace search stopped unexpectedly.".into()),
                }]
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

impl Drop for EditorHostState {
    fn drop(&mut self) {
        // The throttled flush timer holds only a weak reference; without this
        // final flush any recovery snapshot staged in the last few hundred
        // milliseconds would be lost when the surface is torn down (pane
        // close, workspace rerender, app quit).
        self.stage_recovery_operations();
        self.flush_recovery();
        if let Some(worker) = self.search_worker.get_mut().take() {
            worker.cancellation.cancel();
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
) -> EditorBridgeDispatch {
    let received = bridge.receive(raw);
    let mut scripts = received.scripts;
    let mut focus_direction = None;
    let mut native_edit_action = None;
    let mut native_edit_text = None;
    if let Some(message) = received.message {
        match message {
            EditorMessage::FocusDirectionRequested { direction } => {
                focus_direction = Some(direction);
            }
            EditorMessage::NativeEditRequested { action, text } => {
                native_edit_action = Some(action);
                native_edit_text = text;
            }
            message => {
                scripts.extend(queue_host_messages(bridge, host.handle(message)));
                if host.stage_recovery_operations() {
                    schedule_recovery_flush(host);
                }
            }
        }
    }
    EditorBridgeDispatch {
        scripts,
        focus_direction,
        native_edit_action,
        native_edit_text,
    }
}

// Throttle, not debounce: the flush fires a fixed delay after the first
// pending write. A debounce that re-arms per keystroke would defer the crash
// snapshot indefinitely while the user types continuously.
fn schedule_recovery_flush(host: &Rc<EditorHostState>) {
    if host.recovery_flush_pending.replace(true) {
        return;
    }
    let host = Rc::downgrade(host);
    gtk::glib::timeout_add_local_once(RECOVERY_DEBOUNCE, move || {
        let Some(host) = host.upgrade() else {
            return;
        };
        host.recovery_flush_pending.set(false);
        host.flush_recovery();
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
    use std::fs;

    #[cfg(target_os = "macos")]
    #[test]
    fn editor_navigation_keys_map_to_monaco_commands() {
        let cases = [
            (EditorNavigationKey::Left, "cursorLeft", "cursorLeftSelect"),
            (
                EditorNavigationKey::Right,
                "cursorRight",
                "cursorRightSelect",
            ),
            (EditorNavigationKey::Up, "cursorUp", "cursorUpSelect"),
            (EditorNavigationKey::Down, "cursorDown", "cursorDownSelect"),
            (
                EditorNavigationKey::PageUp,
                "cursorPageUp",
                "cursorPageUpSelect",
            ),
            (
                EditorNavigationKey::PageDown,
                "cursorPageDown",
                "cursorPageDownSelect",
            ),
            (EditorNavigationKey::Home, "cursorHome", "cursorHomeSelect"),
            (EditorNavigationKey::End, "cursorEnd", "cursorEndSelect"),
        ];

        for (key, plain, selecting) in cases {
            assert_eq!(key.monaco_action(false), plain);
            assert_eq!(key.monaco_action(true), selecting);
        }
    }

    fn wait_for_search(host: &EditorHostState) -> Vec<HostMessage> {
        for _ in 0..100 {
            let messages = host.poll_search_messages();
            if !messages.is_empty() {
                return messages;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("editor search worker did not complete");
    }

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

    #[test]
    fn focus_navigation_message_bypasses_document_session() {
        let workspace = tempfile::tempdir().unwrap();
        let host = Rc::new(EditorHostState::new(
            workspace.path(),
            EditorSessionState::default(),
        ));
        let bridge = EditorBridgeState::new(SurfaceId::new());
        let raw = serde_json::json!({
            "protocolVersion": flowmux_editor::PROTOCOL_VERSION,
            "surfaceId": bridge.surface_id,
            "type": "focus_direction_requested",
            "direction": "down"
        })
        .to_string();

        let dispatch = handle_bridge_message(&bridge, &host, &raw);

        assert_eq!(dispatch.focus_direction, Some(EditorFocusDirection::Down));
        assert!(dispatch.scripts.is_empty());
    }

    #[test]
    fn native_copy_message_preserves_the_monaco_selection() {
        let workspace = tempfile::tempdir().unwrap();
        let host = Rc::new(EditorHostState::new(
            workspace.path(),
            EditorSessionState::default(),
        ));
        let bridge = EditorBridgeState::new(SurfaceId::new());
        let raw = serde_json::json!({
            "protocolVersion": flowmux_editor::PROTOCOL_VERSION,
            "surfaceId": bridge.surface_id,
            "type": "native_edit_requested",
            "action": "copy",
            "text": "선택한 text",
        })
        .to_string();

        let dispatch = handle_bridge_message(&bridge, &host, &raw);

        assert_eq!(
            dispatch.native_edit_action,
            Some(EditorNativeEditAction::Copy)
        );
        assert_eq!(dispatch.native_edit_text.as_deref(), Some("선택한 text"));
        assert!(dispatch.scripts.is_empty());
    }

    #[test]
    fn latest_workspace_search_opens_multilingual_result_at_range() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("문서-日本語🙂.txt");
        fs::write(&path, "첫 줄\n찾을 값🙂\n").unwrap();
        let host = EditorHostState::new(workspace.path(), EditorSessionState::default());

        host.handle(EditorMessage::WorkspaceSearchRequested {
            request_id: "search-old".into(),
            query: "missing".into(),
            options: SearchOptions::default(),
        });
        host.handle(EditorMessage::WorkspaceSearchRequested {
            request_id: "search-latest".into(),
            query: "값🙂".into(),
            options: SearchOptions::default(),
        });

        let completion = wait_for_search(&host);
        let [HostMessage::WorkspaceSearchCompleted {
            request_id,
            result,
            error,
        }] = completion.as_slice()
        else {
            panic!("expected workspace search completion");
        };
        assert_eq!(request_id, "search-latest");
        assert!(error.is_none());
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].path, "문서-日本語🙂.txt");

        let messages = host.handle(EditorMessage::SearchResultOpenRequested {
            path: result.matches[0].path.clone(),
            line: result.matches[0].line,
            column: result.matches[0].column,
            length: result.matches[0].length,
        });
        assert!(matches!(
            messages.as_slice(),
            [
                HostMessage::OpenDocument { .. },
                HostMessage::RevealRange {
                    line: 1,
                    column: 3,
                    length: 3,
                    ..
                }
            ]
        ));
    }
}
