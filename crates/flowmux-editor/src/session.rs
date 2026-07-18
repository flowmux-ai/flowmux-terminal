// SPDX-License-Identifier: GPL-3.0-or-later
//! Headless document session behind one editor surface.

use crate::{
    DiskStatus, DocumentError, DocumentId, DocumentPayload, DocumentService, DocumentSnapshot,
    EditorMessage, HostMessage, LineEnding, TextDocumentEncoding, TextDocumentLineEnding,
    TextEncoding,
};
use std::collections::HashMap;
use std::path::Path;
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
}

impl EditorSession {
    pub fn new(workspace_root: impl AsRef<Path>) -> Result<Self, DocumentError> {
        Ok(Self {
            documents: DocumentService::new(workspace_root)?,
            protocol_ids: HashMap::new(),
            open_order: Vec::new(),
            active: None,
        })
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

    pub fn open_document(&mut self, path: impl AsRef<Path>) -> Result<HostMessage, DocumentError> {
        let opened = self.documents.open(path)?;
        let id = opened.document.id;
        let document_id = protocol_id(id);
        self.protocol_ids.insert(document_id.clone(), id);
        if !self.open_order.contains(&id) {
            self.open_order.push(id);
        }
        self.active = Some(id);

        if opened.already_open {
            Ok(HostMessage::SetActiveDocument {
                document_id,
                document_version: opened.document.version,
            })
        } else {
            Ok(HostMessage::OpenDocument {
                document: self.payload(opened.document),
            })
        }
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
            } => {
                let id = self.checked_document(&document_id, document_version)?;
                self.documents.close(id)?;
                self.protocol_ids.remove(&document_id);
                self.open_order.retain(|candidate| *candidate != id);
                if self.active == Some(id) {
                    self.active = self.open_order.last().copied();
                }
                Ok(vec![HostMessage::CloseDocument {
                    document_id,
                    document_version,
                }])
            }
        }
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
            let snapshot = self.documents.snapshot(id)?;
            let version = if snapshot.text == content {
                snapshot.version
            } else {
                self.documents
                    .update_text(id, document_version, content)?
                    .version
            };
            Ok(self.documents.save(id, version)?)
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
        let external_change = !matches!(
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn open_payload(message: HostMessage) -> DocumentPayload {
        let HostMessage::OpenDocument { document } = message else {
            panic!("expected a newly opened document");
        };
        document
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
            session.open_document(&path).unwrap(),
            HostMessage::SetActiveDocument { document_id, .. } if document_id == document.id
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
                document_id: document.id,
                document_version: 2,
                dirty: false,
            })
            .unwrap_err();
        assert!(matches!(
            error,
            EditorSessionError::Document(DocumentError::Dirty(_))
        ));
    }
}
