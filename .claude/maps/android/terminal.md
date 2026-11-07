# android/terminal Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/android/terminal.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 1 research concern; 6 claims checked by 2 independent verifiers — 5 confirmed and 1 partial; bugs and debts recorded)
>
> Maintained: 2026-11-08 (targeted verification only since the full mapping, which predates libghostty-vt: Termux replaced by it; bars, extra keys, themes; mouse; insets; tab animations; scrolling; links; unwired libghostty-vt features, Kitty graphics last)

## Responsibilities

`android/terminal` is the terminal screen and the pieces that render the service's sessions:

- `MainActivity`: the screen's layout, the current session, closing tabs, keyboard shortcuts, font
  zoom, insets and the IME, with the tab bar (`TabBarView`) and the extra keys (`ExtraKeysView`);
- the terminal: `VtSession` (a tab: its shell, the terminal, pump, waiter and writer threads),
  `VtView` (drawing with the bundled JetBrains Mono, input, selection, links), and the emulator shim and
  JNI in the native library (`vt.c`, `vt.rs`) around Ghostty's libghostty-vt;
- `ConsolePty`: the tab's kernel console (`/dev/pts/N`) and login shell;
- `LicensesActivity`: the notices generated at build time;
- the app's strings and themes: `Theme.Appine` (Material 3 DayNight, for Settings and the
  licenses) and `Theme.Appine.Terminal` (Material 3, always dark, with a black window and
  system bars, for `MainActivity`), and the Material 3 page layouts that Settings and the licenses share
  (`page.xml`, `list_item.xml`, `list_header.xml`).

Root: `android/app/src/main/java/io/github/mthli/appine/MainActivity.kt`, `VtSession.kt`,
`VtView.kt`, `VtGlyphs.kt`, `VtKeys.kt`, `VtNative.kt`, `ConsolePty.kt`, `TabBarView.kt`,
`ExtraKeysView.kt`, `LicensesActivity.kt`, `android/app/src/main/res/`, `android/app/src/main/assets/fonts/`,
`hosts/android/src/vt.c` and `hosts/android/src/vt.rs`.

Outside the module: session lifecycle, saving and kill routing (`android/service`); the host console
in the kernel (`kernel/tty`); the notices task, the JNI crate and the build, including compiling
libghostty-vt with Zig (`android/native`).

## Key types

- `MainActivity` — a `TerminalService.Listener`; one `VtView`, `TabBarView` and `ExtraKeysView`,
  with `Host : VtView.Host`, `Bar : TabBarView.Target` and `Keys : ExtraKeysView.Target`; the
  current session and its place, from which its successor is taken.
- `TabBarView` — the tab bar (a scrolling row of numbered tabs, with `×`, `+` and `⋮` over its
  end): shows the sessions and the current one it is given (`render`) and sends taps on tabs and
  buttons, and the titles long presses show, to a `Target`; `Tab` (one per session: its number in
  the row, which slides, rolls to a new number and fades out when closed); `TabNumber` (the
  number, a `TextView` drawing its digits itself so they can roll).
