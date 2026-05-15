// SPDX-License-Identifier: GPL-3.0-or-later
//! libghostty-vt backed terminal pane.
//!
//! The pane owns a real PTY child, feeds child output into libghostty's VT
//! state machine, and renders the visible viewport with GTK. This replaces
//! the former external terminal widget dependency while keeping the public `TerminalPane`
//! surface used by the rest of flowmux.

use crate::ui::terminal_surface::TerminalSurface;
use flowmux_core::{PaneId, SurfaceId};
use gtk::glib;
use gtk::graphene;
use gtk::prelude::*;
use libghostty_vt::key::{
    Action as KeyAction, Encoder as KeyEncoder, Event as KeyEvent, Key as GhosttyKey,
    Mods as GhosttyMods,
};
use libghostty_vt::mouse::{
    Action as MouseAction, Button as MouseButton, Encoder as MouseEncoder,
    EncoderSize as MouseEncoderSize, Event as MouseEvent, Position as MousePosition,
};
use libghostty_vt::render::{CellIterator, CursorVisualStyle, RenderState, RowIterator};
use libghostty_vt::screen::{CellContentTag, CellWide};
use libghostty_vt::style::{RgbColor, Style, StyleColor};
use libghostty_vt::terminal::{Mode, ScrollViewport};
use libghostty_vt::{Terminal, TerminalOptions};
use nix::pty::Winsize;
#[cfg(not(test))]
use nix::pty::{forkpty, ForkptyResult};
use nix::unistd::Pid;
use std::cell::{Cell, RefCell};
#[cfg(not(test))]
use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::rc::{Rc, Weak};

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
const MAX_SCROLLBACK: usize = 20_000;
const DEFAULT_CELL_WIDTH: f64 = 8.0;
const DEFAULT_CELL_HEIGHT: f64 = 17.0;

#[derive(Clone)]
pub struct TerminalPane {
    pub id: PaneId,
    /// Root GTK widget for this terminal surface. The object identity is
    /// stable across split tree changes so the live PTY child survives pane
    /// reparenting.
    pub widget: TerminalSurface,
    /// PID of the spawned shell or pty-tee wrapper.
    pub pid: Rc<Cell<Option<i32>>>,
    runtime: Rc<TerminalRuntime>,
}

struct TerminalRuntime {
    id: PaneId,
    widget: TerminalSurface,
    /// `gtk::Overlay` whose child is the `TerminalSurface` and whose
    /// overlay is the vertical scrollback scrollbar pinned to the right
    /// edge. Using an Overlay keeps the surface's allocation identical
    /// to what it was before the scrollbar existed, which matters
    /// because some GTK/compositor combinations on the desktop render
    /// the inner widget as fully transparent when it is wrapped in a
    /// horizontal `gtk::Box` instead.
    container: gtk::Overlay,
    /// Drives the scrollback scrollbar. Mirrors libghostty's viewport
    /// state — see [`TerminalRuntime::refresh_scrollbar`].
    scroll_adjustment: gtk::Adjustment,
    im_context: gtk::IMMulticontext,
    /// Single `pango::Layout` reused across every snapshot so heavy
    /// TUIs (tig, opencode, vim, htop) avoid re-fetching the
    /// PangoContext from the widget on every frame. Pango Layouts are
    /// glib Objects; cloning shares the same instance via refcount,
    /// which is what we want — the layout's font/text are mutated in
    /// place between cells inside `snapshot_terminal`.
    pango_layout: gtk::pango::Layout,
    master: OwnedFd,
    pid: Rc<Cell<Option<i32>>>,
    state: RefCell<TerminalState>,
    io_source: RefCell<Option<glib::SourceId>>,
    child_source: RefCell<Option<glib::SourceId>>,
    title_handlers: RefCell<Vec<TitleHandler>>,
    cwd_handlers: RefCell<Vec<CwdHandler>>,
}

struct TerminalState {
    terminal: Terminal<'static, 'static>,
    render: RenderState<'static>,
    key_encoder: KeyEncoder<'static>,
    /// Lazy mouse-event encoder reused across all scroll/click events.
    /// Allocating a new encoder per event would burn memory on every
    /// scroll wheel notch; reusing keeps the hot path allocation-free.
    mouse_encoder: MouseEncoder<'static>,
    /// Pointer position in surface-space pixels. Updated by the motion
    /// controller installed in `install_scroll_input` so the scroll
    /// handler can place mouse-tracking wheel events at the right cell
    /// when forwarding them to apps that opted into reporting (tig,
    /// htop, opencode, fzf, …). The mouse encoder converts pixel
    /// coordinates into protocol-specific cell coordinates internally.
    pointer_px: (f32, f32),
    metrics: CellMetrics,
    visuals: TerminalVisuals,
    last_title: String,
    last_pwd: String,
    selection: Option<SelectionRange>,
}

type TitleHandler = Box<dyn Fn(&TerminalPane, String) + 'static>;
type CwdHandler = Box<dyn Fn(&TerminalPane) + 'static>;

#[derive(Clone)]
struct CellMetrics {
    width: f64,
    height: f64,
    baseline: f64,
    cols: u16,
    rows: u16,
    font: gtk::pango::FontDescription,
    font_scale: f64,
}

#[derive(Clone)]
struct TerminalVisuals {
    font: gtk::pango::FontDescription,
    bg: gtk::gdk::RGBA,
    fg: gtk::gdk::RGBA,
    cursor: gtk::gdk::RGBA,
    selection_bg: Option<gtk::gdk::RGBA>,
    selection_fg: Option<gtk::gdk::RGBA>,
    palette: [gtk::gdk::RGBA; 16],
}

#[derive(Clone, Copy)]
struct GridPoint {
    col: u16,
    row: u16,
}

#[derive(Clone, Copy)]
struct SelectionRange {
    anchor: GridPoint,
    focus: GridPoint,
}

impl TerminalPane {
    /// Best-effort current working directory of the shell.
    ///
    /// Preference order:
    ///   1. OSC 7 parsed by libghostty (`file://...` URI).
    ///   2. `/proc/<pid>/cwd` symlink target.
    pub fn current_dir(&self) -> Option<PathBuf> {
        let pwd = self.runtime.state.borrow().last_pwd.clone();
        if !pwd.is_empty() {
            if let Some(p) = uri_to_path(&pwd) {
                return Some(p);
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
        self.runtime.container.clone().upcast::<gtk::Widget>()
    }

    pub fn grab_focus(&self) {
        self.widget.grab_focus();
    }

    pub fn set_font_scale(&self, scale: f64) {
        let mut state = self.runtime.state.borrow_mut();
        state.metrics.font_scale = scale.clamp(0.1, 2.0);
        state.metrics.font = scaled_font(&state.visuals.font, state.metrics.font_scale);
        drop(state);
        self.recompute_metrics_and_resize();
        self.widget.queue_draw();
    }

    pub fn apply_theme(&self, theme: &crate::theme::ResolvedTheme) {
        let mut palette = [gtk::gdk::RGBA::BLACK; 16];
        for (idx, color) in theme.palette.iter().take(16).enumerate() {
            palette[idx] = *color;
        }
        let mut state = self.runtime.state.borrow_mut();
        state.visuals = TerminalVisuals {
            font: theme.font.clone(),
            bg: theme.bg,
            fg: theme.fg,
            cursor: theme.cursor,
            selection_bg: theme.selection_bg,
            selection_fg: theme.selection_fg,
            palette,
        };
        state.metrics.font = scaled_font(&state.visuals.font, state.metrics.font_scale);
        drop(state);
        self.recompute_metrics_and_resize();
        self.widget.queue_draw();
    }

    pub fn has_selection(&self) -> bool {
        self.runtime.state.borrow().selection.is_some()
    }

    pub fn copy_selection_to_clipboard(&self) {
        let text = selected_text(&mut self.runtime.state.borrow_mut());
        if text.is_empty() {
            return;
        }
        if let Some(display) = gtk::gdk::Display::default() {
            display.clipboard().set_text(&text);
        }
    }

    pub fn paste_clipboard(&self) {
        let Some(display) = gtk::gdk::Display::default() else {
            return;
        };
        let weak = Rc::downgrade(&self.runtime);
        display
            .clipboard()
            .read_text_async(gtk::gio::Cancellable::NONE, move |res| {
                let Some(runtime) = weak.upgrade() else {
                    return;
                };
                match res {
                    Ok(Some(text)) => runtime.write_child(text.as_bytes()),
                    Ok(None) => {}
                    Err(e) => tracing::warn!(error = %e, "clipboard read failed"),
                }
            });
    }

    /// Build a fresh terminal widget and spawn `argv` in `cwd`. If
    /// `argv` is empty we fall back to the user's `$SHELL`.
    pub fn spawn(
        id: PaneId,
        surface: SurfaceId,
        argv: Vec<String>,
        cwd: Option<std::path::PathBuf>,
        extra_env: Vec<(String, String)>,
        callbacks: PaneCallbacks,
    ) -> Self {
        let _unused_notification_cb = &callbacks.on_notification;
        let widget = TerminalSurface::new();
        widget.set_hexpand(true);
        widget.set_vexpand(true);
        widget.set_focusable(true);
        widget.add_css_class("flowmux-terminal");
        widget.set_size_request(
            (DEFAULT_COLS as f64 * DEFAULT_CELL_WIDTH).ceil() as i32,
            (DEFAULT_ROWS as f64 * DEFAULT_CELL_HEIGHT).ceil() as i32,
        );

        let argv = if argv.is_empty() {
            default_shell_argv()
        } else {
            argv
        };
        let argv = wrap_argv_with_pty_tee(argv, id, surface);
        let extra_env = terminal_child_env(extra_env);

        let (master, child_pid) = spawn_terminal_endpoint(&argv, cwd.as_deref(), &extra_env)
            .unwrap_or_else(|e| panic!("failed to spawn terminal child: {e}"));
        set_nonblocking(master.as_raw_fd());

        let pid: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(child_pid.map(|pid| pid.as_raw())));

        let terminal = Terminal::new(TerminalOptions {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            max_scrollback: MAX_SCROLLBACK,
        })
        .expect("libghostty terminal should initialize");

        let master_for_pty = master.as_raw_fd();

        let state = TerminalState {
            terminal,
            render: RenderState::new().expect("libghostty render state should initialize"),
            key_encoder: KeyEncoder::new().expect("libghostty key encoder should initialize"),
            mouse_encoder: MouseEncoder::new().expect("libghostty mouse encoder should initialize"),
            pointer_px: (0.0, 0.0),
            metrics: CellMetrics {
                width: DEFAULT_CELL_WIDTH,
                height: DEFAULT_CELL_HEIGHT,
                baseline: 13.0,
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
                font: gtk::pango::FontDescription::from_string("monospace 12"),
                font_scale: 1.0,
            },
            visuals: TerminalVisuals::default(),
            last_title: String::new(),
            last_pwd: String::new(),
            selection: None,
        };

        let im_context = gtk::IMMulticontext::new();
        im_context.set_use_preedit(true);

        let pango_layout = widget.create_pango_layout(None::<&str>);
        pango_layout.set_font_description(Some(&state.metrics.font));

        // Wrap the terminal surface in a `gtk::Overlay` so we can pin
        // a vertical scrollback scrollbar to the right edge without
        // changing the surface's allocation. The Overlay becomes the
        // pane's root widget while the surface continues to own every
        // controller (keyboard, focus, snapshot, selection, …). An
        // earlier attempt that used a horizontal `gtk::Box` rendered
        // the surface fully transparent on some desktop GTK builds,
        // so the Overlay layout is now the only path.
        let container = gtk::Overlay::new();
        container.set_hexpand(true);
        container.set_vexpand(true);
        container.set_child(Some(&widget));

        let scroll_adjustment = gtk::Adjustment::new(
            0.0,
            0.0,
            DEFAULT_ROWS as f64,
            1.0,
            DEFAULT_ROWS as f64,
            DEFAULT_ROWS as f64,
        );
        let scrollbar = gtk::Scrollbar::new(gtk::Orientation::Vertical, Some(&scroll_adjustment));
        scrollbar.set_vexpand(true);
        scrollbar.set_halign(gtk::Align::End);
        scrollbar.set_valign(gtk::Align::Fill);
        container.add_overlay(&scrollbar);

        let runtime = Rc::new(TerminalRuntime {
            id,
            widget: widget.clone(),
            container: container.clone(),
            scroll_adjustment: scroll_adjustment.clone(),
            im_context: im_context.clone(),
            pango_layout: pango_layout.clone(),
            master,
            pid: pid.clone(),
            state: RefCell::new(state),
            io_source: RefCell::new(None),
            child_source: RefCell::new(None),
            title_handlers: RefCell::new(Vec::new()),
            cwd_handlers: RefCell::new(Vec::new()),
        });

        // Register libghostty callbacks only after the Terminal lives at a
        // stable heap address inside the Rc. libghostty-vt stores
        // `&terminal.vtable` as the C-side userdata pointer; if we
        // registered before this point the pointer would dangle as soon
        // as Terminal was moved into TerminalState / RefCell / Rc, and
        // the next pty-write or bell callback would dereference freed
        // stack memory.
        {
            let mut state = runtime.state.borrow_mut();
            state
                .terminal
                .on_pty_write(move |_term, data| {
                    write_fd(master_for_pty, data);
                })
                .expect("libghostty pty callback should install");
            let bell_cb = callbacks.on_bell.clone();
            state
                .terminal
                .on_bell(move |_term| {
                    (bell_cb.borrow_mut())(id);
                })
                .expect("libghostty bell callback should install");
        }

        {
            let weak = Rc::downgrade(&runtime);
            im_context.connect_commit(move |_, text| {
                let Some(rt) = weak.upgrade() else {
                    return;
                };
                rt.write_child(text.as_bytes());
            });
        }

        // Connect the scrollbar adjustment to libghostty's viewport.
        // Skip the call when `target == current` so the `configure`
        // calls in `refresh_scrollbar` cannot re-enter and re-scroll.
        {
            let weak = Rc::downgrade(&runtime);
            scroll_adjustment.connect_value_changed(move |adj| {
                let Some(runtime) = weak.upgrade() else {
                    return;
                };
                let current_offset = {
                    let state = runtime.state.borrow();
                    match state.terminal.scrollbar() {
                        Ok(sb) => sb.offset,
                        Err(_) => return,
                    }
                };
                let target = adj.value().round() as i64;
                let delta = target - current_offset as i64;
                if delta == 0 {
                    return;
                }
                let clamped = delta.clamp(isize::MIN as i64, isize::MAX as i64) as isize;
                {
                    let mut state = runtime.state.borrow_mut();
                    state
                        .terminal
                        .scroll_viewport(ScrollViewport::Delta(clamped));
                }
                runtime.widget.queue_draw();
                // Snap the adjustment back to libghostty's actual offset
                // in case the delta was clamped (e.g. dragging past the
                // scrollback top or bottom). The handler's identity
                // guard (`delta == 0`) prevents the configure call from
                // re-entering — the second pass reads the new offset
                // and sees it equal to `target`.
                runtime.refresh_scrollbar();
            });
        }

        let pane = Self {
            id,
            widget: widget.clone(),
            pid,
            runtime,
        };

        install_draw_func(&pane);
        install_resize_handler(&pane);
        install_focus_handler(&pane, callbacks.on_focus.clone());
        install_context_menu(
            &pane,
            callbacks.on_split_right.clone(),
            callbacks.on_split_down.clone(),
            callbacks.on_close_pane.clone(),
        );
        install_key_input(&pane);
        install_scroll_input(&pane);
        install_selection(&pane);
        install_url_click(&pane, callbacks.on_open_url.clone());
        install_io_watch(&pane, callbacks.on_child_exited.clone());

        pane
    }

    pub fn feed(&self, bytes: &[u8]) {
        self.runtime.write_child(bytes);
    }

    pub fn add_controller(&self, controller: impl IsA<gtk::EventController>) {
        self.widget.add_controller(controller);
    }

    pub fn connect_current_dir_notify(&self, callback: impl Fn(&Self) + Clone + 'static) {
        let pane = self.clone();
        self.runtime
            .cwd_handlers
            .borrow_mut()
            .push(Box::new(move |_| callback(&pane)));
    }

    pub fn connect_title_notify(&self, callback: impl Fn(&Self, String) + Clone + 'static) {
        let pane = self.clone();
        self.runtime
            .title_handlers
            .borrow_mut()
            .push(Box::new(move |_, title| callback(&pane, title)));
    }

    fn recompute_metrics_and_resize(&self) {
        let layout = self.widget.create_pango_layout(Some("W"));
        let mut state = self.runtime.state.borrow_mut();
        layout.set_font_description(Some(&state.metrics.font));
        // Read the layout's size in Pango units (1/1024 of a pixel) so
        // the cell width keeps the font's actual advance with sub-pixel
        // precision. `pixel_size()` rounds to whole pixels, which made
        // text-run batching drift one fractional pixel per cell — the
        // accumulated drift was visible as wider-than-expected gaps
        // inside a run of characters.
        let (w_units, h_units) = layout.size();
        let scale = gtk::pango::SCALE as f64;
        let width = (w_units as f64 / scale).max(DEFAULT_CELL_WIDTH);
        let height = (h_units as f64 / scale).max(DEFAULT_CELL_HEIGHT);
        state.metrics.width = width;
        state.metrics.height = height;
        state.metrics.baseline = (height * 0.78).round();
        let alloc = self.widget.allocation();
        resize_state_to_pixels(&self.runtime, &mut state, alloc.width(), alloc.height());
    }
}

impl TerminalRuntime {
    fn write_child(&self, bytes: &[u8]) {
        // Mirror common terminal behavior: any input destined for the
        // PTY snaps the viewport back to the live cursor row, so users
        // who scrolled up to inspect history are not silently typing
        // off-screen. Skips when the viewport is already at the bottom
        // to avoid a redundant redraw on every keystroke.
        self.snap_viewport_to_bottom();
        write_fd(self.master.as_raw_fd(), bytes);
    }

