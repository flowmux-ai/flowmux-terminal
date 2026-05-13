// SPDX-License-Identifier: GPL-3.0-or-later
//! Modal dialog opened by the options button in the side-panel footer.
//!
//! It exposes:
//!
//! * Global zoom percentage (10..=200% SpinButton)
//! * Default web view engine for new browser tabs (DropDown: WebKit / Chrome / Firefox)
//! * Focused-pane 1px border color (ColorButton, default pale yellow)
//! * Browser session persistence toggle (CheckButton, default checked) —
//!   keeps cookies / localStorage / IndexedDB across flowmux restarts so
//!   site logins survive a quit/relaunch.
//!
//! OK / Cancel close the dialog. `on_apply` is called only on OK, and the
//! dialog closes itself so the caller only handles the callback.
//!
//! Layering: this module only owns GTK widgets. Saving options to disk and
//! applying zoom to VTE/WebView are handled by [`crate::ui::window`]. The
//! dialog returns the user's intended [`Options`] through the callback.

use adw::prelude::*;
use flowmux_config::options::{
    BrowserEngine, Options, FOCUS_BORDER_OPACITY_MAX, FOCUS_BORDER_OPACITY_MIN, ZOOM_MAX, ZOOM_MIN,
};

/// Present the modal options dialog. If the user clicks OK, `on_apply` is
/// called with the new [`Options`]. Cancel or window close does not call back.
pub fn present(
    parent: &adw::ApplicationWindow,
    current: Options,
    on_apply: impl Fn(Options) + 'static,
) {
    let dialog = build_dialog(parent, &current, on_apply);
    dialog.present();
}

/// Build only the dialog widget tree so tests can inspect widget state
/// without calling `present`.
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
        .title("Options")
        .build();

    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);

    let cancel_btn = gtk::Button::with_label("Cancel");
    let ok_btn = gtk::Button::with_label("OK");
    ok_btn.add_css_class("suggested-action");
    header.pack_start(&cancel_btn);
    header.pack_end(&ok_btn);

    let zoom_spin = build_zoom_spin(current.zoom_percent);
    let engine_drop = build_engine_drop(&current.default_browser_engine);
    let focus_color_btn = build_focus_color_button(current.focus_border_color_or_default());
    let opacity_widgets = build_focus_opacity_row(current.focus_border_opacity);
    let persist_check = build_persist_check(current.persist_browser_session);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.set_margin_top(16);
    body.set_margin_bottom(16);
    body.set_margin_start(20);
    body.set_margin_end(20);
    body.append(&row("Global zoom (%)", &zoom_spin));
    body.append(&row("Browser web view", &engine_drop));
    body.append(&row("Focus border color", &focus_color_btn));
    body.append(&row("Focus border opacity (%)", &opacity_widgets.row));
    body.append(&row("Keep browser session data", &persist_check));

    let hint = gtk::Label::new(Some(
        "The selected label isolates the cookie/session directory for new \
         browser tabs. All labels currently render through WebKitGTK. \
         Already-open browser tabs are unchanged.",
    ));
    hint.set_wrap(true);
    hint.set_max_width_chars(46);
    hint.add_css_class("dim-label");
    hint.set_xalign(0.0);
    body.append(&hint);

    // Bottom actions. Reset applies Options::default() through the same
    // on_apply path the OK button uses, so the caller handles persistence
    // and live CSS reloads while the dialog only closes itself.
    let reset_btn = gtk::Button::with_label("Reset to defaults");
    reset_btn.add_css_class("destructive-action");
    let about_btn = gtk::Button::with_label("About");
    let footer_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    footer_spacer.set_hexpand(true);
    let footer_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer_row.set_margin_top(4);
    footer_row.append(&reset_btn);
    footer_row.append(&footer_spacer);
    footer_row.append(&about_btn);
    body.append(&footer_row);

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.append(&header);
    outer.append(&body);
    dialog.set_content(Some(&outer));

    {
        let dialog = dialog.clone();
        cancel_btn.connect_clicked(move |_| dialog.close());
    }
    let on_apply = std::rc::Rc::new(on_apply);
    {
        let dialog = dialog.clone();
        let zoom_spin = zoom_spin.clone();
        let engine_drop = engine_drop.clone();
        let focus_color_btn = focus_color_btn.clone();
        let opacity_spin = opacity_widgets.spin.clone();
        let persist_check = persist_check.clone();
        let on_apply = on_apply.clone();
        ok_btn.connect_clicked(move |_| {
            let opts = collect_options(
                &zoom_spin,
                &engine_drop,
                &focus_color_btn,
                &opacity_spin,
                &persist_check,
            );
            (on_apply)(opts);
            dialog.close();
        });
    }
    {
        let dialog = dialog.clone();
        let on_apply = on_apply.clone();
        reset_btn.connect_clicked(move |_| {
            (on_apply)(Options::default());
            dialog.close();
        });
    }
    {
        let dialog = dialog.clone();
        about_btn.connect_clicked(move |_| show_about_popup(&dialog));
    }

    dialog
}

