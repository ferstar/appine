# Terminal screen (android/terminal) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/terminal"`
>
> Last distilled: 2026-11-15 (HEAD = 221a89b1). D1–D3 were seeded by hand from the retired design
> doc §11.5 and the round records in `docs/rounds/`, so their sources cite rounds; later entries
> cite commits.

## Constraint index (compressed active decisions)

> Full text of D51–D52: `git show 410ef192ce13aa39fc4661c0cfa032f917066801:.agents/decisions/android/terminal.md`

> Full text of D2, D6, D9, D11, D12, D16–D17, D20, D22 and D23: `git show 248d79f637c8f05dadc6ef9fdb0733c568c6a468:.claude/decisions/android/terminal.md`

> Full text of D19, D21, D24, D26–D29, D33–D35 and D37: `git show c1a37da58eada92fc78df580db7b870071cab435:.claude/decisions/android/terminal.md`

- **D2** — Appine's messages (`VtSession.showMessage`, `appine_vt_message`) are inserted above the cursor line with the cursor saved and restored, skipped on the alternate screen, overwriting the guest's saved-cursor slot (round 195; 6ffb912)
- **D6** — vt.rs's `Tab` holds the kernel handle rather than an `Arc<Machine>` and looks the kernel up per console read, so the service must release tabs before shutting the kernel down or a restart can open the new store before the pump's read returns (ff9e2f9)
- **D9** — A wide cell's glyph is scaled evenly by two cells over the measured advance of "水" (1.2 with JetBrains Mono and Noto Sans CJK), centred and still shrunk when wider, switched by `VtView.setFillWide` from the setting, and a CJK fallback that is not one em wide fills differently (0478dd5)
- **D11** — Ctrl+Shift+T, Ctrl+Shift+W, Ctrl+Tab and Ctrl+Shift+Tab open, close and cycle tabs (listed by `onProvideKeyboardShortcuts`) and never reach programs, and Ctrl+Shift+W closes a running tab without ×'s confirmation (2af55e2)
- **D12** — Extra keys act on release with a haptic, repeat the arrows every 80 ms and make CTRL, ALT and SHIFT one-shot on a tap and locked by a long press with a toast through `ExtraKeysView.Target.notice` (shared with tab titles), and they send presses only, so Kitty-protocol programs see no releases (c3c3b38; ba60e25)
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
- **D33** — `appine_vt_message` writes a CAN before a kill report when the parser is not at ground, so the report's room depends on that check and the unfinished sequence is dropped (fcf4c93, 3beb038, b43ff37)
- **D34** — Selection handles flip at the screen's edges, and their bounds plus the touch slop are excluded from gesture navigation from `onDraw`, sharing Android's 200 dp per edge with the bars (D23) (f77b591)
- **D35** — Every `VtNative` body runs in `guarded!` without throwing, so a panic shows as a missing frame or an ignored key, and one inside the pump leaves that tab's output unread until it is reopened (be72f31)
- **D37** — `KEYCODE_MENU` reaches programs as `CSI 29 ~`, ⋮ is labelled "More options", and `VtNative.resize` returns the resize replies (mode 2048) for `VtSession.resize` to write at once (64e5c33)

- **D51** — Internal `TabNumber` owns its roll/blur constants and uses `TabBarView.STANDARD`, while shared `Ui.kt` dp/themeColor/themeDrawable/touchFeedback extensions read current resource density and can be silently shadowed by same-named members (4311150)

- **D52** — `MainActivity` reads `Settings.fillWide` in onStart and onSettingsChanged for VtView.setFillWide, including before service binding (0553229)

> Full text of D45, D47, D49, D53, D54: `git show 221a89b12e1dffa7b1432b9076078c1bffde46cb:.agents/decisions/android/terminal.md`

