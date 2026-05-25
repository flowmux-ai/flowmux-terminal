// SPDX-License-Identifier: GPL-3.0-or-later
//! SenseVoice recognizer wrapper. Uses sherpa-rs's high-level
//! `SenseVoiceRecognizer` (already a safe wrapper over the offline
//! FFI) so the unsafe surface stays inside the crate.

use super::SenseVoiceEngineConfig;
use sherpa_rs::sense_voice::{SenseVoiceConfig, SenseVoiceRecognizer};
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum SenseVoiceError {
    #[error("sherpa-rs failed to build the recognizer: {0}")]
    Build(String),
}

/// SenseVoice engine — one model instance, multiple transcriptions.
pub struct SenseVoiceEngine {
    recognizer: Mutex<SenseVoiceRecognizer>,
}

impl SenseVoiceEngine {
    pub fn load(config: SenseVoiceEngineConfig) -> Result<Self, SenseVoiceError> {
        let num_threads = if config.num_threads > 0 {
            config.num_threads
        } else {
            std::thread::available_parallelism()
                .map(|n| (n.get() as i32 - 2).clamp(2, 8))
                .unwrap_or(2)
        };
        // Explicit "ko" forces SenseVoice to decode in the Korean
        // sub-graph rather than running language ID first; on noisy
        // mic captures that decision saves a few hundred ms and
        // measurably reduces WER on Korean speech.
        let language = match config.language.trim() {
            "" => "ko".to_string(),
            other => other.to_string(),
        };
        let cfg = SenseVoiceConfig {
            model: config.model,
            tokens: config.tokens,
            language,
            use_itn: true,
            num_threads: Some(num_threads),
            ..Default::default()
        };
        let recognizer = SenseVoiceRecognizer::new(cfg).map_err(|e| SenseVoiceError::Build(e.to_string()))?;
        Ok(Self {
            recognizer: Mutex::new(recognizer),
        })
    }

    /// Transcribe a buffer of 16 kHz mono `f32` PCM. Blocking — the
    /// caller wraps it in `spawn_blocking`.
    pub fn transcribe(&self, sample_rate: u32, samples: &[f32]) -> String {
        let mut guard = match self.recognizer.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let result = guard.transcribe(sample_rate, samples);
        result.text
    }
}
