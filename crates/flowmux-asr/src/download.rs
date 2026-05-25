// SPDX-License-Identifier: GPL-3.0-or-later
//! Async, integrity-checked model downloader.
//!
//! `ModelDownloader::start(runtime, entry)` streams a `.tar.bz2`
//! archive from the catalog URL into the store's
//! `<id>.tar.bz2.partial` path, hashing every byte into a streaming
//! SHA-256. When the body finishes:
//!
//! 1. The computed digest is compared against `entry.archive_sha256`
//!    (skipped with a warning when the field is empty — upstream
//!    `sherpa-onnx` releases do not publish per-asset hashes).
//! 2. The archive is extracted in place via `tar` + `bzip2` so the
//!    `<root>/<directory_name>/` layout the store expects appears
//!    atomically.
//! 3. The verified `.tar.bz2` is deleted.
//!
//! Progress is surfaced through a `tokio::sync::mpsc` channel of
//! [`DownloadEvent`] values consumed on the GTK side.

use crate::catalog::ModelEntry;
use crate::store::ModelStore;
use bzip2::read::BzDecoder;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::PathBuf;
use tar::Archive;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy)]
pub struct DownloadProgress {
    pub bytes_received: u64,
    pub total: Option<u64>,
}

impl DownloadProgress {
    pub fn ratio(&self) -> Option<f64> {
        match self.total {
            Some(total) if total > 0 => {
                let r = self.bytes_received as f64 / total as f64;
                Some(r.clamp(0.0, 1.0))
            }
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum DownloadEvent {
    Started {
        total: Option<u64>,
    },
    Progress(DownloadProgress),
    /// Archive transfer completed; extraction has started.
    Extracting,
    /// Incremental extraction progress.
    /// `ratio` is bytes-decompressed / bytes-compressed-total (0..=1).
    /// `current_entry` is the most-recently-extracted file name.
    ExtractProgress {
        ratio: f64,
        current_entry: Option<String>,
    },
    /// Final state: extraction finished and the unpacked directory is
    /// ready for the engine to load.
    Finished {
        directory: PathBuf,
    },
    Failed(DownloadError),
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("http error: {0}")]
    Http(String),
    #[error("non-success status: {0}")]
    BadStatus(u16),
    #[error("write to disk failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("hash mismatch (expected {expected}, got {actual})")]
    HashMismatch { expected: String, actual: String },
    #[error("archive extraction failed: {0}")]
    Extract(String),
    #[error("cancelled by caller")]
    Cancelled,
}

impl From<reqwest::Error> for DownloadError {
    fn from(value: reqwest::Error) -> Self {
        DownloadError::Http(value.to_string())
    }
}

pub struct ModelDownloader {
    store: ModelStore,
    client: reqwest::Client,
}

impl ModelDownloader {
    pub fn new(store: ModelStore) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("flowmux-asr/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest::Client::build with default config never fails");
        Self { store, client }
    }

    /// Start the download on the supplied tokio runtime handle. The
    /// returned receiver carries `DownloadEvent` values until the
    /// transfer completes or fails.
    pub fn start(
        &self,
        runtime: &tokio::runtime::Handle,
        entry: ModelEntry,
    ) -> mpsc::Receiver<DownloadEvent> {
        let (tx, rx) = mpsc::channel(16);
        let store = self.store.clone();
        let client = self.client.clone();
        runtime.spawn(async move {
            let result = run_download(client, store, entry, tx.clone()).await;
            if let Err(err) = result {
                let _ = tx.send(DownloadEvent::Failed(err)).await;
            }
        });
        rx
    }
}

async fn run_download(
    client: reqwest::Client,
    store: ModelStore,
    entry: ModelEntry,
    tx: mpsc::Sender<DownloadEvent>,
) -> Result<(), DownloadError> {
    store.ensure_dir().map_err(DownloadError::Io)?;
    let partial = store.partial_archive_path(&entry);
    if partial.exists() {
        let _ = std::fs::remove_file(&partial);
    }
    // Drop any previously-extracted directory so a stale partial
    // install cannot mask a fresh download.
    let target_dir = store.model_dir(&entry);
    if target_dir.exists() {
        let _ = std::fs::remove_dir_all(&target_dir);
    }

    let response = client.get(&entry.archive_url).send().await?;
    if !response.status().is_success() {
        return Err(DownloadError::BadStatus(response.status().as_u16()));
    }
    let total = response.content_length().or(Some(entry.archive_size_bytes));
    let _ = tx.send(DownloadEvent::Started { total }).await;

    let mut file = std::fs::File::create(&partial)?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    let mut stream = response.bytes_stream();
    let mut last_progress_bytes: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk)?;
        received += chunk.len() as u64;
        if received - last_progress_bytes >= 1_048_576 {
            last_progress_bytes = received;
            let _ = tx
                .send(DownloadEvent::Progress(DownloadProgress {
                    bytes_received: received,
                    total,
                }))
                .await;
        }
    }
    file.sync_all()?;
    drop(file);

    let computed = format!("{:x}", hasher.finalize());
    if entry.archive_sha256.is_empty() {
        tracing::warn!(
            model = %entry.id.as_str(),
            "ASR model archive downloaded without sha256 verification — upstream sherpa-onnx releases ship without per-asset hashes; fill in catalog entry before pinning to a release"
        );
    } else if computed != entry.archive_sha256 {
        let _ = std::fs::remove_file(&partial);
        return Err(DownloadError::HashMismatch {
            expected: entry.archive_sha256.clone(),
            actual: computed,
        });
    }

    let _ = tx.send(DownloadEvent::Extracting).await;

    // Extract `.tar.bz2` into the store root with per-entry progress.
    // The compressed-byte counter lets the UI show a real percentage:
    // we cannot pre-compute the uncompressed total cheaply, so we
    // report progress as "compressed bytes consumed / archive size",
    // which matches what the user just watched download.
    let partial_clone = partial.clone();
    let root_clone = store.root().to_path_buf();
    let compressed_total = std::fs::metadata(&partial_clone)
        .map(|m| m.len())
        .unwrap_or(0);
    let tx_extract = tx.clone();
    let extract_result = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let f = std::fs::File::open(&partial_clone)
            .map_err(|e| format!("open archive: {e}"))?;
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counting = CountingReader::new(f, counter.clone());
        let bz = BzDecoder::new(counting);
        let mut archive = Archive::new(bz);
        let entries = archive
            .entries()
            .map_err(|e| format!("read entries: {e}"))?;
        for entry_result in entries {
            let mut entry = entry_result.map_err(|e| format!("entry header: {e}"))?;
            let entry_path = entry
                .path()
                .map(|p| p.display().to_string())
                .ok();
            entry
                .unpack_in(&root_clone)
                .map_err(|e| format!("unpack entry: {e}"))?;
            let read = counter.load(std::sync::atomic::Ordering::Relaxed);
            let ratio = if compressed_total > 0 {
                (read as f64 / compressed_total as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let _ = tx_extract.blocking_send(DownloadEvent::ExtractProgress {
                ratio,
                current_entry: entry_path,
            });
        }
        Ok(())
    })
    .await
    .map_err(|e| DownloadError::Extract(format!("worker join: {e}")))?;
    if let Err(e) = extract_result {
        return Err(DownloadError::Extract(e));
    }

    let _ = std::fs::remove_file(&partial);

    let _ = tx
        .send(DownloadEvent::Finished {
            directory: target_dir.clone(),
        })
        .await;
    Ok(())
}

