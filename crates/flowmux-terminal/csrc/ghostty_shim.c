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
    GhosttyMouseEncoder mouse_enc;
    GhosttyMouseEvent mouse_ev;
    GhosttyKeyEncoder key_enc;
    GhosttyKeyEvent key_ev;
    /* Geometry the mouse encoder needs to map pixels -> cells. */
    uint16_t cols;
    uint16_t rows;
    uint32_t cell_w_px;
    uint32_t cell_h_px;
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
    // Mouse encoder/event are best-effort: if they fail to allocate, mouse
    // reporting is simply unavailable (left NULL) and the rest still works.
    if (ghostty_mouse_encoder_new(NULL, &ctx->mouse_enc) != GHOSTTY_SUCCESS) {
        ctx->mouse_enc = NULL;
    }
    if (ghostty_mouse_event_new(NULL, &ctx->mouse_ev) != GHOSTTY_SUCCESS) {
        ctx->mouse_ev = NULL;
    }
    if (ghostty_key_encoder_new(NULL, &ctx->key_enc) != GHOSTTY_SUCCESS) {
        ctx->key_enc = NULL;
    }
    if (ghostty_key_event_new(NULL, &ctx->key_ev) != GHOSTTY_SUCCESS) {
        ctx->key_ev = NULL;
    }
    ctx->cols = cols;
    ctx->rows = rows;
    ctx->cell_w_px = 1;
    ctx->cell_h_px = 1;
    return ctx;
}

