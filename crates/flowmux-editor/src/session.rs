// SPDX-License-Identifier: GPL-3.0-or-later
//! Headless document session behind one editor surface.

use crate::{
    ConflictAction, DiskStatus, DocumentDiskStatus, DocumentError, DocumentId, DocumentPayload,
    DocumentService, DocumentSnapshot, EditorMessage, HostMessage, LineEnding, RecoveryChoice,
    RecoveryDiskState, RecoveryOperation, RecoverySnapshot, RecoveryStore, SearchDocument,
    TextDocumentEncoding, TextDocumentLineEnding, TextEncoding,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EditorSessionError {
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error("unknown editor document ID: {0}")]
    UnknownDocument(String),
    #[error("stale editor message for {document_id}: expected version {expected}, got {actual}")]
    StaleMessageVersion {
        document_id: String,
        expected: u64,
        actual: u64,
    },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditorViewState {
    pub cursor_line: u32,
    pub cursor_column: u32,
    pub scroll_top: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditorFileSessionState {
    pub path: PathBuf,
    pub view: EditorViewState,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditorSessionSnapshot {
    pub open_files: Vec<EditorFileSessionState>,
    pub active_file: Option<PathBuf>,
}

/// Runtime state for the documents displayed by one editor WebView.
pub struct EditorSession {
    documents: DocumentService,
    protocol_ids: HashMap<String, DocumentId>,
    open_order: Vec<DocumentId>,
    active: Option<DocumentId>,
    reported_disk_status: HashMap<DocumentId, DiskStatus>,
    view_states: HashMap<DocumentId, EditorViewState>,
    recovery_store: Option<RecoveryStore>,
    pending_recoveries: HashMap<DocumentId, (RecoverySnapshot, RecoveryDiskState)>,
    recovery_base_hashes: HashMap<DocumentId, String>,
    recovery_operations: Vec<RecoveryOperation>,
}

impl EditorSession {
    pub fn new(workspace_root: impl AsRef<Path>) -> Result<Self, DocumentError> {
        Ok(Self {
            documents: DocumentService::new(workspace_root)?,
            protocol_ids: HashMap::new(),
            open_order: Vec::new(),
            active: None,
            reported_disk_status: HashMap::new(),
            view_states: HashMap::new(),
            recovery_store: None,
            pending_recoveries: HashMap::new(),
            recovery_base_hashes: HashMap::new(),
            recovery_operations: Vec::new(),
        })
    }

    pub fn with_recovery_store(
        workspace_root: impl AsRef<Path>,
        recovery_store: RecoveryStore,
    ) -> Result<Self, DocumentError> {
        let mut session = Self::new(workspace_root)?;
        session.recovery_store = Some(recovery_store);
        Ok(session)
    }

    pub fn take_recovery_operations(&mut self) -> Vec<RecoveryOperation> {
        std::mem::take(&mut self.recovery_operations)
    }

    pub fn session_snapshot(&self) -> EditorSessionSnapshot {
        EditorSessionSnapshot {
            open_files: self
                .open_order
                .iter()
                .filter_map(|id| {
                    let document = self.documents.snapshot(*id).ok()?;
                    Some(EditorFileSessionState {
                        path: document.display_path,
                        view: self.view_states.get(id).cloned().unwrap_or_default(),
                    })
                })
                .collect(),
            active_file: self
                .active
                .and_then(|id| self.documents.snapshot(id).ok())
                .map(|document| document.display_path),
        }
    }

    pub fn activate_path(&mut self, path: impl AsRef<Path>) {
        let path = path.as_ref();
        let active = self.open_order.iter().copied().find(|id| {
            self.documents.snapshot(*id).is_ok_and(|document| {
                document.display_path == path || document.identity_path == path
            })
        });
        if active.is_some() {
            self.active = active;
        }
    }

    pub fn initialize_message(&self, workspace_name: String) -> HostMessage {
        let documents = self
            .open_order
            .iter()
            .filter_map(|id| self.documents.snapshot(*id).ok())
            .map(|snapshot| self.payload(snapshot))
            .collect();
        HostMessage::InitializeEditor {
            workspace_name,
            documents,
            active_document_id: self.active.map(protocol_id),
        }
    }

    pub fn contains_document(&self, path: impl AsRef<Path>) -> bool {
        self.documents.contains_path(path)
    }

    pub fn dirty_document_paths(&self) -> Vec<PathBuf> {
        self.open_order
            .iter()
            .filter_map(|id| self.documents.snapshot(*id).ok())
            .filter(|snapshot| snapshot.is_dirty())
            .map(|snapshot| snapshot.display_path)
            .collect()
    }

    pub fn search_documents(&self) -> Vec<SearchDocument> {
        self.open_order
            .iter()
            .filter_map(|id| self.documents.snapshot(*id).ok())
            .map(|snapshot| SearchDocument {
                path: snapshot.identity_path,
                content: snapshot.text,
            })
            .collect()
    }

    pub fn open_search_result(
        &mut self,
        path: impl AsRef<Path>,
        line: u32,
        column: u32,
        length: u32,
    ) -> Result<Vec<HostMessage>, DocumentError> {
        let mut messages = self.open_document(path)?;
        let Some(active) = self.active else {
            return Ok(messages);
        };
        let snapshot = self.documents.snapshot(active)?;
        messages.push(HostMessage::RevealRange {
            document_id: protocol_id(active),
            document_version: snapshot.version,
            line,
            column,
            length,
        });
        Ok(messages)
    }

    /// Save every dirty document before its editor surface is closed.
    ///
    /// Successful saves are returned as replacement messages so a surface that
    /// remains open after a later save failure is resynchronized with the host.
    pub fn save_all_dirty(&mut self) -> (Vec<HostMessage>, Result<(), EditorSessionError>) {
        let mut messages = Vec::new();
        for id in self.open_order.clone() {
            let snapshot = match self.documents.snapshot(id) {
                Ok(snapshot) => snapshot,
                Err(error) => return (messages, Err(error.into())),
            };
            if !snapshot.is_dirty() {
                continue;
            }
            if self.recovery_base_hashes.contains_key(&id) {
                let error = DocumentError::ExternalChange {
                    path: snapshot.display_path,
                };
                return (messages, Err(error.into()));
            }
            match self.documents.save(id, snapshot.version) {
                Ok(saved) => {
                    self.reported_disk_status.insert(id, DiskStatus::Unchanged);
                    self.queue_recovery_removal(id, &saved.identity_path);
                    messages.push(HostMessage::ReplaceDocument {
                        document: self.payload(saved),
                    });
                }
                Err(error) => return (messages, Err(error.into())),
            }
        }
        (messages, Ok(()))
    }

    pub fn discard_all_dirty(&mut self) {
        let dirty_ids: Vec<DocumentId> = self
            .open_order
            .iter()
            .copied()
            .filter(|id| {
                self.documents
                    .snapshot(*id)
                    .is_ok_and(|snapshot| snapshot.is_dirty())
            })
            .collect();
        for id in dirty_ids {
            let Ok(snapshot) = self.documents.snapshot(id) else {
                continue;
            };
            // "Discard" drops the changes, not the file: revert to the disk
            // content so the session keeps the open tabs across a restart.
            // Only when the file cannot be reloaded (e.g. deleted) is the
            // document forgotten entirely.
            if self.documents.reload_discarding_changes(id).is_ok() {
                self.reported_disk_status.insert(id, DiskStatus::Unchanged);
            } else {
                if self.documents.discard(id).is_err() {
                    continue;
                }
                self.protocol_ids.retain(|_, mapped| *mapped != id);
                self.open_order.retain(|candidate| *candidate != id);
                self.reported_disk_status.remove(&id);
                self.view_states.remove(&id);
            }
            self.queue_recovery_removal(id, &snapshot.identity_path);
        }
        if self.active.is_some_and(|id| !self.open_order.contains(&id)) {
            self.active = self.open_order.last().copied();
        }
    }

    pub fn open_document(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<Vec<HostMessage>, DocumentError> {
        self.open_document_with_view(path, EditorViewState::default())
    }

    pub fn restore_document(
        &mut self,
        path: impl AsRef<Path>,
        view: EditorViewState,
    ) -> Result<Vec<HostMessage>, DocumentError> {
        self.open_document_with_view(path, view)
    }

    fn open_document_with_view(
        &mut self,
        path: impl AsRef<Path>,
        view: EditorViewState,
    ) -> Result<Vec<HostMessage>, DocumentError> {
        let opened = self.documents.open(path)?;
        let id = opened.document.id;
        let document_id = protocol_id(id);
        self.protocol_ids.insert(document_id.clone(), id);
        if !self.open_order.contains(&id) {
            self.open_order.push(id);
        }
        if !opened.already_open {
            self.reported_disk_status.insert(id, DiskStatus::Unchanged);
            self.view_states.insert(id, view);
        }
        self.active = Some(id);

        if opened.already_open {
            return Ok(vec![HostMessage::SetActiveDocument {
                document_id,
                document_version: opened.document.version,
            }]);
        }

        let mut messages = vec![HostMessage::OpenDocument {
            document: self.payload(opened.document.clone()),
        }];
        if let Some(store) = &self.recovery_store {
            if let Ok(Some((recovery, disk_state))) = store.read(&opened.document.identity_path) {
                if recovery.content == opened.document.text {
                    self.recovery_operations.push(RecoveryOperation::Remove(
                        opened.document.identity_path.clone(),
                    ));
                } else {
                    messages.push(HostMessage::RecoveryAvailable {
                        document_id: document_id.clone(),
                        document_version: opened.document.version,
                        disk_state,
                    });
                    self.pending_recoveries.insert(id, (recovery, disk_state));
                }
            }
        }
        Ok(messages)
    }

    pub fn poll_external_changes(&mut self) -> Result<Vec<HostMessage>, EditorSessionError> {
        let mut messages = Vec::new();
        for id in self.open_order.clone() {
            let status = self.documents.disk_status(id)?;
            let previous = self
                .reported_disk_status
                .get(&id)
                .copied()
                .unwrap_or(DiskStatus::Unchanged);
            let (version, dirty) = self.documents.version_and_dirty(id)?;

            // A pending recovery proposal pins the document version; reloading
            // here would bump it and make the eventual `RecoveryDecision` stale.
            if status == DiskStatus::Modified
                && !dirty
                && !self.pending_recoveries.contains_key(&id)
            {
                if let Ok(reloaded) = self.documents.reload_from_disk(id) {
                    self.reported_disk_status.insert(id, DiskStatus::Unchanged);
                    messages.push(HostMessage::ReplaceDocument {
                        document: self.payload(reloaded),
                    });
                    continue;
                }
            }

            if status != previous {
                self.reported_disk_status.insert(id, status);
                messages.push(HostMessage::DocumentDiskStatus {
                    document_id: protocol_id(id),
                    document_version: version,
                    status: protocol_disk_status(status),
                });
            }
        }
        Ok(messages)
    }

    pub fn handle_editor_message(
        &mut self,
        message: EditorMessage,
    ) -> Result<Vec<HostMessage>, EditorSessionError> {
        match message {
            EditorMessage::EditorReady => Ok(Vec::new()),
            EditorMessage::ActiveDocumentChanged {
                document_id,
                document_version,
            } => {
                let id = self.checked_document(&document_id, document_version)?;
                self.active = Some(id);
                Ok(Vec::new())
            }
            EditorMessage::DocumentChanged {
                document_id,
                document_version,
                content,
                ..
            } => {
                let id = self.checked_document(&document_id, document_version)?;
                self.documents.update_text(id, document_version, content)?;
                self.queue_recovery_write(id)?;
                Ok(Vec::new())
            }
            EditorMessage::SaveRequested {
                document_id,
                document_version,
                change_sequence,
                content,
            } => Ok(vec![self.save_requested(
                document_id,
                document_version,
                change_sequence,
                content,
            )]),
            EditorMessage::SaveAsRequested {
                document_id,
                document_version,
                change_sequence,
                content,
                path,
                overwrite,
            } => Ok(vec![self.save_as_requested(
                document_id,
                document_version,
                change_sequence,
                content,
                path,
                overwrite,
            )]),
            EditorMessage::CloseRequested {
                document_id,
                document_version,
                ..
            } => self.close_document(document_id, document_version, false),
            EditorMessage::DiscardCloseRequested {
                document_id,
                document_version,
            } => self.close_document(document_id, document_version, true),
            EditorMessage::RecoveryDecision {
                document_id,
                document_version,
                choice,
            } => self.recovery_decision(document_id, document_version, choice),
            EditorMessage::ViewStateChanged {
                document_id,
                document_version,
                cursor_line,
                cursor_column,
                scroll_top,
            } => {
                let id = self.checked_document(&document_id, document_version)?;
                self.view_states.insert(
                    id,
                    EditorViewState {
                        cursor_line,
                        cursor_column,
                        scroll_top,
                    },
                );
                Ok(Vec::new())
            }
            // Search and diff are handled by the pane on worker threads; the
            // session only validates and supplies their inputs.
            EditorMessage::QuickOpenRequested { .. }
            | EditorMessage::WorkspaceSearchRequested { .. }
            | EditorMessage::SearchCancelled { .. }
            | EditorMessage::SearchResultOpenRequested { .. }
            | EditorMessage::DiffRequested { .. } => Ok(Vec::new()),
            EditorMessage::ConflictActionRequested {
                document_id,
                document_version,
                action,
            } => Ok(vec![self.conflict_action_requested(
                document_id,
                document_version,
                action,
            )]),
        }
    }

    fn close_document(
        &mut self,
        document_id: String,
        document_version: u64,
        discard: bool,
    ) -> Result<Vec<HostMessage>, EditorSessionError> {
        let id = self.checked_document(&document_id, document_version)?;
        let identity_path = self.documents.snapshot(id)?.identity_path;
        if discard {
            self.documents.discard(id)?;
        } else {
            self.documents.close(id)?;
        }
        self.protocol_ids.remove(&document_id);
        self.open_order.retain(|candidate| *candidate != id);
        self.reported_disk_status.remove(&id);
        self.view_states.remove(&id);
        // An undecided recovery proposal must survive the close: deleting the
        // snapshot here would silently discard the only copy of crash edits
        // the user never answered `RecoveryChoice::Discard` for.
        if self.pending_recoveries.remove(&id).is_some() {
            self.recovery_base_hashes.remove(&id);
        } else {
            self.queue_recovery_removal(id, &identity_path);
        }
        if self.active == Some(id) {
            self.active = self.open_order.last().copied();
        }
        Ok(vec![HostMessage::CloseDocument {
            document_id,
            document_version,
        }])
    }

    fn save_requested(
        &mut self,
        document_id: String,
        document_version: u64,
        change_sequence: u64,
        content: String,
    ) -> HostMessage {
        let result: Result<DocumentSnapshot, EditorSessionError> = (|| {
            let id = self.checked_document(&document_id, document_version)?;
            if self.recovery_base_hashes.contains_key(&id) {
                return Err(DocumentError::ExternalChange {
                    path: self.documents.snapshot(id)?.display_path,
                }
                .into());
            }
            let snapshot = self.documents.snapshot(id)?;
            // Run the failure-prone checks before `update_text`: a failed save
            // after a version bump would leave the WebView without the new
            // version, so every later message would be rejected as stale.
            if snapshot.read_only {
                return Err(DocumentError::ReadOnly {
                    path: snapshot.display_path.clone(),
                }
                .into());
            }
            if self.documents.disk_status(id)? != DiskStatus::Unchanged {
                return Err(DocumentError::ExternalChange {
                    path: snapshot.display_path.clone(),
                }
                .into());
            }
            let version = if snapshot.text == content {
                snapshot.version
            } else {
                self.documents
                    .update_text(id, document_version, content)?
                    .version
            };
            let saved = self.documents.save(id, version)?;
            self.queue_recovery_removal(id, &saved.identity_path);
            Ok(saved)
        })();

        match result {
            Ok(snapshot) => HostMessage::SaveCompleted {
                document_id,
                document_version: snapshot.version,
                change_sequence,
            },
            Err(error) => {
                let conflict = matches!(
                    &error,
                    EditorSessionError::Document(DocumentError::ExternalChange { .. })
                );
                HostMessage::SaveFailed {
                    document_id,
                    document_version,
                    change_sequence,
                    reason: error.to_string(),
                    conflict,
                }
            }
        }
    }

    fn save_as_requested(
        &mut self,
        document_id: String,
        document_version: u64,
        change_sequence: u64,
        content: String,
        relative_path: String,
        overwrite: bool,
    ) -> HostMessage {
        let result: Result<DocumentSnapshot, EditorSessionError> = (|| {
            let id = self.checked_document(&document_id, document_version)?;
            let snapshot = self.documents.snapshot(id)?;
            let target = self.documents.workspace_root().join(relative_path);
            // Check the common refusal before `update_text` so a failed save
            // does not bump the version behind the WebView's back.
            if !overwrite && std::fs::symlink_metadata(&target).is_ok() {
                return Err(DocumentError::TargetExists { path: target }.into());
            }
            let version = if snapshot.text == content {
                snapshot.version
            } else {
                self.documents
                    .update_text(id, document_version, content)?
                    .version
            };
            let old_identity = snapshot.identity_path;
            let saved = self.documents.save_as(id, version, target, overwrite)?;
            self.reported_disk_status.insert(id, DiskStatus::Unchanged);
            self.queue_recovery_removal(id, &old_identity);
            Ok(saved)
        })();

        match result {
            Ok(snapshot) => HostMessage::SaveAsCompleted {
                document: self.payload(snapshot),
                change_sequence,
            },
            Err(error) => HostMessage::SaveAsFailed {
                document_id,
                document_version,
                change_sequence,
                target_exists: matches!(
                    &error,
                    EditorSessionError::Document(DocumentError::TargetExists { .. })
                ),
                reason: error.to_string(),
            },
        }
    }

    fn conflict_action_requested(
        &mut self,
        document_id: String,
        document_version: u64,
        action: ConflictAction,
    ) -> HostMessage {
        let result: Result<HostMessage, EditorSessionError> = (|| {
            let id = self.checked_document(&document_id, document_version)?;
            match action {
                ConflictAction::Compare => Ok(HostMessage::ShowDiff {
                    document_id: document_id.clone(),
                    document_version,
                    disk_content: self.documents.disk_text(id)?,
                }),
                ConflictAction::KeepMine => {
                    self.documents.accept_external_as_base(id)?;
                    self.recovery_base_hashes.remove(&id);
                    self.reported_disk_status.insert(id, DiskStatus::Unchanged);
                    self.queue_recovery_write(id)?;
                    Ok(HostMessage::ReplaceDocument {
                        document: self.payload(self.documents.snapshot(id)?),
                    })
                }
                ConflictAction::ReloadFromDisk => {
                    let identity_path = self.documents.snapshot(id)?.identity_path;
                    let reloaded = self.documents.reload_discarding_changes(id)?;
                    self.reported_disk_status.insert(id, DiskStatus::Unchanged);
                    self.queue_recovery_removal(id, &identity_path);
                    Ok(HostMessage::ReplaceDocument {
                        document: self.payload(reloaded),
                    })
                }
            }
        })();

        result.unwrap_or_else(|error| HostMessage::ConflictActionFailed {
            document_id,
            document_version,
            reason: error.to_string(),
        })
    }

    /// Recovery proposals the user has not answered yet, re-emitted with the
    /// documents' current versions. Used to rebuild the WebView's prompts
    /// after its web process crashed and the page reloaded.
    pub fn pending_recovery_messages(&self) -> Vec<HostMessage> {
        self.pending_recoveries
            .iter()
            .filter_map(|(id, (_, disk_state))| {
                let (version, _) = self.documents.version_and_dirty(*id).ok()?;
                Some(HostMessage::RecoveryAvailable {
                    document_id: protocol_id(*id),
                    document_version: version,
                    disk_state: *disk_state,
                })
            })
            .collect()
    }

    /// Validate a `DiffRequested` message and return the file the diff base
    /// should be computed for. The content fetch itself (`diff_base_content`)
    /// runs `git` and reads the disk, so callers run it off the UI thread.
    pub fn diff_target(
        &self,
        document_id: &str,
        document_version: u64,
    ) -> Result<PathBuf, EditorSessionError> {
        let id = self.checked_document(document_id, document_version)?;
        Ok(self.documents.snapshot(id)?.identity_path)
    }

    fn recovery_decision(
        &mut self,
        document_id: String,
        document_version: u64,
        choice: RecoveryChoice,
    ) -> Result<Vec<HostMessage>, EditorSessionError> {
        let id = self.checked_document(&document_id, document_version)?;
        let Some((mut recovery, disk_state)) = self.pending_recoveries.remove(&id) else {
            return Ok(Vec::new());
        };
        if choice == RecoveryChoice::Discard {
            self.recovery_operations
                .push(RecoveryOperation::Remove(recovery.identity_path));
            return Ok(Vec::new());
        }

        let restored =
            self.documents
                .update_text(id, document_version, recovery.content.clone())?;
        recovery.document_version = restored.version;
        if disk_state != RecoveryDiskState::Unchanged {
            self.recovery_base_hashes
                .insert(id, recovery.base_hash.clone());
        }
        self.recovery_operations
            .push(RecoveryOperation::Write(recovery));
        Ok(vec![HostMessage::ReplaceDocument {
            document: self.payload(restored),
        }])
    }

    fn queue_recovery_write(&mut self, id: DocumentId) -> Result<(), DocumentError> {
        let Some(store) = &self.recovery_store else {
            return Ok(());
        };
        let mut recovery = self.documents.recovery_snapshot(id, store)?;
        if let Some(base_hash) = self.recovery_base_hashes.get(&id) {
            recovery.base_hash.clone_from(base_hash);
        }
        // Last write wins: drop a not-yet-drained write for the same document
        // so a typing burst queues one snapshot instead of one per keystroke.
        self.recovery_operations.retain(|operation| {
            !matches!(operation, RecoveryOperation::Write(pending)
                if pending.identity_path == recovery.identity_path)
        });
        self.recovery_operations
            .push(RecoveryOperation::Write(recovery));
        Ok(())
    }

    fn queue_recovery_removal(&mut self, id: DocumentId, identity_path: &Path) {
        self.pending_recoveries.remove(&id);
        self.recovery_base_hashes.remove(&id);
        if self.recovery_store.is_some() {
            self.recovery_operations
                .retain(|operation| operation.identity_path() != identity_path);
            self.recovery_operations
                .push(RecoveryOperation::Remove(identity_path.to_path_buf()));
        }
    }

    fn checked_document(
        &self,
        document_id: &str,
        document_version: u64,
    ) -> Result<DocumentId, EditorSessionError> {
        let id = self
            .protocol_ids
            .get(document_id)
            .copied()
            .ok_or_else(|| EditorSessionError::UnknownDocument(document_id.to_string()))?;
        let snapshot = self.documents.snapshot(id)?;
        if snapshot.version != document_version {
            return Err(EditorSessionError::StaleMessageVersion {
                document_id: document_id.to_string(),
                expected: snapshot.version,
                actual: document_version,
            });
        }
        Ok(id)
    }

    fn payload(&self, snapshot: DocumentSnapshot) -> DocumentPayload {
        let relative_path = snapshot
            .display_path
            .strip_prefix(self.documents.workspace_root())
            .or_else(|_| {
                snapshot
                    .identity_path
                    .strip_prefix(self.documents.workspace_root())
            })
            .unwrap_or(&snapshot.display_path)
            .to_string_lossy()
            .into_owned();
        let name = snapshot
            .display_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| relative_path.clone());
        let external_change = self.recovery_base_hashes.contains_key(&snapshot.id)
            || !matches!(
                self.documents.disk_status(snapshot.id),
                Ok(DiskStatus::Unchanged)
            );
        let dirty = snapshot.is_dirty();
        let encoding = match snapshot.encoding {
            TextEncoding::Utf8 => TextDocumentEncoding::Utf8,
            TextEncoding::Utf8Bom => TextDocumentEncoding::Utf8Bom,
        };
        let eol = match snapshot.line_ending {
            LineEnding::Lf => TextDocumentLineEnding::Lf,
            LineEnding::CrLf => TextDocumentLineEnding::CrLf,
        };
        let view = self
            .view_states
            .get(&snapshot.id)
            .cloned()
            .unwrap_or_default();

        DocumentPayload {
            id: protocol_id(snapshot.id),
            uri: format!("flowmux-document://{}", protocol_id(snapshot.id)),
            relative_path,
            name,
            content: snapshot.text,
            version: snapshot.version,
            language: None,
            encoding,
            eol,
            dirty,
            read_only: snapshot.read_only,
            external_change,
            cursor_line: view.cursor_line,
            cursor_column: view.cursor_column,
            scroll_top: view.scroll_top,
        }
    }
}

/// Content the diff view compares the editing buffer against: the file's
/// `HEAD` blob in a Git workspace, the on-disk content otherwise. Runs `git`
/// subprocesses and reads the file — call from a worker thread, not the UI.
pub fn diff_base_content(workspace_root: &Path, identity_path: &Path) -> String {
    match git_head_content(workspace_root, identity_path) {
        Some(content) => content.unwrap_or_default(),
        None => crate::read_text_document(identity_path, crate::DEFAULT_MAX_DOCUMENT_BYTES)
            .map(|loaded| loaded.text)
            .unwrap_or_default(),
    }
}

fn git_head_content(workspace_root: &Path, path: &Path) -> Option<Option<String>> {
    let root = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["rev-parse", "--show-toplevel"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !root.status.success() {
        return None;
    }
    let root = String::from_utf8(root.stdout).ok()?;
    let root = PathBuf::from(root.trim_end_matches(['\r', '\n']));
    let root = root.canonicalize().ok()?;
    let relative = path.strip_prefix(&root).ok()?;
    let relative = relative
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    let object = format!("HEAD:{relative}");

    let size = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["cat-file", "-s"])
        .arg(&object)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !size.status.success() {
        return Some(None);
    }
    let size = String::from_utf8(size.stdout).ok()?;
    let size = size.trim().parse::<u64>().ok()?;
    if size > crate::DEFAULT_MAX_DOCUMENT_BYTES {
        return None;
    }

    let content = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "-p"])
        .arg(object)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !content.status.success() {
        return None;
    }
    Some(Some(String::from_utf8(content.stdout).ok()?))
}

fn protocol_id(id: DocumentId) -> String {
    format!("document-{}", id.get())
}

fn protocol_disk_status(status: DiskStatus) -> DocumentDiskStatus {
    match status {
        DiskStatus::Unchanged => DocumentDiskStatus::Unchanged,
        DiskStatus::Modified => DocumentDiskStatus::Modified,
        DiskStatus::Deleted => DocumentDiskStatus::Deleted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::tempdir;

    fn open_payload(messages: Vec<HostMessage>) -> DocumentPayload {
        let [HostMessage::OpenDocument { document }] = messages.as_slice() else {
            panic!("expected a newly opened document");
        };
        document.clone()
    }

    fn apply_recovery_operations(session: &mut EditorSession, store: &RecoveryStore) {
        for operation in session.take_recovery_operations() {
            store.apply(&operation).unwrap();
        }
    }

    fn git(directory: &Path, arguments: &[&str]) {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .env("GIT_AUTHOR_NAME", "JunsuChoi")
            .env("GIT_AUTHOR_EMAIL", "jsuya.choi@samsung.com")
            .env("GIT_COMMITTER_NAME", "JunsuChoi")
            .env("GIT_COMMITTER_EMAIL", "jsuya.choi@samsung.com")
            .status()
            .unwrap();
        assert!(status.success(), "git {arguments:?} failed");
    }

    #[test]
    fn diff_uses_head_content_as_the_base_for_working_tree_changes() {
        let workspace = tempdir().unwrap();
        git(workspace.path(), &["init", "-q"]);
        let path = workspace.path().join("변경-日本語🙂.txt");
        fs::write(&path, "기준 내용\n").unwrap();
        git(workspace.path(), &["add", "."]);
        git(
            workspace.path(),
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "baseline",
            ],
        );
        fs::write(&path, "작업 중인 내용\n").unwrap();

        let mut session = EditorSession::new(workspace.path()).unwrap();
        let document = open_payload(session.open_document(&path).unwrap());
        let target = session.diff_target(&document.id, document.version).unwrap();

        assert_eq!(target, fs::canonicalize(&path).unwrap());
        assert_eq!(
            diff_base_content(&fs::canonicalize(workspace.path()).unwrap(), &target),
            "기준 내용\n"
        );
    }

    #[test]
    fn multilingual_open_and_duplicate_focus_preserve_path_and_text() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("문서-日本語-🙂.rs");
        fs::write(&path, "fn main() { println!(\"안녕하세요\"); }\n").unwrap();
        let mut session = EditorSession::new(workspace.path()).unwrap();

        let document = open_payload(session.open_document(&path).unwrap());
        assert_eq!(document.relative_path, "문서-日本語-🙂.rs");
        assert_eq!(document.name, "문서-日本語-🙂.rs");
        assert!(document.content.contains("안녕하세요"));
        assert!(!document.dirty);

        assert!(matches!(
            session.open_document(&path).unwrap().as_slice(),
            [HostMessage::SetActiveDocument { document_id, .. }] if document_id == &document.id
        ));
        assert!(session.contains_document(&path));
        assert!(!session.contains_document(workspace.path().join("missing.rs")));
    }

    #[test]
    fn restored_files_active_document_and_view_state_round_trip() {
        let workspace = tempdir().unwrap();
        let first_path = workspace.path().join("첫째.rs");
        let second_path = workspace.path().join("第二.rs");
        fs::write(&first_path, "first\n").unwrap();
        fs::write(&second_path, "second\n").unwrap();
        let mut session = EditorSession::new(workspace.path()).unwrap();
        let first = open_payload(
            session
                .restore_document(
                    &first_path,
                    EditorViewState {
                        cursor_line: 7,
                        cursor_column: 3,
                        scroll_top: 88.5,
                    },
                )
                .unwrap(),
        );
        session.open_document(&second_path).unwrap();
        session.activate_path(&first_path);

        let HostMessage::InitializeEditor {
            documents,
            active_document_id,
            ..
        } = session.initialize_message("workspace".into())
        else {
            panic!("expected editor initialization");
        };
        assert_eq!(active_document_id.as_deref(), Some(first.id.as_str()));
        assert_eq!(documents[0].cursor_line, 7);
        assert_eq!(documents[0].cursor_column, 3);
        assert_eq!(documents[0].scroll_top, 88.5);

        session
            .handle_editor_message(EditorMessage::ViewStateChanged {
                document_id: first.id,
                document_version: first.version,
                cursor_line: 12,
                cursor_column: 5,
                scroll_top: 144.0,
            })
            .unwrap();
        let snapshot = session.session_snapshot();
        assert_eq!(snapshot.active_file.as_deref(), Some(first_path.as_path()));
        assert_eq!(snapshot.open_files.len(), 2);
        assert_eq!(snapshot.open_files[0].view.cursor_line, 12);
        assert_eq!(snapshot.open_files[0].view.cursor_column, 5);
        assert_eq!(snapshot.open_files[0].view.scroll_top, 144.0);
    }

    #[test]
    fn search_documents_use_live_text_and_result_reveals_utf16_range() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("검색🙂.txt");
        fs::write(&path, "disk\n").unwrap();
        let mut session = EditorSession::new(workspace.path()).unwrap();
        let document = open_payload(session.open_document(&path).unwrap());
        session
            .handle_editor_message(EditorMessage::DocumentChanged {
                document_id: document.id,
                document_version: document.version,
                change_sequence: 1,
                content: "live 검색🙂\n".into(),
            })
            .unwrap();

        assert_eq!(
            session.search_documents(),
            vec![SearchDocument {
                path: fs::canonicalize(&path).unwrap(),
                content: "live 검색🙂\n".into(),
            }]
        );
        let messages = session.open_search_result(path, 7, 3, 4).unwrap();
        assert!(matches!(
            messages.last(),
            Some(HostMessage::RevealRange {
                line: 7,
                column: 3,
                length: 4,
                ..
            })
        ));
    }

    #[test]
    fn sequential_edits_and_save_round_trip_to_disk() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("save.txt");
        fs::write(&path, "처음\n").unwrap();
        let mut session = EditorSession::new(workspace.path()).unwrap();
        let document = open_payload(session.open_document(&path).unwrap());

        session
            .handle_editor_message(EditorMessage::DocumentChanged {
                document_id: document.id.clone(),
                document_version: 1,
                change_sequence: 1,
                content: "두 번째\n".into(),
            })
            .unwrap();
        session
            .handle_editor_message(EditorMessage::DocumentChanged {
                document_id: document.id.clone(),
                document_version: 2,
                change_sequence: 2,
                content: "저장됨 日本語 🙂\n".into(),
            })
            .unwrap();
        let response = session
            .handle_editor_message(EditorMessage::SaveRequested {
                document_id: document.id,
                document_version: 3,
                change_sequence: 2,
                content: "저장됨 日本語 🙂\n".into(),
            })
            .unwrap();

        assert!(matches!(
            response.as_slice(),
            [HostMessage::SaveCompleted {
                document_version: 3,
                change_sequence: 2,
                ..
            }]
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), "저장됨 日本語 🙂\n");
    }

    #[test]
    fn save_as_requires_overwrite_and_updates_multilingual_session_path() {
        let workspace = tempdir().unwrap();
        let source = workspace.path().join("source.txt");
        let target = workspace.path().join("저장-日本語🙂.txt");
        fs::write(&source, "source\n").unwrap();
        fs::write(&target, "existing\n").unwrap();
        let mut session = EditorSession::new(workspace.path()).unwrap();
        let document = open_payload(session.open_document(&source).unwrap());

        let failed = session
            .handle_editor_message(EditorMessage::SaveAsRequested {
                document_id: document.id.clone(),
                document_version: document.version,
                change_sequence: 0,
                content: "새 내용🙂\n".into(),
                path: "저장-日本語🙂.txt".into(),
                overwrite: false,
            })
            .unwrap();
        assert!(matches!(
            failed.as_slice(),
            [HostMessage::SaveAsFailed {
                target_exists: true,
                ..
            }]
        ));
        assert_eq!(fs::read_to_string(&target).unwrap(), "existing\n");

        let current = session.documents.snapshot(DocumentId(1)).unwrap();
        let saved = session
            .handle_editor_message(EditorMessage::SaveAsRequested {
                document_id: document.id,
                document_version: current.version,
                change_sequence: 1,
                content: "새 내용🙂\n".into(),
                path: "저장-日本語🙂.txt".into(),
                overwrite: true,
            })
            .unwrap();
        assert!(matches!(
            saved.as_slice(),
            [HostMessage::SaveAsCompleted { document, .. }]
                if document.relative_path == "저장-日本語🙂.txt" && !document.dirty
        ));
        assert_eq!(fs::read_to_string(&target).unwrap(), "새 내용🙂\n");
        let canonical_target = fs::canonicalize(target).unwrap();
        assert_eq!(
            session.session_snapshot().active_file.as_deref(),
            Some(canonical_target.as_path())
        );
    }

    #[test]
    fn conflict_compare_keep_mine_and_reload_are_explicit_session_actions() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("충돌.txt");
        fs::write(&path, "base\n").unwrap();
        let mut session = EditorSession::new(workspace.path()).unwrap();
        let document = open_payload(session.open_document(&path).unwrap());
        session
            .handle_editor_message(EditorMessage::DocumentChanged {
                document_id: document.id.clone(),
                document_version: document.version,
                change_sequence: 1,
                content: "내 변경🙂\n".into(),
            })
            .unwrap();
        fs::write(&path, "외부 변경 日本語\n").unwrap();
        let current = session.documents.snapshot(DocumentId(1)).unwrap();

        let compared = session
            .handle_editor_message(EditorMessage::ConflictActionRequested {
                document_id: document.id.clone(),
                document_version: current.version,
                action: ConflictAction::Compare,
            })
            .unwrap();
        assert!(matches!(
            compared.as_slice(),
            [HostMessage::ShowDiff { disk_content, .. }]
                if disk_content == "외부 변경 日本語\n"
        ));
        assert_eq!(
            session.documents.snapshot(DocumentId(1)).unwrap().text,
            "내 변경🙂\n"
        );

        let kept = session
            .handle_editor_message(EditorMessage::ConflictActionRequested {
                document_id: document.id.clone(),
                document_version: current.version,
                action: ConflictAction::KeepMine,
            })
            .unwrap();
        assert!(matches!(
            kept.as_slice(),
            [HostMessage::ReplaceDocument { document }]
                if document.dirty && !document.external_change
        ));
        let saved = session
            .handle_editor_message(EditorMessage::SaveRequested {
                document_id: document.id.clone(),
                document_version: current.version,
                change_sequence: 1,
                content: "내 변경🙂\n".into(),
            })
            .unwrap();
        assert!(matches!(
            saved.as_slice(),
            [HostMessage::SaveCompleted { .. }]
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "내 변경🙂\n");

        let current = session.documents.snapshot(DocumentId(1)).unwrap();
        session
            .handle_editor_message(EditorMessage::DocumentChanged {
                document_id: document.id.clone(),
                document_version: current.version,
                change_sequence: 2,
                content: "버릴 변경\n".into(),
            })
            .unwrap();
        fs::write(&path, "디스크 선택\n").unwrap();
        let current = session.documents.snapshot(DocumentId(1)).unwrap();
        let reloaded = session
            .handle_editor_message(EditorMessage::ConflictActionRequested {
                document_id: document.id,
                document_version: current.version,
                action: ConflictAction::ReloadFromDisk,
            })
            .unwrap();
        assert!(matches!(
            reloaded.as_slice(),
            [HostMessage::ReplaceDocument { document }]
                if document.content == "디스크 선택\n" && !document.dirty
        ));
    }

    #[test]
    fn close_guard_saves_all_dirty_multilingual_documents() {
        let workspace = tempdir().unwrap();
        let first = workspace.path().join("한국어.txt");
        let second = workspace.path().join("日本語🙂.txt");
        fs::write(&first, "처음\n").unwrap();
        fs::write(&second, "最初\n").unwrap();
        let mut session = EditorSession::new(workspace.path()).unwrap();
        let first_document = open_payload(session.open_document(&first).unwrap());
        let second_document = open_payload(session.open_document(&second).unwrap());

        session
            .handle_editor_message(EditorMessage::DocumentChanged {
                document_id: first_document.id,
                document_version: first_document.version,
                change_sequence: 1,
                content: "저장됨🙂\n".into(),
            })
            .unwrap();
        session
            .handle_editor_message(EditorMessage::DocumentChanged {
                document_id: second_document.id,
                document_version: second_document.version,
                change_sequence: 1,
                content: "保存済み🙂\n".into(),
            })
            .unwrap();

        assert_eq!(
            session.dirty_document_paths(),
            vec![first.clone(), second.clone()]
        );
        let (messages, result) = session.save_all_dirty();

        result.unwrap();
        assert_eq!(messages.len(), 2);
        assert!(messages
            .iter()
            .all(|message| matches!(message, HostMessage::ReplaceDocument { document } if !document.dirty)));
        assert!(session.dirty_document_paths().is_empty());
        assert_eq!(fs::read_to_string(first).unwrap(), "저장됨🙂\n");
        assert_eq!(fs::read_to_string(second).unwrap(), "保存済み🙂\n");
    }

    #[test]
    fn close_guard_stops_at_external_conflict_without_overwriting_it() {
        let workspace = tempdir().unwrap();
        let first = workspace.path().join("first.txt");
        let conflict = workspace.path().join("충돌.txt");
        fs::write(&first, "base one\n").unwrap();
        fs::write(&conflict, "base two\n").unwrap();
        let mut session = EditorSession::new(workspace.path()).unwrap();
        let first_document = open_payload(session.open_document(&first).unwrap());
        let conflict_document = open_payload(session.open_document(&conflict).unwrap());

        for document in [&first_document, &conflict_document] {
            session
                .handle_editor_message(EditorMessage::DocumentChanged {
                    document_id: document.id.clone(),
                    document_version: document.version,
                    change_sequence: 1,
                    content: format!("editor {}\n", document.name),
                })
                .unwrap();
        }
        fs::write(&conflict, "external 日本語\n").unwrap();

        let (messages, result) = session.save_all_dirty();

        assert_eq!(messages.len(), 1);
        assert!(matches!(
            result,
            Err(EditorSessionError::Document(
                DocumentError::ExternalChange { .. }
            ))
        ));
        assert_eq!(fs::read_to_string(&first).unwrap(), "editor first.txt\n");
        assert_eq!(fs::read_to_string(&conflict).unwrap(), "external 日本語\n");
        assert_eq!(session.dirty_document_paths(), vec![conflict]);
    }

    #[test]
    fn close_guard_discard_removes_dirty_recovery_without_saving() {
        let workspace = tempdir().unwrap();
        let state = tempdir().unwrap();
        let path = workspace.path().join("버리기-日本語.txt");
        fs::write(&path, "disk\n").unwrap();
        let identity_path = fs::canonicalize(&path).unwrap();
        let store = RecoveryStore::new(state.path(), workspace.path()).unwrap();
        let mut session =
            EditorSession::with_recovery_store(workspace.path(), store.clone()).unwrap();
        let document = open_payload(session.open_document(&path).unwrap());
        session
            .handle_editor_message(EditorMessage::DocumentChanged {
                document_id: document.id,
                document_version: document.version,
                change_sequence: 1,
                content: "discard me\n".into(),
            })
            .unwrap();
        apply_recovery_operations(&mut session, &store);
        assert!(store.read(&identity_path).unwrap().is_some());

        session.discard_all_dirty();
        apply_recovery_operations(&mut session, &store);

        assert!(session.dirty_document_paths().is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), "disk\n");
        assert!(store.read(identity_path).unwrap().is_none());
    }

    #[test]
    fn stale_edits_are_rejected_before_mutating_the_document() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("stale.txt");
        fs::write(&path, "base\n").unwrap();
        let mut session = EditorSession::new(workspace.path()).unwrap();
        let document = open_payload(session.open_document(&path).unwrap());

        let error = session
            .handle_editor_message(EditorMessage::DocumentChanged {
                document_id: document.id,
                document_version: 9,
                change_sequence: 1,
                content: "bad\n".into(),
            })
            .unwrap_err();

        assert!(matches!(
            error,
            EditorSessionError::StaleMessageVersion { .. }
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), "base\n");
    }

    #[test]
    fn external_change_returns_a_save_failure_without_overwriting_disk() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("conflict.txt");
        fs::write(&path, "base\n").unwrap();
        let mut session = EditorSession::new(workspace.path()).unwrap();
        let document = open_payload(session.open_document(&path).unwrap());
        session
            .handle_editor_message(EditorMessage::DocumentChanged {
                document_id: document.id.clone(),
                document_version: 1,
                change_sequence: 1,
                content: "editor\n".into(),
            })
            .unwrap();
        fs::write(&path, "external\n").unwrap();

        let response = session
            .handle_editor_message(EditorMessage::SaveRequested {
                document_id: document.id,
                document_version: 2,
                change_sequence: 1,
                content: "editor\n".into(),
            })
            .unwrap();

        assert!(matches!(
            response.as_slice(),
            [HostMessage::SaveFailed { reason, .. }] if reason.contains("changed on disk")
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), "external\n");
    }

    #[test]
    fn dirty_close_is_refused_until_the_document_is_saved() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("dirty.txt");
        fs::write(&path, "base\n").unwrap();
        let mut session = EditorSession::new(workspace.path()).unwrap();
        let document = open_payload(session.open_document(&path).unwrap());
        session
            .handle_editor_message(EditorMessage::DocumentChanged {
                document_id: document.id.clone(),
                document_version: 1,
                change_sequence: 1,
                content: "dirty\n".into(),
            })
            .unwrap();

        let error = session
            .handle_editor_message(EditorMessage::CloseRequested {
                document_id: document.id.clone(),
                document_version: 2,
                dirty: false,
            })
            .unwrap_err();
        assert!(matches!(
            error,
            EditorSessionError::Document(DocumentError::Dirty(_))
        ));

        let messages = session
            .handle_editor_message(EditorMessage::DiscardCloseRequested {
                document_id: document.id,
                document_version: 2,
            })
            .unwrap();
        assert!(matches!(
            messages.as_slice(),
            [HostMessage::CloseDocument { .. }]
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), "base\n");
    }

    #[test]
    fn clean_external_change_is_reloaded_once() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("자동-새로고침.txt");
        fs::write(&path, "처음\n").unwrap();
        let mut session = EditorSession::new(workspace.path()).unwrap();
        let document = open_payload(session.open_document(&path).unwrap());
        fs::write(&path, "외부 변경 日本語 🙂\n").unwrap();

        let messages = session.poll_external_changes().unwrap();
        assert!(matches!(
            messages.as_slice(),
            [HostMessage::ReplaceDocument { document: replacement }]
                if replacement.id == document.id
                    && replacement.content == "외부 변경 日本語 🙂\n"
                    && replacement.version == document.version + 1
                    && !replacement.dirty
                    && !replacement.external_change
        ));
        assert!(session.poll_external_changes().unwrap().is_empty());
    }

    #[test]
    fn dirty_external_change_reports_conflict_without_reloading() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("충돌.txt");
        fs::write(&path, "base\n").unwrap();
        let mut session = EditorSession::new(workspace.path()).unwrap();
        let document = open_payload(session.open_document(&path).unwrap());
        session
            .handle_editor_message(EditorMessage::DocumentChanged {
                document_id: document.id.clone(),
                document_version: document.version,
                change_sequence: 1,
                content: "편집 중\n".into(),
            })
            .unwrap();
        fs::write(&path, "외부 변경\n").unwrap();

        let messages = session.poll_external_changes().unwrap();
        assert!(matches!(
            messages.as_slice(),
            [HostMessage::DocumentDiskStatus {
                document_id,
                document_version: 2,
                status: DocumentDiskStatus::Modified,
            }] if document_id == &document.id
        ));
        assert!(session.poll_external_changes().unwrap().is_empty());
        let initialized = session.initialize_message("workspace".into());
        assert!(matches!(
            initialized,
            HostMessage::InitializeEditor { documents, .. }
                if documents[0].content == "편집 중\n" && documents[0].dirty
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), "외부 변경\n");
    }

    #[test]
    fn deleted_document_is_reported_once() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("삭제.txt");
        fs::write(&path, "내용\n").unwrap();
        let mut session = EditorSession::new(workspace.path()).unwrap();
        let document = open_payload(session.open_document(&path).unwrap());
        fs::remove_file(path).unwrap();

        let messages = session.poll_external_changes().unwrap();
        assert!(matches!(
            messages.as_slice(),
            [HostMessage::DocumentDiskStatus {
                document_id,
                status: DocumentDiskStatus::Deleted,
                ..
            }] if document_id == &document.id
        ));
        assert!(session.poll_external_changes().unwrap().is_empty());
    }

    #[test]
    fn invalid_external_content_reports_conflict_and_retries_after_next_change() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("invalid-then-valid.txt");
        fs::write(&path, "base\n").unwrap();
        let mut session = EditorSession::new(workspace.path()).unwrap();
        let document = open_payload(session.open_document(&path).unwrap());
        fs::write(&path, [0xff, 0xfe]).unwrap();

        assert!(matches!(
            session.poll_external_changes().unwrap().as_slice(),
            [HostMessage::DocumentDiskStatus {
                status: DocumentDiskStatus::Modified,
                ..
            }]
        ));

        fs::write(&path, "복구됨 日本語 🙂\n").unwrap();
        assert!(matches!(
            session.poll_external_changes().unwrap().as_slice(),
            [HostMessage::ReplaceDocument { document: replacement }]
                if replacement.id == document.id && replacement.content == "복구됨 日本語 🙂\n"
        ));
    }

    #[test]
    fn dirty_multilingual_document_is_offered_and_restored_after_restart() {
        let workspace = tempdir().unwrap();
        let state = tempdir().unwrap();
        let path = workspace.path().join("복구-日本語-🙂.txt");
        fs::write(&path, "원본\n").unwrap();
        let store = RecoveryStore::new(state.path(), workspace.path()).unwrap();
        let mut first =
            EditorSession::with_recovery_store(workspace.path(), store.clone()).unwrap();
        let document = open_payload(first.open_document(&path).unwrap());
        first
            .handle_editor_message(EditorMessage::DocumentChanged {
                document_id: document.id,
                document_version: document.version,
                change_sequence: 1,
                content: "복구됨\n日本語 🙂\n".into(),
            })
            .unwrap();
        apply_recovery_operations(&mut first, &store);

        let mut restarted =
            EditorSession::with_recovery_store(workspace.path(), store.clone()).unwrap();
        let messages = restarted.open_document(&path).unwrap();
        let [HostMessage::OpenDocument { document }, HostMessage::RecoveryAvailable {
            document_id,
            document_version,
            disk_state,
        }] = messages.as_slice()
        else {
            panic!("expected the document and its recovery proposal");
        };
        assert_eq!(document.content, "원본\n");
        assert_eq!(*disk_state, RecoveryDiskState::Unchanged);

        let restored = restarted
            .handle_editor_message(EditorMessage::RecoveryDecision {
                document_id: document_id.clone(),
                document_version: *document_version,
                choice: RecoveryChoice::Restore,
            })
            .unwrap();
        assert!(matches!(
            restored.as_slice(),
            [HostMessage::ReplaceDocument { document }]
                if document.content == "복구됨\n日本語 🙂\n" && document.dirty
        ));
        let saved = restarted
            .handle_editor_message(EditorMessage::SaveRequested {
                document_id: document_id.clone(),
                document_version: document_version + 1,
                change_sequence: 0,
                content: "복구됨\n日本語 🙂\n".into(),
            })
            .unwrap();
        assert!(matches!(
            saved.as_slice(),
            [HostMessage::SaveCompleted { .. }]
        ));
        apply_recovery_operations(&mut restarted, &store);
        assert_eq!(fs::read_to_string(&path).unwrap(), "복구됨\n日本語 🙂\n");
        assert!(store
            .read(fs::canonicalize(&path).unwrap())
            .unwrap()
            .is_none());
    }

    #[test]
    fn changed_original_blocks_saving_restored_recovery() {
        let workspace = tempdir().unwrap();
        let state = tempdir().unwrap();
        let path = workspace.path().join("충돌.txt");
        fs::write(&path, "원본\n").unwrap();
        let store = RecoveryStore::new(state.path(), workspace.path()).unwrap();
        let mut first =
            EditorSession::with_recovery_store(workspace.path(), store.clone()).unwrap();
        let document = open_payload(first.open_document(&path).unwrap());
        first
            .handle_editor_message(EditorMessage::DocumentChanged {
                document_id: document.id,
                document_version: document.version,
                change_sequence: 1,
                content: "편집 복구본\n".into(),
            })
            .unwrap();
        apply_recovery_operations(&mut first, &store);
        fs::write(&path, "외부 변경\n").unwrap();

        let mut restarted =
            EditorSession::with_recovery_store(workspace.path(), store.clone()).unwrap();
        let messages = restarted.open_document(&path).unwrap();
        let HostMessage::RecoveryAvailable {
            document_id,
            document_version,
            disk_state,
            ..
        } = &messages[1]
        else {
            panic!("expected a recovery conflict");
        };
        assert_eq!(*disk_state, RecoveryDiskState::Changed);
        let restored = restarted
            .handle_editor_message(EditorMessage::RecoveryDecision {
                document_id: document_id.clone(),
                document_version: *document_version,
                choice: RecoveryChoice::Restore,
            })
            .unwrap();
        assert!(matches!(
            restored.as_slice(),
            [HostMessage::ReplaceDocument { document }]
                if document.content == "편집 복구본\n" && document.external_change
        ));
        let response = restarted
            .handle_editor_message(EditorMessage::SaveRequested {
                document_id: document_id.clone(),
                document_version: document_version + 1,
                change_sequence: 0,
                content: "편집 복구본\n".into(),
            })
            .unwrap();
        assert!(matches!(
            response.as_slice(),
            [HostMessage::SaveFailed { .. }]
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), "외부 변경\n");
    }

    #[test]
    fn save_all_dirty_refuses_restored_recovery_with_changed_base() {
        let workspace = tempdir().unwrap();
        let state = tempdir().unwrap();
        let path = workspace.path().join("복구-충돌.txt");
        fs::write(&path, "원본\n").unwrap();
        let store = RecoveryStore::new(state.path(), workspace.path()).unwrap();
        let mut first =
            EditorSession::with_recovery_store(workspace.path(), store.clone()).unwrap();
        let document = open_payload(first.open_document(&path).unwrap());
        first
            .handle_editor_message(EditorMessage::DocumentChanged {
                document_id: document.id,
                document_version: document.version,
                change_sequence: 1,
                content: "크래시 편집본\n".into(),
            })
            .unwrap();
        apply_recovery_operations(&mut first, &store);
        fs::write(&path, "외부 변경\n").unwrap();

        let mut restarted =
            EditorSession::with_recovery_store(workspace.path(), store.clone()).unwrap();
        let messages = restarted.open_document(&path).unwrap();
        let HostMessage::RecoveryAvailable {
            document_id,
            document_version,
            ..
        } = &messages[1]
        else {
            panic!("expected a recovery conflict");
        };
        restarted
            .handle_editor_message(EditorMessage::RecoveryDecision {
                document_id: document_id.clone(),
                document_version: *document_version,
                choice: RecoveryChoice::Restore,
            })
            .unwrap();

        let (_, result) = restarted.save_all_dirty();
        assert!(matches!(
            result,
            Err(EditorSessionError::Document(
                DocumentError::ExternalChange { .. }
            ))
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "외부 변경\n");
    }

    #[test]
    fn closing_with_undecided_recovery_keeps_the_snapshot() {
        let workspace = tempdir().unwrap();
        let state = tempdir().unwrap();
        let path = workspace.path().join("미결정.txt");
        fs::write(&path, "disk\n").unwrap();
        let identity_path = fs::canonicalize(&path).unwrap();
        let store = RecoveryStore::new(state.path(), workspace.path()).unwrap();
        let recovery = RecoverySnapshot::new(
            store.workspace_id().to_string(),
            identity_path.clone(),
            b"disk\n",
            2,
            "unsaved crash edits\n".into(),
            TextEncoding::Utf8,
            LineEnding::Lf,
        );
        store.write(&recovery).unwrap();
        let mut session =
            EditorSession::with_recovery_store(workspace.path(), store.clone()).unwrap();
        let messages = session.open_document(&path).unwrap();
        let HostMessage::RecoveryAvailable {
            document_id,
            document_version,
            ..
        } = &messages[1]
        else {
            panic!("expected a recovery proposal");
        };

        session
            .handle_editor_message(EditorMessage::CloseRequested {
                document_id: document_id.clone(),
                document_version: *document_version,
                dirty: false,
            })
            .unwrap();
        apply_recovery_operations(&mut session, &store);

        assert!(store.read(&identity_path).unwrap().is_some());
    }

    #[test]
    fn failed_save_keeps_the_reported_version_valid() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("실패-후-동기화.txt");
        fs::write(&path, "base\n").unwrap();
        let mut session = EditorSession::new(workspace.path()).unwrap();
        let document = open_payload(session.open_document(&path).unwrap());
        fs::write(&path, "external\n").unwrap();

        // The save carries content the host has not seen yet; a failure must
        // not bump the version, or the WebView could never talk again.
        let response = session
            .handle_editor_message(EditorMessage::SaveRequested {
                document_id: document.id.clone(),
                document_version: document.version,
                change_sequence: 1,
                content: "unsent edits\n".into(),
            })
            .unwrap();
        assert!(matches!(
            response.as_slice(),
            [HostMessage::SaveFailed { .. }]
        ));
        session
            .handle_editor_message(EditorMessage::ViewStateChanged {
                document_id: document.id,
                document_version: document.version,
                cursor_line: 1,
                cursor_column: 1,
                scroll_top: 0.0,
            })
            .unwrap();
    }

    #[test]
    fn discard_all_dirty_reverts_documents_but_keeps_them_open() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("되돌리기.txt");
        fs::write(&path, "disk\n").unwrap();
        let mut session = EditorSession::new(workspace.path()).unwrap();
        let document = open_payload(session.open_document(&path).unwrap());
        session
            .handle_editor_message(EditorMessage::DocumentChanged {
                document_id: document.id,
                document_version: document.version,
                change_sequence: 1,
                content: "버릴 편집\n".into(),
            })
            .unwrap();

        session.discard_all_dirty();

        assert!(session.dirty_document_paths().is_empty());
        assert!(session.contains_document(&path));
        assert_eq!(
            session.session_snapshot().open_files.len(),
            1,
            "discarding changes must not forget the open tab"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "disk\n");
    }

    #[test]
    fn discarding_recovery_keeps_disk_and_removes_snapshot() {
        let workspace = tempdir().unwrap();
        let state = tempdir().unwrap();
        let path = workspace.path().join("discard.txt");
        fs::write(&path, "disk\n").unwrap();
        let store = RecoveryStore::new(state.path(), workspace.path()).unwrap();
        let recovery = RecoverySnapshot::new(
            store.workspace_id().to_string(),
            fs::canonicalize(&path).unwrap(),
            b"disk\n",
            2,
            "unsaved\n".into(),
            TextEncoding::Utf8,
            LineEnding::Lf,
        );
        store.write(&recovery).unwrap();
        let mut session =
            EditorSession::with_recovery_store(workspace.path(), store.clone()).unwrap();
        let messages = session.open_document(&path).unwrap();
        let HostMessage::RecoveryAvailable {
            document_id,
            document_version,
            ..
        } = &messages[1]
        else {
            panic!("expected a recovery proposal");
        };

        assert!(session
            .handle_editor_message(EditorMessage::RecoveryDecision {
                document_id: document_id.clone(),
                document_version: *document_version,
                choice: RecoveryChoice::Discard,
            })
            .unwrap()
            .is_empty());
        apply_recovery_operations(&mut session, &store);
        assert_eq!(fs::read_to_string(&path).unwrap(), "disk\n");
        assert!(store
            .read(fs::canonicalize(path).unwrap())
            .unwrap()
            .is_none());
    }
}
