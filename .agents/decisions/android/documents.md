# Files app provider (android/documents) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/documents"`
>
> Last distilled: 2026-11-14 (HEAD = 410ef192). D1–D6 were seeded by hand from the retired design doc
> §11.3 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Active

### D1: The provider reads the store directly, without the kernel
- **What**: Every call resolves guest paths in the store through the browse API, opening the store afresh and closing it when done; regular files are handed out as the host object's fd; mounts are invisible.
- **Why**: The Files app or a picker can wake the app at any time, usually with no kernel running, and starting a kernel to list a directory is too heavy; handing out the object fd avoids copies and pipes.
- **Tradeoffs**: The provider sees the store as it is, not the running system (`/proc`, tmpfs and bind mounts are absent); while its connection is open the kernel stops deferring commits, so connections must stay short.
- **Watch out**: `Rootfs.access` keeps provider calls from overlapping the kernel's exclusive boot open and store deletion.
- **Source**: round 196 (`196-documents-read`)

### D2: Document IDs are guest absolute paths
- **What**: IDs are canonical absolute paths with `%`, control characters and non-UTF-8 bytes escaped as `%XX`; only canonical IDs inside a shown root are accepted.
- **Why**: Paths are readable when debugging, and the escaping keeps arbitrary byte names reversible.
- **Tradeoffs**: A file re-created at the same path is the same document to the framework.
- **Source**: round 196 (`196-documents-read`)