    fn snap_viewport_to_bottom(&self) {
        let already_at_bottom = {
            let state = self.state.borrow();
            match state.terminal.scrollbar() {
                Ok(sb) => sb.offset + sb.len >= sb.total,
                Err(_) => return,
            }
        };
        if already_at_bottom {
            return;
        }
        {
            let mut state = self.state.borrow_mut();
            state.terminal.scroll_viewport(ScrollViewport::Bottom);
        }
        self.widget.queue_draw();
        self.refresh_scrollbar();
    }

    /// Resync the scrollbar adjustment from libghostty's current viewport.
    ///
    /// `terminal.scrollbar()` reports `{total, offset, len}` in rows: the
    /// total scrollable area, the current top-of-viewport row, and the
    /// visible window. Map those to the GTK adjustment so the scrollbar
    /// thumb position matches the rendered viewport. Borrow the state
    /// read-only inside a tight scope so the `configure` call below
    /// happens with no borrow held — its `value-changed` handler needs
    /// a mutable borrow of `state` to scroll.
    fn refresh_scrollbar(&self) {
        let (offset, total, len) = {
            let state = self.state.borrow();
            match state.terminal.scrollbar() {
                Ok(sb) => (sb.offset as f64, sb.total as f64, sb.len as f64),
                Err(_) => return,
            }
        };
        let page = len.max(1.0);
        let upper = total.max(page);
        self.scroll_adjustment
            .configure(offset, 0.0, upper, 1.0, page, page);
    }

    /// Feed one PTY chunk into the VT state machine. Cheap hot path —
    /// nothing here issues GTK redraws or fires title/pwd handlers, so
    /// alt-screen TUIs that flood the PTY with many small chunks per
    /// frame (tig, htop, vim) only pay for the libghostty parser per
    /// chunk. Pair every call with a later [`Self::finalize_batch`]
    /// that runs the post-batch work exactly once.
    fn feed_bytes(self: &Rc<Self>, bytes: &[u8]) {
        let mut state = self.state.borrow_mut();
        state.terminal.vt_write(bytes);
    }

    /// Post-batch work after the io watch drains every available chunk:
    /// check title/pwd transitions, fire registered handlers, and queue
    /// one widget redraw. Title and pwd are compared as `&str` first so
    /// the common case (no change) avoids any String allocation.
    fn finalize_batch(self: &Rc<Self>) {
        let mut state = self.state.borrow_mut();
        let title_str = state.terminal.title().unwrap_or_default();
        let title_changed = title_str != state.last_title;
        let new_title: Option<String> = if title_changed {
            let owned = title_str.to_string();
            state.last_title = owned.clone();
            Some(owned)
        } else {
            None
        };
        let pwd_str = state.terminal.pwd().unwrap_or_default();
        let pwd_changed = pwd_str != state.last_pwd;
        if pwd_changed {
            state.last_pwd = pwd_str.to_string();
        }
        drop(state);

        if let Some(title) = new_title {
            let pane = self.pane_for_callbacks();
            for handler in self.title_handlers.borrow().iter() {
                handler(&pane, title.clone());
            }
        }
        if pwd_changed {
            let pane = self.pane_for_callbacks();
            for handler in self.cwd_handlers.borrow().iter() {
                handler(&pane);
            }
        }
        self.widget.queue_draw();
    }

    fn pane_for_callbacks(self: &Rc<Self>) -> TerminalPane {
        TerminalPane {
            id: self.id,
            widget: self.widget.clone(),
            pid: self.pid.clone(),
            runtime: self.clone(),
        }
    }
}

impl Drop for TerminalRuntime {
    fn drop(&mut self) {
        if let Some(source) = self.io_source.borrow_mut().take() {
            source.remove();
        }
        if let Some(source) = self.child_source.borrow_mut().take() {
            source.remove();
        }
        if let Some(pid) = self.pid.get() {
            unsafe {
                libc::kill(pid, libc::SIGHUP);
            }
        }
    }
}

impl TerminalVisuals {
    fn rgba_from_rgb(color: RgbColor) -> gtk::gdk::RGBA {
        gtk::gdk::RGBA::new(
            color.r as f32 / 255.0,
            color.g as f32 / 255.0,
            color.b as f32 / 255.0,
            1.0,
        )
    }

    fn fg_for_style(&self, style: Style) -> gtk::gdk::RGBA {
        resolve_style_color(style.fg_color, &self.palette).unwrap_or(self.fg)
    }

