// SPDX-License-Identifier: GPL-3.0-or-later
//! Render snapshot: a flat, owned, GTK-friendly view of the terminal grid.
//!
//! The GTK renderer must not hold the [`Term`] lock while it builds a
//! `GskRenderNode` tree, and it must not re-resolve palette colors per
//! frame. This module locks the term once, resolves every visible cell's
//! foreground/background to a concrete RGB triple (named → palette,
//! indexed → palette/256-cube, spec → literal), applies `INVERSE`/`DIM`,
//! and returns an owned [`FrameSnapshot`].
//!
//! Color resolution lived in the GTK widget last time and "never settled
//! cleanly" (see `docs/pure-rust-terminal-migration.md`). Here it is pure
//! and unit-tested so the renderer can stay dumb.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Point;
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::Term;
pub use alacritty_terminal::vte::ansi::CursorShape;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

/// 8-bit RGB triple ready for `gdk::RGBA` / GSK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl CellColor {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
    pub const fn from_rgb(rgb: Rgb) -> Self {
        Self {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        }
    }
    /// Scale toward black for the `DIM` attribute (~⅔ brightness, the
    /// conventional SGR 2 rendering).
    const fn dimmed(self) -> Self {
        Self {
            r: (self.r as u16 * 2 / 3) as u8,
            g: (self.g as u16 * 2 / 3) as u8,
            b: (self.b as u16 * 2 / 3) as u8,
        }
    }
}

/// One resolved cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyledCell {
    pub ch: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikeout: bool,
    /// East-Asian wide glyph occupying two columns.
    pub wide: bool,
    /// The trailing column owned by a wide glyph (render nothing).
    pub wide_spacer: bool,
    /// Covered by the active selection.
    pub selected: bool,
}

impl StyledCell {
    fn blank(fg: CellColor, bg: CellColor) -> Self {
        Self {
            ch: ' ',
            fg,
            bg,
            bold: false,
            italic: false,
            underline: false,
            strikeout: false,
            wide: false,
            wide_spacer: false,
            selected: false,
        }
    }
}

/// The user's resolved theme: default fg/bg/cursor plus the 16 ANSI
/// colors. Used as the fallback when the terminal's own palette has not
/// set a slot (apps may still override at runtime via OSC 4, which lands
/// in the live `Colors` and takes precedence). Mirrors `ResolvedTheme`
/// in the GUI crate.
#[derive(Debug, Clone, Copy)]
pub struct ThemePalette {
    pub fg: CellColor,
    pub bg: CellColor,
    pub cursor: CellColor,
    pub ansi: [CellColor; 16],
    /// Selection highlight; `None` → renderer picks a default.
    pub selection_bg: Option<CellColor>,
    pub selection_fg: Option<CellColor>,
}

impl Default for ThemePalette {
    /// Standard xterm/VGA defaults — matches the historical
    /// `default_indexed` table so behavior is unchanged with no theme.
    fn default() -> Self {
        let mut ansi = [CellColor::new(0, 0, 0); 16];
        const BASE: [(u8, u8, u8); 16] = [
            (0x00, 0x00, 0x00),
            (0x80, 0x00, 0x00),
            (0x00, 0x80, 0x00),
            (0x80, 0x80, 0x00),
            (0x00, 0x00, 0x80),
            (0x80, 0x00, 0x80),
            (0x00, 0x80, 0x80),
            (0xc0, 0xc0, 0xc0),
            (0x80, 0x80, 0x80),
            (0xff, 0x00, 0x00),
            (0x00, 0xff, 0x00),
            (0xff, 0xff, 0x00),
            (0x00, 0x00, 0xff),
            (0xff, 0x00, 0xff),
            (0x00, 0xff, 0xff),
            (0xff, 0xff, 0xff),
        ];
        let mut i = 0;
        while i < 16 {
            ansi[i] = CellColor::new(BASE[i].0, BASE[i].1, BASE[i].2);
            i += 1;
        }
        Self {
            fg: CellColor::new(0xd0, 0xd0, 0xd0),
            bg: CellColor::new(0x0a, 0x0a, 0x0a),
            cursor: CellColor::new(0xd0, 0xd0, 0xd0),
            ansi,
            selection_bg: None,
            selection_fg: None,
        }
    }
}

