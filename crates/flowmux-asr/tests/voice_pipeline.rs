// SPDX-License-Identifier: GPL-3.0-or-later
//! Voice-pipeline integration tests against the SenseVoice engine.
//!
//! Lightweight tests (catalog, store, resampler, artifact stripping)
//! run unconditionally. The engine smoke test requires the catalog
//! default to be installed under `$XDG_DATA_HOME/flowmux/asr/models`;
//! when the model is missing the test prints a notice and exits
//! successfully so headless CI still passes.

use flowmux_asr::audio::resample::{resample_to_16k_mono, TARGET_SAMPLE_RATE};
use flowmux_asr::catalog;
use flowmux_asr::engine::load_engine;
use flowmux_asr::session::{clean_asr_artifacts, sanitize_for_pty};
use flowmux_asr::ModelStore;

#[test]
fn catalog_has_at_least_one_entry() {
    let entries = catalog::entries();
    assert!(!entries.is_empty());
    for entry in &entries {
        assert!(entry.archive_url.ends_with(".tar.bz2"));
        assert!(!entry.model_file.is_empty());
        assert!(!entry.tokens_file.is_empty());
    }
}

#[test]
fn store_install_detection_uses_catalog_filenames() {
    let tmp = tempfile::tempdir().unwrap();
    let store = ModelStore::new(tmp.path().join("models"));
    store.ensure_dir().unwrap();
    let entry = catalog::recommended_default();
    assert!(!store.is_installed(&entry));

    let dir = store.model_dir(&entry);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(&entry.model_file), b"m").unwrap();
    assert!(!store.is_installed(&entry));
    std::fs::write(dir.join(&entry.tokens_file), b"t").unwrap();
    assert!(store.is_installed(&entry));
}

#[test]
fn resampler_produces_16k_mono_from_stereo_48k() {
    let frames = 48_000;
    let mut interleaved = Vec::with_capacity(frames * 2);
    for _ in 0..frames {
        interleaved.push(0.0_f32);
        interleaved.push(0.0_f32);
    }
    let mono = resample_to_16k_mono(&interleaved, 48_000, 2).unwrap();
    let expected = TARGET_SAMPLE_RATE as usize;
    let lo = expected.saturating_sub(64);
    let hi = expected + 64;
    assert!(
        (lo..=hi).contains(&mono.len()),
        "expected ~{expected} samples, got {}",
        mono.len()
    );
}

#[test]
fn artifact_cleaner_strips_known_meta_tokens() {
    let raw = "안녕[BLANK_AUDIO]하세요 [Music]테스트";
    let cleaned = clean_asr_artifacts(raw);
    assert!(!cleaned.contains("[BLANK_AUDIO]"));
    assert!(!cleaned.contains("[Music]"));
    assert!(cleaned.contains("안녕"));
    assert!(cleaned.contains("하세요"));
    assert!(cleaned.contains("테스트"));
}

#[test]
fn sanitizer_drops_control_bytes() {
    let raw = "ls\x1b[2Jrm -rf";
    let cleaned = sanitize_for_pty(raw);
    assert!(!cleaned.contains('\x1b'));
    assert!(cleaned.starts_with("ls"));
}

/// End-to-end engine smoke test: loads SenseVoice + decodes a 1-s
/// silence buffer. Skips when the model has not been downloaded.
#[test]
fn engine_loads_and_decodes_silence_when_installed() {
    let Some(store) = ModelStore::xdg_default() else {
        eprintln!("skip: XDG data dir unavailable");
        return;
    };
    let entry = catalog::recommended_default();
    if !store.is_installed(&entry) {
        eprintln!(
            "skip: model {} not installed under {}",
            entry.id.as_str(),
            store.model_dir(&entry).display()
        );
        return;
    }

    let engine = load_engine(&entry, &store).expect("engine load");
    let silence = vec![0.0_f32; TARGET_SAMPLE_RATE as usize];
    let text = engine.transcribe(TARGET_SAMPLE_RATE, &silence);
    eprintln!("silence decoded text: {text:?}");
    // Empty or near-empty text is the expected outcome for silence;
    // we just confirm the engine returned without panicking.
}

/// If a previously-recorded capture is on disk at
/// `/tmp/flowmux-asr-last.wav` (the session dump path), run it
/// through the engine to verify the audio path is producing
/// recognisable speech. Skipped when the file is absent.
#[test]
fn engine_decodes_last_dumped_capture_when_present() {
    let path = std::env::temp_dir().join("flowmux-asr-last.wav");
    if !path.exists() {
        eprintln!("skip: no /tmp/flowmux-asr-last.wav yet");
        return;
    }
    let Some(store) = ModelStore::xdg_default() else {
        return;
    };
    let entry = catalog::recommended_default();
    if !store.is_installed(&entry) {
        return;
    }
    let mut reader = hound::WavReader::open(&path).expect("open wav");
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / i16::MAX as f32)
            .collect(),
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
    };
    let peak = samples.iter().map(|s| s.abs()).fold(0.0_f32, f32::max);
    eprintln!(
        "[dump] loaded {} samples ({} Hz) peak={:.3}",
        samples.len(),
        spec.sample_rate,
        peak
    );
    let engine = load_engine(&entry, &store).expect("engine load");
    let text = engine.transcribe(spec.sample_rate, &samples);
    eprintln!("[dump] decoded text: {text:?}");
}
