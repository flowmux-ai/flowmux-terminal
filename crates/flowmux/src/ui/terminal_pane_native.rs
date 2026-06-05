// SPDX-License-Identifier: GPL-3.0-or-later
//! Pure-Rust terminal pane: wires the `flowmux-terminal` engine
//! (alacritty_terminal PTY + parser + grid) to the GTK4
//! [`TerminalRenderArea`] widget. Drop-in replacement for the VTE-backed
//! [`super::terminal_pane::TerminalPane`] — same public surface
//! (`spawn(PaneCallbacks)`, `root_widget`, `grab_focus`, `set_font`,
//! selection/clipboard, `connect_*_notify`, …) so the swap in
//! `workspace_view` / `window` is a type change.
//!
//! Built alongside the VTE pane so the tree stays compiling while the new
//! path reaches parity (see `docs/pure-rust-terminal-migration.md`).
//! Still pending: IME (Hangul) preedit, scrollback view wired to the
//! scrollbar, URL Ctrl-click, mouse-tracking forward.
//!
//! ## Threading
//!
//! The engine reads the PTY on its own thread and fires [`TermEvent`]s
//! from there. The sink forwards them down an `async_channel`; a
//! `glib::MainContext::spawn_local` loop on the GTK thread drains it and
//! is the only place that touches widgets — on `Wakeup` it locks the
//! term, builds a [`FrameSnapshot`], and repaints only the changed rows.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use flowmux_core::{PaneId, SurfaceId};
use flowmux_terminal::engine::{EngineSpec, TermEngine, TermEvent};
use flowmux_terminal::render::{self, ThemePalette};
use gtk::glib;
use gtk::prelude::*;

use super::pane_common::{wrap_argv_with_pty_tee, PaneCallbacks};
use super::terminal_render::TerminalRenderArea;

const DEFAULT_SCROLLBACK: usize = 20_000;
const DEFAULT_FONT_FAMILY: &str = "monospace";
const DEFAULT_FONT_SIZE_PT: f64 = 12.0;

type NotifyCb<T> = Rc<RefCell<Option<Box<dyn Fn(T)>>>>;

/// A pure-Rust terminal surface.
#[derive(Clone)]
pub struct TerminalPaneNative {
    pub id: PaneId,
    /// The render widget. Public so split-tree identity checks can clone +
    /// compare it (mirrors the VTE pane's `widget` field).
    pub render: TerminalRenderArea,
    /// Root container: an `Overlay` holding the render widget plus a
    /// vertical scrollbar, matching the VTE pane's layout.
    pub container: gtk::Overlay,
    engine: Rc<RefCell<TermEngine>>,
    title: Rc<RefCell<String>>,
    theme: Rc<RefCell<ThemePalette>>,
    /// Base font (family, size_pt) before the global zoom scale.
    base_font: Rc<RefCell<(String, f64)>>,
    scale: Rc<Cell<f64>>,
    on_title_notify: NotifyCb<(TerminalPaneNative, String)>,
    on_cwd_notify: NotifyCb<TerminalPaneNative>,
    /// Scrollback scrollbar position; `adj_updating` guards the
    /// programmatic-update ↔ user-drag feedback loop.
    scroll_adj: gtk::Adjustment,
    adj_updating: Rc<Cell<bool>>,
    /// The IME context (None under the unit-test harness). Held so the
    /// Shift+Enter "insert newline" path can flush a pending preedit
    /// before writing the newline.
    im: Rc<RefCell<Option<gtk::IMMulticontext>>>,
    /// True while an IME preedit (composing) string is showing.
    preedit_active: Rc<Cell<bool>>,
    /// Newline bytes parked while the IME finalises a composing syllable;
    /// written by the `commit` handler right after the syllable so the
    /// order is "요" then newline, not newline then "요".
    pending_newline: Rc<Cell<Option<&'static [u8]>>>,
}

