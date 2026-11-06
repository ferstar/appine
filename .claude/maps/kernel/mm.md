# kernel/mm Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/kernel/mm.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 6 research concerns; 28 claims checked by 3 independent verifiers — 16 confirmed, 11 partial and 1 refuted, all corrected below; 14 bugs or debts recorded)
>
> Maintained: 2026-11-07 (targeted verification only since the full mapping: `MADV_WIPEONFORK`/`MADV_DONTFORK` at fork and mremap growth, madvise judging only overlapping VMAs; the OOM killer; mremap with an old length of 0)

## Responsibilities

`kernel/mm` is the guest memory subsystem: a lightweight user-mode soft-MMU. It does not emulate
ARM hardware translation (no TTBR, descriptor formats, TLBI/ASID, exception levels or physical
address layer), but it implements Linux mm semantics for every guest process:

- one address space (`Mm`) per guest process: a VMA table plus a software page table;
- host memory behind every VMA, including sharing, recycling and deferred release;
- demand population of page-table entries and the guest page-fault policy;
- multi-thread consistency (TLB shootdown) and the code-change log that Tier 1 translations consume;
- `fork` with page-granular copy-on-write implemented only in the soft page tables;
- read-only views for procfs, core dumps, rusage, cgroups and the OOM killer;
- the OOM killer, which keeps the app under a memory limit by ending guest processes, and the
  periodic release of Tier 1 code written through the writable view.

Root:

- `core/kernel/src/mm.rs` — almost everything (address space, host backing, CoW, code groups,
  accounting, unit tests).
- `core/kernel/src/syscall/mem.rs` — argument validation for mmap, mremap, remap_file_pages,
  msync, munmap, mprotect, madvise, mlock/mlock2/munlock, mlockall/munlockall, mincore.
- `core/engine/src/c/mmu.c` and the `Mmu` wrapper in `core/engine/src/lib.rs` — the radix page table.
- `core/engine/src/c/cpu.c` and `core/engine/src/c/internal.h` — the per-thread software TLB and
  Tier 0 refill.
- The page-fault branch of the task run loop in `core/kernel/src/task.rs`, and the stack-growth
  helpers on `Cx` in `core/kernel/src/kernel.rs`.
- `core/kernel/src/oom.rs` — the OOM killer and the release of written code views, run from the
  timer thread.

Outside the module (dependencies only): the CPU engine and the Tier 1 translator (`dbt.c`,
including its inline TLB fast path and refill), SysV IPC objects (`kernel/ipc`), futexes, the ELF
loader, the timer thread (`kernel/process`), procfs and the cgroup files (`kernel/procfs`), core
dumps and the host abstraction layer (`core/hal`).

## Key types

### Address space

- `Mm` — one guest address space. Holds the page table (`mmu`, dropped first), the VMA table
  (`vmas: BTreeMap<start, Vma>`), the `graveyard` of host regions awaiting shootdown, brk and
  layout fields (`brk_start`, `brk`, `mmap_top`, `start_stack`, `stack_low`), the consistency
  counters, CoW state, the code-group membership, and fields consumed elsewhere (`exe`, `dbt_regs`,
  `vdso`, `args`, `env`, `auxv`, `membarrier`, `lock_future`, `lock_onfault`). Used only under the
  BKL, as `Rc<RefCell<Mm>>`.
- `Vma` — one mapping: length, guest prot, `Backing`, `may_write`/`may_exec` (Linux
  `VM_MAYWRITE`/`VM_MAYEXEC`), `Arc<HostRegion>` plus host pointer, `name`/`ino` for maps,
  `file_len` (bytes before EOF), `file_id`/`content` (file identity and version), a dup'd fd for
  shared file mappings, `shm`, and flags `cow`, `locked`, `written` (sticky), `growsdown`,
  `dontdump`, `wipeonfork`, `dontfork`, `noreserve`.
- `Backing` — `Anonymous` (private anonymous), `PrivateFile` (private file mapping), `Shared`
  (shared file, shared anonymous, `/dev/zero` or SysV shm).
- `flags` — guest ABI values of the `MAP_*` flags the mm interprets.
- `Refill` — the private-file piece re-mapped at the old address after `MREMAP_DONTUNMAP`.

### Host backing

- `HostRegion` — owns one host range (`base`, `len`). `anonymous: Some(reserve)` marks a range that
  is recycled on drop. `reserved` marks a placeholder with no host memory, whose base is the fake
  address `RESERVED_HOST` and is never dereferenced.
- `recycled` — process-wide reuse pool of anonymous host ranges keyed by `(len, reserve)`: at most
  256 MB in total and 32 ranges per key.
- `spare` — pre-mapped `MAP_NORESERVE` host pieces that `fork` takes while holding the BKL: two
  per length, at most eight lengths (least recently wanted evicted).
- `Carved` — small child mappings carved from one spare region at fork.
- `Unmapped` / `UNMAPPED` / `UNMAPPING` — thread-local list of host ranges and page tables whose
  release is deferred while the BKL is held, and a global byte count of what is still pending.
- `FileMaps` — kernel-wide LRU cache (at most 128 entries) of read-only private file mappings,
  shared by all address spaces, keyed by device, inode, offset, length, ctime and size.
