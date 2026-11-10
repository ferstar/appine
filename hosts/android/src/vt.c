// SPDX-License-Identifier: AGPL-3.0-or-later
//
// The tabs' terminal emulator (android/terminal map): one libghostty-vt terminal per tab, behind
// a small C ABI that vt.rs calls. This file only talks to
// libghostty-vt; the console, threads and locking are vt.rs's.
//
// Threading: libghostty-vt is not thread-safe, and neither is anything here. vt.rs holds one lock
// around every call, so the terminal, the render state and the buffers below are only used by one
// thread at a time.
//
// Frame format (little-endian), written by appine_vt_frame and read by VtView.kt; the two change
// together:
//
//   header, 56 bytes:
//     u16 cols, u16 rows
//     u8  dirty            0 nothing changed, 1 some rows, 2 every row
//     u8  cursor_visible   1 when visible and inside the viewport
//     u8  cursor_style     0 bar, 1 block, 2 underline, 3 hollow block
//     u8  cursor_blinking
//     u16 cursor_x, u16 cursor_y
//     u32 foreground       0xAARRGGBB, the default colours (after OSC 10/11 and reverse video)
//     u32 background
//     u32 cursor_color     0 when the program did not set one (OSC 12)
//     u16 row_count        number of rows that follow
//     u8  viewport_bottom  1 when the viewport shows the active area (not scrolled back)
//     u8  flags            bit 0 mouse tracking on, 1 alternate screen, 2 alternate scroll
//                          (mode 1007), 3 focus events (mode 1004), 4 a selection exists,
//                          5 it is rectangular, 6 the search has not caught up with the
//                          terminal yet (draw again to go on)
//     u16 selection_start_x, u16 selection_start_y
//                          the selection's first cell (in terminal order; a rectangle's top
//                          left corner) in the viewport, 0xffff for both when there is none or
//                          it is outside the viewport
//     u16 selection_end_x, u16 selection_end_y
//                          its last cell (a rectangle's bottom right corner), likewise
//     u32 scroll_total     the scrollable area in rows (scrollback and active area), as
//                          GhosttyTerminalScrollbar has it
//     u32 scroll_offset    the viewport's first row in that area
//     u32 scroll_len       the rows the viewport shows
//     u32 search_total     the search's matches on the active screen so far, 0 without a search
//     u32 search_selected  the selected match's place counted from the newest (1 for the
//                          newest), 0 when none is selected
//   row_count rows:
//     u16 y, u16 cell_count, u32 extra_count
//     u16 selection_from, u16 selection_to
//                          the row's selected columns (inclusive), 0xffff for both when none
//     cell_count cells of 16 bytes:
//       u32 codepoint      first codepoint of the cell's grapheme, 0 for an empty cell
//       u32 fg             0xAARRGGBB, 0 for the default foreground
//       u32 bg             0xAARRGGBB, 0 for the default background
//       u16 attrs          bit 0 bold, 1 italic, 2 faint, 3 blink, 4 inverse, 5 invisible,
//                          6 strikethrough, 7 overline, 8-10 underline (GhosttySgrUnderline),
//                          12-13 width (GhosttyCellWide: 0 narrow, 1 wide, 2 spacer tail,
//                          3 spacer head), 14 in a search match, 15 in the selected match
//       u16 extra          number of further codepoints in the grapheme
//     extra_count u32 codepoints: the further codepoints of the row's cells, in cell order
//
// A row is sent when its cells changed or when its search marks did (a new needle, another
// selected match, a match found), so the view's mirror always carries the marks.

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <ghostty/vt.h>

#define HEADER_BYTES 56
#define ROW_HEADER_BYTES 12
#define CELL_BYTES 16

#define FLAG_MOUSE 0x01
#define FLAG_ALT_SCREEN 0x02
#define FLAG_ALT_SCROLL 0x04
#define FLAG_FOCUS_EVENTS 0x08
#define FLAG_SELECTION 0x10
#define FLAG_RECTANGLE 0x20
#define FLAG_SEARCH_PENDING 0x40

// A cell's search mark (AppineVt.marks), shifted into its attrs at bit 14
#define MARK_NONE 0
#define MARK_MATCH 1
#define MARK_SELECTED 2
#define MARK_SHIFT 14

#define NONE16 0xffff

// How long one frame may spend catching a search up with the terminal; the view draws again until
// it has (FLAG_SEARCH_PENDING), so a search through a long scrollback does not hold up a frame
#define SEARCH_BUDGET_NS (4LL * 1000 * 1000)

// Scrollback kept per terminal: whichever limit is reached first prunes the oldest whole pages
// (about 400 KiB each, libghostty-vt rounding up). A page holds fewer rows the wider the terminal,
// so the line limit applies up to about 120 columns and the byte limit, which bounds a tab's
// memory, beyond. Both must be set: libghostty-vt's C terminal starts with a byte limit of 10,000
// bytes, which it raises to two pages
#define SCROLLBACK_LINES ((size_t)10000)
#define SCROLLBACK_BYTES ((size_t)10 << 20)

// The default colours, those of the render state's built-ins and the terminal's black window.
// Unset, libghostty-vt answers no OSC 10 or 11 query, reverse video (DECSCNM) swaps nothing, and a
// colour set with OSC 10 or 11 alone does not show. The cursor gets none, so it follows the
// foreground, as in Ghostty without cursor-color (and an OSC 12 query answers the foreground)
static const GhosttyColorRgb DEFAULT_FOREGROUND = {.r = 0xff, .g = 0xff, .b = 0xff};
static const GhosttyColorRgb DEFAULT_BACKGROUND = {.r = 0, .g = 0, .b = 0};

// Synchronized output (mode 2026) holds the screen at most this long, so a program that never
// ends its hold cannot freeze the tab
#define HOLD_LIMIT_NS (1000LL * 1000 * 1000)

// The largest clipboard write (OSC 52) taken: Android's clipboard goes through a binder
// transaction, which fails beyond about 1 MB (the text is sent as UTF-16)
#define CLIPBOARD_MAX ((size_t)256 * 1024)

// The most an OSC 5522 write may carry in all its representations: libghostty-vt buffers a write
// whole before handing it over, up to 64 MiB by default (the protocol's minimum), and fails one
// beyond this limit with EFBIG. It leaves room for other types of the same text (text/html), which
// are dropped
#define KITTY_CLIPBOARD_MAX ((size_t)1 << 20)

// What a notification (OSC 9, OSC 777) keeps of its title and body
#define NOTIFICATION_MAX ((size_t)4096)

// The terminfo entry programs run under (ConsolePty sets TERM to it), the answer to XTGETTCAP's
// TN. Unset, libghostty-vt leaves that query unanswered; the other capabilities it answers from
// Ghostty's own entry either way
static const char TERMINFO_NAME[] = "xterm-256color";

// A program status report (OSC 7501) as appine_vt_take_effects queues it for vt.rs, which reads the
// same layout: u8 state (GhosttyProgramStatusState), u8 kind (GhosttyProgramStatusKind), i8 progress
// (0-100, -1 for none), u8 id length, u8 title length, u16 message length, then the id, title and
// message bytes. The specification's limits (id 128, title 192, message 2048 bytes) fit the fields
#define STATUS_HEADER_BYTES 7

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

static uint32_t clamp_u32(uint64_t v) {
  return v > UINT32_MAX ? UINT32_MAX : (uint32_t)v;
}

static uint32_t argb(GhosttyColorRgb c) {
  return 0xff000000u | ((uint32_t)c.r << 16) | ((uint32_t)c.g << 8) | c.b;
}