    fn bg_for_style(&self, style: Style) -> gtk::gdk::RGBA {
        resolve_style_color(style.bg_color, &self.palette).unwrap_or(self.bg)
    }
}

impl Default for TerminalVisuals {
    fn default() -> Self {
        let palette = [
            rgba("#1d1f21"),
            rgba("#cc6666"),
            rgba("#b5bd68"),
            rgba("#f0c674"),
            rgba("#81a2be"),
            rgba("#b294bb"),
            rgba("#8abeb7"),
            rgba("#c5c8c6"),
            rgba("#666666"),
            rgba("#d54e53"),
            rgba("#b9ca4a"),
            rgba("#e7c547"),
            rgba("#7aa6da"),
            rgba("#c397d8"),
            rgba("#70c0b1"),
            rgba("#eaeaea"),
        ];
        Self {
            font: gtk::pango::FontDescription::from_string("monospace 12"),
            bg: rgba("#282c34"),
            fg: rgba("#ffffff"),
            cursor: rgba("#ffffff"),
            selection_bg: None,
            selection_fg: None,
            palette,
        }
    }
}

fn install_draw_func(pane: &TerminalPane) {
    let weak = Rc::downgrade(&pane.runtime);
    pane.widget.set_snapshot_fn(move |snapshot, widget| {
        let Some(runtime) = weak.upgrade() else {
            return;
        };
        snapshot_terminal(&runtime, widget, snapshot);
    });
}

fn snapshot_terminal(
    runtime: &Rc<TerminalRuntime>,
    widget: &gtk::Widget,
    snapshot: &gtk::Snapshot,
) {
    let mut state = runtime.state.borrow_mut();
    let visuals = state.visuals.clone();
    let metrics = state.metrics.clone();
    let selection = state.selection;
    // Reuse the cached layout instead of asking the widget for a new
    // one every frame. The font description is refreshed in case it
    // changed since the previous snapshot (zoom / theme switch).
    let layout = runtime.pango_layout.clone();
    layout.set_font_description(Some(&metrics.font));

    // Whole-widget background. The Snapshot pipeline does not auto-
    // clear because each frame is composited against the parent's
    // node tree, so we still emit a single ColorNode covering the
    // allocation before any per-cell fill is appended on top.
    let alloc_w = widget.width() as f32;
    let alloc_h = widget.height() as f32;
    snapshot.append_color(
        &visuals.bg,
        &graphene::Rect::new(0.0, 0.0, alloc_w, alloc_h),
    );

    let TerminalState {
        terminal, render, ..
    } = &mut *state;
    let Ok(render_snapshot) = render.update(terminal) else {
        return;
    };

    let mut row_iter = match RowIterator::new() {
        Ok(iter) => iter,
        Err(e) => {
            tracing::warn!(error = ?e, "failed to create ghostty row iterator");
            return;
        }
    };
    let mut rows = match row_iter.update(&render_snapshot) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = ?e, "failed to read ghostty rows");
            return;
        }
    };

    // Reused across rows so the per-draw allocation cost is one Vec
    // rather than one per row. Each entry caches the painted cell's
    // computed colors and grapheme cluster so the row can be drawn in
    // two passes (background fills first, glyphs second) without
    // re-iterating libghostty's cell stream.
    let mut row_buf: Vec<RowCell> = Vec::with_capacity(metrics.cols as usize);
    let mut y = 0u16;
    while let Some(row) = rows.next() {
        let mut cell_iter = match CellIterator::new() {
            Ok(iter) => iter,
            Err(_) => break,
        };
        let mut cells = match cell_iter.update(row) {
            Ok(cells) => cells,
            Err(_) => break,
        };
        row_buf.clear();
        let mut x = 0u16;
        while let Some(cell) = cells.next() {
            let raw = cell.raw_cell().ok();
            let style = cell.style().unwrap_or_default();
            let mut fg = visuals.fg_for_style(style);
            let mut bg = cell_background(raw, style, &visuals)
                .unwrap_or_else(|| visuals.bg_for_style(style));
            if style.inverse {
                std::mem::swap(&mut fg, &mut bg);
            }
            if selection_contains(selection, x, y) {
                bg = visuals
                    .selection_bg
                    .unwrap_or_else(|| blend_rgba(&visuals.fg, &visuals.bg, 0.28));
                fg = visuals.selection_fg.unwrap_or(visuals.fg);
            }

            // Wide cells (e.g. Hangul) span two columns: the head holds
            // the grapheme and renders its glyph across both, the tail
            // acts as a placeholder. The bg run extends two columns at
            // the head, and the spacer tail is skipped so it does not
            // paint over the right half of the head's glyph.
            let wide = raw.map(|c| c.wide().ok()).flatten();
            let is_spacer_tail = matches!(wide, Some(CellWide::SpacerTail));
            let cell_span: u16 = if matches!(wide, Some(CellWide::Wide)) {
                2
            } else {
                1
            };

            let graphemes = cell.graphemes().unwrap_or_default();
            let text: Option<String> = if is_spacer_tail || graphemes.is_empty() {
                None
            } else {
                Some(graphemes.into_iter().collect())
            };

            row_buf.push(RowCell {
                x,
                span: cell_span,
                fg,
                bg,
                text,
                is_spacer_tail,
            });
            x = x.saturating_add(1);
        }

        // Pass 1: background fills. Contiguous non-default-bg cells are
        // merged into a single ColorNode so a styled line that used to
        // be one fill per cell becomes one node per color change.
        // Spacer tails are skipped — the preceding wide head already
        // extended the run two columns to cover them.
        let mut bg_run: Option<(u16, u16, gtk::gdk::RGBA)> = None;
        for info in &row_buf {
            if info.is_spacer_tail {
                continue;
            }
            if info.bg == visuals.bg {
                if let Some((start, end, color)) = bg_run.take() {
                    append_bg_run(snapshot, &metrics, start, end, y, &color);
                }
                continue;
            }
            match bg_run {
                Some(ref mut run) if run.1 == info.x && run.2 == info.bg => {
                    run.1 = info.x.saturating_add(info.span);
                }
                _ => {
                    if let Some((start, end, color)) = bg_run.take() {
                        append_bg_run(snapshot, &metrics, start, end, y, &color);
                    }
                    bg_run = Some((info.x, info.x.saturating_add(info.span), info.bg));
                }
            }
        }
        if let Some((start, end, color)) = bg_run.take() {
            append_bg_run(snapshot, &metrics, start, end, y, &color);
        }

        // Pass 2: glyphs. Appending text after every background node
        // for the row guarantees no later flush paints on top of an
        // already-rendered grapheme — the structural reason we now
        // use Snapshot rather than cairo's z-ordered command stream.
        //
        // Contiguous narrow cells sharing the same foreground color
        // are merged into a single TextNode so one Pango shape +
        // append_layout serves the whole run. Wide cells (CJK and
        // similar) stay per-cell because batching them across cells
        // would defeat the per-column anchoring that keeps the
        // grapheme glyph aligned to the grid. Empty cells break the
        // run — a hard break is cheaper than padding the layout text
        // with trailing spaces and keeps cell positions exact.
        let mut text_run: Option<(u16, gtk::gdk::RGBA, String)> = None;
        for info in &row_buf {
            if info.is_spacer_tail {
                continue;
            }
            if info.span > 1 {
                if let Some((start_x, fg, text)) = text_run.take() {
                    flush_text_run(snapshot, &layout, &metrics, y, start_x, &fg, &text);
                }
                if let Some(text) = info.text.as_ref() {
                    flush_text_run(snapshot, &layout, &metrics, y, info.x, &info.fg, text);
                }
                continue;
            }
            let Some(cell_text) = info.text.as_deref() else {
                if let Some((start_x, fg, text)) = text_run.take() {
                    flush_text_run(snapshot, &layout, &metrics, y, start_x, &fg, &text);
                }
                continue;
            };
            match text_run.as_mut() {
                Some((start_x, fg, buf))
                    if *fg == info.fg
                        && (*start_x as usize + buf.chars().count()) == info.x as usize =>
                {
                    buf.push_str(cell_text);
                }
                _ => {
                    if let Some((start_x, fg, text)) = text_run.take() {
                        flush_text_run(snapshot, &layout, &metrics, y, start_x, &fg, &text);
                    }
                    text_run = Some((info.x, info.fg, cell_text.to_string()));
                }
            }
        }
        if let Some((start_x, fg, text)) = text_run.take() {
            flush_text_run(snapshot, &layout, &metrics, y, start_x, &fg, &text);
        }

        y = y.saturating_add(1);
    }

    if render_snapshot.cursor_visible().unwrap_or(false) {
        if let Ok(Some(cursor)) = render_snapshot.cursor_viewport() {
            let cx = cursor.x as f64 * metrics.width;
            let cy = cursor.y as f64 * metrics.height;
            let cursor_color = render_snapshot
                .cursor_color()
                .ok()
                .flatten()
                .map(TerminalVisuals::rgba_from_rgb)
                .unwrap_or(visuals.cursor);
            let style = render_snapshot
                .cursor_visual_style()
                .unwrap_or(CursorVisualStyle::Block);
            append_cursor(
                snapshot,
                &metrics,
                cx as f32,
                cy as f32,
                style,
                &cursor_color,
            );
        }
    }
}

fn install_resize_handler(pane: &TerminalPane) {
    let weak = Rc::downgrade(&pane.runtime);
    pane.widget.connect_resize(move |width, height| {
        let Some(runtime) = weak.upgrade() else {
            return;
        };
        {
            let mut state = runtime.state.borrow_mut();
            resize_state_to_pixels(&runtime, &mut state, width, height);
        }
        runtime.refresh_scrollbar();
    });
}

fn resize_state_to_pixels(
    runtime: &TerminalRuntime,
    state: &mut TerminalState,
    width: i32,
    height: i32,
) {
    let cols = ((width.max(1) as f64) / state.metrics.width)
        .floor()
        .max(1.0) as u16;
    let rows = ((height.max(1) as f64) / state.metrics.height)
        .floor()
        .max(1.0) as u16;
    if cols == state.metrics.cols && rows == state.metrics.rows {
        return;
    }
    state.metrics.cols = cols;
    state.metrics.rows = rows;
    let _ = state.terminal.resize(
        cols,
        rows,
        state.metrics.width.ceil() as u32,
        state.metrics.height.ceil() as u32,
    );
    set_pty_winsize(runtime.master.as_raw_fd(), rows, cols);
    runtime.widget.queue_draw();
}

fn install_focus_handler(pane: &TerminalPane, on_focus: Rc<RefCell<dyn FnMut(PaneId)>>) {
    let id = pane.id;
    let focus_ctrl = gtk::EventControllerFocus::new();
    let im_in = pane.runtime.im_context.clone();
    focus_ctrl.connect_enter(move |_| {
        im_in.focus_in();
        (on_focus.borrow_mut())(id);
    });
    let im_out = pane.runtime.im_context.clone();
    focus_ctrl.connect_leave(move |_| {
        im_out.focus_out();
    });
    pane.widget.add_controller(focus_ctrl);
}

