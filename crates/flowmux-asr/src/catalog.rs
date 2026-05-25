// SPDX-License-Identifier: GPL-3.0-or-later
//! Static catalog of supported ASR models hosted by the upstream
//! `k2-fsa/sherpa-onnx` release pages.
//!
//! The crate now targets non-streaming SenseVoice — a single ONNX
//! file plus a tokens table, multilingual, high accuracy. Streaming
//! Zipformer was dropped because its Korean variant produced empty
//! output on live mic captures with realistic SNR.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(pub String);

impl ModelId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ModelId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// SenseVoice (or future single-file) ASR model entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: ModelId,
    pub display: String,
    /// `.tar.bz2` download URL.
    pub archive_url: String,
    /// Lower-case hex SHA-256 of the tarball. Empty = skip
    /// verification + warn.
    pub archive_sha256: String,
    pub archive_size_bytes: u64,
    /// Directory created by extraction (usually matches archive stem).
    pub directory_name: String,
    /// ONNX model filename inside the directory.
    pub model_file: String,
    /// Vocabulary file. Usually `tokens.txt`.
    pub tokens_file: String,
    pub language: String,
    pub recommendation: String,
}

impl ModelEntry {
    pub fn archive_filename(&self) -> String {
        format!("{}.tar.bz2", self.directory_name)
    }
}

pub fn entries() -> Vec<ModelEntry> {
    vec![ModelEntry {
        id: ModelId::from("sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17"),
        display: "SenseVoice Small (다국어, 한국어 포함)".into(),
        archive_url:
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17.tar.bz2"
                .into(),
        archive_sha256: String::new(),
        archive_size_bytes: 234_000_000,
        directory_name: "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17".into(),
        model_file: "model.int8.onnx".into(),
        tokens_file: "tokens.txt".into(),
        language: "ko".into(),
        recommendation: "권장 (한국어 WER ~5%)".into(),
    }]
}

pub fn find(id: &ModelId) -> Option<ModelEntry> {
    entries().into_iter().find(|e| &e.id == id)
}

pub fn recommended_default() -> ModelEntry {
    entries()
        .into_iter()
        .next()
        .expect("catalog must have at least one entry")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_non_empty_and_unique() {
        let rows = entries();
        assert!(!rows.is_empty());
        let mut ids: Vec<_> = rows.iter().map(|r| r.id.as_str().to_string()).collect();
        ids.sort();
        let original_len = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), original_len);
    }

    #[test]
    fn every_url_is_https_github_release() {
        for entry in entries() {
            assert!(entry.archive_url.starts_with("https://github.com/"));
            assert!(entry.archive_url.ends_with(".tar.bz2"));
        }
    }

    #[test]
    fn archive_filename_matches_directory_name() {
        for entry in entries() {
            assert_eq!(
                entry.archive_filename(),
                format!("{}.tar.bz2", entry.directory_name)
            );
        }
    }
}
