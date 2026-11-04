# android/terminal Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/android/terminal.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 1 research concern; 6 claims checked by 2 independent verifiers — 5 confirmed and 1 partial; bugs and debts recorded)
>
> Maintained: 2026-11-04 (targeted verification: added the experimental libghostty-vt terminal in the debug source set and its emulator shim and JNI in `hosts/android`; its tabs now name their kernel by handle; its input: selection and copy, paste, mouse, focus, IME and Kitty releases)

## Responsibilities

`android/terminal` is the terminal screen and the pieces that render the service's sessions:

- `MainActivity`: the tab bar, the terminal view, font zoom, extra keys, insets and the IME;
- `ConsolePty`: the adapter between a Termux `TerminalSession` and a kernel console
  (`/dev/pts/N`);
- the vendored Termux `terminal-emulator` and `terminal-view`, with appine's changes to
  `TerminalSession`;
- `LicensesActivity`: the notices generated at build time;
- the app's strings and themes;
- the experimental terminal (debug builds only): `VtActivity` with one login shell shown by
  `VtView`, which draws a libghostty-vt terminal held in the native library (`vt.c`, `vt.rs`) with
  bundled JetBrains Mono; a prototype for replacing the Termux view, not a tab of the service.

Root: `android/app/src/main/java/io/github/mthli/appine/MainActivity.kt`, `ConsolePty.kt`,
`ExtraKeysView.kt`, `LicensesActivity.kt`, `android/terminal-emulator/`, `android/terminal-view/`,
`android/app/src/main/res/`; for the experimental terminal, `VtActivity.kt`, `VtView.kt`,
`VtGlyphs.kt`, `VtKeys.kt`, `VtNative.kt` in `android/app/src/debug/java/io/github/mthli/appine/`,
`android/app/src/debug/assets/fonts/`, `android/app/src/debug/res/values*/`,
`hosts/android/src/vt.c` and `hosts/android/src/vt.rs`.

Outside the module: session lifecycle, saving and kill routing (`android/service`); the host console
in the kernel (`kernel/tty`); the notices task, the JNI crate and the build, including the `vt`
feature that compiles libghostty-vt (`android/native`).

## Key types

- `MainActivity` — a `TerminalService.Listener`; `ViewClient : TerminalViewClient`.
- `ExtraKeysView` — one row: ESC, TAB, CTRL, ALT, `-`, `/`, `|`, arrows.
- `ConsolePty : TerminalSession.Pty` — `start` (open console, spawn `login.sh`), `read`, `write`,
  `resize`, `close`; `console` is −1 until started.
- `TerminalSession` (vendored, rewritten) — takes a `Pty` instead of creating a subprocess; adds
  `setRestored`, `showMessage`, `getTranscriptText`.
- `LicensesActivity` — list from `assets/notices/index.tsv`, full text view.

### Experimental terminal

- `VtActivity` — a `TerminalService.Listener` and `VtView.Host`: its own extra-keys row, one
  `ConsolePty` (used only to open the console, spawn the login, wait and close), one tab handle, a
  `vt-pump` thread and a single-thread `vt-writer` executor.
- `VtView` — the view: cell metrics, a mirror of the screen (codepoint, colours, attributes,
  grapheme clusters per cell, each row's selected columns), a backing bitmap of the grid, the modes
  and selection bounds of the last frame, the IME connection, gestures, selection handles and the
  floating menu; `Host.onGrid` / `Host.onInput` go to the activity.
- `VtGlyphs` — box drawing (U+2500–U+257F), block elements (U+2580–U+259F) and the Powerline
  triangles (U+E0B0–U+E0B3) drawn as shapes.
- `VtKeys` — Android key codes to libghostty-vt's `GhosttyKey` numbers, modifier bits, actions.
- `VtNative` — `@JvmStatic external` functions on the native library; `available` probes for them.
- In `vt.rs`: `Tab` (the kernel's handle, console number, the terminal under a mutex, a `dirty`
  flag) in a table `TABS` keyed by handles from 1, never reused. In `vt.c`: `AppineVt` (the
  terminal, its render state, key and mouse encoders, reply, frame and copy buffers, the
  synchronized-output hold, and the tracked anchor of a selection drag).

## Public entry points

- Launcher activity `MainActivity` (`singleTask`, `adjustResize`, handles most configuration
  changes itself); listener callbacks `onMachineReady`, `onProgress`, `onSessionsChanged`,
  `onTextChanged`, `onExitAll`, `onRestarting`, `onNotice`.
- `LicensesActivity`, opened from Settings.
- `VtActivity` (debug manifest, not exported), opened from the main screen's `⋮` menu item "New
  terminal (experimental)", which exists only when `BuildConfig.DEBUG`; without the `vt` feature
  in the native library it shows a toast and finishes.
- `VtNative`: `create`, `destroy`, `pump`, `frame`, `resize`, `scroll`, `key`, `paste`, `mouse`,
  `focus`, `selectWord`, `selectGrab`, `selectTo`, `selectAll`, `selectClear`, `copy`, `print`.

## Data flow / lifecycle

### Binding and tabs

- `onStart` starts and binds the service; `onServiceConnected` registers the listener and replays
  ready/error/progress and pending notices; `onStop` unregisters and unbinds.
- Tab bar: `N: title` (`sh` by default, ` ✕` once the shell ended); tap switches, long-press closes
  at once (hang-up), `+` opens a session while the kernel runs, `⋮` has Settings (and the
  experimental terminal in debug builds). Closing the last
  tab while the kernel runs finishes the activity (the service stays). After a reconnect the focused
  tab is the service's `focus` or the last one.

### A tab's console

- When the view first knows its size, `initializeEmulator` calls `Pty.start`: open a console with
  that size and spawn `/bin/sh -c <login.sh> appine-login <login> <cwd>` with a fixed environment.
  Restored transcripts (`setRestored`) are written before the shell starts.
- `ConsolePty` uses a reader thread (`Native.read`, end at ≤ 0), a writer thread and a waiter
  thread (`waitFor`, then up to 0.5 s for the reader to drain, since background jobs may keep the
  slave open); `resize` runs on the main thread. After exit the session shows "[Process completed
  … - press Enter]" and Enter (key event) closes the tab.
