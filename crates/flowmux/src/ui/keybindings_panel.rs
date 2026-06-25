// SPDX-License-Identifier: GPL-3.0-or-later
//! Keybindings tab inside the options dialog.
//!
//! Lists every editable [`ActionId`] with its currently bound
//! accelerators and lets the user edit, capture, unbind, or reset each
//! one. Edits are written into a shared `Rc<RefCell<KeybindingOverrides>>`
//! that the parent dialog hands to `collect_options` when the user
//! clicks OK, so changes only persist on OK — Cancel or window close
//! discards everything.
//!
//! Live application is not wired here. The parent [`super::options_dialog`]
//! shows a "Changes take effect after restart" hint, and the install
//! routine in [`crate::keybindings::install_accels`] re-reads
//! `options.json` on the next launch.
//!
//! Layout:
//!
//! * [Restart hint label]
//! * Scrollable [`gtk::ListBox`] grouped logically (pane / workspace /
//!   tabs / clipboard / window).
//! * "Reset all keybindings to defaults" footer button.
//!
//! Each row is an [`adw::ActionRow`] with the action label, a current
//! accel chip, and an Edit button. The edit dialog accepts a
//! comma-separated list of accel strings, plus a Capture button that
//! listens for one key press and writes the formatted accel back into
//! the entry.

use adw::prelude::*;
use flowmux_config::keybindings::{default_accels, ActionId, KeybindingOverrides};
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;

/// Shared editable state passed in by the parent dialog. Edits made
/// inside the panel mutate this cell; the parent reads it on OK.
pub type SharedOverrides = Rc<RefCell<KeybindingOverrides>>;

/// Build the keybindings tab widget.
///
/// `state` is the same `Rc<RefCell<...>>` the dialog later reads inside
/// its OK handler — anything written here is what the user keeps.
pub fn build(state: SharedOverrides) -> gtk::Box {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 8);
    outer.set_margin_top(12);
    outer.set_margin_bottom(12);
    outer.set_margin_start(16);
    outer.set_margin_end(16);

    let hint = gtk::Label::new(Some(
        "Changes take effect immediately when you click OK. Use GTK \
         accelerator syntax (e.g. <Ctrl><Shift>c). Separate multiple \
         shortcuts with commas. Leave the field blank to unbind an action.",
    ));
    hint.set_wrap(true);
    hint.set_max_width_chars(56);
    hint.add_css_class("dim-label");
    hint.set_xalign(0.0);
    outer.append(&hint);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");

    // Track each row's accel label so the global Reset button can
    // refresh the visible chip after wiping the user map.
    let row_refresh: Rc<RefCell<Vec<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(Vec::new()));

    for action in ActionId::editable() {
        let (row, refresh) = build_row(action, state.clone());
        list.append(&row);
        row_refresh.borrow_mut().push(refresh);
    }

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_min_content_height(360);
    scroll.set_child(Some(&list));
    outer.append(&scroll);

    let reset_all = gtk::Button::with_label("Reset all keybindings to defaults");
    reset_all.add_css_class("destructive-action");
    {
        let state = state.clone();
        let row_refresh = row_refresh.clone();
        reset_all.connect_clicked(move |_| {
            state.borrow_mut().clear_all();
            for refresh in row_refresh.borrow().iter() {
                refresh();
            }
        });
    }
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    footer.append(&spacer);
    footer.append(&reset_all);
    outer.append(&footer);

    outer
}

/// Build one row for `action` and return both the row widget and a
/// closure the global Reset path calls to refresh the visible accel
/// chip after the underlying `KeybindingOverrides` changes.
fn build_row(action: ActionId, state: SharedOverrides) -> (adw::ActionRow, Rc<dyn Fn()>) {
    let row = adw::ActionRow::new();
    row.set_title(action.label());
    row.set_subtitle(action.as_str());

    let accel_label = gtk::Label::new(None);
    accel_label.add_css_class("dim-label");
    accel_label.set_xalign(1.0);
    accel_label.set_selectable(false);
    refresh_accel_label(&accel_label, &state.borrow(), action);

    let edit_btn = gtk::Button::with_label("Edit");
    edit_btn.set_valign(gtk::Align::Center);

    row.add_suffix(&accel_label);
    row.add_suffix(&edit_btn);

    {
        let state = state.clone();
        let accel_label = accel_label.clone();
        edit_btn.connect_clicked(move |btn| {
            present_edit_dialog(btn, action, state.clone(), accel_label.clone());
        });
    }

    let refresh: Rc<dyn Fn()> = {
        let accel_label = accel_label.clone();
        let state = state.clone();
        Rc::new(move || {
            refresh_accel_label(&accel_label, &state.borrow(), action);
        })
    };

    (row, refresh)
}

