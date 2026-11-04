// SPDX-License-Identifier: AGPL-3.0-or-later
//
// The experimental terminal view's emulator (the `vt` feature, android/terminal map): one
// libghostty-vt terminal per tab, behind a small C ABI that vt.rs calls. This file only talks to
// libghostty-vt; the console, threads and locking are vt.rs's.
//
// Threading: libghostty-vt is not thread-safe, and neither is anything here. vt.rs holds one lock
// around every call, so the terminal, the render state and the buffers below are only used by one
// thread at a time.
//
// Frame format (little-endian), written by appine_vt_frame and read by VtView.kt; the two change
// together:
//
//   header, 32 bytes:
//     u16 cols, u16 rows
//     u8  dirty            0 nothing changed, 1 some rows, 2 every row
//     u8  cursor_visible   1 when visible and inside the viewport
//     u8  cursor_style     0 bar, 1 block, 2 underline, 3 hollow block
//     u8  cursor_blinking
//     u16 cursor_x, u16 cursor_y
//     u32 foreground       0xAARRGGBB, the default colours (after OSC 10/11)
//     u32 background
//     u32 cursor_color     0 when the program did not set one (OSC 12)
//     u16 row_count        number of rows that follow
//     u8  viewport_bottom  1 when the viewport shows the active area (not scrolled back)
//     u8  reserved
//     u32 reserved
//   row_count rows:
//     u16 y, u16 cell_count, u32 extra_count
//     cell_count cells of 16 bytes:
//       u32 codepoint      first codepoint of the cell's grapheme, 0 for an empty cell
//       u32 fg             0xAARRGGBB, 0 for the default foreground
//       u32 bg             0xAARRGGBB, 0 for the default background
//       u16 attrs          bit 0 bold, 1 italic, 2 faint, 3 blink, 4 inverse, 5 invisible,
//                          6 strikethrough, 7 overline, 8-10 underline (GhosttySgrUnderline),
//                          12-13 width (GhosttyCellWide: 0 narrow, 1 wide, 2 spacer tail,
//                          3 spacer head)
//       u16 extra          number of further codepoints in the grapheme
//     extra_count u32 codepoints: the further codepoints of the row's cells, in cell order

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <ghostty/vt.h>

#define HEADER_BYTES 32
#define ROW_HEADER_BYTES 8
#define CELL_BYTES 16

// Scrollback kept per terminal, in lines (libghostty-vt rounds up to whole pages)
#define SCROLLBACK_LINES ((size_t)10000)

// Synchronized output (mode 2026) holds the screen at most this long, so a program that never
// ends its hold cannot freeze the tab
#define HOLD_LIMIT_NS (1000LL * 1000 * 1000)

typedef struct {
  uint8_t *ptr;
  size_t len;
  size_t cap;
} Buf;

static bool buf_reserve(Buf *b, size_t extra) {
  if (b->len + extra <= b->cap) return true;
  size_t cap = b->cap ? b->cap : 4096;
  while (cap < b->len + extra) cap *= 2;
  uint8_t *p = realloc(b->ptr, cap);
  if (p == NULL) return false;
  b->ptr = p;
  b->cap = cap;
  return true;
}

static void put_u16(uint8_t *p, uint16_t v) {
  p[0] = (uint8_t)v;
  p[1] = (uint8_t)(v >> 8);
}

static void put_u32(uint8_t *p, uint32_t v) {
  p[0] = (uint8_t)v;
  p[1] = (uint8_t)(v >> 8);
  p[2] = (uint8_t)(v >> 16);
  p[3] = (uint8_t)(v >> 24);
}

static uint32_t argb(GhosttyColorRgb c) {
  return 0xff000000u | ((uint32_t)c.r << 16) | ((uint32_t)c.g << 8) | c.b;
}

