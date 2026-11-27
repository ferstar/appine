# android/vtrender Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/vtrender.md` for the paired decision history (not yet created; pre-split decisions remain D1–D3 and the rendering part of D5 in `.agents/decisions/android/vtview.md`, and D5, D9, D16, D17, D32 and the frame/rendering parts of D7, D30, D36, D41 and D46–D48 in `.agents/decisions/android/terminal.md`).
>
> Verified: 2026-11-14 (split out of android/vtview, inherited from the android/terminal full mapping of 2026-11-11; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-27 (targeted verification only since the full mapping; latest change: per-row RenderNode hardware rendering in VtRenderer, immutable-per-generation VtImages bitmaps, and immediate row invalidation)

## Responsibilities

`android/vtrender` defines and paints a terminal frame: the native frame format and image pixels,
`VtFrame`'s decoder, `VtScreen`'s mirror, `VtRenderer`'s cell metrics and drawing, `VtImages`' picture
cache and `VtGlyphs`' shapes, with the bundled JetBrains Mono font.

Root: `VtFrame.kt`, `VtScreen.kt`, `VtRenderer.kt`, `VtImages.kt` and `VtGlyphs.kt` in
`android/app/src/main/java/io/github/mthli/appine/`, `android/app/src/main/assets/fonts/`, and the
frame-format specification and Frames section of `hosts/android/src/vt.c` (including frame-driven
search progress, viewport match encoding and Kitty image extraction).

Outside the module: `VtView` frame orchestration, redraw and cursor/password timers, input and
search requests (`android/vtview`); touch/mouse gestures, selection handles/overlays and pixel
scrolling (`android/vtgestures`); terminal creation, state ownership, PNG decoding,
synchronized-output hold lifecycle, cursor defaults, resize, the JNI functions and locking
(`android/vtcore`). The flow below describes the view's duties as a caller as well as rendering;
it does not transfer those lifecycle methods here.

## Key types

- `VtFrame` — a frame read in place (an inline class over the direct buffer): the header's
  fields, each image placement, and copying the rows into a `VtScreen`.
- `VtScreen` — the screen as of the last frame: a mirror of the cells (codepoint, colours,
  attributes with search marks, grapheme clusters, each row's selected columns and `rowHasMarks`
  flag), one row taller than the grid (the overscan row), the default colours and the cursor.
- `VtRenderer` — four paints (regular/bold × upright/italic) and the cell metrics from them, an
  unboxed open-addressed `GlyphWidthCache` (keyed by paint index and codepoint, cleared on font
  change, bounded at 16,384 slots), per-row hardware `RenderNode` display lists (one per viewport
  row plus the overscan row, tracked by `RowDisplayState`, with direct software `Canvas` fallback)
  and the drawing: lazy per-row recording and replay, the cursor (or the lock) and the composing
  text into the view's canvas; the cell attribute, underline, cursor and search colour constants.
- `VtImages` — the last frame's image placements (`Placement`, pooled) and the bitmaps they show
  by id and generation (`Picture`, allocated fresh per generation without manual recycling of drawn
  bitmaps), copied, drawn per layer into a row and dropped.
- `VtGlyphs` — box drawing (U+2500–U+257F), block elements (U+2580–U+259F) and the Powerline
  triangles (U+E0B0–U+E0B3) drawn as shapes.

## Public entry points

- Native: `appine_vt_render_new`, `appine_vt_render_free`, `appine_vt_frame_begin`,
  `appine_vt_frame_end` and `appine_vt_image`, reached through android/vtcore's `VtNative.frame`
  (holding `Tab.render` across both phases and `Tab.term` only during `begin`) and
  `VtNative.image` (holding `Tab.term`) with its JNI buffer/bitmap handling.
- `VtFrame`: header accessors, `placement` and `takeRows`; the format is shared with vt.c.
- `VtScreen`: `resize`, `span`, cell arrays, default colours and cursor state.
- `VtRenderer`: `setFont`, cell metrics, `resize`, `hasGrid`, `takeRows`, `invalidateRows`,
  `discardDisplayLists`, `drawGrid`, `drawCursor`, `advance` and `drawPreedit`; `VtImages`:
  `take`, `draw`, `drop`; `VtGlyphs`: `covers`, `draw`.

## Data flow / lifecycle

### Frames and drawing

- `onOutput` posts an invalidate on animation to the attached view; `onDraw` takes a frame
  (`VtNative.frame`, main thread only) in two phases and applies it: `frame` clears `dirty`, locks
  `Tab.render`, then locks `Tab.term` only for `appine_vt_frame_begin` (shows animation frames due
  unless held, runs `ghostty_render_state_begin_update` or hands off `vt->held_state`, reads image
  placements when their generation changed, catches a running search up for at most 4 ms and marks
  its viewport matches, and snapshots terminal-owned header metadata into `FrameSnap`). After
  releasing `Tab.term`, `appine_vt_frame_end` runs under `Tab.render` alone: calls
  `ghostty_render_state_end_update` to finish deferred style denormalization, writes the 64-byte
  header, image placements (56 bytes each) and dirty/mark-changed rows (all rows when full or
  placements changed while some were or are shown) in the format documented in `vt.c` (12-byte row
  headers, 16-byte cells, a grapheme's extra codepoints after its row), and cleans the render
  state; the direct `ByteBuffer` is valid until the next `frame`. `appine_vt_frame_end` uses
  `ghostty_render_state_row_iterator_next_dirty` on incremental frames whose search marks did not
  change (safe because `overscan.above == 0`), falling back to `row_iterator_next` when `full` or
  search marks changed; `marks_nonempty` and `marks_sent_nonempty` skip full-grid mark `memset`,
  `memcmp` and `memcpy` when no search highlights are active or clearing. `put_row` batches row
  reads via `ghostty_render_state_row_get_multi` (`CELLS`, then optional `SELECTION`) and per-cell
  reads via `ghostty_render_state_row_cells_get_multi` (`RAW`, `HAS_STYLING`, `GRAPHEMES_BUF` into
  a 16-codepoint stack buffer falling back to `render->grapheme` on `GHOSTTY_OUT_OF_SPACE`, and
  optional `BG_COLOR` last; plus `STYLE` and optional `FG_COLOR` last when styled). A search not
  yet caught up sets a header flag, and the view draws again on the next animation frame to go on.
- Overscan: while the viewport is scrolled back the render state captures the row after it too,
  the header says so (byte 58) and that row comes as row `rows` (vt.c numbers rows by the
  iterator, which is right while nothing above the viewport is asked for), with the same cells,
  dirty flag and selection columns as the others but no search marks (they come once it is in the
  viewport). The view then draws the grid up to a row higher (`scrollPx`, `android/vtgestures`'s
  Scrolling and flings), so as much of that row shows at the bottom as is hidden of the first.
- Images (the Kitty graphics protocol): libghostty-vt stores what programs send (PNG decoded at
  once with Wuffs, which libghostty-vt.a links, into straight-alpha RGBA; larger than the storage
  refused) and its render state lists what to draw: per placement the image, a source rectangle,
  a destination in pixels from the grid's corner (which may start above or left of it), a layer
  (below the backgrounds, below the text, above it) and a z-index. `frame` sorts them as Ghostty
  does (layer, z-index, image), then by placement and position, and joins the rows of a virtual
  placement (one per row of placeholder cells) back into one rectangle where they meet, since
  each row's source comes in whole image pixels and rows scaled apart leave seams. Every frame
  carries the whole list, and the header the milliseconds to the next animation frame (at least
  8, 0 when none plays), for which the view draws again.
- The view keeps a bitmap per image shown (`VtImages`), by id with the generation of its pixels:
  a placement of another generation allocates a fresh `ARGB_8888` bitmap and copies into it
  (`VtNative.image`, through libjnigraphics' bitmap locking, premultiplied; gray and RGB images
  widened) so hardware `RenderNode` display lists on `RenderThread` never race with in-place writes;
  a copy fails when the terminal has moved on to another generation (immediately recycling the
  unrecorded bitmap and removing the stale entry), and the next frame names that. Bitmaps no
  placement shows are removed from the map without manual `recycle()` so GC reclaims them after
  `RenderThread` finishes; attaching another tab or hiding the window drops them all and calls
  `VtRenderer.discardDisplayLists()` so row nodes release bitmap references, after which the next
  visible frame copies them again from libghostty-vt and re-records rows from `VtScreen`. Newly
  arrived pixels call `VtRenderer.invalidateRows()` so painted rows re-record on the current frame.
- A hold (mode 2026) captures the screen into `vt->held_state` (under `Tab.term` only, never
  racing with `AppineVtRender`) when it begins; it ends with the program, a resize or a full
  reset, or by force once a frame is taken more than 1 s after it began. On the first frame while
  held, `vt->held_state` is moved into `AppineVtRender.state`; if a hold began and ended between
  frames, `AppineVtRender.state` is rebuilt fresh before `begin_update` so rows whose dirty flags
  `held_state` consumed are never missed. During a hold the rows, cursor and colours come from the
  capture, and so does the overscan byte, while the header's modes, selection and scroll position
  are read live.
- The view asks for a full frame after a regrid, a change of the wide-cell setting, attaching
  another tab or the attached tab's shell starting; default colour changes (OSC 10 or 11, reverse
  video) and newly arrived image pixels invalidate cached row display lists in place via
  `VtRenderer.invalidateRows()`. A frame captured at an old size is dropped and a full one
  requested. Attaching also resizes the tab's terminal and console to the view's grid when it
  differs.
- The header also carries mode flags (mouse tracking, alternate screen, alternate scroll 1007,
  focus events 1004, a selection exists, it is a rectangle, the search is still catching up), plus
  output pending (flags bit 7: resize output has not settled at parser ground, or a synchronized-output hold), the
  selection's first and last cells in the viewport (a rectangle's top left and bottom right
  corners), the scroll position libghostty-vt reports for the scroll bar (the scrollable area in
  rows, the viewport's first row in it and the rows it shows), the search's count (matches so
  far, the selected one's place from the newest) and the mouse pointer's shape the program asked
  for (OSC 22, byte 59); each row carries its selected columns (a
  rectangle's columns on each of its rows) and each cell its search mark (attributes bits 14 and
  15). `VtView` reads the dirty byte only while searching to decide whether to refresh match rows,
  the focus-events flag not at all, and reports the count to the activity when it changes.
- Cell metrics: the width is the font's advance rounded to whole pixels and the text size is
  adjusted so the advance equals it; the height is the ceiling of ascent+descent, with the baseline
  centred; the light line width is the font's underline thickness. The wide scale is two cells over
  the advance of "水" (the system's CJK fallback, one em, so 1.2 with JetBrains Mono; a font's own
  ideograph when it has one), at least 1, or 1 with the setting off.
- `onDraw` fills the view with the opaque default background, applies the frame, draws the grid
  clipped to the grid's whole cells (the margin below keeps the background) and shifted up by
  `scrollPx`, then the cursor and the composing text translated by it, then the handles (whose
  places subtract it). Applying a frame copies incoming rows into `VtScreen` and marks them dirty in
  `RowDisplayState`. On hardware-accelerated canvases, `drawGrid` translates by `-scrollPx` and
  replays each visible row's `RenderNode` (skipping the overscan row when `scrollPx == 0`), lazily
  recording any dirty or discarded row into its node (`setPosition(0, top, bw, top + cellHeight)`,
  `clipToBounds = true`, `translate(0, -top)`); on software canvases, `drawGrid` draws each valid
  visible row directly under a row clip. Each row paints over the canvas's opaque default
  background: the images below the backgrounds (showing through cells of the default background),
  runs of other backgrounds (a search match's colour over a cell's own), the images below the text,
  the selection's translucent highlight, text (black in a search match), then the images above the
  text; images are scaled from their source to their destination with bilinear filtering, as
  Ghostty samples them, and only the part in the row is drawn. Plain ASCII runs of
  one paint, colour and decoration go out in one draw call (skipping blank undecorated cells
  outside active runs and trimming trailing spaces when undecorated); other glyphs are centred in
  their one or two cells, single codepoints encoded into a reusable `CharArray(2)` with advances
  cached in `GlyphWidthCache`, a wide cell's glyph enlarged evenly by the wide scale about the
  cell's vertical middle, and any glyph still wider than its cells shrunk evenly to fit (never
  stretched), with consecutive undecorated same-scale glyphs in a row sharing one canvas scale
  transform pivoted at `(0, midY)` (`x = l / s`); `markOf` skips wide-tail lookup when
  `screen.rowHasMarks[y]` is false; single codepoints `VtGlyphs` covers are shapes across their one
  or two cells; Kitty placeholder cells
  (U+10EEEE, with or without their diacritics) have no glyph drawn, as in Ghostty; four paints
  (regular/bold × upright/italic) from the variable font with ligatures off. Underlines (single,
  double, curly, dotted, dashed), strikethrough and overline are drawn; inverse and faint apply;
  SGR 8 hides both glyph and decorations. Cell, search and selection backgrounds still draw.
  Text blink is ignored.
- The cursor (bar and underline two light lines thick, block with the glyph redrawn in the
  background colour, underline, hollow; a block is drawn hollow while the view lacks focus; all
  but the bar cover two cells on a wide cell; in the OSC 12 colour, else the default foreground;
  a lock while the console reads a password, below), the IME's composing text (shifted
  left at the right edge) and the theme's text selection handles are drawn over the grid each
  draw. During android/vtview's bounded height-shrink preview, that view can reject a frame whose
  output-pending bit is set and draw its own capture of the prior grid, cursor and preedit instead.
  It requests all rows again until it accepts a frame; the renderer's grid and row `RenderNode`s
  still resize at once, allocating fresh nodes without discarding the previous ones in place so a
  hardware preview `RenderNode` referencing them stays valid until cleared.