- `ExtraKeysView` — Termux's default keys widened to two rows of eight (ESC, `/`, `-`, HOME, ↑, END,
  `|`, SHIFT; TAB, CTRL, ALT, ←, ↓, →, `~`, SYM), with a second page of symbols for every key but
  SYM (`` ` ``, `/`, `-`, `=`, `^`, `\`, `|`, `%`; `[`, `]`, `{`, `}`, `<`, `>`, `~`: the ASCII
  symbols Gboard gives neither on its letters nor from a long press on one, and `-`, with `/ - | ~`
  where they are on the first page): a `Page` is a
  grid of the first seven columns and the key above SYM; the grid shown is in a
  `HorizontalScrollView` across the view, the last column (SHIFT or `%` over SYM) fixed over its
  end; keeps CTRL, ALT
  and SHIFT (one-shot or locked) and the page shown, and sends keys, text and notices (a modifier
  locked) to a `Target`; `Press` handles the touch on one key. Its companion's `drawIndicator`
  draws the indicator under a label, for the keys and for the tab row.
- `ConsolePty` — `start` (open console, spawn `login.sh`), `waitFor`, `close`; `console` is −1
  until started.
- `LicensesActivity` — an `AppCompatActivity` on the Material 3 page: list from
  `assets/notices/index.tsv` (one `list_item.xml` row per entry), full text view; an
  `OnBackPressedCallback`, enabled while a full text is shown, returns to the list.

### The terminal

- `VtSession` — a tab owned by the service: its `ConsolePty`, the terminal handle (0 until the
  shell starts), the view showing it (`view`, read on the pump thread), pending restored text and
  messages, a `vt-pump` thread, a `vt-waiter` thread and its own single-thread `vt-writer`
  executor; `Events` (title changed, finished) go to the service.
- `VtView` — the view: the attached `VtSession`, cell metrics, a mirror of the screen (codepoint,
  colours, attributes, grapheme clusters per cell, each row's selected columns), a backing bitmap
  of the grid, the modes, scroll position and selection bounds of the last frame, the IME
  connection, gestures and the fling's `OverScroller`, selection handles and the floating menu.
  `Host` (the activity): `onGrid`, `onInput`, `onZoom`, the extra keys' `ctrl`, `alt`, `shift`
  and `modifiersUsed`, and `onLink` (opens a link in another app).
- `VtGlyphs` — box drawing (U+2500–U+257F), block elements (U+2580–U+259F) and the Powerline
  triangles (U+E0B0–U+E0B3) drawn as shapes.
- `VtKeys` — Android key codes to libghostty-vt's `GhosttyKey` numbers, modifier bits, actions.
- `VtNative` — `@JvmStatic external` functions on the native library.
- In `vt.rs`: `Tab` (the kernel's handle, console number, the terminal under a mutex, a `dirty`
  flag) in a table `TABS` keyed by handles from 1, never reused. In `vt.c`: `AppineVt` (the
  terminal, its render state, key and mouse encoders, reply, frame and copy buffers, the
  synchronized-output hold, and the tracked anchor of a selection drag).

## Public entry points

- Launcher activity `MainActivity` (`singleTask`, `adjustResize`, `Theme.Appine.Terminal`, handles
  most configuration changes itself, the dark mode included); listener callbacks `onMachineReady`, `onProgress`, `onSessionsChanged`,
  `onExitAll`, `onRestarting`, `onSettingsChanged`, `onNotice`.
- `LicensesActivity`, opened from Settings.
- `VtNative`: `create`, `destroy`, `pump`, `frame`, `resize`, `scroll`, `key`,
  `paste`, `mouse`, `focus`, `selectWord`, `selectGrab`, `selectTo`, `selectAll`, `selectClear`,
  `copy`, `link`, `title`, `message`, `transcript`, `print`; `pump` calls `VtSession.onOutput` and
  `onTitle` back.

## Data flow / lifecycle

### Binding and tabs

- `onStart` starts and binds the service; `onServiceConnected` registers the listener and replays
  ready/error/progress and pending notices; `onStop` unregisters and unbinds.
- The bar is one 32 dp row in any window: a scrolling row of tabs across its whole width, with
  `×` (closes the current tab; hidden while there is none), `+` (opens a session while the
  kernel runs) and `⋮` (a menu: "Close other tabs", disabled when there is no other, "Close all
  tabs" and Settings) over its end, every tab and button a 32 dp square with its label centred.
  The tabs pass under the buttons, which hide them, and an end padding as wide as the buttons lets
  the last tab scroll out from under them; while more tabs lie beyond, the row fades out over
  32 dp ending 12 dp inside `×`, in the blank before its glyph (the scroll view's right fading
  edge, shortening towards that end as the row nears its own; the left edge does not fade). A
  tab shows its number (from 1, by position): the current one in the primary colour above the
  indicator, the others muted, an ended one faded (38% opacity, its indicator too). The current
  tab is scrolled into view after each change, clear of the fade, or to the row's end when less
  than a fade's width of tabs would be left beyond the padding; scrolled away from it (out of view
  or into the fade), the row brings it back the same way 3 s after coming to rest: a finger on the
  row, on a tab too, holds that off, and each later move without one (a fling, TalkBack's
  scrolling) starts the wait again. With gesture navigation, a slide from the screen's left edge
  over the tabs scrolls them instead of going back: the bar but for its buttons is excluded from
  the system's back gesture. The indicator is a rounded bar 16 dp wide and 1 dp high in the
  primary colour, centred 4 dp below the label's baseline, in whole pixels rounded down; the row
  draws it over its tabs. Labels are sized in dp (numbers 12 in the default weight, buttons 14) so
  the system font scale leaves them as they are (dialogs, menus and toasts follow it).
- Closing tabs is animated (150–300 ms a step, under the system's animator duration scale): a
  closed tab shrinks and fades out where it was; the tabs after it slide into the places left,
  still showing their old numbers, then roll to the new ones (300 ms, like SwiftUI's numeric
  text): each digit that changes moves down and out, fading and blurring, as the new one comes in
  from above and sharpens (from below for a higher number, which closing never gives); digits
  that stay the same in place stay still, and a number of another length rolls whole. The indicator
  keeps to the current tab's place on screen, so closing the current tab of a row that does not
  scroll shows the next one sliding in above it; a row at its end scrolls back as it shortens, the
  tabs before slide right instead, and the indicator slides on to the new current tab. Showing
  another tab (a tap, Ctrl+Tab, a new tab) slides the indicator there the same way (200 ms) from
  where it is drawn, so a switch during a slide sends it on without a jump, and one to a tab out
  of view slides it while the row scrolls; only the row's first layout puts it in place at once.
  The numbers' colours change at once, and so do TalkBack's descriptions.
- Tapping a tab shows it; a long press shows its title in a toast: the program's title (`sh` when
  it set none), with "(ended)" once the shell ended. The activity shows one toast at a time (a
  tab's title, a locked modifier), cancelling the previous one. TalkBack reads "Tab N: title".
- `×` and the menu's "Close other tabs" and "Close all tabs" close at once when every shell they
  close has ended, else after a confirmation dialog ("Close this tab?" or "Close N tabs?"; closing
  hangs up a shell, which usually ends the programs in it); several tabs are closed with the
  current one last, so no other tab is shown (and its shell started) on the way, and closing all
  ends like closing the last tab. Ctrl+Shift+W and Enter in an ended tab close without asking.
- Its dialogs (close, notices, the paste confirmation) and its menu (⋮) are Material 3 in the
  theme's dark colours, from the wallpaper on Android 12 and later (`DynamicColors`). The tab bar,
  the extra keys and what lies under the system bars are black like the terminal; their labels,
  buttons and the status text take the muted on-surface-variant colour, and the current tab's
  number and indicator the primary one.
- Hardware-keyboard shortcuts, taken by the activity (`dispatchKeyEvent`) before the terminal sees
  them, together with the matching key release: Ctrl+Shift+T opens a tab, Ctrl+Shift+W closes the
  current one (both once per press), Ctrl+Tab and Ctrl+Shift+Tab show the next and the previous
  one, going round and repeating while held. They are also listed in the system's keyboard
  shortcuts screen (Meta+/).
- Closing a tab hangs it up at once; when it was the current one, the tab after it, now in its
  place, is shown (the one before when it was the last). Closing the last tab while the kernel runs
  finishes the activity (the service stays). After a reconnect the focused tab is the service's `focus` or the
  last one.
- Showing a tab attaches it to the view (`VtView.attach`); `onRestarting` and `onDestroy` detach
  it, so sessions never keep a destroyed Activity's view.

### Input and display

- The font size (pixels) is kept in the activity's preferences.
- Whether glyphs in wide cells fill them is a setting kept by the service
  (`TerminalService.fillWide`, on by default, switched in Settings); the activity applies it to
  the view in `onStart` and `onSettingsChanged`, and a change repaints the whole screen.
- Extra keys: rows of 32 dp with 12 dp labels (not sp, so the system font scale leaves them as they
  are); the eight columns share the width, each at least
  45 dp (the last one takes the pixels left over), so they all fit from 360 dp on. Narrower, the
  first seven columns of both rows scroll sideways together under the finger (turning the page
  keeps the scroll position) and under the last, which stays at the right edge; while more keys
  lie beyond, they fade out over 32 dp ending 6 dp inside the last column, in the blank before
  SHIFT, as the tab row's into `×` (no fade at the row's end or on the left), and a touch there
  goes to the last column's key. A sideways slide past the touch slop then scrolls and cancels the
  key it started on, unless that key is already held, and with gesture navigation a slide from the
  screen's left edge scrolls them instead of going back: the scrolling part beside the last column
  is excluded from the system's back gesture. With nothing to scroll the keys keep every touch and
  the edge its gesture. No lines are drawn between or around the keys.
- A key acts when the finger lifts (with a key-click haptic), not at all if the finger
  left it by more than the touch slop, and is shaded while pressed (the highest surface container
  colour). Held past the
  long-press timeout, the arrows repeat every 80 ms (no act on release then); no key has a swipe
  or popup. CTRL,
  ALT and SHIFT: a tap turns them on for the next key (primary colour only) or off, a locked one
  too; a long press locks them on (primary colour and the tab bar's indicator) until tapped, one a
  tap turned on included, with a toast naming the key ("CTRL locked"), or turns a locked one off.
  Keys go to `VtView.press` and `type`, which read the
  modifiers through the host and report when a key took them; that clears one-shot modifiers and
  leaves locked ones on.
- SYM shows the symbols in place of every other key (`%` in place of SHIFT) and keeps them, for
  any number of keys, until tapped again (a long press acts as a tap on release); its label takes
  the primary colour and the indicator while they are shown. Turning the page leaves the
  modifiers as they are: CTRL
  and ALT apply to symbols too (CTRL, SYM, `]` sends Ctrl+]), though they cannot be seen on the
  symbols page, while SHIFT waits there: it applies to no key, the symbols or the IME's, and a
  one-shot SHIFT is not used up, so it applies again once the first page is back. A key still held
  on the page going away is cancelled. The page lives as long as the view: a new activity starts
  on the first.
- The content is edge-to-edge with the root padded by system-bar, display-cutout and IME insets
  (from Android 15 the window reaches into the cutout, which in landscape is at a side). As the
  IME shows or hides, the root is laid out for the end at once (the terminal regrids once, at the
  start) and the extra keys are drawn moving from their old place to the new one along the IME
  animation's curve (`keysFollowIme`), rising through the IME's room or sinking over the
  terminal's new rows; an animation that interrupts another starts from where the keys are drawn.

### Tabs

#### Start and end

- A `VtSession` starts its shell the first time a view showing it knows the grid (`VtView.attach`
  and every regrid call `Host.onGrid`, which starts the current tab once): `ConsolePty.start`
  opens a console of the grid's size and spawns the login; `VtNative.create` makes the terminal
  (scrollback 10,000 lines; grapheme clustering, mode 2027, on by default as in Ghostty; Kitty
  graphics, with a storage limit of 0, and the Glyph Protocol off, so neither is advertised and
  programs that probe fall back to text); restored
  text and queued messages are printed into it; then the pump, the waiter and the writer start.
  If spawning fails, the reason is printed, the console is closed and the tab counts as ended
  (127).
- The pump reads the console into a 64 KB buffer, feeds the terminal under the tab's lock, writes
  the terminal's replies (DA, DSR, XTVERSION, size reports) back to the console before reading
  again, calls `VtSession.onOutput` only when it sets `dirty` (so a burst asks for one frame, and
  nothing is asked while no view takes frames) and `onTitle` after OSC 0/2. It runs whether or not
  a view shows the tab.
- The waiter waits for the shell, gives the pump up to 0.5 s to take the last output, then on the
  main thread closes the console (once) and prints "[Process completed (code N | signal N) - press
  Enter]". Enter from a key or the IME then closes the tab (`Host.onInput`).
- `release` (the tab closed, or its kernel going away) hangs up the console once, stops the writer
  and destroys the terminal handle; a running pump keeps the terminal until it returns.
- Saving: `transcriptText` formats the primary screen with its scrollback as plain text (soft
  wraps joined, trailing blanks trimmed); on the alternate screen it returns the last transcript
  taken on the primary one. Kill reports go through `VtNative.message` (a yellow line inserted above
  the cursor line: down and back, save the cursor, insert lines, write, restore and move down;
  skipped on the alternate screen) or wait until the terminal exists.

#### Frames and drawing

- `onOutput` posts an invalidate on animation to the attached view; `onDraw` takes a frame
  (`VtNative.frame`, main thread only) and applies it: `frame` clears `dirty`, updates the render
  state (unless a synchronized-output hold is active), and writes the header plus the dirty rows
  (all rows when the view asks for a full frame) in the format documented in `vt.c`; the direct
  `ByteBuffer` is valid until the next `frame`.
- A hold (mode 2026) captures the screen when it begins and is ended by force after 1 s.
- The view asks for a full frame after a regrid, a new bitmap, new default colours, a change of
  the wide-cell setting, attaching another tab or the attached tab's shell starting; a frame
  captured at an old size is dropped and a full one requested. Attaching also resizes the tab's
  terminal and console to the view's grid when it differs.
- The header also carries mode flags (mouse tracking, alternate screen, alternate scroll 1007,
  focus events 1004, a selection exists), the selection's first and last cells in the viewport,
  and the scroll position libghostty-vt reports for the scroll bar (the scrollable area in rows,
  scrollback and screen, the viewport's first row in it and the rows it shows); each row carries
  its selected columns.
- Cell metrics: the width is the font's advance rounded to whole pixels and the text size is
  adjusted so the advance equals it; the height is the ceiling of ascent+descent, with the baseline
  centred; the light line width is the font's underline thickness. The wide scale is two cells over
  the advance of "水" (the system's CJK fallback, one em, so 1.2 with JetBrains Mono; a font's own
  ideograph when it has one), at least 1, or 1 with the setting off.
- Each dirty row is repainted into the bitmap under a clip: default background, runs of other
  backgrounds, the selection's translucent highlight, then text. Plain ASCII runs of one paint,
  colour and decoration go out in one draw call; other glyphs are centred in their one or two
  cells, a wide cell's glyph enlarged evenly by the wide scale about the cell's vertical middle,
  and any glyph still wider than its cells shrunk evenly to fit (never stretched); `VtGlyphs`
  characters are shapes; four paints (regular/bold × upright/italic) from the variable font with
  ligatures off. Underlines (single, double, curly, dotted, dashed), strikethrough and overline are
  drawn; inverse, faint and invisible apply; blink is ignored.
- The cursor (bar, block with the glyph redrawn in the background colour, underline, hollow; hollow
  when unfocused), the IME's composing text (shifted left at the right edge) and the theme's text
  selection handles are drawn over the bitmap each draw.

#### Input

- Keys: mapped keys go through `VtNative.key`, which encodes them in the terminal's current modes
  (cursor and keypad modes, Kitty keyboard protocol, including releases from `onKeyUp` and lone
  modifiers, which are sent only when the encoder emits something); unmapped keys that type a
  character without Ctrl or Alt are sent as text. CTRL, ALT and SHIFT on the extra keys (SHIFT
  only with the first page shown) apply to the next key or one typed character (turned back into a key event through the virtual keyboard's
  key map), or to every one while locked: they are added to the key event's meta state, so with
  SHIFT a key types its shifted character in that map (`a` → `A`, `-` → `_`, `/` → `?`) and Tab
  is sent as back-tab. A character the key map lacks (CJK) or a commit of several characters goes
  as text and leaves them on. The extra keys send presses only (a held key repeats as new presses),
  never releases. Ctrl+Shift+C and V (held, or from the extra keys) and the dedicated copy and
  paste keys copy and paste.
- Typed and pasted input ends a selection; all input first scrolls the viewport back to the bottom
  (and, but for a fling's own wheel events and arrow keys, stops a fling), then goes through the
  host to the tab's `vt-writer` thread (`Native.write`).
- IME: committed text is sent at once (`\n` as `\r`, a lone `\n` as Enter, several lines as a
  paste, which covers the IME's clipboard); the editor action is Enter; composing text is only
  drawn at the cursor; the editable is kept empty so Backspace with nothing composed arrives as
  `deleteSurroundingText` and becomes DEL. `requestCursorUpdates` is answered with the cursor and
  composing text's position (`CursorAnchorInfo`), again whenever the cursor moves while the IME
  monitors it.
- Paste (`VtNative.paste`, through `ghostty_terminal_paste`): bracketed with mode 2004, else line
  breaks become carriage returns; unsafe control bytes become spaces. Text that could run commands
  (line breaks without bracketed paste, or the bracketed terminator) is refused, and the view asks
  in a dialog with a preview before pasting it anyway.
- Focus: window focus changes are sent as focus events when the program asked (mode 1004).

#### Gestures and selection

- A tap ends a selection or its menu; otherwise, when the program does not track the mouse and the
  tap is on a link, it opens the link instead of the IME; otherwise it opens the IME and, when the
  program tracks the mouse, also sends a left click (press and release at the tap's pixel position,
  in the program's format). Taps act at once: `VtView` tells a double tap itself (the second tap's press within the
  system's double-tap timeout and slop of the first's lift), so without mouse tracking the second
  tap selects the word under it (with the menu), and with it both taps are clicks, for the
  program's own double click; a third tap counts as a first, and so does a tap after one that
  opened a link.
- A link is a cell's OSC 8 hyperlink, else an `http://` or `https://` URL written in the text
  (`VtNative.link`): vt.c joins the cell's soft-wrapped line (up to 64 rows above and below), takes
  the run's first scheme at or before the cell and runs on over ASCII URL characters (letters,
  digits and RFC 3986 punctuation but the single quote) while brackets balance, then drops a
  trailing `.,;:!?`; any other character, CJK included, ends it. Only `http`, `https`, `mailto`
  and `file` links open (an OSC 8 link can name any scheme, other apps' deep links among them). The
  activity opens a web or mail link as a browsable `ACTION_VIEW` intent; a `file:` link (`ls
  --hyperlink`) is looked up off the main thread through the Files app's provider
  (`Documents.fileLink`) and opened as its document, typed by it and with read access granted: a
  directory in the Files app, a file in the app the system picks. A link from another host, outside
  the home while the whole system is not shown, or to nothing in the store gets a toast instead, as
  does a link no app takes.
- A vertical drag, or a mouse wheel (3 rows a notch), scrolls by whole rows: wheel events when the
  program tracks the mouse, arrow keys on the alternate screen with alternate scroll (1007, on by
  default), else the viewport.
- A fling goes on scrolling that way, slowing down as a ScrollView does (Android's fling physics
  in pixels, taken as whole rows like a drag): the viewport until it reaches either end, wheel
  events and arrow keys for at most a screen of rows. It stops when the way to scroll changes
  (mouse tracking, the alternate screen), at a touch (whose tap then does nothing else), at any
  other input to the program, when the window loses focus and when another tab is shown.
- The viewport scrolling (a drag, wheel, fling, a selection's autoscroll, input returning it to the
  bottom) shows the theme's scroll bar at the right edge, faded out after a moment as a
  ScrollView's; output alone does not show it, and with no scrollback (the alternate screen) it is
  never drawn.
- A long press selects the word under it (`ghostty_terminal_select_word`, installed as the
  terminal's selection) and keeps it as the anchor: moving on extends the selection from the far
  side of the word to the cell under the finger. On blank cells it selects nothing and shows a menu
  with Paste and Select all only. When the program tracks the mouse, the long press selects the
  same way, but what it becomes waits for the finger: lifted, it keeps the word (or the Paste
  menu); moved on past the touch slop, the selection is dropped and the program gets a left-button
  drag instead (press at the long-pressed point, motion while moving, release where the finger
  lifts).
- A mouse or touchpad (not a finger) pressed while the program tracks the mouse goes to it as it
  is: the first button pressed (left, right or middle; a touchpad tap is left) with modifiers,
  motion while it is held and its release, bypassing the gestures. Hover (no button) is sent as
  motion too. Which of these the program gets is the encoder's choice by its mode (1000 none of
  the motion, 1002 motion with a button held, 1003 all), and vt.c reports motion once per cell
  (except in SGR-Pixels format).
- The two handles (start below the first cell, end below the last, when in the viewport) drag the
  selection's ends; the other end is the anchor, kept as tracked grid references so output does not
  move it. The finger keeps its offset from the dragged cell; held above or below the grid, the
  viewport scrolls one row every 50 ms and the selection follows.
- After a drag, a floating action mode (`TYPE_FLOATING`) offers Open link (when the long press
  that began the selection was on a link, also in mouse mode), Copy (with a selection), Paste
  (with a clipboard), Select all (scrollback included), with the system's own labels but Open
  link's, placed over the selected rows. Open link ends the selection and opens the link. Copy takes the selection as plain text with soft wraps joined and trailing
  blanks trimmed (as Ghostty copies), puts it on the clipboard and ends the selection.
- Pinch zoom changes the font by 1 dp beyond a 0.9–1.1 scale band, within 6–32 dp, which regrids
  and resizes the terminal and then the console (SIGWINCH).

### Licenses

- The build writes `notices/` (Appine's licence; Ghostty's libghostty-vt, uucode, simdutf,
  Highway, Wuffs, Zig's compiler_rt and JetBrains Mono; Alpine packages from the rootfs database,
  the doas package, Android libraries, Rust crates, SQLite, then full texts); full text is
  monospace, selectable and sized to fit 74 columns.
- The page (`page.xml`): a `MaterialToolbar` (title, back arrow) in an `AppBarLayout` that lifts
  while the content below it is scrolled, over a `NestedScrollView`; edge to edge (wallpaper
  colours from Android 12 on through `DynamicColors`), with the bar padded under the status bar
  and the scroll view clear of the navigation bar. The toolbar shows the entry's title over its
  full text; the back arrow and back both return to the list, then close.
- It follows the system's dark mode: a change recreates the activity, which shows the same full
  text again (saved state).

## Dependencies (inbound / outbound)

### Inbound

- `TerminalService` (creates and holds the `VtSession`s: `showMessage`, `setRestored`,
  `transcriptText`, `release`; listener callbacks); `SettingsActivity` (licenses).

### Outbound

- `Native` (`openConsole`, `spawn`, `write`, `closeConsole`, `waitFor`), hence the kernel's host
  console; `login.sh`; `assets/notices`.
- Material Components 1.14.0 with AppCompat and androidx.activity (both themes, `LicensesActivity`
  and the page layouts, `MainActivity`'s dialogs and menus, `VtView`'s paste confirmation).
- libghostty-vt 1.4.0 (the terminal state, render state, key and mouse encoders, paste, focus
  encoding, selection, plain-text formatting, title and width), linked into `libappine.so`
  (`android/native`); `Machine::read_console`, `write_console` and `resize_console` called from
  `vt.rs` directly; the system clipboard; the app the system picks for a link (a browser, or a
  viewer or the Files app for a guest file through `Documents.fileLink`, android/documents);
  JetBrains Mono 2.304 (variable, upright and italic) in the assets, and the system's fallback
  fonts for everything else (CJK from Noto Sans CJK on AOSP images); the wide-cell setting
  `TerminalService.fillWide`.

## Invariants and gotchas

- libghostty-vt's mouse encoder forgets its last reported cell (its motion deduplication)
  whenever `setopt_from_terminal` or the size option is set, which `appine_vt_mouse` does for
  every event; so vt.c keeps the last cell itself, updated only when an event was encoded.
- `GestureDetector` with a double-tap listener (any `SimpleOnGestureListener`) gives the second
  tap of a double tap to that listener: `onSingleTapUp` never sees it and a drag starting there
  does not scroll. `VtView` therefore uses a plain `OnGestureListener`.
- The console opens lazily, so restored tabs that were never shown have no console and no shell.
- `openConsole`, `spawn`, `resize` and `closeConsole` run on the main thread and take the kernel's
  BKL; input is written on each tab's own thread, so a program that stops reading blocks only its
  tab.
- Console numbers are freed at close and reused lowest-first by the kernel, and a close carries no
  generation, so closing a stale number closes whichever console now has it.
- Programs never receive Ctrl+Shift+T, Ctrl+Shift+W, Ctrl+Tab or Ctrl+Shift+Tab from a hardware
  keyboard (the tab shortcuts); the release of the shortcut's key is swallowed too, so the Kitty
  protocol sees no release without a press.
- The tab bar's buttons sit over the row's end, so their group is clickable and black but for the
  left part of `×` the fade reaches into: a hidden (`INVISIBLE`) `×` takes no touches, and
  without the group's a tap there would reach a tab under it. The black is an `InsetDrawable`,
  whose inset a background also sets as the view's padding, so the group clears its padding
  after it, as the extra keys' last column does (its keys would narrow). The group is not
  important for accessibility, so TalkBack reads each button alone.
- The tab row and the extra keys' grid draw into their end padding (`clipToPadding` off).
  `HorizontalScrollView` leaves the padding out of its scroll range and fade strength, and `View`
  draws the fading edge at the padding; a right padding offset (`isPaddingOffsetRequired`,
  `getRightPaddingOffset`) carries the edge into `×` or the last column without changing either.
  `canScrollHorizontally` counts the end padding as shown (from 315 dp on the keys would find
  nothing to scroll), so the extra keys compare the grid's width with the room between the
  paddings. How far a fade reaches in (12 dp into `×` at 14 dp, 6 dp into SHIFT at 12 dp) fits
  the blank before the glyph in Roboto; another label size or font can put the fade over it.
- The row brings the current tab into view after a change with a plain post of its own, not the
  delayed return's `Runnable`: the scroll that laying out a shorter row makes restarts the return's
  wait, which would remove that post and leave the tab out of view for 3 s.
- A `HorizontalScrollView` takes any sideways slide past the touch slop even when it has nothing
  to scroll, cancelling the child's touch; the extra keys' scroll view intercepts only when it can
  scroll, and a held key asks its parents not to intercept.
- Android honours at most 200 dp of back-gesture exclusion on each edge of a window; the tab
  bar's 32 dp and the scrolling extra keys' 64 dp leave room under it.
- The column width is set on the keys' layout params during `ExtraKeysView.onMeasure`; the rows
  and the grids inside the scroll view are measured without a width limit, so they are forced to
  measure again whenever the column width changes, the page not shown too (it keeps the flag until
  it is added back).
- The indicators are drawn by the containers (the tab row, each page's grid, the fixed column) in
  `dispatchDraw`, over their labels, so a change of state must invalidate the container, not the
  label: `refresh` invalidates every grid and the fixed column, and `TabBarView.render` the tab row. The
  label's place is the sum of `left` and `top` up to the container:
  `offsetDescendantRectToMyCoords` subtracts each view's scroll, and a single-line `TextView` (the
  extra keys' labels) scrolls far sideways to centre its text, which also draws a foreground
  drawable it is given off the label.
- The tab row keeps its tabs across changes and relies on sessions being added only at the end.
  It animates a change from the on-screen places taken at the first change before the next
  layout (closing the current tab renders the row twice), in a pre-draw listener once laid out.
  Closed tabs are drawn in the row's overlay, past its end once it shortens, so its scroll view
  does not clip its children; a roll under way goes on there. The indicator goes by the label's
  layout place, not its translation (the row's own slide comes as `drawIndicator`'s `dx`), and
  takes the label's opacity, so a number rolls inside the label, which keeps its opacity and place.
- `TabNumber` draws its digits one by one, centred between its paddings, rolling or not, never
  through `TextView`'s layout, whose centring rounds differently: a digit that stays would shift
  by a fraction of a pixel as a roll starts or ends. The blur is a `BlurMaskFilter` on the
  `TextView`'s own paint, made in the roll's update listener (`onDraw` must not allocate) and
  cleared after each draw.

### The terminal

- A tab refers to its kernel by handle and looks it up for each call (each console read of the
  pump, each reply write, each resize), like the `Native` functions: `Native.stop` and `shutdown`
  only end the processes, and the store is closed when the last `Arc<Machine>` goes, so a tab
  holding one would keep the old store open after a reboot, tier change or distro switch, and the
  next kernel would start without the boot cleanup. The service releases its tabs (closing their
  consoles) before shutting the kernel down, so the pump's read returns first.
- A `VtSession` closes its console exactly once (`hangUp`), so a later close cannot hang up a tab
  that reused the number.
- libghostty-vt is not thread-safe: every call into `vt.c` holds the tab's lock; the frame buffer is
  written only by `frame`, which only the main thread calls, so the main thread reads it after the
  lock is released.
- The frame format is defined in `vt.c` and parsed in `VtView.apply`; the two change together. The
  key numbers in `VtKeys` are those of Ghostty 1.4.0's `key/event.h` and change with that header.
- Since libghostty-vt 1.4, `GHOSTTY_TERMINAL_OPT_USERDATA` takes the pointer itself, not its
  address (older code passing `&ptr` crashes in the reply callback).
- Without a DA reply, programs that probe for it (vim, tmux) wait for a timeout, so `vt.c` answers
  DA1/DA2 (VT220) and XTVERSION ("appine") and reports a dark colour scheme.
- libghostty-vt as a library turns Kitty graphics on with a 10 MB store and the Glyph Protocol on,
  answering `OK` and `fmt=glyf` to their queries, though frames carry no images and the C API
  gives no access to registered glyphs; vt.c turns both off at create. A full reset keeps the
  storage limit (the alternate screen takes the primary's), so they stay off.
- With grapheme clustering on, programs that count widths per codepoint (`wcwidth`) disagree on
  emoji sequences; they can turn mode 2027 off.
- Grid references from libghostty-vt are valid only until the terminal changes, so every selection
  call takes fresh ones; the anchor of a drag is tracked. A changed selection makes the next frame a
  full one (the render state redraws everything when the screen's selection changes).
- With the IME shown, a hardware Esc press is taken by the IME (Gboard hides itself), so a program
  using the Kitty protocol's release events sees only the release.
- The scroll bar is `View`'s own (`computeVerticalScrollRange`, `Offset` and `Extent` return the
  last frame's rows), shown with `awakenScrollBars` wherever `VtView` moves the viewport. A view
  built in code has no scroll bar until it is given a thumb (`verticalScrollbarThumbDrawable`, the
  theme's `scrollbarThumbVertical`) and fading is turned on explicitly; `View` also flashes it
  each time the window becomes visible again.
- A fling runs in `computeScroll`, before each draw takes its frame, so its end checks (top row,
  bottom) see the frame of the step before: it stops one frame after reaching an end.
- `VtView` turns off the default focus highlight: focused outside touch mode (after hardware or
  injected keys) the system would cover the whole view with a translucent grey.
- `MainActivity` is a framework `Activity` on a Material theme: Material dialogs and AppCompat's
  `PopupMenu` take the theme, but the views it builds or inflates from XML are the framework's (no
  AppCompat inflater), so AppCompat view attributes such as `app:tint` do nothing there. The text
  selection handles `VtView` draws and the floating text menu read framework attributes, which
  AppCompat forwards to the Material 3 colours (the handles are the primary colour).
  Theme.Material3 does not set the system bars' colours, which Android applies up to version 14,
  so `Theme.Appine.Terminal` sets them black, as the framework theme had them.
- The system lays the window out with the IME's end insets as its animation starts, when it hides
  as well as when it shows, so without a `WindowInsetsAnimation` callback the extra keys jump
  there at once. The keys rising with the IME are drawn inside the root's bottom padding, which
  already holds the IME's room, so the root must not clip to its padding (`clipToPadding` off) or
  they stay hidden until they reach the terminal.
- The tab bar's buttons are characters in the system font: `+`, `⋮` and `×` (the multiplication
  sign, which Roboto draws like its `+`). `✕` (U+2715) is not in Roboto, and the fallback font's
  glyph is larger and thinner than the others.
- Settings and the licenses leave `uiMode` out of their `configChanges` so that a dark-mode
  change recreates them with the other colours; an AppCompat activity that handles `uiMode`
  itself would keep the views it already made in the old ones.

## Confirmed bugs / technical debt

- **The terminal lacks** the bell, triple-tap (line) or rectangular selection, and handles that
  flip at the screen edges (a selection starting at column 0 has its start handle mostly
  off-screen). libghostty-vt has the two selections (`ghostty_terminal_select_line`, a
  selection's `rectangle` flag).
- **libghostty-vt features not wired**: no callbacks for OSC 52 clipboard writes, OSC 9/777
  notifications, OSC 9;4 progress, OSC 7501 program status, OSC 7 working directories or OSC 133
  prompts (the last two also need shell integration, which Ghostty ships for bash, zsh, fish,
  elvish and nushell but not ash, and Appine installs none); no search; no scrollback compression
  (`TerminalService.trimMemory` trims only the kernel); tabs are restored from plain text, not
  libghostty-vt snapshots; no terminfo name is set, so XTGETTCAP `TN` goes unanswered; and
  `VtView` ignores the frame header's `cursor_blinking`, so the cursor never blinks.
- **More of libghostty-vt left unused**:
  - selection: `select_word_between` (a word drag that extends by words), `selection_adjust`,
    `select_output` (needs OSC 133), and the selection gesture state machine (click counts to
    cell, word, line or output selection), for which `VtView` has its own taps and long presses;
  - drawing: overscan (rows captured beyond the viewport, to scroll by pixels; drags and flings
    move whole rows); the default palette, colour and cursor style/blink options (the 256 colours
    are libghostty-vt's built-in ones, and the colour scheme reply is fixed to dark); the OSC 22
    pointer shape for a mouse; `row_iterator_next_dirty` and the two-phase render update
    (`begin_update` under the lock, `end_update` after), so `frame` holds the tab's lock while
    it encodes every row;
  - the rest: no action to reset the terminal or clear the scrollback (`ghostty_terminal_reset`)
    and no RIS callback; transcripts are plain text, though the formatter can emit VT or HTML with
    colours and OSC 8 links; the memory usage is never read; the library's log and
    unknown-sequence callbacks are unset, so its warnings never reach logcat; KAM (mode 2) is
    ignored: Ghostty's own surface drops keys while it is set, but the key encoder does not, and
    neither does vt.c;
  - out of reach: the C API has no setter for the password-input flag (Ghostty's lock cursor) or
    the terminal's visibility, so mode 2033 reports every tab as visible, background ones too.
- **A kill report after a program died inside an OSC, DCS or APC string** is partly lost:
  `appine_vt_message` writes into the stream wherever its parser stands, and the line feeds that
  make the report's room are ignored in an OSC and taken as payload in a DCS or APC (the escape
  after them ends the string, so the rest is drawn). No room is scrolled open, so the inserted
  lines push the cursor's line down by the report's height (off the screen when it was the bottom
  row) while the cursor goes back to its old row, now another line; after a partial UTF-8
  sequence, a U+FFFD is printed first.
  `GHOSTTY_TERMINAL_DATA_VT_GROUND` tells when the stream is between sequences, and a CAN (0x18)
  returns the parser to ground from any state. Read from the parser, not reproduced.
- **A display size change while the terminal is open** keeps the sizes its views were built with
  (tab bar, extra keys, the terminal's font in pixels): `MainActivity` handles `density` itself and
  builds its views once in `onCreate`, so the new sizes apply only once the activity is created
  again.

## Open questions

- Should a Nerd Font or a CJK fallback font be bundled?

## To verify

- Opening links on real devices and with other browsers, mail apps, viewers and file managers;
  tried only on a Pixel 4a API 34 emulator with Chrome, HTML Viewer and the Files app, with taps and
  long presses from adb (http, https and `file`, not `mailto`).
- Which IMEs on real devices deliver Enter as text; the cutout insets and the keys following the
  IME on real devices and other IMEs; tried only with Gboard on a Pixel 4a API 34 emulator and the
  outer display of a Pixel 10 Pro Fold API 37 emulator (landscape both ways).
- The tab bar's 32 dp tabs and buttons (below the 48 dp touch target) on real phones, tablets and
  foldables; tried only on a Pixel 4a API 34 emulator, upright and on its side (the closing and
  switching animations upright only, slowed down). The rolling numbers' blur was seen only there,
  not on Android 11 or real GPUs.
- The terminal on real devices and 16 KB-page devices, and with CJK IMEs' composing text and
  floating candidate windows (`CursorAnchorInfo`); tried only on a 4 KB-page API 34 emulator with
  Gboard in English. Flings and the scroll bar were tried there with adb swipes only (viewport and
  alternate-scroll arrows; wheel events to a mouse-tracking program not tried), not with a finger.
- What programs that probe for Kitty graphics or the Glyph Protocol (yazi, chafa, timg,
  `kitten icat`) fall back to in the app; that both queries go unanswered with vt.c's options,
  also after a full reset and on the alternate screen, was checked on a macOS build of
  libghostty-vt 1.4.0, not on a device.
