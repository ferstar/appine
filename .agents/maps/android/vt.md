# android/vt Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/vt.md` for the paired decision history (it still holds the pre-split decisions for `android/vtcore`; earlier decisions remain D2, D5, D6, D8, D16, D17, D32, D33, D35–D39 and D42–D49 in `.agents/decisions/android/terminal.md`).
>
> Verified: 2026-11-23 (full mapping: 3 research concerns; 9 claims checked by 2 independent verifiers — 9 confirmed; bugs and debts recorded)

## Responsibilities

`android/vt` owns the Kotlin session of each terminal tab and its kernel console wrapper:

- opening the console and spawning `login.sh` lazily on the first known grid, creating or restoring
  the native terminal, starting the `vt-writer`, `vt-pump` and `vt-waiter` threads, resizing,
  waiting for exit and closing the console exactly once;
- bridging native `vt-pump` callbacks onto the main thread (`Events`), queuing asynchronous OSC 52
  clipboard reads, and managing OSC 9;4 progress and OSC 7501 program-status records;
- staging restored snapshots/transcripts/notices, serving capped primary-screen transcripts and
  three-way native snapshots for persistence, and executing session-side message, cursor, clear
  and reset actions.

Root: `VtSession.kt` and `ConsolePty.kt` in
`android/app/src/main/java/io/github/mthli/appine/`.

Outside the module: `VtNative`, JNI, the pump implementation, terminal ownership and locks,
protocol parsing, command timing, snapshots, reset semantics and compression (`android/vtcore`);
view lifecycle, input and search (`android/vtview`); gestures, selection and scrolling
(`android/vtgestures`); frame encoding and drawing (`android/vtrender`); the surrounding screen
(`android/terminal`); session ownership, notifications and persistence policy (`android/service`,
`android/reports`); the kernel console (`kernel/tty`); kernel JNI (`android/native`); and the
login script and prompt-mark producers (`android/setup`).

## Key types

- `VtSession` — per-tab session owned by `TerminalService`: holds `id`, `ConsolePty`, `machine`,
  `Events`, `@Volatile tab` (`0L` until created; never reset on `release`), `@Volatile view` (read
  on `vt-pump`), `console` (`pty.console`), `consoleOpen` (`pty.console >= 0 && !hungUp`),
  `isRunning` (`exit == null`, true before start as well), `busy` (`isRunning && tab != 0L &&
  !VtNative.atPrompt`), restore staging (`restoredSnapshot`, `restoredText`, `pending`),
  `lastTranscript` (capped to `Tabs.MAX_TRANSCRIPT` = 256 K chars), worker-only `@Volatile` save
  metadata (`resolvedLogin`, `savedSnapshot`), cursor defaults (`cursorStyle`, `cursorBlink`),
  grid cache, main-thread `progress` (`Progress?`, 15 s `dropProgress` timer) and `status`
  (`Status?` selected from `LinkedHashMap<String, Status> records`, LRU-ordered, max 64),
  synchronized `reports` handoff buffer from `vt-pump`, `clipboardReads` queue, `AtomicBoolean
  hungUp`, and single-thread `vt-writer` executor.
- `VtSession.Events` — main-thread callback interface implemented by `TerminalService`:
  `onTitleChanged`, `onBell`, `onClipboard`, `onNotification`, `onClipboardRead`,
  `onStatusChanged`, `onCommandFinished`, `onTmuxDetached`, `onFinished`.
- `VtSession.Progress(state, percent)` & `VtSession.Status(id, state, kind, percent, title,
  message)` — value types for OSC 9;4 progress and OSC 7501 status; companion defines protocol
  constants (`PROGRESS_*`, `STATUS_*`, `KIND_*`) and localized `describe(context, status)`.
- `ConsolePty` — per-tab kernel PTY handle and login-shell launcher (`machine`, `script`, `login`,
  `cwd`, `@Volatile console` starting at `-1` and retained after `close`, `pid`). Companion holds
  `LOGIN_AUTO`, `LOGIN_ROOT`, cached `script(context)` (`login.sh`), `argv(script, login, cwd)`
  and `ENVIRONMENT`.

