// SPDX-License-Identifier: GPL-3.0-or-later
//! flowmux 사용자 옵션 (전체 줌 + 새 탭브라우저 기본 웹뷰 엔진).
//!
//! 저장 위치: `$XDG_CONFIG_HOME/flowmux/options.json`. 모든 필드는
//! `#[serde(default)]`이라 사용자가 일부만 적어둬도 안전하게 로드된다.
//!
//! 줌은 정수 % (10..=200)이고, [`Options::zoom_factor`]가 GTK/VTE/
//! WebView가 받는 0.1..=2.0 배율을 돌려준다. 웹뷰 엔진은 옵션을
//! 바꿔도 이미 띄워진 탭브라우저에는 영향이 없고, 다음에 새로
//! 만드는 탭브라우저부터 사용된다.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 줌 % 하한.
pub const ZOOM_MIN: u16 = 10;
/// 줌 % 상한.
pub const ZOOM_MAX: u16 = 200;
/// 줌 % 기본값.
pub const ZOOM_DEFAULT: u16 = 100;

/// 새 탭브라우저를 만들 때 쓸 웹뷰 엔진. 현 단계에서는 모두
/// WebKitGTK로 fallback되며, 외부 엔진 spawn 분기는 다음 단계
/// 작업이다 — 그래도 사용자가 고른 값을 옵션 파일에 기록해 둬
/// 향후 연결만 하면 된다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserEngine {
    /// In-pane WebKitGTK (기본).
    Webkit,
    /// Chromium 계열.
    Chrome,
    /// Firefox 계열.
    Firefox,
    /// 사용자 정의 외부 엔진.
    Custom { name: String },
}

impl Default for BrowserEngine {
    fn default() -> Self {
        Self::Webkit
    }
}

impl BrowserEngine {
    /// 옵션 다이얼로그 / 디버그 로그용 사람-읽기 라벨.
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

    /// 드롭박스에 보일 빌트인 항목 순서.
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
}

fn default_zoom() -> u16 {
    ZOOM_DEFAULT
}

impl Default for Options {
    fn default() -> Self {
        Self {
            zoom_percent: ZOOM_DEFAULT,
            default_browser_engine: BrowserEngine::default(),
        }
    }
}

impl Options {
    /// `[ZOOM_MIN, ZOOM_MAX]`로 잘라낸 % 값.
    pub fn clamp_zoom(p: u16) -> u16 {
        p.clamp(ZOOM_MIN, ZOOM_MAX)
    }

    /// VTE `set_font_scale`, WebView `set_zoom_level`에 그대로 넘길
    /// 0.1..=2.0 배율.
    pub fn zoom_factor(&self) -> f64 {
        Self::clamp_zoom(self.zoom_percent) as f64 / 100.0
    }

    /// `with_zoom_percent(120)` 식으로 빌더 패턴 — 즉시 clamp.
    pub fn with_zoom_percent(mut self, p: u16) -> Self {
        self.zoom_percent = Self::clamp_zoom(p);
        self
    }

    /// 새 탭브라우저용 기본 엔진 교체.
    pub fn with_engine(mut self, engine: BrowserEngine) -> Self {
        self.default_browser_engine = engine;
        self
    }
}

/// `$XDG_CONFIG_HOME/flowmux/options.json` 경로. XDG dir 미해결이면
/// `None`.
pub fn options_path() -> Option<PathBuf> {
    crate::paths::config_dir().map(|d| d.join("options.json"))
}

/// 옵션 파일이 있으면 읽어 [`Options`]를 만들고, 없거나 깨졌으면
/// 기본값. zoom은 항상 clamp된 상태로 반환.
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
    Options {
        zoom_percent: Options::clamp_zoom(opts.zoom_percent),
        ..opts
    }
}

/// 옵션을 `options.json`에 직렬화. 부모 디렉터리가 없으면 만든다.
pub fn save(opts: &Options) -> std::io::Result<()> {
    let path = options_path()
        .ok_or_else(|| std::io::Error::other("XDG config dir unavailable"))?;
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
    use std::sync::{Mutex, OnceLock};

    fn xdg_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn with_xdg<R>(f: impl FnOnce(&std::path::Path) -> R) -> R {
        let _g = xdg_lock().lock().unwrap();
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
        assert_eq!(
            BrowserEngine::Custom {
                name: "".into()
            }
            .label(),
            "Custom"
        );
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
}
