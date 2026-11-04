# Terminal service (android/service) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/service"`
>
> Last distilled: 2026-11-05 (HEAD = 173da76). D1–D6 were seeded by hand from the retired design
> doc §11.2 and the round records in `docs/rounds/`, so their sources cite rounds; later entries
> cite commits.

## Active

### D1: The kernel and all sessions live in a `specialUse` foreground service
- **What**: `TerminalService` owns the kernel handle and every terminal session; activities bind, register listeners and forward user actions; session events go to whichever listeners are present.
- **Why**: Destroying or recreating an Activity must not affect sessions or guest processes, and the foreground service keeps the app process from being treated as a cached process while the kernel runs.
- **Tradeoffs**: With a foreground service, Android 14 and later deliver no `onTrimMemory` callbacks, so memory release has to watch the UI itself (D4).
- **Source**: rounds 07 (`07-m5`), 187 (`187-app-init`)

### D2: The service outlives the last tab and stops only after an inittab shutdown
- **What**: Closing the last tab only finishes the Activity; the 2 s poll ends the service when there are no sessions, no UI and no guest processes; "exit all", guest `poweroff` and `halt` run the inittab shutdown before `stopSelf`; guest `reboot` starts a new kernel; boot runs on its own thread without blocking tabs.
- **Why**: Services started by OpenRC and `nohup` jobs should keep running without a tab, and exiting used to SIGKILL every process instead of letting them shut down.
- **Tradeoffs**: Nothing is shut down cleanly when the system kills the app; there is no autostart at Android boot, only on app cold start.
- **Watch out**: `exitAll` must clear the listeners immediately: a shutdown that finished before the Activity's `onStop` once looked like the user returning and restarted the kernel.
- **Source**: round 187 (`187-app-init`); design doc §11.2

### D3: Keeping the CPU awake is an opt-in partial wake lock, with no battery-optimisation exemption
- **What**: A setting and a notification toggle (default off) hold an unreferenced `PARTIAL_WAKE_LOCK` while the kernel runs or shuts down.
- **Why**: AOSP disables partial wake locks in Doze only for apps whose process state is below `BOUND_FOREGROUND_SERVICE`, and network access uses the same threshold, so a foreground-service app keeps both; requesting the exemption directly is restricted on Play to app categories that exclude terminals.
- **Tradeoffs**: The lock is held whenever the kernel runs, even with only an idle shell; vendor background managers (MIUI, ColorOS) are not handled; Play's vitals may count the lock.
- **Source**: round 191 (`191-keep-awake`)

### D4: Memory is trimmed a minute after the UI disappears
- **What**: When the last listener leaves, a 60 s timer calls `Machine::trim_memory`; returning cancels it; `RUNNING_LOW` and above (except `UI_HIDDEN`) trim at once.
- **Why**: Android 14 and later send nothing a foreground-service process can act on; the delay avoids retranslating node or python after a brief look away.
- **Watch out**: Trimming and stopping share one single-thread executor, so a trim never runs on a released kernel.
- **Source**: round 37 (`37-trim-memory`)

### D5: After a kill, tabs come back without their processes
- **What**: A `running` marker detects an abnormal end; each running tab's login setting, name, foreground cwd and plain-text scrollback (last 256 K, in transcripts named by their content) are saved every minute, when the UI hides, on memory pressure and after tabs open, close, end or are renamed; on restart each tab gets a new shell in its directory below the old scrollback and a notice, and saved tabs are kept until a killed run's tabs have been restored.
- **Why**: v1 restores only the layout, directories and scrollback, and says so; kills mostly happen in the background, hence the save on hide; unchanged files are not rewritten so the minute timer does not wear the flash.
- **Tradeoffs**: Programs are not restored; colours are lost; tabs never shown since the restore keep their old data; each rename rewrites `index.json`.
- **Source**: round 194 (`194-restore-tabs`); design doc §11.2; 2af55e2, 89ad551

### D6: Kernel kills are explained in the tab or a notification; appine bugs always notify with a copyable report
- **What**: Each kill report goes to the tab of its controlling console as a yellow line, and to a notification when the UI is away, the program is full-screen or there is no tab; appine's own bugs always notify, with "copy report" (app, device, distro, tier, the kernel's report line with panic location, recent kernel log records); OpenRC services that failed at boot are notified too.
- **Why**: Tabs used to show only "Killed" or "Segmentation fault", which does not tell an OOM kill from an appine bug; after release the copied report is what a user can bring to the developer.
- **Tradeoffs**: Reports are polled, so they arrive after the shell's prompt (hence the insertion above the cursor, `android/terminal` D2).
- **Source**: rounds 195 (`195-kill-reports`), 205 (`205-copy-bug-report`), 207 (`207-report-kernel-log`)