fn install_context_menu(
    pane: &TerminalPane,
    on_split_right: Rc<RefCell<dyn FnMut(PaneId)>>,
    on_split_down: Rc<RefCell<dyn FnMut(PaneId)>>,
    on_close_pane: Rc<RefCell<dyn FnMut(PaneId)>>,
) {
    let id = pane.id;
    let term_widget = pane.widget.clone();
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_SECONDARY);
    click.connect_pressed(move |gesture, _n_press, x, y| {
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
    pane.widget.add_controller(click);
}

fn install_key_input(pane: &TerminalPane) {
    if ibus_nav_workaround_enabled() {
        install_ibus_nav_bypass(pane);
    }

    let weak = Rc::downgrade(&pane.runtime);
    let controller = gtk::EventControllerKey::new();
    controller.set_im_context(Some(&pane.runtime.im_context));
    controller.connect_key_pressed(move |_controller, key, _keycode, state| {
        match weak.upgrade() {
            Some(runtime) => handle_key(&runtime, key, state),
            None => glib::Propagation::Proceed,
        }
    });
    pane.widget.add_controller(controller);
}

/// Single keyboard dispatch entry. Called from both the regular
/// `gtk::EventControllerKey` path and the capture-phase
/// `gtk::ShortcutController` IBus 22.04 bypass so the two paths can
/// never disagree about how a given key resolves. Order matters:
///
///   1. Application accelerator (`app_accel_should_win`) — pass through
///      so `Ctrl+T` and friends reach the application chrome.
///   2. PgUp/PgDn smart paging — viewport scroll on the primary screen,
///      escape forwarding on the alt screen, Shift always scrolls.
///   3. libghostty key encoder (DECCKM/kitty/keyboard-protocol aware)
///      with a static fallback for keys ghostty doesn't model.
///
/// Returning `Stop` consumes the event; `Proceed` lets it continue down
/// the controller stack (and, in capture phase, lets the bubble-phase
/// `EventControllerKey` see it).
fn handle_key(
    runtime: &Rc<TerminalRuntime>,
    key: gtk::gdk::Key,
    mods: gtk::gdk::ModifierType,
) -> glib::Propagation {
    if app_accel_should_win(key, mods) {
        return glib::Propagation::Proceed;
    }
    if let Some(action) = page_action_for_key(key, mods) {
        handle_page_key(runtime, action);
        return glib::Propagation::Stop;
    }
    let Some(bytes) = encode_key(runtime, key, mods) else {
        return glib::Propagation::Proceed;
    };
    runtime.write_child(&bytes);
    glib::Propagation::Stop
}

/// Whether the Flatpak/IBus 22.04 navigation-key drop bypass should run.
///
/// On Ubuntu 22.04 hosts the GTK4 ibus immodule (compiled against IBus
/// >= 1.5.30 expectations) and the system IBus 1.5.26 daemon disagree on
/// the `PostProcessKeyEvent` DBus property, which causes plain
/// Backspace/Delete/Tab/Esc/Return/arrows/Home/End/PageUp/PageDown to
/// be silently dropped while a Hangul preedit is on screen. We can't
/// detect the host daemon version from inside the Flatpak sandbox, so
/// we gate on the sandbox flag and let users opt out with
/// `FLOWMUX_NO_IBUS_NAV_WORKAROUND=1`.
fn ibus_nav_workaround_enabled() -> bool {
    if std::env::var_os("FLOWMUX_NO_IBUS_NAV_WORKAROUND").is_some() {
        return false;
    }
    is_flatpak_sandbox()
}

/// Install a capture-phase `gtk::ShortcutController` that intercepts
/// the plain navigation/edit keys IBus 1.5.26 drops on the 22.04 host
/// Flatpak path. ShortcutController is the same mechanism the old
/// VTE-era fix (`eb9059f`) used: it fires before any
/// `EventControllerKey` on the widget — including the IM-attached one
/// installed by `install_key_input` — so the broken ibus-gtk4 round
/// trip never sees these keys. Using a second `EventControllerKey`
/// here triggered an empty-terminal Backspace crash on Flatpak 22.04,
/// presumably because the immodule cannot tolerate two key controllers
/// sharing one widget when only one has an IM context bound.
fn install_ibus_nav_bypass(pane: &TerminalPane) {
    let controller = gtk::ShortcutController::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.set_scope(gtk::ShortcutScope::Local);

    // The capture-phase shortcuts now share the regular `handle_key`
    // dispatcher with the bubble-phase EventControllerKey. Two wins
    // over the previous hard-coded byte tables:
    //
    //   * Arrow keys correctly track the terminal's DECCKM mode (i.e.
    //     `vim`/`less` arrow-key bindings respond as the app expects)
    //     because the same `encode_key` path runs.
    //   * Adding a new key only requires touching the table below
    //     plus the encoder, not two parallel lookup tables.
    //
    // The list mirrors the IBus 1.5.26 set of plain navigation/edit
    // keys that the 22.04 ibus-gtk4 immodule silently drops during a
    // Hangul preedit, plus the Shift variants of Page_Up/Down so
    // forced viewport scroll keeps working from inside alt-screen
    // TUIs even on hosts that need this bypass.
    const EMPTY: gtk::gdk::ModifierType = gtk::gdk::ModifierType::empty();
    const SHIFT: gtk::gdk::ModifierType = gtk::gdk::ModifierType::SHIFT_MASK;
    let bypass_keys: &[(gtk::gdk::Key, gtk::gdk::ModifierType)] = &[
        (gtk::gdk::Key::BackSpace, EMPTY),
        (gtk::gdk::Key::Delete, EMPTY),
        (gtk::gdk::Key::Tab, EMPTY),
        (gtk::gdk::Key::Escape, EMPTY),
        (gtk::gdk::Key::Return, EMPTY),
        (gtk::gdk::Key::ISO_Enter, EMPTY),
        (gtk::gdk::Key::Left, EMPTY),
        (gtk::gdk::Key::Right, EMPTY),
        (gtk::gdk::Key::Up, EMPTY),
        (gtk::gdk::Key::Down, EMPTY),
        (gtk::gdk::Key::Home, EMPTY),
        (gtk::gdk::Key::End, EMPTY),
        (gtk::gdk::Key::Page_Up, EMPTY),
        (gtk::gdk::Key::Page_Down, EMPTY),
        (gtk::gdk::Key::KP_Delete, EMPTY),
        (gtk::gdk::Key::KP_Enter, EMPTY),
        (gtk::gdk::Key::KP_Left, EMPTY),
        (gtk::gdk::Key::KP_Right, EMPTY),
        (gtk::gdk::Key::KP_Up, EMPTY),
        (gtk::gdk::Key::KP_Down, EMPTY),
        (gtk::gdk::Key::KP_Home, EMPTY),
        (gtk::gdk::Key::KP_End, EMPTY),
        (gtk::gdk::Key::KP_Page_Up, EMPTY),
        (gtk::gdk::Key::KP_Page_Down, EMPTY),
        (gtk::gdk::Key::Page_Up, SHIFT),
        (gtk::gdk::Key::Page_Down, SHIFT),
        (gtk::gdk::Key::KP_Page_Up, SHIFT),
        (gtk::gdk::Key::KP_Page_Down, SHIFT),
    ];

    for (key, mods) in bypass_keys {
        let weak = Rc::downgrade(&pane.runtime);
        let key = *key;
        let mods = *mods;
        let action = gtk::CallbackAction::new(move |_, _| {
            if let Some(runtime) = weak.upgrade() {
                return handle_key(&runtime, key, mods);
            }
            glib::Propagation::Stop
        });
        let trigger = gtk::KeyvalTrigger::new(key, mods);
        controller.add_shortcut(gtk::Shortcut::new(Some(trigger), Some(action)));
    }

    pane.widget.add_controller(controller);
}

/// Behaviour applied when PgUp/Dn fires. `Smart*` is the default — scroll
/// the viewport on the primary screen and forward the legacy CSI escape
/// (`\x1b[5~` / `\x1b[6~`) on the alt screen so vim/less/man can still
/// page their own buffers. `Scroll*` is the unconditional viewport-only
/// path used by Shift+PgUp/Dn and reachable from any screen.
#[derive(Clone, Copy)]
enum PageAction {
    SmartUp,
    SmartDown,
    ScrollUp,
    ScrollDown,
}

const SCREEN_ALTERNATE: u32 = 1;

fn is_alternate_screen(runtime: &TerminalRuntime) -> bool {
    let state = runtime.state.borrow();
    matches!(state.terminal.active_screen(), Ok(s) if s == SCREEN_ALTERNATE)
}

fn handle_page_key(runtime: &Rc<TerminalRuntime>, action: PageAction) {
    let up = matches!(action, PageAction::SmartUp | PageAction::ScrollUp);
    let should_scroll = match action {
        PageAction::ScrollUp | PageAction::ScrollDown => true,
        PageAction::SmartUp | PageAction::SmartDown => !is_alternate_screen(runtime),
    };
    if should_scroll {
        scroll_viewport_by_page(runtime, up);
    } else {
        runtime.write_child(if up { b"\x1b[5~" } else { b"\x1b[6~" });
    }
}

fn scroll_viewport_by_page(runtime: &Rc<TerminalRuntime>, up: bool) {
    let rows = runtime.state.borrow().metrics.rows as isize;
    let step = (rows.max(2) - 1).max(1);
    let delta = if up { -step } else { step };
    {
        let mut state = runtime.state.borrow_mut();
        state.terminal.scroll_viewport(ScrollViewport::Delta(delta));
    }
    runtime.widget.queue_draw();
    runtime.refresh_scrollbar();
}

fn page_action_for_key(key: gtk::gdk::Key, mods: gtk::gdk::ModifierType) -> Option<PageAction> {
    // Pass anything with Ctrl/Alt/Super through to `encode_key` so app
    // shortcuts that involve PgUp/Dn (e.g. tab navigation in shells)
    // keep their original meaning.
    let other = mods - gtk::gdk::ModifierType::SHIFT_MASK;
    if !other.is_empty() {
        return None;
    }
    let shift = mods.contains(gtk::gdk::ModifierType::SHIFT_MASK);
    match key {
        gtk::gdk::Key::Page_Up | gtk::gdk::Key::KP_Page_Up => Some(if shift {
            PageAction::ScrollUp
        } else {
            PageAction::SmartUp
        }),
        gtk::gdk::Key::Page_Down | gtk::gdk::Key::KP_Page_Down => Some(if shift {
            PageAction::ScrollDown
        } else {
            PageAction::SmartDown
        }),
        _ => None,
    }
}

fn install_scroll_input(pane: &TerminalPane) {
    // Pointer motion controller keeps `pointer_cell` in sync with the
    // current cursor position so the scroll handler can forward
    // mouse-tracking wheel events at the right grid coordinate. Apps
    // like tig/htop/opencode enable mouse reporting and expect to see
    // button-4/button-5 events at the pointer's cell.
    let motion_weak = Rc::downgrade(&pane.runtime);
    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion(move |_motion, x, y| {
        let Some(runtime) = motion_weak.upgrade() else {
            return;
        };
        let mut state = runtime.state.borrow_mut();
        state.pointer_px = (x as f32, y as f32);
    });
    pane.widget.add_controller(motion);

    let weak = Rc::downgrade(&pane.runtime);
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    scroll.connect_scroll(move |_scroll, _dx, dy| {
        let Some(runtime) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        // Mouse-tracking apps (tig, htop, vim with `mouse=a`, opencode,
        // fzf, …) toggle the terminal into a reporting mode where the
        // wheel must be forwarded as a button-4/button-5 mouse event.
        // Falling back to viewport scrollback in that mode breaks the
        // app's own scroll bindings.
        let mouse_tracking = runtime
            .state
            .borrow()
            .terminal
            .is_mouse_tracking()
            .unwrap_or(false);
        if mouse_tracking {
            if let Some(bytes) = encode_wheel_event(&runtime, dy) {
                runtime.write_child(&bytes);
            }
            return glib::Propagation::Stop;
        }
        {
            let mut state = runtime.state.borrow_mut();
            let delta = if dy > 0.0 { 3 } else { -3 };
            state.terminal.scroll_viewport(ScrollViewport::Delta(delta));
        }
        runtime.widget.queue_draw();
        runtime.refresh_scrollbar();
        glib::Propagation::Stop
    });
    pane.widget.add_controller(scroll);
}

/// Encode a wheel notch into a mouse-tracking event so apps that
/// turned on the reporting mode see button 4 (wheel up) or button 5
/// (wheel down). Skips when the encoder fails or produces no bytes.
fn encode_wheel_event(runtime: &Rc<TerminalRuntime>, dy: f64) -> Option<Vec<u8>> {
    if dy == 0.0 {
        return None;
    }
    let button = if dy < 0.0 {
        MouseButton::Four
    } else {
        MouseButton::Five
    };
    let mut state = runtime.state.borrow_mut();
    let (px, py) = state.pointer_px;
    let cell_w = state.metrics.width.max(1.0) as u32;
    let cell_h = state.metrics.height.max(1.0) as u32;
    let cols = state.metrics.cols.max(1) as u32;
    let rows = state.metrics.rows.max(1) as u32;
    let size = MouseEncoderSize {
        screen_width: cols * cell_w,
        screen_height: rows * cell_h,
        cell_width: cell_w,
        cell_height: cell_h,
        padding_top: 0,
        padding_bottom: 0,
        padding_right: 0,
        padding_left: 0,
    };
    let TerminalState {
        terminal,
        mouse_encoder,
        ..
    } = &mut *state;
    let mut event = MouseEvent::new().ok()?;
    event
        .set_action(MouseAction::Press)
        .set_button(Some(button))
        .set_mods(GhosttyMods::empty())
        .set_position(MousePosition { x: px, y: py });
    mouse_encoder
        .set_options_from_terminal(terminal)
        .set_size(size);
    let mut out = Vec::with_capacity(16);
    mouse_encoder.encode_to_vec(&event, &mut out).ok()?;
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn install_selection(pane: &TerminalPane) {
    let weak = Rc::downgrade(&pane.runtime);
    let start = Rc::new(Cell::new(None::<GridPoint>));
    let drag = gtk::GestureDrag::new();
    drag.set_button(gtk::gdk::BUTTON_PRIMARY);
    {
        let weak = weak.clone();
        let start = start.clone();
        drag.connect_drag_begin(move |gesture, x, y| {
            let modifiers = gesture
                .current_event()
                .map(|e| e.modifier_state())
                .unwrap_or_else(gtk::gdk::ModifierType::empty);
            if modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                start.set(None);
                gesture.set_state(gtk::EventSequenceState::Denied);
                return;
            }
            let Some(runtime) = weak.upgrade() else {
                return;
            };
            let point = point_for_position(&runtime, x, y);
            start.set(Some(point));
            runtime.state.borrow_mut().selection = Some(SelectionRange {
                anchor: point,
                focus: point,
            });
            runtime.widget.grab_focus();
            runtime.widget.queue_draw();
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
    }
    {
        let weak = weak.clone();
        let start = start.clone();
        drag.connect_drag_update(move |_gesture, dx, dy| {
            let Some(anchor) = start.get() else {
                return;
            };
            let Some(runtime) = weak.upgrade() else {
                return;
            };
            let focus = point_for_position(
                &runtime,
                anchor.col as f64 * runtime.state.borrow().metrics.width + dx,
                anchor.row as f64 * runtime.state.borrow().metrics.height + dy,
            );
            runtime.state.borrow_mut().selection = Some(SelectionRange { anchor, focus });
            runtime.widget.queue_draw();
        });
    }
    {
        let weak = weak.clone();
        drag.connect_drag_end(move |_gesture, _dx, _dy| {
            if let Some(runtime) = weak.upgrade() {
                runtime.widget.queue_draw();
            }
        });
    }
    pane.widget.add_controller(drag);
}

fn install_url_click(pane: &TerminalPane, on_open_url: Rc<RefCell<dyn FnMut(PaneId, String)>>) {
    let weak = Rc::downgrade(&pane.runtime);
    let pane_id = pane.id;
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_PRIMARY);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed(move |gesture, _n_press, x, y| {
        let modifiers = gesture
            .current_event()
            .map(|e| e.modifier_state())
            .unwrap_or_else(gtk::gdk::ModifierType::empty);
        if !modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        }
        let Some(runtime) = weak.upgrade() else {
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        };
        let Some(url) = url_at_position(&runtime, x, y) else {
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        };
        (on_open_url.borrow_mut())(pane_id, url);
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    pane.widget.add_controller(click);
}

fn install_io_watch(pane: &TerminalPane, on_child_exited: Rc<RefCell<dyn FnMut(PaneId, i32)>>) {
    let fd = pane.runtime.master.as_raw_fd();
    let weak: Weak<TerminalRuntime> = Rc::downgrade(&pane.runtime);
    let source = glib::source::unix_fd_add_local(
        fd,
        glib::IOCondition::IN | glib::IOCondition::HUP | glib::IOCondition::ERR,
        move |fd, condition| {
            if condition.intersects(glib::IOCondition::HUP | glib::IOCondition::ERR) {
                return glib::ControlFlow::Break;
            }
            let Some(runtime) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            // Pump every available chunk before yielding. The hot
            // path (`feed_bytes`) only forwards the bytes to the VT
            // parser. Title/pwd detection, handler dispatch, queue
            // redraw, and the scrollbar refresh all run once at the
            // end of the batch — alt-screen TUIs like tig, vim, and
            // opencode flood the PTY with many short chunks per
            // frame, and doing those expensive steps per chunk was
            // the dominant cost on slower hosts.
            let mut buf = [0u8; 8192];
            let mut received_any = false;
            loop {
                match read_fd(fd, &mut buf) {
                    ReadResult::Data(n) => {
                        runtime.feed_bytes(&buf[..n]);
                        received_any = true;
                    }
                    ReadResult::WouldBlock => {
                        if received_any {
                            runtime.finalize_batch();
                            runtime.refresh_scrollbar();
                        }
                        return glib::ControlFlow::Continue;
                    }
                    ReadResult::Eof | ReadResult::Error => {
                        if received_any {
                            runtime.finalize_batch();
                            runtime.refresh_scrollbar();
                        }
                        return glib::ControlFlow::Break;
                    }
                }
            }
        },
    );
    *pane.runtime.io_source.borrow_mut() = Some(source);

    if let Some(pid) = pane.runtime.pid.get() {
        let id = pane.id;
        let weak = Rc::downgrade(&pane.runtime);
        let child_source =
            glib::source::child_watch_add_local(glib::Pid(pid), move |_pid, status| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.pid.set(None);
                }
                (on_child_exited.borrow_mut())(id, status);
            });
        *pane.runtime.child_source.borrow_mut() = Some(child_source);
    }
}

fn encode_key(
    runtime: &TerminalRuntime,
    key: gtk::gdk::Key,
    mods: gtk::gdk::ModifierType,
) -> Option<Vec<u8>> {
    let shift = mods.contains(gtk::gdk::ModifierType::SHIFT_MASK);

    if shift
        && matches!(
            key,
            gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter | gtk::gdk::Key::ISO_Enter
        )
    {
        return Some(ALT_ENTER_BYTES.to_vec());
    }

    encode_key_with_ghostty(runtime, key, mods).or_else(|| encode_key_legacy(runtime, key, mods))
}

fn encode_key_with_ghostty(
    runtime: &TerminalRuntime,
    key: gtk::gdk::Key,
    mods: gtk::gdk::ModifierType,
) -> Option<Vec<u8>> {
    let ghostty_key = gdk_key_to_ghostty_key(key)?;
    let mut event = KeyEvent::new().ok()?;
    let ghostty_mods = ghostty_mods_from_gdk(mods);
    event
        .set_action(KeyAction::Press)
        .set_key(ghostty_key)
        .set_mods(ghostty_mods)
        .set_consumed_mods(consumed_mods_for_key(key, mods));

    if let Some(ch) = key.to_unicode().filter(|ch| !ch.is_control()) {
        event
            .set_utf8(Some(ch.to_string()))
            .set_unshifted_codepoint(unshifted_codepoint_for_key(key, ch));
    }

    let mut state = runtime.state.borrow_mut();
    let TerminalState {
        terminal,
        key_encoder,
        ..
    } = &mut *state;
    key_encoder
        .set_options_from_terminal(terminal)
        .set_alt_esc_prefix(true);
    let mut out = Vec::with_capacity(16);
    key_encoder.encode_to_vec(&event, &mut out).ok()?;
    if out.is_empty() { None } else { Some(out) }
}

fn encode_key_legacy(
    runtime: &TerminalRuntime,
    key: gtk::gdk::Key,
    mods: gtk::gdk::ModifierType,
) -> Option<Vec<u8>> {
    let shift = mods.contains(gtk::gdk::ModifierType::SHIFT_MASK);
    let ctrl = mods.contains(gtk::gdk::ModifierType::CONTROL_MASK);
    let alt = mods.contains(gtk::gdk::ModifierType::ALT_MASK)
        || mods.contains(gtk::gdk::ModifierType::META_MASK);

    if shift
        && matches!(
            key,
            gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter | gtk::gdk::Key::ISO_Enter
        )
    {
        return Some(ALT_ENTER_BYTES.to_vec());
    }

    let app_cursor = runtime
        .state
        .borrow()
        .terminal
        .mode(Mode::DECCKM)
        .unwrap_or(false);

    let bytes: &'static [u8] = match key {
        gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter | gtk::gdk::Key::ISO_Enter => b"\r",
        gtk::gdk::Key::BackSpace => b"\x7f",
        gtk::gdk::Key::Delete | gtk::gdk::Key::KP_Delete => b"\x1b[3~",
        gtk::gdk::Key::Tab => b"\t",
        gtk::gdk::Key::ISO_Left_Tab => b"\x1b[Z",
        gtk::gdk::Key::Escape => b"\x1b",
        gtk::gdk::Key::Up | gtk::gdk::Key::KP_Up => {
            if app_cursor {
                b"\x1bOA"
            } else {
                b"\x1b[A"
            }
        }
        gtk::gdk::Key::Down | gtk::gdk::Key::KP_Down => {
            if app_cursor {
                b"\x1bOB"
            } else {
                b"\x1b[B"
            }
        }
        gtk::gdk::Key::Right | gtk::gdk::Key::KP_Right => {
            if app_cursor {
                b"\x1bOC"
            } else {
                b"\x1b[C"
            }
        }
        gtk::gdk::Key::Left | gtk::gdk::Key::KP_Left => {
            if app_cursor {
                b"\x1bOD"
            } else {
                b"\x1b[D"
            }
        }
        gtk::gdk::Key::Home | gtk::gdk::Key::KP_Home => b"\x1b[H",
        gtk::gdk::Key::End | gtk::gdk::Key::KP_End => b"\x1b[F",
        gtk::gdk::Key::Page_Up | gtk::gdk::Key::KP_Page_Up => b"\x1b[5~",
        gtk::gdk::Key::Page_Down | gtk::gdk::Key::KP_Page_Down => b"\x1b[6~",
        gtk::gdk::Key::F1 => b"\x1bOP",
        gtk::gdk::Key::F2 => b"\x1bOQ",
        gtk::gdk::Key::F3 => b"\x1bOR",
        gtk::gdk::Key::F4 => b"\x1bOS",
        gtk::gdk::Key::F5 => b"\x1b[15~",
        gtk::gdk::Key::F6 => b"\x1b[17~",
        gtk::gdk::Key::F7 => b"\x1b[18~",
        gtk::gdk::Key::F8 => b"\x1b[19~",
        gtk::gdk::Key::F9 => b"\x1b[20~",
        gtk::gdk::Key::F10 => b"\x1b[21~",
        gtk::gdk::Key::F11 => b"\x1b[23~",
        gtk::gdk::Key::F12 => b"\x1b[24~",
        _ => {
            if ctrl {
                if let Some(ch) = key.to_unicode().map(|c| c.to_ascii_lowercase()) {
                    if ch.is_ascii_alphabetic() {
                        let mut out = vec![(ch as u8) & 0x1f];
                        if alt {
                            out.insert(0, 0x1b);
                        }
                        return Some(out);
                    }
                }
                return None;
            }
            let ch = key.to_unicode()?;
            if ch.is_control() {
                return None;
            }
            let mut out = Vec::new();
            if alt {
                out.push(0x1b);
            }
            let mut tmp = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
            return Some(out);
        }
    };
    let mut out = Vec::new();
    if alt && !bytes.starts_with(b"\x1b") {
        out.push(0x1b);
    }
    out.extend_from_slice(bytes);
    Some(out)
}

