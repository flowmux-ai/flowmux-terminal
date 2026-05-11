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

        // URL recognition for opening terminal URLs in an internal browser tab
        // via Ctrl+click. A PCRE2 regex match changes hover to the pointer
        // cursor; Ctrl+left-click opens the URL in a new browser tab in the
        // same pane. Plain clicks continue into VTE text selection.
        install_url_link_handling(&term, id, callbacks.on_open_url.clone());
        install_shift_enter_newline_handling(&term);

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
            root: term.clone().upcast(),
            widget: term,
            pid,
        }
    }

    pub fn feed(&self, bytes: &[u8]) {
        self.widget.feed_child(bytes);
    }
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

const ALT_ENTER_BYTES: &[u8] = b"\x1b\r";

fn install_shift_enter_newline_handling(term: &vte::Terminal) {
    // A traditional PTY does not carry a distinct Shift+Enter event.
    // Agent TUIs already treat Alt+Enter as "insert newline", so synthesize
    // that byte sequence for Shift+Enter before VTE sees a plain Enter.
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);

    let term_widget = term.clone();
    key.connect_key_pressed(move |_, keyval, _keycode, state| {
        if should_translate_shift_enter(keyval, state) {
            term_widget.feed_child(ALT_ENTER_BYTES);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });

    term.add_controller(key);
}

fn should_translate_shift_enter(keyval: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> bool {
    if !is_enter_key(keyval) || !state.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
        return false;
    }

    let native_modifiers = gtk::gdk::ModifierType::CONTROL_MASK
        | gtk::gdk::ModifierType::ALT_MASK
        | gtk::gdk::ModifierType::SUPER_MASK
        | gtk::gdk::ModifierType::HYPER_MASK
        | gtk::gdk::ModifierType::META_MASK;
    !state.intersects(native_modifiers)
}

fn is_enter_key(keyval: gtk::gdk::Key) -> bool {
    keyval == gtk::gdk::Key::Return
        || keyval == gtk::gdk::Key::KP_Enter
        || keyval == gtk::gdk::Key::ISO_Enter
}

/// argv used when the caller asks for the default shell (no explicit
/// command). The exact construction depends on whether we're running
/// inside a Flatpak sandbox.
///
/// * **Outside a sandbox** — run `$SHELL -l` (login shell). This is
///   the convention xterm / alacritty / kitty use; it makes any
///   POSIX-ish shell source the per-shell profile (.bash_profile /
///   .profile / .zprofile / fish login conf), which in turn pulls
///   .bashrc / .zshrc so the user's PS1 + helpers are defined before
///   the first prompt.
///
/// * **Inside Flatpak** — wrap with `flatpak-spawn --host --watch-bus`
///   so the shell actually runs on the host, not inside the sandbox.
///   Without this the sandbox's shell can't see the host's `~/.bashrc`
///   nor any host-installed tools (`git`, `tig`, `xset`, …), producing
///   `sh: git: command not found` for every command. `--watch-bus`
///   ties the host-side process lifetime to ours so closing the pane
///   actually reaps the shell. Requires
///   `--talk-name=org.freedesktop.Flatpak` in the Flatpak manifest's
///   finish-args.
fn default_shell_argv() -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    if is_flatpak_sandbox() {
        vec![
            "flatpak-spawn".into(),
            "--host".into(),
            "--watch-bus".into(),
            shell,
            "-l".into(),
        ]
    } else {
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
    fn shift_enter_is_translated_for_prompt_newlines() {
        assert!(should_translate_shift_enter(
            gtk::gdk::Key::Return,
            gtk::gdk::ModifierType::SHIFT_MASK
        ));
        assert!(should_translate_shift_enter(
            gtk::gdk::Key::KP_Enter,
            gtk::gdk::ModifierType::SHIFT_MASK | gtk::gdk::ModifierType::LOCK_MASK
        ));
        assert_eq!(ALT_ENTER_BYTES, b"\x1b\r");
    }

    #[test]
    fn plain_or_modified_enter_keeps_native_terminal_handling() {
        assert!(!should_translate_shift_enter(
            gtk::gdk::Key::Return,
            gtk::gdk::ModifierType::empty()
        ));
        assert!(!should_translate_shift_enter(
            gtk::gdk::Key::Return,
            gtk::gdk::ModifierType::SHIFT_MASK | gtk::gdk::ModifierType::CONTROL_MASK
        ));
        assert!(!should_translate_shift_enter(
            gtk::gdk::Key::Return,
            gtk::gdk::ModifierType::SHIFT_MASK | gtk::gdk::ModifierType::ALT_MASK
        ));
    }
}