/// Counts the number of bytes read from `inner`. Wrapped around the
/// raw archive file so the extraction loop can compute a progress
/// ratio while bzip2 + tar pull bytes through it.
struct CountingReader<R> {
    inner: R,
    bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl<R> CountingReader<R> {
    fn new(inner: R, bytes: std::sync::Arc<std::sync::atomic::AtomicU64>) -> Self {
        Self { inner, bytes }
    }
}

impl<R: std::io::Read> std::io::Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes
            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(n)
    }
}

pub fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_ratio_is_none_without_total() {
        let p = DownloadProgress {
            bytes_received: 100,
            total: None,
        };
        assert!(p.ratio().is_none());
    }

    #[test]
    fn progress_ratio_clamps_to_unit_interval() {
        let p = DownloadProgress {
            bytes_received: 500,
            total: Some(1000),
        };
        assert_eq!(p.ratio(), Some(0.5));
        let over = DownloadProgress {
            bytes_received: 2000,
            total: Some(1000),
        };
        assert_eq!(over.ratio(), Some(1.0));
    }

    #[test]
    fn sha256_file_matches_in_memory_digest() {
        let payload = b"flowmux ASR streaming sha256 round-trip";
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), payload).unwrap();
        let file_hash = sha256_file(tmp.path()).unwrap();
        let one_shot = format!("{:x}", Sha256::digest(payload));
        assert_eq!(file_hash, one_shot);
    }
}