static int64_t now_ns(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (int64_t)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

typedef struct AppineVt {
  GhosttyTerminal term;
  GhosttyRenderState state;
  GhosttyRenderStateRowIterator rows;
  GhosttyRenderStateRowCells cells;
  GhosttyKeyEncoder encoder;
  GhosttyKeyEvent event;
  uint16_t cols;
  uint16_t height;
  uint32_t cell_width;
  uint32_t cell_height;
  // Bytes the terminal answers with (DA, DSR, mode reports), for the console's input
  Buf replies;
  // The last frame, see the format above
  Buf frame;
  // Scratch for one cell's grapheme
  uint32_t *grapheme;
  size_t grapheme_cap;
  // Scratch for one row's further codepoints
  Buf extras;
  // Synchronized output: the render state was captured when the hold began and is not updated
  // again until it ends (or HOLD_LIMIT_NS passes)
  bool held;
  int64_t held_since;
} AppineVt;

// ---- Effects (called inside ghostty_terminal_vt_write, under vt.rs's lock) ----

static void on_write_pty(GhosttyTerminal t, void *userdata, const uint8_t *data, size_t len) {
  (void)t;
  AppineVt *vt = userdata;
  if (!buf_reserve(&vt->replies, len)) return;
  memcpy(vt->replies.ptr + vt->replies.len, data, len);
  vt->replies.len += len;
}

static bool on_size(GhosttyTerminal t, void *userdata, GhosttySizeReportSize *out) {
  (void)t;
  AppineVt *vt = userdata;
  out->columns = vt->cols;
  out->rows = vt->height;
  out->cell_width = vt->cell_width;
  out->cell_height = vt->cell_height;
  return true;
}

static bool on_color_scheme(GhosttyTerminal t, void *userdata, GhosttyColorScheme *out) {
  (void)t;
  (void)userdata;
  *out = GHOSTTY_COLOR_SCHEME_DARK;
  return true;
}

// Without an answer to DA, programs that probe for it (vim, tmux) wait for a timeout
static bool on_device_attributes(GhosttyTerminal t, void *userdata, GhosttyDeviceAttributes *out) {
  (void)t;
  (void)userdata;
  out->primary.conformance_level = GHOSTTY_DA_CONFORMANCE_VT220;
  out->primary.features[0] = GHOSTTY_DA_FEATURE_COLUMNS_132;
  out->primary.features[1] = GHOSTTY_DA_FEATURE_SELECTIVE_ERASE;
  out->primary.features[2] = GHOSTTY_DA_FEATURE_ANSI_COLOR;
  out->primary.num_features = 3;
  out->secondary.device_type = 1;
  out->secondary.firmware_version = 1;
  out->secondary.rom_cartridge = 0;
  out->tertiary.unit_id = 0;
  return true;
}

static GhosttyString on_xtversion(GhosttyTerminal t, void *userdata) {
  (void)t;
  (void)userdata;
  static const char name[] = "appine";
  return (GhosttyString){.ptr = (const uint8_t *)name, .len = sizeof(name) - 1};
}

// A hold began: capture the screen now, which is the frame the program wants to keep showing
static void on_render_hold(GhosttyTerminal t, void *userdata, bool held) {
  AppineVt *vt = userdata;
  if (held) {
    ghostty_render_state_update(vt->state, t);
    vt->held = true;
    vt->held_since = now_ns();
  } else {
    vt->held = false;
  }
}

// ---- Lifecycle ----

void appine_vt_free(AppineVt *vt);

AppineVt *appine_vt_new(uint16_t cols, uint16_t rows, uint32_t cell_width, uint32_t cell_height) {
  AppineVt *vt = calloc(1, sizeof(*vt));
  if (vt == NULL) return NULL;
  vt->cols = cols;
  vt->height = rows;
  vt->cell_width = cell_width;
  vt->cell_height = cell_height;
  if (ghostty_terminal_new(NULL, &vt->term, cols, rows) != GHOSTTY_SUCCESS ||
      ghostty_render_state_new(NULL, &vt->state) != GHOSTTY_SUCCESS ||
      ghostty_render_state_row_iterator_new(NULL, &vt->rows) != GHOSTTY_SUCCESS ||
      ghostty_render_state_row_cells_new(NULL, &vt->cells) != GHOSTTY_SUCCESS ||
      ghostty_key_encoder_new(NULL, &vt->encoder) != GHOSTTY_SUCCESS ||
      ghostty_key_event_new(NULL, &vt->event) != GHOSTTY_SUCCESS) {
    appine_vt_free(vt);
    return NULL;
  }
  // Cell sizes are only known to libghostty-vt through resize (size reports, images)
  ghostty_terminal_resize(vt->term, cols, rows, cell_width, cell_height);

  // The value itself is the userdata (since 1.4; it was read through *(void **)value before)
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_USERDATA, vt);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_WRITE_PTY, (const void *)on_write_pty);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SIZE, (const void *)on_size);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_COLOR_SCHEME, (const void *)on_color_scheme);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES,
                       (const void *)on_device_attributes);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_XTVERSION, (const void *)on_xtversion);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_RENDER_HOLD, (const void *)on_render_hold);
  size_t lines = SCROLLBACK_LINES;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES, &lines);
  // Grapheme clustering (mode 2027) by default, as in Ghostty (grapheme-width-method = unicode):
  // emoji sequences (ZWJ, skin tones, flags) take one or two cells instead of one per codepoint.
  // Programs that count widths per codepoint (wcwidth) then disagree on such sequences; they can
  // turn the mode off
  GhosttyTerminalModeConfig graphemes = {.mode = GHOSTTY_MODE_GRAPHEME_CLUSTER, .value = true};
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_MODE_DEFAULT, &graphemes);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_MODE, &graphemes);
  return vt;
}

