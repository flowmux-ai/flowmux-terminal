// SPDX-License-Identifier: GPL-3.0-or-later
//! Microphone capture loop.
//!
//! `cpal::Stream` is intentionally **not** `Send`, so the capture lives
//! on its own OS thread. The public side of the module exposes a small
//! [`CaptureHandle`] that the main thread keeps so it can stop the
//! session and read the resampled PCM back synchronously.
//!
//! Two design choices worth flagging:
//!
//! * Samples are accumulated in a [`PcmBuffer`] up to a wall-clock
//!   ceiling (default 30 s, matching the Whisper context window).
//! * Resampling to 16 kHz mono happens at stop time rather than inline
//!   in the audio callback, because rubato requires a contiguous chunk
//!   and the callback runs on a realtime thread we should not block.

use super::resample::resample_to_16k_mono;
use super::AudioError;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use crossbeam_channel::{bounded, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Configuration the GUI hands to [`AudioCapture::start_session`].
#[derive(Debug, Clone)]
pub struct CaptureSpec {
    /// `None` selects the host's default input. Specific device names
    /// match `cpal::Device::name()`.
    pub device_name: Option<String>,
    /// Hard cap on captured length. Whisper accepts ~30 s in one shot;
    /// anything longer is split into multiple sessions.
    pub max_duration: Duration,
}

impl Default for CaptureSpec {
    fn default() -> Self {
        Self {
            device_name: None,
            max_duration: Duration::from_secs(30),
        }
    }
}

/// Accumulated PCM buffer shared between the audio callback thread and
/// the stop handler. Wrapped in a `Mutex` because the callback fires
/// from a realtime thread and the stop handler reads from the main
/// thread.
#[derive(Debug, Default)]
pub struct PcmBuffer {
    pub interleaved: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
    pub truncated: bool,
}

impl PcmBuffer {
    pub fn duration_seconds(&self) -> f32 {
        if self.sample_rate == 0 || self.channels == 0 {
            return 0.0;
        }
        let frames = (self.interleaved.len() as u64) / (self.channels as u64);
        frames as f32 / self.sample_rate as f32
    }

    fn append(&mut self, samples: &[f32], max_frames: u64) {
        let max_samples = (max_frames * self.channels as u64) as usize;
        let space_left = max_samples.saturating_sub(self.interleaved.len());
        if space_left == 0 {
            self.truncated = true;
            return;
        }
        if samples.len() > space_left {
            self.interleaved.extend_from_slice(&samples[..space_left]);
            self.truncated = true;
        } else {
            self.interleaved.extend_from_slice(samples);
        }
    }
}

/// What [`CaptureHandle::stop`] returns once the session ends.
#[derive(Debug, Clone)]
pub struct CapturedAudio {
    pub pcm_16k_mono: Vec<f32>,
    pub original_sample_rate: u32,
    pub original_channels: u16,
    pub duration_seconds: f32,
    /// True when the wall-clock ceiling was reached and trailing audio
    /// was dropped. The UI surfaces this as a toast.
    pub truncated: bool,
}

/// Public capture facade. Stateless — every session runs on its own
/// thread and is owned by the returned [`CaptureHandle`].
pub struct AudioCapture;

impl AudioCapture {
    /// Build the input stream + play it + return a handle. The stream
    /// itself lives on the worker thread until `stop()` is called.
    pub fn start_session(spec: CaptureSpec) -> Result<CaptureHandle, AudioError> {
        let (stop_tx, stop_rx) = bounded::<StopReason>(1);
        let buffer = Arc::new(Mutex::new(PcmBuffer::default()));
        let buffer_clone = buffer.clone();

        let join = thread::Builder::new()
            .name("flowmux-asr-capture".into())
            .spawn(move || run_capture(spec, buffer_clone, stop_rx))
            .map_err(|e| AudioError::BuildStream(format!("spawn thread: {e}")))?;

        Ok(CaptureHandle {
            stop_tx,
            join: Some(join),
            buffer,
        })
    }
}

/// Reason supplied with the stop signal. Currently only `Finalise` is
/// used; `Abort` is reserved for the Esc-cancellation path that drops
/// the accumulated buffer.
#[derive(Debug, Clone, Copy)]
pub enum StopReason {
    Finalise,
    Abort,
}

pub struct CaptureHandle {
    stop_tx: Sender<StopReason>,
    join: Option<thread::JoinHandle<Result<(), AudioError>>>,
    buffer: Arc<Mutex<PcmBuffer>>,
}

impl CaptureHandle {
    /// Clone the shared PCM buffer so a periodic worker can take
    /// snapshots while the capture thread keeps appending to it. The
    /// worker locks the mutex only long enough to copy the current
    /// samples + sample-rate + channel layout out, then releases the
    /// lock so the audio callback is not stalled.
    pub fn buffer_arc(&self) -> Arc<Mutex<PcmBuffer>> {
        self.buffer.clone()
    }

    /// Signal the worker thread, join it, and resample the captured
    /// PCM down to 16 kHz mono.
    pub fn stop(mut self) -> Result<CapturedAudio, AudioError> {
        let _ = self.stop_tx.send(StopReason::Finalise);
        let result = self
            .join
            .take()
            .expect("join handle exists on first stop")
            .join()
            .map_err(|_| AudioError::Backend("capture thread panicked".into()))?;
        result?;
        let pcm = self.buffer.lock().unwrap();
        let mono = resample_to_16k_mono(&pcm.interleaved, pcm.sample_rate, pcm.channels)
            .map_err(AudioError::Backend)?;
        Ok(CapturedAudio {
            pcm_16k_mono: mono,
            original_sample_rate: pcm.sample_rate,
            original_channels: pcm.channels,
            duration_seconds: pcm.duration_seconds(),
            truncated: pcm.truncated,
        })
    }

    /// Drop the buffer without resampling. Used by the cancel
    /// (Esc / double-tap PTT) path.
    pub fn abort(mut self) {
        let _ = self.stop_tx.send(StopReason::Abort);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = self.stop_tx.send(StopReason::Abort);
            let _ = join.join();
        }
    }
}

