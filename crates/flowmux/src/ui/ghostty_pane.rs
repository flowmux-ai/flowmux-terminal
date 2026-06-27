// SPDX-License-Identifier: GPL-3.0-or-later
//! libghostty-vt-backed terminal pane — flowmux's only terminal backend.
//!
//! Renders the grid itself from `flowmux_terminal`'s libghostty-vt core (PTY +
//! `vt::Vt` + a Cairo/Pango `DrawingArea`). Stored as
//! [`crate::ui::pane_terminal::PaneTerminal`] (an alias for this type).
//!
//! Parity status: rendering (theme font/colors/metrics), PTY I/O, keyboard +
//! inline IME preedit (Hangul/CJK), focus, font, resize, drag selection +
//! clipboard copy/paste, mouse reporting (modes 1000/1002/1003), wheel
//! scrollback, Ctrl-click URLs, OSC 0/2 title tracking, and OSC 7 / `/proc`
//! cwd all work. Not yet matched: a visible scrollbar widget (libghostty
//! exposes no viewport-offset query) — wheel scrolling works regardless.

use std::cell::{Cell, RefCell};
use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::cairo;
use gtk::gdk;
use gtk::glib;
use gtk::pango;
use gtk::prelude::*;

use flowmux_core::{PaneId, SurfaceId};
use flowmux_terminal::pty::Pty;
use flowmux_terminal::vt::{Cell as VtCell, Colors, MouseAction, MouseButton, Rgb, Vt};

use crate::ui::pane_terminal::PaneCallbacks;

const DEFAULT_FONT: &str = "Monospace 12";
const SCROLLBACK: usize = 10_000;
/// Shift+Enter inserts a literal newline in common shell line editors without
/// submitting the command: Ctrl+V quotes the following Ctrl+J/LF. Quoting
/// Enter/CR instead inserts a visible `^M`.
pub const INSERT_NEWLINE_BYTES: &[u8] = b"\x16\n";
/// Right-edge gutter reserved for the overlaid scrollbar (its 12px width plus a
/// little slack). Terminal columns are computed against the width minus this so
/// content never renders under the scrollbar — which made a long line at the
/// right edge look unwrapped and corrupted.
const SCROLLBAR_GUTTER: f64 = 14.0;

/// Columns that fit in a pane `w` pixels wide for a `cell_w`-wide cell,
/// reserving the scrollbar gutter. Reserved unconditionally so the column count
/// stays stable whether or not the scrollbar is currently shown.
fn cols_for_width(w: i32, cell_w: f64) -> u16 {
    if cell_w <= 0.0 {
        return 1;
    }
    let usable = (w as f64 - SCROLLBAR_GUTTER).max(cell_w);
    ((usable / cell_w).floor() as i64).clamp(1, u16::MAX as i64) as u16
}

/// Shared, mutable terminal state behind an `Rc<RefCell<…>>` so the draw,
/// resize, key, and PTY-pump closures can all reach it on the GTK thread.
struct State {
    vt: Vt,
    pty: Pty,
    font: pango::FontDescription,
    font_scale: f64,
    cell_w: f64,
    cell_h: f64,
    ascent: f64,
    cols: u16,
    rows: u16,
    /// Theme cursor/selection colors for host-side drawing (libghostty owns the
    /// fg/bg/palette via `Vt::set_default_colors`/`set_palette`; selection and
    /// cursor are painted by us to match VTE).
    cursor_color: Option<Rgb>,
    selection_bg: Option<Rgb>,
    selection_fg: Option<Rgb>,
    /// True while a drag selection is active (drives has_selection/copy).
    has_sel: bool,
    /// Inline IME preedit string (composing Hangul/CJK) shown at the cursor.
    preedit: String,
    /// Last pointer position in surface pixels (for wheel mouse reports).
    pointer: (f64, f64),
    /// Whether a left-button drag selection is in progress, and its anchor cell.
    selecting: bool,
    anchor: (u16, u16),
    /// A primary press deferred while app mouse reporting is on: we don't yet
    /// know if it's a click (report to the app) or the start of a drag (local
    /// text selection). Holds the press `(x, y, mods)`. Resolved on the first
    /// motion that leaves the anchor cell (→ selection) or on release without
    /// movement (→ click is sent to the app). See the press/motion/release
    /// handlers in `install_input`.
    pending_app_press: Option<(f64, f64, u8)>,
    /// Last OSC title seen, to fire the title-changed callback only on change.
    last_title: String,
    /// Last working directory forwarded, to fire the cwd-changed callback only
    /// on change. Tracks the shell's OSC 7 announcement (the VTE path used
    /// VTE's `current-directory-uri`; libghostty has no signal so we poll the
    /// vt's pwd after each read).
    last_cwd: Option<PathBuf>,
    /// Cursor blink: whether blinking is enabled, the current on/off phase, and
    /// whether this pane is focused. The cursor is only hidden mid-blink while
    /// `blink_enabled && focused && !blink_phase_on`; an unfocused or
    /// blink-disabled pane always shows a steady cursor. The timer driving
    /// `blink_phase_on` lives on `GhosttyPane` (see `restart_cursor_blink`).
    blink_enabled: bool,
    blink_phase_on: bool,
    focused: bool,
    /// Reused per-frame buffer for `Vt::read_grid_into`, so the draw callback
    /// doesn't allocate a fresh `cols*rows` cell `Vec` on every repaint.
    grid_scratch: Vec<VtCell>,
}

type PendingInput = Rc<RefCell<Option<Vec<u8>>>>;

impl State {
    /// Recompute cell metrics for the current (scaled) font.
    fn remeasure(&mut self) {
        let (w, h, a) = measure_cell(&self.scaled_font());
        self.cell_w = w;
        self.cell_h = h;
        self.ascent = a;
    }

    fn scaled_font(&self) -> pango::FontDescription {
        let mut f = self.font.clone();
        // Pango sizes are in 1024ths of a point; scale around the base size.
        let base = if self.font.size() > 0 {
            self.font.size()
        } else {
            12 * pango::SCALE
        };
        f.set_size(((base as f64) * self.font_scale).round() as i32);
        f
    }
}

/// libghostty-backed terminal pane. Cheap to clone (all handles are refcounted).
#[derive(Clone)]
pub struct GhosttyPane {
    pub id: PaneId,
    surface: SurfaceId,
    pub container: gtk::Overlay,
    area: gtk::DrawingArea,
    state: Rc<RefCell<State>>,
    pid: Rc<Cell<Option<i32>>>,
    im: gtk::IMMulticontext,
    pending_after_preedit: PendingInput,
    /// Overlaid vertical scrollbar + its adjustment, shown only when scrollback
    /// exists. `syncing` suppresses the value-changed handler while we push the
    /// terminal's own scroll position into the adjustment.
    scrollbar: gtk::Scrollbar,
    adj: gtk::Adjustment,
    syncing: Rc<Cell<bool>>,
    /// Cursor blink timer (a `glib` timeout) and its half-period. Shared so the
    /// cheap `Clone` keeps one timer per pane. `restart_cursor_blink` tears down
    /// the old source and installs a new one whenever the setting changes.
    blink_source: Rc<RefCell<Option<glib::SourceId>>>,
    blink_interval_ms: Rc<Cell<u32>>,
    /// Last cwd reported by the 1-second poll fallback. The poll diffs against
    /// this so unchanged panes never reach the daemon state mutex; only a real
    /// `cd` (on OSC-7-naive shells) takes the lock + tree walk.
    last_polled_cwd: Rc<RefCell<Option<PathBuf>>>,
}

