# android/documents Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/android/documents.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 1 research concern; 5 claims checked by 1 independent verifier — 5 confirmed; bugs and debts recorded)
>
> Maintained: 2026-11-04 (targeted verification: the system root no longer depends on a home root; a home of `/` is no home, and the native side refuses `/` as the writable top; two bugs removed)

## Responsibilities

`android/documents` is the `DocumentsProvider` that shows the guest's files in the system Files
app and in other apps' pickers:

- the roots: the login account's home (writable) and, behind a setting, the whole system
  (read-only);
- document IDs, columns, flags and MIME types;
- reading, writing, creating, deleting, renaming, moving and searching through the store's browse
  API, without the kernel;
- change notifications in both directions.

Root: `android/app/src/main/java/io/github/mthli/appine/Documents.kt` (and its manifest entry).

Outside the module: the store-side browse implementation, real-path checks, naming and ownership
of new files (`kernel/store`); the poll that delivers guest changes (`android/service`); account
resolution and the store lock (`android/setup`); the JNI calls (`android/native`).

## Key types

- `Documents : DocumentsProvider` — roots `home` and `system`; search limit 200.
- `Home(account, uid, gid, path)` — the resolved home root, cached 5 s per login setting.

## Public entry points

- Framework: `queryRoots`, `queryDocument`, `queryChildDocuments`, `openDocument`,
  `createDocument`, `deleteDocument`, `renameDocument`, `moveDocument`, `isChildDocument`,
  `querySearchDocuments` (no `copyDocument`, `removeDocument`, recents or thumbnails).
- Static, for the service: `rootsChanged`, `dirsChanged`.

## Data flow / lifecycle

### Roots and IDs

- With a ready rootfs, the home comes from the login setting through `Rootfs.loginAccount`
  (`/etc/passwd` read from the store; root with `/root` when unreadable) and is shown only if it
  exists and is not `/` (which would make the whole system writable). Home root: summary
  "account · distro", flags local-only, is-child, search, create (making it a "Save to" target),
  free space from `getAllocatableBytes`. System root `/`: same without create, when the setting
  (default off) is on, with or without a home root.
- Document IDs are absolute guest paths with `%`, control characters and non-UTF-8 bytes written
  as `%XX`; only canonical IDs (no empty, `.` or `..` components) count. With the system root off
  only the home and IDs under it are visible; IDs under the home are writable, the home itself only
  as a create or move target.
- Columns: ID, display name (decoded, U+FFFD for bad bytes), MIME type, size (files), modified
  time, flags from the literal path (home: create; under home: delete, rename, move, plus write or
  create; elsewhere none).
- MIME: by extension (`MimeTypeMap`); files without a known extension are sniffed in one batch
  (first 4 KB without NUL and valid UTF-8 → `text/plain`, at most 1000 per call); otherwise
  `application/octet-stream`.

### Operations

- Every call holds `Rootfs.access`'s read lock and opens the store afresh per JNI call; errors
  surface as `FileNotFoundException`.
- Read: the host object file opened read-only and adopted as the returned fd.
- Write modes: the native side checks the real path against the home, truncates if asked and
  returns an fd; the provider reopens the host object path with a close listener (on a
  `documents-close` thread) that reports the write, so the kernel sees `IN_MODIFY` and
  `IN_CLOSE_WRITE`; data goes directly into the host object.
- Create: names sanitised (`/` and NUL to `_`; empty, `.`, `..` to `_`), an extension added from
  the MIME type for files; owned by the home account, 0644/0755, numbered on collision. Delete,
  rename (no overwrite, no extension added) and move are limited to the home by real path; each
  revokes grants on the source ID and notifies the affected directories.
- Search: by name under the root, breadth first (200 results, 50,000 entries).

### Notifications

- Provider to kernel: the store's change path replays the changes under each running kernel's lock
  (guest inotify events).
- Kernel to Files app: the service's 2 s poll passes directories the guest changed to
  `dirsChanged`, which notifies their child-document URIs; `rootsChanged` follows login and setting
  changes and each successful kernel start.
- With no rootfs `queryRoots` returns nothing and other calls fail; during a rootfs replacement the
  write lock holds provider calls until the store is gone.

## Dependencies (inbound / outbound)

### Inbound

- The framework (manifest provider: exported, grant URI permissions, `MANAGE_DOCUMENTS`);
  `TerminalService` (`rootsChanged`, `dirsChanged`); `MachineTest`.

### Outbound

- `Native.browse*`; `Rootfs` (`isReady`, `installed`, `access`, `loginAccount`); the service's
  static settings.

## Invariants and gotchas

- Visibility is decided on the literal path while writes are limited by real path; reads follow
  symlinks wherever they point, so an app holding a grant on the home tree can read outside it
  through a link the guest user made (an accepted limitation: no guest permission checks apply).
- Flags shown in the Files app are a hint; the real write check is in the native code.
- If reopening a write fd with a close listener fails, the plain fd is returned and the guest gets
  no `IN_MODIFY`/`IN_CLOSE_WRITE` for that write (the data still lands).

## Confirmed bugs / technical debt

- **Grants outlive moved or deleted directories.** Only the exact source ID is revoked; grants on
  its descendants survive, and since IDs are paths a file later created at the same path is covered
  by the old grant.

## Open questions

- Should `/etc/passwd` changes (an `adduser` while the login is "auto") refresh the roots?

## To verify

- The framework's grant semantics for path-shaped IDs (taken from AOSP, not checked on a device).