void fxvt_free(FxvtCtx *ctx) {
    if (ctx == NULL) {
        return;
    }
    if (ctx->key_ev) ghostty_key_event_free(ctx->key_ev);
    if (ctx->key_enc) ghostty_key_encoder_free(ctx->key_enc);
    if (ctx->mouse_ev) ghostty_mouse_event_free(ctx->mouse_ev);
    if (ctx->mouse_enc) ghostty_mouse_encoder_free(ctx->mouse_enc);
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
    int rc = ghostty_terminal_resize(ctx->terminal, cols, rows, cell_w_px, cell_h_px)
                     == GHOSTTY_SUCCESS
                 ? 0
                 : -1;
    if (rc == 0) {
        ctx->cols = cols;
        ctx->rows = rows;
        ctx->cell_w_px = cell_w_px ? cell_w_px : 1;
        ctx->cell_h_px = cell_h_px ? cell_h_px : 1;
    }
    return rc;
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

int fxvt_colors(FxvtCtx *ctx, uint8_t out_fg[3], uint8_t out_bg[3],
                uint8_t out_cursor[3], int *out_cursor_has) {
    if (ctx == NULL || out_fg == NULL || out_bg == NULL || out_cursor == NULL
        || out_cursor_has == NULL) {
        return -1;
    }
    GhosttyRenderStateColors colors = GHOSTTY_INIT_SIZED(GhosttyRenderStateColors);
    if (ghostty_render_state_colors_get(ctx->render, &colors) != GHOSTTY_SUCCESS) {
        return -1;
    }
    out_fg[0] = colors.foreground.r;
    out_fg[1] = colors.foreground.g;
    out_fg[2] = colors.foreground.b;
    out_bg[0] = colors.background.r;
    out_bg[1] = colors.background.g;
    out_bg[2] = colors.background.b;
    out_cursor[0] = colors.cursor.r;
    out_cursor[1] = colors.cursor.g;
    out_cursor[2] = colors.cursor.b;
    *out_cursor_has = colors.cursor_has_value ? 1 : 0;
    return 0;
}

int fxvt_set_default_colors(FxvtCtx *ctx, const uint8_t fg[3],
                            const uint8_t bg[3], const uint8_t cursor[3]) {
    if (ctx == NULL) {
        return -1;
    }
    GhosttyColorRgb f = {fg[0], fg[1], fg[2]};
    GhosttyColorRgb b = {bg[0], bg[1], bg[2]};
    GhosttyColorRgb c = {cursor[0], cursor[1], cursor[2]};
    int ok = 0;
    ok |= ghostty_terminal_set(ctx->terminal, GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, &f)
          != GHOSTTY_SUCCESS;
    ok |= ghostty_terminal_set(ctx->terminal, GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND, &b)
          != GHOSTTY_SUCCESS;
    ok |= ghostty_terminal_set(ctx->terminal, GHOSTTY_TERMINAL_OPT_COLOR_CURSOR, &c)
          != GHOSTTY_SUCCESS;
    return ok ? -1 : 0;
}

int fxvt_set_palette(FxvtCtx *ctx, const uint8_t *rgb, int count) {
    if (ctx == NULL || rgb == NULL || count < 0 || count > 256) {
        return -1;
    }
    /* Start from libghostty's standard default palette so untouched entries
     * (the 16-231 cube + 232-255 grayscale) stay xterm-standard, matching how
     * VTE fills a <=16 color theme out to 256. */
    GhosttyColorRgb pal[256];
    if (ghostty_terminal_get(ctx->terminal, GHOSTTY_TERMINAL_DATA_COLOR_PALETTE_DEFAULT, pal)
        != GHOSTTY_SUCCESS) {
        return -1;
    }
    for (int i = 0; i < count; i++) {
        pal[i].r = rgb[i * 3 + 0];
        pal[i].g = rgb[i * 3 + 1];
        pal[i].b = rgb[i * 3 + 2];
    }
    return ghostty_terminal_set(ctx->terminal, GHOSTTY_TERMINAL_OPT_COLOR_PALETTE, pal)
                   == GHOSTTY_SUCCESS
               ? 0
               : -1;
}

int fxvt_set_selection(FxvtCtx *ctx, uint16_t sx, uint32_t sy, uint16_t ex,
                       uint32_t ey, int rectangle) {
    if (ctx == NULL) {
        return -1;
    }
    GhosttyPoint ps;
    ps.tag = GHOSTTY_POINT_TAG_VIEWPORT;
    ps.value.coordinate.x = sx;
    ps.value.coordinate.y = sy;
    GhosttyPoint pe;
    pe.tag = GHOSTTY_POINT_TAG_VIEWPORT;
    pe.value.coordinate.x = ex;
    pe.value.coordinate.y = ey;

    GhosttyGridRef rs, re;
    if (ghostty_terminal_grid_ref(ctx->terminal, ps, &rs) != GHOSTTY_SUCCESS) {
        return -1;
    }
    if (ghostty_terminal_grid_ref(ctx->terminal, pe, &re) != GHOSTTY_SUCCESS) {
        return -1;
    }

    GhosttySelection sel = GHOSTTY_INIT_SIZED(GhosttySelection);
    sel.start = rs;
    sel.end = re;
    sel.rectangle = rectangle ? true : false;
    return ghostty_terminal_set(ctx->terminal, GHOSTTY_TERMINAL_OPT_SELECTION, &sel)
                   == GHOSTTY_SUCCESS
               ? 0
               : -1;
}

void fxvt_clear_selection(FxvtCtx *ctx) {
    if (ctx != NULL) {
        /* A NULL value clears the selection. */
        ghostty_terminal_set(ctx->terminal, GHOSTTY_TERMINAL_OPT_SELECTION, NULL);
    }
}

static size_t fxvt_copy_string(FxvtCtx *ctx, GhosttyTerminalData kind, char *buf, size_t cap) {
    if (ctx == NULL || buf == NULL || cap == 0) {
        return 0;
    }
    buf[0] = '\0';
    GhosttyString s;
    s.ptr = NULL;
    s.len = 0;
    if (ghostty_terminal_get(ctx->terminal, kind, &s) != GHOSTTY_SUCCESS || s.ptr == NULL) {
        return 0;
    }
    size_t n = s.len < cap - 1 ? s.len : cap - 1;
    memcpy(buf, s.ptr, n);
    buf[n] = '\0';
    return n;
}

size_t fxvt_title(FxvtCtx *ctx, char *buf, size_t cap) {
    return fxvt_copy_string(ctx, GHOSTTY_TERMINAL_DATA_TITLE, buf, cap);
}

size_t fxvt_pwd(FxvtCtx *ctx, char *buf, size_t cap) {
    return fxvt_copy_string(ctx, GHOSTTY_TERMINAL_DATA_PWD, buf, cap);
}

/* Defined later (shared with fxvt_row_text). */
static size_t fxvt_utf8_encode(uint32_t cp, char *buf);

static GhosttyKey fxvt_map_key(int named_key) {
    switch (named_key) {
        case FXVT_KEY_ENTER: return GHOSTTY_KEY_ENTER;
        case FXVT_KEY_TAB: return GHOSTTY_KEY_TAB;
        case FXVT_KEY_BACKSPACE: return GHOSTTY_KEY_BACKSPACE;
        case FXVT_KEY_ESCAPE: return GHOSTTY_KEY_ESCAPE;
        case FXVT_KEY_SPACE: return GHOSTTY_KEY_SPACE;
        case FXVT_KEY_UP: return GHOSTTY_KEY_ARROW_UP;
        case FXVT_KEY_DOWN: return GHOSTTY_KEY_ARROW_DOWN;
        case FXVT_KEY_LEFT: return GHOSTTY_KEY_ARROW_LEFT;
        case FXVT_KEY_RIGHT: return GHOSTTY_KEY_ARROW_RIGHT;
        case FXVT_KEY_HOME: return GHOSTTY_KEY_HOME;
        case FXVT_KEY_END: return GHOSTTY_KEY_END;
        case FXVT_KEY_PAGE_UP: return GHOSTTY_KEY_PAGE_UP;
        case FXVT_KEY_PAGE_DOWN: return GHOSTTY_KEY_PAGE_DOWN;
        case FXVT_KEY_DELETE: return GHOSTTY_KEY_DELETE;
        case FXVT_KEY_INSERT: return GHOSTTY_KEY_INSERT;
        case FXVT_KEY_KP_ENTER: return GHOSTTY_KEY_NUMPAD_ENTER;
        case FXVT_KEY_F1 + 0: return GHOSTTY_KEY_F1;
        case FXVT_KEY_F1 + 1: return GHOSTTY_KEY_F2;
        case FXVT_KEY_F1 + 2: return GHOSTTY_KEY_F3;
        case FXVT_KEY_F1 + 3: return GHOSTTY_KEY_F4;
        case FXVT_KEY_F1 + 4: return GHOSTTY_KEY_F5;
        case FXVT_KEY_F1 + 5: return GHOSTTY_KEY_F6;
        case FXVT_KEY_F1 + 6: return GHOSTTY_KEY_F7;
        case FXVT_KEY_F1 + 7: return GHOSTTY_KEY_F8;
        case FXVT_KEY_F1 + 8: return GHOSTTY_KEY_F9;
        case FXVT_KEY_F1 + 9: return GHOSTTY_KEY_F10;
        case FXVT_KEY_F1 + 10: return GHOSTTY_KEY_F11;
        case FXVT_KEY_F1 + 11: return GHOSTTY_KEY_F12;
        default: return GHOSTTY_KEY_UNIDENTIFIED;
    }
}

size_t fxvt_encode_key(FxvtCtx *ctx, int named_key, uint32_t unshifted_cp,
                       int mods, int composing, char *buf, size_t cap) {
    if (ctx == NULL || ctx->key_enc == NULL || ctx->key_ev == NULL || buf == NULL || cap == 0) {
        return 0;
    }
    /* Pull the terminal's current modes (DECCKM app cursor keys, keypad,
     * Kitty keyboard flags, Alt-as-ESC, …) so the encoding matches what the
     * foreground app (vim/claude/codex) negotiated. */
    ghostty_key_encoder_setopt_from_terminal(ctx->key_enc, ctx->terminal);

    ghostty_key_event_set_action(ctx->key_ev, GHOSTTY_KEY_ACTION_PRESS);
    ghostty_key_event_set_key(ctx->key_ev, fxvt_map_key(named_key));

    GhosttyMods m = 0;
    if (mods & FXVT_MOD_SHIFT) m |= GHOSTTY_MODS_SHIFT;
    if (mods & FXVT_MOD_CTRL) m |= GHOSTTY_MODS_CTRL;
    if (mods & FXVT_MOD_ALT) m |= GHOSTTY_MODS_ALT;
    ghostty_key_event_set_mods(ctx->key_ev, m);
    ghostty_key_event_set_consumed_mods(ctx->key_ev, 0);
    ghostty_key_event_set_composing(ctx->key_ev, composing ? true : false);

    if (unshifted_cp != 0) {
        ghostty_key_event_set_unshifted_codepoint(ctx->key_ev, unshifted_cp);
        char u8[8];
        size_t n = fxvt_utf8_encode(unshifted_cp, u8);
        ghostty_key_event_set_utf8(ctx->key_ev, u8, n);
    } else {
        ghostty_key_event_set_unshifted_codepoint(ctx->key_ev, 0);
        ghostty_key_event_set_utf8(ctx->key_ev, "", 0);
    }

    size_t out_len = 0;
    if (ghostty_key_encoder_encode(ctx->key_enc, ctx->key_ev, buf, cap, &out_len)
        != GHOSTTY_SUCCESS) {
        return 0;
    }
    return out_len;
}

void fxvt_scroll(FxvtCtx *ctx, long delta) {
    if (ctx == NULL) {
        return;
    }
    GhosttyTerminalScrollViewport behavior;
    behavior.tag = GHOSTTY_SCROLL_VIEWPORT_DELTA;
    behavior.value.delta = (intptr_t)delta;
    ghostty_terminal_scroll_viewport(ctx->terminal, behavior);
}

void fxvt_scroll_bottom(FxvtCtx *ctx) {
    if (ctx == NULL) {
        return;
    }
    GhosttyTerminalScrollViewport behavior;
    behavior.tag = GHOSTTY_SCROLL_VIEWPORT_BOTTOM;
    behavior.value.delta = 0;
    ghostty_terminal_scroll_viewport(ctx->terminal, behavior);
}

int fxvt_scrollbar(FxvtCtx *ctx, unsigned long long *out_total,
                   unsigned long long *out_offset, unsigned long long *out_len) {
    if (ctx == NULL || out_total == NULL || out_offset == NULL || out_len == NULL) {
        return -1;
    }
    GhosttyTerminalScrollbar sb;
    sb.total = 0;
    sb.offset = 0;
    sb.len = 0;
    if (ghostty_terminal_get(ctx->terminal, GHOSTTY_TERMINAL_DATA_SCROLLBAR, &sb)
        != GHOSTTY_SUCCESS) {
        return -1;
    }
    *out_total = (unsigned long long)sb.total;
    *out_offset = (unsigned long long)sb.offset;
    *out_len = (unsigned long long)sb.len;
    return 0;
}

int fxvt_mouse_enabled(FxvtCtx *ctx) {
    if (ctx == NULL) {
        return 0;
    }
    bool v = false;
    if (ghostty_terminal_mode_get(ctx->terminal, GHOSTTY_MODE_NORMAL_MOUSE, &v) == GHOSTTY_SUCCESS
        && v) {
        return 1;
    }
    if (ghostty_terminal_mode_get(ctx->terminal, GHOSTTY_MODE_BUTTON_MOUSE, &v) == GHOSTTY_SUCCESS
        && v) {
        return 1;
    }
    if (ghostty_terminal_mode_get(ctx->terminal, GHOSTTY_MODE_ANY_MOUSE, &v) == GHOSTTY_SUCCESS
        && v) {
        return 1;
    }
    return 0;
}

size_t fxvt_encode_mouse(FxvtCtx *ctx, int action, int button, double px,
                         double py, int mods, char *buf, size_t cap) {
    if (ctx == NULL || ctx->mouse_enc == NULL || ctx->mouse_ev == NULL || buf == NULL) {
        return 0;
    }
    /* Refresh the encoder's active tracking mode + report format from the
     * terminal. */
    ghostty_mouse_encoder_setopt_from_terminal(ctx->mouse_enc, ctx->terminal);

    /* Give the encoder the pixel geometry so it can map the surface-pixel
     * position to a cell (without this, encode produces nothing). */
    GhosttyMouseEncoderSize enc_size;
    memset(&enc_size, 0, sizeof(enc_size));
    enc_size.size = sizeof(GhosttyMouseEncoderSize);
    enc_size.screen_width = (uint32_t)ctx->cols * ctx->cell_w_px;
    enc_size.screen_height = (uint32_t)ctx->rows * ctx->cell_h_px;
    enc_size.cell_width = ctx->cell_w_px;
    enc_size.cell_height = ctx->cell_h_px;
    ghostty_mouse_encoder_setopt(ctx->mouse_enc, GHOSTTY_MOUSE_ENCODER_OPT_SIZE, &enc_size);

    bool any_pressed = (action == FXVT_MOUSE_MOTION && button != FXVT_BTN_NONE);
    ghostty_mouse_encoder_setopt(ctx->mouse_enc,
                                 GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED, &any_pressed);

    GhosttyMouseAction a = GHOSTTY_MOUSE_ACTION_PRESS;
    if (action == FXVT_MOUSE_RELEASE) {
        a = GHOSTTY_MOUSE_ACTION_RELEASE;
    } else if (action == FXVT_MOUSE_MOTION) {
        a = GHOSTTY_MOUSE_ACTION_MOTION;
    }
    ghostty_mouse_event_set_action(ctx->mouse_ev, a);

    if (button == FXVT_BTN_NONE) {
        ghostty_mouse_event_clear_button(ctx->mouse_ev);
    } else {
        GhosttyMouseButton b = GHOSTTY_MOUSE_BUTTON_LEFT;
        switch (button) {
            case FXVT_BTN_RIGHT: b = GHOSTTY_MOUSE_BUTTON_RIGHT; break;
            case FXVT_BTN_MIDDLE: b = GHOSTTY_MOUSE_BUTTON_MIDDLE; break;
            case FXVT_BTN_WHEEL_UP: b = GHOSTTY_MOUSE_BUTTON_FOUR; break;
            case FXVT_BTN_WHEEL_DOWN: b = GHOSTTY_MOUSE_BUTTON_FIVE; break;
            default: b = GHOSTTY_MOUSE_BUTTON_LEFT; break;
        }
        ghostty_mouse_event_set_button(ctx->mouse_ev, b);
    }

    GhosttyMousePosition pos;
    pos.x = (float)px;
    pos.y = (float)py;
    ghostty_mouse_event_set_position(ctx->mouse_ev, pos);

    GhosttyMods m = 0;
    if (mods & FXVT_MOD_SHIFT) m |= GHOSTTY_MODS_SHIFT;
    if (mods & FXVT_MOD_CTRL) m |= GHOSTTY_MODS_CTRL;
    if (mods & FXVT_MOD_ALT) m |= GHOSTTY_MODS_ALT;
    ghostty_mouse_event_set_mods(ctx->mouse_ev, m);

    size_t out_len = 0;
    if (ghostty_mouse_encoder_encode(ctx->mouse_enc, ctx->mouse_ev, buf, cap, &out_len)
        != GHOSTTY_SUCCESS) {
        return 0;
    }
    return out_len;
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

    /* Seed foreground with the terminal default so cells without an explicit
     * color render correctly; the FG_COLOR query below overwrites it only when
     * the cell carries one (matching Ghostty's own renderer). */
    GhosttyRenderStateColors defaults = GHOSTTY_INIT_SIZED(GhosttyRenderStateColors);
    if (ghostty_render_state_colors_get(ctx->render, &defaults) == GHOSTTY_SUCCESS) {
        out->fg[0] = defaults.foreground.r;
        out->fg[1] = defaults.foreground.g;
        out->fg[2] = defaults.foreground.b;
    }

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