void appine_vt_free(AppineVt *vt) {
  if (vt == NULL) return;
  if (vt->event) ghostty_key_event_free(vt->event);
  if (vt->encoder) ghostty_key_encoder_free(vt->encoder);
  if (vt->cells) ghostty_render_state_row_cells_free(vt->cells);
  if (vt->rows) ghostty_render_state_row_iterator_free(vt->rows);
  if (vt->state) ghostty_render_state_free(vt->state);
  if (vt->term) ghostty_terminal_free(vt->term);
  free(vt->replies.ptr);
  free(vt->frame.ptr);
  free(vt->extras.ptr);
  free(vt->grapheme);
  free(vt);
}

// ---- Output from the console ----

void appine_vt_feed(AppineVt *vt, const uint8_t *data, size_t len) {
  ghostty_terminal_vt_write(vt->term, data, len);
}

// The answers collected since the last call; valid until the next feed
const uint8_t *appine_vt_take_replies(AppineVt *vt, size_t *len) {
  *len = vt->replies.len;
  vt->replies.len = 0;
  return vt->replies.ptr;
}

void appine_vt_resize(AppineVt *vt, uint16_t cols, uint16_t rows, uint32_t cell_width,
                      uint32_t cell_height) {
  vt->cols = cols;
  vt->height = rows;
  vt->cell_width = cell_width;
  vt->cell_height = cell_height;
  ghostty_terminal_resize(vt->term, cols, rows, cell_width, cell_height);
}

// Scrolls the viewport by delta rows (negative: back into the scrollback); 0 goes to the bottom
void appine_vt_scroll(AppineVt *vt, int32_t delta) {
  GhosttyTerminalScrollViewport s = {0};
  if (delta == 0) {
    s.tag = GHOSTTY_SCROLL_VIEWPORT_BOTTOM;
  } else {
    s.tag = GHOSTTY_SCROLL_VIEWPORT_DELTA;
    s.value.delta = delta;
  }
  ghostty_terminal_scroll_viewport(vt->term, s);
}

// ---- Frames ----

