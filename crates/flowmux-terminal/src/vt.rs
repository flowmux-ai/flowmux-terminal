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

use std::os::raw::{c_char, c_int};

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

extern "C" {
    fn fxvt_new(cols: u16, rows: u16, scrollback: usize) -> *mut FxvtCtx;
    fn fxvt_free(ctx: *mut FxvtCtx);
    fn fxvt_write(ctx: *mut FxvtCtx, data: *const u8, len: usize);
    fn fxvt_resize(ctx: *mut FxvtCtx, cols: u16, rows: u16, cw: u32, ch: u32) -> c_int;
    fn fxvt_update(ctx: *mut FxvtCtx) -> c_int;
    fn fxvt_dims(ctx: *mut FxvtCtx, cols: *mut u16, rows: *mut u16) -> c_int;
    fn fxvt_cursor(ctx: *mut FxvtCtx, x: *mut u16, y: *mut u16, vis: *mut c_int) -> c_int;
    fn fxvt_cell(ctx: *mut FxvtCtx, row: u16, col: u16, out: *mut FxvtCell) -> c_int;
    fn fxvt_row_text(ctx: *mut FxvtCtx, row: u16, buf: *mut c_char, cap: usize) -> usize;
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

    /// Read one cell at `(row, col)`. Returns `None` for out-of-range coords.
    pub fn cell(&self, row: u16, col: u16) -> Option<Cell> {
        let mut raw = FxvtCell {
            codepoints: [0; 8],
            cp_len: 0,
            fg: [0; 3],
            bg: [0; 3],
            has_bg: 0,
            flags: 0,
            selected: 0,
            wide: 0,
            _pad: [0; 2],
        };
        let found = unsafe { fxvt_cell(self.ctx, row, col, &mut raw) == 1 };
        if !found {
            return None;
        }

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

        Some(Cell {
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
        })
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