static int64_t now_ns(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (int64_t)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

// What appine_vt_take_effects reports (vt.rs's `Effects` has the same layout)
#define APPINE_CLIPBOARD_NONE 0
#define APPINE_CLIPBOARD_TEXT 1
#define APPINE_CLIPBOARD_CLEAR 2

typedef struct {
  uint32_t bells;
  int32_t clipboard;
  const uint8_t *clip;
  size_t clip_len;
  bool notified;
  const uint8_t *title;
  size_t title_len;
  const uint8_t *body;
  size_t body_len;
  // The last progress report (OSC 9;4): its GhosttyTerminalProgressState, -1 for none, and its
  // percentage, -1 when it has none
  int32_t progress_state;
  int32_t progress;
  // Every program status report (OSC 7501), in order, as STATUS_HEADER_BYTES describes
  const uint8_t *statuses;
  size_t statuses_len;
} AppineVtEffects;

typedef struct AppineVt {
  GhosttyTerminal term;
  GhosttyRenderState state;
  GhosttyRenderStateRowIterator rows;
  GhosttyRenderStateRowCells cells;
  GhosttyKeyEncoder encoder;
  GhosttyKeyEvent event;
  GhosttyMouseEncoder mouse;
  GhosttyMouseEvent mouse_event;
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
  // The last text handed out (copy, title, transcript), valid until the next such call
  Buf text;
  // A program set the title (OSC 0/2) since appine_vt_take_title_changed
  bool title_changed;
  // Effects for the app since appine_vt_take_effects: bells rung (BEL), the last clipboard write
  // (OSC 52: text, or a request to clear), the last notification (OSC 9, OSC 777), the last
  // progress report (OSC 9;4; progress_state -1 for none) and the program status reports (OSC 7501)
  uint32_t bells;
  int clipboard;
  Buf clip;
  bool notified;
  Buf note_title;
  Buf note_body;
  int32_t progress_state;
  int32_t progress;
  Buf statuses;
  // The fixed end of a selection being dragged (appine_vt_select_to): the word first selected,
  // or the end opposite the handle being dragged; tracked, so output does not move it
  GhosttyTrackedGridRef anchor_start;
  GhosttyTrackedGridRef anchor_end;
  // Synchronized output: the render state was captured when the hold began and is not updated
  // again until it ends (or HOLD_LIMIT_NS passes)
  bool held;
  int64_t held_since;
  // The cell of the last mouse event reported, to report motion once per cell rather than for
  // every pixel a drag or hover moves: the encoder's own deduplication forgets the cell whenever
  // its options are set, which appine_vt_mouse does for every event
  int mouse_x;
  int mouse_y;
  // The search (appine_vt_search), NULL while none runs. Frames drive it: each one catches it up
  // with the terminal and marks its matches in the viewport
  GhosttySearch search;
  // Scratch for the matches on the viewport's pages
  GhosttySelection *matches;
  size_t matches_cap;
  // The search mark (MARK_*) of each viewport cell, row by row: as of this frame, and as last sent
  // (a row whose marks changed is sent again); marks_cells cells each, the grid's when allocated
  uint8_t *marks;
  uint8_t *marks_sent;
  size_t marks_cells;
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

// Without an answer to DA, programs that probe for it (vim, tmux) wait for a timeout. Clipboard
// access (52) is listed because clipboard writes are taken (on_clipboard_write), as Ghostty lists
// it unless clipboard-write = deny; programs looking for it treat OSC 52 as unsupported otherwise
static bool on_device_attributes(GhosttyTerminal t, void *userdata, GhosttyDeviceAttributes *out) {
  (void)t;
  (void)userdata;
  out->primary.conformance_level = GHOSTTY_DA_CONFORMANCE_VT220;
  out->primary.features[0] = GHOSTTY_DA_FEATURE_COLUMNS_132;
  out->primary.features[1] = GHOSTTY_DA_FEATURE_SELECTIVE_ERASE;
  out->primary.features[2] = GHOSTTY_DA_FEATURE_ANSI_COLOR;
  out->primary.features[3] = GHOSTTY_DA_FEATURE_CLIPBOARD;
  out->primary.num_features = 4;
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

static void on_title_changed(GhosttyTerminal t, void *userdata) {
  (void)t;
  AppineVt *vt = userdata;
  vt->title_changed = true;
}

static void on_bell(GhosttyTerminal t, void *userdata) {
  (void)t;
  AppineVt *vt = userdata;
  vt->bells++;
}

static bool keep(Buf *b, GhosttyString s, size_t max) {
  size_t len = s.len < max ? s.len : max;
  b->len = 0;
  if (!buf_reserve(b, len)) return false;
  memcpy(b->ptr, s.ptr, len);
  b->len = len;
  return true;
}

static bool is_text(GhosttyString mime) {
  static const char plain[] = "text/plain";
  return mime.len >= sizeof(plain) - 1 && memcmp(mime.ptr, plain, sizeof(plain) - 1) == 0;
}

// A program writes the clipboard (OSC 52, OSC 1337, OSC 5522; Ghostty's default
// clipboard-write = allow). Android has one clipboard, so every location goes there; only text is
// taken, up to CLIPBOARD_MAX. Reads (OSC 52 "?") have no callback and so go unanswered, as with
// Ghostty's clipboard-read = deny
static void on_clipboard_write(GhosttyTerminal t, void *userdata, const GhosttyClipboardWrite *w) {
  (void)t;
  AppineVt *vt = userdata;
  GhosttyClipboardWriteReply reply = {.size = sizeof(reply)};
  const GhosttyClipboardContent *text = NULL;
  for (size_t i = 0; i < w->contents_len && text == NULL; i++) {
    if (is_text(w->contents[i].mime)) text = &w->contents[i];
  }
  if (w->contents_len == 0) {
    vt->clipboard = APPINE_CLIPBOARD_CLEAR;
    reply.result = GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS;
  } else if (text == NULL) {
    reply.result = GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED;
  } else if (text->data.len > CLIPBOARD_MAX) {
    reply.result = GHOSTTY_CLIPBOARD_WRITE_RESULT_DENIED;
  } else if (!keep(&vt->clip, text->data, CLIPBOARD_MAX)) {
    reply.result = GHOSTTY_CLIPBOARD_WRITE_RESULT_IO_ERROR;
  } else {
    vt->clipboard = APPINE_CLIPBOARD_TEXT;
    reply.result = GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS;
  }
  w->reply(w, &reply);
}

// OSC 9 (body only) or OSC 777 notify (title and body)
static void on_notification(GhosttyTerminal t, void *userdata,
                            const GhosttyTerminalDesktopNotification *n) {
  (void)t;
  AppineVt *vt = userdata;
  if (keep(&vt->note_title, n->title, NOTIFICATION_MAX) &&
      keep(&vt->note_body, n->body, NOTIFICATION_MAX)) {
    vt->notified = true;
  }
}

// OSC 9;4: only the last report of a read matters, as each replaces the one before. A full reset
// reports a removal
static void on_progress(GhosttyTerminal t, void *userdata, const GhosttyTerminalProgressReport *r) {
  (void)t;
  AppineVt *vt = userdata;
  if (r->size < offsetof(GhosttyTerminalProgressReport, progress) + sizeof(r->progress)) return;
  vt->progress_state = (int32_t)r->state;
  vt->progress = r->progress;
}

static size_t at_most(size_t len, size_t max) {
  return len < max ? len : max;
}

// OSC 7501: every report is queued, since each replaces the record of its id only; the app keeps
// the records (VtSession). A full reset reports a clear of every record
static void on_program_status(GhosttyTerminal t, void *userdata,
                              const GhosttyTerminalProgramStatus *s) {
  (void)t;
  AppineVt *vt = userdata;
  if (s->size < offsetof(GhosttyTerminalProgramStatus, message) + sizeof(s->message)) return;
  size_t id = at_most(s->id.len, UINT8_MAX);
  size_t title = at_most(s->title.len, UINT8_MAX);
  size_t message = at_most(s->message.len, UINT16_MAX);
  if (!buf_reserve(&vt->statuses, STATUS_HEADER_BYTES + id + title + message)) return;
  uint8_t *p = vt->statuses.ptr + vt->statuses.len;
  p[0] = (uint8_t)s->state;
  p[1] = (uint8_t)s->kind;
  p[2] = (uint8_t)s->progress;
  p[3] = (uint8_t)id;
  p[4] = (uint8_t)title;
  put_u16(p + 5, (uint16_t)message);
  p += STATUS_HEADER_BYTES;
  if (id > 0) memcpy(p, s->id.ptr, id);
  if (title > 0) memcpy(p + id, s->title.ptr, title);
  if (message > 0) memcpy(p + id + title, s->message.ptr, message);
  vt->statuses.len += STATUS_HEADER_BYTES + id + title + message;
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
      ghostty_key_event_new(NULL, &vt->event) != GHOSTTY_SUCCESS ||
      ghostty_mouse_encoder_new(NULL, &vt->mouse) != GHOSTTY_SUCCESS ||
      ghostty_mouse_event_new(NULL, &vt->mouse_event) != GHOSTTY_SUCCESS) {
    appine_vt_free(vt);
    return NULL;
  }
  // Cell sizes are only known to libghostty-vt through resize (size reports, images)
  ghostty_terminal_resize(vt->term, cols, rows, cell_width, cell_height);
  vt->mouse_x = -1;
  vt->mouse_y = -1;
  vt->progress_state = -1;

  // The value itself is the userdata (since 1.4; it was read through *(void **)value before)
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_USERDATA, vt);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_WRITE_PTY, (const void *)on_write_pty);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SIZE, (const void *)on_size);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_COLOR_SCHEME, (const void *)on_color_scheme);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES,
                       (const void *)on_device_attributes);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_XTVERSION, (const void *)on_xtversion);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_RENDER_HOLD, (const void *)on_render_hold);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_TITLE_CHANGED,
                       (const void *)on_title_changed);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_BELL, (const void *)on_bell);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE,
                       (const void *)on_clipboard_write);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION,
                       (const void *)on_notification);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT, (const void *)on_progress);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS,
                       (const void *)on_program_status);
  GhosttyString terminfo = {.ptr = (const uint8_t *)TERMINFO_NAME,
                            .len = sizeof(TERMINFO_NAME) - 1};
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_TERMINFO_NAME, &terminfo);
  size_t lines = SCROLLBACK_LINES;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES, &lines);
  size_t bytes = SCROLLBACK_BYTES;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES, &bytes);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, &DEFAULT_FOREGROUND);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND, &DEFAULT_BACKGROUND);
  size_t kitty_clipboard = KITTY_CLIPBOARD_MAX;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE_MAX_BYTES, &kitty_clipboard);
  // Grapheme clustering (mode 2027) by default, as in Ghostty (grapheme-width-method = unicode):
  // emoji sequences (ZWJ, skin tones, flags) take one or two cells instead of one per codepoint.
  // Programs that count widths per codepoint (wcwidth) then disagree on such sequences; they can
  // turn the mode off
  GhosttyTerminalModeConfig graphemes = {.mode = GHOSTTY_MODE_GRAPHEME_CLUSTER, .value = true};
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_MODE_DEFAULT, &graphemes);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_MODE, &graphemes);
  // Images and registered glyphs are never drawn (frames carry cells only), so neither protocol is
  // advertised: libghostty-vt's 10 MB default would answer a Kitty graphics query with OK and
  // a Glyph Protocol query with support, and programs such as yazi or chafa would send images that
  // never appear instead of falling back to text. A limit of 0 turns the Kitty protocol off
  // entirely (queries unanswered, as in a terminal without it); a full reset keeps the limit
  uint64_t kitty_limit = 0;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT, &kitty_limit);
  bool glyphs = false;
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_GLYPH_PROTOCOL, &glyphs);
  return vt;
}

