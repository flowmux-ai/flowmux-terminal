// SPDX-License-Identifier: GPL-3.0-or-later
//! flowmux user options: global zoom and default web view engine for new
//! browser tabs.
//!
//! Stored at `$XDG_CONFIG_HOME/flowmux/options.json`. All fields use
//! `#[serde(default)]`, so partial user files load safely.
//!
//! Zoom is an integer percentage (10..=200), and [`Options::zoom_factor`]
//! returns the 0.1..=2.0 scale accepted by GTK/terminal/WebView. Changing the web
//! view engine option does not affect existing browser tabs; it applies only
//! to newly created browser tabs.

use crate::keybindings::KeybindingOverrides;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Minimum zoom percentage.
pub const ZOOM_MIN: u16 = 10;
/// Maximum zoom percentage.
pub const ZOOM_MAX: u16 = 200;
/// Default zoom percentage.
pub const ZOOM_DEFAULT: u16 = 100;

/// Default 1px border color for the focused pane: pale yellow (Champagne).
/// Chosen to stay visible on both dark and light themes while keeping the
/// highlight subtle and distinct from cmux.
pub const FOCUS_BORDER_COLOR_DEFAULT: &str = "#fff4b3";

/// Focus border opacity (`0..=100` %). 100 = fully opaque, 0 = fully transparent.
/// The default is 30% so the focus highlight is visible without dominating the
/// surrounding pane chrome on first launch.
pub const FOCUS_BORDER_OPACITY_MIN: u8 = 0;
pub const FOCUS_BORDER_OPACITY_MAX: u8 = 100;
pub const FOCUS_BORDER_OPACITY_DEFAULT: u8 = 30;

/// Default for [`Options::persist_browser_session`]. The user expectation
/// modeled after every mainstream browser is that signing into a site once
/// and quitting flowmux still leaves the user signed in on the next launch,
/// so the option ships enabled.
pub const PERSIST_BROWSER_SESSION_DEFAULT: bool = true;

/// Default for [`Options::system_notifications_enabled`]. Desktop toasts ship
/// enabled so flowmux behaves like every other notifying app on first launch;
/// the user can opt out to keep only the in-app bell list.
pub const SYSTEM_NOTIFICATIONS_ENABLED_DEFAULT: bool = true;

/// Default for [`Options::cursor_blink`]. The terminal cursor blinks on first
/// launch, matching VTE / most terminals.
pub const CURSOR_BLINK_DEFAULT: bool = true;

/// Cursor blink half-period in milliseconds: the time the cursor stays shown
/// before toggling to hidden (and vice versa). Range clamps to
/// `[CURSOR_BLINK_INTERVAL_MIN, CURSOR_BLINK_INTERVAL_MAX]`. The default 530ms
/// matches GTK's historical `gtk-cursor-blink-time` (1060ms full period).
pub const CURSOR_BLINK_INTERVAL_MIN: u32 = 100;
pub const CURSOR_BLINK_INTERVAL_MAX: u32 = 2000;
pub const CURSOR_BLINK_INTERVAL_DEFAULT: u32 = 530;

/// Web view engine to use for new browser tabs. At this stage every variant
/// falls back to WebKitGTK; external engine spawning is a later step. The
/// selected value is still persisted so the future wiring can use it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserEngine {
    /// In-pane WebKitGTK (default).
    #[default]
    Webkit,
    /// Chromium family.
    Chrome,
    /// Firefox family.
    Firefox,
    /// User-defined external engine.
    Custom { name: String },
}

impl BrowserEngine {
    /// Human-readable label for the options dialog and debug logs.
    pub fn label(&self) -> String {
        match self {
            Self::Webkit => "WebKit".into(),
            Self::Chrome => "Chrome".into(),
            Self::Firefox => "Firefox".into(),
            Self::Custom { name } => {
                if name.is_empty() {
                    "Custom".into()
                } else {
                    name.clone()
                }
            }
        }
    }

