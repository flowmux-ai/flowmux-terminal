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
//! Add explicit parsing here if those OSC forms need GUI-native signals.

use crate::ui::pane_terminal::PaneCallbacks;
use flowmux_core::{PaneId, SurfaceId};
use gtk::glib;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::rc::Rc;
use vte::prelude::*;

#[derive(Clone)]
pub struct GhosttyPane {
    id: Rc<Cell<PaneId>>,
    /// The VTE widget itself. Sits inside `container` and owns every
    /// event controller, IM context, focus, and PTY child. Theme and
    /// font calls target this widget directly.
    pub widget: vte::Terminal,
    /// Root container exposed to the pane tree. A `gtk::Overlay` whose
    /// main child is `widget` (so the VTE keeps its natural-size
    /// propagation) plus an overlaid `gtk::Scrollbar` on the right edge
    /// bound to the VTE's vadjustment. The Overlay deliberately does
    /// **not** wrap the VTE in a `gtk::Box` — the latter approach
    /// (commit eb2d176, reverted) broke `gtk::Paned` minimum-size
    /// propagation and clipped tig / vim / htop in nested splits.
    pub container: gtk::Overlay,
    /// PID of the spawned shell.
    pub pid: Rc<Cell<Option<i32>>>,
    /// Last cwd returned by [`Self::poll_cwd_if_changed`], so the poller only
    /// reports real changes.
    last_polled_cwd: Rc<RefCell<Option<PathBuf>>>,
    /// Last non-empty text selection VTE reported. Agent TUIs (Claude Code,
    /// Codex) repaint constantly; VTE clears the drag-selection on the next
    /// output frame (`deselect_all` in `process_incoming`), so by the time the
    /// user hits Copy `has_selection()` is already false and the live copy is a
    /// no-op. We snapshot the selected text on every `selection-changed` and
    /// copy from this cache when the live selection is gone. Cleared on the
    /// next primary-button press so a fresh click starts clean.
    last_selection: Rc<RefCell<Option<String>>>,
    /// Owns the forked child + PTY master. Kept alive for the pane's lifetime so
    /// the shell survives; pane close starts bounded off-thread group reaping.
    /// VTE renders/IOs a dup of the same master.
    _pty: Rc<RefCell<Option<flowmux_terminal::pty::Pty>>>,
}

/// Shift+Enter input sequence: VTE-era agent TUIs treat ESC+CR as "insert a
/// literal newline" at the prompt without submitting.
pub const INSERT_NEWLINE_BYTES: &[u8] = b"\x1b\r";

impl GhosttyPane {
    pub fn id(&self) -> PaneId {
        self.id.get()
    }

    pub fn set_pane_id(&self, id: PaneId) {
        self.id.set(id);
    }

    /// Enable/disable the VTE cursor blink. VTE drives the blink interval from
    /// the GTK `gtk-cursor-blink-time` setting, so `interval_ms` is advisory.
    pub fn set_cursor_blink(&self, enabled: bool, _interval_ms: u32) {
        self.widget.set_cursor_blink_mode(if enabled {
            vte::CursorBlinkMode::On
        } else {
            vte::CursorBlinkMode::Off
        });
    }

    /// Feed raw bytes to the child PTY (snapping the view to the bottom first).
    pub fn write_input(&self, bytes: &[u8]) -> std::io::Result<()> {
        scroll_terminal_to_bottom(&self.widget);
        self.widget.feed_child(bytes);
        Ok(())
    }

    /// Replay persisted plain text into VTE's display only. This never writes
    /// into the child PTY, so old prompts and agent output cannot be executed.
    pub fn restore_scrollback(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.widget.feed(&scrollback_replay_bytes(text));
    }

    /// The widget used for focus tracking / identity comparisons in the window
    /// controller. With VTE this is the terminal widget itself.
    #[allow(dead_code)]
    pub fn render_area(&self) -> gtk::Widget {
        self.widget.clone().upcast::<gtk::Widget>()
    }

    /// Return the cwd only if it changed since the last poll.
    pub fn poll_cwd_if_changed(&self) -> Option<PathBuf> {
        let cur = self.current_dir();
        self.record_polled_cwd(cur)
    }

    pub fn record_polled_cwd(&self, cur: Option<PathBuf>) -> Option<PathBuf> {
        let mut last = self.last_polled_cwd.borrow_mut();
        if *last != cur {
            *last = cur.clone();
            cur
        } else {
            None
        }
    }

    /// Apply theme colors to the VTE widget. Palette indices beyond what VTE
    /// stores are ignored; VTE fills the rest with its own xterm defaults.
    pub fn apply_colors(
        &self,
        fg: flowmux_terminal::Rgb,
        bg: flowmux_terminal::Rgb,
        cursor: flowmux_terminal::Rgb,
        palette: &[flowmux_terminal::Rgb],
        selection_bg: Option<flowmux_terminal::Rgb>,
        selection_fg: Option<flowmux_terminal::Rgb>,
    ) {
        let fg_c = rgb_to_rgba(fg);
        let bg_c = rgb_to_rgba(bg);
        let pal: Vec<gtk::gdk::RGBA> = palette.iter().copied().map(rgb_to_rgba).collect();
        self.widget
            .set_colors(Some(&fg_c), Some(&bg_c), &pal.iter().collect::<Vec<_>>());
        self.widget.set_color_cursor(Some(&rgb_to_rgba(cursor)));
        self.widget
            .set_color_highlight(selection_bg.map(rgb_to_rgba).as_ref());
        self.widget
            .set_color_highlight_foreground(selection_fg.map(rgb_to_rgba).as_ref());
    }
    /// Best-effort current working directory of the shell.
    ///
    /// Preference order:
    ///   1. VTE's `current-directory-uri` (OSC 7) — set by zsh / bash
    ///      / fish when the shell announces its cwd. Always reflects
    ///      `cd` exactly.
    ///   2. `/proc/<pid>/cwd` symlink target — works for any spawned
    ///      shell on Linux even without OSC 7 support.
    pub fn current_dir(&self) -> Option<PathBuf> {
        if let Some(path) = self.announced_current_dir() {
            return Some(path);
        }
        if let Some(pid) = self.pid.get() {
            if let Ok(p) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
                return Some(p);
            }
        }
        None
    }

    pub fn announced_current_dir(&self) -> Option<PathBuf> {
        if let Some(uri) = self.widget.current_directory_uri() {
            let s: String = uri.into();
            if !s.is_empty() {
                if let Some(p) = uri_to_path(&s) {
                    return Some(p);
                }
            }
        }
        None
    }

    pub fn root_widget(&self) -> gtk::Widget {
        self.container.clone().upcast::<gtk::Widget>()
    }

    pub fn grab_focus(&self) {
        self.widget.grab_focus();
    }

    pub fn set_font_scale(&self, scale: f64) {
        self.widget.set_font_scale(scale);
    }

    /// Replace the base terminal font. The independent font scale set by
    /// [`Self::set_font_scale`] (global zoom) still multiplies this size.
    pub fn set_font(&self, desc: &gtk::pango::FontDescription) {
        self.widget.set_font(Some(desc));
    }

    /// Copy the current selection to the clipboard, returning `true` when text
    /// was actually placed there. Falls back to [`Self::last_selection`] when
    /// VTE has already dropped the live selection after an app repaint, so Copy
    /// still works inside agent TUIs that redraw between select and copy.
    pub fn copy_selection_to_clipboard(&self) -> bool {
        if self.widget.has_selection() {
            self.widget.copy_clipboard_format(vte::Format::Text);
            return true;
        }
        let cached = self.last_selection.borrow().clone();
        if let Some(text) = cached {
            if !text.is_empty() {
                self.widget.clipboard().set_text(&text);
                return true;
            }
        }
        false
    }

    pub fn paste_clipboard(&self) {
        scroll_terminal_to_bottom(&self.widget);
        self.widget.paste_clipboard();
    }
}

