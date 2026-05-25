// SPDX-License-Identifier: GPL-3.0-or-later
//! Local automatic-speech-recognition (ASR) engine for the push-to-talk
//! voice input feature.
//!
//! The crate is split along the architectural seams shipped by flowmux:
//!
//! * [`catalog`] — the static list of supported Whisper models, with
//!   `(id, display, url, sha256, size)` for each entry.
//! * [`store`] — XDG-style on-disk layout for downloaded model files.
//! * [`download`] — async streaming download + integrity verification.
//! * [`audio`] — cpal-based microphone capture and rubato resampling
//!   to the 16 kHz mono PCM expected by Whisper.
//! * [`engine`] — the `AsrEngine` trait and its mock + whisper.cpp
//!   implementations.
//! * [`session`] — the push-to-talk session state machine glued on top
//!   of the engine.
//!
//! The crate is intentionally GTK-free; the GUI binary brokers between
//! GTK widgets and this crate through `async_channel`-style commands.

// The streaming Zipformer engine wraps raw `sherpa-rs-sys` FFI and
// therefore needs `unsafe` blocks. `forbid` is replaced with `deny`
// so the ASR-specific FFI module can opt in while everything else
// still gets the "no unsafe" guarantee.
#![deny(unsafe_code)]

pub mod audio;
pub mod catalog;
pub mod download;
pub mod engine;
pub mod session;
pub mod store;

pub use catalog::{ModelEntry, ModelId};
pub use download::{DownloadEvent, DownloadProgress, ModelDownloader};
pub use engine::{AsrEngineError, SenseVoiceEngine};
pub use session::{PttEvent, PttSession, SessionConfig};
pub use store::ModelStore;

/// Top-level errors surfaced by the public API.
#[derive(Debug, thiserror::Error)]
pub enum AsrError {
    #[error("model not installed: {0}")]
    ModelMissing(String),
    #[error("download failed: {0}")]
    Download(#[from] download::DownloadError),
    #[error("audio capture failed: {0}")]
    Audio(#[from] audio::AudioError),
    #[error("engine failed: {0}")]
    Engine(#[from] engine::AsrEngineError),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}