impl GhosttyPane {
    /// Build a libghostty terminal and spawn `argv` (falling back to `$SHELL`),
    /// matching [`TerminalPane::spawn`]'s signature so the two are
    /// interchangeable at the call site.
    pub fn spawn(
        id: PaneId,
        surface: SurfaceId,
        argv: Vec<String>,
        cwd: Option<PathBuf>,
        extra_env: Vec<(String, String)>,
        callbacks: PaneCallbacks,
    ) -> Self {
        let font = pango::FontDescription::from_string(DEFAULT_FONT);
        let (cell_w, cell_h, ascent) = measure_cell(&font);

        // Initial geometry; the first allocation resizes to fit.
        let cols: u16 = 80;
        let rows: u16 = 24;

        let vt = Vt::new(cols, rows, SCROLLBACK).expect("libghostty vt new");

        let argv_owned = if argv.is_empty() {
            vec![std::env::var("SHELL").unwrap_or_else(|_| "bash".into())]
        } else {
            argv
        };
        let argv_ref: Vec<&str> = argv_owned.iter().map(|s| s.as_str()).collect();

        // Ensure the child has a valid terminal identity. The VTE widget sets
        // TERM for its child automatically; the libghostty path must do it
        // itself. Without TERM, readline can't load terminfo and falls back to
        // horizontal-scroll line editing — a long line stops wrapping and shows
        // "<"/">" scroll markers instead. libghostty emulates an xterm-class
        // terminal, so xterm-256color (universally installed) is the right id.
        let mut extra_env = extra_env;
        if !extra_env.iter().any(|(k, _)| k == "TERM") {
            extra_env.push(("TERM".to_string(), "xterm-256color".to_string()));
        }

        let pty = Pty::spawn(&argv_ref, cwd.as_deref(), &extra_env, cols, rows)
            .expect("libghostty pty spawn");

        let pid = Rc::new(Cell::new(Some(pty.child_pid())));

        let state = Rc::new(RefCell::new(State {
            vt,
            pty,
            font,
            font_scale: 1.0,
            cell_w,
            cell_h,
            ascent,
            cols,
            rows,
            cursor_color: None,
            selection_bg: None,
            selection_fg: None,
            has_sel: false,
            preedit: String::new(),
            pointer: (0.0, 0.0),
            selecting: false,
            pending_app_press: None,
            blink_enabled: true,
            blink_phase_on: true,
            focused: false,
            anchor: (0, 0),
            last_title: String::new(),
            last_cwd: None,
            grid_scratch: Vec::new(),
        }));

        let area = gtk::DrawingArea::new();
        area.set_hexpand(true);
        area.set_vexpand(true);
        area.set_focusable(true);
        area.set_can_focus(true);

        let container = gtk::Overlay::new();
        container.set_hexpand(true);
        container.set_vexpand(true);
        container.set_child(Some(&area));

        // Vertical scrollbar pinned to the right edge, like the VTE pane. Hidden
        // until there is scrollback to show.
        let adj = gtk::Adjustment::new(0.0, 0.0, 1.0, 1.0, 1.0, 1.0);
        let scrollbar = gtk::Scrollbar::new(gtk::Orientation::Vertical, Some(&adj));
        scrollbar.set_halign(gtk::Align::End);
        scrollbar.set_valign(gtk::Align::Fill);
        scrollbar.set_can_focus(false);
        scrollbar.set_width_request(12);
        scrollbar.set_visible(false);
        container.add_overlay(&scrollbar);

        let im = gtk::IMMulticontext::new();
        let pending_after_preedit = Rc::new(RefCell::new(None));

        let pane = GhosttyPane {
            id,
            surface,
            container,
            area: area.clone(),
            state: state.clone(),
            pid: pid.clone(),
            im,
            pending_after_preedit,
            scrollbar,
            adj,
            syncing: Rc::new(Cell::new(false)),
            blink_source: Rc::new(RefCell::new(None)),
            blink_interval_ms: Rc::new(Cell::new(
                flowmux_config::options::CURSOR_BLINK_INTERVAL_DEFAULT,
            )),
            last_polled_cwd: Rc::new(RefCell::new(None)),
        };

        pane.install_draw();
        pane.install_resize();
        pane.install_pty_pump(callbacks.clone());
        pane.install_input();
        pane.install_mouse(callbacks.clone());
        pane.install_focus(callbacks);
        pane.install_scrollbar();
        pane.restart_cursor_blink();

        pane
    }

    fn install_draw(&self) {
        let state = self.state.clone();
        self.area.set_draw_func(move |_area, cr, w, h| {
            draw(&mut state.borrow_mut(), cr, w, h);
        });
    }

    fn install_resize(&self) {
        let state = self.state.clone();
        let area = self.area.clone();
        let adj = self.adj.clone();
        let scrollbar = self.scrollbar.clone();
        let syncing = self.syncing.clone();
        self.area.connect_resize(move |_area, w, h| {
            let mut s = state.borrow_mut();
            if s.cell_w <= 0.0 || s.cell_h <= 0.0 {
                return;
            }
            let cols = cols_for_width(w, s.cell_w);
            let rows = ((h as f64 / s.cell_h).floor() as i64).clamp(1, u16::MAX as i64) as u16;
            if (cols, rows) != (s.cols, s.rows) {
                s.cols = cols;
                s.rows = rows;
                let cw = s.cell_w as u16;
                let chh = s.cell_h as u16;
                s.vt.resize(cols, rows, cw as u32, chh as u32);
                let _ = s.pty.resize(cols, rows, cw, chh);
                drop(s);
                area.queue_draw();
                sync_scrollbar_adj(&state, &adj, &scrollbar, &syncing);
            }
        });
    }

    fn install_pty_pump(&self, callbacks: PaneCallbacks) {
        let state = self.state.clone();
        let area = self.area.clone();
        let pid = self.pid.clone();
        let id = self.id;
        let surface = self.surface;
        let adj = self.adj.clone();
        let scrollbar = self.scrollbar.clone();
        let syncing = self.syncing.clone();
        let fd = self.state.borrow().pty.master_fd();
        glib::source::unix_fd_add_local(fd, glib::IOCondition::IN, move |_fd, _cond| {
            let mut buf = [0u8; 16384];
            let mut s = state.borrow_mut();
            // OSC title / cwd changes to forward after we drop the borrow (the
            // VTE path tracked these via connect_title_notify /
            // current-directory-uri; libghostty has no signal so we poll the vt
            // after each read).
            let mut title_change: Option<String> = None;
            let mut cwd_change: Option<PathBuf> = None;
            match s.pty.read(&mut buf) {
                Ok(0) => {
                    // Child exited: reap to learn the status and notify.
                    let status = s.pty.try_wait().ok().flatten().unwrap_or(0);
                    drop(s);
                    pid.set(None);
                    (callbacks.on_child_exited.borrow_mut())(id, status);
                    return glib::ControlFlow::Break;
                }
                Ok(n) => {
                    s.vt.write(&buf[..n]);
                    if let Some(title) = s.vt.title() {
                        if !title.is_empty() && title != s.last_title {
                            s.last_title = title.clone();
                            title_change = Some(title);
                        }
                    }
                    // OSC 7 working-directory announcement. Only forward on a
                    // real change so the controller's title/VCS refresh runs
                    // once per `cd`, matching the VTE path.
                    if let Some(cwd) = s.vt.pwd().as_deref().and_then(pwd_to_path) {
                        if s.last_cwd.as_deref() != Some(cwd.as_path()) {
                            s.last_cwd = Some(cwd.clone());
                            cwd_change = Some(cwd);
                        }
                    }
                }
                Err(_) => return glib::ControlFlow::Break,
            }
            drop(s);
            if let Some(title) = title_change {
                (callbacks.on_terminal_title_changed.borrow_mut())(id, surface, title);
            }
            if let Some(cwd) = cwd_change {
                (callbacks.on_terminal_cwd_changed.borrow_mut())(id, surface, cwd);
            }
            area.queue_draw();
            sync_scrollbar_adj(&state, &adj, &scrollbar, &syncing);
            glib::ControlFlow::Continue
        });
    }

    fn install_input(&self) {
        let key = gtk::EventControllerKey::new();
        let im = self.im.clone();
        let pending_after_preedit = self.pending_after_preedit.clone();

        // IME: route committed text (e.g. composed Hangul syllables) to the PTY
        // and show the in-progress preedit inline at the cursor. Setting the IM
        // context makes the controller filter text keys through the IME first,
        // so key-pressed below only fires for keys the IME did not consume.
        key.set_im_context(Some(&im));

        // Non-text keys (control combos, navigation, function, Enter/Tab/…) are
        // encoded by libghostty honoring the terminal's modes (application
        // cursor keys, keypad, Kitty keyboard, Alt-as-ESC) — what vim/claude/
        // codex rely on. Plain text arrives via the IM commit above instead.
        {
            let state = self.state.clone();
            let area = self.area.clone();
            let im_for_key = im.clone();
            let adj = self.adj.clone();
            let scrollbar = self.scrollbar.clone();
            let syncing = self.syncing.clone();
            let pending_after_preedit = pending_after_preedit.clone();
            key.connect_key_pressed(move |_kc, keyval, _code, gtk_mods| {
                if is_modifier_only_key(keyval) {
                    return glib::Propagation::Proceed;
                }

                let mods = mods_from_state(gtk_mods);

                if is_insert_newline_key(keyval, mods) {
                    if queue_after_preedit_if_needed(
                        &state,
                        &pending_after_preedit,
                        &im_for_key,
                        INSERT_NEWLINE_BYTES,
                    ) {
                        return glib::Propagation::Stop;
                    }
                    let mut s = state.borrow_mut();
                    let _ = s.pty.write(INSERT_NEWLINE_BYTES);
                    s.vt.scroll_to_bottom();
                    drop(s);
                    area.queue_draw();
                    sync_scrollbar_adj(&state, &adj, &scrollbar, &syncing);
                    return glib::Propagation::Stop;
                }

                // Smart paging: PgUp/PgDn page through local scrollback when
                // there is some (Shift always pages), matching the VTE path;
                // otherwise the key falls through to the app (e.g. less/vim in
                // the alternate screen, which has no scrollback).
                if let Some(dir) = page_dir(keyval) {
                    if (mods & (flowmux_terminal::vt::MOD_CTRL | flowmux_terminal::vt::MOD_ALT))
                        == 0
                    {
                        let shift = mods & flowmux_terminal::vt::MOD_SHIFT != 0;
                        let mut s = state.borrow_mut();
                        let geom = s.vt.scrollbar();
                        let has_scrollback = geom.map(|(t, _, l)| t > l).unwrap_or(false);
                        if shift || has_scrollback {
                            let page = geom.map(|(_, _, l)| l.max(1)).unwrap_or(1) as isize;
                            s.vt.scroll(dir * page);
                            drop(s);
                            area.queue_draw();
                            sync_scrollbar_adj(&state, &adj, &scrollbar, &syncing);
                            return glib::Propagation::Stop;
                        }
                    }
                }

                let (named, cp) = map_keyval(keyval);
                let mut s = state.borrow_mut();
                match s.vt.encode_key(named, cp, mods, false) {
                    Some(bytes) => {
                        drop(s);
                        if queue_after_preedit_if_needed(
                            &state,
                            &pending_after_preedit,
                            &im_for_key,
                            &bytes,
                        ) {
                            return glib::Propagation::Stop;
                        }
                        let mut s = state.borrow_mut();
                        let _ = s.pty.write(&bytes);
                        // Snap the viewport to the live row on input, so typing
                        // while scrolled up brings the view back (matches VTE's
                        // scroll-on-keystroke; output never snaps).
                        s.vt.scroll_to_bottom();
                        drop(s);
                        area.queue_draw();
                        sync_scrollbar_adj(&state, &adj, &scrollbar, &syncing);
                        glib::Propagation::Stop
                    }
                    None => glib::Propagation::Proceed,
                }
            });
        }
        {
            let state = self.state.clone();
            let area = self.area.clone();
            let adj = self.adj.clone();
            let scrollbar = self.scrollbar.clone();
            let syncing = self.syncing.clone();
            let pending_after_preedit = pending_after_preedit.clone();
            im.connect_commit(move |_im, text| {
                let mut s = state.borrow_mut();
                s.preedit.clear();
                let bytes = commit_bytes_with_pending(text, &pending_after_preedit);
                let _ = s.pty.write(&bytes);
                s.vt.scroll_to_bottom();
                drop(s);
                area.queue_draw();
                sync_scrollbar_adj(&state, &adj, &scrollbar, &syncing);
            });
        }
        {
            // Inline preedit (composing Hangul/CJK) — store the string so the
            // renderer can draw it underlined at the cursor, matching VTE.
            let state = self.state.clone();
            let area = self.area.clone();
            im.connect_preedit_changed(move |im| {
                let (text, _attrs, _cursor) = im.preedit_string();
                state.borrow_mut().preedit = text.to_string();
                set_im_cursor_location(im, &state.borrow());
                area.queue_draw();
            });
        }
        {
            let state = self.state.clone();
            let area = self.area.clone();
            im.connect_preedit_end(move |_im| {
                state.borrow_mut().preedit.clear();
                area.queue_draw();
            });
        }
        // Keep the IME focused so composition works as soon as the pane has
        // keyboard focus, and place the candidate window near the cursor.
        let im_for_focus = im.clone();
        let state_for_loc = self.state.clone();
        let focus = gtk::EventControllerFocus::new();
        focus.connect_enter(move |_| {
            im_for_focus.focus_in();
            set_im_cursor_location(&im_for_focus, &state_for_loc.borrow());
        });
        let im_for_focus_out = im;
        let focus_out = gtk::EventControllerFocus::new();
        focus_out.connect_leave(move |_| im_for_focus_out.focus_out());

        self.area.add_controller(key);
        self.area.add_controller(focus);
        self.area.add_controller(focus_out);
    }