static void drop_anchor(AppineVt *vt);

void appine_vt_free(AppineVt *vt) {
  if (vt == NULL) return;
  drop_anchor(vt);
  // Before the terminal, so it releases what it tracks there
  if (vt->search) ghostty_search_free(vt->search);
  free(vt->matches);
  free(vt->marks);
  free(vt->marks_sent);
  if (vt->mouse_event) ghostty_mouse_event_free(vt->mouse_event);
  if (vt->mouse) ghostty_mouse_encoder_free(vt->mouse);
  if (vt->event) ghostty_key_event_free(vt->event);
  if (vt->encoder) ghostty_key_encoder_free(vt->encoder);
  if (vt->cells) ghostty_render_state_row_cells_free(vt->cells);
  if (vt->rows) ghostty_render_state_row_iterator_free(vt->rows);
  if (vt->state) ghostty_render_state_free(vt->state);
  if (vt->term) ghostty_terminal_free(vt->term);
  free(vt->replies.ptr);
  free(vt->frame.ptr);
  free(vt->extras.ptr);
  free(vt->text.ptr);
  free(vt->clip.ptr);
  free(vt->note_title.ptr);
  free(vt->note_body.ptr);
  free(vt->statuses.ptr);
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

static void search_reselect(AppineVt *vt, size_t index);

// Resizes the terminal. A search keeps its selected match: libghostty-vt drops it on a resize,
// which here comes with every keyboard shown or hidden
void appine_vt_resize(AppineVt *vt, uint16_t cols, uint16_t rows, uint32_t cell_width,
                      uint32_t cell_height) {
  vt->cols = cols;
  vt->height = rows;
  vt->cell_width = cell_width;
  vt->cell_height = cell_height;
  size_t selected = 0;
  bool reselect = vt->search != NULL &&
                  ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_SELECTED_INDEX, &selected) ==
                      GHOSTTY_SUCCESS;
  ghostty_terminal_resize(vt->term, cols, rows, cell_width, cell_height);
  if (reselect) search_reselect(vt, selected);
}

// Selects the match index places from the newest again, after a resize: the search has to catch
// up first (a resize restarts it, the matches are the same text), then steps from the newest
// without scrolling, but for the last step, which brings the match into view if it is not
static void search_reselect(AppineVt *vt, size_t index) {
  if (ghostty_search_run(vt->search) != GHOSTTY_SUCCESS) return;
  GhosttySearchScroll quiet = GHOSTTY_SEARCH_SCROLL_NONE;
  ghostty_search_set(vt->search, GHOSTTY_SEARCH_OPT_SELECT_SCROLL, &quiet);
  for (size_t i = 0; i < index; i++) {
    if (ghostty_search_set(vt->search, GHOSTTY_SEARCH_OPT_SELECT_NEXT, NULL) != GHOSTTY_SUCCESS) {
      break;
    }
  }
  ghostty_search_set(vt->search, GHOSTTY_SEARCH_OPT_SELECT_SCROLL, NULL);
  ghostty_search_set(vt->search, GHOSTTY_SEARCH_OPT_SELECT_NEXT, NULL);
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

static bool mode_on(AppineVt *vt, GhosttyMode mode) {
  GhosttyTerminalModeConfig m = {.mode = mode};
  return ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_MODE, &m) == GHOSTTY_SUCCESS &&
         m.value;
}

// ---- Frames ----

// Writes the row the iterator is on as row y of the frame; marks is the row's search marks (NULL
// for none)
static bool put_row(AppineVt *vt, uint16_t y, const uint8_t *marks) {
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
    if (marks != NULL) attrs |= (uint16_t)((uint32_t)marks[count] << MARK_SHIFT);

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

  GhosttyRenderStateRowSelection sel = GHOSTTY_INIT_SIZED(GhosttyRenderStateRowSelection);
  bool selected = ghostty_render_state_row_get(vt->rows, GHOSTTY_RENDER_STATE_ROW_DATA_SELECTION,
                                               &sel) == GHOSTTY_SUCCESS;

  uint8_t *h = vt->frame.ptr + row_at;
  put_u16(h, y);
  put_u16(h + 2, count);
  put_u32(h + 4, (uint32_t)(vt->extras.len / 4));
  put_u16(h + 8, selected ? sel.start_x : NONE16);
  put_u16(h + 10, selected ? sel.end_x : NONE16);
  return true;
}

// Writes a grid reference's viewport coordinates, or 0xffff for both when it is not in the
// viewport
static void put_viewport(AppineVt *vt, uint8_t *p, const GhosttyGridRef *ref) {
  GhosttyPointCoordinate c;
  if (ghostty_terminal_point_from_grid_ref(vt->term, ref, GHOSTTY_POINT_TAG_VIEWPORT, &c) ==
          GHOSTTY_SUCCESS &&
      c.y < NONE16) {
    put_u16(p, c.x);
    put_u16(p + 2, (uint16_t)c.y);
  } else {
    put_u16(p, NONE16);
    put_u16(p + 2, NONE16);
  }
}