### D3: Only the login account's home is writable, limited by real path, and new files belong to that account
- **What**: Inside the home, files can be written, created, deleted, renamed and moved; the whole-system root (off by default) is read-only; symlinks leading outside the home are refused for changes; new files are owned by the account, 0644/0755, numbered on collision; renames and moves never overwrite; a home of `/` counts as no home, and the whole-system root is listed whenever its setting is on, with or without a home.
- **Why**: Copying files in and editing them with Android apps is the common use, and files must be editable by the same account in the terminal.
- **Tradeoffs**: No guest permission checks (everything in the store belongs to the device's user anyway); no `copyDocument`, so copies go through create-and-write; an account whose home is `/` gets no home root, so with the system root off the Files app shows nothing for it.
- **Watch out**: A home that is a symlink to `/` passes the Kotlin check (its flags are only a hint), and only the store's check (kernel/store D9) refuses its writes.
- **Source**: rounds 196 (`196-documents-read`), 197 (`197-documents-write`); d5dbe29

### D4: Changes reach the other side in both directions
- **What**: Provider changes are replayed under each running kernel's lock as inotify events; write fds are wrapped by `ParcelFileDescriptor.wrap` with a close listener on the documents-close thread that reports `IN_MODIFY`/`IN_CLOSE_WRITE`; directories the guest changed are polled by the service and notified to the Files app in one `notifyChange` call.
- **Why**: Hot-reload tools in the guest (`vite`, `cargo watch`) must see edits made with Android editors, as they do in Termux where files are real host files; the kernel's inotify is emulated and knew nothing of the provider's separate store connection. The Files app also did not refresh after guest changes.
- **Tradeoffs**: If `wrap` throws, the same fd number is adopted again and handed out without close events, relying on `wrap` detaching the fd before it can fail (as AOSP has since API 30); the batched notification drops the sync-to-network flag, which no sync adapter here uses.
- **Watch out**: A platform whose `wrap` fails before detaching would make the fallback adopt an fd `wrap` already closed.
- **Source**: round 201 (`201-documents-notify`); 88b9138

### D5: Files without a known extension are recognised as text by content
- **What**: Unknown types are sniffed in one batch: the first 4 KB without NUL and valid UTF-8 means `text/plain`; at most 1000 files per call.
- **Why**: Extensionless text (`/etc/profile`, scripts, `Makefile`) and dotfiles such as `.bashrc` otherwise had no app to open them, which matters now that the home is writable.
- **Tradeoffs**: Each sniffed file costs a host open and read; listings of 1000 such files stayed within noise on the emulator.
- **Source**: round 204 (`204-text-by-content`)

### D6: Android's shared storage is reached through the provider; SAF mounts are planned for v1.x
- **What**: v1 asks for no storage permission: files move in and out through this provider ("Copy to…"). Planned for v1.x and not implemented: SAF directories mounted at `/mnt/<name>`, with synthetic modes (directories 0755, files 0644), `chmod`/`chown` succeeding silently, `symlink`/`link` returning `EPERM`, directory listings cached in memory, and case-insensitivity documented.
- **Why**: Play does not approve `MANAGE_EXTERNAL_STORAGE` for terminal apps (design doc §15).
- **Tradeoffs**: Until then guest programs cannot open files in Android's shared storage directly.
- **Source**: design doc §11.3, §15

### D7: The Files app provider is a module of its own
- **What**: android/documents covers `Documents.kt`, while the store-side browse API stays in kernel/store.
- **Why**: §11.3 of the retired design doc, shared storage, needed a home.
- **Tradeoffs**: Reading out of the home through symlinks remains an accepted limitation.
- **Watch out**: Transient grants on a moved or deleted directory's descendants outlive it (open in the map); persisted ones are revoked (D10).
- **Source**: 12c4afa (bug list since updated by 3f761df)

### D8: Provider helpers follow the framework callbacks that use them
- **What**: Documents orders types, roots, queries, ancestry/path callbacks, opens and changes with nearby helpers, sharing `percentDecode` for link/display names and `within` for containment checks.
- **Why**: Execution order was hard to follow and decoding/containment logic was duplicated.
- **Tradeoffs**: Moved lines need blame -M, and the shared companion helper remains public despite only provider use.
- **Watch out**: Device checks did not cover navigation/decoding and visibility after the deduplication.
- **Source**: a546fea; 9d485e6e

### D9: `file:` links open as the provider's documents
- **What**: `Documents.fileLink` maps a `file:` URI whose host is empty, `localhost` or the name in `/etc/hostname` to the document with the same guest path (percent-decoded, written back as an ID, empty and `.` components dropped, `..` refused), typed by the provider's own `getType` so its visibility rules apply; the terminal resolves it off the main thread and grants the viewer read access only (android/terminal D31), `findDocumentPath` lets the Files app open a directory it is sent to, and remote, hidden and missing files get their own toasts.
- **Why**: `ls --hyperlink` and similar tools write `file:` links to guest paths, which have no host file, and a `file://` URI cannot be handed to another app (`FileUriExposedException`).
- **Tradeoffs**: Files outside the home open only with "Show the whole system in Files" on; viewers can read but not save, so edits go through the Files app; `/proc`, `/dev` and other mounts are not in the store, so links into them count as missing.
- **Watch out**: The Kotlin ID encoding must change together with the native `encode_path` (D2); the host is matched against `/etc/hostname`, not the running host name; `findDocumentPath` now also serves the Files app's own navigation.
- **Source**: 1cc504b

### D10: Moving or deleting a directory revokes the persisted grants below it
- **What**: `revokeTree` revokes the source document's ID and then every persisted outgoing grant on this provider whose document (a tree URI by its tree's document) is the path or lies below it (`Documents.grantedUnder`).
- **Why**: Grants on the descendants of a deleted or moved directory survived, and since IDs are paths (D2) they covered a file created at the same path later.
- **Tradeoffs**: Transient grants, given for an activity, cannot be listed and last until that activity ends (map debt).
- **Watch out**: A failing `getOutgoingPersistedUriPermissions` counts as no grants, and revoking the provider's own URIs relies on the framework's documented behaviour, not exercised end to end.
- **Source**: 3f761df

### D11: The provider reads the login and whole-system root settings through `Settings`
- **What**: `Documents` calls `Settings.login` and `Settings.systemDocuments` (android/service D23) instead of TerminalService's static functions.
- **Why**: Those statics moved to `Settings` when notifications and settings left the service.
- **Tradeoffs**: None beyond one more class the provider names.
- **Watch out**: None known; the keys and defaults are unchanged.
- **Source**: 0553229

## Superseded

- ~~Write fds reopened by their `/proc/self/fd` path to get a close callback, since `wrap` needed API 30 above minSdk 29 (D4 in part, round 201)~~ → replaced by **D4** in 88b9138 (2026-11-07)
- ~~D8’s companion path/MIME helpers keeping their old order~~ → replaced by **D8** in 9d485e6e (2026-11-12)