    /// Built-in item order shown in the drop-down.
    pub fn builtin_order() -> [Self; 3] {
        [Self::Webkit, Self::Chrome, Self::Firefox]
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Options {
    #[serde(default = "default_zoom")]
    pub zoom_percent: u16,
    #[serde(default)]
    pub default_browser_engine: BrowserEngine,
    /// Focused-pane 1px border color in CSS form, usually `#rrggbb`.
    /// The color selected from the options dialog is stored as-is; invalid or
    /// empty values fall back to [`FOCUS_BORDER_COLOR_DEFAULT`].
    #[serde(default = "default_focus_color")]
    pub focus_border_color: String,
    /// Focus border opacity (%: `0..=100`). 100 is opaque, 0 is transparent.
    /// Selected by the options dialog slider; out-of-range values are clamped
    /// by [`Options::clamp_focus_border_opacity`].
    #[serde(default = "default_focus_border_opacity")]
    pub focus_border_opacity: u8,
    /// When true, browser tabs persist cookies, localStorage, IndexedDB, and
    /// related site data across flowmux restarts so logins and other site
    /// state survive a quit/relaunch. When false, the WebKit network session
    /// is built ephemerally and every launch starts signed-out.
    /// Default: [`PERSIST_BROWSER_SESSION_DEFAULT`] (`true`).
    #[serde(default = "default_persist_browser_session")]
    pub persist_browser_session: bool,
    /// When true, notifications are delivered as system desktop toasts
    /// (libnotify / D-Bus) in addition to the in-app bell list. When false,
    /// notifications still appear in the in-app bell list but no system toast
    /// is sent. Default: [`SYSTEM_NOTIFICATIONS_ENABLED_DEFAULT`] (`true`).
    #[serde(default = "default_system_notifications_enabled")]
    pub system_notifications_enabled: bool,
    /// When true, the terminal cursor blinks. Default:
    /// [`CURSOR_BLINK_DEFAULT`] (`true`).
    #[serde(default = "default_cursor_blink")]
    pub cursor_blink: bool,
    /// Cursor blink half-period in milliseconds (time shown before toggling).
    /// Clamped to `[CURSOR_BLINK_INTERVAL_MIN, CURSOR_BLINK_INTERVAL_MAX]` by
    /// [`Options::clamp_cursor_blink_interval`]. Default:
    /// [`CURSOR_BLINK_INTERVAL_DEFAULT`].
    #[serde(default = "default_cursor_blink_interval")]
    pub cursor_blink_interval_ms: u32,
    /// Terminal font family selected in the options dialog. `None` means
    /// "inherit the resolved theme font" (the `font-family` from the theme
    /// file, or the built-in `monospace` fallback). A `Some` value overrides
    /// the theme font for all terminals live.
    #[serde(default)]
    pub font_family: Option<String>,
    /// Terminal font size in points. `None` inherits the resolved theme size
    /// (the theme file's `font-size`, or the built-in 12pt fallback).
    #[serde(default)]
    pub font_size: Option<f32>,
    /// User overrides for keyboard shortcuts. Partial overlay over the
    /// built-in defaults exposed by
    /// [`crate::keybindings::defaults`] — actions absent from this map
    /// keep their defaults, and an empty accel array marks an action as
    /// explicitly unbound. Unknown keys are dropped at install time
    /// with a warning so a typo in `options.json` does not break the
    /// rest of the config.
    #[serde(default)]
    pub keybindings: KeybindingOverrides,
}

fn default_zoom() -> u16 {
    ZOOM_DEFAULT
}

fn default_focus_color() -> String {
    FOCUS_BORDER_COLOR_DEFAULT.to_string()
}

fn default_focus_border_opacity() -> u8 {
    FOCUS_BORDER_OPACITY_DEFAULT
}

fn default_persist_browser_session() -> bool {
    PERSIST_BROWSER_SESSION_DEFAULT
}

fn default_system_notifications_enabled() -> bool {
    SYSTEM_NOTIFICATIONS_ENABLED_DEFAULT
}

fn default_cursor_blink() -> bool {
    CURSOR_BLINK_DEFAULT
}

fn default_cursor_blink_interval() -> u32 {
    CURSOR_BLINK_INTERVAL_DEFAULT
}

impl Default for Options {
    fn default() -> Self {
        Self {
            zoom_percent: ZOOM_DEFAULT,
            default_browser_engine: BrowserEngine::default(),
            focus_border_color: default_focus_color(),
            focus_border_opacity: default_focus_border_opacity(),
            persist_browser_session: default_persist_browser_session(),
            system_notifications_enabled: default_system_notifications_enabled(),
            cursor_blink: default_cursor_blink(),
            cursor_blink_interval_ms: default_cursor_blink_interval(),
            font_family: None,
            font_size: None,
            keybindings: KeybindingOverrides::default(),
        }
    }
}

impl Options {
    /// Percentage clamped to `[ZOOM_MIN, ZOOM_MAX]`.
    pub fn clamp_zoom(p: u16) -> u16 {
        p.clamp(ZOOM_MIN, ZOOM_MAX)
    }

    /// Blink interval clamped to
    /// `[CURSOR_BLINK_INTERVAL_MIN, CURSOR_BLINK_INTERVAL_MAX]` ms.
    pub fn clamp_cursor_blink_interval(ms: u32) -> u32 {
        ms.clamp(CURSOR_BLINK_INTERVAL_MIN, CURSOR_BLINK_INTERVAL_MAX)
    }

    /// Scale in 0.1..=2.0 form for terminal `set_font_scale` and WebView
    /// `set_zoom_level`.
    pub fn zoom_factor(&self) -> f64 {
        Self::clamp_zoom(self.zoom_percent) as f64 / 100.0
    }

    /// Builder-style setter such as `with_zoom_percent(120)`, clamped immediately.
    pub fn with_zoom_percent(mut self, p: u16) -> Self {
        self.zoom_percent = Self::clamp_zoom(p);
        self
    }

    /// Replace the default engine for new browser tabs.
    pub fn with_engine(mut self, engine: BrowserEngine) -> Self {
        self.default_browser_engine = engine;
        self
    }

    /// Set a new focus border color. Invalid values, including empty strings,
    /// missing `#`, or non-hex content, fall back to the default color.
    pub fn with_focus_border_color(mut self, color: impl Into<String>) -> Self {
        let color = color.into();
        self.focus_border_color = if is_valid_hex_color(&color) {
            color
        } else {
            FOCUS_BORDER_COLOR_DEFAULT.to_string()
        };
        self
    }

    /// Same validation used during load-time sanitization.
    pub fn focus_border_color_or_default(&self) -> &str {
        if is_valid_hex_color(&self.focus_border_color) {
            &self.focus_border_color
        } else {
            FOCUS_BORDER_COLOR_DEFAULT
        }
    }

    /// Focus border opacity percentage clamped to 0..=100.
    pub fn clamp_focus_border_opacity(p: u8) -> u8 {
        p.clamp(FOCUS_BORDER_OPACITY_MIN, FOCUS_BORDER_OPACITY_MAX)
    }

    /// Alpha value in 0.0..=1.0 form for CSS rgba().
    pub fn focus_border_alpha(&self) -> f32 {
        Self::clamp_focus_border_opacity(self.focus_border_opacity) as f32 / 100.0
    }

    /// Builder-style setter, clamped immediately.
    pub fn with_focus_border_opacity(mut self, p: u8) -> Self {
        self.focus_border_opacity = Self::clamp_focus_border_opacity(p);
        self
    }

    /// Builder-style setter for the browser-session persistence flag.
    pub fn with_persist_browser_session(mut self, persist: bool) -> Self {
        self.persist_browser_session = persist;
        self
    }

    /// Builder-style setter for the system-notification (desktop toast) flag.
    pub fn with_system_notifications_enabled(mut self, enabled: bool) -> Self {
        self.system_notifications_enabled = enabled;
        self
    }

    /// Builder-style setter for the terminal font family. An empty string
    /// resets to `None` (inherit the theme font).
    pub fn with_font_family(mut self, family: Option<String>) -> Self {
        self.font_family = family.filter(|s| !s.trim().is_empty());
        self
    }

    /// Builder-style setter for the terminal font size in points.
    pub fn with_font_size(mut self, size: Option<f32>) -> Self {
        self.font_size = size;
        self
    }
}

/// Check whether a string is an allowed hex CSS color form:
///   `#rgb` / `#rgba` / `#rrggbb` / `#rrggbbaa`
/// Other forms, such as rgba() or color names, are rejected conservatively so
/// option files fall back to the default.
pub fn is_valid_hex_color(s: &str) -> bool {
    let Some(body) = s.strip_prefix('#') else {
        return false;
    };
    matches!(body.len(), 3 | 4 | 6 | 8) && body.chars().all(|c| c.is_ascii_hexdigit())
}

/// Path to `$XDG_CONFIG_HOME/flowmux/options.json`, or `None` when the XDG dir
/// cannot be resolved.
pub fn options_path() -> Option<PathBuf> {
    crate::paths::config_dir().map(|d| d.join("options.json"))
}

/// Load [`Options`] from the options file. Missing or corrupt files return
/// defaults. Zoom is always returned clamped.
pub fn load() -> Options {
    let Some(path) = options_path() else {
        return Options::default();
    };
    let Ok(s) = std::fs::read_to_string(&path) else {
        return Options::default();
    };
    let opts: Options = match serde_json::from_str(&s) {
        Ok(o) => o,
        Err(_) => return Options::default(),
    };
    let focus_border_color = if is_valid_hex_color(&opts.focus_border_color) {
        opts.focus_border_color
    } else {
        FOCUS_BORDER_COLOR_DEFAULT.to_string()
    };
    Options {
        zoom_percent: Options::clamp_zoom(opts.zoom_percent),
        focus_border_color,
        focus_border_opacity: Options::clamp_focus_border_opacity(opts.focus_border_opacity),
        ..opts
    }
}

/// Serialize options to `options.json`, creating the parent directory if needed.
pub fn save(opts: &Options) -> std::io::Result<()> {
    let path = options_path().ok_or_else(|| std::io::Error::other("XDG config dir unavailable"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let s = serde_json::to_string_pretty(opts)
        .map_err(|e| std::io::Error::other(format!("serialize options: {e}")))?;
    std::fs::write(path, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_xdg<R>(f: impl FnOnce(&std::path::Path) -> R) -> R {
        let _g = crate::test_env::env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        let result = f(tmp.path());
        match prev {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        result
    }

    #[test]
    fn clamp_zoom_keeps_value_inside_range() {
        assert_eq!(Options::clamp_zoom(0), ZOOM_MIN);
        assert_eq!(Options::clamp_zoom(5), ZOOM_MIN);
        assert_eq!(Options::clamp_zoom(10), 10);
        assert_eq!(Options::clamp_zoom(100), 100);
        assert_eq!(Options::clamp_zoom(200), 200);
        assert_eq!(Options::clamp_zoom(500), ZOOM_MAX);
    }

    #[test]
    fn zoom_factor_is_percent_over_one_hundred() {
        let opts = Options::default().with_zoom_percent(125);
        assert!((opts.zoom_factor() - 1.25).abs() < 1e-9);
        let opts = Options::default().with_zoom_percent(50);
        assert!((opts.zoom_factor() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn with_zoom_percent_clamps_immediately() {
        let opts = Options::default().with_zoom_percent(0);
        assert_eq!(opts.zoom_percent, ZOOM_MIN);
        let opts = Options::default().with_zoom_percent(9999);
        assert_eq!(opts.zoom_percent, ZOOM_MAX);
    }

    #[test]
    fn defaults_are_webkit_at_one_hundred_percent() {
        let opts = Options::default();
        assert_eq!(opts.zoom_percent, 100);
        assert_eq!(opts.default_browser_engine, BrowserEngine::Webkit);
    }

    #[test]
    fn engine_label_falls_back_for_empty_custom() {
        assert_eq!(BrowserEngine::Webkit.label(), "WebKit");
        assert_eq!(BrowserEngine::Custom { name: "".into() }.label(), "Custom");
        assert_eq!(
            BrowserEngine::Custom {
                name: "Brave".into()
            }
            .label(),
            "Brave"
        );
    }

    #[test]
    fn engine_serde_roundtrip_for_each_variant() {
        for e in [
            BrowserEngine::Webkit,
            BrowserEngine::Chrome,
            BrowserEngine::Firefox,
            BrowserEngine::Custom {
                name: "Brave".into(),
            },
        ] {
            let s = serde_json::to_string(&e).unwrap();
            let back: BrowserEngine = serde_json::from_str(&s).unwrap();
            assert_eq!(e, back);
        }
    }

    #[test]
    fn engine_uses_snake_case_kind_tag() {
        let s = serde_json::to_string(&BrowserEngine::Webkit).unwrap();
        assert!(s.contains("\"kind\":\"webkit\""));
        let s = serde_json::to_string(&BrowserEngine::Chrome).unwrap();
        assert!(s.contains("\"kind\":\"chrome\""));
    }

    #[test]
    fn options_serde_roundtrip_with_custom_engine() {
        let opts = Options::default()
            .with_zoom_percent(140)
            .with_engine(BrowserEngine::Custom {
                name: "Brave".into(),
            });
        let s = serde_json::to_string(&opts).unwrap();
        let back: Options = serde_json::from_str(&s).unwrap();
        assert_eq!(opts, back);
    }

    #[test]
    fn options_load_falls_back_to_defaults_when_file_absent() {
        with_xdg(|_| {
            let opts = load();
            assert_eq!(opts, Options::default());
        });
    }

    #[test]
    fn options_load_falls_back_to_defaults_on_corrupt_json() {
        with_xdg(|root| {
            let path = root.join("flowmux").join("options.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "{not json").unwrap();
            let opts = load();
            assert_eq!(opts, Options::default());
        });
    }

    #[test]
    fn options_load_clamps_out_of_range_zoom() {
        with_xdg(|root| {
            let path = root.join("flowmux").join("options.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                r#"{"zoom_percent": 500, "default_browser_engine": {"kind": "webkit"}}"#,
            )
            .unwrap();
            let opts = load();
            assert_eq!(opts.zoom_percent, ZOOM_MAX);
        });
    }

    #[test]
    fn options_save_then_load_preserves_values() {
        with_xdg(|_| {
            let opts = Options::default()
                .with_zoom_percent(125)
                .with_engine(BrowserEngine::Firefox);
            save(&opts).unwrap();
            let back = load();
            assert_eq!(opts, back);
        });
    }

    #[test]
    fn options_path_under_flowmux_subtree() {
        with_xdg(|root| {
            let path = options_path().unwrap();
            assert!(path.starts_with(root));
            assert!(path.ends_with("flowmux/options.json"));
        });
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let opts: Options = serde_json::from_str("{}").unwrap();
        assert_eq!(opts, Options::default());
    }

    #[test]
    fn default_focus_border_color_is_pale_yellow() {
        assert_eq!(Options::default().focus_border_color, "#fff4b3");
    }

    #[test]
    fn is_valid_hex_color_accepts_known_lengths() {
        assert!(is_valid_hex_color("#abc"));
        assert!(is_valid_hex_color("#abcd"));
        assert!(is_valid_hex_color("#aabbcc"));
        assert!(is_valid_hex_color("#aabbccdd"));
        assert!(is_valid_hex_color("#FFF4B3"));
    }

    #[test]
    fn is_valid_hex_color_rejects_other_formats() {
        assert!(!is_valid_hex_color(""));
        assert!(!is_valid_hex_color("#"));
        assert!(!is_valid_hex_color("#g00"));
        assert!(!is_valid_hex_color("#12345"));
        assert!(!is_valid_hex_color("rgb(255,0,0)"));
        assert!(!is_valid_hex_color("yellow"));
    }

    #[test]
    fn with_focus_border_color_falls_back_for_invalid_input() {
        let opts = Options::default().with_focus_border_color("not-a-color");
        assert_eq!(opts.focus_border_color, "#fff4b3");

        let opts = Options::default().with_focus_border_color("#deadbe");
        assert_eq!(opts.focus_border_color, "#deadbe");
    }

    #[test]
    fn focus_border_color_or_default_protects_callers() {
        let opts = Options {
            focus_border_color: "garbage".into(),
            ..Default::default()
        };
        assert_eq!(opts.focus_border_color_or_default(), "#fff4b3");
    }

    #[test]
    fn options_load_sanitizes_corrupt_focus_color() {
        with_xdg(|root| {
            let path = root.join("flowmux").join("options.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                r#"{"zoom_percent": 100, "default_browser_engine": {"kind": "webkit"}, "focus_border_color": "blueish"}"#,
            )
            .unwrap();
            let opts = load();
            assert_eq!(opts.focus_border_color, "#fff4b3");
        });
    }

    #[test]
    fn options_save_then_load_preserves_focus_color() {
        with_xdg(|_| {
            let opts = Options::default().with_focus_border_color("#0bd968");
            save(&opts).unwrap();
            let back = load();
            assert_eq!(back.focus_border_color, "#0bd968");
        });
    }

    // ===== focus_border_opacity =====

    #[test]
    fn default_focus_border_opacity_is_thirty() {
        // First-run default is 30% so the highlight is visible without
        // overpowering surrounding pane chrome.
        assert_eq!(
            Options::default().focus_border_opacity,
            FOCUS_BORDER_OPACITY_DEFAULT
        );
        assert_eq!(FOCUS_BORDER_OPACITY_DEFAULT, 30);
        assert!((Options::default().focus_border_alpha() - 0.3).abs() < 1e-6);
    }

    #[test]
    fn clamp_focus_border_opacity_keeps_value_inside_range() {
        // u8 cannot be below 0, but values above 100 are clamped.
        assert_eq!(Options::clamp_focus_border_opacity(0), 0);
        assert_eq!(Options::clamp_focus_border_opacity(100), 100);
        assert_eq!(Options::clamp_focus_border_opacity(101), 100);
        assert_eq!(Options::clamp_focus_border_opacity(255), 100);
    }

    #[test]
    fn focus_border_alpha_is_percent_over_one_hundred() {
        let opts = Options::default().with_focus_border_opacity(50);
        assert!((opts.focus_border_alpha() - 0.5).abs() < 1e-6);
        let opts = Options::default().with_focus_border_opacity(0);
        assert!(opts.focus_border_alpha().abs() < 1e-6);
    }

    #[test]
    fn with_focus_border_opacity_clamps_immediately() {
        let opts = Options::default().with_focus_border_opacity(200);
        assert_eq!(opts.focus_border_opacity, 100);
    }

    #[test]
    fn options_load_clamps_out_of_range_opacity() {
        with_xdg(|root| {
            let path = root.join("flowmux").join("options.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                r##"{"zoom_percent": 100, "default_browser_engine": {"kind": "webkit"}, "focus_border_color": "#fff4b3", "focus_border_opacity": 250}"##,
            )
            .unwrap();
            let opts = load();
            assert_eq!(opts.focus_border_opacity, 100);
        });
    }

    #[test]
    fn options_load_falls_back_to_default_opacity_when_field_missing() {
        with_xdg(|root| {
            let path = root.join("flowmux").join("options.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                r##"{"zoom_percent": 100, "default_browser_engine": {"kind": "webkit"}, "focus_border_color": "#fff4b3"}"##,
            )
            .unwrap();
            let opts = load();
            assert_eq!(opts.focus_border_opacity, FOCUS_BORDER_OPACITY_DEFAULT);
        });
    }

    #[test]
    fn options_save_then_load_preserves_opacity() {
        with_xdg(|_| {
            let opts = Options::default().with_focus_border_opacity(35);
            save(&opts).unwrap();
            let back = load();
            assert_eq!(back.focus_border_opacity, 35);
        });
    }

    // ===== persist_browser_session =====

    #[test]
    fn default_persist_browser_session_is_true() {
        // The user expectation modeled after every mainstream browser is to
        // stay signed in across quit/relaunch, so the option ships enabled.
        assert!(Options::default().persist_browser_session);
        assert_eq!(
            Options::default().persist_browser_session,
            PERSIST_BROWSER_SESSION_DEFAULT
        );
    }

    #[test]
    fn with_persist_browser_session_sets_flag() {
        let opts = Options::default().with_persist_browser_session(false);
        assert!(!opts.persist_browser_session);
        let opts = opts.with_persist_browser_session(true);
        assert!(opts.persist_browser_session);
    }

    #[test]
    fn options_load_falls_back_to_default_persist_when_field_missing() {
        with_xdg(|root| {
            let path = root.join("flowmux").join("options.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            // Older option files predate the new flag — load() must still
            // return a sane default rather than failing or silently disabling
            // session persistence.
            std::fs::write(
                &path,
                r##"{"zoom_percent": 100, "default_browser_engine": {"kind": "webkit"}, "focus_border_color": "#fff4b3", "focus_border_opacity": 50}"##,
            )
            .unwrap();
            let opts = load();
            assert!(opts.persist_browser_session);
        });
    }

    #[test]
    fn options_save_then_load_preserves_persist_browser_session_false() {
        with_xdg(|_| {
            let opts = Options::default().with_persist_browser_session(false);
            save(&opts).unwrap();
            let back = load();
            assert!(!back.persist_browser_session);
        });
    }

    #[test]
    fn options_save_then_load_preserves_persist_browser_session_true() {
        with_xdg(|_| {
            let opts = Options::default().with_persist_browser_session(true);
            save(&opts).unwrap();
            let back = load();
            assert!(back.persist_browser_session);
        });
    }

    #[test]
    fn empty_object_deserializes_persist_browser_session_default() {
        let opts: Options = serde_json::from_str("{}").unwrap();
        assert!(opts.persist_browser_session);
    }

    // ===== font_family / font_size =====

    #[test]
    fn default_font_is_inherited_from_theme() {
        let opts = Options::default();
        assert_eq!(opts.font_family, None);
        assert_eq!(opts.font_size, None);
    }

    #[test]
    fn with_font_family_drops_empty_to_none() {
        assert_eq!(
            Options::default()
                .with_font_family(Some("JetBrains Mono".into()))
                .font_family,
            Some("JetBrains Mono".into())
        );
        assert_eq!(
            Options::default()
                .with_font_family(Some("  ".into()))
                .font_family,
            None
        );
    }

    #[test]
    fn options_save_then_load_preserves_font_overrides() {
        with_xdg(|_| {
            let opts = Options::default()
                .with_font_family(Some("Fira Code".into()))
                .with_font_size(Some(14.0));
            save(&opts).unwrap();
            let back = load();
            assert_eq!(back.font_family, Some("Fira Code".into()));
            assert_eq!(back.font_size, Some(14.0));
        });
    }

    #[test]
    fn missing_font_fields_load_as_none() {
        let opts: Options = serde_json::from_str(r#"{"zoom_percent": 100}"#).unwrap();
        assert_eq!(opts.font_family, None);
        assert_eq!(opts.font_size, None);
    }

    #[test]
    fn explicit_false_persist_browser_session_round_trips_via_serde() {
        let opts = Options::default().with_persist_browser_session(false);
        let s = serde_json::to_string(&opts).unwrap();
        assert!(s.contains("\"persist_browser_session\":false"));
        let back: Options = serde_json::from_str(&s).unwrap();
        assert_eq!(opts, back);
    }
}
