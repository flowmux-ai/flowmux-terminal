// SPDX-License-Identifier: GPL-3.0-or-later
//! 사이드 패널 좌하단 옵션 버튼이 띄우는 모달 다이얼로그.
//!
//! 세 항목을 노출한다:
//!
//! * 전체 줌 배율 (10..=200% SpinButton)
//! * 새 탭브라우저 기본 웹뷰 엔진 (DropDown: WebKit / Chrome / Firefox)
//! * 포커스된 pane의 1px 테두리 색 (ColorDialogButton, 기본 연한 노란색)
//!
//! [확인] / [취소] 버튼으로 닫는다. [확인] 시에만 `on_apply`가 호출되며,
//! 다이얼로그는 자기 자신이 닫는다 (호출 측은 콜백만 처리).
//!
//! 레이어링: 이 모듈은 GTK 위젯만 다룬다. 옵션을 디스크에 저장하거나
//! VTE/WebView에 줌을 적용하는 책임은 [`crate::ui::window`]가 진다.
//! 다이얼로그는 사용자의 의도(`Options`)만 콜백으로 돌려준다.

use adw::prelude::*;
use flowmux_config::options::{BrowserEngine, Options, ZOOM_MAX, ZOOM_MIN};

/// 모달로 옵션 다이얼로그를 띄운다. 사용자가 [확인]을 누르면
/// `on_apply`가 새 [`Options`]로 호출된다. [취소]를 누르거나 창을
/// 닫으면 콜백은 호출되지 않는다.
pub fn present(
    parent: &adw::ApplicationWindow,
    current: Options,
    on_apply: impl Fn(Options) + 'static,
) {
    let dialog = build_dialog(parent, &current, on_apply);
    dialog.present();
}

/// 다이얼로그 위젯 트리만 만든다 — 단위 테스트가 위젯 상태를
/// 검사할 수 있도록 `present` 호출 없이 분리.
fn build_dialog(
    parent: &adw::ApplicationWindow,
    current: &Options,
    on_apply: impl Fn(Options) + 'static,
) -> adw::Window {
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(440)
        .default_height(220)
        .title("옵션")
        .build();

    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);

    let cancel_btn = gtk::Button::with_label("취소");
    let ok_btn = gtk::Button::with_label("확인");
    ok_btn.add_css_class("suggested-action");
    header.pack_start(&cancel_btn);
    header.pack_end(&ok_btn);

    let zoom_spin = build_zoom_spin(current.zoom_percent);
    let engine_drop = build_engine_drop(&current.default_browser_engine);
    let focus_color_btn = build_focus_color_button(current.focus_border_color_or_default());

    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.set_margin_top(16);
    body.set_margin_bottom(16);
    body.set_margin_start(20);
    body.set_margin_end(20);
    body.append(&row("전체 줌 (%)", &zoom_spin));
    body.append(&row("브라우저 웹뷰", &engine_drop));
    body.append(&row("포커스 테두리 색", &focus_color_btn));

    let hint = gtk::Label::new(Some(
        "선택한 라벨은 새 탭브라우저의 쿠키/세션 디렉토리를 격리합니다 \
         (모든 라벨이 WebKitGTK 엔진으로 그려집니다). 이미 열려 있는 \
         탭브라우저는 그대로 유지됩니다.",
    ));
    hint.set_wrap(true);
    hint.set_max_width_chars(46);
    hint.add_css_class("dim-label");
    hint.set_xalign(0.0);
    body.append(&hint);

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.append(&header);
    outer.append(&body);
    dialog.set_content(Some(&outer));

    {
        let dialog = dialog.clone();
        cancel_btn.connect_clicked(move |_| dialog.close());
    }
    {
        let dialog = dialog.clone();
        let zoom_spin = zoom_spin.clone();
        let engine_drop = engine_drop.clone();
        let focus_color_btn = focus_color_btn.clone();
        let on_apply = std::rc::Rc::new(on_apply);
        ok_btn.connect_clicked(move |_| {
            let opts = collect_options(&zoom_spin, &engine_drop, &focus_color_btn);
            (on_apply)(opts);
            dialog.close();
        });
    }

    dialog
}

fn row(label_text: &str, value_widget: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let label = gtk::Label::new(Some(label_text));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_halign(gtk::Align::Start);
    row.append(&label);
    row.append(value_widget);
    row
}

/// 10..=200% SpinButton. 1% 단위, 키보드/마우스 휠로도 조정 가능,
/// 사용자가 직접 텍스트로 입력해 범위를 벗어나면 [`Options::clamp_zoom`]
/// 가 [확인] 시점에 잘라낸다.
fn build_zoom_spin(initial: u16) -> gtk::SpinButton {
    let initial = Options::clamp_zoom(initial);
    let adj = gtk::Adjustment::new(
        initial as f64,
        ZOOM_MIN as f64,
        ZOOM_MAX as f64,
        1.0,
        10.0,
        0.0,
    );
    let spin = gtk::SpinButton::new(Some(&adj), 1.0, 0);
    spin.set_numeric(true);
    spin.set_snap_to_ticks(true);
    spin.set_value(initial as f64);
    spin.set_width_chars(6);
    spin
}

