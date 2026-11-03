# Guest memory (kernel/mm) Decisions
> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/mm"`
> Last distilled: 2026-11-03 (HEAD = 4b19037) — seeded by hand from the retired design doc §5 and
> the round records in `docs/rounds/`; no `MODULE: kernel/mm` Decision blocks exist yet, so sources
> cite rounds instead of commits.

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
- **What**: Every guest VMA keeps its own host memory; adjacent VMAs are joined only for display in `/proc/<pid>/maps` and in fork's commit estimate.
- **Why**: Each anonymous mmap is separate host memory, so VMAs cannot be joined without moving memory.
- **Tradeoffs**: Programs have more VMAs than on Linux, and the host's `vm.max_map_count` is shared by the whole app, so the guest limit is enforced per address space.
- **Watch out**: Display joining must compare every flag shown in maps and smaps.
- **Source**: rounds 17 (`17-compat`), 18 (`18-small-gaps`)

### D8: The heap reserves 256 MB of host address space on first growth
- **What**: The first brk growth reserves 256 MB (`MAP_NORESERVE`) and later growth lengthens the same VMA; fork recreates the reservation.
- **Why**: Keep the heap a single VMA instead of one host mapping per brk step.
- **Tradeoffs**: Address space is reserved even for small heaps.
- **Watch out**: Growth beyond the reservation falls back to separate mappings.
- **Source**: round 17 (`17-compat`)

### D9: The main stack grows on demand with host memory reserved below
- **What**: execve maps a small stack inside a host region reserved down to the stack limit (at most 8 MB); growth extends into it, then into new chunks reserving about as much again.
- **Why**: `RLIMIT_STACK` used to be ignored (a fixed 8 MB stack), and growing one page at a time as separate host mappings would exhaust `max_map_count`.
- **Tradeoffs**: The VMA count grows logarithmically; the kernel grows the stack only at syscall entry and for signal frames.
- **Watch out**: The guard gap is checked against the single nearest VMA below, as on Linux.
- **Source**: round 22 (`22-stack-limit`)

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