fn show_about_popup(parent: &impl IsA<gtk::Widget>) {
    let body = about_body();
    let root = parent.root().and_then(|r| r.downcast::<gtk::Window>().ok());
    let dialog = gtk::MessageDialog::builder()
        .modal(true)
        .message_type(gtk::MessageType::Info)
        .text("About")
        .secondary_text(body.as_str())
        .secondary_use_markup(true)
        .build();
    dialog.set_transient_for(root.as_ref());
    dialog.add_button("OK", gtk::ResponseType::Ok);
    dialog.connect_response(move |dialog, _| {
        dialog.close();
    });
    dialog.present();
}

fn about_body() -> String {
    about_body_with_version(&about_version())
}

fn about_body_with_version(version: &str) -> String {
    format!(
        "FlowMux - Agent Workflow Multiplexer Terminal\n\n\
         FlowMux was inspired by the cmux (macOS) project.\n\n\
         Maintained by JSUYA (Junsu Choi).\n\
         <a href=\"https://github.com/JSUYA/flowmux\">https://github.com/JSUYA/flowmux</a>\n\n\
         Version: v{}",
        version
    )
}

fn about_version() -> String {
    installed_package_version().unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

fn installed_package_version() -> Option<String> {
    let output = std::process::Command::new("dpkg-query")
        .args(["-W", "-f=${Version}", "flowmux"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let raw = String::from_utf8(output.stdout).ok()?;
    clean_installed_version(&raw)
}

fn clean_installed_version(raw: &str) -> Option<String> {
    let version = raw.trim();
    if version.is_empty() || version.contains('\n') {
        return None;
    }
    if !version.chars().all(is_safe_version_char) {
        return None;
    }
    Some(version.to_string())
}

fn is_safe_version_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+' | '~' | ':')
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

/// 10..=200% SpinButton. It steps by 1% and supports keyboard or mouse-wheel
/// changes. If direct text entry goes out of range, [`Options::clamp_zoom`]
/// clamps it when OK is clicked.
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

/// DropDown for WebKit / Chrome / Firefox. Custom engines are serializable in
/// `Options` but not exposed in this UI step.
fn build_engine_drop(initial: &BrowserEngine) -> gtk::DropDown {
    let labels: Vec<String> = engine_options().iter().map(|e| e.label()).collect();
    let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    let drop = gtk::DropDown::from_strings(&label_refs);
    let idx = engine_index_of(initial);
    drop.set_selected(idx as u32);
    drop
}

/// Collect the user's intent from dialog widgets into [`Options`]. SpinButton
/// values may be out of range due to direct text entry, so clamp zoom and
/// opacity again. [`color_button_hex`] normalizes the focus color from GdkRGBA
/// to six-digit `#rrggbb`.
fn collect_options(
    spin: &gtk::SpinButton,
    drop: &gtk::DropDown,
    focus_color: &gtk::ColorButton,
    opacity_spin: &gtk::SpinButton,
    persist_check: &gtk::CheckButton,
) -> Options {
    let zoom = Options::clamp_zoom(spin.value_as_int().max(0) as u16);
    let engine = engine_options()
        .get(drop.selected() as usize)
        .cloned()
        .unwrap_or(BrowserEngine::Webkit);
    let color_hex = color_button_hex(focus_color);
    let opacity =
        Options::clamp_focus_border_opacity(opacity_spin.value_as_int().clamp(0, 255) as u8);
    Options {
        zoom_percent: zoom,
        default_browser_engine: engine,
        focus_border_color: color_hex,
        focus_border_opacity: opacity,
        persist_browser_session: persist_check.is_active(),
    }
}

/// Serialize the current gtk::ColorButton RGBA as `#rrggbb`.
/// The alpha channel is ignored because opacity has its own option.
fn color_button_hex(button: &gtk::ColorButton) -> String {
    let rgba = button.rgba();
    let r = (rgba.red().clamp(0.0, 1.0) * 255.0).round() as u8;
    let g = (rgba.green().clamp(0.0, 1.0) * 255.0).round() as u8;
    let b = (rgba.blue().clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", r, g, b)
}

/// Widgets for focus border opacity (%). The slider and SpinButton share the
/// same [`gtk::Adjustment`], so dragging the slider updates the number and
/// direct numeric input moves the slider. Append `row` to the dialog body and
/// read `spin` from `collect_options` when OK is clicked.
struct FocusOpacityRow {
    row: gtk::Box,
    spin: gtk::SpinButton,
}

fn build_focus_opacity_row(initial: u8) -> FocusOpacityRow {
    let initial = Options::clamp_focus_border_opacity(initial);
    let adj = gtk::Adjustment::new(
        initial as f64,
        FOCUS_BORDER_OPACITY_MIN as f64,
        FOCUS_BORDER_OPACITY_MAX as f64,
        1.0,
        10.0,
        0.0,
    );

    let scale = gtk::Scale::new(gtk::Orientation::Horizontal, Some(&adj));
    scale.set_hexpand(true);
    scale.set_draw_value(false);
    scale.set_round_digits(0);
    // Small markers at 0 / 50 / 100 so the user can gauge position quickly.
    scale.add_mark(0.0, gtk::PositionType::Bottom, None);
    scale.add_mark(50.0, gtk::PositionType::Bottom, None);
    scale.add_mark(
        FOCUS_BORDER_OPACITY_MAX as f64,
        gtk::PositionType::Bottom,
        None,
    );

    let spin = gtk::SpinButton::new(Some(&adj), 1.0, 0);
    spin.set_numeric(true);
    spin.set_snap_to_ticks(true);
    spin.set_value(initial as f64);
    spin.set_width_chars(4);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.set_hexpand(true);
    row.append(&scale);
    row.append(&spin);

    FocusOpacityRow { row, spin }
}

/// CheckButton seeded with the current persistence flag. New browser tabs
/// pick up the saved value through [`Options::persist_browser_session`];
/// already-open browser tabs keep their existing NetworkSession (matching
/// the dropdown engine option above).
fn build_persist_check(initial: bool) -> gtk::CheckButton {
    let check = gtk::CheckButton::with_label("Persist cookies, sign-ins, and site data");
    check.set_active(initial);
    check
}

/// Parse `#rrggbb` or another hex form as GdkRGBA and seed the
/// ColorButton. Fall back to the default pale yellow on parse failure.
fn build_focus_color_button(initial_hex: &str) -> gtk::ColorButton {
    let parsed = gtk::gdk::RGBA::parse(initial_hex)
        .ok()
        .or_else(|| gtk::gdk::RGBA::parse("#fff4b3").ok())
        .expect("default focus color must be a valid RGBA literal");
    let button = gtk::ColorButton::with_rgba(&parsed);
    button.set_modal(true);
    button.set_title("Focus Border Color");
    button.set_use_alpha(false);
    button
}

/// Built-in engine order exposed in the DropDown. Serialization uses
/// [`BrowserEngine`] itself, so this array only controls UI display order.
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

    #[test]
    fn about_body_contains_requested_copy() {
        let body = about_body_with_version("9.8.7-6");
        assert!(body.contains("FlowMux - Agent Workflow Multiplexer Terminal"));
        assert!(body.contains("FlowMux was inspired by the cmux (macOS) project."));
        assert!(body.contains("Maintained by JSUYA (Junsu Choi)."));
        assert!(body.contains(
            "<a href=\"https://github.com/JSUYA/flowmux\">https://github.com/JSUYA/flowmux</a>"
        ));
        assert!(body.ends_with("Version: v9.8.7-6"));
    }

    #[test]
    fn clean_installed_version_accepts_debian_versions() {
        assert_eq!(
            clean_installed_version(" 1:0.1.0-2+ubuntu~24.04 \n"),
            Some("1:0.1.0-2+ubuntu~24.04".into())
        );
    }

    #[test]
    fn clean_installed_version_rejects_empty_multiline_or_markup() {
        assert_eq!(clean_installed_version(""), None);
        assert_eq!(clean_installed_version("1.2.3\n4.5.6"), None);
        assert_eq!(clean_installed_version("<b>1.2.3</b>"), None);
    }

    /// The persistence checkbox should reflect the seeded value so the
    /// dialog opens in the correct state when the user reviews their
    /// existing options.
    #[gtk::test]
    fn persist_check_reflects_initial_value() {
        let check_on = build_persist_check(true);
        assert!(check_on.is_active());
        let check_off = build_persist_check(false);
        assert!(!check_off.is_active());
    }

    /// `collect_options` must round-trip the checkbox state into the
    /// returned [`Options`]. We seed each widget with a known value and
    /// confirm the collected options match — including the new
    /// `persist_browser_session` flag — so a regression that drops the
    /// flag from `collect_options` would fail loudly.
    #[gtk::test]
    fn collect_options_round_trips_persist_browser_session() {
        let zoom = build_zoom_spin(120);
        let engine = build_engine_drop(&BrowserEngine::Firefox);
        let focus_color = build_focus_color_button("#abcdef");
        let opacity = build_focus_opacity_row(40);
        let persist_off = build_persist_check(false);
        let opts = collect_options(&zoom, &engine, &focus_color, &opacity.spin, &persist_off);
        assert_eq!(opts.zoom_percent, 120);
        assert_eq!(opts.default_browser_engine, BrowserEngine::Firefox);
        assert_eq!(opts.focus_border_opacity, 40);
        assert!(!opts.persist_browser_session);

        let persist_on = build_persist_check(true);
        let opts = collect_options(&zoom, &engine, &focus_color, &opacity.spin, &persist_on);
        assert!(opts.persist_browser_session);
    }

    /// GTK init is needed to verify that the slider and SpinButton share the
    /// same Adjustment.
    #[gtk::test]
    fn focus_opacity_row_widgets_share_adjustment_and_clamp_initial() {
        // Out-of-range initial values clamp the spin button to 100.
        let widgets = build_focus_opacity_row(250);
        assert_eq!(widgets.spin.value_as_int(), 100);

        // Changing the spin value updates the scale through the shared adjustment.
        widgets.spin.set_value(42.0);
        let scale = widgets
            .row
            .first_child()
            .and_then(|w| w.downcast::<gtk::Scale>().ok())
            .expect("first child should be the Scale");
        assert!((scale.value() - 42.0).abs() < f64::EPSILON);

        // Changing the scale value updates the spin button as well.
        scale.set_value(7.0);
        assert_eq!(widgets.spin.value_as_int(), 7);
    }
}