    /// Mouse handling: drag selection, mouse reporting to apps that request it
    /// (modes 1000/1002/1003), wheel scrollback, and Ctrl-click URL opening.
    fn install_mouse(&self, callbacks: PaneCallbacks) {
        // Pointer motion: extend an active selection, or report motion to the
        // app; always remember the position for wheel reports.
        let motion = gtk::EventControllerMotion::new();
        {
            let state = self.state.clone();
            let area = self.area.clone();
            motion.connect_motion(move |_c, x, y| {
                let mut s = state.borrow_mut();
                s.pointer = (x, y);
                let (cols, rows) = s.vt.dims().unwrap_or((s.cols, s.rows));
                let end = px_to_cell(x, y, s.cell_w, s.cell_h, cols, rows);
                if s.selecting {
                    let anchor = s.anchor;
                    if anchor == end {
                        // Still on the anchor cell — a click/jitter, not a drag.
                        // Don't paint a stray one-cell block; clear instead.
                        s.vt.clear_selection();
                        s.has_sel = false;
                    } else if s.vt.set_selection(anchor, end, false) {
                        s.has_sel = true;
                    }
                    drop(s);
                    area.queue_draw();
                } else if s.pending_app_press.is_some() {
                    // A deferred primary press (app mouse reporting on) that has
                    // now moved off its anchor cell → treat as a drag and start
                    // a local text selection instead of forwarding to the app.
                    if end != s.anchor {
                        s.pending_app_press = None;
                        s.vt.clear_selection();
                        s.has_sel = false;
                        let anchor = s.anchor;
                        if s.vt.set_selection(anchor, end, false) {
                            s.selecting = true;
                            s.has_sel = anchor != end;
                        }
                        drop(s);
                        area.queue_draw();
                    }
                } else if s.vt.mouse_enabled() {
                    if let Some(bytes) =
                        s.vt.encode_mouse(MouseAction::Motion, MouseButton::None, x, y, 0)
                    {
                        let _ = s.pty.write(&bytes);
                    }
                }
            });
        }
        self.area.add_controller(motion);

        // Press/release: start/extend selection, report to the app, or open a
        // Ctrl-clicked URL.
        let click = gtk::GestureClick::new();
        click.set_button(0); // listen to every button
        {
            let state = self.state.clone();
            let area = self.area.clone();
            let on_open_url = callbacks.on_open_url.clone();
            let on_focus = callbacks.on_focus.clone();
            let id = self.id;
            // Clones for the right-click context menu (cheap: refcounted handles).
            let pane_for_menu = self.clone();
            let cb_for_menu = callbacks.clone();
            click.connect_pressed(move |g, _n, x, y| {
                area.grab_focus();
                (on_focus.borrow_mut())(id);

                let button = g.current_button();
                let mods = mods_from_state(g.current_event_state());
                let mut s = state.borrow_mut();
                s.pointer = (x, y);
                s.pending_app_press = None;

                // Ctrl + left click → open the URL under the pointer.
                if button == gdk::BUTTON_PRIMARY && (mods & flowmux_terminal::vt::MOD_CTRL) != 0 {
                    let (cols, rows) = s.vt.dims().unwrap_or((s.cols, s.rows));
                    let (col, row) = px_to_cell(x, y, s.cell_w, s.cell_h, cols, rows);
                    s.vt.update();
                    let line = s.vt.row_text(row);
                    drop(s);
                    if let Some(url) = find_url_at(&line, col as usize) {
                        (on_open_url.borrow_mut())(id, url);
                    }
                    return;
                }

                // Right click always opens the terminal-body context menu,
                // even when the app has mouse reporting on — the menu (Copy /
                // Paste / …) is more useful there than forwarding the click.
                let shift = (mods & flowmux_terminal::vt::MOD_SHIFT) != 0;
                if button == gdk::BUTTON_SECONDARY {
                    drop(s);
                    pane_for_menu.show_context_menu(x, y, &cb_for_menu);
                    g.set_state(gtk::EventSequenceState::Claimed);
                    return;
                }

                if s.vt.mouse_enabled() && !shift {
                    // App mouse reporting is on. A primary press is ambiguous:
                    // a click should reach the app, but a drag should select
                    // text locally. Defer it — motion resolves to a selection,
                    // release-without-motion forwards the click (see those
                    // handlers). Non-primary buttons report immediately.
                    if button == gdk::BUTTON_PRIMARY {
                        // Clear any existing selection now: a press that turns
                        // into a click should deselect (and a drag will set a
                        // fresh selection from this anchor in motion).
                        if s.has_sel {
                            s.vt.clear_selection();
                            s.has_sel = false;
                        }
                        let (cols, rows) = s.vt.dims().unwrap_or((s.cols, s.rows));
                        s.anchor = px_to_cell(x, y, s.cell_w, s.cell_h, cols, rows);
                        s.pending_app_press = Some((x, y, mods));
                        drop(s);
                        area.queue_draw();
                        return;
                    }
                    let gb = ghostty_button(button);
                    if let Some(bytes) = s.vt.encode_mouse(MouseAction::Press, gb, x, y, mods) {
                        let _ = s.pty.write(&bytes);
                    }
                    return;
                }

                // Mouse reporting off (or Shift held): begin a selection on the
                // primary button immediately.
                if button == gdk::BUTTON_PRIMARY {
                    s.vt.clear_selection();
                    s.has_sel = false;
                    let (cols, rows) = s.vt.dims().unwrap_or((s.cols, s.rows));
                    s.anchor = px_to_cell(x, y, s.cell_w, s.cell_h, cols, rows);
                    s.selecting = true;
                    drop(s);
                    area.queue_draw();
                }
            });
        }
        {
            let state = self.state.clone();
            click.connect_released(move |g, _n, x, y| {
                let button = g.current_button();
                let mods = mods_from_state(g.current_event_state());
                let mut s = state.borrow_mut();
                let shift = (mods & flowmux_terminal::vt::MOD_SHIFT) != 0;
                if s.selecting {
                    s.selecting = false;
                } else if let Some((px, py, pmods)) = s.pending_app_press.take() {
                    // Deferred primary press never moved off its anchor cell →
                    // it was a click, not a drag. Forward press+release to the
                    // app now so single clicks still reach it.
                    let gb = ghostty_button(gdk::BUTTON_PRIMARY);
                    if let Some(bytes) = s.vt.encode_mouse(MouseAction::Press, gb, px, py, pmods) {
                        let _ = s.pty.write(&bytes);
                    }
                    if let Some(bytes) = s.vt.encode_mouse(MouseAction::Release, gb, x, y, mods) {
                        let _ = s.pty.write(&bytes);
                    }
                } else if s.vt.mouse_enabled() && !shift {
                    let gb = ghostty_button(button);
                    if let Some(bytes) = s.vt.encode_mouse(MouseAction::Release, gb, x, y, mods) {
                        let _ = s.pty.write(&bytes);
                    }
                }
            });
        }
        self.area.add_controller(click);

        // Wheel: report to the app when mouse tracking is on, else scroll
        // through scrollback.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        {
            let state = self.state.clone();
            let area = self.area.clone();
            let adj = self.adj.clone();
            let scrollbar = self.scrollbar.clone();
            let syncing = self.syncing.clone();
            scroll.connect_scroll(move |_c, _dx, dy| {
                let mut s = state.borrow_mut();
                if s.vt.mouse_enabled() {
                    let (px, py) = s.pointer;
                    let btn = if dy < 0.0 {
                        MouseButton::WheelUp
                    } else {
                        MouseButton::WheelDown
                    };
                    if let Some(bytes) = s.vt.encode_mouse(MouseAction::Press, btn, px, py, 0) {
                        let _ = s.pty.write(&bytes);
                    }
                } else {
                    // Up (dy < 0) scrolls into history; ~3 lines per notch.
                    s.vt.scroll((dy * 3.0).round() as isize);
                }
                drop(s);
                area.queue_draw();
                sync_scrollbar_adj(&state, &adj, &scrollbar, &syncing);
                glib::Propagation::Stop
            });
        }
        self.area.add_controller(scroll);
    }

    fn install_focus(&self, callbacks: PaneCallbacks) {
        let focus = gtk::EventControllerFocus::new();
        let id = self.id;
        // Blinking only runs on the focused pane: enter starts the timer (phase
        // reset to visible), leave stops it and leaves a steady cursor.
        let pane_enter = self.clone();
        focus.connect_enter(move |_| {
            (callbacks.on_focus.borrow_mut())(id);
            pane_enter.state.borrow_mut().focused = true;
            pane_enter.restart_cursor_blink();
        });
        let pane_leave = self.clone();
        focus.connect_leave(move |_| {
            pane_leave.state.borrow_mut().focused = false;
            pane_leave.restart_cursor_blink();
        });
        self.area.add_controller(focus);
    }

    /// Tear down any running blink timer and, if blinking is enabled and this
    /// pane is focused, install a fresh `glib` timeout at the current interval.
    /// The phase is reset to visible so the cursor never disappears at the
    /// moment the setting or focus changes.
    fn restart_cursor_blink(&self) {
        if let Some(src) = self.blink_source.borrow_mut().take() {
            src.remove();
        }
        {
            let mut s = self.state.borrow_mut();
            s.blink_phase_on = true;
        }
        self.area.queue_draw();

        let (enabled, focused) = {
            let s = self.state.borrow();
            (s.blink_enabled, s.focused)
        };
        if !enabled || !focused {
            return;
        }
        let interval = self.blink_interval_ms.get().max(1) as u64;
        let state = self.state.clone();
        let area = self.area.clone();
        let src = glib::timeout_add_local(std::time::Duration::from_millis(interval), move || {
            {
                let mut s = state.borrow_mut();
                s.blink_phase_on = !s.blink_phase_on;
            }
            area.queue_draw();
            glib::ControlFlow::Continue
        });
        *self.blink_source.borrow_mut() = Some(src);
    }

    /// Apply the cursor-blink option (enabled + half-period in ms). Clamps the
    /// interval and restarts the timer so the change takes effect live.
    pub fn set_cursor_blink(&self, enabled: bool, interval_ms: u32) {
        self.state.borrow_mut().blink_enabled = enabled;
        self.blink_interval_ms
            .set(flowmux_config::options::Options::clamp_cursor_blink_interval(interval_ms));
        self.restart_cursor_blink();
    }

    // ---- Method surface used by the pane registry (see pane_terminal.rs) ----

    /// The owning leaf pane id (also available as the `id` field; this method
    /// mirrors the call sites that used the former PaneTerminal enum).
    pub fn id(&self) -> PaneId {
        self.id
    }

    pub fn root_widget(&self) -> gtk::Widget {
        self.container.clone().upcast::<gtk::Widget>()
    }

    pub fn grab_focus(&self) {
        self.area.grab_focus();
    }

    /// Current working directory: prefer the shell's OSC 7 announcement (exact),
    /// falling back to the `/proc/<pid>/cwd` symlink.
    pub fn current_dir(&self) -> Option<PathBuf> {
        if let Some(pwd) = self.state.borrow().vt.pwd() {
            if let Some(p) = pwd_to_path(&pwd) {
                return Some(p);
            }
        }
        let pid = self.pid.get()?;
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    /// Poll variant of [`current_dir`]: returns the cwd only when it changed
    /// since the previous poll. Unchanged panes return `None`, so the 1-second
    /// fallback never takes the daemon state mutex for an idle terminal — the
    /// per-tick cost stays a single `/proc` readlink, not a global lock + tree
    /// walk that scales with workspace/tab count.
    pub fn poll_cwd_if_changed(&self) -> Option<PathBuf> {
        let cwd = self.current_dir()?;
        let mut last = self.last_polled_cwd.borrow_mut();
        if last.as_ref() == Some(&cwd) {
            return None;
        }
        *last = Some(cwd.clone());
        Some(cwd)
    }

    pub fn set_font_scale(&self, scale: f64) {
        let mut s = self.state.borrow_mut();
        s.font_scale = if scale > 0.0 { scale } else { 1.0 };
        s.remeasure();
        // Re-fit the grid to the new cell size on the next allocation.
        let (w, h) = (self.area.width(), self.area.height());
        if w > 0 && h > 0 && s.cell_w > 0.0 && s.cell_h > 0.0 {
            let cols = cols_for_width(w, s.cell_w);
            let rows = ((h as f64 / s.cell_h).floor() as i64).clamp(1, u16::MAX as i64) as u16;
            s.cols = cols;
            s.rows = rows;
            let cw = s.cell_w as u16;
            let chh = s.cell_h as u16;
            s.vt.resize(cols, rows, cw as u32, chh as u32);
            let _ = s.pty.resize(cols, rows, cw, chh);
        }
        drop(s);
        self.area.queue_draw();
    }

    pub fn set_font(&self, desc: &pango::FontDescription) {
        // Read the scale out and release the borrow before calling
        // set_font_scale (which borrows again) to avoid a RefCell double-borrow.
        let scale = {
            let mut s = self.state.borrow_mut();
            s.font = desc.clone();
            s.font_scale
        };
        self.set_font_scale(scale);
    }

    /// Apply the host theme colors. Default fg/bg/palette are pushed into
    /// libghostty so resolved cell colors match VTE; cursor and selection
    /// colors are kept for host-side drawing. Mirrors
    /// `theme.apply_to_terminal` for the VTE path.
    pub fn apply_colors(
        &self,
        fg: Rgb,
        bg: Rgb,
        cursor: Rgb,
        palette: &[Rgb],
        selection_bg: Option<Rgb>,
        selection_fg: Option<Rgb>,
    ) {
        {
            let mut s = self.state.borrow_mut();
            s.vt.set_default_colors(fg, bg, cursor);
            s.vt.set_palette(palette);
            s.cursor_color = Some(cursor);
            s.selection_bg = selection_bg;
            s.selection_fg = selection_fg;
        }
        self.area.queue_draw();
    }

    pub fn has_selection(&self) -> bool {
        self.state.borrow().has_sel
    }

    /// Copy the live selection to the clipboard. Returns `true` only when
    /// non-empty text was actually placed on the clipboard, so callers can
    /// gate a "copied" toast on a real copy rather than the cached `has_sel`
    /// flag — which drifts out of sync when output scrolls the viewport and
    /// libghostty silently drops its viewport-anchored selection.
    pub fn copy_selection_to_clipboard(&self) -> bool {
        let text = {
            let mut s = self.state.borrow_mut();
            if !s.has_sel {
                return false;
            }
            // Refresh the snapshot so selection_text reads current `selected`
            // flags, then extract the selected run.
            s.vt.update();
            // The viewport-anchored selection may have been dropped by
            // libghostty (e.g. new output scrolled it away); reflect that in
            // our cached flag so a stale selection can't lie to callers.
            let text = s.vt.selection_text().filter(|t| !t.is_empty());
            if text.is_none() {
                s.has_sel = false;
            }
            text
        };
        let Some(text) = text else { return false };
        let Some(display) = gdk::Display::default() else {
            return false;
        };
        display.clipboard().set_text(&text);
        true
    }

    pub fn paste_clipboard(&self) {
        let state = self.state.clone();
        let area = self.area.clone();
        let adj = self.adj.clone();
        let scrollbar = self.scrollbar.clone();
        let syncing = self.syncing.clone();
        if let Some(display) = gdk::Display::default() {
            let clipboard = display.clipboard();
            clipboard.read_text_async(gtk::gio::Cancellable::NONE, move |res| {
                if let Ok(Some(text)) = res {
                    let mut s = state.borrow_mut();
                    let _ = s.pty.write(text.as_bytes());
                    s.vt.scroll_to_bottom();
                    drop(s);
                    area.queue_draw();
                    sync_scrollbar_adj(&state, &adj, &scrollbar, &syncing);
                }
            });
        }
    }

    /// Terminal-body right-click context menu: Copy / Paste / Split Right /
    /// Split Down / Copy path / Close Pane. Ports the popover the VTE pane used
    /// (a plain `Popover` of flat `Button`s, not PopoverMenu + `win.*` actions,
    /// whose action lookup chain dropped through in some GTK versions). Copy is
    /// a no-op without a selection so it never clobbers the clipboard.
    fn show_context_menu(&self, x: f64, y: f64, cb: &PaneCallbacks) {
        let id = self.id;
        let surface = self.surface;
        // Focus the pane first, mirroring the VTE menu, so the action targets it.
        (cb.on_focus.borrow_mut())(id);

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

        let copy = mk("Copy");
        let pop = popover.clone();
        let pane = self.clone();
        copy.connect_clicked(move |_| {
            pop.popdown();
            pane.copy_selection_to_clipboard();
        });
        v.append(&copy);

        let paste = mk("Paste");
        let pop = popover.clone();
        let pane = self.clone();
        paste.connect_clicked(move |_| {
            pop.popdown();
            pane.paste_clipboard();
        });
        v.append(&paste);

        v.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let split_r = mk("Split Right");
        let pop = popover.clone();
        let h = cb.on_split_right.clone();
        split_r.connect_clicked(move |_| {
            pop.popdown();
            (h.borrow_mut())(id);
        });
        v.append(&split_r);

        let split_d = mk("Split Down");
        let pop = popover.clone();
        let h = cb.on_split_down.clone();
        split_d.connect_clicked(move |_| {
            pop.popdown();
            (h.borrow_mut())(id);
        });
        v.append(&split_d);

        v.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let copy_path = mk("Copy path");
        let pop = popover.clone();
        let h = cb.on_copy_surface_text.clone();
        copy_path.connect_clicked(move |_| {
            pop.popdown();
            (h.borrow_mut())(id, surface);
        });
        v.append(&copy_path);

        v.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let close_p = mk("Close Pane");
        let pop = popover.clone();
        let h = cb.on_close_pane.clone();
        close_p.connect_clicked(move |_| {
            pop.popdown();
            (h.borrow_mut())(id);
        });
        v.append(&close_p);

        popover.set_child(Some(&v));
        popover.set_parent(&self.area);
        popover.set_has_arrow(false);
        crate::ui::popover_pos::anchor_at_click(&popover, &self.area, x, y);
        popover.connect_closed(|p| p.unparent());
        popover.popup();
    }

    /// Send raw bytes to the child PTY (`flowmux send-keys` / `send-key`).
    pub fn write_input(&self, mut bytes: &[u8]) -> io::Result<()> {
        let mut s = self.state.borrow_mut();
        while !bytes.is_empty() {
            let n = s.pty.write(bytes)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "pty write returned 0",
                ));
            }
            bytes = &bytes[n..];
        }
        s.vt.scroll_to_bottom();
        drop(s);
        self.area.queue_draw();
        sync_scrollbar_adj(&self.state, &self.adj, &self.scrollbar, &self.syncing);
        Ok(())
    }

    pub fn feed_after_preedit_commit(&self, bytes: &'static [u8]) {
        if queue_after_preedit_if_needed(&self.state, &self.pending_after_preedit, &self.im, bytes)
        {
            return;
        }
        let _ = self.write_input(bytes);
    }

    /// Visible screen text (all viewport rows joined), for `read-screen`.
    pub fn screen_text(&self) -> Option<String> {
        let mut s = self.state.borrow_mut();
        s.vt.update();
        let (_, rows) = s.vt.dims().unwrap_or((s.cols, s.rows));
        let mut out = String::new();
        for row in 0..rows {
            out.push_str(&s.vt.row_text(row));
            out.push('\n');
        }
        Some(out)
    }

    pub fn add_controller(&self, controller: impl IsA<gtk::EventController>) {
        self.container.add_controller(controller);
    }

    /// Test-only access to the render widget, for the split-tree identity
    /// assertions in `window.rs` (a pane must keep the same widget across
    /// rebuilds so its PTY child survives).
    #[cfg(test)]
    pub fn render_area(&self) -> gtk::DrawingArea {
        self.area.clone()
    }

    /// Drive the overlaid scrollbar from the terminal's own scroll position.
    fn install_scrollbar(&self) {
        let state = self.state.clone();
        let area = self.area.clone();
        let scrollbar = self.scrollbar.clone();
        let syncing = self.syncing.clone();
        self.adj.connect_value_changed(move |adj| {
            if syncing.get() {
                return; // programmatic update from sync_scrollbar_adj
            }
            let target = adj.value().round() as i64;
            {
                let mut s = state.borrow_mut();
                if let Some((_total, offset, _len)) = s.vt.scrollbar() {
                    let delta = target - offset as i64;
                    if delta != 0 {
                        s.vt.scroll(delta as isize);
                    }
                }
            }
            area.queue_draw();
            sync_scrollbar_adj(&state, &adj, &scrollbar, &syncing);
        });
    }
}

