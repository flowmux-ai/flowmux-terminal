// SPDX-License-Identifier: GPL-3.0-or-later
//! Safe Rust wrapper over the libghostty-vt C shim (`csrc/ghostty_shim.*`).
//!
//! libghostty-vt owns VT parsing, terminal state, scrollback and reflow;
//! flowmux drives it from a real PTY and renders the grid itself. This module
//! is the boundary: feed bytes with [`Vt::write`], snapshot with
//! [`Vt::update`], then read the grid with [`Vt::cell`] / [`Vt::row_text`].
//!
//! Compiled only under the `libghostty` cargo feature. libghostty-vt is
//! extracted from Ghostty (MIT, © Mitchell Hashimoto and Ghostty
//! contributors); see the crate `NOTICE` for attribution.

use std::os::raw::{c_char, c_int, c_long};

/// Opaque per-pane VT context owned by the C shim.
#[repr(C)]
struct FxvtCtx {
    _private: [u8; 0],
}

/// Flattened grid cell as produced by the shim. Field order/size mirrors
/// `FxvtCell` in `csrc/ghostty_shim.h` exactly (ABI-critical).
#[repr(C)]
#[derive(Clone, Copy)]
struct FxvtCell {
    codepoints: [u32; 8],
    cp_len: u8,
    fg: [u8; 3],
    bg: [u8; 3],
    has_bg: u8,
    flags: u8,
    selected: u8,
    wide: u8,
    _pad: [u8; 2],
}

// Style flag bits — must match the FXVT_FLAG_* macros in the shim header.
const FXVT_FLAG_BOLD: u8 = 1 << 0;
const FXVT_FLAG_ITALIC: u8 = 1 << 1;
const FXVT_FLAG_UNDERLINE: u8 = 1 << 2;
const FXVT_FLAG_INVERSE: u8 = 1 << 3;
const FXVT_FLAG_STRIKETHROUGH: u8 = 1 << 4;
const FXVT_FLAG_FAINT: u8 = 1 << 5;
const FXVT_FLAG_BLINK: u8 = 1 << 6;

impl FxvtCell {
    fn zeroed() -> Self {
        FxvtCell {
            codepoints: [0; 8],
            cp_len: 0,
            fg: [0; 3],
            bg: [0; 3],
            has_bg: 0,
            flags: 0,
            selected: 0,
            wide: 0,
            _pad: [0; 2],
        }
    }
}

/// Decode a raw shim cell (as produced by `fxvt_cell`/`fxvt_read_grid`) into the
/// safe [`Cell`] view.
fn cell_from_raw(raw: &FxvtCell) -> Cell {
    let mut text = String::new();
    for &cp in raw.codepoints.iter().take(raw.cp_len as usize) {
        if let Some(c) = char::from_u32(cp) {
            text.push(c);
        }
    }
    let style = CellStyle {
        bold: raw.flags & FXVT_FLAG_BOLD != 0,
        italic: raw.flags & FXVT_FLAG_ITALIC != 0,
        underline: raw.flags & FXVT_FLAG_UNDERLINE != 0,
        inverse: raw.flags & FXVT_FLAG_INVERSE != 0,
        strikethrough: raw.flags & FXVT_FLAG_STRIKETHROUGH != 0,
        faint: raw.flags & FXVT_FLAG_FAINT != 0,
        blink: raw.flags & FXVT_FLAG_BLINK != 0,
    };
    Cell {
        text,
        fg: Rgb {
            r: raw.fg[0],
            g: raw.fg[1],
            b: raw.fg[2],
        },
        bg: (raw.has_bg != 0).then_some(Rgb {
            r: raw.bg[0],
            g: raw.bg[1],
            b: raw.bg[2],
        }),
        style,
        selected: raw.selected != 0,
        wide: raw.wide != 0,
    }
}

