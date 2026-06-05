// SPDX-License-Identifier: GPL-3.0-or-later
//! Pure-Rust terminal engine built on `alacritty_terminal`.
//!
//! This replaces the VTE widget's bundled VT/PTY/grid pipeline. One
//! [`TermEngine`] owns one PTY + parser + grid for a single pane/tab:
//!
//! * The PTY and the byte→grid parser run on `alacritty_terminal`'s own
//!   reader thread (`event_loop`), so flowmux never blocks the GTK main
//!   thread on PTY I/O.
//! * Terminal state lives behind a [`FairMutex<Term>`]; the GTK renderer
//!   locks it to read the grid and the *damage* (dirty-line) set, then
//!   redraws only changed rows. Dirty-line tracking is the optimization
//!   whose absence sank the previous (libghostty) renderer attempt — see
//!   `docs/pure-rust-terminal-migration.md`.
//! * Side-band events (new content, title, bell, child exit, clipboard)
//!   are surfaced to the embedder through a [`TermEvent`] sink. They fire
//!   on the reader thread; the embedder marshals them onto its own main
//!   thread.
//!
//! This module is GTK-free and headless-testable.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use alacritty_terminal::event::{Event as AlacEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options as PtyOptions, Shell};

use crate::TerminalError;

/// Minimal [`Dimensions`] for `Term::new` / `Term::resize`. alacritty's
/// own impls for `(usize, usize)` and `TermSize` are `#[cfg(test)]`-only.
struct GridSize {
    lines: usize,
    cols: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Events the engine surfaces to the embedder. They arrive on the PTY
/// reader thread; the embedder (GTK layer) is responsible for hopping
/// onto its main thread before touching widgets.
#[derive(Debug, Clone)]
pub enum TermEvent {
    /// New grid content is available — redraw the damaged lines.
    Wakeup,
    /// Window/tab title change (OSC 0/2).
    Title(String),
    /// Reset the title to its default.
    ResetTitle,
    /// Terminal bell.
    Bell,
    /// Application asked to store text in the clipboard (OSC 52).
    ClipboardStore(String),
    /// PTY child exited (or the terminal requested shutdown).
    Exit,
}

/// Sink for [`TermEvent`]s. Must be cheap to clone and thread-safe.
pub type TermEventSink = Arc<dyn Fn(TermEvent) + Send + Sync + 'static>;

/// `alacritty_terminal` event listener that maps engine events onto the
/// embedder's [`TermEventSink`]. Cloned into both `Term` and `EventLoop`.
///
/// Public only because it appears in the `Term<Proxy>` returned by
/// [`TermEngine::term`]; it has no public API of its own.
#[derive(Clone)]
pub struct Proxy {
    sink: TermEventSink,
}

impl EventListener for Proxy {
    fn send_event(&self, event: AlacEvent) {
        let mapped = match event {
            AlacEvent::Wakeup => TermEvent::Wakeup,
            AlacEvent::Title(t) => TermEvent::Title(t),
            AlacEvent::ResetTitle => TermEvent::ResetTitle,
            AlacEvent::Bell => TermEvent::Bell,
            AlacEvent::ClipboardStore(_, s) => TermEvent::ClipboardStore(s),
            AlacEvent::Exit | AlacEvent::ChildExit(_) => TermEvent::Exit,
            // MouseCursorDirty, ClipboardLoad, ColorRequest, PtyWrite,
            // TextAreaSizeRequest, CursorBlinkingChange — not needed by
            // the renderer yet; ignore rather than stub.
            _ => return,
        };
        (self.sink)(mapped);
    }
}

/// How to launch a terminal.
#[derive(Debug, Clone)]
pub struct EngineSpec {
    /// argv[0] is the program; the rest are its arguments. Empty → the
    /// platform default shell.
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
    /// Cell pixel size — forwarded to the PTY winsize so full-screen apps
    /// (and image protocols) compute pixel geometry correctly.
    pub cell_width: u16,
    pub cell_height: u16,
    /// Scrollback line cap.
    pub scrollback: usize,
}

impl Default for EngineSpec {
    fn default() -> Self {
        Self {
            argv: Vec::new(),
            cwd: None,
            env: Vec::new(),
            rows: 24,
            cols: 80,
            cell_width: 8,
            cell_height: 16,
            scrollback: 10_000,
        }
    }
}

/// The Debian/Ubuntu default login `PATH`. Appended (missing entries only)
/// to the inherited `PATH` so a shell spawned from a minimal desktop session
/// can still resolve base tools in `/usr/bin` etc.
const STANDARD_PATH_DIRS: &[&str] = &[
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
    "/usr/games",
    "/usr/local/games",
];

/// Return `current` with any missing [`STANDARD_PATH_DIRS`] appended. Existing
/// entries keep their position and priority; only absent standard dirs are
/// added, at the end.
pub fn ensure_standard_path(current: Option<std::ffi::OsString>) -> String {
    let mut dirs: Vec<PathBuf> = current
        .as_ref()
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();
    for std_dir in STANDARD_PATH_DIRS {
        let p = PathBuf::from(std_dir);
        if !dirs.iter().any(|d| d == &p) {
            dirs.push(p);
        }
    }
    std::env::join_paths(&dirs)
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| STANDARD_PATH_DIRS.join(":"))
}

