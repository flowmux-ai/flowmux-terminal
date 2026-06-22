/* SPDX-License-Identifier: GPL-3.0-or-later */
/*
 * Implementation of the stable libghostty-vt shim. See ghostty_shim.h for the
 * rationale (insulating the Rust FFI from libghostty's unstable C ABI).
 */
#include "ghostty_shim.h"

/* Static link: libghostty-vt.a, so the API macros must not expand to
 * dllimport/visibility attributes for an external shared object. */
#define GHOSTTY_STATIC
#include <ghostty/vt.h>

#include <stdlib.h>
#include <string.h>

struct FxvtCtx {
    GhosttyTerminal terminal;
    GhosttyRenderState render;
    GhosttyRenderStateRowIterator row_iter;
    GhosttyRenderStateRowCells cells;
};

FxvtCtx *fxvt_new(uint16_t cols, uint16_t rows, size_t scrollback) {
    if (cols == 0 || rows == 0) {
        return NULL;
    }
    FxvtCtx *ctx = (FxvtCtx *)calloc(1, sizeof(FxvtCtx));
    if (ctx == NULL) {
        return NULL;
    }

    GhosttyTerminalOptions opts = {
        .cols = cols,
        .rows = rows,
        .max_scrollback = scrollback,
    };
    /* NULL allocator => libghostty's default allocator. */
    if (ghostty_terminal_new(NULL, &ctx->terminal, opts) != GHOSTTY_SUCCESS) {
        free(ctx);
        return NULL;
    }
    if (ghostty_render_state_new(NULL, &ctx->render) != GHOSTTY_SUCCESS) {
        ghostty_terminal_free(ctx->terminal);
        free(ctx);
        return NULL;
    }
    if (ghostty_render_state_row_iterator_new(NULL, &ctx->row_iter) != GHOSTTY_SUCCESS) {
        ghostty_render_state_free(ctx->render);
        ghostty_terminal_free(ctx->terminal);
        free(ctx);
        return NULL;
    }
    if (ghostty_render_state_row_cells_new(NULL, &ctx->cells) != GHOSTTY_SUCCESS) {
        ghostty_render_state_row_iterator_free(ctx->row_iter);
        ghostty_render_state_free(ctx->render);
        ghostty_terminal_free(ctx->terminal);
        free(ctx);
        return NULL;
    }
    return ctx;
}

void fxvt_free(FxvtCtx *ctx) {
    if (ctx == NULL) {
        return;
    }
    if (ctx->cells) ghostty_render_state_row_cells_free(ctx->cells);
    if (ctx->row_iter) ghostty_render_state_row_iterator_free(ctx->row_iter);
    if (ctx->render) ghostty_render_state_free(ctx->render);
    if (ctx->terminal) ghostty_terminal_free(ctx->terminal);
    free(ctx);
}

void fxvt_write(FxvtCtx *ctx, const uint8_t *data, size_t len) {
    if (ctx == NULL || data == NULL || len == 0) {
        return;
    }
    ghostty_terminal_vt_write(ctx->terminal, data, len);
}

int fxvt_resize(FxvtCtx *ctx, uint16_t cols, uint16_t rows,
                uint32_t cell_w_px, uint32_t cell_h_px) {
    if (ctx == NULL || cols == 0 || rows == 0) {
        return -1;
    }
    return ghostty_terminal_resize(ctx->terminal, cols, rows, cell_w_px, cell_h_px)
                   == GHOSTTY_SUCCESS
               ? 0
               : -1;
}

int fxvt_update(FxvtCtx *ctx) {
    if (ctx == NULL) {
        return -1;
    }
    return ghostty_render_state_update(ctx->render, ctx->terminal) == GHOSTTY_SUCCESS
               ? 0
               : -1;
}

int fxvt_dims(FxvtCtx *ctx, uint16_t *out_cols, uint16_t *out_rows) {
    if (ctx == NULL || out_cols == NULL || out_rows == NULL) {
        return -1;
    }
    if (ghostty_render_state_get(ctx->render, GHOSTTY_RENDER_STATE_DATA_COLS, out_cols)
        != GHOSTTY_SUCCESS) {
        return -1;
    }
    if (ghostty_render_state_get(ctx->render, GHOSTTY_RENDER_STATE_DATA_ROWS, out_rows)
        != GHOSTTY_SUCCESS) {
        return -1;
    }
    return 0;
}

