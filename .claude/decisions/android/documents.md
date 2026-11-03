# Files app provider (android/documents) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/documents"`
>
> Last distilled: 2026-11-03 (HEAD = c3df5d3) — seeded by hand from the retired design doc §11.3 and
> the round records in `docs/rounds/`; no `MODULE: android/documents` Decision blocks exist yet, so
> sources cite rounds instead of commits.

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
- **What**: Inside the home, files can be written, created, deleted, renamed and moved; the whole-system root (off by default) is read-only; symlinks leading outside the home are refused for changes; new files are owned by the account, 0644/0755, numbered on collision; renames and moves never overwrite.
- **Why**: Copying files in and editing them with Android apps is the common use, and files must be editable by the same account in the terminal.
- **Tradeoffs**: No guest permission checks (everything in the store belongs to the device's user anyway); no `copyDocument`, so copies go through create-and-write.
- **Source**: rounds 196 (`196-documents-read`), 197 (`197-documents-write`)

### D4: Changes reach the other side in both directions
- **What**: Provider changes are replayed under each running kernel's lock as inotify events; write fds carry a close callback that reports `IN_MODIFY`/`IN_CLOSE_WRITE`; directories the guest changed are polled by the service and notified to the Files app.
- **Why**: Hot-reload tools in the guest (`vite`, `cargo watch`) must see edits made with Android editors, as they do in Termux where files are real host files; the kernel's inotify is emulated and knew nothing of the provider's separate store connection. The Files app also did not refresh after guest changes.
- **Tradeoffs**: `ParcelFileDescriptor.wrap` needs API 30 (minSdk is 29), so the fd is reopened by path to get a close callback, and a failed reopen falls back to the plain fd without the close events.
- **Source**: round 201 (`201-documents-notify`)

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
