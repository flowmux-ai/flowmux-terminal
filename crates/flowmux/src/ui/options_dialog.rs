// SPDX-License-Identifier: GPL-3.0-or-later
//! Modal dialog opened by the options button in the side-panel footer.
//!
//! It exposes:
//!
//! * Global zoom percentage (10..=200% SpinButton)
//! * Terminal font family (DropDown of installed, curated developer fonts,
//!   plus a "System default" sentinel that inherits the theme font) and size
//!   (SpinButton, points). Applied live to every open terminal.
//! * Default web view engine for new browser tabs (DropDown: WebKit / Chrome / Firefox)
//! * Focused-pane 1px border color (ColorDialogButton, default pale yellow)
//! * Browser session persistence toggle (CheckButton, default checked) —
//!   keeps cookies / localStorage / IndexedDB across flowmux restarts so
//!   site logins survive a quit/relaunch.
//! * Agent Bar visibility toggle (Switch, default on).
//! * Software update status, manual release check, and update action.
//!
//! OK / Cancel close the dialog. `on_apply` is called only on OK, and the
//! dialog closes itself so the caller only handles the callback.
//!
//! Layering: this module only owns GTK widgets. Saving options to disk and
//! applying zoom to terminal/WebView are handled by [`crate::ui::window`]. The
//! dialog returns the user's intended [`Options`] through the callback.

use crate::update::{check::Version, BannerState, Stage};
use adw::prelude::*;
use flowmux_config::keybindings::KeybindingOverrides;
use flowmux_config::options::{
    BrowserEngine, Options, CURSOR_BLINK_INTERVAL_MAX, CURSOR_BLINK_INTERVAL_MIN,
    FOCUS_BORDER_OPACITY_MAX, FOCUS_BORDER_OPACITY_MIN, ZOOM_MAX, ZOOM_MIN,
};
use flowmux_core::AgentNotificationTarget;
use std::cell::RefCell;
use std::rc::Rc;

type UpdateCheckCompletion = Box<dyn FnOnce(Result<BannerState, String>)>;

/// Present the modal options dialog. If the user clicks OK, `on_apply` is
/// called with the new [`Options`]. Cancel or window close does not call back.
/// `default_font_family` / `default_font_size` are the resolved theme font,
/// used to seed the font widgets when the user has no override.
pub fn present(
    parent: &adw::ApplicationWindow,
    current: Options,
    default_font_family: String,
    default_font_size: f32,
    on_apply: impl Fn(Options) + 'static,
    on_preview: impl Fn(&Options) + 'static,
    update_state: BannerState,
    on_check_update: impl Fn(UpdateCheckCompletion) -> bool + 'static,
    on_update: impl Fn(Version) -> bool + 'static,
) {
    let dialog = build_dialog(
        parent,
        &current,
        default_font_family,
        default_font_size,
        on_apply,
        on_preview,
        update_state,
        on_check_update,
        on_update,
    );
    dialog.present();
}