fn scrollback_replay_bytes(text: &str) -> Vec<u8> {
    let mut replay = Vec::with_capacity(text.len() + text.lines().count());
    let mut previous = None;
    for byte in text.bytes() {
        if byte == b'\n' && previous != Some(b'\r') {
            replay.push(b'\r');
        }
        replay.push(byte);
        previous = Some(byte);
    }
    if !text.ends_with('\n') {
        replay.extend_from_slice(b"\r\n");
    }
    replay
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

fn rgb_to_rgba(c: flowmux_terminal::Rgb) -> gtk::gdk::RGBA {
    gtk::gdk::RGBA::new(
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        1.0,
    )
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

impl GhosttyPane {
    /// Build a fresh terminal widget and spawn `argv` in `cwd`. If
    /// `argv` is empty we fall back to the user's `$SHELL`.
    ///
    /// `extra_env` is added to the child's environment as `KEY=VALUE`
    /// pairs. flowmux uses this to inject `FLOWMUX_PANE_ID`, `FLOWMUX_SURFACE_ID`,
    /// `FLOWMUX_WORKSPACE_ID`, `FLOWMUX_SOCKET_PATH`, etc., so terminal-side
    /// agents (claude/codex/opencode) can discover their context without
    /// explicit flags. Build the vector with `flowmux_terminal::agent_pty_env`.
    pub fn spawn(
        id: PaneId,
        surface: SurfaceId,
        argv: Vec<String>,
        cwd: Option<std::path::PathBuf>,
        extra_env: Vec<(String, String)>,
        callbacks: PaneCallbacks,
    ) -> Self {
        let pane_id = Rc::new(Cell::new(id));
        let last_selection: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let term = vte::Terminal::new();
        term.set_hexpand(true);
        term.set_vexpand(true);
        term.set_scrollback_lines(10_000);
        term.set_audible_bell(false);
        // Snap the viewport back to the live cursor row whenever the user
        // types: someone who scrolled up to inspect scrollback should not
        // end up typing off-screen with no visible echo. VTE's built-in
        // `scroll-on-keystroke` covers every key-driven input path
        // (keyboard + IM commit), mirroring the explicit snap-to-bottom the
        // pure-Rust terminal backend does in `write_child` (commit 9e12edb).
        term.set_scroll_on_keystroke(true);

        // Snapshot every non-empty selection. Agent TUIs repaint constantly and
        // VTE drops the drag-selection on the next output frame, so this cache
        // is what Copy reads once `has_selection()` has gone false. Empty
        // notifications (the app-repaint deselect) intentionally do not clear
        // the cache; a fresh primary press does (see install_url_link_handling).
        {
            let cache = last_selection.clone();
            term.connect_selection_changed(move |t| {
                if t.has_selection() {
                    if let Some(text) = t.text_selected(vte::Format::Text) {
                        if !text.is_empty() {
                            *cache.borrow_mut() = Some(text.to_string());
                        }
                    }
                }
            });
        }

        // Wrap the VTE in a `gtk::Overlay` so we can pin a vertical
        // `gtk::Scrollbar` to its right edge without going through a
        // `gtk::Box` (which broke `gtk::Paned` minimum-size propagation
        // in commit eb2d176, since reverted).
        //
        // Some VTE builds (notably the 0.78 source build we use in the
        // 22.04 Flatpak path) hand back a fresh `gtk::Adjustment` from
        // the `vadjustment` property right after construction, before
        // the widget is realized. Binding the scrollbar to that
        // transient adjustment leaves the bar empty / invisible.
        // Instead, create our own `gtk::Adjustment`, push it into the
        // VTE via the `vadjustment` property, and hand the same
        // instance to the `gtk::Scrollbar`. The Adjustment outlives
        // both widgets and stays stable across realization, which is
        // what GTK 4.16+'s scrollbar drawing path needs to keep the
        // thumb visible.
        let scroll_adjustment = gtk::Adjustment::new(0.0, 0.0, 1.0, 1.0, 1.0, 1.0);
        term.set_property("vadjustment", &scroll_adjustment);
        let container = gtk::Overlay::new();
        container.set_hexpand(true);
        container.set_vexpand(true);
        container.set_child(Some(&term));
        let scrollbar = gtk::Scrollbar::new(gtk::Orientation::Vertical, Some(&scroll_adjustment));
        scrollbar.set_halign(gtk::Align::End);
        scrollbar.set_valign(gtk::Align::Fill);
        // Keep a real standalone scrollbar for the 22.04 path, but only show it
        // when VTE reports scrollback. Full-screen TUIs such as claude handle
        // wheel scrolling inside the alternate screen; VTE's adjustment then
        // stays at one page, which would otherwise draw a full-height thumb.
        scrollbar.set_visible(false);
        scrollbar.set_can_focus(false);
        scrollbar.set_width_request(12);
        container.add_overlay(&scrollbar);
        install_terminal_scrollbar_adjustment_sync(&term, &scrollbar);

        // Make inline IME preedit (e.g. a composing Hangul syllable) visible
        // even when the foreground app has hidden the terminal cursor.
        install_preedit_redraw_on_keystroke(&container, &term);

        let pid: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));

        if crate::platform::running_under_wsl() {
            install_wsl_ctrl_c_interrupt_passthrough(&container, &term);
        }

        // On the ibus path, recover Shift+symbol keys (notably `?`) that
        // get swallowed while a Korean input mode is active.
        if ibus_im_module_active() {
            install_ibus_shifted_symbol_passthrough(&container, &term);
        }

        // Order Enter behind a still-composing IME syllable so "안녕하세요"
        // + Enter submits "…요\n" and never "…세\n요". Only the ibus
        // immodule has the asynchronous-commit hazard this fixes.
        if ibus_im_module_active() {
            install_enter_preedit_commit_ordering(&term);
        }

        // Shift+Left/Right: move the cursor like a plain arrow instead
        // of letting VTE send the modified `CSI 1;2 C/D` form, which
        // line editors (Claude Code, shell readline) don't recognise and
        // echo as a stray "C"/"D". flowmux can't offer keyboard text
        // selection on VTE (no public set-selection API), so the least
        // surprising behaviour is a bare cursor move.
        install_shift_arrow_cursor_move(&term);

        let smart_page_enabled = terminal_capture_key_controllers_enabled(env_flag_enabled(
            "FLOWMUX_ENABLE_VTE_CAPTURE_KEYS",
        ));
        if smart_page_enabled {
            install_smart_page_keys(&term);
        }

        // OSC 99 (Konsole-format) is not exposed as a signal on Ubuntu's
        // VTE 0.68 / 0.76 builds — the `notification-received` signal is
        // a Konsole extension compiled out in upstream VTE. We capture
        // OSC 9 / 99 / 777 by wrapping the shell argv with
        // `flowmuxctl pty-tee` (see `wrap_argv_with_pty_tee` below):
        // the helper forks the shell on an inner PTY, snoops every
        // inner→outer byte through `flowmux_notify::OscExtractor`, and
        // forwards parsed notifications to the daemon via
        // `Request::Notify`. The GUI's `on_notification` callback is
        // therefore not the path that fires for real OSCs; the daemon
        // dispatches them directly, identical to a `flowmux notify`
        // CLI call. Notifications are dispatched by the daemon, not this widget.

        // Forward cwd (OSC 7) and window title (OSC 0/2) changes to the
        // controller so the window title / VCS sidebar and per-pane cwd stay in
        // sync, and a new tab inherits the surface's directory.
        {
            let cb = callbacks.on_terminal_cwd_changed.clone();
            let pane_id = pane_id.clone();
            // Forward OSC 7 cwd only on a real change, matching the controller's
            // expectation that the title / VCS / file-browser refresh runs once
            // per `cd` rather than on every redundant announcement.
            let last_cwd: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
            term.connect_current_directory_uri_notify(move |t| {
                if let Some(uri) = t.current_directory_uri() {
                    if let Some(path) = uri_to_path(uri.as_str()) {
                        let mut last = last_cwd.borrow_mut();
                        if last.as_deref() != Some(path.as_path()) {
                            *last = Some(path.clone());
                            drop(last);
                            (cb.borrow_mut())(pane_id.get(), surface, path);
                        }
                    }
                }
            });
        }
        {
            let cb = callbacks.on_terminal_title_changed.clone();
            let pane_id = pane_id.clone();
            let coalesce: Rc<TitleCoalesce> = Rc::new(TitleCoalesce::default());
            term.connect_window_title_notify(move |t| {
                let title = t.window_title().map(|g| g.to_string()).unwrap_or_default();
                if title.is_empty() {
                    return;
                }
                if coalesce.window_open.get() {
                    // Quiet window open: keep only the newest title; the
                    // window timer delivers it (or drops it if it settled
                    // back to the last delivered value).
                    *coalesce.pending.borrow_mut() = Some(title);
                    return;
                }
                if *coalesce.last_sent.borrow() == title {
                    return;
                }
                *coalesce.last_sent.borrow_mut() = title.clone();
                (cb.borrow_mut())(pane_id.get(), surface, title);
                coalesce.window_open.set(true);
                arm_title_coalesce_window(coalesce.clone(), cb.clone(), pane_id.clone(), surface);
            });
        }

        // URL recognition for opening terminal URLs in an internal browser tab
        // via Ctrl+click. A PCRE2 regex match changes hover to the pointer
        // cursor; Ctrl+left-click opens the URL in a new browser tab in the
        // same pane. Plain clicks continue into VTE text selection.
        install_url_link_handling(
            &term,
            id,
            pid.clone(),
            callbacks.on_open_url.clone(),
            callbacks.on_open_image.clone(),
            callbacks.on_open_markdown.clone(),
            last_selection.clone(),
        );
        install_ibus_nav_workaround(&term, smart_page_enabled);

        // Process exit.
        {
            let cb = callbacks.on_child_exited.clone();
            let pane_id = pane_id.clone();
            term.connect_child_exited(move |_term, status| {
                (cb.borrow_mut())(pane_id.get(), status);
            });
        }

        // Focus tracking — keyboard shortcuts (split right/down, etc.)
        // need to know which pane is currently focused.
        {
            let cb = callbacks.on_focus.clone();
            let pane_id = pane_id.clone();
            let focus_ctrl = gtk::EventControllerFocus::new();
            focus_ctrl.connect_enter(move |_| (cb.borrow_mut())(pane_id.get()));
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
            let on_copy_text = callbacks.on_copy_surface_text.clone();
            let surface_for_menu = surface;
            let pane_id = pane_id.clone();
            let term_widget = term.clone();
            let last_selection_for_menu = last_selection.clone();
            let click = gtk::GestureClick::new();
            click.set_button(gtk::gdk::BUTTON_SECONDARY);
            click.connect_pressed(move |gesture, _n_press, x, y| {
                let id = pane_id.get();
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

                // Copy / Paste at the top, mirroring the copy/paste
                // keybindings. Copy is a no-op with no selection so we
                // never clobber the clipboard; Paste lets VTE bracket
                // the text when the app set DECSET 2004.
                let copy = mk("Copy");
                let pop = popover.clone();
                let term_for_copy = term_widget.clone();
                let cache_for_copy = last_selection_for_menu.clone();
                copy.connect_clicked(move |_| {
                    pop.popdown();
                    if term_for_copy.has_selection() {
                        term_for_copy.copy_clipboard_format(vte::Format::Text);
                        return;
                    }
                    // Live selection already cleared by app repaint — copy the
                    // last snapshot instead, matching the Copy keybinding.
                    if let Some(text) = cache_for_copy.borrow().clone() {
                        if !text.is_empty() {
                            term_for_copy.clipboard().set_text(&text);
                        }
                    }
                });
                v.append(&copy);

                let paste = mk("Paste");
                let pop = popover.clone();
                let term_for_paste = term_widget.clone();
                paste.connect_clicked(move |_| {
                    pop.popdown();
                    term_for_paste.paste_clipboard();
                });
                v.append(&paste);

                v.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

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

                let copy_path = mk("Copy path");
                let pop = popover.clone();
                let cb = on_copy_text.clone();
                copy_path.connect_clicked(move |_| {
                    pop.popdown();
                    (cb.borrow_mut())(id, surface_for_menu);
                });
                v.append(&copy_path);

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
            default_shell_argv()
        } else {
            argv
        };
        // Wrap the shell argv with `flowmuxctl pty-tee` so OSC 9/99/777
        // emitted by terminal-side agents (Claude Code, Codex, …)
        // reach the desktop notification subsystem even on VTE
        // versions (0.68 on Ubuntu 22.04, 0.76 on Ubuntu 24.04) that
        // silently drop those escapes. If the helper is missing we
        // fall back to a direct shell spawn so the terminal still
        // works — only the OSC sniffer is lost.
        let argv = wrap_argv_with_pty_tee(argv, id, surface);
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let cwd_str = cwd.as_ref().and_then(|p| p.to_str());

        let mut extra_env = extra_env;
        prepare_terminal_child_env(&mut extra_env);

        let _ = cwd_str;
        // Spawn the child synchronously with a forkpty so the PID is available
        // immediately: the OSC-7-less cwd safety net reads `/proc/<pid>/cwd`, and
        // pane teardown hangs up the shell by pid. VTE then renders and drives
        // I/O on the *same* PTY master through a foreign-pty attach. We hand it a
        // dup of the master and keep flowmux's `Pty` (and thus the child's
        // lifecycle); `watch_child` lets VTE reap the child and fire
        // `child-exited` with the real status.
        let (init_cols, init_rows): (u16, u16) = (80, 24);
        let pty = flowmux_terminal::pty::Pty::spawn(
            &argv_refs,
            cwd.as_deref(),
            &extra_env,
            init_cols,
            init_rows,
        )
        .expect("forkpty spawn");
        pid.set(Some(pty.child_pid()));
        let child_pid = pty.child_pid();
        let dup_fd = unsafe { libc::dup(pty.master_fd()) };
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(dup_fd) };
        let vpty =
            vte::Pty::foreign_sync(owned, gtk::gio::Cancellable::NONE).expect("vte foreign pty");
        term.set_pty(Some(&vpty));
        term.watch_child(glib::Pid(child_pid));

        Self {
            id: pane_id,
            widget: term,
            container,
            pid,
            last_polled_cwd: Rc::new(RefCell::new(None)),
            last_selection,
            _pty: Rc::new(RefCell::new(Some(pty))),
        }
    }

    /// Plain-text dump of the terminal buffer for `flowmux read-screen`.
    /// Read-only — VTE's text extraction (enabled by the `v0_76` feature) does
    /// not mutate the grid, selection, or scroll position.
    pub fn screen_text(&self) -> Option<String> {
        self.widget
            .text_format(vte::Format::Text)
            .map(|g| g.to_string())
    }

    /// Feed `bytes` to the child, but only *after* any IME syllable still
    /// in preedit (e.g. a composing Hangul block) has been committed —
    /// otherwise the bytes overtake ibus's asynchronous commit and the
    /// last syllable lands behind them. This is the Shift+Enter ("insert
    /// newline" in agent TUIs) counterpart to the plain-Enter ordering in
    /// [`install_enter_preedit_commit_ordering`]: typing "안녕하세요" and
    /// pressing Shift+Enter while "요" is still composing must produce
    /// "안녕하세요\n", not "안녕하세\n요".
    ///
    /// Shift+Enter reaches the child through a window accelerator
    /// (`win.insert-newline`), so unlike plain Enter the keypress never
    /// touches the VTE / IME path — the composing syllable is not
    /// committed by it. This is needed for the asynchronous ibus path and
    /// for GTK's macOS IM path, where the native Korean IME can otherwise
    /// commit the last syllable after the injected Shift+Enter bytes. We
    /// force the commit with a focus-cycle flush, feed
    /// `bytes` from VTE's `commit` signal on an idle tick (VTE writes the
    /// committed bytes during that emission, so an idle feed always lands
    /// behind them), and fall back to a direct feed when nothing is
    /// composing (no commit fires). The `commit` handler is one-shot:
    /// armed per call and disconnected on the first commit or the
    /// fallback, so it never disturbs ordinary typing.
    pub fn feed_after_preedit_commit(&self, bytes: &'static [u8]) {
        scroll_terminal_to_bottom(&self.widget);
        if !insert_newline_needs_preedit_commit_ordering() {
            self.widget.feed_child(bytes);
            return;
        }

        let widget = self.widget.clone();
        let done = Rc::new(Cell::new(false));
        let handler_id: Rc<RefCell<Option<glib::SignalHandlerId>>> = Rc::new(RefCell::new(None));

        {
            let done = done.clone();
            let hid = handler_id.clone();
            let id = self.widget.connect_commit(move |t, _text, _size| {
                if done.replace(true) {
                    return;
                }
                let t2 = t.clone();
                glib::idle_add_local_once(move || t2.feed_child(bytes));
                if let Some(id) = hid.borrow_mut().take() {
                    t.disconnect(id);
                }
            });
            *handler_id.borrow_mut() = Some(id);
        }

        flush_pending_preedit(&widget);

        let done_fb = done.clone();
        let hid_fb = handler_id.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(20), move || {
            if done_fb.replace(true) {
                return;
            }
            widget.feed_child(bytes);
            if let Some(id) = hid_fb.borrow_mut().take() {
                widget.disconnect(id);
            }
        });
    }

    pub fn add_controller(&self, controller: impl IsA<gtk::EventController>) {
        self.widget.add_controller(controller);
    }

    pub fn close_pty(&self) {
        if let Some(pty) = self._pty.borrow_mut().take() {
            pty.close_async();
        }
    }
}