## Public entry points

- Lifecycle: `VtSession.start`, `resize`, `write`, `finishIfRunning`, `release`; `ConsolePty.start`,
  `waitFor`, `close`, `script`, `argv`, `ENVIRONMENT`, `LOGIN_AUTO`, `LOGIN_ROOT`.
- State and saving: `id`, `login`, `resolvedLogin`, `savedSnapshot`, `tab`, `view`, `console`,
  `consoleOpen`, `title`, `isRunning`, `busy`, `cwd`, `setRestored`, `transcriptText`,
  `takeSnapshot`, `showMessage`, `setCursor`, `clear`, `reset`.
- Program reports and native pump callbacks: `progress`, `status`, `statusSeen`, `describe`,
  `answerClipboardRead`, `Events`; JNI callbacks invoked on `vt-pump`: `onOutput`, `onTitle`,
  `onBell`, `onClipboard`, `onNotification`, `onClipboardRead`, `onProgress`, `onStatus`,
  `onForeground`, `onCommandFinished`, `onTmuxDetached`.

## Data flow / lifecycle

### Start, execution and teardown

- `TerminalService.createSession` builds `ConsolePty` and `VtSession` and calls `setCursor`;
  restored tabs also call `setRestored(text, snapshotLoader, notice)`. While unstarted (`tab == 0L`),
  `isRunning` is true, `busy` is false, `console` is `-1`, `transcriptText` returns `null` (so
  `saveTabs` preserves the original unshown tab record), and `showMessage` appends yellow ANSI
  lines to `pending`.
- `VtSession.start(cols, rows, cellWidth, cellHeight)` runs once on the main thread when a view
  first reports its grid (`Host.onGrid`):
  1. Calls `ConsolePty.start(cols, rows)` -> `Native.openConsole(machine, rows, cols)` and
     `Native.spawn(machine, console, "/bin/sh", argv, ENVIRONMENT)` (passing `HOME=/root`, `PATH`,
     `TERM=xterm-256color`, `LANG=C.UTF-8`, `SHELL=/bin/sh`, `USER=root`, `LOGNAME=root`,
     `BROWSER=/usr/local/bin/xdg-open`, `NMAP_UNPRIVILEGED=1`), catching `IOException` into `error`.
  2. `createTerminal` invokes and clears `restoredSnapshot` and `restoredText`: tries
     `VtNative.restore` if a snapshot exists (logging and falling back on `0L`), else calls
     `VtNative.create` and prints `restoredText` if present; JNI `IOException` yields `0L`.
  3. **Terminal-creation failure (`t == 0L`)**: calls `hangUp()` and `exited(127)` (silent — no
     completion line, no `view?.onStarted()`, no threads, and later `showMessage` returns `false`).
  4. **Terminal initialized (`t != 0L`)**: sets `tab = t`, applies `VtNative.setCursor` before
     printing and clearing `pending`, and caches grid dimensions.
  5. **Console/spawn failure (`error != null`)**: prints `"\r\n[$error]\r\n"`, calls `hangUp()`,
     sets `exit = 127` directly (no `"[Process completed ...]"` line, no `writer`), calls
     `view?.onStarted()` and `events.onFinished(this)`, and returns without starting threads.
  6. **Normal start**: starts `vt-writer` (single-thread `ExecutorService`), non-daemon `vt-pump`
     (`VtNative.pump(t, this)`), and non-daemon `vt-waiter` (`pty.waitFor()`, `pump.join(500)`,
     then posts `exited(code)` on `main`), and calls `view?.onStarted()`.
- `exited(code)` (main thread, idempotent on `exit != null`): sets `exit = code`, calls `hangUp()`,
  calls `writer?.shutdown()` (draining queued writes), cancels `dropProgress`, returns early if
  `released`, else clears `progress`/`records`/`status` (firing `onStatusChanged` if any existed),
  prints `"[Process completed (code N | signal N) - press Enter]"` (omitting reason for exit 0),
  invalidates `view`, and fires `events.onFinished(this)`.