int fxvt_cursor(FxvtCtx *ctx, uint16_t *out_x, uint16_t *out_y, int *out_visible) {
    if (ctx == NULL || out_x == NULL || out_y == NULL || out_visible == NULL) {
        return -1;
    }
    bool visible = false;
    /* Visibility is best-effort: if the query fails (e.g. cursor off-viewport),
     * report not-visible rather than erroring the whole read. */
    if (ghostty_render_state_get(ctx->render, GHOSTTY_RENDER_STATE_DATA_CURSOR_VISIBLE,
                                 &visible)
        != GHOSTTY_SUCCESS) {
        visible = false;
    }
    *out_visible = visible ? 1 : 0;

    uint16_t x = 0, y = 0;
    if (ghostty_render_state_get(ctx->render, GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_X,
                                 &x)
        != GHOSTTY_SUCCESS) {
        x = 0;
    }
    if (ghostty_render_state_get(ctx->render, GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_Y,
                                 &y)
        != GHOSTTY_SUCCESS) {
        y = 0;
    }
    *out_x = x;
    *out_y = y;
    return 0;
}

/* Bind ctx->row_iter to the snapshot and advance it to `row`, then bind
 * ctx->cells to that row. Returns 1 on success, 0 if the row is out of range. */
static int fxvt_seek_row(FxvtCtx *ctx, uint16_t row) {
    GhosttyRenderStateRowIterator it = ctx->row_iter;
    /* Re-binding the iterator from the render state resets it to the top. */
    if (ghostty_render_state_get(ctx->render, GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, &it)
        != GHOSTTY_SUCCESS) {
        return 0;
    }
    uint16_t i = 0;
    while (ghostty_render_state_row_iterator_next(it)) {
        if (i == row) {
            if (ghostty_render_state_row_get(it, GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                                             &ctx->cells)
                != GHOSTTY_SUCCESS) {
                return 0;
            }
            return 1;
        }
        i++;
    }
    return 0;
}

int fxvt_cell(FxvtCtx *ctx, uint16_t row, uint16_t col, FxvtCell *out) {
    if (ctx == NULL || out == NULL) {
        return -1;
    }
    if (!fxvt_seek_row(ctx, row)) {
        return 0;
    }
    if (ghostty_render_state_row_cells_select(ctx->cells, col) != GHOSTTY_SUCCESS) {
        return 0;
    }

    memset(out, 0, sizeof(*out));

    uint32_t glen = 0;
    ghostty_render_state_row_cells_get(
        ctx->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN, &glen);
    if (glen > 8) {
        glen = 8;
    }
    if (glen > 0) {
        uint32_t cps[16];
        ghostty_render_state_row_cells_get(
            ctx->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF, cps);
        for (uint32_t i = 0; i < glen; i++) {
            out->codepoints[i] = cps[i];
        }
    }
    out->cp_len = (uint8_t)glen;

    /* Foreground: defaults are applied by the renderer when no explicit color. */
    GhosttyColorRgb fg = {0, 0, 0};
    if (ghostty_render_state_row_cells_get(
            ctx->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR, &fg)
        == GHOSTTY_SUCCESS) {
        out->fg[0] = fg.r;
        out->fg[1] = fg.g;
        out->fg[2] = fg.b;
    }

    GhosttyColorRgb bg = {0, 0, 0};
    if (ghostty_render_state_row_cells_get(
            ctx->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR, &bg)
        == GHOSTTY_SUCCESS) {
        out->bg[0] = bg.r;
        out->bg[1] = bg.g;
        out->bg[2] = bg.b;
        out->has_bg = 1;
    }

    GhosttyStyle style = GHOSTTY_INIT_SIZED(GhosttyStyle);
    if (ghostty_render_state_row_cells_get(
            ctx->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE, &style)
        == GHOSTTY_SUCCESS) {
        uint8_t flags = 0;
        if (style.bold) flags |= FXVT_FLAG_BOLD;
        if (style.italic) flags |= FXVT_FLAG_ITALIC;
        if (style.underline != 0) flags |= FXVT_FLAG_UNDERLINE;
        if (style.inverse) flags |= FXVT_FLAG_INVERSE;
        if (style.strikethrough) flags |= FXVT_FLAG_STRIKETHROUGH;
        if (style.faint) flags |= FXVT_FLAG_FAINT;
        if (style.blink) flags |= FXVT_FLAG_BLINK;
        out->flags = flags;
    }

    bool selected = false;
    if (ghostty_render_state_row_cells_get(
            ctx->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_SELECTED, &selected)
        == GHOSTTY_SUCCESS) {
        out->selected = selected ? 1 : 0;
    }

    /* Wide-glyph state lives on the low-level GhosttyCell (a packed handle),
     * reachable via the RAW cell value. The renderer advances two columns for
     * a WIDE lead cell and skips its SPACER_TAIL. */
    GhosttyCell raw_cell = 0;
    if (ghostty_render_state_row_cells_get(
            ctx->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW, &raw_cell)
        == GHOSTTY_SUCCESS) {
        GhosttyCellWide wide = GHOSTTY_CELL_WIDE_NARROW;
        if (ghostty_cell_get(raw_cell, GHOSTTY_CELL_DATA_WIDE, &wide) == GHOSTTY_SUCCESS) {
            out->wide = (wide == GHOSTTY_CELL_WIDE_WIDE) ? 1 : 0;
        }
    }

    return 1;
}

