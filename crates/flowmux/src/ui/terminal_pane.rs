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

    pub fn has_selection(&self) -> bool {
        self.widget.has_selection()
    }

    pub fn copy_selection_to_clipboard(&self) {
        self.widget.copy_clipboard_format(vte::Format::Text);
    }

    pub fn paste_clipboard(&self) {
        self.widget.paste_clipboard();
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
    /// Pane-local new browser tab.
    pub on_new_browser_surface: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Pane-local close tab.
    pub on_close_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// Pane-local rename tab.
    pub on_rename_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// Tab right-click "Show in folder" → open file manager at the
    /// terminal surface's current working directory. Only invoked from
    /// terminal tab popovers; browser tabs skip the menu entirely.
    pub on_show_surface_folder: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// Per-surface "Copy path" / "Copy URL" handler. The dispatcher
    /// reads the surface kind and copies cwd or URL accordingly, so
    /// the same callback is reused by both terminal and browser
    /// right-click menus.
    pub on_copy_surface_text: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// Reorder a tab within the same pane by drag and drop. The third argument
    /// is the final 0-based index after the move, clamped if it exceeds length.
    pub on_reorder_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, usize)>>,
    /// A tab drag ended without landing on another tab drop target. The caller
    /// moves that live surface into a new top-level window and removes it from
    /// the source pane.
    pub on_tab_drag_to_new_window: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// Shared across all surface tabs in one window for the duration of a drag.
    /// The source tab uses this to distinguish a true no-target drag from a
    /// rejected drop on a known tab (self/cross-pane/invalid payload).
    pub tab_drag_drop_seen: Rc<Cell<bool>>,
    /// VTE reported that a terminal surface changed its cwd.
    pub on_terminal_cwd_changed: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, PathBuf)>>,
    /// WebKit reported that a browser pane navigated to a new URL.
    pub on_browser_uri_changed: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, String)>>,
    /// WebKit reported that a browser pane's page title changed.
    pub on_browser_title_changed: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, String)>>,
    /// VTE reported an OSC 0/2 window title, often emitted by programs such as
    /// vi, claude, codex, or tmux inside the shell. Empty titles are ignored by
    /// the caller.
    pub on_terminal_title_changed: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, String)>>,
    /// Return the current user options. Used when creating a new BrowserPane to
    /// choose the engine and apply zoom immediately after widget creation. This
    /// cheaply clones the `Rc<RefCell<Options>>` held by WindowController, so
    /// dialog updates are visible on the next call.
    pub read_options: Rc<dyn Fn() -> flowmux_config::options::Options>,
    /// Return the surface's current 0-based index within the same pane. Tab DnD
    /// uses PaneRegistry::surface_tabs to compute final_index from the source
    /// and target relative positions.
    pub position_of_surface_in_pane: Rc<dyn Fn(PaneId, SurfaceId) -> Option<usize>>,
    /// Called when Ctrl+click selects a URL inside the terminal. The caller
    /// opens that URL in a new browser tab in the same pane
    /// (GtkCommand::OpenUrlInBrowserTab). The URL arrives with trailing
    /// punctuation already trimmed.
    pub on_open_url: Rc<RefCell<dyn FnMut(PaneId, String)>>,
}

impl TerminalPane {
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
        let term = vte::Terminal::new();
        term.set_hexpand(true);
        term.set_vexpand(true);
        term.set_scrollback_lines(10_000);
        term.set_audible_bell(false);

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
        let scrollbar = gtk::Scrollbar::new(
            gtk::Orientation::Vertical,
            Some(&scroll_adjustment),
        );
        scrollbar.set_halign(gtk::Align::End);
        scrollbar.set_valign(gtk::Align::Fill);
        // Force the scrollbar to render unconditionally so the 22.04
        // Flatpak path does not auto-hide it. A standalone bar with
        // explicit width survives the GTK 4.16+ overlay-scrolling
        // heuristics that the runtime applies by default.
        scrollbar.set_visible(true);
        scrollbar.set_can_focus(false);
        scrollbar.set_width_request(12);
        container.add_overlay(&scrollbar);

        // Make inline IME preedit (e.g. a composing Hangul syllable) visible
        // even when the foreground app has hidden the terminal cursor.
        install_preedit_redraw_on_keystroke(&container, &term);

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
            install_smart_page_keys(&term, &scroll_adjustment);
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
        // CLI call. The callback remains in `PaneCallbacks` for the
        // hypothetical day VTE upstream reinstates the signal.
        let _unused_notification_cb = &callbacks.on_notification;

        // BEL — generic attention.
        {
            let cb = callbacks.on_bell.clone();
            let id = id;
            term.connect_bell(move |_term| {
                (cb.borrow_mut())(id);
            });
        }