fn refresh_accel_label(label: &gtk::Label, overrides: &KeybindingOverrides, action: ActionId) {
    let accels = match overrides.get(action) {
        Some(user) => user.to_vec(),
        None => default_accels(action)
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
    };
    if accels.is_empty() {
        label.set_text("(unbound)");
    } else {
        label.set_text(&accels.join(", "));
    }
}

/// Modal "Edit shortcut" dialog. Reads the current accels for `action`,
/// lets the user retype them or capture a single press, then writes the
/// result into `state` and refreshes the row's accel chip.
fn present_edit_dialog(
    anchor: &gtk::Button,
    action: ActionId,
    state: SharedOverrides,
    accel_label: gtk::Label,
) {
    let parent = anchor.root().and_then(|r| r.downcast::<gtk::Window>().ok());

    let dialog = gtk::Window::builder()
        .modal(true)
        .default_width(420)
        .default_height(220)
        .title(format!("Edit shortcut: {}", action.label()))
        .build();
    if let Some(p) = &parent {
        dialog.set_transient_for(Some(p));
    }

    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.set_margin_top(16);
    body.set_margin_bottom(16);
    body.set_margin_start(20);
    body.set_margin_end(20);

    let info = gtk::Label::new(Some(
        "Use GTK accelerator syntax. Comma-separate to bind several keys. \
         Empty means unbind.",
    ));
    info.set_wrap(true);
    info.set_max_width_chars(46);
    info.add_css_class("dim-label");
    info.set_xalign(0.0);
    body.append(&info);

    let initial = current_accels_for(&state.borrow(), action).join(", ");
    let entry = gtk::Entry::new();
    entry.set_text(&initial);
    entry.set_hexpand(true);
    body.append(&entry);

    let error_label = gtk::Label::new(None);
    error_label.add_css_class("error");
    error_label.set_xalign(0.0);
    error_label.set_visible(false);
    body.append(&error_label);

    let btn_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let capture_btn = gtk::Button::with_label("Capture key…");
    let reset_btn = gtk::Button::with_label("Reset");
    let unbind_btn = gtk::Button::with_label("Unbind");
    let cancel_btn = gtk::Button::with_label("Cancel");
    let ok_btn = gtk::Button::with_label("OK");
    ok_btn.add_css_class("suggested-action");
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    btn_row.append(&capture_btn);
    btn_row.append(&reset_btn);
    btn_row.append(&unbind_btn);
    btn_row.append(&spacer);
    btn_row.append(&cancel_btn);
    btn_row.append(&ok_btn);
    body.append(&btn_row);

    dialog.set_child(Some(&body));

    {
        let entry = entry.clone();
        unbind_btn.connect_clicked(move |_| {
            entry.set_text("");
        });
    }
    {
        let entry = entry.clone();
        reset_btn.connect_clicked(move |_| {
            entry.set_text(&default_accels(action).join(", "));
        });
    }
    {
        let dialog = dialog.clone();
        cancel_btn.connect_clicked(move |_| dialog.close());
    }
    {
        let entry = entry.clone();
        capture_btn.connect_clicked(move |btn| {
            present_capture_overlay(btn, entry.clone());
        });
    }
    {
        let dialog = dialog.clone();
        let state = state.clone();
        let entry = entry.clone();
        let accel_label = accel_label.clone();
        let error_label = error_label.clone();
        ok_btn.connect_clicked(move |_| {
            let raw = entry.text().to_string();
            match parse_accel_list(&raw) {
                Ok(accels) => {
                    let mut overrides = state.borrow_mut();
                    if accels_equal_to_default(&accels, action) {
                        // User typed back the default value — drop the
                        // override so the json file stays minimal.
                        overrides.clear(action);
                    } else {
                        overrides.set(action, accels);
                    }
                    drop(overrides);
                    refresh_accel_label(&accel_label, &state.borrow(), action);
                    dialog.close();
                }
                Err(bad) => {
                    error_label.set_text(&format!("Invalid accelerator: {bad}"));
                    error_label.set_visible(true);
                }
            }
        });
    }

    dialog.present();
}

