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
use gtk::glib::translate::{from_glib_full, ToGlibPtr};
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use vte::prelude::*;

#[derive(Clone)]
pub struct TerminalPane {
    pub id: PaneId,
    /// The VTE widget itself. Doubles as the pane's *root* widget — the
    /// thing that gets inserted into the workspace's pane stack. Callers
    /// pass `pane.widget.clone().upcast::<gtk::Widget>()` whenever they
    /// need a generic widget handle.
    ///
    /// **Do not wrap this in another widget when inserting it into the
    /// pane tree.** A previous attempt (commit eb2d176, reverted) hosted
    /// a Shift+Enter `ShortcutController` on a one-child `gtk::Box`
    /// wrapper around the VTE; the wrapper's measure() did not propagate
    /// VTE's natural character-cell minimum the way a direct child does,
    /// so once two `gtk::Paned` splits were nested, tig / vim / htop
    /// rendered with the left/right/top/bottom edges clipped. The bare
    /// `vte::Terminal` must remain the immediate child of whatever
    /// container holds it for the existing `set_shrink_*_child(false)`
    /// fix (commit b507b7a) on each `gtk::Paned` to keep producing
    /// correct sizes.
    pub widget: vte::Terminal,
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
    /// Pane-local new browser tab.
    pub on_new_browser_surface: Rc<RefCell<dyn FnMut(PaneId)>>,
    /// Pane-local close tab.
    pub on_close_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// Pane-local rename tab.
    pub on_rename_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    /// Reorder a tab within the same pane by drag and drop. The third argument
    /// is the final 0-based index after the move, clamped if it exceeds length.
    pub on_reorder_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, usize)>>,
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
        install_shift_enter_newline_handling(&term);
        install_flatpak_ibus_nav_workaround(&term);

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
            default_shell_argv()
        } else {
            argv
        };
        // Wrap the shell argv with `flowmuxctl pty-tee` so OSC 9/99/777
        // emitted by terminal-side agents (Claude Code, Codex, …)
        // reach the desktop notification subsystem even on VTE
        // versions (0.68-compatible GTK4 builds, 0.76 on Ubuntu 24.04) that
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
            pid,
        }
    }

    pub fn feed(&self, bytes: &[u8]) {
        self.widget.feed_child(bytes);
    }
}

/// Prepend `flowmuxctl pty-tee --pane <id> --surface <id> --` in front
/// of the user's shell argv so OSC 9 / 99 / 777 escapes emitted by
/// terminal-side agents (Claude Code, Codex, OpenCode, …) reach the
/// daemon's `Request::Notify` path. VTE 0.68-compatible GTK4 builds and 0.76
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

    // 2) Left-click gesture. Inspect button-press in capture phase first to
    //    determine whether Ctrl is held.
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
    click.set_propagation_phase(gtk::PropagationPhase::Capture);

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

        // VTE's point-based URL APIs are gated behind VTE 0.70. The
        // Ubuntu 22.04-compatible feature floor is 0.66, so use the older
        // cell-based `match_check` entry point and convert click pixels to
        // terminal grid coordinates. This keeps regex URL activation on the
        // downgraded backend; OSC 8 hyperlink activation remains a VTE 0.70+
        // enhancement.
        let url_raw = check_regex_match_at(&term_widget, x, y);

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

fn check_regex_match_at(term: &vte::Terminal, x: f64, y: f64) -> Option<String> {
    let char_width = term.char_width();
    let char_height = term.char_height();
    if char_width <= 0 || char_height <= 0 {
        return None;
    }
    let column = (x / char_width as f64).floor().max(0.0) as std::os::raw::c_long;
    let row = (y / char_height as f64).floor().max(0.0) as std::os::raw::c_long;
    let mut tag = 0;
    let matched: Option<glib::GString> = unsafe {
        from_glib_full(vte::ffi::vte_terminal_match_check(
            term.to_glib_none().0,
            column,
            row,
            &mut tag,
        ))
    };
    matched.map(|g| g.to_string())
}

const ALT_ENTER_BYTES: &[u8] = b"\x1b\r";