fn wrap_argv_with_pty_tee(argv: Vec<String>, pane: PaneId, surface: SurfaceId) -> Vec<String> {
    let Some(ctl) = flowmux_terminal::find_flowmuxctl() else {
        tracing::warn!(
            "flowmuxctl not found next to the GUI binary; OSC 9/99/777 alarms \
             from terminal-side agents will be silently dropped until it is \
             installed. Falling back to a direct shell spawn."
        );
        return argv;
    };
    let mut wrapped = Vec::with_capacity(argv.len() + 6);
    wrapped.push(ctl.display().to_string());
    wrapped.push("pty-tee".to_string());
    wrapped.push("--pane".to_string());
    wrapped.push(pane.to_string());
    wrapped.push("--surface".to_string());
    wrapped.push(surface.to_string());
    wrapped.push("--".to_string());
    wrapped.extend(argv);
    wrapped
}

// ---- URL link handling --------------------------------------------------

/// PCRE2 pattern: match http(s), ftp, and file URLs until whitespace, angle
/// brackets, quotes, or backticks. `(?i)` makes the scheme case-insensitive.
/// A match may include trailing sentence punctuation, so trim it immediately
/// before dispatch.
const URL_REGEX_PATTERN: &str = r#"(?i)(?:https?|ftp|file)://[^\s<>"'`]+"#;
const IMAGE_PATH_REGEX_PATTERN: &str = r#"(?i)(?<![^\s<>"'`])(?:/|~/|\.{1,2}/)?(?:[^\s<>"'`:]+/)*[^\s<>"'`:]+\.(?:gif|svg|png|jpe?g|webp?|lottie|json)"#;
const MARKDOWN_PATH_REGEX_PATTERN: &str = r#"(?i)(?<![^\s<>"'`])(?:/|~/|\.{1,2}/)?(?:[^\s<>"'`:]+/)*[^\s<>"'`:]+\.(?:md|markdown|mdown|mkd|mkdn)"#;

/// PCRE2 compile flags.
///   * PCRE2_MULTILINE (0x400): keep matches working across wrapped terminal output.
///   * PCRE2_UTF (0x80000): VTE passes UTF-8 text to the match engine; without
///     this flag PCRE2 treats input as raw bytes and hover / cursor changes fail.
///   * PCRE2_NO_UTF_CHECK (0x4000_0000): VTE has already validated UTF-8, so skip
///     PCRE2's extra validation cost on each match.
const URL_REGEX_COMPILE_FLAGS: u32 = 0x0000_0400 | 0x0008_0000 | 0x4000_0000;

/// Return a URL with trailing punctuation removed: `.`, `,`, `;`, `:`, `!`,
/// `?`, closing brackets, and quotes. These characters can be intentional in a
/// URL path/query, but in the common user scenario it is better to drop the
/// sentence-ending punctuation than to open a 404 with the final `.` included.
fn trim_url_trailing(s: &str) -> String {
    s.trim_end_matches(|c: char| {
        matches!(
            c,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"' | '`'
        )
    })
    .to_string()
}

#[derive(Debug, PartialEq, Eq)]
enum TerminalClickTarget {
    Url(String),
    Image(PathBuf),
    Markdown(PathBuf),
}

#[derive(Debug, PartialEq, Eq)]
struct PendingTerminalClick {
    target: TerminalClickTarget,
    open_in_system_browser: bool,
}

#[derive(Default)]
struct TerminalClickActivation {
    pending: Option<PendingTerminalClick>,
}

impl TerminalClickActivation {
    fn press(&mut self, target: TerminalClickTarget, open_in_system_browser: bool) {
        self.pending = Some(PendingTerminalClick {
            target,
            open_in_system_browser,
        });
    }

    fn release(&mut self) -> Option<PendingTerminalClick> {
        self.pending.take()
    }

    fn clear(&mut self) {
        self.pending = None;
    }
}

fn is_primary_button_release(event_type: gtk::gdk::EventType, button: Option<u32>) -> bool {
    event_type == gtk::gdk::EventType::ButtonRelease && button == Some(gtk::gdk::BUTTON_PRIMARY)
}

fn terminal_image_path(raw: &str, cwd: Option<&std::path::Path>) -> Option<PathBuf> {
    let trimmed = raw.trim_end_matches(|c: char| {
        matches!(
            c,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"' | '`'
        )
    });
    let path = PathBuf::from(trimmed);
    if !is_supported_image_path(&path) {
        return None;
    }

    if path.is_absolute() {
        return Some(path);
    }

    if let Some(rest) = trimmed.strip_prefix("~/") {
        return std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest));
    }

    cwd.map(|cwd| cwd.join(path))
}

fn terminal_markdown_path(raw: &str, cwd: Option<&std::path::Path>) -> Option<PathBuf> {
    let trimmed = raw.trim_end_matches(|c: char| {
        matches!(
            c,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"' | '`'
        )
    });
    let path = PathBuf::from(trimmed);
    if !is_supported_markdown_path(&path) {
        return None;
    }

    if path.is_absolute() {
        return Some(path);
    }

    if let Some(rest) = trimmed.strip_prefix("~/") {
        return std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest));
    }

    cwd.map(|cwd| cwd.join(path))
}

fn is_supported_image_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "gif" | "svg" | "png" | "jpg" | "jpeg" | "web" | "webp" | "lottie" | "json"
            )
        })
}

fn is_supported_markdown_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "md" | "markdown" | "mdown" | "mkd" | "mkdn"
            )
        })
}

fn terminal_current_dir(term: &vte::Terminal, pid: &Rc<Cell<Option<i32>>>) -> Option<PathBuf> {
    if let Some(uri) = term.current_directory_uri() {
        let s: String = uri.into();
        if !s.is_empty() {
            if let Some(path) = uri_to_path(&s) {
                return Some(path);
            }
        }
    }

    pid.get()
        .and_then(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok())
}

/// AI-agent TUIs animate the OSC 0 title with a spinner. Repainting the tab,
/// side panel, agent bar, and window title for every frame invalidates most of
/// the GSK scene and can make adjacent terminal text shimmer. Deliver the first
/// change immediately, suppress intermediate frames, and deliver the final
/// title only after a complete quiet window.
const TITLE_COALESCE_WINDOW_MS: u64 = 200;

#[derive(Default)]
struct TitleCoalesce {
    /// Last title actually delivered to the callback.
    last_sent: RefCell<String>,
    /// Newest title seen during the current window.
    pending: RefCell<Option<String>>,
    /// Candidate retained until a full window passes without a newer title.
    settling: RefCell<Option<String>>,
    window_open: Cell<bool>,
}

#[derive(Debug, PartialEq, Eq)]
enum TitleWindowResult {
    Rearm,
    Settled(Option<String>),
}

impl TitleCoalesce {
    fn finish_window(&self) -> TitleWindowResult {
        if let Some(title) = self.pending.borrow_mut().take() {
            *self.settling.borrow_mut() = Some(title);
            return TitleWindowResult::Rearm;
        }

        self.window_open.set(false);
        let title = self
            .settling
            .borrow_mut()
            .take()
            .filter(|title| *title != *self.last_sent.borrow());
        if let Some(title) = &title {
            *self.last_sent.borrow_mut() = title.clone();
        }
        TitleWindowResult::Settled(title)
    }
}

fn arm_title_coalesce_window(
    coalesce: Rc<TitleCoalesce>,
    cb: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, String)>>,
    pane_id: Rc<Cell<PaneId>>,
    surface: SurfaceId,
) {
    glib::timeout_add_local_once(
        std::time::Duration::from_millis(TITLE_COALESCE_WINDOW_MS),
        move || match coalesce.finish_window() {
            TitleWindowResult::Rearm => {
                arm_title_coalesce_window(coalesce, cb, pane_id, surface);
            }
            TitleWindowResult::Settled(title) => {
                if let Some(title) = title {
                    (cb.borrow_mut())(pane_id.get(), surface, title);
                }
            }
        },
    );
}

fn install_url_link_handling(
    term: &vte::Terminal,
    pane_id: PaneId,
    pid: Rc<Cell<Option<i32>>>,
    on_open_url: Rc<RefCell<dyn FnMut(PaneId, String)>>,
    on_open_image: Rc<RefCell<dyn FnMut(PaneId, PathBuf)>>,
    on_open_markdown: Rc<RefCell<dyn FnMut(PaneId, PathBuf)>>,
    last_selection: Rc<RefCell<Option<String>>>,
) {
    // 1) Compile and register the regex. If this fails, usually due to a PCRE2
    //    build issue, link recognition is disabled while the terminal itself
    //    keeps working.
    let regex = match vte::Regex::for_match(URL_REGEX_PATTERN, URL_REGEX_COMPILE_FLAGS) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "failed to compile URL regex; link clicking disabled");
            return;
        }
    };
    let url_tag = term.match_add_regex(&regex, 0);
    // Show a pointer cursor on hover. The pointer appears even without Ctrl,
    // but activation requires Ctrl, matching the gnome-terminal UX pattern:
    // always show the hint, gate the action behind the modifier.
    term.match_set_cursor_name(url_tag, "pointer");
    tracing::debug!(%pane_id, tag = url_tag, "URL match registered on terminal");

    let image_regex = match vte::Regex::for_match(IMAGE_PATH_REGEX_PATTERN, URL_REGEX_COMPILE_FLAGS)
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "failed to compile image path regex; image clicking disabled");
            return;
        }
    };
    let image_tag = term.match_add_regex(&image_regex, 0);
    term.match_set_cursor_name(image_tag, "pointer");
    tracing::debug!(%pane_id, tag = image_tag, "image path match registered on terminal");

    let markdown_regex = match vte::Regex::for_match(
        MARKDOWN_PATH_REGEX_PATTERN,
        URL_REGEX_COMPILE_FLAGS,
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "failed to compile markdown path regex; markdown clicking disabled");
            return;
        }
    };
    let markdown_tag = term.match_add_regex(&markdown_regex, 0);
    term.match_set_cursor_name(markdown_tag, "pointer");
    tracing::debug!(%pane_id, tag = markdown_tag, "markdown path match registered on terminal");

    // 2) Left-click gesture. Keep it out of capture phase so VTE's keyboard
    //    IMContext stays on the same path as a plain terminal widget.
    //
    //    The key trap: GtkGestureSingle automatically claims its sequence on
    //    button-press. If we do nothing, that event never reaches other
    //    controllers such as VTE selection drag, so text selection breaks.
    //
    //    Fix: when Ctrl is not held, explicitly set the sequence to Denied so
    //    VTE's selection gesture can claim it. Keep it Claimed only for
    //    Ctrl-clicks so selection does not start.
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_PRIMARY);
    click.set_propagation_phase(gtk::PropagationPhase::Bubble);
    let activation = Rc::new(RefCell::new(TerminalClickActivation::default()));

    let term_widget = term.clone();
    let activation_for_press = activation.clone();
    click.connect_pressed(move |gesture, _n_press, x, y| {
        activation_for_press.borrow_mut().clear();
        // A new primary press starts a fresh interaction: drop the stale
        // selection snapshot so Copy never resurrects text from a selection the
        // user has moved on from. A drag re-populates it via `selection-changed`.
        *last_selection.borrow_mut() = None;

        let modifiers = gesture
            .current_event()
            .map(|e| e.modifier_state())
            .unwrap_or_else(gtk::gdk::ModifierType::empty);
        if !modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
            // Release our sequence so VTE selection drag can handle the same
            // button-press. Otherwise GestureSingle's auto-claim blocks
            // selection permanently.
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        }

        // Prefer OSC 8 hyperlinks, where the URL is attached by escape
        // sequence, then fall back to regex matches. Links produced by ls,
        // git, or build tools that support OSC 8 should win.
        let link = term_widget
            .check_hyperlink_at(x, y)
            .map(|g| TerminalClickTarget::Url(g.to_string()))
            .or_else(|| {
                let (m, tag) = term_widget.check_match_at(x, y);
                let raw = m.map(|g| g.to_string())?;
                if tag == image_tag {
                    let cwd = terminal_current_dir(&term_widget, &pid);
                    terminal_image_path(&raw, cwd.as_deref()).map(TerminalClickTarget::Image)
                } else if tag == markdown_tag {
                    let cwd = terminal_current_dir(&term_widget, &pid);
                    terminal_markdown_path(&raw, cwd.as_deref()).map(TerminalClickTarget::Markdown)
                } else if tag == url_tag {
                    Some(TerminalClickTarget::Url(raw))
                } else {
                    None
                }
            });

        let Some(link) = link else {
            // Ctrl was held, but the click was not on a URL. Treat it as a
            // selection attempt and release the sequence so VTE features such
            // as Ctrl+drag block selection still work.
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        };
        let link = match link {
            TerminalClickTarget::Url(raw) => {
                let url = trim_url_trailing(&raw);
                if url.is_empty() {
                    gesture.set_state(gtk::EventSequenceState::Denied);
                    return;
                }
                TerminalClickTarget::Url(url)
            }
            other => other,
        };
        activation_for_press
            .borrow_mut()
            .press(link, modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK));
        // Claim now so VTE does not begin a selection, but defer opening until
        // button release. On macOS, presenting a viewer during button press
        // lets the matching release return focus to the terminal window.
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });

    let release = gtk::EventControllerLegacy::new();
    release.set_propagation_phase(gtk::PropagationPhase::Capture);
    let term_widget = term.clone();
    release.connect_event(move |_, event| {
        let button = event
            .downcast_ref::<gtk::gdk::ButtonEvent>()
            .map(|event| event.button());
        if !is_primary_button_release(event.event_type(), button) {
            return glib::Propagation::Proceed;
        }
        let Some(pending) = activation.borrow_mut().release() else {
            return glib::Propagation::Proceed;
        };
        match pending.target {
            TerminalClickTarget::Image(path) => {
                tracing::info!(%pane_id, path = %path.display(), "Ctrl+click on terminal image path");
                (on_open_image.borrow_mut())(pane_id, path);
            }
            TerminalClickTarget::Markdown(path) => {
                tracing::info!(%pane_id, path = %path.display(), "Ctrl+click on terminal markdown path");
                (on_open_markdown.borrow_mut())(pane_id, path);
            }
            TerminalClickTarget::Url(url) => {
                if pending.open_in_system_browser {
                    // Ctrl+Shift+click → open in the system default browser instead of
                    // an in-app browser tab.
                    tracing::info!(%pane_id, %url, "Ctrl+Shift+click on terminal URL → open in system browser");
                    let parent = term_widget.root().and_downcast::<gtk::Window>();
                    let launcher = gtk::UriLauncher::new(&url);
                    launcher.launch(
                        parent.as_ref(),
                        gtk::gio::Cancellable::NONE,
                        |res| {
                            if let Err(e) = res {
                                tracing::warn!(error = %e, "failed to open URL in system browser");
                            }
                        },
                    );
                } else {
                    tracing::info!(%pane_id, %url, "Ctrl+click on terminal URL → open in browser tab");
                    (on_open_url.borrow_mut())(pane_id, url);
                }
            }
        }
        // Consume the physical release before VTE can return focus to the
        // terminal window. The open callbacks dispatch asynchronously after
        // this handler returns, so the viewer is presented after mouse-up.
        glib::Propagation::Stop
    });
    term.add_controller(click);
    term.add_controller(release);
}