/// Cursor placement in viewport coordinates (0 = top visible row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorState {
    pub line: usize,
    pub col: usize,
    pub shape: CursorShape,
}

/// Owned snapshot of one frame, row-major (`cells[line * cols + col]`).
#[derive(Debug, Clone)]
pub struct FrameSnapshot {
    pub rows: usize,
    pub cols: usize,
    pub cells: Vec<StyledCell>,
    /// `None` when the cursor is hidden / off-screen.
    pub cursor: Option<CursorState>,
    /// Caret cell `(line, col)` — present even when the cursor is hidden,
    /// so an IME preedit can anchor there (apps like Claude Code hide the
    /// cursor mid-prompt; the composing Hangul syllable must still show).
    pub caret: Option<(usize, usize)>,
    pub default_fg: CellColor,
    pub default_bg: CellColor,
    pub cursor_color: CellColor,
    pub selection_bg: Option<CellColor>,
    pub selection_fg: Option<CellColor>,
}

impl FrameSnapshot {
    pub fn cell(&self, line: usize, col: usize) -> Option<&StyledCell> {
        self.cells.get(line * self.cols + col)
    }
}

/// Resolve a terminal [`Color`] to a concrete RGB. `palette` is the live
/// term palette (honors app OSC 4 changes); unset slots fall back to the
/// user's `theme` (and, for indexed 16..255, the 256-color cube).
fn resolve(color: Color, palette: &Colors, theme: &ThemePalette) -> CellColor {
    match color {
        Color::Spec(rgb) => CellColor::from_rgb(rgb),
        Color::Named(named) => palette[named]
            .map(CellColor::from_rgb)
            .unwrap_or_else(|| default_named(named, theme)),
        Color::Indexed(idx) => palette[idx as usize]
            .map(CellColor::from_rgb)
            .unwrap_or_else(|| default_indexed(idx, theme)),
    }
}

/// Build an owned snapshot from a locked terminal. Caller holds the lock.
pub fn snapshot<T: EventListener>(term: &Term<T>, theme: &ThemePalette) -> FrameSnapshot {
    let content = term.renderable_content();
    let palette = content.colors;
    let default_fg = resolve(Color::Named(NamedColor::Foreground), palette, theme);
    let default_bg = resolve(Color::Named(NamedColor::Background), palette, theme);
    let cursor_color = palette[NamedColor::Cursor]
        .map(CellColor::from_rgb)
        .unwrap_or(theme.cursor);
    let selection = content.selection;

    let grid = term.grid();
    let rows = grid.screen_lines();
    let cols = grid.columns();
    let mut cells = vec![StyledCell::blank(default_fg, default_bg); rows * cols];

    // `display_iter` yields *absolute* grid lines: history is negative,
    // the active screen is `0..rows`. When scrolled back, `display_offset`
    // is how far up we are, so the viewport row is `line + display_offset`
    // (alacritty's own `point_to_viewport`). Mapping by the raw absolute
    // line instead dropped every history row and left the bottom blank.
    let display_offset = content.display_offset as i32;
    for indexed in content.display_iter {
        let point: Point = indexed.point;
        let row = point.line.0 + display_offset;
        if row < 0 {
            continue;
        }
        let line = row as usize;
        let col = point.column.0;
        if line >= rows || col >= cols {
            continue;
        }

        let cell = indexed.cell;
        let flags = cell.flags;
        let mut fg = resolve(cell.fg, palette, theme);
        let mut bg = resolve(cell.bg, palette, theme);
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if flags.contains(Flags::DIM) {
            fg = fg.dimmed();
        }
        let selected = selection.is_some_and(|r| point_in_range(point, r));
        cells[line * cols + col] = StyledCell {
            ch: cell.c,
            fg,
            bg,
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            underline: flags.contains(Flags::UNDERLINE),
            strikeout: flags.contains(Flags::STRIKEOUT),
            wide: flags.contains(Flags::WIDE_CHAR),
            wide_spacer: flags.contains(Flags::WIDE_CHAR_SPACER),
            selected,
        };
    }

    let caret = {
        // The cursor point is absolute too; map it into the viewport and
        // drop it when scrolled out of view (off the bottom).
        let line = content.cursor.point.line.0 + display_offset;
        (line >= 0 && (line as usize) < rows).then(|| {
            (
                line as usize,
                content.cursor.point.column.0.min(cols.saturating_sub(1)),
            )
        })
    };
    let cursor = {
        let c = content.cursor;
        if matches!(c.shape, CursorShape::Hidden) {
            None
        } else {
            caret.map(|(line, col)| CursorState {
                line,
                col,
                shape: c.shape,
            })
        }
    };

    FrameSnapshot {
        rows,
        cols,
        cells,
        cursor,
        caret,
        default_fg,
        default_bg,
        cursor_color,
        selection_bg: theme.selection_bg,
        selection_fg: theme.selection_fg,
    }
}

