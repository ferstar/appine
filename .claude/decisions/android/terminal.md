# Terminal UI (android/terminal) Decisions
> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/terminal"`
> Last distilled: 2026-11-03 (HEAD = c3df5d3) — seeded by hand from the retired design doc §11.5 and
> the round records in `docs/rounds/`; no `MODULE: android/terminal` Decision blocks exist yet, so
> sources cite rounds instead of commits.

## Active

### D1: Termux's terminal libraries are vendored, with `TerminalSession` rewritten around a PTY interface
- **What**: `terminal-emulator` and `terminal-view` are vendored from Termux; JNI and native code are removed; `TerminalSession` talks to a `Pty` (`ConsolePty`, the kernel's in-process console) with the same thread structure; the extra-keys row is appine's own.
- **Why**: Upstream `TerminalSession` is a final class that hard-codes `JNI.createSubprocess`; the extra-keys bar belongs to Termux's GPLv3 main app module, so it was rewritten rather than copied.
- **Tradeoffs**: Upstream fixes must be merged by hand; the vendored code carries GPLv3, so the app as a whole is distributed under GPLv3-compatible terms (appine itself is AGPL-3.0-or-later).
- **Watch out**: Source commit and changes are recorded in `android/README.md`; keep it current.
- **Source**: rounds 07 (`07-m5`), 192 (`192-license`); design doc §11.5

### D2: Messages from appine are inserted above the cursor line
- **What**: `TerminalSession.showMessage` scrolls if needed, saves the cursor, inserts lines above the current one, writes the yellow message and restores the cursor below it; it is skipped on the alternate screen.
- **Why**: Kill reports arrive after the shell has already printed its prompt; appending put the message after the prompt and left the cursor on an empty line, which looked like a lost prompt and desynchronised line editing.
- **Tradeoffs**: The shell does not know the screen changed (its cursor and line still match); the guest's saved-cursor slot is overwritten.
- **Source**: round 195 (`195-kill-reports`)

### D3: Licence notices are generated at build time and shown in-app
- **What**: The build writes `notices/` (appine, Termux and jackpal, Alpine packages from the bundled rootfs, Android libraries, Rust crates, SQLite, full licence texts); Settings shows the AGPL statement and opens `LicensesActivity`.
- **Why**: AGPL sections 0 and 5 require the interface to show copyright, the lack of warranty, the licence and where to read it; generating from the actual inputs keeps the list complete.
- **Tradeoffs**: The licence pages are English only.
- **Source**: round 192 (`192-license`)