static bool put_row(AppineVt *vt, uint16_t y) {
  if (ghostty_render_state_row_get(vt->rows, GHOSTTY_RENDER_STATE_ROW_DATA_CELLS, &vt->cells) !=
      GHOSTTY_SUCCESS) {
    return false;
  }
  if (!buf_reserve(&vt->frame, ROW_HEADER_BYTES + (size_t)vt->cols * CELL_BYTES)) return false;
  size_t row_at = vt->frame.len;
  vt->frame.len += ROW_HEADER_BYTES;
  vt->extras.len = 0;
  uint16_t count = 0;
  while (count < vt->cols && ghostty_render_state_row_cells_next(vt->cells)) {
    uint32_t len = 0;
    ghostty_render_state_row_cells_get(vt->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN,
                                       &len);
    uint32_t codepoint = 0;
    uint16_t extra = 0;
    if (len > 0) {
      if (len > vt->grapheme_cap) {
        uint32_t *p = realloc(vt->grapheme, len * sizeof(uint32_t));
        if (p == NULL) return false;
        vt->grapheme = p;
        vt->grapheme_cap = len;
      }
      ghostty_render_state_row_cells_get(vt->cells,
                                         GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                                         vt->grapheme);
      codepoint = vt->grapheme[0];
      extra = (uint16_t)(len - 1 > 0xffff ? 0xffff : len - 1);
      if (extra > 0) {
        if (!buf_reserve(&vt->extras, (size_t)extra * 4)) return false;
        for (uint16_t i = 0; i < extra; i++) {
          put_u32(vt->extras.ptr + vt->extras.len, vt->grapheme[i + 1]);
          vt->extras.len += 4;
        }
      }
    }

    GhosttyCell raw = 0;
    ghostty_render_state_row_cells_get(vt->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW, &raw);
    GhosttyCellWide wide = GHOSTTY_CELL_WIDE_NARROW;
    ghostty_cell_get(raw, GHOSTTY_CELL_DATA_WIDE, &wide);
    uint16_t attrs = (uint16_t)(((uint32_t)wide & 0x3) << 12);

    // Colours resolve through the palette; no value means the default colour
    GhosttyColorRgb c;
    uint32_t fg = 0, bg = 0;
    if (ghostty_render_state_row_cells_get(vt->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR,
                                           &c) == GHOSTTY_SUCCESS) {
      bg = argb(c);
    }
    bool styled = false;
    ghostty_render_state_row_cells_get(vt->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_HAS_STYLING,
                                       &styled);
    if (styled) {
      if (ghostty_render_state_row_cells_get(vt->cells,
                                             GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR,
                                             &c) == GHOSTTY_SUCCESS) {
        fg = argb(c);
      }
      GhosttyStyle style = GHOSTTY_INIT_SIZED(GhosttyStyle);
      ghostty_render_state_row_cells_get(vt->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
                                         &style);
      if (style.bold) attrs |= 1u << 0;
      if (style.italic) attrs |= 1u << 1;
      if (style.faint) attrs |= 1u << 2;
      if (style.blink) attrs |= 1u << 3;
      if (style.inverse) attrs |= 1u << 4;
      if (style.invisible) attrs |= 1u << 5;
      if (style.strikethrough) attrs |= 1u << 6;
      if (style.overline) attrs |= 1u << 7;
      attrs |= (uint16_t)(((uint32_t)style.underline & 0x7) << 8);
    }

    uint8_t *p = vt->frame.ptr + vt->frame.len;
    put_u32(p, codepoint);
    put_u32(p + 4, fg);
    put_u32(p + 8, bg);
    put_u16(p + 12, attrs);
    put_u16(p + 14, extra);
    vt->frame.len += CELL_BYTES;
    count++;
  }
  if (!buf_reserve(&vt->frame, vt->extras.len)) return false;
  memcpy(vt->frame.ptr + vt->frame.len, vt->extras.ptr, vt->extras.len);
  vt->frame.len += vt->extras.len;

  uint8_t *h = vt->frame.ptr + row_at;
  put_u16(h, y);
  put_u16(h + 2, count);
  put_u32(h + 4, (uint32_t)(vt->extras.len / 4));
  return true;
}