impl TerminalPaneNative {
    pub fn spawn(
        id: PaneId,
        surface: SurfaceId,
        argv: Vec<String>,
        cwd: Option<PathBuf>,
        extra_env: Vec<(String, String)>,
        callbacks: PaneCallbacks,
    ) -> Self {
        let render = TerminalRenderArea::new();
        render.set_font(DEFAULT_FONT_FAMILY, DEFAULT_FONT_SIZE_PT);
        let metrics = render.font_metrics().expect("font metrics after set_font");

        // Default shell when none given: a plain interactive (NON-login)
        // shell, matching gnome-terminal's default. A login shell would run
        // the user's login-only profile (~/.profile / ~/.bash_profile); on
        // some setups (observed on Ubuntu 22.04) that file *resets* PATH
        // without /usr/bin, so base tools like xset go missing — while a
        // non-login shell sources only ~/.bashrc and keeps the inherited
        // PATH. Configured shells (workspace_view) are already non-login, so
        // this also makes the default consistent with them.
        let argv = if argv.is_empty() {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
            vec![shell]
        } else {
            argv
        };
        // Wrap with `flowmuxctl pty-tee` so OSC 9/99/777 alarms from
        // terminal-side agents reach the daemon — backend-independent, so
        // notifications keep working exactly as on VTE.
        let argv = wrap_argv_with_pty_tee(argv, id, surface);

        let (tx, rx) = async_channel::unbounded::<TermEvent>();
        let sink: Sink = Arc::new(move |ev| {
            let _ = tx.send_blocking(ev);
        });

        let spec = EngineSpec {
            argv,
            cwd,
            env: extra_env,
            rows: 24,
            cols: 80,
            cell_width: metrics.cell_w.round().max(1.0) as u16,
            cell_height: metrics.cell_h.round().max(1.0) as u16,
            scrollback: DEFAULT_SCROLLBACK,
        };
        // The GUI test suite builds many workspaces in one process; forking
        // a real shell per pane there exhausts processes/threads (the old
        // VTE pane never forked in tests because its spawn was async). Use a
        // headless stub under test; the real path always spawns.
        let engine = Rc::new(RefCell::new(if cfg!(test) {
            let _ = &sink;
            TermEngine::stub(spec.rows, spec.cols)
        } else {
            TermEngine::spawn(spec, sink).expect("spawn terminal engine")
        }));

        // Container: render widget + overlaid vertical scrollbar.
        let container = gtk::Overlay::new();
        container.set_hexpand(true);
        container.set_vexpand(true);
        container.set_child(Some(&render));
        let scroll_adjustment = gtk::Adjustment::new(0.0, 0.0, 1.0, 1.0, 1.0, 1.0);
        let scrollbar = gtk::Scrollbar::new(gtk::Orientation::Vertical, Some(&scroll_adjustment));
        scrollbar.set_halign(gtk::Align::End);
        scrollbar.set_valign(gtk::Align::Fill);
        scrollbar.set_can_focus(false);
        scrollbar.set_width_request(12);
        container.add_overlay(&scrollbar);

        let title = Rc::new(RefCell::new(String::new()));
        let theme = Rc::new(RefCell::new(ThemePalette::default()));
        let base_font = Rc::new(RefCell::new((
            DEFAULT_FONT_FAMILY.to_string(),
            DEFAULT_FONT_SIZE_PT,
        )));
        let scale = Rc::new(Cell::new(1.0));
        let on_title_notify: NotifyCb<(TerminalPaneNative, String)> = Rc::new(RefCell::new(None));
        let on_cwd_notify: NotifyCb<TerminalPaneNative> = Rc::new(RefCell::new(None));
        let adj_updating = Rc::new(Cell::new(false));

        let pane = Self {
            id,
            render: render.clone(),
            container,
            engine: engine.clone(),
            title: title.clone(),
            theme: theme.clone(),
            base_font,
            scale,
            on_title_notify: on_title_notify.clone(),
            on_cwd_notify: on_cwd_notify.clone(),
            scroll_adj: scroll_adjustment.clone(),
            adj_updating: adj_updating.clone(),
            im: Rc::new(RefCell::new(None)),
            preedit_active: Rc::new(Cell::new(false)),
            pending_newline: Rc::new(Cell::new(None)),
        };

        // User drags the scrollbar → scroll the viewport to match.
        {
            let engine = engine.clone();
            let render_w = render.clone();
            let theme = theme.clone();
            let updating = adj_updating.clone();
            scroll_adjustment.connect_value_changed(move |adj| {
                if updating.get() {
                    return;
                }
                let (offset, history) = engine.borrow().scrollback_state();
                // value = history - target_offset (bottom = max).
                let target = (history as f64 - adj.value()).round().max(0.0) as usize;
                let delta = target as i32 - offset as i32;
                if delta != 0 {
                    engine.borrow().scroll_lines(delta);
                    repaint(&engine, &render_w, &theme);
                    sync_scrollbar(&engine, adj, &updating);
                }
            });
        }

        wire_resize(&pane);
        wire_keyboard(&pane);
        wire_scroll(&pane);
        wire_mouse_report(&pane);
        wire_url_click(&pane, &callbacks);
        wire_mouse_selection(&pane, &callbacks);
        wire_focus_and_menu(&pane, surface, &callbacks);
        wire_event_loop(&pane, surface, rx, &callbacks);

        // Cursor blink: a self-terminating ~530 ms timer flips the blink
        // phase. The weak ref breaks the source once the pane (and its render
        // widget) drops. Skipped under the test harness, which has no main
        // loop and builds/destroys panes without one.
        if !cfg!(test) {
            let render = render.downgrade();
            glib::timeout_add_local(std::time::Duration::from_millis(530), move || match render
                .upgrade()
            {
                Some(r) => {
                    r.toggle_blink();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            });
        }

        pane
    }

    // ---- VTE-pane-compatible surface ------------------------------------

    pub fn root_widget(&self) -> gtk::Widget {
        self.container.clone().upcast::<gtk::Widget>()
    }

    pub fn grab_focus(&self) {
        self.render.grab_focus();
    }

    /// Best-effort shell cwd via `/proc/<pid>/cwd` (updates on `cd`).
    pub fn current_dir(&self) -> Option<PathBuf> {
        let pid = self.engine.borrow().pid()?;
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    pub fn title(&self) -> String {
        self.title.borrow().clone()
    }

    /// Global zoom: multiplies the base font size.
    pub fn set_font_scale(&self, scale: f64) {
        self.scale.set(scale.max(0.1));
        self.reapply_font();
    }

    /// Replace the base font from a pango description (family + size).
    pub fn set_font(&self, desc: &gtk::pango::FontDescription) {
        let family = desc
            .family()
            .map(|f| f.to_string())
            .unwrap_or_else(|| DEFAULT_FONT_FAMILY.to_string());
        let size_pt = if desc.size() > 0 {
            desc.size() as f64 / gtk::pango::SCALE as f64
        } else {
            DEFAULT_FONT_SIZE_PT
        };
        *self.base_font.borrow_mut() = (family, size_pt);
        self.reapply_font();
    }

    fn reapply_font(&self) {
        let (family, size) = self.base_font.borrow().clone();
        let effective = (size * self.scale.get()).max(1.0);
        self.render.set_font(&family, effective);
        if let Some(m) = self.render.font_metrics() {
            let cols = (self.render.width().max(1) as f32 / m.cell_w)
                .floor()
                .max(1.0) as u16;
            let rows = (self.render.height().max(1) as f32 / m.cell_h)
                .floor()
                .max(1.0) as u16;
            self.engine.borrow_mut().resize(
                rows,
                cols,
                m.cell_w.round().max(1.0) as u16,
                m.cell_h.round().max(1.0) as u16,
            );
        }
        self.repaint();
    }

    /// Apply the user's resolved theme palette and repaint. Live app
    /// OSC 4 changes still take precedence per cell.
    pub fn set_theme_palette(&self, palette: ThemePalette) {
        *self.theme.borrow_mut() = palette;
        self.render.invalidate_all();
        self.repaint();
    }

    pub fn has_selection(&self) -> bool {
        self.engine.borrow().has_selection()
    }

    pub fn copy_selection_to_clipboard(&self) {
        if let Some(text) = self.engine.borrow().selection_text() {
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&text);
            }
        }
    }

