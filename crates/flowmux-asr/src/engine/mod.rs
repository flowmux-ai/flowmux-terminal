// SPDX-License-Identifier: GPL-3.0-or-later
//! Non-streaming ASR engine backed by sherpa-onnx SenseVoice.
//!
//! SenseVoice is multilingual (Korean / English / Chinese / Japanese
//! / Cantonese) with WER around 5 % on Korean — significantly better
//! than the streaming Zipformer model the crate previously used. The
//! tradeoff is no live partials: the controller accumulates audio
//! while the user holds the push-to-talk key and transcribes the
//! whole buffer on release.

pub mod sense_voice;

pub use sense_voice::SenseVoiceEngine;

use crate::catalog::ModelEntry;
use crate::store::ModelStore;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum AsrEngineError {
    #[error("model files missing in {0}")]
    ModelMissing(String),
    #[error("engine load failed: {0}")]
    Load(String),
    #[error("transcription failed: {0}")]
    Transcribe(String),
}

/// Load and warm the SenseVoice engine once at startup.
pub fn load_engine(
    entry: &ModelEntry,
    store: &ModelStore,
) -> Result<Arc<SenseVoiceEngine>, AsrEngineError> {
    let dir = store.model_dir(entry);
    let model = dir.join(&entry.model_file);
    let tokens = dir.join(&entry.tokens_file);
    for path in [&model, &tokens] {
        if !path.exists() {
            return Err(AsrEngineError::ModelMissing(path.display().to_string()));
        }
    }
    let engine = SenseVoiceEngine::load(SenseVoiceEngineConfig {
        model: model.to_string_lossy().into(),
        tokens: tokens.to_string_lossy().into(),
        language: entry.language.clone(),
        num_threads: 0,
    })
    .map_err(|e| AsrEngineError::Load(e.to_string()))?;
    Ok(Arc::new(engine))
}

#[derive(Debug, Clone)]
pub struct SenseVoiceEngineConfig {
    pub model: String,
    pub tokens: String,
    /// `"auto"` lets SenseVoice pick from `zh / en / ja / ko / yue`.
    pub language: String,
    /// 0 = pick a sensible default based on CPU count.
    pub num_threads: i32,
}
