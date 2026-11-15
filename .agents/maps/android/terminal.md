# android/terminal Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/terminal.md` for the paired decision history (it still holds the pre-split decisions for `android/vt`, `android/vtcore`, `android/vtview`, `android/vtrender` and `android/licenses`; pre-split privacy decisions and newer license decisions remain in their original android/terminal commits until distilled).
>
> Verified: 2026-11-14 (full mapping: 5 research concerns; 18 claims checked by 3 independent verifiers — 15 confirmed and 3 partial, corrected below)
>
> Maintained: 2026-11-15 (targeted verification only since the full mapping; latest change: add a key-click haptic after volume-key tab switches)

## Responsibilities

`android/terminal` is the terminal screen around the tabs' sessions (`android/vt`, with native
terminals in `android/vtcore`, shown by `android/vtview` using android/vtrender):

- `MainActivity`: the screen's lifecycle and status text, the tab shown and its successor, closing
  tabs, keyboard shortcuts and the volume keys switching tabs, keeping the font size and the
  screen-on choice, and the question whether a program may read the clipboard, with the tab bar (`TabBarView`), the
  search bar in its place while searching (`SearchBarView`), the current tab's progress line over
  the terminal (`ProgressLine`) and the extra keys (`ExtraKeysView`);
- `TerminalLayout`: the view hierarchy, status visibility, insets and the IME's animation;
  `TerminalLinks`: opening web, mail and guest-file links, using the activity's shared toast queue;
- `TerminalFiles`: importing selected Android documents into the login account's home and
  retaining the request, copy and result across activity recreation;
