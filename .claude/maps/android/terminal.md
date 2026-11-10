# android/terminal Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/android/terminal.md` for the paired decision history.
>
> Verified: 2026-11-10 (full mapping: 5 research concerns; 25 claims checked by 3 independent verifiers — 19 confirmed and 6 partial, corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-10 (targeted verification only since the full mapping: console state, ⋮'s label, menu key, resize replies, scrollback size, colours, DA1, OSC 5522, progress, status, TN, selection, search, compression, cell reads, Kitty graphics last)

## Responsibilities

`android/terminal` is the terminal screen and the pieces that render the service's sessions:

- `MainActivity`: the screen's layout and status text, the current session and its successor,
  closing tabs, keyboard shortcuts, font zoom, insets and the IME, links, with the tab bar
  (`TabBarView`), the search bar in its place while searching (`SearchBarView`) and the extra keys
  (`ExtraKeysView`);
- the terminal: `VtSession` (a tab: its shell, the terminal, pump, waiter and writer threads,
  what its programs report about their progress and status), `VtView` (drawing with the bundled
  JetBrains Mono, the images programs show, input, selection, search marks, gestures),
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

Outside the module: session ownership, saving, kill reports and where bells, notifications,
program statuses and clipboard writes go (`android/service`); the host console in the kernel (`kernel/tty`); the
notices task, the JNI crate, `guarded!` and the build, including compiling libghostty-vt with Zig
(`android/native`).

## Key types

- `MainActivity` — a `TerminalService.Listener` built in code: a black column of one bar's place
  (`TabBarView`, or `SearchBarView` while searching), a frame with the `VtView`, a status line and
  the `ProgressLine` along its top, and `ExtraKeysView`; adapters `Host : VtView.Host`,
  `Bar : TabBarView.Target`, `Find : SearchBarView.Target` and `Keys : ExtraKeysView.Target`; the
  current session and its index, from which its successor is taken; a tab requested by a
  notification (`EXTRA_TAB`); the IME animation callback `keysFollowIme`; Back's callback while
  the search bar is open (`closeSearchOnBack`, Android 16 and later).
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
  reset), the view showing it (`view`, read on the pump thread), pending restored text and
  messages, its login and the account an `auto` login resolved to (`resolvedLogin`, written by the
  service), the last transcript (whole, also returned while the screen is unchanged), the last
  progress report (`progress`, a `Progress`), the program status records by id (`Status`, least
  recently updated first, at most 64) and the one most in need of the user (`status`), a
  `vt-pump` thread, a `vt-waiter` thread and its own single-thread `vt-writer` executor; `Events`
  (title changed, bell, clipboard write, notification, progress or status changed, finished) go
  to the service on the main thread. Its companion holds
  libghostty-vt's progress states, status states and kinds, and `describe` (a status in words).
- `VtView` — the view: the attached `VtSession`, cell metrics, four paints, a mirror of the screen
  (codepoint, colours, attributes with search marks, grapheme clusters per cell, each row's
  selected columns), a backing bitmap of the grid, the modes, scroll position and selection
  bounds and shape of the last frame, whether a search runs and its count as last reported, the
  last frame's image placements (`Placement`, a pool reused across frames) and the bitmaps of the
  images they show by id with the generation each holds (`Picture`), the animation's redraw, the
  IME connection, gestures (with the taps in a row) and their drag states, the fling's
  `OverScroller`, selection handles and the floating menu, the cursor's blink timer and the bell's
  last vibration. `Host` (the activity): `onGrid`, `onInput`, `onZoom`, the extra keys' `ctrl`,
  `alt`, `shift` and `modifiersUsed`, `onLink` (opens a link in another app) and `onSearchCount`.
- `ProgressLine` — a 2 dp view over the top of the terminal showing the current tab's progress
  report; `Colors` (primary, error, muted by progress state); its companion draws progress into any
  rectangle (`draw`, a fill or a sweep) and gives a report's fraction (`fractionOf`).
- `VtGlyphs` — box drawing (U+2500–U+257F), block elements (U+2580–U+259F) and the Powerline
  triangles (U+E0B0–U+E0B3) drawn as shapes.
- `VtKeys` — Android key codes to libghostty-vt's `GhosttyKey` numbers (`of`, 0 when unknown),
  modifier bits (Shift, Ctrl, Alt, Meta as Super; no lock bits), actions, `isModifier`.
- `VtNative` — 28 `@JvmStatic external` functions on the native library.
- In `vt.rs`: `Tab` (its own handle, the kernel's handle, console number, the terminal under a
  mutex, a `dirty` flag) in a table `TABS` keyed by handles from 1, never reused; `TermGuard` (a
  tab's locked terminal, which schedules a compression pass when it is released); `Term` (owns the
  `AppineVt`, freed on drop); `RawEffects`, laid out as vt.c's `AppineVtEffects`, turned into
  `Effects` (with the `StatusReport`s decoded from vt.c's queue); the compression schedule
  `COMPRESS` (a `Due` per tab: when to look at it next and when it was scheduled), served by the
  `vt-compress` thread (`compressor`).
- In `vt.c`: `AppineVt` (the terminal, its render state, key and mouse encoders, the grid and
  cell pixel sizes, reply, frame and scratch buffers, one text buffer shared by title, copy, link
  and transcript, the title-changed flag, the synchronized-output hold, the tracked anchor of a
  selection drag, the last reported mouse cell, the effects since they were last taken: bells,
  the last clipboard write, the last notification, the last progress report and every program
  status report, queued in a byte layout vt.rs reads; `AppineVtEffects` is how they are handed
  out; the search: a `GhosttySearch` while one runs, scratch for the viewport's matches, and
  each viewport cell's search mark for this frame and as last sent; the compression state: a pass
  scheduled, the activity token last seen, a read that restored compressed pages since the last
  pass; whether the last transcript is still the screen's text; and the render state's image list
  as last read, in drawing order (`Placement`), with its placements generation). Process-wide: the
  PNG decoder installed into libghostty-vt's sys interface with the first terminal.

## Public entry points

- Launcher activity `MainActivity` (`singleTask`, `adjustResize`, `Theme.Appine.Terminal`, handles
  most configuration changes itself, the dark mode and density included), started with
  `EXTRA_TAB` (a `VtSession.id`) by a tab's notification; listener callbacks `onProgress`,
  `onMachineReady`, `onSessionsChanged`, `onBell`, `onTabStatus`, `onSettingsChanged`,
  `onNotice`, `onRestarting`, `onExitAll`.
- `LicensesActivity`, opened from Settings.
- `VtNative`: `create`, `print`, `pump`, `title`, `frame`, `image`, `resize`, `key`, `paste`,
  `mouse`, `focus`, `scroll`, `selectWord`, `selectLine`, `selectGrab`, `selectTo`,
  `selectRectangle`, `selectAll`, `selectClear`, `copy`, `link`, `search`, `searchSelect`,
  `searchEnd`, `message`, `transcript`, `compressAll`, `destroy`; `pump` runs on `vt-pump` and calls
  `VtSession.onOutput`, `onTitle`, `onBell`, `onClipboard`, `onNotification`, `onProgress`,
  `onStatus` and `onForeground` back; `compressAll` runs on the service's worker, the rest on the
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
  `onRestarting` detaches and shows "Restarting…".
- Showing a tab sets it as the service's `current` (which takes its notification away), attaches
  it to the view (`VtView.attach`) and renders the bar; `onRestarting` and `onDestroy` detach it,
  so sessions never keep a destroyed Activity's view.
- Closing a tab hangs it up at once; when it was the current one, the tab after it, now in its
  place, is shown (the one before when it was the last). Closing the last tab while the kernel runs
  finishes the activity (the service stays).

### The tab bar

- One 32 dp row in any window: a scrolling row of tabs across its whole width, with `×` (closes
  the current tab; hidden while there is none), `+` (opens a session while the kernel runs) and
  `⋮` (a menu: "Find", disabled with no current tab, "Close other tabs", disabled when there is no
  other, "Close all tabs", disabled with no tabs, and Settings) over its end, every tab and button
  a 32 dp square with its label centred. The tabs pass under the buttons, which hide them, and an end padding as wide as the
  buttons lets the last tab scroll out from under them; while more tabs lie beyond, the row fades
  out over 32 dp ending 12 dp inside `×`, in the blank before its glyph (the scroll view's right
  fading edge; the left edge does not fade).
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
  it reported in parentheses ("Waiting for your approval", "Done", "Failed", "Paused", "Working",
  with the percentage when there is one), and the `×`, `+` and `⋮` buttons by name (`⋮` as "More
  options").
- `×` and the menu's "Close other tabs" and "Close all tabs" close at once when no shell they
  close is running, else after a confirmation dialog ("Close this tab?" for one tab, else "Close N
  tabs?"); several tabs are closed with the current one last, so no other tab is shown (and its
  shell started) on the way, and closing all ends like closing the last tab. Ctrl+Shift+W closes
  the current tab without asking, even while its shell runs, and in an ended tab any input with a
  line break (Enter) closes it.
- Its dialogs (close, notices, the paste confirmation) and its menu (⋮, a `PopupMenu` placed like a
  toolbar's overflow menu) are Material 3 in the theme's dark colours, from the wallpaper on
  Android 12 and later (`DynamicColors`). The tab bar, the extra keys and what lies under the
  system bars are black like the terminal; their labels, buttons and the status text take the
  muted on-surface-variant colour, and the current tab's number and indicator the primary one.

### Hardware-keyboard shortcuts

- Taken by the activity (`dispatchKeyEvent`) before the terminal sees them, together with the
  matching key release, when the key event (from a hardware keyboard, or an IME sending key
  events) has Ctrl without Alt or Meta: Ctrl+Shift+T opens a tab,
  Ctrl+Shift+W closes the current one, Ctrl+Shift+F opens the search bar (these once per press;
  repeats are swallowed), Ctrl+Tab and
  Ctrl+Shift+Tab show the next and the previous one, going round and repeating while held (and
  swallowed when there is nothing to switch to). They also work with the search bar's text
  focused, and are listed in the system's keyboard shortcuts screen (Meta+/; the tab ones under
  Tabs, Find under Terminal). The extra keys' modifiers do not make shortcuts: with them these keys
  reach the program.

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
  bar shows only when it scrolled). The count reads "selected/total", the selected one counted
  from the newest, 0 before a step; it grows as a long scrollback is searched over several frames.
- `×`, Esc in the text, or Back (after Back has hidden the keyboard) closes the bar: the search
  ends and its marks go, the tab bar comes back, the terminal takes the focus and the viewport
  stays where the search left it. Showing another tab (Ctrl+Tab while searching) or a restart
  closes it too, as a search belongs to the tab it was opened in.
- Matches are drawn as Ghostty draws them: black text on yellow, the selected match on orange, the
  second cell of a wide character with its first. Tapping the terminal while the bar is open gives
  the keyboard back to the terminal and leaves the bar and its marks; the extra keys always go to
  the terminal, the bar's text focused or not.

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
  opens a console of the grid's size and spawns the login; `VtNative.create` makes the terminal
  (scrollback limited to 10,000 lines or 10 MiB, whichever is reached first: the lines up to about
  120 columns, the bytes beyond; default colours white on black and no default cursor colour, so
  the cursor follows the foreground as in Ghostty; grapheme clustering, mode 2027, on by default as
  in Ghostty; Kitty graphics on, with 32 MiB of decoded images per screen (Ghostty's default is
  320 MB) and only data sent in the escape sequences (the file, temporary file and shared memory
  media, which would name the host's files and objects, answer `EINVAL: unsupported medium`, so
  programs fall back to sending the data); the Glyph Protocol off, so it is not advertised and
  programs that probe fall back to text; the terminfo name `xterm-256color`, the
  `TERM` the console's login gets, which XTGETTCAP's `TN` reports); restored text and queued
  messages are printed into it; then the writer, the pump and the waiter start.
- If opening the console or spawning fails, the reason is printed, the console is closed and the
  tab counts as ended (127), with no "Process completed" line and no writer. If the terminal cannot
  be made (the kernel is gone, libghostty-vt fails, a panic), the tab ends (127) without a word,
  and later messages for it are refused.
- Restored text replaces whatever was pending; a message shown before the terminal exists is
  queued as a plain yellow line, not inserted as below.
- The pump reads the console into a 64 KB buffer, feeds the terminal under the tab's lock, writes
  the terminal's replies (DA, DSR, XTVERSION, size reports, pastes' bytes) back to the console
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
  decoded; with the callback set it also answers the protocol's `?` query). Clipboard reads have
  no callback: OSC 52 `?` goes unanswered, OSC 5522 reads are refused, and Kitty paste events are
  never sent, as with `clipboard-read = deny`. What the app prints itself (`VtNative.print`) asks
  nothing: its replies and effects are dropped. Where they go is the service's choice
  (android/service): the bell of the tab in front vibrates through `VtView.bell`, other bells (at
  most once a second) and notifications from tabs not in front become the tab's notification, and
  clipboard writes always reach the clipboard.
- Progress and status (`VtSession`, main thread): a progress report replaces the last one (a
  removal clears it) and lasts 15 s without another, as in Ghostty. Status reports of a read are
  applied together, as the specification keeps them: one record per id, a report replacing its
  record whole, a clear removing the record and those beneath it (`build` takes `build/test`; an
  empty id, as after a full reset, takes all), at most 64 records, the one updated longest ago
  dropped; text direction controls are removed from titles and messages. `status` is the record
  most in need of the user: blocked, then error, done, working, idle, the most recently updated
  among equals. A change of either (by value) is reported to the service (`onStatusChanged`).
- The pump stands in for shell integration's prompt mark: after a read that carried a progress or
  status report it notes the console's foreground process group (`Machine::console_foreground`),
  and on a later read where another group (or none) has the foreground it calls `onForeground`,
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
  since the last transcript (no output, message or resize), or when formatting fails, it returns
  the last transcript taken on the primary one ("" when none was).
- Kill reports go through `VtNative.message` (a yellow line inserted above the cursor line: a CAN
  first when the parser is not at ground (`VT_GROUND`, which includes the UTF-8 decoder), so a
  program that died inside an OSC, DCS or APC string does not swallow it; then as many line feeds
  as the text needs rows, up again, save the cursor, insert lines, write, restore and move down,
  dropping any replies; skipped on the alternate screen) or are queued until the terminal exists.

#### Scrollback compression

- libghostty-vt compresses the primary screen's cold pages (wholly in the scrollback and outside
  the viewport; the alternate screen keeps no scrollback) to about a fifth of their 400 KiB, and
  restores a page when something reads it: drawing a viewport scrolled back over it, the formatter
  (transcripts, copies), select all (which reads the oldest and newest pages) and a tapped link's
  line; searching restores nothing. The scrollback limits count pages uncompressed, so the same
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
  compressed again afterwards.

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
  rows, cursor and colours come from the capture while the header's modes, selection and scroll
  position are read live.