    /// Paste clipboard text, wrapping it in bracketed-paste markers when
    /// the app requested DECSET 2004 so editors/shells treat it as pasted
    /// data rather than typed commands. Same effect as the `paste` keybinding;
    /// also the right-click "Paste" menu item.
    pub fn paste_clipboard(&self) {
        let engine = self.engine.clone();
        if let Some(display) = gtk::gdk::Display::default() {
            display
                .clipboard()
                .read_text_async(gtk::gio::Cancellable::NONE, move |res| {
                    if let Ok(Some(text)) = res {
                        let e = engine.borrow();
                        e.write_keys(bracketed_paste_payload(&text, e.bracketed_paste()));
                    }
                });
        }
    }

    /// Feed raw bytes to the PTY (paste / programmatic input).
    pub fn feed(&self, bytes: &[u8]) {
        self.engine.borrow().write(bytes.to_vec());
    }

    /// Feed `bytes` (e.g. Shift+Enter's `ESC CR`) to the PTY, but if an IME
    /// preedit is composing, finalise it first so the committed syllable
    /// lands *before* `bytes`. Shift+Enter arrives via a window accelerator
    /// that bypasses the terminal's IME, so a naive write would emit the
    /// newline ahead of the still-composing syllable ("안녕하세\n요"). We
    /// park the newline and `reset()` the IME: its `commit` handler writes
    /// the syllable and then the parked newline, in order. A short timeout
    /// is the fallback for IMEs that cancel rather than commit on reset.
    pub fn feed_after_preedit_commit(&self, bytes: &'static [u8]) {
        if !self.preedit_active.get() {
            self.engine.borrow().write_keys(bytes.to_vec());
            return;
        }
        self.pending_newline.set(Some(bytes));
        if let Some(im) = self.im.borrow().as_ref() {
            im.reset();
        }
        // Fallback: if reset() produced no commit, the parked newline is
        // still set after a tick — flush it so Shift+Enter never no-ops.
        let pending = self.pending_newline.clone();
        let engine = self.engine.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(50), move || {
            if let Some(nl) = pending.take() {
                engine.borrow().write_keys(nl.to_vec());
            }
        });
    }

    pub fn add_controller(&self, controller: impl IsA<gtk::EventController>) {
        self.render.add_controller(controller);
    }

    /// Register a cwd-change observer. Fired (best-effort) when the title
    /// changes, since the native backend has no OSC 7 signal yet; callers
    /// read [`Self::current_dir`] inside the callback.
    pub fn connect_current_dir_notify(&self, callback: impl Fn(&Self) + 'static) {
        let pane = self.clone();
        *self.on_cwd_notify.borrow_mut() = Some(Box::new(move |p: TerminalPaneNative| {
            let _ = &pane;
            callback(&p);
        }));
    }

    /// Register an OSC 0/2 title observer.
    pub fn connect_title_notify(&self, callback: impl Fn(&Self, String) + 'static) {
        *self.on_title_notify.borrow_mut() =
            Some(Box::new(move |(p, title): (TerminalPaneNative, String)| {
                callback(&p, title);
            }));
    }

    fn repaint(&self) {
        let engine = self.engine.borrow();
        let term = engine.term().lock();
        let frame = render::snapshot(&*term, &self.theme.borrow());
        drop(term);
        self.render.set_frame(frame);
    }
}

/// Build the byte payload for a paste. When the app set DECSET 2004
/// (`bracketed`), the text is wrapped in `ESC [ 200 ~` … `ESC [ 201 ~` so
/// shells/editors treat it as pasted data rather than typed commands;
/// otherwise the raw UTF-8 bytes are sent as-is.
fn bracketed_paste_payload(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut buf = Vec::with_capacity(text.len() + 12);
        buf.extend_from_slice(b"\x1b[200~");
        buf.extend_from_slice(text.as_bytes());
        buf.extend_from_slice(b"\x1b[201~");
        buf
    } else {
        text.as_bytes().to_vec()
    }
}

