// SPDX-License-Identifier: GPL-3.0-or-later
//! Private, atomic recovery snapshots for unsaved editor documents.

use crate::{LineEnding, TextEncoding, DEFAULT_MAX_DOCUMENT_BYTES};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, Permissions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use thiserror::Error;

pub const RECOVERY_FORMAT_VERSION: u16 = 1;
const MAX_RECOVERY_FILE_BYTES: u64 = DEFAULT_MAX_DOCUMENT_BYTES + 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryDiskState {
    Unchanged,
    Changed,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOperation {
    Write(RecoverySnapshot),
    Remove(PathBuf),
}

impl RecoveryOperation {
    pub fn identity_path(&self) -> &Path {
        match self {
            Self::Write(snapshot) => &snapshot.identity_path,
            Self::Remove(path) => path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoverySnapshot {
    pub format_version: u16,
    pub workspace_id: String,
    pub identity_path: PathBuf,
    pub base_hash: String,
    pub document_version: u64,
    pub content: String,
    pub encoding: TextEncoding,
    pub line_ending: LineEnding,
}

impl RecoverySnapshot {
    pub fn new(
        workspace_id: String,
        identity_path: PathBuf,
        base_bytes: &[u8],
        document_version: u64,
        content: String,
        encoding: TextEncoding,
        line_ending: LineEnding,
    ) -> Self {
        Self {
            format_version: RECOVERY_FORMAT_VERSION,
            workspace_id,
            identity_path,
            base_hash: content_hash(base_bytes),
            document_version,
            content,
            encoding,
            line_ending,
        }
    }
}

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("failed to {operation} editor recovery data at {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid editor recovery data at {path}: {source}")]
    InvalidJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("unsupported editor recovery format {actual} at {path}")]
    UnsupportedFormat { path: PathBuf, actual: u16 },
    #[error("editor recovery data belongs to a different workspace: {path}")]
    WrongWorkspace { path: PathBuf },
    #[error("editor recovery data refers to a different document: {path}")]
    WrongDocument { path: PathBuf },
    #[error("editor recovery data exceeds the {limit} byte limit: {path}")]
    TooLarge { path: PathBuf, limit: u64 },
}

#[derive(Debug, Clone)]
pub struct RecoveryStore {
    workspace_id: String,
    directory: PathBuf,
}

impl RecoveryStore {
    pub fn new(
        state_root: impl AsRef<Path>,
        workspace_root: impl AsRef<Path>,
    ) -> Result<Self, RecoveryError> {
        let workspace_root = fs::canonicalize(workspace_root.as_ref())
            .map_err(|source| recovery_io("resolve workspace", workspace_root.as_ref(), source))?;
        let workspace_id = content_hash(workspace_root.to_string_lossy().as_bytes());
        let directory = state_root
            .as_ref()
            .join("editor-recovery")
            .join(&workspace_id);
        create_private_directory(&directory)?;
        Ok(Self {
            workspace_id,
            directory,
        })
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub fn write(&self, snapshot: &RecoverySnapshot) -> Result<(), RecoveryError> {
        let path = self.snapshot_path(&snapshot.identity_path);
        if snapshot.workspace_id != self.workspace_id {
            return Err(RecoveryError::WrongWorkspace { path });
        }
        if snapshot.content.len() > DEFAULT_MAX_DOCUMENT_BYTES as usize {
            return Err(RecoveryError::TooLarge {
                path,
                limit: DEFAULT_MAX_DOCUMENT_BYTES,
            });
        }
        let bytes = serde_json::to_vec(snapshot).map_err(|source| RecoveryError::InvalidJson {
            path: path.clone(),
            source,
        })?;
        if bytes.len() as u64 > MAX_RECOVERY_FILE_BYTES {
            return Err(RecoveryError::TooLarge {
                path,
                limit: MAX_RECOVERY_FILE_BYTES,
            });
        }

        let permissions = private_file_permissions();
        let mut temporary = NamedTempFile::new_in(&self.directory)
            .map_err(|source| recovery_io("create temporary file", &path, source))?;
        temporary
            .as_file_mut()
            .set_permissions(permissions)
            .map_err(|source| recovery_io("set file permissions", &path, source))?;
        temporary
            .write_all(&bytes)
            .map_err(|source| recovery_io("write snapshot", &path, source))?;
        temporary
            .as_file_mut()
            .sync_all()
            .map_err(|source| recovery_io("flush snapshot", &path, source))?;
        temporary
            .persist(&path)
            .map_err(|error| recovery_io("replace snapshot", &path, error.error))?;
        Ok(())
    }

    pub fn read(
        &self,
        identity_path: impl AsRef<Path>,
    ) -> Result<Option<(RecoverySnapshot, RecoveryDiskState)>, RecoveryError> {
        let identity_path = identity_path.as_ref();
        let path = self.snapshot_path(identity_path);
        let mut file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(recovery_io("open snapshot", &path, source)),
        };
        let length = file
            .metadata()
            .map_err(|source| recovery_io("inspect snapshot", &path, source))?
            .len();
        if length > MAX_RECOVERY_FILE_BYTES {
            return Err(RecoveryError::TooLarge {
                path,
                limit: MAX_RECOVERY_FILE_BYTES,
            });
        }
        let mut bytes = Vec::with_capacity(length as usize);
        file.read_to_end(&mut bytes)
            .map_err(|source| recovery_io("read snapshot", &path, source))?;
        let snapshot: RecoverySnapshot =
            serde_json::from_slice(&bytes).map_err(|source| RecoveryError::InvalidJson {
                path: path.clone(),
                source,
            })?;
        if snapshot.format_version != RECOVERY_FORMAT_VERSION {
            return Err(RecoveryError::UnsupportedFormat {
                path,
                actual: snapshot.format_version,
            });
        }
        if snapshot.workspace_id != self.workspace_id {
            return Err(RecoveryError::WrongWorkspace { path });
        }
        if snapshot.identity_path != identity_path {
            return Err(RecoveryError::WrongDocument { path });
        }
        if snapshot.content.len() > DEFAULT_MAX_DOCUMENT_BYTES as usize {
            return Err(RecoveryError::TooLarge {
                path,
                limit: DEFAULT_MAX_DOCUMENT_BYTES,
            });
        }
        let disk_state = match fs::read(identity_path) {
            Ok(bytes) if content_hash(&bytes) == snapshot.base_hash => RecoveryDiskState::Unchanged,
            Ok(_) => RecoveryDiskState::Changed,
            Err(error) if error.kind() == io::ErrorKind::NotFound => RecoveryDiskState::Deleted,
            Err(source) => return Err(recovery_io("read document", identity_path, source)),
        };
        Ok(Some((snapshot, disk_state)))
    }

    pub fn remove(&self, identity_path: impl AsRef<Path>) -> Result<(), RecoveryError> {
        let path = self.snapshot_path(identity_path.as_ref());
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(recovery_io("remove snapshot", path, source)),
        }
    }

    pub fn apply(&self, operation: &RecoveryOperation) -> Result<(), RecoveryError> {
        match operation {
            RecoveryOperation::Write(snapshot) => self.write(snapshot),
            RecoveryOperation::Remove(path) => self.remove(path),
        }
    }

    pub fn snapshot_path(&self, identity_path: impl AsRef<Path>) -> PathBuf {
        self.directory.join(format!(
            "{}.json",
            content_hash(identity_path.as_ref().to_string_lossy().as_bytes())
        ))
    }
}