fn ghostty_mods_from_gdk(mods: gtk::gdk::ModifierType) -> GhosttyMods {
    let mut out = GhosttyMods::empty();
    if mods.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
        out |= GhosttyMods::SHIFT;
    }
    if mods.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
        out |= GhosttyMods::CTRL;
    }
    if mods.contains(gtk::gdk::ModifierType::ALT_MASK)
        || mods.contains(gtk::gdk::ModifierType::META_MASK)
    {
        out |= GhosttyMods::ALT;
    }
    if mods.contains(gtk::gdk::ModifierType::SUPER_MASK) {
        out |= GhosttyMods::SUPER;
    }
    out
}

fn consumed_mods_for_key(key: gtk::gdk::Key, mods: gtk::gdk::ModifierType) -> GhosttyMods {
    if mods.contains(gtk::gdk::ModifierType::SHIFT_MASK)
        && key.to_unicode().is_some_and(|ch| !ch.is_control())
    {
        GhosttyMods::SHIFT
    } else {
        GhosttyMods::empty()
    }
}

fn unshifted_codepoint_for_key(key: gtk::gdk::Key, ch: char) -> char {
    match key {
        gtk::gdk::Key::A
        | gtk::gdk::Key::B
        | gtk::gdk::Key::C
        | gtk::gdk::Key::D
        | gtk::gdk::Key::E
        | gtk::gdk::Key::F
        | gtk::gdk::Key::G
        | gtk::gdk::Key::H
        | gtk::gdk::Key::I
        | gtk::gdk::Key::J
        | gtk::gdk::Key::K
        | gtk::gdk::Key::L
        | gtk::gdk::Key::M
        | gtk::gdk::Key::N
        | gtk::gdk::Key::O
        | gtk::gdk::Key::P
        | gtk::gdk::Key::Q
        | gtk::gdk::Key::R
        | gtk::gdk::Key::S
        | gtk::gdk::Key::T
        | gtk::gdk::Key::U
        | gtk::gdk::Key::V
        | gtk::gdk::Key::W
        | gtk::gdk::Key::X
        | gtk::gdk::Key::Y
        | gtk::gdk::Key::Z => ch.to_ascii_lowercase(),
        gtk::gdk::Key::exclam => '1',
        gtk::gdk::Key::at => '2',
        gtk::gdk::Key::numbersign => '3',
        gtk::gdk::Key::dollar => '4',
        gtk::gdk::Key::percent => '5',
        gtk::gdk::Key::asciicircum => '6',
        gtk::gdk::Key::ampersand => '7',
        gtk::gdk::Key::asterisk => '8',
        gtk::gdk::Key::parenleft => '9',
        gtk::gdk::Key::parenright => '0',
        gtk::gdk::Key::asciitilde => '`',
        gtk::gdk::Key::bar => '\\',
        gtk::gdk::Key::braceleft => '[',
        gtk::gdk::Key::braceright => ']',
        gtk::gdk::Key::less => ',',
        gtk::gdk::Key::greater => '.',
        gtk::gdk::Key::plus => '=',
        gtk::gdk::Key::underscore => '-',
        gtk::gdk::Key::quotedbl => '\'',
        gtk::gdk::Key::colon => ';',
        gtk::gdk::Key::question => '/',
        _ => ch,
    }
}

