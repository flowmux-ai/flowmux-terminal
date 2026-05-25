// SPDX-License-Identifier: GPL-3.0-or-later
//! Voice-input (ASR / push-to-talk) options.
//!
//! Stored under the `asr` field of `options.json`. Every field defaults
//! sensibly so an existing options file from an older flowmux release
//! still loads — newly-added fields fall back to the defaults below.
//!
//! Wire types here are intentionally simple and serde-friendly; the
//! richer types used by `flowmux-asr` (model ids, language enums) are
//! constructed from these on demand so the config crate stays GTK-free
//! and dependency-light.

use serde::{Deserialize, Serialize};

/// Spoken language. `Auto` lets the engine guess, which works well for
/// multilingual Whisper checkpoints. A specific code (`ko`, `en`, `ja`,
/// …) is faster and a little more accurate when the user only speaks
/// one language.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum AsrLanguage {
    Auto,
    Code(String),
}

impl Default for AsrLanguage {
    fn default() -> Self {
        Self::Auto
    }
}

impl AsrLanguage {
    /// Two-letter wire form used by `flowmux-asr::config::Language`.
    pub fn as_code(&self) -> &str {
        match self {
            Self::Auto => "auto",
            Self::Code(s) => s.as_str(),
        }
    }
}

/// Voice-input options. All fields are `#[serde(default)]` through
/// [`Options`], so missing entries fall back to first-run defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AsrOptions {
    /// Master switch. When `false` the engine is not loaded and the
    /// shortcut is treated as unbound.
    #[serde(default)]
    pub enabled: bool,
    /// Id of the model the engine should load (matches the catalog in
    /// `flowmux-asr::catalog`). Empty until the user picks one.
    #[serde(default)]
    pub active_model_id: String,
    #[serde(default)]
    pub language: AsrLanguage,
    /// Append `Enter` to the injected text. Default off so the user
    /// can review the line before submitting.
    #[serde(default)]
    pub auto_enter: bool,
    /// `None` selects the host's default input device.
    #[serde(default)]
    pub input_device: Option<String>,
    /// Hard ceiling on captured length. Whisper accepts ~30 s in one
    /// shot. Stored in whole seconds for friendliness in `options.json`.
    #[serde(default = "default_max_seconds")]
    pub max_seconds: u16,
    /// Translate non-English speech to English. Whisper supports this
    /// natively.
    #[serde(default)]
    pub translate_to_english: bool,
    /// User has been shown the mic-permission explanation dialog at
    /// least once. Lets the options dialog suppress the auto-prompt on
    /// later launches.
    #[serde(default)]
    pub mic_permission_acknowledged: bool,
    /// Linear gain applied to captured audio before feeding into the
    /// recognizer. Default 1.0 (no change). Range 1.0..=10.0.
    /// KsponSpeech-trained streaming Zipformer expects samples near
    /// peak 0.9; a quiet mic (peak < 0.3) often needs a 3-5x boost
    /// for the model to emit tokens.
    #[serde(default = "default_input_gain")]
    pub input_gain: f32,
}

fn default_input_gain() -> f32 {
    1.0
}

fn default_max_seconds() -> u16 {
    30
}

impl Default for AsrOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            active_model_id: String::new(),
            language: AsrLanguage::Auto,
            auto_enter: false,
            input_device: None,
            max_seconds: default_max_seconds(),
            translate_to_english: false,
            mic_permission_acknowledged: false,
            input_gain: default_input_gain(),
        }
    }
}

impl AsrOptions {
    /// Clamp the wall-clock ceiling to a sane range. Anything below
    /// 5 s would make the feature feel broken; the upper bound matches
    /// the Whisper context window.
    pub fn clamp_max_seconds(p: u16) -> u16 {
        p.clamp(5, 120)
    }

