# android/service Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/android/service.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 2 research concerns; 10 claims checked by 1 independent verifier — 8 confirmed and 2 partial; bugs and debts recorded)
>
> Maintained: 2026-11-04 (targeted verification: stale source comments fixed; removed the remarks about them)

## Responsibilities

`android/service` is the app's foreground service, `TerminalService`, which owns everything that
outlives an Activity:

- the kernel handle (`machine`), starting, booting (inittab), shutting down, rebooting and
  replacing the kernel;
- all terminal sessions and their PTY adapters, and restoring tabs after the process was killed
  (`Tabs`);
- the ongoing notification and its actions, the wake lock, memory trimming;
- a 2 s poll that fetches kernel events (listens, kills, power requests, process count, changed
  directories) and turns them into tab messages, notifications and lifecycle actions;
- the settings state that the activities only display and forward (tier, keep-awake, login
  account, system documents, distro switch, mirror).

Root: `android/app/src/main/java/io/github/mthli/appine/TerminalService.kt`, `Tabs.kt`.

Outside the module: rootfs import, downloads, mirrors and `login.sh` (`android/setup`); rendering,
the tab bar and the PTY adapter (`android/terminal`); the Files app provider
(`android/documents`); JNI and the kernel API (`android/native`, `kernel/process`).

## Key types

- `TerminalService` — a same-process service; `LocalBinder` hands activities a direct reference.
- `Listener` — UI callbacks with no-op defaults, always on the main thread: `onMachineReady(error)`,
  `onProgress`, `onSessionsChanged`, `onTextChanged`, `onExitAll`, `onRestarting`,
  `onSettingsChanged`, `onNotice`. Activities register in `onServiceConnected` and leave in
  `onStop`; "UI visible" means at least one listener (Settings included).
- `worker` — a single-thread executor (`appine-worker`) that serialises native calls taking the
  BKL: poll, shutdown, stop, trim, routes, `consoleCwd`.
- Session state: `sessions`, `ptys` (session → `ConsolePty`), `unshown` (restored tabs not yet
  displayed), `restore` (saved tabs from a killed run), `focus`, `current`.
- `Switch` — a distro download in progress (volatile `done`, `total`, `cancelled`).
- `Tabs` — the `files/tabs/` store: `running` marker, `index.json`, `t<i>.txt`; `Tab(login, cwd,
  transcript)`, `Saved(tabs, current, time)`.
- Constants: poll 2 s, trim delay 60 s, periodic save 60 s, save-soon 2 s.

## Public entry points

- Lifecycle: `onCreate`, `onStartCommand` (EXIT_ALL, CANCEL_SWITCH, TOGGLE_KEEP_AWAKE, COPY_REPORT;
  `START_NOT_STICKY`), `onTrimMemory`, `onDestroy`.
- Called by activities: `addListener`/`removeListener`, `createSession`/`removeSession`, `exitAll`,
  `setTier`, `setKeepAwake`, `switchTo`/`cancelSwitch`/`canSwitch`, `setMirror`/`autoMirror`,
  `setLogin`, `setSystemDocuments`, `takeNotice`.
- Static settings read by `Documents`: `login()`, `showSystemDocuments()`.

## Data flow / lifecycle

### Start and boot

- `onCreate` starts foreground (type `specialUse` on API 34+), takes saved tabs from a killed run
  (`Tabs.takeKilled`) and calls `start(ALPINE)` when no rootfs is ready, else `start(null)`.
- `start` runs on an `appine-start` thread: import when a distro is given (`Rootfs.prepare`, then a
  mirror-probing thread), `ensureNetworkConfig`, `ensureLoginTools`, `updateProfile`, then
  `Native.start(store, tier)` under `Rootfs.access`'s write lock. On success (main thread): set
  `machine`, refresh Files roots, update the wake lock, mark running, restore saved tabs once,
  schedule periodic saves, start the boot thread, register a default-network callback (routes),
  post the poll, `onMachineReady(null)`.