/// A running terminal: PTY + parser + grid for one pane/tab.
pub struct TermEngine {
    term: Arc<FairMutex<Term<Proxy>>>,
    /// `None` for a headless stub (tests): no PTY, no reader thread, so
    /// `write`/`resize` are no-ops. The real spawn always sets it.
    loop_tx: Option<EventLoopSender>,
    size: WindowSize,
    pid: Option<u32>,
    /// PTY reader thread. Joined on `Drop` so it stops touching the shared
    /// `Term` before it is freed — detaching it leaks a thread per pane and
    /// races the `Term` teardown (observed as a process-exit SIGSEGV once
    /// many panes had been spawned).
    io_thread: Option<IoThread>,
}

type IoThread = std::thread::JoinHandle<(
    EventLoop<alacritty_terminal::tty::Pty, Proxy>,
    alacritty_terminal::event_loop::State,
)>;

impl TermEngine {
    /// Spawn the process and start pumping its PTY. `sink` receives
    /// [`TermEvent`]s from the reader thread.
    pub fn spawn(spec: EngineSpec, sink: TermEventSink) -> Result<Self, TerminalError> {
        let window_size = WindowSize {
            num_lines: spec.rows,
            num_cols: spec.cols,
            cell_width: spec.cell_width,
            cell_height: spec.cell_height,
        };

        let config = Config {
            scrolling_history: spec.scrollback,
            ..Default::default()
        };
        let proxy = Proxy { sink };
        let dims = GridSize {
            lines: spec.rows as usize,
            cols: spec.cols as usize,
        };
        let term = Term::new(config, &dims, proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        let shell = spec
            .argv
            .split_first()
            .map(|(program, args)| Shell::new(program.clone(), args.to_vec()));
        // The GUI process (launched from a desktop file) usually has no
        // TERM, and `tty::new` does not set one — so the child shell and
        // ncurses tools (`clear`, vim) fail with "TERM environment variable
        // not set" and readline miscomputes autowrap, garbling long lines.
        // Advertise an xterm-256color terminfo (always installed) plus
        // truecolor, unless the caller already pinned these.
        let mut env: std::collections::HashMap<String, String> = spec.env.into_iter().collect();
        env.entry("TERM".into())
            .or_insert_with(|| "xterm-256color".into());
        env.entry("COLORTERM".into())
            .or_insert_with(|| "truecolor".into());
        // A GUI launched from a minimal desktop session can inherit a PATH
        // without `/usr/bin`, so a spawned `bash -l` cannot find base tools
        // like `xset` (the Flatpak build dodged this by running the shell on
        // the host). Guarantee the standard system bin dirs are present,
        // appending only the missing ones so the user's own entries and
        // their order are untouched.
        if !env.contains_key("PATH") {
            env.insert(
                "PATH".into(),
                ensure_standard_path(std::env::var_os("PATH")),
            );
        }
        let pty_opts = PtyOptions {
            shell,
            working_directory: spec.cwd,
            drain_on_exit: false,
            env,
        };

        let pty =
            tty::new(&pty_opts, window_size, 0).map_err(|e| TerminalError::Spawn(e.to_string()))?;
        let pid = Some(pty.child().id());

        let event_loop = EventLoop::new(term.clone(), proxy, pty, false, false)
            .map_err(|e| TerminalError::Spawn(e.to_string()))?;
        let loop_tx = event_loop.channel();
        // Keep the reader thread's handle so `Drop` can join it after
        // sending `Msg::Shutdown`; the loop also exits on its own when the
        // child process exits.
        let io_thread = Some(event_loop.spawn());

        Ok(Self {
            term,
            loop_tx: Some(loop_tx),
            size: window_size,
            pid,
            io_thread,
        })
    }

    /// A headless terminal with no PTY and no reader thread, for unit
    /// tests that build the widget tree but must not fork a real shell
    /// (the GUI test suite constructs many workspaces in one process; the
    /// old VTE pane never forked in tests because its spawn was async).
    pub fn stub(rows: u16, cols: u16) -> Self {
        let config = Config::default();
        let dims = GridSize {
            lines: rows as usize,
            cols: cols as usize,
        };
        let proxy = Proxy {
            sink: Arc::new(|_| {}),
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &dims, proxy)));
        Self {
            term,
            loop_tx: None,
            size: WindowSize {
                num_lines: rows,
                num_cols: cols,
                cell_width: 8,
                cell_height: 16,
            },
            pid: None,
            io_thread: None,
        }
    }

    /// PID of the spawned child process (for `/proc/<pid>/cwd` lookups).
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Write raw bytes to the PTY (already-encoded keystrokes / paste).
    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return;
        }
        if let Some(tx) = &self.loop_tx {
            let _ = tx.send(Msg::Input(bytes));
        }
    }

    /// Resize the grid and the PTY winsize together.
    pub fn resize(&mut self, rows: u16, cols: u16, cell_width: u16, cell_height: u16) {
        let window_size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width,
            cell_height,
        };
        self.size = window_size;
        self.term.lock().resize(GridSize {
            lines: rows as usize,
            cols: cols as usize,
        });
        if let Some(tx) = &self.loop_tx {
            let _ = tx.send(Msg::Resize(window_size));
        }
    }

    /// Lock the terminal to read the grid / damage for rendering.
    pub fn term(&self) -> &Arc<FairMutex<Term<Proxy>>> {
        &self.term
    }

    /// Text of viewport row `line` (cursor/scrollback-relative top = 0),
    /// for URL detection on Ctrl-click.
    pub fn row_text(&self, line: usize) -> String {
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Line, Point};
        let term = self.term.lock();
        let grid = term.grid();
        let cols = grid.columns();
        if line >= grid.screen_lines() {
            return String::new();
        }
        let mut s = String::with_capacity(cols);
        for col in 0..cols {
            let cell = &grid[Point::new(Line(line as i32), Column(col))];
            s.push(cell.c);
        }
        s
    }

    /// Begin a linear selection at viewport cell `(col, line)`. `right`
    /// picks the right half of the cell (caret after the glyph).
    pub fn selection_start(&self, col: usize, line: usize, right: bool) {
        use alacritty_terminal::index::{Column, Line, Point, Side};
        use alacritty_terminal::selection::{Selection, SelectionType};
        let side = if right { Side::Right } else { Side::Left };
        let mut term = self.term.lock();
        // The viewport row is `abs_line + display_offset` (see `render`), so
        // the absolute grid line is `line - display_offset`. Anchoring in
        // absolute coordinates lets the highlight stay over the same text
        // when the viewport is scrolled through scrollback mid-drag.
        let off = term.grid().display_offset() as i32;
        let point = Point::new(Line(line as i32 - off), Column(col));
        let sel = Selection::new(SelectionType::Simple, point, side);
        term.selection = Some(sel);
    }

    /// Extend the active selection to viewport cell `(col, line)`.
    pub fn selection_update(&self, col: usize, line: usize, right: bool) {
        use alacritty_terminal::index::{Column, Line, Point, Side};
        let side = if right { Side::Right } else { Side::Left };
        let mut term = self.term.lock();
        // Same viewport→absolute mapping as `selection_start`.
        let off = term.grid().display_offset() as i32;
        let point = Point::new(Line(line as i32 - off), Column(col));
        if let Some(sel) = term.selection.as_mut() {
            sel.update(point, side);
        }
    }

    /// Select the semantic word (double-click) at viewport cell `(col, line)`.
    /// Uses alacritty's `Semantic` selection so the range snaps to word
    /// boundaries and follows word-wise when the drag is extended.
    pub fn selection_word(&self, col: usize, line: usize) {
        use alacritty_terminal::index::{Column, Line, Point, Side};
        use alacritty_terminal::selection::{Selection, SelectionType};
        let mut term = self.term.lock();
        // Same viewport→absolute mapping as `selection_start`.
        let off = term.grid().display_offset() as i32;
        let point = Point::new(Line(line as i32 - off), Column(col));
        let sel = Selection::new(SelectionType::Semantic, point, Side::Left);
        term.selection = Some(sel);
    }

    /// Clear any active selection.
    pub fn selection_clear(&self) {
        self.term.lock().selection = None;
    }

    /// Scroll the viewport through scrollback by `delta` lines (positive =
    /// toward older output). No-op while an app holds the alternate screen.
    pub fn scroll_lines(&self, delta: i32) {
        use alacritty_terminal::grid::Scroll;
        self.term.lock().scroll_display(Scroll::Delta(delta));
    }

    /// Scroll the viewport by a whole page (positive = older).
    pub fn scroll_page(&self, up: bool) {
        use alacritty_terminal::grid::{Dimensions, Scroll};
        let mut term = self.term.lock();
        let page = term.grid().screen_lines().saturating_sub(1) as i32;
        term.scroll_display(if up {
            Scroll::Delta(page)
        } else {
            Scroll::Delta(-page)
        });
    }

    /// Write user-typed input (keys / IME commit / paste) to the PTY and,
    /// if the viewport was scrolled up into scrollback, pin it back to the
    /// bottom so the live input line is brought into view (matches xterm/
    /// VTE). Returns `true` if it pinned, so the caller can repaint at once
    /// instead of waiting for the echo. No-op pin on the alternate screen,
    /// which has no scrollback.
    pub fn write_keys(&self, bytes: impl Into<Cow<'static, [u8]>>) -> bool {
        use alacritty_terminal::grid::Scroll;
        let scrolled = {
            let mut term = self.term.lock();
            if term.grid().display_offset() != 0 {
                term.scroll_display(Scroll::Bottom);
                true
            } else {
                false
            }
        };
        self.write(bytes);
        scrolled
    }

    /// Scrollback position as `(display_offset, history_lines)`:
    /// `display_offset` 0 = pinned to the bottom, `history_lines` = how
    /// many scrolled-off lines exist. Drives the scrollbar thumb.
    pub fn scrollback_state(&self) -> (usize, usize) {
        use alacritty_terminal::grid::Dimensions;
        let term = self.term.lock();
        let grid = term.grid();
        (grid.display_offset(), grid.history_size())
    }

    /// DECCKM application-cursor-keys mode is active (apps like vim/tig/
    /// claude switch arrows to `ESC O A` etc. while it is set).
    pub fn app_cursor_mode(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.lock().mode().contains(TermMode::APP_CURSOR)
    }

    /// Bracketed-paste mode (DECSET 2004): pasted text must be wrapped in
    /// `ESC [ 200 ~` … `ESC [ 201 ~` so the app does not run it as input.
    pub fn bracketed_paste(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.lock().mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// The foreground app is on the alternate screen (full-screen TUI like
    /// vim/tig/less). The wheel should drive the app, not our scrollback.
    pub fn alt_screen(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.lock().mode().contains(TermMode::ALT_SCREEN)
    }

    /// The app requested mouse reporting (any click/drag/motion mode), so
    /// pointer events should be forwarded to it instead of starting a
    /// local text selection.
    pub fn mouse_report(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.lock().mode().intersects(TermMode::MOUSE_MODE)
    }

    /// Mouse reporting uses SGR encoding (`ESC [ < … M/m`) rather than the
    /// legacy byte form.
    pub fn sgr_mouse(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.lock().mode().contains(TermMode::SGR_MOUSE)
    }

    /// 1002: report pointer motion only while a button is held (button-event
    /// tracking — drag-select in vim, pane drag-resize in tmux).
    pub fn mouse_drag_mode(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.lock().mode().contains(TermMode::MOUSE_DRAG)
    }

    /// 1003: report all pointer motion, button held or not (any-event
    /// tracking — hover effects).
    pub fn mouse_motion_mode(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.lock().mode().contains(TermMode::MOUSE_MOTION)
    }

    /// 1004: the app wants focus in/out reports (`CSI I` / `CSI O`) when the
    /// terminal gains/loses focus (vim/tmux focus-events).
    pub fn focus_event_mode(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.lock().mode().contains(TermMode::FOCUS_IN_OUT)
    }

    pub fn has_selection(&self) -> bool {
        self.term
            .lock()
            .selection
            .as_ref()
            .is_some_and(|s| !s.is_empty())
    }

    /// The currently selected text, if any.
    pub fn selection_text(&self) -> Option<String> {
        self.term.lock().selection_to_string()
    }

    /// Current window size (rows/cols/cell px).
    pub fn window_size(&self) -> WindowSize {
        self.size
    }
}

