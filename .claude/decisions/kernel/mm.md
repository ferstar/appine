# Guest memory (kernel/mm) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/mm"`
>
> Last distilled: 2026-11-08 (HEAD = fffe1c1). D1–D17 were seeded by hand from the retired design
> doc §5 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Constraint index (compressed active decisions)

> Full text of D1, D2, D4, D6–D9, D11–D17, D19–D21, D23–D26: `git show 248d79f637c8f05dadc6ef9fdb0733c568c6a468:.claude/decisions/kernel/mm.md`

- **D1** — Guest memory uses a software radix page table per address space and a software TLB per thread, never emulated ARM translation hardware, and the TLB layout is a contract with the Tier 1 translator, which inlines the check (design review 2026-10-24, round 23)
- **D2** — Unmapped host memory is released only after the other threads are quiesced (the graveyard) and then after the BKL drops (a thread-local list), so any new path freeing host memory under the BKL must use the same deferral, and the OOM killer skips a round while releases are pending (rounds 04, 29, 36, 109)
- **D4** — New `PROT_NONE` mappings get no page-table entries, so mprotect away from `PROT_NONE` must rebuild them, and `MADV_DONTNEED` hands whole host pages to the host's madvise while zeroing partial ones on 16 KB hosts (round 12)
- **D6** — fork finds the pages the host touched through `/proc/self/pagemap`, never `mincore` (which reports zram-swapped pages as absent), and lends every page of mappings of at most 16 pages or when pagemap is unreadable (rounds 13, 26, 179)
- **D7** — VMAs are never merged and each keeps its own host memory, so `same_vma` treats adjacent pieces as one Linux VMA (D22) and must compare every flag shown in maps and smaps, and the guest's `max_map_count` is enforced per address space (rounds 17, 18; e03c0cc)
- **D8** — The first brk growth reserves 256 MB of `MAP_NORESERVE` host address space that later growth lengthens as one VMA and fork recreates, with growth beyond it falling back to separate mappings (round 17)
- **D9** — execve reserves host memory below the main stack down to `RLIMIT_STACK` (at most 8 MB), and the stack grows into it and then into chunks about as large again, but only at syscall entry, for signal frames and on failed accesses through the user-copy helpers and `/proc/<pid>/mem`, never through direct `Mm::read`/`write` (round 22; e03c0cc)
- **D11** — When the last borrower returns a lent page, the owner's entries get the VMA permission back at once instead of waiting for a write fault (round 26)
- **D12** — Read-only private mappings of files with a trusted content version share one kernel-wide read-only host mapping, so a VMA must copy before it becomes writable, and a missed copy faults instead of corrupting other processes' code (round 28)
- **D13** — Address spaces of the same executable with consistent executable layouts share one Tier 1 translation space, a diverging member leaves for good with a shootdown, and the last member to leave must not leave other members' translations behind (rounds 108, 181)
- **D14** — Private anonymous host ranges are discarded with `MADV_DONTNEED` and reused for the next request of the same size and accounting class (up to 32 ranges per size, 256 MB in all), and must be discarded before reuse so guests never see stale data (round 109)
- **D15** — fork carves the child's small mappings, heap and stack from a pool of pre-mapped host memory refilled after the BKL drops, a carved region is returned only when all its pieces are unmapped, and pool misses and mappings over 1 MB still mmap under the BKL (round 109)
- **D16** — Mappings over 1 MB (except executable ones) get page-table entries on first access in 2 MB leaf tables, so every private anonymous page with content must have an entry and any kernel path writing guest memory directly must fill it, since fork and RSS look only where entries exist (round 179)
- **D17** — A private anonymous mapping that has been `PROT_NONE` since creation holds no host memory until it becomes accessible or the kernel writes into it, and its fake host base must never be dereferenced (round 199)
- **D19** — `MADV_WIPEONFORK` and `MADV_DONTFORK` are VMA flags that fork honours (dontfork VMAs are skipped, in the commit check too, and wipeonfork ones are never lent, so the child gets zeroes), and every path that copies a VMA must carry them along with dontdump (e020977)
- **D20** — mm.rs, syscall/mem.rs and oom.rs read in the order their code runs (mm.rs in the sections Address space, Consistency and code, Copy-on-write, Introspection types and Host backing), so new code goes where it runs rather than at the end of `impl Mm` or of the file (80d41b5)
- **D21** — mremap with an old length of 0 duplicates shared mappings in 6.1's error order, mapping shared files again and cloning shared anonymous and SysV VMAs onto the same host memory, so a copy's `file_len` is capped at its host region's end and need not equal its length, and SysV copies count as attachments per kernel/ipc D2 (5dcf7db)
- **D23** — mlock, mlock2 and munlock validate and page-align their range in the syscall with `do_mlock`'s arithmetic before calling `Mm::set_locked` (09188ec)
- **D24** — `vm.overcommit_memory=1` makes fork skip `commit_too_big` (kernel/procfs D16), while mode 2's strict accounting is not emulated and behaves as 0 (42d2ffe)
- **D25** — `Mm::fork` copies the membarrier registrations, as Linux's `dup_mm` copies `membarrier_state` (cbf7267)
- **D26** — `resident_split` sorts whole VMAs into anonymous, file and shared pages for status's Rss lines (kernel/procfs D20), counting a written private file mapping as anonymous, and shares `shmem_like` with core dumps (566da95)