/// Push the terminal's scroll geometry into the overlaid scrollbar's
/// adjustment, showing it only when there is scrollback. `syncing` guards the
/// value-changed handler from treating this programmatic update as user input.
fn sync_scrollbar_adj(
    state: &Rc<RefCell<State>>,
    adj: &gtk::Adjustment,
    scrollbar: &gtk::Scrollbar,
    syncing: &Rc<Cell<bool>>,
) {
    let geom = state.borrow().vt.scrollbar();
    let Some((total, offset, len)) = geom else {
        return;
    };
    if total > len && len > 0 {
        syncing.set(true);
        adj.set_lower(0.0);
        adj.set_upper(total as f64);
        adj.set_page_size(len as f64);
        adj.set_step_increment(1.0);
        adj.set_page_increment(len as f64);
        adj.set_value(offset.min(total - len) as f64);
        syncing.set(false);
        if !scrollbar.is_visible() {
            scrollbar.set_visible(true);
        }
    } else if scrollbar.is_visible() {
        scrollbar.set_visible(false);
    }
}

fn rgb(c: Rgb) -> (f64, f64, f64) {
    (c.r as f64 / 255.0, c.g as f64 / 255.0, c.b as f64 / 255.0)
}

/// Map a surface pixel position to a viewport cell, clamped to the grid.
fn px_to_cell(x: f64, y: f64, cell_w: f64, cell_h: f64, cols: u16, rows: u16) -> (u16, u16) {
    let col = if cell_w > 0.0 {
        (x / cell_w).floor().max(0.0)
    } else {
        0.0
    } as u32;
    let row = if cell_h > 0.0 {
        (y / cell_h).floor().max(0.0)
    } else {
        0.0
    } as u32;
    (
        col.min(cols.saturating_sub(1) as u32) as u16,
        row.min(rows.saturating_sub(1) as u32) as u16,
    )
}

