// SPDX-License-Identifier: GPL-3.0-or-later
//! Versioned messages exchanged with the editor WebView.

use crate::DEFAULT_MAX_DOCUMENT_BYTES;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_BRIDGE_MESSAGE_BYTES: usize = DEFAULT_MAX_DOCUMENT_BYTES as usize + 64 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextDocumentEncoding {
    #[serde(rename = "UTF-8")]
    Utf8,
    #[serde(rename = "UTF-8 BOM")]
    Utf8Bom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextDocumentLineEnding {
    #[serde(rename = "LF")]
    Lf,
    #[serde(rename = "CRLF")]
    CrLf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentPayload {
    pub id: String,
    pub uri: String,
    pub relative_path: String,
    pub name: String,
    pub content: String,
    pub version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub encoding: TextDocumentEncoding,
    pub eol: TextDocumentLineEnding,
    pub dirty: bool,
    pub read_only: bool,
    pub external_change: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum HostMessage {
    InitializeEditor {
        workspace_name: String,
        documents: Vec<DocumentPayload>,
        active_document_id: Option<String>,
    },
    OpenDocument {
        document: DocumentPayload,
    },
    ReplaceDocument {
        document: DocumentPayload,
    },
    CloseDocument {
        document_id: String,
        document_version: u64,
    },
    SetActiveDocument {
        document_id: String,
        document_version: u64,
    },
    SaveCompleted {
        document_id: String,
        document_version: u64,
        change_sequence: u64,
    },
    SaveFailed {
        document_id: String,
        document_version: u64,
        change_sequence: u64,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum EditorMessage {
    EditorReady,
    ActiveDocumentChanged {
        document_id: String,
        document_version: u64,
    },
    DocumentChanged {
        document_id: String,
        document_version: u64,
        change_sequence: u64,
        content: String,
    },
    SaveRequested {
        document_id: String,
        document_version: u64,
        change_sequence: u64,
        content: String,
    },
    CloseRequested {
        document_id: String,
        document_version: u64,
        dirty: bool,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HostEnvelope<'a> {
    protocol_version: u16,
    surface_id: &'a str,
    #[serde(flatten)]
    message: &'a HostMessage,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditorEnvelope {
    protocol_version: u16,
    surface_id: String,
    #[serde(flatten)]
    message: EditorMessage,
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("editor bridge message exceeds the {limit} byte limit")]
    MessageTooLarge { limit: usize },
    #[error("invalid editor bridge JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("unsupported editor protocol version: {actual}")]
    UnsupportedVersion { actual: u16 },
    #[error("invalid {field} in editor bridge message")]
    InvalidIdentifier { field: &'static str },
    #[error("editor document content exceeds the {limit} byte limit")]
    DocumentTooLarge { limit: usize },
}

pub fn parse_editor_message(input: &str) -> Result<(String, EditorMessage), ProtocolError> {
    if input.len() > MAX_BRIDGE_MESSAGE_BYTES {
        return Err(ProtocolError::MessageTooLarge {
            limit: MAX_BRIDGE_MESSAGE_BYTES,
        });
    }
    let envelope: EditorEnvelope = serde_json::from_str(input)?;
    if envelope.protocol_version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion {
            actual: envelope.protocol_version,
        });
    }
    validate_identifier("surface ID", &envelope.surface_id)?;
    validate_editor_message(&envelope.message)?;
    Ok((envelope.surface_id, envelope.message))
}

pub fn serialize_host_message(
    surface_id: &str,
    message: &HostMessage,
) -> Result<String, ProtocolError> {
    validate_identifier("surface ID", surface_id)?;
    validate_host_message(message)?;
    let encoded = serde_json::to_string(&HostEnvelope {
        protocol_version: PROTOCOL_VERSION,
        surface_id,
        message,
    })?;
    if encoded.len() > MAX_BRIDGE_MESSAGE_BYTES {
        return Err(ProtocolError::MessageTooLarge {
            limit: MAX_BRIDGE_MESSAGE_BYTES,
        });
    }
    Ok(encoded)
}

fn validate_editor_message(message: &EditorMessage) -> Result<(), ProtocolError> {
    match message {
        EditorMessage::EditorReady => Ok(()),
        EditorMessage::ActiveDocumentChanged { document_id, .. }
        | EditorMessage::CloseRequested { document_id, .. } => {
            validate_identifier("document ID", document_id)
        }
        EditorMessage::DocumentChanged {
            document_id,
            content,
            ..
        }
        | EditorMessage::SaveRequested {
            document_id,
            content,
            ..
        } => {
            validate_identifier("document ID", document_id)?;
            validate_document_size(content)
        }
    }
}

fn validate_host_message(message: &HostMessage) -> Result<(), ProtocolError> {
    match message {
        HostMessage::InitializeEditor {
            documents,
            active_document_id,
            ..
        } => {
            for document in documents {
                validate_document(document)?;
            }
            if let Some(document_id) = active_document_id {
                validate_identifier("active document ID", document_id)?;
            }
            Ok(())
        }
        HostMessage::OpenDocument { document } | HostMessage::ReplaceDocument { document } => {
            validate_document(document)
        }
        HostMessage::CloseDocument { document_id, .. }
        | HostMessage::SetActiveDocument { document_id, .. }
        | HostMessage::SaveCompleted { document_id, .. }
        | HostMessage::SaveFailed { document_id, .. } => {
            validate_identifier("document ID", document_id)
        }
    }
}

fn validate_document(document: &DocumentPayload) -> Result<(), ProtocolError> {
    validate_identifier("document ID", &document.id)?;
    if document.uri.is_empty() {
        return Err(ProtocolError::InvalidIdentifier { field: "URI" });
    }
    validate_document_size(&document.content)
}

fn validate_document_size(content: &str) -> Result<(), ProtocolError> {
    if content.len() > DEFAULT_MAX_DOCUMENT_BYTES as usize {
        return Err(ProtocolError::DocumentTooLarge {
            limit: DEFAULT_MAX_DOCUMENT_BYTES as usize,
        });
    }
    Ok(())
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ProtocolError::InvalidIdentifier { field });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn multilingual_document() -> DocumentPayload {
        DocumentPayload {
            id: "document-1".into(),
            uri: "file:///workspace/%EB%AC%B8%EC%84%9C/%EC%9D%B8%EC%82%AC%EB%A7%90.rs".into(),
            relative_path: "문서/인사말.rs".into(),
            name: "인사말.rs".into(),
            content: "fn main() { println!(\"안녕하세요 日本語 العربية 🌏\"); }\n".into(),
            version: 7,
            language: Some("rust".into()),
            encoding: TextDocumentEncoding::Utf8,
            eol: TextDocumentLineEnding::Lf,
            dirty: true,
            read_only: false,
            external_change: false,
        }
    }

    #[test]
    fn host_message_matches_web_protocol_shape() {
        let encoded = serialize_host_message(
            "surface-1",
            &HostMessage::OpenDocument {
                document: multilingual_document(),
            },
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();

        assert_eq!(value["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(value["surfaceId"], "surface-1");
        assert_eq!(value["type"], "open_document");
        assert_eq!(value["document"]["relativePath"], "문서/인사말.rs");
        assert_eq!(
            value["document"]["content"],
            multilingual_document().content
        );
    }

    #[test]
    fn editor_message_round_trips_multilingual_content() {
        let input = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "surfaceId": "surface-1",
            "type": "document_changed",
            "documentId": "document-1",
            "documentVersion": 7,
            "changeSequence": 2,
            "content": "한글 日本語 العربية 🙂\n"
        })
        .to_string();

        let (surface_id, message) = parse_editor_message(&input).unwrap();
        assert_eq!(surface_id, "surface-1");
        assert_eq!(
            message,
            EditorMessage::DocumentChanged {
                document_id: "document-1".into(),
                document_version: 7,
                change_sequence: 2,
                content: "한글 日本語 العربية 🙂\n".into(),
            }
        );
    }

    #[test]
    fn rejects_version_mismatch_and_unknown_messages() {
        let mismatch = r#"{"protocolVersion":2,"surfaceId":"surface-1","type":"editor_ready"}"#;
        assert!(matches!(
            parse_editor_message(mismatch),
            Err(ProtocolError::UnsupportedVersion { actual: 2 })
        ));

        let unknown = r#"{"protocolVersion":1,"surfaceId":"surface-1","type":"run_extension"}"#;
        assert!(matches!(
            parse_editor_message(unknown),
            Err(ProtocolError::InvalidJson(_))
        ));
    }

    #[test]
    fn rejects_unsafe_identifiers_and_oversized_documents() {
        let unsafe_id = r#"{"protocolVersion":1,"surfaceId":"../surface","type":"editor_ready"}"#;
        assert!(matches!(
            parse_editor_message(unsafe_id),
            Err(ProtocolError::InvalidIdentifier {
                field: "surface ID"
            })
        ));

        let mut document = multilingual_document();
        document.content = "x".repeat(DEFAULT_MAX_DOCUMENT_BYTES as usize + 1);
        assert!(matches!(
            serialize_host_message("surface-1", &HostMessage::OpenDocument { document }),
            Err(ProtocolError::DocumentTooLarge { .. })
        ));
    }
}
