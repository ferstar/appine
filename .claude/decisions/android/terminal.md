# Terminal UI (android/terminal) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/terminal"`
>
> Last distilled: 2026-11-07 (HEAD = 2d78930). D1–D3 were seeded by hand from the retired design
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
- **Tradeoffs**: No triple-tap or rectangular selection; handles do not flip at the edges; every selection change costs a full frame; typing ends a selection.
- **Watch out**: Grid references from the API are valid only until the terminal changes, so vt.c must take fresh ones per call; under the Kitty protocol a program that exits without popping its flags leaves the shell receiving CSI u keys, Ctrl-C included.
- **Source**: ee830a0; 62f6092 (mouse tracking and double taps replaced by D25)

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

### D11: Desktop shortcuts open, close and cycle tabs
- **What**: Ctrl+Shift+T, Ctrl+Shift+W, Ctrl+Tab and Ctrl+Shift+Tab open, close and cycle tabs, listed by `onProvideKeyboardShortcuts`.
- **Why**: Hardware-keyboard users had no keys for tabs.
- **Tradeoffs**: Programs no longer receive those four key combinations, and Ctrl+Shift+W closes a tab without the confirmation × asks for while its shell runs.
- **Watch out**: The shortcuts are untested with real keyboards and with IMEs that take Ctrl combinations.
- **Source**: 2af55e2 (its swipe and renaming removed in 1eac62e)

### D12: Extra keys keep Termux's touch behaviour
- **What**: Keys act on release with a key-click haptic and not at all if the finger left the key, the arrows repeat every 80 ms after the long-press timeout, and CTRL, ALT and SHIFT are one-shot on a tap while a long press locks one (from on as well as off) and toasts "<label> locked" through `ExtraKeysView.Target.notice`, unlocking silently; the layout is D14's.
- **Why**: One row of eleven keys was too cramped to read or hit, held arrows did not repeat, modifiers could not be kept on for several keys, and a long press on a modifier a tap had turned on turned it off without saying so.
- **Tradeoffs**: A long press no longer turns a one-shot modifier off (a tap does), the toast covers the keys' first row for about two seconds, and there are no custom key sets or toggle to hide the bar.
- **Watch out**: The keys send presses only, so a program using the Kitty protocol's release events sees no release; a new caller of `Target.notice` shares the toast with tab titles; TalkBack is untested.
- **Source**: c3c3b38; ba60e25 (layout replaced by D14)

### D13: The app is Material 3 with dynamic colours, the terminal always dark
- **What**: `Theme.Appine` (Material 3 DayNight) serves Settings and the licences on shared page layouts and `Theme.Appine.Terminal` (Material 3 Dark, with a black window and system bars) serves `MainActivity`, both with DynamicColors; the terminal stays a framework `Activity` using Material dialogs and AppCompat's menus and list popup, and its hand-built tab bar and extra keys, black like the terminal, read the theme's colour roles through `MaterialColors`.
- **Why**: The user asked for Material 3, starting with Settings and then the terminal's dialogs, menus, list of tabs, tab bar and extra keys, and the Material classes need a Material theme on the activity.
- **Tradeoffs**: Views `MainActivity` inflates are the framework's, so AppCompat attributes such as `app:tint` do nothing there; the selection handles take the primary colour instead of cyan; Android 10/11 get Material's baseline purple; and the bars have no Material elevation, their indicator (D20) being drawn by hand.
- **Watch out**: Turning `MainActivity` into an `AppCompatActivity` later changes its inflation, insets and key handling, colours are read once per activity, and DynamicColors, insets and the black system bars were checked only on an API 34 emulator.
- **Source**: 55be399, 97707b7, d16025a; 883e652, d48dd10