/// Convert an OSC 7 working-directory value to a path. Accepts either a bare
/// path or a `file://host/path` URI (the host segment is dropped — flowmux is
/// local).
fn pwd_to_path(pwd: &str) -> Option<PathBuf> {
    if pwd.is_empty() {
        return None;
    }
    if let Some(rest) = pwd.strip_prefix("file://") {
        let path = match rest.find('/') {
            Some(idx) => &rest[idx..],
            None => rest,
        };
        return Some(PathBuf::from(path));
    }
    Some(PathBuf::from(pwd))
}

/// GTK modifier state → shim mouse-modifier bits.
fn mods_from_state(state: gdk::ModifierType) -> u8 {
    let mut m = 0;
    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        m |= flowmux_terminal::vt::MOD_SHIFT;
    }
    if state.contains(gdk::ModifierType::CONTROL_MASK) {
        m |= flowmux_terminal::vt::MOD_CTRL;
    }
    if state.contains(gdk::ModifierType::ALT_MASK) {
        m |= flowmux_terminal::vt::MOD_ALT;
    }
    m
}

/// Map a GTK button number to a libghostty mouse button.
fn ghostty_button(button: u32) -> MouseButton {
    if button == gdk::BUTTON_MIDDLE {
        MouseButton::Middle
    } else if button == gdk::BUTTON_SECONDARY {
        MouseButton::Right
    } else {
        MouseButton::Left
    }
}

/// Find a URL covering column `col` in a row's text. Columns are treated as
/// char indices, which is exact for the ASCII URLs this targets. Trailing
/// sentence punctuation is trimmed; bare `www.` hosts get an `https://` scheme.
fn find_url_at(line: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    if col >= chars.len() || chars[col].is_whitespace() {
        return None;
    }
    let mut start = col;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let mut end = col;
    while end < chars.len() && !chars[end].is_whitespace() {
        end += 1;
    }
    let token: String = chars[start..end].iter().collect();
    let trimmed = token
        .trim_start_matches(|c: char| "([{<\"'".contains(c))
        .trim_end_matches(|c: char| ".,;:!?)]}'\"".contains(c));
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Some(trimmed.to_string())
    } else if trimmed.starts_with("www.") {
        Some(format!("https://{trimmed}"))
    } else {
        None
    }
}

fn is_modifier_only_key(keyval: gdk::Key) -> bool {
    use gdk::Key;
    matches!(
        keyval,
        Key::Shift_L
            | Key::Shift_R
            | Key::Shift_Lock
            | Key::Control_L
            | Key::Control_R
            | Key::Alt_L
            | Key::Alt_R
            | Key::Meta_L
            | Key::Meta_R
            | Key::Super_L
            | Key::Super_R
            | Key::Hyper_L
            | Key::Hyper_R
            | Key::Caps_Lock
            | Key::Num_Lock
            | Key::Scroll_Lock
            | Key::ISO_Level3_Shift
            | Key::ISO_Level5_Shift
            | Key::Mode_switch
    )
}

fn is_insert_newline_key(keyval: gdk::Key, mods: u8) -> bool {
    use gdk::Key;
    mods == flowmux_terminal::vt::MOD_SHIFT
        && matches!(keyval, Key::Return | Key::KP_Enter | Key::ISO_Enter)
}

fn queue_after_preedit_if_needed(
    state: &Rc<RefCell<State>>,
    pending: &PendingInput,
    im: &gtk::IMMulticontext,
    bytes: &[u8],
) -> bool {
    if state.borrow().preedit.is_empty() {
        return false;
    }
    queue_pending_input(pending, bytes);
    im.reset();
    true
}

fn queue_pending_input(pending: &PendingInput, bytes: &[u8]) {
    let mut pending = pending.borrow_mut();
    if let Some(existing) = pending.as_mut() {
        existing.extend_from_slice(bytes);
    } else {
        *pending = Some(bytes.to_vec());
    }
}

fn commit_bytes_with_pending(text: &str, pending: &PendingInput) -> Vec<u8> {
    let mut bytes = Vec::from(text.as_bytes());
    if let Some(pending) = pending.borrow_mut().take() {
        bytes.extend_from_slice(&pending);
    }
    bytes
}

