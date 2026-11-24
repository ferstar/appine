# android/file-import Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/file-import.md` for the paired decision history (pre-split decisions D4 and D62 stay in `.agents/decisions/android/terminal.md`).
>
> Verified: 2026-11-15 (split out of the android/terminal full mapping of 2026-11-14; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-24 (targeted verification only since the full mapping; latest change: cancel blocked provider reads and publish staged files only into the original rootfs generation)

## Responsibilities

`android/file-import` owns `TerminalFiles.kt` in `android/app/src/main/java/io/github/mthli/appine/`:
process-local picker state, retained copy work/results, safe home-file creation and shell path
escaping. Picker launching, privacy checks, result dialogs and clipboard writes in MainActivity
remain `android/terminal`; the import button belongs to `android/tabbar`.

## Key types

- `TerminalFiles : AndroidViewModel` — one process-local picker request, selected URIs, worker and
  result (successful guest paths and failures); read-only `changes` tells the activity to update
  its import button and present the result. `importSelected` reports whether it started a worker,
  `copySelected` performs the batch, and MainActivity's `showImportResult` builds its dialog.
  It keeps the Application, never the activity.

## Public entry points

- `begin(login)`, `selected(uris)`, `importSelected()`, `cancelImport()`, `dismissResult()`.
- `occupied`, `copying`, `result`, `changes`; `copyIntoHome` and `clipboardPaths` helpers.

## Data flow / lifecycle

### Importing files

- The top-right Import button opens the system multiple-document picker for `*/*`, only while
  the page is unlocked and the kernel runs. No storage permission or external file-access setting
  is needed. One request, copy or undismissed result occupies the button; cancelling releases it.
- `TerminalFiles.begin` retains the login preference. The picker callback accepts results only
  for that process-local request and deduplicates URIs. A resumed unlocked page starts the copy;
  the worker captures the account and rootfs generation under a short read lock. The account's
  home is independent of tab cwd, effective user and mounts; a missing home or `/` is refused.
- Each provider query and read runs outside the rootfs lock, writing one unlinked app-cache file.
  After source close, publication takes the read lock and checks the generation again.
  `Native.browseImport` copies from the cache into a detached store object, checks cancellation,
  and publishes under a collision-safe name. Successful files notify watchers and Files listings.
  An individual failure is recorded and the batch continues. Staging may temporarily use two
  copies of the current file.
- The retained result lists successful guest paths in monospace and per-file failures in a protected
  dialog. A blank line separates the explanation from the paths; message assembly preserves spans.
  Copy paths explicitly puts space-separated shell arguments on the clipboard, prefixing whitespace
  and shell metacharacters with backslashes and leaving ordinary paths unquoted. It appends no
  newline and sends no terminal input. Result display
  requires a resumed unlocked page, and copying checks the gate again.
- Recreation retains selection/copy/result; destruction dismisses its dialog without consuming
  the result, so it can reappear after unlock. Clearing the ViewModel or pressing the tab bar's
  stop button wakes the coordinator and signals the provider. Completed publications remain in the result. A
  provider ignoring cancellation may keep its worker alive, without the store lock.

## Dependencies (inbound / outbound)

### Inbound

- `android/terminal` owns the picker callback, observes state, resumes copies after unlock and
  consumes results only on explicit dismissal. `android/tests` exercises native copy and lifecycle paths.

### Outbound

- ContentResolver supplies Android document streams; `android/setup` resolves accounts and
  holds the Rootfs store read lock. `android/documents` supplies naming/path helpers and
  `dirsChanged` notifications.
- `android/native` supplies the staging fd and publishes the completed file through `kernel/store`.
- `android/terminal` supplies localized resources and privacy-protected presentation.

## Invariants and gotchas

- The ViewModel keeps the Application, never the activity; recreation retains state but process
  death does not. Clearing the ViewModel cancels its import.
- MainActivity must be resumed and unlocked before starting a pending copy or showing a result;
  clipboard copying checks the gate again.

## Confirmed bugs / technical debt

- An interrupted import cannot resume after process death. Completed files survive.

## Open questions

None.

## To verify

- File import was exercised through the API 34 system picker with multiple files, cancellation,
  collision numbering and activity recreation, including redisplay of the result. The
  instrumentation cases cover publication after source close, a concurrent name collision,
  cancellation while a source ignores interrupts, failure cleanup, provider notifications, guest reads, shell escaping and locked-picker retention.
  An API 34 process kill during a copy verified cleanup on reopen and retry. Cloud document
  providers and vendor pickers remain unverified.
