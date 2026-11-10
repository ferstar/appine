# android/terminal Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/android/terminal.md` for the paired decision history.
>
> Verified: 2026-11-11 (full mapping: 5 research concerns; 18 claims checked by 3 independent verifiers — 11 confirmed and 7 partial, corrected below; no bugs recorded)

## Responsibilities

`android/terminal` is the terminal screen and the pieces that render the service's sessions:

- `MainActivity`: the screen's layout and status text, the current session and its successor,
  closing tabs, keyboard shortcuts, font zoom, insets and the IME, links, the question whether a
  program may read the clipboard, with the tab bar
  (`TabBarView`), the search bar in its place while searching (`SearchBarView`) and the extra keys
  (`ExtraKeysView`);
- the terminal: `VtSession` (a tab: its shell, the terminal, pump, waiter and writer threads,
  what its programs report about their progress and status, whether its shell waits at its prompt
  and in which directory, its terminal's snapshots and coming back from one, clipboard reads
  waiting for an answer, the commands its shell's prompt marks show finishing, clearing and
  resetting, the cursor's default from Settings, tmux's control mode ending), `VtView` (drawing
  with the bundled JetBrains Mono, the images programs show, input, selection by touch and by
  mouse, search marks, gestures and scrolling by pixels),
  `ProgressLine` (the current tab's progress over the terminal), `VtKeys`, `VtGlyphs`,
  `VtNative`, and the emulator shim and JNI in
  the native library (`vt.c`, `vt.rs`) around Ghostty's libghostty-vt, with the `vt-compress`
  thread that compresses idle tabs' scrollback;
- `ConsolePty`: the tab's kernel console (`/dev/pts/N`) and login shell;
- `LicensesActivity`: the notices generated at build time;
- the app's strings (English and Simplified Chinese) and themes: `Theme.Appine` (Material 3
  DayNight, for Settings and the licenses), `Theme.Appine.Terminal` (Material 3, always dark, with
  a black window and system bars, for `MainActivity`), the ⋮ menu's `Widget.Appine.MorePopup`, and
  the Material 3 page layouts (`page.xml` and `list_item.xml`, shared by Settings and the licenses;
  `list_header.xml`, Settings only).

Root: `android/app/src/main/java/io/github/mthli/appine/MainActivity.kt`, `VtSession.kt`,
`VtView.kt`, `VtGlyphs.kt`, `VtKeys.kt`, `VtNative.kt`, `ConsolePty.kt`, `TabBarView.kt`,
`SearchBarView.kt`, `ProgressLine.kt`, `ExtraKeysView.kt`, `LicensesActivity.kt`,
`android/app/src/main/res/`, `android/app/src/main/assets/fonts/`, `hosts/android/src/vt.c` and `hosts/android/src/vt.rs`.

Outside the module: session ownership, saving, kill reports and where bells, notifications, program
statuses, clipboard writes and clipboard reads go (`android/service`); the host console in the
kernel (`kernel/tty`); the notices task, the JNI crate, `guarded!` and the build, including
compiling libghostty-vt with Zig (`android/native`).

## Key types

- `MainActivity` — a `TerminalService.Listener` built in code: a black column of one bar's place
  (`TabBarView`, or `SearchBarView` while searching), a frame with the `VtView`, a status line and
  the `ProgressLine` along its top, and `ExtraKeysView`; adapters `Host : VtView.Host`,
  `Bar : TabBarView.Target`, `Find : SearchBarView.Target` and `Keys : ExtraKeysView.Target`; the
  current session and its index, from which its successor is taken; a tab requested by a
  notification (`EXTRA_TAB`); the IME animation callback `keysFollowIme`; Back's callback while
  the search bar is open (`closeSearchOnBack`, Android 16 and later); the open question whether a
  program may read the clipboard (`clipboardQuestion`).
- `SearchBarView` — the search bar, a row as tall as the tab bar: the text to find (an `EditText`
  without suggestions, its IME action Search), the count ("selected/total"), `↑`, `↓` and `×`; it
  shows the count it is given (`setCount`) and sends text changes, steps and closing to a
  `Target` (`search`, `step`, `close`).
- `TabBarView` — the tab bar (a scrolling row of numbered tabs, with `×`, `+` and `⋮` over its
  end): shows the sessions and the current one it is given (`render`) and sends taps on tabs and
  buttons, the ⋮ anchor and the titles long presses show to a `Target` (`show`, `closeCurrent`,
  `newTab`, `menu`, `notice`); it draws the other tabs' progress and status marks and redraws them
  on `refreshStatus`; `Tab` (one per session: its number in the row, which slides, rolls to a new
  number and fades out when closed); `TabNumber` (the number, a `TextView` drawing its digits
  itself so they can roll).
