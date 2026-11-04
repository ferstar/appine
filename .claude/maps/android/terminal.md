# android/terminal Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/android/terminal.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 1 research concern; 6 claims checked by 2 independent verifiers — 5 confirmed and 1 partial; bugs and debts recorded)
>
> Maintained: 2026-11-04 (targeted verification: added the experimental libghostty-vt terminal in the debug source set and its emulator shim and JNI in `hosts/android`; its tabs now name their kernel by handle; its input: selection and copy, paste, mouse, focus, IME and Kitty releases; libghostty-vt tabs in `TerminalService`, `VtActivity` removed; the Termux emulator removed, libghostty-vt the only terminal; the tab bar by window width, a row of tabs or a switcher with a list of tabs, and closing tabs from a menu or the list; swiping over the switcher, renaming tabs, the tab shortcuts of a hardware keyboard; the extra keys in Termux's two-row layout with held keys repeating, a swipe-up popup and locked modifiers; glyphs in wide cells enlarged evenly to fill them, a setting in Settings)

## Responsibilities

`android/terminal` is the terminal screen and the pieces that render the service's sessions:

- `MainActivity`: the tab bar, the terminal view, font zoom, extra keys, insets and the IME;
- the terminal: `VtSession` (a tab: its shell, the terminal, pump, waiter and writer threads),
  `VtView` (drawing with the bundled JetBrains Mono, input, selection), and the emulator shim and
  JNI in the native library (`vt.c`, `vt.rs`) around Ghostty's libghostty-vt;
- `ConsolePty`: the tab's kernel console (`/dev/pts/N`) and login shell;
- `LicensesActivity`: the notices generated at build time;
- the app's strings and themes.

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
  while open (`ListPopupWindow` over the `TabList` adapter); `Host : VtView.Host`,
  `Keys : ExtraKeysView.Target`.
- `ExtraKeysView` — Termux's default layout, two rows of seven (ESC, `/`, `-`, HOME, ↑, END, PGUP;
  TAB, CTRL, ALT, ←, ↓, →, PGDN); keeps CTRL and ALT (one-shot or locked) and sends keys and text
  to a `Target`; `Press` handles the touch on one key.
- `ConsolePty` — `start` (open console, spawn `login.sh`), `waitFor`, `close`; `console` is −1
  until started.
- `LicensesActivity` — list from `assets/notices/index.tsv`, full text view.

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

- Launcher activity `MainActivity` (`singleTask`, `adjustResize`, handles most configuration
  changes itself); listener callbacks `onMachineReady`, `onProgress`, `onSessionsChanged`,
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
  program set no title), grey with "(ended)" once the shell ended. The bar ends with `+` (opens a
  session while the kernel runs) and `⋮` (Settings); its other part depends on the window's width,
  checked again on every configuration change:
  - at least 600 dp: a scrolling row of all tabs, each at most 220 dp wide with an ellipsis, the
    current one white and scrolled into view after each change; tap switches;
  - narrower (a phone held upright): the switcher, with the number of tabs in an outlined box and
    the current tab's label; tapping it opens the list of tabs below the bar: one row per tab, the
    current one highlighted, each with a ✕ that closes it; tapping a row switches and closes the
    list. The list is modal and leaves the IME up, follows tab and title changes while open, and
    is closed on configuration changes, when the activity stops, when the tab bar switches to the
    row and when no tabs are left. A horizontal swipe of at least 48 dp over the switcher shows
    the next tab (to the left) or the previous one (to the right), without going round; the title
    follows the finger, with more resistance where there is no tab.
- A long press on a tab in the row, on the switcher (the current tab) or on a row of the list
  opens a menu: "Rename tab" (closes the list; a dialog with the name selected and the program's
  title as hint; Enter confirms; an empty name shows the program's title again;
  `TerminalService.renameSession`) and "Close tab".
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
- Extra keys: a key acts when the finger lifts (with a key-click haptic), not at all if the finger
  left it by more than the touch slop, and is shaded while pressed. Held past the long-press
  timeout, the arrows, PGUP and PGDN repeat every 80 ms (no act on release then); swiping up from
  `-` shows `|` in a popup right above it, sent on release (sliding back down sends `-`). CTRL and
  ALT: a tap turns them on for the next key (cyan) or off; a long press locks them on (cyan,
  underlined) until tapped, or turns them off. Keys go to `VtView.press` and `type`, which read the
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

## Dependencies (inbound / outbound)

### Inbound

- `TerminalService` (creates and holds the `VtSession`s: `showMessage`, `setRestored`,
  `transcriptText`, `name`, `release`; listener callbacks); `SettingsActivity` (licenses).

### Outbound

- `Native` (`openConsole`, `spawn`, `write`, `closeConsole`, `waitFor`), hence the kernel's host
  console; `login.sh`; `assets/notices`.
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

## Confirmed bugs / technical debt

- **The terminal lacks** the bell, mouse drags and motion for programs (only clicks and the wheel
  are sent), double- and triple-tap or rectangular selection, and handles that flip at the screen
  edges (a selection starting at column 0 has its start handle mostly off-screen).

## Open questions

- Should a Nerd Font or a CJK fallback font be bundled?

## To verify

- Which IMEs on real devices deliver Enter as text; edge-to-edge insets on display-cutout devices
  (the root ignores cutout insets).
- The terminal on real devices and 16 KB-page devices, and with CJK IMEs' composing text and
  floating candidate windows (`CursorAnchorInfo`); tried only on a 4 KB-page API 34 emulator with
  Gboard in English.