/// Intercept Shift+Enter on the VTE widget and translate it to ESC+CR — the
/// byte sequence agent TUIs (Claude, Codex, OpenCode) already treat as
/// "insert newline" — before VTE's own Enter handler sees the event.
///
/// ### Why a `ShortcutController` and not an `EventControllerKey`
///
/// An earlier version of this hook used `gtk::EventControllerKey` in
/// `PropagationPhase::Capture`. That sits in front of VTE's internal IM
/// filter on every keystroke, and on GTK 4.6 + VTE 0.68-compatible builds
/// combination the IBus Hangul preedit handler ends up desynchronized when
/// any capture-phase key controller is attached to the VTE widget. The
/// reported symptom is that Backspace and the arrow keys stop reacting
/// while a Korean syllable is being composed — IBus Hangul never receives
/// them, so the preedit cannot be edited or committed. Plain ASCII typing,
/// composition itself, and any key event outside of preedit are unaffected,
/// which is why the regression slipped through.
///
/// `gtk::ShortcutController` only ever fires when an incoming event matches
/// one of its `KeyvalTrigger`s. Every other keystroke — including the keys
/// IBus Hangul cares about during preedit — propagates untouched to VTE's
/// native IM path, so the GTK4 + IBus pipeline behaves exactly as it would
/// on a vanilla `vte::Terminal` with no controller attached.
fn install_shift_enter_newline_handling(term: &vte::Terminal) {
    let controller = gtk::ShortcutController::new();
    // Capture phase: VTE's own Shift+Enter handling sends \r to the PTY at
    // the target phase, which would defeat the translation. Capture beats
    // it; matching events are consumed before VTE sees them, non-matching
    // events propagate normally.
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.set_scope(gtk::ShortcutScope::Local);

    let term_widget = term.clone();
    let action = gtk::CallbackAction::new(move |_, _| {
        term_widget.feed_child(ALT_ENTER_BYTES);
        glib::Propagation::Stop
    });

    // Cover the three keysyms a layout can produce for Enter:
    //   - Return: the main keyboard's Enter key
    //   - KP_Enter: numpad Enter
    //   - ISO_Enter: keyboards that route through an ISO layout group
    // `KeyvalTrigger` matches only when the requested modifiers are exactly
    // present (CapsLock / NumLock latches are ignored), so Ctrl+Shift+Enter
    // and Alt+Shift+Enter stay on VTE's native path.
    for keyval in [
        gtk::gdk::Key::Return,
        gtk::gdk::Key::KP_Enter,
        gtk::gdk::Key::ISO_Enter,
    ] {
        let trigger = gtk::KeyvalTrigger::new(keyval, gtk::gdk::ModifierType::SHIFT_MASK);
        let shortcut = gtk::Shortcut::new(Some(trigger), Some(action.clone()));
        controller.add_shortcut(shortcut);
    }

    term.add_controller(controller);
}

