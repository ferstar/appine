# Guest memory (kernel/mm) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/mm"`
>
> Last distilled: 2026-11-07 (HEAD = 2d78930). D1–D17 were seeded by hand from the retired design
> doc §5 and the round records in `docs/rounds/`, so their sources cite rounds; later entries cite
> commits.

## Active

### D1: Software page tables and TLB, no hardware MMU emulation
- **What**: Each address space has a software radix page table and each thread a software TLB; Linux mm semantics are implemented without emulating ARM translation hardware.
- **Why**: Downloaded guest code may only run under an interpreter or translator (Play policy), and all guest processes share one host process, so isolation and fork have to be done in software.
- **Tradeoffs**: Every guest memory access pays a TLB check, which Tier 1 inlines.
- **Watch out**: The TLB check dominates memory-heavy code; its layout is a contract with the translator.
- **Source**: design review 2026-10-24 (decision 4), round 23 (`23-tlb-checks`)

### D2: Release host memory only after shootdown and after the BKL is dropped
- **What**: Unmapped host regions wait in the graveyard until other threads are quiesced, then in a thread-local list until the BKL is released.
- **Why**: Guest data races must never reach memory already returned to the host, and host munmap contends on the host `mmap_lock`, so it should not lengthen BKL hold times.
- **Tradeoffs**: Memory is returned slightly later, so the OOM killer skips a round while releases are pending.
- **Watch out**: Any new path that frees host memory under the BKL must go through the same deferral.
- **Source**: rounds 04 (`04-m3a`), 29 (`29-bkl-wait-hold`), 36 (`36-parallel-processes`), 109 (`109-spawn-mmap-bkl`)