- `release()` (idempotent on `released`): sets `released = true`, calls `finishIfRunning()` ->
  `hangUp()` (`AtomicBoolean.compareAndSet(false, true)` guarding `pty.close()` so a closed console
  number reused by the kernel can never be closed twice), calls `writer?.shutdown()`, cancels
  `dropProgress`, and calls `VtNative.destroy(tab)` (removing the handle from native `TABS` while
  any running `vt-pump` retains its `Arc<Tab>` until return).

### Pump callbacks, clipboard, progress and status

- `onOutput()` runs directly on `vt-pump` without checking `released` and calls
  `view?.postInvalidateOnAnimation()`. All other native callbacks post to `main`: `onProgress` and
  `applyReports` guard `!released && isRunning`; `onTitle`, `onBell`, `onClipboard`,
  `onNotification`, `onClipboardRead`, `onCommandFinished`, `onTmuxDetached` and `onForeground`
  guard `!released`.
- Clipboard reads (`OSC 52 ; <clipboard> ; ?`): `onClipboardRead(kind, count)` appends `count`
  entries to `clipboardReads` and notifies `events.onClipboardRead(this)` only when the queue was
  empty (coalescing concurrent reads into one user prompt). `answerClipboardRead(text)` formats
  one `ESC ] 52 ; <clipboard> ; <base64> ESC \` (ST, `Base64.NO_WRAP`; empty payload for `null` or
  `""` when denied/dismissed/background) per queued entry, clears `clipboardReads`, and sends via
  `write(...)` on `vt-writer`.
- Progress (`OSC 9;4`): `onProgress` maps `PROGRESS_REMOVE` to `null`, else `Progress(state,
  percent)`. `setProgress` cancels and re-posts the 15 s `dropProgress` timer (`PROGRESS_TIMEOUT_MS`)
  before checking `p == progress`, so identical heartbeats extend the timeout without firing
  `onStatusChanged`.
- Status (`OSC 7501`): `onStatus` strips Unicode direction controls (`U+202A..202E`,
  `U+2066..2069`, `U+200E`, `U+200F`, `U+061C`) from `title` and `message` (not `id`) on `vt-pump`,
  appends to `synchronized(reports)`, and posts `::applyReports` when `reports.size == 1`.
  `applyReports` drains `reports` on `main`: `STATUS_CLEAR` clears all records when `id` is empty
  or removes `id` and any `id + "/..."` descendants; other states replace `id` at the tail of
  `records` (`LinkedHashMap`), evicting the oldest entry past `MAX_STATUS_RECORDS` (64).
  `updateStatus` selects the most urgent record (`blocked > error > done > working > idle`, ties to
  most recently updated) and fires `events.onStatusChanged` on value change.
- Foreground clearing and seen: when `android/vtcore`'s pump sees the console's foreground process
  group change on a later read with no report, `onForeground` calls `setProgress(null)` and drops
  all status records except `STATUS_DONE` and `STATUS_ERROR` (firing `onStatusChanged` per cleared
  side). `statusSeen()` (called when the tab is in front) removes `STATUS_DONE` and `STATUS_ERROR`
  records; shell exit drops everything.
- `onCommandFinished(hasExit, exit, millis)` converts `exit.takeIf { hasExit }` and forwards to
  `Events`; `onTmuxDetached` forwards to `Events` (where `TerminalService` displays a notice).

### Saving, messages, actions and shell integration

- `transcriptText` (main thread): returns `null` when `tab == 0L`. Otherwise calls
  `VtNative.transcript(tab)`: a non-null result (primary screen changed) is truncated to its last
  `Tabs.MAX_TRANSCRIPT` (256 K) characters, cached in `lastTranscript` and returned; a `null`
  result (unchanged, alternate screen, or error) returns `lastTranscript.orEmpty()`.
- `takeSnapshot()` (service worker): returns `null` if `tab == 0L`, else forwards
  `VtNative.snapshot(t)`'s three-way result (`null` = unchanged -> keep `savedSnapshot`; empty
  `ByteArray` = failed / >16 MiB -> clear `savedSnapshot`; non-empty bytes -> write new snapshot).
  `resolvedLogin` caches `Rootfs.loginAccount(LOGIN_AUTO)` on the service worker on first save.
- `showMessage(text)` queues yellow ANSI lines in `pending` before `start()`, returns `false` if
  `started && tab == 0L`, or calls `VtNative.message` + `view?.invalidate()` (refused on alternate
  screen). `setCursor` updates defaults and applies via `VtNative.setCursor` + `view?.invalidate()`
  if started.
- `resize(cols, rows, cellWidth, cellHeight)` skips unchanged dimensions or `tab == 0L`, calls
  `VtNative.resize` (reflow + console `SIGWINCH`), and immediately passes any non-empty replies
  (e.g. mode 2048 size reports) to `write(replies)`. `write(data)` queues `Native.write` on
  `vt-writer` while `writer != null && isRunning` (swallowing `RejectedExecutionException`).
- `clear()` calls `VtNative.clear(tab)`: `-1` (alt screen / gone) is a no-op; `0` invalidates
  `view`; `1` (at prompt) writes `^L` (`0x0c`) and invalidates. `reset()` calls `VtNative.reset`,
  writes `^L` if at prompt, invalidates `view`, and emits `events.onTitleChanged(this)` — leaving
  `progress`, `records` and `status` intact.
- `busy` is `isRunning && tab != 0L && !VtNative.atPrompt(tab)` (unmarked shells stay busy while
  running). `cwd()` calls `Native.consoleCwd(machine, pty.console)` only when `isRunning &&
  consoleOpen` (taking the kernel BKL on the caller's thread).

## Dependencies (inbound / outbound)

### Inbound

- `android/service` (`TerminalService`, `Notifications`): constructs/restores/saves/releases
  sessions, implements `Events`, reads `isRunning`/`transcriptText`/`console`/`consoleOpen`/`tab`/
  `id`/`title`/`progress`/`status`, mutates `resolvedLogin`/`savedSnapshot`, calls `setCursor`,
  `setRestored`, `showMessage`, `takeSnapshot`, `answerClipboardRead`, `statusSeen` and `describe`.
- `android/terminal` (`MainActivity`): calls `start`, `cwd`, `clear`, `reset`, `write`,
  `answerClipboardRead`; reads `id`, `tab`, `busy`, `isRunning`, `progress`, `status`.
- `android/tabbar` (`TabBarView`, `TabListView`, `TabMarks`, `ProgressLine`): reads `id`, `title`,
  `isRunning`, `progress`, `status`, `describe` and protocol constants.
- `android/vtview` (`VtView`): sets `view`, reads `tab`, calls `resize`.
- `android/vtcore` (`vt.rs`): invokes the 11 `on*` callbacks on `vt-pump`.
- `android/setup` / `android/settings` (`Rootfs`, `Settings`, `SettingsActivity`): reuse
  `ConsolePty.LOGIN_AUTO`, `LOGIN_ROOT`, `script`, `argv` and `ENVIRONMENT`.

### Outbound

- `android/vtcore` (`VtNative`): `create`, `restore`, `setCursor`, `print`, `pump`, `destroy`,
  `title`, `atPrompt`, `transcript`, `snapshot`, `message`, `resize`, `clear`, `reset`.
- `android/native` (`Native`): `openConsole`, `spawn`, `write`, `closeConsole`, `waitFor`,
  `consoleCwd`.
- `android/service`: `VtSession.Events` and `Tabs.MAX_TRANSCRIPT`.
- `android/vtview`: `VtView` (`onStarted`, `invalidate`, `postInvalidateOnAnimation`).
- `android/setup`: `assets/login.sh`; Android framework (`Handler`, `Base64`, `R.string.status_*`).

## Invariants and gotchas

- `ConsolePty.start(columns, rows)` swaps argument order to `Native.openConsole(machine, rows,
  columns)`.
- `ConsolePty.console` retains its integer after `close()` (for kill-report routing to ended tabs);
  active-ownership checks must use `consoleOpen` (`pty.console >= 0 && !hungUp.get()`), and
  `hangUp()`'s `AtomicBoolean` guarantees `closeConsole` runs at most once so a reused console
  number is never closed for another tab.
- `VtSession.tab` is never zeroed on `release()`; subsequent `VtNative.*` calls rely on `vt.rs`'s
  `TABS` lookup returning `None`.
- `openConsole`, `spawn`, `closeConsole`, `VtNative.resize` and `VtSession.cwd()` take the kernel
  BKL on the caller's thread (main thread unless called on the service worker); per-tab `vt-writer`
  isolates blocked PTY writes.
- `ConsolePty.ENVIRONMENT` sets `BROWSER=/usr/local/bin/xdg-open` without VT transport state.
- `VtNative.create` and `VtNative.restore` omit `@Throws(IOException::class)` in Kotlin though JNI
  throws `IOException` when the machine is stopped, caught by `VtSession.start`.
- If `openConsole` throws, `pty.console` remains `-1` and is passed to `createTerminal` (cast to
  `u32::MAX` in `vt.rs`), which is harmless only because `error != null` prevents `vt-pump` from
  starting and `pty.close()` guards `console >= 0`.
- Re-entrant `statusSeen()`: when a `STATUS_DONE`/`STATUS_ERROR` report arrives for the foreground
  tab, `updateStatus()` -> `onStatusChanged` -> `Notifications.status` calls `statusSeen()`
  synchronously and re-enters `updateStatus()` (terminating on the second pass).
- A program's RIS emits `PROGRESS_REMOVE` and `STATUS_CLEAR(id="")` via libghostty-vt callbacks,
  whereas `VtSession.reset()` leaves `progress`/`records`/`status` untouched.
- Same-read OSC 52 `?` requests collapse to the last clipboard target's `kind` in `vt.c`; separate
  reads preserve individual targets in `clipboardReads`. Clipboard answers must always go through
  `answerClipboardRead` -> `vt-writer`, never synchronously under native terminal locks.
- Release sessions and close consoles before stopping the kernel so `vt-pump` can return; native
  pumps must not hold a session-long `Arc<Machine>`.

## Confirmed bugs / technical debt

- **Foreground-group status tracking outlives exited or background commands:** progress and status
  records clear on foreground process-group changes rather than OSC 133 prompt marks (even though
  `android/vtcore` installs the OSC 133 callback for command timing), so reports from an
  already-exited command or a background job can outlive their command.
- **Graceful `vt-writer` shutdown races console-number reuse:** `exited()` and `release()` call
  `writer?.shutdown()` rather than cancelling queued tasks after `hangUp()` closes the console;
  queued write runnables capture `pty.console` at submit time without rechecking `hungUp`, so a
  write draining after close can inject bytes into a newly opened tab if the kernel immediately
  reuses that console number.
- **`VtSession.reset()` leaves progress, status and `dropProgress` active:** unlike a program's
  RIS (which emits `PROGRESS_REMOVE` and `STATUS_CLEAR`), the menu's Reset terminal action leaves
  stale progress/status records and any active 15 s `dropProgress` timer in place.
- **Shell mark coverage debt inherited from `android/vtcore`:** `busy` and command-completion
  events inherit `android/vtcore`'s prompt-marking and ash/bash parsing limitations.

## Open questions

- Should a background tab's progress and status clear at the next OSC 133 prompt mark instead of on
  foreground process-group changes?
- Should a status notification name the record's `app` when it has no title rather than the tab's
  title?

## To verify

- Session behavior on physical and 16 KB-page devices: progress/status from real programs (`cargo`,
  `systemd`, AI coding CLIs), clipboard copy toast on Android 12 and earlier, and end-to-end
  restore with Kitty images, bash prompt marks, alternate-screen saves, or across a libghostty-vt
  version change.