- The view asks for a full frame after a regrid, a new bitmap, new default colours (OSC 10 or 11,
  reverse video), a change of the wide-cell setting, attaching another tab or the attached tab's
  shell starting; a frame captured at an old size is dropped and a full one requested. Attaching
  also resizes the tab's terminal and console to the view's grid when it differs.
- The header also carries mode flags (mouse tracking, alternate screen, alternate scroll 1007,
  focus events 1004, a selection exists, it is a rectangle, the search is still catching up), the
  selection's first and last cells in the viewport (a rectangle's top left and bottom right
  corners), the scroll position libghostty-vt reports for the scroll bar (the scrollable area in
  rows, the viewport's first row in it and the rows it shows) and the search's count (matches so
  far, the selected one's place from the newest); each row carries its selected columns (a
  rectangle's columns on each of its rows) and each cell its search mark (attributes bits 14 and
  15). `VtView` reads neither the header's dirty byte nor the focus-events flag, and reports the
  count to the activity when it changes.
- Cell metrics: the width is the font's advance rounded to whole pixels and the text size is
  adjusted so the advance equals it; the height is the ceiling of ascent+descent, with the baseline
  centred; the light line width is the font's underline thickness. The wide scale is two cells over
  the advance of "水" (the system's CJK fallback, one em, so 1.2 with JetBrains Mono; a font's own
  ideograph when it has one), at least 1, or 1 with the setting off.
- `onDraw` fills the view (the margin past the last whole cell included) with the default
  background, then each dirty row is repainted into the bitmap under a clip: default background,
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
  but the bar cover two cells on a wide cell; in the OSC 12 colour, else the default foreground), the IME's composing text (shifted
  left at the right edge) and the theme's text selection handles are drawn over the bitmap each
  draw.