/// Tell the IME where the cursor is so the candidate window appears near the
/// composing text.
fn set_im_cursor_location(im: &gtk::IMMulticontext, state: &State) {
    if let Some(c) = state.vt.cursor() {
        let rect = gdk::Rectangle::new(
            (c.x as f64 * state.cell_w) as i32,
            (c.y as f64 * state.cell_h) as i32,
            state.cell_w.max(1.0) as i32,
            state.cell_h.max(1.0) as i32,
        );
        im.set_cursor_location(&rect);
    }
}

/// Measure monospace cell metrics (width, height, ascent) for `font`, derived
/// the same way VTE sizes a cell: `ceil(approximate_char_width)` for the column
/// width and `ceil(ascent + descent)` for the row height, so the grid layout,
/// line spacing, and top/bottom placement match the VTE path.
fn measure_cell(font: &pango::FontDescription) -> (f64, f64, f64) {
    let surf = cairo::ImageSurface::create(cairo::Format::ARgb32, 8, 8).unwrap();
    let cr = cairo::Context::new(&surf).unwrap();
    let layout = pangocairo::functions::create_layout(&cr);
    layout.set_font_description(Some(font));

    // Cell width = the monospace advance, measured precisely by laying out a
    // run of identical glyphs and dividing. `approximate_char_width` rounded up
    // is ~1px too wide for many fonts, which reads as loose letter-spacing
    // (자간); the measured advance keeps glyphs snug like VTE.
    layout.set_text("0000000000");
    let cell_w = (layout.pixel_size().0 as f64 / 10.0).round().max(1.0);

    let ctx = layout.context();
    let metrics = ctx.metrics(Some(font), None);
    let scale = pango::SCALE as f64;
    let ascent = metrics.ascent() as f64 / scale;
    let descent = metrics.descent() as f64 / scale;
    let cell_h = (ascent + descent).ceil().max(1.0);
    (cell_w, cell_h, ascent)
}

/// The fill color a cell's background pass should paint, if any: the selection
/// wash when selected, the foreground when inverse, otherwise the cell's own
/// explicit background (`None` for the default background, already cleared).
fn cell_bg_fill(cell: &VtCell, sel_bg: Rgb) -> Option<Rgb> {
    if cell.selected {
        Some(sel_bg)
    } else if cell.style.inverse {
        Some(cell.fg)
    } else {
        cell.bg
    }
}

/// The foreground color a cell's glyph/decoration is drawn in, after resolving
/// inverse video and a selection foreground override.
fn cell_fg(cell: &VtCell, colors: &Colors, sel_fg: Option<Rgb>) -> Rgb {
    let mut fg = if cell.style.inverse {
        cell.bg.unwrap_or(colors.bg)
    } else {
        cell.fg
    };
    if cell.selected {
        if let Some(sfg) = sel_fg {
            fg = sfg;
        }
    }
    fg
}

/// Whether `cell` is printable ASCII in the primary monospace font (one byte in
/// `0x20..=0x7e`, not double-width). Such cells advance by exactly one cell and
/// sit on the primary baseline, so a horizontal run of them with the same
/// foreground can be shaped as a single Pango layout — the cheap path.
fn is_ascii_run_cell(cell: &VtCell) -> bool {
    if cell.wide || cell.text.len() != 1 {
        return false;
    }
    matches!(cell.text.as_bytes()[0], 0x20..=0x7e)
}

/// Top-level draw callback: snapshot the grid and paint it straight onto `cr`
/// (the DrawingArea's own surface — drawing to an offscreen image and blitting
/// each frame was measurably slower under streaming output). The cursor and IME
/// preedit are painted on top as an overlay.
fn draw(state: &mut State, cr: &cairo::Context, w: i32, h: i32) {
    let _ = state.vt.update();
    let colors = state.vt.colors().unwrap_or(Colors {
        fg: Rgb {
            r: 220,
            g: 220,
            b: 220,
        },
        bg: Rgb { r: 0, g: 0, b: 0 },
        cursor: Rgb {
            r: 220,
            g: 220,
            b: 220,
        },
        cursor_has_value: false,
    });

    let (cols, rows) = state.vt.dims().unwrap_or((state.cols, state.rows));
    // Read into the persistent scratch buffer so a repaint doesn't allocate a
    // fresh cols*rows Vec each frame (the cells themselves reuse their text
    // buffers — see `Vt::read_grid_into`).
    state.vt.read_grid_into(cols, rows, &mut state.grid_scratch);

    paint_grid(
        cr,
        &state.scaled_font(),
        w,
        h,
        cols,
        rows,
        &state.grid_scratch,
        state.cell_w,
        state.cell_h,
        state.ascent,
        &colors,
        state.selection_bg,
        state.selection_fg,
    );

    draw_overlay(state, cr, cols, rows, &colors);
}

/// Render the grid (backgrounds + glyphs + underline/strikethrough) straight
/// into `cr`. No cursor or preedit — those are an overlay drawn separately. A
/// free function (no `&State`) so it can be driven directly by the render
/// benchmark in this module's tests.
#[allow(clippy::too_many_arguments)]
fn paint_grid(
    cr: &cairo::Context,
    font: &pango::FontDescription,
    w: i32,
    h: i32,
    cols: u16,
    rows: u16,
    grid: &[VtCell],
    cw: f64,
    ch: f64,
    ascent: f64,
    colors: &Colors,
    selection_bg: Option<Rgb>,
    selection_fg: Option<Rgb>,
) {
    let (br, bgc, bb) = rgb(colors.bg);
    cr.set_source_rgb(br, bgc, bb);
    cr.rectangle(0.0, 0.0, w as f64, h as f64);
    let _ = cr.fill();

    let layout = pangocairo::functions::create_layout(cr);
    layout.set_font_description(Some(font));
    // Baseline of the primary monospace font. It does not vary across ASCII
    // cells, so query it once here instead of per cell in the glyph pass below.
    layout.set_text("M");
    let primary_baseline = layout.baseline() as f64 / pango::SCALE as f64;

    // Selection wash comes from the theme (host-drawn to match VTE); fall back
    // to a neutral blue-grey / the default fg when unset.
    let sel_bg = selection_bg.unwrap_or(Rgb {
        r: 51,
        g: 87,
        b: 140,
    });
    let sel_fg = selection_fg;

    // Scratch reused across rows/runs so the ASCII glyph-run pass allocates
    // nothing per frame.
    let mut run = String::new();

    for row in 0..rows {
        let y = row as f64 * ch;
        // Render each row in passes (all backgrounds, then glyphs, then
        // decorations) so a wide glyph's right half is not erased by the next
        // spacer cell's background fill — the cause of "left-half-only" Hangul
        // on a colored background.
        let row_start = row as usize * cols as usize;
        let cells = &grid[row_start..row_start + cols as usize];

        // Pass 1: backgrounds + selection wash. Coalesce horizontal runs of the
        // same fill color into one rectangle to cut the cairo path-op count.
        // Wide (double-width) lead cells keep their own 2-cell rectangle.
        let mut col = 0usize;
        while col < cells.len() {
            let cell = &cells[col];
            let Some(color) = cell_bg_fill(cell, sel_bg) else {
                col += 1;
                continue;
            };
            let (r, g, bl) = rgb(color);
            cr.set_source_rgb(r, g, bl);
            if cell.wide {
                cr.rectangle(col as f64 * cw, y, cw * 2.0, ch);
                let _ = cr.fill();
                col += 1;
            } else {
                let start = col;
                let mut end = col + 1;
                while end < cells.len() {
                    let c2 = &cells[end];
                    if c2.wide || cell_bg_fill(c2, sel_bg) != Some(color) {
                        break;
                    }
                    end += 1;
                }
                cr.rectangle(start as f64 * cw, y, (end - start) as f64 * cw, ch);
                let _ = cr.fill();
                col = end;
            }
        }

        // Pass 2: glyphs. Coalesce horizontal runs of same-foreground printable
        // ASCII into one `set_text`/`show_layout`; the monospace advance makes a
        // run pixel-identical to per-cell drawing while skipping a Pango shaping
        // pass per cell. Wide/fallback glyphs are still measured and centered.
        let mut col = 0usize;
        while col < cells.len() {
            let cell = &cells[col];
            let cell_px_w = if cell.wide { cw * 2.0 } else { cw };
            let fg = cell_fg(cell, colors, sel_fg);
            let (fr, fgc, fb) = rgb(fg);

            if is_ascii_run_cell(cell) {
                run.clear();
                run.push(cell.text.as_bytes()[0] as char);
                let start = col;
                let mut end = col + 1;
                while end < cells.len() {
                    let c2 = &cells[end];
                    if !is_ascii_run_cell(c2) || cell_fg(c2, colors, sel_fg) != fg {
                        break;
                    }
                    run.push(c2.text.as_bytes()[0] as char);
                    end += 1;
                }
                layout.set_text(&run);
                cr.set_source_rgb(fr, fgc, fb);
                cr.move_to(start as f64 * cw, y + ascent - primary_baseline);
                pangocairo::functions::show_layout(cr, &layout);
                col = end;
            } else {
                if !cell.text.is_empty() {
                    let x = col as f64 * cw;
                    layout.set_text(&cell.text);
                    cr.set_source_rgb(fr, fgc, fb);
                    // Baseline-align to `y + ascent` so fallback (CJK) glyphs
                    // line up with ASCII regardless of their font's own metrics,
                    // and center the glyph in its cell box without distorting it.
                    let baseline = layout.baseline() as f64 / pango::SCALE as f64;
                    let glyph_w = layout.pixel_size().0 as f64;
                    let x_off = ((cell_px_w - glyph_w) / 2.0).max(0.0);
                    cr.move_to(x + x_off, y + ascent - baseline);
                    pangocairo::functions::show_layout(cr, &layout);
                }
                col += 1;
            }
        }

        // Pass 3: underline/strikethrough, on top of the glyphs. Skip the whole
        // traversal on rows with no decorations (the overwhelming majority,
        // including all-CJK rows) so the pass costs nothing there.
        if !cells
            .iter()
            .any(|c| c.style.underline || c.style.strikethrough)
        {
            continue;
        }
        for (col, cell) in cells.iter().enumerate() {
            if !cell.style.underline && !cell.style.strikethrough {
                continue;
            }
            let x = col as f64 * cw;
            let cell_px_w = if cell.wide { cw * 2.0 } else { cw };
            let (fr, fgc, fb) = rgb(cell_fg(cell, colors, sel_fg));
            cr.set_source_rgb(fr, fgc, fb);
            if cell.style.underline {
                cr.rectangle(x, y + ascent + 2.0, cell_px_w, 1.0);
                let _ = cr.fill();
            }
            if cell.style.strikethrough {
                cr.rectangle(x, y + ch * 0.5, cell_px_w, 1.0);
                let _ = cr.fill();
            }
        }
    }
}

