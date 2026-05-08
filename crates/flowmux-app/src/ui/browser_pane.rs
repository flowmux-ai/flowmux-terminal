// SPDX-License-Identifier: GPL-3.0-or-later
//! WebKitGTK 6.0 in-app browser pane.
//!
//! Replaces the macOS WKWebView path. Each pane owns:
//!
//! * a [`webkit::WebView`] for rendering;
//! * a small chrome row (back / forward / reload / address bar);
//! * a scriptable API entry point — `evaluate_javascript_async` is
//!   already exposed by webkit6, so the Task 15 work mostly involves
//!   wrapping it in a stable IPC verb shape, not new widgets.
//!
//! 옵션 모델: cmux 원본은 단일 엔진(WKWebView) + 프로필별
//! `WKWebsiteDataStore` 분리만 합니다 (`Sources/Panels/BrowserPanel.swift:443`).
//! flowmux도 같은 모델을 따라 WebKitGTK 6.0 한 엔진으로만 그리고,
//! 옵션의 엔진 라벨(WebKit / Chrome / Firefox / Custom)은
//! [`BrowserProfile`]로 매핑되어 cookies / localStorage / IndexedDB
//! 디렉토리를 분리한다.

use crate::ui::terminal_pane::PaneCallbacks;
use flowmux_browser::BrowserProfile;
use flowmux_config::options::BrowserEngine;
use flowmux_core::{PaneId, SurfaceId};
use gtk::prelude::*;
use webkit6::prelude::*;

#[derive(Clone)]
pub struct BrowserPane {
    pub id: PaneId,
    pub root: gtk::Box,
    pub web_view: webkit6::WebView,
    pub address_bar: gtk::Entry,
}

