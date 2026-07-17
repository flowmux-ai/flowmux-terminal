// SPDX-License-Identifier: GPL-3.0-or-later
//! Theme tab for the options dialog.
//!
//! A list of built-in theme presets (from `flowmux-config`) with color
//! swatches, plus per-color override pickers. Every interaction updates
//! the shared [`ThemeSelection`] and calls `on_change` so the dialog can
//! live-preview the look; nothing is persisted until the dialog's OK
//! button collects the state.

use adw::prelude::*;
use flowmux_config::ghostty::GhosttyConfig;
use flowmux_config::options::ThemeOverrides;
use flowmux_config::presets::PRESETS;
use gtk::gdk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Theme selection shared with the options dialog; read on OK.
#[derive(Clone, Default)]
pub struct ThemeSelection {
    /// Preset id, or `None` when the user has not picked one (legacy
    /// theme-file / built-in default behavior).
    pub theme: Option<String>,
    pub overrides: ThemeOverrides,
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Background,
    Foreground,
    Cursor,
    SelectionBackground,
    SelectionForeground,
}

const FIELDS: [(Field, &str); 5] = [
    (Field::Background, "Terminal background"),
    (Field::Foreground, "Terminal text"),
    (Field::Cursor, "Cursor"),
    (Field::SelectionBackground, "Selection background"),
    (Field::SelectionForeground, "Selection text"),
];

fn override_slot(overrides: &mut ThemeOverrides, field: Field) -> &mut Option<String> {
    match field {
        Field::Background => &mut overrides.background,
        Field::Foreground => &mut overrides.foreground,
        Field::Cursor => &mut overrides.cursor,
        Field::SelectionBackground => &mut overrides.selection_background,
        Field::SelectionForeground => &mut overrides.selection_foreground,
    }
}

fn override_value(overrides: &ThemeOverrides, field: Field) -> Option<String> {
    match field {
        Field::Background => overrides.background.clone(),
        Field::Foreground => overrides.foreground.clone(),
        Field::Cursor => overrides.cursor.clone(),
        Field::SelectionBackground => overrides.selection_background.clone(),
        Field::SelectionForeground => overrides.selection_foreground.clone(),
    }
}

/// The theme's own color for `field`, with the same fallbacks
/// `ResolvedTheme::from_ghostty` applies, so the override buttons open
/// showing what is actually on screen.
fn base_color(cfg: &GhosttyConfig, field: Field) -> String {
    let bg = cfg
        .background
        .clone()
        .unwrap_or_else(|| crate::theme::DEFAULT_BG.to_string());
    let fg = cfg
        .foreground
        .clone()
        .unwrap_or_else(|| crate::theme::DEFAULT_FG.to_string());
    match field {
        Field::Background => bg,
        Field::Foreground => fg,
        Field::Cursor => cfg.cursor_color.clone().unwrap_or(fg),
        Field::SelectionBackground => cfg.selection_background.clone().unwrap_or(bg),
        Field::SelectionForeground => cfg.selection_foreground.clone().unwrap_or(fg),
    }
}

/// Base config for the current selection: the preset when one is picked,
/// otherwise the user's theme file (the legacy source).
fn base_config(theme: Option<&str>) -> GhosttyConfig {
    match theme {
        Some(id) => flowmux_config::presets::config(id).unwrap_or_default(),
        None => flowmux_config::theme::load().unwrap_or_default(),
    }
}

fn parse_rgba(color: &str) -> gdk::RGBA {
    gdk::RGBA::parse(color).unwrap_or_else(|_| gdk::RGBA::new(0.0, 0.0, 0.0, 1.0))
}

/// Small rounded color chip used in the preset rows.
fn swatch(color: &str) -> gtk::Widget {
    let rgba = parse_rgba(color);
    let area = gtk::DrawingArea::new();
    area.set_content_width(14);
    area.set_content_height(14);
    area.set_valign(gtk::Align::Center);
    area.set_draw_func(move |_, cr, w, h| {
        cr.set_source_rgba(
            rgba.red() as f64,
            rgba.green() as f64,
            rgba.blue() as f64,
            1.0,
        );
        cr.rectangle(0.0, 0.0, w as f64, h as f64);
        let _ = cr.fill();
    });
    area.upcast()
}

/// bg, fg, and ANSI colors 1..=6 — enough to recognize a scheme at a glance.
fn swatch_colors(cfg: &GhosttyConfig) -> Vec<String> {
    let mut colors = vec![
        base_color(cfg, Field::Background),
        base_color(cfg, Field::Foreground),
    ];
    for i in 1..=6 {
        colors.push(
            cfg.palette[i]
                .clone()
                .unwrap_or_else(|| crate::theme::DEFAULT_PALETTE[i].to_string()),
        );
    }
    colors
}