### D14: Extra keys are a black grid of eight columns with SHIFT and SYM fixed at the right
- **What**: Two 32 dp rows (ESC / - HOME ↑ END | over TAB CTRL ALT ← ↓ → ~) in columns of max(45 dp, width / 8), scrolling below 360 dp, sit beside a fixed last column with SHIFT on top and SYM below; SHIFT is one-shot or locked like CTRL and ALT and folded with them into the key event's meta state (Shift+Tab is back-tab, CTRL+SHIFT+C/V copy and paste); PGUP, PGDN, the swipe up from - and every line between keys are gone, and both bars are black like the terminal.
- **Why**: The user wanted cells that fit 360 dp without crowding four-letter labels, Shift with Tab and the arrows instead of page keys that only send sequences, the layer key below Shift as on keyboards and away from the IME, and no lines on either bar.
- **Tradeoffs**: No page keys without a hardware keyboard; ` and forward delete are left to the IME; the shifted character follows the virtual keyboard's US map; Shift+Tab spans both rows; only labels set the black bars apart, and a key's cell shows only while pressed.
- **Watch out**: Below 360 dp the seventh column barely shows until scrolled (hence D21's fade), a character missing from the key map (CJK) leaves the modifiers on, the column width relies on the rows and grid measuring again when it changes, and the layout was tried only on emulators, not on real phones below 360 dp, in landscape or with TalkBack.
- **Source**: 883e652, 969e714, 394cd83, d70ee01, a8bdaa2

### D15: A SYM page swaps the scrolling keys for ASCII symbols
- **What**: SYM turns the other keys into ` / - = ^ \ | over [ ] { } < > ~, with % in SHIFT's cell, until tapped again, its label in the primary colour and marked by D20's indicator meanwhile; CTRL and ALT carry over and apply to symbols, SHIFT waits unused, and the page is not saved.
- **Why**: The ASCII symbols Gboard has no long press for took four taps each, and only / | ~ were on the bar.
- **Tradeoffs**: $ & ; : get no key; CTRL, ALT and a waiting SHIFT cannot be seen on the symbols page; the arrows, TAB and ESC take a tap on SYM first while it stays up.
- **Watch out**: The set follows English Gboard's long presses on one phone, other keyboards and layouts (Samsung Keyboard, CJK) differ, and a key held while another finger turns the page relies on the removed view's touch being cancelled (untested).
- **Source**: 2c024c1, d70ee01

### D16: Terminal files read in the order their code runs
- **What**: Kotlin files put types first, properties and init blocks in their original relative order, lifecycle and listener callbacks as they fire, each private helper after its first caller, small utilities and inner adapters near the end and the companion last (VtView by call chain within its sections, ExtraKeysView by its construction); vt.rs follows VtNative.kt's order with each helper after its last caller.
- **Why**: The user wanted each file to read in the order its code runs.
- **Tradeoffs**: git blame on the moved lines points at these commits unless read with -M or --color-moved; a helper shared by several features sits next to only one of them.
- **Watch out**: Code appended at the end of a class drifts from this order; reorders must keep initialised properties in their relative order, since Kotlin initialises them in textual order; a function added to VtNative.kt goes to the same place in vt.rs.
- **Source**: a546fea, 3511da5

### D17: VtView draws through androidx.core's KTX helpers
- **What**: The font-size write uses `edit { }`, and VtView's bitmap, row clip, glyph scale and cursor clip use `createBitmap`, `Canvas.withClip` and `withScale`, with `drawRow`'s body inside `withClip`.
- **Why**: Lint's UseKtx warnings now fail the build (android/native D10).
- **Tradeoffs**: `drawRow`'s body is reindented (blame needs -w), and each clip and scale runs inside try/finally.
- **Watch out**: Drawing was only compiled and linted when the helpers went in.
- **Source**: 4c1a3ce

