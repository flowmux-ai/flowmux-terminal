// SPDX-License-Identifier: GPL-3.0-or-later
//! Safe document I/O for the Flowmux editor.
//!
//! UI code never reads or writes arbitrary paths directly. `DocumentService`
//! owns workspace boundaries, text decoding, version checks, external-change
//! detection, and atomic persistence.

mod protocol;
mod recovery;
mod session;
mod web_assets;

pub use protocol::{
    javascript_for_host_message, parse_editor_message, serialize_host_message, DocumentDiskStatus,
    DocumentPayload, EditorMessage, HostMessage, ProtocolError, RecoveryChoice,
    TextDocumentEncoding, TextDocumentLineEnding, MAX_BRIDGE_MESSAGE_BYTES, PROTOCOL_VERSION,
};
pub use recovery::{
    RecoveryDiskState, RecoveryError, RecoveryOperation, RecoverySnapshot, RecoveryStore,
    RECOVERY_FORMAT_VERSION,
};
pub use session::{EditorSession, EditorSessionError};
pub use web_assets::{EditorAssetServer, EditorAssetServerError};

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, Permissions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use thiserror::Error;

pub const DEFAULT_MAX_DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;
const UTF8_BOM: &[u8] = b"\xef\xbb\xbf";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DocumentId(u64);

