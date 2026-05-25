// SPDX-License-Identifier: GPL-3.0-or-later
//! Push-to-talk session: glues the cpal capture loop to the
//! non-streaming SenseVoice recogniser.
//!
//! Flow:
//!   * `start()` opens the cpal stream and begins accumulating PCM
//!     into the shared capture buffer.
//!   * The controller polls `peak_so_far()` for UI feedback (dot
//!     pulse intensity) but does not transcribe mid-stream.
//!   * `finish()` stops the capture, downmixes + dumps a debug WAV,
//!     then feeds the entire 16 kHz mono buffer through the
//!     SenseVoice recogniser and returns the resulting text.

use crate::audio::capture::{AudioCapture, CaptureHandle, CaptureSpec, PcmBuffer};
use crate::audio::resample::resample_to_16k_mono;
use crate::engine::SenseVoiceEngine;
use crate::AsrError;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub device_name: Option<String>,
    pub max_duration: Duration,
    pub auto_enter: bool,
    pub min_duration: Duration,
    /// Linear gain applied to the captured samples before they reach
    /// the recogniser. `1.0` = pass-through.
    pub input_gain: f32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            device_name: None,
            max_duration: Duration::from_secs(30),
            auto_enter: false,
            min_duration: Duration::from_millis(150),
            input_gain: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
pub enum PttEvent {
    Recording,
    Done(String),
    Truncated,
    Cancelled,
    Failed(String),
}

pub struct PttSession {
    config: SessionConfig,
    capture: Option<CaptureHandle>,
}

impl PttSession {
    pub fn new(config: SessionConfig) -> Self {
        Self {
            config,
            capture: None,
        }
    }

    pub fn start(&mut self) -> Result<(), AsrError> {
        if self.capture.is_some() {
            return Err(AsrError::Other("session already running".into()));
        }
        let spec = CaptureSpec {
            device_name: self.config.device_name.clone(),
            max_duration: self.config.max_duration,
        };
        eprintln!(
            "[flowmux-asr] session.start: device={:?} max={:.1}s",
            self.config.device_name,
            self.config.max_duration.as_secs_f32()
        );
        let handle = AudioCapture::start_session(spec)?;
        self.capture = Some(handle);
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.capture.is_some()
    }

    /// Shared PCM buffer for snapshot-style readers. `None` when no
    /// session is active.
    pub fn buffer_arc(&self) -> Option<Arc<Mutex<PcmBuffer>>> {
        self.capture.as_ref().map(|c| c.buffer_arc())
    }

    /// Take a 16 kHz mono snapshot of the audio captured so far,
    /// optionally amplified by the session's `input_gain`. Used by
    /// the streaming pump to feed SenseVoice mid-utterance.
    pub fn snapshot_pcm_16k_mono(&self) -> Option<Vec<f32>> {
        let buffer = self.buffer_arc()?;
        let (samples, sample_rate, channels) = {
            let buf = buffer.lock().unwrap();
            if buf.duration_seconds() < 0.5 {
                return None;
            }
            (buf.interleaved.clone(), buf.sample_rate, buf.channels)
        };
        let mut mono = resample_to_16k_mono(&samples, sample_rate, channels).ok()?;
        let gain = self.config.input_gain.clamp(1.0, 30.0);
        if gain > 1.001 {
            for s in mono.iter_mut() {
                *s = (*s * gain).clamp(-1.0, 1.0);
            }
        }
        Some(mono)
    }