/// WebKit / Chrome / Firefox 셋 중 하나를 고르는 DropDown. Custom
/// 엔진은 이번 단계에서는 노출하지 않는다 (`Options`에는 직렬화
/// 가능 — 추후 확장 시 활성화).
fn build_engine_drop(initial: &BrowserEngine) -> gtk::DropDown {
    let labels: Vec<String> = engine_options().iter().map(|e| e.label()).collect();
    let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    let drop = gtk::DropDown::from_strings(&label_refs);
    let idx = engine_index_of(initial);
    drop.set_selected(idx as u32);
    drop
}

/// 다이얼로그 위젯에서 사용자 의도를 다시 [`Options`]로 모아낸다.
/// SpinButton 값이 직접 타이핑으로 범위를 살짝 넘었을 가능성이 있어
/// `clamp_zoom`으로 한 번 더 보정한다. 포커스 색은 [`color_button_hex`]
/// 가 GdkRGBA → `#rrggbb` 6-자리 hex로 정규화해 돌려준다.
fn collect_options(
    spin: &gtk::SpinButton,
    drop: &gtk::DropDown,
    focus_color: &gtk::ColorDialogButton,
) -> Options {
    let zoom = Options::clamp_zoom(spin.value_as_int().max(0) as u16);
    let engine = engine_options()
        .get(drop.selected() as usize)
        .cloned()
        .unwrap_or(BrowserEngine::Webkit);
    let color_hex = color_button_hex(focus_color);
    Options {
        zoom_percent: zoom,
        default_browser_engine: engine,
        focus_border_color: color_hex,
    }
}

/// gtk::ColorDialogButton의 현재 RGBA를 `#rrggbb` 형식으로 직렬화.
/// 알파 채널은 무시(테두리에는 투명도가 의미 없음).
fn color_button_hex(button: &gtk::ColorDialogButton) -> String {
    let rgba = button.rgba();
    let r = (rgba.red().clamp(0.0, 1.0) * 255.0).round() as u8;
    let g = (rgba.green().clamp(0.0, 1.0) * 255.0).round() as u8;
    let b = (rgba.blue().clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", r, g, b)
}

/// `#rrggbb` (또는 다른 hex 형식)을 GdkRGBA로 파싱해 ColorDialogButton에
/// 미리 채운다. 파싱 실패 시 기본 연한 노란색으로 fallback.
fn build_focus_color_button(initial_hex: &str) -> gtk::ColorDialogButton {
    let dialog = gtk::ColorDialog::new();
    dialog.set_with_alpha(false);
    let button = gtk::ColorDialogButton::new(Some(dialog));
    let parsed = gtk::gdk::RGBA::parse(initial_hex)
        .ok()
        .or_else(|| gtk::gdk::RGBA::parse("#fff4b3").ok())
        .expect("default focus color must be a valid RGBA literal");
    button.set_rgba(&parsed);
    button
}

/// DropDown에 노출되는 빌트인 엔진 순서. 직렬화에는 [`BrowserEngine`]
/// 자체가 그대로 사용되므로 이 배열 순서는 UI 표시용일 뿐이다.
fn engine_options() -> [BrowserEngine; 3] {
    [
        BrowserEngine::Webkit,
        BrowserEngine::Chrome,
        BrowserEngine::Firefox,
    ]
}

fn engine_index_of(engine: &BrowserEngine) -> usize {
    engine_options()
        .iter()
        .position(|e| e == engine)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_options_lists_three_builtin_variants_in_label_order() {
        let engines = engine_options();
        assert_eq!(engines.len(), 3);
        assert_eq!(engines[0], BrowserEngine::Webkit);
        assert_eq!(engines[1], BrowserEngine::Chrome);
        assert_eq!(engines[2], BrowserEngine::Firefox);
    }

    #[test]
    fn engine_index_of_returns_zero_for_unknown_custom_engine() {
        let idx = engine_index_of(&BrowserEngine::Custom {
            name: "Brave".into(),
        });
        assert_eq!(idx, 0);
    }

    #[test]
    fn engine_index_of_matches_each_builtin() {
        assert_eq!(engine_index_of(&BrowserEngine::Webkit), 0);
        assert_eq!(engine_index_of(&BrowserEngine::Chrome), 1);
        assert_eq!(engine_index_of(&BrowserEngine::Firefox), 2);
    }
}