static bool search_catch_up(AppineVt *vt);
static bool mark_matches(AppineVt *vt);

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
  bool search_pending = vt->search != NULL && !search_catch_up(vt);
  // Without memory for the marks, rows go without them
  bool marked = mark_matches(vt);
  uint32_t search_total = 0;
  uint32_t search_selected = 0;
  if (vt->search != NULL) {
    size_t total = 0;
    size_t index = 0;
    ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_TOTAL_MATCHES, &total);
    if (ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_SELECTED_INDEX, &index) ==
        GHOSTTY_SUCCESS) {
      search_selected = clamp_u32((uint64_t)index + 1);
    }
    search_total = clamp_u32(total);
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
  GhosttyTerminalScrollbar scrollbar = {0};
  ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_SCROLLBAR, &scrollbar);

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
  uint8_t flags = 0;
  bool mouse = false;
  ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING, &mouse);
  if (mouse) flags |= FLAG_MOUSE;
  GhosttyTerminalScreen screen = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
  ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen);
  if (screen == GHOSTTY_TERMINAL_SCREEN_ALTERNATE) flags |= FLAG_ALT_SCREEN;
  if (mode_on(vt, GHOSTTY_MODE_ALT_SCROLL)) flags |= FLAG_ALT_SCROLL;
  if (mode_on(vt, GHOSTTY_MODE_FOCUS_EVENT)) flags |= FLAG_FOCUS_EVENTS;
  put_u16(h + 28, NONE16);
  put_u16(h + 30, NONE16);
  put_u16(h + 32, NONE16);
  put_u16(h + 34, NONE16);
  GhosttySelection sel = GHOSTTY_INIT_SIZED(GhosttySelection);
  GhosttySelection ordered = GHOSTTY_INIT_SIZED(GhosttySelection);
  if (ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_SELECTION, &sel) == GHOSTTY_SUCCESS &&
      ghostty_terminal_selection_ordered(vt->term, &sel, GHOSTTY_SELECTION_ORDER_FORWARD,
                                         &ordered) == GHOSTTY_SUCCESS) {
    flags |= FLAG_SELECTION;
    if (ordered.rectangle) flags |= FLAG_RECTANGLE;
    put_viewport(vt, h + 28, &ordered.start);
    put_viewport(vt, h + 32, &ordered.end);
  }
  if (search_pending) flags |= FLAG_SEARCH_PENDING;
  h[27] = flags;
  put_u32(h + 36, clamp_u32(scrollbar.total));
  put_u32(h + 40, clamp_u32(scrollbar.offset));
  put_u32(h + 44, clamp_u32(scrollbar.len));
  put_u32(h + 48, search_total);
  put_u32(h + 52, search_selected);
  vt->frame.len = HEADER_BYTES;

  uint16_t count = 0;
  if (ghostty_render_state_get(vt->state, GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, &vt->rows) ==
      GHOSTTY_SUCCESS) {
    uint16_t y = 0;
    while (ghostty_render_state_row_iterator_next(vt->rows)) {
      const uint8_t *marks = marked && y < vt->height ? vt->marks + (size_t)y * vt->cols : NULL;
      bool changed = dirty == GHOSTTY_RENDER_STATE_DIRTY_FULL;
      if (!changed && dirty != GHOSTTY_RENDER_STATE_DIRTY_FALSE) {
        ghostty_render_state_row_get(vt->rows, GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY, &changed);
      }
      if (!changed && marks != NULL) {
        changed = memcmp(marks, vt->marks_sent + (size_t)y * vt->cols, vt->cols) != 0;
      }
      if (changed) {
        if (!put_row(vt, y, marks)) return NULL;
        count++;
      }
      y++;
    }
  }
  if (marked) memcpy(vt->marks_sent, vt->marks, vt->marks_cells);
  // put_row may have moved the buffer
  put_u16(vt->frame.ptr + 24, count);
  ghostty_render_state_clean(vt->state);
  *len = vt->frame.len;
  return vt->frame.ptr;
}

// Catches the search up with the terminal: feeds it what changed and works through what it has,
// for at most SEARCH_BUDGET_NS. True once it is caught up, false when it needs more frames.
static bool search_catch_up(AppineVt *vt) {
  int64_t start = now_ns();
  for (;;) {
    if (ghostty_search_feed(vt->search) != GHOSTTY_SUCCESS) return true;
    GhosttySearchStatus status = GHOSTTY_SEARCH_STATUS_COMPLETE;
    do {
      if (ghostty_search_tick(vt->search, &status) != GHOSTTY_SUCCESS) return true;
    } while (status == GHOSTTY_SEARCH_STATUS_RUNNING && now_ns() - start < SEARCH_BUDGET_NS);
    if (status == GHOSTTY_SEARCH_STATUS_COMPLETE) return true;
    // Running out of time, or waiting for the next part of the scrollback
    if (now_ns() - start >= SEARCH_BUDGET_NS) return false;
  }
}

static bool viewport_ref(AppineVt *vt, uint16_t x, uint16_t y, GhosttyGridRef *out);
static bool screen_point(AppineVt *vt, const GhosttyGridRef *ref, GhosttyPointCoordinate *out);

// Marks the viewport cells match covers with mark (over any mark there before); top is the
// viewport's first row in screen coordinates. Screen coordinates, unlike viewport ones, exist for a
// match that begins above the viewport or ends below it, so it is cut to the rows shown.
static void mark_match(AppineVt *vt, const GhosttySelection *match, uint32_t top, uint8_t mark) {
  GhosttySelection ordered = GHOSTTY_INIT_SIZED(GhosttySelection);
  GhosttyPointCoordinate start, end;
  if (ghostty_terminal_selection_ordered(vt->term, match, GHOSTTY_SELECTION_ORDER_FORWARD,
                                         &ordered) != GHOSTTY_SUCCESS ||
      !screen_point(vt, &ordered.start, &start) || !screen_point(vt, &ordered.end, &end) ||
      end.y < top || start.y >= top + vt->height) {
    return;
  }
  for (uint32_t y = start.y < top ? top : start.y; y <= end.y && y < top + vt->height; y++) {
    uint16_t from = y == start.y ? start.x : 0;
    uint16_t to = y == end.y ? end.x : vt->cols - 1;
    if (to >= vt->cols) to = vt->cols - 1;
    uint8_t *row = vt->marks + (size_t)(y - top) * vt->cols;
    for (uint16_t x = from; x <= to; x++) row[x] = mark;
  }
}

// Fills vt->marks for this frame: the search's matches in the viewport and its selected match,
// none without a search. False when the marks cannot be allocated.
static bool mark_matches(AppineVt *vt) {
  size_t cells = (size_t)vt->cols * vt->height;
  if (cells != vt->marks_cells) {
    // A new grid: its first frame is a full one, and every row is sent
    free(vt->marks);
    free(vt->marks_sent);
    vt->marks = calloc(cells, 1);
    vt->marks_sent = calloc(cells, 1);
    if (vt->marks == NULL || vt->marks_sent == NULL) {
      free(vt->marks);
      free(vt->marks_sent);
      vt->marks = NULL;
      vt->marks_sent = NULL;
      vt->marks_cells = 0;
      return false;
    }
    vt->marks_cells = cells;
  }
  memset(vt->marks, MARK_NONE, cells);
  GhosttyGridRef first;
  GhosttyPointCoordinate top;
  if (vt->search == NULL || !viewport_ref(vt, 0, 0, &first) || !screen_point(vt, &first, &top)) {
    return true;
  }
  GhosttySelectionBuffer found = {.ptr = vt->matches, .cap = vt->matches_cap};
  GhosttyResult r = ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_VIEWPORT_MATCHES, &found);
  if (r == GHOSTTY_OUT_OF_SPACE) {
    GhosttySelection *p = realloc(vt->matches, found.len * sizeof(*p));
    if (p == NULL) return true;
    vt->matches = p;
    vt->matches_cap = found.len;
    found = (GhosttySelectionBuffer){.ptr = vt->matches, .cap = vt->matches_cap};
    r = ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_VIEWPORT_MATCHES, &found);
  }
  if (r == GHOSTTY_SUCCESS) {
    for (size_t i = 0; i < found.len; i++) mark_match(vt, &vt->matches[i], top.y, MARK_MATCH);
  }
  GhosttySelection selected = GHOSTTY_INIT_SIZED(GhosttySelection);
  if (ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_SELECTED_MATCH, &selected) ==
      GHOSTTY_SUCCESS) {
    mark_match(vt, &selected, top.y, MARK_SELECTED);
  }
  return true;
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