impl DocumentId {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextEncoding {
    Utf8,
    Utf8Bom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineEnding {
    Lf,
    CrLf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentSnapshot {
    pub id: DocumentId,
    pub display_path: PathBuf,
    pub identity_path: PathBuf,
    pub text: String,
    pub version: u64,
    pub saved_version: u64,
    pub encoding: TextEncoding,
    pub line_ending: LineEnding,
    pub has_final_newline: bool,
    pub read_only: bool,
}

impl DocumentSnapshot {
    pub fn is_dirty(&self) -> bool {
        self.version != self.saved_version
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenDocument {
    pub document: DocumentSnapshot,
    pub already_open: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskStatus {
    Unchanged,
    Modified,
    Deleted,
}

#[derive(Debug, Error)]
pub enum DocumentError {
    #[error("failed to {operation} {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("path is outside the editor workspace: {path}")]
    OutsideWorkspace { path: PathBuf },
    #[error("path is not a regular file: {path}")]
    NotAFile { path: PathBuf },
    #[error("document exceeds the {limit} byte limit: {path}")]
    TooLarge { path: PathBuf, limit: u64 },
    #[error("document appears to be binary: {path}")]
    Binary { path: PathBuf },
    #[error("document is not valid UTF-8: {path}")]
    InvalidUtf8 { path: PathBuf },
    #[error("document mixes LF and CRLF line endings: {path}")]
    MixedLineEndings { path: PathBuf },
    #[error("document is not open: {0:?}")]
    NotOpen(DocumentId),
    #[error("stale document version for {id:?}: expected {expected}, got {actual}")]
    StaleVersion {
        id: DocumentId,
        expected: u64,
        actual: u64,
    },
    #[error("document has unsaved changes: {0:?}")]
    Dirty(DocumentId),
    #[error("document changed on disk: {path}")]
    ExternalChange { path: PathBuf },
    #[error("document is read-only: {path}")]
    ReadOnly { path: PathBuf },
    #[error("save target already exists: {path}")]
    TargetExists { path: PathBuf },
    #[error("save target is already open in another document: {path}")]
    TargetAlreadyOpen { path: PathBuf },
}

struct Document {
    snapshot: DocumentSnapshot,
    base_bytes: Vec<u8>,
    permissions: Permissions,
}

struct LoadedDocument {
    text: String,
    encoding: TextEncoding,
    line_ending: LineEnding,
    bytes: Vec<u8>,
    permissions: Permissions,
    read_only: bool,
}

pub struct DocumentService {
    workspace_root: PathBuf,
    max_document_bytes: u64,
    next_id: u64,
    documents: HashMap<DocumentId, Document>,
    identities: HashMap<PathBuf, DocumentId>,
}

impl DocumentService {
    pub fn new(workspace_root: impl AsRef<Path>) -> Result<Self, DocumentError> {
        Self::with_max_document_bytes(workspace_root, DEFAULT_MAX_DOCUMENT_BYTES)
    }

    pub fn with_max_document_bytes(
        workspace_root: impl AsRef<Path>,
        max_document_bytes: u64,
    ) -> Result<Self, DocumentError> {
        let requested = workspace_root.as_ref();
        let workspace_root = fs::canonicalize(requested)
            .map_err(|source| io_error("resolve workspace", requested, source))?;
        Ok(Self {
            workspace_root,
            max_document_bytes,
            next_id: 1,
            documents: HashMap::new(),
            identities: HashMap::new(),
        })
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn contains_path(&self, path: impl AsRef<Path>) -> bool {
        let Ok(display_path) = absolute_path(path.as_ref()) else {
            return false;
        };
        let Ok(identity_path) = fs::canonicalize(display_path) else {
            return false;
        };
        self.identities.contains_key(&identity_path)
    }

    pub fn open(&mut self, path: impl AsRef<Path>) -> Result<OpenDocument, DocumentError> {
        let display_path = absolute_path(path.as_ref())?;
        let identity_path = fs::canonicalize(&display_path)
            .map_err(|source| io_error("resolve document", &display_path, source))?;
        self.ensure_within_workspace(&identity_path)?;

        if let Some(id) = self.identities.get(&identity_path).copied() {
            return Ok(OpenDocument {
                document: self.snapshot(id)?,
                already_open: true,
            });
        }

        let loaded = read_text_document(&identity_path, self.max_document_bytes)?;
        let id = DocumentId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        let snapshot = DocumentSnapshot {
            id,
            display_path,
            identity_path: identity_path.clone(),
            has_final_newline: loaded.text.ends_with('\n'),
            text: loaded.text,
            version: 1,
            saved_version: 1,
            encoding: loaded.encoding,
            line_ending: loaded.line_ending,
            read_only: loaded.read_only,
        };
        self.documents.insert(
            id,
            Document {
                snapshot: snapshot.clone(),
                base_bytes: loaded.bytes,
                permissions: loaded.permissions,
            },
        );
        self.identities.insert(identity_path, id);
        Ok(OpenDocument {
            document: snapshot,
            already_open: false,
        })
    }

    pub fn snapshot(&self, id: DocumentId) -> Result<DocumentSnapshot, DocumentError> {
        self.documents
            .get(&id)
            .map(|document| document.snapshot.clone())
            .ok_or(DocumentError::NotOpen(id))
    }

    fn recovery_snapshot(
        &self,
        id: DocumentId,
        store: &RecoveryStore,
    ) -> Result<RecoverySnapshot, DocumentError> {
        let document = self.documents.get(&id).ok_or(DocumentError::NotOpen(id))?;
        Ok(RecoverySnapshot::new(
            store.workspace_id().to_string(),
            document.snapshot.identity_path.clone(),
            &document.base_bytes,
            document.snapshot.version,
            document.snapshot.text.clone(),
            document.snapshot.encoding,
            document.snapshot.line_ending,
        ))
    }

    pub fn update_text(
        &mut self,
        id: DocumentId,
        base_version: u64,
        text: String,
    ) -> Result<DocumentSnapshot, DocumentError> {
        let document = self
            .documents
            .get_mut(&id)
            .ok_or(DocumentError::NotOpen(id))?;
        require_version(&document.snapshot, base_version)?;
        document.snapshot.version = document.snapshot.version.saturating_add(1);
        document.snapshot.has_final_newline = text.ends_with('\n');
        document.snapshot.text = text;
        Ok(document.snapshot.clone())
    }

    pub fn save(
        &mut self,
        id: DocumentId,
        requested_version: u64,
    ) -> Result<DocumentSnapshot, DocumentError> {
        let document = self.documents.get(&id).ok_or(DocumentError::NotOpen(id))?;
        require_version(&document.snapshot, requested_version)?;
        if document.snapshot.read_only {
            return Err(DocumentError::ReadOnly {
                path: document.snapshot.display_path.clone(),
            });
        }

        let current = fs::read(&document.snapshot.identity_path).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                DocumentError::ExternalChange {
                    path: document.snapshot.display_path.clone(),
                }
            } else {
                io_error("read before save", &document.snapshot.identity_path, source)
            }
        })?;
        if current != document.base_bytes {
            return Err(DocumentError::ExternalChange {
                path: document.snapshot.display_path.clone(),
            });
        }

        let bytes = encode_document(&document.snapshot);
        atomic_write(
            &document.snapshot.identity_path,
            &bytes,
            &document.permissions,
        )?;
        let document = self
            .documents
            .get_mut(&id)
            .expect("document remains open during synchronous save");
        document.base_bytes = bytes;
        document.snapshot.saved_version = requested_version;
        document.snapshot.read_only = fs::metadata(&document.snapshot.identity_path)
            .map(|metadata| metadata.permissions().readonly())
            .unwrap_or(false);
        Ok(document.snapshot.clone())
    }

    pub fn save_as(
        &mut self,
        id: DocumentId,
        requested_version: u64,
        target: impl AsRef<Path>,
        overwrite: bool,
    ) -> Result<DocumentSnapshot, DocumentError> {
        let target = absolute_path(target.as_ref())?;
        let parent = target
            .parent()
            .ok_or_else(|| DocumentError::OutsideWorkspace {
                path: target.clone(),
            })?;
        let canonical_parent = fs::canonicalize(parent)
            .map_err(|source| io_error("resolve save directory", parent, source))?;
        self.ensure_within_workspace(&canonical_parent)?;
        let file_name = target.file_name().ok_or_else(|| DocumentError::NotAFile {
            path: target.clone(),
        })?;
        let existing_metadata = fs::symlink_metadata(&target).ok();
        let identity_path = if existing_metadata.is_some() {
            let resolved = fs::canonicalize(&target)
                .map_err(|source| io_error("resolve save target", &target, source))?;
            self.ensure_within_workspace(&resolved)?;
            if !resolved.is_file() {
                return Err(DocumentError::NotAFile {
                    path: target.clone(),
                });
            }
            resolved
        } else {
            canonical_parent.join(file_name)
        };

        if existing_metadata.is_some() && !overwrite {
            return Err(DocumentError::TargetExists {
                path: target.clone(),
            });
        }
        if self
            .identities
            .get(&identity_path)
            .is_some_and(|open_id| *open_id != id)
        {
            return Err(DocumentError::TargetAlreadyOpen {
                path: target.clone(),
            });
        }

        let document = self.documents.get(&id).ok_or(DocumentError::NotOpen(id))?;
        require_version(&document.snapshot, requested_version)?;
        let bytes = encode_document(&document.snapshot);
        let permissions = if existing_metadata.is_some() {
            fs::metadata(&identity_path)
                .map_err(|source| io_error("inspect save target", &target, source))?
                .permissions()
        } else {
            document.permissions.clone()
        };
        let old_identity = document.snapshot.identity_path.clone();
        atomic_write(&identity_path, &bytes, &permissions)?;

        self.identities.remove(&old_identity);
        self.identities.insert(identity_path.clone(), id);
        let document = self
            .documents
            .get_mut(&id)
            .expect("document remains open during synchronous save as");
        document.snapshot.display_path = target;
        document.snapshot.identity_path = identity_path;
        document.snapshot.saved_version = requested_version;
        document.snapshot.read_only = false;
        document.base_bytes = bytes;
        Ok(document.snapshot.clone())
    }

    pub fn disk_status(&self, id: DocumentId) -> Result<DiskStatus, DocumentError> {
        let document = self.documents.get(&id).ok_or(DocumentError::NotOpen(id))?;
        match fs::read(&document.snapshot.identity_path) {
            Ok(bytes) if bytes == document.base_bytes => Ok(DiskStatus::Unchanged),
            Ok(_) => Ok(DiskStatus::Modified),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(DiskStatus::Deleted),
            Err(source) => Err(io_error(
                "inspect document",
                &document.snapshot.identity_path,
                source,
            )),
        }
    }

    pub fn reload_from_disk(&mut self, id: DocumentId) -> Result<DocumentSnapshot, DocumentError> {
        let document = self.documents.get(&id).ok_or(DocumentError::NotOpen(id))?;
        if document.snapshot.is_dirty() {
            return Err(DocumentError::Dirty(id));
        }
        let identity_path = document.snapshot.identity_path.clone();
        let loaded = read_text_document(&identity_path, self.max_document_bytes)?;

        let document = self
            .documents
            .get_mut(&id)
            .expect("document remains open during synchronous reload");
        let version = document.snapshot.version.saturating_add(1);
        document.snapshot.text = loaded.text;
        document.snapshot.version = version;
        document.snapshot.saved_version = version;
        document.snapshot.encoding = loaded.encoding;
        document.snapshot.line_ending = loaded.line_ending;
        document.snapshot.has_final_newline = document.snapshot.text.ends_with('\n');
        document.snapshot.read_only = loaded.read_only;
        document.base_bytes = loaded.bytes;
        document.permissions = loaded.permissions;
        Ok(document.snapshot.clone())
    }

    pub fn close(&mut self, id: DocumentId) -> Result<(), DocumentError> {
        let document = self.documents.get(&id).ok_or(DocumentError::NotOpen(id))?;
        if document.snapshot.is_dirty() {
            return Err(DocumentError::Dirty(id));
        }
        self.discard(id)
    }

    pub fn discard(&mut self, id: DocumentId) -> Result<(), DocumentError> {
        let document = self
            .documents
            .remove(&id)
            .ok_or(DocumentError::NotOpen(id))?;
        self.identities.remove(&document.snapshot.identity_path);
        Ok(())
    }

    fn ensure_within_workspace(&self, path: &Path) -> Result<(), DocumentError> {
        if path.starts_with(&self.workspace_root) {
            Ok(())
        } else {
            Err(DocumentError::OutsideWorkspace {
                path: path.to_path_buf(),
            })
        }
    }
}

fn require_version(
    snapshot: &DocumentSnapshot,
    requested_version: u64,
) -> Result<(), DocumentError> {
    if snapshot.version == requested_version {
        Ok(())
    } else {
        Err(DocumentError::StaleVersion {
            id: snapshot.id,
            expected: snapshot.version,
            actual: requested_version,
        })
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, DocumentError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|source| io_error("resolve current directory for", path, source))
    }
}

fn read_text_document(
    path: &Path,
    max_document_bytes: u64,
) -> Result<LoadedDocument, DocumentError> {
    let metadata = fs::metadata(path).map_err(|source| io_error("inspect", path, source))?;
    if !metadata.is_file() {
        return Err(DocumentError::NotAFile {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() > max_document_bytes {
        return Err(DocumentError::TooLarge {
            path: path.to_path_buf(),
            limit: max_document_bytes,
        });
    }
    let bytes = fs::read(path).map_err(|source| io_error("read", path, source))?;
    if bytes.contains(&0) {
        return Err(DocumentError::Binary {
            path: path.to_path_buf(),
        });
    }
    let (encoding, payload) = if let Some(payload) = bytes.strip_prefix(UTF8_BOM) {
        (TextEncoding::Utf8Bom, payload)
    } else {
        (TextEncoding::Utf8, bytes.as_slice())
    };
    let decoded = std::str::from_utf8(payload).map_err(|_| DocumentError::InvalidUtf8 {
        path: path.to_path_buf(),
    })?;
    let line_ending = detect_line_ending(decoded, path)?;
    let text = match line_ending {
        LineEnding::Lf => decoded.to_string(),
        LineEnding::CrLf => decoded.replace("\r\n", "\n"),
    };
    let permissions = metadata.permissions();
    let read_only = permissions.readonly();
    Ok(LoadedDocument {
        text,
        encoding,
        line_ending,
        bytes,
        permissions,
        read_only,
    })
}

fn detect_line_ending(text: &str, path: &Path) -> Result<LineEnding, DocumentError> {
    let bytes = text.as_bytes();
    let mut crlf = 0usize;
    let mut lf = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        if index > 0 && bytes[index - 1] == b'\r' {
            crlf += 1;
        } else {
            lf += 1;
        }
    }
    if crlf > 0 && lf > 0 {
        Err(DocumentError::MixedLineEndings {
            path: path.to_path_buf(),
        })
    } else if crlf > 0 {
        Ok(LineEnding::CrLf)
    } else {
        Ok(LineEnding::Lf)
    }
}

fn encode_document(snapshot: &DocumentSnapshot) -> Vec<u8> {
    let text = match snapshot.line_ending {
        LineEnding::Lf => snapshot.text.clone(),
        LineEnding::CrLf => snapshot.text.replace('\n', "\r\n"),
    };
    let mut bytes = Vec::with_capacity(
        text.len()
            + if snapshot.encoding == TextEncoding::Utf8Bom {
                UTF8_BOM.len()
            } else {
                0
            },
    );
    if snapshot.encoding == TextEncoding::Utf8Bom {
        bytes.extend_from_slice(UTF8_BOM);
    }
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

fn atomic_write(path: &Path, bytes: &[u8], permissions: &Permissions) -> Result<(), DocumentError> {
    let parent = path.parent().ok_or_else(|| DocumentError::NotAFile {
        path: path.to_path_buf(),
    })?;
    let mut temporary = NamedTempFile::new_in(parent)
        .map_err(|source| io_error("create temporary file for", path, source))?;
    temporary
        .as_file_mut()
        .set_permissions(permissions.clone())
        .map_err(|source| io_error("set temporary file permissions for", path, source))?;
    temporary
        .write_all(bytes)
        .map_err(|source| io_error("write temporary file for", path, source))?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|source| io_error("flush temporary file for", path, source))?;
    temporary
        .persist(path)
        .map_err(|error| io_error("replace", path, error.error))?;
    Ok(())
}

fn io_error(operation: &'static str, path: impl AsRef<Path>, source: io::Error) -> DocumentError {
    DocumentError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn multilingual_text_and_paths_round_trip() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("한글-日本語-🙂.txt");
        write(&path, "안녕하세요\nこんにちは\nمرحبا\n🙂\n".as_bytes());
        let mut service = DocumentService::new(workspace.path()).unwrap();

        let opened = service.open(&path).unwrap();
        assert_eq!(opened.document.encoding, TextEncoding::Utf8);
        assert_eq!(opened.document.line_ending, LineEnding::Lf);
        assert!(opened.document.has_final_newline);
        let updated = service
            .update_text(
                opened.document.id,
                opened.document.version,
                "수정됨\n変更済み\nتم التعديل\n🚀\n".into(),
            )
            .unwrap();
        assert!(updated.is_dirty());
        let saved = service.save(updated.id, updated.version).unwrap();
        assert!(!saved.is_dirty());
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "수정됨\n変更済み\nتم التعديل\n🚀\n"
        );
    }

    #[test]
    fn utf8_bom_and_crlf_are_preserved() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("bom.txt");
        let mut original = UTF8_BOM.to_vec();
        original.extend_from_slice("첫 줄\r\n二行目\r\n".as_bytes());
        write(&path, &original);
        let mut service = DocumentService::new(workspace.path()).unwrap();

        let opened = service.open(&path).unwrap().document;
        assert_eq!(opened.encoding, TextEncoding::Utf8Bom);
        assert_eq!(opened.line_ending, LineEnding::CrLf);
        assert_eq!(opened.text, "첫 줄\n二行目\n");
        let updated = service
            .update_text(opened.id, opened.version, "첫 줄\n二行目\n세 번째\n".into())
            .unwrap();
        service.save(updated.id, updated.version).unwrap();

        let saved = fs::read(path).unwrap();
        assert!(saved.starts_with(UTF8_BOM));
        assert_eq!(
            &saved[UTF8_BOM.len()..],
            "첫 줄\r\n二行目\r\n세 번째\r\n".as_bytes()
        );
    }

    #[test]
    fn mixed_line_endings_are_rejected_without_rewriting() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("mixed.txt");
        let original = b"one\r\ntwo\n";
        write(&path, original);
        let mut service = DocumentService::new(workspace.path()).unwrap();

        assert!(matches!(
            service.open(&path),
            Err(DocumentError::MixedLineEndings { .. })
        ));
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn binary_and_invalid_utf8_are_rejected() {
        let workspace = tempdir().unwrap();
        let binary = workspace.path().join("binary.dat");
        let invalid = workspace.path().join("invalid.txt");
        write(&binary, b"text\0data");
        write(&invalid, &[0xff, 0xfe]);
        let mut service = DocumentService::new(workspace.path()).unwrap();

        assert!(matches!(
            service.open(&binary),
            Err(DocumentError::Binary { .. })
        ));
        assert!(matches!(
            service.open(&invalid),
            Err(DocumentError::InvalidUtf8 { .. })
        ));
    }

    #[test]
    fn stale_version_never_reaches_disk() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("version.txt");
        write(&path, b"base\n");
        let mut service = DocumentService::new(workspace.path()).unwrap();
        let opened = service.open(&path).unwrap().document;
        let updated = service
            .update_text(opened.id, opened.version, "new\n".into())
            .unwrap();

        assert!(matches!(
            service.save(updated.id, opened.version),
            Err(DocumentError::StaleVersion { .. })
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), "base\n");
    }