/// Workaround for the Ubuntu 22.04 host + GNOME 48 Flatpak runtime
/// IBus regression where plain navigation / editing keys during
/// Hangul preedit are silently dropped. The deciding reproducer was
/// that the same keys with `Ctrl` held down worked fine on the same
/// setup — Ctrl takes the event past GTK's IM filter without
/// involving IBus at all. That places the bug in the IBus daemon-
/// path the runtime's GTK4 immodule uses for plain non-character
/// keys, somewhere between the in-sandbox client and the host's
/// IBus 1.5.26 daemon. Neither half is under flowmux's control, so
/// the only available fix is to bypass that path from the
/// application side.
///
/// Approach: install a capture-phase `ShortcutController` on the
/// VTE widget for the affected plain keys. When one matches we
/// feed the equivalent terminal byte sequence straight to the PTY
/// and consume the event so VTE's own IM-aware handler never sees
/// it — exactly the path a plain key takes on a working host
/// (IBus says "not for me", GTK passes the event through, VTE
/// feeds the PTY). Letter / number / punctuation keys are not
/// intercepted, so Korean composition itself still goes through
/// IBus and preedit keeps working for character input.
///
/// What is intentionally **not** intercepted:
///   * Space. Its natural role inside IBus is "commit the current
///     preedit + insert space", and bypassing it would feed bare
///     0x20 to the PTY without committing the Korean syllable,
///     dropping the user's text on the floor. Commit with
///     `Ctrl+Space` (which works on this setup precisely because
///     the Ctrl modifier bypasses IBus) before pressing Space.
///   * Letter / number / punctuation keys. IBus needs to see them.
///   * Function keys F1..F12. Encoding varies enough across
///     terminfo profiles that getting it wrong is worse than
///     leaving them on the broken-but-rarely-used IBus path.
///
/// **Enter is bypassed by user request, with the same caveat as
/// Space would carry.** Pressing plain Enter while preedit is on
/// screen sends only `\r` to the PTY; the in-progress Korean
/// syllable is NOT committed first and is lost. Users who need to
/// keep that syllable should commit it with `Ctrl+Enter` before
/// hitting Enter, or fall back to typing the syllable + Space (no
/// bypass) + Enter (bypass). The trade-off is deliberate: a
/// working plain Enter is more useful than a no-op one.
///
/// Active only inside a Flatpak sandbox (`/.flatpak-info` exists).
/// Native builds talk to a matching-version IBus daemon and do
/// not exhibit the drop, so they keep the regular path with no
/// behavioural change. `FLOWMUX_NO_IBUS_NAV_WORKAROUND=1`
/// disables the bypass for bisection.
fn install_flatpak_ibus_nav_workaround(term: &vte::Terminal) {
    if std::env::var_os("FLOWMUX_NO_IBUS_NAV_WORKAROUND").is_some() {
        return;
    }
    if !std::path::Path::new("/.flatpak-info").exists() {
        return;
    }

    let controller = gtk::ShortcutController::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.set_scope(gtk::ShortcutScope::Local);

    let bind = |keyval: gtk::gdk::Key, bytes: &'static [u8]| {
        let term_widget = term.clone();
        let action = gtk::CallbackAction::new(move |_, _| {
            term_widget.feed_child(bytes);
            glib::Propagation::Stop
        });
        let trigger = gtk::KeyvalTrigger::new(keyval, gtk::gdk::ModifierType::empty());
        controller.add_shortcut(gtk::Shortcut::new(Some(trigger), Some(action)));
    };

    // Standard xterm encodings — same bytes VTE itself would write
    // to the PTY when its key handler reached the forward-to-PTY
    // branch on a working IM path. We are not changing semantics,
    // only skipping the broken IBus round trip.
    bind(gtk::gdk::Key::BackSpace, b"\x7f");
    bind(gtk::gdk::Key::Delete, b"\x1b[3~");
    bind(gtk::gdk::Key::Tab, b"\t");
    bind(gtk::gdk::Key::Escape, b"\x1b");
    // Enter: plain CR. Cost — any pending Hangul preedit is silently
    // dropped (the GTK4 immodule does not expose an external commit
    // hook). Documented in the function header above.
    bind(gtk::gdk::Key::Return, b"\r");
    bind(gtk::gdk::Key::ISO_Enter, b"\r");
    bind(gtk::gdk::Key::Left, b"\x1b[D");
    bind(gtk::gdk::Key::Right, b"\x1b[C");
    bind(gtk::gdk::Key::Up, b"\x1b[A");
    bind(gtk::gdk::Key::Down, b"\x1b[B");
    bind(gtk::gdk::Key::Home, b"\x1b[H");
    bind(gtk::gdk::Key::End, b"\x1b[F");
    bind(gtk::gdk::Key::Page_Up, b"\x1b[5~");
    bind(gtk::gdk::Key::Page_Down, b"\x1b[6~");
    // Keypad variants of the same keys — some layouts (notebook + Fn,
    // X11 with NumLock off, …) report them as the KP_* keysyms even
    // when the user hits the equivalent unshifted key.
    bind(gtk::gdk::Key::KP_Delete, b"\x1b[3~");
    bind(gtk::gdk::Key::KP_Enter, b"\r");
    bind(gtk::gdk::Key::KP_Left, b"\x1b[D");
    bind(gtk::gdk::Key::KP_Right, b"\x1b[C");
    bind(gtk::gdk::Key::KP_Up, b"\x1b[A");
    bind(gtk::gdk::Key::KP_Down, b"\x1b[B");
    bind(gtk::gdk::Key::KP_Home, b"\x1b[H");
    bind(gtk::gdk::Key::KP_End, b"\x1b[F");
    bind(gtk::gdk::Key::KP_Page_Up, b"\x1b[5~");
    bind(gtk::gdk::Key::KP_Page_Down, b"\x1b[6~");

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
}