extern "C" {
    fn fxvt_new(cols: u16, rows: u16, scrollback: usize) -> *mut FxvtCtx;
    fn fxvt_free(ctx: *mut FxvtCtx);
    fn fxvt_write(ctx: *mut FxvtCtx, data: *const u8, len: usize);
    fn fxvt_resize(ctx: *mut FxvtCtx, cols: u16, rows: u16, cw: u32, ch: u32) -> c_int;
    fn fxvt_update(ctx: *mut FxvtCtx) -> c_int;
    fn fxvt_dims(ctx: *mut FxvtCtx, cols: *mut u16, rows: *mut u16) -> c_int;
    fn fxvt_cursor(ctx: *mut FxvtCtx, x: *mut u16, y: *mut u16, vis: *mut c_int) -> c_int;
    fn fxvt_colors(
        ctx: *mut FxvtCtx,
        fg: *mut u8,
        bg: *mut u8,
        cursor: *mut u8,
        cursor_has: *mut c_int,
    ) -> c_int;
    fn fxvt_cell(ctx: *mut FxvtCtx, row: u16, col: u16, out: *mut FxvtCell) -> c_int;
    fn fxvt_read_grid(ctx: *mut FxvtCtx, out: *mut FxvtCell, cols: u16, rows: u16) -> c_int;
    fn fxvt_row_text(ctx: *mut FxvtCtx, row: u16, buf: *mut c_char, cap: usize) -> usize;
    fn fxvt_set_default_colors(
        ctx: *mut FxvtCtx,
        fg: *const u8,
        bg: *const u8,
        cursor: *const u8,
    ) -> c_int;
    fn fxvt_set_palette(ctx: *mut FxvtCtx, rgb: *const u8, count: c_int) -> c_int;
    fn fxvt_set_selection(
        ctx: *mut FxvtCtx,
        sx: u16,
        sy: u32,
        ex: u16,
        ey: u32,
        rectangle: c_int,
    ) -> c_int;
    fn fxvt_clear_selection(ctx: *mut FxvtCtx);
    fn fxvt_scroll(ctx: *mut FxvtCtx, delta: c_long);
    fn fxvt_scroll_bottom(ctx: *mut FxvtCtx);
    fn fxvt_scrollbar(
        ctx: *mut FxvtCtx,
        total: *mut u64,
        offset: *mut u64,
        len: *mut u64,
    ) -> c_int;
    fn fxvt_title(ctx: *mut FxvtCtx, buf: *mut c_char, cap: usize) -> usize;
    fn fxvt_pwd(ctx: *mut FxvtCtx, buf: *mut c_char, cap: usize) -> usize;
    fn fxvt_encode_key(
        ctx: *mut FxvtCtx,
        named_key: c_int,
        unshifted_cp: u32,
        mods: c_int,
        composing: c_int,
        buf: *mut c_char,
        cap: usize,
    ) -> usize;
    fn fxvt_mouse_enabled(ctx: *mut FxvtCtx) -> c_int;
    fn fxvt_encode_mouse(
        ctx: *mut FxvtCtx,
        action: c_int,
        button: c_int,
        px: f64,
        py: f64,
        mods: c_int,
        buf: *mut c_char,
        cap: usize,
    ) -> usize;
}

/// Mouse event kind for [`Vt::encode_mouse`] (matches the shim's `FXVT_MOUSE_*`).
#[derive(Debug, Clone, Copy)]
pub enum MouseAction {
    Press = 0,
    Release = 1,
    Motion = 2,
}

/// Mouse button for [`Vt::encode_mouse`] (matches the shim's `FXVT_BTN_*`).
/// `WheelUp`/`WheelDown` are reported as buttons 4/5.
#[derive(Debug, Clone, Copy)]
pub enum MouseButton {
    None = 0,
    Left = 1,
    Right = 2,
    Middle = 3,
    WheelUp = 4,
    WheelDown = 5,
}

/// Modifier bits for [`Vt::encode_mouse`]/[`Vt::encode_key`] (shim `FXVT_MOD_*`).
pub const MOD_SHIFT: u8 = 1;
pub const MOD_CTRL: u8 = 2;
pub const MOD_ALT: u8 = 4;

/// Named (non-text) key codes for [`Vt::encode_key`]. Plain character keys use
/// [`NONE`] and carry their identity in the unshifted codepoint. Values mirror
/// the shim's `FXVT_KEY_*`.
pub mod named_key {
    pub const NONE: i32 = 0;
    pub const ENTER: i32 = 1;
    pub const TAB: i32 = 2;
    pub const BACKSPACE: i32 = 3;
    pub const ESCAPE: i32 = 4;
    pub const SPACE: i32 = 5;
    pub const UP: i32 = 6;
    pub const DOWN: i32 = 7;
    pub const LEFT: i32 = 8;
    pub const RIGHT: i32 = 9;
    pub const HOME: i32 = 10;
    pub const END: i32 = 11;
    pub const PAGE_UP: i32 = 12;
    pub const PAGE_DOWN: i32 = 13;
    pub const DELETE: i32 = 14;
    pub const INSERT: i32 = 15;
    pub const KP_ENTER: i32 = 16;
    /// Function key Fn (1-12) → 101..112.
    pub const fn f(n: u32) -> i32 {
        101 + n as i32 - 1
    }
}

