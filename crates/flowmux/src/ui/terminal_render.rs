// SPDX-License-Identifier: GPL-3.0-or-later
//! GTK4 terminal renderer widget for the pure-Rust backend.
//!
//! Renders a [`FrameSnapshot`] (resolved cells from `flowmux-terminal`)
//! into the widget via GTK4's `gtk::Snapshot` / `GskRenderNode` API.
//!
//! ## Why this shape (the rollback lesson)
//!
//! The previous non-VTE renderer (`557ffb2`, reverted by `21b8ea9`)
//! rebuilt the entire `O(rows × cols)` node tree every frame and let
//! glyphs flow through one pango layout, so cell advance drifted and
//! heavy TUIs lagged on weak hosts. This renderer fixes both:
//!
//! * **Per-row node cache.** Each visible row's background+text is built
//!   into one cached `gsk::RenderNode`. A row is rebuilt only when its
//!   resolved cells actually change (diffed against the previous frame).
//!   Clean rows reuse their cached node — a scrolling TUI that only
//!   touches a few rows per frame does O(changed-rows) work, not O(rows).
//! * **Cell-locked geometry.** Text is drawn in *runs* of contiguous
//!   narrow same-style cells positioned at `col * cell_w`; East-Asian
//!   wide glyphs are drawn individually over a two-cell span. Advance is
//!   never inherited from pango's flow, so it cannot drift.

use flowmux_terminal::render::{CellColor, CursorState, FrameSnapshot, StyledCell};
// CellColor used by eff_bg/eff_fg + DEFAULT_SELECTION_BG.
use gtk::gdk;
use gtk::glib;
use gtk::graphene;
use gtk::pango;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

/// Monospace cell geometry derived from the active font.
#[derive(Clone, Debug)]
pub struct FontMetrics {
    pub desc: pango::FontDescription,
    pub cell_w: f32,
    pub cell_h: f32,
    /// Baseline offset from the top of the cell, in pixels.
    pub ascent: f32,
}

