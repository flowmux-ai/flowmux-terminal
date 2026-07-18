// SPDX-License-Identifier: GPL-3.0-or-later
//! Versioned messages exchanged with the editor WebView.

use crate::{RecoveryDiskState, SearchOptions, WorkspaceSearchResult, DEFAULT_MAX_DOCUMENT_BYTES};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};
use thiserror::Error;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_BRIDGE_MESSAGE_BYTES: usize = DEFAULT_MAX_DOCUMENT_BYTES as usize + 64 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 128;
const MAX_SEARCH_QUERY_BYTES: usize = 4 * 1024;
const MAX_SEARCH_PATH_BYTES: usize = 16 * 1024;
const MAX_SEARCH_GLOBS: usize = 32;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentDiskStatus {
    Unchanged,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub cursor_line: u32,
    pub cursor_column: u32,
    pub scroll_top: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    DocumentDiskStatus {
        document_id: String,
        document_version: u64,
        status: DocumentDiskStatus,
    },
    RecoveryAvailable {
        document_id: String,
        document_version: u64,
        disk_state: RecoveryDiskState,
    },
    ShowWorkspaceSearch,
    QuickOpenCompleted {
        request_id: String,
        paths: Vec<String>,
        truncated: bool,
    },
    WorkspaceSearchCompleted {
        request_id: String,
        result: WorkspaceSearchResult,
        error: Option<String>,
    },
    RevealRange {
        document_id: String,
        document_version: u64,
        line: u32,
        column: u32,
        length: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryChoice {
    Restore,
    Discard,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    DiscardCloseRequested {
        document_id: String,
        document_version: u64,
    },
    RecoveryDecision {
        document_id: String,
        document_version: u64,
        choice: RecoveryChoice,
    },
    ViewStateChanged {
        document_id: String,
        document_version: u64,
        cursor_line: u32,
        cursor_column: u32,
        scroll_top: f64,
    },
    QuickOpenRequested {
        request_id: String,
    },
    WorkspaceSearchRequested {
        request_id: String,
        query: String,
        options: SearchOptions,
    },
    SearchCancelled {
        request_id: String,
    },
    SearchResultOpenRequested {
        path: String,
        line: u32,
        column: u32,
        length: u32,
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
    #[error("invalid editor document view state")]
    InvalidViewState,
    #[error("invalid editor workspace search request")]
    InvalidSearchRequest,
    #[error("invalid editor workspace search path")]
    InvalidSearchPath,
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

pub fn javascript_for_host_message(
    surface_id: &str,
    message: &HostMessage,
) -> Result<String, ProtocolError> {
    let message = serialize_host_message(surface_id, message)?;
    let quoted = serde_json::to_string(&message)?;
    Ok(format!(
        "window.flowmuxEditorHost.receive(JSON.parse({quoted}));"
    ))
}

fn validate_editor_message(message: &EditorMessage) -> Result<(), ProtocolError> {
    match message {
        EditorMessage::EditorReady => Ok(()),
        EditorMessage::ActiveDocumentChanged { document_id, .. }
        | EditorMessage::CloseRequested { document_id, .. }
        | EditorMessage::DiscardCloseRequested { document_id, .. }
        | EditorMessage::RecoveryDecision { document_id, .. } => {
            validate_identifier("document ID", document_id)
        }
        EditorMessage::ViewStateChanged {
            document_id,
            scroll_top,
            ..
        } => {
            validate_identifier("document ID", document_id)?;
            if !scroll_top.is_finite() || *scroll_top < 0.0 {
                return Err(ProtocolError::InvalidViewState);
            }
            Ok(())
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
        EditorMessage::QuickOpenRequested { request_id }
        | EditorMessage::SearchCancelled { request_id } => {
            validate_identifier("search request ID", request_id)
        }
        EditorMessage::WorkspaceSearchRequested {
            request_id,
            query,
            options,
        } => {
            validate_identifier("search request ID", request_id)?;
            if query.is_empty()
                || query.len() > MAX_SEARCH_QUERY_BYTES
                || options.include.len() > MAX_SEARCH_GLOBS
                || options.exclude.len() > MAX_SEARCH_GLOBS
                || options.max_results == 0
                || options.max_results > crate::DEFAULT_MAX_SEARCH_RESULTS
                || options.max_file_bytes == 0
                || options.max_file_bytes > crate::DEFAULT_MAX_SEARCH_FILE_BYTES
                || options
                    .include
                    .iter()
                    .chain(&options.exclude)
                    .any(|pattern| pattern.is_empty() || pattern.len() > MAX_SEARCH_QUERY_BYTES)
            {
                return Err(ProtocolError::InvalidSearchRequest);
            }
            Ok(())
        }
        EditorMessage::SearchResultOpenRequested { path, .. } => validate_search_path(path),
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
        | HostMessage::SaveFailed { document_id, .. }
        | HostMessage::DocumentDiskStatus { document_id, .. } => {
            validate_identifier("document ID", document_id)
        }
        HostMessage::RecoveryAvailable { document_id, .. }
        | HostMessage::RevealRange { document_id, .. } => {
            validate_identifier("document ID", document_id)
        }
        HostMessage::ShowWorkspaceSearch => Ok(()),
        HostMessage::QuickOpenCompleted {
            request_id, paths, ..
        } => {
            validate_identifier("search request ID", request_id)?;
            for path in paths {
                validate_search_path(path)?;
            }
            Ok(())
        }
        HostMessage::WorkspaceSearchCompleted {
            request_id, result, ..
        } => {
            validate_identifier("search request ID", request_id)?;
            for found in &result.matches {
                validate_search_path(&found.path)?;
            }
            Ok(())
        }
    }
}

fn validate_document(document: &DocumentPayload) -> Result<(), ProtocolError> {
    validate_identifier("document ID", &document.id)?;
    if document.uri.is_empty() {
        return Err(ProtocolError::InvalidIdentifier { field: "URI" });
    }
    if !document.scroll_top.is_finite() || document.scroll_top < 0.0 {
        return Err(ProtocolError::InvalidViewState);
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

fn validate_search_path(value: &str) -> Result<(), ProtocolError> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > MAX_SEARCH_PATH_BYTES
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ProtocolError::InvalidSearchPath);
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
            cursor_line: 12,
            cursor_column: 4,
            scroll_top: 96.5,
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
    fn discard_close_message_is_explicit_and_versioned() {
        let input = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "surfaceId": "surface-1",
            "type": "discard_close_requested",
            "documentId": "document-1",
            "documentVersion": 8
        })
        .to_string();

        assert_eq!(
            parse_editor_message(&input).unwrap(),
            (
                "surface-1".into(),
                EditorMessage::DiscardCloseRequested {
                    document_id: "document-1".into(),
                    document_version: 8,
                }
            )
        );
    }

    #[test]
    fn disk_status_message_uses_stable_lowercase_values() {
        let encoded = serialize_host_message(
            "surface-1",
            &HostMessage::DocumentDiskStatus {
                document_id: "document-1".into(),
                document_version: 3,
                status: DocumentDiskStatus::Deleted,
            },
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();

        assert_eq!(value["type"], "document_disk_status");
        assert_eq!(value["status"], "deleted");
    }

    #[test]
    fn recovery_messages_are_versioned_and_explicit() {
        let encoded = serialize_host_message(
            "surface-1",
            &HostMessage::RecoveryAvailable {
                document_id: "document-1".into(),
                document_version: 3,
                disk_state: RecoveryDiskState::Changed,
            },
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["type"], "recovery_available");
        assert_eq!(value["diskState"], "changed");

        let decision = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "surfaceId": "surface-1",
            "type": "recovery_decision",
            "documentId": "document-1",
            "documentVersion": 3,
            "choice": "restore"
        })
        .to_string();
        assert_eq!(
            parse_editor_message(&decision).unwrap().1,
            EditorMessage::RecoveryDecision {
                document_id: "document-1".into(),
                document_version: 3,
                choice: RecoveryChoice::Restore,
            }
        );
    }

    #[test]
    fn view_state_message_accepts_multilingual_document_and_rejects_negative_scroll() {
        let message = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "surfaceId": "surface-1",
            "type": "view_state_changed",
            "documentId": "document-1",
            "documentVersion": 3,
            "cursorLine": 14,
            "cursorColumn": 6,
            "scrollTop": 128.5
        })
        .to_string();
        assert_eq!(
            parse_editor_message(&message).unwrap().1,
            EditorMessage::ViewStateChanged {
                document_id: "document-1".into(),
                document_version: 3,
                cursor_line: 14,
                cursor_column: 6,
                scroll_top: 128.5,
            }
        );

        let invalid = message.replace("128.5", "-1");
        assert!(matches!(
            parse_editor_message(&invalid),
            Err(ProtocolError::InvalidViewState)
        ));
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

    #[test]
    fn host_javascript_keeps_script_like_and_line_separator_text_inside_json() {
        let mut document = multilingual_document();
        document.content = "</script>\u{2028}다음 줄\u{2029}".into();
        let script =
            javascript_for_host_message("surface-1", &HostMessage::OpenDocument { document })
                .unwrap();

        assert!(script.starts_with("window.flowmuxEditorHost.receive(JSON.parse("));
        assert!(script.contains("JSON.parse(\"{\\\"protocolVersion"));
        let quoted = script
            .strip_prefix("window.flowmuxEditorHost.receive(JSON.parse(")
            .unwrap()
            .strip_suffix("));")
            .unwrap();
        let json: String = serde_json::from_str(quoted).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["document"]["content"],
            "</script>\u{2028}다음 줄\u{2029}"
        );
    }

    #[test]
    fn workspace_search_request_is_bounded_and_versioned() {
        let request = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "surfaceId": "surface-1",
            "type": "workspace_search_requested",
            "requestId": "search-7",
            "query": "안녕|hello",
            "options": {
                "caseSensitive": false,
                "wholeWord": false,
                "useRegex": true,
                "include": ["src/**"],
                "exclude": ["target/**"],
                "maxResults": 100,
                "maxFileBytes": 1024
            }
        })
        .to_string();
        assert!(matches!(
            parse_editor_message(&request).unwrap().1,
            EditorMessage::WorkspaceSearchRequested { request_id, query, .. }
                if request_id == "search-7" && query == "안녕|hello"
        ));

        let invalid = request.replace("\"src/**\"", "\"../**\"");
        assert!(parse_editor_message(&invalid).is_ok());
        let invalid = request.replace("\"maxResults\":100", "\"maxResults\":501");
        assert!(matches!(
            parse_editor_message(&invalid),
            Err(ProtocolError::InvalidSearchRequest)
        ));
    }

    #[test]
    fn search_result_open_rejects_workspace_escape() {
        let valid = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "surfaceId": "surface-1",
            "type": "search_result_open_requested",
            "path": "src/문서.rs",
            "line": 4,
            "column": 2,
            "length": 3
        })
        .to_string();
        assert!(matches!(
            parse_editor_message(&valid).unwrap().1,
            EditorMessage::SearchResultOpenRequested { path, line: 4, .. }
                if path == "src/문서.rs"
        ));

        let invalid = valid.replace("src/문서.rs", "../secret.txt");
        assert!(matches!(
            parse_editor_message(&invalid),
            Err(ProtocolError::InvalidSearchPath)
        ));
    }
}