- **D45** — Clear screen follows Ghostty on the primary screen only, Reset terminal clears both screens, scrollback and modes while keeping Appine options and OSC 10–12 colors, and an at-prompt shell receives form feed; reset retains progress/status records and forgets prompt cursor movement until the next mark (600700b, 5256ae2)
- **D47** — Cursor blinking defaults on and shape/blink settings reach every session, while DECSCUSR and mode 12 still override them, settle preserves modes 12/1001, focused blinking redraws twice per second, changing shape under mode 1049 misses the primary screen, and raw cursor enum values must stay aligned across JNI (5256ae2, 77d7f36)
- **D49** — vt.c detects tmux control-mode DCS 1000p, sends a carriage return to detach and reports the ST end to the service; control mode has no support or retry, DCS parsing adds two stops, and the behavior depends on tmux accepting the blank command (verified with 3.5a/3.7c) (77d7f36)
- **D53** — The default-enabled volume shortcut reads its preference per press, switches once only when another tab exists, consumes repeats/releases only after switching, ends search and otherwise preserves system volume, with dialogs retaining volume behavior; adb-only tests leave IME/accessibility interception and lost-release state unverified (61df6915; 20137e4c)
- **D54** — TabBarView groups render, slide/indicator movement, reveal/return, status marks and Tab helpers, matches sessions by identity and keeps safe property-initializer order, while marks/animations remain together with duplicated alpha arithmetic (807e9ba1; 20137e4c)

## Active (full detail)

### D31: Tapped links open in other apps, `file:` links as the provider's documents
- **What**: Without mouse tracking a tap opens an OSC 8 link or detected HTTP(S) URL, and long press offers Open link; supported schemes remain http, https, mailto and file, while TerminalLinks opens HTTP(S) through unpinned Custom Tabs and shared startViewer handling, preserving browsable categorization, missing-handler feedback, guest-file routing and other supported protocols; guest bridge requests require MainActivity focus and return launch acceptance.
- **Why**: Terminal links and guest browser requests should share the project-link browsing experience with a meaningful launch result.
- **Tradeoffs**: Installed handlers choose Custom Tabs, browsers or associated apps, background guest requests fail immediately, and acceptance does not prove page load or OAuth completion.
- **Watch out**: Verify device navigation, App Links, alternate handlers, missing handlers and complete OAuth callbacks; guest files still require the provider access gate.
- **Source**: 1cc504b; d8dfedb2, 8e211142

### D15: A SYM page swaps the scrolling keys for ASCII symbols
- **What**: SYM below SHIFT toggles an unsaved symbol page with $, *, & and ; replacing the former /, -, | and ~ positions, retaining the other symbols, bracket pairs and fixed % key.
- **Why**: Replacing duplicate symbols makes room for four additional shell characters.
- **Tradeoffs**: The removed duplicates remain on the main page, CTRL and ALT carry over, and users must learn the new positions.
- **Watch out**: A held key still depends on view-removal touch cancellation; device interaction with the new layout remains unverified.
- **Source**: 2c024c1, d70ee01; 34df148a

### D13: The app is Material 3 with dynamic colours, the terminal always dark
- **What**: Activities use Material 3 dynamic colors with a dark terminal theme; MainActivity now extends FragmentActivity for AndroidX authentication and Back dispatch while retaining Material dialogs and AppCompat menus.
- **Why**: The user asked for Material 3, starting with Settings and then the terminal's dialogs, menus, list of tabs, tab bar and extra keys, and the Material classes need a Material theme on the activity.
- **Tradeoffs**: Views `MainActivity` inflates are the framework's, so AppCompat attributes such as `app:tint` do nothing there; the selection handles take the primary colour instead of cyan; Android 11 gets Material's baseline purple; and the bars have no Material elevation, their indicator (D20) being drawn by hand.
- **Watch out**: Apply dynamic colors before constructing views, retain secure activity/dialog windows, and verify insets, key handling and vendor authentication behavior when changing activity bases.
- **Source**: 55be399, 97707b7, d16025a; 883e652, d48dd10; f61072f3; 0709982

### D3: License notices are checked-in offline assets
- **What**: Settings exposes the AGPL statement and license navigation backed by a static catalog and HTML notices, with packaging in android/native and the reader now in android/licenses.
- **Why**: License attribution must remain available offline while notice resources are maintained explicitly in the repository.
- **Tradeoffs**: Dependency and bundled-package changes require manual notice updates rather than automatic build discovery.
- **Watch out**: Keep versions, attribution and full texts synchronized with packaged dependencies and upstream reference copies.
- **Source**: round 192 (`192-license`); list updated in b38d29c (android/native D6); bd580227, 9c6c51a7