// ---- Paste ----

typedef struct {
  const uint8_t *ptr;
  size_t len;
} PasteText;

static bool paste_read(void *userdata, GhosttyString mime, GhosttyWriter writer) {
  (void)mime;
  const PasteText *text = userdata;
  return writer.write(writer.userdata, text->ptr, text->len);
}

// Pastes text from the clipboard as the terminal's modes ask (bracketed with mode 2004, else
// newlines as carriage returns; unsafe control bytes become spaces); the bytes for the console
// are left in the replies. Returns 0 when pasted, 1 when refused because the text could run
// commands (a line break without bracketed paste, or the bracketed paste terminator) and
// allow_unsafe is false, -1 on failure. Nothing is written unless it returns 0.
int appine_vt_paste(AppineVt *vt, const uint8_t *data, size_t len, bool allow_unsafe) {
  static const char plain[] = "text/plain";
  GhosttyString mime = {.ptr = (const uint8_t *)plain, .len = sizeof(plain) - 1};
  PasteText text = {.ptr = data, .len = len};
  GhosttyPaste paste = GHOSTTY_INIT_SIZED(GhosttyPaste);
  paste.location = GHOSTTY_CLIPBOARD_LOCATION_STANDARD;
  paste.source = GHOSTTY_PASTE_SOURCE_CLIPBOARD;
  paste.mimes = &mime;
  paste.mimes_len = 1;
  paste.reader = (GhosttyMimeReader){.read = paste_read, .userdata = &text};
  paste.allow_unsafe = allow_unsafe;
  GhosttyResult r = ghostty_terminal_paste(vt->term, &paste, NULL);
  if (r == GHOSTTY_REJECTED) return 1;
  return r == GHOSTTY_SUCCESS ? 0 : -1;
}

// ---- Mouse and focus ----

// Encodes a mouse event at pixel (x, y) of the grid for the program, in its tracking mode and
// format; returns the length, 0 when the program does not take it (no tracking, or a motion it
// did not ask for), -1 when it does not fit. action 0 press, 1 release, 2 motion; button 0 none
// (motion with no button held), 1 left, 2 right, 3 middle, 4 and 5 the wheel.
ptrdiff_t appine_vt_mouse(AppineVt *vt, int action, int button, uint16_t mods, float x, float y,
                          bool pressed, char *out, size_t cap) {
  // The cell as the encoder takes it (clamped to the grid); SGR-Pixels reports every pixel
  int cx = x < 0 ? 0 : (int)(x / (float)vt->cell_width);
  int cy = y < 0 ? 0 : (int)(y / (float)vt->cell_height);
  if (cx >= vt->cols) cx = vt->cols - 1;
  if (cy >= vt->height) cy = vt->height - 1;
  if (action == GHOSTTY_MOUSE_ACTION_MOTION && cx == vt->mouse_x && cy == vt->mouse_y &&
      !mode_on(vt, GHOSTTY_MODE_SGR_PIXELS_MOUSE)) {
    return 0;
  }
  ghostty_mouse_encoder_setopt_from_terminal(vt->mouse, vt->term);
  GhosttyMouseEncoderSize size = GHOSTTY_INIT_SIZED(GhosttyMouseEncoderSize);
  size.screen_width = (uint32_t)vt->cols * vt->cell_width;
  size.screen_height = (uint32_t)vt->height * vt->cell_height;
  size.cell_width = vt->cell_width;
  size.cell_height = vt->cell_height;
  ghostty_mouse_encoder_setopt(vt->mouse, GHOSTTY_MOUSE_ENCODER_OPT_SIZE, &size);
  ghostty_mouse_encoder_setopt(vt->mouse, GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED, &pressed);
  ghostty_mouse_event_set_action(vt->mouse_event, (GhosttyMouseAction)action);
  if (button == 0) {
    ghostty_mouse_event_clear_button(vt->mouse_event);
  } else {
    ghostty_mouse_event_set_button(vt->mouse_event, (GhosttyMouseButton)button);
  }
  ghostty_mouse_event_set_mods(vt->mouse_event, mods);
  ghostty_mouse_event_set_position(vt->mouse_event, (GhosttyMousePosition){.x = x, .y = y});
  size_t len = 0;
  if (ghostty_mouse_encoder_encode(vt->mouse, vt->mouse_event, out, cap, &len) !=
      GHOSTTY_SUCCESS) {
    return -1;
  }
  if (len > 0) {
    vt->mouse_x = cx;
    vt->mouse_y = cy;
  }
  return (ptrdiff_t)len;
}

// Encodes focus gained or lost when the program asked for focus events (mode 1004); returns the
// length, 0 when it did not ask, -1 when it does not fit.
ptrdiff_t appine_vt_focus(AppineVt *vt, bool gained, char *out, size_t cap) {
  if (!mode_on(vt, GHOSTTY_MODE_FOCUS_EVENT)) return 0;
  size_t len = 0;
  if (ghostty_focus_encode(gained ? GHOSTTY_FOCUS_GAINED : GHOSTTY_FOCUS_LOST, out, cap, &len) !=
      GHOSTTY_SUCCESS) {
    return -1;
  }
  return (ptrdiff_t)len;
}

// Copies text allocated by libghostty-vt into vt->text and frees it; NULL with len 0 when empty
// or out of memory
static const uint8_t *keep_text(AppineVt *vt, uint8_t *data, size_t data_len, size_t *len) {
  vt->text.len = 0;
  bool ok = data_len > 0 && buf_reserve(&vt->text, data_len);
  if (ok) {
    memcpy(vt->text.ptr, data, data_len);
    vt->text.len = data_len;
  }
  ghostty_free(NULL, data, data_len);
  *len = vt->text.len;
  return ok ? vt->text.ptr : NULL;
}

// ---- Selection ----
//
// The selection itself is the terminal's (GHOSTTY_TERMINAL_OPT_SELECTION): it follows its cells
// through output and scrolling, and frames report it. Grid references from the API are only
// valid until the terminal changes, so every function takes fresh ones.

static bool viewport_ref(AppineVt *vt, uint16_t x, uint16_t y, GhosttyGridRef *out) {
  if (x >= vt->cols) x = vt->cols - 1;
  if (y >= vt->height) y = vt->height - 1;
  GhosttyPoint p = {.tag = GHOSTTY_POINT_TAG_VIEWPORT, .value.coordinate = {.x = x, .y = y}};
  *out = GHOSTTY_INIT_SIZED(GhosttyGridRef);
  return ghostty_terminal_grid_ref(vt->term, p, out) == GHOSTTY_SUCCESS;
}

static void drop_anchor(AppineVt *vt) {
  if (vt->anchor_start) ghostty_tracked_grid_ref_free(vt->anchor_start);
  if (vt->anchor_end) ghostty_tracked_grid_ref_free(vt->anchor_end);
  vt->anchor_start = NULL;
  vt->anchor_end = NULL;
}

// Tracks the cells at the screen coordinates (start, end) as the anchor
static bool set_anchor(AppineVt *vt, GhosttyPointCoordinate start, GhosttyPointCoordinate end) {
  drop_anchor(vt);
  GhosttyPoint a = {.tag = GHOSTTY_POINT_TAG_SCREEN, .value.coordinate = start};
  GhosttyPoint b = {.tag = GHOSTTY_POINT_TAG_SCREEN, .value.coordinate = end};
  if (ghostty_terminal_grid_ref_track(vt->term, a, &vt->anchor_start) != GHOSTTY_SUCCESS ||
      ghostty_terminal_grid_ref_track(vt->term, b, &vt->anchor_end) != GHOSTTY_SUCCESS) {
    drop_anchor(vt);
    return false;
  }
  return true;
}

