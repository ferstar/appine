# android/service Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/android/service.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 2 research concerns; 10 claims checked by 1 independent verifier — 8 confirmed and 2 partial; bugs and debts recorded)
>
> Maintained: 2026-11-08 (targeted verification only since the full mapping: sessions on libghostty-vt; robust start, exit and tab saving; tab names removed; kill reports; the app named Appine; resumed imports; auto tabs saved as their account; rootless containers file last)

## Responsibilities

`android/service` is the app's foreground service, `TerminalService`, which owns everything that
outlives an Activity:

- the kernel handle (`machine`), starting, booting (inittab), shutting down, rebooting and
  replacing the kernel;
- all terminal sessions (`VtSession`s, `android/terminal`), and restoring tabs after the process
  was killed (`Tabs`);
- the ongoing notification and its actions, the wake lock, memory trimming;
- a 2 s poll that fetches kernel events (listens, kills, power requests, process count, changed
  directories) and turns them into tab messages, notifications and lifecycle actions;
- the settings state that the activities only display and forward (tier in debug builds,
  keep-awake, login account, system documents, CJK characters filling two cells, distro switch,
  mirror).

Root: `android/app/src/main/java/io/github/mthli/appine/TerminalService.kt`, `Tabs.kt`.

Outside the module: rootfs import, downloads, mirrors and `login.sh` (`android/setup`); rendering,
the tab bar and the PTY adapter (`android/terminal`); the Files app provider
(`android/documents`); JNI and the kernel API (`android/native`, `kernel/process`).

## Key types

- `TerminalService` — a same-process service; `LocalBinder` hands activities a direct reference.
- `Listener` — UI callbacks with no-op defaults, always on the main thread: `onMachineReady(error)`,
  `onProgress`, `onSessionsChanged`, `onExitAll`, `onRestarting`, `onSettingsChanged`, `onNotice`
  (sessions redraw their view themselves).
  Activities register in `onServiceConnected` and leave in `onStop`; "UI visible" means at least
  one listener (Settings included).
- `worker` — a single-thread executor (`appine-worker`) that serialises native calls taking the
  BKL: poll, shutdown, stop, trim, routes, `consoleCwd`.
- Session state (internal): `sessions`, `unshown` (restored tabs not yet displayed), `restore`
  (saved tabs from a killed run), `ended` (why that run ended), `focus`, `current`;
  `sessionEvents` (title changed, finished).
- Lifecycle state (internal): `exiting`, `starting` (a start thread is running), `transitioning`
  (restart or replace between letting go of the old kernel and starting the new one),
  `pendingReplace` (a distro download that finished during a restart).
- `Switch` — a distro download in progress (volatile `done`, `total`, `cancelled`).
- `Tabs` — the `files/tabs/` store: `running` marker (the pid of the run that left it),
  `index.json`, transcripts named by content (`t-<128-bit SHA-256 prefix>.txt`; earlier versions
  wrote `t<i>.txt`, still read); `Tab(login, cwd, transcript)`, `Saved(tabs, current, time)`,
  `Killed(pid, saved)`.
- Constants: poll 2 s, trim delay 60 s, periodic save 60 s, save-soon 2 s.

## Public entry points

- Lifecycle: `onCreate`, `onStartCommand` (EXIT_ALL, CANCEL_SWITCH, TOGGLE_KEEP_AWAKE, COPY_REPORT;
  `START_NOT_STICKY`), `onTrimMemory`, `onDestroy`.
- Called by activities: `addListener`/`removeListener`, `createSession`/`removeSession`, `exitAll`,
  `setTier` (ignored in a release build), `setKeepAwake`, `switchTo`/`cancelSwitch`/`canSwitch`,
  `setMirror`/`autoMirror`, `setLogin`, `setSystemDocuments`, `setFillWide`, `takeNotice`.
- Static settings read without the service: `login()` and `showSystemDocuments()` by `Documents`,
  `fillWide()` by `MainActivity` (whenever it starts).

## Data flow / lifecycle

### Start and boot

- `onCreate` starts foreground (type `specialUse` on API 34+), takes a killed run
  (`Tabs.takeKilled`): its saved tabs, and how its process ended, looked up by the marker's pid in
  Android's exit records (`ApplicationExitInfo`); a crash or ANR of Appine posts a notification at
  once. Then it calls `start(Rootfs.importing())` when no rootfs is ready (Alpine on the first
  run, else the distro an import was last started for), else `start(null)`.