### D7: The service is a module of its own, split from the app by responsibility
- **What**: android/service holds `TerminalService.kt` and `Tabs.kt`; the app is split into service, setup, documents, terminal and native modules by responsibility, in the maps only.
- **Why**: §11.2's lifecycle table had to leave the retired design doc, and the user asked for the app to be split.
- **Tradeoffs**: The code stays in one Gradle module, so cross-module statics remain (the login setting, the store lock).
- **Watch out**: A restored "auto" tab may log in as another user, since the saved value is the setting (open in the map).
- **Source**: 12c4afa (bug list since updated by 89ad551)

### D8: The service holds `VtSession`s and releases them before the kernel stops
- **What**: `sessions`, `unshown`, `focus` and `current` are `VtSession`s; saving reads each session's console and login on the worker (no separate pty map); `removeSession`, `detach` and `onDestroy` release sessions (hang up and free the emulator) before the kernel is shut down or stopped; kill reports find the tab by console; title changes and ends arrive through `VtSession.Events` as `onSessionsChanged`.
- **Why**: The service held Termux `TerminalSession`s directly with a separate session → `ConsolePty` map, so the libghostty-vt terminal could not be one of its tabs (android/terminal D8).
- **Tradeoffs**: Listeners get no per-text-change callback; tabs redraw their view directly.
- **Watch out**: Finished tabs keep the service alive, saved tabs can be lost or mismatched, and notification ids collide.
- **Source**: 6ffb912

### D9: Exit all, restart and replace finish around an ongoing start instead of racing it
- **What**: While `starting` or `transitioning` is set, Exit all only marks and detaches and the start, restart or replace finishes the exit; the wake lock is held throughout, a download finished during a restart waits in `pendingReplace` until the new kernel is up or has failed, a failed start says so in the notification and stops the service once no UI is left, and kill notifications take ids counting up from 100000.
- **Why**: Nothing tied the start thread, restart and replace to Exit all, the wake lock or a finished download, so an exit during a start left a kernel running after `onDestroy`, reboots dropped the wake lock and downloads finishing mid-reboot were lost.
- **Tradeoffs**: An exit during a distro replacement still imports the chosen distro first (and waits for a long import), and a kill shown in a background tab now also notifies.
- **Watch out**: Every new path that ends `starting` or `transitioning` must check `exiting` (a map invariant), and kill notification ids from a new service instance can still collide with old ones after wrapping.
- **Source**: 89ad551

### D10: The execution tier is read from preferences only in debug builds
- **What**: `TerminalService.tier` reads the `tier` preference only in a debug build and is 1 otherwise, `setTier` does nothing in a release build, and the copied bug report keeps "Tier N".
- **Why**: A release build must run Tier 1 even on a device where an earlier debug install recorded Tier 0.
- **Tradeoffs**: A stale `tier` key stays in the preferences of devices that once ran a debug build.
- **Watch out**: Code reading `KEY_TIER` directly would bypass the gate, so every read goes through `tier`.
- **Source**: 62a8c07

### D11: Display settings the terminal needs before binding live in the service's `settings` file
- **What**: `fill_wide` (default on) is kept in the `settings` preferences, changed through the service's `setFillWide`, which notifies listeners via `settingsChanged`, and read by `MainActivity` through a static `fillWide(context)`.
- **Why**: Settings rows display and forward state the service keeps, and the terminal's activity needs the value before it binds the service.
- **Tradeoffs**: It is one more cross-module static setting, like `login()` and `showSystemDocuments()`, and the service keeps a display preference it never uses itself.
- **Watch out**: `MainActivity` is not a listener while stopped (Settings covers it), so it must keep rereading the value in `onStart`.
- **Source**: 0478dd5

## Superseded

- ~~A `Session` interface in the service, with `createSession` building a `TermuxSession` or a `VtSession` by `VtNative.available` (6ffb912)~~ → narrowed to `VtSession` only by android/terminal **D8** in b38d29c (2026-11-04)
