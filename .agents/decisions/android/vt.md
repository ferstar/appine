# Terminal sessions (android/vt) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/vt"`
>
> Last distilled: 2026-11-11 (HEAD = c1a37da)
>
> Decisions on the tabs' sessions made before android/vt was split from android/terminal keep their `android/terminal` tags and live in `.agents/decisions/android/terminal.md`: D2, D5, D6, D8, D16, D17, D32, D33, D35–D39 and D42–D49.

## Active

### D1: android/vt is split out of android/terminal
- **What**: android/vt owns `VtSession.kt`, `ConsolePty.kt`, `VtNative.kt`, `vt.rs` and `vt.c` except its Frames, Input, Paste, Mouse and focus, Selection, Links, Taps on the prompt and Search sections: start and end, effects, tmux control mode, messages and transcripts, scrollback compression, snapshots and restoring, clearing and resetting, and shell integration (marks, timing, at-prompt).
- **Why**: The sessions, consoles and libghostty-vt shim change separately from the view, and the terminal map (133 KB) could not be read in one Read.
- **Tradeoffs**: `vt.c` is shared with android/vtview by section, so the file's header names both maps and a new section has to be assigned to one of them.
- **Watch out**: A `vt.c` section added without saying which map owns it.
- **Source**: d42013d

### D2: A tab keeps only the part of its transcript a save keeps
- **What**: `transcriptText` takes the last `Tabs.MAX_TRANSCRIPT` characters, keeps them and returns them, so a kept and a fresh transcript of the same screen are the same string, and `Tabs` makes the constant `internal` for it while still cutting what it saves.
- **Why**: The transcript kept for saves while the screen is unchanged or on the alternate screen held the whole scrollback's text, of which saving keeps only the last 256 K characters.
- **Tradeoffs**: The formatter and JNI still make the whole text once per save before it is cut, and `VtSession` now depends on a constant of android/service's `Tabs`.
- **Watch out**: A cut can start inside a surrogate pair or a line, as `Tabs`'s own cut already could.
- **Source**: ca5f5c2

### D3: Background tabs' Kitty images are cut by lowering the storage limit
- **What**: `appine_vt_drop_images` sets `GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT` to `KITTY_TRIMMED` (4 MiB) and back to `KITTY_STORAGE` (32 MiB) on both screens, called by `VtNative.dropImages(tab)` under the tab's lock (`Tab::term`) when the service trims under memory pressure (android/service D4), leaving the transcript and the snapshot state alone.
- **Why**: A tab's Kitty images (up to 32 MiB a screen) could not be given back, and libghostty-vt's C API can neither compress, drop nor count them; only lowering its storage limit evicts.
- **Tradeoffs**: Images are deleted outright with their placements, shown ones included, a program placing an image again by id finds it gone, and a restore from the snapshot saved before the cut brings the deleted images back.
- **Watch out**: A libghostty-vt upgrade could change how lowering the limit evicts or what a limit change marks as mutated, and the view's frames rely on the placements generation changing.
- **Source**: c1a37da