        // URL recognition for opening terminal URLs in an internal browser tab
        // via Ctrl+click. A PCRE2 regex match changes hover to the pointer
        // cursor; Ctrl+left-click opens the URL in a new browser tab in the
        // same pane. Plain clicks continue into VTE text selection.
        install_url_link_handling(&term, id, callbacks.on_open_url.clone());
        install_flatpak_ibus_nav_workaround(&term, smart_page_enabled);

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
            let on_copy_text = callbacks.on_copy_surface_text.clone();
            let surface_for_menu = surface;
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

                // Copy / Paste at the top, mirroring the copy/paste
                // keybindings. Copy is a no-op with no selection so we
                // never clobber the clipboard; Paste lets VTE bracket
                // the text when the app set DECSET 2004.
                let copy = mk("Copy");
                let pop = popover.clone();
                let term_for_copy = term_widget.clone();
                copy.connect_clicked(move |_| {
                    pop.popdown();
                    if term_for_copy.has_selection() {
                        term_for_copy.copy_clipboard_format(vte::Format::Text);
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

        // VTE's spawn_async expects an envv array of `KEY=VALUE` strings,
        // or an empty slice to inherit the parent's environment. We build
        // a minimal extension on top of inheritance: parent env is
        // implicit, and we append flowmux-specific entries so agents can
        // self-discover their pane.
        let envv_strings = flowmux_terminal::env_to_kv_strings(&extra_env);
        let envv_refs: Vec<&str> = envv_strings.iter().map(String::as_str).collect();

        let pid: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));
        let pid_for_cb = pid.clone();
        term.spawn_async(
            vte::PtyFlags::DEFAULT,
            cwd_str,
            &argv_refs,
            &envv_refs,
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
            widget: term,
            container,
            pid,
        }
    }