- `SharedMemory` — host backing of a SysV shm segment.

### Translation

- `struct appine_mmu` / `Mmu` — 4-level radix tree, 512 entries per level, covering 48-bit guest
  addresses; each leaf table spans 2 MB. A leaf entry is a 4 KB-aligned host page address ORed with
  R/W/X bits; 0 means absent, and prot 0 with a nonzero address means "present, no access". `Mmu`
  is neither `Send` nor `Sync`; `map`/`unmap`/`protect`/`move_entries` take `&mut`, while
  `fill_shared`/`map_shared`/`protect_shared` take `&self` so CoW can edit another address space's
  table.
- `struct appine_tlb_entry` — data TLB entry `{read tag, read addend, write tag, write addend}`,
  4096 direct-mapped entries per thread; tag is the inverted page number and 0 is invalid, so
  zeroed memory is an empty TLB. A `tlb_used` bitmap records filled slots.
- `struct appine_itlb_entry` — 512-entry instruction TLB, used only by Tier 0 instruction fetch.
- `UserSlice` — an unchecked guest range passed to kernel-side accessors.

### Copy-on-write

- `Loan` — one lent host page: its owner (`None` for an orphan) and its borrowers.
- `Loans` — `HashMap<host page address, Loan>` shared by a whole fork family (execve starts a fresh
  one), hashed with `PageHasher`.
- `Cow` — per-mm sets of guest pages it has `lent` and has `borrowed`; `Borrowed` keeps the owner's
  `HostRegion` alive.
- `Runs` — coalesces consecutive pages into runs for PTE updates; a run never crosses a VMA.
- `MmRef` — weak self-handle (`me`) recorded in loans.

### Consistency and code

- `TaskSync` — per-thread `in_guest` flag plus a `Kicker`, shared without the BKL.
- `Generations` — clones of the three counters (`tlb`, `code`, `space`) and the code log; the run
  loop reads them without the BKL.
- `CodeLog` — log of `(generation, start, len)` code changes covering the last 4096 code
  generations.
- `CodeSpace` — the Tier 1 translation space (`DbtSpace`) used by an `Mm`, with its tag and the last
  code generation applied.
- `CodeGroup` / `CodeGroups` — groups of address spaces running the same executable that share one
  `DbtSpace` (at most 256 registered trusted ranges); the kernel keeps a list of weak references.

### Introspection

- `Region` — one `/proc/<pid>/maps` / `smaps` line.
- `RssPlan`, `FileResidency` — one RSS sample plan, and a 1 s, 1024-entry cache of file-mapping
  residency.
- `DumpRegion`, `dump_filter` — core-dump segments and the `coredump_filter` bits (default `0x33`).

### OOM killer

- `Oom` (`Kernel::oom`) — the app-wide limit (`u64::MAX` is none), the root's peak, the next check
  deadline (`None` while every task sleeps), the last victim and when it was picked, and the next
  code-view release.
- `Events` — a cgroup's `memory.events` counters (`max`, `oom`, `oom_kill`, `oom_group_kill`),
  kept hierarchical and local.
- Intervals: 20 ms, 5 ms above 3/4 of a limit, at least 20 times the measurement's own time; 1 s
  victim timeout and code-view interval.

## Public entry points

- **Syscalls** (`syscall/mem.rs` → `Mm`): `mmap`, `munmap`, `mprotect`, `mremap` (including
  `MREMAP_DONTUNMAP` via `mremap_dontunmap`), `remap_file_pages` (emulated as a shared fixed
  mmap), `msync`, `madvise`, `mlock`/`mlock2`/`munlock`, `mlockall`/`munlockall`, `mincore`.
  `brk` calls `Mm::brk` directly.
- **execve / loader**: `Mm::new`, `share_files`, `set_layout`, `map_anonymous`, `find_free`
  (interpreter and vDSO placement), `set_brk`, `map_stack`, `set_start_stack`, `label`,
  `write_kernel` (segments, `.bss`), plus the public layout fields.
- **Fault path** (task run loop): `cow_break`, `fault_in`, `fault_past_eof`, `Cx::grow_stack`
  (→ `grow_down`), `is_mapped`.
- **Stack growth from the kernel**: `Cx::grow_stack_to` at syscall entry; `Cx::grow_stack` before
  building a signal frame.
- **Kernel access to guest memory**: `read`, `write` (breaks CoW per page), `translate` (does not
  break CoW), `read_cstr`, `write_kernel`, `write_force` (ptrace and `/proc/<pid>/mem` writes),
  `check_mapped`, `populate`.
- **Threads and processes**: `add_user`, `remove_user`, `sync` (after every BKL syscall),
  `quiesce`, `generations`, `code_space`, `invalidate_code`, `fork`, `futex_key`, `shared_files`.
- **Files**: `file_truncated` (truncate, ftruncate, `O_TRUNC`), `writably_maps` (`F_SEAL_WRITE`),
  `shared_file_ranges` (msync), `content_version`.
- **SysV shm**: `SharedMemory::new`, `attach_shm`, `detach_shm`, `shm_attachments`.
- **Introspection**: `regions`, `resident_in`, `cow_shared`, `residency`, `pagemap`, `size`,
  `resident_pages`, `peak_resident_pages`, `locked_bytes`, `locked_in`, `dump_regions`, `dump_page`.