/// Modal capture popup. Listens for one key press, formats it with
/// [`gtk::accelerator_name`], writes the result into `entry`, then
/// closes itself. Esc cancels without writing.
fn present_capture_overlay(anchor: &gtk::Button, entry: gtk::Entry) {
    let parent = anchor.root().and_then(|r| r.downcast::<gtk::Window>().ok());

    let dialog = gtk::Window::builder()
        .modal(true)
        .default_width(320)
        .default_height(140)
        .title("Press a shortcut")
        .build();
    if let Some(p) = &parent {
        dialog.set_transient_for(Some(p));
    }

    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_margin_top(20);
    body.set_margin_bottom(20);
    body.set_margin_start(20);
    body.set_margin_end(20);

    let label = gtk::Label::new(Some(
        "Press the key combination you want to bind.\n(Esc cancels.)",
    ));
    label.set_xalign(0.5);
    body.append(&label);

    dialog.set_child(Some(&body));

    let key_ctl = gtk::EventControllerKey::new();
    {
        let dialog = dialog.clone();
        let entry = entry.clone();
        key_ctl.connect_key_pressed(move |_, keyval, _keycode, state| {
            // Drop pure-modifier presses so a stray Ctrl while reaching
            // for the real combo does not register as the shortcut.
            if is_modifier_key(keyval) {
                return glib::Propagation::Stop;
            }
            if keyval == gtk::gdk::Key::Escape {
                dialog.close();
                return glib::Propagation::Stop;
            }
            let mods = state & default_mod_mask();
            let accel = gtk::accelerator_name(keyval, mods);
            let accel = accel.to_string();
            if !accel.is_empty() && gtk::accelerator_parse(&accel).is_some() {
                entry.set_text(&accel);
            }
            dialog.close();
            glib::Propagation::Stop
        });
    }
    dialog.add_controller(key_ctl);

    dialog.present();
}

/// Comma-split an accelerator entry, trim each piece, drop empties,
/// and validate every survivor. On the first invalid accel returns
/// the offending string for the caller to surface in the error label.
pub fn parse_accel_list(raw: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for piece in raw.split(',') {
        let p = piece.trim();
        if p.is_empty() {
            continue;
        }
        if gtk::accelerator_parse(p).is_none() {
            return Err(p.to_string());
        }
        out.push(p.to_string());
    }
    Ok(out)
}

/// Same comparison the `OK` handler uses to drop overrides that match
/// the default — keeps `options.json` from filling up with redundant
/// entries when the user resets via the dialog.
fn accels_equal_to_default(accels: &[String], action: ActionId) -> bool {
    let def: Vec<&str> = default_accels(action).to_vec();
    if accels.len() != def.len() {
        return false;
    }
    accels.iter().zip(def.iter()).all(|(a, b)| a == *b)
}

fn current_accels_for(overrides: &KeybindingOverrides, action: ActionId) -> Vec<String> {
    overrides
        .get(action)
        .map(|user| user.to_vec())
        .unwrap_or_else(|| {
            default_accels(action)
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        })
}

/// Modifier subset that flowmux accepts in user shortcuts. GTK4 dropped
/// the `DEFAULT_MOD_MASK` constant exposed by gtk3-rs, so the mask is
/// built explicitly from the same five primary modifier bits a normal
/// keyboard shortcut needs (Shift/Ctrl/Alt/Super/Meta). Lock and Mode
/// switch bits are dropped so caps-lock or NumLock state cannot leak
/// into the recorded accel.
fn default_mod_mask() -> gtk::gdk::ModifierType {
    use gtk::gdk::ModifierType;
    ModifierType::SHIFT_MASK
        | ModifierType::CONTROL_MASK
        | ModifierType::ALT_MASK
        | ModifierType::SUPER_MASK
        | ModifierType::META_MASK
}