/// Draw the cursor and any inline IME preedit on top of the (cached) grid.
fn draw_overlay(state: &State, cr: &cairo::Context, cols: u16, rows: u16, colors: &Colors) {
    let cw = state.cell_w;
    let ch = state.cell_h;
    let ascent = state.ascent;
    let cursor_rgb = state.cursor_color.unwrap_or(if colors.cursor_has_value {
        colors.cursor
    } else {
        colors.fg
    });
    // Hide the cursor on the "off" half of a blink, but only while this pane is
    // focused and blinking is enabled — an unfocused or steady cursor always
    // draws. Applies to the IME-preedit cursor too, so composing Hangul/CJK
    // still blinks.
    let blink_hidden = state.blink_enabled && state.focused && !state.blink_phase_on;
    let Some(cursor) = state.vt.cursor() else {
        return;
    };
    let cx = cursor.x as f64 * cw;
    let cy = cursor.y as f64 * ch;
    if !state.preedit.is_empty() {
        // Inline IME preedit (composing Hangul/CJK): paint the composing text at
        // the cursor with an underline, then the block cursor right after it —
        // matching the VTE path's inline preedit.
        let layout = pangocairo::functions::create_layout(cr);
        layout.set_font_description(Some(&state.scaled_font()));
        layout.set_text(&state.preedit);
        let pw = layout.pixel_size().0 as f64;
        let baseline = layout.baseline() as f64 / pango::SCALE as f64;
        let (br2, bg2, bb2) = rgb(colors.bg);
        cr.set_source_rgb(br2, bg2, bb2);
        cr.rectangle(cx, cy, pw, ch);
        let _ = cr.fill();
        let (fr, fgc, fb) = rgb(colors.fg);
        cr.set_source_rgb(fr, fgc, fb);
        cr.move_to(cx, cy + ascent - baseline);
        pangocairo::functions::show_layout(cr, &layout);
        cr.rectangle(cx, cy + ascent + 2.0, pw, 1.0);
        let _ = cr.fill();
        if cursor.visible && !blink_hidden {
            let (r, g, b) = rgb(cursor_rgb);
            cr.set_source_rgba(r, g, b, 0.6);
            cr.rectangle(cx + pw, cy, cw, ch);
            let _ = cr.fill();
        }
    } else if cursor.visible && !blink_hidden && cursor.x < cols && cursor.y < rows {
        let (r, g, b) = rgb(cursor_rgb);
        cr.set_source_rgba(r, g, b, 0.6);
        cr.rectangle(cx, cy, cw, ch);
        let _ = cr.fill();
    }
}

/// Page-key direction for smart paging: -1 = up (PgUp), +1 = down (PgDn).
fn page_dir(keyval: gdk::Key) -> Option<isize> {
    match keyval {
        gdk::Key::Page_Up | gdk::Key::KP_Page_Up => Some(-1),
        gdk::Key::Page_Down | gdk::Key::KP_Page_Down => Some(1),
        _ => None,
    }
}