/// Make VTE redraw the inline IME preedit string (a composing Hangul
/// syllable, Japanese kana, Chinese pinyin, …) on every keystroke, even
/// when the foreground app has hidden the terminal cursor.
///
/// VTE paints the preedit unconditionally in `paint_im_preedit_string()`,
/// but it only *schedules* that repaint from `invalidate_cursor_once()`,
/// which early-returns whenever DECTCEM is off (cursor hidden, `\x1b[?25l`).
/// Claude Code renders its own cursor and keeps the terminal cursor hidden
/// for the entire lifetime of its input box, so each composing keystroke
/// updates the preedit buffer without ever queuing a frame: the syllable
/// only becomes visible when unrelated output (a commit echo, Space) forces
/// a redraw. The result is that `ㅇ`/`아`/`안` never show while composing and
/// Backspace decompose is invisible too. Ghostty invalidates preedit
/// independently of cursor visibility, which is why the same Claude session
/// composes Hangul correctly there; Codex keeps the cursor visible so VTE's
/// own invalidation already fires.
///
/// Fix: attach a capture-phase `EventControllerKey` to the Overlay (an
/// ancestor of the VTE widget). Capture phase visits ancestors before the
/// focused VTE consumes the key, so it observes every key — including the
/// letter / jamo / Backspace keys IBus swallows for composition — and returns
/// `Proceed`, never touching the event, so VTE's IM path is unchanged. The
/// immediate `queue_draw` covers the synchronous IBus path
/// (`IBUS_ENABLE_SYNC_MODE=1`, enabled by default outside WSL); a short
/// follow-up redraw covers async input methods (fcitx, IBus without sync) whose
/// `preedit-changed` lands just after the key event. When the cursor is
/// visible (a normal shell, Codex, vim) the redraw is redundant with VTE's
/// own invalidation and harmless — it is paced by human keystrokes, not
/// terminal output.
fn install_preedit_redraw_on_keystroke(container: &gtk::Overlay, term: &vte::Terminal) {
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    let term_widget = term.clone();
    key.connect_key_pressed(move |_, _keyval, _keycode, _state| {
        term_widget.queue_draw();
        let term_follow = term_widget.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(16), move || {
            term_follow.queue_draw();
        });
        glib::Propagation::Proceed
    });
    container.add_controller(key);
}

/// WSLg can lose plain Ctrl+C before VTE turns it into the terminal VINTR byte.
/// Keep terminal semantics by feeding ETX directly, while leaving Ctrl+Shift+C
/// available for Copy.
fn install_wsl_ctrl_c_interrupt_passthrough(container: &gtk::Overlay, term: &vte::Terminal) {
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    let term_widget = term.clone();
    key.connect_key_pressed(move |_, keyval, _keycode, state| {
        if !is_plain_ctrl_c(keyval, state) {
            return glib::Propagation::Proceed;
        }
        scroll_terminal_to_bottom(&term_widget);
        term_widget.feed_child(b"\x03");
        glib::Propagation::Stop
    });
    container.add_controller(key);
}

fn is_plain_ctrl_c(keyval: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> bool {
    use gtk::gdk::ModifierType;

    let relevant = state
        & (ModifierType::CONTROL_MASK
            | ModifierType::ALT_MASK
            | ModifierType::SHIFT_MASK
            | ModifierType::SUPER_MASK
            | ModifierType::META_MASK);
    relevant == ModifierType::CONTROL_MASK
        && keyval.to_unicode().is_some_and(|ch| ch == 'c' || ch == 'C')
}

/// Recover Shift+symbol keys that the ibus sync-mode path swallows while
/// a Korean (ibus-hangul) input mode is active.
///
/// Symptom: with `GTK_IM_MODULE=ibus` + `IBUS_ENABLE_SYNC_MODE=1` (both
/// forced on the Ubuntu/IBus terminal path), pressing `?` (Shift+`/`) —
/// or other Shift+symbol combos — in Korean mode types nothing.
/// ibus-hangul correctly declines the non-jamo key, but the
/// synchronously-forwarded event never reaches the PTY.
///
/// Fix: at the container capture phase — the same safe vantage point as
/// [`install_preedit_redraw_on_keystroke`], never a capture controller
/// directly on VTE (which would suppress inline Hangul preedit) — feed
/// the layout-resolved character straight to the PTY, committing any
/// pending preedit first so a half-composed syllable lands before the
/// symbol rather than after it. Scoped to keys whose only modifier is
/// Shift and whose resolved character is ASCII punctuation, so jamo
/// (letters, including Shift+letter double consonants) and Ctrl/Alt
/// bindings are untouched.
fn install_ibus_shifted_symbol_passthrough(container: &gtk::Overlay, term: &vte::Terminal) {
    use gtk::gdk::ModifierType;
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    let term_widget = term.clone();
    key.connect_key_pressed(move |_, keyval, _keycode, state| {
        let relevant = state
            & (ModifierType::CONTROL_MASK | ModifierType::ALT_MASK | ModifierType::SHIFT_MASK);
        if relevant != ModifierType::SHIFT_MASK {
            return glib::Propagation::Proceed;
        }
        let Some(ch) = keyval.to_unicode() else {
            return glib::Propagation::Proceed;
        };
        if !ch.is_ascii_punctuation() {
            return glib::Propagation::Proceed;
        }
        // Commit any in-progress Hangul syllable, then feed the symbol so
        // ordering stays correct (e.g. "안?" not "?안").
        flush_pending_preedit(&term_widget);
        scroll_terminal_to_bottom(&term_widget);
        let mut buf = [0u8; 4];
        term_widget.feed_child(ch.encode_utf8(&mut buf).as_bytes());
        glib::Propagation::Stop
    });
    container.add_controller(key);
}

/// Force VTE's internal IMContext to commit any pending preedit text
/// (e.g. a Hangul syllable still being composed) to the PTY before
/// we inject a bypass byte. Without this, the 22.04 Flatpak IBus
/// path emits the bypass byte ahead of the deferred preedit commit,
/// so "안녕?" arrives at the foreground app as "안?녕".
///
/// VTE does not expose its IMContext, so the only public-API hook
/// that triggers a commit is a focus cycle on the widget — the
/// IMContext flushes preedit on focus-out by GTK convention. The
/// cycle defocuses the parent window then immediately re-grabs
/// focus on the terminal, which is fast enough to leave the cursor
/// blink and selection state visually unchanged but does run the
/// IMContext's commit handler in between.
fn flush_pending_preedit(term: &vte::Terminal) {
    let Some(root) = term.root() else {
        return;
    };
    let Some(window) = root.dynamic_cast_ref::<gtk::Window>() else {
        return;
    };
    gtk::prelude::GtkWindowExt::set_focus(window, gtk::Widget::NONE);
    term.grab_focus();
}

/// True when the active GTK IM module is the ibus immodule — the only
/// configuration with the asynchronous-commit ordering hazard the Enter
/// handler below works around. `main.rs` forces `GTK_IM_MODULE=ibus`
/// when ibus is reachable, so reading the env here reflects that choice.
fn ibus_im_module_active() -> bool {
    std::env::var("GTK_IM_MODULE")
        .map(|m| m.trim() == "ibus")
        .unwrap_or(false)
}

fn insert_newline_needs_preedit_commit_ordering() -> bool {
    insert_newline_needs_preedit_commit_ordering_for(
        ibus_im_module_active(),
        cfg!(target_os = "macos"),
    )
}

fn insert_newline_needs_preedit_commit_ordering_for(ibus_active: bool, macos: bool) -> bool {
    ibus_active || macos
}

/// Submit Enter as a carriage return, but only *after* any IME syllable
/// still in preedit (e.g. a composing Hangul block) has been committed
/// to the PTY. Otherwise the newline overtakes the asynchronous commit
/// and the last syllable lands on the next line: typing "안녕하세요" and
/// pressing Enter while "요" is still composing produces "안녕하세\n요"
/// instead of "안녕하세요\n".
///
/// `IBUS_ENABLE_SYNC_MODE=1` alone does not order this on GTK4 + VTE, and
/// feeding CR straight after `flush_pending_preedit` races the commit on
/// both the native (24.04) and Flatpak (22.04) paths.
///
/// Mechanism: intercept a plain Enter, force the pending syllable to
/// commit (focus-cycle flush), and arm a one-shot. VTE writes committed
/// bytes to the child during the `commit` emission, so queuing CR from
/// that handler — on an idle tick, after the emission returns — orders
/// it behind the syllable no matter how VTE sequences feed-vs-emit. A
/// short timeout fallback submits a bare Enter when nothing is composing
/// (no commit fires) or for an IM that does not commit on focus-out.
///
/// Uses a `ShortcutController` (fires only on the Enter keysyms), not a
/// blanket capture `EventControllerKey`, so every other key — including
/// the jamo keys IBus needs during composition — reaches VTE untouched.
fn install_enter_preedit_commit_ordering(term: &vte::Terminal) {
    let armed = Rc::new(Cell::new(false));

    {
        let armed = armed.clone();
        term.connect_commit(move |t, _text, _size| {
            if armed.replace(false) {
                let t = t.clone();
                glib::idle_add_local_once(move || t.feed_child(b"\r"));
            }
        });
    }

    let controller = gtk::ShortcutController::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.set_scope(gtk::ShortcutScope::Local);

    let term_widget = term.clone();
    let armed_action = armed.clone();
    let action = gtk::CallbackAction::new(move |_, _| {
        scroll_terminal_to_bottom(&term_widget);
        armed_action.set(true);
        flush_pending_preedit(&term_widget);
        let t = term_widget.clone();
        let armed_fb = armed_action.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(20), move || {
            if armed_fb.replace(false) {
                t.feed_child(b"\r");
            }
        });
        glib::Propagation::Stop
    });

    for keyval in [
        gtk::gdk::Key::Return,
        gtk::gdk::Key::ISO_Enter,
        gtk::gdk::Key::KP_Enter,
    ] {
        let trigger = gtk::KeyvalTrigger::new(keyval, gtk::gdk::ModifierType::empty());
        controller.add_shortcut(gtk::Shortcut::new(Some(trigger), Some(action.clone())));
    }

    term.add_controller(controller);
}

