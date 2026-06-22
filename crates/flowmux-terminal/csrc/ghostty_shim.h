/* SPDX-License-Identifier: GPL-3.0-or-later */
/*
 * Stable C shim over libghostty-vt for flowmux's Rust FFI.
 *
 * libghostty-vt's C API is explicitly unstable (enum values and struct
 * layouts may change between revisions). Rather than hard-code enum integer
 * values and struct offsets on the Rust side, flowmux binds to this thin
 * shim: the names are resolved by the C compiler against the pinned headers,
 * so a libghostty bump only touches this file and never the Rust bindings.
 *
 * The shim owns one terminal + render-state + reusable iterator handles per
 * context. It exposes feed/resize/snapshot plus a flat per-cell read model
 * that the GTK renderer (and headless tests) consume.
 */
#ifndef FLOWMUX_GHOSTTY_SHIM_H
#define FLOWMUX_GHOSTTY_SHIM_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque per-pane VT context (terminal + render state + iterators). */
typedef struct FxvtCtx FxvtCtx;

/* Style flag bits packed into FxvtCell.flags. */
#define FXVT_FLAG_BOLD          (1u << 0)
#define FXVT_FLAG_ITALIC        (1u << 1)
#define FXVT_FLAG_UNDERLINE     (1u << 2)
#define FXVT_FLAG_INVERSE       (1u << 3)
#define FXVT_FLAG_STRIKETHROUGH (1u << 4)
#define FXVT_FLAG_FAINT         (1u << 5)
#define FXVT_FLAG_BLINK         (1u << 6)

/*
 * A single rendered grid cell, flattened for the host renderer. Colors are
 * already resolved (palette + style + content tags) to RGB; `has_bg` says
 * whether the background differs from the terminal default.
 */
typedef struct {
    uint32_t codepoints[8]; /* grapheme cluster scalar values */
    uint8_t  cp_len;        /* number of valid codepoints; 0 = blank cell */
    uint8_t  fg[3];         /* resolved foreground RGB */
    uint8_t  bg[3];         /* resolved background RGB (valid iff has_bg) */
    uint8_t  has_bg;        /* 1 if the cell carries an explicit background */
    uint8_t  flags;         /* FXVT_FLAG_* bitset */
    uint8_t  selected;      /* 1 if the cell is inside the active selection */
    uint8_t  wide;          /* 1 if this is the lead cell of a wide glyph */
    uint8_t  _pad[2];
} FxvtCell;

/* Create a VT context sized cols x rows with `scrollback` history lines.
 * Returns NULL on allocation/initialization failure. */
FxvtCtx *fxvt_new(uint16_t cols, uint16_t rows, size_t scrollback);

/* Destroy a context (NULL-safe). */
void fxvt_free(FxvtCtx *ctx);

/* Feed raw VT output bytes (PTY -> terminal). */
void fxvt_write(FxvtCtx *ctx, const uint8_t *data, size_t len);

/* Resize the grid. cell_w_px/cell_h_px feed mouse/pixel reporting; pass the
 * renderer's cell metrics (or 1,1 headless). Returns 0 on success. */
int fxvt_resize(FxvtCtx *ctx, uint16_t cols, uint16_t rows,
                uint32_t cell_w_px, uint32_t cell_h_px);

/* Take a render snapshot of current terminal state. Must be called before any
 * fxvt_dims/fxvt_cursor/fxvt_cell/fxvt_row_text read. Returns 0 on success. */
int fxvt_update(FxvtCtx *ctx);

/* Read the snapshot viewport dimensions. Returns 0 on success. */
int fxvt_dims(FxvtCtx *ctx, uint16_t *out_cols, uint16_t *out_rows);

/* Read the cursor viewport position + visibility. *out_visible is 0/1.
 * Returns 0 on success. */
int fxvt_cursor(FxvtCtx *ctx, uint16_t *out_x, uint16_t *out_y, int *out_visible);

/* Read one cell at (row, col) in the viewport into *out.
 * Returns 1 if the cell exists and was written, 0 otherwise. */
int fxvt_cell(FxvtCtx *ctx, uint16_t row, uint16_t col, FxvtCell *out);

/* Write the UTF-8 text of a whole viewport row into buf (capacity cap,
 * always NUL-terminated when cap > 0). Trailing blank cells are trimmed.
 * Returns the number of bytes written excluding the NUL. */
size_t fxvt_row_text(FxvtCtx *ctx, uint16_t row, char *buf, size_t cap);

#ifdef __cplusplus
}
#endif

#endif /* FLOWMUX_GHOSTTY_SHIM_H */
