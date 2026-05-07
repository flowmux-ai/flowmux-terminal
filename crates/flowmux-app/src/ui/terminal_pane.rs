// SPDX-License-Identifier: GPL-3.0-or-later
//! VTE-backed terminal pane.
//!
//! Spawns the user's shell in a PTY and surfaces:
//!
//! * `notification-received` (OSC 99 / Konsole) → forwarded as a
//!   structured notification to the app handler;
//! * `bell` (BEL) → optional attention signal;
//! * `child-exited` → caller decides whether to recycle the pane.
//!
//! For OSC 9 / 777 cmux supports, those are not fired by VTE as
//! distinct signals — agents wishing to use them should pipe through
//! `flowmux notify-stream` (which uses the same parser the GUI uses).
//! We will revisit when libghostty backend lands.

use flowmux_core::{PaneId, SurfaceId};
use gtk::glib;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use vte::prelude::*;

#[derive(Clone)]
pub struct TerminalPane {
    pub id: PaneId,
    /// The VTE widget itself — apply_to_vte / feed call into this.
    pub widget: vte::Terminal,
    /// Widget that goes into a pane-local surface stack.
    pub root: gtk::Widget,
    /// PID of the spawned shell.
    pub pid: Rc<Cell<Option<i32>>>,
}

impl TerminalPane {
    /// Best-effort current working directory of the shell.
    ///
    /// Preference order:
    ///   1. VTE's `current-directory-uri` (OSC 7) — set by zsh / bash
    ///      / fish when the shell announces its cwd. Always reflects
    ///      `cd` exactly.
    ///   2. `/proc/<pid>/cwd` symlink target — works for any spawned
    ///      shell on Linux even without OSC 7 support.
    pub fn current_dir(&self) -> Option<PathBuf> {
        if let Some(uri) = self.widget.current_directory_uri() {
            let s: String = uri.into();
            if !s.is_empty() {
                if let Some(p) = uri_to_path(&s) {
                    return Some(p);
                }
            }
        }
        if let Some(pid) = self.pid.get() {
            if let Ok(p) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
                return Some(p);
            }
        }
        None
    }
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    // file:///foo/bar  → /foo/bar
    // file://host/foo  → /foo  (host is dropped; flowmux is local)
    let rest = uri.strip_prefix("file://")?;
    let path_only = match rest.find('/') {
        Some(idx) => &rest[idx..],
        None => rest,
    };
    Some(PathBuf::from(percent_decode(path_only)))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(a), Some(b)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(a * 16 + b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[derive(Clone)]
pub struct PaneCallbacks {
    pub on_notification: Rc<RefCell<dyn FnMut(PaneId, String, String)>>,
    pub on_bell: Rc<RefCell<dyn FnMut(PaneId)>>,
    pub on_child_exited: Rc<RefCell<dyn FnMut(PaneId, i32)>>,
    pub on_focus: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Per-pane close button on the Overlay + 'Close Pane' menu item.
    pub on_close_pane: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Right-click menu 'Split Right'.
    pub on_split_right: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Right-click menu 'Split Down'.
    pub on_split_down: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Pane-local surface tab activation.
    pub on_activate_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// Pane-local new terminal tab.
    pub on_new_surface: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Pane-local close tab.
    pub on_close_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// Pane-local rename tab.
    pub on_rename_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
}

impl TerminalPane {
    /// Build a fresh terminal widget and spawn `argv` in `cwd`. If
    /// `argv` is empty we fall back to the user's `$SHELL`.
    pub fn spawn(
        id: PaneId,
        argv: Vec<String>,
        cwd: Option<std::path::PathBuf>,
        callbacks: PaneCallbacks,
    ) -> Self {
        let term = vte::Terminal::new();
        term.set_hexpand(true);
        term.set_vexpand(true);
        term.set_scrollback_lines(10_000);
        term.set_audible_bell(false);

        // OSC 99 (Konsole-format) is not exposed as a signal on Ubuntu's
        // VTE 0.76 build — the `notification-received` signal is a
        // Konsole extension compiled out in upstream VTE. We capture
        // OSC notifications via the `flowmux notify-stream` CLI today,
        // and a PTY-tee path is planned in flowmux-terminal so the GUI
        // can subscribe directly without wrapping every command.
        let _unused_notification_cb = &callbacks.on_notification;

        // BEL — generic attention.
        {
            let cb = callbacks.on_bell.clone();
            let id = id;
            term.connect_bell(move |_term| {
                (cb.borrow_mut())(id);
            });
        }

        // Process exit.
        {
            let cb = callbacks.on_child_exited.clone();
            let id = id;
            term.connect_child_exited(move |_term, status| {
                (cb.borrow_mut())(id, status);
            });
        }

        // Focus tracking — keyboard shortcuts (split right/down, etc.)
        // need to know which pane is currently focused.
        {
            let cb = callbacks.on_focus.clone();
            let id = id;
            let focus_ctrl = gtk::EventControllerFocus::new();
            focus_ctrl.connect_enter(move |_| (cb.borrow_mut())(id));
            term.add_controller(focus_ctrl);
        }

        // Right-click — show a context menu with Split / Close.
        // We deliberately avoid PopoverMenu+win.* actions because the
        // action lookup chain can drop through PopoverMenu's internal
        // implementation in some GTK versions; instead we use a plain
        // Popover with Buttons whose connect_clicked closures fire
        // the per-pane callbacks directly through the bridge.
        {
            let on_focus = callbacks.on_focus.clone();
            let on_split_right = callbacks.on_split_right.clone();
            let on_split_down = callbacks.on_split_down.clone();
            let on_close_pane = callbacks.on_close_pane.clone();
            let id = id;
            let term_widget = term.clone();
            let click = gtk::GestureClick::new();
            click.set_button(gtk::gdk::BUTTON_SECONDARY);
            click.connect_pressed(move |gesture, _n_press, x, y| {
                (on_focus.borrow_mut())(id);

                let popover = gtk::Popover::new();
                let v = gtk::Box::new(gtk::Orientation::Vertical, 0);
                v.set_margin_top(4);
                v.set_margin_bottom(4);

                let mk = |label: &str| -> gtk::Button {
                    let b = gtk::Button::with_label(label);
                    b.add_css_class("flat");
                    b.set_halign(gtk::Align::Fill);
                    b.set_hexpand(true);
                    if let Some(label) = b.child().and_downcast::<gtk::Label>() {
                        label.set_xalign(0.0);
                    }
                    b
                };

                let split_r = mk("Split Right");
                let pop = popover.clone();
                let cb = on_split_right.clone();
                split_r.connect_clicked(move |_| {
                    pop.popdown();
                    (cb.borrow_mut())(id);
                });
                v.append(&split_r);

                let split_d = mk("Split Down");
                let pop = popover.clone();
                let cb = on_split_down.clone();
                split_d.connect_clicked(move |_| {
                    pop.popdown();
                    (cb.borrow_mut())(id);
                });
                v.append(&split_d);

                v.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

                let close_p = mk("Close Pane");
                let pop = popover.clone();
                let cb = on_close_pane.clone();
                close_p.connect_clicked(move |_| {
                    pop.popdown();
                    (cb.borrow_mut())(id);
                });
                v.append(&close_p);

                popover.set_child(Some(&v));
                popover.set_parent(&term_widget);
                popover.set_has_arrow(false);
                crate::ui::popover_pos::anchor_at_click(&popover, &term_widget, x, y);
                popover.connect_closed(|p| p.unparent());
                popover.popup();
                gesture.set_state(gtk::EventSequenceState::Claimed);
            });
            term.add_controller(click);
        }

        let argv: Vec<String> = if argv.is_empty() {
            vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())]
        } else {
            argv
        };
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let cwd_str = cwd.as_ref().and_then(|p| p.to_str());

        let pid: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));
        let pid_for_cb = pid.clone();
        term.spawn_async(
            vte::PtyFlags::DEFAULT,
            cwd_str,
            &argv_refs,
            &[], // envv: inherit
            glib::SpawnFlags::DEFAULT,
            || {}, // child setup (runs in child after fork)
            -1,    // no timeout
            gtk::gio::Cancellable::NONE,
            move |result| {
                match result {
                    Ok(pid_value) => {
                        // glib::Pid wraps libc::pid_t (i32 on Linux).
                        pid_for_cb.set(Some(pid_value.0 as i32));
                    }
                    Err(e) => tracing::warn!(error = %e, "vte spawn failed"),
                }
            },
        );

        Self {
            id,
            root: term.clone().upcast(),
            widget: term,
            pid,
        }
    }

    pub fn feed(&self, bytes: &[u8]) {
        self.widget.feed_child(bytes);
    }
}
