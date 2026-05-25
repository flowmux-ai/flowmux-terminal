// SPDX-License-Identifier: GPL-3.0-or-later
//! 16 kHz mono `f32` is the only sample format Whisper accepts. The
//! resampler here is a thin wrapper around [`rubato::SincFixedIn`] that
//! exposes a chunk-friendly `process(...)` API.

use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

/// Whisper expects exactly this sample rate.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Downmix interleaved multi-channel samples to mono `f32`. Picks
/// the channel with the loudest absolute value per frame so a stereo
/// open with only one channel carrying actual mic data (common on
/// PulseAudio "default" sources that wrap a mono mic in a stereo
/// stream) does not halve the signal through averaging.
fn downmix_to_mono(interleaved: &[f32], channels: u16) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    let ch = channels as usize;
    let frames = interleaved.len() / ch;
    let mut out = Vec::with_capacity(frames);
    for f in 0..frames {
        let base = f * ch;
        let mut best = interleaved[base];
        let mut best_abs = best.abs();
        for c in 1..ch {
            let s = interleaved[base + c];
            let a = s.abs();
            if a > best_abs {
                best_abs = a;
                best = s;
            }
        }
        out.push(best);
    }
    out
}

/// Resample mono `f32` samples from `input_rate` Hz to 16 kHz. Returns
/// the input untouched when the rates already match — Whisper is fine
/// with that and the resampler adds latency.
fn resample_mono(mono: &[f32], input_rate: u32) -> Result<Vec<f32>, String> {
    if input_rate == TARGET_SAMPLE_RATE {
        return Ok(mono.to_vec());
    }
    if mono.is_empty() {
        return Ok(Vec::new());
    }

    let params = SincInterpolationParameters {
        sinc_len: 128,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 64,
        window: WindowFunction::BlackmanHarris2,
    };
    let ratio = TARGET_SAMPLE_RATE as f64 / input_rate as f64;
    let mut resampler = SincFixedIn::<f32>::new(ratio, 2.0, params, mono.len(), 1)
        .map_err(|e| format!("rubato build: {e}"))?;
    let waves_in = vec![mono.to_vec()];
    let waves_out = resampler
        .process(&waves_in, None)
        .map_err(|e| format!("rubato process: {e}"))?;
    Ok(waves_out.into_iter().next().unwrap_or_default())
}

/// One-shot helper: downmix to mono, then resample to 16 kHz.
pub fn resample_to_16k_mono(
    interleaved: &[f32],
    input_rate: u32,
    channels: u16,
) -> Result<Vec<f32>, String> {
    let mono = downmix_to_mono(interleaved, channels);
    resample_mono(&mono, input_rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_returns_input_untouched_for_mono() {
        let input = vec![0.1_f32, 0.2, 0.3];
        let out = downmix_to_mono(&input, 1);
        assert_eq!(out, input);
    }

    #[test]
    fn downmix_picks_loudest_channel_per_frame() {
        // First frame: L=1.0, R=-1.0 → both abs are equal so the
        // first non-strict-greater branch keeps `best = 1.0`.
        // Second frame: L=0.5, R=-0.5 → same magnitudes, keep L.
        let input = vec![1.0_f32, -1.0, 0.5, -0.5];
        let out = downmix_to_mono(&input, 2);
        assert_eq!(out, vec![1.0, 0.5]);
    }

    #[test]
    fn resample_passthrough_when_rates_match() {
        let input = vec![0.0_f32; 1024];
        let out = resample_mono(&input, TARGET_SAMPLE_RATE).unwrap();
        assert_eq!(out.len(), input.len());
    }

    #[test]
    fn resample_changes_length_for_different_rate() {
        let frames = 48000;
        let input = vec![0.0_f32; frames];
        let out = resample_mono(&input, 48_000).unwrap();
        // 48 kHz → 16 kHz is a 3× downsample; rubato can drift by a few
        // samples but should stay close to one-third the input length.
        let expected = frames / 3;
        let lo = expected.saturating_sub(64);
        let hi = expected + 64;
        assert!(
            (lo..=hi).contains(&out.len()),
            "got {} samples, expected near {}",
            out.len(),
            expected
        );
    }

    #[test]
    fn resample_to_16k_mono_handles_stereo_input() {
        let frames = 16_000;
        let mut interleaved = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            interleaved.push(0.5);
            interleaved.push(-0.5);
        }
        let out = resample_to_16k_mono(&interleaved, TARGET_SAMPLE_RATE, 2).unwrap();
        assert_eq!(out.len(), frames);
        for sample in out {
            // Loudest-channel pick keeps the full amplitude rather
            // than cancelling against the opposing channel.
            assert!(sample.abs() > 0.4, "expected loudest channel preserved");
        }
    }
}
