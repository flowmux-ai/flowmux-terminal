// SPDX-License-Identifier: GPL-3.0-or-later
//! Microphone capture + resample pipeline.
//!
//! [`capture::AudioCapture`] starts a `cpal` input stream on the
//! requested device, copies interleaved samples into a [`PcmBuffer`],
//! and resamples them down to 16 kHz mono `f32`. [`probe`] runs a tiny
//! capture session that the options dialog uses to surface microphone
//! permission state to the user.

pub mod capture;
pub mod probe;
pub mod resample;

pub use capture::{AudioCapture, CaptureHandle, CapturedAudio, PcmBuffer};
pub use probe::{probe_microphone, MicProbeOutcome};
pub use resample::{resample_to_16k_mono, TARGET_SAMPLE_RATE};

/// Enumerate the names of every input device cpal exposes on the
/// default host. Used by the options dialog to populate the device
/// dropdown — picking the actual mic instead of PulseAudio's
/// "default" source (which on some setups is an output monitor that
/// never captures actual speech) is the most common diagnostic step
/// when the engine produces no text from a hold-press.
pub fn enumerate_input_devices() -> Vec<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let mut names = Vec::new();
    if let Ok(devices) = host.input_devices() {
        for d in devices {
            if let Ok(n) = d.name() {
                names.push(n);
            }
        }
    }
    names
}

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no input device available")]
    NoInputDevice,
    #[error("requested input device not found: {0}")]
    DeviceNotFound(String),
    #[error("default config unavailable: {0}")]
    DefaultConfig(String),
    #[error("input stream build failed: {0}")]
    BuildStream(String),
    #[error("input stream play failed: {0}")]
    PlayStream(String),
    #[error("microphone permission denied")]
    PermissionDenied,
    #[error("audio backend reported error: {0}")]
    Backend(String),
}