- **BKL hooks**: `defer_unmaps` when the BKL is taken; `release_unmapped`, then `spare::refill`
  when it is released.
- **Machine**: `FileMaps::clear` from `Machine::trim_memory`.
- **OOM killer**: `oom::check` and `oom::unmap_code_views` (timer thread), `out_of_memory` (also
  from a lowered cgroup `memory.max`), `Oom::{measure, recheck}`, `default_limit`, `app_memory`;
  `Shared::resume_checks` from the run loop.

## Data flow / lifecycle

### Creation and layout

- execve builds a fresh `Mm` (with a fresh `Loans` table) that shares the kernel-wide `FileMaps`.
- `set_layout` sets `mmap_top` from the `RLIMIT_STACK` at execve: `STACK_TOP` minus the limit
  plus the stack guard gap, clamped between 1 GB + 8 MB and 5/6 of the address space. An unlimited
  stack still gives a top-down layout with the largest gap.
- `STACK_TOP` is `1 << 47`. A position-independent main program loads at `0x5555_5555_0000`.
  Non-fixed `mmap` allocates top-down from `mmap_top` (`find_free`), with a floor of `0x10000`.
- `map_stack` maps the arguments and auxv plus 128 KB at the top of one host region of
  `max(min(page-aligned RLIMIT_STACK, 8 MB), mapped length)`, leaving the rest below as reserved
  growth room.

### mmap and host backing

- Lengths round up to a page; zero is `EINVAL`.
- `MAP_FIXED` / `MAP_FIXED_NOREPLACE`: the address must be aligned and the end at most `1 << 48`;
  there is no lower bound. `NOREPLACE` on an occupied range is `EEXIST`; otherwise the old range is
  unmapped first, before host allocation and the map-count check.
- A hint is rounded down to a page and used only if it is at least `0x10000`, ends at or below
  `mmap_top`, and is free.
- Host backing by kind:
  - private anonymous `PROT_NONE` → a `reserved` `HostRegion` (no host address space);
  - other private anonymous → the `recycled` pool, else a new host anonymous mapping;
  - read-only private file with a trusted content version → a `FileMaps` hit (VMA marked `cow`),
    or a new read-only host mapping inserted into the cache;
  - other file mappings → host `mmap` of `objects/<ino>` (read-write for private mappings);
  - shared anonymous and SysV shm → host shared memory.
- A file has no trusted content version while its ctime is within the last 2 s, or once it has been
  mapped shared from a writably opened descriptor (`WRITABLY_SHARED`, never cleared).
- Shared file mappings map the same host object file with host `MAP_SHARED`, so they stay
  coherent with `read`/`write`, which go to that file through the host page cache.
- On a 16 KB-page host, a file mapping covers the 16 KB-aligned host range around the target and
  points the VMA at the right 4 KB offset inside it.
- Host commit accounting: private anonymous memory is requested as accounted only when the guest
  mapping is writable and not `MAP_NORESERVE`; writable shared anonymous memory is accounted even
  with `MAP_NORESERVE`; SysV shm is always accounted. The main stack, stack chunks, the heap
  reservation and fork's spare and carved pieces are always host `MAP_NORESERVE`.
- `insert` enforces `vm.max_map_count` (default 65530, writable through procfs), builds page-table
  entries (section 3), and sets `locked` from `mlockall(MCL_FUTURE)`.

### Page-table population and the TLB

- `map_pages` builds entries eagerly when the part of the VMA before EOF is at most 1 MB, or when
  the mapping is executable and not private anonymous (file mappings, shared anonymous, SysV shm).
  `PROT_NONE` mappings get none.
- Larger mappings are filled on demand by `fault_in`. It fails if no VMA covers the page, the page
  is at or past EOF, the VMA prot (with write implying read) lacks the access, or the page already
  has an entry (including a prot-0 one). Otherwise it fills every empty slot of the 2 MB leaf range
  clipped to the VMA and EOF; existing entries are left alone and no flush is needed.
- `mprotect` to `PROT_NONE` keeps entries with prot 0 (the host page keeps its content); changing
  back rewrites the kept entries and fills the rest under the eager rule above, otherwise on demand.
- Execute-only mappings (`PROT_EXEC` without `PROT_READ`) do not gain read permission: data loads
  and kernel reads fault, instruction fetch works.
- Tier 0 looks up the data TLB in C and calls `appine_tlb_fill` on a miss. The fill fails if the
  entry is absent or lacks the needed bit. A data fill sets both halves at once, leaving the read
  half invalid on pages with a load watchpoint and the write half invalid on pages with any
  watchpoint; it records the slot in `tlb_used` and stores `addend = host page − guest page base`.
  Exec fills only the ITLB. Tier 1's refill defers to Tier 0 whenever any watchpoint is set.
- Cross-page accesses of up to 16 bytes look up both pages before copying anything; the reported
  fault address is the start of the second page.
- A TLB flush clears only the slots marked in `tlb_used`, plus the whole ITLB.

### Guest page faults (task run loop, under the BKL, in this order)