- `start` sets `starting` (the wake lock follows) and runs on an `appine-start` thread: import when
  a distro is given (`Rootfs.prepare`, then a mirror-probing thread), `ensureNetworkConfig`,
  `ensureContainersConfig`, `ensureLoginTools`, `updateProfile`, then `Native.start(store, tier)` under `Rootfs.access`'s
  write lock (`tier` is the setting in a debug build and always 1 in a release build, whatever an
  earlier debug install recorded). Back on the main thread it clears `starting`; if "exit all"
  came meanwhile it stops the new kernel (`Native.stop`, not booted) and finishes the exit. On
  success: set `machine`, refresh Files roots; a `pendingReplace` replaces the store at once;
  otherwise update the wake lock, mark running, restore saved tabs once, schedule periodic saves,
  start the boot thread, register a default-network callback (routes), post the poll,
  `onMachineReady(null)`.
- Boot (`appine-boot` thread): run the osclock script without a console, `Native.boot` (blocks
  until inittab's boot entries finish), then notify OpenRC services that failed to start.
- A failed import of a downloaded distro falls back to Alpine with a notice; with a
  `pendingReplace` a failure replaces the store; any other failure sets `startError`, releases the
  wake lock, shows "Cannot start: …" in the notification, reports it, and stops the service as soon
  as no UI is left (there is no kernel, so no poll would end it; opening the app tries again).

### Poll (every 2 s, on the worker)

- Fetches `takeListens`, `takeKills`, `takePower`, `processes`, `takeChangedDirs`; results are
  applied on the main thread only if the kernel is still the same.
- Reboot → `restart()`; halt/poweroff → `exitAll()`; no running sessions (tabs whose shell ended
  do not count; restored tabs not shown yet do), no listeners and no guest processes →
  `exitAll()`. Changed directories go to `Documents.dirsChanged`.

### Stopping, restarting, replacing

- `exitAll`: mark exiting, cancel a switch and a `pendingReplace`, `onExitAll`, clear listeners
  immediately, `detach`, show "stopping services". While `starting` or `transitioning` it stops
  there and the start, restart or replace finishes the exit once its kernel is up or has failed;
  otherwise `finishExit`: `Native.shutdown` on the worker (inittab shutdown, 30 s; `Native.stop`
  for a kernel that was never booted), then release the wake lock and clear saved tabs unless tabs
  of a killed run are still waiting to be restored; restart the kernel if a listener came back
  meanwhile, else `stopSelf`.
- `detach`: unregister the network callback, clear `machine` (before sessions end, so the UI does
  not treat it as the last tab closing), update the wake lock, cancel poll and saves, release
  (hang up and free) and clear all sessions; it runs before the kernel is shut down or stopped.
- `restart` (guest reboot, tier change in a debug build): `transitioning`, `detach`,
  `onRestarting`, shutdown on the worker, then `start(null)` (or, after "exit all" meanwhile,
  finish the exit); the UI opens a new tab when the new kernel is ready and no sessions exist.
- Distro switch/reset: download in the background (progress in Settings and the notification, both
  cancellable); when it finishes, `replace` if a kernel runs or the start failed, or record it in
  `pendingReplace` while a restart is under way. `replace`: reset the login setting,
  `transitioning`, `detach`, `Native.stop` (kill all, no inittab shutdown), join the mirror thread,
  delete the store, `start(distro)` — also after "exit all" meanwhile, so the chosen distro is
  imported (and then stopped) rather than the next start importing Alpine.
- `onDestroy` with a kernel still present shuts it down on the worker, best effort, and clears
  saved tabs unless tabs of a killed run are still waiting to be restored.

### Wake lock and memory

- `PARTIAL_WAKE_LOCK` held when keep-awake is on and the kernel is starting (import included),
  running, restarting, being replaced or shutting down; not reference-counted, no timeout; the
  notification shows a toggle and a "keep awake" suffix.
- The last listener leaving starts a 60 s timer that trims memory (`Native.trimMemory` on the
  worker) and saves tabs at once; a returning listener cancels it. `onTrimMemory` at
  `RUNNING_LOW` and above (except `UI_HIDDEN`) trims and saves immediately.

### Sessions, saving and restore

- `createSession(login, cwd)` builds a `ConsolePty` and a `VtSession`; the shell starts when a
  view first knows its size. Sessions report title changes and ends, which become
  `onSessionsChanged`. `removeSession` releases the session.
- Saving (`saveTabs`): for running sessions only, the login setting and the transcript
  (plain text, last 256 K chars; `VtSession.transcriptText`) on the main thread and the foreground
  group's cwd (`consoleCwd` of the session's console) on the worker, where an `auto` login is saved
  as the account it resolves to (`Rootfs.loginAccount`, kept in `VtSession.resolvedLogin` from the
  tab's first save) so a restored tab logs in as the same user; unshown restored tabs keep
  their saved transcript and cwd; a `name` key in an index from versions that could rename tabs
  is ignored. Files are written temp-then-rename; a transcript's name is its
  content digest, so unchanged ones are not rewritten, the index is skipped when unchanged, and
  transcripts no index names any more are deleted only after the index is replaced (a kill at any
  point leaves a consistent set). Triggers: every 60 s, 2 s after a tab opens, closes or its shell
  ends, when the last listener leaves, on memory trim.
- The `running` marker, holding the process's pid, is written after each successful start and
  removed (with saved tabs) on a normal exit; `takeKilled` loads the tabs when it is present at
  service creation and leaves the marker and files alone, so a second kill (or a failed start)
  before the kernel is up keeps them, and the marker still names the run they belong to. A marker
  from an earlier version is empty (pid 0).
- Restore: each saved tab reopens with its saved login setting and cwd, preceded by its old
  transcript and a yellow notice saying why the run ended and when the tabs were saved; the saved
  current tab is focused. Reasons told apart: low memory, too many resources in the background,
  force-stopped, the app updated (reported from Android 14), and a crash or an ANR of Appine;
  anything else, no record or no pid gives "Android stopped the app".

### Reports and notifications

- Kill reports (one tab-separated line each): written into the tab whose console matches
  (`VtSession.showMessage`), in yellow above the cursor line (skipped on the alternate screen); a
  notification follows unless the message was shown in the tab the terminal Activity is showing
  while it is visible (another tab or Settings in front does not count), and always for an Appine
  bug. Bugs always notify with "copy report": app version
  and build, device (Android release and API, maker and model, ABI, page size), distro and tier
  (always 1 in a release build), the kernel's report line, and up to 10 earlier kernel log records.
- Crashes of the last run: a crash (Java or native) or ANR record of the killed run posts "Appine
  crashed" or "Appine stopped responding" with "copy report": the bug report's app, device, distro
  and tier lines and one line with the pid, the kind (with the signal for a native crash), the time
  in UTC and, for an ANR, Android's description.
- Boot failures: `ERROR: <service> failed to start` lines in `/var/log/boot.log` after boot.
- Listens: the first listen on a non-loopback address per (guest port, host port) pair and kernel
  (the kernel queues at most 32), with remap text when the ports differ.
- Channels: `sessions` (low), `network`, `problems`; ids 1 (ongoing), 2 (boot), 3 (a crash of the
  last run), 1000 + guest port (listen), and from 100000 one id per kill report, counting up from
  a clock-seeded start (wrapping after 100000).
- The ongoing notification counts running tabs; "Running in the background" when none runs, and
  "Cannot start: …" after a failed start.

## Dependencies (inbound / outbound)

### Inbound

- `MainActivity`, `SettingsActivity` (binder and listeners), notification `PendingIntent`s,
  `ConnectivityManager`, system trim callbacks, `Documents` and `MainActivity` (static settings).

### Outbound

- `Native` (start, boot, shutdown, stop, trim, poll calls, routes, spawn, waitFor, consoleCwd);
  `Rootfs` (prepare, download, mirrors, ensure-files, access lock, reset, failed services); `Tabs`;
  `ActivityManager` (exit records);
  `ConsolePty` and `VtSession`; `Documents` (`rootsChanged`, `dirsChanged`).

## Invariants and gotchas

- Every native call that takes the BKL goes through the single worker, so polls and saves are
  ordered before stop and shutdown; a stale kernel handle is harmless (the JNI table no longer
  finds it).
- Results of a poll are dropped if the kernel changed meanwhile (reports fetched just before a
  restart are lost).
- Android names low-memory kills only where `isLowMemoryKillReportSupported`; elsewhere they are
  plain signals, as is a `SIGKILL` from anyone, and get the general notice. Exit records are kept
  for a limited number of processes per app, so a run killed long ago may have none.
- `exitAll` clears listeners at once so that a quick shutdown is not mistaken for the user
  returning.
- An `exitAll` during a start, restart or replace leaves the rest of the exit to them: every path
  that ends `starting` or `transitioning` must check `exiting`, or the service stops with a kernel
  still running (or never stops).

## Confirmed bugs / technical debt

none

## Open questions

- Should a restored "auto" tab keep the account it actually ran as?

## To verify

- Whether tapping "copy report" after the service stopped recreates the service (and starts a
  kernel) through its `getService` pending intent.