## Active (full detail)

### D3: Host commit accounting follows the guest's mapping arguments
- **What**: Host memory is requested as accounted or `MAP_NORESERVE` according to the guest's prot and flags under Linux's rules (shared anonymous `MAP_NORESERVE` memory included), not according to the always-read-write host mapping.
- **Why**: Guest permissions live only in the page table; accounting every mapping as writable private memory made V8's 8 GB `PROT_NONE` wasm reservations fail.
- **Tradeoffs**: The host and guest views of commit can differ (fork's spare pieces are host `MAP_NORESERVE` but counted by the guest check).
- **Watch out**: None known.
- **Source**: rounds 10 (`10-m8`), 179 (`179-demand-page-tables`), 199 (`199-reserved-mappings`); 5d23297

### D5: Truncation removes entries past the new end in every address space
- **What**: truncate, ftruncate and `O_TRUNC` remove page-table entries past the new EOF from every address space that maps the file and sync each that changed, the caller's included, before the host file shrinks (kernel/vfs D33).
- **Why**: A guest access past the new end used to raise a host `SIGBUS` that took down every session, and with the host file shortened first a concurrent access through a stale translation still did; the guest should get its own `SIGBUS` as on Linux.
- **Tradeoffs**: Truncation walks every address space, and the caller's own is quiesced during the syscall rather than at its exit.
- **Watch out**: If the host truncation fails after the entries are gone, the mappings keep the requested end of file and accesses past it raise `SIGBUS` until remapped.
- **Source**: round 12 (`12-stability-memory`); 400817d

### D10: Page-granular copy-on-write in the soft page tables
- **What**: fork lends written private anonymous pages: owner and borrowers map the owner's host page read-only, borrowers copy on write, and the owner recalls copies before writing.
- **Why**: fork copied every touched page while holding the BKL (21 ms versus 0.6 ms native at 100 MB) and doubled memory use.
- **Tradeoffs**: Per-page loan bookkeeping (made cheap with a multiplicative hash, merged traversal and no allocation for a single borrower), and every kernel write path must handle CoW pages.
- **Watch out**: `translate` does not break CoW, so kernel code writing through it must break CoW or fault the page in first, as the robust-futex death path now does.
- **Source**: round 26 (`26-fork-cow`); e4d3386

### D18: Guest memory, with the page table and software TLB, is one kernel/mm module
- **What**: kernel/mm covers `mm.rs`, `syscall/mem.rs`, the page table in `mmu.c`, the software TLB in `cpu.c` and `internal.h`, the run loop's page-fault branch and the OOM killer in `oom.rs`, so the engine module stops at instruction execution.
- **Why**: The retired design doc's §5 was the only current-state description of guest memory, much shorter than the code warranted, and had drifted from it in several places.
- **Tradeoffs**: The module spans the Rust kernel and the C engine, and a full mapping of a module this size cost about 1.3M subagent tokens.
- **Watch out**: Bugs confirmed at the split or found since stay open in the map, among them ignored `MADV_DONTNEED_LOCKED` and `MADV_POPULATE_*`, past-EOF faults ignoring permissions, approximated Linux VMA boundaries, an ignored `READ_IMPLIES_EXEC` and mlock succeeding on inaccessible ranges.
- **Source**: cd53f35 (scope since extended by 3f3db11; bug list since updated by 3f3db11, 1459292, 4e64e74, 400817d and 5d23297)

### D22: Linux VMA boundaries are computed over unmerged pieces
- **What**: `same_vma` decides which adjacent pieces form one Linux VMA (contents source, backing, protection, flags, grows-down and an `anon` identity that fork renews per parent VMA) for maps, mremap's one-VMA rule and `PROT_GROWSDOWN`; mprotect follows `do_mprotect_pkey`; a guard gap below grows-down VMAs and `MMAP_MIN_ADDR` (32768) bound placement; and failed kernel accesses grow stacks.
- **Why**: With VMAs never merged (D7), maps, mremap, `PROT_GROWSDOWN`, merging after fork, stack growth on kernel accesses, partial mprotect and mmap_min_addr all differed from Linux, as Starnix's mmap tests showed.
- **Tradeoffs**: fork reads pagemap once per written private file mapping's host region (hal D12), `anon_vma` is still approximated, and direct `Mm::read`/`write` callers do not grow stacks.
- **Watch out**: mremap now refuses ranges spanning differently flagged pieces with `EFAULT` (as Linux before 6.17), partial mprotect leaves earlier VMAs changed on error, and the fork pagemap read may show in fork-heavy benchmarks.
- **Source**: 1459292, e03c0cc

### D27: `MADV_DONTNEED` reverts written private file mappings to the file
- **What**: For written private file VMAs, `discard` gives the whole host pages of a host private file mapping back with the host's `MADV_DONTNEED` (`discard_file_host`, shared with the anonymous path) and returns `Reread`s for partial host pages (16 KB hosts) and VMAs copied into anonymous memory by `unshare`, which madvise reads from the store object (zeros past EOF or when it cannot be opened) and stores with `Mm::fill_private`, queueing executable ranges for invalidation.
- **Why**: `discard` only gave private anonymous memory back to the host, so a program resetting a private file mapping with `MADV_DONTNEED` kept its own changes where Linux reads the file again.
- **Tradeoffs**: Re-read pieces are private copies of the file at that moment, so later writes to the file do not show in them as they would on Linux.
- **Watch out**: The 16 KB partial-page path runs only on 16 KB devices and no differential test covers it, and an inode reused after its object was reclaimed would be read in place of the old file.
- **Source**: 8127cfe

### D28: `Mm::mapped_above` answers find_vma's question for forced SIGSEGVs
- **What**: `Mm::mapped_above` is true when the last VMA ends above an address, which decides the code of the SIGSEGV forced when a signal frame cannot be used (kernel/signal D18).
- **Why**: That code depends on whether Linux's find_vma finds a mapping ending above the address, and `is_mapped` is wrong for addresses in a gap below a mapping.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: 41bd6ec

### D29: The timer thread's checks stop only through `stop_checks`
- **What**: The OOM check, the code-view release and the load-average sampler stop through `Shared::stop_checks`, which sets `checks_idle` and then looks at the threads again, with every `sleeping` flag and both sides' `checks_idle` accesses SeqCst so a thread waking from a fast-path sleep either sees the flag or is seen running.
- **Why**: Reading "every task sleeping" and setting `checks_idle` in two steps let a waking thread miss the store, so the checks stayed off until the next code-view round (up to a second).
- **Tradeoffs**: Every sleep and wake now does SeqCst stores on its flag.
- **Watch out**: None known.
- **Source**: 5d23297

### D30: File mapping identities use `st_dev` and `st_ino` as they are
- **What**: A file mapping's identity takes the host's `st_dev` and `st_ino` without casts, since both are `u64` on the aarch64 Linux and Android hosts that remain (hal D1).
- **Why**: The casts existed only because `st_dev` is `i32` on macOS, which is no longer a host.
- **Tradeoffs**: A 32-bit target or another libc would need the casts back.
- **Watch out**: Only a compile error on such a target.
- **Source**: 7c5e636

### D31: Private futexes are keyed by address space and address
- **What**: `Mm::id` (never reused) names the address space for private futex keys, which need only a user address and no mapping, and `shared_maps` (renamed from `shared_files`) lists every writable shared mapping, file, anonymous or SysV, recounted when one is created (mmap, shmat), moved or forked, while `futex_key` no longer copies a borrowed page first (kernel/process D32).
- **Why**: Keys of private memory moved from host addresses to the address space and address, so a waiter on a page replaced by `MAP_FIXED` is found, and gVisor's WakeOp tests pass NULL to private futexes.
- **Tradeoffs**: Futexes in shared anonymous memory (process-shared mutexes) now take the BKL path.
- **Watch out**: Stale ranges only slow the fast path, but a shared mapping not yet noted (the window inside mmap and shmat) could be keyed as private by a concurrent fast-path futex.
- **Source**: f6a82cc, 5d7b936

### D32: The OOM killer skips stuck victims and scores as `oom_badness`
- **What**: `Oom::stuck` keeps every victim that outlived its second while it has threads and `select` skips them, and scores are unclamped with the adjustment times total/1000, equal scores going to the later process.
- **Why**: A stuck victim was skipped only while it stayed the recorded victim, so picked again later it was only waited for and the killer stalled a second without killing, and scores differed from 6.1's `oom_badness`.
- **Tradeoffs**: Scores still omit swap and page tables.
- **Watch out**: A stuck victim stays listed until its last thread ends.
- **Source**: b43ff37

### D33: `MADV_REMOVE` frees storage through the host, and discarding keeps the vDSO
- **What**: `MADV_REMOVE` uses the host's `MADV_REMOVE` for whole host pages and zeroes the edges or what the host refuses, and the vDSO is refilled from its image after a discard.
- **Why**: `MADV_REMOVE` zeroed instead of freeing storage, and `MADV_DONTNEED` zeroed the vDSO, so the next clock call raised `SIGILL`.
- **Tradeoffs**: Breakpoints a debugger wrote into the vDSO are lost by `MADV_DONTNEED`, as on Linux.
- **Watch out**: tmpfs accounting does not see `MADV_REMOVE` until a recount.
- **Source**: 5d7b936

### D34: `Mm::can_write` checks a buffer before a kernel write, and unmapping reaps destroyed segments
- **What**: `Mm::can_write` tells whether a range lies in writable mappings, for a kernel write that must not fail halfway (AF_UNIX recvmsg, before it takes queued data), and munmap, mremap and `MAP_FIXED` call `reap_shm` (kernel/ipc D7).
- **Why**: recvmsg needed a writability check so that a bad buffer fails with `EFAULT` before data is consumed, and shm segments marked for destruction outlived their last mapping when it went away through these calls.
- **Tradeoffs**: `can_write` looks only at VMA permissions, not at mapped pages or stack growth.
- **Watch out**: None known.
- **Source**: 5d7b936

## Superseded

- ~~Adjacent VMAs joined only for display in maps and in fork's commit estimate (D7 in part, rounds 17, 18)~~ → replaced by **D22** in e03c0cc (2026-11-07)
- ~~The kernel grows the stack only at syscall entry and for signal frames (D9 in part, round 22)~~ → replaced by **D22** in e03c0cc (2026-11-07)