/// Map a GTK keyval to a libghostty named-key code + unshifted codepoint.
/// Named (non-text) keys get an `nk::*` code; everything else carries its
/// Unicode scalar so the encoder can apply modifiers (Ctrl/Alt) correctly.
fn map_keyval(keyval: gdk::Key) -> (i32, u32) {
    use flowmux_terminal::vt::named_key as nk;
    use gdk::Key;
    let named = match keyval {
        Key::Return => nk::ENTER,
        Key::KP_Enter => nk::KP_ENTER,
        Key::Tab | Key::ISO_Left_Tab => nk::TAB,
        Key::BackSpace => nk::BACKSPACE,
        Key::Escape => nk::ESCAPE,
        Key::Up | Key::KP_Up => nk::UP,
        Key::Down | Key::KP_Down => nk::DOWN,
        Key::Left | Key::KP_Left => nk::LEFT,
        Key::Right | Key::KP_Right => nk::RIGHT,
        Key::Home | Key::KP_Home => nk::HOME,
        Key::End | Key::KP_End => nk::END,
        Key::Page_Up | Key::KP_Page_Up => nk::PAGE_UP,
        Key::Page_Down | Key::KP_Page_Down => nk::PAGE_DOWN,
        Key::Delete | Key::KP_Delete => nk::DELETE,
        Key::Insert | Key::KP_Insert => nk::INSERT,
        Key::F1 => nk::f(1),
        Key::F2 => nk::f(2),
        Key::F3 => nk::f(3),
        Key::F4 => nk::f(4),
        Key::F5 => nk::f(5),
        Key::F6 => nk::f(6),
        Key::F7 => nk::f(7),
        Key::F8 => nk::f(8),
        Key::F9 => nk::f(9),
        Key::F10 => nk::f(10),
        Key::F11 => nk::f(11),
        Key::F12 => nk::f(12),
        _ => nk::NONE,
    };
    if named != nk::NONE {
        (named, 0)
    } else {
        (nk::NONE, keyval.to_unicode().map(|c| c as u32).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        commit_bytes_with_pending, find_url_at, is_insert_newline_key, is_modifier_only_key,
        queue_pending_input, PendingInput, INSERT_NEWLINE_BYTES,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    use gtk::gdk;

    // ---- Render-path microbenchmark (run manually) -------------------------
    //
    //   cargo test -p flowmux --bin flowmux render_throughput_bench \
    //       -- --ignored --nocapture
    //
    // Times `paint_grid` over representative grids, comparing three strategies:
    //   direct      — paint straight onto a persistent surface (the shipped path)
    //   off_fresh   — alloc a new offscreen ImageSurface every frame + blit
    //                 (the regressed path: a per-frame multi-MB malloc)
    //   off_reused  — reuse one offscreen ImageSurface + blit every frame
    // The gap between `direct` and `off_*` is the cost the offscreen cache added.

    fn bench_grid(cols: usize, rows: usize, kind: &str) -> Vec<super::VtCell> {
        use flowmux_terminal::vt::{Cell, CellStyle, Rgb};
        let mut g = Vec::with_capacity(cols * rows);
        for i in 0..cols * rows {
            let mut cell = Cell {
                text: String::new(),
                fg: Rgb {
                    r: 200,
                    g: 200,
                    b: 200,
                },
                bg: None,
                style: CellStyle::default(),
                selected: false,
                wide: false,
            };
            match kind {
                // Dense printable ASCII (code / `cat` of a source file).
                "ascii_dense" => {
                    cell.text
                        .push((0x21 + (i * 7 + (i / cols) * 13) % 0x5e) as u8 as char);
                }
                // ASCII with ~20% spaces (prose / command output).
                "ascii_words" => {
                    let c = if (i * 31) % 10 < 2 {
                        b' '
                    } else {
                        b'a' + ((i * 7) % 26) as u8
                    };
                    cell.text.push(c as char);
                }
                // Per-cell background color (htop / colored TUIs).
                "colored" => {
                    cell.text.push((b'a' + ((i * 7) % 26) as u8) as char);
                    cell.bg = Some(Rgb {
                        r: ((i * 5) % 256) as u8,
                        g: ((i * 9) % 256) as u8,
                        b: ((i * 13) % 256) as u8,
                    });
                }
                // Wide Hangul: lead wide cell + blank spacer.
                "cjk" => {
                    if i % 2 == 0 {
                        cell.text.push('한');
                        cell.wide = true;
                    }
                }
                // Mostly blank, short prompt on the first row (idle shell).
                _ => {
                    if i < 12 {
                        cell.text.push((b'$' + (i % 3) as u8) as char);
                    }
                }
            }
            g.push(cell);
        }
        g
    }

    #[derive(Clone, Copy)]
    enum Mode {
        Direct,
        PerCell,
        OffFresh,
        OffReused,
    }

    /// The pre-0.5.5 per-cell glyph/background path (no run coalescing), used as
    /// the "before" baseline in the benchmark. Mirrors the shipped logic at
    /// 0.5.3/0.5.4: a `set_text`/`show_layout` per non-empty cell and a
    /// `rectangle`/`fill` per colored cell, with the ASCII baseline fast path.
    #[allow(clippy::too_many_arguments)]
    fn paint_grid_percell(
        cr: &gtk::cairo::Context,
        font: &gtk::pango::FontDescription,
        w: i32,
        h: i32,
        cols: u16,
        rows: u16,
        grid: &[super::VtCell],
        cw: f64,
        ch: f64,
        ascent: f64,
        colors: &flowmux_terminal::vt::Colors,
        selection_bg: Option<flowmux_terminal::vt::Rgb>,
        selection_fg: Option<flowmux_terminal::vt::Rgb>,
    ) {
        use flowmux_terminal::vt::Rgb;
        let (br, bgc, bb) = super::rgb(colors.bg);
        cr.set_source_rgb(br, bgc, bb);
        cr.rectangle(0.0, 0.0, w as f64, h as f64);
        let _ = cr.fill();
        let layout = pangocairo::functions::create_layout(cr);
        layout.set_font_description(Some(font));
        layout.set_text("M");
        let primary_baseline = layout.baseline() as f64 / gtk::pango::SCALE as f64;
        let sel_bg = selection_bg.unwrap_or(Rgb {
            r: 51,
            g: 87,
            b: 140,
        });
        for row in 0..rows {
            let y = row as f64 * ch;
            let row_start = row as usize * cols as usize;
            let cells = &grid[row_start..row_start + cols as usize];
            for (col, cell) in cells.iter().enumerate() {
                let cell_px_w = if cell.wide { cw * 2.0 } else { cw };
                if let Some(c) = super::cell_bg_fill(cell, sel_bg) {
                    let (r, g, bl) = super::rgb(c);
                    cr.set_source_rgb(r, g, bl);
                    cr.rectangle(col as f64 * cw, y, cell_px_w, ch);
                    let _ = cr.fill();
                }
            }
            for (col, cell) in cells.iter().enumerate() {
                let x = col as f64 * cw;
                let cell_px_w = if cell.wide { cw * 2.0 } else { cw };
                let (fr, fgc, fb) = super::rgb(super::cell_fg(cell, colors, selection_fg));
                if !cell.text.is_empty() {
                    layout.set_text(&cell.text);
                    cr.set_source_rgb(fr, fgc, fb);
                    let ascii = !cell.wide
                        && cell.text.len() == 1
                        && cell.text.as_bytes()[0].is_ascii_graphic();
                    if ascii {
                        cr.move_to(x, y + ascent - primary_baseline);
                    } else {
                        let baseline = layout.baseline() as f64 / gtk::pango::SCALE as f64;
                        let glyph_w = layout.pixel_size().0 as f64;
                        let x_off = ((cell_px_w - glyph_w) / 2.0).max(0.0);
                        cr.move_to(x + x_off, y + ascent - baseline);
                    }
                    pangocairo::functions::show_layout(cr, &layout);
                }
                if cell.style.underline {
                    cr.set_source_rgb(fr, fgc, fb);
                    cr.rectangle(x, y + ascent + 2.0, cell_px_w, 1.0);
                    let _ = cr.fill();
                }
                if cell.style.strikethrough {
                    cr.set_source_rgb(fr, fgc, fb);
                    cr.rectangle(x, y + ch * 0.5, cell_px_w, 1.0);
                    let _ = cr.fill();
                }
            }
        }
    }

    fn time_mode(grid: &[super::VtCell], cols: u16, rows: u16, mode: Mode, loops: u32) -> f64 {
        use flowmux_terminal::vt::{Colors, Rgb};
        use gtk::cairo::{Context, Format, ImageSurface};
        use std::time::Instant;

        let font = gtk::pango::FontDescription::from_string(super::DEFAULT_FONT);
        let (cw, ch, ascent) = super::measure_cell(&font);
        let w = (cols as f64 * cw).ceil() as i32;
        let h = (rows as f64 * ch).ceil() as i32;
        let colors = Colors {
            fg: Rgb {
                r: 220,
                g: 220,
                b: 220,
            },
            bg: Rgb { r: 0, g: 0, b: 0 },
            cursor: Rgb {
                r: 220,
                g: 220,
                b: 220,
            },
            cursor_has_value: false,
        };
        let screen = ImageSurface::create(Format::ARgb32, w, h).unwrap();
        let scr = Context::new(&screen).unwrap();
        let reused = ImageSurface::create(Format::ARgb32, w, h).unwrap();

        let start = Instant::now();
        for _ in 0..loops {
            match mode {
                Mode::Direct => {
                    super::paint_grid(
                        &scr, &font, w, h, cols, rows, grid, cw, ch, ascent, &colors, None, None,
                    );
                }
                Mode::PerCell => {
                    paint_grid_percell(
                        &scr, &font, w, h, cols, rows, grid, cw, ch, ascent, &colors, None, None,
                    );
                }
                Mode::OffFresh => {
                    let s = ImageSurface::create(Format::ARgb32, w, h).unwrap();
                    {
                        let c = Context::new(&s).unwrap();
                        super::paint_grid(
                            &c, &font, w, h, cols, rows, grid, cw, ch, ascent, &colors, None, None,
                        );
                    }
                    scr.set_source_surface(&s, 0.0, 0.0).unwrap();
                    scr.paint().unwrap();
                }
                Mode::OffReused => {
                    {
                        let c = Context::new(&reused).unwrap();
                        super::paint_grid(
                            &c, &font, w, h, cols, rows, grid, cw, ch, ascent, &colors, None, None,
                        );
                    }
                    scr.set_source_surface(&reused, 0.0, 0.0).unwrap();
                    scr.paint().unwrap();
                }
            }
        }
        start.elapsed().as_secs_f64() * 1000.0 / loops as f64
    }

    #[test]
    #[ignore = "manual render benchmark; run with --ignored --nocapture"]
    fn render_throughput_bench() {
        let (cols, rows) = (200u16, 50u16);
        let loops = 100u32;
        let kinds = ["ascii_dense", "ascii_words", "colored", "cjk", "sparse"];
        println!("\ngrid {cols}x{rows}, {loops} frames/measure — ms/frame (lower=better)");
        println!(
            "{:<13} {:>10} {:>10} {:>10} {:>10} {:>9}",
            "scenario", "per_cell", "direct", "off_fresh", "off_reuse", "gain%"
        );
        // per_cell = pre-0.5.5 baseline; direct = shipped 0.5.5; off_fresh = the
        // 0.5.4 offscreen-cache regression; gain% = per_cell→direct improvement.
        for kind in kinds {
            let grid = bench_grid(cols as usize, rows as usize, kind);
            // Warm up font/glyph caches so the first measure is not penalized.
            let _ = time_mode(&grid, cols, rows, Mode::Direct, 5);
            let p = time_mode(&grid, cols, rows, Mode::PerCell, loops);
            let d = time_mode(&grid, cols, rows, Mode::Direct, loops);
            let f = time_mode(&grid, cols, rows, Mode::OffFresh, loops);
            let r = time_mode(&grid, cols, rows, Mode::OffReused, loops);
            let gain = (p - d) / p * 100.0;
            println!("{kind:<13} {p:>10.3} {d:>10.3} {f:>10.3} {r:>10.3} {gain:>8.1}%");
        }
    }

    #[test]
    fn finds_http_url_under_column() {
        let line = "see https://example.com/path for details";
        // Column 10 falls inside the URL token.
        assert_eq!(
            find_url_at(line, 10).as_deref(),
            Some("https://example.com/path")
        );
    }

    #[test]
    fn trims_trailing_punctuation() {
        let line = "visit (https://rust-lang.org).";
        assert_eq!(
            find_url_at(line, 20).as_deref(),
            Some("https://rust-lang.org")
        );
    }

    #[test]
    fn bare_www_gets_https_scheme() {
        let line = "go to www.example.org now";
        assert_eq!(
            find_url_at(line, 8).as_deref(),
            Some("https://www.example.org")
        );
    }

    #[test]
    fn no_url_on_plain_or_whitespace() {
        assert_eq!(find_url_at("just some words here", 5), None);
        assert_eq!(find_url_at("a  b", 1), None); // whitespace column
    }

    #[test]
    fn modifier_key_does_not_force_ime_preedit_reset() {
        assert!(is_modifier_only_key(gdk::Key::Shift_L));
        assert!(is_modifier_only_key(gdk::Key::Shift_R));
        assert!(is_modifier_only_key(gdk::Key::ISO_Level3_Shift));
        assert!(!is_modifier_only_key(gdk::Key::T));
        assert!(!is_modifier_only_key(gdk::Key::Return));
    }

    #[test]
    fn shift_enter_is_terminal_insert_newline_shortcut() {
        assert_eq!(INSERT_NEWLINE_BYTES, b"\x16\n");
        assert!(is_insert_newline_key(
            gdk::Key::Return,
            flowmux_terminal::vt::MOD_SHIFT
        ));
        assert!(is_insert_newline_key(
            gdk::Key::KP_Enter,
            flowmux_terminal::vt::MOD_SHIFT
        ));
        assert!(is_insert_newline_key(
            gdk::Key::ISO_Enter,
            flowmux_terminal::vt::MOD_SHIFT
        ));
        assert!(!is_insert_newline_key(gdk::Key::Return, 0));
        assert!(!is_insert_newline_key(
            gdk::Key::Return,
            flowmux_terminal::vt::MOD_SHIFT | flowmux_terminal::vt::MOD_CTRL
        ));
    }

    #[test]
    fn pending_input_is_written_after_ime_commit_text() {
        let pending: PendingInput = Rc::new(RefCell::new(None));
        queue_pending_input(&pending, INSERT_NEWLINE_BYTES);

        let bytes = commit_bytes_with_pending("녕", &pending);

        let mut expected = Vec::from("녕".as_bytes());
        expected.extend_from_slice(INSERT_NEWLINE_BYTES);
        assert_eq!(bytes, expected);
        assert!(pending.borrow().is_none());
    }

    #[test]
    fn pending_input_preserves_queue_order() {
        let pending: PendingInput = Rc::new(RefCell::new(None));
        queue_pending_input(&pending, b"a");
        queue_pending_input(&pending, b"b");

        assert_eq!(commit_bytes_with_pending("녕", &pending), "녕ab".as_bytes());
    }
}