### D4: The terminal screen owns controls and shared UI resources
- **What**: android/terminal owns MainActivity, TerminalLayout, TerminalLinks, TerminalFiles, bars, extra keys, Ui.kt and shared resources, while privacy, licenses, Kotlin sessions, native terminal state, interaction and rendering have separate registered modules.
- **Why**: Independent authentication, reading, session, native, interaction and rendering concerns made the original terminal map too large to maintain as one unit.
- **Tradeoffs**: Features span several maps, shared page/string/theme resources and Ui.showPrivate remain here, and earlier decisions retain their original tags and IDs.
- **Watch out**: Preserve final input and clipboard checks, explicit shared-file exclusions and the distinction between frame-driven search highlights and view overlays.
- **Source**: 12c4afa, d42013d (file list since updated by 394aecf, 6ffb912, b38d29c, d965886 and 4311150; map fully refreshed in d9e1f0c; debts since closed by 22c8ab0, ddbfb9b, 3f45ad0, 600700b and e12f560); b58c6494, 165b5f99, 9c6c51a7, 9015b244, 81f60bef

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
- **What**: vt.c keeps the last OSC 9;4 report and queues every OSC 7501 report, `VtSession` keeps the 7501 records as the specification says (one per id, at most 64) and the one most in need of the user, and the pump notes the console's foreground group after a report (kernel/tty D22), dropping progress and the working, blocked and idle records once another group has the terminal while done and error go when the tab is seen; `ProgressLine` draws the current tab's progress with the shared `Ui.line()` thickness (1 dp, at least one pixel), a sweep without a percentage and a 15 s duration, the other tabs get a bar in the indicator's place and a status dot read by TalkBack, background statuses notify (android/service D21), and XTGETTCAP TN answers xterm-256color, the console's TERM; switching to an equal non-null report takes the immediate jump path instead of retaining the previous animation.
- **Why**: Programs' progress and status reports were dropped, so a tab could not show that its program was working or waiting for the user, and TN went unanswered.
- **Tradeoffs**: A report from a command that exits at once is dropped at the next read, and a background job's report is tied to whatever group had the foreground; XTGETTCAP still answers other capabilities from Ghostty's own entry; the tab row redraws every frame during a sweep.
- **Watch out**: A nearly full background progress bar looks like the current tab's indicator (open in the map); a program that hands the foreground to a child group of its own loses its marks; `AppineVtEffects` and `RawEffects` must change together; untried with real programs and on devices.
- **Source**: 22c8ab0; 37fa3f17

