# android/file-import Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/file-import.md` for the paired decision history (pre-split decisions D4 and D62 stay in `.agents/decisions/android/terminal.md`).
>
> Verified: 2026-11-15 (split out of the android/terminal full mapping of 2026-11-14; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-24 (targeted verification only since the full mapping; latest change: publish completed home imports from detached store directories)

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

- `begin(login)`, `selected(uris)`, `importSelected()`, `dismissResult()`.
- `occupied`, `copying`, `result`, `changes`; `copyIntoHome` and `clipboardPaths` helpers.

## Data flow / lifecycle

### Importing files

- The top-right Import button opens the system multiple-document picker for `*/*`, only while
  the page is unlocked and the kernel runs. No storage permission or external file-access setting
  is needed. One request, copy or undismissed result occupies the button; cancelling releases it.
- `TerminalFiles.begin` retains the login preference. The picker callback accepts results only
  for that process-local request and deduplicates URIs. A resumed unlocked page starts the copy;
  the worker resolves the account once against the current store and holds `Rootfs.access`'s read
  lock for the batch. Destination is that account's home, independent of the tab's cwd, effective
  user and mounts. A missing home or `/` is refused.
- Provider display names are used with a localized fallback. Controls, slash and NUL are replaced,
  empty/dot names are sanitized, and extensions are preserved. `Native.browseImport` stages a
  regular file owned by the account outside the guest tree. Its callback opens the source and
  streams it to a duplicate of the borrowed fd. After both streams close, the store syncs the
  data. One transaction rechecks the home and publishes the file under an available name.
  Guest watchers receive the create, open and write/close events after commit. On success
  `copySelected` refreshes Files app listings through `Documents.dirsChanged`. A failed file
  leaves the home untouched and the batch continues.
- The retained result lists successful guest paths in monospace and per-file failures in a protected
  dialog. A blank line separates the explanation from the paths; message assembly preserves spans.
  Copy paths explicitly puts space-separated shell arguments on the clipboard, prefixing whitespace
  and shell metacharacters with backslashes and leaving ordinary paths unquoted. It appends no
  newline and sends no terminal input. Result display
  requires a resumed unlocked page, and copying checks the gate again.
- Recreation retains selection/copy/result; destruction dismisses its dialog without consuming
  the result, so it can reappear after unlock. Process death loses this state. An already-started
  worker continues after backgrounding or closing the activity. There is no cancellation; slow
  document providers hold up rootfs reset/replacement. Publication is per file. Completed files
  survive an interruption, and the next exclusive store open reclaims abandoned staging files.

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
  death does not. A started worker keeps running after the activity closes.
- MainActivity must be resumed and unlocked before starting a pending copy or showing a result;
  clipboard copying checks the gate again. The batch lock can delay rootfs replacement.

## Confirmed bugs / technical debt

- A running import cannot be cancelled or resumed after process death. Closing its activity can
  lose the eventual result display.

## Open questions

None.

## To verify

- File import was exercised through the API 34 system picker with multiple files, cancellation,
  collision numbering and activity recreation, including redisplay of the result. The nine
  instrumentation cases cover publication after source close, a concurrent name collision,
  failure cleanup, provider notifications, guest reads, shell escaping and locked-picker retention.
  An API 34 process kill during a copy verified cleanup on reopen and retry. Cloud document
  providers and vendor pickers remain unverified.