    pub fn with_enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }

    pub fn with_active_model(mut self, id: impl Into<String>) -> Self {
        self.active_model_id = id.into();
        self
    }

    pub fn with_language(mut self, language: AsrLanguage) -> Self {
        self.language = language;
        self
    }

    pub fn with_auto_enter(mut self, on: bool) -> Self {
        self.auto_enter = on;
        self
    }

    pub fn with_input_device(mut self, device: Option<String>) -> Self {
        self.input_device = device;
        self
    }

    pub fn with_max_seconds(mut self, secs: u16) -> Self {
        self.max_seconds = Self::clamp_max_seconds(secs);
        self
    }

    pub fn with_translate(mut self, on: bool) -> Self {
        self.translate_to_english = on;
        self
    }

    pub fn with_mic_acknowledged(mut self, on: bool) -> Self {
        self.mic_permission_acknowledged = on;
        self
    }

    /// True if the engine is ready to use (toggle is on and a model has
    /// been picked). Note: the model file must also exist on disk; the
    /// GUI checks that through `flowmux-asr::store`.
    pub fn is_ready(&self) -> bool {
        self.enabled && !self.active_model_id.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_disabled_auto() {
        let opts = AsrOptions::default();
        assert!(!opts.enabled);
        assert!(opts.active_model_id.is_empty());
        assert_eq!(opts.language, AsrLanguage::Auto);
        assert!(!opts.auto_enter);
        assert_eq!(opts.max_seconds, 30);
        assert!(!opts.translate_to_english);
        assert!(!opts.mic_permission_acknowledged);
    }

    #[test]
    fn clamp_max_seconds_keeps_value_inside_range() {
        assert_eq!(AsrOptions::clamp_max_seconds(0), 5);
        assert_eq!(AsrOptions::clamp_max_seconds(4), 5);
        assert_eq!(AsrOptions::clamp_max_seconds(30), 30);
        assert_eq!(AsrOptions::clamp_max_seconds(200), 120);
    }

    #[test]
    fn is_ready_requires_enabled_and_model() {
        let opts = AsrOptions::default().with_enabled(true);
        assert!(!opts.is_ready());
        let opts = opts.with_active_model("ggml-small-q5_1");
        assert!(opts.is_ready());
    }

    #[test]
    fn language_serializes_as_kind_tagged_enum() {
        let auto = AsrLanguage::Auto;
        let s = serde_json::to_string(&auto).unwrap();
        assert!(s.contains("\"kind\":\"auto\""));

        let ko = AsrLanguage::Code("ko".into());
        let s = serde_json::to_string(&ko).unwrap();
        assert!(s.contains("\"kind\":\"code\""));
        assert!(s.contains("\"value\":\"ko\""));
    }

    #[test]
    fn language_as_code_returns_wire_string() {
        assert_eq!(AsrLanguage::Auto.as_code(), "auto");
        assert_eq!(AsrLanguage::Code("ko".into()).as_code(), "ko");
    }

    #[test]
    fn options_round_trip_via_serde() {
        let opts = AsrOptions::default()
            .with_enabled(true)
            .with_active_model("ggml-small-q5_1")
            .with_language(AsrLanguage::Code("ko".into()))
            .with_auto_enter(true)
            .with_input_device(Some("USB Mic".into()))
            .with_max_seconds(45)
            .with_translate(false)
            .with_mic_acknowledged(true);
        let s = serde_json::to_string(&opts).unwrap();
        let back: AsrOptions = serde_json::from_str(&s).unwrap();
        assert_eq!(opts, back);
    }

    #[test]
    fn empty_object_loads_as_default() {
        let opts: AsrOptions = serde_json::from_str("{}").unwrap();
        assert_eq!(opts, AsrOptions::default());
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let opts: AsrOptions = serde_json::from_str(r#"{"enabled": true}"#).unwrap();
        assert!(opts.enabled);
        assert_eq!(opts.max_seconds, 30);
        assert_eq!(opts.language, AsrLanguage::Auto);
    }
}