    /// Stop the capture, downmix + resample, optionally apply gain,
    /// then transcribe the whole buffer with the supplied SenseVoice
    /// engine. The engine is passed in (rather than owned) so the
    /// controller can defer engine loading without blocking the
    /// capture start path.
    pub fn finish(&mut self, engine: &SenseVoiceEngine) -> Result<String, AsrError> {
        let Some(handle) = self.capture.take() else {
            return Err(AsrError::Other("no active session".into()));
        };
        let audio = handle.stop()?;
        eprintln!(
            "[flowmux-asr] finish: total_samples={} duration={:.2}s rate={} ch={}",
            audio.pcm_16k_mono.len(),
            audio.duration_seconds,
            audio.original_sample_rate,
            audio.original_channels
        );
        let wav_path = std::env::temp_dir().join("flowmux-asr-last.wav");
        if let Err(e) = write_wav_16k_mono(&wav_path, &audio.pcm_16k_mono) {
            eprintln!("[flowmux-asr] dump wav failed: {e}");
        } else {
            eprintln!(
                "[flowmux-asr] dumped capture to {} ({} samples, 16kHz mono)",
                wav_path.display(),
                audio.pcm_16k_mono.len()
            );
        }
        if audio.duration_seconds < self.config.min_duration.as_secs_f32() {
            return Err(AsrError::Other(format!(
                "capture too short ({:.2}s)",
                audio.duration_seconds
            )));
        }
        // Refuse to call the recogniser on a captured-silence buffer.
        // SenseVoice hallucinates a fixed single-syllable string on
        // pure zeros ("그." for the multilingual checkpoint); leaving
        // that path enabled used to inject the same wrong text every
        // time the user's PulseAudio default source went mute.
        let peak = audio
            .pcm_16k_mono
            .iter()
            .map(|s| s.abs())
            .fold(0.0_f32, f32::max);
        let rms = if audio.pcm_16k_mono.is_empty() {
            0.0
        } else {
            (audio.pcm_16k_mono.iter().map(|s| s * s).sum::<f32>()
                / audio.pcm_16k_mono.len() as f32)
                .sqrt()
        };
        eprintln!(
            "[flowmux-asr] capture levels: peak={peak:.4} rms={rms:.4}"
        );
        if peak < 0.005 {
            return Err(AsrError::Other(
                "오디오 입력이 무음입니다. 옵션 → Voice input → 입력 장치에서 다른 마이크를 선택하거나 시스템 사운드 설정에서 입력 소스를 확인하세요.".into(),
            ));
        }
        let gain = self.config.input_gain.clamp(1.0, 30.0);
        let pcm: Vec<f32> = if gain > 1.001 {
            audio
                .pcm_16k_mono
                .iter()
                .map(|s| (s * gain).clamp(-1.0, 1.0))
                .collect()
        } else {
            audio.pcm_16k_mono.clone()
        };
        let text = engine.transcribe(16_000, &pcm);
        eprintln!("[flowmux-asr] final text: {text:?}");
        Ok(text)
    }

    pub fn cancel(&mut self) {
        if let Some(handle) = self.capture.take() {
            handle.abort();
        }
    }
}

/// Write 16 kHz mono f32 samples to a WAV file (i16 PCM).
fn write_wav_16k_mono(path: &std::path::Path, samples: &[f32]) -> std::io::Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|e| std::io::Error::other(format!("wav writer: {e}")))?;
    for s in samples {
        let clamped = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        writer
            .write_sample(clamped)
            .map_err(|e| std::io::Error::other(format!("wav write: {e}")))?;
    }
    writer
        .finalize()
        .map_err(|e| std::io::Error::other(format!("wav finalize: {e}")))?;
    Ok(())
}

/// Strip control bytes so an ANSI escape from noisy audio cannot
/// smuggle CSI into the shell.
pub fn sanitize_for_pty(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch == '\n' || ch == '\t' {
            out.push(ch);
        } else if (ch as u32) < 0x20 || ch == '\u{7f}' {
            continue;
        } else {
            out.push(ch);
        }
    }
    out
}

/// Strip whisper / sense-voice meta tokens (`[BLANK_AUDIO]`, `<|...|>`).
pub fn clean_asr_artifacts(input: &str) -> String {
    const META_TOKENS: &[&str] = &[
        "[BLANK_AUDIO]",
        "[blank_audio]",
        "[Music]",
        "[music]",
        "[Applause]",
        "(applause)",
        "[Inaudible]",
        "(inaudible)",
        "[Laughter]",
        "(laughter)",
        "[Noise]",
        "(noise)",
        "[_BEG_]",
    ];
    let mut stripped = input.to_string();
    for t in META_TOKENS {
        stripped = stripped.replace(t, "");
    }
    let mut cleaned = String::with_capacity(stripped.len());
    let mut chars = stripped.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' && chars.peek() == Some(&'|') {
            chars.next();
            let mut last = '\0';
            for inner in chars.by_ref() {
                if last == '|' && inner == '>' {
                    break;
                }
                last = inner;
            }
        } else {
            cleaned.push(c);
        }
    }
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_drops_escape_keeps_tab_newline() {
        let raw = "hello\t world\nrm -rf /\x1b]0;evil\x07";
        let out = sanitize_for_pty(raw);
        assert!(!out.contains('\x1b'));
        assert!(!out.contains('\x07'));
        assert!(out.contains('\t'));
        assert!(out.contains('\n'));
    }

    #[test]
    fn clean_asr_artifacts_removes_blank_audio_marker() {
        let raw = "[BLANK_AUDIO]hello world";
        assert_eq!(clean_asr_artifacts(raw), "hello world");
    }
}