/// Read a NUL-terminated string the shim writes into a caller buffer.
fn read_c_string(f: impl Fn(*mut c_char, usize) -> usize) -> Option<String> {
    let mut buf = vec![0i8; 4096];
    let n = f(buf.as_mut_ptr(), buf.len());
    if n == 0 {
        return None;
    }
    let bytes: Vec<u8> = buf[..n].iter().map(|&b| b as u8).collect();
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// A 24-bit RGB color resolved by libghostty (palette + style flattened).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Per-cell text styling, decoded from the shim flag bits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CellStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub strikethrough: bool,
    pub faint: bool,
    pub blink: bool,
}

/// A single rendered grid cell.
#[derive(Debug, Clone)]
pub struct Cell {
    /// The grapheme cluster as a `String` (empty for a blank cell).
    pub text: String,
    pub fg: Rgb,
    /// Background color, present only when the cell carries an explicit one.
    pub bg: Option<Rgb>,
    pub style: CellStyle,
    /// Whether the cell is part of the active selection. Because selection is
    /// terminal state (not a renderer overlay), it survives output repaints —
    /// the behavior the VTE path needed a patch to get.
    pub selected: bool,
    /// True if this is the lead cell of a double-width glyph.
    pub wide: bool,
}

/// Cursor position + visibility from the latest [`Vt::update`] snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
}

/// Default palette colors from the latest snapshot: the terminal default
/// foreground/background and the cursor color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colors {
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
    /// When false the cursor has no explicit color; invert the cell instead.
    pub cursor_has_value: bool,
}

/// A libghostty-vt terminal instance. `!Send`/`!Sync` (raw pointer): drive it
/// from a single thread (the GTK main thread in the GUI).
pub struct Vt {
    ctx: *mut FxvtCtx,
}

impl Vt {
    /// Create a `cols` x `rows` terminal with `scrollback` history lines.
    pub fn new(cols: u16, rows: u16, scrollback: usize) -> Option<Self> {
        let ctx = unsafe { fxvt_new(cols, rows, scrollback) };
        if ctx.is_null() {
            None
        } else {
            Some(Self { ctx })
        }
    }

    /// Feed raw VT output bytes (typically a chunk read from the PTY master).
    pub fn write(&mut self, data: &[u8]) {
        if !data.is_empty() {
            unsafe { fxvt_write(self.ctx, data.as_ptr(), data.len()) };
        }
    }

    /// Resize the grid. `cell_w_px`/`cell_h_px` feed pixel/mouse reporting;
    /// pass the renderer's cell metrics (or 1,1 when headless).
    pub fn resize(&mut self, cols: u16, rows: u16, cell_w_px: u32, cell_h_px: u32) -> bool {
        unsafe { fxvt_resize(self.ctx, cols, rows, cell_w_px, cell_h_px) == 0 }
    }

    /// Take a render snapshot. Call before any read for the current frame.
    pub fn update(&mut self) -> bool {
        unsafe { fxvt_update(self.ctx) == 0 }
    }

    /// Snapshot viewport dimensions as `(cols, rows)`.
    pub fn dims(&self) -> Option<(u16, u16)> {
        let mut cols = 0u16;
        let mut rows = 0u16;
        let ok = unsafe { fxvt_dims(self.ctx, &mut cols, &mut rows) == 0 };
        ok.then_some((cols, rows))
    }

    /// Cursor position + visibility from the latest snapshot.
    pub fn cursor(&self) -> Option<Cursor> {
        let mut x = 0u16;
        let mut y = 0u16;
        let mut vis: c_int = 0;
        let ok = unsafe { fxvt_cursor(self.ctx, &mut x, &mut y, &mut vis) == 0 };
        ok.then_some(Cursor {
            x,
            y,
            visible: vis != 0,
        })
    }

    /// Set the terminal's default foreground/background/cursor colors so the
    /// rendered palette matches the host theme (the libghostty equivalent of
    /// VTE's `set_colors`/`set_color_cursor`).
    pub fn set_default_colors(&mut self, fg: Rgb, bg: Rgb, cursor: Rgb) -> bool {
        let f = [fg.r, fg.g, fg.b];
        let b = [bg.r, bg.g, bg.b];
        let c = [cursor.r, cursor.g, cursor.b];
        unsafe {
            fxvt_set_default_colors(self.ctx, f.as_ptr(), b.as_ptr(), c.as_ptr()) == 0
        }
    }

