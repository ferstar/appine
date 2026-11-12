# android/vtview Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/android/vtview.md` for the paired decision history (decisions from before the split are D5, D7, D9, D16, D17, D25, D30–D32, D34, D36, D37, D40, D41, D43 and D46–D48 in `.claude/decisions/android/terminal.md`).
>
> Verified: 2026-11-11 (split out of the android/terminal full mapping of the same day: 5 research concerns, 18 claims checked by 3 independent verifiers; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-12 (targeted verification only since the split; latest change: VtView's frame reading, screen mirror, drawing and images moved into VtFrame, VtScreen, VtRenderer and VtImages)

## Responsibilities

`android/vtview` is the view that draws a tab's terminal and takes its input: `VtView` (frames,
the cursor's blinking and the bell, keys, the IME and pastes, selection by touch and by mouse,
links, search, gestures and scrolling by pixels, and the uses of the shell's prompt marks: jumping,
selecting a command's output and moving the cursor by taps), with `VtFrame` (reading a frame),
`VtScreen` (the screen as of the last frame), `VtRenderer` (drawing it with the bundled JetBrains
Mono), `VtImages` (the images programs show), `VtKeys` (key codes) and `VtGlyphs` (characters
drawn as shapes), and vt.c's side of each: the frame format, key, paste and mouse encoding,
selection, links, taps on the prompt and search.

Root: `VtView.kt`, `VtFrame.kt`, `VtScreen.kt`, `VtRenderer.kt`, `VtImages.kt`, `VtGlyphs.kt` and
`VtKeys.kt` in `android/app/src/main/java/io/github/mthli/appine/`, the terminal font in
`android/app/src/main/assets/fonts/`, and the Frames, Input, Paste, Mouse and focus, Selection,
Links, Taps on the prompt and Search sections of `hosts/android/src/vt.c`.

Outside the module: the sessions, the terminal handle and its lock, the JNI functions (`VtNative`,
`vt.rs`) and the rest of vt.c (`android/vt`); the screen around the view, the search bar and
opening links (`android/terminal`).

## Key types

- `VtView` — the view: the attached `VtSession`; its `VtScreen`, `VtImages` and `VtRenderer`
  (whose cell metrics it reads); the font size and the wide-cell setting; the last frame's modes
  (`altScreen`), scroll position (and whether the viewport is at the bottom) and selection bounds
  and shape; `scrollPx` (pixels, less than a row, the grid is drawn above its place while the
  viewport scrolls) with the drag's sub-pixel remainder; the search's state and count and the rows
  of its matches along the scroll bar; the animation's redraw; the IME connection; the mouse
  pointer (the program's shape, the cell hovered, a link, Ctrl or Meta held); gestures and their
  drag states (taps in a row; `pointerButton`, a mouse's held button; its last left press;
  rectangle drags; `pointerSelection`, which hides the handles); the `OverScroller` of flings and
  of settling on a whole row (`settling`); handles and the floating menu; the cursor's blink
  timer; the password check (`passwordCheck`, `imePassword`); the bell's last vibration. `Host`
  (the activity): `onGrid`, `onInput`, `onZoom`, the extra keys' `ctrl`, `alt`, `shift` and
  `modifiersUsed`, `onLink` and `onSearchCount`.
- `VtFrame` — a frame read in place (an inline class over the direct buffer): the header's
  fields, each image placement, and copying the rows into a `VtScreen`.
- `VtScreen` — the screen as of the last frame: a mirror of the cells (codepoint, colours,
  attributes with search marks, grapheme clusters, each row's selected columns), one row taller
  than the grid (the overscan row), the default colours and the cursor.
- `VtRenderer` — four paints (regular/bold × upright/italic) and the cell metrics from them, the
  backing bitmap (one row taller than the grid, like the mirror) and the drawing: rows into the
  bitmap, the bitmap, the cursor (or the lock) and the composing text into the view's canvas; the
  cell attribute, underline, cursor and search colour constants.
- `VtImages` — the last frame's image placements (`Placement`, pooled) and the bitmaps they show
  by id and generation (`Picture`), copied, drawn per layer into a row and dropped.
- `VtGlyphs` — box drawing (U+2500–U+257F), block elements (U+2580–U+259F) and the Powerline
  triangles (U+E0B0–U+E0B3) drawn as shapes.
- `VtKeys` — Android key codes to libghostty-vt's `GhosttyKey` numbers (`of`, 0 when unknown),
  modifier bits (Shift, Ctrl, Alt, Meta as Super; no lock bits), actions, `isModifier`.

## Public entry points

- `VtView(context, host)`, built by `MainActivity`: `attach` (show a tab, or none), `setFontSize`,
  `setFillWide`, `press` and `type` (the extra keys), `search`, `searchStep` and `endSearch`,
  `jumpToPrompt`, `bell`, `altScreen`; the framework's drawing, scrolling, touch, hover, pointer,
  key, focus and IME callbacks. `Host` is the activity's side.
- From the session (android/vt): `onStarted` (the shell started: take the whole screen) and an
  invalidate on animation after output.

## Data flow / lifecycle

### Frames and drawing

- `onOutput` posts an invalidate on animation to the attached view; `onDraw` takes a frame
  (`VtNative.frame`, main thread only) and applies it: `frame` clears `dirty`, shows the animation
  frames due (unless a synchronized-output hold is active), updates the render state (likewise),
  reads the image placements again when their generation changed, catches a running search up
  with the terminal for at most 4 ms and marks its matches in the viewport, and writes a 64-byte
  header, the image placements (56 bytes each) and the dirty rows and the rows whose search marks
  changed since the last frame (all rows when the view asks for a full frame, or when the
  placements changed while some were or are on the screen) in the format documented in `vt.c`
  (12-byte row headers, 16-byte cells, a grapheme's extra codepoints after its row); the direct
  `ByteBuffer` is valid until the next `frame`. A search not yet caught up sets a header flag, and
  the view draws again on the next animation frame to go on.
- Overscan: while the viewport is scrolled back the render state captures the row after it too,
  the header says so (byte 58) and that row comes as row `rows` (vt.c numbers rows by the
  iterator, which is right while nothing above the viewport is asked for), with the same cells,
  dirty flag and selection columns as the others but no search marks (they come once it is in the
  viewport). The view then draws the grid up to a row higher (`scrollPx`, Gestures and selection),
  so as much of that row shows at the bottom as is hidden of the first.
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
  a placement of another generation has them copied (`VtNative.image`, through libjnigraphics' bitmap
  locking, premultiplied as `ARGB_8888` holds them; gray and RGB images widened), reusing the
  bitmap when the size is the same; a copy fails when the terminal has moved on to another
  generation, and the next frame names that. Bitmaps no placement shows are recycled; attaching
  another tab drops them all, and so does the window being hidden (the activity stopped), after
  which the next frame copies them again from libghostty-vt while the backing bitmap still holds
  the rows painted with them. Pixels that arrive in a frame that does not repaint every row ask
  for a full frame.
- A hold (mode 2026) captures the screen when it begins; it ends with the program, a resize or a
  full reset, or by force once a frame is taken more than 1 s after it began. During a hold the
  rows, cursor and colours come from the capture, and so does the overscan byte, while the
  header's modes, selection and scroll position are read live.
- The view asks for a full frame after a regrid, a new bitmap, new default colours (OSC 10 or 11,
  reverse video), a change of the wide-cell setting, attaching another tab or the attached tab's
  shell starting; a frame captured at an old size is dropped and a full one requested. Attaching
  also resizes the tab's terminal and console to the view's grid when it differs.
- The header also carries mode flags (mouse tracking, alternate screen, alternate scroll 1007,
  focus events 1004, a selection exists, it is a rectangle, the search is still catching up), the
  selection's first and last cells in the viewport (a rectangle's top left and bottom right
  corners), the scroll position libghostty-vt reports for the scroll bar (the scrollable area in
  rows, the viewport's first row in it and the rows it shows), the search's count (matches so
  far, the selected one's place from the newest) and the mouse pointer's shape the program asked
  for (OSC 22, byte 59); each row carries its selected columns (a
  rectangle's columns on each of its rows) and each cell its search mark (attributes bits 14 and
  15). `VtView` reads the dirty byte only to tell whether late image pixels need a full frame and,
  while searching, whether to take the matches' rows again, the focus-events flag not at all, and
  reports the count to the activity when it changes.
- Cell metrics: the width is the font's advance rounded to whole pixels and the text size is
  adjusted so the advance equals it; the height is the ceiling of ascent+descent, with the baseline
  centred; the light line width is the font's underline thickness. The wide scale is two cells over
  the advance of "水" (the system's CJK fallback, one em, so 1.2 with JetBrains Mono; a font's own
  ideograph when it has one), at least 1, or 1 with the setting off.
- `onDraw` fills the view with the default background, applies the frame, draws the bitmap
  clipped to the grid's whole cells (the margin below keeps the background) and shifted up by
  `scrollPx`, then the cursor and the composing text translated by it, then the handles (whose
  places subtract it). Applying a frame repaints each row it brings into the bitmap under a
  clip: default background,
  the images below the backgrounds (showing through cells of the default background), runs of
  other backgrounds (a search match's colour over a cell's own), the images below the text, the
  selection's translucent highlight, text (black in a search match), then the images above the
  text; images are scaled from their source to their destination with bilinear filtering, as
  Ghostty samples them, and only the part in the row is drawn. Plain ASCII runs of
  one paint, colour and decoration go out in one draw call; other glyphs are centred in their one
  or two cells, a wide cell's glyph enlarged evenly by the wide scale about the cell's vertical
  middle, and any glyph still wider than its cells shrunk evenly to fit (never stretched); single
  codepoints `VtGlyphs` covers are shapes across their one or two cells; Kitty placeholder cells
  (U+10EEEE, with or without their diacritics) have no glyph drawn, as in Ghostty; four paints
  (regular/bold × upright/italic) from the variable font with ligatures off. Underlines (single,
  double, curly, dotted, dashed), strikethrough and overline are drawn; inverse and faint apply;
  invisible hides the glyph but not its decorations; blink is ignored.
- The cursor (bar and underline two light lines thick, block with the glyph redrawn in the
  background colour, underline, hollow; a block is drawn hollow while the view lacks focus; all
  but the bar cover two cells on a wide cell; in the OSC 12 colour, else the default foreground;
  a lock while the console reads a password, below), the IME's composing text (shifted
  left at the right edge) and the theme's text selection handles are drawn over the bitmap each
  draw.
- While the console reads a password the frame gives the cursor as a lock (style 4), as Ghostty
  shows one at password prompts: wherever the cursor is in the viewport, even when the program
  hid it, never blinking, with or without the focus; `VtRenderer` draws a padlock about as tall as a
  capital letter on the baseline (Ghostty draws its Nerd Font's U+F023, which JetBrains Mono
  lacks), and a block instead while the IME composes. The pump notes the state at every read;
  while the lock shows in a visible window, `VtView` asks the kernel again every 200 ms (Ghostty
  polls termios as often, all the time), since a program can turn echo back on without printing
  anything (sudo writes its line break before restoring the terminal), and stops once it ends.
  While the lock shows, the IME is told a password is typed (Input), as Ghostty turns macOS's
  secure input on at password prompts.
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
- The bell of the tab shown vibrates once through `performHapticFeedback(REJECT)` (a double
  click, subject to the system's touch feedback setting), at most once per 100 ms.

### Input

- Keys: mapped keys go through `VtNative.key`, which encodes them in the terminal's current modes,
  copied into the encoder before each key (`set_from_terminal`: cursor and keypad modes, the
  keypad with NumLock (1035) and Alt's ESC prefix (1036), both on by default, the backarrow key
  (67), modifyOtherKeys and the Kitty keyboard protocol, including releases from `onKeyUp` and
  lone modifiers, which are sent only when the encoder emits something). An unmapped key that
  types a character goes as text without Ctrl or Alt, and to the encoder as an unidentified key
  with that text with them; one that types nothing goes to the system. System keys (Back, Home,
  volume, media) go to the system, except the menu key, a hardware keyboard's context menu key,
  which goes
  to the encoder (`CSI 29 ~` in the legacy encoding). A key the encoder cannot encode is
  swallowed and leaves the modifiers on.
- CTRL, ALT and SHIFT on the extra keys (SHIFT only with the first page shown) apply to the next
  key or one typed character (turned back into a key event through the virtual keyboard's key
  map), or to every one while locked: they are added to the key event's meta state, so with SHIFT
  a key types its shifted character in that map (`a` → `A`, `-` → `_`, `/` → `?`) and Tab is sent
  as back-tab. A character the key map lacks (CJK) or a commit of several characters goes as text
  and leaves them on; so does any character typed with no modifier on. The extra keys send presses
  only (a held key repeats as new presses), never releases, and a key's release never carries
  their modifiers.
- Ctrl+Shift+C and V (held, or from the extra keys, also with a character the IME types) and the
  dedicated copy and paste keys copy and paste; copying without a selection does nothing, and the
  release still reaches the program.
- Typed and pasted input ends a selection; all input that encodes to bytes (focus events and hover
  motion included) first scrolls the viewport back to the bottom (the grid on a whole row) and,
  but for scrolling's own wheel events and arrow keys, stops a fling or settling, then goes through
  the host to the tab's `vt-writer` thread (`Native.write`).
- IME: committed text is sent at once (`\n` as `\r`, a lone `\n` as Enter, several lines as a
  paste, which covers the IME's clipboard); finishing a composition types it; the editor action is
  Enter; composing text is only drawn at the cursor; the editable is kept empty so Backspace with
  nothing composed arrives as `deleteSurroundingText` and becomes as many DELs (text after the
  cursor is ignored). `requestCursorUpdates` is answered with the cursor and composing text's
  position (`CursorAnchorInfo`, less `scrollPx`), again whenever the cursor or composing text
  moves while the IME monitors it (not when only `scrollPx` changes).
- The editor is text without suggestions (`TYPE_TEXT_FLAG_NO_SUGGESTIONS`), and a password
  (`TYPE_TEXT_VARIATION_PASSWORD` with `IME_FLAG_NO_PERSONALIZED_LEARNING`) while the frames give
  the cursor as a lock: when a frame changes that, the view posts an input restart (outside
  drawing); attaching another tab during one turns it off and restarts the input at once; the IME
  then takes a new connection of the other kind. Gboard then shows its password layout (a number
  row, no toolbar or suggestions) and is
  asked to learn nothing typed; restarting the input ends a composition, which password layouts
  do not make.
- Paste (`VtNative.paste`, through `ghostty_terminal_paste`): bracketed with mode 2004, else line
  breaks become carriage returns; unsafe control bytes become spaces. Text that could run commands
  (line breaks without bracketed paste, or the bracketed terminator) is refused, and the view asks
  in a dialog with a preview (300 characters) before pasting it anyway.
- Focus: window focus changes are sent as focus events when the program asked (mode 1004); showing
  another tab sends none.

### Gestures and selection

- A finger's (or a stylus's) tap ends a selection or its menu; a mouse's or touchpad's press takes
  the pointer paths below. Otherwise, when the program does not track the mouse and the
  tap is on a link, it opens the link instead of the IME; otherwise it opens the IME and, when the
  program tracks the mouse, also sends a left click (press and release at the tap's pixel position,
  in the program's format), or else moves the shell's cursor when it is on the input at the prompt
  (Prompt marks). A tap that stopped a fling does nothing else (one during settling on
  a whole row is no fling stop and acts). Taps act at once:
  `VtView` counts taps in a row itself (each within the system's double-tap timeout and slop of
  the last tap's lift). With mouse tracking every tap is a click, for
  the program's own double and triple clicks. Without it, the press of the second tap selects the
  word under it and that of the third the line, before the gesture detector sees it (`tapPress`),
  and the view keeps that touch: moved on past the touch slop, it drags the selection by whole
  words or lines (Ghostty's double- and triple-click drags, through libghostty-vt's selection
  gesture, see below); lifted, the menu comes up, and a press that did not drag counts as a tap
  (one that dragged ends the row of taps). A fourth tap counts as a first, and so does a tap after
  one that opened a link. The line is Ghostty's triple-click line: the soft-wrapped line with
  blanks at its ends left out, cut where an OSC 133 prompt mark starts or ends a part of it. A
  second or third tap on blanks selects nothing and goes on as a single tap.
- Word and line selections and the drags after them go through libghostty-vt's selection gesture
  (`VtNative.gesturePress`, `gestureDrag`, `gestureEnd`; vt.c keeps one gesture per terminal): the
  press selects the word or line at its cell, which the gesture tracks through output, and a drag
  takes the selection from the pressed cell's word or line to the one under the finger
  (`selectWordBetween` both ways for words), so over blanks and punctuation a word drag stays on the
  last word reached, while a line drag takes in blank rows too (libghostty-vt 1.0's fix; before,
  the selection stayed as it was); any drag once the pressed cell's screen has changed (the
  alternate screen) leaves the selection as it was. The view counts a finger's taps itself, since
  a finger press may become a scroll, so each finger press goes to the gesture untimed (a first
  click in libghostty-vt's count) with the behaviour the view asks for as a first click's; a
  mouse's left presses go to the same gesture timed (below). Lifting a finger ends the gesture
  (its tracking goes, the selection stays); so does clearing the selection, a right-click selecting
  a word, or attaching another tab during a drag, a mouse's included.
- A link is a cell's OSC 8 hyperlink, else an `http://` or `https://` URL written in the text
  (`VtNative.link`): vt.c joins the cell's soft-wrapped line (up to 64 rows above and below), takes
  the run's first scheme at or before the cell and runs on over ASCII URL characters (letters,
  digits and RFC 3986 punctuation but the single quote) while brackets balance, then drops a
  trailing `.,;:!?`; any other character, CJK included, ends it. Only `http`, `https`, `mailto`
  and `file` links open (an OSC 8 link can name any scheme, other apps' deep links among them);
  the view hands them to the activity (`Host.onLink`), which opens them (android/terminal).
- A vertical drag, or a mouse wheel or touchpad scroll (3 rows per scroll event, whatever its
  size), scrolls: wheel events when the program tracks the mouse and arrow keys on the alternate
  screen with alternate scroll (1007, on by default), by whole rows; else the viewport, which a
  drag moves by pixels (`scrollPixels`): within a row by drawing the grid up to a row higher
  (`scrollPx`, with the overscan row after the viewport showing below), across rows by scrolling
  libghostty-vt's viewport, as far as either end (by the last frame's scroll total and length,
  with the rows moved since), where the grid rests on a whole row. A wheel moves it by whole rows
  and keeps the pixels.
- With mouse tracking a mouse's or touchpad's sideways scroll goes to the program as wheel buttons
  6 (left) and 7 (right), one per scroll event, as Ghostty sends them; without it nothing scrolls
  sideways.
- A fling goes on scrolling that way, slowing down as a ScrollView does (Android's fling physics,
  in pixels for the viewport and taken as whole rows otherwise): the viewport until it cannot
  move, wheel events and arrow keys for at most a screen of rows. It stops when the way to scroll
  changes (mouse tracking, the alternate screen, alternate scroll), when the tab has no terminal,
  at a touch, at any other input to the program (not scrolling's own), when the window loses
  focus and when another tab is shown.
- The viewport comes to rest on the nearest whole row (`settle`, 150 ms through the same
  scroller) when a fling stops between rows, when a touch ends there without a fling (a touch
  that interrupts settling settles again when it lifts), and when the window loses focus (unless
  the program asked for focus events, whose report, like any input, returns the viewport to the
  bottom); prompt jumps, search steps, a selection's keyboard adjustment (Shift with the arrows,
  Page Up or Down, Home or End), its autoscroll and input returning to the bottom put the grid on
  a whole row at once, and so do a regrid,
  attaching a tab and a frame without the overscan row or with scrolling going another way.
  Touches are taken to cells with the shift (`cellY`; a touch on the part of the overscan row
  shown counts as the last row), and the handles, the menu's place and a drag's offset subtract
  it.
- The viewport scrolling (a drag, wheel, fling, a selection's autoscroll, input returning it to the
  bottom) shows the theme's scroll bar at the right edge, faded out after a moment as a
  ScrollView's; output alone does not show it, and with no scrollback (the alternate screen) it is
  never drawn.
- A long press (ignored during a pinch, with a long-press haptic) selects the word under it as a
  gesture's press (`GESTURE_WORD`), notes whether the cell is in a command's output (Prompt
  marks), and moving on extends the selection by whole words as a double tap's drag does. On
  blank cells it selects
  nothing and shows a menu with Paste (with a clipboard), Select all and, on a link, Open link. When
  the program tracks the mouse, the long press selects the same way, but what it becomes waits for
  the finger: lifted, it keeps the word (or the point menu); moved on past the touch slop, the
  selection and the output found are dropped and the program gets a left-button drag instead (press
  at the long-pressed point, motion while moving, release where the finger lifts).
- A mouse or touchpad (not a finger) pressed while the program tracks the mouse, Shift not held at
  the press, goes to it as it is: the first button pressed (left, right or middle; a touchpad tap
  is left) with modifiers, motion while it is held and its release, bypassing the gestures (it
  clears a selection and takes focus without opening the IME). Without tracking, or with Shift held
  (always: the C API does not expose a program's XTSHIFTESCAPE, so this is Ghostty's
  `mouse-shift-capture = never`, not its default), it selects as in a desktop terminal (below). A
  touchpad's two-finger swipe or pinch (classified from Android 14 on) takes the finger gestures
  instead; earlier, and for other multi-finger swipes, delivered as mouse touches it would select.
  Hover (no button) is sent as motion while the program tracks the mouse. Which of these the
  program gets is the
  encoder's choice by its mode (1000 none of the motion, 1002 motion with a button held, 1003
  all), and vt.c skips motion within the cell of the last event it encoded, press or release
  included (except in SGR-Pixels format).
- Selecting with a mouse (`pointer`, as Ghostty does on Linux; the first button pressed owns the
  rest of its stream, and every press takes the focus):
  - The left button (a touchpad's tap) goes to libghostty-vt's selection gesture with its press
    timed (`VtNative.mousePress`, `mouseDrag`, `mouseRelease`), so libghostty-vt counts the clicks:
    a press within the double-tap timeout of the last press and a cell's width of the series' first
    is the next click, up to three. A first click clears the selection, and dragging from it
    selects by cells, a cell counting once the drag passes its point 60% of the way across
    (libghostty-vt's threshold), a rectangle while Ctrl (or Meta) and Alt are held (checked at each
    move); a second click selects the word and a third the line (with Ctrl or Meta, the command's
    output, Prompt marks), each dragging on by its unit; a second or third on empty cells
    leaves the selection as it was. Held past the grid's edges, the drag scrolls as below.
  - Shift+press, with a selection and later than the double-tap timeout after the last left press,
    extends the selection from that press by its unit instead (Ghostty's), when the gesture still
    has it; otherwise it is a plain press.
  - A release keeps the click count for the next press; only a first click that did not drag (it
    stayed in its cell and selected nothing) acts: with Ctrl or Meta on a link it opens the link,
    otherwise it moves the shell's cursor there at the prompt (Prompt marks) and brings the
    keyboard up, as a tap does. A press that stopped a fling still acts.
  - The right button, as Ghostty's before its context menu: a selection holding the cell stays,
    otherwise the word there is selected (on a written space the run of spaces; on empty cells the
    selection stays as it was), and the menu
    comes up pinned at the click once the frame has the selection, with Open link for a link under
    it (no Ctrl needed) and Select output in a command's output.
  - The middle button only takes the focus.
  - No menu comes up after the left button, and a selection made or right-clicked with a mouse
    shows no handles (`pointerSelection`) until the next press that reaches the touch path (a
    finger, a stylus, a touchpad's swipe or pinch), which itself cannot grab one; Ctrl+Shift+C or
    the right-click menu copies it.
  - Unlike Ghostty, a double click on a URL or a right-click on a link selects the word, not the
    link, and Ctrl or Meta count with other modifiers held too.
- The mouse pointer over the terminal (`updatePointer`) is the shape the program asked for with
  OSC 22 (an I-beam by default, as in Ghostty; libghostty-vt's names are CSS's cursors), shown as
  the closest of Android's system pointers (progress as waiting, move as all-scroll, not-allowed as
  no-drop, edge resizes by their axis), or a hand over a link while Ctrl or Meta is held and the
  program does not track the mouse (a Ctrl+click opens it then), looked up again when a hovering
  mouse reaches another cell or a hardware Ctrl or Meta that reaches the view is pressed or let go
  (the extra keys' CTRL counts for neither). It is set on the view, so a program changing it while
  the mouse rests shows at once; Ghostty's I-beam and crosshair for its Shift override and
  Ctrl+Alt rectangle have no counterpart.
- The two handles (shown for a selection not made with a mouse; start below the first cell, end
  below the last, when in the viewport; a
  rectangle's top left and bottom right corners) drag the selection's ends; the other end is the
  anchor, kept as tracked grid references so output does not move it, a handle drag extends by
  cells, and a rectangle stays one
  with the anchor and the finger's cell at opposite corners. A handle that would stick out of the view at the left edge (start) or right edge
  (end) is drawn with the other handle's drawable, on the other side of the same point, so both
  stay on the screen, and the handles' areas (with an 8 dp margin) are excluded from the system's
  back gesture while they are shown, so a drag from the edge moves the handle. The finger keeps
  its offset from the dragged cell; held above or below the grid (a handle's drag or a gesture's:
  a long press's, a double or triple tap's, a mouse's left-button drag), the viewport scrolls one
  row every 50 ms (the grid on
  a whole row) and the selection follows (a gesture's to the edge row's word or line, by the
  view's own timer, not libghostty-vt's autoscroll tick).
- A floating action mode (`TYPE_FLOATING`) appears once the next frame is drawn after a selection
  drag, a long press's word or a double or triple tap's press ends (the finger lifted), Select
  all, or a right-click (pinned at the click), and at once, pinned at the press point, after a long
  press on blanks or a mouse-mode long press that selected nothing; never after a mouse's left
  button. It offers Open link (when the long press that began the selection, or the right-click,
  was on a link, also in
  mouse mode), Copy (with a selection), Paste (with a clipboard), Select output (when that long
  press or right-click was in a command's output), Select all (scrollback included), and with a
  selection Rectangle, or Lines for a
  rectangle, with the system's own labels but Open link's, Select output's and the last two, placed
  over the selected rows and moved with them on every draw; a frame without a selection closes it.
  Open link ends the selection and opens the link. Select output selects the whole output (Prompt
  marks) and the menu comes back for it, without the item, which a double or triple tap,
  Select all, clearing, a mouse-mode drag or a mouse press to select also drop; when the output has
  left the scrollback the
  menu comes back without the item and the selection as it was. Rectangle makes the selection's
  first and last cells opposite corners of a rectangle; Lines makes it linear again from the
  rectangle's top left to its bottom right corner; the menu comes back with the other item. Copy
  takes the selection as plain text with soft wraps joined (not in a rectangle, whose rows are lines
  of their own) and trailing blanks trimmed (as Ghostty copies), puts it on the clipboard and ends
  the selection.
- With a selection, Shift with an arrow, Page Up or Down, Home or End (a hardware keyboard, or the
  extra keys' SHIFT with their arrows, HOME and END) moves its end as Ghostty's adjust_selection
  keys do (`VtNative.selectAdjust`): to the previous or next non-blank cell (going round rows), a
  row or a page up or down at its column, the screen's top left cell, or the end of its last
  non-blank row; the anchor of a drag is dropped and the viewport scrolls to show the end (at its
  top when above, at its bottom when below). Without a selection the keys reach the program.
- Pinch zoom changes the font by 1 dp beyond a 0.9–1.1 scale band, within 6–32 dp, which regrids
  and resizes the terminal and then the console (SIGWINCH); the activity saves the new size.
- A resize (`VtSession.resize`, from attaching and every regrid) takes the replies the terminal
  made for it, the in-band size report of a program using mode 2048, and writes them through the
  tab's writer at once, rather than leaving them for the pump's next read. A search keeps its
  selected match through a resize (the keyboard showing or hiding is one).

### Search

- `VtView.search` (`VtNative.search`) searches the tab's terminal for the search bar's text
  (android/terminal): libghostty-vt matches ASCII letters in either case and everything else
  exactly, in the screen and its scrollback (the primary screen's results are kept while a program
  uses the alternate one); empty text searches for nothing. `searchStep` (`VtNative.searchSelect`)
  selects the next match above or below, going round, and scrolls the viewport to it unless it is
  shown (the scroll bar shows only when it scrolled; a step stops a fling and puts the grid on a
  whole row). The count, reported to the activity when it changes and once after every `search` (so a needle
  that is not found reads "0/0"), grows as a long scrollback is
  searched over several frames (Frames and drawing). `endSearch` (`VtNative.searchEnd`) ends it and
  takes its marks away.
- Matches are drawn as Ghostty draws them: black text on yellow, the selected match on orange, the
  second cell of a wide character with its first. Along the scroll bar, as browsers mark found
  text, a 2 dp mark as wide as the scroll bar in the same yellow (the selected match's in orange)
  stands for each row a match starts on, at that row's place among the scrollable rows (where the
  thumb's top is when the row is the viewport's first, unless the thumb is held at its least
  length), rows closer than a pixel sharing one; the marks stay while the search runs and there is
  a scrollback, under the thumb, which fades as before. The view takes the rows
  (`VtNative.searchRows`) when the count or the selected match changes, when the scrollable rows
  shrink (rows left the scrollback at its limit, or it was cleared) and at most every 500 ms while
  frames bring changes, since rows that leave the scrollback move every match's row. vt.c reads all
  the matches (newest first) and looks up only each page's first row, adding a match's row within
  its page, which reads no compressed page.

### Prompt marks

The view reads the shell's prompt marks (android/vt's Shell integration) to jump between prompts,
select a command's output and move the cursor by taps:

- Jumping (`VtView.jumpToPrompt`, `VtNative.jumpPrompt`): libghostty-vt scrolls the viewport so the
  prompt the given number of prompts above or below its top starts it, as far as there are
  prompts (a jump past the newest returns to the bottom); a fling stops, the grid goes back to a
  whole row (`scrollPx` 0), and the next frame shows the scroll bar if the viewport moved.
- Output (`VtNative.outputAt`, `selectOutput`): a cell is in a command's output when libghostty-vt
  can derive an output selection from it (the cell is output, after a prompt, or above the first
  prompt); vt.c then tracks the cell, and selecting it later takes the whole output as it is then
  (a command still running may have added to it), trailing blanks left out, anchored by cells (no
  drag of the view uses that anchor: a handle anchors anew).
- Taps on the input (`VtNative.promptClick`), as Ghostty's cursor-click-to-move: a prompt start
  (OSC 133;A) on the primary screen that declares how the shell moves its cursor is noted by vt.c
  from the OSC string (the C API does not expose the value libghostty-vt keeps) and holds until
  another declares a way or a full reset: Kitty's `click_events=1` or `2` (fish's) first, else
  `cl=` (`line`, or `m`, `v` and `w`, which Ghostty treats as line too; the profile sends
  `cl=line` from bash and ash); an option with a value it does not know is ignored like a missing
  one, so the way the last prompt declared stays, and only OSC strings of at most 64 bytes, not
  cancelled, are read. A tap without mouse tracking that is not on a link, or a mouse's left click
  (without tracking, or with Shift) that did not drag and is no Ctrl+click on a link, with the
  cursor at the prompt and the tapped row not above the prompt's
  first one (found going up from the cursor, at most 64 rows) moves the cursor there: with `cl`,
  left or right arrows (in the cursor key mode's form), one for each input cell between the cursor
  and the tapped cell over the soft-wrapped rows of the cursor's line (rows after it must continue
  the prompt), a tap on the prompt going to the input's start and one past it or below to its end
  (one more arrow when the cursor is on input); unlike Ghostty, which counts both halves, a wide
  character takes one arrow and its second half stands for it; with `click_events`, a left button
  press in SGR without a release, its row counted from the screen's top (`1`) or from the
  prompt's first row (`2`). The bytes go out as typing does and the keyboard comes up as for any
  tap, or such a click; a tap that is no prompt's sends nothing.

## Dependencies (inbound / outbound)

### Inbound

- `MainActivity` (android/terminal): the entry points above; it also passes the service's bell for
  the tab in front (`bell`) and the wide-cell setting.
- android/vt: `VtSession.view`, through which the session invalidates the view and reports its
  shell's start.

### Outbound

- android/vt: the attached `VtSession` (`tab`, the terminal handle, and `resize`) and `VtNative`'s
  functions for frames, images, the password state, input, scrolling, selection, output, copies,
  links, taps on the prompt and search, which hold the tab's lock.
- android/terminal's `MainActivity` through `Host` (`onGrid`, `onInput`, `onZoom`, the extra keys'
  `ctrl`, `alt`, `shift` and `modifiersUsed`, `onLink`, `onSearchCount`); input reaches the tab's
  writer that way.
- libghostty-vt through vt.c: the render state with one overscan row and its image list, the key
  and mouse encoders, paste, focus encoding, selection gestures (a finger's untimed word and line
  presses and drags; a mouse's timed clicks with their count, cell, word, line and output
  behaviours, rectangle drags, releases, dragged flag and anchor), word selection and selection
  containment (right-click), output derivation and keyboard adjustment, scrolling by prompts,
  search with the list of all its matches, plain-text formatting of copies.
- The system's libjnigraphics (images' pixels), the clipboard, Material Components' dialog for the
  paste confirmation, the system pointers, haptics; JetBrains Mono 2.304 (variable, upright and
  italic) in the assets and the system's fallback fonts for everything else (CJK from Noto Sans CJK
  on AOSP images).

## Invariants and gotchas

- libghostty-vt's mouse encoder deduplicates motion only when asked (off by default) and forgets
  its last cell whenever `set_from_terminal` or the size option is set, which `appine_vt_mouse`
  does for every event; so vt.c keeps the last cell itself, updated only when an event was encoded.
- `GestureDetector` with a double-tap listener (any `SimpleOnGestureListener`) gives the second
  tap of a double tap to that listener: `onSingleTapUp` never sees it and a drag starting there
  does not scroll. `VtView` therefore uses a plain `OnGestureListener`, and takes the press of a
  double or triple tap that selected before the detector (or the pinch detector) sees any of that
  touch: the view then counts the tap itself when the finger lifts without dragging, and a quick
  tap followed by a drag selects rather than scrolls.
- With IME shown, a hardware Esc press is taken by the IME (Gboard hides itself), so a program
  using the Kitty protocol's release events sees only the release.
- The scroll bar is `View`'s own (`computeVerticalScrollRange`, `Offset` and `Extent` return the
  last frame's rows in pixels, the offset with `scrollPx`, so it follows pixel scrolling), shown
  with `awakenScrollBars` wherever `VtView` moves the viewport. A view
  built in code has no scroll bar until it is given a thumb (`verticalScrollbarThumbDrawable`, the
  theme's `scrollbarThumbVertical`) and fading is turned on explicitly; `View` also flashes it
  each time the window becomes visible again.
- A fling runs in `computeScroll`, before each draw takes its frame, so the viewport's ends are
  those of the frame before, kept up by `scrollPixels` with the rows it moved: `VtNative.scroll`
  is relative, and output that changes the scroll total meanwhile can make the two disagree until
  the next frame. A viewport fling ends at the first step that cannot move.
- `scrollPx` is nonzero only while scrolling moves the viewport and there is a row after it; a
  frame without the overscan row puts it back to 0, which keeps the view from showing the
  mirror's stale extra row. Mouse positions sent to programs ignore it, which is safe because
  mouse tracking (another way to scroll) puts it to 0.
- `VtView` turns off the default focus highlight: focused outside touch mode (after hardware or
  injected keys) the system would cover the whole view with a translucent grey.
- Without a terminal (`tab` 0) `onDraw` draws the bitmap as it was last painted.
- A pointer set on the view while the mouse rests is resolved again by the system with an event
  whose source is no mouse, and `View.onResolvePointerIcon` returns the view's pointer only for
  mouse events (seen on API 34), which shows the default arrow; `VtView` returns its own pointer
  for such events (a stylus keeps the system's). A pointer set while the view handles a hover
  event shows only at the next one.
- The selection gesture is one per terminal, shared by touch and mouse: a mouse's click count
  survives its releases, but vt.c's own clearing (`appine_vt_select_clear`: the view's
  `clearSelection`, typing, Clear screen, Reset terminal), a finger's word or line press and the
  finger lifting from its drag, a right-click that selects a word, a failed mouse press and
  attaching another tab during a drag reset it; selecting all, a rectangle, an output or a
  handle's grab do not, so a later Shift+press after the timeout replaces those with an extension
  from the last left press. A cell press yields no selection (vt.c clears it on a first click, and
  a cell drag not yet past its threshold leaves none, neither resetting the gesture), and a repeat
  keeps the first click's cell, so a release on another cell counts as a drag.
- A cursor shape set while a full-screen program shows the alternate screen reaches only that
  screen: libghostty-vt keeps the shape per screen, and leaving the alternate screen with mode 1049
  restores no shape (its saved cursor carries none), so the primary keeps the old one until the
  next DECSCUSR 0, DECSTR, RIS or setting change; leaving with mode 47 or 1047 copies the whole
  cursor, shape included, back (from libghostty-vt's source, not tried).
- The frame format is defined in `vt.c` and read by `VtFrame`; the two change together. The
  key numbers in `VtKeys` are `GhosttyKey` values, which `key/event.h` spells out and libghostty-vt's
  ABI keeps from 1.0 on; key, action and mouse-button values it does not know make its setters
  fail (vt.c then encodes nothing), where the copy in Ghostty 1.4.0 took them and could crash.
- The render state's source rectangles are whole image pixels, so a virtual placement's rows,
  drawn one by one, overlap or gap by a pixel each and show seams (Ghostty's renderer draws them
  so); vt.c joins rows that meet. Rows only partly in the viewport keep the rounding at their top.
- The view paints images into its row bitmap, so a changed placement list must repaint every row:
  vt.c makes such a frame a full one, and the view asks for one when pixels arrive late. An image
  read for a frame during a synchronized-output hold may be gone or replaced by then; its copy
  fails (by generation) and the frame after the hold names the new one.
- Grid references from libghostty-vt are valid only until the terminal changes, so every selection
  call takes fresh ones; the anchor of a handle's drag and the gesture's pressed cell are tracked,
  and the gesture's selections are installed at once. A changed selection makes the next frame a
  full one (the render state redraws everything when the screen's selection changes), as does
  scrolling the viewport.
- libghostty-vt's render state keeps search highlights internally, but the C API does not expose
  them: vt.c marks the matches itself, from the viewport's matches in screen coordinates (viewport
  ones fail for a match that starts above the viewport), and resends a row whose marks differ from
  the last frame's, since the render state does not count them as a change.
- A resize restarts libghostty-vt's search and drops its selected match. vt.c notes the match's
  place from the newest before resizing, runs the search to the end afterwards and steps to that
  place again (the steps scroll nothing but the last); a resize that changed which matches exist
  could select another one.
- The C API's `select_line` defaults to ignoring prompt marks (`semantic_prompt_boundary` off);
  the selection gesture's line is Ghostty's own, with them on, so a triple tap or triple click
  stops at a prompt mark. vt.c no longer calls `select_line`; a direct call would need the option
  set.
- vt.c's tracked cell of an output frees its tracking only after the output's selection is
  installed: untracking may change the page list the references taken from it point into.

## Confirmed bugs / technical debt

- **libghostty-vt's selection and drawing features left unused**:
  - selection: the gesture's click counting for a finger, its autoscroll ticks and data (the view
    scrolls on its own timer), deep presses, behaviour data and word-boundary characters;
  - drawing: the default palette option (the colour scheme reply is fixed to dark); overscan above
    the viewport; `row_iterator_next_dirty`; the two-phase render update: `frame` holds the tab's
    lock while it encodes every row (about 95% of its time on a macOS build), where only
    libghostty-vt's deferred style work would leave it, and taking the encoding out would need the
    render state kept apart from the terminal's lock; `ghostty_render_state_row_cells_get_multi`
    in place of the four to six reads `frame` makes per cell (it stops at the first failing key, so
    the colours would go last; gain unmeasured); the colour utilities (`palette_generate`, contrast
    and luminance), of use only with themes;
- **libghostty-vt's images are trimmed only under pressure before Android 14**: each screen of
  each tab keeps up to 32 MiB of decoded Kitty images, and while a tab is shown the view holds a
  second copy of those placed on its screen (dropped while the window is hidden). Only the
  `RUNNING_LOW` and higher trim levels, which Android 14 and later no longer send, cut the tabs
  not shown down to 4 MiB a screen (android/vt), deleting the oldest images outright; the routine
  trim after the UI leaves keeps them all. A tab restored from its snapshot gets its images back
  (libghostty-vt's snapshot encoder writes them when built with Kitty graphics, though a comment
  in its `snapshot/terminal.zig` says otherwise); one restored from its plain transcript has text
  only.

## Open questions

- Should a Nerd Font or a CJK fallback font be bundled?
- Should showing another tab send focus-out and focus-in (mode 1004) to the two tabs?
- Should invisible cells keep their decorations (as now) or hide them as Ghostty's renderer does?
- Should a mouse wheel or touchpad scrolling the viewport go by pixels and by the event's size
  (high-resolution wheels, touchpads) rather than 3 rows a scroll event?
- Should a mouse's middle button paste the clipboard (Ghostty's `middle-click-action`; Android has
  no primary selection), and should a left-button selection copy on release (Ghostty's
  `copy-on-select`) or bring up the menu?
- Should a double click on a URL, or a right-click on a link, select the whole link, as Ghostty
  does, and should the extra keys' CTRL make Ctrl+click and the hand pointer work?
- Should a cursor shape changed while a full-screen program shows reach the primary screen too?

## To verify

- Tried only on a Pixel 4a API 34 emulator (4 KB pages, Gboard in English, input from adb,
  `sendevent` and a `uinput` virtual mouse) and through vt.c's harnesses on a macOS build, where
  the search and frame times come from; not on real or 16 KB-page devices or Android 11–13.
  Unverified beyond that:
  - input: CJK IMEs' composing text and floating candidates (`CursorAnchorInfo`), which IMEs
    deliver Enter as text, other IMEs' password mode; hardware keyboards; real fingers, styluses,
    mice and touchpads (their swipe and pinch classification), DeX, ChromeOS and desktop
    windowing; sideways scrolling and wheel events to a mouse-tracking program on a device; the
    autoscroll of a drag held past the grid's edges; taps on the input with fish, or ash with wide
    characters;
  - drawing: Kitty graphics with yazi, timg, `kitten icat`, tmux passthrough and the 32 MiB storage
    filled; the bell with haptics off; the cursor setting in Chinese or while a full-screen program
    shows; the lock cursor with other IMEs composing at it; the search's marks while rows leave a
    full scrollback.