/// Whether a grid point falls inside a selection range. `start` is the
/// top-left, `end` the bottom-right (alacritty guarantees this ordering).
fn point_in_range(point: Point, range: SelectionRange) -> bool {
    if range.is_block {
        point.line >= range.start.line
            && point.line <= range.end.line
            && point.column >= range.start.column
            && point.column <= range.end.column
    } else {
        let after_start = point.line > range.start.line
            || (point.line == range.start.line && point.column >= range.start.column);
        let before_end = point.line < range.end.line
            || (point.line == range.end.line && point.column <= range.end.column);
        after_start && before_end
    }
}

/// Default for a named palette slot when the term has not set one,
/// resolved against the user's theme.
fn default_named(named: NamedColor, theme: &ThemePalette) -> CellColor {
    use NamedColor::*;
    match named {
        Foreground | BrightForeground => theme.fg,
        Background => theme.bg,
        Cursor => theme.cursor,
        DimForeground => theme.fg.dimmed(),
        Black => theme.ansi[0],
        Red => theme.ansi[1],
        Green => theme.ansi[2],
        Yellow => theme.ansi[3],
        Blue => theme.ansi[4],
        Magenta => theme.ansi[5],
        Cyan => theme.ansi[6],
        White => theme.ansi[7],
        BrightBlack => theme.ansi[8],
        BrightRed => theme.ansi[9],
        BrightGreen => theme.ansi[10],
        BrightYellow => theme.ansi[11],
        BrightBlue => theme.ansi[12],
        BrightMagenta => theme.ansi[13],
        BrightCyan => theme.ansi[14],
        BrightWhite => theme.ansi[15],
        DimBlack => theme.ansi[0].dimmed(),
        DimRed => theme.ansi[1].dimmed(),
        DimGreen => theme.ansi[2].dimmed(),
        DimYellow => theme.ansi[3].dimmed(),
        DimBlue => theme.ansi[4].dimmed(),
        DimMagenta => theme.ansi[5].dimmed(),
        DimCyan => theme.ansi[6].dimmed(),
        DimWhite => theme.ansi[7].dimmed(),
    }
}