impl BrowserPane {
    pub fn new(
        id: PaneId,
        surface_id: SurfaceId,
        initial_url: Option<&str>,
        callbacks: PaneCallbacks,
        engine: BrowserEngine,
    ) -> Self {
        // 옵션의 BrowserEngine 라벨은 cmux 원본과 동일하게 WebsiteDataStore
        // 격리에만 영향을 준다 — 모든 탭은 동일한 WebKitGTK 엔진으로 그려
        // 진다. flowmux-browser::BrowserProfile으로 1:1 매핑해 데이터
        // 디렉토리를 분리한다.
        let profile = engine_to_profile(&engine);
        tracing::debug!(
            engine = ?engine,
            profile = ?profile,
            "creating browser pane (WebKitGTK + profile-isolated NetworkSession)"
        );
        // Idempotent webkit sandbox bypass — main.rs entry에서도 같은
        // env를 설정하지만, 단위 테스트(bin이 아닌 lib 경로)에서는
        // main.rs를 거치지 않으므로 BrowserPane을 만드는 시점에 한
        // 번 더 설정해 두 경로 모두에서 일관되게 동작하게 한다.
        // 자세한 배경은 main.rs의 동일 set_var 주석 참조.
        if std::env::var_os("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS").is_none() {
            std::env::set_var("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "1");
        }

        // 프로필별 NetworkSession 구성. Default는 webkit이 관리하는
        // 글로벌 default 세션을 그대로 써서 시스템 표준 위치에 영속,
        // 그 외 프로필은 `$XDG_DATA_HOME/flowmux/browser/<slug>/` 아래로
        // 분리되어 같은 flowmux 안에서도 쿠키/localStorage가 섞이지 않는다.
        let network_session = build_network_session(&profile);
        let web_view = webkit6::WebView::builder()
            .network_session(&network_session)
            .build();
        web_view.set_hexpand(true);
        web_view.set_vexpand(true);

        // cmux의 `configureWebViewConfiguration` (BrowserPanel.swift:2586-)와
        // 동일한 핵심 옵션을 WebKitGTK Settings로 매핑:
        //   * mediaTypesRequiringUserActionForPlayback = []
        //         → media-playback-requires-user-gesture = false
        //         + media-playback-allows-inline = true
        //   * developerExtrasEnabled = true
        //         → enable-developer-extras = true
        //   * isElementFullscreenEnabled = true
        //         → enable-fullscreen = true
        //   * defaultWebpagePreferences.allowsContentJavaScript = true
        //         → enable-javascript = true (WebKitGTK 기본값)
        //
        // 추가로 WebKitGTK 기본값이 비활성인 미디어 관련 항목을 켠다
        // (cmux의 WKWebView는 macOS WebKit 기본 켜져 있는 것을 명시하지
        // 않은 것뿐이라, 동일 동작을 보장하려면 명시적으로 set):
        //   * enable-mediasource (HLS / DASH 등 adaptive streaming)
        //   * enable-encrypted-media (DRM — Netflix/Disney+ 등)
        //   * enable-webaudio (오디오 컨텍스트)
        //   * hardware-acceleration-policy = ALWAYS (영상 디코딩 GPU 가속)
        // WebView가 갓 만들어진 시점이라 settings는 항상 Some. 보수적으로
        // Option을 펴서 미디어 옵션을 적용한다 (None이면 webkit 측 이슈로
        // 미디어 옵션은 시스템 기본값이 적용됨).
        if let Some(settings) = webkit6::prelude::WebViewExt::settings(&web_view) {
            settings.set_media_playback_requires_user_gesture(false);
            settings.set_media_playback_allows_inline(true);
            settings.set_enable_developer_extras(true);
            settings.set_enable_fullscreen(true);
            settings.set_enable_javascript(true);
            settings.set_enable_mediasource(true);
            settings.set_enable_encrypted_media(true);
            settings.set_enable_webaudio(true);
            // Always로 전 페이지 GPU 가속을 유지. 종료 시
            // `eglDestroySync` 부재 / `corrupted size vs. prev_size`
            // race는 main.rs의 `WEBKIT_DISABLE_DMABUF_RENDERER=1`로
            // DMA-BUF renderer를 꺼서 차단한다 — webkit6 0.4 바인딩
            // 에는 ON_DEMAND가 없고 Always / Never 둘만 노출되므로
            // Never로 가면 동영상 가속까지 잃는다.
            settings.set_hardware_acceleration_policy(
                webkit6::HardwareAccelerationPolicy::Always,
            );
        } else {
            tracing::warn!("WebView::settings() returned None — media options skipped");
        }

        let back = gtk::Button::from_icon_name("go-previous-symbolic");
        let forward = gtk::Button::from_icon_name("go-next-symbolic");
        let reload = gtk::Button::from_icon_name("view-refresh-symbolic");
        let address = gtk::Entry::new();
        address.set_hexpand(true);
        address.set_placeholder_text(Some("Enter URL — e.g. http://localhost:3000"));

        let chrome = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        chrome.set_margin_top(4);
        chrome.set_margin_bottom(4);
        chrome.set_margin_start(4);
        chrome.set_margin_end(4);
        chrome.append(&back);
        chrome.append(&forward);
        chrome.append(&reload);
        chrome.append(&address);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.append(&chrome);
        root.append(&web_view);

        // Wire chrome buttons.
        {
            let v = web_view.clone();
            back.connect_clicked(move |_| {
                if v.can_go_back() {
                    v.go_back();
                }
            });
        }
        {
            let v = web_view.clone();
            forward.connect_clicked(move |_| {
                if v.can_go_forward() {
                    v.go_forward();
                }
            });
        }
        {
            let v = web_view.clone();
            reload.connect_clicked(move |_| v.reload());
        }
        {
            let v = web_view.clone();
            let a = address.clone();
            address.connect_activate(move |_| {
                let raw = a.text().to_string();
                let uri = normalize_uri(&raw);
                v.load_uri(&uri);
            });
        }

        // Reflect navigation in the address bar AND mirror the new URL
        // back to the daemon — 앱이 종료되어도 다음 실행 때 마지막
        // 페이지로 복원되도록 state에 반영한다.
        {
            let a = address.clone();
            let uri_cb = callbacks.on_browser_uri_changed.clone();
            web_view.connect_uri_notify(move |w| {
                if let Some(uri) = w.uri() {
                    let uri_str = uri.to_string();
                    a.set_text(&uri_str);
                    (uri_cb.borrow_mut())(id, surface_id, uri_str);
                }
            });
        }

        // 브라우저 페이지 title이 바뀌면 surface 탭 이름도 함께 갱신.
        // 사용자가 직접 rename 한 경우(title_locked)는 daemon 쪽에서
        // 무시하므로 여기서는 항상 통보만 한다.
        {
            let title_cb = callbacks.on_browser_title_changed.clone();
            web_view.connect_title_notify(move |w| {
                let title = w.title().map(|t| t.to_string()).unwrap_or_default();
                if !title.trim().is_empty() {
                    (title_cb.borrow_mut())(id, surface_id, title);
                }
            });
        }

        if let Some(url) = initial_url {
            let normalized = normalize_uri(url);
            address.set_text(&normalized);
            web_view.load_uri(&normalized);
        } else {
            web_view.load_uri("about:blank");
        }

        Self {
            id,
            root,
            web_view,
            address_bar: address,
        }
    }

    pub fn navigate(&self, url: &str) {
        self.web_view.load_uri(url);
    }

    /// Move backwards in session history. Returns false if there's
    /// nothing to go back to.
    pub fn back(&self) -> bool {
        if self.web_view.can_go_back() {
            self.web_view.go_back();
            true
        } else {
            false
        }
    }

    pub fn forward(&self) -> bool {
        if self.web_view.can_go_forward() {
            self.web_view.go_forward();
            true
        } else {
            false
        }
    }

    pub fn reload(&self) {
        self.web_view.reload();
    }

    pub fn current_url(&self) -> String {
        self.web_view
            .uri()
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    pub fn current_title(&self) -> String {
        self.web_view
            .title()
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    /// Run JS and call `on_done` with the JS result string. The
    /// scriptable API wraps this with a oneshot channel that the IPC
    /// handler awaits.
    pub fn evaluate_js<F: FnOnce(Result<String, String>) + 'static>(
        &self,
        source: &str,
        on_done: F,
    ) {
        self.web_view.evaluate_javascript(
            source,
            None,
            None,
            gtk::gio::Cancellable::NONE,
            move |result| {
                let r = match result {
                    Ok(value) => Ok(value.to_str().to_string()),
                    Err(e) => Err(e.to_string()),
                };
                on_done(r);
            },
        );
    }
}

fn normalize_uri(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return "about:blank".into();
    }
    if raw.starts_with("http://")
        || raw.starts_with("https://")
        || raw.starts_with("about:")
        || raw.starts_with("file://")
    {
        return raw.to_string();
    }
    if raw.starts_with("localhost") || raw.starts_with("127.") || raw.starts_with("[::1]") {
        return format!("http://{raw}");
    }
    if raw.contains('.') && !raw.contains(' ') {
        return format!("https://{raw}");
    }
    format!("https://duckduckgo.com/?q={}", urlencode(raw))
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".into(),
            other => format!("%{:02X}", other),
        })
        .collect()
}