static bool screen_point(AppineVt *vt, const GhosttyGridRef *ref, GhosttyPointCoordinate *out) {
  return ghostty_terminal_point_from_grid_ref(vt->term, ref, GHOSTTY_POINT_TAG_SCREEN, out) ==
         GHOSTTY_SUCCESS;
}

static bool set_selection(AppineVt *vt, GhosttyGridRef start, GhosttyGridRef end, bool rectangle) {
  GhosttySelection sel = GHOSTTY_INIT_SIZED(GhosttySelection);
  sel.start = start;
  sel.end = end;
  sel.rectangle = rectangle;
  return ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SELECTION, &sel) == GHOSTTY_SUCCESS;
}

// Installs a selection libghostty-vt derived (a word, a line) and anchors later drags to it; false
// when it cannot be anchored.
static bool select_anchored(AppineVt *vt, const GhosttySelection *derived) {
  GhosttySelection ordered = GHOSTTY_INIT_SIZED(GhosttySelection);
  GhosttyPointCoordinate start, end;
  if (ghostty_terminal_selection_ordered(vt->term, derived, GHOSTTY_SELECTION_ORDER_FORWARD,
                                         &ordered) != GHOSTTY_SUCCESS ||
      !screen_point(vt, &ordered.start, &start) || !screen_point(vt, &ordered.end, &end)) {
    return false;
  }
  // Tracking may change the terminal's page list, so it is selected from fresh references
  GhosttyGridRef a = GHOSTTY_INIT_SIZED(GhosttyGridRef);
  GhosttyGridRef b = GHOSTTY_INIT_SIZED(GhosttyGridRef);
  return set_anchor(vt, start, end) &&
         ghostty_tracked_grid_ref_snapshot(vt->anchor_start, &a) == GHOSTTY_SUCCESS &&
         ghostty_tracked_grid_ref_snapshot(vt->anchor_end, &b) == GHOSTTY_SUCCESS &&
         set_selection(vt, a, b, false);
}

// Selects the word at viewport cell (x, y) and anchors later drags to it; false when there is no
// word there (blank cells), with any earlier selection cleared.
bool appine_vt_select_word(AppineVt *vt, uint16_t x, uint16_t y) {
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SELECTION, NULL);
  drop_anchor(vt);
  GhosttyTerminalSelectWordOptions options = GHOSTTY_INIT_SIZED(GhosttyTerminalSelectWordOptions);
  GhosttySelection word = GHOSTTY_INIT_SIZED(GhosttySelection);
  return viewport_ref(vt, x, y, &options.ref) &&
         ghostty_terminal_select_word(vt->term, &options, &word) == GHOSTTY_SUCCESS &&
         select_anchored(vt, &word);
}

// Selects the line at viewport cell (x, y), as Ghostty's triple click does: the whole soft-wrapped
// line with its leading and trailing blanks trimmed, stopping where a prompt mark (OSC 133) starts
// or ends a part of it, so a command is selected without its prompt; false when the line is blank,
// with any earlier selection cleared.
bool appine_vt_select_line(AppineVt *vt, uint16_t x, uint16_t y) {
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SELECTION, NULL);
  drop_anchor(vt);
  GhosttyTerminalSelectLineOptions options = GHOSTTY_INIT_SIZED(GhosttyTerminalSelectLineOptions);
  // Off by default in the C API, on in Ghostty's own selectLine
  options.semantic_prompt_boundary = true;
  GhosttySelection line = GHOSTTY_INIT_SIZED(GhosttySelection);
  return viewport_ref(vt, x, y, &options.ref) &&
         ghostty_terminal_select_line(vt->term, &options, &line) == GHOSTTY_SUCCESS &&
         select_anchored(vt, &line);
}

// Starts dragging a selection handle: the other end becomes the anchor. end is the handle of the
// selection's last cell, else its first. False when there is no selection.
bool appine_vt_select_grab(AppineVt *vt, bool end) {
  GhosttySelection sel = GHOSTTY_INIT_SIZED(GhosttySelection);
  GhosttySelection ordered = GHOSTTY_INIT_SIZED(GhosttySelection);
  GhosttyPointCoordinate fixed;
  if (ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_SELECTION, &sel) != GHOSTTY_SUCCESS ||
      ghostty_terminal_selection_ordered(vt->term, &sel, GHOSTTY_SELECTION_ORDER_FORWARD,
                                         &ordered) != GHOSTTY_SUCCESS ||
      !screen_point(vt, end ? &ordered.start : &ordered.end, &fixed)) {
    return false;
  }
  return set_anchor(vt, fixed, fixed);
}

static bool rectangular(AppineVt *vt) {
  GhosttySelection sel = GHOSTTY_INIT_SIZED(GhosttySelection);
  return ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_SELECTION, &sel) ==
             GHOSTTY_SUCCESS &&
         sel.rectangle;
}

// Extends the selection from the anchor to viewport cell (x, y): the anchor stays selected
// whole, and the selection runs from its far side to the cell; a rectangle stays one, with the
// anchor and the cell at opposite corners. False without an anchor.
bool appine_vt_select_to(AppineVt *vt, uint16_t x, uint16_t y) {
  if (vt->anchor_start == NULL || vt->anchor_end == NULL) return false;
  GhosttyGridRef start = GHOSTTY_INIT_SIZED(GhosttyGridRef);
  GhosttyGridRef end = GHOSTTY_INIT_SIZED(GhosttyGridRef);
  GhosttyGridRef cell;
  bool rectangle = rectangular(vt);
  if (ghostty_tracked_grid_ref_snapshot(vt->anchor_start, &start) != GHOSTTY_SUCCESS ||
      ghostty_tracked_grid_ref_snapshot(vt->anchor_end, &end) != GHOSTTY_SUCCESS ||
      !viewport_ref(vt, x, y, &cell)) {
    return false;
  }
  GhosttySelection probe = GHOSTTY_INIT_SIZED(GhosttySelection);
  GhosttySelectionOrder order = GHOSTTY_SELECTION_ORDER_FORWARD;
  probe.start = start;
  probe.end = cell;
  ghostty_terminal_selection_order(vt->term, &probe, &order);
  if (order == GHOSTTY_SELECTION_ORDER_REVERSE) return set_selection(vt, end, cell, rectangle);
  probe.start = end;
  ghostty_terminal_selection_order(vt->term, &probe, &order);
  return set_selection(vt, start, order == GHOSTTY_SELECTION_ORDER_REVERSE ? end : cell,
                       rectangle);
}

// Turns the selection into a rectangle (on) with its first and last cells as opposite corners, or
// back into a linear selection from the rectangle's top left to its bottom right corner. False
// when there is no selection.
bool appine_vt_select_rectangle(AppineVt *vt, bool on) {
  drop_anchor(vt);
  GhosttySelection sel = GHOSTTY_INIT_SIZED(GhosttySelection);
  GhosttySelection ordered = GHOSTTY_INIT_SIZED(GhosttySelection);
  if (ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_SELECTION, &sel) != GHOSTTY_SUCCESS ||
      ghostty_terminal_selection_ordered(vt->term, &sel, GHOSTTY_SELECTION_ORDER_FORWARD,
                                         &ordered) != GHOSTTY_SUCCESS) {
    return false;
  }
  return set_selection(vt, ordered.start, ordered.end, on);
}

// Selects everything, scrollback included; false when there is nothing to select.
bool appine_vt_select_all(AppineVt *vt) {
  drop_anchor(vt);
  GhosttySelection sel = GHOSTTY_INIT_SIZED(GhosttySelection);
  return ghostty_terminal_select_all(vt->term, &sel) == GHOSTTY_SUCCESS &&
         ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SELECTION, &sel) == GHOSTTY_SUCCESS;
}

void appine_vt_select_clear(AppineVt *vt) {
  drop_anchor(vt);
  ghostty_terminal_set(vt->term, GHOSTTY_TERMINAL_OPT_SELECTION, NULL);
}

