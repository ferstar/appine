# android/terminal Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/android/terminal.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 1 research concern; 6 claims checked by 2 independent verifiers — 5 confirmed and 1 partial; bugs and debts recorded)
>
> Maintained: 2026-11-05 (targeted verification: added the experimental libghostty-vt terminal in the debug source set and its emulator shim and JNI in `hosts/android`; its tabs now name their kernel by handle; its input: selection and copy, paste, mouse, focus, IME and Kitty releases; libghostty-vt tabs in `TerminalService`, `VtActivity` removed; the Termux emulator removed, libghostty-vt the only terminal; the tab bar by window width, a row of tabs or a switcher with a list of tabs, and closing tabs from a menu or the list; swiping over the switcher, renaming tabs, the tab shortcuts of a hardware keyboard; the extra keys in Termux's two-row layout with held keys repeating, a swipe-up popup and locked modifiers; glyphs in wide cells enlarged evenly to fill them, a setting in Settings; Material 3 themes and page layouts for Settings and the licenses; the terminal's dialogs, menus and list of tabs in Material 3, always dark; the tab bar and extra keys in the theme's colours; the extra keys in two rows of nine, the first eight columns scrolling below 360 dp, 1 px lines between the keys; the tab bar, extra keys and system-bar areas black)

## Responsibilities

`android/terminal` is the terminal screen and the pieces that render the service's sessions:

- `MainActivity`: the tab bar, the terminal view, font zoom, extra keys, insets and the IME;
- the terminal: `VtSession` (a tab: its shell, the terminal, pump, waiter and writer threads),
  `VtView` (drawing with the bundled JetBrains Mono, input, selection), and the emulator shim and
  JNI in the native library (`vt.c`, `vt.rs`) around Ghostty's libghostty-vt;
- `ConsolePty`: the tab's kernel console (`/dev/pts/N`) and login shell;
- `LicensesActivity`: the notices generated at build time;
- the app's strings and themes: `Theme.Appine` (Material 3 DayNight, for Settings and the
  licenses) and `Theme.Appine.Terminal` (Material 3, always dark, with a black window and
  system bars, for `MainActivity`), and the Material 3 page layouts that Settings and the licenses share
  (`page.xml`, `list_item.xml`, `list_header.xml`).

Root: `android/app/src/main/java/io/github/mthli/appine/MainActivity.kt`, `VtSession.kt`,
`VtView.kt`, `VtGlyphs.kt`, `VtKeys.kt`, `VtNative.kt`, `ConsolePty.kt`, `ExtraKeysView.kt`,
`LicensesActivity.kt`, `android/app/src/main/res/`, `android/app/src/main/assets/fonts/`,
`hosts/android/src/vt.c` and `hosts/android/src/vt.rs`.

Outside the module: session lifecycle, saving and kill routing (`android/service`); the host console
in the kernel (`kernel/tty`); the notices task, the JNI crate and the build, including compiling
libghostty-vt with Zig (`android/native`).

## Key types

- `MainActivity` — a `TerminalService.Listener`; one `VtView`; the tab bar (a scrolling row of
  tabs and a switcher with its `Swipe` touch listener, one of them shown) and the list of tabs
  while open (AppCompat's `ListPopupWindow` over the `TabList` adapter, rows from
  `list_tab.xml`); `Host : VtView.Host`,
  `Keys : ExtraKeysView.Target`.
