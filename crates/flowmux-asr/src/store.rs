// SPDX-License-Identifier: GPL-3.0-or-later
//! On-disk layout for downloaded streaming Zipformer models.
//!
//! Files live under `$XDG_DATA_HOME/flowmux/asr/models/<directory>/`.
//! The downloader writes the source `.tar.bz2` into a `.partial`
//! sibling, verifies the SHA-256, then extracts the archive in place
//! and removes the tarball. A model is considered installed when the
//! encoder ONNX file exists inside the expected directory.

use crate::catalog::{ModelEntry, ModelId};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ModelStore {
    root: PathBuf,
}

impl ModelStore {
    /// `$XDG_DATA_HOME/flowmux/asr/models` on Linux.
    pub fn xdg_default() -> Option<Self> {
        let data = dirs::data_dir()?;
        Some(Self {
            root: data.join("flowmux").join("asr").join("models"),
        })
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Top-level directory that holds the unpacked model files.
    pub fn model_dir(&self, entry: &ModelEntry) -> PathBuf {
        self.root.join(&entry.directory_name)
    }

    /// Path to a specific ONNX/tokens file inside the model directory.
    pub fn file_path(&self, entry: &ModelEntry, filename: &str) -> PathBuf {
        self.model_dir(entry).join(filename)
    }

    /// Where a partially-downloaded archive lives before verification.
    pub fn partial_archive_path(&self, entry: &ModelEntry) -> PathBuf {
        self.root.join(format!("{}.partial", entry.archive_filename()))
    }

    /// True when the model ONNX + tokens file both exist on disk.
    pub fn is_installed(&self, entry: &ModelEntry) -> bool {
        let dir = self.model_dir(entry);
        dir.join(&entry.model_file).exists() && dir.join(&entry.tokens_file).exists()
    }

    pub fn ensure_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)
    }

    /// Delete the unpacked model directory and any leftover archive.
    pub fn remove(&self, entry: &ModelEntry) -> std::io::Result<()> {
        let dir = self.model_dir(entry);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        let partial = self.partial_archive_path(entry);
        if partial.exists() {
            std::fs::remove_file(&partial)?;
        }
        Ok(())
    }

    /// Recursive size of every installed model. Used by the options
    /// dialog to surface a "총 사용량" line.
    pub fn disk_usage(&self) -> std::io::Result<u64> {
        Ok(walk_size(&self.root))
    }

    /// Directory names currently present under the root. Used by the
    /// options dialog to surface a "설치된 모델" hint.
    pub fn installed_ids(&self) -> Vec<ModelId> {
        let Ok(read_dir) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in read_dir.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(name) = entry.file_name().to_str() {
                    out.push(ModelId::from(name));
                }
            }
        }
        out
    }
}

fn walk_size(root: &Path) -> u64 {
    let Ok(read_dir) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut total = 0;
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Ok(md) = path.metadata() {
                total += md.len();
            }
        } else if path.is_dir() {
            total += walk_size(&path);
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::recommended_default;
    use tempfile::TempDir;

    fn tmp() -> (TempDir, ModelStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ModelStore::new(dir.path().join("models"));
        store.ensure_dir().unwrap();
        (dir, store)
    }

    #[test]
    fn ensure_dir_creates_the_root_recursively() {
        let (_d, store) = tmp();
        assert!(store.root().is_dir());
    }

    #[test]
    fn is_installed_only_true_when_every_file_exists() {
        let (_d, store) = tmp();
        let entry = recommended_default();
        assert!(!store.is_installed(&entry));
        let dir = store.model_dir(&entry);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(&entry.model_file), b"m").unwrap();
        assert!(!store.is_installed(&entry));
        std::fs::write(dir.join(&entry.tokens_file), b"t").unwrap();
        assert!(store.is_installed(&entry));
    }

    #[test]
    fn remove_drops_directory_and_archive() {
        let (_d, store) = tmp();
        let entry = recommended_default();
        std::fs::create_dir_all(store.model_dir(&entry)).unwrap();
        std::fs::write(store.model_dir(&entry).join("x"), b"x").unwrap();
        std::fs::write(store.partial_archive_path(&entry), b"y").unwrap();
        store.remove(&entry).unwrap();
        assert!(!store.model_dir(&entry).exists());
        assert!(!store.partial_archive_path(&entry).exists());
    }

    #[test]
    fn installed_ids_lists_directories() {
        let (_d, store) = tmp();
        std::fs::create_dir_all(store.root().join("model-a")).unwrap();
        std::fs::create_dir_all(store.root().join("model-b")).unwrap();
        std::fs::write(store.root().join("README"), b"docs").unwrap();
        let mut ids: Vec<_> = store
            .installed_ids()
            .into_iter()
            .map(|i| i.as_str().to_string())
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["model-a".to_string(), "model-b".to_string()]);
    }

    #[test]
    fn disk_usage_sums_nested_files() {
        let (_d, store) = tmp();
        let sub = store.root().join("model-a");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("encoder"), vec![0u8; 512]).unwrap();
        std::fs::write(sub.join("decoder"), vec![0u8; 256]).unwrap();
        assert_eq!(store.disk_usage().unwrap(), 512 + 256);
    }
}