/// Pure-modifier keyvals — pressing only Ctrl/Shift/Alt/Super on the
/// capture overlay should not register as the shortcut, otherwise the
/// user can never type a multi-modifier combo because the first
/// modifier press would close the dialog.
fn is_modifier_key(keyval: gtk::gdk::Key) -> bool {
    use gtk::gdk::Key;
    matches!(
        keyval,
        Key::Control_L
            | Key::Control_R
            | Key::Shift_L
            | Key::Shift_R
            | Key::Alt_L
            | Key::Alt_R
            | Key::Super_L
            | Key::Super_R
            | Key::Meta_L
            | Key::Meta_R
            | Key::Hyper_L
            | Key::Hyper_R
            | Key::ISO_Level3_Shift
    )
}

/// Conflict report: every accel string bound to two or more actions
/// after applying the user's overrides. Empty result means the OK
/// handler can save without prompting.
pub fn detect_conflicts(overrides: &KeybindingOverrides) -> Vec<(String, Vec<ActionId>)> {
    use std::collections::BTreeMap;
    let mut by_accel: BTreeMap<String, Vec<ActionId>> = BTreeMap::new();
    for (action, accels) in overrides.resolve() {
        for accel in accels {
            by_accel.entry(accel).or_default().push(action);
        }
    }
    by_accel
        .into_iter()
        .filter(|(_, owners)| owners.len() > 1)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn parse_accel_list_drops_empty_pieces_and_trims() {
        if gtk::init().is_err() {
            return;
        }
        let parsed = parse_accel_list(" <Ctrl>c ,, <Ctrl>v , ").unwrap();
        assert_eq!(parsed, vec!["<Ctrl>c".to_string(), "<Ctrl>v".to_string()]);
    }

    #[cfg(not(target_os = "macos"))]
    #[gtk::test]
    fn parse_accel_list_returns_invalid_piece() {
        if gtk::init().is_err() {
            return;
        }
        let err = parse_accel_list("<Ctrl>c, totally-not-an-accel").unwrap_err();
        assert_eq!(err, "totally-not-an-accel");
    }

    #[test]
    fn parse_accel_list_empty_input_yields_empty_vec() {
        // No GTK call on the empty paths — every piece is filtered out
        // before `gtk::accelerator_parse` runs, so this is safe to run
        // without an initialised display.
        assert!(parse_accel_list("").unwrap().is_empty());
        assert!(parse_accel_list("   ").unwrap().is_empty());
        assert!(parse_accel_list(",,,").unwrap().is_empty());
    }

    #[test]
    fn accels_equal_to_default_matches_built_in_defaults() {
        let copy_default = default_accels(ActionId::Copy)
            .iter()
            .map(|s| (*s).to_string())
            .collect::<Vec<_>>();
        assert!(accels_equal_to_default(&copy_default, ActionId::Copy));
        assert!(!accels_equal_to_default(
            &["<Ctrl>c".to_string()],
            ActionId::Copy
        ));
    }

    #[test]
    fn detect_conflicts_flags_duplicate_accel_across_actions() {
        let mut overrides = KeybindingOverrides::new();
        // Bind close-surface to split-right's default key. Both actions
        // are user-editable, so the conflict surfaces in the report.
        overrides.set(ActionId::CloseSurface, vec!["<Ctrl><Shift>Page_Up".into()]);
        let conflicts = detect_conflicts(&overrides);
        assert!(
            conflicts.iter().any(|(accel, owners)| {
                accel == "<Ctrl><Shift>Page_Up"
                    && owners.contains(&ActionId::CloseSurface)
                    && owners.contains(&ActionId::SplitRight)
            }),
            "expected conflict between close-surface and split-right, got {:?}",
            conflicts
        );
    }

    #[test]
    fn detect_conflicts_clean_when_overrides_are_empty() {
        let overrides = KeybindingOverrides::new();
        assert!(detect_conflicts(&overrides).is_empty());
    }
}