/* Minimal UTF-8 encoder for a single scalar value. Returns bytes written. */
static size_t fxvt_utf8_encode(uint32_t cp, char *buf) {
    if (cp < 0x80) {
        buf[0] = (char)cp;
        return 1;
    } else if (cp < 0x800) {
        buf[0] = (char)(0xC0 | (cp >> 6));
        buf[1] = (char)(0x80 | (cp & 0x3F));
        return 2;
    } else if (cp < 0x10000) {
        buf[0] = (char)(0xE0 | (cp >> 12));
        buf[1] = (char)(0x80 | ((cp >> 6) & 0x3F));
        buf[2] = (char)(0x80 | (cp & 0x3F));
        return 3;
    } else {
        buf[0] = (char)(0xF0 | (cp >> 18));
        buf[1] = (char)(0x80 | ((cp >> 12) & 0x3F));
        buf[2] = (char)(0x80 | ((cp >> 6) & 0x3F));
        buf[3] = (char)(0x80 | (cp & 0x3F));
        return 4;
    }
}

size_t fxvt_row_text(FxvtCtx *ctx, uint16_t row, char *buf, size_t cap) {
    if (ctx == NULL || buf == NULL || cap == 0) {
        return 0;
    }
    buf[0] = '\0';
    if (!fxvt_seek_row(ctx, row)) {
        return 0;
    }

    uint16_t cols = 0, rows = 0;
    if (fxvt_dims(ctx, &cols, &rows) != 0) {
        return 0;
    }

    size_t pos = 0;
    size_t trimmed = 0; /* byte length up to the last non-blank cell */
    for (uint16_t x = 0; x < cols; x++) {
        if (ghostty_render_state_row_cells_select(ctx->cells, x) != GHOSTTY_SUCCESS) {
            break;
        }
        uint32_t glen = 0;
        ghostty_render_state_row_cells_get(
            ctx->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN, &glen);
        if (glen == 0) {
            /* Blank cell: emit a space but do not extend the trimmed length. */
            if (pos + 1 < cap) {
                buf[pos++] = ' ';
            }
            continue;
        }
        uint32_t cps[16];
        uint32_t n = glen < 16 ? glen : 16;
        ghostty_render_state_row_cells_get(
            ctx->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF, cps);
        for (uint32_t i = 0; i < n; i++) {
            char u8[4];
            size_t w = fxvt_utf8_encode(cps[i], u8);
            if (pos + w >= cap) {
                goto done;
            }
            memcpy(&buf[pos], u8, w);
            pos += w;
        }
        trimmed = pos;
    }
done:
    /* Trim trailing blanks. */
    buf[trimmed] = '\0';
    return trimmed;
}