fn terminal_capture_key_controllers_enabled(enable_env_enabled: bool) -> bool {
    enable_env_enabled
}

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .as_deref()
        .is_some_and(env_flag_value_enabled)
}

fn env_flag_value_enabled(value: &str) -> bool {
    let value = value.trim();
    value == "1"
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("on")
}

/// Make Shift+Left/Right behave like a plain Left/Right cursor move.
///
/// VTE's default for a shifted cursor key is the modified xterm form
/// `CSI 1 ; 2 C` / `CSI 1 ; 2 D`. Line editors that only parse the bare
/// `CSI C` / `CSI D` (Claude Code's TUI, shell readline) don't recognise
/// the `1;2` parameters and surface the trailing letter as a literal
/// "C"/"D" in the input. We can't offer a copyable keyboard selection on
/// VTE — it exposes no API to set a selection by coordinate — so the
/// least surprising behaviour is to drop the Shift modifier and feed the
/// normal-mode cursor escape. `flowmuxctl pty-tee` rewrites it to the
/// application-cursor form (`SS3 C` / `SS3 D`) while a foreground TUI has
/// DECCKM enabled, so this stays correct in full-screen apps too.
///
/// Scoped to the Shift+Left/Right keysyms via a `ShortcutController`
/// (like the Enter handler), so letter / jamo keys still reach VTE's IM
/// path untouched and inline Hangul preedit is unaffected.
/// Snap the scrollback viewport back to the live cursor row. VTE's
/// `scroll-on-keystroke` only fires for input routed through VTE's own
/// key handler; the capture-phase shortcut paths and clipboard paste
/// reach the PTY through direct `feed_child` / `paste_clipboard` calls
/// that bypass it. Call this first on those paths so a user who scrolled
/// up to read history is snapped back before their input is echoed
/// off-screen. No-op when already pinned to the bottom — mirrors the
/// pure-Rust backend's `write_child` snap (commit 9e12edb).
fn scroll_terminal_to_bottom(term: &vte::Terminal) {
    if let Some(adj) = term.vadjustment() {
        let bottom = adj.upper() - adj.page_size();
        if adj.value() < bottom {
            adj.set_value(bottom);
        }
    }
}

fn adjustment_has_scrollable_range(lower: f64, upper: f64, page_size: f64) -> bool {
    upper > lower + page_size.max(1.0)
}

fn terminal_adjustment_has_scrollback(adj: &gtk::Adjustment) -> bool {
    adjustment_has_scrollable_range(adj.lower(), adj.upper(), adj.page_size())
}

fn sync_terminal_scrollbar_visibility(scrollbar: &gtk::Scrollbar) {
    let adj = scrollbar.adjustment();
    scrollbar.set_visible(terminal_adjustment_has_scrollback(&adj));
}

fn sync_terminal_scrollbar_adjustment(
    term: &vte::Terminal,
    scrollbar: &gtk::Scrollbar,
    watched_adjustment: &Rc<RefCell<Option<gtk::Adjustment>>>,
) {
    if let Some(adj) = term.vadjustment() {
        if scrollbar.adjustment().as_ptr() != adj.as_ptr() {
            scrollbar.set_adjustment(Some(&adj));
        }

        let already_watching = {
            let watched = watched_adjustment.borrow();
            watched
                .as_ref()
                .is_some_and(|watched| watched.as_ptr() == adj.as_ptr())
        };
        if !already_watching {
            let scrollbar_for_changed = scrollbar.clone();
            adj.connect_changed(move |_| {
                sync_terminal_scrollbar_visibility(&scrollbar_for_changed);
            });
            *watched_adjustment.borrow_mut() = Some(adj);
        }
    }

    sync_terminal_scrollbar_visibility(scrollbar);
}

fn install_terminal_scrollbar_adjustment_sync(term: &vte::Terminal, scrollbar: &gtk::Scrollbar) {
    let watched_adjustment = Rc::new(RefCell::new(None::<gtk::Adjustment>));
    sync_terminal_scrollbar_adjustment(term, scrollbar, &watched_adjustment);

    let scrollbar_for_notify = scrollbar.clone();
    let watched_for_notify = watched_adjustment.clone();
    term.connect_vadjustment_notify(move |term| {
        sync_terminal_scrollbar_adjustment(term, &scrollbar_for_notify, &watched_for_notify);
    });

    let scrollbar_for_realize = scrollbar.clone();
    let watched_for_realize = watched_adjustment.clone();
    term.connect_realize(move |term| {
        sync_terminal_scrollbar_adjustment(term, &scrollbar_for_realize, &watched_for_realize);
    });

    let term_for_idle = term.clone();
    let scrollbar_for_idle = scrollbar.clone();
    glib::idle_add_local_once(move || {
        sync_terminal_scrollbar_adjustment(
            &term_for_idle,
            &scrollbar_for_idle,
            &watched_adjustment,
        );
    });
}

fn install_shift_arrow_cursor_move(term: &vte::Terminal) {
    let controller = gtk::ShortcutController::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.set_scope(gtk::ShortcutScope::Local);

    let bindings: &[(gtk::gdk::Key, &'static [u8])] = &[
        (gtk::gdk::Key::Left, b"\x1b[D"),
        (gtk::gdk::Key::KP_Left, b"\x1b[D"),
        (gtk::gdk::Key::Right, b"\x1b[C"),
        (gtk::gdk::Key::KP_Right, b"\x1b[C"),
    ];

    for (keyval, bytes) in bindings {
        let term_widget = term.clone();
        let bytes: &'static [u8] = bytes;
        let action = gtk::CallbackAction::new(move |_, _| {
            scroll_terminal_to_bottom(&term_widget);
            term_widget.feed_child(bytes);
            glib::Propagation::Stop
        });
        let trigger = gtk::KeyvalTrigger::new(*keyval, gtk::gdk::ModifierType::SHIFT_MASK);
        controller.add_shortcut(gtk::Shortcut::new(Some(trigger), Some(action)));
    }

    term.add_controller(controller);
}

/// Legacy smart paging for PgUp / PgDn / Shift+PgUp / Shift+PgDn.
///
/// VTE's default binding for plain PgUp/Dn is to send the cursor-key
/// escape `\x1b[5~` / `\x1b[6~` to the PTY. In a regular shell prompt
/// bash's readline binds those to `history-search-backward` /
/// `history-search-forward` — visually the same as Up/Down arrow,
/// which is not what users expect from PgUp/Dn in a multiplexed
/// terminal. Apps that opt into mouse / alt-screen modes (tig, vim,
/// less, htop, opencode) do want the raw escape though.
///
/// Heuristic: if the VTE has scrollable history (vadjustment range
/// extends past the visible page), scroll the viewport by one page;
/// otherwise forward the same escape VTE would have sent so the
/// foreground app receives the key. Shift+PgUp/Dn always scrolls —
/// the user has signaled "scroll" by holding Shift.
///
/// Disabled by default because on the Ubuntu 22.04 / IBus / GTK4 path,
/// any capture-phase key controller attached directly to VTE can prevent
/// inline Hangul preedit from being drawn. Set
/// `FLOWMUX_ENABLE_VTE_CAPTURE_KEYS=1` to restore the legacy paging hook.
fn install_smart_page_keys(term: &vte::Terminal) {
    let controller = gtk::ShortcutController::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.set_scope(gtk::ShortcutScope::Local);

    const EMPTY: gtk::gdk::ModifierType = gtk::gdk::ModifierType::empty();
    const SHIFT: gtk::gdk::ModifierType = gtk::gdk::ModifierType::SHIFT_MASK;
    let bindings: &[(gtk::gdk::Key, gtk::gdk::ModifierType, i32, bool)] = &[
        (gtk::gdk::Key::Page_Up, EMPTY, -1, false),
        (gtk::gdk::Key::Page_Down, EMPTY, 1, false),
        (gtk::gdk::Key::KP_Page_Up, EMPTY, -1, false),
        (gtk::gdk::Key::KP_Page_Down, EMPTY, 1, false),
        (gtk::gdk::Key::Page_Up, SHIFT, -1, true),
        (gtk::gdk::Key::Page_Down, SHIFT, 1, true),
        (gtk::gdk::Key::KP_Page_Up, SHIFT, -1, true),
        (gtk::gdk::Key::KP_Page_Down, SHIFT, 1, true),
    ];

    for (key, mods, direction, always_scroll) in bindings {
        let term_widget = term.clone();
        let direction = *direction;
        let always_scroll = *always_scroll;
        let action = gtk::CallbackAction::new(move |_, _| {
            let Some(adj) = term_widget.vadjustment() else {
                let bytes: &[u8] = if direction < 0 {
                    b"\x1b[5~"
                } else {
                    b"\x1b[6~"
                };
                term_widget.feed_child(bytes);
                return glib::Propagation::Stop;
            };
            let upper = adj.upper();
            let page = adj.page_size().max(1.0);
            let has_scrollback =
                adjustment_has_scrollable_range(adj.lower(), upper, adj.page_size());
            if !always_scroll && !has_scrollback {
                // Alt-screen / empty scrollback: forward the legacy
                // PgUp/Dn escape so foreground apps (tig, vim, less,
                // htop) can page their own buffer. This action already
                // owns the capture-phase decision, so forwarding the
                // exact VTE byte sequence preserves the native outcome.
                let bytes: &[u8] = if direction < 0 {
                    b"\x1b[5~"
                } else {
                    b"\x1b[6~"
                };
                term_widget.feed_child(bytes);
                return glib::Propagation::Stop;
            }
            let mut target = adj.value() + (direction as f64) * page;
            if target < adj.lower() {
                target = adj.lower();
            }
            let max = (upper - page).max(adj.lower());
            if target > max {
                target = max;
            }
            adj.set_value(target);
            glib::Propagation::Stop
        });
        let trigger = gtk::KeyvalTrigger::new(*key, *mods);
        controller.add_shortcut(gtk::Shortcut::new(Some(trigger), Some(action)));
    }

    term.add_controller(controller);
}

/// Work around IBus daemon paths that silently drop plain navigation /
/// editing keys during Hangul preedit. This was first needed for the
/// Ubuntu 22.04 host + GNOME 48 Flatpak runtime path, and WSLg's IBus
/// portal path shows the same symptom for BackSpace/Delete in Korean
/// mode. The deciding reproducer was that the same keys with `Ctrl`
/// held down worked fine on the same setup — Ctrl takes the event past
/// GTK's IM filter without involving IBus at all.
///
/// Approach: install a capture-phase `ShortcutController` on the VTE
/// widget for the affected plain keys. When one matches we feed a
/// normal-mode terminal byte sequence straight to the PTY and consume
/// the event so VTE's own IM-aware handler never sees it. `flowmuxctl
/// pty-tee` observes smkx/rmkx on the terminal output side and
/// rewrites cursor keys to application mode when foreground programs
/// such as tig request it.
///
/// The bypass only consumes plain non-character keys IBus would forward
/// (and drop) anyway, so it does not starve composition: letters,
/// digits, punctuation and Space still reach IBus and compose normally,
/// and the inline preedit stays visible (drawn by
/// `install_preedit_redraw_on_keystroke`). An earlier attempt to gate
/// this whole bypass behind `FLOWMUX_ENABLE_IBUS_NAV_WORKAROUND=1` and
/// rely on `IBUS_ENABLE_SYNC_MODE=1` instead regressed every edit/nav
/// key on the 22.04 host, because sync mode never fixed the drop
/// (confirmed standalone) — so the bypass is on by default in Flatpak.
///
/// What is intentionally **not** intercepted:
///   * Space. Its natural role inside IBus is "commit the current
///     preedit + insert space", and bypassing it would feed bare
///     0x20 to the PTY without committing the Korean syllable,
///     dropping the user's text on the floor.
///   * Letter / number / punctuation keys. IBus needs to see them
///     for preedit; bypassing them made Claude show only committed
///     syllables instead of the in-progress Hangul composition.
///
/// BackSpace / Delete / KP_Delete ARE intercepted — otherwise the 22.04
/// daemon-path drops them in Hangul mode and deletion stalls. See the
/// inline note at their `bind_key` calls; they rely on
/// `flush_pending_preedit` to commit any composing syllable first. Enter
/// is handled separately by `install_enter_preedit_commit_ordering`.
///
/// Active when, and `FLOWMUX_NO_IBUS_NAV_WORKAROUND=1` overrides:
///   * running inside a Flatpak sandbox (`/.flatpak-info` exists), or
///   * running under WSL/WSLg, or
///   * `FLOWMUX_ENABLE_IBUS_NAV_WORKAROUND=1` is set (force-on elsewhere).
fn flatpak_ibus_bypass_bytes(keyval: gtk::gdk::Key) -> Option<&'static [u8]> {
    use gtk::gdk::Key;

    if keyval == Key::Tab {
        Some(b"\t")
    } else if keyval == Key::Escape {
        Some(b"\x1b")
    } else if keyval == Key::BackSpace {
        Some(b"\x7f")
    } else if keyval == Key::Delete || keyval == Key::KP_Delete {
        Some(b"\x1b[3~")
    } else if keyval == Key::Left || keyval == Key::KP_Left {
        Some(b"\x1b[D")
    } else if keyval == Key::Right || keyval == Key::KP_Right {
        Some(b"\x1b[C")
    } else if keyval == Key::Up || keyval == Key::KP_Up {
        Some(b"\x1b[A")
    } else if keyval == Key::Down || keyval == Key::KP_Down {
        Some(b"\x1b[B")
    } else if keyval == Key::Home || keyval == Key::KP_Home {
        Some(b"\x1b[H")
    } else if keyval == Key::End || keyval == Key::KP_End {
        Some(b"\x1b[F")
    } else if keyval == Key::Page_Up || keyval == Key::KP_Page_Up {
        Some(b"\x1b[5~")
    } else if keyval == Key::Page_Down || keyval == Key::KP_Page_Down {
        Some(b"\x1b[6~")
    } else if keyval == Key::Insert {
        Some(b"\x1b[2~")
    } else if keyval == Key::F1 {
        Some(b"\x1bOP")
    } else if keyval == Key::F2 {
        Some(b"\x1bOQ")
    } else if keyval == Key::F3 {
        Some(b"\x1bOR")
    } else if keyval == Key::F4 {
        Some(b"\x1bOS")
    } else if keyval == Key::F5 {
        Some(b"\x1b[15~")
    } else if keyval == Key::F6 {
        Some(b"\x1b[17~")
    } else if keyval == Key::F7 {
        Some(b"\x1b[18~")
    } else if keyval == Key::F8 {
        Some(b"\x1b[19~")
    } else if keyval == Key::F9 {
        Some(b"\x1b[20~")
    } else if keyval == Key::F10 {
        Some(b"\x1b[21~")
    } else if keyval == Key::F11 {
        Some(b"\x1b[23~")
    } else if keyval == Key::F12 {
        Some(b"\x1b[24~")
    } else {
        None
    }
}