fn content_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn create_private_directory(path: &Path) -> Result<(), RecoveryError> {
    fs::create_dir_all(path).map_err(|source| recovery_io("create directory", path, source))?;
    fs::set_permissions(path, private_directory_permissions())
        .map_err(|source| recovery_io("set directory permissions", path, source))
}

#[cfg(unix)]
fn private_directory_permissions() -> Permissions {
    use std::os::unix::fs::PermissionsExt;
    Permissions::from_mode(0o700)
}

#[cfg(not(unix))]
fn private_directory_permissions() -> Permissions {
    fs::metadata(std::env::temp_dir())
        .expect("the temporary directory has permissions")
        .permissions()
}

#[cfg(unix)]
fn private_file_permissions() -> Permissions {
    use std::os::unix::fs::PermissionsExt;
    Permissions::from_mode(0o600)
}

#[cfg(not(unix))]
fn private_file_permissions() -> Permissions {
    let mut permissions = fs::metadata(std::env::temp_dir())
        .expect("the temporary directory has permissions")
        .permissions();
    permissions.set_readonly(false);
    permissions
}

fn recovery_io(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: io::Error,
) -> RecoveryError {
    RecoveryError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn snapshot(store: &RecoveryStore, path: &Path, base: &[u8]) -> RecoverySnapshot {
        RecoverySnapshot::new(
            store.workspace_id().to_string(),
            path.to_path_buf(),
            base,
            7,
            "복구할 내용\n日本語 🙂\n".into(),
            TextEncoding::Utf8,
            LineEnding::Lf,
        )
    }

    #[test]
    fn multilingual_snapshot_round_trips_and_detects_disk_changes() {
        let workspace = tempdir().unwrap();
        let state = tempdir().unwrap();
        let path = workspace.path().join("문서-日本語-🙂.txt");
        let base = "원본\n".as_bytes();
        fs::write(&path, base).unwrap();
        let store = RecoveryStore::new(state.path(), workspace.path()).unwrap();
        let recovery = snapshot(&store, &path, base);

        store.write(&recovery).unwrap();
        let (loaded, status) = store.read(&path).unwrap().unwrap();
        assert_eq!(loaded, recovery);
        assert_eq!(status, RecoveryDiskState::Unchanged);

        fs::write(&path, "외부 변경\n").unwrap();
        assert_eq!(
            store.read(&path).unwrap().unwrap().1,
            RecoveryDiskState::Changed
        );
        fs::remove_file(&path).unwrap();
        assert_eq!(
            store.read(&path).unwrap().unwrap().1,
            RecoveryDiskState::Deleted
        );
    }

    #[test]
    fn removing_snapshot_is_idempotent() {
        let workspace = tempdir().unwrap();
        let state = tempdir().unwrap();
        let path = workspace.path().join("file.txt");
        fs::write(&path, "base\n").unwrap();
        let store = RecoveryStore::new(state.path(), workspace.path()).unwrap();
        store.write(&snapshot(&store, &path, b"base\n")).unwrap();

        store.remove(&path).unwrap();
        store.remove(&path).unwrap();
        assert!(store.read(&path).unwrap().is_none());
    }

    #[test]
    fn tampered_snapshot_is_rejected() {
        let workspace = tempdir().unwrap();
        let state = tempdir().unwrap();
        let path = workspace.path().join("file.txt");
        fs::write(&path, "base\n").unwrap();
        let store = RecoveryStore::new(state.path(), workspace.path()).unwrap();
        let mut recovery = snapshot(&store, &path, b"base\n");
        recovery.workspace_id = "different".into();

        let error = store.write(&recovery).unwrap_err();
        assert!(matches!(error, RecoveryError::WrongWorkspace { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_and_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempdir().unwrap();
        let state = tempdir().unwrap();
        let path = workspace.path().join("file.txt");
        fs::write(&path, "base\n").unwrap();
        let store = RecoveryStore::new(state.path(), workspace.path()).unwrap();
        store.write(&snapshot(&store, &path, b"base\n")).unwrap();

        let snapshot_mode = fs::metadata(store.snapshot_path(&path))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let directory_mode = fs::metadata(store.snapshot_path(&path).parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(snapshot_mode, 0o600);
        assert_eq!(directory_mode, 0o700);
    }
}