// Captures the screen and writes a frame with the rows that changed since the last frame (all of
// them when full is set); the result is valid until the next call. NULL when out of memory.
const uint8_t *appine_vt_frame(AppineVt *vt, bool full, size_t *len) {
  *len = 0;
  if (vt->held && now_ns() - vt->held_since > HOLD_LIMIT_NS) {
    GhosttyTerminalModeConfig sync = {.mode = GHOSTTY_MODE_SYNC_OUTPUT, .value = false};
    ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_MODE, &sync);
    vt->held = false;
  }
  if (!vt->held && ghostty_render_state_update(vt->state, vt->term) != GHOSTTY_SUCCESS) {
    return NULL;
  }

  GhosttyRenderStateDirty dirty = GHOSTTY_RENDER_STATE_DIRTY_FALSE;
  ghostty_render_state_get(vt->state, GHOSTTY_RENDER_STATE_DATA_DIRTY, &dirty);
  if (full) dirty = GHOSTTY_RENDER_STATE_DIRTY_FULL;
  GhosttyRenderStateColors colors = GHOSTTY_INIT_SIZED(GhosttyRenderStateColors);
  ghostty_render_state_get(vt->state, GHOSTTY_RENDER_STATE_DATA_COLORS, &colors);
  GhosttyRenderStateCursor cursor = GHOSTTY_INIT_SIZED(GhosttyRenderStateCursor);
  ghostty_render_state_get(vt->state, GHOSTTY_RENDER_STATE_DATA_CURSOR, &cursor);
  bool bottom = true;
  ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_VIEWPORT_ACTIVE, &bottom);

  vt->frame.len = 0;
  if (!buf_reserve(&vt->frame, HEADER_BYTES)) return NULL;
  uint8_t *h = vt->frame.ptr;
  memset(h, 0, HEADER_BYTES);
  put_u16(h, vt->cols);
  put_u16(h + 2, vt->height);
  h[4] = (uint8_t)dirty;
  h[5] = cursor.visible && cursor.viewport_has_value;
  h[6] = (uint8_t)cursor.visual_style;
  h[7] = cursor.blinking;
  if (cursor.viewport_has_value) {
    put_u16(h + 8, cursor.viewport_x);
    put_u16(h + 10, cursor.viewport_y);
  }
  put_u32(h + 12, argb(colors.foreground));
  put_u32(h + 16, argb(colors.background));
  put_u32(h + 20, colors.cursor_has_value ? argb(colors.cursor) : 0);
  h[26] = bottom;
  vt->frame.len = HEADER_BYTES;

  uint16_t count = 0;
  if (dirty != GHOSTTY_RENDER_STATE_DIRTY_FALSE &&
      ghostty_render_state_get(vt->state, GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, &vt->rows) ==
          GHOSTTY_SUCCESS) {
    uint16_t y = 0;
    while (ghostty_render_state_row_iterator_next(vt->rows)) {
      bool changed = dirty == GHOSTTY_RENDER_STATE_DIRTY_FULL;
      if (!changed) {
        ghostty_render_state_row_get(vt->rows, GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY, &changed);
      }
      if (changed) {
        if (!put_row(vt, y)) return NULL;
        count++;
      }
      y++;
    }
  }
  // put_row may have moved the buffer
  put_u16(vt->frame.ptr + 24, count);
  ghostty_render_state_clean(vt->state);
  *len = vt->frame.len;
  return vt->frame.ptr;
}

// ---- Input ----

// Encodes a key press into out; returns the length, or -1 when it does not fit (or fails). text
// is the character the key types without Ctrl and Alt (NULL for none), unshifted the key's
// character without Shift (0 for none).
ptrdiff_t appine_vt_key(AppineVt *vt, int key, uint16_t mods, uint16_t consumed, int action,
                        const char *text, size_t text_len, uint32_t unshifted, char *out,
                        size_t cap) {
  ghostty_key_encoder_setopt_from_terminal(vt->encoder, vt->term);
  ghostty_key_event_set_action(vt->event, (GhosttyKeyAction)action);
  ghostty_key_event_set_key(vt->event, (GhosttyKey)key);
  ghostty_key_event_set_mods(vt->event, mods);
  ghostty_key_event_set_consumed_mods(vt->event, consumed);
  ghostty_key_event_set_utf8(vt->event, text, text_len);
  ghostty_key_event_set_unshifted_codepoint(vt->event, unshifted);
  size_t len = 0;
  if (ghostty_key_encoder_encode(vt->encoder, vt->event, out, cap, &len) != GHOSTTY_SUCCESS) {
    return -1;
  }
  return (ptrdiff_t)len;
}

bool appine_vt_bracketed_paste(AppineVt *vt) {
  GhosttyTerminalModeConfig mode = {.mode = GHOSTTY_MODE_BRACKETED_PASTE};
  return ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_MODE, &mode) == GHOSTTY_SUCCESS &&
         mode.value;
}