fn gdk_key_to_ghostty_key(key: gtk::gdk::Key) -> Option<GhosttyKey> {
    Some(match key {
        gtk::gdk::Key::a | gtk::gdk::Key::A => GhosttyKey::A,
        gtk::gdk::Key::b | gtk::gdk::Key::B => GhosttyKey::B,
        gtk::gdk::Key::c | gtk::gdk::Key::C => GhosttyKey::C,
        gtk::gdk::Key::d | gtk::gdk::Key::D => GhosttyKey::D,
        gtk::gdk::Key::e | gtk::gdk::Key::E => GhosttyKey::E,
        gtk::gdk::Key::f | gtk::gdk::Key::F => GhosttyKey::F,
        gtk::gdk::Key::g | gtk::gdk::Key::G => GhosttyKey::G,
        gtk::gdk::Key::h | gtk::gdk::Key::H => GhosttyKey::H,
        gtk::gdk::Key::i | gtk::gdk::Key::I => GhosttyKey::I,
        gtk::gdk::Key::j | gtk::gdk::Key::J => GhosttyKey::J,
        gtk::gdk::Key::k | gtk::gdk::Key::K => GhosttyKey::K,
        gtk::gdk::Key::l | gtk::gdk::Key::L => GhosttyKey::L,
        gtk::gdk::Key::m | gtk::gdk::Key::M => GhosttyKey::M,
        gtk::gdk::Key::n | gtk::gdk::Key::N => GhosttyKey::N,
        gtk::gdk::Key::o | gtk::gdk::Key::O => GhosttyKey::O,
        gtk::gdk::Key::p | gtk::gdk::Key::P => GhosttyKey::P,
        gtk::gdk::Key::q | gtk::gdk::Key::Q => GhosttyKey::Q,
        gtk::gdk::Key::r | gtk::gdk::Key::R => GhosttyKey::R,
        gtk::gdk::Key::s | gtk::gdk::Key::S => GhosttyKey::S,
        gtk::gdk::Key::t | gtk::gdk::Key::T => GhosttyKey::T,
        gtk::gdk::Key::u | gtk::gdk::Key::U => GhosttyKey::U,
        gtk::gdk::Key::v | gtk::gdk::Key::V => GhosttyKey::V,
        gtk::gdk::Key::w | gtk::gdk::Key::W => GhosttyKey::W,
        gtk::gdk::Key::x | gtk::gdk::Key::X => GhosttyKey::X,
        gtk::gdk::Key::y | gtk::gdk::Key::Y => GhosttyKey::Y,
        gtk::gdk::Key::z | gtk::gdk::Key::Z => GhosttyKey::Z,
        gtk::gdk::Key::_0 | gtk::gdk::Key::parenright => GhosttyKey::Digit0,
        gtk::gdk::Key::_1 | gtk::gdk::Key::exclam => GhosttyKey::Digit1,
        gtk::gdk::Key::_2 | gtk::gdk::Key::at => GhosttyKey::Digit2,
        gtk::gdk::Key::_3 | gtk::gdk::Key::numbersign => GhosttyKey::Digit3,
        gtk::gdk::Key::_4 | gtk::gdk::Key::dollar => GhosttyKey::Digit4,
        gtk::gdk::Key::_5 | gtk::gdk::Key::percent => GhosttyKey::Digit5,
        gtk::gdk::Key::_6 | gtk::gdk::Key::asciicircum => GhosttyKey::Digit6,
        gtk::gdk::Key::_7 | gtk::gdk::Key::ampersand => GhosttyKey::Digit7,
        gtk::gdk::Key::_8 | gtk::gdk::Key::asterisk => GhosttyKey::Digit8,
        gtk::gdk::Key::_9 | gtk::gdk::Key::parenleft => GhosttyKey::Digit9,
        gtk::gdk::Key::grave | gtk::gdk::Key::asciitilde => GhosttyKey::Backquote,
        gtk::gdk::Key::backslash | gtk::gdk::Key::bar => GhosttyKey::Backslash,
        gtk::gdk::Key::bracketleft | gtk::gdk::Key::braceleft => GhosttyKey::BracketLeft,
        gtk::gdk::Key::bracketright | gtk::gdk::Key::braceright => GhosttyKey::BracketRight,
        gtk::gdk::Key::comma | gtk::gdk::Key::less => GhosttyKey::Comma,
        gtk::gdk::Key::period | gtk::gdk::Key::greater => GhosttyKey::Period,
        gtk::gdk::Key::equal | gtk::gdk::Key::plus => GhosttyKey::Equal,
        gtk::gdk::Key::minus | gtk::gdk::Key::underscore => GhosttyKey::Minus,
        gtk::gdk::Key::apostrophe | gtk::gdk::Key::quotedbl => GhosttyKey::Quote,
        gtk::gdk::Key::semicolon | gtk::gdk::Key::colon => GhosttyKey::Semicolon,
        gtk::gdk::Key::slash | gtk::gdk::Key::question => GhosttyKey::Slash,
        gtk::gdk::Key::space => GhosttyKey::Space,
        gtk::gdk::Key::Delete => GhosttyKey::Delete,
        gtk::gdk::Key::Return | gtk::gdk::Key::ISO_Enter => GhosttyKey::Enter,
        gtk::gdk::Key::Escape => GhosttyKey::Escape,
        gtk::gdk::Key::Tab | gtk::gdk::Key::ISO_Left_Tab => GhosttyKey::Tab,
        gtk::gdk::Key::Up => GhosttyKey::ArrowUp,
        gtk::gdk::Key::Down => GhosttyKey::ArrowDown,
        gtk::gdk::Key::Left => GhosttyKey::ArrowLeft,
        gtk::gdk::Key::Right => GhosttyKey::ArrowRight,
        gtk::gdk::Key::Home => GhosttyKey::Home,
        gtk::gdk::Key::End => GhosttyKey::End,
        gtk::gdk::Key::Insert => GhosttyKey::Insert,
        gtk::gdk::Key::Page_Up => GhosttyKey::PageUp,
        gtk::gdk::Key::Page_Down => GhosttyKey::PageDown,
        gtk::gdk::Key::F1 => GhosttyKey::F1,
        gtk::gdk::Key::F2 => GhosttyKey::F2,
        gtk::gdk::Key::F3 => GhosttyKey::F3,
        gtk::gdk::Key::F4 => GhosttyKey::F4,
        gtk::gdk::Key::F5 => GhosttyKey::F5,
        gtk::gdk::Key::F6 => GhosttyKey::F6,
        gtk::gdk::Key::F7 => GhosttyKey::F7,
        gtk::gdk::Key::F8 => GhosttyKey::F8,
        gtk::gdk::Key::F9 => GhosttyKey::F9,
        gtk::gdk::Key::F10 => GhosttyKey::F10,
        gtk::gdk::Key::F11 => GhosttyKey::F11,
        gtk::gdk::Key::F12 => GhosttyKey::F12,
        gtk::gdk::Key::KP_0 => GhosttyKey::Numpad0,
        gtk::gdk::Key::KP_1 => GhosttyKey::Numpad1,
        gtk::gdk::Key::KP_2 => GhosttyKey::Numpad2,
        gtk::gdk::Key::KP_3 => GhosttyKey::Numpad3,
        gtk::gdk::Key::KP_4 => GhosttyKey::Numpad4,
        gtk::gdk::Key::KP_5 => GhosttyKey::Numpad5,
        gtk::gdk::Key::KP_6 => GhosttyKey::Numpad6,
        gtk::gdk::Key::KP_7 => GhosttyKey::Numpad7,
        gtk::gdk::Key::KP_8 => GhosttyKey::Numpad8,
        gtk::gdk::Key::KP_9 => GhosttyKey::Numpad9,
        gtk::gdk::Key::KP_Add => GhosttyKey::NumpadAdd,
        gtk::gdk::Key::KP_Subtract => GhosttyKey::NumpadSubtract,
        gtk::gdk::Key::KP_Multiply => GhosttyKey::NumpadMultiply,
        gtk::gdk::Key::KP_Divide => GhosttyKey::NumpadDivide,
        gtk::gdk::Key::KP_Decimal => GhosttyKey::NumpadDecimal,
        gtk::gdk::Key::KP_Separator => GhosttyKey::NumpadSeparator,
        gtk::gdk::Key::KP_Equal => GhosttyKey::NumpadEqual,
        gtk::gdk::Key::KP_Enter => GhosttyKey::NumpadEnter,
        gtk::gdk::Key::KP_Insert => GhosttyKey::NumpadInsert,
        gtk::gdk::Key::KP_Delete => GhosttyKey::NumpadDelete,
        gtk::gdk::Key::KP_Home => GhosttyKey::NumpadHome,
        gtk::gdk::Key::KP_End => GhosttyKey::NumpadEnd,
        gtk::gdk::Key::KP_Page_Up => GhosttyKey::NumpadPageUp,
        gtk::gdk::Key::KP_Page_Down => GhosttyKey::NumpadPageDown,
        gtk::gdk::Key::KP_Up => GhosttyKey::NumpadUp,
        gtk::gdk::Key::KP_Down => GhosttyKey::NumpadDown,
        gtk::gdk::Key::KP_Left => GhosttyKey::NumpadLeft,
        gtk::gdk::Key::KP_Right => GhosttyKey::NumpadRight,
        _ => return None,
    })
}

fn app_accel_should_win(key: gtk::gdk::Key, mods: gtk::gdk::ModifierType) -> bool {
    let ctrl = mods.contains(gtk::gdk::ModifierType::CONTROL_MASK);
    let shift = mods.contains(gtk::gdk::ModifierType::SHIFT_MASK);
    let alt = mods.contains(gtk::gdk::ModifierType::ALT_MASK)
        || mods.contains(gtk::gdk::ModifierType::META_MASK);

    if ctrl && shift {
        return matches!(
            key,
            gtk::gdk::Key::Page_Up
                | gtk::gdk::Key::Page_Down
                | gtk::gdk::Key::w
                | gtk::gdk::Key::W
                | gtk::gdk::Key::c
                | gtk::gdk::Key::C
                | gtk::gdk::Key::v
                | gtk::gdk::Key::V
                | gtk::gdk::Key::t
                | gtk::gdk::Key::T
                | gtk::gdk::Key::b
                | gtk::gdk::Key::B
                | gtk::gdk::Key::n
                | gtk::gdk::Key::N
                | gtk::gdk::Key::Tab
                | gtk::gdk::Key::ISO_Left_Tab
        );
    }
    if ctrl {
        return matches!(
            key,
            gtk::gdk::Key::Tab | gtk::gdk::Key::ISO_Left_Tab | gtk::gdk::Key::n | gtk::gdk::Key::N
        );
    }
    if alt {
        return matches!(
            key,
            gtk::gdk::Key::Left
                | gtk::gdk::Key::Right
                | gtk::gdk::Key::Up
                | gtk::gdk::Key::Down
                | gtk::gdk::Key::_1
                | gtk::gdk::Key::_2
                | gtk::gdk::Key::_3
                | gtk::gdk::Key::_4
                | gtk::gdk::Key::_5
                | gtk::gdk::Key::_6
                | gtk::gdk::Key::_7
                | gtk::gdk::Key::_8
                | gtk::gdk::Key::w
                | gtk::gdk::Key::W
        );
    }
    false
}

