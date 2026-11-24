# kernel/mm Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/kernel/mm.md` for the paired decision history (it still holds the decisions on the soft-MMU, the OOM killer, fork/copy-on-write and host backing made before they were split out into `kernel/mmu`, `kernel/oom`, `kernel/cow` and `kernel/hostmem`).
>
> Verified: 2026-11-10 (full mapping: 6 research concerns; 18 claims checked by 3 independent verifiers — 13 confirmed and 5 partial, all corrected below; bugs and debts recorded)
>
> Maintained: 2026-11-24 (targeted verification only since the full mapping; latest change: main executable and interpreter open records across unmap, fork and last-user exit)

## Responsibilities

`kernel/mm` implements Linux address-space and VMA semantics on top of `kernel/mmu`'s soft-MMU and
`kernel/hostmem`'s host backing: per-process `Mm`/VMA tables, memory syscalls, brk, stack growth,
guest fault policy, `[vvar]`/vDSO mapping, file EOF/truncation/object-move updates, on-demand FUSE
mappings (`Fill`), kernel guest access (`UserSlice`) and `/proc/<pid>/maps`/core-dump views.

Root: `core/kernel/src/mm.rs` (excluding `kernel/mmu`'s entry population/consistency/code groups,
`kernel/oom`'s resident-memory measures/`Mm::release`, `kernel/cow`'s `fork`/`Copy-on-write`, and
`kernel/hostmem`'s `Host backing`), `core/kernel/src/syscall/mem.rs` (excluding `process_mrelease`),
`core/kernel/src/uaccess.rs` (`UserSlice`), the page-fault branch in `task.rs`, and `Cx` stack-growth
helpers in `kernel.rs`.

Outside the module: `kernel/cow`, `kernel/hostmem`, `kernel/mmu`, `kernel/oom`, `engine`,
`engine/dbt`, `kernel/ipc`, `kernel/futex`, `kernel/exec`, `kernel/time`, `kernel/procfs`,
`kernel/cgroup`, `kernel/signal`, `kernel/vdso`, `kernel/fuse`, `kernel/fileio` and `core/hal`.

## Key types

### Address space

- `Mm` — per-process guest address space (`Rc<RefCell<Mm>>` under the BKL; built unlocked at execve):
  owns `mmu` (dropped first), `vmas: BTreeMap<start, Vma>`, deferred-release `graveyard`, layout
  bounds (`brk_start`, `brk`, `mmap_top`, `start_stack`, `stack_low`), non-reused `id`, `shared_maps`
  (shared writable ranges for the fast futex path), FUSE-fill flag, loader/procfs/vDSO/membarrier/
  `mlockall` metadata, and per-mm handles for `kernel/mmu`, `kernel/cow` and `kernel/hostmem`.
- `Vma` — one guest mapping: range, guest prot, `Backing`, `may_write`/`may_exec`,
  `Arc<HostRegion>` (`kernel/hostmem`) + host pointer, file metadata (`file_len`, identity/version,
  mapped-file fd, display `name`/`ino`, optional `MountPin` `mnt` whose drop reports to
  `vfs::released`), SysV `shm`, optional `Fill`, and Linux VMA flags (locked, sticky `written`,
  growsdown, dump/fork/noreserve, `anon`).
- `Fill` (`Rc`) — a FUSE file mapping read on demand: a sparse host file (memfd) as long as the
  file was when mapped, holding what was read at the file's own offsets; the open file to read
  from (kept open while mapped); the runs of file pages read; and the read-ahead state. Every VMA
  made from one mmap call (split, forked, moved, grown) holds the same one.
- `same_vma` — whether two adjacent VMAs are one VMA on Linux: same contents source (both
  private and file-less, the same store inode at contiguous offsets, or contiguous offsets in
  the same shared host region with the same SysV attachment if any), backing, prot, lock,
  may_write, growsdown, dump and fork flags, `anon` and name (not `may_exec`, `noreserve` or the
  fill).
- `Backing` — `Anonymous` (private anonymous, private `/dev/zero`), `PrivateFile` (private file
  mapping), `Shared` (shared file, shared anonymous, shared `/dev/zero`, GPU or SysV shm; also
  `[vvar]`, whose host memory is shared with forked children).
- `flags` — guest ABI values of the `MAP_*` flags the mm interprets.
- `Refill` (private-file piece remapped at old address after `MREMAP_DONTUNMAP`), `Reread` (written
  private-file piece reread after `MADV_DONTNEED`), and `AdviseRange` (host-page-aligned slice of a
  retained `Arc<HostRegion>` for unlocked non-destructive `madvise`).
- `WANTED` (thread-local) — the page of a `Fill` a kernel access found not read, for the syscall
  dispatcher (`take_wanted`).
- `UserSlice` — an unchecked guest range passed to kernel-side accessors.

### Introspection

- `Region` — one `/proc/<pid>/maps` / `smaps` line.
- `DumpRegion`, `dump_filter` — core-dump segments and the `coredump_filter` bits (default `0x33`).

## Public entry points

- **Syscalls** (`syscall/mem.rs` → `Mm`): `mmap`, `mmap_fill`, `munmap`, `protect` (mprotect),
  `mremap` (`mremap_dontunmap`), `remap_file_pages`, `msync`, `madvise`, `mlock`/`mlock2`/
  `munlock`, `mlockall`/`munlockall`, `mincore`, `brk`. (`process_madvise` is `ENOSYS`;
  `process_mrelease` is `kernel/oom`'s; `mmap_gpu` backing is `kernel/hostmem`'s.)
- **execve / loader / faults**: `Mm::new`, `share_files`, `into_shared`, `set_layout`,
  `map_anonymous`, `map_vvar`, `find_free`, `set_brk`, `map_stack`, `set_start_stack`, `label`,
  `vdso_syscalls`; `fill_wanted`, `fault_past_eof`, `Cx::grow_stack`/`grow_stack_to`/`grow_for`,
  `is_mapped` (`cow_break` is `kernel/cow`'s; `mmu().translate`/`fault_in` `kernel/mmu`'s).
- **FUSE fills & kernel access**: `Fill::new`/`size`/`has`/`window`/`window_from`/`store`/`fd`,
  `take_wanted`, `has_fills`, `first_unread`; `read`, `write` (breaks CoW via `kernel/cow`),
  `translate` (no CoW break), `read_cstr`, `write_kernel`, `write_force`, `populate`,
  `populate_mlock`, `mapped_above`, `check_mapped`.
- **Files, shm, futex & introspection**: `futex_key`, `shared_maps`, `id`; `file_truncated`,
  `writably_maps`, `maps_file`, `maps_ino`, `mapped_inos`, `set_mnt`, `mapped_mounts`,
  `shared_file_ranges`; `attach_shm`/`detach_shm`/`shm_attachments`;
  `regions`, `residency`, `size`, `locked_bytes`/`locked_in`, `dump_regions`, `dump_page`.

## Data flow / lifecycle

### Creation and layout

- execve builds a fresh `Mm` (with a fresh `Loans` table) that shares the kernel-wide `FileMaps`;
  the loader fills it with the BKL released, and after the point of no return it is shared and
  installed.
- `set_layout` sets `mmap_top` from the `RLIMIT_STACK` at execve (capped at 8 MB for a secure
  exec): `STACK_TOP` minus the limit plus the stack guard gap, clamped between 1 GB + 8 MB and 5/6
  of the address space. An unlimited stack still gives a top-down layout with the largest gap.
- `STACK_TOP` is `1 << 47`. A position-independent main program loads at `0x5555_5555_0000`.
  Non-fixed `mmap` allocates top-down from `mmap_top` (`find_free`), 2 MB-aligning mappings ≥
  `LEAF_SPAN` (2 MB) when VA space permits (falling back to page alignment) to form `kernel/mmu` L2
  superpages, skipping `stack_guard_gap` (256 pages) below grows-down VMAs, floored at
  `vm.mmap_min_addr` (`MMAP_MIN_ADDR`, 32768, ≥1 page; global, writable with `CAP_SYS_RAWIO`).
- `map_stack` maps the arguments and auxv plus 128 KB (capped at the page-aligned limit, never
  below what the arguments use) at the top of one `MAP_NORESERVE` host region of
  `max(min(RLIMIT_STACK, 8 MB), mapped length)`, leaving the rest below as reserved growth room.

### vDSO and `[vvar]`

- The loader maps the vDSO image (`kernel/vdso`) as private anonymous memory written by the
  kernel. When the image is the host's vDSO, `map_vvar` first maps `[vvar]` right below it, as
  long as the host's vvar mappings and at the same distance, since the host's code addresses its
  data relative to itself: a `Shared` VMA, readable only (`may_write` and `may_exec` false, so
  mprotect asking for more is `EACCES`), on a `foreign` `HostRegion` (`kernel/hostmem`) at the
  host's vvar address. Only its first `data` bytes (the clock data the host's code reads) count as
  contents and get entries; the rest behaves like a file mapping past EOF (`SIGBUS`), as the
  time-namespace page does on Linux outside a time namespace.