### D3: Host commit accounting follows the guest's mapping arguments
- **What**: Host memory is requested as accounted or `MAP_NORESERVE` according to the guest's prot and flags under Linux's rules, not according to the always-read-write host mapping.
- **Why**: Guest permissions live only in the page table; accounting every mapping as writable private memory made V8's 8 GB `PROT_NONE` wasm reservations fail.
- **Tradeoffs**: The host and guest views of commit can differ (fork's spare pieces are host `MAP_NORESERVE` but counted by the guest check).
- **Watch out**: Shared anonymous `MAP_NORESERVE` memory is currently accounted, contrary to this rule.
- **Source**: rounds 10 (`10-m8`), 179 (`179-demand-page-tables`), 199 (`199-reserved-mappings`)

### D4: `PROT_NONE` gets no new entries; `MADV_DONTNEED` goes to the host
- **What**: New `PROT_NONE` mappings are not entered into the page table, and `MADV_DONTNEED` hands whole host pages to the host's madvise.
- **Why**: Large reservations were walked page by page, and zeroing pages to "free" them touched every page being returned.
- **Tradeoffs**: Partial host pages (16 KB hosts) are zeroed instead of discarded.
- **Watch out**: mprotect away from `PROT_NONE` must rebuild entries.
- **Source**: round 12 (`12-stability-memory`)

### D5: Truncation removes entries past the new end in every address space
- **What**: truncate, ftruncate and `O_TRUNC` remove page-table entries past the new EOF from every address space that maps the file and shoot down their TLBs.
- **Why**: A guest access past the new end used to raise a host `SIGBUS` that took down every session; the guest should get its own `SIGBUS` as on Linux.
- **Tradeoffs**: Truncation walks every address space.
- **Watch out**: The host file is shortened before the entries are removed, which leaves a race (see the map's confirmed bugs).
- **Source**: round 12 (`12-stability-memory`)

### D6: fork inspects only pages the host has touched, using pagemap
- **What**: For large written anonymous mappings, fork reads the host `/proc/self/pagemap` to find pages the host touched; untouched pages are known zero and the child keeps its own.
- **Why**: Copying or lending untouched memory is wasted work, and `mincore` cannot be used because it reports pages swapped to zram as absent, which would lose their contents.
- **Tradeoffs**: pagemap reads cost time, so mappings of at most 16 pages lend everything and larger ones read only windows that have page-table entries.
- **Watch out**: If pagemap is unreadable every page in the window is lent.
- **Source**: rounds 13 (`13-profiling`), 26 (`26-fork-cow`), 179 (`179-demand-page-tables`)

### D7: VMAs are never merged; each is its own host mapping
- **What**: Every guest VMA keeps its own host memory; adjacent pieces that would be one Linux VMA are treated as one by maps, mremap and `PROT_GROWSDOWN` (D22) and in fork's commit estimate.
- **Why**: Each anonymous mmap is separate host memory, so VMAs cannot be joined without moving memory.
- **Tradeoffs**: Programs have more VMAs than on Linux, and the host's `vm.max_map_count` is shared by the whole app, so the guest limit is enforced per address space.
- **Watch out**: Joining (`same_vma`) must compare every flag shown in maps and smaps.
- **Source**: rounds 17 (`17-compat`), 18 (`18-small-gaps`); e03c0cc

### D8: The heap reserves 256 MB of host address space on first growth
- **What**: The first brk growth reserves 256 MB (`MAP_NORESERVE`) and later growth lengthens the same VMA; fork recreates the reservation.
- **Why**: Keep the heap a single VMA instead of one host mapping per brk step.
- **Tradeoffs**: Address space is reserved even for small heaps.
- **Watch out**: Growth beyond the reservation falls back to separate mappings.
- **Source**: round 17 (`17-compat`)

### D9: The main stack grows on demand with host memory reserved below
- **What**: execve maps a small stack inside a host region reserved down to the stack limit (at most 8 MB); growth extends into it, then into new chunks reserving about as much again.
- **Why**: `RLIMIT_STACK` used to be ignored (a fixed 8 MB stack), and growing one page at a time as separate host mappings would exhaust `max_map_count`.
- **Tradeoffs**: The VMA count grows logarithmically; the kernel grows the stack at syscall entry, for signal frames and on failed accesses through the shared user-copy helpers and `/proc/<pid>/mem` (D22), not through direct `Mm::read`/`write` calls.
- **Watch out**: The guard gap is checked against the single nearest VMA below, as on Linux.
- **Source**: round 22 (`22-stack-limit`); e03c0cc

### D10: Page-granular copy-on-write in the soft page tables
- **What**: fork lends written private anonymous pages: owner and borrowers map the owner's host page read-only, borrowers copy on write, and the owner recalls copies before writing.
- **Why**: fork copied every touched page while holding the BKL (21 ms versus 0.6 ms native at 100 MB) and doubled memory use.
- **Tradeoffs**: Per-page loan bookkeeping (made cheap with a multiplicative hash, merged traversal and no allocation for a single borrower), and every kernel write path must handle CoW pages.
- **Watch out**: Paths that look up guest memory with `translate` do not break CoW (the robust-futex death path is affected).
- **Source**: round 26 (`26-fork-cow`)

### D11: Restore the owner's write permission as soon as the last borrower returns a page
- **What**: When the last borrower returns a page, the owner's entries get the VMA permission back immediately, without waiting for a fault.
- **Why**: A shell's child usually execs right after fork, so the parent should not take a write fault on every page it lent.
- **Tradeoffs**: Returning a loan may touch the owner's page table from the borrower's side.
- **Watch out**: The restore is skipped if the owner cannot be borrowed, which is unreachable today but would leave a stale lent page.
- **Source**: round 26 (`26-fork-cow`)

### D12: Read-only private file mappings share one host mapping across address spaces
- **What**: Read-only private mappings of files with a trusted content version (program and library code) reuse a kernel-wide cache of read-only host mappings.
- **Why**: Every spawn re-mapped and re-faulted the code segments of the program and the dynamic linker.
- **Tradeoffs**: A VMA must copy before it becomes writable; files changed recently or mapped shared-writable get no trusted version.
- **Watch out**: The host mapping is read-only, so a missed copy faults loudly instead of corrupting other processes' code.
- **Source**: round 28 (`28-spawn-host-cost`)

### D13: Address spaces running the same executable share Tier 1 translations
- **What**: Address spaces of the same executable with consistent executable layouts join a code group that shares one translation space.
- **Why**: Each process had its own translation cache (four parallel cc1 used 34–40 MB each).
- **Tradeoffs**: Members must stay consistent; a member whose executable layout diverges leaves for good and forces a shootdown.
- **Watch out**: The last member leaving must not leave other members' translations behind.
- **Source**: rounds 108 (`108-shared-translation`), 181 (`181-last-member-leaves`)

### D14: Recycle private anonymous host memory by discarding instead of unmapping
- **What**: Private anonymous host ranges are kept after `MADV_DONTNEED` and reused for the next request of the same size and accounting class.
- **Why**: Every host mmap and munmap from any guest process takes the one host `mmap_lock` for write, while `MADV_DONTNEED` takes it for read, and musl's malloc and gcc's garbage collector map and unmap the same sizes repeatedly.
- **Tradeoffs**: Up to 256 MB of address space (32 ranges per size) stays mapped.
- **Watch out**: Recycled ranges must be discarded before reuse so guests never see stale data.
- **Source**: round 109 (`109-spawn-mmap-bkl`)

### D15: Pre-map spare host memory for fork and carve small child mappings from it
- **What**: fork takes pre-mapped pieces for the child's small mappings, heap and stack, and refills the pool after the BKL is released.
- **Why**: Keep host mmap out of fork's BKL hold and avoid one host mapping per small child VMA.
- **Tradeoffs**: A carved region is returned only when all its pieces are unmapped.
- **Watch out**: Pool misses and mappings over 1 MB still mmap under the BKL.
- **Source**: round 109 (`109-spawn-mmap-bkl`)

### D16: Page tables are built on demand for mappings over 1 MB
- **What**: Small and executable mappings get entries at creation; larger ones fill a 2 MB leaf table on first access.
- **Why**: Page tables grew with mapping size (2 MB per GB), which made AddressSanitizer's multi-terabyte shadow memory and LTP fork14 unusable.
- **Tradeoffs**: Every private anonymous page with content must have an entry, because fork and RSS measurement only look where entries exist.
- **Watch out**: Any kernel path that writes guest memory directly must also fill the entry.
- **Source**: round 179 (`179-demand-page-tables`)

### D17: Never-accessible `PROT_NONE` mappings are reservations without host address space
- **What**: A private anonymous mapping that has been `PROT_NONE` since creation holds no host memory until it becomes accessible or the kernel writes into it.
- **Why**: 4 KB-page GKI hosts give the whole app only 39 bits (512 GB) of address space, and Chromium, V8, the JVM, Go and wasm reserve huge `PROT_NONE` ranges.
- **Tradeoffs**: Large writable mappings are still one-to-one, so ASan's shadow memory still cannot be mapped on such hosts.
- **Watch out**: Reserved regions have a fake host base that must never be dereferenced.
- **Source**: round 199 (`199-reserved-mappings`)

### D18: Guest memory, with the page table and software TLB, is one kernel/mm module
- **What**: kernel/mm covers `mm.rs`, `syscall/mem.rs`, the page table in `mmu.c`, the software TLB in `cpu.c` and `internal.h`, the run loop's page-fault branch and the OOM killer in `oom.rs`, so the engine module stops at instruction execution.
- **Why**: The retired design doc's §5 was the only current-state description of guest memory, much shorter than the code warranted, and had drifted from it in several places.
- **Tradeoffs**: The module spans the Rust kernel and the C engine, and a full mapping of a module this size cost about 1.3M subagent tokens.
- **Watch out**: Bugs confirmed at the split or found since stay open in the map, among them the truncation race, ignored `MADV_DONTNEED_LOCKED` and `MADV_POPULATE_*`, past-EOF faults ignoring permissions, approximated Linux VMA boundaries, an ignored `READ_IMPLIES_EXEC`, mlock succeeding on inaccessible ranges and OOM checks stopping for about a second.
- **Source**: cd53f35 (scope since extended by 3f3db11; bug list since updated by 3f3db11, 1459292 and 4e64e74)

### D19: `MADV_WIPEONFORK` and `MADV_DONTFORK` are VMA flags applied at fork
- **What**: madvise sets `wipeonfork` and `dontfork` VMA flags, splitting VMAs as for dontdump; fork skips dontfork VMAs (also in the commit check) and does not lend wipeonfork ones, so the child gets zeroed memory, while the child keeps dontdump and wipeonfork and mremap growth carries all three.
- **Why**: fork ignored both advices and dropped dontdump, and `advice_range` folded in the VMA below the range even when it did not overlap, so madvise failed with `EINVAL` next to file or locked mappings.
- **Tradeoffs**: The child's wiped VMA keeps `written` set, so a later fork of the child still lends it, since clearing it would skip pages the child writes without an mprotect.
- **Watch out**: Any new path that copies a VMA must carry these flags too, and dontfork on the stack or heap kills the child, exactly as on Linux.
- **Source**: e020977

### D20: Memory files read in the order their code runs
- **What**: mm.rs is sectioned into Address space (`impl Mm` from creation and layout through threads and shootdown, faults, kernel access, memory syscalls, fork, files and introspection to VMAs, host backing and population), Consistency and code, Copy-on-write (entry points before their machinery), Introspection types and Host backing; syscall/mem.rs follows a mapping's life from mmap to mincore; oom.rs, filed here, puts the timer thread's `check` before its helpers.
- **Why**: The user wanted each Rust file to read in the order its code runs, as the Kotlin code does since a546fea; mm.rs had no sections and 40 functions sat above their first caller.
- **Tradeoffs**: git blame on the moved lines points at this commit unless read with -M or an ignore-revs file; section headers inside an impl block are new in the repository, and Introspection names two sections.
- **Watch out**: Code appended at the end of `impl Mm` or of a file drifts from this order; oom.rs has since been registered here.
- **Source**: 80d41b5; registry since 3f3db11

### D21: mremap with an old length of 0 duplicates shared mappings
- **What**: `Mm::duplicate` follows 6.1's order (`EFAULT` without a VMA, a `MREMAP_FIXED` target validated and unmapped, `EINVAL` for private mappings, `ENOMEM` without `MREMAP_MAYMOVE`), maps shared files again from the address's offset and clones shared anonymous and SysV VMAs onto the same host memory with `file_len` capped at the host region's end (kernel/ipc D2).
- **Why**: Linux 6.1 lets mremap with an old length of 0 duplicate a shared mapping, and Appine refused it outright.
- **Tradeoffs**: The copy may run past the source VMA only within the original host region, and accesses past that raise `SIGBUS` as past the end of a shmem object.
- **Watch out**: A copied VMA's `file_len` is bounded by its shared host region, so code assuming a shared anonymous VMA's `file_len` equals its length is wrong for such copies.
- **Source**: 5dcf7db

### D22: Linux VMA boundaries are computed over unmerged pieces
- **What**: `same_vma` decides which adjacent pieces form one Linux VMA (contents source, backing, protection, flags, grows-down and an `anon` identity that fork renews per parent VMA) for maps, mremap's one-VMA rule and `PROT_GROWSDOWN`; mprotect follows `do_mprotect_pkey`; a guard gap below grows-down VMAs and `MMAP_MIN_ADDR` (32768) bound placement; and failed kernel accesses grow stacks.
- **Why**: With VMAs never merged (D7), maps, mremap, `PROT_GROWSDOWN`, merging after fork, stack growth on kernel accesses, partial mprotect and mmap_min_addr all differed from Linux, as Starnix's mmap tests showed.
- **Tradeoffs**: fork reads pagemap once per written private file mapping's host region (hal D12), `anon_vma` is still approximated, and direct `Mm::read`/`write` callers do not grow stacks.
- **Watch out**: mremap now refuses ranges spanning differently flagged pieces with `EFAULT` (as Linux before 6.17), partial mprotect leaves earlier VMAs changed on error, and the fork pagemap read may show in fork-heavy benchmarks.
- **Source**: 1459292, e03c0cc

### D23: mlock computes its range as `do_mlock` does
- **What**: mlock, mlock2 and munlock validate and page-align the range in the syscall with Linux's arithmetic before passing it to `Mm::set_locked`.
- **Why**: The span was computed with saturating arithmetic before validation, so a length near `u64::MAX` panicked in debug builds and gave a wrong range in release.
- **Tradeoffs**: A zero-length mlock inside a page now locks that page, as on Linux.
- **Watch out**: None known.
- **Source**: 09188ec

### D24: `vm.overcommit_memory=1` lets fork skip its commit check
- **What**: A global `OVERCOMMIT_MEMORY`, set through procfs (kernel/procfs D16), makes fork skip `commit_too_big` when it is 1.
- **Why**: Redis and similar servers set it so that fork for snapshots does not fail with `ENOMEM`, and fork's commit check is Appine's version of Linux's heuristic mode.
- **Tradeoffs**: Mode 2's strict accounting is not emulated and behaves as 0.
- **Watch out**: With 1, a huge fork that would have failed goes on and can run the app out of memory.
- **Source**: 42d2ffe

### D25: membarrier registrations survive fork
- **What**: `Mm::fork` copies `Mm::membarrier`, as Linux's `dup_mm` copies `membarrier_state`.
- **Why**: A forked child of a membarrier user (liburcu) started unregistered and got `EPERM` from expedited barriers.
- **Tradeoffs**: None.
- **Watch out**: None known.
- **Source**: cbf7267

### D26: Resident memory is split into anonymous, file and shared pages
- **What**: `resident_split` sorts whole VMAs into the three kinds (a written private file mapping counts as anonymous), measuring as `rss_plan` does, for status's Rss lines (kernel/procfs D20), and `shmem_like` is shared with core dumps.
- **Why**: status needs resident pages by kind, and `resident_pages` gives only the total.
- **Tradeoffs**: A written data segment counts all its resident pages as anonymous where Linux counts only the copied ones, and mincore runs per VMA without merging neighbours.
- **Watch out**: Reading status now costs one more measurement pass.
- **Source**: 566da95

## Superseded

- ~~Adjacent VMAs joined only for display in maps and in fork's commit estimate (D7 in part, rounds 17, 18)~~ → replaced by **D22** in e03c0cc (2026-11-07)
- ~~The kernel grows the stack only at syscall entry and for signal frames (D9 in part, round 22)~~ → replaced by **D22** in e03c0cc (2026-11-07)