/// Sink type for engine events (Send + Sync for the reader thread).
type Sink = Arc<dyn Fn(TermEvent) + Send + Sync + 'static>;

fn wire_resize(pane: &TerminalPaneNative) {
    let engine = pane.engine.clone();
    let render_w = pane.render.clone();
    let theme = pane.theme.clone();
    pane.render.connect_grid_resize(move |cols, rows| {
        let Some(m) = render_w.font_metrics() else {
            return;
        };
        engine.borrow_mut().resize(
            rows,
            cols,
            m.cell_w.round().max(1.0) as u16,
            m.cell_h.round().max(1.0) as u16,
        );
        repaint(&engine, &render_w, &theme);
    });
}

fn wire_keyboard(pane: &TerminalPaneNative) {
    let key = gtk::EventControllerKey::new();

    // Input method: drives Hangul / CJK composition. `set_im_context`
    // makes the controller present key events to the IM first; `commit`
    // delivers finished text and key-pressed fires only for keys the IM
    // did not consume. The composing syllable is shown via `set_preedit`
    // and committed synchronously, so an Enter that finalises a syllable
    // writes the text before the newline — no async reorder hazard.
    // IME teardown (IMMulticontext + client widget) is skipped under the
    // unit-test harness, which builds/destroys hundreds of panes without a
    // main loop and crashes in GTK's IM teardown path.
    let im = gtk::IMMulticontext::new();
    if !cfg!(test) {
        im.set_client_widget(Some(&pane.render));
        key.set_im_context(Some(&im));
        *pane.im.borrow_mut() = Some(im.clone());
        // Detach the IM context from the widget before the widget is
        // destroyed, so GTK's IM teardown never dereferences a dropped
        // client widget when a pane closes.
        let im_teardown = im.clone();
        pane.render.connect_unrealize(move |_| {
            im_teardown.set_client_widget(gtk::Widget::NONE);
        });

        // Plain Enter while a Hangul/CJK syllable is composing: ibus-hangul
        // commits the syllable on Enter and *swallows* the key, so the IM
        // controller's `key-pressed` never fires and the terminal receives no
        // newline ("한글 조합 중 Enter 무반응"). Catch Enter in the capture
        // phase, before the IM context sees it: when composing, finalise the
        // syllable and send exactly one CR after it (same park-then-commit
        // path as Shift+Enter). When nothing is composing we Proceed, so
        // normal Enter still flows through the IM + key handler untouched.
        let enter = gtk::EventControllerKey::new();
        enter.set_propagation_phase(gtk::PropagationPhase::Capture);
        let pane_for_enter = pane.clone();
        enter.connect_key_pressed(move |_, keyval, _keycode, state| {
            use gtk::gdk::Key;
            let plain = !state.intersects(
                gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::ALT_MASK,
            );
            if plain
                && matches!(keyval, Key::Return | Key::KP_Enter | Key::ISO_Enter)
                && pane_for_enter.preedit_active.get()
            {
                pane_for_enter.feed_after_preedit_commit(b"\r");
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        pane.render.add_controller(enter);
    }

    // commit → write the finished text to the PTY, clear preedit. If a
    // Shift+Enter newline is parked (composing syllable being finalised),
    // write it right after the committed text so the order is correct.
    {
        let engine = pane.engine.clone();
        let render = pane.render.clone();
        let preedit_active = pane.preedit_active.clone();
        let pending_newline = pane.pending_newline.clone();
        im.connect_commit(move |_, text| {
            if !text.is_empty() {
                engine.borrow().write_keys(text.as_bytes().to_vec());
            }
            render.set_preedit("");
            preedit_active.set(false);
            if let Some(nl) = pending_newline.take() {
                engine.borrow().write_keys(nl.to_vec());
            }
        });
    }
    // preedit changed → show the composing string inline at the caret.
    {
        let render = pane.render.clone();
        let preedit_active = pane.preedit_active.clone();
        im.connect_preedit_changed(move |im| {
            let (s, _attrs, _pos) = im.preedit_string();
            preedit_active.set(!s.is_empty());
            render.set_preedit(&s);
        });
    }
    {
        let render = pane.render.clone();
        let preedit_active = pane.preedit_active.clone();
        im.connect_preedit_end(move |_| {
            preedit_active.set(false);
            render.set_preedit("");
        });
    }
    // Keep the IM focused while the terminal is focused so candidate
    // windows position correctly.
    {
        let im_focus = im.clone();
        let fc = gtk::EventControllerFocus::new();
        fc.connect_enter(move |_| im_focus.focus_in());
        let im_blur = im.clone();
        fc.connect_leave(move |_| im_blur.focus_out());
        pane.render.add_controller(fc);
    }

    let engine = pane.engine.clone();
    let render_w = pane.render.clone();
    let theme = pane.theme.clone();
    let adj = pane.scroll_adj.clone();
    let updating = pane.adj_updating.clone();
    key.connect_key_pressed(move |_, keyval, _keycode, state| {
        use gtk::gdk::Key;
        // Shift+PageUp/Down pages the local scrollback instead of sending
        // the key to the app (matches the prior smart-paging behavior).
        if state.contains(gtk::gdk::ModifierType::SHIFT_MASK)
            && matches!(keyval, Key::Page_Up | Key::Page_Down)
            && !engine.borrow().alt_screen()
        {
            engine.borrow().scroll_page(keyval == Key::Page_Up);
            repaint(&engine, &render_w, &theme);
            sync_scrollbar(&engine, &adj, &updating);
            return glib::Propagation::Stop;
        }
        let app_cursor = engine.borrow().app_cursor_mode();
        match encode_key(keyval, state, app_cursor) {
            Some(bytes) => {
                // Typing keeps the cursor solid and dismisses any highlight,
                // matching every other terminal.
                render_w.reset_blink();
                let e = engine.borrow();
                let scrolled = e.write_keys(bytes);
                let had_sel = e.has_selection();
                if had_sel {
                    e.selection_clear();
                }
                if scrolled || had_sel {
                    drop(e);
                    repaint(&engine, &render_w, &theme);
                    if scrolled {
                        sync_scrollbar(&engine, &adj, &updating);
                    }
                }
                glib::Propagation::Stop
            }
            None => glib::Propagation::Proceed,
        }
    });
    pane.render.add_controller(key);
}

fn wire_scroll(pane: &TerminalPaneNative) {
    let engine = pane.engine.clone();
    let render_w = pane.render.clone();
    let theme = pane.theme.clone();
    let adj = pane.scroll_adj.clone();
    let updating = pane.adj_updating.clone();
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    scroll.connect_scroll(move |_, _dx, dy| {
        let e = engine.borrow();
        let notches = dy.abs().round().max(1.0) as usize;
        if e.alt_screen() {
            // Full-screen TUIs (vim/tig/less) have no scrollback of their
            // own; forward the wheel as cursor keys so they scroll, matching
            // how VTE-based terminals drive alt-screen apps.
            let app_cursor = e.app_cursor_mode();
            let csi: &[u8] = if app_cursor { b"\x1bO" } else { b"\x1b[" };
            let tail = if dy < 0.0 { b'A' } else { b'B' };
            let mut seq = Vec::with_capacity(notches * 3);
            for _ in 0..notches.min(5) {
                seq.extend_from_slice(csi);
                seq.push(tail);
            }
            e.write(seq);
        } else {
            let lines = (-dy * 3.0).round() as i32;
            if lines != 0 {
                e.scroll_lines(lines);
                drop(e);
                repaint(&engine, &render_w, &theme);
                sync_scrollbar(&engine, &adj, &updating);
            }
        }
        glib::Propagation::Stop
    });
    pane.render.add_controller(scroll);
}

/// Forward pointer button press/release to the app when it requested mouse
/// reporting (vim/htop/tmux click handling). Claims the event so it does
/// not also start a local selection.
fn wire_mouse_report(pane: &TerminalPaneNative) {
    let click = gtk::GestureClick::new();
    click.set_button(0); // any button
    {
        let engine = pane.engine.clone();
        let render_w = pane.render.clone();
        click.connect_pressed(move |g, _n, x, y| {
            send_mouse_report(&engine, &render_w, g, x, y, true);
        });
    }
    {
        let engine = pane.engine.clone();
        let render_w = pane.render.clone();
        click.connect_released(move |g, _n, x, y| {
            send_mouse_report(&engine, &render_w, g, x, y, false);
        });
    }
    pane.render.add_controller(click);
}

fn send_mouse_report(
    engine: &Rc<RefCell<TermEngine>>,
    render_w: &TerminalRenderArea,
    gesture: &gtk::GestureClick,
    x: f64,
    y: f64,
    press: bool,
) {
    let e = engine.borrow();
    if !e.mouse_report() {
        return;
    }
    let Some((col, line, _)) = cell_at(render_w, x, y) else {
        return;
    };
    let btn: u8 = match gesture.current_button() {
        2 => 1, // middle
        3 => 2, // right
        _ => 0, // left / other
    };
    let (c, r) = (col as u32 + 1, line as u32 + 1);
    let seq = if e.sgr_mouse() {
        let kind = if press { 'M' } else { 'm' };
        format!("\x1b[<{btn};{c};{r}{kind}").into_bytes()
    } else {
        // Legacy X10: ESC [ M Cb Cx Cy. Release reports button 3.
        let cb = 32 + if press { btn } else { 3 };
        vec![
            0x1b,
            b'[',
            b'M',
            cb,
            32u8.saturating_add(c.min(223) as u8),
            32u8.saturating_add(r.min(223) as u8),
        ]
    };
    e.write(seq);
    gesture.set_state(gtk::EventSequenceState::Claimed);
}

/// Ctrl+left-click on a URL opens it in a browser tab (via `on_open_url`).
fn wire_url_click(pane: &TerminalPaneNative, callbacks: &PaneCallbacks) {
    let engine = pane.engine.clone();
    let render_w = pane.render.clone();
    let on_open_url = callbacks.on_open_url.clone();
    let id = pane.id;
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_PRIMARY);
    click.connect_pressed(move |gesture, _n, x, y| {
        if !gesture
            .current_event_state()
            .contains(gtk::gdk::ModifierType::CONTROL_MASK)
        {
            return;
        }
        let Some((col, line, _)) = cell_at(&render_w, x, y) else {
            return;
        };
        let row = engine.borrow().row_text(line);
        if let Some(url) = url_at(&row, col) {
            (on_open_url.borrow_mut())(id, url);
            gesture.set_state(gtk::EventSequenceState::Claimed);
        }
    });
    pane.render.add_controller(click);
}

/// Find a URL token in `row` that covers column `col`, trimmed of trailing
/// sentence punctuation.
fn url_at(row: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = row.chars().collect();
    if col >= chars.len() {
        return None;
    }
    let is_url_char = |c: char| !c.is_whitespace() && !matches!(c, '<' | '>' | '"' | '\'' | '`');
    let mut start = col;
    while start > 0 && is_url_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end < chars.len() && is_url_char(chars[end]) {
        end += 1;
    }
    let token: String = chars[start..end].iter().collect();
    let lower = token.to_ascii_lowercase();
    let has_scheme = ["http://", "https://", "ftp://", "file://"]
        .iter()
        .any(|s| lower.starts_with(s));
    if !has_scheme {
        return None;
    }
    let trimmed = token.trim_end_matches(|c: char| {
        matches!(
            c,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"' | '`'
        )
    });
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn wire_mouse_selection(pane: &TerminalPaneNative, callbacks: &PaneCallbacks) {
    let engine = pane.engine.clone();
    let render_w = pane.render.clone();
    let theme = pane.theme.clone();
    let on_focus = callbacks.on_focus.clone();
    let id = pane.id;

    // Press count for the in-flight gesture (1 = single, 2 = double, …).
    // A capture-phase GestureClick stamps it *before* the drag gesture's
    // `drag-begin` fires for the same press, so double-click can pick word
    // selection without the drag clobbering it with a fresh 1-cell anchor.
    let click_count = Rc::new(Cell::new(1u32));
    {
        let counter = gtk::GestureClick::new();
        counter.set_button(gtk::gdk::BUTTON_PRIMARY);
        counter.set_propagation_phase(gtk::PropagationPhase::Capture);
        let click_count = click_count.clone();
        counter.connect_pressed(move |_, n, _, _| click_count.set((n as u32).max(1)));
        pane.render.add_controller(counter);
    }

    let drag = gtk::GestureDrag::new();
    drag.connect_drag_begin({
        let engine = engine.clone();
        let render_w = render_w.clone();
        let theme = theme.clone();
        let click_count = click_count.clone();
        move |g, x, y| {
            render_w.grab_focus();
            (on_focus.borrow_mut())(id);
            // App wants the mouse → don't start a local selection.
            if engine.borrow().mouse_report() {
                return;
            }
            if let Some((col, line, right)) = cell_at(&render_w, x, y) {
                render_w.set_selecting(true);
                // Shift+click extends the existing selection from its anchor
                // instead of starting a fresh one; fall back to a new
                // selection if there is nothing to extend.
                let shift = g
                    .current_event_state()
                    .contains(gtk::gdk::ModifierType::SHIFT_MASK);
                if click_count.get() >= 2 {
                    // Double-click (or more): select the word under the cursor.
                    engine.borrow().selection_word(col, line);
                } else if shift && engine.borrow().has_selection() {
                    engine.borrow().selection_update(col, line, right);
                } else {
                    engine.borrow().selection_start(col, line, right);
                }
                repaint(&engine, &render_w, &theme);
            }
        }
    });
    drag.connect_drag_update({
        let engine = engine.clone();
        let render_w = render_w.clone();
        let theme = theme.clone();
        move |g, dx, dy| {
            if let Some((sx, sy)) = g.start_point() {
                if let Some((col, line, right)) = cell_at(&render_w, sx + dx, sy + dy) {
                    engine.borrow().selection_update(col, line, right);
                    repaint(&engine, &render_w, &theme);
                }
            }
        }
    });
    drag.connect_drag_end(move |_, _, _| {
        render_w.set_selecting(false);
        // A click (or a drag that never covered a cell) leaves an empty
        // selection; drop it so `has_selection()` / Copy stay accurate and no
        // stale highlight lingers.
        if !engine.borrow().has_selection() {
            engine.borrow().selection_clear();
        }
        repaint(&engine, &render_w, &theme);
    });
    pane.render.add_controller(drag);
}

fn wire_focus_and_menu(pane: &TerminalPaneNative, surface: SurfaceId, callbacks: &PaneCallbacks) {
    let id = pane.id;
    // Focus tracking.
    {
        let cb = callbacks.on_focus.clone();
        let focus = gtk::EventControllerFocus::new();
        focus.connect_enter(move |_| (cb.borrow_mut())(id));
        pane.render.add_controller(focus);
    }
    // Right-click menu: Copy / Paste / Split Right / Split Down / Copy path /
    // Close Pane.
    {
        let on_focus = callbacks.on_focus.clone();
        let on_split_right = callbacks.on_split_right.clone();
        let on_split_down = callbacks.on_split_down.clone();
        let on_close_pane = callbacks.on_close_pane.clone();
        let on_copy_text = callbacks.on_copy_surface_text.clone();
        let host = pane.render.clone();
        let engine = pane.engine.clone();
        let pane_menu = pane.clone();
        let click = gtk::GestureClick::new();
        click.set_button(gtk::gdk::BUTTON_SECONDARY);
        click.connect_pressed(move |gesture, _n, x, y| {
            (on_focus.borrow_mut())(id);
            // When the app wants the mouse, the right-click goes to it
            // (handled by wire_mouse_report); don't pop the local menu.
            if engine.borrow().mouse_report() {
                return;
            }
            let popover = gtk::Popover::new();
            let v = gtk::Box::new(gtk::Orientation::Vertical, 0);
            v.set_margin_top(4);
            v.set_margin_bottom(4);
            let mk = |label: &str| -> gtk::Button {
                let b = gtk::Button::with_label(label);
                b.add_css_class("flat");
                b.set_halign(gtk::Align::Fill);
                b.set_hexpand(true);
                if let Some(l) = b.child().and_downcast::<gtk::Label>() {
                    l.set_xalign(0.0);
                }
                b
            };
            // Copy / Paste mirror the keybindings: Copy puts the current
            // selection on the clipboard (no-op when nothing is selected),
            // Paste sends the clipboard text to the PTY.
            let copy = mk("Copy");
            let pop = popover.clone();
            let p = pane_menu.clone();
            copy.connect_clicked(move |_| {
                pop.popdown();
                p.copy_selection_to_clipboard();
            });
            v.append(&copy);
            let paste = mk("Paste");
            let pop = popover.clone();
            let p = pane_menu.clone();
            paste.connect_clicked(move |_| {
                pop.popdown();
                p.paste_clipboard();
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
                (cb.borrow_mut())(id, surface);
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
            popover.set_parent(&host);
            popover.set_has_arrow(false);
            crate::ui::popover_pos::anchor_at_click(&popover, &host, x, y);
            popover.connect_closed(|p| p.unparent());
            popover.popup();
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        pane.render.add_controller(click);
    }
}

fn wire_event_loop(
    pane: &TerminalPaneNative,
    surface: SurfaceId,
    rx: async_channel::Receiver<TermEvent>,
    callbacks: &PaneCallbacks,
) {
    let id = pane.id;
    let engine = pane.engine.clone();
    let render = pane.render.clone();
    let title_store = pane.title.clone();
    let theme = pane.theme.clone();
    let on_bell = callbacks.on_bell.clone();
    let on_child_exited = callbacks.on_child_exited.clone();
    let on_title_changed = callbacks.on_terminal_title_changed.clone();
    let on_title_notify = pane.on_title_notify.clone();
    let on_cwd_notify = pane.on_cwd_notify.clone();
    let pane_for_notify = pane.clone();
    let adj = pane.scroll_adj.clone();
    let updating = pane.adj_updating.clone();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(ev) = rx.recv().await {
            match ev {
                TermEvent::Wakeup => {
                    repaint(&engine, &render, &theme);
                    sync_scrollbar(&engine, &adj, &updating);
                }
                TermEvent::Title(t) => {
                    *title_store.borrow_mut() = t.clone();
                    (on_title_changed.borrow_mut())(id, surface, t.clone());
                    if let Some(f) = on_title_notify.borrow().as_ref() {
                        f((pane_for_notify.clone(), t));
                    }
                    if let Some(f) = on_cwd_notify.borrow().as_ref() {
                        f(pane_for_notify.clone());
                    }
                }
                TermEvent::ResetTitle => {
                    title_store.borrow_mut().clear();
                    (on_title_changed.borrow_mut())(id, surface, String::new());
                }
                TermEvent::Bell => (on_bell.borrow_mut())(id),
                TermEvent::ClipboardStore(s) => {
                    if let Some(display) = gtk::gdk::Display::default() {
                        display.clipboard().set_text(&s);
                    }
                }
                TermEvent::Exit => {
                    (on_child_exited.borrow_mut())(id, 0);
                    break;
                }
            }
        }
    });
}

/// Push the engine's scrollback position into the scrollbar adjustment.
/// Guarded so the programmatic update does not re-enter value-changed.
fn sync_scrollbar(
    engine: &Rc<RefCell<TermEngine>>,
    adj: &gtk::Adjustment,
    updating: &Rc<Cell<bool>>,
) {
    let (offset, history) = engine.borrow().scrollback_state();
    updating.set(true);
    adj.set_lower(0.0);
    adj.set_upper(history as f64);
    adj.set_page_size(0.0);
    adj.set_value((history as i32 - offset as i32).max(0) as f64);
    updating.set(false);
}

/// Lock the term, build a snapshot, repaint only the changed rows.
fn repaint(
    engine: &Rc<RefCell<TermEngine>>,
    render: &TerminalRenderArea,
    theme: &Rc<RefCell<ThemePalette>>,
) {
    let engine = engine.borrow();
    let term = engine.term().lock();
    let frame = render::snapshot(&*term, &theme.borrow());
    drop(term);
    render.set_frame(frame);
}

/// Map widget pixel `(x, y)` to a viewport `(col, line, right_half)`.
fn cell_at(render: &TerminalRenderArea, x: f64, y: f64) -> Option<(usize, usize, bool)> {
    let m = render.font_metrics()?;
    if m.cell_w <= 0.0 || m.cell_h <= 0.0 {
        return None;
    }
    let col_f = (x as f32 / m.cell_w).max(0.0);
    let line = (y as f32 / m.cell_h).max(0.0) as usize;
    let col = col_f as usize;
    let right = (col_f - col as f32) >= 0.5;
    Some((col, line, right))
}

/// GTK key → terminal byte encoding. Honors DECCKM (`app_cursor`) for the
/// arrow/Home/End keys so vim / tig / claude / codex get `ESC O A` vs
/// `ESC [ A`. Full kitty-keyboard / modifier-encoded function keys land
/// with the IME work in Phase 4.
fn encode_key(
    keyval: gtk::gdk::Key,
    state: gtk::gdk::ModifierType,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    use gtk::gdk::Key;
    let ctrl = state.contains(gtk::gdk::ModifierType::CONTROL_MASK);
    let alt = state.contains(gtk::gdk::ModifierType::ALT_MASK);

    if ctrl {
        if let Some(ch) = keyval.to_unicode() {
            if ch.is_ascii_alphabetic() {
                let c = (ch.to_ascii_uppercase() as u8) - b'@';
                return Some(maybe_alt(alt, vec![c]));
            }
        }
    }

    let csi = if app_cursor { b"\x1bO" } else { b"\x1b[" };
    let cursor = |tail: u8| {
        let mut v = csi.to_vec();
        v.push(tail);
        v
    };

    let bytes: Vec<u8> = match keyval {
        Key::Return | Key::KP_Enter => b"\r".to_vec(),
        Key::BackSpace => b"\x7f".to_vec(),
        Key::Tab => b"\t".to_vec(),
        Key::ISO_Left_Tab => b"\x1b[Z".to_vec(),
        Key::Escape => b"\x1b".to_vec(),
        Key::Up => cursor(b'A'),
        Key::Down => cursor(b'B'),
        Key::Right => cursor(b'C'),
        Key::Left => cursor(b'D'),
        Key::Home => cursor(b'H'),
        Key::End => cursor(b'F'),
        Key::Insert => b"\x1b[2~".to_vec(),
        Key::Delete => b"\x1b[3~".to_vec(),
        Key::Page_Up => b"\x1b[5~".to_vec(),
        Key::Page_Down => b"\x1b[6~".to_vec(),
        Key::F1 => b"\x1bOP".to_vec(),
        Key::F2 => b"\x1bOQ".to_vec(),
        Key::F3 => b"\x1bOR".to_vec(),
        Key::F4 => b"\x1bOS".to_vec(),
        Key::F5 => b"\x1b[15~".to_vec(),
        Key::F6 => b"\x1b[17~".to_vec(),
        Key::F7 => b"\x1b[18~".to_vec(),
        Key::F8 => b"\x1b[19~".to_vec(),
        Key::F9 => b"\x1b[20~".to_vec(),
        Key::F10 => b"\x1b[21~".to_vec(),
        Key::F11 => b"\x1b[23~".to_vec(),
        Key::F12 => b"\x1b[24~".to_vec(),
        _ => {
            if let Some(ch) = keyval.to_unicode() {
                if !ch.is_control() {
                    let mut buf = [0u8; 4];
                    return Some(maybe_alt(alt, ch.encode_utf8(&mut buf).as_bytes().to_vec()));
                }
            }
            return None;
        }
    };
    Some(maybe_alt(alt, bytes))
}

fn maybe_alt(alt: bool, mut bytes: Vec<u8>) -> Vec<u8> {
    if alt {
        let mut v = Vec::with_capacity(bytes.len() + 1);
        v.push(0x1b);
        v.append(&mut bytes);
        v
    } else {
        bytes
    }
}

#[cfg(test)]
mod paste_tests {
    use super::bracketed_paste_payload;

    #[test]
    fn wraps_in_markers_when_bracketed() {
        assert_eq!(
            bracketed_paste_payload("ls -la", true),
            b"\x1b[200~ls -la\x1b[201~".to_vec()
        );
    }

    #[test]
    fn raw_bytes_when_not_bracketed() {
        assert_eq!(bracketed_paste_payload("ls -la", false), b"ls -la".to_vec());
    }

    #[test]
    fn preserves_utf8_and_newlines() {
        let text = "에코\n안녕";
        assert_eq!(
            bracketed_paste_payload(text, false),
            text.as_bytes().to_vec()
        );
        let wrapped = bracketed_paste_payload(text, true);
        assert!(wrapped.starts_with(b"\x1b[200~"));
        assert!(wrapped.ends_with(b"\x1b[201~"));
        assert_eq!(&wrapped[6..wrapped.len() - 6], text.as_bytes());
    }

    #[test]
    fn empty_text() {
        assert_eq!(bracketed_paste_payload("", false), Vec::<u8>::new());
        assert_eq!(
            bracketed_paste_payload("", true),
            b"\x1b[200~\x1b[201~".to_vec()
        );
    }
}

#[cfg(test)]
mod url_tests {
    use super::url_at;

    #[test]
    fn detects_url_under_column_and_trims_punct() {
        let row = "see https://example.com/path, ok";
        // column inside the URL
        assert_eq!(url_at(row, 10).as_deref(), Some("https://example.com/path"));
        // trailing comma trimmed
        assert!(!url_at(row, 10).unwrap().ends_with(','));
        // column outside any URL
        assert_eq!(url_at(row, 0), None);
        // non-url token
        assert_eq!(url_at("just words here", 2), None);
    }
}
