# android/terminal Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/android/terminal.md` for the paired decision history (it still holds the decisions on the tabs' sessions and the terminal view made before they were split out into `android/vt` and `android/vtview`).
>
> Verified: 2026-11-11 (full mapping: 5 research concerns; 18 claims checked by 3 independent verifiers — 11 confirmed and 7 partial, corrected below; no bugs recorded)
>
> Maintained: 2026-11-11 (targeted verification only since the full mapping: the terminal view split out to android/vtview and the sessions, consoles and emulator shim to android/vt; recreation, a new activity's tab and the font size in dp last)

## Responsibilities

`android/terminal` is the terminal screen around the tabs' terminals (`android/vt`, drawn by
`android/vtview`):

- `MainActivity`: the screen's layout and status text, the tab shown and its successor, closing
  tabs, keyboard shortcuts, keeping the font size, insets and the IME's animation, opening links,
  and the question whether a program may read the clipboard, with the tab bar (`TabBarView`), the
  search bar in its place while searching (`SearchBarView`), the current tab's progress line over
  the terminal (`ProgressLine`) and the extra keys (`ExtraKeysView`);
- `LicensesActivity`: the notices generated at build time;
- the app's strings (English and Simplified Chinese) and themes: `Theme.Appine` (Material 3
  DayNight, for Settings and the licenses), `Theme.Appine.Terminal` (Material 3, always dark, with
  a black window and system bars, for `MainActivity`), the ⋮ menu's `Widget.Appine.MorePopup`, and
  the Material 3 page layouts (`page.xml` and `list_item.xml`, shared by Settings and the licenses;
  `list_header.xml`, Settings only).

Root: `MainActivity.kt`, `TabBarView.kt`, `SearchBarView.kt`, `ProgressLine.kt`,
`ExtraKeysView.kt` and `LicensesActivity.kt` in `android/app/src/main/java/io/github/mthli/appine/`,
and `android/app/src/main/res/`.

Outside the module: the terminal view (`android/vtview`); the tabs' sessions and consoles and the
native emulator shim around libghostty-vt (`android/vt`); session ownership, saving, kill reports
and where bells, notifications, program statuses and clipboard reads go (`android/service`); the
notices task and the build (`android/native`).

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
- `ProgressLine` — a 2 dp view over the top of the terminal showing the current tab's progress
  report; `Colors` (primary, error, muted by progress state); its companion draws progress into any
  rectangle (`draw`, a fill or a sweep) and gives a report's fraction (`fractionOf`).
- `LicensesActivity` — an `AppCompatActivity` on the Material 3 page: list from
  `assets/notices/index.tsv` (one `list_item.xml` row per line with three fields), full text view;
  an `OnBackPressedCallback`, enabled while a full text is shown, returns to the list.

## Public entry points

- Launcher activity `MainActivity` (`singleTask`, `adjustResize`, `Theme.Appine.Terminal`, handles
  most configuration changes itself, the dark mode included, but not density, locale or font
  scale, which recreate it), started with
  `EXTRA_TAB` (a `VtSession.id`) by a tab's notification; listener callbacks `onProgress`,
  `onMachineReady`, `onSessionsChanged`, `onBell`, `onClipboardRead`, `onTabStatus`,
  `onSettingsChanged`,
  `onNotice`, `onRestarting`, `onExitAll`.
- `LicensesActivity`, opened from Settings.

## Data flow / lifecycle

### Binding and choosing a tab

- `onCreate` applies dynamic colours, reads `EXTRA_TAB` (only when the activity is not being
  recreated and was not opened from Recents: such an activity still has the intent that first
  started it), reads the font size (kept in dp as `font_size_dp`, so it scales with a display size
  change; a pixel size an earlier version kept as `font_size` is converted at the current density
  and removed), builds the views (the status line says "Starting…"), sets up insets and, on
  Android 13 and later, asks for the notification permission. `onStart` applies the wide-cell
  setting, starts and binds the service; `onServiceConnected` registers the listener and replays
  the start's result or progress and pending notices; `onStop` unregisters and unbinds.