1. A write fault where `cow_break` succeeds → re-execute.
2. The page now translates (another thread fixed it while this one waited for the BKL) → re-execute.
3. `fault_in` succeeds → re-execute.
4. `fault_past_eof`: past EOF in a shared file mapping whose file has grown → map the new part and
   re-execute; otherwise past EOF → `SIGBUS` / `BUS_ADRERR`.
5. Stack growth (`grow_down`) succeeds → re-execute.
6. Otherwise `SIGSEGV`: `SEGV_ACCERR` if the address is mapped, else `SEGV_MAPERR`.

"Re-execute" forces no signal and leaves the PC on the faulting instruction (already-pending
signals may still be delivered).

### munmap, mprotect, mremap

- VMAs are only ever split (`split_at` adjusts file offsets and host pointers), never merged.
- `munmap` moves removed host regions to the `graveyard`, sets `dirty`, and records executable
  ranges in `code_ranges`.
- `mprotect` requires the range to be fully mapped (`ENOMEM`) and the `may_write`/`may_exec`
  permissions (`EACCES`). A `cow` VMA is copied (`unshare`, which sets `dirty`) before it becomes
  writable, and a `reserved` region gets real host memory (`back_reserved`) before it becomes
  accessible. Otherwise only a permission downgrade sets `dirty`; a range containing fork CoW pages
  gets an immediate `shootdown` either way.
- `mremap` may span several contiguous VMAs. Growing in place needs the old end to be a VMA end,
  free space above, and the new end at or below `mmap_top`; the grown part becomes a separate VMA
  with the same lock, dump and fork flags. A move re-points the same host memory and moves the
  page-table entries with it.