- `showMessage` (kill reports) inserts a yellow line above the cursor line: newline and back,
  save cursor, insert lines, write, restore cursor and move down; skipped on the alternate screen
  and queued until the emulator exists.

### Input and display

- Pinch zoom changes the font by 1 dp steps beyond a 0.9–1.1 scale band, within 6–32 dp, stored as
  pixels in the activity's preferences.
- Extra keys: CTRL and ALT are one-shot toggles consumed when the next key is read; special keys go
  through the view's key-code path (cursor application mode applies), characters through code-point
  input.
- A tap opens the IME; the content is edge-to-edge with the root padded by system-bar and IME
  insets; long-press selects text; copy and paste go through the service.

### Experimental terminal

#### Start and end

- `VtActivity` binds the service (it does not start it) and starts once both the kernel and the
  grid are known: `ConsolePty.start` opens a console of the grid's size and spawns the same login as
  a tab; `VtNative.create` makes the libghostty-vt terminal for it (scrollback 10,000 lines;
  grapheme clustering, mode 2027, on by default as in Ghostty) and the `vt-pump` thread runs
  `VtNative.pump`.
- The pump reads the console into a 64 KB buffer, feeds the terminal under the tab's lock, writes
  the terminal's replies (DA, DSR, XTVERSION, size reports) back to the console before reading
  again, and calls `view.onOutput()` only when it sets `dirty`, so a burst asks for one frame. It
  ends at EOF or a console error; the thread then waits for the shell and prints "[Process completed
  (code)]" locally through `VtNative.print`.
- `onDestroy` closes the console (ending the pump) and destroys the tab; the terminal is freed when
  the last `Arc<Tab>` (the table's or a running pump's) goes.

#### Frames and drawing

- `onOutput` posts an invalidate on animation; `onDraw` takes a frame (`VtNative.frame`, main thread
  only) and applies it: `frame` clears `dirty`, updates the render state (unless a
  synchronized-output hold is active), and writes the header plus the dirty rows (all rows when the
  view asks for a full frame) in the format documented in `vt.c`; the direct `ByteBuffer` is valid
  until the next `frame`.
- A hold (mode 2026) captures the screen when it begins and is ended by force after 1 s.
- The view asks for a full frame after a regrid, a new bitmap or new default colours; a frame
  captured at an old size is dropped and a full one requested.
- The header also carries mode flags (mouse tracking, alternate screen, alternate scroll 1007,
  focus events 1004, a selection exists) and the selection's first and last cells in the
  viewport; each row carries its selected columns.
- Cell metrics: the width is the font's advance rounded to whole pixels and the text size is
  adjusted so the advance equals it; the height is the ceiling of ascent+descent, with the baseline
  centred; the light line width is the font's underline thickness.
- Each dirty row is repainted into the bitmap under a clip: default background, runs of other
  backgrounds, the selection's translucent highlight, then text. Plain ASCII runs of one paint,
  colour and decoration go out in one draw call; other glyphs are centred in their one or two
  cells, or shrunk evenly when wider (never stretched); `VtGlyphs` characters are shapes; four
  paints (regular/bold × upright/italic) from the variable font with ligatures off. Underlines (single, double, curly, dotted, dashed),
  strikethrough and overline are drawn; inverse, faint and invisible apply; blink is ignored.
- The cursor (bar, block with the glyph redrawn in the background colour, underline, hollow; hollow
  when unfocused), the IME's composing text (shifted left at the right edge) and the theme's text
  selection handles are drawn over the bitmap each draw.

#### Input

- Keys: mapped keys go through `VtNative.key`, which encodes them in the terminal's current modes
  (cursor and keypad modes, Kitty keyboard protocol, including releases from `onKeyUp` and lone
  modifiers, which are sent only when the encoder emits something); unmapped keys that type a
  character without Ctrl or Alt are sent as text. CTRL and ALT on the extra-keys row are one-shot
  and also apply to one typed character (turned back into a key event). Ctrl+Shift+C and V and the
  dedicated copy and paste keys copy and paste.