- When the kernel is ready the tab shown is, in order: one a notification asked for (`EXTRA_TAB`,
  from `onCreate` or `onNewIntent`; a tab closed meanwhile is skipped), the activity's own current
  tab, the service's `current` (the tab the last activity showed, for a new one: recreated, or
  opened again after Back finished the last on Android 11), the service's `focus` (the restored
  current tab), each if it still exists, else the last tab; with no
  sessions a new one opens. A failed start hides the terminal and shows "Cannot start: …";
  `onRestarting` detaches and shows "Restarting…". `onServiceConnected` calls `onMachineReady` on
  every bind (each `onStart`) when the kernel runs or failed to start (while it starts, the
  progress, and `onMachineReady` later from the service), so coming back to the activity (from
  Settings, a link's app, the background) shows the current tab again, which closes the search
  bar.
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
  and Settings; Clear screen and Reset terminal are android/vt's; Find is enabled for a current tab
  whose terminal does not exist yet, where searching does nothing) over its end, every tab and
  button a 32 dp square with its label centred. The tabs pass under the buttons, which hide them,
  and an end padding as wide as the buttons lets the last tab scroll out from under them; while more
  tabs lie beyond, the row fades out over 32 dp ending 12 dp inside `×`, in the blank before its
  glyph (the scroll view's right fading edge; the left edge does not fade).
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
  waits at its prompt as the prompt marks tell, `VtSession.busy`, android/vt's Shell integration),
  else after a confirmation dialog ("Close this tab?" for one tab, else "Close N tabs?", counting
  all of them); a shell without prompt marks always counts as busy. Several tabs are closed with the
  current one last, so no other tab is shown (and its shell started) on the way, and closing all
  ends like closing the last tab. Ctrl+Shift+W closes the current tab without asking, even while its
  shell runs, and in an ended tab any input with a line break (Enter) closes it.
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
  that text is searched for again at once. Every change of the text searches anew in the tab shown
  (`VtView.search`; matching, steps and marks are android/vtview's Search). Empty text searches for
  nothing and clears the count.
- No match is selected until a step: `↑`, Enter or the keyboard's Search key selects the next match
  above (towards older output; the first step selects the newest), `↓` or Shift+Enter the one
  below, going round at either end (`VtView.searchStep`). The count reads "selected/total" as the
  view reports it (`Host.onSearchCount`), the selected one counted from the newest, 0 before a step.
- `×`, Esc in the text, or Back (after Back has hidden the keyboard) closes the bar: the search
  ends and its marks go (`VtView.endSearch`), the tab bar comes back, the terminal takes the focus
  and the viewport stays where the search left it. Showing a tab (another one, Ctrl+Tab while
  searching, or the same one again when the activity comes back, see Binding and choosing a tab) or
  a restart closes it too, as a search belongs to the tab it was opened in.
- Tapping the terminal (or pressing a mouse on it) while the bar is open gives the keyboard back to
  the terminal and leaves the bar and its marks; the extra keys always go to the terminal, the
  bar's text focused or not.

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

### The progress line

- The current tab's progress report is drawn by `ProgressLine`, 2 dp high over the top of the
  terminal's first row, as Ghostty draws it: filled from the left to the percentage in the primary
  colour, the error colour for an error, the muted colour while paused (all of it for a pause
  without a percentage); without a percentage, or indeterminate, a quarter of its width sweeps
  across and back (1.2 s each way, ease-in-out, timed by the clock) over a track at 30% opacity,
  redrawn every frame. It fades in and out in 150 ms and slides to a new percentage in 200 ms;
  showing another tab puts that tab's progress there at once (a tab without one still lets the
  old one fade out). It takes no touches and is hidden from accessibility.

### Links and the clipboard question

- The view reports a link to open (`Host.onLink`: a tap or Ctrl+click on one, or the menu's Open
  link; which links count and which schemes open are android/vtview's). The activity opens a web or
  mail link as a browsable `ACTION_VIEW` intent; a `file:` link (`ls --hyperlink`) is looked up off
  the main thread through the Files app's provider (`Documents.fileLink`) and opened as its
  document, typed by it and with read access granted: a directory in the Files app, a file in the
  app the system picks. A link from another host, outside the home while the whole system is not
  shown, or to nothing in the store gets a toast instead, as does a link no app takes.
- A tab's clipboard read reaches the activity through the service (`onClipboardRead`; the queue and
  the answers are `VtSession`'s, android/vt). `MainActivity` reads the clipboard's first item as
  text and, when there is some, asks ("Let the program read the clipboard?", up to 300 characters
  shown, Allow and Deny), one question at a time, whose answer covers every read queued meanwhile;
  an empty clipboard, or one without text, is answered at once (`VtSession.answerClipboardRead`).

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
  text again (saved state), as a display size change does. A rotation does not recreate it, so a
  full text shown keeps its size.

## Dependencies (inbound / outbound)

### Inbound

- `TerminalService`: listener callbacks (`onProgress`, `onMachineReady`, `onSessionsChanged`,
  `onBell`, `onClipboardRead`, `onTabStatus`, `onSettingsChanged`, `onNotice`, `onRestarting`,
  `onExitAll`) and the notification intent with `EXTRA_TAB`.
- android/vtview: `VtView.Host` (`onGrid`, `onInput`, `onZoom`, the extra keys' `ctrl`, `alt`,
  `shift` and `modifiersUsed`, `onLink`, `onSearchCount`).
- `SettingsActivity` (licenses; it also uses the page layouts).

### Outbound

- android/vtview: `VtView` (`attach`, `setFontSize`, `setFillWide`, `press` and `type` from the
  extra keys, `search`, `searchStep`, `endSearch`, `jumpToPrompt`, `bell`, `altScreen`).
- android/vt: `VtSession` (`busy`, `cwd()`, `title`, `progress`, `status`, `describe`, `clear`,
  `reset`, `answerClipboardRead`); `ProgressLine` draws the tab's `Progress`.
- `TerminalService` (`createSession`, with the current tab's directory for a new tab,
  `removeSession`, `current`, `focus`, `sessions`, `fillWide`); `Documents.fileLink`
  (android/documents); `assets/notices`.
- Material Components 1.14.0 with AppCompat and androidx.activity (both themes, `LicensesActivity`
  and the page layouts, `MainActivity`'s dialogs and menus); androidx.core's insets and animation
  compat; the app the system picks for a link.

## Invariants and gotchas

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
  itself would keep the views it already made in the old ones. No activity handles `density`,
  for the same reason: every view is sized in dp when built.
- An activity the system recreates (a configuration change it does not handle, or after killing
  the app) or that is opened from Recents gets the intent that first started it, so `EXTRA_TAB`
  is read in `onCreate` only without a saved state and without
  `FLAG_ACTIVITY_LAUNCHED_FROM_HISTORY`; otherwise a notification's tab would come back to the
  front. A recreated `MainActivity` starts without open dialogs, the search bar or the extra
  keys' modifiers and page.
- Up to Android 15 Back reaches `MainActivity.onBackPressed` (the manifest does not opt in to
  predictive back, so an `OnBackInvokedCallback` would be ignored there); from Android 16 on, the
  app targeting it, `onBackPressed` is never called and only the callback registered while
  searching closes the bar. Lint's GestureBackNavigation check is suppressed on the override.

## Confirmed bugs / technical debt

none

## Open questions

- Should Ctrl+Shift+W ask before closing a running shell, as `×` does, and should "Close other
  tabs" with one other tab name it rather than ask "Close this tab?"?
- Should a background tab's progress take another colour than the primary one? Nearly full, it
  looks like the current tab's indicator, told apart only by the numbers' colours.
- Should a clipboard read from a tab not in front wait until the tab is shown rather than get an
  empty clipboard, and should the question offer to remember the answer for the tab (Ghostty asks
  every time)?
- Should a clipboard question left open when the activity is destroyed be answered then? Until
  the tab's queued reads are answered it asks for no later read.
- Should typing in the search bar select the newest match at once, as Chrome's find bar does,
  rather than wait for a step, as Ghostty does? And should the extra keys act on the search text
  while it has the keyboard, rather than on the terminal?

## To verify

- Everything here was tried only on emulators (a Pixel 4a API 34, and the outer display of a Pixel
  10 Pro Fold API 37 for the cutout and the IME), with Gboard in English and input from adb; not on
  real phones, tablets or foldables. Unverified beyond that:
  - links with other browsers, mail apps, viewers and file managers, and `mailto` at all;
  - the cutout insets and the keys following the IME with other IMEs;
  - the tab bar's 32 dp tabs and buttons (below the 48 dp touch target) under real fingers, and the
    rolling numbers' blur on Android 11 and real GPUs;
  - the progress line and the other tabs' marks for real programs (cargo, systemd, AI coding CLIs),
    and TalkBack's descriptions heard rather than read through uiautomator;
  - the search bar's Back callback on Android 16;
  - Back finishing the activity on Android 11 (a new activity's tab was tried after removing the
    task, a font scale change and a display size change on API 34).