fn should_install_ibus_nav_workaround(
    disable_env_present: bool,
    enable_env_present: bool,
    flatpak_info_exists: bool,
    running_under_wsl: bool,
) -> bool {
    !disable_env_present && (flatpak_info_exists || running_under_wsl || enable_env_present)
}

fn install_ibus_nav_workaround(term: &vte::Terminal, smart_page_enabled: bool) {
    if !should_install_ibus_nav_workaround(
        std::env::var_os("FLOWMUX_NO_IBUS_NAV_WORKAROUND").is_some(),
        env_flag_enabled("FLOWMUX_ENABLE_IBUS_NAV_WORKAROUND"),
        std::path::Path::new("/.flatpak-info").exists(),
        crate::platform::running_under_wsl(),
    ) {
        return;
    }

    let controller = gtk::ShortcutController::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.set_scope(gtk::ShortcutScope::Local);

    let bind = |keyval: gtk::gdk::Key, bytes: &'static [u8]| {
        let term_widget = term.clone();
        let action = gtk::CallbackAction::new(move |_, _| {
            scroll_terminal_to_bottom(&term_widget);
            flush_pending_preedit(&term_widget);
            term_widget.feed_child(bytes);
            glib::Propagation::Stop
        });
        let trigger = gtk::KeyvalTrigger::new(keyval, gtk::gdk::ModifierType::empty());
        controller.add_shortcut(gtk::Shortcut::new(Some(trigger), Some(action)));
    };
    let bind_key = |keyval: gtk::gdk::Key| {
        if let Some(bytes) = flatpak_ibus_bypass_bytes(keyval) {
            bind(keyval, bytes);
        }
    };

    // Normal-mode xterm encodings. `flowmuxctl pty-tee` adjusts the
    // cursor-key subset to application mode while DECCKM is active.
    //
    // BackSpace / Delete (and KP_Delete) ARE bypassed. The 22.04 IBus
    // daemon-path drops every plain edit key it does not itself consume,
    // so once the Hangul preedit is empty (e.g. after typing 안녕하세요 the
    // last syllable is committed) BackSpace is forwarded by IBus and then
    // silently dropped — deletion of already-committed text stalls. The
    // briefly-tried alternative of letting these reach IBus under
    // `IBUS_ENABLE_SYNC_MODE=1` did not help: sync mode never fixed the
    // drop (confirmed standalone), it only enabled jamo-level decompose
    // while a syllable is still composing. Restoring the bypass trades
    // that in-preedit jamo decompose for reliable deletion: bypassing
    // BackSpace runs `flush_pending_preedit` (commits the pending
    // syllable) then sends DEL, so a composing syllable is committed and
    // removed whole, and committed text deletes one cell at a time.
    bind_key(gtk::gdk::Key::BackSpace);
    bind_key(gtk::gdk::Key::Delete);
    bind_key(gtk::gdk::Key::Tab);
    bind_key(gtk::gdk::Key::Escape);
    // Enter (Return / ISO_Enter / KP_Enter) is handled by
    // `install_enter_preedit_commit_ordering`, which also fixes its
    // commit-vs-newline ordering, so it is intentionally not bound here.
    bind_key(gtk::gdk::Key::Left);
    bind_key(gtk::gdk::Key::Right);
    bind_key(gtk::gdk::Key::Up);
    bind_key(gtk::gdk::Key::Down);
    bind_key(gtk::gdk::Key::Home);
    bind_key(gtk::gdk::Key::End);
    // Page_Up / Page_Down: when the opt-in smart-page hook owns them
    // (`FLOWMUX_ENABLE_VTE_CAPTURE_KEYS=1`) leave them to it — binding
    // them here too would double-handle the key and defeat its scroll
    // path. Otherwise bypass them so they are not dropped in Hangul mode;
    // the bytes match VTE's native escape (`\x1b[5~` / `\x1b[6~`).
    if !smart_page_enabled {
        bind_key(gtk::gdk::Key::Page_Up);
        bind_key(gtk::gdk::Key::Page_Down);
        bind_key(gtk::gdk::Key::KP_Page_Up);
        bind_key(gtk::gdk::Key::KP_Page_Down);
    }
    // Keypad variants of the same keys — some layouts (notebook + Fn,
    // X11 with NumLock off, …) report them as the KP_* keysyms even
    // when the user hits the equivalent unshifted key.
    // KP_Delete bypassed for the same reason as Delete above.
    bind_key(gtk::gdk::Key::KP_Delete);
    bind_key(gtk::gdk::Key::KP_Left);
    bind_key(gtk::gdk::Key::KP_Right);
    bind_key(gtk::gdk::Key::KP_Up);
    bind_key(gtk::gdk::Key::KP_Down);
    bind_key(gtk::gdk::Key::KP_Home);
    bind_key(gtk::gdk::Key::KP_End);
    bind_key(gtk::gdk::Key::Insert);

    // Function keys F1 .. F12 using the xterm normal-mode encoding
    // that bash / vim / less / tig / opencode all expect. F1-F4 are
    // the SS3 family (`ESC O <letter>`); F5+ are the CSI tilde
    // family (`ESC [ <digits> ~`). flowmuxctl pty-tee does not
    // rewrite these, so forwarding the literal bytes here is
    // sufficient.
    bind_key(gtk::gdk::Key::F1);
    bind_key(gtk::gdk::Key::F2);
    bind_key(gtk::gdk::Key::F3);
    bind_key(gtk::gdk::Key::F4);
    bind_key(gtk::gdk::Key::F5);
    bind_key(gtk::gdk::Key::F6);
    bind_key(gtk::gdk::Key::F7);
    bind_key(gtk::gdk::Key::F8);
    bind_key(gtk::gdk::Key::F9);
    bind_key(gtk::gdk::Key::F10);
    bind_key(gtk::gdk::Key::F11);
    bind_key(gtk::gdk::Key::F12);

    term.add_controller(controller);
}

/// argv used when the caller asks for the default shell (no explicit
/// command).
///
/// **Outside a sandbox on Linux** — run `$SHELL -l`. The `-l` flag makes
/// any POSIX-ish shell source the per-shell profile (.bash_profile /
/// .profile / .zprofile / fish login conf), which in turn pulls .bashrc /
/// .zshrc so the user's PS1 + helpers are defined before the first prompt.
/// Same convention xterm / alacritty / kitty use.
///
/// **On macOS** — run the user's shell as a login shell too. Finder-launched
/// apps do not inherit the user's terminal PATH or UTF-8 locale, so skipping
/// shell startup files makes the first prompt plain and can leave Korean input
/// decoded under a `C` locale. The PTY env still carries flowmux's pane vars and
/// a small PATH/locale fallback for GUI launches.
///
/// **Inside Flatpak** — wrap the host shell in an inline Python
/// bridge. `flatpak-spawn --host` forwards stdin/stdout FDs to a host
/// process but cannot grant `TIOCSCTTY` on the sandbox PTY (kernel
/// keeps the in-sandbox `flatpak-spawn` as the session leader), so a
/// bare host shell starts with no controlling terminal: `tig` /
/// `vim` / `less` / `htop` and similar tools that open `/dev/tty`
/// outright fail. We worked around this several ways before, all of
/// which produced worse symptoms: `script -c …` caused a runaway
/// prompt-redraw loop through `flatpak-spawn`'s FD-forwarding pipe;
/// `setsid --ctty` got `EPERM` because the original PTY belongs to
/// another session.
///
/// The Python bridge does the job by hand and cleanly:
///
///   1. `pty.fork()` (= libc `forkpty(3)`) allocates a *fresh* PTY
///      pair on the host. `TIOCSCTTY` on a newly-created PTY always
///      succeeds, so the child shell gets full ctty + job control +
///      `/dev/tty`.
///   2. The user's actual host shell is resolved via
///      `pwd.getpwuid(os.getuid()).pw_shell`, i.e. from the host's
///      `/etc/passwd` rather than the sandbox's potentially mangled
///      `$SHELL` (which on the GNOME Platform runtime tends to
///      arrive as `/bin/sh`, putting bash into POSIX mode and
///      breaking `.bashrc`'s `export var-with-dash=…` lines).
///   3. The parent process is a tiny select loop that pumps bytes
///      between the forwarded sandbox PTY (FD 0 / FD 1) and the new
///      host PTY's master FD, plus polls `TIOCGWINSZ` to forward
///      window-resize updates. Hand-written so we don't trip the
///      same FD-forwarding edge case that broke `script(1)`.
///   4. On exit it `SIGHUP`s the shell's process group so the host
///      side is reaped when flowmux's pane goes away.
///
/// Python 3 ships with every mainstream Linux desktop and `pty` is
/// in the stdlib, so this needs no additional packages on the host.
/// Requires `--talk-name=org.freedesktop.Flatpak` in the Flatpak
/// manifest's finish-args so `flatpak-spawn` can reach the session
/// helper.
fn default_shell_argv() -> Vec<String> {
    if is_flatpak_sandbox() {
        vec![
            "flatpak-spawn".into(),
            "--host".into(),
            "--watch-bus".into(),
            "--".into(),
            "python3".into(),
            "-u".into(),
            "-c".into(),
            FLATPAK_HOST_SHELL_BRIDGE.into(),
        ]
    } else {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        vec![shell, "-l".into()]
    }
}

fn prepare_terminal_child_env(extra_env: &mut Vec<(String, String)>) {
    #[cfg(target_os = "macos")]
    add_macos_gui_env_fallbacks(extra_env);
    prepend_agent_shim_dir(extra_env);
    #[cfg(target_os = "macos")]
    add_macos_zsh_agent_path_guard(extra_env);
}

#[cfg(target_os = "macos")]
fn add_macos_gui_env_fallbacks(extra_env: &mut Vec<(String, String)>) {
    if !inherited_locale_is_utf8() {
        extra_env.push(("LANG".to_string(), "en_US.UTF-8".to_string()));
        extra_env.push(("LC_CTYPE".to_string(), "en_US.UTF-8".to_string()));
    }
    if let Some(path) = macos_terminal_path_from(
        std::env::var_os("HOME").map(PathBuf::from),
        std::env::var("PATH").ok().as_deref(),
    ) {
        extra_env.push(("PATH".to_string(), path));
    }
}

