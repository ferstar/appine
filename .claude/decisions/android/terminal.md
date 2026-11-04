# Terminal UI (android/terminal) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/terminal"`
>
> Last distilled: 2026-11-04 (HEAD = b38d29c). D1–D3 were seeded by hand from the retired design
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
- **What**: Each tab has one libghostty-vt 1.4.0 terminal in the native library, fed straight from the kernel console by a pump thread that writes the terminal's replies back and asks for one frame of dirty rows per burst; `VtView` mirrors the frames onto an integer cell grid in JetBrains Mono (glyphs centred or shrunk evenly, never stretched; box drawing, blocks and Powerline triangles as shapes; ligatures off), keys go through libghostty-vt's encoder, grapheme clustering (mode 2027) is on and synchronized output holds the screen for at most 1 s.
- **Why**: The Termux view flattened CJK (horizontal-only scaling to the cell), broke box-drawing lines between rows, showed Powerline symbols as boxes and split emoji sequences; Ghostty's full renderer targets macOS and GTK, while libghostty-vt is a plain C library for the terminal state.
- **Tradeoffs**: CJK glyphs still come from the system fallback fonts; programs that count widths per codepoint disagree with mode 2027 on emoji sequences.
- **Watch out**: The frame format (vt.c and `VtView`) and the key numbers (`VtKeys`, from Ghostty 1.4.0's header) must change together with their counterparts.
- **Source**: 394aecf

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

## Superseded

- ~~D1: Termux's terminal libraries are vendored, with `TerminalSession` rewritten around a PTY interface~~ → replaced by **D8** in b38d29c (2026-11-04)
- ~~A `Session` interface over Termux and libghostty-vt tabs, the kind chosen by `VtNative.available` (6ffb912)~~ → replaced by **D8** in b38d29c (2026-11-04)
- ~~The libghostty-vt terminal as a debug-only `VtActivity` next to the Termux tabs (394aecf)~~ → replaced by **D8** in 6ffb912 (2026-11-04)
