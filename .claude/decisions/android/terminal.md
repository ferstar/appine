# Terminal UI (android/terminal) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/terminal"`
>
> Last distilled: 2026-11-05 (HEAD = 173da76). D1–D3 were seeded by hand from the retired design
> doc §11.5 and the round records in `docs/rounds/`, so their sources cite rounds; later entries
> cite commits.

## Active

### D2: Messages from appine are inserted above the cursor line
- **What**: `VtSession.showMessage` (`appine_vt_message` in vt.c) scrolls if needed, saves the cursor, inserts lines above the current one, writes the yellow message and restores the cursor below it; it is skipped on the alternate screen.
- **Why**: Kill reports arrive after the shell has already printed its prompt; appending put the message after the prompt and left the cursor on an empty line, which looked like a lost prompt and desynchronised line editing.
- **Tradeoffs**: The shell does not know the screen changed (its cursor and line still match); the guest's saved-cursor slot is overwritten.
- **Source**: round 195 (`195-kill-reports`); 6ffb912

### D3: Licence notices are generated at build time and shown in-app
- **What**: The build writes `notices/` (appine, libghostty-vt's components and JetBrains Mono, Alpine packages from the bundled rootfs, Android libraries, Rust crates, SQLite, full licence texts); Settings shows the AGPL statement and opens `LicensesActivity`.
- **Why**: AGPL sections 0 and 5 require the interface to show copyright, the lack of warranty, the licence and where to read it; generating from the actual inputs keeps the list complete.
- **Tradeoffs**: The licence pages are English only.
- **Source**: round 192 (`192-license`); list updated in b38d29c (android/native D6)

### D4: The terminal UI is a module of its own
- **What**: android/terminal holds `MainActivity`, the `Vt*` view and session files, `ConsolePty`, `ExtraKeysView`, `LicensesActivity`, `res/`, the terminal fonts and the vt.c/vt.rs shim; building libghostty-vt belongs to android/native.
- **Why**: §11.5 of the retired design doc needed a map, and the app was split into modules by responsibility rather than folding the UI into android/service.
- **Tradeoffs**: `ConsolePty` and `VtSession` are filed here although the service creates and owns them.
- **Source**: 12c4afa (file list since updated by 394aecf, 6ffb912, b38d29c)

### D5: Terminal state lives in libghostty-vt inside the native library, drawn by appine's own view
- **What**: Each tab has one libghostty-vt 1.4.0 terminal in the native library, fed straight from the kernel console by a pump thread that writes the terminal's replies back and asks for one frame of dirty rows per burst; `VtView` mirrors the frames onto an integer cell grid in JetBrains Mono (glyphs scaled evenly, never stretched, with wide cells filled as in D9; box drawing, blocks and Powerline triangles as shapes; ligatures off), keys go through libghostty-vt's encoder, grapheme clustering (mode 2027) is on and synchronized output holds the screen for at most 1 s.
- **Why**: The Termux view flattened CJK (horizontal-only scaling to the cell), broke box-drawing lines between rows, showed Powerline symbols as boxes and split emoji sequences; Ghostty's full renderer targets macOS and GTK, while libghostty-vt is a plain C library for the terminal state.
- **Tradeoffs**: CJK glyphs still come from the system fallback fonts; programs that count widths per codepoint disagree with mode 2027 on emoji sequences.
- **Watch out**: The frame format (vt.c and `VtView`) and the key numbers (`VtKeys`, from Ghostty 1.4.0's header) must change together with their counterparts.
- **Source**: 394aecf; 0478dd5

### D6: A tab names its kernel by handle, not by `Arc<Machine>`
- **What**: vt.rs's `Tab` stores the kernel handle; the pump looks the kernel up for every console read, uses that reference for the reply write and drops it before calling into Java, resize looks it up too, and create only checks that the handle is live, as the `Native` functions do (android/native D1).
- **Why**: A held `Arc<Machine>` kept a stopped kernel and its store open, so a reboot or tier change started the new kernel on a store still in use (skipping the boot cleanup), a distro switch deleted the store under the old kernel, and the final drop ran on the main thread.
- **Tradeoffs**: One table lookup (a brief lock) per console read; the pump still holds the kernel while blocked in a read, so the final drop can run on the pump thread.
- **Watch out**: The service must keep releasing tabs (closing their consoles) before shutting the kernel down, or a restart can open the new store before the pump's read returns.
- **Source**: ff9e2f9

### D7: Selection, paste, mouse and focus go through libghostty-vt
- **What**: Under the tab's lock, vt.c keeps the selection as the terminal's own (a long press selects a word as a tracked anchor, finger or handle drags extend it and autoscroll beyond the grid), pastes through `ghostty_terminal_paste` (asking with a preview when it returns REJECTED), and encodes mouse clicks and wheel, alternate-scroll arrows (mode 1007) and focus (mode 1004); frame and row headers carry the selection for `VtView`'s highlight and handles, keys add releases and lone modifiers for the Kitty protocol, and the IME turns a lone newline into Enter and a multi-line commit into a paste.
- **Why**: The prototype could only type keys and scroll: no copy or paste, no mouse for programs that track it, and unsafe multi-line pastes ran at once; libghostty-vt's selection tracks its cells through output, soft wraps and scrollback, which a Kotlin selection over the screen mirror could not.
- **Tradeoffs**: A long press always selects, even when the program tracks the mouse, which gets only clicks and the wheel; no double- or triple-tap or rectangular selection; handles do not flip at the edges; every selection change costs a full frame; typing ends a selection.
- **Watch out**: Grid references from the API are valid only until the terminal changes, so vt.c must take fresh ones per call; under the Kitty protocol a program that exits without popping its flags leaves the shell receiving CSI u keys, Ctrl-C included.
- **Source**: ee830a0

### D8: libghostty-vt is the only terminal, and each tab is a `VtSession` owned by the service
- **What**: `TerminalService` owns one `VtSession` per tab, which keeps parsing output without a view (starting its shell once a view knows the grid, printing restored text and queued messages first, with pump, waiter and writer threads); `MainActivity` has a single `VtView` that tabs attach to and closes an ended tab on Enter; the Termux libraries, `TermuxSession` and the `Session` interface are gone, `ConsolePty` keeps only start, waitFor and close, and the fonts are main assets.
- **Why**: The debug-only prototype screen could not be saved, restored, reached by kill reports or closed like a tab; once libghostty-vt tabs covered everything the Termux tabs did, keeping two emulators meant two code paths, a `Session` interface and GPL-3.0-only vendored code in the app.
- **Tradeoffs**: Every Android build needs Zig 0.17.0 and the fetched Ghostty source (android/native D6); the bell, mouse drags, double/triple-tap, rectangular selection and edge-aware handles are still missing; `VtView`'s focus highlight is off, so keyboard focus is not shown.
- **Watch out**: libghostty-vt has been tried only on a 4 KB-page emulator with Gboard in English; real devices, 16 KB pages and CJK input methods are unchecked.
- **Source**: 6ffb912, b38d29c

### D9: Glyphs in wide cells are enlarged evenly to fill both cells
- **What**: A wide cell's glyph is scaled by two cells over the measured advance of "水" (at least 1; 1.2 with JetBrains Mono and Noto Sans CJK), centred horizontally and about the cell's vertical middle, still shrunk evenly when wider than the cells (emoji), and `VtView.setFillWide` turns it off or on from the setting in `onStart` and `onSettingsChanged`.
- **Why**: CJK text looked spaced out: each ideograph (1 em from the system fallback) sat centred in two JetBrains Mono cells (1.2 em) with 0.2 em of blank.
- **Tradeoffs**: CJK glyphs look about 20% larger and slightly heavier than Latin text, every wide glyph narrower than two cells (fullwidth forms, kana, Hangul) gets the ideograph's factor, and the IME's composing text stays at natural size.
- **Watch out**: The factor comes from one ideograph per font size, so a device whose CJK fallback is not one em wide fills differently, and at 1.2× ideographs come close to the cell's top and bottom, checked only at 6 and 32 dp on an API 34 emulator.
- **Source**: 0478dd5

### D10: Narrow windows switch tabs through a count, the current title and a list
- **What**: Below 600 dp (`screenWidthDp`, checked on every configuration change) the bar shows the tab count and the current tab's `N: title`, which open a list of tabs with the current one highlighted and a ✕ to close each; from 600 dp the row stays, each tab at most 220 dp with the current one scrolled into view; ended tabs read "(ended)" in grey.
- **Why**: On an upright phone the row fit two or three long titles, hid the rest without a hint, could leave the current tab scrolled out of view, and a long press hung a tab up without confirmation.
- **Tradeoffs**: In narrow windows other tabs' titles take a tap to see and switching takes two taps, and closing still hangs up at once, with no confirmation even when a program is running.
- **Watch out**: The 600 dp breakpoint is untuned on real phones, tablets and foldables, hardware keys go to the open list rather than the terminal, and the list's height, its overlap with the IME and TalkBack are untested on devices.
- **Source**: e29142d

### D11: Tabs can be swiped, renamed and switched with desktop shortcuts
- **What**: A horizontal swipe of at least 48 dp over the narrow switcher shows the next or previous tab without going round; a long press on a tab, the switcher or a list row offers Rename tab and Close tab; names show before the program's title; and Ctrl+Shift+T, Ctrl+Shift+W, Ctrl+Tab and Ctrl+Shift+Tab open, close and cycle tabs, listed by `onProvideKeyboardShortcuts`.
- **Why**: Reaching another tab in narrow windows took opening the list, tabs whose programs set long or identical titles could not be told apart, and hardware-keyboard users had no keys for tabs.
- **Tradeoffs**: The swipe exists only in narrow windows, programs no longer receive those four key combinations, and a name hides later program titles until it is cleared.
- **Watch out**: A swipe from a screen edge goes to the system back gesture, the shortcuts are untested with real keyboards and with IMEs that take Ctrl combinations, and `Swipe` suppresses ClickableViewAccessibility assuming the switcher keeps calling `performClick`.
- **Source**: 2af55e2

### D12: Extra keys use Termux's two-row layout and touch behaviour
- **What**: Two rows of 37.5 dp (ESC / - HOME ↑ END PGUP; TAB CTRL ALT ← ↓ → PGDN) act on release with a key-click haptic and not at all if the finger left the key, the arrows, PGUP and PGDN repeat every 80 ms after the long-press timeout, a swipe up from - sends |, and a long press locks CTRL or ALT until tapped.
- **Why**: One row of eleven keys was too cramped to read or hit, held arrows did not repeat, and Ctrl or Alt could not be kept on for several keys.
- **Tradeoffs**: The bar takes about 75 dp instead of about 39, so the terminal loses two or three rows with the IME up, | needs a swipe, and there are no custom key sets, SHIFT/FN or toggle to hide the bar.
- **Watch out**: The keys send presses only, so a program using the Kitty protocol's release events sees no release, and TalkBack and the popup in split screen or cramped landscape are untested.
- **Source**: c3c3b38

### D13: The app is Material 3 with dynamic colours, the terminal always dark
- **What**: `Theme.Appine` (Material 3 DayNight) serves Settings and the licences on shared page layouts and `Theme.Appine.Terminal` (Material 3 Dark, with a black window and system bars) serves `MainActivity`, both with DynamicColors; the terminal stays a framework `Activity` using Material dialogs and AppCompat's menus and list popup, and its hand-built tab bar and extra keys read the theme's colour roles through `MaterialColors`.
- **Why**: The user asked for Material 3, starting with Settings and then the terminal's dialogs, menus, list of tabs, tab bar and extra keys, and the Material classes need a Material theme on the activity.
- **Tradeoffs**: Views `MainActivity` inflates are the framework's, so AppCompat attributes such as `app:tint` do nothing there; the selection handles take the primary colour instead of cyan; the current tab in the row is told apart by the primary colour; Android 10/11 get Material's baseline purple; and the bar has no Material elevation or tab indicator.
- **Watch out**: Turning `MainActivity` into an `AppCompatActivity` later changes its inflation, insets and key handling, colours are read once per activity, and DynamicColors, insets and the black system bars were checked only on an API 34 emulator.
- **Source**: 55be399, 97707b7, d16025a

## Superseded

- ~~D1: Termux's terminal libraries are vendored, with `TerminalSession` rewritten around a PTY interface~~ → replaced by **D8** in b38d29c (2026-11-04)
- ~~A `Session` interface over Termux and libghostty-vt tabs, the kind chosen by `VtNative.available` (6ffb912)~~ → replaced by **D8** in b38d29c (2026-11-04)
- ~~The libghostty-vt terminal as a debug-only `VtActivity` next to the Termux tabs (394aecf)~~ → replaced by **D8** in 6ffb912 (2026-11-04)
- ~~Wide glyphs are drawn at natural size, centred, and only ever shrunk (D5 in part, 394aecf)~~ → replaced by **D9** in 0478dd5 (2026-11-04)
- ~~The terminal keeps the framework theme as `Theme.Appine.Terminal` so its dialogs and menus keep their look (55be399)~~ → replaced by **D13** in 97707b7 (2026-11-04)