fn spawn_terminal_endpoint(
    argv: &[String],
    cwd: Option<&std::path::Path>,
    extra_env: &[(String, String)],
) -> Result<(OwnedFd, Option<Pid>), String> {
    #[cfg(test)]
    {
        let _ = (argv, cwd, extra_env);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .map_err(|e| e.to_string())?;
        return Ok((OwnedFd::from(file), None));
    }

    #[cfg(not(test))]
    {
        spawn_pty_child(argv, cwd, extra_env).map(|(master, pid)| (master, Some(pid)))
    }
}

#[cfg(not(test))]
fn spawn_pty_child(
    argv: &[String],
    cwd: Option<&std::path::Path>,
    extra_env: &[(String, String)],
) -> Result<(OwnedFd, Pid), String> {
    let argv_c = argv
        .iter()
        .map(|s| CString::new(s.as_str()).map_err(|_| format!("argv contains NUL: {s:?}")))
        .collect::<Result<Vec<_>, _>>()?;
    if argv_c.is_empty() {
        return Err("argv is empty".into());
    }
    let cwd_c = cwd
        .map(|p| CString::new(p.as_os_str().as_encoded_bytes()).map_err(|_| "cwd contains NUL"))
        .transpose()
        .map_err(|e| e.to_string())?;
    let env_c = extra_env
        .iter()
        .map(|(k, v)| {
            Ok::<_, String>((
                CString::new(k.as_str()).map_err(|_| format!("env key contains NUL: {k:?}"))?,
                CString::new(v.as_str()).map_err(|_| format!("env value contains NUL for {k}"))?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let winsize = Winsize {
        ws_row: DEFAULT_ROWS,
        ws_col: DEFAULT_COLS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    match unsafe { forkpty(Some(&winsize), None) }.map_err(|e| e.to_string())? {
        ForkptyResult::Parent { master, child } => Ok((master, child)),
        ForkptyResult::Child => {
            if let Some(cwd) = cwd_c.as_ref() {
                unsafe {
                    libc::chdir(cwd.as_ptr());
                }
            }
            for (key, value) in &env_c {
                unsafe {
                    libc::setenv(key.as_ptr(), value.as_ptr(), 1);
                }
            }
            let mut ptrs = argv_c.iter().map(|s| s.as_ptr()).collect::<Vec<_>>();
            ptrs.push(std::ptr::null());
            unsafe {
                libc::execvp(argv_c[0].as_ptr(), ptrs.as_ptr());
                libc::_exit(127);
            }
        }
    }
}

fn terminal_child_env(mut extra_env: Vec<(String, String)>) -> Vec<(String, String)> {
    upsert_env(&mut extra_env, "TERM", "xterm-256color");
    upsert_env(&mut extra_env, "COLORTERM", "truecolor");
    extra_env
}

fn upsert_env(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some((_, v)) = env.iter_mut().find(|(k, _)| k == key) {
        *v = value.to_string();
    } else {
        env.push((key.to_string(), value.to_string()));
    }
}

fn set_nonblocking(fd: i32) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

fn set_pty_winsize(fd: i32, rows: u16, cols: u16) {
    let winsize = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &winsize);
    }
}

enum ReadResult {
    Data(usize),
    WouldBlock,
    Eof,
    Error,
}

fn read_fd(fd: i32, buf: &mut [u8]) -> ReadResult {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n > 0 {
        ReadResult::Data(n as usize)
    } else if n == 0 {
        ReadResult::Eof
    } else {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EAGAIN) || err.raw_os_error() == Some(libc::EWOULDBLOCK)
        {
            ReadResult::WouldBlock
        } else {
            ReadResult::Error
        }
    }
}

fn write_fd(fd: i32, bytes: &[u8]) {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        let n = unsafe { libc::write(fd, remaining.as_ptr().cast(), remaining.len()) };
        if n > 0 {
            remaining = &remaining[n as usize..];
            continue;
        }
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if matches!(err.raw_os_error(), Some(libc::EINTR)) {
                continue;
            }
        }
        break;
    }
}

fn cell_background(
    raw: Option<libghostty_vt::screen::Cell>,
    style: Style,
    visuals: &TerminalVisuals,
) -> Option<gtk::gdk::RGBA> {
    let raw = raw?;
    match raw.content_tag().ok()? {
        CellContentTag::BgColorPalette => {
            let idx = raw.bg_color_palette().ok()?.0 as usize;
            visuals.palette.get(idx).copied()
        }
        CellContentTag::BgColorRgb => raw.bg_color_rgb().ok().map(TerminalVisuals::rgba_from_rgb),
        _ => resolve_style_color(style.bg_color, &visuals.palette),
    }
}

fn resolve_style_color(
    color: StyleColor,
    palette: &[gtk::gdk::RGBA; 16],
) -> Option<gtk::gdk::RGBA> {
    match color {
        StyleColor::None => None,
        StyleColor::Palette(idx) => palette.get(idx.0 as usize).copied(),
        StyleColor::Rgb(rgb) => Some(TerminalVisuals::rgba_from_rgb(rgb)),
    }
}

fn point_for_position(runtime: &TerminalRuntime, x: f64, y: f64) -> GridPoint {
    let state = runtime.state.borrow();
    GridPoint {
        col: (x / state.metrics.width)
            .floor()
            .max(0.0)
            .min(state.metrics.cols.saturating_sub(1) as f64) as u16,
        row: (y / state.metrics.height)
            .floor()
            .max(0.0)
            .min(state.metrics.rows.saturating_sub(1) as f64) as u16,
    }
}

fn selection_contains(selection: Option<SelectionRange>, col: u16, row: u16) -> bool {
    let Some((start, end)) = selection.map(normalize_selection) else {
        return false;
    };
    if row < start.row || row > end.row {
        return false;
    }
    if start.row == end.row {
        return row == start.row && col >= start.col && col <= end.col;
    }
    if row == start.row {
        return col >= start.col;
    }
    if row == end.row {
        return col <= end.col;
    }
    true
}

fn normalize_selection(selection: SelectionRange) -> (GridPoint, GridPoint) {
    let a = selection.anchor;
    let b = selection.focus;
    if (a.row, a.col) <= (b.row, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

fn selected_text(state: &mut TerminalState) -> String {
    let Some(selection) = state.selection else {
        return String::new();
    };
    let (start, end) = normalize_selection(selection);
    let lines = visible_lines_from_state(state);
    let mut out = Vec::new();
    for row in start.row..=end.row {
        let Some(line) = lines.get(row as usize) else {
            continue;
        };
        let chars: Vec<char> = line.chars().collect();
        let start_col = if row == start.row {
            start.col as usize
        } else {
            0
        };
        let end_col = if row == end.row {
            end.col as usize
        } else {
            chars.len().saturating_sub(1)
        };
        if start_col >= chars.len() || start_col > end_col {
            out.push(String::new());
            continue;
        }
        out.push(
            chars[start_col..=end_col.min(chars.len().saturating_sub(1))]
                .iter()
                .collect::<String>()
                .trim_end()
                .to_string(),
        );
    }
    out.join("\n")
}

fn visible_lines_from_state(state: &mut TerminalState) -> Vec<String> {
    let TerminalState {
        terminal, render, ..
    } = state;
    let Ok(snapshot) = render.update(terminal) else {
        return Vec::new();
    };
    let mut row_iter = match RowIterator::new() {
        Ok(iter) => iter,
        Err(_) => return Vec::new(),
    };
    let mut rows = match row_iter.update(&snapshot) {
        Ok(rows) => rows,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    while let Some(row) = rows.next() {
        let mut cell_iter = match CellIterator::new() {
            Ok(iter) => iter,
            Err(_) => break,
        };
        let mut cells = match cell_iter.update(row) {
            Ok(cells) => cells,
            Err(_) => break,
        };
        let mut line = String::new();
        while let Some(cell) = cells.next() {
            let raw = cell.raw_cell().ok();
            if raw.map(|c| c.wide().ok()) == Some(Some(CellWide::SpacerTail)) {
                continue;
            }
            let graphemes = cell.graphemes().unwrap_or_default();
            if graphemes.is_empty() {
                line.push(' ');
            } else {
                line.extend(graphemes);
            }
        }
        out.push(line);
    }
    out
}

fn url_at_position(runtime: &TerminalRuntime, x: f64, y: f64) -> Option<String> {
    let mut state = runtime.state.borrow_mut();
    let col = (x / state.metrics.width).floor().max(0.0) as usize;
    let row = (y / state.metrics.height).floor().max(0.0) as u32;
    let line = visible_lines_from_state(&mut state)
        .get(row as usize)
        .cloned()?;
    find_url_containing(&line, col).map(trim_url_trailing)
}

fn find_url_containing(line: &str, col: usize) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut start = 0usize;
    while start < bytes.len() {
        while start < bytes.len() && bytes[start].is_ascii_whitespace() {
            start += 1;
        }
        let mut end = start;
        while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
            end += 1;
        }
        let token = &line[start..end];
        if col >= start
            && col <= end
            && (token.starts_with("http://")
                || token.starts_with("https://")
                || token.starts_with("ftp://")
                || token.starts_with("file://"))
        {
            return Some(token);
        }
        start = end.saturating_add(1);
    }
    None
}

fn trim_url_trailing(s: &str) -> String {
    s.trim_end_matches(|c: char| {
        matches!(
            c,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"' | '`'
        )
    })
    .to_string()
}

fn scaled_font(font: &gtk::pango::FontDescription, scale: f64) -> gtk::pango::FontDescription {
    let mut out = font.clone();
    let size = if font.is_size_absolute() {
        font.size() as f64
    } else {
        font.size() as f64 / gtk::pango::SCALE as f64
    };
    let scaled = (size * scale).max(1.0);
    out.set_size((scaled * gtk::pango::SCALE as f64).round() as i32);
    out
}

/// Cached paint data for one cell, collected during the row-iteration
/// pass in `snapshot_terminal` so the row can then be rendered in two
/// independent passes (backgrounds, then glyphs).
struct RowCell {
    x: u16,
    span: u16,
    fg: gtk::gdk::RGBA,
    bg: gtk::gdk::RGBA,
    text: Option<String>,
    is_spacer_tail: bool,
}

/// Emit one TextNode for a run of cells starting at `start_x`. The
/// caller batches contiguous narrow cells sharing the same foreground
/// color so a row of ASCII characters becomes one Pango shape + one
/// append_layout call instead of N. Wide cells call this directly
/// with a single-cell run.
fn flush_text_run(
    snapshot: &gtk::Snapshot,
    layout: &gtk::pango::Layout,
    metrics: &CellMetrics,
    y: u16,
    start_x: u16,
    fg: &gtk::gdk::RGBA,
    text: &str,
) {
    if text.is_empty() {
        return;
    }
    layout.set_text(text);
    // Align the layout's own baseline to the row baseline. Appending
    // every layout from its own top-left would make fallback-font
    // scripts like Hangul render with a visibly different baseline
    // from monospace Latin because each font reports its own ascent.
    let layout_baseline = layout.baseline() as f64 / gtk::pango::SCALE as f64;
    let px = start_x as f64 * metrics.width;
    let py = y as f64 * metrics.height + metrics.baseline - layout_baseline;
    snapshot.save();
    snapshot.translate(&graphene::Point::new(px as f32, py as f32));
    snapshot.append_layout(layout, fg);
    snapshot.restore();
}

/// Append a ColorNode covering columns `start..end` on row `y` with
/// `color`. Centralized helper for the batched background pass in
/// `snapshot_terminal` so the run-flush sites stay short and identical.
fn append_bg_run(
    snapshot: &gtk::Snapshot,
    metrics: &CellMetrics,
    start: u16,
    end: u16,
    y: u16,
    color: &gtk::gdk::RGBA,
) {
    if end <= start {
        return;
    }
    let x_px = start as f32 * metrics.width as f32;
    let y_px = y as f32 * metrics.height as f32;
    let w_px = (end - start) as f32 * metrics.width as f32;
    let h_px = metrics.height.ceil() as f32;
    snapshot.append_color(color, &graphene::Rect::new(x_px, y_px, w_px, h_px));
}

/// Append the cursor shape at the cell position `(cx, cy)` using the
/// given `style` and `color`. `BlockHollow` is rendered as a 1px outline
/// made of four thin ColorNodes; every other variant is a single fill.
fn append_cursor(
    snapshot: &gtk::Snapshot,
    metrics: &CellMetrics,
    cx: f32,
    cy: f32,
    style: CursorVisualStyle,
    color: &gtk::gdk::RGBA,
) {
    let cell_w = metrics.width as f32;
    let cell_h = metrics.height as f32;
    match style {
        CursorVisualStyle::Bar => {
            snapshot.append_color(color, &graphene::Rect::new(cx, cy, 2.0, cell_h));
        }
        CursorVisualStyle::Underline => {
            snapshot.append_color(
                color,
                &graphene::Rect::new(cx, cy + cell_h - 2.0, cell_w, 2.0),
            );
        }
        CursorVisualStyle::BlockHollow => {
            // 1px outline mimics the cairo `stroke` path the previous
            // implementation used for the unfocused-pane cursor look.
            snapshot.append_color(color, &graphene::Rect::new(cx, cy, cell_w, 1.0));
            snapshot.append_color(
                color,
                &graphene::Rect::new(cx, cy + cell_h - 1.0, cell_w, 1.0),
            );
            snapshot.append_color(color, &graphene::Rect::new(cx, cy, 1.0, cell_h));
            snapshot.append_color(
                color,
                &graphene::Rect::new(cx + cell_w - 1.0, cy, 1.0, cell_h),
            );
        }
        _ => {
            snapshot.append_color(color, &graphene::Rect::new(cx, cy, cell_w, cell_h));
        }
    }
}

fn rgba(s: &str) -> gtk::gdk::RGBA {
    gtk::gdk::RGBA::parse(s).unwrap_or(gtk::gdk::RGBA::BLACK)
}

fn blend_rgba(fg: &gtk::gdk::RGBA, bg: &gtk::gdk::RGBA, alpha: f32) -> gtk::gdk::RGBA {
    gtk::gdk::RGBA::new(
        bg.red() * (1.0 - alpha) + fg.red() * alpha,
        bg.green() * (1.0 - alpha) + fg.green() * alpha,
        bg.blue() * (1.0 - alpha) + fg.blue() * alpha,
        1.0,
    )
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
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
    pub on_close_pane: Rc<RefCell<dyn FnMut(PaneId)>>,
    pub on_split_right: Rc<RefCell<dyn FnMut(PaneId)>>,
    pub on_split_down: Rc<RefCell<dyn FnMut(PaneId)>>,
    pub on_activate_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    pub on_new_surface: Rc<RefCell<dyn FnMut(PaneId)>>,
    pub on_new_browser_surface: Rc<RefCell<dyn FnMut(PaneId)>>,
    pub on_close_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    pub on_rename_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    pub on_reorder_surface: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, usize)>>,
    pub on_tab_drag_to_new_window: Rc<RefCell<dyn FnMut(PaneId, SurfaceId)>>,
    pub tab_drag_drop_seen: Rc<Cell<bool>>,
    pub on_terminal_cwd_changed: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, PathBuf)>>,
    pub on_browser_uri_changed: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, String)>>,
    pub on_browser_title_changed: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, String)>>,
    pub on_terminal_title_changed: Rc<RefCell<dyn FnMut(PaneId, SurfaceId, String)>>,
    pub read_options: Rc<dyn Fn() -> flowmux_config::options::Options>,
    pub position_of_surface_in_pane: Rc<dyn Fn(PaneId, SurfaceId) -> Option<usize>>,
    pub on_open_url: Rc<RefCell<dyn FnMut(PaneId, String)>>,
}