    #[test]
    fn external_change_blocks_save_and_is_reported() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("conflict.txt");
        write(&path, b"base\n");
        let mut service = DocumentService::new(workspace.path()).unwrap();
        let opened = service.open(&path).unwrap().document;
        let updated = service
            .update_text(opened.id, opened.version, "editor\n".into())
            .unwrap();
        write(&path, b"external\n");

        assert_eq!(
            service.disk_status(updated.id).unwrap(),
            DiskStatus::Modified
        );
        assert!(matches!(
            service.save(updated.id, updated.version),
            Err(DocumentError::ExternalChange { .. })
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), "external\n");
    }

    #[test]
    fn clean_external_change_reloads_multilingual_content() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("외부-日本語.txt");
        write(&path, "처음\n".as_bytes());
        let mut service = DocumentService::new(workspace.path()).unwrap();
        let opened = service.open(&path).unwrap().document;

        write(&path, "외부 변경 日本語 🙂\n".as_bytes());
        assert_eq!(
            service.disk_status(opened.id).unwrap(),
            DiskStatus::Modified
        );

        let reloaded = service.reload_from_disk(opened.id).unwrap();
        assert_eq!(reloaded.text, "외부 변경 日本語 🙂\n");
        assert_eq!(reloaded.version, opened.version + 1);
        assert!(!reloaded.is_dirty());
        assert_eq!(
            service.disk_status(opened.id).unwrap(),
            DiskStatus::Unchanged
        );
    }

    #[test]
    fn dirty_document_cannot_be_reloaded_from_disk() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("dirty-reload.txt");
        write(&path, b"base\n");
        let mut service = DocumentService::new(workspace.path()).unwrap();
        let opened = service.open(&path).unwrap().document;
        service
            .update_text(opened.id, opened.version, "editor\n".into())
            .unwrap();
        write(&path, b"external\n");

        assert!(matches!(
            service.reload_from_disk(opened.id),
            Err(DocumentError::Dirty(id)) if id == opened.id
        ));
        assert_eq!(service.snapshot(opened.id).unwrap().text, "editor\n");
        assert_eq!(fs::read_to_string(path).unwrap(), "external\n");
    }

    #[test]
    fn save_as_uses_multilingual_target_and_updates_identity() {
        let workspace = tempdir().unwrap();
        let source = workspace.path().join("source.txt");
        let target = workspace.path().join("저장-日本語.txt");
        write(&source, b"source\n");
        let mut service = DocumentService::new(workspace.path()).unwrap();
        let opened = service.open(&source).unwrap().document;
        let updated = service
            .update_text(opened.id, opened.version, "새 파일\n新しい\n".into())
            .unwrap();

        let saved = service
            .save_as(updated.id, updated.version, &target, false)
            .unwrap();
        assert_eq!(saved.display_path, target);
        assert_eq!(fs::read_to_string(&target).unwrap(), "새 파일\n新しい\n");
        assert!(service.open(&target).unwrap().already_open);
    }

    #[test]
    fn dirty_close_requires_explicit_discard() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("dirty.txt");
        write(&path, b"base\n");
        let mut service = DocumentService::new(workspace.path()).unwrap();
        let opened = service.open(&path).unwrap().document;
        let updated = service
            .update_text(opened.id, opened.version, "dirty\n".into())
            .unwrap();

        assert!(matches!(
            service.close(updated.id),
            Err(DocumentError::Dirty(_))
        ));
        service.discard(updated.id).unwrap();
        assert!(matches!(
            service.snapshot(updated.id),
            Err(DocumentError::NotOpen(_))
        ));
    }

    #[test]
    fn oversized_file_is_rejected_before_reading() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("large.txt");
        write(&path, b"12345");
        let mut service = DocumentService::with_max_document_bytes(workspace.path(), 4).unwrap();

        assert!(matches!(
            service.open(path),
            Err(DocumentError::TooLarge { .. })
        ));
    }

    #[test]
    fn outside_workspace_is_rejected() {
        let workspace = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let path = outside.path().join("outside.txt");
        write(&path, b"outside\n");
        let mut service = DocumentService::new(workspace.path()).unwrap();

        assert!(matches!(
            service.open(path),
            Err(DocumentError::OutsideWorkspace { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempdir().unwrap();
        let path = workspace.path().join("script.sh");
        write(&path, b"#!/bin/sh\n");
        fs::set_permissions(&path, Permissions::from_mode(0o754)).unwrap();
        let mut service = DocumentService::new(workspace.path()).unwrap();
        let opened = service.open(&path).unwrap().document;
        let updated = service
            .update_text(opened.id, opened.version, "#!/bin/sh\necho 안녕\n".into())
            .unwrap();
        service.save(updated.id, updated.version).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o754
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_identity_deduplicates_and_outside_target_is_rejected() {
        use std::os::unix::fs::symlink;

        let workspace = tempdir().unwrap();
        let target = workspace.path().join("target.txt");
        let alias = workspace.path().join("별칭.txt");
        write(&target, b"inside\n");
        symlink(&target, &alias).unwrap();
        let mut service = DocumentService::new(workspace.path()).unwrap();
        let first = service.open(&alias).unwrap();
        let second = service.open(&target).unwrap();
        assert!(second.already_open);
        assert_eq!(first.document.id, second.document.id);

        let outside = tempdir().unwrap();
        let outside_target = outside.path().join("outside.txt");
        let outside_alias = workspace.path().join("outside-link.txt");
        write(&outside_target, b"outside\n");
        symlink(outside_target, &outside_alias).unwrap();
        assert!(matches!(
            service.open(outside_alias),
            Err(DocumentError::OutsideWorkspace { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn save_as_existing_symlink_updates_target_without_replacing_link() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let workspace = tempdir().unwrap();
        let source = workspace.path().join("source.txt");
        let target = workspace.path().join("target.txt");
        let alias = workspace.path().join("대상-別名.txt");
        write(&source, b"source\n");
        write(&target, b"target\n");
        fs::set_permissions(&target, Permissions::from_mode(0o640)).unwrap();
        symlink(&target, &alias).unwrap();
        let mut service = DocumentService::new(workspace.path()).unwrap();
        let opened = service.open(&source).unwrap().document;
        let updated = service
            .update_text(opened.id, opened.version, "변경\n変更\n".into())
            .unwrap();

        service
            .save_as(updated.id, updated.version, &alias, true)
            .unwrap();

        assert!(fs::symlink_metadata(&alias)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "변경\n変更\n");
        assert_eq!(
            fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn document_snapshot_serializes_unicode_without_loss() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("serialize.txt");
        write(&path, "한글 日本語 العربية 🙂\n".as_bytes());
        let mut service = DocumentService::new(workspace.path()).unwrap();
        let snapshot = service.open(path).unwrap().document;

        let json = serde_json::to_string(&snapshot).unwrap();
        let restored: DocumentSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, snapshot);
    }

    #[test]
    fn atomic_save_replaces_complete_file() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("atomic.txt");
        write(&path, b"old\n");
        let mut service = DocumentService::new(workspace.path()).unwrap();
        let opened = service.open(&path).unwrap().document;
        let new_text = "새 내용\n".repeat(4096);
        let updated = service
            .update_text(opened.id, opened.version, new_text.clone())
            .unwrap();
        service.save(updated.id, updated.version).unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), new_text);
    }

    #[test]
    fn read_only_document_refuses_save() {
        let workspace = tempdir().unwrap();
        let path = workspace.path().join("readonly.txt");
        write(&path, b"base\n");
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions).unwrap();
        let mut service = DocumentService::new(workspace.path()).unwrap();
        let opened = service.open(&path).unwrap().document;
        assert!(opened.read_only);
        let updated = service
            .update_text(opened.id, opened.version, "changed\n".into())
            .unwrap();

        assert!(matches!(
            service.save(updated.id, updated.version),
            Err(DocumentError::ReadOnly { .. })
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), "base\n");
    }

    #[test]
    fn target_exists_requires_overwrite_confirmation() {
        let workspace = tempdir().unwrap();
        let source = workspace.path().join("source.txt");
        let target = workspace.path().join("target.txt");
        write(&source, b"source\n");
        write(&target, b"target\n");
        let mut service = DocumentService::new(workspace.path()).unwrap();
        let opened = service.open(source).unwrap().document;

        assert!(matches!(
            service.save_as(opened.id, opened.version, &target, false),
            Err(DocumentError::TargetExists { .. })
        ));
        assert_eq!(fs::read_to_string(target).unwrap(), "target\n");
    }
}