    /// Override the low palette entries (typically the 16 themeable ANSI
    /// colors) while keeping libghostty's standard xterm 16..256 fill, matching
    /// how VTE expands a small theme palette.
    pub fn set_palette(&mut self, palette: &[Rgb]) -> bool {
        let count = palette.len().min(256);
        let mut flat = Vec::with_capacity(count * 3);
        for c in &palette[..count] {
            flat.push(c.r);
            flat.push(c.g);
            flat.push(c.b);
        }
        unsafe { fxvt_set_palette(self.ctx, flat.as_ptr(), count as c_int) == 0 }
    }

    /// Default fg/bg/cursor colors from the latest snapshot. The renderer
    /// clears with `bg`, uses `fg` for default-colored text, and `cursor` for
    /// the cursor block.
    pub fn colors(&self) -> Option<Colors> {
        let mut fg = [0u8; 3];
        let mut bg = [0u8; 3];
        let mut cur = [0u8; 3];
        let mut has: c_int = 0;
        let ok = unsafe {
            fxvt_colors(
                self.ctx,
                fg.as_mut_ptr(),
                bg.as_mut_ptr(),
                cur.as_mut_ptr(),
                &mut has,
            ) == 0
        };
        ok.then_some(Colors {
            fg: Rgb {
                r: fg[0],
                g: fg[1],
                b: fg[2],
            },
            bg: Rgb {
                r: bg[0],
                g: bg[1],
                b: bg[2],
            },
            cursor: Rgb {
                r: cur[0],
                g: cur[1],
                b: cur[2],
            },
            cursor_has_value: has != 0,
        })
    }

    /// Read one cell at `(row, col)`. Returns `None` for out-of-range coords.
    pub fn cell(&self, row: u16, col: u16) -> Option<Cell> {
        let mut raw = FxvtCell::zeroed();
        let found = unsafe { fxvt_cell(self.ctx, row, col, &mut raw) == 1 };
        found.then(|| cell_from_raw(&raw))
    }

    /// Read the entire viewport grid in a single render pass (O(cols*rows)),
    /// returning a row-major `Vec` of `cols*rows` cells (blank where there is no
    /// content). This is the renderer's hot path — far cheaper than calling
    /// [`Vt::cell`] per cell, which re-seeks the row iterator every time.
    pub fn read_grid(&self, cols: u16, rows: u16) -> Vec<Cell> {
        let n = cols as usize * rows as usize;
        let mut raw = vec![FxvtCell::zeroed(); n];
        unsafe { fxvt_read_grid(self.ctx, raw.as_mut_ptr(), cols, rows) };
        raw.iter().map(cell_from_raw).collect()
    }

    /// Set the active selection to the inclusive viewport range
    /// `start..=end` (each `(col, row)`); `rectangle` selects a block. The next
    /// snapshot's per-cell `selected` flags reflect it.
    pub fn set_selection(&mut self, start: (u16, u16), end: (u16, u16), rectangle: bool) -> bool {
        unsafe {
            fxvt_set_selection(
                self.ctx,
                start.0,
                start.1 as u32,
                end.0,
                end.1 as u32,
                rectangle as c_int,
            ) == 0
        }
    }

    /// Clear any active selection.
    pub fn clear_selection(&mut self) {
        unsafe { fxvt_clear_selection(self.ctx) };
    }

    /// Scroll the viewport by `delta` rows through scrollback (up is negative).
    pub fn scroll(&mut self, delta: isize) {
        unsafe { fxvt_scroll(self.ctx, delta as c_long) };
    }

    /// Snap the viewport to the bottom (live cursor row). Call on input so that
    /// typing while scrolled up brings the view back; a no-op when already live.
    pub fn scroll_to_bottom(&mut self) {
        unsafe { fxvt_scroll_bottom(self.ctx) };
    }

