// SPDX-License-Identifier: GPL-3.0-or-later
//! Microphone permission probe used by the options dialog.
//!
//! On Linux there is no explicit "request mic permission" API; opening
//! a capture stream is itself the permission grant. Flatpak adds a
//! portal layer that prompts the user the first time. Either way the
//! probe runs a short capture and reports back what happened, which
//! the dialog turns into a green check / red X / "open portal" message.

use super::capture::{AudioCapture, CaptureSpec};
use super::AudioError;
use std::time::Duration;

/// Outcome flavor surfaced to the UI. Compared to bubbling the raw
/// [`AudioError`] this gives the dialog enough structure to pick a
/// helpful localised label without leaking platform jargon.
#[derive(Debug)]
pub enum MicProbeOutcome {
    /// Capture started and stopped cleanly; permission is granted and
    /// the device produced samples.
    Ok {
        sample_rate: u32,
        channels: u16,
        captured_samples: usize,
    },
    /// The host had no input device at all (laptop with no internal
    /// mic, USB cable unplugged, etc.).
    NoDevice,
    /// The device exists but opening it failed in a way that smells
    /// like a permission denial — common when running in Flatpak
    /// without the audio socket exposed.
    PermissionDenied { detail: String },
    /// Other backend error (busy device, exotic format, ...).
    Failed { detail: String },
}

/// Run a short capture session (default 200 ms) and report the
/// outcome. Blocking: the caller wraps it in `gio::spawn_blocking`
/// (or a `std::thread::spawn`) so the GTK main loop stays responsive
/// while the capture thread runs.
///
/// Made sync on purpose — earlier revisions used `tokio::time::sleep`
/// which silently hangs when the function is invoked from
/// `glib::MainContext::spawn_local`, because that executor is not a
/// Tokio runtime.
pub fn probe_microphone(device_name: Option<String>) -> MicProbeOutcome {
    let spec = CaptureSpec {
        device_name,
        max_duration: Duration::from_millis(200),
    };
    let handle = match AudioCapture::start_session(spec) {
        Ok(h) => h,
        Err(err) => return classify(err),
    };

    // Let the capture thread run for the same window the spec asks for
    // — `stop()` joins the thread and resamples.
    std::thread::sleep(Duration::from_millis(250));

    match handle.stop() {
        Ok(audio) => MicProbeOutcome::Ok {
            sample_rate: audio.original_sample_rate,
            channels: audio.original_channels,
            captured_samples: audio.pcm_16k_mono.len(),
        },
        Err(err) => classify(err),
    }
}

fn classify(err: AudioError) -> MicProbeOutcome {
    match err {
        AudioError::NoInputDevice => MicProbeOutcome::NoDevice,
        AudioError::DeviceNotFound(name) => MicProbeOutcome::Failed {
            detail: format!("입력 장치 '{name}' 를 찾을 수 없습니다"),
        },
        AudioError::PermissionDenied => MicProbeOutcome::PermissionDenied {
            detail: "마이크 권한이 거부되었습니다".into(),
        },
        AudioError::BuildStream(detail) | AudioError::DefaultConfig(detail) => {
            if looks_like_permission_error(&detail) {
                MicProbeOutcome::PermissionDenied { detail }
            } else {
                MicProbeOutcome::Failed { detail }
            }
        }
        AudioError::PlayStream(detail) | AudioError::Backend(detail) => {
            if looks_like_permission_error(&detail) {
                MicProbeOutcome::PermissionDenied { detail }
            } else {
                MicProbeOutcome::Failed { detail }
            }
        }
    }
}

fn looks_like_permission_error(detail: &str) -> bool {
    let lc = detail.to_ascii_lowercase();
    lc.contains("permission")
        || lc.contains("denied")
        || lc.contains("not authoriz")
        || lc.contains("access")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_keywords_are_detected() {
        assert!(looks_like_permission_error("Permission denied"));
        assert!(looks_like_permission_error("Access not authorized"));
        assert!(looks_like_permission_error("not authorized"));
        assert!(!looks_like_permission_error("Unknown sample format f64"));
    }

    #[test]
    fn classify_no_device_maps_to_outcome() {
        let out = classify(AudioError::NoInputDevice);
        matches!(out, MicProbeOutcome::NoDevice);
    }

    #[test]
    fn classify_permission_phrase_routes_to_permission_denied() {
        let out = classify(AudioError::BuildStream("Permission denied".into()));
        assert!(matches!(out, MicProbeOutcome::PermissionDenied { .. }));
    }
}