// The selection as plain text, soft-wrapped lines joined and trailing blanks trimmed (as
// Ghostty copies); NULL with len 0 when there is no selection or it is empty. Valid until the
// next call.
const uint8_t *appine_vt_copy(AppineVt *vt, size_t *len) {
  *len = 0;
  GhosttyTerminalSelectionFormatOptions options =
      GHOSTTY_INIT_SIZED(GhosttyTerminalSelectionFormatOptions);
  options.emit = GHOSTTY_FORMATTER_FORMAT_PLAIN;
  options.unwrap = true;
  options.trim = true;
  uint8_t *text = NULL;
  size_t text_len = 0;
  if (ghostty_terminal_selection_format_alloc(vt->term, NULL, options, &text, &text_len) !=
      GHOSTTY_SUCCESS) {
    return NULL;
  }
  return keep_text(vt, text, text_len, len);
}

// ---- Links ----
//
// A tap opens the link under it: the cell's OSC 8 hyperlink, else an http or https URL written as
// text, found in the cell's line with its soft wraps joined (libghostty-vt leaves matching URLs in
// text to the embedder; Ghostty's own renderer does it by regex). The scheme an OSC 8 link may
// have is VtView's to check.

// Rows of a soft-wrapped line read above and below the tapped one, bounding a tap's work
#define LINK_ROWS 64

static bool screen_ref(AppineVt *vt, uint16_t x, uint32_t y, GhosttyGridRef *out) {
  GhosttyPoint p = {.tag = GHOSTTY_POINT_TAG_SCREEN, .value.coordinate = {.x = x, .y = y}};
  *out = GHOSTTY_INIT_SIZED(GhosttyGridRef);
  return ghostty_terminal_grid_ref(vt->term, p, out) == GHOSTTY_SUCCESS;
}

// Whether screen row y is soft-wrapped onto the next one
static bool row_wraps(AppineVt *vt, uint32_t y) {
  GhosttyGridRef ref;
  GhosttyRow row;
  bool wrap = false;
  return screen_ref(vt, 0, y, &ref) && ghostty_grid_ref_row(&ref, &row) == GHOSTTY_SUCCESS &&
         ghostty_row_get(row, GHOSTTY_ROW_DATA_WRAP, &wrap) == GHOSTTY_SUCCESS && wrap;
}

// The characters a URL in text is made of: ASCII letters, digits and the punctuation RFC 3986
// allows, less the quote that usually encloses one (as in Ghostty's URL regex). Text in other
// scripts ends a URL, so one followed by Chinese without a space stops before it.
static bool url_char(uint32_t c) {
  return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
         (c != 0 && c < 0x80 && strchr("-._~:/?#[]@!$&*+,;=%()", (int)c) != NULL);
}

// The length of "http://" or "https://" (any case) at cps[i], else 0
static size_t url_scheme(const uint32_t *cps, size_t count, size_t i) {
  static const char *const schemes[] = {"https://", "http://"};
  for (size_t k = 0; k < sizeof(schemes) / sizeof(schemes[0]); k++) {
    size_t n = strlen(schemes[k]);
    size_t j = 0;
    for (; j < n && i + j < count; j++) {
      uint32_t c = cps[i + j];
      if ((c >= 'A' && c <= 'Z' ? c + ('a' - 'A') : c) != (uint32_t)schemes[k][j]) break;
    }
    if (j == n) return n;
  }
  return 0;
}

// Finds the URL around cps[at] in a line of count characters: from the run's first scheme at or
// before at, over URL characters while brackets balance (an unmatched closing one ends it), less
// the punctuation that ends a sentence. Sets [*start, *end) and returns true when at is inside it.
static bool find_url(const uint32_t *cps, size_t count, size_t at, size_t *start, size_t *end) {
  if (at >= count || !url_char(cps[at])) return false;
  size_t s = at;
  while (s > 0 && url_char(cps[s - 1])) s--;
  size_t rest = 0;
  for (; s <= at; s++) {
    if ((rest = url_scheme(cps, count, s)) != 0) break;
  }
  if (rest == 0) return false;
  size_t e = s + rest;
  int depth = 0;
  for (; e < count && url_char(cps[e]); e++) {
    if (cps[e] == '(' || cps[e] == '[') {
      depth++;
    } else if (cps[e] == ')' || cps[e] == ']') {
      if (depth == 0) break;
      depth--;
    }
  }
  while (e > s + rest && strchr(".,;:!?", (int)cps[e - 1]) != NULL) e--;
  if (e == s + rest || at >= e) return false;
  *start = s;
  *end = e;
  return true;
}

// The link at viewport cell (x, y): its OSC 8 URI, else an http or https URL in the text there;
// NULL with len 0 when there is none. Valid until the next text call.
const uint8_t *appine_vt_link(AppineVt *vt, uint16_t x, uint16_t y, size_t *len) {
  *len = 0;
  vt->text.len = 0;
  GhosttyGridRef ref;
  GhosttyPointCoordinate at;
  if (!viewport_ref(vt, x, y, &ref) || !screen_point(vt, &ref, &at)) return NULL;
  size_t n = 0;
  if (ghostty_grid_ref_hyperlink_uri(&ref, NULL, 0, &n) == GHOSTTY_OUT_OF_SPACE && n > 0 &&
      buf_reserve(&vt->text, n) &&
      ghostty_grid_ref_hyperlink_uri(&ref, vt->text.ptr, n, &n) == GHOSTTY_SUCCESS && n > 0) {
    vt->text.len = n;
    *len = n;
    return vt->text.ptr;
  }

  // The tapped row's line: up while the row above wraps onto it, down while it wraps onward
  uint32_t top = at.y;
  uint32_t bottom = at.y;
  while (top > 0 && at.y - top < LINK_ROWS && row_wraps(vt, top - 1)) top--;
  while (bottom - at.y < LINK_ROWS && row_wraps(vt, bottom)) bottom++;
  uint32_t *cps = malloc((size_t)(bottom - top + 1) * vt->cols * sizeof(*cps));
  if (cps == NULL) return NULL;
  // One character per cell, wide characters once (the tapped cell may be their second half);
  // the blank a wide character left at the end of a wrapped row is not part of the line
  size_t count = 0;
  size_t tapped = SIZE_MAX;
  for (uint32_t row = top; row <= bottom; row++) {
    for (uint16_t col = 0; col < vt->cols; col++) {
      GhosttyGridRef c;
      GhosttyCell cell;
      if (!screen_ref(vt, col, row, &c) || ghostty_grid_ref_cell(&c, &cell) != GHOSTTY_SUCCESS) {
        row = bottom;
        break;
      }
      bool here = row == at.y && col == at.x;
      GhosttyCellWide wide = GHOSTTY_CELL_WIDE_NARROW;
      ghostty_cell_get(cell, GHOSTTY_CELL_DATA_WIDE, &wide);
      if (wide == GHOSTTY_CELL_WIDE_SPACER_TAIL || wide == GHOSTTY_CELL_WIDE_SPACER_HEAD) {
        if (here && wide == GHOSTTY_CELL_WIDE_SPACER_TAIL && count > 0) tapped = count - 1;
        continue;
      }
      uint32_t cp = 0;
      ghostty_cell_get(cell, GHOSTTY_CELL_DATA_CODEPOINT, &cp);
      if (here) tapped = count;
      cps[count++] = cp;
    }
  }
  size_t start = 0;
  size_t end = 0;
  bool found = tapped != SIZE_MAX && find_url(cps, count, tapped, &start, &end) &&
               buf_reserve(&vt->text, end - start);
  if (found) {
    // URL characters are all ASCII
    for (size_t i = start; i < end; i++) vt->text.ptr[i - start] = (uint8_t)cps[i];
    vt->text.len = end - start;
    *len = vt->text.len;
  }
  free(cps);
  return found ? vt->text.ptr : NULL;
}

// ---- Search ----
//
// One search per terminal, over the active screen and its scrollback (libghostty-vt keeps the
// primary screen's results while a program uses the alternate one). Frames drive it
// (search_catch_up) and carry its count and marks; the matches themselves stay in libghostty-vt,
// which keeps them on their text through output, resizes and pruned scrollback.