### D18: The tab bar is one 32 dp row of numbered tabs
- **What**: In every window a 32 dp bar holds numbered 32 dp tabs (ended ones faded), then ×, + and ⋮ as system-font characters; a long press on a tab toasts its title (the program's, `sh` when none), × asks while the tab's shell runs, closing the current tab shows the one after it (before it when last), and ⋮ offers Close other tabs and Close all tabs, asking once if any runs; tab names, the switcher, its list, the swipe, the tab menu and renaming are gone.
- **Why**: Upright, reaching another tab took two taps through a list, the title the bar made room for read `sh` in almost every tab on Alpine, and closing hung up a running shell without asking.
- **Tradeoffs**: Other tabs' titles take a long press or TalkBack, tabs cannot be named or swiped, and tabs and buttons are 32 dp, under the 48 dp touch target.
- **Watch out**: Touch targets and the × glyph under vendor fonts are unchecked on real phones, × sits next to + with a confirmation only for running tabs, numbers from 100 overflow the 20 dp box, and the toast appears over the IME.
- **Source**: a8bdaa2, 1eac62e, 7c35183

### D19: The bars' labels are sized in dp
- **What**: The tab bar's and extra keys' labels use dp constants beside the bars' fixed sizes (`BAR_LABEL_DP` 14 for the buttons and `TAB_NUMBER_DP` 12 for the numbers in `MainActivity`, `LABEL_DP` 12 in `ExtraKeysView`), while dialogs, menus and toasts follow the font scale.
- **Why**: The bars are fixed in dp, so labels in sp would clip at large font scales (Android 14's nonlinear 200% turns 12 sp into about 24 dp).
- **Tradeoffs**: Users who raise the system font get no larger labels on the two bars; dimens.xml waits until sizes need per-configuration overrides, when all the bars' sizes should move together.
- **Watch out**: Not checked at a large font scale, and accessibility scanners may flag text that ignores it.
- **Source**: 4aa2abf

### D20: An indicator bar marks the current tab, locked modifiers and SYM
- **What**: A rounded 16 × 1 dp bar in the primary colour, centred 4 dp below the label's baseline (whole pixels, rounded down), is drawn by the containers in `dispatchDraw` through `ExtraKeysView.drawIndicator` under the current tab, a locked modifier and SYM while the symbols show; numbers use the default weight, and one-shot modifiers keep the primary colour alone.
- **Why**: The current tab's 2 dp box looked heavy, bold numbers outweighed every other label, the primary colour cannot be told from muted labels in grayscale (both tone 80 in Material 3's dark scheme), and a locked modifier's underline matched nothing in the tab bar.
- **Tradeoffs**: A one-shot modifier still relies on colour alone, a state change must invalidate the drawing container rather than the label, and the bar is 2 px at densities from 2 to 3.
- **Watch out**: Seen only on an API 34 emulator at density 2.75 with Roboto; the 4 dp gap assumes labels without descenders; changing a label's opacity (as D24's renumbering might) would fade the indicator too.
- **Source**: d48dd10

### D21: Scrolling rows fade out into the fixed buttons at their end
- **What**: The tab bar and the extra keys are FrameLayouts whose scrolling row spans them with an end padding as wide as the fixed part and `clipToPadding` off, the fixed part sits over the end in black except a blank left strip (12 dp of ×, 6 dp of SHIFT and SYM, as InsetDrawables), and the row's right fading edge of `ExtraKeysView.FADE_DP` (32 dp) is carried that far in by a padding offset, with no left fade; the tab row scrolls the current tab clear of the fade.
- **Why**: Nothing showed that more tabs lay beyond × or, below 360 dp, more keys beyond the last column.
- **Tradeoffs**: Items under the fixed part are drawn only to be hidden, a tap on the blank strip where a fading item shows goes to the fixed button, and a manual scroll can stop with the last tab barely faded.
- **Watch out**: The strips fit the blank before × and SHIFT's S (about 0.7 dp) at the current label sizes in Roboto, so an OEM font or label change can put the fade over a glyph; `ExtraKeysView.scrolls` must compare the grid with the room between the paddings, since `canScrollHorizontally` counts the end padding as shown; checked only on a Pixel 4a API 34 emulator, not on real devices or Android 10/11.
- **Source**: a75981b, db4f809

### D22: The current tab comes back into view 3 s after the tab row rests
- **What**: `revealCurrentTab` runs from a delayed `Runnable` `RETURN_TO_CURRENT_TAB_MS` (3 s) after the row rests; ACTION_DOWN on the row cancels it, ACTION_UP/CANCEL and every finger-less `onScrollChanged` restart the wait, and a tab change still scrolls at once.
- **Why**: Scrolling the row could leave the tab the terminal shows out of view for as long as nothing else changed.
- **Tradeoffs**: The row slides back while someone may still be looking at other tabs, a current tab partly under the fade counts as away, and 3 s is a guess.
- **Watch out**: Checked only with adb swipes and held touches on an API 34 emulator, not on a real phone, with TalkBack's scroll actions or with a hardware keyboard.
- **Source**: 1118f81

### D23: Scrolling rows are excluded from the left-edge back gesture
- **What**: The tab bar sets `systemGestureExclusionRects` to itself less the buttons' width on every layout, and the extra keys' scroll view excludes itself up to the last column only while it can scroll, with the rectangle and list allocated once; the fixed parts keep the right edge's gesture.
- **Why**: With gesture navigation, a left-edge slide over either row went back to the launcher instead of scrolling it.
- **Tradeoffs**: The left edge no longer goes back at the tab bar's 32 dp, or at the keys' 64 dp below 360 dp.
- **Watch out**: Android honours at most 200 dp of exclusion per window edge (96 dp used now); checked only on an API 34 emulator in portrait, not with an OEM's own gesture navigation, on real narrow devices or in landscape with side insets.
- **Source**: ced9f06, 9f21a29