/// 256-color table: 0–15 from the theme, 16–231 6×6×6 cube, 232–255
/// grayscale ramp.
fn default_indexed(idx: u8, theme: &ThemePalette) -> CellColor {
    match idx {
        0..=15 => theme.ansi[idx as usize],
        16..=231 => {
            let i = idx - 16;
            let to = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
            CellColor::new(to(i / 36), to((i / 6) % 6), to(i % 6))
        }
        232..=255 => {
            let v = (idx - 232) * 10 + 8;
            CellColor::new(v, v, v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_palette() -> Colors {
        Colors::default()
    }

    fn theme() -> ThemePalette {
        ThemePalette::default()
    }

    #[test]
    fn spec_color_is_literal() {
        let p = empty_palette();
        assert_eq!(
            resolve(Color::Spec(Rgb { r: 1, g: 2, b: 3 }), &p, &theme()),
            CellColor::new(1, 2, 3)
        );
    }

    #[test]
    fn indexed_cube_and_grayscale_match_xterm() {
        let t = theme();
        // 196 = pure red in the cube.
        assert_eq!(default_indexed(196, &t), CellColor::new(0xff, 0, 0));
        // 16 = cube origin (black).
        assert_eq!(default_indexed(16, &t), CellColor::new(0, 0, 0));
        // 231 = cube white.
        assert_eq!(default_indexed(231, &t), CellColor::new(0xff, 0xff, 0xff));
        // 232 = darkest gray.
        assert_eq!(default_indexed(232, &t), CellColor::new(8, 8, 8));
    }

    #[test]
    fn named_ansi_maps_to_theme_table() {
        let p = empty_palette();
        let t = theme();
        assert_eq!(
            resolve(Color::Named(NamedColor::Red), &p, &t),
            CellColor::new(0x80, 0, 0)
        );
        assert_eq!(
            resolve(Color::Named(NamedColor::BrightGreen), &p, &t),
            CellColor::new(0, 0xff, 0)
        );
    }

    #[test]
    fn theme_overrides_named_fallback() {
        let p = empty_palette();
        let mut t = theme();
        t.ansi[1] = CellColor::new(0xab, 0xcd, 0xef);
        assert_eq!(
            resolve(Color::Named(NamedColor::Red), &p, &t),
            CellColor::new(0xab, 0xcd, 0xef)
        );
    }

    #[test]
    fn dim_darkens() {
        let c = CellColor::new(0xff, 0x60, 0x00);
        assert_eq!(c.dimmed(), CellColor::new(0xaa, 0x40, 0x00));
    }

    #[test]
    fn selection_membership_linear_and_block() {
        use alacritty_terminal::index::{Column, Line};
        let pt = |l: i32, c: usize| Point::new(Line(l), Column(c));
        // Linear selection from (1,3) to (3,2): wraps across rows.
        let lin = SelectionRange::new(pt(1, 3), pt(3, 2), false);
        assert!(super::point_in_range(pt(1, 3), lin));
        assert!(super::point_in_range(pt(2, 99), lin)); // middle row, any col
        assert!(super::point_in_range(pt(3, 2), lin));
        assert!(!super::point_in_range(pt(1, 2), lin)); // before start col
        assert!(!super::point_in_range(pt(3, 3), lin)); // after end col
        assert!(!super::point_in_range(pt(0, 5), lin)); // above

        // Block selection: column window on every row.
        let blk = SelectionRange::new(pt(1, 3), pt(3, 6), true);
        assert!(super::point_in_range(pt(2, 4), blk));
        assert!(!super::point_in_range(pt(2, 7), blk)); // outside col window
        assert!(!super::point_in_range(pt(2, 2), blk));
    }

    #[test]
    fn scrolled_snapshot_maps_history_into_viewport() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::grid::Scroll;
        use alacritty_terminal::term::test::TermSize;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::Processor;

        // 5-row screen, feed 20 numbered lines so 15 land in scrollback.
        let size = TermSize::new(10, 5);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        // No trailing newline, or the cursor lands on a blank 21st line and
        // the bottom screen row would be empty.
        let feed = (0..20)
            .map(|i| format!("L{i:02}"))
            .collect::<Vec<_>>()
            .join("\r\n");
        parser.advance(&mut term, feed.as_bytes());

        let row_text = |frame: &FrameSnapshot, r: usize| -> String {
            (0..frame.cols)
                .map(|c| frame.cells[r * frame.cols + c].ch)
                .collect::<String>()
                .trim_end()
                .to_string()
        };

        // Pinned to the bottom: last 5 lines visible, no blanks.
        let frame = snapshot(&term, &theme());
        assert_eq!(row_text(&frame, 0), "L15");
        assert_eq!(row_text(&frame, 4), "L19");

        // Scroll up 3 → viewport shows L12..L16. The bug skipped the
        // history rows and left the bottom blank; assert every row is the
        // right history line instead.
        term.scroll_display(Scroll::Delta(3));
        let frame = snapshot(&term, &theme());
        for (r, want) in ["L12", "L13", "L14", "L15", "L16"].iter().enumerate() {
            assert_eq!(&row_text(&frame, r), want, "row {r} mismatch");
        }
    }

    #[test]
    fn live_palette_overrides_theme() {
        let mut p = empty_palette();
        p[NamedColor::Red] = Some(Rgb {
            r: 0x12,
            g: 0x34,
            b: 0x56,
        });
        assert_eq!(
            resolve(Color::Named(NamedColor::Red), &p, &theme()),
            CellColor::new(0x12, 0x34, 0x56)
        );
    }
}