- Boot (`appine-boot` thread): run the osclock script without a console, `Native.boot` (blocks
  until inittab's boot entries finish), then notify OpenRC services that failed to start.
- A failed import of a downloaded distro falls back to Alpine with a notice; any other failure sets
  `startError` and reports it.

### Poll (every 2 s, on the worker)

- Fetches `takeListens`, `takeKills`, `takePower`, `processes`, `takeChangedDirs`; results are
  applied on the main thread only if the kernel is still the same.
- Reboot → `restart()`; halt/poweroff → `exitAll()`; no sessions, no listeners and no guest
  processes → `exitAll()`. Changed directories go to `Documents.dirsChanged`.

### Stopping, restarting, replacing

- `exitAll`: mark exiting, cancel a switch, `onExitAll`, clear listeners immediately, `detach`, show
  "stopping services", `Native.shutdown` on the worker (inittab shutdown, 30 s), then release the
  wake lock and clear saved tabs; restart the kernel if a listener came back meanwhile, else
  `stopSelf`.
- `detach`: unregister the network callback, clear `machine` (before sessions end, so the UI does
  not treat it as the last tab closing), update the wake lock, cancel poll and saves, finish and
  clear all sessions.
- `restart` (guest reboot, tier change): `detach`, `onRestarting`, shutdown on the worker, then
  `start(null)`; the UI opens a new tab when the new kernel is ready and no sessions exist.
- Distro switch/reset: download in the background (progress in Settings and the notification, both
  cancellable), then `replace`: reset the login setting, `detach`, `Native.stop` (kill all, no
  inittab shutdown), join the mirror thread, delete the store, `start(distro)`.
- `onDestroy` with a kernel still present shuts it down on the worker, best effort.

### Wake lock and memory

- `PARTIAL_WAKE_LOCK` held when keep-awake is on and the kernel runs or is shutting down; not
  reference-counted, no timeout; the notification shows a toggle and a "keep awake" suffix.
- The last listener leaving starts a 60 s timer that trims memory (`Native.trimMemory` on the
  worker) and saves tabs at once; a returning listener cancels it. `onTrimMemory` at
  `RUNNING_LOW` and above (except `UI_HIDDEN`) trims and saves immediately.

### Sessions, saving and restore

- `createSession(login, cwd)` builds a `ConsolePty` and a `TerminalSession` (2000 rows); the shell
  starts when the view first knows its size. The shared session client forwards events to all
  listeners; clipboard requests are handled in the service.
- Saving (`saveTabs`): for running sessions only, the transcript (plain text, last 256 K chars) on
  the main thread and the foreground group's cwd (`consoleCwd`) on the worker; unshown restored tabs
  keep their saved data; files are written temp-then-rename and skipped when unchanged. Triggers:
  every 60 s, 2 s after a tab opens or closes, when the last listener leaves, on memory trim.
- The `running` marker is written after a successful start and removed (with saved tabs) on a
  normal exit; `takeKilled` deletes it and loads the tabs when it is present at service creation.
- Restore: each saved tab reopens with its saved login setting and cwd, preceded by its old
  transcript and a yellow notice with the save time; the saved current tab is focused.

### Reports and notifications

- Kill reports (one tab-separated line each): written into the tab whose console matches, in
  yellow above the cursor line (skipped on the alternate screen); a notification follows when the
  message could not be shown, when no listener exists (Settings counts as one) or when it is an
  appine bug. Bugs always notify with "copy
  report": app version and build, device (Android release and API, maker and model, ABI, page
  size), distro and tier setting, the kernel's report line, and up to 10 earlier kernel log records.
- Boot failures: `ERROR: <service> failed to start` lines in `/var/log/boot.log` after boot.
- Listens: the first listen on a non-loopback address per (guest port, host port) pair and kernel
  (the kernel queues at most 32), with remap text when the ports differ.
- Channels: `sessions` (low), `network`, `problems`; ids 1 (ongoing), 2 (boot), 1000 + guest port
  (listen), 3000 + pid % 1000 (kill).

## Dependencies (inbound / outbound)

### Inbound

- `MainActivity`, `SettingsActivity` (binder and listeners), notification `PendingIntent`s,
  `ConnectivityManager`, system trim callbacks, `Documents` (static settings).

### Outbound

- `Native` (start, boot, shutdown, stop, trim, poll calls, routes, spawn, waitFor, consoleCwd);
  `Rootfs` (prepare, download, mirrors, ensure-files, access lock, reset, failed services); `Tabs`;
  `ConsolePty` and the vendored `TerminalSession` (`setRestored`, `showMessage`,
  `getTranscriptText`); `Documents` (`rootsChanged`, `dirsChanged`).

## Invariants and gotchas

- Every native call that takes the BKL goes through the single worker, so polls and saves are
  ordered before stop and shutdown; a stale kernel handle is harmless (the JNI table no longer
  finds it).
- Results of a poll are dropped if the kernel changed meanwhile (reports fetched just before a
  restart are lost).
- `exitAll` clears listeners at once so that a quick shutdown is not mistaken for the user
  returning.

## Confirmed bugs / technical debt

- **"Exit all" during a start leaks a kernel.** It is offered while preparing and starting, but
  nothing cancels the start thread: its success callback still sets `machine`, takes the wake lock
  (never released, `onDestroy` already ran), writes the running marker, boots and keeps reposting
  the poll every 2 s; if the UI returns meanwhile, a second start runs and the first kernel is
  overwritten without shutdown.
- **After a start failure the service never stops by itself.** The poll is only posted on success,
  so the exit rule never runs; the notification then reads "0 sessions running" instead of the
  error.
- **Reboots and distro switches run without the wake lock.** `detach` clears `machine` without
  setting `exiting`, so with keep-awake on the guest shutdown, import and restart can stall with the
  screen off.
- **A distro download that finishes during a guest reboot is dropped** without replacing or telling
  the user (the tarball stays cached).
- **Saved tabs can be lost or mismatched.** The running marker is deleted at service creation, so a
  second kill (or a failed start) before the kernel is up wipes the saved tabs; a save rewrites
  transcripts by position before replacing the index, so a kill in between pairs old tabs with other
  tabs' transcripts.
- **Finished tabs keep the service alive.** A shell that exits while the UI is away leaves its ✕
  session in `sessions`, so the exit rule never fires; its exit also does not trigger a save.
- **Notification ids collide.** Listens use 1000 + guest port and kills 3000 + pid % 1000, so ports
  2000–2999 clash with kill notifications, and a later kill with
  the same pid % 1000 replaces an earlier bug notification and its "copy report".
- **Kill messages can go unseen.** With Settings open, or another tab in front, an OOM message goes
  only to its tab and no notification is posted.
- **A restored "auto" tab may log in as another user**, since the saved value is the setting, which
  `login.sh` resolves again.
- The console of a closed tab is closed twice, which can hang up a newly opened tab (see
  `android/terminal`).

## Open questions

- Should "exit all" be disabled while the rootfs is being prepared or the kernel is starting?
- Should the wake lock also cover reboots and distro replacement?
- Should a restored "auto" tab keep the account it actually ran as?

## To verify

- Whether tapping "copy report" after the service stopped recreates the service (and starts a
  kernel) through its `getService` pending intent.