### D40: Taps, long presses and keys select through libghostty-vt's selection gesture, by words, lines or rectangles
- **What**: Without mouse tracking `VtView` counts taps and starts libghostty-vt's selection gesture on a second or third tap's press (words, or lines through `ghostty_terminal_select_line` with semantic prompt boundaries, as Ghostty's double and triple clicks) and on a long press, finger drags extend it through the gesture while handle drags use `select_to`, the floating menu toggles a selection between linear and rectangular, and Shift with the arrows, Page Up/Down or Home/End adjusts an existing selection (`appine_vt_select_adjust`).
- **Why**: The terminal could select neither a line nor a rectangle and no drag followed a double or triple tap, though libghostty-vt derives line selections, takes rectangular ones and offers Ghostty's own click-selection gesture.
- **Tradeoffs**: A tap followed at once by a drag selects instead of scrolling; a long-press word drag over cells whose word has gone keeps the selection, as Ghostty does; the gesture's own click counting, autoscroll ticks and deep press stay unused; fingers have no Alt-drag rectangles; the release of a Shift key that adjusted a selection still reaches the program; a double tap right after hiding the keyboard can select another row.
- **Watch out**: A libghostty-vt upgrade that changes untimed presses or the behaviour table breaks the gesture; drags were tried only with sendevent on an API 34 emulator, not with a real finger or past the grid's edges.
- **Source**: ddbfb9b, 5256ae2, 600700b, 48982fe

### D41: The scrollback is searched from a bar in the tab bar's place, with matches marked in the cells and on the scroll bar
- **What**: `SearchBarView` stands in for `TabBarView` (Find in ⋮ or Ctrl+Shift+F; Enter or ↑ for older, Shift+Enter or ↓ for newer, nothing selected until a step; AndroidX Back, ×, Esc or another tab closing it, the last text kept); vt.c drives one `GhosttySearch` from `appine_vt_frame` for at most 4 ms per frame, marks the viewport's matches in cell attribute bits 14–15 with the count in the header and steps back to the selected match's place after a resize, and `appine_vt_search_rows` gives the matches' rows for 2 dp marks under the scroll bar's thumb, taken again when the count or the selected match changes or every 500 ms while frames change.
- **Why**: There was no way to find text in the scrollback, though libghostty-vt searches the screen and scrollback and lists every match.
- **Tradeoffs**: The tab bar is hidden while searching, so Ctrl+Tab or enabled volume-key switching ends the search; the extra keys still type into the terminal; a resize with a match selected blocks on finishing the search; vt.c keeps a copy of all matches (about 64 bytes each) while searching, and marks may lag a pruned scrollback by up to 500 ms.
- **Watch out**: The frame format changes in vt.c and `VtFrame` together; reselecting by place could pick another match if a resize changed which exist; libghostty-vt's search (case-insensitive for ASCII only) may change with an upgrade; the Android 16 Back path is untried on a device; the field disables fallback line spacing and the count uses 14 dp labels, risking tall-glyph clipping and less input width on narrow screens.
- **Source**: ddbfb9b, 48982fe; 49e8b08 (read by `VtFrame`); 61df6915; dfbf8eab; 5e1f851d; 20137e4c; f61072f3

### D42: Idle tabs' scrollback is compressed, and all of it on a memory trim
- **What**: Releasing vt.rs's `TermGuard` asks vt.c (`appine_vt_compress_wanted`) whether the activity token changed or a read restored pages and then schedules the tab on the `vt-compress` thread, which waits 250 ms per change (at most 5 s from scheduling, `COMPRESS_MAX_WAIT`) and runs incremental steps 1 ms apart under the tab's lock; `appine_vt_transcript` returns NULL while `transcribed`, so `VtSession` keeps its last transcript, and `VtNative.compressAll` runs a full pass on every tab when the service trims (android/service D4).
- **Why**: A full scrollback held 3.5–10 MiB a tab uncompressed, while libghostty-vt compresses cold pages to about a fifth and restores them when read but leaves the scheduling to the embedder.
- **Tradeoffs**: Steps take the tab's lock (up to 5 ms under endless output), a tab with constant output is compressed every 5 s where Ghostty would wait, a tab whose text changed is still formatted at every save, and compressed pages keep their virtual memory reserved.
- **Watch out**: A new vt.c path that reads beyond the viewport without setting `compress_touched` or writes without clearing `transcribed`, or a caller locking `tab.term` directly, loses compression or keeps a stale transcript; 16 KB-page devices and the battery cost of the 250 ms looks are untested.
- **Source**: 3f45ad0

### D43: Prompt marks let tabs close at the prompt, jump between commands, select output and time commands
- **What**: vt.c reads libghostty-vt's prompt state only: `appine_vt_at_prompt` makes `VtSession.busy` false so closing does not ask (D18), `appine_vt_jump_prompt` serves Ctrl+Shift+Up/Down on the primary screen (passed to the program on the alternate one), `appine_vt_output_at` backs the menu's Select output, new tabs start in the current tab's foreground-group directory (`consoleCwd`), and `on_semantic_prompt` times a command from the C mark (for busybox ash, from the cursor leaving the prompt) to the next D for `VtSession.onCommandFinished` (android/service D21); OSC 7 stays unwired.
- **Why**: Every × on a live tab asked even with the shell idle at its prompt, there was no way to move between commands or take one command's output, new tabs always started at home and long commands ended unnoticed, though libghostty-vt tracks prompts, input and output once shells mark them (android/setup D20).
- **Tradeoffs**: Unmarked shells still ask on close and lack prompt features, but zsh, fish/nushell and supported nested bash/ash now have integration (android/setup D20 and android/vt D5–D7); Ctrl+Shift+Up/Down remain reserved on the primary screen and consoleCwd takes the BKL on tab creation.
- **Watch out**: With marks libghostty-vt clears the prompt's rows on every resize and relies on the shell to redraw them; `busy` follows whatever the shell sends, so a user's own prompt changes it; bash in the app on Debian, real devices, hardware keyboards and TalkBack are untried.
- **Source**: 6c5f921, 5256ae2; a7d8c9f5; a809c5a8; daab4446; 7aa817af (child-module refinements); 20137e4c

### D44: Restored tabs come back from libghostty-vt snapshots recorded with the library's version
- **What**: A terminal is encoded as a snapshot only after a change (at most 16 MiB, with 64 KiB continuation tracking) and recorded with libghostty-vt's own version ("1.0.0"); a restored terminal is settled (CAN, primary screen, DECSTR, program modes reset by name, Kitty flags popped, link closed, title cleared, OSC 133;C before a new line, the rest erased) with the same options set through a shared `wrap()`, and a snapshot from another version is not decoded, the tab falling back to its transcript (android/service D5).
- **Why**: Restored tabs lost colours, images and layout with plain transcripts, and since 1.0.0 libghostty-vt's own version string names its release, where 1.4's always said 0.1.0-dev and the source directory's name had to stand in.
- **Tradeoffs**: Any libghostty-vt upgrade, even a minor one, sends tabs back to their transcripts once, since the snapshot format is outside the ABI promise; a restored program's modes are reset.
- **Watch out**: `settle`'s mode list must follow modes libghostty-vt adds; the format has no compatibility guarantee within a version either; images in snapshots, bash and real devices are untried.
- **Source**: 600700b, 172be7c

### D46: Touch scrolls by pixels and settles on a row; library logs, unknown sequences and the sideways wheel are wired
- **What**: The render state asks for one overscan row below (header byte 58, row `y == rows`), `VtView` draws the grid up to a row higher (`scrollPx`), scrolls the viewport by pixels and settles on the nearest row in 150 ms through the fling's `OverScroller`, a tap during settling still acting; `AXIS_HSCROLL` reaches mouse-tracking programs as buttons 6 and 7; `on_log` (warnings and errors) and `on_unknown_sequence` (an OSC's number or an APC's letter) share 20 log lines a second; KAM stays ignored, as Ghostty ignores it by default.
- **Why**: Comparing libghostty-vt with the tabs found overscan, the sideways wheel and the library's log unused and touch scrolling jumping by whole rows.
- **Tradeoffs**: Wheel and alternate-scroll arrows still go by rows; the mirror and bitmap hold an extra row; touches map through `scrollPx`, the overscan row's visible part counting as the last row.
- **Watch out**: The frame format (byte 58, rows up to `y == rows`) must change with its counterparts; vt.c numbers rows by iterator, so overscan above the viewport would shift them; during a synchronized-output hold the overscan byte is the capture's; pixel scrolling was tried only with adb on an emulator and sideways scrolling not on a device.
- **Source**: 5256ae2

### D48: Taps move the shell's cursor, password prompts show a lock, and OSC 22 sets the pointer
- **What**: vt.c keeps `prompt_click` from OSC 133;A (read from an OSC of at most 64 bytes, forgotten on a reset) and `appine_vt_prompt_click` leaves arrows (or Kitty's SGR press for `click_events`) in the replies for a tap without mouse tracking that opened no link; the frame's cursor style 4 is a padlock while vt.c's `password_input` is set (by the pump at each read and every 200 ms while the lock shows in a visible window, kernel/tty D23), and the IME then gets `TYPE_TEXT_VARIATION_PASSWORD` with `IME_FLAG_NO_PERSONALIZED_LEARNING` through a posted `restartInput`; header byte 59 carries OSC 22's shape, and `VtView` sets Android's closest system pointer (a hand over a link without mouse tracking) and returns it from `onResolvePointerIcon` for non-mouse events.
- **Why**: After comparing libghostty-vt 1.0.0's C API with vt.c, the user chose three Ghostty behaviours the tabs lacked: taps on the prompt moving the shell's cursor (most useful on a touch screen), the lock cursor at password prompts with the IME's password mode as the counterpart of macOS secure input, and OSC 22 pointers.
- **Tradeoffs**: Taps move nothing for shells that declare no `cl=` or `click_events` while marked ash continuation rows now work (android/vtview D6); after background output, earlier-row movement remains limited (android/vt D6); a program that turns echo off without printing shows no lock until its next output; each password prompt restarts the IME, ending a composition and changing Gboard's layout; hovering calls `VtNative.link` once per cell; Ghostty's modifier pointers have no counterpart.
- **Watch out**: The frame format changed on both sides (cursor style 4, byte 59); `onResolvePointerIcon`'s handling of non-mouse events was seen on API 34 only, and the IME restart is untried with CJK IMEs and hardware keyboards.
- **Source**: 2b04a43; a809c5a8; daab4446 (child-module refinements); 20137e4c

### D50: A recreated `MainActivity` keeps its tab, and a display size change recreates it
- **What**: `onMachineReady` shows the activity's own current tab, else the service's `current`, else `focus`, else the last; `onCreate` reads `EXTRA_TAB` only without a saved state and not from history; `MainActivity` and the licences leave `density` out of `configChanges` (Settings too, android/setup D22), and the font size is kept as `font_size_dp`, an old pixel `font_size` being converted at the current density on first read.
- **Why**: A new `MainActivity` lost the tab being viewed (falling back to the restore-only `focus`, and a recreated one re-read its first intent's `EXTRA_TAB`), and a display size change left the tab bar, extra keys, terminal font and `VtView`'s slops at their old pixel sizes.
- **Tradeoffs**: A display size change drops open dialogs, the search bar and the extra keys' modifiers and page, as a locale or font scale change already did, and an old pixel size is taken at the density in force at the first launch after the update.
- **Watch out**: An open clipboard question lost to a recreation leaves that tab's reads unanswered (open in the map), and Back on Android 11 was not tried on a device.
- **Source**: e12f560

### D14: Extra keys add a persistent function-key page through SYM

- **What**: The black two-row grid retains fixed SHIFT/SYM and width/scroll behavior; SYM long press toggles a function page with F1–F12 and repeating PGUP/PGDN, tap returns to the main page, and long-click accessibility shares modifier-aware key handling.
- **Why**: Function-key programs need keys and Shift combinations absent from the soft keyboard.
- **Tradeoffs**: The page has no hint, arrows/CTRL/ALT are on other pages, SHIFT persists between main and function pages, and SYM has no lock or one-shot state.
- **Watch out**: Only the shown grid is attached and refreshed, column-width changes need remeasurement, and real-program/device/TalkBack long clicks remain unverified.
- **Source**: 883e652, 969e714, 394cd83, d70ee01, a8bdaa2, ed8c208a, b79baa10; 20137e4c

### D55: Settings and licenses share a frame but use distinct rows

- **What**: Settings binds MaterialSwitch through the standard preference widget slot, while licenses keeps a component title/summary row and shared page frame, with the dedicated offline reader described in D58.
- **Why**: Standard Preferences replace custom Settings rows, and license creation/transition code repeated responsibilities.
- **Tradeoffs**: License asset reads remain synchronous, and the screens no longer share row implementations.
- **Watch out**: Preference upgrades must preserve switch binding; license navigation, restoration and theme recreation were not exercised on a device.
- **Source**: 55be399, d35669e1, f8822b8b; 20137e4c; bd580227

### D56: Settings labels distinguish choices, current values and screen-off policy

- **What**: English and Chinese labels keep login choices distinct from username summaries, distinguish writable home from read-only system files behind the shared access gate, simplify CJK/cursor/volume hints, retain Tier numbers and session/screen-off notification state, add project/version labels and offer Paste as is or Drop trailing newlines.
- **Why**: Settings and paste actions need concise labels that describe the actual choice and access boundary.
- **Tradeoffs**: Summaries omit tutorials, performance ratios, wraparound examples and instantaneous lock status, revocation limits stay in developer documentation, and long English paste buttons stack while the checked Chinese layout uses one row.
- **Watch out**: Preserve placeholders and confirmation meaning; final copy, vendor fonts and larger accessibility scales still need visual checks.
- **Source**: b2f96a63, 75123eef, cf4f5e20, 1e5de64e; 20137e4c; d0a74e94, 0b3c5d8a, d0ddf170, 6f5be4da

### D57: Keep screen on is a persistent activity preference

- **What**: A default-off overflow toggle above Settings persists in MainActivity preferences and applies FLAG_KEEP_SCREEN_ON with a themed standard checkbox.
- **Why**: Watching logs or long output should not require repeated touches to avoid timeout.
- **Tradeoffs**: The choice covers every tab and survives launches, increasing battery use while the terminal is visible.
- **Watch out**: Keep it distinct from the service screen-off run policy; vendor behavior, power use and accessibility remain unverified.
- **Source**: 5d82c4c3

### D58: License lists open a dedicated offline reader

- **What**: LicenseCatalog supplies stable IDs and localized summaries, the case-insensitive component list opens LicenseActivity with wrapping themed HTML and relative scroll restoration, and local links stay in the catalog while HTTP(S) uses Custom Tabs.
- **Why**: The former same-activity fixed-column reader made navigation and reading awkward.
- **Tradeoffs**: The reader adds a WebView lifecycle and WebKit dependency, catalog reads stay synchronous and full license texts are reached through related components.
- **Watch out**: Renderer recovery, zoom, large fonts and tablets need more device coverage; reader ownership is now android/licenses but its pre-split history stays here.
- **Source**: bd580227, 9c6c51a7

### D59: Layout and link handling are cohesive helpers beside MainActivity

- **What**: TerminalLayout owns the hierarchy, status visibility, insets and IME animation, while TerminalLinks owns routing with the shared toast callback and MainActivity retains lifecycle and session coordination.
- **Why**: Construction, animation and routing obscured activity lifecycle, search and shortcuts.
- **Tradeoffs**: Two internal classes replace one large activity and expose interactive views to its coordinator.
- **Watch out**: Construct after dynamic colors and assign before root attachment; preserve session detachment and destroyed-activity guards, with IME/navigation device regressions still unverified.
- **Source**: b58c6494

### D60: A shared lifecycle gate protects activities without stopping workloads

- **What**: AndroidX authentication uses process-local unlock state and retained prompt generations, locks on the last stop and screen-off, protects activity/dialog windows and defers sensitive callbacks behind a scrollable shield.
- **Why**: Returning to terminal or secondary pages must not reveal protected content before authentication.
- **Tradeoffs**: Visible paused pages may remain visible while protected operations are denied, and access protection neither encrypts stored data nor stops guest programs.
- **Watch out**: Real biometric lockout/enrollment changes, overlapping windows, vendor credential screens and Back behavior need device coverage; gate ownership is now android/privacy.
- **Source**: f61072f3, 9c6c51a7

### D61: Diagnostic export authorizes a specific handoff with fresh authentication

- **What**: EXPORT_LOGS requires fresh generation-bound authorization before collection and handoff, permits only the prepared save across the picker and suppresses automatic unlock on the returning gate's next resume.
- **Why**: An authorized save should finish without a second prompt while ordinary protected content stays locked.
- **Tradeoffs**: An intentional archive transfer can finish while the page is locked and the user must explicitly unlock to return to protected content.
- **Watch out**: Prompt deferral belongs to the current activity gate; real biometric and vendor prompt behavior remain unverified, and this pre-split contract stays here after android/privacy takes ownership.
- **Source**: c7309c0c, 9c6c51a7

### D62: The file button imports copies into the resolved login home

- **What**: The @ action uses OpenMultipleDocuments to copy into the native login home with ownership, sanitized names and numbered collisions, retains work/results in an AndroidViewModel, gates initiation/presentation on privacy and explicitly copies shell-escaped paths without a newline.
- **Why**: CLI users need a direct file picker and usable paths to the imported copies.
- **Tradeoffs**: The target ignores the tab cwd and effective user, a batch holds the rootfs read lock, and import has neither cancellation nor process-death recovery.
- **Watch out**: Slow providers delay replacement, interruption or cleanup failure can leave partial files, and cloud/vendor pickers remain unverified; displayed paths use monospace after a blank line.
- **Source**: 165b5f99

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
- ~~D14’s lack of page keys~~ → replaced by **D14** in ed8c208a (2026-11-12)
- ~~D41’s Ctrl+Tab-only tab switching during search~~ → replaced by **D41** in 61df6915 (2026-11-12)
- ~~Shared Settings/licenses list_item rows and the Settings-only list_header from 55be399~~ → replaced by **D55** in f8822b8b (2026-11-13)
- ~~D3 build-generated license notices~~ → replaced by **D3** in bd580227 (2026-11-14, cross-module packaging supersession recorded in android/native)
- ~~D4 ownership of license pages and the pre-split privacy implementation~~ → replaced by **D4** in 9c6c51a7 (2026-11-14)
- ~~D13 framework Activity base~~ → replaced by **D13 and D60** in f61072f3 (2026-11-14, implicit compatibility refinement)
- ~~D15 duplicated /, -, | and ~ on the SYM page~~ → replaced by **D15** in 34df148a (2026-11-14)
- ~~D31 plain VIEW handling for HTTP(S)~~ → replaced by **D31** in d8dfedb2 (2026-11-14, implicit refinement)
- ~~D56 system-directory-only labels and outside-home hint~~ → replaced by **D56** in d0ddf170 (2026-11-14)
- ~~The same-activity showPage license transition from d35669e1 retained under D55~~ → replaced by **D58** in bd580227 (2026-11-14, fixed-column sizing has no matching explicit predecessor Decision and remains unresolved)
- ~~Version-specific search Back callbacks from ddbfb9b~~ → replaced by **D41 and D60** in f61072f3 (2026-11-14)