- Typed and pasted input ends a selection; all input first scrolls the viewport back to the bottom,
  then goes to the `vt-writer` thread (`Native.write`).
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

- The build writes `notices/` (appine's licence, Termux and jackpal, Alpine packages from the
  rootfs database, the doas package, Android libraries, Rust crates, SQLite, then full texts);
  full text is monospace, selectable and sized to fit 74 columns.

## Dependencies (inbound / outbound)

### Inbound

- `TerminalService` (sessions, focus, listener callbacks, `showMessage`, `setRestored`,
  `getTranscriptText`); `SettingsActivity` (licenses).
- Experimental terminal: `MainActivity`'s menu (debug builds); `TerminalService` gives `machine`,
  `login` and listener callbacks.

### Outbound

- `Native` (`openConsole`, `spawn`, `read`, `write`, `resize`, `closeConsole`, `waitFor`), hence
  the kernel's host console; `login.sh`; `assets/notices`.
- Experimental terminal: libghostty-vt 1.4.0 (the terminal state, render state, key and mouse
  encoders, paste, focus encoding, selection and its plain-text formatting), linked into
  `libappine.so` by the `vt` feature (`android/native`); `Machine::read_console`,
  `write_console` and `resize_console` called from `vt.rs` directly; `ConsolePty` and
  `Native.write`; the system clipboard; JetBrains Mono (variable, upright and italic) from the
  debug assets.

## Invariants and gotchas

- The console opens lazily, so restored tabs that were never shown have no console and no shell.
- `openConsole`, `spawn`, `resize` and `closeConsole` run on the main thread and take the kernel's
  BKL (the `Pty` javadoc wrongly says everything but `start` runs on background threads).
- Typing blocks the main thread once Termux's 4 KB input queue is full (inherited).
- Console numbers are freed at close and reused lowest-first by the kernel, and a close carries no
  generation, so closing a stale number closes whichever console now has it.

### Experimental terminal

- A tab refers to its kernel by handle and looks it up for each call (each console read of the
  pump, each reply write, each resize), like the `Native` functions: `Native.stop` and `shutdown`
  only end the processes, and the store is closed when the last `Arc<Machine>` goes, so a tab
  holding one would keep the old store open after a reboot, tier change or distro switch, and the
  next kernel would start without the boot cleanup. The pump still holds the kernel during a read,
  which returns once the shutdown has ended the shell; the service does not wait for that, since
  `VtActivity` does not close its console on `onRestarting` as the service's own tabs are closed.
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
- The session is not a `TerminalService` tab: it is not saved or restored, kill reports do not reach
  it, and `VtActivity` ignores `onExitAll` and `onRestarting`, so after a reboot, tier change or
  exit it stays open with an ended session (a later `onMachineReady` does not start another).
- Zoom is not saved. Opening the app from the launcher brings the `singleTask` `MainActivity` to the
  front and so closes `VtActivity` above it.

## Confirmed bugs / technical debt

- **Closing a tab can hang up another tab.** A running tab's console is closed by `finishIfRunning`
  and again by `cleanupResources` once the old shell has exited (plus up to 0.5 s; unbounded if it
  ignores SIGHUP); a tab opened or first shown in between (a restored tab revealed by the close, or
  "+") can get the same number and is hung up by the second close.
- **Special extra keys crash without a session.** The extra-keys row stays tappable during startup,
  progress and a failed first start; ESC, TAB and the arrows reach `TerminalView.handleKeyCode`,
  which dereferences the null session (character keys are guarded).
- **A console leaks when spawning the shell fails.** `Pty.start` opens the console first; if `spawn`
  throws the session is marked finished without closing it, and nothing closes it until the kernel
  stops. `VtActivity` leaks the same way and tries again on every later grid change (zoom,
  rotation, IME), leaking one more console each time.
- **The experimental terminal lacks** titles and the bell, tabs, mouse drags and motion for
  programs (only clicks and the wheel are sent), double- and triple-tap or rectangular selection,
  and handles that flip at the screen edges (a selection starting at column 0 has its start
  handle mostly off-screen); the plan is tabs in `TerminalService` next (save and restore, kill
  reports), then removing the Termux view.
- **Tab saves during full-screen programs lose the scrollback.** `getTranscriptText` reads the
  current screen, which is the alternate buffer (no scrollback) while vim or less runs, and replaces
  the earlier save.
- **Enter from the IME does not close a finished tab.** Only a `KEYCODE_ENTER` key event closes it;
  keyboards that commit `\n` as text go to the dead session and are dropped.

## Open questions

- Should the extra keys be disabled until a session is attached?
- Should saved transcripts come from the main buffer rather than the alternate screen?
- Does the libghostty-vt view replace the Termux view (pending a check on a real device)? Should a
  Nerd Font or a CJK fallback font be bundled?

## To verify

- Which IMEs on real devices deliver Enter as text; edge-to-edge insets on display-cutout devices
  (the root ignores cutout insets).
- The experimental terminal on real devices and 16 KB-page devices, and with CJK IMEs' composing
  text and floating candidate windows (`CursorAnchorInfo`); tried only on a 4 KB-page API 34
  emulator with Gboard in English.
