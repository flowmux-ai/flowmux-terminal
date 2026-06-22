// SPDX-License-Identifier: GPL-3.0-or-later
//! libghostty-vt-backed terminal pane (task C).
//!
//! A drop-in alternative to [`crate::ui::terminal_pane::TerminalPane`] that
//! renders the grid itself from `flowmux_terminal`'s libghostty-vt core instead
//! of embedding a VTE widget. Selected at pane-creation time by
//! [`crate::ui::pane_terminal::PaneTerminal`] when the libghostty backend is
//! toggled on; the VTE path stays the default so nothing regresses.
//!
//! Parity status: rendering (theme font/colors/metrics), PTY I/O, keyboard +
//! inline IME preedit (Hangul/CJK), focus, font, resize, drag selection +
//! clipboard copy/paste, mouse reporting (modes 1000/1002/1003), wheel
//! scrollback, Ctrl-click URLs, OSC 0/2 title tracking, and OSC 7 / `/proc`
//! cwd all work. Not yet matched: a visible scrollbar widget (libghostty
//! exposes no viewport-offset query) — wheel scrolling works regardless.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use gtk::cairo;
use gtk::gdk;
use gtk::glib;
use gtk::pango;
use gtk::prelude::*;

use flowmux_core::{PaneId, SurfaceId};
use flowmux_terminal::pty::Pty;
use flowmux_terminal::vt::{Colors, MouseAction, MouseButton, Rgb, Vt};

use crate::ui::terminal_pane::PaneCallbacks;