/// Build only the dialog widget tree so tests can inspect widget state
/// without calling `present`.
fn build_dialog(
    parent: &adw::ApplicationWindow,
    current: &Options,
    default_font_family: String,
    default_font_size: f32,
    on_apply: impl Fn(Options) + 'static,
    on_preview: impl Fn(&Options) + 'static,
    update_state: BannerState,
    on_check_update: impl Fn(UpdateCheckCompletion) -> bool + 'static,
    on_update: impl Fn(Version) -> bool + 'static,
) -> adw::Window {
    // Intentionally NOT modal. A modal transient dialog gets attached to its
    // parent's titlebar by window managers that honour the "attach modal
    // dialogs" behaviour (Mutter's `attach-modal-dialogs`, and similar),
    // which makes the WM move the dialog and the main window together — so
    // the user can no longer drag the options popup on its own. Keeping
    // `transient_for` (floats above the main window, centred on it) without
    // `modal` lets the WM treat it as an independent, freely-movable window.
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .default_width(760)
        .default_height(620)
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
    let font_widgets = build_font_widgets(parent, current, &default_font_family, default_font_size);
    let engine_drop = build_engine_drop(&current.default_browser_engine);
    let focus_color_btn = build_focus_color_button(current.focus_border_color_or_default());
    let opacity_widgets = build_focus_opacity_row(current.focus_border_opacity);
    let persist_check = build_persist_check(current.persist_browser_session);
    let auto_resume_check = build_persist_check(current.auto_resume_agent_sessions);
    let scrollback_check = build_persist_check(current.restore_terminal_scrollback);
    let system_notify_switch = build_system_notify_switch(current.system_notifications_enabled);
    let agent_bar_switch = build_agent_bar_switch(current.agent_bar_enabled);
    let cursor_blink_switch = build_cursor_blink_switch(current.cursor_blink);
    let blink_interval_spin = build_blink_interval_spin(current.cursor_blink_interval_ms);

    // General tab body — the original options dialog contents.
    let general = gtk::Box::new(gtk::Orientation::Vertical, 12);
    general.set_margin_top(16);
    general.set_margin_bottom(16);
    general.set_margin_start(20);
    general.set_margin_end(20);
    general.append(&row("Global zoom (%)", &zoom_spin));
    general.append(&row("Terminal font", &font_widgets.family_drop));
    general.append(&row("Font size (pt)", &font_widgets.size_spin));
    general.append(&row("Browser web view", &engine_drop));
    general.append(&row("Focus border color", &focus_color_btn));
    general.append(&row("Focus border opacity (%)", &opacity_widgets.row));
    general.append(&row("Keep browser session data", &persist_check));
    general.append(&row("Resume agent sessions on reopen", &auto_resume_check));
    general.append(&row("Restore terminal scrollback", &scrollback_check));
    general.append(&row("System notifications", &system_notify_switch));
    general.append(&row("Agent Bar", &agent_bar_switch));
    general.append(&row("Cursor blink", &cursor_blink_switch));
    general.append(&row("Cursor blink interval (ms)", &blink_interval_spin));

    let hint = gtk::Label::new(Some(
        "The selected label isolates the cookie/session directory for new \
         browser tabs. All labels currently render through WebKitGTK. \
         Already-open browser tabs are unchanged.",
    ));
    hint.set_wrap(true);
    hint.set_max_width_chars(46);
    hint.add_css_class("dim-label");
    hint.set_xalign(0.0);
    general.append(&hint);

    // Keybindings tab — edits write into kb_state below, picked up by
    // collect_options when the user clicks OK.
    let kb_state = std::rc::Rc::new(std::cell::RefCell::new(current.keybindings.clone()));
    let keybindings_tab = crate::ui::keybindings_panel::build(kb_state.clone());

    // Theme tab — selections write into theme_state and preview live via
    // on_preview; OK persists them through collect_options, and Cancel /
    // close restores the original look (see connect_close_request below).
    let on_preview = std::rc::Rc::new(on_preview);
    let theme_state = std::rc::Rc::new(std::cell::RefCell::new(
        crate::ui::theme_tab::ThemeSelection {
            theme: current.theme.clone(),
            overrides: current.theme_overrides.clone(),
        },
    ));
    let preview_theme: std::rc::Rc<dyn Fn()> = {
        let theme_state = theme_state.clone();
        let on_preview = on_preview.clone();
        let base = current.clone();
        std::rc::Rc::new(move || {
            let selection = theme_state.borrow();
            let mut opts = base.clone();
            opts.theme = selection.theme.clone();
            opts.theme_overrides = selection.overrides.clone();
            on_preview(&opts);
        })
    };
    let theme_tab = crate::ui::theme_tab::build(theme_state.clone(), preview_theme);
    let update_tab = build_update_tab(
        &about_version(),
        update_state,
        Rc::new(on_check_update),
        Rc::new(on_update),
    );

    // Use freedesktop symbolic icon names so the switcher picks up the
    // current Adwaita theme automatically. `preferences-system-symbolic`
    // is the standard "settings cog" used across GNOME apps; the
    // keyboard glyph in `input-keyboard-symbolic` matches the system
    // Settings → Keyboard panel and reads as "shortcuts" at a glance.
    let stack = adw::ViewStack::new();
    stack.add_titled_with_icon(
        &general,
        Some("general"),
        "General",
        "preferences-system-symbolic",
    );
    stack.add_titled_with_icon(
        &theme_tab,
        Some("theme"),
        "Theme",
        "applications-graphics-symbolic",
    );
    stack.add_titled_with_icon(
        &keybindings_tab,
        Some("keybindings"),
        "Keybindings",
        "input-keyboard-symbolic",
    );
    stack.add_titled_with_icon(
        &update_tab,
        Some("update"),
        "Update",
        "software-update-available-symbolic",
    );
    let switcher = adw::ViewSwitcher::new();
    switcher.set_stack(Some(&stack));
    switcher.set_policy(adw::ViewSwitcherPolicy::Wide);
    header.set_title_widget(Some(&switcher));

    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.append(&stack);

    // Bottom actions. Reset applies Options::default() through the same
    // on_apply path the OK button uses, so the caller handles persistence
    // and live CSS reloads while the dialog only closes itself. The
    // global Reset only wipes General-tab options — Keybindings has its
    // own Reset-all button on its own tab so a misclick here does not
    // also blow away every shortcut the user customised.
    let reset_btn = gtk::Button::with_label("Reset to defaults");
    reset_btn.add_css_class("destructive-action");
    let about_btn = gtk::Button::with_label("About");
    let footer_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    footer_spacer.set_hexpand(true);
    let footer_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer_row.set_margin_top(4);
    footer_row.set_margin_start(20);
    footer_row.set_margin_end(20);
    footer_row.set_margin_bottom(12);
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
    // Theme previews already repainted the app; when the dialog closes any
    // way other than OK / Reset, restore the look the user started with.
    let applied = std::rc::Rc::new(std::cell::Cell::new(false));
    {
        let applied = applied.clone();
        let on_preview = on_preview.clone();
        let original = current.clone();
        dialog.connect_close_request(move |_| {
            if !applied.get() {
                on_preview(&original);
            }
            gtk::glib::Propagation::Proceed
        });
    }
    let on_apply = std::rc::Rc::new(on_apply);
    {
        let dialog = dialog.clone();
        let zoom_spin = zoom_spin.clone();
        let engine_drop = engine_drop.clone();
        let focus_color_btn = focus_color_btn.clone();
        let opacity_spin = opacity_widgets.spin.clone();
        let persist_check = persist_check.clone();
        let auto_resume_check = auto_resume_check.clone();
        let scrollback_check = scrollback_check.clone();
        let system_notify_switch = system_notify_switch.clone();
        let agent_bar_switch = agent_bar_switch.clone();
        let cursor_blink_switch = cursor_blink_switch.clone();
        let blink_interval_spin = blink_interval_spin.clone();
        let family_drop = font_widgets.family_drop.clone();
        let font_size_spin = font_widgets.size_spin.clone();
        let families = font_widgets.families.clone();
        let on_apply = on_apply.clone();
        let kb_state = kb_state.clone();
        let theme_state = theme_state.clone();
        let applied = applied.clone();
        let agent_notification_target = current.agent_notification_target;
        ok_btn.connect_clicked(move |_| {
            let kb = kb_state.borrow().clone();
            let conflicts = crate::ui::keybindings_panel::detect_conflicts(&kb);
            if !conflicts.is_empty() {
                tracing::warn!(
                    count = conflicts.len(),
                    "saving keybindings with overlapping accels — last writer wins at install time"
                );
            }
            let opts = collect_options(
                &zoom_spin,
                &engine_drop,
                &focus_color_btn,
                &opacity_spin,
                &persist_check,
                &auto_resume_check,
                &scrollback_check,
                &system_notify_switch,
                &agent_bar_switch,
                &cursor_blink_switch,
                &blink_interval_spin,
                &family_drop,
                &font_size_spin,
                &families,
                default_font_size,
                agent_notification_target,
                &kb,
                &theme_state.borrow(),
            );
            applied.set(true);
            (on_apply)(opts);
            dialog.close();
        });
    }
    {
        let dialog = dialog.clone();
        let on_apply = on_apply.clone();
        let applied = applied.clone();
        reset_btn.connect_clicked(move |_| {
            applied.set(true);
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

#[derive(Debug, PartialEq, Eq)]
struct UpdateTabProps {
    status: String,
    update_version: Option<Version>,
    update_label: Option<String>,
    check_sensitive: bool,
}

fn update_tab_props(state: &BannerState) -> UpdateTabProps {
    match state {
        BannerState::Hidden => UpdateTabProps {
            status: "Check for updates to see if a newer version is available.".into(),
            update_version: None,
            update_label: None,
            check_sensitive: true,
        },
        BannerState::Current => UpdateTabProps {
            status: "You are using the latest version of FlowMux.".into(),
            update_version: None,
            update_label: None,
            check_sensitive: true,
        },
        BannerState::Available(version) | BannerState::Ignored(version) => UpdateTabProps {
            status: format!("FlowMux v{version} is available."),
            update_version: Some(*version),
            update_label: Some(format!("Update to v{version}")),
            check_sensitive: true,
        },
        BannerState::Running(Stage::Fetching, version) => UpdateTabProps {
            status: format!("Downloading FlowMux v{version}…"),
            update_version: None,
            update_label: None,
            check_sensitive: false,
        },
        BannerState::Running(Stage::Installing, version) => UpdateTabProps {
            status: format!("Building and installing FlowMux v{version}…"),
            update_version: None,
            update_label: None,
            check_sensitive: false,
        },
        BannerState::Done(version) => UpdateTabProps {
            status: format!("FlowMux v{version} is installed. Restart FlowMux to use it."),
            update_version: None,
            update_label: None,
            check_sensitive: true,
        },
        BannerState::Failed(message, version) => UpdateTabProps {
            status: format!("Update to v{version} failed: {message}"),
            update_version: Some(*version),
            update_label: Some("Retry update".into()),
            check_sensitive: true,
        },
    }
}

fn render_update_tab(
    state: &BannerState,
    status_row: &adw::ActionRow,
    check_button: &gtk::Button,
    update_button: &gtk::Button,
) {
    let props = update_tab_props(state);
    status_row.set_subtitle(&props.status);
    check_button.set_sensitive(props.check_sensitive);
    if let Some(label) = props.update_label {
        update_button.set_label(&label);
        update_button.set_sensitive(true);
        update_button.set_visible(true);
    } else {
        update_button.set_visible(false);
    }
}

fn request_update(state: &BannerState, on_update: &dyn Fn(Version) -> bool) -> Option<BannerState> {
    let version = update_tab_props(state).update_version?;
    on_update(version).then_some(BannerState::Running(Stage::Fetching, version))
}

fn build_update_tab(
    current_version: &str,
    initial_state: BannerState,
    on_check: Rc<dyn Fn(UpdateCheckCompletion) -> bool>,
    on_update: Rc<dyn Fn(Version) -> bool>,
) -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.set_margin_top(20);
    page.set_margin_bottom(20);
    page.set_margin_start(20);
    page.set_margin_end(20);

    let group = adw::PreferencesGroup::builder()
        .title("Software Update")
        .description("Check for a new FlowMux release and install it when you are ready.")
        .build();

    let version_row = adw::ActionRow::builder()
        .title("Installed version")
        .subtitle(format!("v{current_version}"))
        .build();
    group.add(&version_row);

    let status_row = adw::ActionRow::builder().title("Update status").build();
    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);
    spinner.set_valign(gtk::Align::Center);
    status_row.add_prefix(&spinner);

    let check_button = gtk::Button::with_label("Check for Updates");
    check_button.set_valign(gtk::Align::Center);
    check_button.set_widget_name("flowmux-check-update-button");
    status_row.add_suffix(&check_button);

    let update_button = gtk::Button::new();
    update_button.add_css_class("suggested-action");
    update_button.set_valign(gtk::Align::Center);
    update_button.set_widget_name("flowmux-install-update-button");
    status_row.add_suffix(&update_button);
    group.add(&status_row);
    page.append(&group);

    let state = Rc::new(RefCell::new(initial_state));
    render_update_tab(&state.borrow(), &status_row, &check_button, &update_button);

    {
        let state = state.clone();
        let status_row = status_row.clone();
        let check_button = check_button.clone();
        let update_button = update_button.clone();
        let spinner = spinner.clone();
        check_button.clone().connect_clicked(move |_| {
            check_button.set_sensitive(false);
            update_button.set_sensitive(false);
            status_row.set_subtitle("Checking for updates…");
            spinner.set_visible(true);
            spinner.start();

            let state_for_result = state.clone();
            let status_for_result = status_row.clone();
            let check_for_result = check_button.clone();
            let update_for_result = update_button.clone();
            let spinner_for_result = spinner.clone();
            let started = on_check(Box::new(move |result| {
                spinner_for_result.stop();
                spinner_for_result.set_visible(false);
                match result {
                    Ok(next) => {
                        *state_for_result.borrow_mut() = next;
                        render_update_tab(
                            &state_for_result.borrow(),
                            &status_for_result,
                            &check_for_result,
                            &update_for_result,
                        );
                    }
                    Err(error) => {
                        render_update_tab(
                            &state_for_result.borrow(),
                            &status_for_result,
                            &check_for_result,
                            &update_for_result,
                        );
                        status_for_result
                            .set_subtitle(&format!("Could not check for updates: {error}"));
                    }
                }
            }));

            if !started {
                spinner.stop();
                spinner.set_visible(false);
                render_update_tab(&state.borrow(), &status_row, &check_button, &update_button);
                status_row.set_subtitle("Update checks are unavailable in this session.");
            }
        });
    }

    {
        let state = state.clone();
        let status_row = status_row.clone();
        let check_button = check_button.clone();
        let update_button = update_button.clone();
        update_button.clone().connect_clicked(move |_| {
            let next = {
                let state = state.borrow();
                request_update(&state, on_update.as_ref())
            };
            if let Some(next) = next {
                *state.borrow_mut() = next;
                render_update_tab(&state.borrow(), &status_row, &check_button, &update_button);
            } else {
                update_button.set_sensitive(false);
                status_row.set_subtitle("This update is already running or installed.");
            }
        });
    }

    page
}

fn show_about_popup(parent: &impl IsA<gtk::Widget>) {
    let body = about_body_with_version(&about_version());
    let dialog = adw::AlertDialog::builder()
        .heading("About")
        .body(body.as_str())
        .body_use_markup(true)
        .default_response("ok")
        .close_response("ok")
        .build();
    dialog.add_response("ok", "OK");
    dialog.connect_response(None, move |dialog, _| {
        dialog.close();
    });
    dialog.present(Some(parent));
}

fn about_body_with_version(version: &str) -> String {
    format!(
        "FlowMux - Agent Workflow Multiplexer Terminal\n\n\
         FlowMux was inspired by the cmux (macOS) project.\n\n\
         Maintained by JSUYA (Junsu Choi).\n\
         <a href=\"https://github.com/flowmux-ai/flowmux-terminal\">https://github.com/flowmux-ai/flowmux-terminal</a>\n\n\
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

pub(crate) fn row(label_text: &str, value_widget: &impl IsA<gtk::Widget>) -> gtk::Box {
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
#[allow(clippy::too_many_arguments)]
fn collect_options(
    spin: &gtk::SpinButton,
    drop: &gtk::DropDown,
    focus_color: &gtk::ColorDialogButton,
    opacity_spin: &gtk::SpinButton,
    persist_check: &gtk::CheckButton,
    auto_resume_check: &gtk::CheckButton,
    scrollback_check: &gtk::CheckButton,
    system_notify_switch: &gtk::Switch,
    agent_bar_switch: &gtk::Switch,
    cursor_blink_switch: &gtk::Switch,
    blink_interval_spin: &gtk::SpinButton,
    family_drop: &gtk::DropDown,
    font_size_spin: &gtk::SpinButton,
    families: &[Option<String>],
    default_font_size: f32,
    agent_notification_target: AgentNotificationTarget,
    keybindings: &KeybindingOverrides,
    theme_selection: &crate::ui::theme_tab::ThemeSelection,
) -> Options {
    let zoom = Options::clamp_zoom(spin.value_as_int().max(0) as u16);
    let engine = engine_options()
        .get(drop.selected() as usize)
        .cloned()
        .unwrap_or(BrowserEngine::Webkit);
    let color_hex = color_button_hex(focus_color);
    let opacity =
        Options::clamp_focus_border_opacity(opacity_spin.value_as_int().clamp(0, 255) as u8);
    // Index 0 is the "System default (theme)" sentinel → `None` (inherit the
    // theme font). Any other index maps back to a concrete family name.
    let font_family = families
        .get(family_drop.selected() as usize)
        .cloned()
        .flatten();
    // Leaving the size at the theme default keeps `None` so the terminal still
    // inherits the theme size; only a deliberate change pins an explicit size.
    let size_val = font_size_spin.value() as f32;
    let font_size = if (size_val - default_font_size).abs() < 0.05 {
        None
    } else {
        Some(size_val)
    };
    Options {
        zoom_percent: zoom,
        default_browser_engine: engine,
        focus_border_color: color_hex,
        focus_border_opacity: opacity,
        persist_browser_session: persist_check.is_active(),
        auto_resume_agent_sessions: auto_resume_check.is_active(),
        restore_terminal_scrollback: scrollback_check.is_active(),
        system_notifications_enabled: system_notify_switch.is_active(),
        agent_bar_enabled: agent_bar_switch.is_active(),
        cursor_blink: cursor_blink_switch.is_active(),
        cursor_blink_interval_ms: Options::clamp_cursor_blink_interval(
            blink_interval_spin.value() as u32
        ),
        font_family,
        font_size,
        agent_notification_target,
        theme: theme_selection.theme.clone(),
        theme_overrides: theme_selection.overrides.clone(),
        keybindings: keybindings.clone(),
    }
}

/// Widgets + index→family map for the terminal font picker. `families[i]`
/// is the [`Options::font_family`] value the DropDown's index `i` represents;
/// index 0 is the "System default (theme)" sentinel and maps to `None`.
struct FontWidgets {
    family_drop: gtk::DropDown,
    size_spin: gtk::SpinButton,
    families: Vec<Option<String>>,
}

/// Build the font family DropDown (installed curated developer fonts + a
/// "System default" sentinel) and the size SpinButton, seeded from the user's
/// current override or the resolved theme font.
fn build_font_widgets(
    parent: &adw::ApplicationWindow,
    current: &Options,
    default_family: &str,
    default_size: f32,
) -> FontWidgets {
    // Build the index→Option<String> map. Index 0 = inherit the theme font.
    let mut families: Vec<Option<String>> = vec![None];
    let mut labels: Vec<String> = vec![format!("System default ({default_family})")];
    // Show only curated developer fonts that are actually installed, ordered
    // by popularity, so the dropdown is not flooded with every monospace face
    // fontconfig reports (emoji fonts, generic aliases, PostScript clones).
    let mut shown = recommended_families(&monospace_families(parent));
    // Keep the user's current pick even if it is not a curated dev font (e.g.
    // a face they set by hand), so opening the dialog never drops their choice.
    if let Some(fam) = current.font_family.as_deref() {
        if !shown.iter().any(|f| f == fam) {
            shown.push(fam.to_string());
        }
    }
    for fam in shown {
        labels.push(fam.clone());
        families.push(Some(fam));
    }

    let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    let family_drop = gtk::DropDown::from_strings(&label_refs);
    let selected = current
        .font_family
        .as_deref()
        .and_then(|fam| families.iter().position(|f| f.as_deref() == Some(fam)))
        .unwrap_or(0);
    family_drop.set_selected(selected as u32);
    family_drop.set_enable_search(true);

    let initial_size = current.font_size.unwrap_or(default_size).clamp(4.0, 96.0);
    let adj = gtk::Adjustment::new(initial_size as f64, 4.0, 96.0, 1.0, 4.0, 0.0);
    let size_spin = gtk::SpinButton::new(Some(&adj), 1.0, 0);
    size_spin.set_numeric(true);
    size_spin.set_value(initial_size as f64);
    size_spin.set_width_chars(6);

    FontWidgets {
        family_drop,
        size_spin,
        families,
    }
}

/// Curated free / open-source developer monospace families, ordered by
/// popularity (web-researched: JetBrains Mono, Fira Code, Cascadia, Source
/// Code Pro, Hack, IBM Plex Mono, Iosevka, …). The font dropdown shows only
/// installed families that match one of these prefixes, so the user picks
/// from known coding fonts instead of every monospace face fontconfig knows.
/// Matching is by lowercase prefix, so foundry variants ("Fira Code Retina",
/// "Iosevka Term", "JetBrains Mono NL", "Cascadia Code PL") are kept.
const CURATED_DEV_FONTS: &[&str] = &[
    // Mainstream coding fonts (all OFL / Apache / BSD — freely redistributable).
    "JetBrains Mono",
    "Fira Code",
    "Cascadia Code",
    "Cascadia Mono",
    "Maple Mono",
    "Monaspace",
    "Geist Mono",
    "Commit Mono",
    "Source Code Pro",
    "Hack",
    "IBM Plex Mono",
    "Iosevka",
    "Intel One Mono",
    "0xProto",
    "Recursive Mono",
    "Inconsolata",
    "Roboto Mono",
    "Ubuntu Mono",
    "Red Hat Mono",
    "Victor Mono",
    "Fantasque Sans Mono",
    "Hasklig",
    "Mononoki",
    "Hermit",
    "Martian Mono",
    "Spline Sans Mono",
    "Overpass Mono",
    "B612 Mono",
    "Azeret Mono",
    "Anonymous Pro",
    "Space Mono",
    "Go Mono",
    "Departure Mono",
    "Sometype Mono",
    // CJK / Korean developer fonts (OFL) — useful for mixed Hangul + code.
    "Sarasa Mono",
    "Sarasa Term",
    "Sarasa Fixed",
    "D2Coding",
    "Nanum Gothic Coding",
    "NanumGothicCoding",
    // Common Linux defaults / fallbacks.
    "Noto Sans Mono",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Cousine",
    "Fira Mono",
    "PT Mono",
];

/// Filter installed monospace families down to the curated developer fonts,
/// returned in [`CURATED_DEV_FONTS`] popularity order (variants grouped under
/// their base, in the alphabetical order `installed` already carries).
fn recommended_families(installed: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for base in CURATED_DEV_FONTS {
        let base = base.to_lowercase();
        for fam in installed {
            let lower = fam.to_lowercase();
            let matches = lower == base || lower.starts_with(&format!("{base} "));
            if matches && !out.iter().any(|f| f == fam) {
                out.push(fam.clone());
            }
        }
    }
    out
}

/// System-installed monospace font families, de-duplicated and sorted
/// case-insensitively. Pulled from the widget's Pango context so it reflects
/// exactly what the terminal can render.
fn monospace_families(widget: &impl IsA<gtk::Widget>) -> Vec<String> {
    let ctx = widget.pango_context();
    let mut names: Vec<String> = ctx
        .list_families()
        .into_iter()
        .filter(|f| f.is_monospace())
        .map(|f| f.name().to_string())
        .collect();
    names.sort_by_key(|s| s.to_lowercase());
    names.dedup();
    names
}

/// Serialize the current gtk::ColorDialogButton RGBA as `#rrggbb`.
/// The alpha channel is ignored because opacity has its own option.
fn color_button_hex(button: &gtk::ColorDialogButton) -> String {
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

/// Toggle for [`Options::system_notifications_enabled`]. When off,
/// notifications still land in the in-app bell list but no desktop toast is
/// sent to the system notification service. Rendered as a switch so it reads
/// as an on/off feature toggle rather than a multi-select option.
fn build_system_notify_switch(initial: bool) -> gtk::Switch {
    let toggle = gtk::Switch::new();
    toggle.set_active(initial);
    // The switch sits in a hexpand row; keep it natural-sized and right-aligned
    // so it lines up with the other right-hand value widgets.
    toggle.set_halign(gtk::Align::End);
    toggle.set_valign(gtk::Align::Center);
    toggle
}

fn build_agent_bar_switch(initial: bool) -> gtk::Switch {
    let toggle = gtk::Switch::new();
    toggle.set_active(initial);
    toggle.set_halign(gtk::Align::End);
    toggle.set_valign(gtk::Align::Center);
    toggle
}

fn build_cursor_blink_switch(initial: bool) -> gtk::Switch {
    let toggle = gtk::Switch::new();
    toggle.set_active(initial);
    toggle.set_halign(gtk::Align::End);
    toggle.set_valign(gtk::Align::Center);
    toggle
}

/// SpinButton for the cursor blink half-period in milliseconds, clamped to
/// `[CURSOR_BLINK_INTERVAL_MIN, CURSOR_BLINK_INTERVAL_MAX]`.
fn build_blink_interval_spin(initial: u32) -> gtk::SpinButton {
    let initial = Options::clamp_cursor_blink_interval(initial);
    let adj = gtk::Adjustment::new(
        initial as f64,
        CURSOR_BLINK_INTERVAL_MIN as f64,
        CURSOR_BLINK_INTERVAL_MAX as f64,
        10.0,
        50.0,
        0.0,
    );
    let spin = gtk::SpinButton::new(Some(&adj), 1.0, 0);
    spin.set_numeric(true);
    spin.set_halign(gtk::Align::End);
    spin
}

/// Parse `#rrggbb` or another hex form as GdkRGBA and seed the
/// ColorDialogButton. Fall back to the default pale yellow on parse failure.
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
            "<a href=\"https://github.com/flowmux-ai/flowmux-terminal\">https://github.com/flowmux-ai/flowmux-terminal</a>"
        ));
        assert!(body.ends_with("Version: v9.8.7-6"));
    }

    #[test]
    fn update_tab_offers_ignored_release_for_later_install() {
        let props = update_tab_props(&BannerState::Ignored(Version(0, 8, 0)));
        assert_eq!(props.update_version, Some(Version(0, 8, 0)));
        assert_eq!(props.update_label.as_deref(), Some("Update to v0.8.0"));
        assert!(props.check_sensitive);
    }

    #[test]
    fn update_tab_hides_install_action_when_current_or_running() {
        let current = update_tab_props(&BannerState::Current);
        assert!(current.update_version.is_none());
        assert!(current.check_sensitive);

        let running = update_tab_props(&BannerState::Running(Stage::Installing, Version(0, 8, 0)));
        assert!(running.update_version.is_none());
        assert!(!running.check_sensitive);
    }

    #[test]
    fn ignored_release_can_start_from_the_update_tab() {
        use std::cell::Cell;

        let selected = Cell::new(None);
        let next = request_update(&BannerState::Ignored(Version(0, 8, 0)), &|version| {
            selected.set(Some(version));
            true
        });

        assert_eq!(selected.get(), Some(Version(0, 8, 0)));
        assert_eq!(
            next,
            Some(BannerState::Running(Stage::Fetching, Version(0, 8, 0)))
        );
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
    /// existing options. Headless environments without a display skip the
    /// assertion; when GTK init succeeds, the active state must match.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn persist_check_reflects_initial_value() {
        if gtk::init().is_err() {
            return;
        }
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
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn collect_options_round_trips_persist_browser_session() {
        if gtk::init().is_err() {
            return;
        }
        let zoom = build_zoom_spin(120);
        let engine = build_engine_drop(&BrowserEngine::Firefox);
        let focus_color = build_focus_color_button("#abcdef");
        let opacity = build_focus_opacity_row(40);
        let persist_off = build_persist_check(false);
        let auto_resume_off = build_persist_check(false);
        let scrollback_on = build_persist_check(true);
        // Two-entry font picker: index 0 = inherit, index 1 = a concrete family.
        let family_drop = gtk::DropDown::from_strings(&["System default", "Fira Code"]);
        let families = vec![None, Some("Fira Code".to_string())];
        let size_spin = gtk::SpinButton::new(
            Some(&gtk::Adjustment::new(12.0, 4.0, 96.0, 1.0, 4.0, 0.0)),
            1.0,
            0,
        );
        let kb = KeybindingOverrides::default();
        let notify_on = build_system_notify_switch(true);
        let agent_bar_on = build_agent_bar_switch(true);
        let blink_on = build_cursor_blink_switch(true);
        let blink_interval = build_blink_interval_spin(300);
        let opts = collect_options(
            &zoom,
            &engine,
            &focus_color,
            &opacity.spin,
            &persist_off,
            &auto_resume_off,
            &scrollback_on,
            &notify_on,
            &agent_bar_on,
            &blink_on,
            &blink_interval,
            &family_drop,
            &size_spin,
            &families,
            12.0,
            AgentNotificationTarget::Both,
            &kb,
            &crate::ui::theme_tab::ThemeSelection::default(),
        );
        assert!(opts.agent_bar_enabled);
        assert!(opts.cursor_blink);
        assert_eq!(opts.cursor_blink_interval_ms, 300);
        assert_eq!(
            opts.agent_notification_target,
            AgentNotificationTarget::Both
        );
        assert_eq!(opts.zoom_percent, 120);
        assert_eq!(opts.default_browser_engine, BrowserEngine::Firefox);
        assert_eq!(opts.focus_border_opacity, 40);
        assert!(!opts.persist_browser_session);
        assert!(!opts.auto_resume_agent_sessions);
        assert!(opts.restore_terminal_scrollback);
        // Index 0 selected + size left at the theme default → font inherits.
        assert_eq!(opts.font_family, None);
        assert_eq!(opts.font_size, None);

        // Pick the concrete family and bump the size → both pin to overrides.
        family_drop.set_selected(1);
        size_spin.set_value(15.0);
        let persist_on = build_persist_check(true);
        let auto_resume_on = build_persist_check(true);
        let scrollback_off = build_persist_check(false);
        let notify_off = build_system_notify_switch(false);
        let agent_bar_off = build_agent_bar_switch(false);
        let blink_off = build_cursor_blink_switch(false);
        let blink_interval = build_blink_interval_spin(800);
        let opts = collect_options(
            &zoom,
            &engine,
            &focus_color,
            &opacity.spin,
            &persist_on,
            &auto_resume_on,
            &scrollback_off,
            &notify_off,
            &agent_bar_off,
            &blink_off,
            &blink_interval,
            &family_drop,
            &size_spin,
            &families,
            12.0,
            AgentNotificationTarget::Workspace,
            &kb,
            &crate::ui::theme_tab::ThemeSelection::default(),
        );
        assert!(!opts.cursor_blink);
        assert_eq!(opts.cursor_blink_interval_ms, 800);
        assert_eq!(
            opts.agent_notification_target,
            AgentNotificationTarget::Workspace
        );
        assert!(opts.persist_browser_session);
        assert!(opts.auto_resume_agent_sessions);
        assert!(!opts.restore_terminal_scrollback);
        assert!(!opts.system_notifications_enabled);
        assert!(!opts.agent_bar_enabled);
        assert_eq!(opts.font_family, Some("Fira Code".to_string()));
        assert_eq!(opts.font_size, Some(15.0));
    }

    #[test]
    fn recommended_families_filters_and_orders_by_popularity() {
        // Mixed list as fontconfig would report it (alphabetical), with junk
        // faces that must be dropped and a couple of curated variants.
        let installed = vec![
            "DejaVu Sans Mono".to_string(),
            "Fira Code Retina".to_string(),
            "JetBrains Mono".to_string(),
            "Liberation Mono".to_string(),
            "Monospace".to_string(),
            "Nimbus Mono PS".to_string(),
            "Noto Color Emoji".to_string(),
        ];
        let got = recommended_families(&installed);
        // Junk (generic alias, PostScript clone, emoji) is gone; curated fonts
        // come back in CURATED_DEV_FONTS popularity order, not alphabetical.
        assert_eq!(
            got,
            vec![
                "JetBrains Mono".to_string(),
                "Fira Code Retina".to_string(),
                "DejaVu Sans Mono".to_string(),
                "Liberation Mono".to_string(),
            ]
        );
    }

    #[test]
    fn recommended_families_dedupes_and_skips_unknown() {
        let installed = vec!["Comic Sans MS".to_string(), "Hack".to_string()];
        assert_eq!(recommended_families(&installed), vec!["Hack".to_string()]);
    }

    /// GTK init is needed to verify that the slider and SpinButton share the
    /// same Adjustment. Headless environments skip this; when GTK starts, the
    /// test checks that the built widgets move together.
    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn focus_opacity_row_widgets_share_adjustment_and_clamp_initial() {
        if gtk::init().is_err() {
            return;
        }
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