- `mremap` with an old length of 0 duplicates (6.1's "duplicate shm area"): the VMA holding the
  address (`EFAULT` if none) must be shared (`EINVAL`; a `MREMAP_FIXED` target is validated and
  unmapped before this check), and without `MREMAP_MAYMOVE` it is `ENOMEM`. A shared file mapping
  maps the file again from the address's offset for the new length; shared anonymous memory and
  System V segments get a VMA on the same host memory, which may run past the source VMA to the
  end of the region and past that has no contents (`SIGBUS`). A segment's copy is a new attach
  base, so it counts as one more attachment and shmdt at its address detaches it.
- `madvise` flag advice (`DONTDUMP`/`DODUMP`, `WIPEONFORK`/`KEEPONFORK`, `DONTFORK`/`DOFORK`)
  needs the range fully mapped, splits the VMAs at its ends and sets the flag. Whether the range is
  locked (`MADV_DONTNEED` is `EINVAL`) or all private anonymous (`MADV_FREE` and `MADV_WIPEONFORK`
  otherwise `EINVAL`) is judged over the VMAs overlapping it only.

### Stack and heap growth

- `grow_down` refuses pages below `0x10000` and addresses already mapped. It applies when the
  nearest VMA at or above the address is `growsdown`. It refuses if the single nearest VMA below is
  non-growsdown, accessible and closer than 256 pages (`stack_guard_gap`; a `PROT_NONE` or
  growsdown neighbour is not looked past), or if the new stack size exceeds the current
  `RLIMIT_STACK`. Otherwise it extends into the reserved room below, or maps a new chunk reserving
  `clamp(size, grow, limit − size)` below it (exactly the growth if that host allocation fails), so
  the VMA count grows only logarithmically.
- The kernel grows the stack itself in only two places: at syscall entry when SP is below the stack
  (arm64 has no red zone), and before writing a signal frame. Other kernel accesses below SP, and
  `/proc/<pid>/mem` accesses, never grow it.
- `MAP_GROWSDOWN` is honoured only for private anonymous mappings (including private `/dev/zero`);
  `MAP_SHARED | MAP_ANONYMOUS` with it is `EINVAL`, and file or shared `/dev/zero` mappings ignore
  it. An mmap'd growsdown VMA has no reserved room, so its first growth maps a new chunk.
- `brk` first grows in place inside the heap's host region, then at the first heap page reserves
  256 MB (`MAP_NORESERVE`), then falls back to a separate anonymous mapping. Shrinking unmaps.
  There is no `RLIMIT_DATA` or `RLIMIT_AS` enforcement.

### Consistency: shootdown and code invalidation

- Mutators run under the BKL and only record what changed: `dirty` (unmap, permission downgrade,
  PTE re-pointing, truncation, leaving a code group), `graveyard`, `code_ranges`, and
  `exec_changed`.
- `Mm::sync` runs after every syscall that takes the BKL (the no-BKL fast path never changes the
  mm). It re-checks the code group, publishes `code_ranges` to the `CodeLog` and bumps the code
  generation, and returns if nothing is dirty and the graveyard is empty. Otherwise it bumps the TLB
  generation (when dirty), runs `quiesce`, and clears the graveyard.
- `quiesce` kicks every other user of the mm and spins until none is in guest code. The kick is
  sticky, so a thread returning from a no-BKL fast syscall stops at its next guest entry.
- `shootdown` is the immediate form used when a PTE is re-pointed (CoW copy, return of a borrowed
  page, mprotect over CoW pages): it bumps the TLB generation, kicks only users in guest code, and
  waits.
- The run loop sets `in_guest`, then reads the counters (a SeqCst store-then-load pairing with the
  mutator's bump-then-read). If `space` changed it re-attaches to a new `CodeSpace`; if `tlb`
  changed it flushes its TLB; if `code` changed it invalidates Tier 1 blocks for the ranges it
  missed, or flushes all translations when the log no longer covers them.
- Code-only changes kick nobody; other threads see them on their next guest entry, and
  `membarrier(SYNC_CORE)` (which quiesces) is the way to force it. Tier 1 handles `ic ivau` inside
  the translator; the log carries mapping changes, Tier 0 invalidations, and leftover `ivau` pages.

### Code groups (mm side of translation sharing)

- The first thread to run an `Mm` calls `code_space`: it joins an open group for the same
  executable if `admit` accepts its executable layout and a tag is free, otherwise it founds a new
  group with its own `DbtSpace`.
- After any executable change, `sync` re-runs `admit`. A member that no longer fits becomes
  `unshareable` (sticky): a sole member closes the group and keeps the space; otherwise it leaves,
  drops its `CodeSpace`, bumps the space generation and forces a shootdown. Unmapping an executable
  VMA does not count as an executable change.
- A forked child starts outside any group and may join its parent's. On drop a member leaves the
  group and its claims are released.

### fork and copy-on-write

- The caller quiesces the parent, then `fork` checks commit (`commit_too_big`: counts private
  writable non-`MAP_NORESERVE` VMAs the child gets, by guest prot, merging adjacent ones the way
  Linux `vma_merge` would; `ENOMEM` when one exceeds RAM plus swap; a limit of 0 disables it).
  `fork` cannot fail after lending starts.
- Per VMA (`MADV_DONTFORK` VMAs are skipped; the child's VMAs keep `dontdump` and `wipeonfork`):
  - shared mappings, reservations and read-only private file mappings share the parent's region;
    read-only private file VMAs are marked `cow` on both sides;
  - non-growsdown, non-reserved, non-heap anonymous or writable private-file VMAs of at most 1 MB
    are carved from one spare region;
  - the heap (the anonymous VMA at the page-aligned `brk_start`, at most 256 MB) gets a spare region
    of 256 MB; a growsdown anonymous VMA with reserved room below gets a spare region of room plus
    length; everything else gets new anonymous memory;
  - private anonymous VMAs that were ever written are lent, except `MADV_WIPEONFORK` ones, where
    the child keeps its own all-zero memory; never-written ones need nothing;
  - writable private file VMAs are copied page by page up to EOF, skipping all-zero pages.
- Lending: VMAs of at most 16 pages lend every page. Larger ones read the host
  `/proc/self/pagemap` only over 2 MB leaf windows that have entries or CoW pages (merged up to
  1 GB); untouched pages are known zero and the child keeps its own. If pagemap is unreadable, every
  page in the windows is lent. Pages the parent itself borrowed are re-lent from their owner. Parent
  and child get read-only entries for the lent pages (none for `PROT_NONE` VMAs; a later mprotect
  rebuilds them).
- Borrower write (`cow_break`): only for a writable private anonymous VMA whose page is borrowed or
  lent. A borrowed page is copied into the VMA's own slot, re-mapped with the VMA prot, the
  borrower's threads are shot down, and the loan is returned.
- Owner write, munmap, mremap move or `MADV_DONTNEED` of a lent page (`recall`): every borrower
  copies first (interrupting its guest threads), then the owner gets write permission back.
- Borrower munmap returns borrowed pages without copying; borrower mremap and `MADV_DONTNEED`
  copy first (`cow_settle`).
- When the last borrower returns a page, the owner's entries are restored at once (`repossess`),
  in runs that never cross VMAs, without a flush.
- mprotect over CoW pages stops other threads first, so none can write into a lent page between
  granting and removing write permission.
- A futex on a borrowed page copies the page first, because private futex keys are host addresses.
- Kernel writes: `Mm::write` breaks CoW per page; `write_kernel` and `write_force` settle CoW
  (`cow_settle`) and ignore VMA permissions. No-BKL fast-path writes go through the CPU's
  `translate(WRITE)` and fall back to the BKL path on failure.
- Owner destruction (exit, exec): borrowed pages are returned. If it has lent pages, their loans
  become orphans held by the borrowers' `Arc`, and every non-lent page of its written private
  anonymous VMAs is discarded. Lent or borrowed pages are never written in place, even by a sole
  orphan borrower.

### Release of host memory

- Regions stay in the `graveyard` until `sync` has quiesced the other threads, so guest data races
  never touch memory already returned to the host.
- A `HostRegion` dropped while the BKL is held is queued in `UNMAPPED` and counted in `UNMAPPING`.
  When the BKL is released, page tables are freed first, anonymous ranges go back to `recycled` one
  at a time (kept after host `MADV_DONTNEED` within the caps, otherwise unmapped), file ranges are
  sorted and unmapped in coalesced runs, and then `spare::refill` tops the fork pool back up.
- Under the BKL, fork still calls host mmap when a needed length is missing from both the spare
  and recycled pools (and for private mappings over 1 MB or growsdown VMAs without reserved room
  when `recycled` has no match), and still unmaps inline the pieces of a length evicted from the
  spare pool.
- `Mm` drop releases CoW state, then the page table (deferred the same way), then VMAs and the
  graveyard.
- The OOM killer skips a round while `UNMAPPING` is nonzero.

### Files: EOF and truncation

- Whole pages past EOF have no entries. A shared mapping whose file grew picks up the new content on
  the next fault; private mappings do not.
- truncate, ftruncate and `O_TRUNC` first shorten the host file, then call `file_truncated` on
  every address space: entries past the new end are removed, executable ranges are queued for
  invalidation, `dirty` is set; other address spaces are synced immediately and the current one at
  syscall exit.

### Introspection

- `regions` labels each VMA (file name, else `[stack]` for the VMA containing `start_stack` and the
  contiguous same-prot growsdown VMAs below it, else `[vdso]`, else `[heap]` inside
  `[brk_start, brk)`) and joins adjacent entries for display when both are private anonymous or the
  same file at contiguous offsets, and prot, shared, locked, may_write, the dump and fork flags and
  name all match (may_exec is not compared). Shared anonymous entries are never joined.
- Residency always comes from host `mincore` on host pointers, never from guest addresses. Borrowed
  pages always count as resident; reserved VMAs never do.
- `resident_pages` (VmRSS, statm, cgroup, OOM victim choice) skips never-written private anonymous
  VMAs, measures private anonymous VMAs over 1 MB only over windows with entries, and uses the 1 s
  file-residency cache for unwritten private file mappings and all shared mappings. `resident_in`
  (smaps, OOM kill log) uses the same windows but no skip and no cache.
- `peak_resident_pages` feeds `ru_maxrss` and VmHWM, assuming spans totalling at most 256 pages
  are resident instead of measuring them; it is sampled at last-thread exit, before execve, and on
  getrusage.
- `pagemap` marks resident private anonymous pages present and exclusive, borrowed pages present
  only, file pages present and file-backed, and reports no frame numbers.
- `dump_regions` applies `coredump_filter` and `MADV_DONTDUMP`; the core-dump writer leaves holes
  where `residency` reports pages absent.

### OOM killer

- All guest memory is host memory of the one app process, and Android's low-memory killer would end
  the whole app, so the OOM killer keeps the app under its own limit by ending guest processes.
  Guest accesses to host-backed pages never reach the kernel layer, so it polls.
- The limit is set once at start from `MachineOptions.memory_limit` (the CLI's `--memory-limit`,
  `none` for no limit; the Android host passes none, meaning half of the host's total RAM, or no
  limit if that reads 0). The guest cannot change it (the root's `memory.max` is `EPERM`); the
  kernel turns it off for good if the usage cannot be read or the check panics.
- The root (whole-app) usage is the app process's host RssAnon + RssShmem + VmSwap (VmRSS and no
  shmem on kernels without RssAnon), minus Tier 1 code written through the writable view and not
  yet unmapped: it includes translations and kernel-layer metadata and excludes host file pages.
  A child cgroup with a numeric `memory.max` uses the sum of its processes' `resident_pages`.
- `check` runs under the BKL each time the timer thread wakes. With no root limit and no cgroup
  limit it does nothing; otherwise it measures every 20 ms, every 5 ms once a scope is above 3/4 of
  its limit, and never more often than 20 times the measurement's own time. The root is measured
  first and cgroups stop at the first scope over its limit, so one scope is handled per round. A
  `memory.max` write forces an immediate round.
- When every task is sleeping, `check` sets `checks_idle` and stops; every guest thread calls
  `resume_checks` at the top of each run-loop iteration, which signals the timer thread if the flag
  was set.
- An over-limit round is skipped while `UNMAPPING` is nonzero or the last victim, picked under 1 s
  ago, still has threads; otherwise it records a `max` event and calls `out_of_memory`. Lowering a
  child's `memory.max` below its usage calls `out_of_memory` once at once, without a `max` event.
- `select` considers every process except tgid 1 (the built-in init), `oom_score_adj` -1000,
  processes outside the over-limit cgroup, zombies, and the last victim while it is still recorded
  and has outlived its second. The score is `max(1, resident_pages + adj * total / 1000)` with
  `total` the larger of RAM and the limit for the root, the cgroup's limit otherwise; a strictly
  greater score wins, so ties go to the lowest tgid. With no eligible process nothing is killed or
  logged.
- The kill records `oom` on the domain and the victim; a victim already exiting is only waited for.
  Otherwise the group gets `SIGKILL` (`group_exit`), its cgroup and ancestors count `oom_kill`, host
  stderr gets a line, the kernel log gets Linux's memcg lines (`memory: usage …`,
  `oom-kill:constraint=CONSTRAINT_MEMCG,…`, `Memory cgroup out of memory: Killed process …`; the
  app-wide limit appears as a memcg OOM), and the app gets a `KillReason::OutOfMemory` report. With
  `memory.oom.group` on the victim's cgroup or an ancestor up to the domain, every other live
  process of the topmost such cgroup except init and -1000 ones is killed too; the victim is logged
  and counted twice, as Linux's `mem_cgroup_scan_tasks` does.
- `unmap_code_views` runs on the timer thread whether or not there is a limit: at once after a
  resume from idle, then once a second while any task runs, and once more when everything is idle,
  it calls `appine_engine::unmap_written_code`, because translation-cache pages written through the
  writable W^X view count twice in RssShmem. It is timed rather than continuous because each unmap
  makes the next write to every such page fault again.

## Dependencies (inbound / outbound)

### Inbound

- Syscall dispatch: `syscall/mem.rs`, and `syscall/mod.rs` (`sync` after every BKL syscall, stack
  growth at entry).
- Processes: `syscall/proc.rs` (clone/fork, execve), `task.rs` (run loop, fault dispatch, users),
  `kernel.rs` (`Cx` stack growth, BKL hooks), `signal.rs` (signal frames).
- Loader (`loader.rs`), ptrace (`write_force`), futexes (`futex_key`, `translate`, `shared_files`
  on the fast path), membarrier (`quiesce`), SysV IPC, `syscall/fs.rs` (truncation, `ETXTBSY` via
  `exe`, seals).
- Observers: procfs (`pseudo.rs`), core dumps, rusage, cgroups.
- `Machine` (initial `FileMaps`, `trim_memory`, the memory limit at start).
- OOM killer: the timer thread (`check`, `unmap_code_views`), the run loop (`resume_checks`), the
  cgroup files (`measure`, `limit`, peak, `app_memory`, `Events`, `out_of_memory` for a lowered
  `memory.max`), and the write path's `recheck`.

### Outbound

- `appine_engine`: `Mmu`, `Cpu::tlb_flush`, `Kicker`, `DbtSpace`/`Dbt` (trust, share,
  retire_outside, set_sources, invalidate, flush).
- `appine_hal`: host `mmap`/`munmap`/`madvise` wrappers, file mapping with host-page alignment,
  `fstat`, `msync`, `mincore`-based residency, pagemap reads (`touched_pages`), `ram_and_swap`,
  `page_size`.
- vfs (file kinds, mount flags, seals), credentials (`CAP_IPC_LOCK`), rlimits.
- OOM killer: `appine_hal` (`memory_usage` from `/proc/self/status`, `memory_info`),
  `appine_engine` (`written_code_bytes`, `unmap_written_code`), cgroups (`memory_limits`,
  `measure_memory`, `memory_event`, `oom_group`), `signal::group_exit`, the kernel log (`printk`)
  and kill reports (`bug::record_kill`).
- The Tier 1 translator depends on the page-table geometry, the PTE bit layout, the TLB entry
  layout and offsets, `tlb_used`, and the watchpoint count.

## Invariants and gotchas

- Only the kernel layer modifies page tables, serialised by the BKL. Other threads may walk them
  concurrently: entries are read and written atomically, and no table (intermediate or leaf) is
  freed before the page table itself is destroyed, which is deferred until the BKL is released.
- After an unmap, a permission downgrade or re-pointing an entry, every other thread of the address
  space must flush before the old host memory is released. Adding permissions never requires a
  flush: a stale TLB half only misses.
- Every private anonymous page with content has a page-table entry. Kernel-side writes, mlock and
  `MAP_POPULATE` fill entries, mremap moves them, and `PROT_NONE` keeps them with prot 0. fork's
  lending and RSS measurement rely on this.
- Lent and borrowed pages are never writable in place. `Mm::translate` does not break CoW, so any
  kernel code that writes guest memory through it must handle CoW pages itself.
- `Mm`, `Loans`, `Cow` and `CodeGroup` use `Rc`/`RefCell` and are correct only under the BKL; every
  BKL release asserts that no such borrow is held. The counters, `CodeLog`, `shared_files`,
  `TaskSync` and `CodeSpace` are shared across threads.
- CoW never mutably borrows two address spaces at once: borrower-side loan returns run inside the
  borrower's own operation or drop, and owner-side `recall` only takes shared borrows of borrowers.
  The `try_borrow` guard in `return_loans` is therefore defensive (see the latent hazard below).
- Kicking a thread is safe only because a thread removes itself from `users` under the BKL before
  its CPU state is destroyed.
- The host's `vm.max_map_count` is shared by the whole app process, so the guest limit is enforced
  per address space.
- VMAs never merge; adjacent anonymous VMAs are separate host mappings and are only joined for
  display and for fork's commit estimate.
- Guest pages are always 4 KB (`AT_PAGESZ` 4096) whatever the host page size.
- Every guest mapping except a never-accessible `PROT_NONE` reservation is a host mapping of the
  same size, and all guest processes live in the one app process. On hosts with a 39-bit user
  address space (512 GB, common 4 KB-page GKI kernels) they must all fit in it: large writable
  mappings such as ASan's shadow memory cannot be mapped there, and because the guest address
  space is not shrunk to match, programs that need fixed high addresses (ASan's allocator) do not
  work.
- Host `mincore` reports pages swapped to zram as absent, which is why fork's touched-page check
  reads pagemap instead; RSS, smaps, mincore(2) and core-dump holes still use mincore.
- Page-table memory only grows over an address space's lifetime.
- The OOM killer uses two measures: the root counts the whole app process (translations, metadata,
  swap, no file pages), while cgroups and victim scores count guest resident pages (file pages
  included, swap excluded, shared pages once per process).
- The OOM killer polls, so a process that allocates and exits within one interval is never seen,
  and under Tier 1 a guest can touch gigabytes between polls: the limit needs headroom.
- `failcnt` in the OOM log is the domain's hierarchical `max` count, including its descendants'.

## Confirmed bugs / technical debt

- **Some madvise advice is ignored:** `MADV_DONTNEED_LOCKED` and `MADV_POPULATE_*` are accepted
  but do nothing.
- **`MADV_DONTNEED` on private file mappings is a no-op:** written private pages keep their data
  instead of reverting to the file. `MADV_REMOVE` zero-fills through the host mapping rather than
  punching a hole, so storage is not freed.
- **Past-EOF faults ignore permissions:** a `PROT_NONE` or write-to-read-only access past EOF gets
  `SIGBUS` instead of `SIGSEGV`/`SEGV_ACCERR`, and private file mappings never see file growth.
- **Truncation race can kill the process or the app:** the host file is shortened before page-table
  entries are removed and other threads are quiesced, so a concurrent guest access to the dropped
  tail takes a host `SIGBUS` — the guest process is killed as an internal error (or the app aborts
  when the fault guard is off) instead of receiving a catchable `SIGBUS`.
- **`mlock` arithmetic overflow:** the span is computed before validation, so a length near
  `u64::MAX` overflows; builds with overflow checks (tests, CI) abort the host process.
- **`MAP_FIXED` below `0x10000` is accepted:** Linux returns `EPERM` for callers without
  `CAP_SYS_RAWIO`.
- **mremap can exceed or corrupt `max_map_count` handling:** `MREMAP_DONTUNMAP` splits VMAs before
  validating the destination and keeps the splits on `EINVAL`; a move that hits the limit after its
  entries were moved drops VMAs and leaves stale entries at the destination.
- **Shared anonymous `MAP_NORESERVE` is accounted:** contrary to its own comment and Linux, so a
  large shared reservation can fail with `ENOMEM`.
- **pagemap reports lent pages as exclusive:** they are mapped by several address spaces, contrary
  to Linux and the function's own description of the bit.
- **Robust futex death skips CoW pages:** `owner_died` looks up the lock word with `translate`,
  which fails on read-only CoW entries, so a thread dying with a robust lock on a page not written
  since fork never sets `FUTEX_OWNER_DIED` or wakes waiters.
- **Latent CoW leak:** if `return_loans` ever ran while the owner was mutably borrowed, the page
  would stay in the owner's `lent` set without a loan; a fork in that state would give the child a
  loan-less borrowed entry and the owner's next write would show in the child. Unreachable today,
  and the guard's comment cites a path (`recall`) that never reaches it.
- **OOM checks can stop for about a second:** `check` (like the code-view release and load-average
  sampling) reads "every task sleeping" and sets `checks_idle` in two steps, while a thread waking
  from a no-BKL fast-path sleep clears its flag and tests `checks_idle` without the BKL, so the
  waker can miss the store; checks then stay off until the next code-view round (up to 1 s), the
  next engine exit of any thread or another timer event, and the relaxed atomics leave the outage
  unbounded in theory on weak-memory hosts. A thread touching already-mapped memory without
  syscalls makes no engine exit.
- **OOM victim choice differs from Linux and can stall:** scores omit swap and page tables, so a
  mostly swapped-out process ranks low although the root counts VmSwap; negative scores clamp to 1
  and ties go to the lowest tgid (Linux 6.1 compares raw scores and lets a later equal one win). A
  victim that outlived its second is skipped only until another is recorded; picked again, it is
  only waited for, so the killer stalls another second without killing.

## Open questions

- Why is `STACK_TOP` `1 << 47` while the page table covers 48 bits and `MAP_FIXED` may reach
  `1 << 48`?
- Should mappings locked through `mlockall(MCL_FUTURE)` be charged against `RLIMIT_MEMLOCK`, and
  should `mlockall(MCL_CURRENT)` count `PROT_NONE` reservations?
- Should `WRITABLY_SHARED` ever be cleared (for example when the inode is deleted)?
- In a code group, every member's drop calls `set_sources` on the shared `DbtSpace`; is
  last-writer-wins intended?
- Is the window in `mmap` between installing entries and `note_shared_files` acceptable for a
  concurrent fast-path futex on a writable shared file page?
- `statm` reports data = rss while `status` reports VmData = VmSize; is the mismatch intentional?
- A kill frees no Tier 1 translations kept in the translator's pool or shared through code groups,
  and the OOM killer never trims the pool; is that acceptable when translations dominate the root
  usage?

## To verify

- Linux 6.1 arm64 semantics for `PROT_EXEC`-only mappings (execute-only with Enhanced PAN, or
  exec implying read): needs the Linux 6.1 source to decide whether appine's execute-only behaviour
  matches.
- Whether the host kernel commit-accounts the read-write `MAP_PRIVATE` host file mappings used for
  private file VMAs that miss `FileMaps`: depends on host kernel rules, not repository code.
- That Tier 1's dependency-ordered (non-acquire) page-table walk is safe against concurrent entry
  stores: only a code comment claims it; the translator is outside this module.
- That `Dbt::invalidate` by range is correct for code-group members whose address spaces differ:
  translator internals, outside this module.
- Code-group admission and departure (section 8) rest on one research pass; they were not part of
  the independent verification sample.