fn wrap_argv_with_pty_tee(argv: Vec<String>, pane: PaneId, surface: SurfaceId) -> Vec<String> {
    let Some(ctl) = flowmux_terminal::find_flowmuxctl() else {
        tracing::warn!(
            "flowmuxctl not found next to the GUI binary; OSC 9/99/777 alarms \
             from terminal-side agents will be unavailable until it is installed. \
             Falling back to a direct shell spawn."
        );
        return argv;
    };
    let mut wrapped = Vec::with_capacity(argv.len() + 7);
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

const ALT_ENTER_BYTES: &[u8] = b"\x1b\r";

fn default_shell_argv() -> Vec<String> {
    if is_flatpak_sandbox() {
        // flatpak-spawn --host does not inherit the sandbox's
        // environment. Forward TERM/COLORTERM explicitly so the host
        // shell (and any ncurses-based program it launches such as
        // opencode, vim, less) sees the same terminal capability hints
        // the sandbox-side PTY child receives from terminal_child_env.
        vec![
            "flatpak-spawn".into(),
            "--host".into(),
            "--watch-bus".into(),
            "--env=TERM=xterm-256color".into(),
            "--env=COLORTERM=truecolor".into(),
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

fn is_flatpak_sandbox() -> bool {
    std::env::var_os("FLATPAK_ID").is_some() || std::path::Path::new("/.flatpak-info").exists()
}

const FLATPAK_HOST_SHELL_BRIDGE: &str = r#"
import pty, os, sys, fcntl, termios, struct, select, signal, pwd, tty
from urllib.parse import quote

shell = pwd.getpwuid(os.getuid()).pw_shell or '/bin/bash'

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
        assert_eq!(ALT_ENTER_BYTES, b"\x1b\r");
    }

    #[test]
    fn gdk_key_mapping_covers_navigation_keys() {
        assert_eq!(
            gdk_key_to_ghostty_key(gtk::gdk::Key::Left),
            Some(GhosttyKey::ArrowLeft)
        );
        assert_eq!(
            gdk_key_to_ghostty_key(gtk::gdk::Key::KP_Left),
            Some(GhosttyKey::NumpadLeft)
        );
        assert_eq!(
            gdk_key_to_ghostty_key(gtk::gdk::Key::Page_Down),
            Some(GhosttyKey::PageDown)
        );
    }

    #[test]
    fn backspace_uses_legacy_encoding_path() {
        assert_eq!(gdk_key_to_ghostty_key(gtk::gdk::Key::BackSpace), None);
    }

    #[test]
    fn mapped_ghostty_keys_encode_without_crashing() {
        let terminal = Terminal::new(TerminalOptions {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            max_scrollback: MAX_SCROLLBACK,
        })
        .unwrap();
        use gtk::glib::translate::FromGlib;

        let keys = (0..=u16::MAX as u32)
            .map(|raw| unsafe { gtk::gdk::Key::from_glib(raw) })
            .filter(|key| gdk_key_to_ghostty_key(*key).is_some())
            .collect::<Vec<_>>();
        assert!(!keys.is_empty());
        let modifier_sets = [
            gtk::gdk::ModifierType::empty(),
            gtk::gdk::ModifierType::SHIFT_MASK,
            gtk::gdk::ModifierType::CONTROL_MASK,
            gtk::gdk::ModifierType::ALT_MASK,
            gtk::gdk::ModifierType::SHIFT_MASK | gtk::gdk::ModifierType::ALT_MASK,
        ];

        for key in keys {
            assert_ne!(key, gtk::gdk::Key::BackSpace);
            for mods in modifier_sets {
                let ghostty_key = gdk_key_to_ghostty_key(key).unwrap();
                let mut event = KeyEvent::new().unwrap();
                event
                    .set_action(KeyAction::Press)
                    .set_key(ghostty_key)
                    .set_mods(ghostty_mods_from_gdk(mods))
                    .set_consumed_mods(consumed_mods_for_key(key, mods));
                if let Some(ch) = key.to_unicode().filter(|ch| !ch.is_control()) {
                    event
                        .set_utf8(Some(ch.to_string()))
                        .set_unshifted_codepoint(unshifted_codepoint_for_key(key, ch));
                }

                let mut encoder = KeyEncoder::new().unwrap();
                encoder
                    .set_options_from_terminal(&terminal)
                    .set_alt_esc_prefix(true);
                let mut out = Vec::new();
                encoder
                    .encode_to_vec(&event, &mut out)
                    .unwrap_or_else(|e| panic!("failed to encode {key:?} with {mods:?}: {e:?}"));
            }
        }
    }

    #[test]
    fn ghostty_key_encoder_uses_decckm_for_application_arrows() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            max_scrollback: MAX_SCROLLBACK,
        })
        .unwrap();
        terminal.vt_write(b"\x1b[?1h");

        let mut encoder = KeyEncoder::new().unwrap();
        let mut event = KeyEvent::new().unwrap();
        event
            .set_action(KeyAction::Press)
            .set_key(GhosttyKey::ArrowUp)
            .set_mods(GhosttyMods::empty())
            .set_consumed_mods(GhosttyMods::empty());

        encoder
            .set_options_from_terminal(&terminal)
            .set_alt_esc_prefix(true);
        let mut out = Vec::new();
        encoder.encode_to_vec(&event, &mut out).unwrap();
        assert_eq!(out, b"\x1bOA");
    }

    #[test]
    fn plain_page_keys_route_through_smart_page_action() {
        assert!(matches!(
            page_action_for_key(gtk::gdk::Key::Page_Up, gtk::gdk::ModifierType::empty()),
            Some(PageAction::SmartUp)
        ));
        assert!(matches!(
            page_action_for_key(gtk::gdk::Key::Page_Down, gtk::gdk::ModifierType::empty()),
            Some(PageAction::SmartDown)
        ));
        assert!(matches!(
            page_action_for_key(gtk::gdk::Key::KP_Page_Up, gtk::gdk::ModifierType::empty()),
            Some(PageAction::SmartUp)
        ));
    }

    #[test]
    fn shift_page_keys_always_scroll_the_viewport() {
        assert!(matches!(
            page_action_for_key(gtk::gdk::Key::Page_Up, gtk::gdk::ModifierType::SHIFT_MASK),
            Some(PageAction::ScrollUp)
        ));
        assert!(matches!(
            page_action_for_key(gtk::gdk::Key::Page_Down, gtk::gdk::ModifierType::SHIFT_MASK),
            Some(PageAction::ScrollDown)
        ));
    }

    /// Ctrl/Alt-combined PgUp/Dn must fall through to `encode_key` so
    /// app-level shortcuts (shell tab-cycling, IDE keybinds inside a
    /// nested terminal, etc.) keep their original meaning.
    #[test]
    fn ctrl_or_alt_modifiers_skip_the_page_action_path() {
        assert!(
            page_action_for_key(gtk::gdk::Key::Page_Up, gtk::gdk::ModifierType::CONTROL_MASK)
                .is_none()
        );
        assert!(
            page_action_for_key(gtk::gdk::Key::Page_Down, gtk::gdk::ModifierType::ALT_MASK)
                .is_none()
        );
        assert!(page_action_for_key(
            gtk::gdk::Key::Page_Up,
            gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::SHIFT_MASK,
        )
        .is_none());
    }

    /// Non-page keys must not match the page-action gate. This catches a
    /// regression where the dispatcher accidentally swallows arrow keys.
    #[test]
    fn non_page_keys_do_not_match_page_action() {
        assert!(
            page_action_for_key(gtk::gdk::Key::Up, gtk::gdk::ModifierType::empty()).is_none()
        );
        assert!(
            page_action_for_key(gtk::gdk::Key::Home, gtk::gdk::ModifierType::empty()).is_none()
        );
    }

    /// Construction smoke test for the Snapshot-based widget. Confirms
    /// the `gobject_subclass` registration succeeds, that the widget
    /// behaves as a `gtk::Widget` for layout properties, and that
    /// installing a snapshot/resize closure does not panic. Skips when
    /// GTK cannot initialize (headless CI without a display).
    #[gtk::test]
    fn terminal_surface_constructs_and_accepts_closures() {
        use crate::ui::terminal_surface::TerminalSurface;
        if gtk::init().is_err() {
            return;
        }
        let surface = TerminalSurface::new();
        surface.set_focusable(true);
        surface.set_hexpand(true);
        surface.set_vexpand(true);
        surface.set_size_request(640, 408);
        // Set both closures and make sure subsequent state queries do
        // not trip a panic in the imp::TerminalSurface accessors.
        surface.set_snapshot_fn(|_snapshot, _widget| {});
        surface.connect_resize(|_w, _h| {});
        assert!(surface.is_focusable());
        let _: gtk::Widget = surface.upcast();
    }
}