- While the console reads a password the frame gives the cursor as a lock (style 4), as Ghostty
  shows one at password prompts: wherever the cursor is in the viewport, even when the program
  hid it, never blinking, with or without the focus; `VtRenderer` draws a padlock about as tall as a
  capital letter on the baseline (Ghostty draws its Nerd Font's U+F023, which JetBrains Mono
  lacks), and a block instead while the IME composes. The pump notes the state at every read;
  while the lock shows in a visible window, `VtView` asks the kernel again every 200 ms (Ghostty
  polls termios as often, all the time), since a program can turn echo back on without printing
  anything (sudo writes its line break before restoring the terminal), and stops once it ends.
  While the lock shows, the IME is told a password is typed (android/vtview's Input), as Ghostty
  turns macOS's secure input on at password prompts.
- The cursor blinks (shown and hidden every 500 ms, as Android's text fields) while the frame
  header says it blinks (the setting, on by default; a program changes it with DECSCUSR or mode
  12) and the view has the focus in a focused window; otherwise no timer runs. Input and a cursor
  move show it again for a whole period.
- The cursor setting (Settings' shape, block, bar or underline, and blinking; android/service
  gives it to every tab, restored ones not shown yet included, as `VtSession.setCursor`) is
  libghostty-vt's default cursor: a change shows at once unless a program chose its own cursor
  (DECSCUSR with a nonzero style), and is kept for DECSCUSR 0, DECSTR, a program's RIS and the
  app's Reset terminal, which bring it back; while the cursor follows its defaults, a change also
  overrides a program's mode 12. No full frame is needed: every frame's header carries the shape
  and blinking. A restored tab shows the current setting whatever its snapshot recorded (`settle`'s
  DECSTR, then the session's setting). The lock ignores it while it shows. Changing it does not
  make the last snapshot stale.

## Dependencies (inbound / outbound)

### Inbound

- android/vtview's `VtView`: asks for frames, applies the header to `VtScreen` and interaction
  state, copies images, repaints rows and calls the renderer; owns font/grid settings, full-frame
  requests, clipping/scroll offset, animation and cursor timers, and dropping view images on tab
  changes or hiding the window. Handles and scroll-bar search marks remain view overlays.
- android/vtcore's JNI frame and image functions: enter the native Frames section under the tab lock.

### Outbound

- android/vtcore: terminal and render-state ownership, `VtNative.frame`/`image`, the frame buffer's
  lifetime, cursor-style constants, PNG decoding and image storage/trimming; libjnigraphics copies
  pixels into Android bitmaps through that module's JNI.
- android/vtview and android/vtgestures: query, selection and viewport changes are reflected in
  frames. Frame capture advances search (`android/vtview`) and calls `android/vtgestures`'s
  Selection section helpers (`viewport_ref`, `screen_point`) to encode highlights; `android/vtview`
  consumes count/pending fields and schedules further frames.
- libghostty-vt: render state, overscan, rows/cells, Kitty placements and animation, viewport search
  matches; Android Canvas/Bitmap and fonts. JetBrains Mono 2.304 (variable, upright and italic) is
  bundled, with system fallback for other glyphs (Noto Sans CJK on AOSP images).

## Invariants and gotchas

- libghostty-vt 1.0.0 stores the concrete cursor shape per screen. DECSCUSR on the alternate
  screen carries its shape back through modes 47 and 1047, which copy the cursor in both
  directions. Mode 1049 copies on entry and restores a saved cursor without a shape on exit,
  leaving the primary shape intact. Blink mode and the flag that selects default or explicit
  appearance are terminal-wide. Desktop Ghostty uses the same code. xterm uses one shape across
  both buffers, including mode 1049.
- Settings always updates the stored defaults. While following defaults, only the active shape
  changes immediately, along with the global blink mode. With an explicit DECSCUSR appearance,
  both new defaults wait for DECSCUSR 0, DECSTR or RIS.
- The frame format is defined in `vt.c` and read by `VtFrame`; the two change together.
- The render state's source rectangles are whole image pixels, so a virtual placement's rows,
  drawn one by one, overlap or gap by a pixel each and show seams (Ghostty's renderer draws them
  so); vt.c joins rows that meet. Rows only partly in the viewport keep the rounding at their top.
- The view records images into per-row `RenderNode`s, so a changed placement list must repaint every
  row: vt.c makes such a frame a full one, and late or restored image pixels call
  `VtRenderer.invalidateRows()` to re-record painted rows on the current frame. An image read for a
  frame during a synchronized-output hold may be gone or replaced by then; its copy fails (by
  generation) and the frame after the hold names the new one. Never call `recycle()` on a `VtImages`
  bitmap after it may have been recorded into a `RenderNode`, and never overwrite a drawn bitmap in
  place via JNI.
- libghostty-vt's render state keeps search highlights internally, but the C API does not expose
  them: vt.c marks the matches itself, from the viewport's matches in screen coordinates (viewport
  ones fail for a match that starts above the viewport), and resends a row whose marks differ from
  the last frame's, since the render state does not count them as a change.
- Capture and consume frames on the main thread. android/vtcore holds `Tab.render` across both
  phases and `Tab.term` only during `appine_vt_frame_begin`; the direct buffer remains valid until
  the next frame call, so it is read after unlocking.
- `VtView` remains the owner of full-frame requests and repaint scheduling. Changing the frame
  format or its view consumers requires checking this map, `android/vtview` and
  `android/vtgestures`; search queries, selection and scrolling use the same terminal state
  through `android/vtcore`'s lock.
- Each screen of each tab keeps up to 32 MiB of decoded Kitty images in libghostty-vt, and while a
  tab is shown the view holds a second copy of those placed on its screen (dropped on tab switch or
  window hide). `android/service` trims non-current tabs down to 4 MiB a screen via
  `VtNative.dropImages` (after 60 s with the UI hidden, after 60 s unseen or beyond 2 recently left
  background tabs while switching, on periodic saves for already-trimmed unseen tabs, and under
  system memory pressure); a tab restored from a snapshot saved before trimming gets its pre-trim
  images back (libghostty-vt's snapshot encoder writes them when built with Kitty graphics, though
  a comment in its `snapshot/terminal.zig` says otherwise), while one restored from its plain
  transcript has text only.

## Confirmed bugs / technical debt

- Changing cursor Settings on the alternate screen can leave the primary screen showing its
  previous default shape after mode 1049 exits. The C API exposes neither an inactive-screen
  shape setter nor per-screen default ownership. Reapplying defaults on exit could overwrite
  a primary shape chosen by a program.

- **libghostty-vt's drawing features left unused**: the default palette option (the colour scheme
  reply is fixed to dark); overscan above the viewport; the colour utilities (`palette_generate`,
  contrast and luminance), of use only with themes.

## Open questions

- Should a Nerd Font or a CJK fallback font be bundled?

## To verify

- Tried only on a Pixel 4a API 34 emulator (4 KB pages, Gboard in English, input from adb,
  `sendevent` and a `uinput` virtual mouse) and through vt.c's harnesses on a macOS build, where
  the search and frame times come from; not on real or 16 KB-page devices or Android 11–13.
  Unverified beyond that:
  - drawing: per-row `RenderNode` hardware rendering, hardware height-shrink preview and
    `TerminalResizeTest.hardwareRecordingPopulatesAndRecoversAfterDiscard` on an Android device or
    emulator; Kitty graphics with yazi, timg, `kitten icat`, tmux passthrough and the 32 MiB storage
    filled; the cursor setting in Chinese or while a full-screen program
    shows; the lock cursor with other IMEs composing at it; the search's marks while rows leave a
    full scrollback.