pub fn build(state: Rc<RefCell<ThemeSelection>>, on_change: Rc<dyn Fn()>) -> gtk::Widget {
    // Suppresses change handlers while widgets are being seeded
    // programmatically (initial selection, reseeding after a preset click).
    let syncing = Rc::new(Cell::new(false));

    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.set_margin_top(16);
    body.set_margin_bottom(16);
    body.set_margin_start(20);
    body.set_margin_end(20);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.add_css_class("boxed-list");
    for preset in PRESETS {
        let cfg = flowmux_config::presets::config(preset.id).unwrap_or_default();
        let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        row_box.set_margin_top(8);
        row_box.set_margin_bottom(8);
        row_box.set_margin_start(10);
        row_box.set_margin_end(10);
        let name = gtk::Label::new(Some(preset.name));
        name.set_xalign(0.0);
        name.set_hexpand(true);
        row_box.append(&name);
        for color in swatch_colors(&cfg) {
            row_box.append(&swatch(&color));
        }
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&row_box));
        list.append(&row);
    }
    body.append(&list);

    let overrides_heading = gtk::Label::new(Some("Custom colors"));
    overrides_heading.set_xalign(0.0);
    overrides_heading.add_css_class("heading");
    overrides_heading.set_margin_top(8);
    body.append(&overrides_heading);

    let override_buttons: Rc<Vec<(Field, gtk::ColorDialogButton)>> = Rc::new(
        FIELDS
            .iter()
            .map(|(field, label)| {
                let color_dialog = gtk::ColorDialog::new();
                color_dialog.set_with_alpha(false);
                let button = gtk::ColorDialogButton::new(Some(color_dialog));
                body.append(&crate::ui::options_dialog::row(label, &button));
                (*field, button)
            })
            .collect(),
    );

    let reset_btn = gtk::Button::with_label("Reset custom colors");
    reset_btn.set_halign(gtk::Align::Start);
    body.append(&reset_btn);

    let seed_buttons = {
        let state = state.clone();
        let buttons = override_buttons.clone();
        let syncing = syncing.clone();
        Rc::new(move || {
            let selection = state.borrow();
            let cfg = base_config(selection.theme.as_deref());
            syncing.set(true);
            for (field, button) in buttons.iter() {
                let color = override_value(&selection.overrides, *field)
                    .unwrap_or_else(|| base_color(&cfg, *field));
                button.set_rgba(&parse_rgba(&color));
            }
            syncing.set(false);
        })
    };
    seed_buttons();

    // Initial selection: the saved preset row, or the Default row when no
    // preset was ever picked (index 0). Guarded so it does not count as a
    // user action — `state.theme` stays `None` until the user clicks.
    let initial_index = state
        .borrow()
        .theme
        .as_deref()
        .and_then(|id| PRESETS.iter().position(|preset| preset.id == id))
        .unwrap_or(0);
    syncing.set(true);
    list.select_row(list.row_at_index(initial_index as i32).as_ref());
    syncing.set(false);

    {
        let state = state.clone();
        let on_change = on_change.clone();
        let seed_buttons = seed_buttons.clone();
        let syncing = syncing.clone();
        list.connect_row_selected(move |_, row| {
            if syncing.get() {
                return;
            }
            let Some(row) = row else {
                return;
            };
            let Some(preset) = PRESETS.get(row.index().max(0) as usize) else {
                return;
            };
            state.borrow_mut().theme = Some(preset.id.to_string());
            seed_buttons();
            on_change();
        });
    }

    for (field, button) in override_buttons.iter() {
        let field = *field;
        let state = state.clone();
        let on_change = on_change.clone();
        let syncing = syncing.clone();
        button.connect_rgba_notify(move |button| {
            if syncing.get() {
                return;
            }
            let rgba = button.rgba();
            let hex = format!(
                "#{:02x}{:02x}{:02x}",
                (rgba.red().clamp(0.0, 1.0) * 255.0).round() as u8,
                (rgba.green().clamp(0.0, 1.0) * 255.0).round() as u8,
                (rgba.blue().clamp(0.0, 1.0) * 255.0).round() as u8,
            );
            *override_slot(&mut state.borrow_mut().overrides, field) = Some(hex);
            on_change();
        });
    }

    {
        let state = state.clone();
        let on_change = on_change.clone();
        let seed_buttons = seed_buttons.clone();
        reset_btn.connect_clicked(move |_| {
            state.borrow_mut().overrides = ThemeOverrides::default();
            seed_buttons();
            on_change();
        });
    }

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_vexpand(true);
    scroller.set_child(Some(&body));
    scroller.upcast()
}