/// 옵션 다이얼로그가 저장하는 [`BrowserEngine`] 라벨을 flowmux-browser
/// 의 [`BrowserProfile`]로 1:1 매핑한다. 매핑 자체는 의미적이며,
/// 모든 결과는 동일한 WebKitGTK 엔진으로 그려진다.
///
/// * `Webkit`  → `Default` (flowmux 기본 데이터 디렉토리)
/// * `Chrome`  → `ChromeImport` (Chromium 계열 쿠키 import 슬롯)
/// * `Firefox` → `FirefoxImport` (Firefox 쿠키 import 슬롯)
/// * `Custom { name }` → `Custom { name }` (사용자 정의 격리 슬롯)
fn engine_to_profile(engine: &BrowserEngine) -> BrowserProfile {
    match engine {
        BrowserEngine::Webkit => BrowserProfile::Default,
        BrowserEngine::Chrome => BrowserProfile::ChromeImport,
        BrowserEngine::Firefox => BrowserProfile::FirefoxImport,
        BrowserEngine::Custom { name } => BrowserProfile::Custom {
            name: name.clone(),
        },
    }
}

/// 프로필별 [`webkit6::NetworkSession`]을 만들어 반환한다.
///
/// * [`BrowserProfile::Default`]는 시스템이 관리하는 글로벌 기본
///   세션을 재사용 — 다른 flowmux 인스턴스가 켜져 있을 때도 같은
///   쿠키 풀을 공유하게 된다 (cmux의 sharedProcessPool과 동일 정신).
/// * 그 외 프로필은 `$XDG_DATA_HOME/flowmux/browser/<slug>/` 아래로
///   data + cache 디렉토리를 분리해 영속 NetworkSession을 만든다.
///   디렉토리 생성에 실패하면(권한 등) 경고를 남기고 글로벌 기본
///   세션으로 폴백해 적어도 페이지는 뜨도록 한다.
fn build_network_session(profile: &BrowserProfile) -> webkit6::NetworkSession {
    match profile {
        BrowserProfile::Default => webkit6::NetworkSession::default()
            .unwrap_or_else(|| webkit6::NetworkSession::new(None, None)),
        other => match other.data_dir() {
            Ok(dir) => {
                let dir_str = dir.to_string_lossy().into_owned();
                webkit6::NetworkSession::new(Some(&dir_str), Some(&dir_str))
            }
            Err(e) => {
                tracing::warn!(
                    profile = ?profile,
                    error = %e,
                    "browser profile data dir unavailable, falling back to default session"
                );
                webkit6::NetworkSession::default()
                    .unwrap_or_else(|| webkit6::NetworkSession::new(None, None))
            }
        },
    }
}
