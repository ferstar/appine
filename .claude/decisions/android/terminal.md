# Terminal UI (android/terminal) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/terminal"`
>
> Last distilled: 2026-11-10 (HEAD = 300d384). D1–D3 were seeded by hand from the retired design
> doc §11.5 and the round records in `docs/rounds/`, so their sources cite rounds; later entries
> cite commits.

## Constraint index (compressed active decisions)

> Full text of D2, D3, D5, D6, D9, D11, D12, D14–D18, D20, D22 and D23: `git show 248d79f637c8f05dadc6ef9fdb0733c568c6a468:.claude/decisions/android/terminal.md`

- **D2** — Appine's messages (`VtSession.showMessage`, `appine_vt_message`) are inserted above the cursor line with the cursor saved and restored, skipped on the alternate screen, overwriting the guest's saved-cursor slot (round 195; 6ffb912)
- **D3** — Licence notices in `notices/` are generated at build time from the actual inputs and shown through Settings' AGPL statement and `LicensesActivity`, as AGPL sections 0 and 5 require (round 192; b38d29c, android/native D6)
- **D5** — Each tab's state is a libghostty-vt 1.4.0 terminal in the native library, fed by a pump thread and drawn by `VtView` on an integer cell grid in JetBrains Mono, so the frame format (vt.c and `VtView`) and the key numbers (`VtKeys`, from Ghostty's header) must change together with their counterparts (394aecf, 0478dd5)
- **D6** — vt.rs's `Tab` holds the kernel handle rather than an `Arc<Machine>` and looks the kernel up per console read, so the service must release tabs before shutting the kernel down or a restart can open the new store before the pump's read returns (ff9e2f9)
- **D9** — A wide cell's glyph is scaled evenly by two cells over the measured advance of "水" (1.2 with JetBrains Mono and Noto Sans CJK), centred and still shrunk when wider, switched by `VtView.setFillWide` from the setting, and a CJK fallback that is not one em wide fills differently (0478dd5)
- **D11** — Ctrl+Shift+T, Ctrl+Shift+W, Ctrl+Tab and Ctrl+Shift+Tab open, close and cycle tabs (listed by `onProvideKeyboardShortcuts`) and never reach programs, and Ctrl+Shift+W closes a running tab without ×'s confirmation (2af55e2)
- **D12** — Extra keys act on release with a haptic, repeat the arrows every 80 ms and make CTRL, ALT and SHIFT one-shot on a tap and locked by a long press with a toast through `ExtraKeysView.Target.notice` (shared with tab titles), and they send presses only, so Kitty-protocol programs see no releases (c3c3b38; ba60e25)
- **D14** — Extra keys are two black 32 dp rows (ESC / - HOME ↑ END | over TAB CTRL ALT ← ↓ → ~) in columns of max(45 dp, width / 8), scrolling below 360 dp, beside a fixed column of SHIFT over SYM folded into the key event's meta state, with no page keys or lines, and the column width relies on the rows and grid measuring again when it changes (883e652, 969e714, 394cd83, d70ee01, a8bdaa2)
- **D15** — SYM swaps the scrolling keys for ` / - = ^ \ | [ ] { } < > ~ with % in SHIFT's cell until tapped again, unsaved, with CTRL and ALT carrying over, and a key held while the page turns relies on the removed view's touch being cancelled (2c024c1, d70ee01)
- **D16** — Terminal Kotlin files read in execution order (types first, callbacks as they fire, each helper after its first caller, companion last) and vt.rs follows VtNative.kt's order, and reorders must keep initialised properties in their textual order, which is the order Kotlin initialises them in (a546fea, 3511da5)
- **D17** — Drawing and preference writes use androidx.core's KTX helpers (`edit { }`, `createBitmap`, `Canvas.withClip`, `withScale`) because lint's UseKtx warnings fail the build (android/native D10) (4c1a3ce)
- **D18** — The tab bar is one 32 dp row of numbered tabs (ended ones faded) followed by ×, + and ⋮, where a long press toasts the title, × asks while the tab's shell runs, closing the current tab shows the one after it (before it when last) and ⋮ offers Close other tabs and Close all tabs, with no tab names, switcher, swipe or renaming (a8bdaa2, 1eac62e, 7c35183)
- **D20** — A rounded 16 × 1 dp primary-colour bar 4 dp below the label's baseline, drawn by the containers in `dispatchDraw` through `ExtraKeysView.drawIndicator`, marks the current tab, locked modifiers and SYM, so a state change must invalidate the drawing container and fading a label fades its indicator too (d48dd10)
- **D22** — `revealCurrentTab` brings the current tab back into view `RETURN_TO_CURRENT_TAB_MS` (3 s) after the tab row rests, a touch on the row cancelling the wait and a release or finger-less scroll restarting it, while a tab change still scrolls at once (1118f81)
- **D23** — The tab bar less its buttons and, while it can scroll, the extra keys up to the last column are excluded from the left-edge back gesture, within Android's 200 dp per window edge, which the selection handles' rectangles (D34) share (ced9f06, 9f21a29)

## Active (full detail)

### D4: The terminal UI is a module of its own
- **What**: android/terminal holds `MainActivity`, `TabBarView`, the `Vt*` view and session files, `ConsolePty`, `ExtraKeysView`, `LicensesActivity`, `res/`, the terminal fonts and the vt.c/vt.rs shim; building libghostty-vt belongs to android/native.
- **Why**: §11.5 of the retired design doc needed a map, and the app was split into modules by responsibility rather than folding the UI into android/service.
- **Tradeoffs**: `ConsolePty` and `VtSession` are filed here although the service creates and owns them.
- **Watch out**: The map's open debts are no triple-tap or rectangular selection, libghostty-vt features left unwired or unused (the OSC 9;4, 7501, 7 and 133 callbacks, search, scrollback compression, snapshot restore, some reports and a terminal reset), and views that keep the sizes they were built with after a display size change until the activity is created again.
- **Source**: 12c4afa (file list since updated by 394aecf, 6ffb912, b38d29c, d965886; map fully refreshed in d9e1f0c)

### D7: Selection, paste, mouse and focus go through libghostty-vt
- **What**: Under the tab's lock, vt.c keeps the selection as the terminal's own (a long press selects a word as a tracked anchor, finger or handle drags extend it and autoscroll beyond the grid), pastes through `ghostty_terminal_paste` (asking with a preview when it returns REJECTED), and encodes mouse clicks and wheel, alternate-scroll arrows (mode 1007) and focus (mode 1004); frame and row headers carry the selection for `VtView`'s highlight and handles, keys add releases and lone modifiers for the Kitty protocol, and the IME turns a lone newline into Enter and a multi-line commit into a paste.
- **Why**: The prototype could only type keys and scroll: no copy or paste, no mouse for programs that track it, and unsafe multi-line pastes ran at once; libghostty-vt's selection tracks its cells through output, soft wraps and scrollback, which a Kotlin selection over the screen mirror could not.
- **Tradeoffs**: No triple-tap or rectangular selection; every selection change costs a full frame; typing ends a selection.
- **Watch out**: Grid references from the API are valid only until the terminal changes, so vt.c must take fresh ones per call; under the Kitty protocol a program that exits without popping its flags leaves the shell receiving CSI u keys, Ctrl-C included.
- **Source**: ee830a0; 62f6092 (mouse tracking and double taps replaced by D25); f77b591 (handles at the edges, D34)

### D8: libghostty-vt is the only terminal, and each tab is a `VtSession` owned by the service
- **What**: `TerminalService` owns one `VtSession` per tab, which keeps parsing output without a view (starting its shell once a view knows the grid, printing restored text and queued messages first, with pump, waiter and writer threads) and tells the service's kill-report routing through `consoleOpen` (started and not hung up) whether its console is still open, since `console` keeps its number after close (android/service D8); `MainActivity` has a single `VtView` that tabs attach to and closes an ended tab on Enter; the Termux libraries, `TermuxSession` and the `Session` interface are gone, `ConsolePty` keeps only start, waitFor and close, and the fonts are main assets.
- **Why**: The debug-only prototype screen could not be saved, restored, reached by kill reports or closed like a tab; once libghostty-vt tabs covered everything the Termux tabs did, keeping two emulators meant two code paths, a `Session` interface and GPL-3.0-only vendored code in the app.
- **Tradeoffs**: Every Android build needs Zig 0.17.0 and the fetched Ghostty source (android/native D6); triple-tap and rectangular selection are still missing; `VtView`'s focus highlight is off, so keyboard focus is not shown.
- **Watch out**: libghostty-vt has been tried only on a 4 KB-page emulator with Gboard in English; real devices, 16 KB pages and CJK input methods are unchecked.
- **Source**: 6ffb912, b38d29c; 62f6092 and f77b591 (drags, double taps and edge-aware handles since added, D25 and D34); d89b5fa (the bell since wired, D36); 6560af6 (`consoleOpen`, for a bug d9e1f0c recorded)

### D13: The app is Material 3 with dynamic colours, the terminal always dark
- **What**: `Theme.Appine` (Material 3 DayNight) serves Settings and the licences on shared page layouts and `Theme.Appine.Terminal` (Material 3 Dark, with a black window and system bars) serves `MainActivity`, both with DynamicColors; the terminal stays a framework `Activity` using Material dialogs and AppCompat's menus and list popup, and its hand-built tab bar and extra keys, black like the terminal, read the theme's colour roles through `MaterialColors`.
- **Why**: The user asked for Material 3, starting with Settings and then the terminal's dialogs, menus, list of tabs, tab bar and extra keys, and the Material classes need a Material theme on the activity.
- **Tradeoffs**: Views `MainActivity` inflates are the framework's, so AppCompat attributes such as `app:tint` do nothing there; the selection handles take the primary colour instead of cyan; Android 11 gets Material's baseline purple; and the bars have no Material elevation, their indicator (D20) being drawn by hand.
- **Watch out**: Turning `MainActivity` into an `AppCompatActivity` later changes its inflation, insets and key handling, colours are read once per activity, and DynamicColors, insets and the black system bars were checked only on an API 34 emulator.
- **Source**: 55be399, 97707b7, d16025a; 883e652, d48dd10; 0709982 (Android 10 dropped, android/native D3)

### D19: The bars' labels are sized in dp
- **What**: The tab bar's and extra keys' labels use dp constants beside the bars' fixed sizes (`BAR_LABEL_DP` 14 for the buttons and `TAB_NUMBER_DP` 12 for the numbers in `TabBarView`, `LABEL_DP` 12 in `ExtraKeysView`), while dialogs, menus and toasts follow the font scale.
- **Why**: The bars are fixed in dp, so labels in sp would clip at large font scales (Android 14's nonlinear 200% turns 12 sp into about 24 dp).
- **Tradeoffs**: Users who raise the system font get no larger labels on the two bars; dimens.xml waits until sizes need per-configuration overrides, when all the bars' sizes should move together.
- **Watch out**: Not checked at a large font scale, and accessibility scanners may flag text that ignores it.
- **Source**: 4aa2abf (constants moved to `TabBarView` in d965886)

### D21: Scrolling rows fade out into the fixed buttons at their end
- **What**: The tab bar and the extra keys are FrameLayouts whose scrolling row spans them with an end padding as wide as the fixed part and `clipToPadding` off, the fixed part sits over the end in black except a blank left strip (12 dp of ×, 6 dp of SHIFT and SYM, as InsetDrawables), and the row's right fading edge of `ExtraKeysView.FADE_DP` (32 dp) is carried that far in by a padding offset, with no left fade; the tab row scrolls the current tab clear of the fade.
- **Why**: Nothing showed that more tabs lay beyond × or, below 360 dp, more keys beyond the last column.
- **Tradeoffs**: Items under the fixed part are drawn only to be hidden, a tap on the blank strip where a fading item shows goes to the fixed button, and a manual scroll can stop with the last tab barely faded.
- **Watch out**: The strips fit the blank before × and SHIFT's S (about 0.7 dp) at the current label sizes in Roboto, so an OEM font or label change can put the fade over a glyph; `ExtraKeysView.scrolls` must compare the grid with the room between the paddings, since `canScrollHorizontally` counts the end padding as shown; checked only on a Pixel 4a API 34 emulator, not on real devices or Android 11.
- **Source**: a75981b, db4f809; 0709982 (Android 10 dropped, android/native D3)

### D24: Tab changes slide the tabs and the indicator, then roll the changed digits
- **What**: `TabBarView.render` keeps one `Tab` holder per session and changes the row in place; after a close, tabs whose place moved slide there with `translationX` (FLIP, 200 ms, Material standard easing) and closed tabs shrink to 0.6 and fade out in 150 ms in the row's overlay; the indicator slides from where it is drawn whenever the current tab's place on screen changes (a tap, Ctrl+Tab, a new tab, a close), in the same 200 ms, and is put there at once on the row's first layout; `TabNumber` then rolls only the digits that changed by 0.7 text sizes (down for a lower number, up for a higher), fading and blurring through a `BlurMaskFilter` of up to 0.12 text sizes over 300 ms, a number of another length rolling whole.
- **Why**: Closing a tab renumbered the later tabs at once, so the tab taking the closed current tab's place showed the same number under the same indicator and the switch went unnoticed; afterwards a switch jumped where a close slid, and the cross-faded renumbering read as a flicker, so the user asked for the slide on switches too and for SwiftUI's numeric-text roll.
- **Tradeoffs**: A close takes about 500 ms to settle; at the row's end the tabs before slide right rather than the next one arriving; numbers lag TalkBack's descriptions; the row keeps state across renders; a held Ctrl+Tab leaves the indicator a tab or two behind until 200 ms after release, and a switch to a tab out of view pairs the slide with the row's smooth scroll on another curve; `TabNumber` draws its digits itself, so TextView's own drawing (ellipsis, compound drawables) is lost to it.
- **Watch out**: `render` matches tabs by position, relying on sessions being added only at the end (reordering tabs would need matching by session); per-digit positions assume tabular digits (true for Roboto, unchecked for OEM fonts); the blur under hardware rendering was seen only on an API 34 emulator, and Close other tabs, a held Ctrl+Tab and TalkBack are untried.
- **Source**: afd086a, 1c6c1bc, 759d0b4

### D25: Mouse-tracking programs get drags, hover and real clicks; a double tap selects a word
- **What**: In mouse mode a long press selects the word and becomes a left-button drag once moved past the touch slop, mouse and touchpad presses bypass the gestures with their buttons and modifiers, hover goes as button-less motion, `VtView` detects double taps itself (selecting a word only without mouse tracking), and vt.c deduplicates motion per cell except in SGR-Pixels.
- **Why**: Programs tracking the mouse could not be dragged in or hovered over, double clicks in them lost the second click, and selecting a word needed a long press.
- **Tradeoffs**: A finger drag in a program needs a long press first, a still long press stays the terminal's selection so programs get no long click, there is no triple tap, and a double tap right after hiding the keyboard can select another row.
- **Watch out**: Touchpads reporting taps without `SOURCE_MOUSE` go through the gestures, mode 1003 hover while scrolled back returns the view to the bottom, and a press begun before tracking was turned on stays with the gestures.
- **Source**: 62f6092

### D26: The terminal avoids the cutout, and the extra keys follow the IME's animation
- **What**: The root is padded by the system bars, the display cutout and the IME and no longer clips to its padding; a `WindowInsetsAnimationCompat` callback (`DISPATCH_MODE_STOP`) keeps the system's end-state layout from the start and draws the extra keys from their old on-screen place to the new one along the IME animation's interpolated fraction, starting from where they are drawn when it interrupts another.
- **Why**: From Android 15 (targetSdk 37) the window reaches into the display cutout, which in landscape covered the terminal's first or last columns, and the extra keys jumped to their new place at the start of every IME animation.
- **Tradeoffs**: In landscape a black strip runs down the cutout's whole side; the terminal regrids as the animation starts, so its text moves before the keys arrive; the keys seem to trail Gboard as it fades out; children of the root may draw over the system bars' room.
- **Watch out**: Real devices, OEM cutouts and IMEs other than Gboard are untested, and since minSdk 30 the keys follow the platform's IME animation on every supported version (android/native D3); a view added to the root that draws past its bounds shows in the system bars' room, and another view meant to follow the IME needs its own translation.
- **Source**: cee87ba

### D27: The tab bar is a view of its own, `TabBarView`
- **What**: `TabBarView` (a FrameLayout with a `Target`, like `ExtraKeysView`) builds the bar and owns `Tab` and `TabNumber`, the slide, roll and indicator animations, the return to the current tab, the back-gesture exclusion and the bar's constants, setting its 32 dp height in `onMeasure` and its exclusion in `onLayout`; `MainActivity` calls `render(sessions, current)` and implements the `Target` as `Bar` (show, closeCurrent, newTab, menu, notice).
- **Why**: The tab bar, a self-contained widget, made up more than half of `MainActivity`, and the user asked to tidy it.
- **Tradeoffs**: One more file and an interface to keep in step; the current tab's index, from which its successor is taken, stays in the activity; blame on the bar's lines stops at the move unless followed across files.
- **Watch out**: The bar reads its colours when built, so it must be built after DynamicColors is applied in `onCreate`; the exclusion is now set on every layout, and the 3 s return to the current tab (D22) was not retried after the move.
- **Source**: d965886

### D28: The app is called Appine in its strings
- **What**: `app_name` and every sentence or title naming the app in `values` and `values-zh-rCN` say Appine, and the clipboard label of copied text reads `app_name`; the package, `libappine.so`, log tags, thread names and XTVERSION's "appine" keep the lowercase name.
- **Why**: The user renamed the app to Appine, and the UI strings named it in lowercase.
- **Tradeoffs**: The name is cased one way in identifiers and another in text.
- **Watch out**: New strings naming the app must say Appine, in both locales.
- **Source**: 3f04bf5 (with android/native D14, android/service D15, android/setup D16)

### D29: The ⋮ menu opens like a toolbar's overflow menu
- **What**: ⋮ opens a `PopupMenu` with `Gravity.END` in `Widget.Appine.MorePopup`, a child of `Widget.Material3.PopupMenu.Overflow` (overlapping its anchor, as `actionOverflowMenuStyle`), offset by -3.5 dp horizontally and 4 dp vertically.
- **Why**: The menu hung below the bar flush against the screen's edge, and the user wanted a toolbar overflow menu's placement with a small gap from the corner.
- **Tradeoffs**: While open the menu covers ×, + and ⋮ and the end of the tab row; the offsets are absolute dp tuned by eye for a left-to-right layout with ⋮ at the screen's edge.
- **Watch out**: Not checked on a real device, in landscape or at 720 dp and wider, where AppCompat shows a cascading popup; turning `supportsRtl` on needs the horizontal offset revisited.
- **Source**: 1ee3562

### D30: The terminal shows a fading scroll bar and flings like a ScrollView
- **What**: vt.c writes libghostty-vt's scroll bar state (total, offset, length) into the frame header, now 48 bytes, which `VtView` returns from `computeVerticalScroll*` for View's own fading scroll bar in the theme's thumb, awakened when the view moves the viewport but not on output; a fling runs an `OverScroller` in `computeScroll` through the same paths as a drag (the viewport to either end, or at most a screen of wheel events or arrow keys), and a touch, other input, focus loss or another tab stops it, the tap that stops one doing nothing else.
- **Why**: Nothing showed where the viewport was in up to 10,000 lines of scrollback, and a fling ended as soon as the finger lifted.
- **Tradeoffs**: The tap that stops a fling neither opens the keyboard nor clicks; the bar flashes whenever the window becomes visible and covers the last column while shown; Alpine's busybox less stays on the primary screen, so flinging in it scrolls the scrollback.
- **Watch out**: The frame format changed on both sides (vt.c and `VtView.apply`) and must keep changing together (D5); flings were tried only with adb swipes on an API 34 emulator, and flings sent as wheel events to a mouse-tracking program are untested.
- **Source**: c7e849d

### D31: Tapped links open in other apps, `file:` links as the provider's documents
- **What**: Without mouse tracking a tap on a link opens it instead of the IME, and a long press puts Open link first in the floating menu (in mouse mode too); vt.c returns the cell's OSC 8 URI, else an http(s) URL found in its line with soft wraps joined (ASCII URL characters, balanced brackets, trailing `.,;:!?` dropped); only http, https, mailto and file links open, the first three as browsable `ACTION_VIEW` intents and `file:` links as android/documents' documents (android/documents D9), with a toast when nothing can open one.
- **Why**: Tapping a URL did nothing, though programs print plain URLs and OSC 8 hyperlinks all the time.
- **Tradeoffs**: A tap on a link no longer brings up the keyboard; in mouse mode links open only from the long-press menu; a URL stops at its first non-ASCII character; other schemes (tel, ssh, deep links) are ignored; the long press still selects Ghostty's word, which stops at `:`.
- **Watch out**: Taps meant to focus the terminal can open a link; tried only with adb taps on an API 34 emulator with Chrome, HTML Viewer and the Files app, not on real devices or with mailto.
- **Source**: 1cc504b

### D32: libghostty-vt does not advertise Kitty graphics or the Glyph Protocol
- **What**: vt.c sets each terminal's Kitty image storage limit to 0 (the library then ignores the protocol and answers no queries) and turns the Glyph Protocol off; the other libghostty-vt features the terminal leaves unwired (the OSC 9;4, 7501, 7 and 133 callbacks, search, scrollback compression, snapshot restore and more) are listed in the map.
- **Why**: libghostty-vt answers both protocols' queries positively by default while `VtView` draws only cells, so programs that probe (yazi, chafa, timg, `kitten icat`) would send images or glyphs that never appear instead of falling back to text.
- **Tradeoffs**: No inline images until drawing them is implemented (a PNG decoder through `ghostty_sys_set`, the render state's image list and a frame format change); the library still carries both protocols' code.
- **Watch out**: A libghostty-vt upgrade could change what a limit of 0 means or reset it on some path (a full reset keeps it today because the alternate screen copies the primary's limit); not yet tried with real image viewers on a device.
- **Source**: fcf4c93, c510bee; d89b5fa (OSC 52 and cursor blinking since wired, D36)

### D33: A kill report first returns the parser to ground with CAN
- **What**: When `GHOSTTY_TERMINAL_DATA_VT_GROUND` says the parser is inside a sequence, `appine_vt_message` writes a CAN (0x18) before inserting the report as in D2.
- **Why**: A program killed inside an OSC, DCS or APC string left the parser there, so the report's line feeds were ignored or taken as payload and the room it makes above the prompt was lost.
- **Tradeoffs**: The unfinished sequence the program left is dropped rather than completed, and a death inside a UTF-8 sequence still leaves a U+FFFD.
- **Watch out**: The report's room depends on the parser being at ground (a map gotcha); the fix rests on a reading of libghostty-vt's parser and was not reproduced on a device.
- **Source**: fcf4c93, 3beb038, b43ff37

### D34: Selection handles flip at the screen's edges and are kept from gesture navigation
- **What**: `handle()` returns the drawable and bounds, using the other handle's drawable (hotspot at a quarter instead of three quarters) when the start handle would pass the left edge or the end handle the view's width, and `drawHandles` sets `systemGestureExclusionRects` to the drawn handles' bounds plus the touch slop whenever they change, and to none without a selection.
- **Why**: A selection from column 0 left the start handle three quarters off the left edge (the end handle likewise at the right), and a drag from the screen's edge was taken by gesture navigation as Back.
- **Tradeoffs**: A flipped start handle points the same way as an end handle, so at the edges the two look alike; handles below the last row still go under the extra keys (no vertical flip).
- **Watch out**: The rectangles count against Android's 200 dp per edge together with the tab bar's and extra keys' (D23), and are updated from `onDraw`, so a selection cleared without a redraw keeps its exclusion until the next frame.
- **Source**: f77b591

### D35: `VtNative` functions catch panics without throwing
- **What**: Every `VtNative` body in vt.rs runs in `guarded!` (android/native D15) without throwing, returning null, 0 or nothing on a panic.
- **Why**: They are JNI entry points too, called on the UI and pump threads.
- **Tradeoffs**: A panic in drawing or input code shows as a missing frame or an ignored key rather than a crash.
- **Watch out**: A panic inside the pump leaves that tab's output unread until it is reopened.
- **Source**: be72f31

### D36: Bells, OSC 52 clipboard writes, OSC 9/777 notifications and cursor blinking are wired
- **What**: vt.c records effects during `feed` (a bell count, the last clipboard write as text/plain up to 256 KiB or a clear, the last notification's title and body up to 4 KiB), the pump takes them after each read (`appine_vt_take_effects`) and calls `VtSession.onBell`, `onClipboard` and `onNotification` (routed by android/service D19), clipboard reads go unanswered, `VtView.bell` vibrates with the REJECT haptic at most every 100 ms, the cursor blinks every 500 ms only while the program asks for it and the view and window have focus, and `MainActivity` shows the tab a notification names (`EXTRA_TAB`) once bound.
- **Why**: Programs' bells, OSC 52 writes, OSC 9/777 notifications and blinking cursors were ignored though libghostty-vt reports them, so copying from tmux or neovim and noticing a CLI waiting for input in another tab were impossible.
- **Tradeoffs**: Several notifications in one read keep only the last; only text clipboard contents are supported, and a write over 256 KiB is denied rather than cut; bells and notifications pass through the main thread's queue.
- **Watch out**: The effects struct is shared by vt.c and vt.rs and must change on both sides; the vibration depends on the system's touch-feedback setting; real devices and Android 11 and 12 were not tried.
- **Source**: d89b5fa (with android/service D19)

### D37: ⋮ has a label, the menu key reaches programs and resize reports go out at once
- **What**: ⋮ is described by the `more_options` string ("More options", "更多选项"); `VtView.systemKey` no longer counts `KEYCODE_MENU` as a system key, so its presses and releases reach `VtKeys`; `VtNative.resize` takes the replies (the mode 2048 size report) under the tab's lock after `appine_vt_resize` and returns them for `VtSession.resize` to send through `write`, as a paste's bytes are.
- **Why**: ⋮ was the only tab bar button TalkBack could not name, a hardware keyboard's menu key never reached programs although `VtKeys` mapped it, and a resize's size report waited for the pump's next read.
- **Tradeoffs**: The menu key no longer reaches the activity, which has no options menu, and resize replies can reach the console out of order with replies the pump takes later, as a paste's can.
- **Watch out**: An IME or launcher sending `KEYCODE_MENU` now types `CSI 29 ~` into the program, and `VtNative.resize`'s signature changed in both vt.rs and VtNative.kt.
- **Source**: 64e5c33 (bugs recorded in the full refresh d9e1f0c)

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
