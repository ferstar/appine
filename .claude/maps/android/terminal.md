# android/terminal Map
> Static understanding snapshot, not a decision history.
> See `.claude/decisions/android/terminal.md` for the paired decision history.
> Verified: 2026-11-03 (full mapping: 1 research concern; 6 claims checked by 2 independent verifiers — 5 confirmed and 1 partial; bugs and debts recorded)

## Responsibilities

`android/terminal` is the terminal screen and the pieces that render the service's sessions:

- `MainActivity`: the tab bar, the terminal view, font zoom, extra keys, insets and the IME;
- `ConsolePty`: the adapter between a Termux `TerminalSession` and a kernel console
  (`/dev/pts/N`);
- the vendored Termux `terminal-emulator` and `terminal-view`, with appine's changes to
  `TerminalSession`;
- `LicensesActivity`: the notices generated at build time;
- the app's strings and themes.

Root: `android/app/src/main/java/io/github/mthli/appine/MainActivity.kt`, `ConsolePty.kt`,
`ExtraKeysView.kt`, `LicensesActivity.kt`, `android/terminal-emulator/`, `android/terminal-view/`,
`android/app/src/main/res/`.

Outside the module: session lifecycle, saving and kill routing (`android/service`); the host console
in the kernel (`kernel/tty`); the notices task and JNI (`android/native`).

## Key types

- `MainActivity` — a `TerminalService.Listener`; `ViewClient : TerminalViewClient`.
- `ExtraKeysView` — one row: ESC, TAB, CTRL, ALT, `-`, `/`, `|`, arrows.
- `ConsolePty : TerminalSession.Pty` — `start` (open console, spawn `login.sh`), `read`, `write`,
  `resize`, `close`; `console` is −1 until started.
- `TerminalSession` (vendored, rewritten) — takes a `Pty` instead of creating a subprocess; adds
  `setRestored`, `showMessage`, `getTranscriptText`.
- `LicensesActivity` — list from `assets/notices/index.tsv`, full text view.

## Public entry points

- Launcher activity `MainActivity` (`singleTask`, `adjustResize`, handles most configuration
  changes itself); listener callbacks `onMachineReady`, `onProgress`, `onSessionsChanged`,
  `onTextChanged`, `onExitAll`, `onRestarting`, `onNotice`.
- `LicensesActivity`, opened from Settings.

## Data flow / lifecycle

**1. Binding and tabs**

- `onStart` starts and binds the service; `onServiceConnected` registers the listener and replays
  ready/error/progress and pending notices; `onStop` unregisters and unbinds.
- Tab bar: `N: title` (`sh` by default, ` ✕` once the shell ended); tap switches, long-press closes
  at once (hang-up), `+` opens a session while the kernel runs, `⋮` has Settings. Closing the last
  tab while the kernel runs finishes the activity (the service stays). After a reconnect the focused
  tab is the service's `focus` or the last one.

**2. A tab's console**

- When the view first knows its size, `initializeEmulator` calls `Pty.start`: open a console with
  that size and spawn `/bin/sh -c <login.sh> appine-login <login> <cwd>` with a fixed environment.
  Restored transcripts (`setRestored`) are written before the shell starts.
- `ConsolePty` uses a reader thread (`Native.read`, end at ≤ 0), a writer thread and a waiter
  thread (`waitFor`, then up to 0.5 s for the reader to drain, since background jobs may keep the
  slave open); `resize` runs on the main thread. After exit the session shows "[Process completed
  … - press Enter]" and Enter (key event) closes the tab.
- `showMessage` (kill reports) inserts a yellow line above the cursor line: newline and back,
  save cursor, insert lines, write, restore cursor and move down; skipped on the alternate screen
  and queued until the emulator exists.

**3. Input and display**

- Pinch zoom changes the font by 1 dp steps beyond a 0.9–1.1 scale band, within 6–32 dp, stored as
  pixels in the activity's preferences.
- Extra keys: CTRL and ALT are one-shot toggles consumed when the next key is read; special keys go
  through the view's key-code path (cursor application mode applies), characters through code-point
  input.
- A tap opens the IME; the content is edge-to-edge with the root padded by system-bar and IME
  insets; long-press selects text; copy and paste go through the service.

**4. Licenses**

- The build writes `notices/` (appine's licence, Termux and jackpal, Alpine packages from the
  rootfs database, the doas package, Android libraries, Rust crates, SQLite, then full texts);
  full text is monospace, selectable and sized to fit 74 columns.

## Dependencies (inbound / outbound)

**Inbound**

- `TerminalService` (sessions, focus, listener callbacks, `showMessage`, `setRestored`,
  `getTranscriptText`); `SettingsActivity` (licenses).

**Outbound**

- `Native` (`openConsole`, `spawn`, `read`, `write`, `resize`, `closeConsole`, `waitFor`), hence
  the kernel's host console; `login.sh`; `assets/notices`.

## Invariants and gotchas

- The console opens lazily, so restored tabs that were never shown have no console and no shell.
- `openConsole`, `spawn`, `resize` and `closeConsole` run on the main thread and take the kernel's
  BKL (the `Pty` javadoc wrongly says everything but `start` runs on background threads).
- Typing blocks the main thread once Termux's 4 KB input queue is full (inherited).
- Console numbers are freed at close and reused lowest-first by the kernel, and a close carries no
  generation, so closing a stale number closes whichever console now has it.

## Confirmed bugs / technical debt

- **Closing a tab can hang up another tab.** A running tab's console is closed by `finishIfRunning`
  and again by `cleanupResources` once the old shell has exited (plus up to 0.5 s; unbounded if it
  ignores SIGHUP); a tab opened or first shown in between (a restored tab revealed by the close, or
  "+") can get the same number and is hung up by the second close.
- **Special extra keys crash without a session.** The extra-keys row stays tappable during startup,
  progress and a failed first start; ESC, TAB and the arrows reach `TerminalView.handleKeyCode`,
  which dereferences the null session (character keys are guarded).
- **A console leaks when spawning the shell fails.** `Pty.start` opens the console first; if `spawn`
  throws the session is marked finished without closing it, and nothing closes it until the kernel
  stops.
- **Tab saves during full-screen programs lose the scrollback.** `getTranscriptText` reads the
  current screen, which is the alternate buffer (no scrollback) while vim or less runs, and replaces
  the earlier save.
- **Enter from the IME does not close a finished tab.** Only a `KEYCODE_ENTER` key event closes it;
  keyboards that commit `\n` as text go to the dead session and are dropped.

## Open questions

- Should the extra keys be disabled until a session is attached?
- Should saved transcripts come from the main buffer rather than the alternate screen?

## To verify

- Which IMEs on real devices deliver Enter as text; edge-to-edge insets on display-cutout devices
  (the root ignores cutout insets).