- `[vvar]` is shared with forked children like any shared mapping, cannot be written by ptrace
  (`write_force` refuses shared mappings without write permission), cannot grow (`EFAULT`, checked
  before looking for room, Linux's `VM_DONTEXPAND`) or be duplicated by mremap (`EINVAL`), counts
  in no resident-memory measure, writes nothing to core files, and shows as private `r--p` in maps.
- `vdso_svc` says whether the vDSO's clock functions are svc stubs: set at execve for the built
  image, and by `vdso_syscalls` once a process using the address space enters a time namespace
  with offsets, which writes the stubs over the host image's clock entries (`write_kernel`) and
  queues the vDSO for code invalidation. It is copied at fork and never cleared.

### mmap and brk

- The syscall checks, in order: unknown prot bits (`PROT_BTI` and `PROT_MTE` included, `EINVAL`),
  an unaligned offset (`EINVAL`), the map type (`MAP_SHARED_VALIDATE` with unknown flags
  `EOPNOTSUPP`), `MAP_SHARED | MAP_ANONYMOUS | MAP_GROWSDOWN` (`EINVAL`), a fixed address below
  `vm.mmap_min_addr` without `CAP_SYS_RAWIO` (`EPERM`; for an anonymous mapping only with a
  non-zero length; for a file after its `EBADF` and zero-length `EINVAL`), then the file's own
  rules (a FUSE mapping gets its `Fill`; regular shared mappings check access, noexec and seals
  before unsharing and pinning the store object), and `MAP_LOCKED` over the
  locked-memory limit (`EAGAIN`). After the mapping: `reap_shm` for `MAP_FIXED`, grows-down for
  private anonymous memory, the label, write and exec denials, locking, and population
  (`MAP_POPULATE`, `MAP_LOCKED`; a FUSE mapping's pages are read then).
- Lengths round up to a page; zero or overflow is `EINVAL`.
- With personality(2)'s `READ_IMPLIES_EXEC` (which every execve clears, as arm64's
  `SET_PERSONALITY` does), a readable mmap is made executable unless it maps a file on a noexec
  mount (do_mmap), mprotect adds execution to each VMA that may execute (`may_exec`, as
  do_mprotect_pkey does per VMA, so one call can leave mixed permissions), and brk's heap is
  executable (arm64's `VM_DATA_DEFAULT_FLAGS`); a heap grown with other permissions than its last
  part becomes a VMA of its own.
- `MAP_FIXED` / `MAP_FIXED_NOREPLACE`: the address must be aligned and the end at most `1 << 48`
  (`EINVAL`, after the syscall's `EPERM`). `NOREPLACE` on an occupied range is `EEXIST`; otherwise
  the old range is unmapped first, before host allocation, the permission check, the fd copy and
  the map-count check, so a later failure leaves a hole.
- A hint is rounded down to a page and raised to `vm.mmap_min_addr` (Linux's `round_hint_to_min`),
  and used if it ends at or below `mmap_top`, is free, and stays clear of the guard gap below a
  grows-down VMA above it.
- Host backing, `FileMaps` sharing, content-version trust, 16 KB alignment and commit accounting
  belong to `kernel/hostmem`. A FUSE file maps its `Fill`'s memfd empty (`mmap_fill`: no content
  version, no `FileMaps`, `file_len` 0, shared writable `EACCES`).
- `insert` enforces `vm.max_map_count` (default 65530, writable through procfs; a new VMA must keep
  the count at or below it), builds page-table entries (`kernel/mmu`), and sets `locked` from
  `mlockall(MCL_FUTURE)`.
- `brk`: below `brk_start` returns the current break; growth needs the range free; it grows in
  place inside the heap's host region when the last part is anonymous, unnamed, with the heap's
  permissions and lock state and no other VMA on that host range (discarding the host range
  first), else at the first heap page reserves 256 MB (`MAP_NORESERVE`) for growth of at most that,
  else falls back to a separate accounted anonymous mapping. Shrinking unmaps. brk ignores
  `mmap_top`, the guard gap and `vm.mmap_min_addr`, and there is no `RLIMIT_DATA` or `RLIMIT_AS`
  enforcement.

### Mappings read on demand (`Fill`)

- Page-table entries build eagerly for small/executable mappings and lazily otherwise (`kernel/mmu`);
  `PROT_NONE` gets none. A `Fill` VMA is the exception below.
- A `Fill` VMA keeps `file_len` 0, so `map_pages` and every other path sizing by `file_len`
  (mprotect, mremap, fork, populate, truncation) build none of its entries, even when executable;
  only `fault_in` does, for a page already read, and then for the runs read within its 2 MB leaf
  range. Its EOF is the size the file had when mapped (`contents_len`).
- Reading a page (`kernel/fuse`'s `fill`): `Fill::window` picks the pages not read around it
  (normally 32 starting 16 before it; from it, twice the last read when it continues that read or
  twice the run read just before it, between 32 and 512), clipped to the runs already read and
  EOF; the daemon is asked with the BKL released, and `store` writes the pages still missing into
  the memfd with `pwrite`, recording the whole window only when the reply reaches `Fill::size`,
  the single page on confirmed 0-byte EOF, and otherwise only complete pages returned (`kernel/fuse`
  retries from `window_from(page)` when backward read-around stopped short and loops on partial-page
  replies until a full page or EOF). A page not read has no private copy
  anywhere (no entry, no kernel write), so the host file reaches every mapping of it, shared or
  private.
- A kernel access (`read`, `write`, `read_cstr`, `write_kernel`) meeting a page of a `Fill` mapping
  not yet read fails with the internal `FUSE_RETRY` and records the page in `WANTED`; the syscall
  dispatcher reads it and runs the syscall again (`kernel/process`). `read` and `write` go page by
  page, so a copy can stop partway; `write_kernel` checks the whole range first (any permission,
  `PROT_NONE` fills included), before changing anything, and gives `PROT_NONE` anonymous memory it
  writes prot-0 entries.

### Guest page faults (task run loop, under the BKL, in this order)

1. A write fault where `cow_break` (`kernel/cow`) succeeds → re-execute.
2. The page now translates (another thread fixed it while this one waited for the BKL) → re-execute.
3. `fault_in` (`kernel/mmu`) succeeds → re-execute.
4. A page of a `Fill` mapping not yet read and allowing the access (`fill_wanted`) → read it through
   the daemon (above) and re-execute; a signal arriving before the daemon took the read is
   delivered first and the access faults again; a read error → `SIGBUS` / `BUS_ADRERR`.
5. `fault_past_eof`: checks VMA permissions first (`v.prot & need == 0` falls through to step 7's
   `SIGSEGV` / `SEGV_ACCERR`); otherwise past EOF in an ordinary file mapping whose file has grown →
   map the new part and re-execute (not a `Fill` mapping, which keeps its size); otherwise past EOF
   → `SIGBUS` / `BUS_ADRERR`.
6. Stack growth (`grow_down`) succeeds → re-execute.
7. Otherwise `SIGSEGV`: `SEGV_ACCERR` if the address is mapped, else `SEGV_MAPERR`.

"Re-execute" forces no signal and leaves the PC on the faulting instruction (already-pending
signals may still be delivered).

### munmap, mprotect, mremap

- VMAs are only ever split (`split_at` adjusts file offsets and host pointers), never merged;
  `same_vma` tells which pieces Linux would have as one VMA.
- `munmap`: an unaligned address, zero or overflowing length or an end past 2^48 is `EINVAL`;
  `ENOMEM` only when one VMA would split in three at `vm.max_map_count`. It moves removed host
  regions to the `graveyard`, clears `vdso` when overlapping it, sets `dirty`, records executable
  ranges in `code_ranges`, returns borrowed pages (`kernel/cow`), and the syscall reaps SysV
  segments left without attachments.
- `mprotect` (`protect`, Linux's `do_mprotect_pkey`): unknown prot bits (`PROT_SEM` included) or
  both grows bits are `EINVAL` at once; an unaligned address is `EINVAL`; then a length of 0
  succeeds and an overflowing one is `ENOMEM`. The VMAs from the first one ending above the
  address are changed in turn; a gap (`ENOMEM`) or a VMA refusing the permissions
  (`may_write`/`may_exec`, `EACCES`) stops it with the VMAs before it already changed. With
  `PROT_GROWSDOWN` the range starts at the start of the Linux VMA holding that first VMA, which
  must grow down (`EINVAL`; `ENOMEM` if it starts at or above the end), so it reaches the parts the
  mapping grew by; `PROT_GROWSUP` is `EINVAL` once the range is found. A `cow` VMA is copied
  (`unshare`, which sets `dirty`; a `Fill` VMA maps its host file privately again, copies the pages
  it changed after reading them and drops its entries) before it becomes writable, and a
  `reserved` region gets real host memory (`back_reserved`) before it becomes accessible.
  Otherwise only a permission downgrade sets `dirty`. For a private anonymous VMA with fork CoW
  pages, non-write perms apply first, then read-only CoW entries (`kernel/cow`, even where
  `PROT_NONE` had none), then write permission on other pages.
- `mremap`: the syscall checks the flags (`MREMAP_DONTUNMAP` needs equal lengths) and gives `EPERM`
  for a fixed target below `vm.mmap_min_addr` before anything else. Then Linux's order: no mapping
  at the old address is `EFAULT`; a `MREMAP_FIXED` target is validated (`EINVAL`) and unmapped and
  a shrunk tail unmapped; then the rest of the old range must lie in one Linux VMA (`EFAULT`,
  `same_vma`, as Linux before 6.17). Shrinking without a target only unmaps the tail. Growing in
  place needs the old end to be a VMA end, free space above, the new end at or below `mmap_top`,
  and room for one more VMA (the grown part is a separate VMA with the same lock, dump, fork,
  `noreserve` and `anon` flags; a `Fill` VMA's maps more of its host file, sharing the fill); a
  locked VMA's grown part is neither charged nor populated. Shared anonymous memory and SysV
  segments retain their backing region across shrink, growth and moves. Regrown pages alias the
  original bytes up to the region's end and fault with `SIGBUS` beyond it. A move settles CoW
  (`kernel/cow`), re-points host memory, moves PTEs, updates SysV shm attach bases (`v.shm`),
  and updates `vdso` when the whole vDSO moves (or clears it on a partial move).
- A move (growing with `MREMAP_MAYMOVE` and no room in place, `MREMAP_FIXED`,
  `MREMAP_DONTUNMAP`) is `ENOMEM` before anything changes unless the address space stays more
  than three VMAs below `vm.max_map_count` (`move_room`, 6.1's `move_vma`), with two more for a
  target's unmapping (`mremap_to`); the moved pieces go back in without the count check
  (`place_vma`), so a move never fails for room halfway.
- `MREMAP_DONTUNMAP` follows 6.1's order: no mapping at the old address `EFAULT`, the target
  `EINVAL`, the margin `ENOMEM`, the target unmapped, the one-VMA rule `EFAULT`; it splits nothing
  before these, and then also needs room for the splits and for one VMA per piece left at the old
  address. What it leaves there: for anonymous memory a new empty VMA, for a private file a
  `Refill` that the syscall maps from the store object (both keeping `may_exec`, dump, fork,
  grows-down, `noreserve`, `anon` and `mnt`), and for shared memory and a `Fill` VMA a copy of the
  old VMA (unlocked; a `Fill` one on its host file mapped privately again); the syscall then calls
  `reap_shm`. `shmat` also refuses non-zero fixed addresses below `vm.mmap_min_addr` without
  `CAP_SYS_RAWIO` (`EPERM`).
- `mremap` with an old length of 0 duplicates (6.1's "duplicate shm area"): the VMA holding the
  address (`EFAULT` if none) must be shared (`EINVAL`; a `MREMAP_FIXED` target is validated and
  unmapped before this check), and without `MREMAP_MAYMOVE` it is `ENOMEM`; no margin is asked. A
  shared file mapping maps the file again from the address's offset for the new length; shared
  anonymous memory and System V segments get a VMA on the same host memory, which may run past the
  source VMA to the end of the region and past that has no contents (`SIGBUS`). A segment's copy is
  a new attach base, so it counts as one more attachment and shmdt at its address detaches it.
- `remap_file_pages`: a non-zero prot is `EINVAL`; start and size are rounded down; the range must
  lie in contiguous shared file pieces of one host file and prot (`EINVAL`; shared anonymous memory
  and segments are refused); it maps the file again with `MAP_SHARED | MAP_FIXED`, keeping the
  label, FUSE page source, exec denial and lock, and populates unless `MAP_NONBLOCK`.
- `msync`: flags and alignment `EINVAL`, the range must be fully mapped (`ENOMEM`), `MS_INVALIDATE`
  on locked pages `EBUSY`; `MS_SYNC` syncs the shared file ranges on the host with the BKL
  released. `mincore`: alignment `EINVAL`, fully mapped or `ENOMEM`; reserved pages report 0 and
  borrowed pages 1; the result is written without growing the stack.
- `mlock`/`mlock2`/`munlock` follow `do_mlock`: the length plus the offset in the page is
  page-aligned with wrapping arithmetic; after mlock's `RLIMIT_MEMLOCK` check (`EPERM` with a limit
  of 0, `ENOMEM` above it; skipped with `CAP_IPC_LOCK`) a range that wraps past the end is `EINVAL`
  and an empty one succeeds; the range must be fully mapped (`ENOMEM`) and is populated via
  `populate_mlock` unless `MLOCK_ONFAULT` (stopping at the first `PROT_NONE` VMA and returning
  `ENOMEM` while leaving the range locked; skipping read-only private anonymous VMAs so RSS stays 0
  and CoW stays intact; settling CoW and atomically write-faulting writable private anonymous VMAs
  so real private host pages are resident). `mlockall(MCL_CURRENT)` checks the whole size (`PROT_NONE` reservations
  included) against the limit, locks every VMA and populates unless `MCL_ONFAULT`; `MCL_FUTURE`
  makes new VMAs locked, uncharged.
- `madvise`: alignment/length/unknown advice (counted as unimplemented) `EINVAL`; length 0 succeeds.
  Flag advice (`DONTDUMP`/`DODUMP`, `WIPEONFORK`/`KEEPONFORK`, `DONTFORK`/`DOFORK`) requires a fully
  mapped range (`ENOMEM`), splits VMAs at ends and sets the flag. Locked (`MADV_DONTNEED` `EINVAL`)
  and private-anon checks (`MADV_FREE`/`MADV_WIPEONFORK` `EINVAL`) inspect overlapping VMAs only.
  Non-destructive hints (`NORMAL`/`RANDOM`/`SEQUENTIAL`/`WILLNEED`/`POPULATE_READ`) forward to
  `appine_hal::advise` unlocked via coalesced `AdviseRange`s clipped to `file_len` (skipping
  reserved/foreign/GPU/`Fill` VMAs; `NORMAL`/`RANDOM`/`SEQUENTIAL` file-backed only; `WILLNEED`/
  `POPULATE_READ` skip untouched anon, pass `MADV_WILLNEED`, and prefill read-only non-FUSE file soft
  PTEs up to 4 GB/call). Other unlisted advice returns 0.
- `MADV_DONTNEED` (`discard`): private anon returns to host and zeroes (CoW settled via `kernel/cow`);
  written private file pages before EOF unshare first if marked `cow` (so reverting never corrupts a
  fork peer), then revert via `kernel/hostmem`'s `discard_file_host` (whole host
  pages) and `Reread`s read from the store (`fill_private`, zeros past EOF or open failure; `Fill`
  partial host pages reread from its memfd so entries stay). Executable ranges queue code
  invalidation; shared/unwritten VMAs untouched; vDSO refills from kernel image (with svc stubs
  when `vdso_svc`).
- `MADV_REMOVE` (`remove`): fully mapped; locked or private anon `EINVAL`, other private or
  never-writable `EACCES` (6.7); shared parts before EOF freed via `kernel/hostmem`'s `remove_host`,
  no code invalidation.

### Stack growth

- `grow_down` refuses pages below `vm.mmap_min_addr` and addresses already mapped. It applies when
  the nearest VMA at or above the address is `growsdown`. It refuses if the single nearest VMA
  below is non-growsdown, accessible and closer than 256 pages (`stack_guard_gap`; a `PROT_NONE` or
  growsdown neighbour is not looked past), or if the new stack size exceeds the current
  `RLIMIT_STACK`. Otherwise it extends into the reserved room below, or maps a new chunk in a host
  allocation of `clamp(size, grow, limit − size)` bytes (exactly the growth if that fails) with the
  chunk at its top, so the VMA count grows only logarithmically; the chunk takes the flags of the
  VMA above it and needs room for one more VMA.
- The kernel grows a stack itself at syscall entry when SP is below the main stack (arm64 has no
  red zone), before writing a signal frame, and when a kernel access to unmapped memory fails
  (`grow_for`, as Linux's fault handler does): the shared syscall helpers (`get`/`put`), read,
  write and readv buffers, getdents64, vmsplice, waitid and FUSE's device ioctls and prefault go
  through `Cx::read_user`/`write_user`, and `/proc/<pid>/mem` accesses grow within the accessor's
  `RLIMIT_STACK`. Other direct `Mm::read`/`write` callers do not grow it.
- `MAP_GROWSDOWN` is honoured only for private anonymous mappings (including private `/dev/zero`);
  `MAP_SHARED | MAP_ANONYMOUS` with it is `EINVAL`, and file or shared `/dev/zero` mappings ignore
  it. An mmap'd growsdown VMA has no reserved room, so its first growth maps a new chunk.

### Files: EOF and truncation

- Whole pages past EOF have no entries. Ordinary shared and private mappings keep the file open
  across descriptor close, VMA splits, fork and mremap. Private mappings share a retained descriptor
  per host inode across a fork family. Guest instruction faults and kernel copies retried through
  `grow_for` check the current extent. If a private mapping has anonymous or shared read-only backing after fork,
  caching or mprotect, only the previously inaccessible tail gets a fresh private host mapping.
- truncate, ftruncate and `O_TRUNC` that shorten a file first call `file_truncated` on every
  address space (entries past the new end are removed, executable ranges are queued for
  invalidation, `dirty` is set; `Fill` VMAs are left alone) and sync each one that changed, the
  current one included (one that cannot be borrowed is skipped), and only then shorten the host
  file: no thread can reach the dropped tail through a stale translation, so an access there faults
  into the kernel and gets the guest's `SIGBUS` once the BKL is free. A host truncation that then
  fails leaves the mappings' EOF at the requested length.
- A named store file mapped shared gets its own backing object before mapping, including
  read-only opens. The VMA retains a duplicate of its locked open description. Splits, fork
  and mremap keep the pin. Clones and overlay copy-up copy bytes when either object is pinned.
  A Files app truncate clips affected address spaces in each running kernel before the host
  shrink; guest faults can then observe EOF or regrowth. Private mappings retain their backing.

### Introspection and teardown

- After guest exec, `exe_open` holds the main program's inotify open record. Fork shares it.
  Non-overlay file-backed interpreter VMAs share their open record across splits and fork.
  Their last unmap releases it. Leaving the address space with no users releases its remaining exec opens.

- `regions` labels each VMA (file name, else `[stack]` for the VMA containing `start_stack` and the
  contiguous same-prot growsdown VMAs below it, else `[vdso]`, else `[vvar]` for the foreign VMA,
  else `[heap]` inside `[brk_start, brk)`) and joins adjacent pieces of one Linux VMA (`same_vma`,
  plus the same label). procfs escapes a newline in a file name as `\012`.
- `dump_regions` applies `coredump_filter` and `MADV_DONTDUMP` (the vDSO is dumped whole anyway; a
  written private file VMA follows `ANON_PRIVATE`; ELF headers' first pages follow their own bit)
  and writes nothing of `[vvar]`; the core-dump writer leaves holes where `residency` reports pages
  absent in regions other than private file mappings.
- `Mm` drop leaves its code group (`kernel/mmu`), releases CoW (`kernel/cow`), defers the page
  table (`kernel/hostmem`), then drops VMAs and graveyard. RSS measures live in `kernel/oom`.

## Dependencies (inbound / outbound)

### Inbound

- `syscall/mem.rs`, `syscall/mod.rs` (stack growth at entry, `take_wanted`), `kernel/fuse` (`Fill`,
  `prefault`), `kernel/process`/`kernel/exec`/`kernel/ns`/`kernel/signal` (execve, setns
  `vdso_syscalls`, faults, `Cx` stack growth, signal frames), `kernel/ptrace` (`write_force`),
  `kernel/futex` (`futex_key`, `translate`, `id`, `shared_maps`), `kernel/ipc` (`attach/detach_shm`),
  `kernel/files`/`kernel/fileio` (`ETXTBSY`, seals, truncation, clones, unsharing), `kernel/procfs`,
  `kernel/oom`, `kernel/mmu` (VMAs/EOF/sync inputs), `kernel/cow`.

### Outbound

- `kernel/cow` (`cow_break`, `cow_settle`, `cow_unmap`/`give_back`, `cow_pages`/`cow_entries`/
  `write_around`, `discard_unlent`, `return_loans`); `kernel/hostmem` (`HostRegion`, `FileMaps`,
  `content_version`, `WRITABLY_SHARED`, `discard_host`/`discard_file_host`/`remove_host`,
  `mmap_gpu`); `kernel/mmu` (PTE builders/movers, `sync`, `quiesce`, `shootdown`, code groups).
- `appine_hal` (`populate`, `fstat`, `msync`, `advise`, `mincore`, memfds,
  `page_size`); `kernel/vdso` (image/svc image, clocks, host vvar); `kernel/fuse` (`Fill::source`);
  `kernel/vfs`/`kernel/fileio`/`kernel/mount`/`kernel/cred`/`kernel/ipc` (`reap_shm`).

## Invariants and gotchas

- Follow `kernel/mmu` (release host memory only after thread flushes; keep PTEs valid at each step
  since `shootdown` does not pause threads; record `dirty`/`graveyard`/`code_ranges` for `sync`),
  `kernel/cow` (never write lent/borrowed pages in place; break/settle CoW before mutating), and
  `kernel/hostmem` (never dereference `RESERVED_HOST`; never discard/unmap `foreign`/`gpu` regions;
  release via `HostRegion` drop).
- A `Fill` VMA has `file_len` 0 and entries only for pages read; a page not read is never written
  anywhere (guest writes need an entry, kernel writes read it first), so storing it into the host
  file shows it to every VMA of the fill, and copies of a `Fill` VMA (fork, `unshare`, mremap)
  map that host file again rather than copying into anonymous memory.
- Every private anonymous page with content has a PTE (except borrowed pages in `PROT_NONE`, which
  `kernel/cow` adds): kernel writes, mlock and `MAP_POPULATE` fill entries, mremap moves them, and
  `PROT_NONE` keeps prot-0 entries (`kernel/cow` lending and `kernel/oom` RSS rely on this).
- `Mm` (`Rc<RefCell>`) is BKL-only (checking builds assert no borrow across BKL release);
  `shared_maps` is shared across threads for the fast path.
- The host's `vm.max_map_count` is shared by the whole app process, so the guest limit is enforced
  per address space.
- VMAs never merge; adjacent anonymous VMAs are separate host mappings and are only joined (as
  `same_vma`) for display, mremap's one-VMA rule, `PROT_GROWSDOWN`, fork's `anon` identities and
  (by its own rule) fork's commit estimate. Code creating a VMA that continues another (stack
  chunks, mremap growth) must copy the flags `same_vma` compares.
- Guest pages are always 4 KB (`AT_PAGESZ` 4096) whatever the host page size.
- Host `mincore` reports pages swapped to zram as absent; RSS (`kernel/oom`), mincore(2), pagemap
  and core-dump holes still use mincore.

## Confirmed bugs / technical debt

- **FUSE mappings read on demand are not Linux's page cache:** a page keeps what was read when first
  touched, so writes through the mount or by another process are not seen and separate mmap calls
  read separately; the size is fixed at mmap (growth not seen, truncation never `SIGBUS`); mlock,
  `MADV_WILLNEED` and core dumps do not read pages (dumps write unread pages as zeros); a kernel
  access to an unread page reruns the whole syscall, so a syscall other than read/write, pread,
  pwrite and the vector forms (whose buffers the dispatcher reads in first) repeats what it did
  before the copy, or returns a short count after a partial transfer.
- **Approximate madvise:** `MADV_DONTNEED_LOCKED`/`MADV_POPULATE_WRITE` are no-ops;
  `MADV_POPULATE_READ` runs advisory host readahead + RO file soft-PTE prefill without faulting or
  failing on inaccessible ranges; `MADV_FREE`/`COLD`/`PAGEOUT` are not refused on locked ranges.
- **Approximate Linux VMA boundaries:** anonymous pieces join ignoring page offsets (mremap-moved
  VMAs join new neighbours); `MREMAP_DONTUNMAP` of a multi-piece VMA needs one VMA slot per piece,
  and in-place growth adds a VMA.
- **Ranges with gaps fail as a whole:** madvise, msync, mlock and mincore refuse a range that is not
  fully mapped before doing anything (`ENOMEM`), where Linux acts on the mapped parts first.

## Open questions

- Why is `STACK_TOP` `1 << 47` while the page table covers 48 bits and `MAP_FIXED` may reach
  `1 << 48`?
- Should mappings locked through `mlockall(MCL_FUTURE)` be charged against `RLIMIT_MEMLOCK`?
- Is the window in `mmap` and `shmat` between installing entries and `note_shared_maps` acceptable
  for a concurrent fast-path futex on a writable shared page?