    pub fn feed(&self, bytes: &[u8]) {
        self.widget.feed_child(bytes);
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
    /// touches the VTE / ibus path — the composing syllable is not
    /// committed by it. We force the commit with a focus-cycle flush, feed
    /// `bytes` from VTE's `commit` signal on an idle tick (VTE writes the
    /// committed bytes during that emission, so an idle feed always lands
    /// behind them), and fall back to a direct feed when nothing is
    /// composing (no commit fires). The `commit` handler is one-shot:
    /// armed per call and disconnected on the first commit or the
    /// fallback, so it never disturbs ordinary typing.
    pub fn feed_after_preedit_commit(&self, bytes: &'static [u8]) {
        if !ibus_im_module_active() {
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

    pub fn connect_current_dir_notify(
        &self,
        callback: impl Fn(&Self) + Clone + 'static,
    ) -> glib::SignalHandlerId {
        let pane = self.clone();
        self.widget
            .connect_current_directory_uri_notify(move |_| callback(&pane))
    }

    pub fn connect_title_notify(
        &self,
        callback: impl Fn(&Self, String) + Clone + 'static,
    ) -> glib::SignalHandlerId {
        let pane = self.clone();
        self.widget.connect_window_title_notify(move |widget| {
            let title = widget
                .window_title()
                .map(|t| t.to_string())
                .unwrap_or_default();
            callback(&pane, title);
        })
    }
}

/// Prepend `flowmuxctl pty-tee --pane <id> --surface <id> --` in front
/// of the user's shell argv so OSC 9 / 99 / 777 escapes emitted by
/// terminal-side agents (Claude Code, Codex, OpenCode, …) reach the
/// daemon's `Request::Notify` path. VTE 0.68 (Ubuntu 22.04) and 0.76
/// (Ubuntu 24.04) both silently swallow those OSCs because they were
/// only ever wired into the Konsole-private `notification-received`
/// signal that upstream VTE compiles out — the tee is the only
/// distribution-agnostic interception point.
///
/// Falls back to the original argv when `flowmuxctl` cannot be
/// located. The terminal then works exactly as before, just without
/// OSC-driven alarms — strictly a graceful degradation.
fn wrap_argv_with_pty_tee(
    argv: Vec<String>,
    pane: PaneId,
    surface: SurfaceId,
) -> Vec<String> {
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

fn install_url_link_handling(
    term: &vte::Terminal,
    pane_id: PaneId,
    on_open_url: Rc<RefCell<dyn FnMut(PaneId, String)>>,
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
    let tag = term.match_add_regex(&regex, 0);
    // Show a pointer cursor on hover. The pointer appears even without Ctrl,
    // but activation requires Ctrl, matching the gnome-terminal UX pattern:
    // always show the hint, gate the action behind the modifier.
    term.match_set_cursor_name(tag, "pointer");
    tracing::debug!(%pane_id, tag, "URL match registered on terminal");

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

    let term_widget = term.clone();
    click.connect_pressed(move |gesture, _n_press, x, y| {
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
        let url_raw: Option<String> = term_widget
            .check_hyperlink_at(x, y)
            .map(|g| g.to_string())
            .or_else(|| {
                let (m, _tag) = term_widget.check_match_at(x, y);
                m.map(|g| g.to_string())
            });

        let Some(raw) = url_raw else {
            // Ctrl was held, but the click was not on a URL. Treat it as a
            // selection attempt and release the sequence so VTE features such
            // as Ctrl+drag block selection still work.
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        };
        let url = trim_url_trailing(&raw);
        if url.is_empty() {
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        }
        tracing::info!(%pane_id, %url, "Ctrl+click on terminal URL → open in browser tab");
        (on_open_url.borrow_mut())(pane_id, url);
        // We handled the URL, so claim the sequence to prevent VTE selection.
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    term.add_controller(click);
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
/// (`IBUS_ENABLE_SYNC_MODE=1`, forced in `main.rs`); a short follow-up redraw
/// covers async input methods (fcitx, IBus without sync) whose
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

pub(crate) const ALT_ENTER_BYTES: &[u8] = b"\x1b\r";

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
fn install_smart_page_keys(term: &vte::Terminal, scroll_adjustment: &gtk::Adjustment) {
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
        let adj = scroll_adjustment.clone();
        let direction = *direction;
        let always_scroll = *always_scroll;
        let action = gtk::CallbackAction::new(move |_, _| {
            let upper = adj.upper();
            let page = adj.page_size().max(1.0);
            let has_scrollback = upper > page;
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

/// Legacy workaround for the Ubuntu 22.04 host + GNOME 48 Flatpak
/// runtime IBus regression where plain navigation / editing keys
/// during Hangul preedit are silently dropped. The deciding
/// reproducer was that the same keys with `Ctrl` held down worked
/// fine on the same setup — Ctrl takes the event past GTK's IM filter
/// without involving IBus at all. That places the bug in the IBus
/// daemon-path the runtime's GTK4 immodule uses for plain
/// non-character keys, somewhere between the in-sandbox client and
/// the host's IBus 1.5.26 daemon.
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

fn should_install_flatpak_ibus_nav_workaround(
    disable_env_present: bool,
    enable_env_present: bool,
    flatpak_info_exists: bool,
) -> bool {
    !disable_env_present && (flatpak_info_exists || enable_env_present)
}

fn install_flatpak_ibus_nav_workaround(term: &vte::Terminal, smart_page_enabled: bool) {
    if !should_install_flatpak_ibus_nav_workaround(
        std::env::var_os("FLOWMUX_NO_IBUS_NAV_WORKAROUND").is_some(),
        env_flag_enabled("FLOWMUX_ENABLE_IBUS_NAV_WORKAROUND"),
        std::path::Path::new("/.flatpak-info").exists(),
    ) {
        return;
    }

    let controller = gtk::ShortcutController::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.set_scope(gtk::ShortcutScope::Local);

    let bind = |keyval: gtk::gdk::Key, bytes: &'static [u8]| {
        let term_widget = term.clone();
        let action = gtk::CallbackAction::new(move |_, _| {
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
/// **Outside a sandbox** — run `$SHELL -l`. The `-l` flag makes any
/// POSIX-ish shell source the per-shell profile (.bash_profile /
/// .profile / .zprofile / fish login conf), which in turn pulls
/// .bashrc / .zshrc so the user's PS1 + helpers are defined before
/// the first prompt. Same convention xterm / alacritty / kitty use.
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

/// Detect whether the current process is running inside a Flatpak
/// sandbox. Flatpak sets `FLATPAK_ID` for sandboxed apps and writes a
/// `/.flatpak-info` file at the sandbox root; either is sufficient
/// proof.
fn is_flatpak_sandbox() -> bool {
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

try:
    while True:
        # 0.5s tick doubles as a winsize poll: flatpak-spawn doesn't
        # forward SIGWINCH reliably so we re-read TIOCGWINSZ on every
        # idle wake-up and push it through to the host PTY.
        rfds, _, _ = select.select([0, fd], [], [], 0.5)
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
    fn shift_enter_byte_sequence_is_esc_cr() {
        // Agent TUIs (Claude, Codex, OpenCode) all treat ESC+CR as "insert
        // newline". Lock the wire format so a future refactor does not turn
        // Shift+Enter into a literal newline submission again.
        assert_eq!(ALT_ENTER_BYTES, b"\x1b\r");
    }

    #[test]
    fn vte_capture_key_controllers_are_legacy_opt_in() {
        assert!(!terminal_capture_key_controllers_enabled(false));
        assert!(terminal_capture_key_controllers_enabled(true));
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
    fn flatpak_ibus_nav_workaround_defaults_on_in_flatpak() {
        // On by default inside the sandbox, with or without the enable env.
        assert!(should_install_flatpak_ibus_nav_workaround(
            false, false, true
        ));
        assert!(should_install_flatpak_ibus_nav_workaround(
            false, true, true
        ));
        // Force-on outside the sandbox via the enable env.
        assert!(should_install_flatpak_ibus_nav_workaround(
            false, true, false
        ));
        // Off when neither condition holds.
        assert!(!should_install_flatpak_ibus_nav_workaround(
            false, false, false
        ));
        // The disable env wins everywhere (bisection kill switch).
        assert!(!should_install_flatpak_ibus_nav_workaround(
            true, true, true
        ));
    }
}