fn prepend_agent_shim_dir(extra_env: &mut Vec<(String, String)>) {
    // Prepend the agent shim dir to PATH so `claude` / `codex`
    // resolve to the PID-capturing wrappers `flowmux fix` installs.
    // VTE merges these entries over the inherited environment, so a
    // PATH entry overrides the inherited one; rebuild it as shim-dir-first
    // using any PATH override already added above.
    if let Some(shim) = flowmux_config::paths::agent_shim_dir() {
        if shim.is_dir() {
            let base = last_env_value(extra_env, "PATH")
                .map(str::to_string)
                .or_else(|| std::env::var("PATH").ok())
                .unwrap_or_default();
            extra_env.push(("PATH".to_string(), prepend_path_entry(&shim, &base)));
        }
    }
}

fn last_env_value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn prepend_path_entry(dir: &std::path::Path, base: &str) -> String {
    let dir = dir.to_string_lossy();
    if base.is_empty() {
        dir.into_owned()
    } else {
        format!("{dir}:{base}")
    }
}

#[cfg(target_os = "macos")]
fn add_macos_zsh_agent_path_guard(extra_env: &mut Vec<(String, String)>) {
    let shell = std::env::var_os("SHELL").map(PathBuf::from);
    if shell
        .as_ref()
        .and_then(|path| path.file_name())
        .is_none_or(|name| name != "zsh")
    {
        return;
    }
    let Some(shim_dir) = flowmux_config::paths::agent_shim_dir().filter(|path| path.is_dir())
    else {
        return;
    };
    let Some(wrapper_dir) = flowmux_config::paths::state_dir().map(|dir| dir.join("zsh-flowmux"))
    else {
        return;
    };
    let real_zdotdir = std::env::var_os("ZDOTDIR")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    if write_zsh_agent_path_guard_files(&wrapper_dir).is_err() {
        return;
    }
    extra_env.push(("ZDOTDIR".to_string(), wrapper_dir.to_string_lossy().into()));
    extra_env.push((
        "FLOWMUX_REAL_ZDOTDIR".to_string(),
        real_zdotdir.to_string_lossy().into(),
    ));
    extra_env.push((
        "FLOWMUX_AGENT_SHIM_DIR".to_string(),
        shim_dir.to_string_lossy().into(),
    ));
}

#[cfg(target_os = "macos")]
fn write_zsh_agent_path_guard_files(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, should_prepend) in [
        (".zshenv", false),
        (".zprofile", true),
        (".zshrc", true),
        (".zlogin", true),
        (".zlogout", false),
    ] {
        std::fs::write(
            dir.join(name),
            zsh_agent_path_guard_file(name, should_prepend),
        )?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn zsh_agent_path_guard_file(name: &str, should_prepend: bool) -> String {
    let prepend = if should_prepend {
        r#"
if [ -n "${FLOWMUX_AGENT_SHIM_DIR:-}" ]; then
  export PATH="$FLOWMUX_AGENT_SHIM_DIR:$PATH"
fi
"#
    } else {
        ""
    };
    format!(
        r#"# flowmux generated zsh startup wrapper; do not edit.
_flowmux_real_zdotdir="${{FLOWMUX_REAL_ZDOTDIR:-$HOME}}"
_flowmux_real_file="$_flowmux_real_zdotdir/{name}"
if [ -r "$_flowmux_real_file" ]; then
  source "$_flowmux_real_file"
fi
{prepend}"#
    )
}

#[cfg(target_os = "macos")]
fn inherited_locale_is_utf8() -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"].iter().any(|key| {
        std::env::var(key)
            .ok()
            .is_some_and(|value| locale_value_is_utf8(&value))
    })
}

#[cfg(target_os = "macos")]
fn locale_value_is_utf8(value: &str) -> bool {
    let value = value.trim();
    value.eq_ignore_ascii_case("UTF-8")
        || value.eq_ignore_ascii_case("UTF8")
        || value.to_ascii_uppercase().ends_with(".UTF-8")
        || value.to_ascii_uppercase().ends_with(".UTF8")
}