- `ExtraKeysView` — Termux's default keys widened to two rows of nine (ESC, `/`, `-`, HOME, ↑, END,
  `|`, `~`, PGUP; TAB, CTRL, ALT, ←, ↓, →, `` ` ``, DEL, PGDN): the first eight columns in a
  `HorizontalScrollView`, the last (PGUP, PGDN) fixed beside it; keeps CTRL and ALT (one-shot or
  locked) and sends keys and text to a `Target`; `Press` handles the touch on one key.
- `ConsolePty` — `start` (open console, spawn `login.sh`), `waitFor`, `close`; `console` is −1
  until started.
- `LicensesActivity` — an `AppCompatActivity` on the Material 3 page: list from
  `assets/notices/index.tsv` (one `list_item.xml` row per entry), full text view; an
  `OnBackPressedCallback`, enabled while a full text is shown, returns to the list.

### The terminal

- `VtSession` — a tab owned by the service: its `ConsolePty`, the terminal handle (0 until the
  shell starts), the name the user gave it (`name`, null when none), the view showing it (`view`,
  read on the pump thread), pending restored text and
  messages, a `vt-pump` thread, a `vt-waiter` thread and its own single-thread `vt-writer`
  executor; `Events` (title changed, finished) go to the service.
- `VtView` — the view: the attached `VtSession`, cell metrics, a mirror of the screen (codepoint,
  colours, attributes, grapheme clusters per cell, each row's selected columns), a backing bitmap
  of the grid, the modes and selection bounds of the last frame, the IME connection, gestures,
  selection handles and the floating menu. `Host` (the activity): `onGrid`, `onInput`, `onZoom`,
  the extra keys' `ctrl`, `alt` and `modifiersUsed`.
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
  `copy`, `title`, `message`, `transcript`, `print`; `pump` calls `VtSession.onOutput` and
  `onTitle` back.

## Data flow / lifecycle

### Binding and tabs

- `onStart` starts and binds the service; `onServiceConnected` registers the listener and replays
  ready/error/progress and pending notices; `onStop` unregisters and unbinds.
- Tabs are labelled `N: name` with the name the user gave them, else `N: title` (`sh` when the
  program set no title), muted with "(ended)" once the shell ended. The bar ends with `+` (opens a
  session while the kernel runs) and `⋮` (Settings); its other part depends on the window's width,
  checked again on every configuration change:
  - at least 600 dp: a scrolling row of all tabs, each at most 220 dp wide with an ellipsis, the
    current one in the primary colour and scrolled into view after each change; tap switches;
  - narrower (a phone held upright): the switcher, with the number of tabs in an outlined box and
    the current tab's label; tapping it opens the list of tabs below the bar: one row per tab, the
    current one highlighted, each with a ✕ that closes it; tapping a row switches and closes the
    list. The list is modal and leaves the IME up, follows tab and title changes while open, and
    is closed on configuration changes, when the activity stops, when the tab bar switches to the
    row and when no tabs are left. A horizontal swipe of at least 48 dp over the switcher shows
    the next tab (to the left) or the previous one (to the right), without going round; the title
    follows the finger, with more resistance where there is no tab.
- A long press on a tab in the row, on the switcher (the current tab) or on a row of the list
  opens a menu: "Rename tab" (closes the list; a dialog with an outlined text field,
  `dialog_rename.xml`, holding the name selected, with the program's title as its placeholder and
  the rule as its helper text; Enter confirms; an empty name shows the program's title again;
  `TerminalService.renameSession`) and "Close tab".
- Its dialogs (rename, notices, the paste confirmation), its menus (⋮, the tab menu) and the list
  of tabs are Material 3 in the theme's dark colours, from the wallpaper on Android 12 and later
  (`DynamicColors`); the list highlights the current tab in the secondary container colour. The
  tab bar, the extra keys and what lies under the system bars are black like the terminal; their
  labels, buttons and the status text take the muted on-surface-variant colour, and the title of
  the tab shown the on-surface one.
- Hardware-keyboard shortcuts, taken by the activity (`dispatchKeyEvent`) before the terminal sees
  them, together with the matching key release: Ctrl+Shift+T opens a tab, Ctrl+Shift+W closes the
  current one (both once per press), Ctrl+Tab and Ctrl+Shift+Tab show the next and the previous
  one, going round and repeating while held. They are also listed in the system's keyboard
  shortcuts screen (Meta+/).
- Closing a tab hangs it up at once. Closing the last tab while the kernel runs finishes the
  activity (the service stays). After a reconnect the focused tab is the service's `focus` or the
  last one.
- Showing a tab attaches it to the view (`VtView.attach`); `onRestarting` and `onDestroy` detach
  it, so sessions never keep a destroyed Activity's view.

### Input and display

- The font size (pixels) is kept in the activity's preferences.
- Whether glyphs in wide cells fill them is a setting kept by the service
  (`TerminalService.fillWide`, on by default, switched in Settings); the activity applies it to
  the view in `onStart` and `onSettingsChanged`, and a change repaints the whole screen.
- Extra keys: rows of 32 dp with 12 sp labels; the nine columns share the width, each at least
  40 dp (the last one takes the pixels left over), so they all fit from 360 dp on. Narrower, the
  first eight columns of both rows scroll sideways together under the finger, while the last stays
  at the right edge; a sideways slide past the touch slop then scrolls and cancels the key it
  started on, unless that key is already held or showing its popup. With nothing to scroll the
  keys keep every touch. Lines one pixel wide in the outline variant colour run above each row, below
  the last one and between the columns, drawn over the keys (a pressed key's shading leaves them visible); those
  of the first eight columns scroll with them.
- A key acts when the finger lifts (with a key-click haptic), not at all if the finger
  left it by more than the touch slop, and is shaded while pressed (the highest surface container
  colour, as is the popup). Held past the long-press
  timeout, the arrows, PGUP, PGDN and DEL (forward delete) repeat every 80 ms (no act on release
  then); swiping up from
  `-` shows `|` in a popup right above it, sent on release (sliding back down sends `-`). CTRL and
  ALT: a tap turns them on for the next key (primary colour) or off; a long press locks them on
  (primary colour, underlined) until tapped, or turns them off. Keys go to `VtView.press` and `type`, which read the
  modifiers through the host and report when a key took them; that clears one-shot modifiers and
  leaves locked ones on.
- The content is edge-to-edge with the root padded by system-bar and IME insets.

### Tabs

#### Start and end

- A `VtSession` starts its shell the first time a view showing it knows the grid (`VtView.attach`
  and every regrid call `Host.onGrid`, which starts the current tab once): `ConsolePty.start`
  opens a console of the grid's size and spawns the login; `VtNative.create` makes the terminal
  (scrollback 10,000 lines; grapheme clustering, mode 2027, on by default as in Ghostty); restored
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
  focus events 1004, a selection exists) and the selection's first and last cells in the
  viewport; each row carries its selected columns.
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
  character without Ctrl or Alt are sent as text. CTRL and ALT on the extra keys apply to the
  next key or one typed character (turned back into a key event), or to every one while locked.
  The extra keys send presses only (a held key repeats as new presses), never releases.
  Ctrl+Shift+C and V and the dedicated copy and paste keys copy and paste.
- Typed and pasted input ends a selection; all input first scrolls the viewport back to the bottom,
  then goes through the host to the tab's `vt-writer` thread (`Native.write`).
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

- A tap ends a selection or its menu; otherwise it opens the IME and, when the program tracks the
  mouse, also sends a left click (press and release at the tap's pixel position, in the program's
  format).
- A vertical drag, or a mouse wheel (3 rows a notch), scrolls by whole rows: wheel events when the
  program tracks the mouse, arrow keys on the alternate screen with alternate scroll (1007, on by
  default), else the viewport.
- A long press selects the word under it (`ghostty_terminal_select_word`, installed as the
  terminal's selection) and keeps it as the anchor: moving on extends the selection from the far
  side of the word to the cell under the finger. On blank cells it selects nothing and shows a menu
  with Paste and Select all only. Long press selects even when the program tracks the mouse.
- The two handles (start below the first cell, end below the last, when in the viewport) drag the
  selection's ends; the other end is the anchor, kept as tracked grid references so output does not
  move it. The finger keeps its offset from the dragged cell; held above or below the grid, the
  viewport scrolls one row every 50 ms and the selection follows.
- After a drag, a floating action mode (`TYPE_FLOATING`) offers Copy (with a selection), Paste
  (with a clipboard), Select all (scrollback included), with the system's own labels, placed over
  the selected rows. Copy takes the selection as plain text with soft wraps joined and trailing
  blanks trimmed (as Ghostty copies), puts it on the clipboard and ends the selection.
- Pinch zoom changes the font by 1 dp beyond a 0.9–1.1 scale band, within 6–32 dp, which regrids
  and resizes the terminal and then the console (SIGWINCH).

### Licenses

- The build writes `notices/` (appine's licence; Ghostty's libghostty-vt, uucode, simdutf,
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
  `transcriptText`, `name`, `release`; listener callbacks); `SettingsActivity` (licenses).

### Outbound

- `Native` (`openConsole`, `spawn`, `write`, `closeConsole`, `waitFor`), hence the kernel's host
  console; `login.sh`; `assets/notices`.
- Material Components 1.14.0 with AppCompat and androidx.activity (both themes, `LicensesActivity`
  and the page layouts, `MainActivity`'s dialogs, menus and list of tabs, `VtView`'s paste
  confirmation).
- libghostty-vt 1.4.0 (the terminal state, render state, key and mouse encoders, paste, focus
  encoding, selection, plain-text formatting, title and width), linked into `libappine.so`
  (`android/native`); `Machine::read_console`, `write_console` and `resize_console` called from
  `vt.rs` directly; the system clipboard; JetBrains Mono 2.304 (variable, upright and italic) in
  the assets, and the system's fallback fonts for everything else (CJK from Noto Sans CJK on AOSP
  images); the wide-cell setting `TerminalService.fillWide`.

## Invariants and gotchas

- The console opens lazily, so restored tabs that were never shown have no console and no shell.
- `openConsole`, `spawn`, `resize` and `closeConsole` run on the main thread and take the kernel's
  BKL; input is written on each tab's own thread, so a program that stops reading blocks only its
  tab.
- Console numbers are freed at close and reused lowest-first by the kernel, and a close carries no
  generation, so closing a stale number closes whichever console now has it.
- Programs never receive Ctrl+Shift+T, Ctrl+Shift+W, Ctrl+Tab or Ctrl+Shift+Tab from a hardware
  keyboard (the tab shortcuts); the release of the shortcut's key is swallowed too, so the Kitty
  protocol sees no release without a press.
- A `HorizontalScrollView` takes any sideways slide past the touch slop even when it has nothing
  to scroll, cancelling the child's touch; the extra keys' scroll view intercepts only when it can
  scroll, and a held key or one showing its popup asks its parents not to intercept.
- The column width is set on the keys' layout params during `ExtraKeysView.onMeasure`; the rows
  and the grid inside the scroll view are measured without a width limit, so they are forced to
  measure again whenever the column width changes.

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
- With grapheme clustering on, programs that count widths per codepoint (`wcwidth`) disagree on
  emoji sequences; they can turn mode 2027 off.
- Grid references from libghostty-vt are valid only until the terminal changes, so every selection
  call takes fresh ones; the anchor of a drag is tracked. A changed selection makes the next frame a
  full one (the render state redraws everything when the screen's selection changes).
- With the IME shown, a hardware Esc press is taken by the IME (Gboard hides itself), so a program
  using the Kitty protocol's release events sees only the release.
- `VtView` turns off the default focus highlight: focused outside touch mode (after hardware or
  injected keys) the system would cover the whole view with a translucent grey.
- `MainActivity` is a framework `Activity` on a Material theme: Material dialogs and AppCompat's
  `PopupMenu` and `ListPopupWindow` take the theme, but the views it inflates from XML are the
  framework's (no AppCompat inflater), so AppCompat view attributes such as `app:tint` do nothing
  there (`ic_close` carries its own tint). The text selection handles `VtView` draws and the
  floating text menu read framework attributes, which AppCompat forwards to the Material 3
  colours (the handles are the primary colour). Theme.Material3 does not set the system bars'
  colours, which Android applies up to version 14, so `Theme.Appine.Terminal` sets them black,
  as the framework theme had them.
- Settings and the licenses leave `uiMode` out of their `configChanges` so that a dark-mode
  change recreates them with the other colours; an AppCompat activity that handles `uiMode`
  itself would keep the views it already made in the old ones.

## Confirmed bugs / technical debt

- **The terminal lacks** the bell, mouse drags and motion for programs (only clicks and the wheel
  are sent), double- and triple-tap or rectangular selection, and handles that flip at the screen
  edges (a selection starting at column 0 has its start handle mostly off-screen).
- **A display size change while the terminal is open** keeps the sizes its views were built with
  (tab bar, extra keys, the terminal's font in pixels): `MainActivity` handles `density` itself and
  builds its views once in `onCreate`, so the new sizes apply only once the activity is created
  again.

## Open questions

- Should a Nerd Font or a CJK fallback font be bundled?

## To verify

- Which IMEs on real devices deliver Enter as text; edge-to-edge insets on display-cutout devices
  (the root ignores cutout insets).
- The terminal on real devices and 16 KB-page devices, and with CJK IMEs' composing text and
  floating candidate windows (`CursorAnchorInfo`); tried only on a 4 KB-page API 34 emulator with
  Gboard in English.
