// SPDX-License-Identifier: GPL-3.0-or-later
//! Headless document session behind one editor surface.

use crate::{
    DiskStatus, DocumentDiskStatus, DocumentError, DocumentId, DocumentPayload, DocumentService,
    DocumentSnapshot, EditorMessage, HostMessage, LineEnding, RecoveryChoice, RecoveryDiskState,
    RecoveryOperation, RecoverySnapshot, RecoveryStore, TextDocumentEncoding,
    TextDocumentLineEnding, TextEncoding,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

/// Runtime state for the documents displayed by one editor WebView.
pub struct EditorSession {
    documents: DocumentService,
    protocol_ids: HashMap<String, DocumentId>,
    open_order: Vec<DocumentId>,
    active: Option<DocumentId>,
    reported_disk_status: HashMap<DocumentId, DiskStatus>,
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
            if self.documents.discard(id).is_err() {
                continue;
            }
            self.protocol_ids.retain(|_, mapped| *mapped != id);
            self.open_order.retain(|candidate| *candidate != id);
            self.reported_disk_status.remove(&id);
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
        let opened = self.documents.open(path)?;
        let id = opened.document.id;
        let document_id = protocol_id(id);
        self.protocol_ids.insert(document_id.clone(), id);
        if !self.open_order.contains(&id) {
            self.open_order.push(id);
        }
        if !opened.already_open {
            self.reported_disk_status.insert(id, DiskStatus::Unchanged);
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
            let snapshot = self.documents.snapshot(id)?;

            if status == DiskStatus::Modified && !snapshot.is_dirty() {
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
                    document_version: snapshot.version,
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
        self.queue_recovery_removal(id, &identity_path);
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
            Err(error) => HostMessage::SaveFailed {
                document_id,
                document_version,
                change_sequence,
                reason: error.to_string(),
            },
        }
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
        self.recovery_operations
            .push(RecoveryOperation::Write(recovery));
        Ok(())
    }

    fn queue_recovery_removal(&mut self, id: DocumentId, identity_path: &Path) {
        self.pending_recoveries.remove(&id);
        self.recovery_base_hashes.remove(&id);
        if self.recovery_store.is_some() {
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
        }
    }
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