fn run_capture(
    spec: CaptureSpec,
    buffer: Arc<Mutex<PcmBuffer>>,
    stop_rx: Receiver<StopReason>,
) -> Result<(), AudioError> {
    let host = cpal::default_host();
    let device = match &spec.device_name {
        None => host
            .default_input_device()
            .ok_or(AudioError::NoInputDevice)?,
        Some(name) => host
            .input_devices()
            .map_err(|e| AudioError::Backend(format!("enumerate devices: {e}")))?
            .find(|d| d.name().map(|n| &n == name).unwrap_or(false))
            .ok_or_else(|| AudioError::DeviceNotFound(name.clone()))?,
    };

    // Prefer mono i16 16 kHz when the device supports it — this is
    // the exact config `arecord -f S16_LE -r 16000 -c 1` succeeds
    // with on the same hardware. cpal's `default_input_config()`
    // often hands back stereo F32 44.1 kHz, which routes through
    // PulseAudio's plug chain and attenuates the captured signal on
    // some setups (VirtualBox AudioPCI in particular).
    let supported = pick_preferred_input_config(&device)
        .ok_or_else(|| AudioError::DefaultConfig("no supported input config".into()))?;
    let config = supported.config();
    let sample_format = supported.sample_format();
    let channels = config.channels;
    let sample_rate = config.sample_rate.0;
    let device_name = device
        .name()
        .unwrap_or_else(|_| "<unknown>".to_string());
    eprintln!(
        "[flowmux-asr] capture device={} rate={} ch={} format={:?}",
        device_name, sample_rate, channels, sample_format
    );

    {
        let mut b = buffer.lock().unwrap();
        b.sample_rate = sample_rate;
        b.channels = channels;
    }

    let max_frames = (spec.max_duration.as_secs_f64() * sample_rate as f64) as u64;
    let err_buf = buffer.clone();
    let err_fn = move |err| {
        tracing::warn!(target: "flowmux_asr::capture", "input stream error: {err}");
        let _ = err_buf;
    };

    let stream = match sample_format {
        SampleFormat::F32 => {
            build_stream_f32(&device, &config, buffer.clone(), max_frames, err_fn)?
        }
        SampleFormat::I16 => {
            build_stream_i16(&device, &config, buffer.clone(), max_frames, err_fn)?
        }
        SampleFormat::U16 => {
            build_stream_u16(&device, &config, buffer.clone(), max_frames, err_fn)?
        }
        other => {
            return Err(AudioError::BuildStream(format!(
                "unsupported sample format: {other:?}"
            )));
        }
    };

    stream
        .play()
        .map_err(|e| AudioError::PlayStream(e.to_string()))?;

    loop {
        match stop_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(StopReason::Finalise) | Ok(StopReason::Abort) => break,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                let frames =
                    (buffer.lock().unwrap().interleaved.len() as u64) / (channels.max(1) as u64);
                if frames >= max_frames {
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }

    drop(stream);
    Ok(())
}

/// Walk every supported input config and pick the one closest to
/// what the Korean Zipformer expects (mono / i16 / 16 kHz). Falls
/// back to whatever cpal's `default_input_config` would return when
/// no candidate is available, matching the previous behaviour.
fn pick_preferred_input_config(device: &cpal::Device) -> Option<cpal::SupportedStreamConfig> {
    use cpal::SampleRate;
    let Ok(configs) = device.supported_input_configs() else {
        return device.default_input_config().ok();
    };
    let configs: Vec<_> = configs.collect();
    let target_rate = 16_000_u32;
    let supports_rate = |cfg: &cpal::SupportedStreamConfigRange| {
        cfg.min_sample_rate().0 <= target_rate && cfg.max_sample_rate().0 >= target_rate
    };
    // Score: lower is better.
    //   +0 if channels==1 else +10
    //   +0 if format==I16 else +20 (other) / +5 (F32)
    //   +0 if 16 kHz is in range else +50
    let score = |cfg: &cpal::SupportedStreamConfigRange| -> i32 {
        let mut s = 0;
        if cfg.channels() != 1 {
            s += 10;
        }
        s += match cfg.sample_format() {
            SampleFormat::I16 => 0,
            SampleFormat::F32 => 5,
            _ => 20,
        };
        if !supports_rate(cfg) {
            s += 50;
        }
        s
    };
    let best = configs
        .iter()
        .min_by_key(|c| score(c))?
        .clone();
    let rate = if supports_rate(&best) {
        SampleRate(target_rate)
    } else {
        best.min_sample_rate()
    };
    Some(best.with_sample_rate(rate))
}

fn build_stream_f32(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    buffer: Arc<Mutex<PcmBuffer>>,
    max_frames: u64,
    err_fn: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, AudioError> {
    let data_buf = buffer.clone();
    device
        .build_input_stream(
            config,
            move |data: &[f32], _info: &cpal::InputCallbackInfo| {
                let mut b = data_buf.lock().unwrap();
                b.append(data, max_frames);
            },
            err_fn,
            None,
        )
        .map_err(|e| AudioError::BuildStream(e.to_string()))
}

fn build_stream_i16(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    buffer: Arc<Mutex<PcmBuffer>>,
    max_frames: u64,
    err_fn: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, AudioError> {
    let data_buf = buffer.clone();
    device
        .build_input_stream(
            config,
            move |data: &[i16], _info: &cpal::InputCallbackInfo| {
                let mut scratch = Vec::with_capacity(data.len());
                for s in data {
                    scratch.push(*s as f32 / i16::MAX as f32);
                }
                let mut b = data_buf.lock().unwrap();
                b.append(&scratch, max_frames);
            },
            err_fn,
            None,
        )
        .map_err(|e| AudioError::BuildStream(e.to_string()))
}

fn build_stream_u16(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    buffer: Arc<Mutex<PcmBuffer>>,
    max_frames: u64,
    err_fn: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, AudioError> {
    let data_buf = buffer.clone();
    device
        .build_input_stream(
            config,
            move |data: &[u16], _info: &cpal::InputCallbackInfo| {
                let mut scratch = Vec::with_capacity(data.len());
                for s in data {
                    let unit = *s as f32 / u16::MAX as f32;
                    scratch.push(unit * 2.0 - 1.0);
                }
                let mut b = data_buf.lock().unwrap();
                b.append(&scratch, max_frames);
            },
            err_fn,
            None,
        )
        .map_err(|e| AudioError::BuildStream(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_buffer_append_truncates_at_ceiling() {
        let mut buf = PcmBuffer {
            sample_rate: 16_000,
            channels: 1,
            ..Default::default()
        };
        let samples = vec![0.0_f32; 1024];
        // ceiling is 512 frames (mono).
        buf.append(&samples, 512);
        assert_eq!(buf.interleaved.len(), 512);
        assert!(buf.truncated);

        // subsequent appends are no-ops once truncated.
        buf.append(&samples, 512);
        assert_eq!(buf.interleaved.len(), 512);
    }

    #[test]
    fn pcm_buffer_duration_uses_sample_rate_and_channels() {
        let buf = PcmBuffer {
            sample_rate: 48_000,
            channels: 2,
            interleaved: vec![0.0; 48_000 * 2], // 1 s of stereo.
            ..Default::default()
        };
        assert!((buf.duration_seconds() - 1.0).abs() < 1e-3);
    }

    #[test]
    fn pcm_buffer_duration_zero_when_rate_or_channels_missing() {
        let buf = PcmBuffer::default();
        assert_eq!(buf.duration_seconds(), 0.0);
    }
}
