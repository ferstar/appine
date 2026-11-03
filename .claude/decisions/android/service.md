# Terminal service (android/service) Decisions
> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/service"`
> Last distilled: 2026-11-03 (HEAD = c3df5d3) — seeded by hand from the retired design doc §11.2 and
> the round records in `docs/rounds/`; no `MODULE: android/service` Decision blocks exist yet, so
> sources cite rounds instead of commits.

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
- **What**: A `running` marker detects an abnormal end; each running tab's login setting, foreground cwd and plain-text scrollback (last 256 K) are saved every minute, when the UI hides, on memory pressure and after tabs open or close; on restart each tab gets a new shell in its directory below the old scrollback and a notice.
- **Why**: v1 restores only the layout, directories and scrollback, and says so; kills mostly happen in the background, hence the save on hide; unchanged files are not rewritten so the minute timer does not wear the flash.
- **Tradeoffs**: Programs are not restored; colours are lost; tabs never shown since the restore keep their old data.
- **Source**: round 194 (`194-restore-tabs`); design doc §11.2

### D6: Kernel kills are explained in the tab or a notification; appine bugs always notify with a copyable report
- **What**: Each kill report goes to the tab of its controlling console as a yellow line, and to a notification when the UI is away, the program is full-screen or there is no tab; appine's own bugs always notify, with "copy report" (app, device, distro, tier, the kernel's report line with panic location, recent kernel log records); OpenRC services that failed at boot are notified too.
- **Why**: Tabs used to show only "Killed" or "Segmentation fault", which does not tell an OOM kill from an appine bug; after release the copied report is what a user can bring to the developer.
- **Tradeoffs**: Reports are polled, so they arrive after the shell's prompt (hence the insertion above the cursor, `android/terminal` D2).
- **Source**: rounds 195 (`195-kill-reports`), 205 (`205-copy-bug-report`), 207 (`207-report-kernel-log`)