- The cursor blinks (shown and hidden every 500 ms, as Android's text fields) while the frame
  header says the program asked for blinking (DECSCUSR, mode 12; libghostty-vt's default is off)
  and the view has the focus in a focused window; otherwise no timer runs. Input and a cursor
  move show it again for a whole period.
- The bell of the tab shown vibrates once through `performHapticFeedback(REJECT)` (a double
  click, subject to the system's touch feedback setting), at most once per 100 ms.
- The current tab's progress report is drawn by `ProgressLine`, 2 dp high over the top of the
  terminal's first row, as Ghostty draws it: filled from the left to the percentage in the primary
  colour, the error colour for an error, the muted colour while paused (all of it for a pause
  without a percentage); without a percentage, or indeterminate, a quarter of its width sweeps
  across and back (1.2 s each way, ease-in-out, timed by the clock) over a track at 30% opacity,
  redrawn every frame. It fades in and out in 150 ms and slides to a new percentage in 200 ms;
  showing another tab puts that tab's progress there at once. It takes no touches and is hidden
  from accessibility.

#### Input

- Keys: mapped keys go through `VtNative.key`, which encodes them in the terminal's current modes
  (cursor and keypad modes, Kitty keyboard protocol, including releases from `onKeyUp` and lone
  modifiers, which are sent only when the encoder emits something). An unmapped key that types a
  character goes as text without Ctrl or Alt, and to the encoder as an unidentified key with that
  text with them; one that types nothing goes to the system. System keys (Back, Home, volume,
  media) go to the system, except the menu key, a hardware keyboard's context menu key, which goes
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
  motion included) first scrolls the viewport back to the bottom and, but for a fling's own wheel
  events and arrow keys, stops a fling, then goes through the host to the tab's `vt-writer` thread
  (`Native.write`).
- IME: committed text is sent at once (`\n` as `\r`, a lone `\n` as Enter, several lines as a
  paste, which covers the IME's clipboard); finishing a composition types it; the editor action is
  Enter; composing text is only drawn at the cursor; the editable is kept empty so Backspace with
  nothing composed arrives as `deleteSurroundingText` and becomes as many DELs (text after the
  cursor is ignored). `requestCursorUpdates` is answered with the cursor and composing text's
  position (`CursorAnchorInfo`), again whenever the cursor or composing text moves while the IME
  monitors it.
- Paste (`VtNative.paste`, through `ghostty_terminal_paste`): bracketed with mode 2004, else line
  breaks become carriage returns; unsafe control bytes become spaces. Text that could run commands
  (line breaks without bracketed paste, or the bracketed terminator) is refused, and the view asks
  in a dialog with a preview (300 characters) before pasting it anyway.
- Focus: window focus changes are sent as focus events when the program asked (mode 1004); showing
  another tab sends none.

#### Gestures and selection

- A tap ends a selection or its menu; otherwise, when the program does not track the mouse and the
  tap is on a link, it opens the link instead of the IME; otherwise it opens the IME and, when the
  program tracks the mouse, also sends a left click (press and release at the tap's pixel position,
  in the program's format). A tap that stopped a fling does nothing else. Taps act at once:
  `VtView` counts taps in a row itself (each press within the system's double-tap timeout of the
  last lift, the two lifts within its double-tap slop), so without mouse tracking the second tap
  selects the word under it and the third the line (with the menu), and with it every tap is a
  click, for the program's own double and triple clicks; a fourth tap counts as a first, and so
  does a tap after one that opened a link. The line is Ghostty's triple-click line
  (`ghostty_terminal_select_line`): the soft-wrapped line with blanks at its ends left out, cut
  where an OSC 133 prompt mark starts or ends a part of it. A second or third tap on blanks acts as
  a single tap.
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
- A vertical drag, or a mouse wheel or touchpad scroll (3 rows per scroll event), scrolls by whole
  rows: wheel events when the program tracks the mouse, arrow keys on the alternate screen with
  alternate scroll (1007, on by default), else the viewport.
- A fling goes on scrolling that way, slowing down as a ScrollView does (Android's fling physics
  in pixels, taken as whole rows like a drag): the viewport until it reaches either end, wheel
  events and arrow keys for at most a screen of rows. It stops when the way to scroll changes
  (mouse tracking, the alternate screen, alternate scroll), when the tab has no terminal, at a
  touch, at any other input to the program (not the wheel), when the window loses focus and when
  another tab is shown.
- The viewport scrolling (a drag, wheel, fling, a selection's autoscroll, input returning it to the
  bottom) shows the theme's scroll bar at the right edge, faded out after a moment as a
  ScrollView's; output alone does not show it, and with no scrollback (the alternate screen) it is
  never drawn.
- A long press (ignored during a pinch, with a long-press haptic) selects the word under it
  (`ghostty_terminal_select_word`, installed as the terminal's selection) and keeps it as the
  anchor: moving on extends the selection from the far side of the word to the cell under the
  finger. On blank cells it selects nothing and shows a menu with Paste (with a clipboard), Select
  all and, on a link, Open link. When the program tracks the mouse, the long press selects the
  same way, but what it becomes waits for the finger: lifted, it keeps the word (or the point
  menu); moved on past the touch slop, the selection is dropped and the program gets a left-button
  drag instead (press at the long-pressed point, motion while moving, release where the finger
  lifts).
- A mouse or touchpad (not a finger) pressed while the program tracks the mouse goes to it as it
  is: the first button pressed (left, right or middle; a touchpad tap is left) with modifiers,
  motion while it is held and its release, bypassing the gestures (it clears a selection and takes
  focus without opening the IME); without tracking a mouse acts like a finger. Hover (no button)
  is sent as motion while the program tracks the mouse. Which of these the program gets is the
  encoder's choice by its mode (1000 none of the motion, 1002 motion with a button held, 1003
  all), and vt.c skips motion within the cell of the last event it encoded, press or release
  included (except in SGR-Pixels format).
- The two handles (start below the first cell, end below the last, when in the viewport; a
  rectangle's top left and bottom right corners) drag the selection's ends; the other end is the
  anchor, kept as tracked grid references so output does not move it, and a rectangle stays one
  with the anchor and the finger's cell at opposite corners. A handle that would stick out of the view at the left edge (start) or right edge
  (end) is drawn with the other handle's drawable, on the other side of the same point, so both
  stay on the screen, and the handles' areas (with an 8 dp margin) are excluded from the system's
  back gesture while they are shown, so a drag from the edge moves the handle. The finger keeps
  its offset from the dragged cell; held above or below the grid (a handle or a long-press word
  drag), the viewport scrolls one row every 50 ms and the selection follows.
- A floating action mode (`TYPE_FLOATING`) appears once the next frame is drawn after a selection
  drag or long-press word ends, a double-tap word or Select all (at once, pinned at the press
  point, after a long press on blanks or a mouse-mode long press that selected nothing), and offers
  Open link (when the long press that began the selection was on a link, also in mouse mode), Copy
  (with a selection), Paste (with a clipboard), Select all (scrollback included), and with a
  selection Rectangle, or Lines for a rectangle, with the system's own labels but Open link's and
  those last two, placed over the selected rows and moved with them on every draw; a frame without
  a selection closes it. Open link ends the selection and opens the link. Rectangle makes the
  selection's first and last cells opposite corners of a rectangle; Lines makes it linear again
  from the rectangle's top left to its bottom right corner; the menu comes back with the other
  item. Copy takes the selection as plain text with soft wraps joined (not in a rectangle, whose
  rows are lines of their own) and trailing blanks trimmed (as Ghostty copies), puts it on the
  clipboard and ends the selection.
- Pinch zoom changes the font by 1 dp beyond a 0.9–1.1 scale band, within 6–32 dp, which regrids
  and resizes the terminal and then the console (SIGWINCH); the activity saves the new size.
- A resize (`VtSession.resize`, from attaching and every regrid) takes the replies the terminal
  made for it, the in-band size report of a program using mode 2048, and writes them through the
  tab's writer at once, rather than leaving them for the pump's next read. A search keeps its
  selected match through a resize (the keyboard showing or hiding is one).

### Licenses

- The build writes `notices/` (Appine's licence; Fuchsia's for the line discipline; Ghostty's
  libghostty-vt, uucode, simdutf, Highway, Wuffs, Zig's compiler_rt and JetBrains Mono; Alpine
  packages from the rootfs database, the doas package, Android libraries, Rust crates, SQLite,
  then full texts); a full text is monospace, selectable and sized so 74 columns fit (at most
  13 sp), scrolling sideways when it does not.
- The page (`page.xml`): a `MaterialToolbar` (title, back arrow) in an `AppBarLayout` that lifts
  while the content below it is scrolled, over a `NestedScrollView`; edge to edge (wallpaper
  colours from Android 12 on through `DynamicColors`), with the bar padded under the status bar
  and the scroll view clear of the navigation bar. The toolbar shows the entry's title over its
  full text; the back arrow and back both return to the list, then close.
- It follows the system's dark mode: a change recreates the activity, which shows the same full
  text again (saved state). A rotation does not recreate it, so a full text shown keeps its size.

## Dependencies (inbound / outbound)

### Inbound

- `TerminalService` (creates and holds the `VtSession`s: `setRestored`, `showMessage`,
  `transcriptText`, `title`, `isRunning`, `console`, `login` and `resolvedLogin`, `id`,
  `status`, `statusSeen`, `describe`, `release`; `current`, `focus`, `sessions`, listener
  callbacks, the notification intent with `EXTRA_TAB`), and its memory trimming
  (`VtNative.compressAll`); `SettingsActivity` (licenses).

### Outbound

- `TerminalService` (`createSession`, `removeSession`, `current`, `fillWide`); `Native`
  (`openConsole`, `spawn`, `write`, `closeConsole`, `waitFor`), hence the kernel's host console;
  `login.sh`; `assets/notices`.
- Material Components 1.14.0 with AppCompat and androidx.activity (both themes, `LicensesActivity`
  and the page layouts, `MainActivity`'s dialogs and menus, `VtView`'s paste confirmation);
  androidx.core's insets and animation compat.
- libghostty-vt 1.4.0 (the terminal state, render state, key and mouse encoders, paste, focus
  encoding, selection with word and line derivation, search, plain-text formatting, title and
  width, progress and program status reports, scrollback compression, Kitty graphics with their
  render-state image list and animation), and the Wuffs PNG decoder it carries, linked into
  `libappine.so` (`android/native`); the system's libjnigraphics (bitmap locking for images'
  pixels); `Machine::read_console`,
  `write_console`, `resize_console` and `console_foreground` (kernel/tty) and the
  bridge's `guarded!`, handle table and string helpers, used from `vt.rs` directly; the system
  clipboard; the app the system picks for a link (a browser, or a viewer or the Files app for a
  guest file through `Documents.fileLink`, android/documents); JetBrains Mono 2.304 (variable,
  upright and italic) in the assets, and the system's fallback fonts for everything else (CJK from
  Noto Sans CJK on AOSP images); the wide-cell setting `TerminalService.fillWide`.

## Invariants and gotchas

- A kill report opens its room with line feeds before moving back up, so the cursor's line moves
  down with the cursor; this needs the parser at ground, which the CAN ensures. A program that died
  in the middle of a UTF-8 sequence leaves a U+FFFD at its last output position: the CAN ends the
  sequence and the decoder replaces it. The report's cursor save overwrites one the program had
  saved.
- libghostty-vt's mouse encoder deduplicates motion only when asked (off by default) and forgets
  its last cell whenever `setopt_from_terminal` or the size option is set, which `appine_vt_mouse`
  does for every event; so vt.c keeps the last cell itself, updated only when an event was encoded.
- `GestureDetector` with a double-tap listener (any `SimpleOnGestureListener`) gives the second
  tap of a double tap to that listener: `onSingleTapUp` never sees it and a drag starting there
  does not scroll. `VtView` therefore uses a plain `OnGestureListener`.
- The console opens lazily, so restored tabs that were never shown have no console and no shell.
- `openConsole`, `spawn` and `closeConsole` run on the main thread and take the kernel's BKL, as
  does a resize (`VtNative.resize`, which resizes the kernel console from `vt.rs`; `Native.resize`
  is no longer used by the app); input is written on each tab's own thread, so a program that stops
  reading blocks only its tab.
- Console numbers are freed at close and reused lowest-first by the kernel, and a close carries no
  generation, so closing a stale number closes whichever console now has it.
- Programs never receive Ctrl+Shift+T, Ctrl+Shift+W, Ctrl+Tab or Ctrl+Shift+Tab typed on a
  hardware keyboard with the modifiers held (the tab shortcuts); the release of the shortcut's key
  is swallowed too, so the Kitty protocol sees no release without a press. Other releases are
  never matched to presses: Ctrl+Shift+C/V's, or Esc's when the IME took the press, reach the
  program alone.
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
  last frame's rows), shown with `awakenScrollBars` wherever `VtView` moves the viewport. A view
  built in code has no scroll bar until it is given a thumb (`verticalScrollbarThumbDrawable`, the
  theme's `scrollbarThumbVertical`) and fading is turned on explicitly; `View` also flashes it
  each time the window becomes visible again.
- A fling runs in `computeScroll`, before each draw takes its frame, so its end checks (top row,
  bottom) see the frame of the step before: it stops one frame after reaching an end.
- `VtView` turns off the default focus highlight: focused outside touch mode (after hardware or
  injected keys) the system would cover the whole view with a translucent grey.
- Without a terminal (`tab` 0) `onDraw` draws the bitmap as it was last painted.

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
  key numbers in `VtKeys` are those of Ghostty 1.4.0's `key/event.h` and change with that header.
- Since libghostty-vt 1.4, `GHOSTTY_TERMINAL_OPT_USERDATA` takes the pointer itself, not its
  address (older code passing `&ptr` crashes in the reply callback).
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
  state between two of them notifies no one. A full reset arrives as a clear of every record and a
  removal of the progress, before libghostty-vt's reset callback (unset here).
- Without default foreground and background colours libghostty-vt treats them as unset: OSC 10
  and 11 queries go unanswered, reverse video (DECSCNM) swaps nothing, and a colour set with OSC 10
  or 11 alone does not reach the render state. With them, an OSC 12 query without a cursor colour
  answers the foreground. A full reset (RIS) resets the palette but keeps colours set with OSC 10,
  11 and 12, as Ghostty does.
- libghostty-vt as a library turns the Glyph Protocol on, answering `fmt=glyf` to its query,
  though the C API gives no access to registered glyphs; vt.c turns it off at create. A full reset
  keeps that setting, the Kitty storage limit and the allowed media (the alternate screen takes
  the primary's), so they stay as set. Its Kitty media default is data in the escape sequences
  only; vt.c sets it again so a library change cannot open the file media, whose paths would be
  the app's own files rather than the guest's.
- As built for an app (`-Demit-lib-vt`), libghostty-vt has no PNG decoder unless one is installed
  (`ghostty_sys_set`, process-wide), though libghostty-vt.a contains Wuffs: vt.c's decoder calls
  Wuffs's C functions in the archive, compiling against the header from Ghostty's Zig packages.
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
  call takes fresh ones; the anchor of a drag is tracked. A changed selection makes the next frame a
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
- The C API's line selection defaults to ignoring prompt marks; vt.c turns
  `semantic_prompt_boundary` on, as Ghostty's own triple click has it.
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
- **libghostty-vt features not wired**: no callbacks for OSC 7 working directories or OSC 133
  prompts (both also need shell integration, which Ghostty ships for bash, zsh, fish, elvish and
  nushell but not ash, and Appine installs none; the progress and status records go by the
  foreground process group instead of the prompt mark); tabs are restored from plain text, not
  libghostty-vt snapshots, and a snapshot keeps a sequence the output stopped inside only with
  continuation tracking (`CONTINUATION_MAX_BYTES`, off by default and needed before that output),
  which vt.c leaves off; ENQ, the title report (CSI 21 t), XTCHECKSUM and window resizing (CSI 8 t) are left unanswered;
  clipboard reads are refused where Ghostty's default (`clipboard-read = ask`) asks, and wiring
  them would turn the IME's multi-line commits, pasted as `GHOSTTY_PASTE_SOURCE_CLIPBOARD`, into
  Kitty paste events for programs using mode 5522.
- **More of libghostty-vt left unused**:
  - selection: `select_word_between` (a word drag that extends by words), `selection_adjust`,
    `select_output` (needs OSC 133), and the selection gesture state machine (click counts to
    cell, word, line or output selection), for which `VtView` has its own taps and long presses;
  - drawing: overscan (rows captured beyond the viewport, to scroll by pixels; drags and flings
    move whole rows); the default palette and cursor style/blink options (the 256 colours are
    libghostty-vt's built-in ones, and the colour scheme reply is fixed to dark); the OSC 22
    pointer shape for a mouse; `row_iterator_next_dirty` and the two-phase render update
    (`begin_update` under the lock, `end_update` after), so `frame` holds the tab's lock while
    it encodes every row; `frame` reads each cell with four to six
    `ghostty_render_state_row_cells_get` calls and a `ghostty_cell_get`, where
    `ghostty_render_state_row_cells_get_multi` takes several keys in one call (it stops at the
    first key that fails, so the colours, which may have no value, would go last, and the
    grapheme buffer still needs its length first); the gain is unmeasured;
  - the rest: no action to reset the terminal or clear the scrollback (`ghostty_terminal_reset`)
    and no RIS callback; transcripts are plain text, though the formatter can emit VT or HTML with
    colours and OSC 8 links; the memory usage and the VT processing-error flag are never read;
    the library's log and unknown-sequence callbacks are unset, so its warnings never reach
    logcat; KAM (mode 2) is ignored: Ghostty's own surface drops keys while it is set, but the key
    encoder does not, and neither does vt.c;
  - out of reach: the C API has no setter for the password-input flag (Ghostty's lock cursor) or
    the terminal's visibility, so mode 2033 reports every tab as visible, background ones too.
- **Images take memory twice for the tab shown, and are never trimmed**: each screen of each tab
  keeps up to 32 MiB of decoded Kitty images in libghostty-vt, and the view holds a bitmap of
  every image placed on the shown tab's screen; memory trimming (`compressAll`) leaves both, and a
  restored tab has its text only (placeholder cells come back blank).
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
- Should typing in the search bar select the newest match at once, as Chrome's find bar does,
  rather than wait for a step, as Ghostty does? And should the extra keys act on the search text
  while it has the keyboard, rather than on the terminal?

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
- Kitty graphics with real programs beyond chafa (`chafa -f kitty`; its own detection picked
  symbols): yazi, timg, `kitten icat`, tmux passthrough with placeholders, and how programs that
  probe for the Glyph Protocol fall back. Tried on a Pixel 4a API 34 emulator with chafa and
  `printf` sequences (fixed placements above and below text, placeholders, a two-frame animation,
  scrolling back, a tab switch, the alternate screen, a 1080 × 1400 PNG: scrolling over it took
  about 2.5 ms a frame, at most 12 ms), and on a macOS build of vt.c (queries, refused media,
  PNG with alpha, oversized and corrupt PNG, deletion, scrolling, full reset); not on real devices,
  16 KB-page devices, or with the 32 MiB storage filled.