const DEFAULT_FONT: &str = "Monospace 12";
const SCROLLBACK: usize = 10_000;

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
    /// Last OSC title seen, to fire the title-changed callback only on change.
    last_title: String,
}

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
        let pty = Pty::spawn(
            &argv_ref,
            cwd.as_deref(),
            &extra_env,
            cols,
            rows,
        )
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
            anchor: (0, 0),
            last_title: String::new(),
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

        let pane = GhosttyPane {
            id,
            surface,
            container,
            area: area.clone(),
            state: state.clone(),
            pid: pid.clone(),
        };

        pane.install_draw();
        pane.install_resize();
        pane.install_pty_pump(callbacks.clone());
        pane.install_input();
        pane.install_mouse(callbacks.clone());
        pane.install_focus(callbacks);

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
        self.area.connect_resize(move |_area, w, h| {
            let mut s = state.borrow_mut();
            if s.cell_w <= 0.0 || s.cell_h <= 0.0 {
                return;
            }
            let cols = ((w as f64 / s.cell_w).floor() as i64).clamp(1, u16::MAX as i64) as u16;
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
            }
        });
    }

    fn install_pty_pump(&self, callbacks: PaneCallbacks) {
        let state = self.state.clone();
        let area = self.area.clone();
        let pid = self.pid.clone();
        let id = self.id;
        let surface = self.surface;
        let fd = self.state.borrow().pty.master_fd();
        glib::source::unix_fd_add_local(fd, glib::IOCondition::IN, move |_fd, _cond| {
            let mut buf = [0u8; 16384];
            let mut s = state.borrow_mut();
            // OSC title change to forward after we drop the borrow (the VTE path
            // tracks this via connect_title_notify; libghostty has no poller).
            let mut title_change: Option<String> = None;
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
                }
                Err(_) => return glib::ControlFlow::Break,
            }
            drop(s);
            if let Some(title) = title_change {
                (callbacks.on_terminal_title_changed.borrow_mut())(id, surface, title);
            }
            area.queue_draw();
            glib::ControlFlow::Continue
        });
    }

    fn install_input(&self) {
        let key = gtk::EventControllerKey::new();

        // IME: route committed text (e.g. composed Hangul syllables) to the PTY
        // and show the in-progress preedit inline at the cursor. Setting the IM
        // context makes the controller filter text keys through the IME first,
        // so key-pressed below only fires for keys the IME did not consume.
        let im = gtk::IMMulticontext::new();
        key.set_im_context(Some(&im));

        // Non-text keys (control combos, navigation, function, Enter/Tab/…) are
        // encoded by libghostty honoring the terminal's modes (application
        // cursor keys, keypad, Kitty keyboard, Alt-as-ESC) — what vim/claude/
        // codex rely on. Plain text arrives via the IM commit above instead.
        {
            let state = self.state.clone();
            let area = self.area.clone();
            let im_for_key = im.clone();
            key.connect_key_pressed(move |_kc, keyval, _code, gtk_mods| {
                // Commit any in-progress IME text before this (non-consumed)
                // key acts, so e.g. Shift+Enter newlines AFTER the composing
                // syllable instead of stranding it after the newline. Scope the
                // borrow so the synchronous commit signal can borrow_mut.
                let composing = !state.borrow().preedit.is_empty();
                if composing {
                    im_for_key.reset();
                }
                let mods = mods_from_state(gtk_mods);
                let (named, cp) = map_keyval(keyval);
                let mut s = state.borrow_mut();
                match s.vt.encode_key(named, cp, mods, false) {
                    Some(bytes) => {
                        let _ = s.pty.write(&bytes);
                        drop(s);
                        area.queue_draw();
                        glib::Propagation::Stop
                    }
                    None => glib::Propagation::Proceed,
                }
            });
        }
        {
            let state = self.state.clone();
            let area = self.area.clone();
            im.connect_commit(move |_im, text| {
                let mut s = state.borrow_mut();
                s.preedit.clear();
                let _ = s.pty.write(text.as_bytes());
                drop(s);
                area.queue_draw();
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
                if s.selecting {
                    let (cols, rows) = s.vt.dims().unwrap_or((s.cols, s.rows));
                    let anchor = s.anchor;
                    let end = px_to_cell(x, y, s.cell_w, s.cell_h, cols, rows);
                    if s.vt.set_selection(anchor, end, false) {
                        s.has_sel = anchor != end;
                    }
                    drop(s);
                    area.queue_draw();
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
            let id = self.id;
            click.connect_pressed(move |g, _n, x, y| {
                let button = g.current_button();
                let mods = mods_from_state(g.current_event_state());
                let mut s = state.borrow_mut();
                s.pointer = (x, y);

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

                // App mouse reporting (unless Shift forces local selection).
                let shift = (mods & flowmux_terminal::vt::MOD_SHIFT) != 0;
                if s.vt.mouse_enabled() && !shift {
                    let gb = ghostty_button(button);
                    if let Some(bytes) = s.vt.encode_mouse(MouseAction::Press, gb, x, y, mods) {
                        let _ = s.pty.write(&bytes);
                    }
                    return;
                }

                // Otherwise begin a selection on the primary button.
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
                glib::Propagation::Stop
            });
        }
        self.area.add_controller(scroll);
    }

    fn install_focus(&self, callbacks: PaneCallbacks) {
        let focus = gtk::EventControllerFocus::new();
        let id = self.id;
        focus.connect_enter(move |_| {
            (callbacks.on_focus.borrow_mut())(id);
        });
        self.area.add_controller(focus);
    }

    // ---- TerminalPane-compatible method surface (see pane_terminal.rs) ----

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

    pub fn set_font_scale(&self, scale: f64) {
        let mut s = self.state.borrow_mut();
        s.font_scale = if scale > 0.0 { scale } else { 1.0 };
        s.remeasure();
        // Re-fit the grid to the new cell size on the next allocation.
        let (w, h) = (self.area.width(), self.area.height());
        if w > 0 && h > 0 && s.cell_w > 0.0 && s.cell_h > 0.0 {
            let cols = ((w as f64 / s.cell_w).floor() as i64).clamp(1, u16::MAX as i64) as u16;
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

    pub fn copy_selection_to_clipboard(&self) {
        let text = {
            let mut s = self.state.borrow_mut();
            if !s.has_sel {
                return;
            }
            // Refresh the snapshot so selection_text reads current `selected`
            // flags, then extract the selected run.
            s.vt.update();
            s.vt.selection_text()
        };
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            if let Some(display) = gdk::Display::default() {
                display.clipboard().set_text(&text);
            }
        }
    }

    pub fn paste_clipboard(&self) {
        let state = self.state.clone();
        let area = self.area.clone();
        if let Some(display) = gdk::Display::default() {
            let clipboard = display.clipboard();
            clipboard.read_text_async(gtk::gio::Cancellable::NONE, move |res| {
                if let Ok(Some(text)) = res {
                    let mut s = state.borrow_mut();
                    let _ = s.pty.write(text.as_bytes());
                    drop(s);
                    area.queue_draw();
                }
            });
        }
    }

    /// Inject bytes into the terminal display (not the child). Mirrors
    /// `TerminalPane::feed` (used to surface inline messages).
    pub fn feed(&self, bytes: &[u8]) {
        self.state.borrow_mut().vt.write(bytes);
        self.area.queue_draw();
    }

    pub fn feed_after_preedit_commit(&self, bytes: &'static [u8]) {
        let mut s = self.state.borrow_mut();
        let _ = s.pty.write(bytes);
        drop(s);
        self.area.queue_draw();
    }

    /// Visible screen text (all viewport rows joined), for `read-screen`.
    pub fn screen_text(&self) -> Option<String> {
        let s = self.state.borrow();
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
}

fn rgb(c: Rgb) -> (f64, f64, f64) {
    (c.r as f64 / 255.0, c.g as f64 / 255.0, c.b as f64 / 255.0)
}

/// Map a surface pixel position to a viewport cell, clamped to the grid.
fn px_to_cell(x: f64, y: f64, cell_w: f64, cell_h: f64, cols: u16, rows: u16) -> (u16, u16) {
    let col = if cell_w > 0.0 { (x / cell_w).floor().max(0.0) } else { 0.0 } as u32;
    let row = if cell_h > 0.0 { (y / cell_h).floor().max(0.0) } else { 0.0 } as u32;
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

fn draw(state: &mut State, cr: &cairo::Context, w: i32, h: i32) {
    let _ = state.vt.update();
    let colors = state.vt.colors().unwrap_or(Colors {
        fg: Rgb { r: 220, g: 220, b: 220 },
        bg: Rgb { r: 0, g: 0, b: 0 },
        cursor: Rgb { r: 220, g: 220, b: 220 },
        cursor_has_value: false,
    });

    let (br, bgc, bb) = rgb(colors.bg);
    cr.set_source_rgb(br, bgc, bb);
    cr.rectangle(0.0, 0.0, w as f64, h as f64);
    let _ = cr.fill();

    let layout = pangocairo::functions::create_layout(cr);
    layout.set_font_description(Some(&state.scaled_font()));

    let (cols, rows) = state.vt.dims().unwrap_or((state.cols, state.rows));
    let cw = state.cell_w;
    let ch = state.cell_h;
    let ascent = state.ascent;
    // Selection wash + cursor color come from the theme (host-drawn to match
    // VTE); fall back to a neutral blue-grey / the default fg when unset.
    let sel_bg = state.selection_bg.unwrap_or(Rgb { r: 51, g: 87, b: 140 });
    let sel_fg = state.selection_fg;
    let cursor_rgb = state.cursor_color.unwrap_or(if colors.cursor_has_value {
        colors.cursor
    } else {
        colors.fg
    });

    for row in 0..rows {
        let y = row as f64 * ch;
        // Read the row's cells once, then render in two passes. Backgrounds
        // must all be painted before any glyph, otherwise a wide glyph's right
        // half (which spills into the next cell) is erased by that spacer
        // cell's background fill — the cause of "left-half-only" Hangul on a
        // colored background.
        let cells: Vec<Option<flowmux_terminal::vt::Cell>> =
            (0..cols).map(|col| state.vt.cell(row, col)).collect();

        // Pass 1: backgrounds + selection wash.
        for (col, cell) in cells.iter().enumerate() {
            let Some(cell) = cell else { continue };
            let x = col as f64 * cw;
            let cell_px_w = if cell.wide { cw * 2.0 } else { cw };
            if cell.selected {
                let (r, g, bl) = rgb(sel_bg);
                cr.set_source_rgb(r, g, bl);
                cr.rectangle(x, y, cell_px_w, ch);
                let _ = cr.fill();
            } else {
                let bg = if cell.style.inverse {
                    Some(cell.fg)
                } else {
                    cell.bg
                };
                if let Some(b) = bg {
                    let (r, g, bl) = rgb(b);
                    cr.set_source_rgb(r, g, bl);
                    cr.rectangle(x, y, cell_px_w, ch);
                    let _ = cr.fill();
                }
            }
        }

        // Pass 2: glyphs + underline/strikethrough, on top of all backgrounds.
        for (col, cell) in cells.iter().enumerate() {
            let Some(cell) = cell else { continue };
            let x = col as f64 * cw;
            let cell_px_w = if cell.wide { cw * 2.0 } else { cw };
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
            let (fr, fgc, fb) = rgb(fg);
            if !cell.text.is_empty() {
                layout.set_text(&cell.text);
                // Baseline-align to `y + ascent` so fallback (CJK) glyphs line
                // up with ASCII regardless of their font's own metrics.
                let baseline = layout.baseline() as f64 / pango::SCALE as f64;
                let glyph_w = layout.pixel_size().0 as f64;
                cr.set_source_rgb(fr, fgc, fb);
                if cell.wide && glyph_w > 1.0 && glyph_w < cell_px_w - 0.5 {
                    // A wide glyph whose fallback font draws it narrower than
                    // two cells (e.g. Hangul at ~1.5 cells) leaves a gap that
                    // reads as loose letter-spacing. Scale it horizontally to
                    // fill the box so CJK looks snug like a CJK monospace font.
                    // A proper 2-cell CJK font has glyph_w ≈ box, so sx ≈ 1.
                    let sx = (cell_px_w / glyph_w).min(1.6);
                    cr.save().ok();
                    cr.translate(x, y + ascent - baseline);
                    cr.scale(sx, 1.0);
                    cr.move_to(0.0, 0.0);
                    pangocairo::functions::show_layout(cr, &layout);
                    cr.restore().ok();
                } else {
                    // Narrow glyphs (ASCII) sit at the cell origin; center any
                    // small slack so nothing rides against the right edge.
                    let x_off = ((cell_px_w - glyph_w) / 2.0).max(0.0);
                    cr.move_to(x + x_off, y + ascent - baseline);
                    pangocairo::functions::show_layout(cr, &layout);
                }
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

    if let Some(cursor) = state.vt.cursor() {
        let cx = cursor.x as f64 * cw;
        let cy = cursor.y as f64 * ch;
        if !state.preedit.is_empty() {
            // Inline IME preedit (composing Hangul/CJK): paint the composing
            // text at the cursor with an underline, then the block cursor right
            // after it — matching the VTE path's inline preedit.
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
            if cursor.visible {
                let (r, g, b) = rgb(cursor_rgb);
                cr.set_source_rgba(r, g, b, 0.6);
                cr.rectangle(cx + pw, cy, cw, ch);
                let _ = cr.fill();
            }
        } else if cursor.visible && cursor.x < cols && cursor.y < rows {
            let (r, g, b) = rgb(cursor_rgb);
            cr.set_source_rgba(r, g, b, 0.6);
            cr.rectangle(cx, cy, cw, ch);
            let _ = cr.fill();
        }
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
    use super::find_url_at;

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
}