- `ExtraKeysView` — Termux's default keys widened to two rows of eight (ESC, `/`, `-`, HOME, ↑, END,
  `|`, SHIFT; TAB, CTRL, ALT, ←, ↓, →, `~`, SYM), with a second page of symbols for every key but
  SYM (`` ` ``, `/`, `-`, `=`, `^`, `\`, `|`, `%`; `[`, `]`, `{`, `}`, `<`, `>`, `~`: the ASCII
  symbols Gboard gives neither on its letters nor from a long press on one, with `/ - | ~` where
  they are on the first page): a `Page` is a grid of the first seven columns and the key above
  SYM; the grid shown is in a `HorizontalScrollView` across the view, the last column (SHIFT or `%`
  over SYM) fixed over its end. It keeps CTRL, ALT and SHIFT (one-shot or locked; `shift` reads
  false while the symbols are shown) and the page shown, exposes them with `modifiersUsed`, and
  sends keys, text and notices (a modifier locked) to a `Target`; `Press` handles the touch on one
  key. Its companion's `FADE_DP`, `indicatorPaint`, `drawIndicator` (the indicator under a label)
  and `indicatorBounds` (where it lies, where the tab row also draws a tab's progress) serve the tab
  row too.
- `ConsolePty` — `start` (open a console, spawn `login.sh` through `sh -c` with the tab's login
  and cwd and a fixed environment, `TERM=xterm-256color` among it), `waitFor`, `close`; `console`
  is −1 until started and keeps its number after `close` (`VtSession.consoleOpen` tells whether it
  is still open, for the routing of kill reports in android/service).
- `LicensesActivity` — an `AppCompatActivity` on the Material 3 page: list from
  `assets/notices/index.tsv` (one `list_item.xml` row per line with three fields), full text view;
  an `OnBackPressedCallback`, enabled while a full text is shown, returns to the list.

### The terminal

- `VtSession` — a tab owned by the service: its `id` (from the service, naming the tab in a
  notification's intent), its `ConsolePty`, the terminal handle (0 until the shell starts, never
  reset), the view showing it (`view`, read on the pump thread), a restored tab's snapshot (a
  loader the service gives) and transcript, the pending notice and messages, its login and the
  account an `auto` login resolved to (`resolvedLogin`, written by the service), the file of its
  last saved snapshot (`savedSnapshot`, used by the service's worker only), the last transcript
  (whole, also returned while the screen is unchanged), the last progress report (`progress`, a
  `Progress`), the program status records by id (`Status`, least recently updated first, at most
  64) and the one most in need of the user (`status`), the clipboard reads waiting for an answer
  (the clipboard each named), a `vt-pump` thread, a `vt-waiter` thread and its own single-thread
  `vt-writer` executor, the cursor's default shape and blinking (`setCursor`, from the service);
  `Events` (title changed, bell, clipboard write, notification, clipboard read, progress or status
  changed, a command finished, tmux's control mode detached, the shell finished) go
  to the service on the main thread; `busy` (closing it would end something: the shell runs, has
  started and is not at its prompt) and `cwd()` (its foreground group's directory, from the
  kernel). Its companion holds
  libghostty-vt's progress states, status states and kinds, and `describe` (a status in words).
- `VtView` — the view: the attached `VtSession`, cell metrics, four paints, a mirror of the screen
  (codepoint, colours, attributes with search marks, grapheme clusters per cell, each row's
  selected columns) and a backing bitmap, both one row taller than the grid (the frame's overscan
  row after the viewport), the modes (the alternate screen's readable as `altScreen`), scroll
  position and selection bounds and shape of the last frame, `scrollPx` (how many pixels, less
  than a row, the grid is drawn above its place while the viewport scrolls) with the drag's
  sub-pixel remainder, whether a search runs and its count as last reported, the
  rows of the search's matches along the scroll bar (taken with the count and selected match they
  belong to), the
  last frame's image placements (`Placement`, a pool reused across frames) and the bitmaps of the
  images they show by id with the generation each holds (`Picture`), the animation's redraw, the
  IME connection, the mouse pointer (the program's shape, the cell hovered, whether it is a link
  and whether Ctrl or Meta is held, the system pointer set), gestures (with the taps in a row) and
  their drag states, a mouse's selecting (the button held, `pointerButton`; when its last left
  press came; whether its drag makes a rectangle; whether the selection came from a mouse,
  `pointerSelection`, which hides the handles), the `OverScroller` of
  flings and of settling on a whole row (`settling`), selection handles and the floating menu,
  the cursor's blink timer, the check whether a password is still read while the cursor is a lock
  (`passwordCheck`) and whether the IME was told a password is typed (`imePassword`), and the
  bell's last vibration. `Host` (the activity): `onGrid`, `onInput`, `onZoom`, the extra keys' `ctrl`,
  `alt`, `shift` and `modifiersUsed`, `onLink` (opens a link in another app) and `onSearchCount`.
- `ProgressLine` — a 2 dp view over the top of the terminal showing the current tab's progress
  report; `Colors` (primary, error, muted by progress state); its companion draws progress into any
  rectangle (`draw`, a fill or a sweep) and gives a report's fraction (`fractionOf`).
- `VtGlyphs` — box drawing (U+2500–U+257F), block elements (U+2580–U+259F) and the Powerline
  triangles (U+E0B0–U+E0B3) drawn as shapes.
- `VtKeys` — Android key codes to libghostty-vt's `GhosttyKey` numbers (`of`, 0 when unknown),
  modifier bits (Shift, Ctrl, Alt, Meta as Super; no lock bits), actions, `isModifier`.
- `VtNative` — 47 `@JvmStatic external` functions on the native library, and libghostty-vt's
  cursor styles (`CURSOR_BAR`, `CURSOR_BLOCK`, `CURSOR_UNDERLINE`), selection gesture behaviours
  (`GESTURE_WORD`, `GESTURE_LINE`) and selection adjustments (`ADJUST_*`), passed to vt.c as raw
  values, so they must match libghostty-vt's headers.
- In `vt.rs`: `Tab` (its own handle, the kernel's handle, console number, the terminal under a
  mutex, a `dirty` flag) in a table `TABS` keyed by handles from 1, never reused; `TermGuard` (a
  tab's locked terminal, which schedules a compression pass when it is released); `Term` (owns the
  `AppineVt`, freed on drop); `RawEffects`, laid out as vt.c's `AppineVtEffects`, turned into
  `Effects` (with the `StatusReport`s decoded from vt.c's queue, the clipboard reads, the last
  command finished: its exit code when reported and how long it ran, and whether tmux's control
  mode, which vt.c detached, ended); the
  compression schedule `COMPRESS` (a `Due` per tab: when to look at it next and when it was
  scheduled), served by the `vt-compress` thread (`compressor`).
- In `vt.c`: `AppineVt` (the terminal, its render state, key and mouse encoders, the grid and cell
  pixel sizes, reply, frame and scratch buffers, one text buffer shared by title, copy, link and
  transcript, the title-changed flag, the synchronized-output hold, whether the console reads a
  password (`password_input`, the cursor then a lock), the tracked anchor of a
  handle's drag, libghostty-vt's selection gesture, shared by touch and mouse, with its reusable
  events (an untimed press and a drag for a finger's word and line selections, a timed press and
  a release for a mouse's clicks, which share the drag), the last
  reported mouse cell, the effects since they were last taken: bells, the
  last clipboard write, the last notification, the last progress report and every program status
  report, queued in a byte layout vt.rs reads, the clipboard reads (OSC 52) with the clipboard
  the last named, the last command finished, and the end of tmux's control mode it detached
  (`tmux_detached`); `AppineVtEffects` is how they are handed out; the
  timing of the command running (`command_start`) and whether the cursor was at the prompt after
  the last feed (`was_at_prompt`); the OSC string the output is in (its
  first 64 bytes and its length), to find those reads and prompt starts, and the DCS string it is
  in (its first 5 bytes, its header, and its length) with whether that is tmux's control mode
  (`tmux_control`); how a tap on the shell's
  input moves its cursor, as the primary screen's last prompt start (OSC 133;A) declared it
  (`prompt_click`: not at all, arrows, or a mouse press with the row counted from the screen's top
  or the prompt's); the search: a `GhosttySearch` while one runs,
  scratch for the viewport's matches and for all matches and the rows they start on (freed when the
  search ends), and each viewport cell's search mark for this frame and as
  last sent; the cell of a command's output a long press found, tracked for the menu's Select
  output; the compression state: a pass scheduled, the activity token last seen, a read that
  restored compressed pages since the last pass; whether the last transcript is still the screen's
  text, and whether the last snapshot is (or none can be taken until the terminal changes); and the
  render state's image list as last read, in drawing order (`Placement`), with its placements
  generation). Process-wide, installed into libghostty-vt's sys interface with the first terminal
  (`install_sys`): the PNG decoder and the log callback (`on_log`), which with
  `on_unknown_sequence` shares a limit of 20 lines a second across all tabs (`log_allowed`).

## Public entry points

- Launcher activity `MainActivity` (`singleTask`, `adjustResize`, `Theme.Appine.Terminal`, handles
  most configuration changes itself, the dark mode and density included), started with
  `EXTRA_TAB` (a `VtSession.id`) by a tab's notification; listener callbacks `onProgress`,
  `onMachineReady`, `onSessionsChanged`, `onBell`, `onClipboardRead`, `onTabStatus`,
  `onSettingsChanged`,
  `onNotice`, `onRestarting`, `onExitAll`.
- `LicensesActivity`, opened from Settings.
- `VtNative`: `create`, `restore`, `setCursor`, `print`, `pump`, `title`, `atPrompt`, `frame`,
  `image`, `passwordInput`,
  `resize`, `key`, `paste`, `mouse`, `focus`, `scroll`, `jumpPrompt`, `gesturePress`,
  `gestureDrag`, `gestureEnd`, `mousePress`, `mouseDrag`, `mouseRelease`, `selectContext`,
  `selectGrab`, `selectTo`, `selectAdjust`, `selectRectangle`,
  `selectAll`, `outputAt`, `selectOutput`, `selectClear`, `copy`, `link`, `promptClick`, `search`,
  `searchSelect`, `searchRows`, `searchEnd`, `clear`,
  `reset`, `message`, `transcript`, `snapshot`, `version`, `compressAll`, `destroy`; `pump` runs on
  `vt-pump` and calls `VtSession.onOutput`, `onTitle`, `onBell`, `onClipboard`, `onNotification`,
  `onProgress`, `onStatus`, `onClipboardRead`, `onCommandFinished`, `onTmuxDetached` and
  `onForeground` back;
  `snapshot` and
  `compressAll` run on the service's worker, `version` wherever `Tabs` needs it, the rest on the
  main thread.

## Data flow / lifecycle

### Binding and choosing a tab

- `onCreate` applies dynamic colours, reads `EXTRA_TAB` and the font size, builds the views (the
  status line says "Starting…"), sets up insets and, on Android 13 and later, asks for the
  notification permission. `onStart` applies the wide-cell setting, starts and binds the service;
  `onServiceConnected` registers the listener and replays the start's result or progress and
  pending notices; `onStop` unregisters and unbinds.
- When the kernel is ready the tab shown is, in order: one a notification asked for (`EXTRA_TAB`,
  from `onCreate` or `onNewIntent`; a tab closed meanwhile is skipped), the activity's own current
  tab if it still exists, the service's `focus` (the restored current tab) if it still exists, the
  last tab; with no
  sessions a new one opens. A failed start hides the terminal and shows "Cannot start: …";
  `onRestarting` detaches and shows "Restarting…". `onServiceConnected` calls `onMachineReady` on
  every bind (each `onStart`) when the kernel runs or failed to start (while it starts, the
  progress, and `onMachineReady` later from the service), so coming back to the activity (from
  Settings, a link's app, the background) shows the current tab again, which closes the search
  bar. A new activity instance has no current tab of its own: it falls back to `EXTRA_TAB` (read
  again from its intent), then the service's `focus` (set only by a restore), then the last tab,
  never the service's `current`.
- Showing a tab sets it as the service's `current` (which takes its notification away), attaches
  it to the view (`VtView.attach`) and renders the bar; `onRestarting` and `onDestroy` detach it,
  so sessions never keep a destroyed Activity's view.
- Closing a tab hangs it up at once; when it was the current one, the tab after it, now in its
  place, is shown (the one before when it was the last). Closing the last tab while the kernel runs
  finishes the activity (the service stays).

### The tab bar

- One 32 dp row in any window: a scrolling row of tabs across its whole width, with `×` (closes the
  current tab; hidden while there is none), `+` (opens a session while the kernel runs, in the
  current tab's directory: its console's foreground group's, read from the kernel on the main thread
  as a spawn is; the home directory without a current tab or once its shell ended) and `⋮` (a menu:
  "Find", disabled with no current tab, "Clear screen", disabled without a current tab whose
  terminal exists or while it shows the alternate screen, "Reset terminal", disabled without such a
  tab, "Close other tabs", disabled when there is no other, "Close all tabs", disabled with no tabs,
  and Settings; see Clearing and resetting; Find is enabled for a current tab whose terminal does
  not exist yet, where searching does nothing) over its end, every tab and button a 32 dp square with
  its label centred. The tabs pass under the buttons, which hide them, and an end padding as wide as
  the buttons lets the last tab scroll out from under them; while more tabs lie beyond, the row
  fades out over 32 dp ending 12 dp inside `×`, in the blank before its glyph (the scroll view's
  right fading edge; the left edge does not fade).
- A tab shows its number (from 1, by position): the current one in the primary colour above the
  indicator, the others muted, an ended one faded (38% opacity, its indicator too). The current
  tab is scrolled into view after each change, clear of the fade, or to the row's end when less
  than a fade's width of tabs would be left beyond the padding; scrolled away from it, the row
  brings it back the same way 3 s after coming to rest: a finger on the row holds that off, and
  each later move without one (a fling, TalkBack's scrolling) starts the wait again. With gesture
  navigation, the bar but for its buttons is excluded from the system's back gesture.
- The indicator is a rounded bar 16 dp wide and 1 dp high in the primary colour, centred under the
  label with its top 4 dp below the baseline, in whole pixels rounded down; the row draws it over
  its tabs. Labels are sized in dp (numbers 12 in the default weight, buttons 14) so the system
  font scale leaves them as they are (dialogs, menus and toasts follow it).
- The other tabs, while their shell runs, show what their programs report (`VtSession.progress`
  and `status`; the current tab has the terminal's progress line instead): progress in the
  indicator's place, its track the progress colour at 30% opacity, filled from the left to the
  percentage or with a quarter of it sweeping across and back, in the progress line's colours (a
  progress report; else a working status, or a blocked one with a percentage, in the primary
  colour); and a dot of 2 dp radius 1 dp right of the number's ink, centred on the top of its
  digits, for a status: blocked in the primary colour, failed in the error colour, done in the muted
  one. The row draws them over its tabs, following a sliding tab and fading with its number, and
  redraws every frame while a sweep is drawn.
- Closing tabs is animated (150–300 ms a step, under the system's animator duration scale): a
  closed tab shrinks and fades out where it was; the tabs after it slide into the places left,
  still showing their old numbers, then roll to the new ones (300 ms, like SwiftUI's numeric
  text): each digit that changes moves down and out, fading and blurring, as the new one comes in
  from above and sharpens (from below for a higher number); digits that stay the same stay still,
  and a number of another length rolls whole. The indicator keeps to the current tab's place on
  screen; a row at its end scrolls back as it shortens, the tabs before slide right instead, and
  the indicator slides on to the new current tab. Showing another tab (a tap, Ctrl+Tab, a new tab)
  slides the indicator there the same way (200 ms) from where it is drawn; only the row's first
  layout puts it in place at once. The numbers' colours change at once, and so do TalkBack's
  descriptions.
- Tapping a tab shows it; a long press shows its title in a toast: the program's title (`sh` when
  it set none), with "(ended)" once the shell ended. The activity shows one toast at a time (a
  tab's title, a locked modifier, a link that cannot be opened), cancelling the previous one.
  TalkBack reads "Tab N: title", followed for a running program that reported something by what
  it reported in parentheses (`VtSession.describe`'s words: "Waiting for your approval", "Waiting
  for your answer", "Waiting for credentials", "Waiting for you", "Done", "Failed", "Working", or
  "Paused" for a paused progress, with the percentage when there is one; an idle status alone says
  nothing; for the
  current tab too, though only the other tabs draw marks), and the `×`, `+` and `⋮` buttons by
  name (`⋮` as "More options").
- `×` and the menu's "Close other tabs" and "Close all tabs" close at once when none of the tabs
  they close is busy (its shell has ended, has not started, as in a restored tab never shown, or
  waits at its prompt as the prompt marks tell, see Shell integration), else after a confirmation
  dialog ("Close this tab?" for one tab, else "Close N tabs?", counting all of them); a shell
  without prompt marks always counts as busy. Several tabs are closed with the current one last,
  so no other tab is shown (and its
  shell started) on the way, and closing all ends like closing the last tab. Ctrl+Shift+W closes
  the current tab without asking, even while its shell runs, and in an ended tab any input with a
  line break (Enter) closes it.
- The close dialog's button closes only the tabs still open, and dismissing it or the clipboard
  question gives the terminal the focus.
- Its dialogs (close, notices, the clipboard question, the paste confirmation) and its menu (⋮, a
  `PopupMenu` placed like a
  toolbar's overflow menu) are Material 3 in the theme's dark colours, from the wallpaper on
  Android 12 and later (`DynamicColors`). The tab bar, the extra keys and what lies under the
  system bars are black like the terminal; their labels, buttons and the status text take the
  muted on-surface-variant colour, and the current tab's number and indicator the primary one.

### Hardware-keyboard shortcuts

- Taken by the activity (`dispatchKeyEvent`) before the terminal sees them, together with the
  matching key release, when the key event (from a hardware keyboard, or an IME sending key events)
  has Ctrl without Alt or Meta: Ctrl+Shift+T opens a tab, Ctrl+Shift+W closes the current one,
  Ctrl+Shift+F opens the search bar (these once per press; repeats are swallowed), Ctrl+Tab and
  Ctrl+Shift+Tab show the next and the previous one, going round and repeating while held (and
  swallowed when there is nothing to switch to), and Ctrl+Shift+↑ and ↓ scroll to the previous and
  the next prompt, repeating while held (Ghostty's `jump_to_prompt` keys on Linux; on the alternate
  screen, which has no prompts, and without a terminal they reach the program). They also work with
  the search bar's text focused, and are listed in the system's keyboard shortcuts screen (Meta+/;
  the tab ones under Tabs, Find and the two prompt jumps under Terminal). The extra keys' modifiers
  do not make shortcuts: with them these keys reach the program.

### Finding text

- "Find" (⋮) or Ctrl+Shift+F, with a current tab, puts the search bar in the tab bar's place
  (the tab bar is gone, the place keeps its 32 dp, so the terminal keeps its rows) and brings the
  keyboard up on its text, which is kept from the last search and selected, so typing replaces it;
  that text is searched for again at once. Every change of the text searches anew (`VtView.search`,
  `VtNative.search`): libghostty-vt matches ASCII letters in either case and everything else
  exactly, in the screen and its scrollback (the primary screen's results are kept while a program
  uses the alternate one). Empty text searches for nothing and clears the count.
- No match is selected until a step: `↑`, Enter or the keyboard's Search key selects the next match
  above (towards older output; the first step selects the newest), `↓` or Shift+Enter the one
  below, going round at either end, and the viewport scrolls to it unless it is shown (the scroll
  bar shows only when it scrolled; a step stops a fling and puts the grid on a whole row). The count reads "selected/total", the selected one counted
  from the newest, 0 before a step; it grows as a long scrollback is searched over several frames.
- `×`, Esc in the text, or Back (after Back has hidden the keyboard) closes the bar: the search
  ends and its marks go, the tab bar comes back, the terminal takes the focus and the viewport
  stays where the search left it. Showing a tab (another one, Ctrl+Tab while searching, or the
  same one again when the activity comes back, see Binding and choosing a tab) or a restart
  closes it too, as a search belongs to the tab it was opened in.
- Matches are drawn as Ghostty draws them: black text on yellow, the selected match on orange, the
  second cell of a wide character with its first. Along the scroll bar, as browsers mark found
  text, a 2 dp mark as wide as the scroll bar in the same yellow (the selected match's in orange)
  stands for each row a match starts on, at that row's place among the scrollable rows (where the
  thumb's top is when the row is the viewport's first, unless the thumb is held at its least
  length), rows closer than a pixel sharing one; the marks stay while the bar is open and there is
  a scrollback, under the thumb, which fades as before. The view takes the rows
  (`VtNative.searchRows`) when the count or the selected match changes, when the scrollable rows
  shrink (rows left the scrollback at its limit, or it was cleared) and at most every 500 ms while
  frames bring changes, since rows that leave the scrollback move every match's row. vt.c reads all
  the matches (newest first) and looks up only each page's first row, adding a match's row within
  its page, which reads no compressed page. Tapping the terminal (or pressing a mouse on it)
  while the bar is open gives the keyboard back to the terminal and leaves the bar and its marks;
  the extra keys always go to
  the terminal, the bar's text focused or not.

### Shell integration

- The guest's login shells mark their prompts with OSC 133 (android/setup's profile: bash marks
  the prompt, the typed command and the output's start; busybox ash the prompt, and the command
  ending with its line). libghostty-vt keeps the marks in the rows and cells it writes (prompt,
  input, output) and the cursor's place among them, which closing, jumping, Select output and taps
  on the input read through vt.c's shell integration functions; a mouse's Ctrl+triple-click
  selects a command's output through libghostty-vt's selection gesture, and a right-click looks
  for one for the menu's Select output. A shell without the marks (zsh,
  fish, dash, a nested non-login shell, a remote one over ssh that sends none) leaves every cell
  output: it is never at a prompt, there is nothing to jump to and no output to select, and no
  command is timed.
- Commands are timed through libghostty-vt's OSC 133 callback (`on_semantic_prompt`): a command
  starts at the output's mark (bash's PS0, `C`) or, as busybox ash marks none, when a feed ends
  with the cursor no longer at the prompt where the one before left it there (the line feed that
  ends the typed input, but also output of a background job at the prompt); it finishes at the
  command end (`D` with the exit code) the next prompt sends, and a primary prompt's start
  without one forgets it. A command end with nothing timed (a prompt drawn again after a resize,
  Ctrl-C at the prompt) reports nothing. The pump hands the last finished command of a read to
  `VtSession.onCommandFinished`, and the service notifies for one that ran at least 10 s in a tab
  not in front (android/service).
- At the prompt (`VtNative.atPrompt`, libghostty-vt's cursor-at-prompt: the cursor in a prompt or
  the input after it, on the primary screen): `VtSession.busy` is false and closing asks nothing.
  A command typed and entered ends the input at its line feed, so a running command, even a silent
  one, counts as busy until the next prompt; jobs left in the background do not.
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

### Extra keys

- Rows of 32 dp with 12 dp labels (not sp, so the system font scale leaves them as they are); the
  eight columns share the width, each at least 45 dp (the last one takes the pixels left over), so
  they all fit from 360 dp on. Narrower, the first seven columns of both rows scroll sideways
  together under the finger (turning the page keeps the scroll position) and under the last, which
  stays at the right edge; while more keys lie beyond, they fade out over 32 dp ending 6 dp inside
  the last column, in the blank before SHIFT, as the tab row's into `×`, and a touch there goes to
  the last column's key. A sideways slide past the touch slop then scrolls and cancels the key it
  started on, unless that key is already held, and with gesture navigation the scrolling part is
  excluded from the system's back gesture. With nothing to scroll the keys keep every touch and the
  edge its gesture. No lines are drawn between or around the keys.
- A key is shaded while pressed and acts when the finger lifts (with a key-click haptic), not at
  all if the finger left it by more than the touch slop. Held past the long-press timeout, the
  arrows repeat every 80 ms until the finger lifts or leaves the key (no act on release then);
  other keys but the modifiers still act once on release. No key has a swipe or popup.
- CTRL, ALT and SHIFT: a tap turns them on for the next key (primary colour only) or off, a locked
  one too; a long press (with a long-press haptic) locks them on (primary colour and the
  indicator) until tapped, one a tap turned on included, with a toast naming the key ("CTRL
  locked"), or turns a locked one off. Keys go to `VtView.press` and `type`, which read the
  modifiers through the host and report when a key took them; that clears one-shot modifiers and
  leaves locked ones on.
- SYM shows the symbols in place of every other key (`%` in place of SHIFT) and keeps them, for
  any number of keys, until tapped again; its label takes the primary colour and the indicator
  while they are shown. Turning the page leaves the modifiers as they are: CTRL and ALT apply to
  symbols too (CTRL, SYM, `]` sends Ctrl+]), though they cannot be seen on the symbols page, while
  SHIFT waits there: it applies to no key, the symbols or the IME's, and a one-shot SHIFT is not
  used up, so it applies again once the first page is back. A key still held on the page going
  away is cancelled. The page lives as long as the view: a new activity starts on the first.

### Insets and the IME

- The content is edge-to-edge with the root padded by system-bar, display-cutout and IME insets
  (from Android 15 the window reaches into the cutout, which in landscape is at a side). As the IME
  shows or hides, the root is laid out for the end at once (the terminal regrids once, at the
  start) and the extra keys are drawn moving from their old place to the new one along the IME
  animation's curve (`keysFollowIme`), rising through the IME's room or sinking over the
  terminal's new rows; an animation that interrupts another starts from where the keys are drawn.

### Sessions

#### Start and end

- A `VtSession` starts its shell the first time a view showing it knows the grid (`VtView.attach`
  and every regrid call `Host.onGrid`, which starts the current tab once): `ConsolePty.start`
  opens a console of the grid's size and spawns the login; then the terminal is made: a restored
  tab's from its saved snapshot when it has one that decodes (`VtNative.restore`, see Snapshots and
  restoring), else a new one (`VtNative.create`), both with the same options
  (scrollback limited to 10,000 lines or 10 MiB, whichever is reached first: the lines up to about
  120 columns, the bytes beyond; default colours white on black and no default cursor colour, so
  the cursor follows the foreground as in Ghostty; the cursor's default shape and blinking from
  Settings (a blinking block unless changed, as Ghostty's `cursor-style` and `cursor-style-blink`
  default): `wrap` itself only turns blinking on over libghostty-vt's steady block, and the session
  gives the terminal the setting right after making it (`VtNative.setCursor`), before the pending
  text; DECSCUSR 0, DECSTR and RIS bring the setting back, DECSCUSR or mode 12 override it, see
  Frames and drawing; grapheme clustering,
  mode 2027, on by default as
  in Ghostty; the render state asks for one overscan row below the viewport; the OSC 133 callback
  (command timing) and unimplemented OSC and APC strings reported with 16 bytes kept, of which
  only an OSC's leading number or an APC's first letter is logged (a malformed string may start
  with the program's data); Kitty graphics on, with 32 MiB of decoded images per screen (Ghostty's default is
  320 MB) and only data sent in the escape sequences (the file, temporary file and shared memory
  media, which would name the host's files and objects, answer `EINVAL: unsupported medium`, so
  programs fall back to sending the data); the Glyph Protocol off, so it is not advertised and
  programs that probe fall back to text; the terminfo name `xterm-256color`, the
  `TERM` the console's login gets, which XTGETTCAP's `TN` reports; continuation tracking of up to
  64 KiB, so a snapshot can be taken while output stops inside a sequence); a restored tab whose
  snapshot did not come back has its transcript printed into the new terminal, then the restore
  notice and queued messages are; then the writer, the pump and the waiter start.
- If opening the console or spawning fails, the reason is printed, the console is closed and the
  tab counts as ended (127), with no "Process completed" line and no writer. If the terminal cannot
  be made (the kernel is gone, libghostty-vt fails, a panic), the tab ends (127) without a word,
  and later messages for it are refused.
- A restore replaces whatever was pending with its notice; a message shown before the terminal
  exists is queued after it as a plain yellow line, not inserted as below.
- The pump reads the console into a 64 KB buffer, asks the kernel whether the console now reads a
  password (`Machine::console_password_input`: lines without echo, which programs set before
  printing their prompt) and notes it in vt.c, feeds the terminal under the tab's lock (in parts
  that end at the start or end of an OSC or DCS string, `ghostty_terminal_vt_write_until`, where
  vt.c keeps an OSC's first 64 bytes to find clipboard reads and prompt starts and a DCS's first 5,
  its header, to find tmux's control mode; no slower than one write), writes
  the terminal's replies (DA, DSR, XTVERSION, size reports, pastes' bytes, the line that detaches
  tmux's control mode) back to the console
  before reading again, calls `VtSession.onOutput` only when it sets `dirty` (so a burst asks for
  one frame, and nothing is asked while no view takes frames) and `onTitle` after OSC 0/2. It runs
  whether or not a view shows the tab, and ends when the kernel is gone, the console reads end or
  fail, a reply cannot be written, a call into Java throws, or it panics.
- What programs ask of the app is taken after each read (`appine_vt_take_effects`) and handed to
  the session, which posts it to the service unless released: the bell (any number of BELs in a
  read counts once); a clipboard write (OSC 52, OSC 1337, OSC 5522, to any location, as Ghostty's
  `clipboard-write = allow`): its first `text/plain…` representation up to 256 KiB (larger is
  denied, other types unsupported; those replies reach only OSC 5522 programs), or a request to
  clear (an OSC 5522 write of more than 1 MiB in all its representations fails with EFBIG inside
  libghostty-vt, which buffers a write whole before handing it over); a notification (OSC 9's
  body, OSC 777's title and body, up to 4 KiB each; only the last of a read); a progress report
  (OSC 9;4, only the last of a read); program status reports (OSC 7501, every one in order, which
  libghostty-vt has already checked against the specification and whose title and message it has
  decoded; with the callback set it also answers the protocol's `?` query); clipboard reads
  (OSC 52 with `?` for data: an OSC string of `52;`, the clipboards named (`c`, `p`, `q`, `s`,
  `0`–`7`, maybe none) and `;?`, ended and not cancelled, however the reads split it), counted
  with the first clipboard the last one named (`p` or `s`, else `c`), which every read of the
  batch is answered for; the last command the prompt marks saw finish (Shell integration); the end
  of tmux's control mode, which vt.c detached (below), delivered last (`onTmuxDetached`).
  libghostty-vt's clipboard-read
  callback is left unset: it must answer before returning, under the tab's lock, which rules out
  asking the user; so OSC 5522 reads are refused and Kitty paste events never sent. What the app
  prints itself (`VtNative.print`) asks nothing: its replies and effects are dropped. Where they go
  is the service's choice (android/service): the bell of the tab in front vibrates through
  `VtView.bell`, other bells (at most once a second) and notifications from tabs not in front
  become the tab's notification, clipboard writes always reach the clipboard, and clipboard reads
  are asked about in the tab in front and answered empty elsewhere.
- Clipboard reads, as Ghostty's default `clipboard-read = ask`: the session queues them (the
  clipboard each named) and tells the service once for the reads that come before an answer;
  `MainActivity` reads the clipboard's first item as text and, when there is some, asks ("Let the
  program read the clipboard?", up to 300 characters shown, Allow and Deny), one question at a
  time, whose answer covers every read queued meanwhile; an empty clipboard, or one without text,
  is answered at once. `VtSession.answerClipboardRead` writes one answer per read through the
  tab's writer, in Ghostty's form `ESC ] 52 ; <clipboard> ; <base64> ESC \` (ST, whatever ended
  the request): the text when allowed, nothing (an empty clipboard, as libghostty-vt answers a
  read it refuses, so the program does not wait for a timeout) when denied, dismissed or asked
  from a tab not in front.
- tmux's control mode (`tmux -CC`), which libghostty-vt built for apps does not parse (Confirmed
  bugs / technical debt), would leave the tab showing nothing while keys reach tmux as commands.
  vt.c finds a DCS whose header is literally `1000p`, however the reads split it, and, as Enter
  would, puts a carriage return among the replies the pump writes before reading again: tmux takes
  the empty line as a detach, leaving its session running, and ends the string with ST. That end
  is reported (only an ESC ending it, which libghostty-vt reports at ST's first byte; 0x9C is
  payload once the header is complete, and a string a program cancels with CAN or SUB, or the app's
  own CAN cuts, reports nothing), and the service shows a yellow `[appine]` line in the tab saying
  so and that `tmux attach` brings the session back (android/service; nothing on the alternate
  screen). Other DCS strings (DECRQSS, XTGETTCAP, tmux passthrough) only cost two extra stops each. Nothing
  retries when tmux does not detach.
- Progress and status (`VtSession`, main thread): a progress report replaces the last one (a
  removal clears it) and lasts 15 s without another, as in Ghostty. Status reports of a read are
  applied together, as the specification keeps them: one record per id, a report replacing its
  record whole, a clear removing the record and those beneath it (`build` takes `build/test`; an
  empty id, as after a full reset, takes all), at most 64 records, the one updated longest ago
  dropped; text direction controls are removed from titles and messages. `status` is the record
  most in need of the user: blocked, then error, done, working, idle, the most recently updated
  among equals. A change of either (by value) is reported to the service (`onStatusChanged`).
- The pump stands in for shell integration's prompt mark, which only some shells send: after a
  read that carried a progress or status report it notes the console's foreground process group
  (`Machine::console_foreground`; a failed lookup notes none, and that report then waits for its
  timeout, being seen or the shell's end), and on a later read where another group (or none) has
  the foreground it calls `onForeground`,
  which drops the progress and the working, blocked and idle records, keeping done and error ones
  until seen. Seeing the tab (`statusSeen`, from the service) drops done and error records; the
  shell's end drops everything.
- The waiter waits for the shell (−9, "signal 9", once the kernel is gone), gives the pump up to
  0.5 s to take the last output, then on the main thread closes the console (once), stops the
  writer and, unless the tab was released, drops the tab's progress and status records, prints
  "[Process completed (code N | signal N) - press Enter]" and reports the end.
- `release` (the tab closed, or its kernel going away) hangs up the console once, stops the writer
  (writes already queued still run) and destroys the terminal handle; a running pump keeps the
  terminal until it returns.
- Saving: `transcriptText` formats the primary screen with its scrollback as plain text (soft
  wraps joined, trailing blanks trimmed); on the alternate screen, when nothing changed the screen
  since the last transcript (no output, message, resize, clearing or reset), or when formatting
  fails, it returns the last transcript taken on the primary one ("" when none was). The service
  also takes the terminal's snapshot (Snapshots and restoring).
- Kill reports go through `VtNative.message` (a yellow line inserted above the cursor line: a CAN
  first when the parser is not at ground (`VT_GROUND`, which includes the UTF-8 decoder), so a
  program that died inside an OSC, DCS or APC string does not swallow it; then as many line feeds
  as the text needs rows, up again, save the cursor, insert lines, write, restore and move down,
  dropping any replies; skipped on the alternate screen) or are queued until the terminal exists.

#### Scrollback compression

- libghostty-vt compresses the primary screen's cold pages (wholly in the scrollback and outside
  the viewport; the alternate screen keeps no scrollback) to about a fifth of their 400 KiB, and
  restores a page when something reads it: drawing a viewport scrolled back over it, the formatter
  (transcripts, copies), select all (which reads the oldest and newest pages), a mouse's
  Ctrl+triple-click that selected a command's output and a tapped link's line; searching restores
  nothing. The scrollback limits count pages uncompressed, so the same
  history is kept in less memory.
- A pass is scheduled when a call into vt.c left work for one, which vt.c tells as the
  `TermGuard` is released: libghostty-vt's activity token changed (rows moving into the
  scrollback, a resize, scrolling), or one of the reads above restored pages, which vt.c marks
  itself since the token does not change for them. The `vt-compress` thread looks at the tab
  250 ms later and, while the terminal keeps changing, waits another 250 ms each time, but not
  beyond 5 s from scheduling, so output that never pauses is compressed too. It then runs
  libghostty-vt's incremental steps 1 ms apart, each under the tab's lock, until a pass finds
  nothing left; the last step frees vt.c's text buffer if a transcript or copy grew it past
  64 KiB.
- Memory trimming (`VtNative.compressAll`, from android/service) compresses every tab's cold pages
  at once, idle or not, one tab's lock at a time.
- Saving does not format a transcript again while nothing changed the screen, since formatting
  restores every compressed page; a tab whose text changed is formatted at each save and
  compressed again afterwards. A snapshot, which reads every page, marks the terminal touched too.

#### Snapshots and restoring

- A snapshot (`VtSession.takeSnapshot`, `VtNative.snapshot`, on the service's worker) is
  libghostty-vt's binary record of the whole terminal: both screens, the scrollback, images,
  colours, modes, the title and the unfinished sequence the parser is in. One is encoded only
  after the terminal changed since the last (output, a message, a resize, clearing or resetting;
  null otherwise, and the service keeps the last file); none can be taken (empty) when encoding
  fails, output having stopped inside a sequence longer than the 64 KiB tracked, or the snapshot
  is larger than 16 MiB (mostly images), and none is tried again until the terminal changes. A full
  scrollback of coloured output takes about 1.5 MB, encoded in about 1 ms and decoded in about
  3 ms on a Mac; the service stores it deflated (android/service).
- Restoring (`VtNative.restore`) decodes a snapshot into a new terminal, compressing its
  scrollback as it comes and keeping its images within the 32 MiB per screen. Before any callback
  is installed (nothing is reported, replies go nowhere), vt.c undoes what the programs that ran
  in it set up for themselves: an unfinished sequence is cancelled (CAN), the alternate screen
  left for the primary one, a soft reset (DECSTR) brings back the cursor, margins, attributes,
  cursor and keypad modes, the palette and the cursor style, reverse video, mouse tracking and its
  formats, focus events, bracketed paste, synchronized output, colour scheme and in-band size
  reports and Kitty paste events are reset, alternate scroll set, the Kitty keyboard flags popped,
  an open hyperlink closed, the title cleared and the mouse pointer's shape (OSC 22, which the
  snapshot carries and a full reset keeps) back to text; what follows is marked as output
  (OSC 133;C), so
  the prompt the snapshot was taken at is not cleared by a resize as a live one is, the cursor
  starts a line of its own when it is not at its start, and the rest of the screen below it is
  erased; the selection goes and the viewport returns to the bottom. The terminal then gets the
  options a new one gets and is resized to the grid. A snapshot that does not decode leaves the
  tab to its transcript (logged).

#### Clearing and resetting

- ⋮'s Clear screen (`VtSession.clear`, `VtNative.clear`) is Ghostty's clear_screen, and does
  nothing on the alternate screen (the item is disabled there): the selection goes and the
  scrollback is erased (ED 3). At the shell's prompt (prompt marks) the whole screen goes too (ED 2,
  which libghostty-vt turns into a move into the scrollback when the last non-blank row is a
  prompt, hence the ED 3 after it) and the shell gets a form feed (^L) to draw its prompt again;
  elsewhere (a command running, or a shell without marks) the rows above the cursor's are deleted
  from the top (DL), so the cursor's row moves up to it with the rows below, and the screen's
  Kitty placements are deleted with the images no other one shows. The viewport returns to the
  bottom.
- Reset terminal (`VtSession.reset`, `VtNative.reset`) is Ghostty's reset: libghostty-vt's full
  reset (RIS) of both screens, the scrollback, modes, the palette, the title and images, keeping
  the app's options and the colours set with OSC 10, 11 and 12; the selection, a
  synchronized-output hold and the prompt's way of moving its cursor end, a shell that was at its
  prompt gets a form feed (^L) to draw it
  on the empty screen, and the tab bar hears that the title went. Unlike a program's RIS, it
  reports no removal of the progress and no clear of the status records (libghostty-vt reports
  them only from the stream), so the tab keeps them (the progress still lapses 15 s after its last
  report).
- Clearing writes its sequences as the app's own (`write_own`, as kill reports do): after a CAN
  when the parser is inside a sequence a program left unfinished, keeping the console's replies
  as they were; resetting calls libghostty-vt's reset directly.

#### Frames and drawing

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
- The view keeps a bitmap per image shown, by id with the generation of its pixels: a placement
  of another generation has them copied (`VtNative.image`, through libjnigraphics' bitmap
  locking, premultiplied as `ARGB_8888` holds them; gray and RGB images widened), reusing the
  bitmap when the size is the same; a copy fails when the terminal has moved on to another
  generation, and the next frame names that. Bitmaps no placement shows are recycled, and
  attaching another tab drops them all. Pixels that arrive in a frame that does not repaint every
  row ask for a full frame.
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
  invisible hides the glyph but not its decorations; blink is ignored. Every 120 frames the view
  logs frame statistics.
- The cursor (bar and underline two light lines thick, block with the glyph redrawn in the
  background colour, underline, hollow; a block is drawn hollow while the view lacks focus; all
  but the bar cover two cells on a wide cell; in the OSC 12 colour, else the default foreground;
  a lock while the console reads a password, below), the IME's composing text (shifted
  left at the right edge) and the theme's text selection handles are drawn over the bitmap each
  draw.
- While the console reads a password the frame gives the cursor as a lock (style 4), as Ghostty
  shows one at password prompts: wherever the cursor is in the viewport, even when the program
  hid it, never blinking, with or without the focus; `VtView` draws a padlock about as tall as a
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
- The current tab's progress report is drawn by `ProgressLine`, 2 dp high over the top of the
  terminal's first row, as Ghostty draws it: filled from the left to the percentage in the primary
  colour, the error colour for an error, the muted colour while paused (all of it for a pause
  without a percentage); without a percentage, or indeterminate, a quarter of its width sweeps
  across and back (1.2 s each way, ease-in-out, timed by the clock) over a track at 30% opacity,
  redrawn every frame. It fades in and out in 150 ms and slides to a new percentage in 200 ms;
  showing another tab puts that tab's progress there at once (a tab without one still lets the
  old one fade out). It takes no touches and is hidden from accessibility.

#### Input

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

#### Gestures and selection

- A finger's (or a stylus's) tap ends a selection or its menu; a mouse's or touchpad's press takes
  the pointer paths below. Otherwise, when the program does not track the mouse and the
  tap is on a link, it opens the link instead of the IME; otherwise it opens the IME and, when the
  program tracks the mouse, also sends a left click (press and release at the tap's pixel position,
  in the program's format), or else moves the shell's cursor when it is on the input at the prompt
  (Shell integration). A tap that stopped a fling does nothing else (one during settling on
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
  and `file` links open (an OSC 8 link can name any scheme, other apps' deep links among them). The
  activity opens a web or mail link as a browsable `ACTION_VIEW` intent; a `file:` link (`ls
  --hyperlink`) is looked up off the main thread through the Files app's provider
  (`Documents.fileLink`) and opened as its document, typed by it and with read access granted: a
  directory in the Files app, a file in the app the system picks. A link from another host, outside
  the home while the whole system is not shown, or to nothing in the store gets a toast instead, as
  does a link no app takes.
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
  gesture's press (`GESTURE_WORD`), notes whether the cell is in a command's output (Shell
  integration), and moving on extends the selection by whole words as a double tap's drag does. On
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
    output, Shell integration), each dragging on by its unit; a second or third on empty cells
    leaves the selection as it was. Held past the grid's edges, the drag scrolls as below.
  - Shift+press, with a selection and later than the double-tap timeout after the last left press,
    extends the selection from that press by its unit instead (Ghostty's), when the gesture still
    has it; otherwise it is a plain press.
  - A release keeps the click count for the next press; only a first click that did not drag (it
    stayed in its cell and selected nothing) acts: with Ctrl or Meta on a link it opens the link,
    otherwise it moves the shell's cursor there at the prompt (Shell integration) and brings the
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
  Open link ends the selection and opens the link. Select output selects the whole output (Shell
  integration) and the menu comes back for it, without the item, which a double or triple tap,
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

### Licenses

- The build writes `notices/` (Appine's licence; Fuchsia's for the line discipline; Ghostty's
  libghostty-vt, uucode, simdutf, Highway, Wuffs, translate-c, Zig's compiler_rt and JetBrains
  Mono; Alpine
  packages from the rootfs database, the doas package, Android libraries, Rust crates, SQLite,
  then full texts); a full text is monospace, selectable and sized so 74 columns fit (at most
  13 sp), scrolling sideways when it does not.
- The page (`page.xml`): a `MaterialToolbar` (title, back arrow) in an `AppBarLayout` that lifts
  while the content below it is scrolled, over a `FrameLayout` with the app bar's scrolling
  behaviour, into which the activity puts a `NestedScrollView` it makes; edge to edge (wallpaper
  colours from Android 12 on through `DynamicColors`), with the bar padded under the status bar
  and the scroll view clear of the navigation bar. The toolbar shows the entry's title over its
  full text; the back arrow and back both return to the list, then close.
- It follows the system's dark mode: a change recreates the activity, which shows the same full
  text again (saved state). A rotation does not recreate it, so a full text shown keeps its size.

## Dependencies (inbound / outbound)

### Inbound

- `TerminalService` (creates and holds the `VtSession`s: `setRestored`, `showMessage`,
  `transcriptText`, `takeSnapshot` and `savedSnapshot`, `title`, `isRunning`, `console`, `login`
  and `resolvedLogin`, `id`, `status`, `statusSeen`, `describe`, `answerClipboardRead`, `release`;
  `current`, `focus`, `sessions`, listener callbacks, the notification intent with `EXTRA_TAB`),
  its memory trimming (`VtNative.compressAll`) and `Tabs` (`VtNative.version`);
  `SettingsActivity` (licenses).

### Outbound

- `TerminalService` (`createSession`, with the current tab's directory for a new tab,
  `removeSession`, `current`, `fillWide`); `Native` (`openConsole`, `spawn`, `write`,
  `closeConsole`, `waitFor`, `consoleCwd`), hence the kernel's host console; `login.sh`, and the
  prompt marks of android/setup's profile (`profile.sh`); `assets/notices`.
- Material Components 1.14.0 with AppCompat and androidx.activity (both themes, `LicensesActivity`
  and the page layouts, `MainActivity`'s dialogs and menus, `VtView`'s paste confirmation);
  androidx.core's insets and animation compat.
- libghostty-vt 1.0.0, Ghostty's terminal library released apart from the app (the terminal
  state and its full reset, the default cursor's shape and blinking, writes that stop at OSC and
  DCS strings' starts and ends, render state with one overscan row, key and mouse encoders, paste,
  focus encoding, selection gestures (a finger's untimed word and line presses and drags; a
  mouse's timed clicks with their count, cell, word, line and output behaviours, rectangle drags,
  releases, dragged flag and anchor), word selection and selection containment (right-click),
  output derivation and keyboard adjustment, search with the list of all its matches, plain-text
  formatting, title and width,
  progress and program status reports, semantic
  prompts (cursor at a prompt, scrolling by prompts, the OSC 133 callback, rows' and cells' marks
  read through grid references), the full reset callback, unimplemented OSC and
  APC strings, its log, scrollback compression, snapshots with continuation tracking, Kitty
  graphics with their render-state image list and animation), and the Wuffs PNG decoder it
  carries, linked into
  `libappine.so` (`android/native`); the system's libjnigraphics (bitmap locking for images'
  pixels); `Machine::read_console`,
  `write_console`, `resize_console`, `console_foreground` and `console_password_input`
  (kernel/tty) and the
  bridge's `guarded!`, handle table and string helpers, used from `vt.rs` directly; the system
  clipboard; the app the system picks for a link (a browser, or a viewer or the Files app for a
  guest file through `Documents.fileLink`, android/documents); JetBrains Mono 2.304 (variable,
  upright and italic) in the assets, and the system's fallback fonts for everything else (CJK from
  Noto Sans CJK on AOSP images); the wide-cell setting `TerminalService.fillWide`.
- Of libghostty-vt's C API, the terminal has no use for: the parsers that work without a terminal
  (OSC and SGR) and the key and mouse events' getters; `wasm_*`, `type_json`, `ghostty_version`
  and `build_info` but its version string (the same text); the formatter's extras (state added to
  VT output); the cursor's SGR style;
  colour string parsing and the X11 colour names; iterating Kitty placements directly (the render
  state's image list is what is drawn); decoding a snapshot in steps and exporting its unfinished
  sequence (a snapshot is decoded at once and settled); the composing flag of key events (the
  IME's composing text is drawn by `VtView` and never goes through the key encoder) and the
  encoder's macOS Option-as-Alt setting, the one option `set_from_terminal` does not copy, which
  only a macOS build reads; the standalone encoders the terminal's own handling replaces
  (`paste_encode`, `paste_is_safe`, `size_report_encode`, `mode_report_encode`,
  `color_scheme_report_encode`); the hollow block cursor style, which the setting does not offer
  (the view draws a block hollow without the focus); setting the title or directory by hand.
  Defaults that suit it stay
  unset: resizing pulls rows back from the scrollback (right for a console that keeps no screen
  of its own), APC strings' size limits,
  and the platform's secure random source (getrandom). OSC 7 (and OSC 9 or 1337 directories) is
  left unwired too: Ghostty takes a directory only from the local host, and for a local shell the
  kernel's foreground directory, which new tabs use, says as much without the shell's help.

## Invariants and gotchas

- A kill report opens its room with line feeds before moving back up, so the cursor's line moves
  down with the cursor; this needs the parser at ground, which the CAN ensures. A program that died
  in the middle of a UTF-8 sequence leaves a U+FFFD at its last output position: the CAN ends the
  sequence and the decoder replaces it. The report's cursor save overwrites one the program had
  saved.
- libghostty-vt's mouse encoder deduplicates motion only when asked (off by default) and forgets
  its last cell whenever `set_from_terminal` or the size option is set, which `appine_vt_mouse`
  does for every event; so vt.c keeps the last cell itself, updated only when an event was encoded.
- `GestureDetector` with a double-tap listener (any `SimpleOnGestureListener`) gives the second
  tap of a double tap to that listener: `onSingleTapUp` never sees it and a drag starting there
  does not scroll. `VtView` therefore uses a plain `OnGestureListener`, and takes the press of a
  double or triple tap that selected before the detector (or the pinch detector) sees any of that
  touch: the view then counts the tap itself when the finger lifts without dragging, and a quick
  tap followed by a drag selects rather than scrolls.
- The console opens lazily, so restored tabs that were never shown have no console and no shell.
- `openConsole`, `spawn` and `closeConsole` run on the main thread and take the kernel's BKL, as
  does a resize (`VtNative.resize`, which resizes the kernel console from `vt.rs`; `Native.resize`
  is no longer used by the app); input is written on each tab's own thread, so a program that stops
  reading blocks only its tab.
- Console numbers are freed at close and reused lowest-first by the kernel, and a close carries no
  generation, so closing a stale number closes whichever console now has it.
- Programs never receive Ctrl+Shift+T, Ctrl+Shift+W, Ctrl+Shift+F, Ctrl+Tab or Ctrl+Shift+Tab
  typed on a hardware keyboard with Ctrl held but neither Alt nor Meta (the tab shortcuts), nor
  Ctrl+Shift+↑ and ↓ on the primary screen
  (the prompt jumps, taken even when there are no prompts); the release of the shortcut's key is
  swallowed too, so the Kitty protocol sees no release without a press. Other releases are never
  matched to presses: Ctrl+Shift+C/V's, a Shift+arrow's (or Page Up or Down, Home or End) that
  moved a selection, or Esc's when the IME took the press, reach the program alone.
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
  bar's 32 dp, the scrolling extra keys' 64 dp and a selection handle at the edge leave room
  under it.
- The column width is set on the keys' layout params during `ExtraKeysView.onMeasure`; the rows
  and the grids inside the scroll view are measured without a width limit, so they are forced to
  measure again whenever the column width changes, the page not shown too (it keeps the flag until
  it is added back).
- The indicators are drawn by the containers (the tab row, each page's grid, the fixed column) in
  `dispatchDraw`, over their labels, so a change of state must invalidate the container, not the
  label: `refresh` invalidates every grid and the fixed column, and `TabBarView.render` the tab
  row. The label's place is the sum of `left` and `top` up to the container:
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

### The terminal

- A tab refers to its kernel by handle and looks it up for each call: the pump once per read,
  holding it through the blocking read and the reply write and dropping it before calling into
  Java; each resize; like the `Native` functions. `Native.stop` and `shutdown` remove the handle
  and end the processes, and the store is closed when the last `Arc<Machine>` goes, so a tab
  holding one would keep the old store open after a reboot, tier change or distro switch, and the
  next kernel would start without the boot cleanup. The service releases its tabs (closing their
  consoles) before shutting the kernel down, so the pump's read returns first.
- A `VtSession` closes its console exactly once (`hangUp`), so a later close cannot hang up a tab
  that reused the number.
- The pump stays in native code for the tab's whole life, so JNI local references it makes for
  each event (the clipboard text, a notification's strings) must be deleted after the call, or
  they pile up in its frame.
- `AppineVtEffects` (vt.c) and `RawEffects` (vt.rs) are one layout and change together, as the
  frame format does (vt.c's comments name vt.rs's `Effects` for it).
- A restored terminal is a new one (the decoder makes it): the userdata and callbacks are not in a
  snapshot, nor are the Kitty storage limit (a decoder option), the media, the Glyph Protocol,
  the continuation limit (the unfinished sequence's bytes are), the terminfo name and the
  clipboard-write and unknown-sequence limits;
  the snapshot carries the scrollback limits, default colours, default modes (2027's) and cursor
  defaults and whether the cursor follows them (older snapshots a steady cursor). vt.c sets every
  option on it as on a new terminal, after `settle` (whose DECSTR makes the cursor follow its
  defaults again), and the session then sets the user's cursor, so the app's values win.
- Snapshots are recorded with the libghostty-vt version that wrote them (`1.0.0`; the copy in
  Ghostty 1.4.0 recorded `ghostty-1.4.0/0.1.0-dev`). One from another version is not decoded,
  since the snapshot format is versioned apart from the library and outside its ABI promise, and
  an app update is the commonest way a run ends with tabs to restore; those tabs come back from
  their transcripts.
- libghostty-vt's clipboard-read callback must answer before it returns, which happens on the pump
  thread under the tab's lock: asking the user there would block every frame (the main thread
  takes the same lock). OSC 52 reads are therefore found by vt.c and answered later through the
  tab's writer, and OSC 5522 reads, which only the callback can serve, stay refused.
- vt.c's OSC and DCS tracking (`osc_open`, `dcs_open` and `tmux_control`) follows only what goes
  through `appine_vt_feed`. App writes that cancel a program's string with CAN (`write_own`) clear
  it (a tmux control string so cut is never reported as detached); any other write into the
  terminal that could end or start such a string must too, or a later string's start is misread.
  Text the app prints (`VtNative.print`) goes through the tracking, but its replies are dropped,
  so it cannot detach tmux.
- The tmux detach rests on tmux taking a lone carriage return in control mode as an empty line,
  which detaches it (tmux 3.5a and 3.7c do); the reply goes out once per string. Should a program
  ever start and end such a string within one read, the carriage return still reaches whatever
  reads the console next (an empty line at a shell's prompt).
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
- Continuation tracking keeps at most 64 KiB of an unfinished sequence: a snapshot taken while
  output stops inside a longer one (an OSC 52 write, a large Kitty transmission) fails, and that
  save leaves the tab without a snapshot until the terminal changes.
- libghostty-vt is not thread-safe: every call into `vt.c` holds the tab's lock (recovered when
  poisoned); the frame buffer is written only by `frame`, which only the main thread calls, so the
  main thread reads it after the lock is released. vt.c's text buffer is shared by title, copy,
  link and transcript, so each result is turned into a Java string under the lock (the end of a
  compression pass frees it when large).
- Compression is scheduled only when a `TermGuard` is released, so every call into vt.c that can
  make work must lock through `Tab::term`; the compressor and `compressAll` lock the mutex
  directly because they make none (through the guard, a step would schedule its own pass). A tab's
  lock is taken before the schedule's, never after: the compressor lets the schedule's lock go
  before a step.
- libghostty-vt's compression activity token does not change when a read restores compressed
  pages, so a vt.c function that reads beyond the viewport (the formatter, select all, grid
  references above or below it) must set `compress_touched`, or what it restored stays
  uncompressed until other activity. A vt.c function that writes into the terminal must clear
  `transcribed`, or saves keep the old transcript.
- Compression gives page memory back with `MADV_DONTNEED` on ranges aligned to Zig's minimum page
  size (4 KiB), which a 16 KB-page kernel rounds up. That stays harmless because every terminal
  page, pooled or not, is a mapping of its own from Zig's page allocator, so the rounding never
  reaches another page; a libghostty-vt that packed pages into shared mappings would break it.
- The frame format is defined in `vt.c` and parsed in `VtView.apply`; the two change together. The
  key numbers in `VtKeys` are `GhosttyKey` values, which `key/event.h` spells out and libghostty-vt's
  ABI keeps from 1.0 on; key, action and mouse-button values it does not know make its setters
  fail (vt.c then encodes nothing), where the copy in Ghostty 1.4.0 took them and could crash.
- Since Ghostty 1.4's libghostty-vt, `GHOSTTY_TERMINAL_OPT_USERDATA` takes the pointer itself, not
  its address (older code passing `&ptr` crashes in the reply callback).
- The `get` and `set` functions take `void *` values, so a value of the wrong type compiles: 1.0
  changed some (a cell's graphemes come in a `GhosttyCodepointBuffer`, a Kitty image's pixels in a
  `GhosttyString`), and an upgrade must check every key's documented type, not only what fails to
  compile.
- Without a DA reply, programs that probe for it (vim, tmux) wait for a timeout, so `vt.c` answers
  DA1 (132 columns, selective erase, ANSI colour, and clipboard access, 52, which Ghostty lists
  whenever clipboard writes are allowed and programs check before using OSC 52), DA2 (VT220), DA3
  and XTVERSION ("appine") and reports a dark colour scheme. XTGETTCAP answers `TN` with the name
  vt.c sets, and every other capability from libghostty-vt's built-in table of Ghostty's own entry,
  not from `xterm-256color`'s.
- A progress or status report from a command that exits at once (a `printf` at the prompt) is
  usually dropped as soon as it shows: the read after it finds the shell back in the foreground. A
  report made after the reporting group has already given the terminal back is kept until the next
  command takes the foreground. A program in the background reporting is tracked against whatever
  group was in the foreground then.
- The status records live on the main thread only; the pump hands reports over through a locked
  list drained by one post, so the reports of one read are usually applied together and a passing
  state between two of them notifies no one. A program's full reset (RIS) arrives as a clear of
  every record and a removal of the progress, before libghostty-vt's reset callback (which only
  forgets how the prompt moves its cursor); the app's Reset terminal sends neither, nor calls the
  callback, so vt.c forgets that itself there (Clearing and resetting).
- Command timing's at-prompt state (`was_at_prompt`) changes only in `appine_vt_feed`, which the
  app's own printed text also goes through; clearing, resetting, messages and resizes leave it.
  For busybox ash, which marks no output start, output of a background job while the shell waits
  at its prompt starts the timing, so the next command's time is counted from then; bash's output
  mark replaces such a start. libghostty-vt is built ReleaseFast, so its log emits info and
  above, of which `on_log` passes warnings and errors; the log is installed when the first
  terminal is wrapped, after it was created or restored, so what that first decode and settle
  would log goes nowhere.
- Without default foreground and background colours libghostty-vt treats them as unset: OSC 10
  and 11 queries go unanswered, reverse video (DECSCNM) swaps nothing, and a colour set with OSC 10
  or 11 alone does not reach the render state. With them, an OSC 12 query without a cursor colour
  answers the foreground. A full reset (RIS) resets the palette but keeps colours set with OSC 10,
  11 and 12, as Ghostty does.
- libghostty-vt leaves the Glyph Protocol off (since 1.0; the copy in Ghostty 1.4.0 turned it on
  and answered `fmt=glyf` to its query), and the C API gives no access to registered glyphs; vt.c
  turns it off at create all the same. A full reset
  keeps that setting, the Kitty storage limit and the allowed media (the alternate screen takes
  the primary's), so they stay as set. Its Kitty media default is data in the escape sequences
  only; vt.c sets it again so a library change cannot open the file media, whose paths would be
  the app's own files rather than the guest's.
- As built for an app (`-Demit-lib-vt`), libghostty-vt has no PNG decoder unless one is installed
  (`ghostty_sys_set`, process-wide), though libghostty-vt.a contains Wuffs: vt.c's decoder calls
  Wuffs's C functions in the archive, compiling against the header from the release's Zig
  packages.
  Without it, PNG images (`f=100`, what most programs send) fail with `EINVAL`.
- The render state's source rectangles are whole image pixels, so a virtual placement's rows,
  drawn one by one, overlap or gap by a pixel each and show seams (Ghostty's renderer draws them
  so); vt.c joins rows that meet. Rows only partly in the viewport keep the rounding at their top.
- Erasing below the cursor (ED 0, what busybox's `clear` sends) leaves images in place, as in
  Ghostty and kitty; ED 2 and 3 delete the visible ones (and the scrollback's).
- The view paints images into its row bitmap, so a changed placement list must repaint every row:
  vt.c makes such a frame a full one, and the view asks for one when pixels arrive late. An image
  read for a frame during a synchronized-output hold may be gone or replaced by then; its copy
  fails (by generation) and the frame after the hold names the new one.
- With grapheme clustering on, programs that count widths per codepoint (`wcwidth`) disagree on
  emoji sequences; they can turn mode 2027 off.
- libghostty-vt's C terminal starts with a scrollback byte limit of 10,000 bytes (the default of
  its `Terminal.Options`, not Ghostty's 50 MB `scrollback-limit-bytes`), raised to its minimum of
  the active area's pages plus one, so a line limit alone keeps about two pages. The byte and line
  limits both apply, the first reached pruning whole 400 KiB pages; a page holds fewer rows the
  wider the terminal (about 1,000 at 45 columns, 580 at 80, 190 at 250).
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
- With prompt marks, every resize while the cursor is at a prompt (the keyboard showing or hiding
  is one) clears the prompt's rows, from its first row down, for the shell to redraw them on
  SIGWINCH (bash's marks ask for the last row only, `redraw=last`, as readline redraws). busybox
  ash and bash redraw on every resize, rows only included; a shell given the marks that did not
  would lose its prompt and typed input until the next one.
- vt.c's tracked cell of an output frees its tracking only after the output's selection is
  installed: untracking may change the page list the references taken from it point into.
- Shell integration lives in the guest (`/etc/profile.d/appine.sh`): a tab's behaviour (closing
  without asking, jumps, Select output) follows whatever the shell in it sends, so a user's own
  prompt or shell changes it, and a remote shell over ssh can send marks too.
- Up to Android 15 Back reaches `MainActivity.onBackPressed` (the manifest does not opt in to
  predictive back, so an `OnBackInvokedCallback` would be ignored there); from Android 16 on, the
  app targeting it, `onBackPressed` is never called and only the callback registered while
  searching closes the bar. Lint's GestureBackNavigation check is suppressed on the override.
- Text the app prints does not consume the title-changed flag, so a title it sets is reported at
  the pump's next read; a notification cut at 4 KiB can split a UTF-8 sequence, which vt.rs decodes
  lossily.

## Confirmed bugs / technical debt

- **Each tab keeps its whole last transcript in the Java heap** (`VtSession.lastTranscript`),
  though saving writes only its last 256 K characters: for a full scrollback of plain text that is
  several hundred KB, about as much as the compressed scrollback itself.
- **Shell integration is partial**: only bash and busybox ash get prompt marks (not zsh, fish or
  nested non-login shells); ash's mark the typed command as ending with its line, so the rows of a
  command longer than the screen's width after the first count as output (ash breaks such lines
  itself), and with the cursor on one of them a tap moves nothing, ash marks no output start, so a
  background job's output at its prompt starts the next command's timing early, and a PS1 set
  after the profile (ash's `~/.profile`) has none; and the progress and status records still go by
  the foreground process group rather than the next prompt, though the OSC 133 callback is now set
  (for command timing only).
- **libghostty-vt features not wired**: ENQ, the title report (CSI 21 t),
  XTCHECKSUM and window resizing (CSI 8 t) are left unanswered, as Ghostty's defaults leave them
  (`enquiry-response` empty, `title-report`, `vt-xt-checksum-report` and
  `vt-window-resize-allowed` false); OSC 5522 clipboard reads are refused and Kitty
  paste events never sent, since only the read callback, which cannot wait for the user, serves
  them (wiring it would also need the IME's multi-line commits, pasted as
  `GHOSTTY_PASTE_SOURCE_CLIPBOARD`, marked `GHOSTTY_PASTE_SOURCE_TEXT`, or they would become paste
  events).
- **More of libghostty-vt left unused**:
  - selection: of the selection gesture, its click counting for a finger (the view counts taps
    and gives finger presses untimed; a mouse's are timed), autoscroll ticks with their viewport
    option and autoscroll data (the view scrolls on its own timer), deep presses, its behaviour
    data and its word-boundary characters (the gesture's defaults, which are Ghostty's);
  - drawing: the default palette option (the 256 colours are libghostty-vt's built-in ones, and
    the colour scheme reply is fixed to dark); overscan above the viewport (only the row below is
    asked for);
    `row_iterator_next_dirty`; `frame` holds the tab's lock
    while it encodes every row, about 95% of its time (on an M2 Pro, vt.c on a macOS build fed a
    line of coloured output a frame: 63 of 67 µs at 45 × 60 cells, 69 of 74 µs at 80 × 40, 265 of
    283 µs at 200 × 60), so the two-phase render update, which would let only libghostty-vt's
    deferred style work (about 2 µs, 7 µs at 200 × 60) leave the lock, is not used, and taking
    the encoding out of the lock would need the render state kept apart from the terminal's lock;
    `frame` reads each cell with four to six
    `ghostty_render_state_row_cells_get` calls and a `ghostty_cell_get`, where
    `ghostty_render_state_row_cells_get_multi` takes several keys in one call (it stops at the
    first key that fails, so the colours, which may have no value, would go last, and the
    grapheme buffer still needs its length first); the gain is unmeasured; the
    colour utilities (`palette_generate`, the 256 colours from the first 16, and contrast and
    luminance for a minimum contrast), of use only with themes;
  - the rest: transcripts are plain text, though the formatter can emit VT or
    HTML with colours and OSC 8 links; the memory usage and the VT processing-error flag are never
    read; the OSC 133 event's command line and error text are not used (the profile sends no
    command line); KAM (mode 2) is ignored, as Ghostty ignores it by default (`vt-kam-allowed =
    false`; the key encoder never drops keys);
  - out of reach: the C API has no setter for the terminal's visibility, so mode 2033 reports
    every tab as visible, background ones too (nor for the password-input flag, which vt.c keeps
    itself for the lock);
    Kitty's drag and drop (OSC 72) has a callback only in the Zig API; OSC 66 text sizing, OSC 99
    Kitty notifications and hierarchical contexts are parsed but not acted on by libghostty-vt
    itself; tmux control mode is not parsed: libghostty-vt built for apps leaves Oniguruma out,
    which turns tmux control mode off with it, so `ESC P 1000 p` would be swallowed as an unknown
    DCS; vt.c detaches tmux as it starts instead (Start and end), so `tmux -CC` gets no windows or
    tabs as iTerm2 gives it.
- **Images take memory twice for the tab shown, and are never trimmed**: each screen of each tab
  keeps up to 32 MiB of decoded Kitty images in libghostty-vt, and the view holds a bitmap of
  every image placed on the shown tab's screen; memory trimming (`compressAll`) leaves both. A tab
  restored from its snapshot gets its images back (libghostty-vt's snapshot encoder writes them
  when built with Kitty graphics, though a comment in its `snapshot/terminal.zig` says otherwise);
  one restored from its plain transcript has text only.
- **A new terminal activity loses the tab being viewed**: it shows the tab its intent's
  `EXTRA_TAB` names (a recreated activity keeps the intent a notification once created it with),
  else the
  service's `focus`, which only a restore after a kill sets and nothing updates, else the last tab,
  never the service's `current`. So when the activity is created again while the service lives
  (Back on Android 11 finishes the root activity; a locale or font scale change recreates it, as
  `configChanges` leaves them out), another tab than the one the user was on comes to the front.
- **A display size change while the terminal is open** keeps the sizes its views were built with
  (tab bar, extra keys, the terminal's font in pixels, `VtView`'s touch and double-tap slops,
  fling velocity and handle margin): `MainActivity` handles `density` itself and builds its views
  once in `onCreate`, so the new sizes apply only once the activity is created again.

## Open questions

- Should a Nerd Font or a CJK fallback font be bundled?
- Should Ctrl+Shift+W ask before closing a running shell, as `×` does, and should "Close other
  tabs" with one other tab name it rather than ask "Close this tab?"?
- Should showing another tab send focus-out and focus-in (mode 1004) to the two tabs?
- Should invisible cells keep their decorations (as now) or hide them as Ghostty's renderer does?
- Should a background tab's progress take another colour than the primary one? Nearly full, it
  looks like the current tab's indicator, told apart only by the numbers' colours.
- Should a status notification name the record's `app` when it has no title, as a key for grouping
  (the specification says it is not a label), rather than the tab's title?
- Should zsh and fish get prompt marks too (Ghostty's bash script, derived from Kitty's, is
  GPLv3; its zsh and fish scripts' licences were not checked), and should a background tab's
  progress and status be dropped at the next prompt mark rather than when the foreground group
  changes?
- Should a clipboard read from a tab not in front wait until the tab is shown rather than get an
  empty clipboard, and should the question offer to remember the answer for the tab (Ghostty asks
  every time)?
- Should the menu's Reset terminal also drop the tab's progress and status records, as a program's
  RIS does?
- Should a mouse wheel or touchpad scrolling the viewport go by pixels and by the event's size
  (high-resolution wheels, touchpads) rather than 3 rows a scroll event?
- Should a clipboard question left open when the activity is destroyed be answered then? Until
  the tab's queued reads are answered it asks for no later read.
- Should typing in the search bar select the newest match at once, as Chrome's find bar does,
  rather than wait for a step, as Ghostty does? And should the extra keys act on the search text
  while it has the keyboard, rather than on the terminal?

- Should a mouse's middle button paste the clipboard (Ghostty's `middle-click-action`; Android has
  no primary selection), and should a left-button selection copy on release (Ghostty's
  `copy-on-select`) or bring up the menu?
- Should a double click on a URL, or a right-click on a link, select the whole link, as Ghostty
  does, and should the extra keys' CTRL make Ctrl+click and the hand pointer work?
- Should a cursor shape changed while a full-screen program shows reach the primary screen too?

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
  alternate-scroll arrows; wheel events to a mouse-tracking program not tried), not with a finger;
  pixel scrolling with `input motionevent` (a held drag shifted by part of a row, the overscan row
  shown below, settling on release) and a fling settling on a whole row. Sideways scrolling was
  not tried on a device (API 34's `input` has no scroll command); only its encoding (buttons 6 and
  7 in SGR) on a macOS build of vt.c. The long-press word drag and the blinking default cursor
  were tried on the emulator.
- Command timing on a macOS build of vt.c fed ash's and bash's marks, and in the app on that
  emulator with busybox ash and bash 5.3 (android/service has the notifications); the log lines
  (an unknown OSC and CSI) were seen in logcat there.
- The bell's vibration on real devices and with haptics turned off; the copy toast Android 12
  and earlier get for a program's clipboard write (only tried on API 34, where the system shows
  its own); how tab notifications look and alert outside an API 34 emulator (there Android
  bundled them with the ongoing notification, so the first tap expands the bundle).
- The scrollback's rows and memory at each limit on a device (the figures above come from vt.c on a
  macOS build of libghostty-vt 1.4.0 fed 30,000 lines, not from Android or 16 KB-page devices);
  the OSC 5522 limit was likewise tried only there. The default colours, DA1 and reverse video
  were also tried in the app on a Pixel 4a API 34 emulator.
- Scrollback compression on 16 KB-page and real devices: it was tried only on a Pixel 4a API 34
  emulator (4 KB pages; a full 45-column scrollback of `ls` and `seq` output went from 12 pages,
  about 4.7 MiB, to about 0.6 MiB resident, a pass taking 1.5–8 ms in steps of at most 5 ms, and
  output that never paused was compressed every 5 s), with scrolling back, a rotation, saves and a
  trim; the 16 KB-page
  emulator did not boot beside it. How a pass's steps and the scheduling thread show in battery
  use is unmeasured.
- Progress and status from real programs (cargo, systemd, AI coding CLIs): tried only with
  `printf` sequences on a Pixel 4a API 34 emulator (the progress line in each state, background
  tabs' marks, notifications, a kill from another tab, seeing a tab, the 15 s timeout, Chinese
  strings, TalkBack descriptions read through uiautomator, not heard).
- Line and rectangular selection and the search bar were tried only on a Pixel 4a API 34 emulator
  (taps from `sendevent`, drags and keys from adb, Gboard in English), so Back went through
  `onBackPressed`: the Android 16 callback is untried. How long searching a full scrollback takes
  was measured only on a macOS build of libghostty-vt 1.4.0 (10,000 lines of 80 columns: about
  10 ms over three frames for 38,544 matches, then 0.5 ms a frame), not on a device.
- Double- and triple-tap drags through the selection gesture were tried only on that emulator with
  `sendevent` (a word drag right and onto the rows below, a line drag down, a plain double tap, a
  long press dragged up and left, each with the menu after lifting), and the gesture's words,
  lines, drags over blank rows and the pressed cell following output in a macOS build of vt.c; not
  with a real finger, on real devices, or held past the grid's edges (the autoscroll). The search's
  marks along the scroll bar were seen there with three matches (the selected one orange under the
  thumb, all gone once the bar closed); taking the rows of 57,816 matches over 9,636 rows took
  0.8–1.2 ms on a macOS build (M2 Pro), not timed on a device, and the marks were not watched while
  rows leave a full scrollback.
- Shell integration in the app only with busybox ash on a Pixel 4a API 34 emulator (`×` at the
  prompt and while `sleep` ran, a new tab from `/etc`, Select output, Ctrl+Shift+↑ and ↓ from adb
  over long outputs and on the alternate screen, the keyboard hidden and shown and a rotation with
  half-typed input), plus vt.c on a macOS build of libghostty-vt fed recorded ash and bash bytes;
  bash's marks were seen only in a Docker container (Ubuntu 26.04's bash 5.3), not in the app on
  Debian, and not with real devices, hardware keyboards or TalkBack.
- Kitty graphics with real programs beyond chafa (`chafa -f kitty`; its own detection picked
  symbols): yazi, timg, `kitten icat`, tmux passthrough with placeholders, and how programs that
  probe for the Glyph Protocol fall back. Tried on a Pixel 4a API 34 emulator with chafa and
  `printf` sequences (fixed placements above and below text, placeholders, a two-frame animation,
  scrolling back, a tab switch, the alternate screen, a 1080 × 1400 PNG: scrolling over it took
  about 2.5 ms a frame, at most 12 ms), and on a macOS build of vt.c (queries, refused media,
  PNG with alpha, oversized and corrupt PNG, deletion, scrolling, full reset); not on real devices,
  16 KB-page devices, or with the 32 MiB storage filled.
- The port to libghostty-vt 1.0.0 was checked on a Pixel 4a API 34 emulator (tabs saved by the
  copy in Ghostty 1.4.0 coming back from their transcripts, every text style, DECIC, a 1.0
  snapshot's round trip, keys) and by vt.c's macOS harnesses run against both versions (the same
  output but for the version, slightly fewer scrollback rows for the byte limit and line drags
  over blank rows); mouse, images, search and selection on a device only through those harnesses,
  and not on 16 KB-page or real devices.
- Taps on the input were tried on a Pixel 4a API 34 emulator with adb taps (busybox ash: before,
  past and on the prompt; bash 5: across a soft-wrapped command and on Chinese characters recalled
  from history; a tap while `cat` ran sent nothing) and on a macOS build of vt.c fed ash's, bash's
  and fish-style marks (wide characters, the cursor key mode, both `click_events`, resets, the
  alternate screen); not fish itself, ash with wide characters, real devices or hardware mice.
- The lock cursor was tried on a Pixel 4a API 34 emulator with busybox ash's `read -s` (the lock at
  its prompt, and gone within a check of entering the password while a silent `sleep` ran), and
  the kernel's test of the termios rule in `appine-run`'s rootfs tests; not with sudo, passwd, ssh
  or doas prompts, CJK IMEs composing at one, or real devices. The IME's password mode was seen
  there with Gboard (`dumpsys input_method`: `inputType` 0x81 and `imeOptions` with
  `IME_FLAG_NO_PERSONALIZED_LEARNING` at the prompt, back to 0x80001 after; its password layout),
  not with other IMEs or hardware keyboards.
- Mouse pointers were tried on a Pixel 4a API 34 emulator with a virtual mouse made by the system's
  `uinput` command (`input mouse` sends no hover events), reading `dumpsys input`'s pointer type
  and screenshots, which show the pointer: the I-beam, `wait` and back set by a program while the
  mouse rested, a hand over URLs, hover motion still reported under mode 1003; and the frame's
  shape byte, a full reset keeping it and a restored snapshot resetting it on a macOS build of
  vt.c. Not with real mice, touchpads, ChromeOS, DeX or desktop windowing, or every shape.
- Mouse selection was tried on a Pixel 4a API 34 emulator with a virtual mouse made by the
  system's `uinput` command, with Ctrl, Alt and Shift keys on the same device (cell drags, double
  and triple clicks, a Ctrl+Alt rectangle, Shift+click extending, right-clicks inside and outside a
  selection and on blanks with Copy and Paste from the menu, the hand pointer only with Ctrl, a
  plain click not opening a URL and Ctrl+click opening Chrome, a mouse-tracking `cat -v` getting
  the plain drag and Shift+drag selecting), and vt.c's functions on a macOS build (clicks, timing,
  distance, thresholds, rectangles, extending, releases outside, right-click words, output with
  Ctrl); not with real mice or touchpads (their swipe and pinch classification), styluses, Android
  11–13, DeX, ChromeOS or desktop windowing, nor a mouse drag's autoscroll on a device.
- The cursor setting was tried there through Settings (Bar and steady, then back), with a program's
  DECSCUSR 2 and 0, and on a macOS build of vt.c (a full reset and the app's Reset terminal keeping
  it); not in Chinese, with a full-screen program showing, or on real devices.
- tmux -CC was tried in the app with Alpine's tmux 3.7c (detached at once, the notice, `tmux ls`
  and a normal `tmux attach`), with tmux 3.5a (Debian) and 3.7c (Alpine) in containers, and on a
  macOS build of vt.c fed tmux's recorded bytes (one byte a read, other DCS strings, a program's
  and the app's CAN); not with other tmux versions or on real devices.
- Snapshot restoring was tried only on a Pixel 4a API 34 emulator (a force-stop and an update
  reinstall, with coloured output, mouse tracking, bracketed paste and a title left on), and the
  encoding, settling and round trip on a macOS build of vt.c; not with images in a snapshot,
  bash's marks, a program on the alternate screen at saving, a library version change or 16 KB-page
  devices. Clipboard reads (Allow and Deny, a shell reading raw input), Clear screen and Reset
  terminal at busybox ash's prompt, and Shift+arrows on a selection (adb key combinations) were
  tried there too; not with real hardware keyboards, bash, or a read from a tab not in front.