impl Drop for TermEngine {
    fn drop(&mut self) {
        if let Some(tx) = &self.loop_tx {
            let _ = tx.send(Msg::Shutdown);
        }
        // Join the reader thread so it is no longer touching the shared
        // `Term` when this struct's `Arc` drops to zero.
        if let Some(handle) = self.io_thread.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn echo_spec(cmd: &str) -> EngineSpec {
        EngineSpec {
            argv: vec!["/bin/sh".to_string(), "-c".to_string(), cmd.to_string()],
            rows: 24,
            cols: 80,
            ..Default::default()
        }
    }

    #[test]
    fn spawn_runs_and_writes_to_grid() {
        let wakeups = Arc::new(AtomicUsize::new(0));
        let w = wakeups.clone();
        let sink: TermEventSink = Arc::new(move |ev| {
            if matches!(ev, TermEvent::Wakeup) {
                w.fetch_add(1, Ordering::SeqCst);
            }
        });

        let engine = TermEngine::spawn(echo_spec("printf HELLO; sleep 0.2"), sink).expect("spawn");

        // Wait for the reader thread to parse output into the grid.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut found = false;
        while Instant::now() < deadline {
            {
                let term = engine.term().lock();
                let grid = term.grid();
                // Read the first row's text.
                let mut row = String::new();
                for col in 0..grid.columns() {
                    let cell = &grid[alacritty_terminal::index::Line(0)]
                        [alacritty_terminal::index::Column(col)];
                    row.push(cell.c);
                }
                if row.contains("HELLO") {
                    found = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(found, "expected HELLO to land in the grid");
        assert!(wakeups.load(Ordering::SeqCst) > 0, "expected wakeups");
    }

    #[test]
    fn selection_anchors_in_scrollback() {
        use alacritty_terminal::vte::ansi::Processor;

        // 5-row screen; feed 20 lines so 15 land in scrollback.
        let engine = TermEngine::stub(5, 10);
        {
            let mut term = engine.term().lock();
            let mut parser: Processor = Processor::new();
            let feed = (0..20)
                .map(|i| format!("L{i:02}"))
                .collect::<Vec<_>>()
                .join("\r\n");
            parser.advance(&mut *term, feed.as_bytes());
        }

        // Scroll up 3 → viewport top row shows L12. Selecting that viewport
        // row must grab L12, not whatever absolute line 0 happens to be: the
        // bug stored the viewport row as the absolute line and ignored the
        // scroll offset.
        engine.scroll_lines(3);
        engine.selection_start(0, 0, false);
        engine.selection_update(9, 0, true);
        let text = engine.selection_text().unwrap_or_default();
        assert!(
            text.trim_end().ends_with("L12"),
            "selection followed the scroll: got {text:?}"
        );
    }

    #[test]
    fn selection_word_snaps_to_word_boundaries() {
        use alacritty_terminal::vte::ansi::Processor;

        let engine = TermEngine::stub(3, 20);
        {
            let mut term = engine.term().lock();
            let mut parser: Processor = Processor::new();
            parser.advance(&mut *term, b"foo bar baz");
        }

        // Double-click anywhere inside "bar" (cols 4..=6) must grab the whole
        // word, not a single cell.
        engine.selection_word(5, 0);
        assert_eq!(engine.selection_text().as_deref(), Some("bar"));
    }

    #[test]
    fn standard_path_appends_only_missing_dirs() {
        use std::ffi::OsString;

        // A minimal session PATH without /usr/bin: the user's dir keeps its
        // leading priority, and the missing standard dirs are appended.
        let got = ensure_standard_path(Some(OsString::from("/home/u/bin:/snap/bin")));
        assert!(got.starts_with("/home/u/bin:/snap/bin:"));
        assert!(got.split(':').any(|d| d == "/usr/bin"));
        assert!(got.split(':').any(|d| d == "/bin"));

        // An already-complete PATH is returned unchanged (no duplicates).
        let full = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/usr/games:/usr/local/games";
        let got = ensure_standard_path(Some(OsString::from(full)));
        assert_eq!(got, full);

        // No inherited PATH at all → the full standard set.
        let got = ensure_standard_path(None);
        assert!(got.split(':').any(|d| d == "/usr/bin"));
    }
}