    /// Encode a key press into terminal bytes via libghostty's key encoder,
    /// which honors the terminal's current modes (application cursor keys,
    /// keypad, Kitty keyboard protocol, Alt-as-ESC). `named_key` is a
    /// [`named_key`] code (0 for plain character keys); `unshifted_cp` is the
    /// base Unicode scalar (0 if none); `composing` is true while an IME
    /// preedit is active. Returns `None` when nothing should be sent.
    pub fn encode_key(
        &mut self,
        named_key: i32,
        unshifted_cp: u32,
        mods: u8,
        composing: bool,
    ) -> Option<Vec<u8>> {
        let mut buf = [0i8; 64];
        let n = unsafe {
            fxvt_encode_key(
                self.ctx,
                named_key as c_int,
                unshifted_cp,
                mods as c_int,
                composing as c_int,
                buf.as_mut_ptr(),
                buf.len(),
            )
        };
        if n == 0 {
            None
        } else {
            Some(buf[..n].iter().map(|&b| b as u8).collect())
        }
    }

    /// Scrollbar geometry as `(total, offset, len)`: total scrollable rows, the
    /// viewport's offset into them, and the visible row count. Maps to a
    /// scrollbar adjustment (value=offset, upper=total, page=len). Call only
    /// after output/scroll — it can be costly to compute.
    pub fn scrollbar(&self) -> Option<(u64, u64, u64)> {
        let mut total = 0u64;
        let mut offset = 0u64;
        let mut len = 0u64;
        let ok = unsafe { fxvt_scrollbar(self.ctx, &mut total, &mut offset, &mut len) == 0 };
        ok.then_some((total, offset, len))
    }

    /// The terminal's OSC 0/2 title, if one has been set.
    pub fn title(&self) -> Option<String> {
        read_c_string(|buf, cap| unsafe { fxvt_title(self.ctx, buf, cap) })
    }

    /// The terminal's working directory from OSC 7, if announced by the shell.
    pub fn pwd(&self) -> Option<String> {
        read_c_string(|buf, cap| unsafe { fxvt_pwd(self.ctx, buf, cap) })
    }

    /// Whether the foreground app has enabled mouse tracking (modes
    /// 1000/1002/1003). When true, pointer events should be reported to it via
    /// [`Vt::encode_mouse`] rather than driving local selection.
    pub fn mouse_enabled(&self) -> bool {
        unsafe { fxvt_mouse_enabled(self.ctx) != 0 }
    }

    /// Encode a mouse event (surface-pixel position) into the bytes the
    /// foreground app expects for its active mouse mode/format. Returns `None`
    /// when there is nothing to send.
    pub fn encode_mouse(
        &mut self,
        action: MouseAction,
        button: MouseButton,
        px: f64,
        py: f64,
        mods: u8,
    ) -> Option<Vec<u8>> {
        let mut buf = [0i8; 64];
        let n = unsafe {
            fxvt_encode_mouse(
                self.ctx,
                action as c_int,
                button as c_int,
                px,
                py,
                mods as c_int,
                buf.as_mut_ptr(),
                buf.len(),
            )
        };
        if n == 0 {
            None
        } else {
            Some(buf[..n].iter().map(|&b| b as u8).collect())
        }
    }

    /// Extract the currently selected text from the latest snapshot by walking
    /// the per-cell `selected` flags, joining selected runs row by row. Returns
    /// `None` when nothing is selected. Call [`Vt::update`] first.
    pub fn selection_text(&self) -> Option<String> {
        let (cols, rows) = self.dims()?;
        let mut out = String::new();
        let mut any = false;
        let mut pending_rows = 0usize; // newlines deferred until the next selected row
        for row in 0..rows {
            let mut line = String::new();
            let mut row_has = false;
            let mut prev_wide = false;
            for col in 0..cols {
                if let Some(cell) = self.cell(row, col) {
                    if cell.selected {
                        row_has = true;
                        if cell.text.is_empty() {
                            // Blank cell inside the selection → a space, so words
                            // separated by cleared cells don't run together on
                            // copy. Skip the spacer that trails a wide glyph.
                            if !prev_wide {
                                line.push(' ');
                            }
                        } else {
                            line.push_str(&cell.text);
                        }
                    }
                    prev_wide = cell.wide;
                } else {
                    prev_wide = false;
                }
            }
            if row_has {
                if any {
                    for _ in 0..=pending_rows {
                        out.push('\n');
                    }
                }
                // Trim trailing spaces from the selected run on this row.
                out.push_str(line.trim_end_matches(' '));
                any = true;
                pending_rows = 0;
            } else if any {
                pending_rows += 1;
            }
        }
        any.then_some(out)
    }

