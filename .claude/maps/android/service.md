# android/service Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/android/service.md` for the paired decision history.
>
> Verified: 2026-11-10 (full mapping: 3 research concerns; 14 claims checked by 2 independent verifiers — 9 confirmed and 5 partial, corrected below; bugs recorded)
>
> Maintained: 2026-11-12 (targeted verification only since the full mapping; latest change: channel names and the ongoing notification following a language change, the list of services that failed at boot)

## Responsibilities

`android/service` is the app's foreground service, `TerminalService`, which owns everything that
outlives an Activity:

- the kernel handle (`machine`), starting, booting (inittab), shutting down, rebooting and
  replacing the kernel and its store;
- all terminal sessions (`VtSession`s, `android/vt`), and restoring tabs after the process
  was killed (`Tabs`);
- the ongoing notification and its actions, the wake lock, memory trimming;
- a 2 s poll that fetches kernel events (listens, kills, power requests, process count, changed
  directories, too-long holds of the BKL) and turns them into tab messages, notifications and
  lifecycle actions, and the routing of what programs ask of the app (bells, notifications,
  program statuses, clipboard writes and reads) and of commands the shells' prompt marks see
  finish;
- the settings state that the activities only display and forward (tier in debug builds,
  keep-awake, login account, system documents, CJK characters filling two cells, the cursor's
  shape and blinking, whether the volume keys switch tabs, distro switch, mirror), the preferences
  kept in `Settings`.

Root: `android/app/src/main/java/io/github/mthli/appine/TerminalService.kt`, `Notifications.kt`,
`Settings.kt`, `Tabs.kt`.

Outside the module: rootfs import, downloads, mirrors and `login.sh` (`android/setup`); the screen
and its tab bar (`android/terminal`); rendering (`android/vtview`); sessions' internals and the
console adapter (`android/vt`); the Files app
provider (`android/documents`); JNI and the kernel API (`android/native`, `kernel/process`).

## Key types

- `TerminalService` — a same-process service; `onBind` returns a `LocalBinder` whose `service` is
  the service itself.
