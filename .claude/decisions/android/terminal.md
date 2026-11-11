# Terminal screen (android/terminal) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/terminal"`
>
> Last distilled: 2026-11-12 (HEAD = 0553229). D1–D3 were seeded by hand from the retired design
> doc §11.5 and the round records in `docs/rounds/`, so their sources cite rounds; later entries
> cite commits.

## Constraint index (compressed active decisions)

> Full text of D2, D3, D6, D9, D11, D12, D14–D17, D20, D22 and D23: `git show 248d79f637c8f05dadc6ef9fdb0733c568c6a468:.claude/decisions/android/terminal.md`

> Full text of D13, D19, D21, D24, D26–D29, D31, D33–D35 and D37: `git show c1a37da58eada92fc78df580db7b870071cab435:.claude/decisions/android/terminal.md`

- **D2** — Appine's messages (`VtSession.showMessage`, `appine_vt_message`) are inserted above the cursor line with the cursor saved and restored, skipped on the alternate screen, overwriting the guest's saved-cursor slot (round 195; 6ffb912)
- **D3** — Licence notices in `notices/` are generated at build time from the actual inputs and shown through Settings' AGPL statement and `LicensesActivity`, as AGPL sections 0 and 5 require (round 192; b38d29c, android/native D6)
- **D6** — vt.rs's `Tab` holds the kernel handle rather than an `Arc<Machine>` and looks the kernel up per console read, so the service must release tabs before shutting the kernel down or a restart can open the new store before the pump's read returns (ff9e2f9)
- **D9** — A wide cell's glyph is scaled evenly by two cells over the measured advance of "水" (1.2 with JetBrains Mono and Noto Sans CJK), centred and still shrunk when wider, switched by `VtView.setFillWide` from the setting, and a CJK fallback that is not one em wide fills differently (0478dd5)
- **D11** — Ctrl+Shift+T, Ctrl+Shift+W, Ctrl+Tab and Ctrl+Shift+Tab open, close and cycle tabs (listed by `onProvideKeyboardShortcuts`) and never reach programs, and Ctrl+Shift+W closes a running tab without ×'s confirmation (2af55e2)
- **D12** — Extra keys act on release with a haptic, repeat the arrows every 80 ms and make CTRL, ALT and SHIFT one-shot on a tap and locked by a long press with a toast through `ExtraKeysView.Target.notice` (shared with tab titles), and they send presses only, so Kitty-protocol programs see no releases (c3c3b38; ba60e25)
- **D13** — The app is Material 3 with dynamic colours (`Theme.Appine` DayNight for Settings and the licences, `Theme.Appine.Terminal` Material 3 Dark with black bars for `MainActivity`, which stays a framework `Activity` using Material dialogs and AppCompat's menus), so turning `MainActivity` into an `AppCompatActivity` changes its inflation, insets and key handling, and colours are read once per activity (55be399, 97707b7, d16025a)
- **D14** — Extra keys are two black 32 dp rows (ESC / - HOME ↑ END | over TAB CTRL ALT ← ↓ → ~) in columns of max(45 dp, width / 8), scrolling below 360 dp, beside a fixed column of SHIFT over SYM folded into the key event's meta state, with no page keys or lines, and the column width relies on the rows and grid measuring again when it changes (883e652, 969e714, 394cd83, d70ee01, a8bdaa2)
- **D15** — SYM swaps the scrolling keys for ` / - = ^ \ | [ ] { } < > ~ with % in SHIFT's cell until tapped again, unsaved, with CTRL and ALT carrying over, and a key held while the page turns relies on the removed view's touch being cancelled (2c024c1, d70ee01)
- **D16** — Terminal Kotlin files read in execution order (types first, callbacks as they fire, each helper after its first caller, companion last) and vt.rs follows VtNative.kt's order, and reorders must keep initialised properties in their textual order, which is the order Kotlin initialises them in (a546fea, 3511da5)
- **D17** — Drawing and preference writes use androidx.core's KTX helpers (`edit { }`, `createBitmap`, `Canvas.withClip`, `withScale`) because lint's UseKtx warnings fail the build (android/native D10) (4c1a3ce)
- **D19** — The tab bar's and extra keys' labels are dp constants (`BAR_LABEL_DP` 14, `TAB_NUMBER_DP` 12, `LABEL_DP` 12) that ignore the font scale so the fixed-size bars never clip, while dialogs, menus and toasts follow it (4aa2abf)
- **D20** — A rounded 16 × 1 dp primary-colour bar 4 dp below the label's baseline, drawn by the containers in `dispatchDraw` through `ExtraKeysView.drawIndicator`, marks the current tab, locked modifiers and SYM, so a state change must invalidate the drawing container and fading a label fades its indicator too (d48dd10)
- **D21** — The tab row and the extra keys scroll under their fixed buttons with an end padding as wide as them and `clipToPadding` off, fading over `ExtraKeysView.FADE_DP` (32 dp) up to a blank strip, so `ExtraKeysView.scrolls` must compare the grid with the room between the paddings and an OEM font or label change can put the fade over a glyph (a75981b, db4f809)
- **D22** — `revealCurrentTab` brings the current tab back into view `RETURN_TO_CURRENT_TAB_MS` (3 s) after the tab row rests, a touch on the row cancelling the wait and a release or finger-less scroll restarting it, while a tab change still scrolls at once (1118f81)
- **D23** — The tab bar less its buttons and, while it can scroll, the extra keys up to the last column are excluded from the left-edge back gesture, within Android's 200 dp per window edge, which the selection handles' rectangles (D34) share (ced9f06, 9f21a29)
- **D24** — `TabBarView.render` keeps one holder per session and matches tabs by position, sliding moved tabs and the indicator (200 ms) and rolling only the changed digits (300 ms), which relies on sessions being added only at the end and on tabular digits (afd086a, 1c6c1bc, 759d0b4)
- **D26** — The root is padded by the system bars, the cutout and the IME without clipping, and a `WindowInsetsAnimationCompat` callback draws the extra keys along the IME's animation, so a view added to the root that draws past its bounds shows in the bars' room and another view meant to follow the IME needs its own translation (cee87ba)
- **D27** — `TabBarView` owns the tab bar (tabs, numbers, animations, the return to the current tab, the back-gesture exclusion and its constants) behind a `Target` that `MainActivity` implements, and must be built after DynamicColors is applied because it reads its colours when built (d965886)
- **D28** — Strings naming the app say Appine in both locales, while the package, `libappine.so`, log tags, thread names and XTVERSION keep "appine" (3f04bf5)
- **D29** — ⋮ opens a `PopupMenu` with `Gravity.END` in `Widget.Appine.MorePopup` (a toolbar overflow style) offset by -3.5 dp and 4 dp, tuned by eye for left-to-right with ⋮ at the screen's edge (1ee3562)
- **D31** — Without mouse tracking a tap on a link (an OSC 8 URI, else an http(s) URL in its soft-wrapped line) opens it, only http, https, mailto and `file:` links (as android/documents' documents) open, and a long press puts Open link first in the floating menu (1cc504b)
- **D33** — `appine_vt_message` writes a CAN before a kill report when the parser is not at ground, so the report's room depends on that check and the unfinished sequence is dropped (fcf4c93, 3beb038, b43ff37)
- **D34** — Selection handles flip at the screen's edges, and their bounds plus the touch slop are excluded from gesture navigation from `onDraw`, sharing Android's 200 dp per edge with the bars (D23) (f77b591)
- **D35** — Every `VtNative` body runs in `guarded!` without throwing, so a panic shows as a missing frame or an ignored key, and one inside the pump leaves that tab's output unread until it is reopened (be72f31)
- **D37** — `KEYCODE_MENU` reaches programs as `CSI 29 ~`, ⋮ is labelled "More options", and `VtNative.resize` returns the resize replies (mode 2048) for `VtSession.resize` to write at once (64e5c33)

## Active (full detail)

### D4: The terminal screen is a module of its own; the sessions and the view are android/vt's and android/vtview's
- **What**: android/terminal holds `MainActivity`, `TabBarView`, `TabNumber`, `SearchBarView`, `ProgressLine`, `ExtraKeysView`, `LicensesActivity`, the shared view helpers in `Ui.kt` and `res/` (tabs, the tab bar, shortcuts, the search bar, the extra keys, insets and the IME's animation, the progress line, opening links, the clipboard question, the app's strings and themes, the licence screen), while the sessions, consoles and the vt.c/vt.rs shim are android/vt's, the view, glyphs, keys and terminal font android/vtview's, and building libghostty-vt android/native's.
- **Why**: §11.5 of the retired design doc needed a map and the app was split into modules by responsibility, and by 2026-11-11 the terminal map (133 KB) could not be read in one Read while most terminal commits touch only the screen, the sessions or the view.
- **Tradeoffs**: A feature spanning the screen and the terminal (search, links, the clipboard question, progress) is described in two or three maps that point at each other's sections, and decisions made before the split keep their android/terminal tags here (listed in the android/vt and android/vtview preambles).
- **Watch out**: The map records no confirmed bugs; its open questions include a nearly full background progress bar looking like the current tab's indicator, clipboard reads from background tabs or left open by a recreation, and whether Ctrl+Shift+W should ask before closing a running shell.
- **Source**: 12c4afa, d42013d (file list since updated by 394aecf, 6ffb912, b38d29c, d965886 and 4311150; map fully refreshed in d9e1f0c; debts since closed by 22c8ab0, ddbfb9b, 3f45ad0, 600700b and e12f560)

### D5: Terminal state lives in libghostty-vt inside the native library, drawn by Appine's own view
- **What**: Each tab has one libghostty-vt 1.0.0 terminal in the native library, fed straight from the kernel console by a pump thread that writes the terminal's replies back and asks for one frame of dirty rows per burst; `VtView` mirrors the frames onto an integer cell grid in JetBrains Mono (glyphs scaled evenly, never stretched, with wide cells filled as in D9; box drawing, blocks and Powerline triangles as shapes; ligatures off), keys go through libghostty-vt's encoder, grapheme clustering (mode 2027) is on and synchronized output holds the screen for at most 1 s.
- **Why**: The Termux view flattened CJK (horizontal-only scaling to the cell), broke box-drawing lines between rows, showed Powerline symbols as boxes and split emoji sequences; Ghostty's full renderer targets macOS and GTK, while libghostty-vt is a plain C library for the terminal state, released on its own with a stable C ABI since 1.0.
- **Tradeoffs**: CJK glyphs still come from the system fallback fonts; programs that count widths per codepoint disagree with mode 2027 on emoji sequences; the 1.0 port made a line drag take in blank rows and gave terminals DECIC, DECDC, DECBI and DECFI.
- **Watch out**: The frame format (vt.c and `VtFrame`, android/vtview D3) and the key numbers (`VtKeys`, from libghostty-vt's header, which its stable ABI keeps) must change together with their counterparts, and `void *` get and set values are not type checked, so each upgrade must check every key's documented type.
- **Source**: 394aecf; 0478dd5; 172be7c; 49e8b08 (the frame's reader moved to `VtFrame`)

### D7: Selection, paste, mouse and focus go through libghostty-vt
- **What**: Under the tab's lock, vt.c keeps the selection as the terminal's own (started by a long press or a double or triple tap through libghostty-vt's selection gesture, D40, extended by drags and handles and autoscrolled beyond the grid), pastes through `ghostty_terminal_paste` (asking with a preview when it returns REJECTED), and encodes mouse clicks and wheel, alternate-scroll arrows (mode 1007) and focus (mode 1004); frame and row headers carry the selection for `VtView`'s highlight and handles, keys add releases and lone modifiers for the Kitty protocol, and the IME turns a lone newline into Enter and a multi-line commit into a paste.
- **Why**: The prototype could only type keys and scroll: no copy or paste, no mouse for programs that track it, and unsafe multi-line pastes ran at once; libghostty-vt's selection tracks its cells through output, soft wraps and scrollback, which a Kotlin selection over the screen mirror could not.
- **Tradeoffs**: Every selection change costs a full frame, and typing ends a selection.
- **Watch out**: Grid references from the API are valid only until the terminal changes, so vt.c must take fresh ones per call; under the Kitty protocol a program that exits without popping its flags leaves the shell receiving CSI u keys, Ctrl-C included.
- **Source**: ee830a0; 62f6092 (mouse tracking and double taps replaced by D25); f77b591 (handles at the edges, D34); ddbfb9b, 48982fe (lines, rectangles and the gesture, D40)

### D8: libghostty-vt is the only terminal, and each tab is a `VtSession` owned by the service
- **What**: `TerminalService` owns one `VtSession` per tab, which keeps parsing output without a view (starting its shell once a view knows the grid, restoring its saved screen (D44) and printing queued messages first, with pump, waiter and writer threads) and tells the service's kill-report routing through `consoleOpen` (started and not hung up) whether its console is still open, since `console` keeps its number after close (android/service D8); `MainActivity` has a single `VtView` that tabs attach to and closes an ended tab on Enter; the Termux libraries, `TermuxSession` and the `Session` interface are gone, `ConsolePty` keeps only start, waitFor and close, and the fonts are main assets.
- **Why**: The debug-only prototype screen could not be saved, restored, reached by kill reports or closed like a tab; once libghostty-vt tabs covered everything the Termux tabs did, keeping two emulators meant two code paths, a `Session` interface and GPL-3.0-only vendored code in the app.
- **Tradeoffs**: Every Android build needs Zig 0.17.0 and the fetched libghostty-vt source (android/native D6), and `VtView`'s focus highlight is off, so keyboard focus is not shown.
- **Watch out**: libghostty-vt has been tried only on a 4 KB-page emulator with Gboard in English; real devices, 16 KB pages and CJK input methods are unchecked.
- **Source**: 6ffb912, b38d29c; 62f6092 and f77b591 (drags, double taps and edge-aware handles since added, D25 and D34); d89b5fa (the bell since wired, D36); 6560af6 (`consoleOpen`, for a bug d9e1f0c recorded); ddbfb9b (triple taps and rectangles, D40); 600700b (snapshots, D44)

### D18: The tab bar is one 32 dp row of numbered tabs
- **What**: In every window a 32 dp bar holds numbered 32 dp tabs (ended ones faded), then ×, + and ⋮ as system-font characters; a long press on a tab toasts its title (the program's, `sh` when none), × and ⋮'s Close other tabs and Close all tabs close at once unless a tab they close is busy (its shell running and not waiting at a marked prompt, D43), asking once otherwise, closing the current tab shows the one after it (before it when last), + opens a session in the current tab's directory, and ⋮ also offers Find (D41), Clear screen and Reset terminal (D45) and Settings; tab names, the switcher, its list, the swipe, the tab menu and renaming are gone.
- **Why**: Upright, reaching another tab took two taps through a list, the title the bar made room for read `sh` in almost every tab on Alpine, closing hung up a running shell without asking, and then every × on a live tab asked even with its shell idle at the prompt.
- **Tradeoffs**: Other tabs' titles take a long press or TalkBack, tabs cannot be named or swiped, tabs and buttons are 32 dp, under the 48 dp touch target, and a shell without prompt marks always counts as busy.
- **Watch out**: Touch targets and the × glyph under vendor fonts are unchecked on real phones, × sits next to + with a confirmation only for busy tabs, numbers from 100 overflow the 20 dp box, and the toast appears over the IME.
- **Source**: a8bdaa2, 1eac62e, 7c35183; 6c5f921 (closing at the prompt and new tabs' directory); ddbfb9b, 600700b (Find, Clear screen and Reset terminal in ⋮)

### D25: Mice select as on a desktop; mouse-tracking programs get drags, hover and real clicks
- **What**: Under mouse tracking without Shift, a long press selects the word and becomes a left-button drag past the touch slop, mouse and touchpad presses go to the program with their buttons and modifiers, hover goes as button-less motion and vt.c deduplicates motion per cell except in SGR-Pixels; otherwise a mouse press takes a pointer path through libghostty-vt's selection gesture with timed presses (clicks counting to words and lines, Ctrl for a command's output, Ctrl+Alt for rectangles, Shift extending), a right click selects the word unless a selection holds the cell and pins the floating menu there, Ctrl+click opens a link, a plain click moves the prompt's cursor and shows the IME, and touchpad swipes and pinches stay finger gestures.
- **Why**: Programs tracking the mouse could not be dragged in or hovered over and lost a double click's second click, and elsewhere a mouse or touchpad acted like a finger (a drag scrolled, selecting needed a long press, and under tracking a mouse could not select at all).
- **Tradeoffs**: A finger drag in a program needs a long press first and a still long press stays the terminal's selection; a plain click no longer opens links, a left-button selection shows no handles or menu (Ctrl+Shift+C or a right click), Shift always overrides tracking, the middle button only focuses, and the extra keys' CTRL counts for neither Ctrl+click nor the link hand.
- **Watch out**: Touchpads reporting taps without `SOURCE_MOUSE` go through the gestures, and on Android 11–13 touchpad swipes are not classified and could select; mode 1003 hover while scrolled back returns the view to the bottom; gesture behaviours cross JNI as raw libghostty-vt enum values; tried only with a uinput mouse on an API 34 emulator.
- **Source**: 62f6092, 77d7f36

### D30: The terminal shows a fading scroll bar and flings like a ScrollView
- **What**: vt.c writes libghostty-vt's scroll bar state (total, offset, length) into the frame header, which `VtView` returns from `computeVerticalScroll*` for View's own fading scroll bar in the theme's thumb, awakened when the view moves the viewport but not on output; a fling runs an `OverScroller` in `computeScroll` through the same paths as a drag (the viewport to either end, or at most a screen of wheel events or arrow keys), and a touch, other input, focus loss or another tab stops it, the tap that stops one doing nothing else.
- **Why**: Nothing showed where the viewport was in up to 10,000 lines of scrollback, and a fling ended as soon as the finger lifted.
- **Tradeoffs**: The tap that stops a fling neither opens the keyboard nor clicks; the bar flashes whenever the window becomes visible and covers the last column while shown; Alpine's busybox less stays on the primary screen, so flinging in it scrolls the scrollback.
- **Watch out**: The frame format changed on both sides (vt.c and `VtFrame`) and must keep changing together (D5); flings were tried only with adb swipes on an API 34 emulator, and flings sent as wheel events to a mouse-tracking program are untested.
- **Source**: c7e849d (the header has grown since); 49e8b08 (read by `VtFrame`)

### D32: Kitty graphics are drawn from direct transmissions; the Glyph Protocol stays off
- **What**: Each screen stores up to 32 MiB of Kitty images sent by direct transmission (file, temporary-file and shared-memory media answer `EINVAL`, since they would name the host's files), PNG is decoded with the Wuffs libghostty-vt.a contains, frames carry a 64-byte image header and 56-byte placements sorted as Ghostty sorts them with a virtual placement's rows joined, and `VtView` keeps one Bitmap per shown image by generation, painting the three layers into each row with bilinear filtering; the Glyph Protocol stays off, and the libghostty-vt features still unwired (OSC 7, the two-phase render update) are listed in the map.
- **Why**: Programs that show images (chafa, yazi, timg, `kitten icat`) fell back to text though libghostty-vt stores Kitty images and its render state says where to draw them, while registered glyphs cannot be drawn through the C API, so advertising the Glyph Protocol would bring glyphs that never appear.
- **Tradeoffs**: The shown tab's images take memory twice (the library's and the Bitmaps; the view drops its copies while hidden, android/vtview D2, and background tabs lose theirs only under memory pressure, android/vt D3); any image change repaints every row; placeholder rows partly in the viewport keep the rounding of their first visible source row; chafa's own detection still picks symbols.
- **Watch out**: Decoding a large PNG holds the tab's lock on the pump thread; the frame format, vt.rs's FFI and `VtFrame` must change together; tried only on a 4 KB-page API 34 emulator and macOS, not with yazi, timg, `kitten icat`, tmux placeholders, real devices or a full 32 MiB store.
- **Source**: fcf4c93, c510bee; d89b5fa; 8811c32; 49e8b08 (read by `VtFrame`)

### D36: Bells, OSC 52 clipboard writes and reads, OSC 9/777 notifications and cursor blinking are wired
- **What**: vt.c records effects during `feed` (a bell count, the last clipboard write as text/plain up to 256 KiB or a clear, the last notification's title and body up to 4 KiB, and OSC 52 reads, found by stopping at OSC boundaries), the pump takes them after each read (`appine_vt_take_effects`) and calls `VtSession.onBell`, `onClipboard`, `onNotification` and the clipboard-read path (routed by android/service D19), a read from the tab in front is asked about in `MainActivity` and answered through the tab's writer in Ghostty's form (empty when denied), `VtView.bell` vibrates with the REJECT haptic at most every 100 ms, the cursor blinks every 500 ms while the terminal's cursor blinks (D47) and the view and window have focus, and `MainActivity` shows the tab a notification names (`EXTRA_TAB`) once bound.
- **Why**: Programs' bells, OSC 52 writes and reads, OSC 9/777 notifications and blinking cursors were ignored though libghostty-vt reports them and Ghostty asks before a read, so copying from tmux or neovim and noticing a CLI waiting for input in another tab were impossible.
- **Tradeoffs**: Several notifications in one read keep only the last; only text clipboard contents are supported, and a write over 256 KiB is denied rather than cut; a read from a tab not in front gets an empty clipboard, and OSC 5522 reads and Kitty paste events stay refused; bells and notifications pass through the main thread's queue.
- **Watch out**: The effects struct is shared by vt.c and vt.rs and must change on both sides; any app write that can end a program's string must clear vt.c's `osc_open`; the vibration depends on the system's touch-feedback setting; real devices and Android 11 and 12 were not tried.
- **Source**: d89b5fa (with android/service D19); 90911a2, 600700b (clipboard reads)

### D38: Terminals keep 10,000 lines or 10 MiB of scrollback, default to white on black, list DA1 52 and cap OSC 5522 writes at 1 MiB
- **What**: vt.c limits the scrollback to 10,000 lines or 10 MiB, whichever comes first, sets white on black as the default colours with no cursor colour (so the cursor follows the foreground, as in Ghostty), lists 52 in DA1 as Ghostty does unless clipboard writes are denied, and refuses OSC 5522 writes over 1 MiB in all their representations.
- **Why**: libghostty-vt's C terminal starts with a 10,000-byte scrollback limit (about two pages, while the map said 10,000 lines) and no default colours, DA1 left out the clipboard the app supports, and an OSC 5522 write could buffer 64 MiB, so colour queries and reverse video did nothing and programs skipped OSC 52.
- **Tradeoffs**: Wider than about 120 columns the scrollback keeps fewer than 10,000 lines (about 4,700 at 250); an OSC 12 query answers the foreground; an OSC 5522 write over 1 MiB fails whole (`EFBIG`), below the protocol's 64 MiB minimum; a full reset keeps colours set with OSC 10–12, as in Ghostty.
- **Watch out**: A full scrollback holds 3.5–10 MiB a tab, given back only by compression (D42); the figures come from a macOS build, not a device or 16 KB pages; `VtView` paints the defaults it reads from the render state, so they must stay white on black.
- **Source**: b5980ae, 90911a2, 755a96b

### D39: Programs' progress and status reports show on the tabs until their program gives the terminal back
- **What**: vt.c keeps the last OSC 9;4 report and queues every OSC 7501 report, `VtSession` keeps the 7501 records as the specification says (one per id, at most 64) and the one most in need of the user, and the pump notes the console's foreground group after a report (kernel/tty D22), dropping progress and the working, blocked and idle records once another group has the terminal while done and error go when the tab is seen; `ProgressLine` draws the current tab's progress as Ghostty does (2 dp, a sweep without a percentage, 15 s), the other tabs get a bar in the indicator's place and a status dot read by TalkBack, background statuses notify (android/service D21), and XTGETTCAP TN answers xterm-256color, the console's TERM.
- **Why**: Programs' progress and status reports were dropped, so a tab could not show that its program was working or waiting for the user, and TN went unanswered.
- **Tradeoffs**: A report from a command that exits at once is dropped at the next read, and a background job's report is tied to whatever group had the foreground; XTGETTCAP still answers other capabilities from Ghostty's own entry; the tab row redraws every frame during a sweep.
- **Watch out**: A nearly full background progress bar looks like the current tab's indicator (open in the map); a program that hands the foreground to a child group of its own loses its marks; `AppineVtEffects` and `RawEffects` must change together; untried with real programs and on devices.
- **Source**: 22c8ab0

### D40: Taps, long presses and keys select through libghostty-vt's selection gesture, by words, lines or rectangles
- **What**: Without mouse tracking `VtView` counts taps and starts libghostty-vt's selection gesture on a second or third tap's press (words, or lines through `ghostty_terminal_select_line` with semantic prompt boundaries, as Ghostty's double and triple clicks) and on a long press, finger drags extend it through the gesture while handle drags use `select_to`, the floating menu toggles a selection between linear and rectangular, and Shift with the arrows, Page Up/Down or Home/End adjusts an existing selection (`appine_vt_select_adjust`).
- **Why**: The terminal could select neither a line nor a rectangle and no drag followed a double or triple tap, though libghostty-vt derives line selections, takes rectangular ones and offers Ghostty's own click-selection gesture.
- **Tradeoffs**: A tap followed at once by a drag selects instead of scrolling; a long-press word drag over cells whose word has gone keeps the selection, as Ghostty does; the gesture's own click counting, autoscroll ticks and deep press stay unused; fingers have no Alt-drag rectangles; the release of a Shift key that adjusted a selection still reaches the program; a double tap right after hiding the keyboard can select another row.
- **Watch out**: A libghostty-vt upgrade that changes untimed presses or the behaviour table breaks the gesture; drags were tried only with sendevent on an API 34 emulator, not with a real finger or past the grid's edges.
- **Source**: ddbfb9b, 5256ae2, 600700b, 48982fe

### D41: The scrollback is searched from a bar in the tab bar's place, with matches marked in the cells and on the scroll bar
- **What**: `SearchBarView` stands in for `TabBarView` (Find in ⋮ or Ctrl+Shift+F; Enter or ↑ for older, Shift+Enter or ↓ for newer, nothing selected until a step; Back, ×, Esc or another tab closing it, the last text kept); vt.c drives one `GhosttySearch` from `appine_vt_frame` for at most 4 ms per frame, marks the viewport's matches in cell attribute bits 14–15 with the count in the header and steps back to the selected match's place after a resize, and `appine_vt_search_rows` gives the matches' rows for 2 dp marks under the scroll bar's thumb, taken again when the count or the selected match changes or every 500 ms while frames change.
- **Why**: There was no way to find text in the scrollback, though libghostty-vt searches the screen and scrollback and lists every match.
- **Tradeoffs**: The tab bar is hidden while searching, so tabs switch only with Ctrl+Tab, which ends the search; the extra keys still type into the terminal; a resize with a match selected blocks on finishing the search; vt.c keeps a copy of all matches (about 64 bytes each) while searching, and marks may lag a pruned scrollback by up to 500 ms.
- **Watch out**: The frame format changes in vt.c and `VtFrame` together; reselecting by place could pick another match if a resize changed which exist; libghostty-vt's search (case-insensitive for ASCII only) may change with an upgrade; the Android 16 Back path is untried on a device.
- **Source**: ddbfb9b, 48982fe; 49e8b08 (read by `VtFrame`)

### D42: Idle tabs' scrollback is compressed, and all of it on a memory trim
- **What**: Releasing vt.rs's `TermGuard` asks vt.c (`appine_vt_compress_wanted`) whether the activity token changed or a read restored pages and then schedules the tab on the `vt-compress` thread, which waits 250 ms per change (at most 5 s from scheduling, `COMPRESS_MAX_WAIT`) and runs incremental steps 1 ms apart under the tab's lock; `appine_vt_transcript` returns NULL while `transcribed`, so `VtSession` keeps its last transcript, and `VtNative.compressAll` runs a full pass on every tab when the service trims (android/service D4).
- **Why**: A full scrollback held 3.5–10 MiB a tab uncompressed, while libghostty-vt compresses cold pages to about a fifth and restores them when read but leaves the scheduling to the embedder.
- **Tradeoffs**: Steps take the tab's lock (up to 5 ms under endless output), a tab with constant output is compressed every 5 s where Ghostty would wait, a tab whose text changed is still formatted at every save, and compressed pages keep their virtual memory reserved.
- **Watch out**: A new vt.c path that reads beyond the viewport without setting `compress_touched` or writes without clearing `transcribed`, or a caller locking `tab.term` directly, loses compression or keeps a stale transcript; 16 KB-page devices and the battery cost of the 250 ms looks are untested.
- **Source**: 3f45ad0

### D43: Prompt marks let tabs close at the prompt, jump between commands, select output and time commands
- **What**: vt.c reads libghostty-vt's prompt state only: `appine_vt_at_prompt` makes `VtSession.busy` false so closing does not ask (D18), `appine_vt_jump_prompt` serves Ctrl+Shift+Up/Down on the primary screen (passed to the program on the alternate one), `appine_vt_output_at` backs the menu's Select output, new tabs start in the current tab's foreground-group directory (`consoleCwd`), and `on_semantic_prompt` times a command from the C mark (for busybox ash, from the cursor leaving the prompt) to the next D for `VtSession.onCommandFinished` (android/service D21); OSC 7 stays unwired.
- **Why**: Every × on a live tab asked even with the shell idle at its prompt, there was no way to move between commands or take one command's output, new tabs always started at home and long commands ended unnoticed, though libghostty-vt tracks prompts, input and output once shells mark them (android/setup D20).
- **Tradeoffs**: Shells without marks (zsh, fish, dash, nested non-login shells, ssh) still ask on every close and get no jumps, output or timing; ash counts a long command's wrapped part as output and starts its timing early after background output; Ctrl+Shift+Up/Down never reach programs on the primary screen; `consoleCwd` takes the BKL on the main thread when a tab opens.
- **Watch out**: With marks libghostty-vt clears the prompt's rows on every resize and relies on the shell to redraw them; `busy` follows whatever the shell sends, so a user's own prompt changes it; bash in the app on Debian, real devices, hardware keyboards and TalkBack are untried.
- **Source**: 6c5f921, 5256ae2

### D44: Restored tabs come back from libghostty-vt snapshots recorded with the library's version
- **What**: A terminal is encoded as a snapshot only after a change (at most 16 MiB, with 64 KiB continuation tracking) and recorded with libghostty-vt's own version ("1.0.0"); a restored terminal is settled (CAN, primary screen, DECSTR, program modes reset by name, Kitty flags popped, link closed, title cleared, OSC 133;C before a new line, the rest erased) with the same options set through a shared `wrap()`, and a snapshot from another version is not decoded, the tab falling back to its transcript (android/service D5).
- **Why**: Restored tabs lost colours, images and layout with plain transcripts, and since 1.0.0 libghostty-vt's own version string names its release, where 1.4's always said 0.1.0-dev and the source directory's name had to stand in.
- **Tradeoffs**: Any libghostty-vt upgrade, even a minor one, sends tabs back to their transcripts once, since the snapshot format is outside the ABI promise; a restored program's modes are reset.
- **Watch out**: `settle`'s mode list must follow modes libghostty-vt adds; the format has no compatibility guarantee within a version either; images in snapshots, bash and real devices are untried.
- **Source**: 600700b, 172be7c

### D45: Clear screen and Reset terminal act as Ghostty's
- **What**: ⋮'s Clear screen (`appine_vt_clear`) is Ghostty's clear_screen, unavailable on the alternate screen, and Reset terminal (`appine_vt_reset`) is Ghostty's reset of both screens, the scrollback and modes, keeping the app's options and the colours set with OSC 10–12; a shell at its prompt then gets a form feed to draw the prompt again.
- **Why**: There was no way to clear or reset a terminal.
- **Tradeoffs**: The form feed after a reset goes beyond Ghostty, and Reset terminal keeps the tab's progress and status records, unlike a program's RIS (open in the map).
- **Watch out**: The reset forgets how the prompt moves its cursor (D48) until the shell's next mark.
- **Source**: 600700b, 5256ae2

### D46: Touch scrolls by pixels and settles on a row; library logs, unknown sequences and the sideways wheel are wired
- **What**: The render state asks for one overscan row below (header byte 58, row `y == rows`), `VtView` draws the grid up to a row higher (`scrollPx`), scrolls the viewport by pixels and settles on the nearest row in 150 ms through the fling's `OverScroller`, a tap during settling still acting; `AXIS_HSCROLL` reaches mouse-tracking programs as buttons 6 and 7; `on_log` (warnings and errors) and `on_unknown_sequence` (an OSC's number or an APC's letter) share 20 log lines a second; KAM stays ignored, as Ghostty ignores it by default.
- **Why**: Comparing libghostty-vt with the tabs found overscan, the sideways wheel and the library's log unused and touch scrolling jumping by whole rows.
- **Tradeoffs**: Wheel and alternate-scroll arrows still go by rows; the mirror and bitmap hold an extra row; touches map through `scrollPx`, the overscan row's visible part counting as the last row.
- **Watch out**: The frame format (byte 58, rows up to `y == rows`) must change with its counterparts; vt.c numbers rows by iterator, so overscan above the viewport would shift them; during a synchronized-output hold the overscan byte is the capture's; pixel scrolling was tried only with adb on an emulator and sideways scrolling not on a device.
- **Source**: 5256ae2

### D47: The cursor blinks by default, and its shape and blinking are settings
- **What**: `wrap()` sets `DEFAULT_CURSOR_BLINK` (DECSCUSR's default, which DECSTR and RIS restore; DECSCUSR and mode 12 still stop it), `settle()` no longer resets modes 12 and 1001, and `appine_vt_set_cursor` sets libghostty-vt's default cursor shape and blink, which `VtSession` gives a terminal when it is made and on every change of the settings (android/service D22, android/setup D21).
- **Why**: Comparing libghostty-vt with the tabs found its cursor-blink default unused, and the user then wanted to choose the cursor's shape and blinking.
- **Tradeoffs**: A focused cursor redraws twice a second, and a shape changed while a 1049 program shows does not reach the primary screen.
- **Watch out**: Cursor styles cross JNI as raw libghostty-vt enum values.
- **Source**: 5256ae2, 77d7f36

### D48: Taps move the shell's cursor, password prompts show a lock, and OSC 22 sets the pointer
- **What**: vt.c keeps `prompt_click` from OSC 133;A (read from an OSC of at most 64 bytes, forgotten on a reset) and `appine_vt_prompt_click` leaves arrows (or Kitty's SGR press for `click_events`) in the replies for a tap without mouse tracking that opened no link; the frame's cursor style 4 is a padlock while vt.c's `password_input` is set (by the pump at each read and every 200 ms while the lock shows in a visible window, kernel/tty D23), and the IME then gets `TYPE_TEXT_VARIATION_PASSWORD` with `IME_FLAG_NO_PERSONALIZED_LEARNING` through a posted `restartInput`; header byte 59 carries OSC 22's shape, and `VtView` sets Android's closest system pointer (a hand over a link without mouse tracking) and returns it from `onResolvePointerIcon` for non-mouse events.
- **Why**: After comparing libghostty-vt 1.0.0's C API with vt.c, the user chose three Ghostty behaviours the tabs lacked: taps on the prompt moving the shell's cursor (most useful on a touch screen), the lock cursor at password prompts with the IME's password mode as the counterpart of macOS secure input, and OSC 22 pointers.
- **Tradeoffs**: Taps move nothing for shells that declare no `cl=` or `click_events` and on ash's rows after the first of a long command; a program that turns echo off without printing shows no lock until its next output; each password prompt restarts the IME, ending a composition and changing Gboard's layout; hovering calls `VtNative.link` once per cell; Ghostty's modifier pointers have no counterpart.
- **Watch out**: The frame format changed on both sides (cursor style 4, byte 59); `onResolvePointerIcon`'s handling of non-mouse events was seen on API 34 only, and the IME restart is untried with CJK IMEs and hardware keyboards.
- **Source**: 2b04a43

### D49: tmux control mode is detached, not supported
- **What**: `appine_vt_feed` stops at DCS starts and ends, finds tmux's `1000p` header, puts a carriage return among the replies to detach it and reports the ST end as `onTmuxDetached`, which the service shows as a notice in the tab (android/service D15).
- **Why**: `tmux -CC`, swallowed as an unknown DCS by a libghostty-vt built without Oniguruma, left the tab blank while keys reached tmux as commands, and the C API offers nothing for control mode.
- **Tradeoffs**: Control mode is not supported, nothing retries if tmux ignores the carriage return, and DCS strings cost two more parser stops.
- **Watch out**: The detach relies on tmux taking a lone carriage return as an empty line (3.5a and 3.7c do).
- **Source**: 77d7f36

### D50: A recreated `MainActivity` keeps its tab, and a display size change recreates it
- **What**: `onMachineReady` shows the activity's own current tab, else the service's `current`, else `focus`, else the last; `onCreate` reads `EXTRA_TAB` only without a saved state and not from history; `MainActivity` and the licences leave `density` out of `configChanges` (Settings too, android/setup D22), and the font size is kept as `font_size_dp`, an old pixel `font_size` being converted at the current density on first read.
- **Why**: A new `MainActivity` lost the tab being viewed (falling back to the restore-only `focus`, and a recreated one re-read its first intent's `EXTRA_TAB`), and a display size change left the tab bar, extra keys, terminal font and `VtView`'s slops at their old pixel sizes.
- **Tradeoffs**: A display size change drops open dialogs, the search bar and the extra keys' modifiers and page, as a locale or font scale change already did, and an old pixel size is taken at the density in force at the first launch after the update.
- **Watch out**: An open clipboard question lost to a recreation leaves that tab's reads unanswered (open in the map), and Back on Android 11 was not tried on a device.
- **Source**: e12f560

### D51: `TabNumber` has a file of its own, and the screens share their view helpers from `Ui.kt`
- **What**: `TabNumber.kt` holds the tab number view as an internal top-level class with its roll timing and blur constants, reading `TabBarView.STANDARD`'s easing through its companion, and `Ui.kt` gives `Context` and `View` the extensions `dp`, `themeColor`, `themeDrawable` and `touchFeedback`, replacing the private copies.
- **Why**: `TabBarView.kt` held a second view class, and `dp`, `themeColor` and `touchFeedback` were copied into `MainActivity`, `LicensesActivity`, `TabBarView`, `SearchBarView` and `ExtraKeysView` (VtView had its own `themeDrawable`).
- **Tradeoffs**: The helpers are package-wide names rather than private members, and a View's `dp` reads the density from resources on each call rather than from a cached field.
- **Watch out**: A member named `dp` or `themeColor` in one of these classes would silently shadow the shared extension.
- **Source**: 4311150

### D52: `MainActivity` reads the wide-cell setting through `Settings`
- **What**: `onStart` and `onSettingsChanged` call `Settings.fillWide` (android/service D11 and D23) for `VtView.setFillWide` (D9).
- **Why**: The static `TerminalService.fillWide` moved to `Settings`.
- **Tradeoffs**: None beyond one more class the activity names.
- **Watch out**: None known; it is still read before the service is bound.
- **Source**: 0553229

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
- ~~Slid tabs cross-fade their numbers (200 ms), and the indicator jumps when another tab is shown (D24 in part, afd086a)~~ → replaced by **D24** in 1c6c1bc and 759d0b4 (2026-11-07)
- ~~No bell (D8's tradeoff, b38d29c), and OSC 52 and cursor blinking among the libghostty-vt features left unwired (D32 in part, fcf4c93)~~ → replaced by **D36** in d89b5fa (2026-11-09)
- ~~OSC 9;4, OSC 7501 and TN among the unwired libghostty-vt features (D4's watch-out and D32, fcf4c93)~~ → replaced by **D39** in 22c8ab0 (2026-11-10)
- ~~No triple-tap or rectangular selection and no search (D4, D7, D8, D25 and D32 in part, 12c4afa, ee830a0, b38d29c, 62f6092, fcf4c93)~~ → replaced by **D40** and **D41** in ddbfb9b (2026-11-10)
- ~~Scrollback compression among the unwired libghostty-vt features (D4's watch-out and D32, 12c4afa, fcf4c93)~~ → replaced by **D42** in 3f45ad0 (2026-11-10)
- ~~libghostty-vt does not advertise Kitty graphics: storage limit 0, frames carrying cells only (D32 in part, fcf4c93, c510bee)~~ → replaced by **D32** in 8811c32 (2026-11-10)
- ~~× asks whenever the tab's shell runs (D18 in part, 1eac62e)~~ → replaced by **D18** and **D43** in 6c5f921 (2026-11-10)
- ~~Clipboard reads go unanswered (D36 in part, d89b5fa)~~ → replaced by **D36** in 600700b (2026-11-10, no SUPERSEDES declared)
- ~~The cursor blinks only while a program asks for it (D36 in part, d89b5fa)~~ → replaced by **D47** in 5256ae2 (2026-11-10, no SUPERSEDES declared)
- ~~A long press selects a word as vt.c's tracked anchor (D7 in part, ee830a0), and word drags go through `anchor_words` (5256ae2)~~ → replaced by **D40** in 48982fe (2026-11-10, no SUPERSEDES declared)
- ~~libghostty-vt 1.4.0 from the Ghostty release, with `VtKeys` numbers from Ghostty 1.4.0's header (D5, 394aecf, 0478dd5), and the snapshot version taken from the Ghostty source's name (600700b)~~ → replaced by **D5** and **D44** in 172be7c (2026-11-11)
- ~~Mouse and touchpad presses always bypass the gestures (D25 in part, 62f6092)~~ → replaced by **D25** in 77d7f36 (2026-11-11)
- ~~android/terminal holds the `Vt*` view and session files, `ConsolePty`, the terminal fonts and the vt.c/vt.rs shim (D4, 12c4afa)~~ → replaced by **D4** in d42013d (2026-11-11)
- ~~Views keep the sizes they were built with after a display size change (D4's watch-out, d9e1f0c)~~ → replaced by **D50** in e12f560 (2026-11-11, no SUPERSEDES declared)