#[cfg(target_os = "macos")]
fn macos_terminal_path_from(home: Option<PathBuf>, inherited: Option<&str>) -> Option<String> {
    let mut entries: Vec<PathBuf> = Vec::new();
    if let Some(home) = home {
        entries.push(home.join(".local").join("bin"));
        entries.push(home.join(".cargo").join("bin"));
    }
    for path in [
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
        "/usr/local/sbin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ] {
        entries.push(PathBuf::from(path));
    }
    if let Some(inherited) = inherited {
        entries.extend(std::env::split_paths(inherited));
    }
    dedup_path_entries(&mut entries);
    std::env::join_paths(entries)
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

#[cfg(target_os = "macos")]
fn dedup_path_entries(entries: &mut Vec<PathBuf>) {
    let mut seen = std::collections::HashSet::new();
    entries.retain(|path| seen.insert(path.clone()));
}

/// Detect whether the current process is running inside a Flatpak
/// sandbox. Flatpak sets `FLATPAK_ID` for sandboxed apps and writes a
/// `/.flatpak-info` file at the sandbox root; either is sufficient
/// proof.
pub(crate) fn is_flatpak_sandbox() -> bool {
    std::env::var_os("FLATPAK_ID").is_some() || std::path::Path::new("/.flatpak-info").exists()
}

/// Inline Python program that runs on the *host* via `flatpak-spawn
/// --host`. See `default_shell_argv` for the why; the script itself
/// is intentionally small and self-contained because it's passed as
/// `python3 -c <source>` argv. Edits here change the behavior of the
/// shell that flowmux's terminal pane exposes when running inside a
/// Flatpak sandbox.
const FLATPAK_HOST_SHELL_BRIDGE: &str = r#"
import pty, os, sys, fcntl, termios, struct, select, signal, pwd, tty
from urllib.parse import quote

shell = pwd.getpwuid(os.getuid()).pw_shell or '/bin/bash'

# Put the inherited sandbox PTY (forwarded into us as fd 0/1 by
# flatpak-spawn) into RAW mode before we start pumping. Without this
# step the chain runs *two* line disciplines back-to-back — the
# sandbox PTY and the inner host PTY — so:
#   * Enter prints a blank line (CR -> NL twice + double echo)
#   * Tab completion doesn't fire until Enter (sandbox PTY ICANON
#     keeps the byte buffered)
#   * Ctrl-C never reaches bash; the sandbox PTY's ISIG fires SIGINT
#     at flatpak-spawn instead and the pane freezes
#   * ncurses apps (tig, vim, htop, less) can't read individual
#     keystrokes — input arrives one cooked line at a time
# Raw mode disables ECHO / ICANON / ISIG / OPOST on the sandbox PTY
# so it becomes a transparent passthrough; only the inner host PTY,
# which the shell already configures correctly, applies line
# discipline. Restore the original attributes on exit.
try:
    saved_attrs = termios.tcgetattr(0)
    tty.setraw(0)
except (termios.error, OSError):
    saved_attrs = None

def restore_pty():
    if saved_attrs is not None:
        try:
            termios.tcsetattr(0, termios.TCSANOW, saved_attrs)
        except (termios.error, OSError):
            pass

def winsize():
    try:
        return struct.unpack('HHHH', fcntl.ioctl(0, termios.TIOCGWINSZ, b'\x00' * 8))
    except OSError:
        return (24, 80, 0, 0)

pid, fd = pty.fork()
if pid == 0:
    # Child: pty.fork() has already made us the session leader of a
    # fresh PTY and wired its slave to stdin/stdout/stderr. exec the
    # user's login shell.
    os.execvp(shell, [shell, '-l'])

last_ws = None
def sync_winsize():
    global last_ws
    cur = winsize()
    if cur != last_ws:
        last_ws = cur
        try:
            fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', *cur))
        except OSError:
            pass

# Track the host shell's cwd and emit OSC 7 to the sandbox PTY whenever
# it changes. flowmux runs inside the sandbox and reads VTE's
# current-directory-uri to keep tab names, workspace labels, the window
# title, and split starting directory in sync with `cd`. The native
# build gets this for free from the shell's own OSC 7 + a
# `/proc/<vte_pid>/cwd` fallback, but in Flatpak the PID VTE sees is
# the sandbox-side `flatpak-spawn` wrapper, not the host shell, so
# /proc fallback resolves to the wrong process and is invisible from
# the sandbox anyway. The bridge runs on the host with /proc access to
# the real shell, so it is the right place to announce cwd.
last_cwd = None
def emit_cwd_if_changed():
    global last_cwd
    try:
        cwd = os.readlink('/proc/%d/cwd' % pid)
    except OSError:
        return
    if cwd == last_cwd:
        return
    last_cwd = cwd
    try:
        seq = b'\x1b]7;file://' + quote(cwd, safe='/').encode('ascii') + b'\x07'
    except (UnicodeError, ValueError):
        return
    try:
        os.write(1, seq)
    except OSError:
        pass

sync_winsize()
emit_cwd_if_changed()

def on_term(*_):
    try:
        os.killpg(os.getpgid(pid), signal.SIGHUP)
    except OSError:
        pass
    restore_pty()
    sys.exit(0)

signal.signal(signal.SIGTERM, on_term)
signal.signal(signal.SIGHUP, on_term)

# Wake the select loop the instant a window resize arrives instead of
# waiting for the next poll tick. flatpak-spawn does not forward
# SIGWINCH across the sandbox boundary reliably, so we cannot count on
# it firing -- but when it does, route it through a self-pipe
# (set_wakeup_fd) so select returns at once and we re-read TIOCGWINSZ.
# The shortened poll timeout below is the always-present fallback for
# the case where the signal never crosses the boundary: it caps the
# host shell's stale-size window at ~0.1s instead of ~0.5s, which is
# what produced visibly mis-wrapped output right after a resize on the
# 22.04 Flatpak path (VTE reflows immediately while the host shell kept
# emitting at the old column count until the next poll).
wake_r, wake_w = os.pipe()
os.set_blocking(wake_r, False)
os.set_blocking(wake_w, False)
signal.set_wakeup_fd(wake_w)
signal.signal(signal.SIGWINCH, lambda *_: None)

try:
    while True:
        try:
            rfds, _, _ = select.select([0, fd, wake_r], [], [], 0.1)
        except InterruptedError:
            rfds = []
        if wake_r in rfds:
            try:
                os.read(wake_r, 4096)
            except OSError:
                pass
        sync_winsize()
        emit_cwd_if_changed()
        if 0 in rfds:
            try:
                data = os.read(0, 4096)
            except OSError:
                data = b''
            if not data:
                break
            try:
                os.write(fd, data)
            except OSError:
                break
        if fd in rfds:
            try:
                data = os.read(fd, 4096)
            except OSError:
                data = b''
            if not data:
                break
            try:
                os.write(1, data)
            except OSError:
                break
except KeyboardInterrupt:
    pass

try:
    os.killpg(os.getpgid(pid), signal.SIGHUP)
except OSError:
    pass
restore_pty()
try:
    _, status = os.waitpid(pid, 0)
    sys.exit(os.waitstatus_to_exitcode(status))
except (ChildProcessError, ValueError):
    sys.exit(0)
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animated_title_storm_only_settles_after_a_quiet_window() {
        let coalesce = TitleCoalesce::default();
        *coalesce.last_sent.borrow_mut() = "frame-0".into();
        coalesce.window_open.set(true);

        for frame in 1..100 {
            *coalesce.pending.borrow_mut() = Some(format!("frame-{frame}"));
            assert_eq!(coalesce.finish_window(), TitleWindowResult::Rearm);
        }

        assert_eq!(
            coalesce.finish_window(),
            TitleWindowResult::Settled(Some("frame-99".into()))
        );
        assert!(!coalesce.window_open.get());
    }

    #[test]
    fn scrollback_replay_uses_terminal_safe_line_endings() {
        assert_eq!(scrollback_replay_bytes("one\ntwo"), b"one\r\ntwo\r\n");
        assert_eq!(scrollback_replay_bytes("one\r\ntwo\r\n"), b"one\r\ntwo\r\n");
    }

    #[test]
    fn image_path_regex_compiles() {
        vte::Regex::for_match(IMAGE_PATH_REGEX_PATTERN, URL_REGEX_COMPILE_FLAGS)
            .expect("image path regex compiles");
    }

    #[test]
    fn terminal_click_activation_waits_for_release_and_runs_once() {
        let mut activation = TerminalClickActivation::default();
        activation.press(
            TerminalClickTarget::Image(PathBuf::from("/tmp/render.png")),
            false,
        );

        assert_eq!(
            activation.release(),
            Some(PendingTerminalClick {
                target: TerminalClickTarget::Image(PathBuf::from("/tmp/render.png")),
                open_in_system_browser: false,
            })
        );
        assert_eq!(activation.release(), None);
    }

    #[test]
    fn terminal_link_dispatch_accepts_only_primary_button_release() {
        assert!(!is_primary_button_release(
            gtk::gdk::EventType::ButtonPress,
            Some(gtk::gdk::BUTTON_PRIMARY)
        ));
        assert!(is_primary_button_release(
            gtk::gdk::EventType::ButtonRelease,
            Some(gtk::gdk::BUTTON_PRIMARY)
        ));
        assert!(!is_primary_button_release(
            gtk::gdk::EventType::ButtonRelease,
            Some(gtk::gdk::BUTTON_SECONDARY)
        ));
    }

    #[test]
    fn trim_url_trailing_strips_common_sentence_punctuation() {
        assert_eq!(
            trim_url_trailing("https://example.com/page."),
            "https://example.com/page"
        );
        assert_eq!(
            trim_url_trailing("https://example.com/page),"),
            "https://example.com/page"
        );
        assert_eq!(
            trim_url_trailing("https://example.com/path?q=1!"),
            "https://example.com/path?q=1"
        );
        assert_eq!(
            trim_url_trailing("https://example.com/'\"`"),
            "https://example.com/"
        );
    }

    #[test]
    fn trim_url_trailing_preserves_internal_punctuation() {
        // Preserve `.` or `,` inside the path; trim only from the end.
        assert_eq!(
            trim_url_trailing("https://example.com/a.b/c"),
            "https://example.com/a.b/c"
        );
        assert_eq!(
            trim_url_trailing("https://example.com/path?a=1,2,3"),
            "https://example.com/path?a=1,2,3"
        );
    }

    #[test]
    fn trim_url_trailing_handles_clean_url() {
        assert_eq!(
            trim_url_trailing("https://example.com/"),
            "https://example.com/"
        );
    }

    #[test]
    fn trim_url_trailing_handles_empty() {
        assert_eq!(trim_url_trailing(""), "");
        assert_eq!(trim_url_trailing("...,,;"), "");
    }

    #[test]
    fn terminal_image_path_accepts_absolute_supported_paths() {
        assert_eq!(
            terminal_image_path("/tmp/render.PNG,", None),
            Some(PathBuf::from("/tmp/render.PNG"))
        );
        assert_eq!(
            terminal_image_path("/tmp/render.web", None),
            Some(PathBuf::from("/tmp/render.web"))
        );
        assert_eq!(
            terminal_image_path("/tmp/vector.svg", None),
            Some(PathBuf::from("/tmp/vector.svg"))
        );
        assert_eq!(
            terminal_image_path("/home/user/anim.lottie)", None),
            Some(PathBuf::from("/home/user/anim.lottie"))
        );
        assert_eq!(
            terminal_image_path("/home/user/bodymovin.json", None),
            Some(PathBuf::from("/home/user/bodymovin.json"))
        );
    }

    #[test]
    fn terminal_image_path_rejects_relative_or_unsupported_paths() {
        assert_eq!(terminal_image_path("render.png", None), None);
        assert_eq!(terminal_image_path("/tmp/readme.txt", None), None);
    }

    #[test]
    fn terminal_image_path_resolves_ls_relative_paths_against_cwd() {
        let cwd = PathBuf::from("/home/user/images");

        assert_eq!(
            terminal_image_path("render.png", Some(&cwd)),
            Some(PathBuf::from("/home/user/images/render.png"))
        );
        assert_eq!(
            terminal_image_path("icons/vector.svg", Some(&cwd)),
            Some(PathBuf::from("/home/user/images/icons/vector.svg"))
        );
    }

    #[test]
    fn markdown_path_regex_compiles() {
        vte::Regex::for_match(MARKDOWN_PATH_REGEX_PATTERN, URL_REGEX_COMPILE_FLAGS)
            .expect("markdown path regex compiles");
    }

    #[test]
    fn terminal_markdown_path_resolves_ls_relative_paths_against_cwd() {
        let cwd = PathBuf::from("/home/user/docs");

        assert_eq!(
            terminal_markdown_path("README.md", Some(&cwd)),
            Some(PathBuf::from("/home/user/docs/README.md"))
        );
        assert_eq!(
            terminal_markdown_path("guides/install.markdown,", Some(&cwd)),
            Some(PathBuf::from("/home/user/docs/guides/install.markdown"))
        );
        assert_eq!(
            terminal_markdown_path("./notes.mkd", Some(&cwd)),
            Some(PathBuf::from("/home/user/docs/./notes.mkd"))
        );
    }

    #[test]
    fn terminal_markdown_path_rejects_non_markdown_paths() {
        let cwd = PathBuf::from("/home/user/docs");

        assert_eq!(terminal_markdown_path("README.txt", Some(&cwd)), None);
        assert_eq!(terminal_markdown_path("image.png", Some(&cwd)), None);
        assert_eq!(terminal_markdown_path("README.md", None), None);
    }

    #[test]
    fn shift_enter_byte_sequence_is_esc_cr() {
        // Agent TUIs (Claude, Codex, OpenCode) all treat ESC+CR as "insert
        // newline". Lock the wire format so a future refactor does not turn
        // Shift+Enter into a literal newline submission again.
        assert_eq!(INSERT_NEWLINE_BYTES, b"\x1b\r");
    }

    #[test]
    fn insert_newline_preedit_ordering_covers_ibus_and_macos() {
        assert!(insert_newline_needs_preedit_commit_ordering_for(
            true, false
        ));
        assert!(insert_newline_needs_preedit_commit_ordering_for(
            false, true
        ));
        assert!(!insert_newline_needs_preedit_commit_ordering_for(
            false, false
        ));
    }

    #[test]
    fn path_prepend_keeps_agent_shim_first() {
        assert_eq!(
            prepend_path_entry(std::path::Path::new("/tmp/flowmux-shims"), "/usr/bin:/bin"),
            "/tmp/flowmux-shims:/usr/bin:/bin"
        );
        assert_eq!(
            prepend_path_entry(std::path::Path::new("/tmp/flowmux-shims"), ""),
            "/tmp/flowmux-shims"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn zsh_agent_path_guard_sources_real_file_then_reprepends_shim() {
        let zshrc = zsh_agent_path_guard_file(".zshrc", true);
        assert!(zshrc.contains("FLOWMUX_REAL_ZDOTDIR"));
        assert!(zshrc.contains("source \"$_flowmux_real_file\""));
        assert!(zshrc.contains("FLOWMUX_AGENT_SHIM_DIR:$PATH"));

        let zshenv = zsh_agent_path_guard_file(".zshenv", false);
        assert!(zshenv.contains(".zshenv"));
        assert!(!zshenv.contains("FLOWMUX_AGENT_SHIM_DIR:$PATH"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_terminal_path_adds_gui_fallbacks_before_inherited_path() {
        let path = macos_terminal_path_from(
            Some(PathBuf::from("/Users/example")),
            Some("/custom/bin:/usr/bin"),
        )
        .expect("path");
        let entries: Vec<_> = std::env::split_paths(&path).collect();

        assert_eq!(entries[0], PathBuf::from("/Users/example/.local/bin"));
        assert_eq!(entries[1], PathBuf::from("/Users/example/.cargo/bin"));
        assert!(entries
            .iter()
            .any(|p| p == std::path::Path::new("/opt/homebrew/bin")));
        assert!(entries
            .iter()
            .any(|p| p == std::path::Path::new("/custom/bin")));
        assert_eq!(
            entries
                .iter()
                .filter(|p| *p == std::path::Path::new("/usr/bin"))
                .count(),
            1
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_locale_detection_accepts_common_utf8_spellings() {
        assert!(locale_value_is_utf8("UTF-8"));
        assert!(locale_value_is_utf8("en_US.UTF-8"));
        assert!(locale_value_is_utf8("ko_KR.UTF8"));
        assert!(!locale_value_is_utf8("C"));
    }

    #[test]
    fn vte_capture_key_controllers_are_legacy_opt_in() {
        assert!(!terminal_capture_key_controllers_enabled(false));
        assert!(terminal_capture_key_controllers_enabled(true));
    }

    #[test]
    fn scrollbar_range_is_hidden_when_adjustment_is_one_page() {
        assert!(!adjustment_has_scrollable_range(0.0, 1.0, 1.0));
        assert!(!adjustment_has_scrollable_range(0.0, 24.0, 24.0));
        assert!(!adjustment_has_scrollable_range(10.0, 34.0, 24.0));
    }

    #[test]
    fn scrollbar_range_is_visible_when_scrollback_exceeds_page() {
        assert!(adjustment_has_scrollable_range(0.0, 25.0, 24.0));
        assert!(adjustment_has_scrollable_range(10.0, 35.0, 24.0));
        assert!(adjustment_has_scrollable_range(0.0, 2.0, 0.0));
    }

    #[test]
    fn enable_env_flags_require_truthy_values() {
        for value in ["1", "true", "TRUE", "yes", "on", " On "] {
            assert!(
                env_flag_value_enabled(value),
                "{value:?} should enable the legacy path"
            );
        }
        for value in ["", "0", "false", "no", "off", "disabled"] {
            assert!(
                !env_flag_value_enabled(value),
                "{value:?} should keep the native VTE IM path"
            );
        }
    }

    #[test]
    fn ctrl_c_bypass_only_matches_plain_ctrl_c() {
        use gtk::gdk::{Key, ModifierType};

        assert!(is_plain_ctrl_c(Key::c, ModifierType::CONTROL_MASK));
        assert!(!is_plain_ctrl_c(Key::c, ModifierType::empty()));
        assert!(!is_plain_ctrl_c(
            Key::c,
            ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK
        ));
        assert!(!is_plain_ctrl_c(
            Key::c,
            ModifierType::CONTROL_MASK | ModifierType::ALT_MASK
        ));
        assert!(!is_plain_ctrl_c(Key::v, ModifierType::CONTROL_MASK));
    }

    #[test]
    fn flatpak_ibus_bypass_leaves_text_on_vte_im_path() {
        use gtk::gdk::Key;

        // Character keys must reach IBus so composition works; only the
        // plain non-character / edit keys the 22.04 daemon-path drops are
        // bypassed. Enter is handled by the dedicated commit-ordering
        // controller, not this table, so it stays off the bypass too.
        for key in [
            Key::space,
            Key::a,
            Key::_1,
            Key::question,
            Key::period,
            Key::KP_1,
            Key::KP_Add,
            Key::Return,
            Key::ISO_Enter,
            Key::KP_Enter,
        ] {
            assert!(
                flatpak_ibus_bypass_bytes(key).is_none(),
                "{key:?} must stay on VTE's IM path"
            );
        }
    }

    #[test]
    fn flatpak_ibus_bypass_keeps_navigation_and_function_keys() {
        use gtk::gdk::Key;

        assert_eq!(flatpak_ibus_bypass_bytes(Key::Left), Some(&b"\x1b[D"[..]));
        assert_eq!(
            flatpak_ibus_bypass_bytes(Key::KP_Right),
            Some(&b"\x1b[C"[..])
        );
        assert_eq!(
            flatpak_ibus_bypass_bytes(Key::Insert),
            Some(&b"\x1b[2~"[..])
        );
        assert_eq!(flatpak_ibus_bypass_bytes(Key::F5), Some(&b"\x1b[15~"[..]));
        assert_eq!(
            flatpak_ibus_bypass_bytes(Key::Page_Up),
            Some(&b"\x1b[5~"[..])
        );
        assert_eq!(
            flatpak_ibus_bypass_bytes(Key::KP_Page_Down),
            Some(&b"\x1b[6~"[..])
        );
    }

    #[test]
    fn flatpak_ibus_bypass_covers_deletion_keys() {
        use gtk::gdk::Key;

        assert_eq!(
            flatpak_ibus_bypass_bytes(Key::BackSpace),
            Some(&b"\x7f"[..])
        );
        assert_eq!(
            flatpak_ibus_bypass_bytes(Key::Delete),
            Some(&b"\x1b[3~"[..])
        );
        assert_eq!(
            flatpak_ibus_bypass_bytes(Key::KP_Delete),
            Some(&b"\x1b[3~"[..])
        );
    }

    #[test]
    fn ibus_nav_workaround_defaults_on_in_flatpak_and_wsl() {
        // On by default inside the sandbox, with or without the enable env.
        assert!(should_install_ibus_nav_workaround(
            false, false, true, false
        ));
        assert!(should_install_ibus_nav_workaround(false, true, true, false));
        // On by default under WSL/WSLg.
        assert!(should_install_ibus_nav_workaround(
            false, false, false, true
        ));
        // Force-on outside the sandbox via the enable env.
        assert!(should_install_ibus_nav_workaround(
            false, true, false, false
        ));
        // Off when neither condition holds.
        assert!(!should_install_ibus_nav_workaround(
            false, false, false, false
        ));
        // The disable env wins everywhere (bisection kill switch).
        assert!(!should_install_ibus_nav_workaround(true, true, true, true));
    }
}