### D24: Tabs slide into closed tabs' places before renumbering
- **What**: `renderTabs` keeps one `Tab` holder per session and changes the row in place; after a close, tabs whose place moved slide there with `translationX` (FLIP, 200 ms, Material standard easing) and then cross-fade their numbers (200 ms), closed tabs shrink to 0.6 and fade out in 150 ms in the row's overlay, and the indicator slides from its old place (it jumps when another tab is shown).
- **Why**: Closing a tab renumbered the later tabs at once, so the tab taking the closed current tab's place showed the same number under the same indicator and the switch went unnoticed.
- **Tradeoffs**: A close takes about 400 ms to settle; at the row's end the tabs before slide right rather than the next one arriving; numbers lag TalkBack's descriptions; the row keeps state across renders.
- **Watch out**: `renderTabs` matches tabs by position, relying on sessions being added only at the end (reordering tabs would need matching by session); Close other tabs is untried; seen only on an API 34 emulator.
- **Source**: afd086a

### D25: Mouse-tracking programs get drags, hover and real clicks; a double tap selects a word
- **What**: In mouse mode a long press selects the word and becomes a left-button drag once moved past the touch slop, mouse and touchpad presses bypass the gestures with their buttons and modifiers, hover goes as button-less motion, `VtView` detects double taps itself (selecting a word only without mouse tracking), and vt.c deduplicates motion per cell except in SGR-Pixels.
- **Why**: Programs tracking the mouse could not be dragged in or hovered over, double clicks in them lost the second click, and selecting a word needed a long press.
- **Tradeoffs**: A finger drag in a program needs a long press first, a still long press stays the terminal's selection so programs get no long click, there is no triple tap, and a double tap right after hiding the keyboard can select another row.
- **Watch out**: Touchpads reporting taps without `SOURCE_MOUSE` go through the gestures, mode 1003 hover while scrolled back returns the view to the bottom, and a press begun before tracking was turned on stays with the gestures.
- **Source**: 62f6092

## Superseded

- ~~D1: Termux's terminal libraries are vendored, with `TerminalSession` rewritten around a PTY interface~~ → replaced by **D8** in b38d29c (2026-11-04)
- ~~A `Session` interface over Termux and libghostty-vt tabs, the kind chosen by `VtNative.available` (6ffb912)~~ → replaced by **D8** in b38d29c (2026-11-04)
- ~~The libghostty-vt terminal as a debug-only `VtActivity` next to the Termux tabs (394aecf)~~ → replaced by **D8** in 6ffb912 (2026-11-04)
- ~~Wide glyphs are drawn at natural size, centred, and only ever shrunk (D5 in part, 394aecf)~~ → replaced by **D9** in 0478dd5 (2026-11-04)
- ~~The terminal keeps the framework theme as `Theme.Appine.Terminal` so its dialogs and menus keep their look (55be399)~~ → replaced by **D13** in 97707b7 (2026-11-04)
- ~~D10: Narrow windows switch tabs through a count, the current title and a list (e29142d)~~ → replaced by **D18** in 1eac62e (2026-11-05)
- ~~Tabs swiped over the narrow switcher, renamed and closed from a long-press menu, names shown before titles (D11 in part, 2af55e2)~~ → replaced by **D18** in 1eac62e and 7c35183 (2026-11-05)
- ~~Extra keys in two rows of 37.5 dp with PGUP and PGDN and | on a swipe up from - (D12 in part, c3c3b38), then in nine and eight columns with PGUP and PGDN fixed and 1 px lines between keys (883e652, 969e714)~~ → replaced by **D14** in 394cd83 and a8bdaa2 (2026-11-05)
- ~~SYM in the fixed column's top cell, above SHIFT (2c024c1)~~ → replaced by **D14** in d70ee01 (2026-11-05)
- ~~The tab bar and extra keys in the surface container colour (D13 in part, d16025a)~~ → replaced by **D14** in 883e652 (2026-11-05)
- ~~A long press toggles a modifier, locking only from off, and a locked modifier is underlined (D12 in part, c3c3b38)~~ → replaced by **D12** in ba60e25 and **D20** in d48dd10 (2026-11-06)
- ~~The current tab told apart by the primary colour (D13 in part, d16025a), then boxed in a 2 dp outline with a bold number (1eac62e)~~ → replaced by **D20** in d48dd10 (2026-11-06)
- ~~A long press always selects even when the program tracks the mouse, which gets only clicks and the wheel, and there is no double-tap selection (D7 in part, ee830a0)~~ → replaced by **D25** in 62f6092 (2026-11-07)