    /// The UTF-8 text of a whole viewport row, trailing blanks trimmed.
    pub fn row_text(&self, row: u16) -> String {
        // Cells hold at most 8 grapheme codepoints; a generous fixed cap keeps
        // this allocation-light while covering any realistic terminal width.
        let mut buf = vec![0i8; 8192];
        let n = unsafe { fxvt_row_text(self.ctx, row, buf.as_mut_ptr(), buf.len()) };
        let bytes: Vec<u8> = buf[..n].iter().map(|&b| b as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Drop for Vt {
    fn drop(&mut self) {
        unsafe { fxvt_free(self.ctx) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn updated(cols: u16, rows: u16) -> Vt {
        Vt::new(cols, rows, 100).expect("vt new")
    }

    #[test]
    fn read_grid_matches_per_cell_reads() {
        let mut vt = updated(20, 4);
        vt.write(b"hi \x1b[1;31mX\x1b[0m\r\n2nd ");
        vt.write("한글".as_bytes());
        assert!(vt.update());
        let (cols, rows) = vt.dims().unwrap();
        let grid = vt.read_grid(cols, rows);
        assert_eq!(grid.len(), cols as usize * rows as usize);
        // Spot-check a few cells against the per-cell path.
        for &(r, c) in &[(0u16, 0u16), (0, 3), (1, 0), (1, 4)] {
            let g = &grid[r as usize * cols as usize + c as usize];
            let single = vt.cell(r, c).unwrap();
            assert_eq!(g.text, single.text, "text mismatch at {r},{c}");
            assert_eq!(g.fg, single.fg, "fg mismatch at {r},{c}");
            assert_eq!(g.wide, single.wide, "wide mismatch at {r},{c}");
            assert_eq!(g.style.bold, single.style.bold, "bold mismatch at {r},{c}");
        }
        // Row 0 reconstructed from the grid reads "hi X".
        let row0: String = (0..cols)
            .map(|c| grid[c as usize].text.clone())
            .collect::<String>();
        assert!(row0.starts_with("hi X"), "row0 was {row0:?}");
    }

    #[test]
    fn writes_plain_text_into_first_row() {
        let mut vt = updated(20, 3);
        vt.write(b"hi there");
        assert!(vt.update());
        assert_eq!(vt.row_text(0), "hi there");
    }

    #[test]
    fn dims_reflect_construction_and_resize() {
        let mut vt = updated(20, 5);
        assert!(vt.update());
        assert_eq!(vt.dims(), Some((20, 5)));

        assert!(vt.resize(40, 10, 8, 16));
        assert!(vt.update());
        assert_eq!(vt.dims(), Some((40, 10)));
    }

    #[test]
    fn newline_advances_to_second_row() {
        let mut vt = updated(20, 3);
        // CRLF so the cursor returns to column 0 on the next line.
        vt.write(b"first\r\nsecond");
        assert!(vt.update());
        assert_eq!(vt.row_text(0), "first");
        assert_eq!(vt.row_text(1), "second");
    }

    #[test]
    fn cursor_tracks_written_text() {
        let mut vt = updated(20, 3);
        vt.write(b"abc");
        assert!(vt.update());
        let cur = vt.cursor().expect("cursor");
        // After "abc" the cursor sits at column 3 on row 0.
        assert_eq!((cur.x, cur.y), (3, 0));
        assert!(cur.visible);
    }

    #[test]
    fn sgr_bold_and_color_decode_into_cell_style() {
        let mut vt = updated(20, 3);
        // Bold (SGR 1) + red foreground (SGR 31), then 'X'.
        vt.write(b"\x1b[1;31mX\x1b[0m");
        assert!(vt.update());
        let cell = vt.cell(0, 0).expect("cell 0,0");
        assert_eq!(cell.text, "X");
        assert!(cell.style.bold, "bold flag should be set");
        // Red is the brightest channel; exact RGB depends on the palette.
        assert!(
            cell.fg.r > cell.fg.g && cell.fg.r > cell.fg.b,
            "fg should be reddish, got {:?}",
            cell.fg
        );
    }

    #[test]
    fn blank_cell_reads_empty_text() {
        let mut vt = updated(20, 3);
        vt.write(b"x");
        assert!(vt.update());
        // Column 5 was never written.
        let cell = vt.cell(0, 5).expect("cell 0,5 in range");
        assert_eq!(cell.text, "");
    }

    #[test]
    fn set_default_colors_overrides_theme_fg_bg() {
        let mut vt = updated(20, 3);
        let fg = Rgb { r: 0xab, g: 0xcd, b: 0xef };
        let bg = Rgb { r: 0x10, g: 0x20, b: 0x30 };
        let cursor = Rgb { r: 0xff, g: 0x88, b: 0x00 };
        assert!(vt.set_default_colors(fg, bg, cursor));
        vt.write(b"z");
        assert!(vt.update());
        let colors = vt.colors().expect("colors");
        assert_eq!(colors.fg, fg, "default fg should match the set value");
        assert_eq!(colors.bg, bg, "default bg should match the set value");
        // A default-colored cell now resolves to the themed fg.
        assert_eq!(vt.cell(0, 0).unwrap().fg, fg);
    }

    #[test]
    fn set_palette_recolors_indexed_ansi() {
        let mut vt = updated(20, 3);
        // Make ANSI red (index 1) a recognizable custom value.
        let mut palette = vec![Rgb { r: 0, g: 0, b: 0 }; 16];
        palette[1] = Rgb { r: 0x12, g: 0x34, b: 0x56 };
        assert!(vt.set_palette(&palette));
        // SGR 31 selects palette index 1 for the foreground.
        vt.write(b"\x1b[31mR\x1b[0m");
        assert!(vt.update());
        assert_eq!(vt.cell(0, 0).unwrap().fg, palette[1]);
    }

    #[test]
    fn default_colors_are_available_after_update() {
        let mut vt = updated(20, 3);
        vt.write(b"x");
        assert!(vt.update());
        let colors = vt.colors().expect("default colors");
        // A default terminal palette has a non-equal fg/bg (text is visible).
        assert_ne!(colors.fg, colors.bg, "fg and bg should differ");
    }

    #[test]
    fn explicit_bg_sets_has_bg_and_default_fg_is_seeded() {
        let mut vt = updated(20, 3);
        // Blue background (SGR 44), then 'Y'.
        vt.write(b"\x1b[44mY\x1b[0m");
        assert!(vt.update());
        let colors = vt.colors().expect("colors");
        let cell = vt.cell(0, 0).expect("cell 0,0");
        assert_eq!(cell.text, "Y");
        assert!(cell.bg.is_some(), "explicit bg should set has_bg");
        // No explicit fg => seeded with the terminal default foreground.
        assert_eq!(cell.fg, colors.fg);
    }

    #[test]
    fn encode_key_respects_application_cursor_mode() {
        let mut vt = updated(40, 5);
        // Normal mode: Up arrow → CSI A.
        assert_eq!(
            vt.encode_key(named_key::UP, 0, 0, false).as_deref(),
            Some(&b"\x1b[A"[..])
        );
        // Enable DECCKM (application cursor keys) as full-screen apps
        // (vim/claude/codex) do; Up arrow now → SS3 A.
        vt.write(b"\x1b[?1h");
        assert_eq!(
            vt.encode_key(named_key::UP, 0, 0, false).as_deref(),
            Some(&b"\x1bOA"[..]),
            "application cursor mode must switch arrows to SS3"
        );
    }

    #[test]
    fn encode_key_control_and_named_keys() {
        let mut vt = updated(40, 5);
        // Ctrl+C → 0x03.
        assert_eq!(
            vt.encode_key(named_key::NONE, 'c' as u32, MOD_CTRL, false).as_deref(),
            Some(&b"\x03"[..])
        );
        // Enter → CR; Tab → HT; Backspace → DEL.
        assert_eq!(vt.encode_key(named_key::ENTER, 0, 0, false).as_deref(), Some(&b"\r"[..]));
        assert_eq!(vt.encode_key(named_key::TAB, 0, 0, false).as_deref(), Some(&b"\t"[..]));
        assert_eq!(
            vt.encode_key(named_key::BACKSPACE, 0, 0, false).as_deref(),
            Some(&b"\x7f"[..])
        );
        // A composing key produces nothing (IME owns it).
        assert_eq!(vt.encode_key(named_key::NONE, 'k' as u32, 0, true), None);
    }

    #[test]
    fn osc_title_is_reported() {
        // The GhosttyPane has no title poller, so OSC 0/2 title tracking must
        // come through this query for tab titles to update.
        let mut vt = updated(40, 3);
        assert_eq!(vt.title(), None, "no title before any OSC");
        vt.write(b"\x1b]2;my-title\x07");
        assert_eq!(vt.title().as_deref(), Some("my-title"));
        // pwd() is best-effort (OSC 7); current_dir falls back to /proc, so we
        // only assert it does not error here.
        let _ = vt.pwd();
    }

    #[test]
    fn scrollbar_geometry_tracks_scrollback_and_position() {
        let mut vt = Vt::new(20, 5, 500).expect("vt");
        for i in 0..60 {
            vt.write(format!("line{i}\r\n").as_bytes());
        }
        assert!(vt.update());
        let (total, offset, len) = vt.scrollbar().expect("scrollbar");
        assert_eq!(len, 5, "visible len == viewport rows");
        assert!(total >= 60, "total covers scrollback, got {total}");
        // At the bottom the viewport sits at the end of the scrollable area.
        assert_eq!(offset, total - len, "live viewport pinned to bottom");

        // Scroll up: the offset must move toward the top.
        vt.scroll(-20);
        assert!(vt.update());
        let (_t2, offset2, _l2) = vt.scrollbar().expect("scrollbar");
        assert!(offset2 < offset, "scrolling up lowers the offset");

        // Snapping to bottom (as input does) returns the viewport to live.
        vt.scroll_to_bottom();
        assert!(vt.update());
        let (t3, offset3, len3) = vt.scrollbar().expect("scrollbar");
        assert_eq!(offset3, t3 - len3, "scroll_to_bottom pins back to live");
    }

    #[test]
    fn scroll_viewport_reveals_scrollback() {
        let mut vt = Vt::new(20, 5, 500).expect("vt");
        // Print 60 numbered lines so plenty scrolls into history.
        for i in 0..60 {
            vt.write(format!("line{i}\r\n").as_bytes());
        }
        assert!(vt.update());
        let bottom_row0 = vt.row_text(0);
        // Scroll up into history; row 0 should now show an earlier line.
        vt.scroll(-40);
        assert!(vt.update());
        let scrolled_row0 = vt.row_text(0);
        assert_ne!(
            bottom_row0, scrolled_row0,
            "scrolling up should reveal earlier scrollback"
        );
        // Back to bottom.
        vt.scroll(1000);
        assert!(vt.update());
        assert_eq!(vt.row_text(0), bottom_row0);
    }

    #[test]
    fn mouse_mode_toggles_with_dec_private_modes() {
        let mut vt = updated(20, 3);
        assert!(!vt.mouse_enabled(), "mouse tracking off by default");
        vt.write(b"\x1b[?1000h"); // enable normal mouse tracking
        assert!(vt.mouse_enabled(), "mode 1000 should enable tracking");
        vt.write(b"\x1b[?1000l"); // disable
        assert!(!vt.mouse_enabled(), "mode 1000 reset should disable tracking");
    }

    #[test]
    fn encode_mouse_emits_sgr_report_when_enabled() {
        let mut vt = updated(80, 24);
        // Size the cells so pixel->cell mapping is well-defined.
        assert!(vt.resize(80, 24, 8, 16));
        vt.write(b"\x1b[?1000h\x1b[?1006h"); // normal tracking + SGR format
        assert!(vt.mouse_enabled());
        let bytes = vt
            .encode_mouse(MouseAction::Press, MouseButton::Left, 8.0, 16.0, 0)
            .expect("a press should encode in SGR mode");
        // SGR mouse reports start with ESC [ < .
        assert!(
            bytes.starts_with(b"\x1b[<"),
            "expected SGR mouse report, got {bytes:?}"
        );
    }

    #[test]
    fn selection_marks_cells_and_extracts_text() {
        let mut vt = updated(40, 3);
        vt.write(b"hello world");
        assert!(vt.update());
        // Select "hello" (cols 0..=4 on row 0).
        assert!(vt.set_selection((0, 0), (4, 0), false));
        assert!(vt.update());
        assert!(vt.cell(0, 0).unwrap().selected, "first cell should be selected");
        assert!(vt.cell(0, 4).unwrap().selected, "last selected cell");
        assert!(!vt.cell(0, 6).unwrap().selected, "cell past selection is unselected");
        assert_eq!(vt.selection_text().as_deref(), Some("hello"));

        vt.clear_selection();
        assert!(vt.update());
        assert!(!vt.cell(0, 0).unwrap().selected, "selection cleared");
        assert_eq!(vt.selection_text(), None);
    }

    #[test]
    fn cjk_wide_grapheme_round_trips() {
        let mut vt = updated(20, 3);
        vt.write("한글".as_bytes());
        assert!(vt.update());
        // The lead cell of the first wide glyph carries the grapheme.
        let cell = vt.cell(0, 0).expect("cell 0,0");
        assert_eq!(cell.text, "한");
        assert!(cell.wide, "CJK glyph should be marked wide");
    }
}