- `Listener` — UI callbacks with no-op defaults, always on the main thread: `onProgress`,
  `onMachineReady(error)`, `onSessionsChanged`, `onBell` (the tab in front), `onClipboardRead`
  (a program in the tab in front, the current one, asks to read the clipboard), `onTabStatus` (a
  tab's progress or program status changed), `onSettingsChanged`,
  `onNotice`, `onRestarting`, `onExitAll` (sessions redraw their view themselves). Activities
  register in `onServiceConnected` and leave in `onStop`; "UI visible" means at least one listener
  (Settings included).
- `worker` — a single-thread executor (`appine-worker`) for the calls that take a kernel handle
  and the BKL in the background (the poll's fetches, `setRoutes`, `consoleCwd`, `trimMemory` with
  the tabs' scrollback compression after it, `shutdown`, `stop`), together with what must stay
  ordered with them: saving tabs (their terminals' snapshots included), resolving an `auto` login, changed directories,
  `Rootfs.setMirror`, and a replacement's mirror join and store reset.
- Session state (internal): `sessions` (every tab, ended ones included), `unshown` (restored tabs
  whose terminal does not exist yet, with their saved copy), `restore` (saved tabs from a killed
  run, kept until a start restores them), `ended` (why that run ended), `focus` (set only by the
  restore), `current` (written only by `MainActivity`; setting it takes the tab's notification and
  bell throttle away and, with the terminal Activity listening, marks the tab's statuses seen),
  `nextSessionId` (counting up from the clock, never wrapped); `sessionEvents` (title changed,
  bell, clipboard write, notification, clipboard read, progress or status changed, a command
  finished, tmux's control mode detached, the shell finished).
- Lifecycle state (internal): `exiting`, `starting` (a start thread is running), `transitioning`
  (restart or replace, between letting go of the old kernel and starting the new one),
  `pendingReplace` (a distro download that finished during a start or restart), `startError`,
  `progress`, `mirrorThread`.
- `Notifications` — the service's notifications but the ongoing one, which the service builds
  itself: the channels, the stuck kernel, the last run's crash, boot failures, listens, kill
  reports and their "copy report" (the bug report's text and the action the service handles), and
  each tab's notification; with their state: `stallNotified` (the stall last notified),
  `killNotifications` (the next kill notification's offset, seeded from the clock), `bellNotified`
  (when each tab's bell last notified), `statusNotified` (the program status a tab's notification
  shows, while it shows one), `channelLocales` (the languages the channels were last named in).
  The service decides what is told (the tab in front, a message
  shown in the tab); `Notifications` builds, posts and cancels.
- `Settings` — the `settings` preferences: getters and plain setters taking a `Context` for the
  tier (always 1 in a release build), login, keep-awake, system documents, wide cells, the
  cursor's shape (stored as Ghostty's names) and blinking, and whether the volume keys switch tabs
  (on by default); `clearLogin` for a replaced store.
- `Switch` — a distro download in progress (volatile `done`, `total`, `cancelled`): `switching` is
  the active one, `downloader` the one whose thread has not ended yet.
- `Tabs` — the `files/tabs/` store: `running` marker (the pid of the run that left it),
  `index.json` (recording the libghostty-vt version, `VtNative.version`, its snapshots were written
  with), transcripts named by content (`t-<128-bit SHA-256 prefix>.txt`; earlier versions wrote
  `t<i>.txt`, still read), terminal snapshots named by content and deflated
  (`s-<128-bit SHA-256 prefix of the snapshot>.snap`); `Tab(login, cwd, transcript, snapshot)`
  (the snapshot's file name, null for none), `Saved(tabs, current, time)`, `Killed(pid, saved)`;
  transcripts capped at 256 K chars, snapshots read back at most 16 MiB.
- Constants: poll 2 s, trim delay 60 s, periodic save 60 s, save-soon 2 s, bell notifications at
  most once a second per tab, commands notifying from 10 s.

## Public entry points

- Lifecycle: `onCreate`, `onStartCommand` (EXIT_ALL, CANCEL_SWITCH, TOGGLE_KEEP_AWAKE, COPY_REPORT;
  `START_NOT_STICKY`), `onBind`, `onTrimMemory`, `onDestroy`.
- Called by activities: `addListener`/`removeListener`, `createSession(account = login setting,
  cwd = null)`/`removeSession`, the `current` setter, `exitAll`, `setTier` (ignored in a release
  build), `setKeepAwake`, `switchTo`/`cancelSwitch`/`canSwitch`, `setMirror`/`autoMirror`,
  `setLogin`, `setSystemDocuments`, `setFillWide`, `setCursorStyle`, `setCursorBlink`,
  `setVolumeKeysSwitchTabs`, `takeNotice`; state read by them: `machine`, `startError`,
  `progress`, `switching`, `sessions`, `focus`, the settings.
- Settings read without the service: `Settings.login` and `systemDocuments` by `Documents`,
  `Settings.fillWide` by `MainActivity` (in `onStart` and `onSettingsChanged`) and
  `Settings.volumeKeysSwitchTabs` by it at each volume key press; the service's getters read
  `Settings` too, and its setters write it and act on the change (`setVolumeKeysSwitchTabs` only
  tells the listeners).

## Data flow / lifecycle

### Start and boot

- `onCreate` creates the four channels, starts foreground (type `specialUse` on API 34+) and takes
  a killed run (`Tabs.takeKilled`; with no marker it deletes `files/tabs/` and there is none): its
  saved tabs, and how its process ended, looked up by the marker's pid in Android's exit records
  (`ApplicationExitInfo`); a crash or ANR of Appine posts a notification at once. Then it calls
  `start(Rootfs.importing())` when no rootfs is ready (Alpine on the first run, else the distro an
  import was last started for), else `start(null)`.
- `start` clears `startError`, sets `starting` (the wake lock follows) and runs on an
  `appine-start` thread: import when a distro is given (`Rootfs.prepare`, with progress to the
  notification and listeners, then an `appine-mirrors` thread choosing a mirror),
  `ensureNetworkConfig`, `ensureContainersConfig`, `ensureLoginTools`, `updateProfile`, then
  `Native.start(store, tier)` under `Rootfs.access`'s write lock (`tier` is the setting in a debug
  build and always 1 in a release build, whatever an earlier debug install recorded).
- Back on the main thread it clears `starting`; if "exit all" came meanwhile it finishes the exit
  with the new kernel (`Native.stop`, not booted). On success: set `machine`, clear `progress`,
  refresh Files roots; a `pendingReplace` replaces the store at once; otherwise update the wake
  lock, write the running marker, restore the killed run's tabs (once), schedule periodic saves,
  start the boot thread, register a default-network callback (routes), post the poll, update the
  notification, `onMachineReady(null)`.
- On failure: a failed import of a downloaded distro (not Alpine, store not ready) falls back to
  Alpine with a notice; with a `pendingReplace` the store is replaced; any other failure sets
  `startError`, updates the wake lock and notification ("Cannot start: …"), reports it to the
  listeners, and stops the service as soon as no UI is left and no distro switch is downloading
  (there is no kernel, so no poll would end it; opening the app tries again).
- Which start: when no store is ready (`startOrImport`, at creation and when an exit finishes with
  a UI back) the distro the last import was started for is imported first, otherwise the store
  starts as it is.
- Boot (`appine-boot` thread): run the osclock script without a console (failures only logged),
  `Native.boot` (blocks until inittab's boot entries finish), then notify OpenRC services that
  failed to start, if the kernel is still the same.

### Poll (every 2 s)

- On the main thread, while a kernel runs: first `bklStall` (it takes no BKL, and the worker
  would wait behind a hold that does not end) reports a too-long hold of the BKL (Reports below);
  then the worker fetches `takeListens`, `takeKills`, `takePower`, `processes` and
  `takeChangedDirs`; the poll reschedules itself at once, whether or not the fetch has run.
- Changed directories are handled on the worker straight away: `Documents.dirsChanged`, then
  `Rootfs.dirsChanged` (a change in `/etc` rewrites the login's NOPASSWD rules, android/setup).
- The rest is applied on the main thread only if the kernel is still the same: listens, kills,
  then power. Reboot → `restart()`; halt/poweroff → `exitAll()`; no running sessions (tabs whose
  shell ended do not count; restored tabs not shown yet do), no listeners and no guest processes
  (init and zombies excluded) → `exitAll()`.

### Stopping, restarting, replacing

- `exitAll` (once): mark exiting, cancel a switch and a `pendingReplace`, `onExitAll`, clear
  listeners immediately, `detach`, show "Stopping services…". While `starting` or `transitioning`
  it stops there and the start or restart finishes the exit once its kernel is up or has failed;
  otherwise `finishExit`: `Native.shutdown` on the worker (inittab shutdown, 30 s; `Native.stop`
  for a kernel that was never booted), then clear `exiting`, release the wake lock and clear saved
  tabs unless tabs of a killed run are still waiting to be restored; start the kernel again
  (importing when no store is ready, as after a failed import the exit came during) if a listener
  came back meanwhile, else `stopSelf`.
- `detach`: unregister the network callback, clear `machine` (before sessions end, so the UI does
  not treat it as the last tab closing), update the wake lock, cancel the poll and saves, release
  (hang up and free) every session and take its notification away, clear `sessions` and
  `unshown`; it runs before the kernel is shut down or stopped.
- `restart` (guest reboot, tier change in a debug build; nothing without a kernel):
  `transitioning`, `detach`, `onRestarting`, shutdown on the worker, then `start(null)` (or, after
  "exit all" meanwhile, finish the exit); the UI opens a new tab when the new kernel is ready and
  no sessions exist.
- Distro switch/reset (`switchTo`, allowed while no switch or download is under way and either a
  kernel runs or the start failed): download on an `appine-download` thread (progress in Settings
  and the notification, both cancellable; a failure other than a cancel shows a notice); when it
  finishes, `replace` if a kernel runs or the start failed, else record it in `pendingReplace`
  while a start or restart is under way (in any other state it is dropped). After a failed start
  the switch keeps the service running without a UI; its failure or cancel then stops it as a
  failed start does. `replace`: reset the login setting, `transitioning`,
  `detach`, then on the worker `Native.stop` (kill all, no inittab shutdown), join the mirror
  thread and delete the store, then `start(distro)` — also after "exit all" meanwhile, so the
  chosen distro is imported (and then stopped) rather than the next start importing Alpine.
- `onDestroy` marks the service destroyed (from then on nothing posts the ongoing notification,
  which threads outliving it, the download's and the mirror probes', would otherwise post again),
  cancels a distro switch still downloading, removes the pending callbacks, clears saved tabs
  unless tabs of a killed run are still waiting to be restored, unregisters the network callback,
  releases the sessions, shuts a remaining kernel down on the worker (best effort), shuts the
  worker down and releases the wake lock.

### Wake lock and notification

- `PARTIAL_WAKE_LOCK` held when keep-awake is on and the kernel is starting (import included),
  running, restarting, being replaced or shutting down; not reference-counted, no timeout.
- The ongoing notification (id 1) shows, by precedence: "Stopping services…" while exiting; the
  import or download progress while no kernel runs; "Cannot start: …" after a failed start;
  "Running in the background" while a kernel runs and no tab does; else the count of running
  sessions ("0 sessions running" while a start is under way, except that a start after an exit,
  because a listener came back, keeps "Stopping services…" until it ends when keep-awake is off,
  as nothing refreshes the notification). A switch adds its progress, subtext
  and a cancel action; "kept awake" is added while the lock is held, the keep-awake toggle is hidden
  while exiting, and "Exit all" is always there.

### Memory

- The last listener leaving saves tabs at once and starts a 60 s timer that trims memory (on the
  worker: `Native.trimMemory`, the kernel's rebuildable caches, then `VtNative.compressAll`, every
  tab's cold scrollback at once, android/vt); a returning listener cancels it. `onTrimMemory` at
  `RUNNING_LOW` and above (except `UI_HIDDEN`) trims and saves immediately, and also cuts the Kitty
  images of every started tab but `current` down to 4 MiB a screen, deleting the oldest
  (`VtNative.dropImages`, android/vt); since Android 14 those levels are no longer sent, so this
  happens only on Android 11–13. Tabs also compress their own scrollback within seconds of their
  output, so the trim mostly finds what the last few seconds left.

### Sessions, saving and restore

- `createSession` builds a `ConsolePty` and a `VtSession` with the next id and gives it the
  cursor settings (`VtSession.setCursor`: the shape, a `GhosttyCursorStyle`, recorded as Ghostty's
  `cursor-style` name, and blinking); changing either in Settings gives every tab, restored ones
  not shown yet included, the new values at once. The shell starts when a view first knows its
  size. A title change only calls the listeners' `onSessionsChanged`; a
  shell's end, and opening or closing a tab, go through `sessionsChanged` (the notification's
  count, the listeners, a save 2 s later while a kernel runs). `removeSession` releases the session
  and takes its notification away, as `detach` and `onDestroy` do for every tab.
- Saving (`saveTabs`, while a kernel runs): for running tabs only, the login setting and the
  transcript (plain text, last 256 K chars; `VtSession.transcriptText`) on the main thread, and on
  the worker the foreground group's cwd (`consoleCwd` of the tab's console, when it has one; null
  once the shell has ended) and, for a tab whose shell started, its terminal's snapshot
  (`VtSession.takeSnapshot`: written with `Tabs.writeSnapshot` when it changed since the last save,
  else the file saved last, `VtSession.savedSnapshot`; none when none can be taken), with an `auto`
  login saved as the account it resolves to
  (`Rootfs.loginAccount`, kept in `VtSession.resolvedLogin` from the tab's first save) so a
  restored tab logs in as the same user. Restored tabs not shown yet keep their saved transcript,
  snapshot and cwd; the saved current tab is an index among the running tabs (0 when none is current).
- `Tabs.save` writes temp-then-rename, as `writeSnapshot` did before it; a transcript's or
  snapshot's name is its content digest, so unchanged ones are not rewritten; the index (with the time) is skipped when its tabs and current tab hash the
  same as the last one this process wrote, so the saved time is that of the last save that found a
  change or was its process's first; transcripts and snapshots no index names any more (and their
  leftover temporary files) are deleted only after the index step (a kill at any point leaves a
  consistent set). A
  `name` key in an index from versions that could rename tabs is ignored. Triggers: every 60 s, 2 s
  after a tab opens, closes or its shell ends, when the last listener leaves, on memory trim.
- The `running` marker, holding the process's pid, is written after each successful start and
  removed (with saved tabs) on a normal exit; `takeKilled` loads the tabs when it is present at
  service creation and leaves the marker and files alone, so a second kill (or a failed start)
  before the kernel is up keeps them, and the marker still names the run they belong to. A marker
  from an earlier version is empty (pid 0).
- Restore, at the first successful start without a pending replacement (a failed start or a
  replacement keeps `restore` for the next one): each saved tab reopens with its saved login
  setting and cwd, below its terminal as saved (its snapshot, read and inflated when the tab is
  first shown, when the index's version is this build's and it decodes; else its transcript) and a
  yellow `[appine]` notice saying why the run ended and when the tabs were saved; the saved current tab becomes `focus`. Seven reasons are told
  apart: low memory, too many resources in the background, force-stopped (user requested or
  stopped), the app updated (from Android 14), a crash (Java or native) and an ANR of Appine;
  anything else, no record or no pid gives "Android stopped the app".

### What programs ask of the app

- A bell in the tab in front (`inFront`: it is `current` and `MainActivity` is a listener, so not
  another tab and not Settings in front) goes to the listeners' `onBell`; elsewhere it becomes the
  tab's notification, at most once a second per tab and alerting only once. An OSC 9 or OSC 777
  notification becomes the tab's notification unless the tab is in front. A change of a tab's
  progress or program status goes to the listeners' `onTabStatus`; a program status that is
  blocked, done or failed (`Notifications.status`) becomes the tab's notification, once per status
  (the same record again posts nothing), unless the tab is in front, where done and error count as seen
  at once (`VtSession.statusSeen`). A status that goes without the user (the program was killed or
  stopped waiting) takes its notification away, unless a bell or a program's notification has
  replaced it since. A clipboard write sets
  the clipboard from any tab (labelled Appine), or clears it; before Android 13, which shows copies
  itself, a toast says a program copied text. A clipboard read (OSC 52) from the tab in front goes
  to the listeners' `onClipboardRead`, where the terminal Activity asks the user; from any other
  tab it is answered at once with an empty clipboard (`VtSession.answerClipboardRead(null)`):
  Android lets an app read the clipboard only while it has the focus, and the program would
  otherwise wait for a timeout.
- tmux's control mode (tmux -CC), which the terminal detaches as it starts (android/vt),
  ending (`onTmuxDetached`) is told in that tab as a yellow `[appine]` line
  (`VtSession.showMessage`): it cannot be shown, its session goes on, and `tmux attach` brings it
  back.
- A command the shell's prompt marks (OSC 133) saw finish (`onCommandFinished`, timed by the
  terminal, android/vt) after at least 10 s, in a tab not in front (another tab shown, or
  the terminal Activity not listening), becomes the tab's notification, as Ghostty's
  `notify-on-command-finish = unfocused` (Ghostty's default is `never` and its threshold 5 s);
  shorter ones and those in the tab in front post nothing. Like a program's notification it
  replaces a status notification (`statusNotified` forgets the tab).

### Reports and notifications

- Kill reports (one tab-separated line each: console, pid, command, `bug` or `oom`, and three
  fields: for a bug the kernel's report line, its log records and the unimplemented paths, for an
  OOM kill the limit): written as `[appine] …` into the tab whose console with that number is open,
  else the newest tab that had it (ended tabs keep their number, which the kernel hands to the next
  console), when the process had one (`VtSession.showMessage`, queued until a tab's terminal exists), in yellow above the cursor line
  (skipped on the alternate screen); a notification "<command> was killed" follows unless the
  message was shown in the tab in front (another tab or Settings in front does not count), and
  always for an Appine bug. OOM kills say whether the app's limit or a cgroup's `memory.max` (in
  MiB or GiB) was reached.
- Bugs always notify with "copy report": app version and build, device (Android release and API,
  maker and model, ABI, page size), distro and tier (always 1 in a release build), the kernel's
  report line, its kernel log records (up to 10 earlier ones and the report's own, one per line of
  it), and under
  "Unimplemented paths reached" the 10 the guest reached most since boot, with a line for the rest
  (`kernel/signal`). "Copy report" puts the text on the clipboard (with a toast before Android 13)
  and takes that notification away.
- Crashes of the last run: a crash (Java or native) or ANR record of the killed run posts "Appine
  crashed" or "Appine stopped responding" (id 3) with "copy report": the bug report's app, device,
  distro and tier lines and one line with the pid, the kind (with the signal for a native crash),
  the time in UTC and, for an ANR, Android's description.
- Too-long holds of the BKL (`kernel/process`'s watchdog): a new stall of the current kernel posts
  "Appine's kernel is stuck" (id 4) with the seconds when first seen (not refreshed) and "copy
  report" (the watchdog's report line); once it has ended, a notification still showing becomes
  "was stuck" with the total seconds and the kernel log record in its report, without alerting
  again (a dismissed one stays dismissed); a stall first seen after it ended posts "was stuck"
  directly.
- Boot failures: `ERROR: <service> failed to start` lines in `/var/log/boot.log` after boot (id 2,
  "N services did not start", with the services listed as the
  app's language lists things (ICU's `ListFormatter`)).
- Listens: the first `listen()` on a non-loopback address per (guest port, host port) pair and
  kernel (the kernel queues at most 32; one dropped from a full queue is reported only when the
  guest listens on that pair again), with remap text when the ports differ; the notification id is
  `1000 + guest port`.
- Tab notifications (`Notifications.tab`, on the `terminal` channel): one per tab (id
  `200000 + tab id mod 100000`), replaced by the tab's next one: titled with OSC 777's title or a status record's
  title, else the program's title (`sh` when none), with the body, "The terminal bell rang", or
  for a status what it says (`VtSession.describe`: "Waiting for your approval", "Waiting for your
  answer", "Waiting for credentials", "Waiting for you", "Done", "Failed") and its message after a
  colon, and "Tab N" as subtext. A finished command's is titled "Command succeeded" (exit code
  0), "Command failed" (another code) or "Command finished" (none reported), with "Took <time>."
  or, after a failure, "Took <time> and exited with code N." (the time in ICU's short units in the
  app's language: hours and minutes, minutes and seconds, or seconds). Tapping
  one opens the activity on that tab (`EXTRA_TAB` with the session id); showing the tab or closing
  it takes it away.
- Channels: `sessions` (low), `network`, `problems`, `terminal` (named "Bells, notifications from
  programs and finished commands"); kill reports take ids from 100000,
  counting up from a clock-seeded start and wrapping after 100000. Their names are in the app's
  language: `onConfigurationChanged` names them anew when the configuration's languages have
  changed and posts the ongoing notification again; the other notifications keep the language
  they were posted in. The bug report is in English whatever the language.

## Dependencies (inbound / outbound)

### Inbound

- `MainActivity`, `SettingsActivity` (start and bind, listeners, setters, state); `VtSession`
  (`Events`); notification `PendingIntent`s; `ConnectivityManager`; system trim callbacks;
  `Documents` and `MainActivity` (`Settings`).

### Outbound

- `Native` (start, boot, spawn, waitFor, shutdown, stop, trim, the poll's calls, `bklStall`, routes,
  `consoleCwd`); `VtNative.compressAll` and `dropImages`, and `VtNative.version` through `Tabs`;
  `Rootfs` (readiness, importing, prepare, download, mirrors, ensure-files, profile, access lock,
  store, reset, failed services, rules refresh, login accounts); `Tabs`; `VtSession` and
  `ConsolePty`; `Documents` (`rootsChanged`, `dirsChanged`).
- Android: `ActivityManager` (exit records), `PowerManager`, `NotificationManager`,
  `ConnectivityManager`, `ClipboardManager`, the `settings` preferences, `BuildConfig`.

## Invariants and gotchas

- Calls that take the BKL in the background go through the single worker, so polls and saves are
  ordered before stop and shutdown; `Native.start` runs on `appine-start`, the boot calls on
  `appine-boot`, a tab's console opening on the main thread. A stale kernel handle is harmless
  (the JNI table no longer finds it). A call that must answer while the BKL is stuck (`bklStall`)
  must not take it and runs on the main thread.
- Results of a poll other than changed directories are dropped if the kernel changed meanwhile
  (reports fetched just before a restart are lost). While a hold of the BKL lasts, the poll keeps
  queueing fetches behind it on the worker, which run back to back once it ends.
- Android names low-memory kills only where `isLowMemoryKillReportSupported`; elsewhere they are
  plain signals, as is a `SIGKILL` from anyone, and get the general notice. Exit records are kept
  for a limited number of processes per app, so a run killed long ago may have none.
- `exitAll` clears listeners at once so that a quick shutdown is not mistaken for the user
  returning.
- An `exitAll` during a start, restart or replace leaves the rest of the exit to them: every path
  that ends `starting` or `transitioning` must check `exiting` (a replacement does so through the
  start it ends with), or the service stops with a kernel still running (or never stops).
- Threads that outlive the service (a download, mirror probes) post to the main thread after
  `onDestroy`: they reach notification 1 only through `updateNotification`, which does nothing once
  destroyed, so a new way of posting it must go through that too.
- The marker is rewritten at a start before the first save replaces the old run's index, so a kill
  in between restores the old tabs again; tabs of a killed run kept through a replacement are
  restored into the new distro, where their directories may not exist.

## Confirmed bugs / technical debt

- **A kill report of a shell can reach a new tab:** when a tab's shell itself is killed and a tab
  opened within the next poll (2 s) gets the freed console number, the report goes to the new tab,
  whose console is open, rather than the ended one.

## Open questions

- Should a long stall's notification update its seconds while the stall goes on?
- Listen notifications are keyed by guest port only, so listens on one guest port mapped to
  different host ports replace each other's notification: intended?

## To verify

- The channel names and the ongoing notification following a language change while the service
  runs (`onConfigurationChanged`) was compiled only, not tried on a device.
- Whether tapping "copy report" after the service stopped recreates the service (and starts a
  kernel) through its `getService` pending intent.
- Finished-command notifications were tried only on a Pixel 4a API 34 emulator (busybox ash and
  bash 5.3 from another tab, ash with the app in the background, English and Simplified Chinese),
  not on real devices or with long durations in hours.