- the app's strings (English and Simplified Chinese) and themes: `Theme.Appine` (Material 3
  DayNight, for Settings and the licenses), `Theme.Appine.Terminal` (Material 3, always dark, with
  a black window and system bars, for `MainActivity`), the ⋮ menu's `Widget.Appine.MorePopup`,
  its `ThemeOverlay.Appine.CheckPopup` (a framework checkbox without its own ripple),
  the Material 3 page frame (`page.xml`, shared by Settings and android/licenses), and
  `preference_switch.xml` (Settings' MaterialSwitch with the standard AndroidX `switchWidget` ID). Settings' categories and rows use AndroidX's layouts.

Root: `MainActivity.kt`, `TerminalLayout.kt`, `TerminalLinks.kt`, `TerminalFiles.kt`, `TabBarView.kt`,
`TabListView.kt`, `TabNumber.kt`, `SearchBarView.kt`, `ProgressLine.kt`, `ExtraKeysView.kt` and `Ui.kt` in
`android/app/src/main/java/io/github/mthli/appine/`, and `android/app/src/main/res/` except
android/privacy's `layout/privacy_shield.xml`, `drawable/privacy_emblem.xml` and `drawable/ic_lock.xml`,
and android/licenses' `layout/list_item.xml` and `values/ids.xml`. Shared strings, themes,
`page.xml` and `preference_switch.xml` remain here.

Outside the module: the terminal view (`android/vtview`) and rendering (`android/vtrender`);
the tabs' sessions and consoles (`android/vt`) and the native emulator shim around libghostty-vt
(`android/vtcore`);
session ownership, saving, kill reports
and where bells, notifications, program statuses and clipboard reads go (`android/service`); the
notice assets and the build (`android/native`); shared authentication, secure windows and
protected notification actions (`android/privacy`); the notices list and reader (`android/licenses`).

## Key types

- `MainActivity` — a `TerminalService.Listener` with a `TerminalLayout` and `TerminalLinks`;
  adapters `Host : VtView.Host`,
  `Bar : TabBarView.Target`, `Tabs : TabListView.Target`, `Find : SearchBarView.Target` and `Keys : ExtraKeysView.Target`; the
  current session and its index, from which its successor is taken; a tab requested by a
  notification (`EXTRA_TAB`); AndroidX Back's callback while the search bar is open (`closeSearchOnBack`);
  the open question whether a
  program may read the clipboard (`clipboardQuestion`).
- `TerminalLayout` — builds the views with the activity's adapters after dynamic colours are
  applied: a black column of one bar's place (`TabBarView`, or `SearchBarView` while searching),
  a frame with `VtView`, the status text, `ProgressLine` along its top and the `TabListView` overlay, and `ExtraKeysView`;
  exposes the root and interactive views, keeps the status view private behind `showStatus` and
  `showTerminal`, and owns the IME animation callback `keysFollowIme`.
- `TerminalLinks` — activity-scoped link routing through `open`, the guest-file lookup thread and
  `startViewer`; keeps the activity and its toast callback, with no service or session state.
- `TerminalFiles : AndroidViewModel` — one process-local picker request, selected URIs, worker and
  result (successful guest paths and failures); read-only `changes` tells the activity to update
  its import button and present the result. `importSelected` reports whether it started a worker,
  `copySelected` performs the batch, and MainActivity's `showImportResult` builds its dialog.
  It keeps the Application, never the activity.
- `SearchBarView` — the search bar, a row as tall as the tab bar: the text to find (an `EditText`
  without suggestions, its IME action Search), the count ("selected/total"), `↑`, `↓` and `×`; it
  shows the count it is given (`setCount`) and sends text changes, steps and closing to a
  `Target` (`search`, `step`, `close`).
- `TabBarView` — the tab bar (`⧉` fixed at the left, a scrolling row of numbered tabs, with `×`, `+`, Import and `⋮` over its
  end): shows the sessions and the current one it is given (`render`) and sends taps on tabs and
  buttons, the ⋮ anchor and the titles long presses show to a `Target` (`show`, `closeCurrent`,
  `newTab`, `importFiles`, `menu`, `notice`, `listTabs`); `setImportEnabled` controls the import button;
  `setListOpen` highlights the list button;
  it draws the other tabs' progress and status marks and redraws them
  on `refreshStatus`; `Tab` (one per session: its number in the row, which slides, rolls to a new
  number and fades out when closed); `TabNumber` (in its own file: the number, a `TextView`
  drawing its digits itself so they can roll).
- `TabListView` — a full-width overlay at the top of the terminal frame, flush below the bar. Its
  RecyclerView shows session numbers, single-line ellipsized titles and independent close buttons;
  immutable row snapshots and stable session IDs keep updates and callbacks tied to sessions.
  `open` renders and reveals the current row without moving input focus; `render` updates an open
  list; `close` hides it and clears retained rows. The activity coordinates Back and accessibility.
- `ExtraKeysView` — Termux's default keys widened to two rows of eight (ESC, `/`, `-`, HOME, ↑, END,
  `|`, SHIFT; TAB, CTRL, ALT, ←, ↓, →, `~`, SYM), with a second page of symbols for every key but
  SYM (`` ` ``, `$`, `*`, `=`, `^`, `\`, `&`, `%`; `[`, `]`, `{`, `}`, `<`, `>`, `;`: shell
  symbols absent from the first page, with bracket pairs together and `&` above `;` in the
  last scrolling column) and a third of function keys for every key but SHIFT and SYM (F1–F6,
  PGUP; F7–F12, PGDN): a `Page` is a grid of the first seven columns and the key above
  SYM; the grid shown is in a `HorizontalScrollView` across the view, the last column (SHIFT or `%`
  over SYM) fixed over its end. It keeps CTRL, ALT and SHIFT (one-shot or locked; `shift` reads
  false while the symbols are shown) and the page shown (`KEYS`, `SYMBOLS` or `FUNCTIONS`), exposes
  them with `modifiersUsed`, and sends keys, text and notices (a modifier locked) to a `Target`; a
  `Key` may have a `longPress` (the modifiers' lock, SYM's function keys); `Press` handles the
  touch on one key. Its companion's `FADE_DP`, `indicatorPaint`, `drawIndicator` (the indicator
  under a label) and `indicatorBounds` (where it lies, where the tab row also draws a tab's
  progress) serve the tab row too.
- `ProgressLine` — a 1 dp view over the top of the terminal showing the current tab's progress
  report; `Colors` (primary, error, muted by progress state); its companion draws progress into any
  rectangle (`draw`, a fill or a sweep) and gives a report's fraction (`fractionOf`).
- `Ui.kt` — what the screens and views share (`VtView` too): `LINE_DP` and `line()` (how thick
  the indicators and the progress line are: 1 dp, at least a pixel), `dp`, `themeColor`
  (white when the theme lacks the colour), `themeDrawable` and `touchFeedback` (the theme's
  `selectableItemBackground`); `showPrivate` registers dialog security with android/privacy before showing it.
  `monospaceMessage` substitutes a styled value into a localized `^1` template with TextUtils,
  leaving the explanation in the dialog's normal font.

## Public entry points

- Launcher activity `MainActivity` (`singleTask`, `adjustResize`, `Theme.Appine.Terminal`, handles
  most configuration changes itself, the dark mode included, but not density, locale or font
  scale, which recreate it), started with
  `EXTRA_TAB` (a `VtSession.id`) by a tab's notification; listener callbacks `onProgress`,
  `onMachineReady`, `onSessionsChanged`, `onBell`, `onClipboardRead`, `onTabStatus`,
  `onSettingsChanged`,
  `onNotice`, `onRestarting`, `onExitAll`, `onOpenUrl`; `onPrivacyUnlocked` from the privacy gate,
  and the `OpenMultipleDocuments` activity-result callback for import.

## Data flow / lifecycle

### Binding and choosing a tab

- `attachBaseContext` applies an override containing only `UI_MODE_NIGHT_YES`, before the window,
  dynamic colours or views read resources. The terminal's night-qualified resources therefore
  match its fixed dark theme; other configuration fields and other activities remain independent.
- `onCreate` applies dynamic colours, reads `EXTRA_TAB` (only when the activity is not being
  recreated and was not opened from Recents: such an activity still has the intent that first
  started it), reads the font size (kept in dp as `font_size_dp`, so it scales with a display size
  change; a pixel size an earlier version kept as `font_size` is converted at the current density
  and removed), applies the saved Keep screen on choice (off by default), builds `TerminalLayout`
  (the status line says "Starting…", insets and IME animation are wired there), installs its root
  edge-to-edge; on Android 13 and later it asks for notification permission after the privacy gate opens. `onStart` applies the wide-cell
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

- One 32 dp row in any window: `⧉` fixed in the leftmost 32 dp opens the tab list, with the
  scrolling tabs starting to its right, and `×` (closes the
  current tab; hidden while there is none), `+` (opens a session while the kernel runs, in the
  current tab's directory: its console's foreground group's, read from the kernel on the main thread
  as a spawn is; the home directory without a current tab or once its shell ended), Import
  (`@`, using the shared text-button style, disabled at 38% opacity while unavailable or occupied),
  and `⋮` (a menu:
  "Find", disabled with no current tab, "Clear screen", disabled without a current tab whose
  terminal exists or while it shows the alternate screen, "Reset terminal", disabled without such a
  tab, "Close other tabs", disabled when there is no other, "Close all tabs", disabled with no tabs,
  "Keep screen on" (a checkbox), and Settings; Clear screen and Reset terminal are android/vt's;
  Find is enabled for a current tab whose terminal does not exist yet, where searching does
  nothing) over its end, every tab and
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
  colour); and a dot of 1.5 dp radius 2 dp right of the number's ink, centred on the top of its
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
- Clicking a tab or a fixed bar button requests `KEYBOARD_TAP` feedback before its action,
  matching the extra keys' tap haptic. Feedback stays in the view's click listener, so Ctrl+Tab
  and programmatic tab changes do not request it.
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
- Import announces "Import files". The four fixed controls reserve 128 dp at the end of the
  scrolling row; the fade still reaches into `×`. The list button reserves another 32 dp at the left.
- The tab list overlays the terminal in the same activity window, without regridding it, on black
  with no gap, margins, border, rounded corners or elevation. Rows are 32 dp, labels 12 dp for the
  number and 14 dp for the title/close button; the current row uses primary text and ended titles
  are faded and marked. At most ten rows fit, further limited by the terminal frame’s height;
  the list scrolls and opens with the current tab visible. Titles use the bar’s `sh` fallback.
- Tapping a list title selects the session and closes the list. Its close button uses the existing
  busy confirmation and leaves the list open, including when the current tab’s successor is shown.
  Both click listeners request `KEYBOARD_TAP` feedback before their action, as the bar does.
  Session/title changes update visible rows and positional numbers without resetting scroll.
  The list button toggles it; an outside terminal tap dismisses and consumes that tap. Back, Escape,
  explicit tab switching, search, the other bar actions, extra-key input, pause and restart dismiss
  it too. Touch opening and scrolling preserve terminal input focus and its IME connection;
  the terminal is hidden from accessibility while the list is open. Closing restores terminal
  focus only if a list control had it; close confirmations return focus to the terminal.
  It lives under the activity’s privacy shield and secure-window policy, rather than in a separate popup.
- The list’s close buttons, `×` and the menu's "Close other tabs" and "Close all tabs" close at once when none of the tabs
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
- "Keep screen on" is available even without a current tab and applies to the whole terminal
  window. Its right-side checkbox reflects the activity's choice, persisted as `keep_screen_on`
  in the activity's private preferences, so recreating or reopening the activity keeps it.
  The choice sets or clears `FLAG_KEEP_SCREEN_ON`: only the visible terminal window keeps the
  screen lit; opening Settings or leaving the app allows the system's normal screen timeout.
  It is independent of the service's screen-off run policy and partial wake lock. The popup's
  context uses `ThemeOverlay.Appine.CheckPopup`, removing the checkbox's own background while
  retaining the menu row's press feedback and the overflow menu's placement.

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

### The volume keys

- While Settings has them switch tabs (`Settings.volumeKeysSwitchTabs`, on by default, read at
  each press), the activity (`dispatchKeyEvent`) takes a volume key's press when there is another
  tab to show: volume up shows the previous tab and volume down the next, going round as Ctrl+Tab
  does, once per press; the key's repeats and release are swallowed with it, so holding it neither
  switches again nor changes the volume. With a single tab (or none, as while the kernel starts) or
  the setting off, the press, its repeats and its release go to the system, which changes the
  volume; programs never get the volume keys either way (`VtView` leaves system keys alone). Open
  dialogs and the ⋮ menu are windows of their own, where the volume keys change the volume.
- A successful volume-key switch requests `KEYBOARD_TAP` feedback through the terminal view,
  matching the bars. Repeats, releases and presses left to system volume control do not request it.

### Finding text

- "Find" (⋮) or Ctrl+Shift+F, with a current tab, puts the search bar in the tab bar's place
  (the tab bar is gone, the place keeps its 32 dp, so the terminal keeps its rows) and brings the
  keyboard up on its text, which is kept from the last search and selected, so typing replaces it;
  that text is searched for again at once. Every change of the text searches anew in the tab shown
  (`VtView.search`; query, steps and scroll-bar marks are android/vtview's Search, frame progress
  and cell highlights android/vtrender's). Empty text searches for nothing and clears the count.
- No match is selected until a step: `↑`, Enter or the keyboard's Search key selects the next match
  above (towards older output; the first step selects the newest), `↓` or Shift+Enter the one
  below, going round at either end (`VtView.searchStep`). The count reads "selected/total" as the
  view reports it (`Host.onSearchCount`), the selected one counted from the newest, 0 before a step; text that is not found reads "0/0".
- `×`, Esc in the text, or Back (after Back has hidden the keyboard) closes the bar: the search
  ends and its marks go (`VtView.endSearch`), the tab bar comes back, the terminal takes the focus
  and the viewport stays where the search left it. Showing a tab (another one, Ctrl+Tab or a volume
  key while searching, or the same one again when the activity comes back, see Binding and choosing a tab) or
  a restart closes it too, as a search belongs to the tab it was opened in.
- Tapping the terminal (or pressing a mouse on it) while the bar is open gives the keyboard back to
  the terminal and leaves the bar and its marks; the extra keys always go to the terminal, the
  bar's text focused or not.
- The search bar's `↑`, `↓` and `×` click listeners request the same `KEYBOARD_TAP` feedback
  as the tab bar and extra keys; Enter, Escape and the IME's Search action bypass those listeners.

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
  arrows, PGUP and PGDN repeat every 80 ms until the finger lifts or leaves the key (no act on
  release then); the modifiers and SYM act on their long press then, with a long-press haptic, and
  not again on release; other keys still act once on release. No key has a swipe or popup. Keys
  with a long press also answer an accessibility service's long click (the touch never reaches
  the view's own long-press detection).
- CTRL, ALT and SHIFT: a tap turns them on for the next key (primary colour only) or off, a locked
  one too; a long press locks them on (primary colour and the
  indicator) until tapped, one a tap turned on included, with a toast naming the key ("CTRL
  locked"), or turns a locked one off. Keys go to `VtView.press` and `type`, which read the
  modifiers through the host and report when a key took them; that clears one-shot modifiers and
  leaves locked ones on.
- A tap on SYM shows the symbols in place of every other key (`%` in place of SHIFT), and from
  either other page shows the first again; a long press shows the function keys in place of
  every key but SHIFT and SYM, and from them the first page again. A page stays, for any number
  of keys, until SYM turns it; SYM's label takes the primary colour and the indicator while another
  page than the first is shown. The function keys and PGUP/PGDN go to `VtView.press` as their key
  codes, so the modifiers apply as on a keyboard (SHIFT, F5 sends `CSI 15;2~`). Turning the page
  leaves the modifiers as they are: CTRL and ALT apply to symbols and function keys too (CTRL,
  SYM, `]` sends Ctrl+]), though they cannot be seen on those pages, while SHIFT waits on the
  symbols page: it applies to no key, the symbols or the IME's, and a one-shot SHIFT is not used
  up, so it applies again once another page is back. A key still held on the page going away is
  cancelled. The page lives as long as the view: a new activity starts on the first.

### Insets and the IME

- The content is edge-to-edge with the root padded by system-bar, display-cutout and IME insets
  (from Android 15 the window reaches into the cutout, which in landscape is at a side). As the IME
  shows or hides, the root is laid out for the end at once (the terminal regrids once, at the
  start) and the extra keys are drawn moving from their old place to the new one along the IME
  animation's curve (`TerminalLayout.keysFollowIme`), rising through the IME's room or sinking over the
  terminal's new rows; an animation that interrupts another starts from where the keys are drawn.

### The progress line

- The current tab's progress report is drawn by `ProgressLine`, as high as the tab bar's
  indicator (`LINE_DP`, 1 dp and at least a pixel; Ghostty's is 2 points) over the top of the terminal's first
  row, otherwise as Ghostty draws it: filled from the left to the percentage in the primary
  colour, the error colour for an error, the muted colour while paused (all of it for a pause
  without a percentage); without a percentage, or indeterminate, a quarter of its width sweeps
  across and back (1.2 s each way, ease-in-out, timed by the clock) over a track at 30% opacity,
  redrawn every frame. It fades in and out in 150 ms and slides to a new percentage in 200 ms;
  showing another tab puts that tab's progress there at once (a tab without one still lets the
  old one fade out). It takes no touches and is hidden from accessibility.

### Links and the clipboard question

- `onOpenUrl` receives a validated guest web URL from the service's kernel-wide browser bridge.
  With window focus it calls `TerminalLinks.openBrowser`; otherwise it returns false. Its Boolean
  acknowledges Android accepting the intent; the guest receives a failure when it is refused.
- The view reports a link to open (`Host.onLink`: a tap or Ctrl+click on one, or the menu's Open
  link; which links count and which schemes open are android/vtview's). The activity delegates to
  `TerminalLinks.open`, which opens HTTP and HTTPS links with AndroidX Custom Tabs, without binding
  a browser package or session, and other
  non-file schemes (such as mail) with plain `ACTION_VIEW`; both carry `CATEGORY_BROWSABLE`.
  `startViewer` passes the Custom Tabs launch options and returns false with a toast if no app handles the link.
  A `file:` link (`ls --hyperlink`) is looked up off
  the main thread through the Files app's provider (`Documents.fileLink`) and opened as its
  document, typed by it and with read access granted: a directory in the Files app, a file in the
  app the system picks. A link from another host, to an existing file while file access is off,
  or to nothing in the store gets a toast instead, as does a link no app takes. The disabled-file
  hint names "Allow other apps to access files" in both locales, matching Settings' consent dialog.
  The lookup result returns on the UI thread and is ignored if its activity is destroyed or locked.
- A tab's clipboard read reaches the activity through the service (`onClipboardRead`; the queue and
  the answers are `VtSession`'s, android/vt). `MainActivity` reads the clipboard's first item as
  text and, when there is some, asks ("Let the program read the clipboard?", up to 300 characters
  shown, Allow and Deny), one question at a time, whose answer covers every read queued meanwhile;
  an empty clipboard, or one without text, is answered at once (`VtSession.answerClipboardRead`).

### Importing files

- The top-right Import button opens the system multiple-document picker for `*/*`, only while
  the page is unlocked and the kernel runs. No storage permission or external file-access setting
  is needed. One request, copy or undismissed result occupies the button; cancelling releases it.
- `TerminalFiles.begin` retains the login preference. The picker callback accepts results only
  for that process-local request and deduplicates URIs. A resumed unlocked page starts the copy;
  the worker resolves the account once against the current store and holds `Rootfs.access`'s read
  lock for the batch. Destination is that account's home, independent of the tab's cwd, effective
  user and mounts. A missing home or `/` is refused.
- Provider display names are used with a localized fallback. Controls, slash and NUL are replaced,
  empty/dot names are sanitized, and extensions are preserved. Native browse creation assigns the
  account's ownership and numbers collisions without overwriting. Source and destination streams
  close, file data is synced, and `browseWritten` reports the write to the guest. Read/write/close
  errors attempt to remove the created file; other files in the batch continue.
- The retained result lists successful guest paths in monospace and per-file failures in a protected
  dialog. A blank line separates the explanation from the paths; message assembly preserves spans.
  Copy paths explicitly puts space-separated shell arguments on the clipboard, prefixing whitespace
  and shell metacharacters with backslashes and leaving ordinary paths unquoted. It appends no
  newline and sends no terminal input. Result display
  requires a resumed unlocked page, and copying checks the gate again.
- Recreation retains selection/copy/result; destruction dismisses its dialog without consuming
  the result, so it can reappear after unlock. Process death loses this state. An already-started
  worker continues after backgrounding or closing the activity. There is no cancellation; slow
  document providers hold up rootfs reset/replacement. Process termination or cleanup failure can
  leave an incomplete file; imports are not transactional across the whole batch.

## Dependencies (inbound / outbound)

### Inbound

- `TerminalService`: listener callbacks (`onProgress`, `onMachineReady`, `onSessionsChanged`,
  `onBell`, `onClipboardRead`, `onTabStatus`, `onSettingsChanged`, `onNotice`, `onRestarting`,
  `onExitAll`) and the notification intent with `EXTRA_TAB`.
- android/vtview: `VtView.Host` (`onGrid`, `onInput`, `onZoom`, the extra keys' `ctrl`, `alt`,
  `shift` and `modifiersUsed`, `onLink`, `onSearchCount`).
- android/setup and android/licenses use the shared page frame, themes and view helpers;
  Settings also uses the preference switch widget. android/privacy uses the shared theme and helpers.
- android/privacy: `onPrivacyUnlocked` resumes deferred terminal UI work.

### Outbound

- android/vtview: `VtView` (`attach`, `setFontSize`, `setFillWide`, `press` and `type` from the
  extra keys, `search`, `searchStep`, `endSearch`, `jumpToPrompt`, `bell`, `altScreen`).
- android/vt: `VtSession` (`busy`, `cwd()`, `title`, `progress`, `status`, `describe`, `clear`,
  `reset`, `answerClipboardRead`); `ProgressLine` draws the tab's `Progress`.
- `TerminalService` (`createSession`, with the current tab's directory for a new tab,
  `removeSession`, `current`, `focus`, `sessions`), `Settings.fillWide` and
  `volumeKeysSwitchTabs`; `Documents.fileLink`
  (android/documents).
- `TerminalFiles` uses ContentResolver document streams, `Rootfs` account resolution/store lock,
  `Documents` naming/path helpers and `Native.browse*` for private copies into the home.
- android/privacy: `Privacy.locked` guards input, clipboard, imports, browser launches and title
  toasts; `Ui.showPrivate` delegates dialog-window security to `Privacy.protect`.
- Material Components 1.14.0 with AppCompat and androidx.activity (the shared themes and page
  layouts, `MainActivity`'s dialogs and menus); androidx.core's insets and animation compat;
  RecyclerView 1.2.1 for the tab list; androidx.browser's Custom Tabs for web links; the app the system picks for a link.

## Invariants and gotchas

- Programs never receive Ctrl+Shift+T, Ctrl+Shift+W, Ctrl+Shift+F, Ctrl+Tab or Ctrl+Shift+Tab
  typed on a hardware keyboard with Ctrl held but neither Alt nor Meta (the tab shortcuts), nor
  Ctrl+Shift+↑ and ↓ on the primary screen
  (the prompt jumps, taken even when there are no prompts); the release of the shortcut's key is
  swallowed too, so the Kitty protocol sees no release without a press. Other releases are never
  matched to presses: Ctrl+Shift+C/V's, a Shift+arrow's (or Page Up or Down, Home or End) that
  moved a selection, or Esc's when the IME took the press, reach the program alone.
- A taken shortcut or volume key is remembered (`shortcutKey`) until its release, which is
  swallowed; a volume key's repeats are taken only while it is the remembered key, so a press
  left to the system keeps its repeats and release there and the volume changes as usual.
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
  measure again whenever the column width changes, the pages not shown too (each keeps the flag
  until it is added back).
- SHIFT's view is the last key of both the first page and the function keys' (a view has one
  parent, and a `Modifier` one view), so turning between those pages leaves it in place in the
  fixed column, and a SHIFT held meanwhile is not cancelled.
- The indicators are drawn by the containers (the tab row, each page's grid, the fixed column) in
  `dispatchDraw`, over their labels, so a change of state must invalidate the container, not the
  label: `refresh` invalidates the shown grid and the fixed column (turning pages refreshes the
  newly attached grid), and `TabBarView.render` the tab
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
- `MainActivity` is a `FragmentActivity` on a Material theme: Material dialogs and AppCompat's
  `PopupMenu` take the theme, but the views it builds or inflates from XML are the framework's (no
  AppCompat inflater), so AppCompat view attributes such as `app:tint` do nothing there. The text
  selection handles `VtView` draws read framework attributes, which AppCompat forwards to the
  Material 3 colours (the handles are the primary colour). The system floating text menu has its
  own theme and resources: on S23 Android 16 its surface/text follow configuration night mode
  while its ripple follows the theme. A fixed dark theme alone does not force `uiMode` to night;
  MainActivity's early configuration override aligns them and survives configuration updates.
  Keep the override limited to night mode so locale, font scale, density and window size still update.
  Theme.Material3 does not set the system bars' colours, which Android applies up to version 14,
  so `Theme.Appine.Terminal` sets them black, as the framework theme had them.
- `TerminalLayout` must be built after dynamic colours are applied and assigned before attaching
  its root to the window: its view adapters access the activity's layout once input, grid and
  search callbacks begin. The activity still detaches the session on restart and destruction.
- The system lays the window out with the IME's end insets as its animation starts, when it hides
  as well as when it shows, so without a `WindowInsetsAnimation` callback the extra keys jump
  there at once. The keys rising with the IME are drawn inside the root's bottom padding, which
  already holds the IME's room, so the root must not clip to its padding (`clipToPadding` off) or
  they stay hidden until they reach the terminal.
- The tab bar's buttons are characters in the system font: `⧉`, `+`, `@`, `⋮` and `×` (the multiplication
  sign, which Roboto draws like its `+`). `✕` (U+2715) is not in Roboto, and the fallback font's
  glyph is larger and thinner than the others. `@` opens file import.
- No activity handles `density`: every view is sized in dp when built. Settings and the license
  pages also recreate for dark mode (android/setup and android/licenses).
- The privacy gate is android/privacy's. Keep checks at the final input and clipboard sinks,
  including delayed IME/paste callbacks; pausing cancels title toasts and denies the open clipboard
  question. Dialogs use `showPrivate`, and deferred import results require a resumed unlocked page.
- An activity the system recreates (a configuration change it does not handle, or after killing
  the app) or that is opened from Recents gets the intent that first started it, so `EXTRA_TAB`
  is read in `onCreate` only without a saved state and without
  `FLAG_ACTIVITY_LAUNCHED_FROM_HISTORY`; otherwise a notification's tab would come back to the
  front. A recreated `MainActivity` drops ordinary dialogs, the search bar and the extra keys'
  modifiers/page; a retained import result is shown again after the page resumes unlocked.
- Back uses `OnBackPressedDispatcher` with `closeSearchOnBack` enabled only during search and
  `closeTabListOnBack` only while the tab list is open. List callbacks carry the session, never
  a position; the activity checks it still exists before selecting or confirming its close. Dismissing
  the list clears its data and recycled rows so it does not retain sessions closed through other controls.

## Confirmed bugs / technical debt

- Imports have no cancellation or process-death recovery. Killing the app during a copy can
  leave an incomplete file, and closing its activity can lose the eventual result display.

## Open questions

- Should tab-bar rendering and extra-key handling get separate maps? Even after moving privacy
  and licenses out, this map exceeds the single-read size target; a further split must assign the
  shared indicators and activity callbacks without duplicating their contracts.
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

- Bar/list clicks and volume-key tab switches use the extra keys' feedback constant; their physical
  feel and device touch-feedback settings still need real-device verification.

- Tab-list identity after removal, the bounded 50-tab layout and preserving editor focus passed
  three API 34 instrumentation cases; the focus case failed against the former requestFocus path. API 34 UI checks covered flush placement with the IME, live titles, busy/current and idle/
  background closes, switching, Back, Escape and outside dismissal. Opening/dismissing with the IME
  visible or hidden retains the same terminal input connection and visibility. Physical touch targets, TalkBack
  traversal and vendor glyph rendering remain unverified.

- The terminal's night override compiles and passes lint; the system selection menu's surface,
  text and ripple on S23, including after switching system dark mode while the activity is alive,
  still need device verification. Settings and licenses should continue following the system.
- File import was exercised through the API 34 system picker with multiple files, cancellation,
  collision numbering and activity recreation, including redisplay of the result. Seven
  instrumentation cases cover native-store copies, read/close cleanup, guest reads, shell escaping
  and locked-picker retention. Cloud providers, process interruption and vendor pickers remain unverified.
- Guest launcher → Chrome CustomTabActivity → Back, plus background refusal with exit 1, were
  checked on API 34; a complete Codex OAuth login and its callback remain unverified.
- Everything here was tried only on emulators (a Pixel 4a API 34, and the outer display of a Pixel
  10 Pro Fold API 37 for the cutout and the IME), with Gboard in English and input from adb; not on
  real phones, tablets or foldables. Unverified beyond that:
  - App Links, browsers without Custom Tabs and missing
    handlers; links with other mail apps, viewers and file managers, and `mailto` at all;
  - the cutout insets and the keys following the IME with other IMEs;
  - the tab bar's 32 dp tabs and buttons (below the 48 dp touch target) under real fingers, and the
    rolling numbers' blur on Android 11 and real GPUs;
  - the progress line and the other tabs' marks for real programs (cargo, systemd, AI coding CLIs),
    and TalkBack's descriptions heard rather than read through uiautomator;
  - the extra keys' long presses (SYM's function keys, locking a modifier) through TalkBack, and
    the function keys in real programs (mc, htop) rather than `cat -v`;
  - the search bar's Back callback on Android 16;
  - the volume keys as real buttons (tried with adb key events: presses, a long press, the
    setting off and on, with three tabs; not with a single tab) and under vendor volume handling;
  - Back finishing the activity on Android 11 (a new activity's tab was tried after removing the
    task, a font scale change and a display size change on API 34).