fn rgba(c: CellColor) -> gdk::RGBA {
    gdk::RGBA::new(
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        1.0,
    )
}

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    pub struct TerminalRenderArea {
        pub(super) frame: RefCell<Option<FrameSnapshot>>,
        pub(super) font: RefCell<Option<FontMetrics>>,
        /// One cached render node per visible row (`None` = dirty).
        pub(super) row_cache: RefCell<Vec<Option<gtk::gsk::RenderNode>>>,
        /// Previous frame's cells per row, to detect which rows changed.
        pub(super) prev_rows: RefCell<Vec<Vec<StyledCell>>>,
        /// Fires `(cols, rows)` when the allocation maps to a new grid size.
        pub(super) resize_fn: RefCell<Option<Box<dyn Fn(u16, u16)>>>,
        pub(super) last_alloc: Cell<(i32, i32)>,
        /// Active IME preedit (composing) text, drawn inline at the caret.
        pub(super) preedit: RefCell<String>,
        /// Cursor blink phase: `true` = draw the cursor this frame. Toggled
        /// by a timer; forced back to `true` on activity so it stays solid
        /// while typing / output is flowing.
        pub(super) blink_on: Cell<bool>,
        /// `true` while a mouse-selection drag is in progress. The cursor is
        /// hidden for the duration so the block cursor can't shimmer over the
        /// drag's end cell; cleared the instant the button is released.
        pub(super) selecting: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TerminalRenderArea {
        const NAME: &'static str = "FlowmuxTerminalRenderArea";
        type Type = super::TerminalRenderArea;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for TerminalRenderArea {}

    impl WidgetImpl for TerminalRenderArea {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let frame_ref = self.frame.borrow();
            let font_ref = self.font.borrow();
            let (Some(frame), Some(font)) = (frame_ref.as_ref(), font_ref.as_ref()) else {
                return;
            };

            let width = obj.width() as f32;
            let height = obj.height() as f32;

            // Default background fills the whole widget first.
            snapshot.append_color(
                &rgba(frame.default_bg),
                &graphene::Rect::new(0.0, 0.0, width, height),
            );

            let mut cache = self.row_cache.borrow_mut();
            if cache.len() != frame.rows {
                cache.clear();
                cache.resize_with(frame.rows, || None);
            }

            for line in 0..frame.rows {
                if cache[line].is_none() {
                    cache[line] = super::build_row_node(&obj, frame, font, line);
                }
                if let Some(node) = &cache[line] {
                    snapshot.save();
                    snapshot.translate(&graphene::Point::new(0.0, line as f32 * font.cell_h));
                    snapshot.append_node(node);
                    snapshot.restore();
                }
            }

            if let Some(cursor) = frame.cursor {
                // Hide during an active drag; otherwise blink only while
                // focused (an unfocused pane shows a steady cursor, never a
                // blinking one).
                let blink = if obj.has_focus() {
                    self.blink_on.get()
                } else {
                    true
                };
                if blink && !self.selecting.get() {
                    super::draw_cursor(snapshot, frame, font, cursor);
                }
            }

            // IME preedit: always drawn at the caret, even when the app
            // hid the cursor — the composing Hangul syllable must be
            // visible (the old VTE hidden-cursor preedit bug cannot recur).
            let preedit = self.preedit.borrow();
            if !preedit.is_empty() {
                if let Some((line, col)) = frame.caret {
                    super::draw_preedit(&obj, snapshot, frame, font, line, col, &preedit);
                }
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            let prev = self.last_alloc.get();
            if prev == (width, height) {
                return;
            }
            self.last_alloc.set((width, height));
            let font_ref = self.font.borrow();
            let Some(font) = font_ref.as_ref() else {
                return;
            };
            if font.cell_w <= 0.0 || font.cell_h <= 0.0 {
                return;
            }
            let cols = (width as f32 / font.cell_w).floor().max(1.0) as u16;
            let rows = (height as f32 / font.cell_h).floor().max(1.0) as u16;
            let cb = self.resize_fn.borrow();
            if let Some(f) = cb.as_ref() {
                f(cols, rows);
            }
        }
    }
}

glib::wrapper! {
    pub struct TerminalRenderArea(ObjectSubclass<imp::TerminalRenderArea>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for TerminalRenderArea {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalRenderArea {
    pub fn new() -> Self {
        let obj: Self = glib::Object::new();
        obj.set_focusable(true);
        obj.imp().blink_on.set(true);
        obj
    }

    /// Flip the cursor blink phase and repaint. Driven by a ~530 ms timer.
    pub fn toggle_blink(&self) {
        let imp = self.imp();
        imp.blink_on.set(!imp.blink_on.get());
        self.queue_draw();
    }

    /// Force the cursor solid (blink phase on). Called on keypress / output so
    /// the cursor never blinks away mid-activity.
    pub fn reset_blink(&self) {
        if !self.imp().blink_on.replace(true) {
            self.queue_draw();
        }
    }

    /// Mark whether a mouse-selection drag is in progress (hides the cursor).
    pub fn set_selecting(&self, selecting: bool) {
        if self.imp().selecting.replace(selecting) != selecting {
            self.queue_draw();
        }
    }

    /// Set the font and recompute cell geometry. Clears the row cache.
    pub fn set_font(&self, family: &str, size_pt: f64) {
        let mut desc = pango::FontDescription::new();
        desc.set_family(family);
        desc.set_size((size_pt * pango::SCALE as f64) as i32);

        let ctx = self.pango_context();
        let metrics = ctx.metrics(Some(&desc), None);
        let scale = pango::SCALE as f32;
        let cell_w = metrics.approximate_digit_width() as f32 / scale;
        let ascent = metrics.ascent() as f32 / scale;
        let descent = metrics.descent() as f32 / scale;
        let cell_h = ascent + descent;

        *self.imp().font.borrow_mut() = Some(FontMetrics {
            desc,
            cell_w: cell_w.max(1.0),
            cell_h: cell_h.max(1.0),
            ascent,
        });
        self.invalidate_all();
    }

    /// Current cell geometry, if a font is set.
    pub fn font_metrics(&self) -> Option<FontMetrics> {
        self.imp().font.borrow().clone()
    }

    /// Replace the frame. Only rows whose resolved cells changed are
    /// invalidated, so a localized update redraws a few rows.
    pub fn set_frame(&self, frame: FrameSnapshot) {
        let imp = self.imp();
        {
            let mut cache = imp.row_cache.borrow_mut();
            let mut prev = imp.prev_rows.borrow_mut();
            if cache.len() != frame.rows {
                cache.clear();
                cache.resize_with(frame.rows, || None);
                prev.clear();
                prev.resize_with(frame.rows, Vec::new);
            }
            for line in 0..frame.rows {
                let start = line * frame.cols;
                let end = start + frame.cols;
                let row = &frame.cells[start..end];
                if prev[line].as_slice() != row {
                    cache[line] = None;
                    prev[line] = row.to_vec();
                }
            }
        }
        *imp.frame.borrow_mut() = Some(frame);
        // Fresh output / cursor movement: keep the cursor solid through it.
        imp.blink_on.set(true);
        self.queue_draw();
    }

    /// Drop all cached row nodes (font/size/theme change).
    pub fn invalidate_all(&self) {
        let imp = self.imp();
        imp.row_cache.borrow_mut().clear();
        imp.prev_rows.borrow_mut().clear();
        self.queue_draw();
    }

    /// Fire `(cols, rows)` whenever the allocation maps to a new grid size.
    pub fn connect_grid_resize<F>(&self, f: F)
    where
        F: Fn(u16, u16) + 'static,
    {
        *self.imp().resize_fn.borrow_mut() = Some(Box::new(f));
    }

    /// Set (or clear) the inline IME preedit text and repaint.
    pub fn set_preedit(&self, text: &str) {
        {
            let mut p = self.imp().preedit.borrow_mut();
            if *p == text {
                return;
            }
            p.clear();
            p.push_str(text);
        }
        self.queue_draw();
    }
}

/// Build one row's background+text into a render node, or `None` if empty.
fn build_row_node(
    widget: &TerminalRenderArea,
    frame: &FrameSnapshot,
    font: &FontMetrics,
    line: usize,
) -> Option<gtk::gsk::RenderNode> {
    let cols = frame.cols;
    let row_snapshot = gtk::Snapshot::new();
    let cell_w = font.cell_w;
    let cell_h = font.cell_h;

    // --- Background: batch contiguous cells with the same bg that differ
    // from the default (default bg is painted once by the widget). ---
    let mut col = 0usize;
    while col < cols {
        let cell = &frame.cells[line * cols + col];
        if cell.wide_spacer {
            col += 1;
            continue;
        }
        let span = if cell.wide { 2 } else { 1 };
        let bg = eff_bg(cell, frame);
        if bg != frame.default_bg {
            // Extend the run over identical-bg neighbours.
            let mut run_cols = span;
            let mut peek = col + span;
            while peek < cols {
                let nc = &frame.cells[line * cols + peek];
                if nc.wide_spacer {
                    peek += 1;
                    continue;
                }
                if eff_bg(nc, frame) != bg {
                    break;
                }
                run_cols += if nc.wide { 2 } else { 1 };
                peek += if nc.wide { 2 } else { 1 };
            }
            row_snapshot.append_color(
                &rgba(bg),
                &graphene::Rect::new(col as f32 * cell_w, 0.0, run_cols as f32 * cell_w, cell_h),
            );
            col = peek;
        } else {
            col += span;
        }
    }

    // --- Text: runs of contiguous narrow cells sharing fg+style render
    // as one layout at the run's x; wide glyphs render individually. ---
    let mut col = 0usize;
    while col < cols {
        let cell = &frame.cells[line * cols + col];
        if cell.wide_spacer || cell.ch == ' ' || cell.ch == '\0' {
            col += 1;
            continue;
        }
        if cell.wide {
            append_text_run(
                widget,
                &row_snapshot,
                font,
                col as f32 * cell_w,
                &cell.ch.to_string(),
                cell,
                eff_fg(cell, frame),
            );
            col += 2;
            continue;
        }
        // Narrow run.
        let start = col;
        let mut text = String::new();
        loop {
            let c = &frame.cells[line * cols + col];
            if c.wide || c.wide_spacer || !same_text_style(c, cell, frame) || c.ch == '\0' {
                break;
            }
            text.push(if c.ch == '\0' { ' ' } else { c.ch });
            col += 1;
            if col >= cols {
                break;
            }
        }
        if text.trim_end().is_empty() {
            continue;
        }
        append_text_run(
            widget,
            &row_snapshot,
            font,
            start as f32 * cell_w,
            &text,
            cell,
            eff_fg(cell, frame),
        );
    }

    row_snapshot.to_node()
}

/// Default selection highlight when the theme leaves it unset.
const DEFAULT_SELECTION_BG: CellColor = CellColor::new(0x3a, 0x3f, 0x55);

/// Background a cell actually renders with — selection overrides cell bg.
fn eff_bg(cell: &StyledCell, frame: &FrameSnapshot) -> CellColor {
    if cell.selected {
        frame.selection_bg.unwrap_or(DEFAULT_SELECTION_BG)
    } else {
        cell.bg
    }
}

/// Foreground a cell actually renders with — selection may recolor text.
fn eff_fg(cell: &StyledCell, frame: &FrameSnapshot) -> CellColor {
    if cell.selected {
        frame.selection_fg.unwrap_or(cell.fg)
    } else {
        cell.fg
    }
}

/// Whether two cells can share one pango layout run. Keys on the *effective*
/// fg, not the raw `selected` flag: when the theme leaves `selection_fg`
/// unset (the common case), a selected and an unselected cell of the same
/// color stay in one run. Splitting on `selected` re-laid-out the two halves
/// independently as the selection grew, so glyphs shifted by sub-pixel
/// kerning and the text appeared to ripple under the drag.
fn same_text_style(a: &StyledCell, b: &StyledCell, frame: &FrameSnapshot) -> bool {
    eff_fg(a, frame) == eff_fg(b, frame)
        && a.bold == b.bold
        && a.italic == b.italic
        && a.underline == b.underline
        && a.strikeout == b.strikeout
}

fn append_text_run(
    widget: &TerminalRenderArea,
    snapshot: &gtk::Snapshot,
    font: &FontMetrics,
    x: f32,
    text: &str,
    style: &StyledCell,
    fg: CellColor,
) {
    let layout = widget.create_pango_layout(Some(text));
    let mut desc = font.desc.clone();
    if style.bold {
        desc.set_weight(pango::Weight::Bold);
    }
    if style.italic {
        desc.set_style(pango::Style::Italic);
    }
    layout.set_font_description(Some(&desc));
    if style.underline || style.strikeout {
        let attrs = pango::AttrList::new();
        if style.underline {
            attrs.insert(pango::AttrInt::new_underline(pango::Underline::Single));
        }
        if style.strikeout {
            attrs.insert(pango::AttrInt::new_strikethrough(true));
        }
        layout.set_attributes(Some(&attrs));
    }
    snapshot.save();
    // Baseline-align every run to the cell's ascent. pango draws from the
    // layout's top; the distance from that top to the text baseline is the
    // run font's own ascent, which differs between the primary monospace
    // font and the CJK fallback used for Hangul. Translating by
    // `cell_ascent - run_baseline` puts every run's baseline on the same
    // line, so Latin and Hangul no longer sit at different heights.
    let run_baseline = layout.baseline() as f32 / pango::SCALE as f32;
    snapshot.translate(&graphene::Point::new(x, font.ascent - run_baseline));
    snapshot.append_layout(&layout, &rgba(fg));
    snapshot.restore();
}

/// Draw the IME preedit string starting at the caret cell: a highlighted,
/// underlined run over the cells it occupies, on top of the grid.
fn draw_preedit(
    widget: &TerminalRenderArea,
    snapshot: &gtk::Snapshot,
    frame: &FrameSnapshot,
    font: &FontMetrics,
    line: usize,
    col: usize,
    text: &str,
) {
    let x = col as f32 * font.cell_w;
    let y = line as f32 * font.cell_h;
    // Width in cells: account for wide (CJK) glyphs counting as two.
    let cells: usize = text.chars().map(|c| if is_wide(c) { 2 } else { 1 }).sum();
    let w = cells.max(1) as f32 * font.cell_w;

    snapshot.save();
    snapshot.translate(&graphene::Point::new(x, y));
    // Highlight background so the composing text is distinct from output.
    let bg = frame
        .selection_bg
        .unwrap_or(CellColor::new(0x33, 0x44, 0x66));
    snapshot.append_color(&rgba(bg), &graphene::Rect::new(0.0, 0.0, w, font.cell_h));

    let layout = widget.create_pango_layout(Some(text));
    layout.set_font_description(Some(&font.desc));
    let attrs = pango::AttrList::new();
    attrs.insert(pango::AttrInt::new_underline(pango::Underline::Single));
    layout.set_attributes(Some(&attrs));
    // Baseline-align like committed text: the composing run's font (CJK
    // fallback for Hangul) has its own ascent, so drawing from the top sits
    // it low. Offset by `cell_ascent - run_baseline` to share the baseline.
    let run_baseline = layout.baseline() as f32 / pango::SCALE as f32;
    snapshot.translate(&graphene::Point::new(0.0, font.ascent - run_baseline));
    snapshot.append_layout(&layout, &rgba(frame.default_fg));
    snapshot.restore();
}

/// East-Asian wide check matching the grid's two-cell glyphs (CJK ranges
/// + Hangul). A coarse but practical range set for preedit width.
fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F |     // Hangul Jamo
        0xAC00..=0xD7A3 |     // Hangul Syllables
        0x2E80..=0x303E |     // CJK radicals / Kangxi / punctuation
        0x3041..=0x33FF |     // Hiragana, Katakana, CJK symbols
        0x3400..=0x4DBF |     // CJK Ext A
        0x4E00..=0x9FFF |     // CJK Unified
        0xF900..=0xFAFF |     // CJK Compatibility
        0xFF00..=0xFF60 |     // Fullwidth forms
        0xFFE0..=0xFFE6
    )
}

fn draw_cursor(
    snapshot: &gtk::Snapshot,
    frame: &FrameSnapshot,
    font: &FontMetrics,
    cursor: CursorState,
) {
    use flowmux_terminal::render::CursorShape;
    let x = cursor.col as f32 * font.cell_w;
    let y = cursor.line as f32 * font.cell_h;
    let color = frame.cursor_color;
    let rect = match cursor.shape {
        CursorShape::Underline => graphene::Rect::new(x, y + font.cell_h - 2.0, font.cell_w, 2.0),
        CursorShape::Beam => graphene::Rect::new(x, y, 1.0, font.cell_h),
        _ => graphene::Rect::new(x, y, font.cell_w, font.cell_h),
    };
    snapshot.append_color(&rgba(color), &rect);
}

#[cfg(test)]
mod render_image_tests {
    use super::*;
    use flowmux_terminal::render::{CellColor, FrameSnapshot, StyledCell};

    fn blank(fg: CellColor, bg: CellColor) -> StyledCell {
        StyledCell {
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

    /// Place `text` at (row, col) with `fg`/`bg`. Hangul/CJK chars take two
    /// cells (wide + spacer), matching the grid.
    fn put(
        cells: &mut [StyledCell],
        cols: usize,
        row: usize,
        col: usize,
        text: &str,
        fg: CellColor,
        bg: CellColor,
        bold: bool,
    ) {
        let mut c = col;
        for ch in text.chars() {
            let wide = is_wide(ch);
            if c >= cols {
                break;
            }
            cells[row * cols + c] = StyledCell {
                ch,
                fg,
                bg,
                bold,
                italic: false,
                underline: false,
                strikeout: false,
                wide,
                wide_spacer: false,
                selected: false,
            };
            if wide && c + 1 < cols {
                cells[row * cols + c + 1] = StyledCell {
                    wide_spacer: true,
                    ..blank(fg, bg)
                };
                c += 2;
            } else {
                c += 1;
            }
        }
    }

    /// Renders a Hangul + ANSI-color frame to a PNG via the real renderer
    /// (offscreen CairoRenderer — no window, no WM, no display server
    /// interaction beyond the font map). Lets a human eyeball that the
    /// pure-Rust renderer draws Hangul glyphs and colors correctly.
    // Manual visual-inspection tool, not a CI assertion: GSK rendering
    // wants the main thread, so it is flaky inside the multi-threaded test
    // binary. Run on demand:
    //   DISPLAY=:1 dbus-run-session -- cargo test -p flowmux --bin flowmux \
    //     -- --ignored render_hangul --nocapture
    // then open /tmp/flowmux_render_test.png.
    #[test]
    #[ignore = "manual visual render dump; run with --ignored"]
    fn render_hangul_and_colors_to_png() {
        if gtk::init().is_err() {
            eprintln!("no display; skipping render image test");
            return;
        }
        let fg = CellColor::new(0xd0, 0xd0, 0xd0);
        let bg = CellColor::new(0x10, 0x12, 0x18);
        let cols = 46;
        let rows = 8;
        let mut cells = vec![blank(fg, bg); rows * cols];
        put(
            &mut cells,
            cols,
            0,
            0,
            "flowmux pure-Rust (alacritty) render test",
            fg,
            bg,
            true,
        );
        put(
            &mut cells,
            cols,
            1,
            0,
            "한글: 안녕하세요 訓民正音 日本語 中文",
            fg,
            bg,
            false,
        );
        put(
            &mut cells,
            cols,
            2,
            0,
            "ascii: The quick brown fox 0123456789",
            fg,
            bg,
            false,
        );
        put(
            &mut cells,
            cols,
            3,
            0,
            "빨강",
            CellColor::new(0xcc, 0x33, 0x33),
            bg,
            false,
        );
        put(
            &mut cells,
            cols,
            3,
            6,
            "초록",
            CellColor::new(0x33, 0xcc, 0x33),
            bg,
            false,
        );
        put(
            &mut cells,
            cols,
            3,
            12,
            "파랑",
            CellColor::new(0x55, 0x88, 0xff),
            bg,
            false,
        );
        put(
            &mut cells,
            cols,
            3,
            18,
            "노랑",
            CellColor::new(0xe0, 0xc0, 0x40),
            bg,
            false,
        );
        put(
            &mut cells,
            cols,
            4,
            0,
            " 초록배경 ",
            CellColor::new(0, 0, 0),
            CellColor::new(0x33, 0xaa, 0x33),
            false,
        );
        put(
            &mut cells,
            cols,
            4,
            12,
            " 파랑배경 ",
            CellColor::new(0xff, 0xff, 0xff),
            CellColor::new(0x33, 0x55, 0xcc),
            false,
        );
        put(&mut cells, cols, 5, 0, "볼드 bold 한글", fg, bg, true);
        put(
            &mut cells,
            cols,
            6,
            0,
            "box ┌────┐ 한글│칸 └────┘",
            fg,
            bg,
            false,
        );
        put(&mut cells, cols, 7, 0, "prompt$ echo 테스트", fg, bg, false);

        let frame = FrameSnapshot {
            rows,
            cols,
            cells,
            cursor: Some(CursorState {
                line: 7,
                col: 18,
                shape: flowmux_terminal::render::CursorShape::Block,
            }),
            caret: Some((7, 18)),
            default_fg: fg,
            default_bg: bg,
            cursor_color: CellColor::new(0x80, 0xc0, 0x80),
            selection_bg: None,
            selection_fg: None,
        };

        let widget = TerminalRenderArea::new();
        widget.set_font("monospace", 18.0);
        let font = widget.font_metrics().expect("font");
        let w = (cols as f32 * font.cell_w).ceil() as i32 + 8;
        let h = (rows as f32 * font.cell_h).ceil() as i32 + 8;
        widget.set_frame(frame.clone());

        // Assemble the frame's node using the real per-row builder.
        let snap = gtk::Snapshot::new();
        snap.append_color(
            &rgba(frame.default_bg),
            &graphene::Rect::new(0.0, 0.0, w as f32, h as f32),
        );
        for line in 0..frame.rows {
            if let Some(node) = build_row_node(&widget, &frame, &font, line) {
                snap.save();
                snap.translate(&graphene::Point::new(0.0, line as f32 * font.cell_h));
                snap.append_node(&node);
                snap.restore();
            }
        }
        if let Some(c) = frame.cursor {
            draw_cursor(&snap, &frame, &font, c);
        }
        let node = snap.to_node().expect("non-empty node");

        let renderer = gtk::gsk::CairoRenderer::new();
        renderer
            .realize(None::<&gdk::Surface>)
            .expect("realize cairo renderer");
        let texture = renderer.render_texture(
            &node,
            Some(&graphene::Rect::new(0.0, 0.0, w as f32, h as f32)),
        );
        let path = "/tmp/flowmux_render_test.png";
        texture.save_to_png(path).expect("save png");
        eprintln!("wrote {path} ({w}x{h})");
        assert!(std::path::Path::new(path).exists());
    }
}