// Searches for needle (UTF-8; ASCII letters match either case, all else exactly), restarting the
// search when it changed; an empty needle leaves nothing found. The next frames find its matches,
// none of them selected. False when the search cannot be made.
bool appine_vt_search(AppineVt *vt, const uint8_t *needle, size_t len) {
  if (vt->search == NULL) {
    if (len == 0) return true;
    if (ghostty_search_new(NULL, &vt->search, vt->term) != GHOSTTY_SUCCESS) {
      vt->search = NULL;
      return false;
    }
  }
  GhosttyString s = {.ptr = needle, .len = len};
  return ghostty_search_set(vt->search, GHOSTTY_SEARCH_OPT_NEEDLE, len > 0 ? &s : NULL) ==
         GHOSTTY_SUCCESS;
}

// Selects the next match towards older output (older; the first call selects the newest match)
// or towards newer output, going round at either end, and scrolls the viewport to it unless it is
// shown already. False when there is no match (or no search).
bool appine_vt_search_select(AppineVt *vt, bool older) {
  if (vt->search == NULL) return false;
  GhosttySearchOption option = older ? GHOSTTY_SEARCH_OPT_SELECT_NEXT : GHOSTTY_SEARCH_OPT_SELECT_PREV;
  return ghostty_search_set(vt->search, option, NULL) == GHOSTTY_SUCCESS;
}

// Ends the search; the next frame sends the rows its marks were on again.
void appine_vt_search_end(AppineVt *vt) {
  if (vt->search) ghostty_search_free(vt->search);
  vt->search = NULL;
}

// ---- Title, effects, messages, transcript ----

// Whether a program set the title since the last call
bool appine_vt_take_title_changed(AppineVt *vt) {
  bool changed = vt->title_changed;
  vt->title_changed = false;
  return changed;
}

// The bells, clipboard write, notification, progress and status reports since the last call
// (vt.rs's `Effects`); the text stays valid until the next feed
void appine_vt_take_effects(AppineVt *vt, AppineVtEffects *out) {
  out->bells = vt->bells;
  out->clipboard = vt->clipboard;
  out->clip = vt->clip.ptr;
  out->clip_len = vt->clipboard == APPINE_CLIPBOARD_TEXT ? vt->clip.len : 0;
  out->notified = vt->notified;
  out->title = vt->note_title.ptr;
  out->title_len = vt->notified ? vt->note_title.len : 0;
  out->body = vt->note_body.ptr;
  out->body_len = vt->notified ? vt->note_body.len : 0;
  out->progress_state = vt->progress_state;
  out->progress = vt->progress;
  out->statuses = vt->statuses.ptr;
  out->statuses_len = vt->statuses.len;
  vt->bells = 0;
  vt->clipboard = APPINE_CLIPBOARD_NONE;
  vt->notified = false;
  vt->progress_state = -1;
  vt->statuses.len = 0;
}

// The title set by the program (OSC 0/2), empty when none; valid until the next text call
const uint8_t *appine_vt_title(AppineVt *vt, size_t *len) {
  *len = 0;
  GhosttyString title = {0};
  if (ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_TITLE, &title) != GHOSTTY_SUCCESS ||
      title.len == 0 || !buf_reserve(&vt->text, title.len)) {
    return NULL;
  }
  memcpy(vt->text.ptr, title.ptr, title.len);
  vt->text.len = title.len;
  *len = title.len;
  return vt->text.ptr;
}

static bool is_alternate(AppineVt *vt) {
  GhosttyTerminalScreen screen = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
  ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen);
  return screen == GHOSTTY_TERMINAL_SCREEN_ALTERNATE;
}

// Display width of UTF-8 text in cells (per codepoint; malformed bytes count as one cell each)
static size_t utf8_width(const uint8_t *s, size_t len) {
  size_t width = 0;
  for (size_t i = 0; i < len;) {
    uint32_t cp = s[i];
    size_t n = cp < 0x80 ? 1 : (cp >> 5) == 0x6 ? 2 : (cp >> 4) == 0xe ? 3 : (cp >> 3) == 0x1e ? 4 : 0;
    if (n == 0 || i + n > len) {
      width++;
      i++;
      continue;
    }
    if (n > 1) cp &= 0x7f >> n;
    for (size_t k = 1; k < n; k++) cp = (cp << 6) | (s[i + k] & 0x3f);
    width += ghostty_unicode_codepoint_width(cp);
    i += n;
  }
  return width;
}

// Shows a yellow line of app notice (a kill report) above the cursor's line: move down and back as
// many lines as it takes (scrolling at the bottom), save the cursor, insert the lines, write,
// restore the cursor and move it down. The prompt and a half-typed command move down with it and
// the shell's line editing is unaffected. False on the alternate screen (a full-screen program),
// where nothing is written.
bool appine_vt_message(AppineVt *vt, const uint8_t *text, size_t len) {
  if (is_alternate(vt)) return false;
  // The program may have died inside a sequence (an OSC, DCS or APC string): CAN returns the
  // parser to ground first, so the line feeds that make the report's room are not ignored or
  // taken as the string's payload
  bool ground = true;
  if (ghostty_terminal_get(vt->term, GHOSTTY_TERMINAL_DATA_VT_GROUND, &ground) == GHOSTTY_SUCCESS &&
      !ground) {
    static const uint8_t can = 0x18;
    ghostty_terminal_vt_write(vt->term, &can, 1);
  }
  size_t width = utf8_width(text, len);
  size_t rows = (width + vt->cols - 1) / vt->cols;
  if (rows > (size_t)vt->height - 1) rows = (size_t)vt->height - 1;
  if (rows < 1) rows = 1;
  char head[64], tail[32];
  int head_len = snprintf(head, sizeof(head), "\x1b[%zuA\x1b" "7\r\x1b[%zuL\x1b[33m", rows, rows);
  int tail_len = snprintf(tail, sizeof(tail), "\x1b[0m\x1b" "8\x1b[%zuB", rows);
  Buf seq = {0};
  bool ok = head_len > 0 && tail_len > 0 &&
            buf_reserve(&seq, rows + (size_t)head_len + len + (size_t)tail_len);
  if (ok) {
    memset(seq.ptr, '\n', rows);
    seq.len = rows;
    memcpy(seq.ptr + seq.len, head, (size_t)head_len);
    seq.len += (size_t)head_len;
    memcpy(seq.ptr + seq.len, text, len);
    seq.len += len;
    memcpy(seq.ptr + seq.len, tail, (size_t)tail_len);
    seq.len += (size_t)tail_len;
    size_t replies = vt->replies.len;
    ghostty_terminal_vt_write(vt->term, seq.ptr, seq.len);
    // Nothing above asks for an answer; keep the console's replies as they were
    vt->replies.len = replies;
  }
  free(seq.ptr);
  return ok;
}

// The primary screen's text with its scrollback, soft-wrapped lines joined and trailing blanks
// trimmed (saving tabs); valid until the next text call. NULL on the alternate screen, whose text
// is not what the tab should come back with, and on failure.
const uint8_t *appine_vt_transcript(AppineVt *vt, size_t *len) {
  *len = 0;
  if (is_alternate(vt)) return NULL;
  GhosttyFormatterTerminalOptions options = GHOSTTY_INIT_SIZED(GhosttyFormatterTerminalOptions);
  options.emit = GHOSTTY_FORMATTER_FORMAT_PLAIN;
  options.unwrap = true;
  options.trim = true;
  options.extra = GHOSTTY_INIT_SIZED(GhosttyFormatterTerminalExtra);
  options.extra.screen = GHOSTTY_INIT_SIZED(GhosttyFormatterScreenExtra);
  GhosttyFormatter formatter = NULL;
  if (ghostty_formatter_terminal_new(NULL, &formatter, vt->term, options) != GHOSTTY_SUCCESS) {
    return NULL;
  }
  uint8_t *data = NULL;
  size_t data_len = 0;
  GhosttyResult r = ghostty_formatter_format_alloc(formatter, NULL, &data, &data_len);
  ghostty_formatter_free(formatter);
  if (r != GHOSTTY_SUCCESS) return NULL;
  if (data_len == 0) {
    // An empty screen: an empty transcript, not a failure
    ghostty_free(NULL, data, 0);
    static const uint8_t empty = 0;
    return &empty;
  }
  return keep_text(vt, data, data_len, len);
}
