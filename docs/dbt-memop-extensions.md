# Extending Tier 1 `memop` Acceleration

> Handoff / future-work design note (no code changes yet).
> Grounded in `core/engine/src/c/dbt.c`, `core/engine/include/appine/engine.h`,
> `core/kernel/src/loader.rs`, `core/kernel/src/mm.rs`, and `.agents/maps/engine/dbt.md`
> as of 2026-11-22.

## Current baseline

Tier 1 (`engine/dbt`) currently accelerates six length-bounded musl memory/string primitives by
intercepting their entry PCs and executing page-validated host libc calls when the byte count is
large enough to amortize a dispatcher exit:

- **Discovery and trust (`core/kernel/src/loader.rs`, `core/kernel/src/mm.rs`)**:
  `bundled_libc` verifies the bundled Alpine `ld-musl-aarch64.so.1` against a pinned size and
  SHA-256 (`BUNDLED_LIBC_SIZE`, `BUNDLED_LIBC_SHA256`), copies it into an immutable `memfd`
  snapshot (`BundledLibc`), parses `STT_FUNC` symbols in executable `PT_LOAD` segments for
  `memcpy`, `memset`, `memmove`, `memcmp`, `memchr`, and `strncmp`, and registers their runtime
  entry PCs via `DbtSpace::set_memops` (`appine_dbt_set_memops`, max `APPINE_DBT_MEMOPS` = 32).
- **Publication and retirement (`core/engine/src/c/dbt.c`)**:
  `appine_dbt_set_memops` sorts and deduplicates `(pc, kind)` into an immutable `struct memop_table`,
  publishes it with release semantics to `d->memops` (retiring older tables on `older` until
  `drop_memops`), and retires any existing translation whose registration changed (`retire_pc`).
  `appine_dbt_free` retires all memop-specialized blocks and calls `drop_memops` before returning
  an instance to the pool.
- **Translation-time gating (`prepare`, `install`)**:
  For shared or ungrouped blocks (`p->tag == 0`), `prepare` probes `find_memop(d, pc)`. When
  registered, the block bypasses both `template_fetch` and `template_store` and emits a
  2-instruction threshold gate right after the entry kick check:
  ```asm
  ubfx T0, X2, #7, #57       // test X2 >= 128 without modifying NZCV
  cbnz T0, R_INTERP_stub
  ```
  `install` rechecks `find_memop(d, p->pc)` under `d->lock` before publishing the block.
- **Runtime execution and fallback (`exec_memop`, `R_INTERP` in `appine_dbt_run`)**:
  On a pure `R_INTERP` exit at a registered PC (excluding `R_UNGROUP` fallthrough and private
  code-group blocks), `exec_memop` checks `c->nwatch == 0` and `n <= MEMOP_MAX_BYTES` (64 MiB),
  untags TBI pointers, pre-validates every guest page for mutating ops (`memop_check_range`) to
  preserve precise faults, runs host `memset`/`memmove`/`memcmp`/`memchr` page-by-page via
  `appine_mmu_translate`, pops the shadow return stack (`c->dbt_rs`) if its top matches `X30`, and
  sets `c->pc = c->x[30]`. Any failure returns `false` and falls straight through to
  `appine_interp_step(c)` (Tier 0) with zero guest-visible side effects.

### Why the pattern works

In a same-ISA pass-through DBT, register ALU/FP/NEON instructions already run at near-native speed.
The dominant overhead in bulk memory routines is executing a soft-TLB check every 8–16 bytes.
Batching memory work turns per-access TLB checks into one `appine_mmu_translate` per 4 KiB guest
page plus host SIMD loops, while keeping precise guest exceptions via pre-validation and Tier 0
fallback.

---

## Candidate extension directions

### Direction 1: Broaden registered-entry memops (lowest risk)

Extend the existing `(pc, kind)` registration pipeline to cover more high-memory-bandwidth routines.

1. **Additional musl primitives**
   - *Length-bounded*: `strnlen` (length in `X1`), `memrchr`, `mempcpy`, `bzero` (length in `X1`).
     Requires parameterizing which guest register the translation-time threshold gate tests instead
     of hardcoding `X2` in `prepare`.
   - *Unbounded string ops* (`strlen`, `strchr`, `strcmp`): no upfront length register exists, so an
     unconditional `R_INTERP` exit will regress short strings (the common case). Only worth pursuing
     if paired with an inline first-chunk probe in translated code (exit only when no NUL/match is
     found in the first 32–64 bytes) and backed by profiling.
2. **Non-musl runtimes and libraries**
   - **Go runtime** (`runtime.memmove`, `runtime.memclrNoHeapPointers`, `runtime.memequal`):
     `engine/dbt` already special-cases Go binaries (`APPINE_DBT_REGS_GO` via Go build-ID detection).
     Go programs bypass musl and spend substantial time in their own statically linked memory
     helpers (using arm64 register ABI). Because Go binaries are statically linked, whole-file
     SHA-256 verification does not apply; safe admission requires matching the ELF symbol plus a
     verified instruction-byte fingerprint (or curated runtime routine hashes), leveraging the
     fact that `dbt.c` already snapshots and re-verifies guest words (`p->words`).
   - **Bundled `zlib` (`crc32`, `adler32`)**: if a pinned shared library exists in the rootfs,
     read-only page-chunked checksums fit the exact `memop_check_range` + host implementation model
     with no write-side fault hazards.

**Key files**: `core/engine/include/appine/engine.h`, `core/engine/src/lib.rs`,
`core/engine/src/c/dbt.c`, `core/kernel/src/loader.rs`, `core/kernel/src/mm.rs`.

---

### Direction 2: Move acceleration into Tier 1 codegen (highest upside, libc-agnostic)

The current approach has two structural ceilings: (a) taking `R_INTERP` pays a full `exit`/`enter`
transition (saving/restoring `V0–V31`, `FPSR`, `NZCV`, and host callee-saved registers), forcing
the 128-byte threshold; (b) it misses HotSpot JIT stubs (`arraycopy`/`fill`), GCC/Clang inlined
loops, and statically linked C/Rust/Go binaries.

#### 2A. In-cache same-page fast path (lowering the threshold)
- Add lightweight helper routines alongside `refill` at the head of the code cache (`emit_routines`).
- For a registered `memcpy`/`memset`/`memmove`, check in generated code whether `[dst, dst+n)` (and
  `[src, src+n)`) stay within a single guest page and hit the inline data TLB (plus `nwatch == 0`,
  already guaranteed because watched pages are never placed in the TLB).
- If they hit, run a tight host loop directly on the translated host pointers without leaving
  translated code (no full `V0–V31` save/restore) and return via the shadow return stack (`ret`).
- On cross-page spans or TLB miss, fall back to the existing `R_INTERP` -> `exec_memop` path.
- Expected effect: profitable threshold drops from `128 B` to roughly `32–64 B`.

#### 2B. Tight-loop idiom recognition (replacing symbol coupling)
- `plan_group` (`core/engine/src/c/dbt.c`) already amortizes TLB checks across straight-line
  same-base accesses inside one basic block (up to `GROUP_SPAN` = 512 B, `GROUP_MAX` = 16).
- Extend `prepare` to recognize canonical single-block self-loops (back-edge targeting `x->pc`):
  post-index `ldr`/`str`/`ldp`/`stp` or `dc zva` zeroing loops with a monotonic counter and simple
  branch termination (`subs` + `b.ne`, `cbnz`, `cmp` + `b.lo`/`b.ne`).
- Emit a loop-entry prologue that computes the safe in-page iteration count (`min(remaining,
  bytes_to_page_end / stride)`), performs one TLB check per pointer for that chunk, and executes a
  native host loop (or calls the in-cache helper) before updating guest registers.
- On TLB miss, aliasing/overlap ambiguity, or watchpoint, fall back to the normal translated loop or
  Tier 0, preserving exact fault addresses and architectural state.
- Expected effect: automatically accelerates HotSpot JIT array stubs, static Rust/Go/C binaries,
  and compiler-inlined loops with zero loader/SHA-256 coupling.

**Invariants to preserve**:
- Must work identically under both `APPINE_DBT_REGS_DEFAULT` and `APPINE_DBT_REGS_GO` (respecting
  stolen registers and never touching host `X18`).
- Must not break template arena reuse (`template_fetch`/`template_store`) or CMODX concurrent
  patching rules.
- Must remain compatible with `nogroup` strike-out / precise faults and `c->nwatch != 0`.

---

### Direction 3: Chunked, kick-responsive `exec_memop` (removing the 64 MiB cap)

Currently `exec_memop` rejects `n > MEMOP_MAX_BYTES` (64 MiB) so a single call cannot starve
`c->pending` kicks (signals, full code-cache flushes).

- Process large ranges in chunks (e.g., 1–4 MiB) and poll `__atomic_load_n(&c->pending,
  __ATOMIC_RELAXED)` between chunks.
- **Important ABI pitfall to handle carefully**:
  - Read-only primitives (`memcmp`, `memchr`, `strncmp`) have no guest memory side effects and can
    cleanly abort back to Tier 0 or resume.
  - Mutating primitives (`memset`, `memcpy`, `memmove`) must return the **original `dst`** in `X0`
    per C ABI. Therefore, if a mutating `exec_memop` yields mid-operation on a kick, it **cannot**
    simply advance `X0` and leave `c->pc` at the function entry — doing so would corrupt the final
    return value in `X0` (and for overlapping `memmove`, change overlap direction).
  - Safe ways to handle mutating kicks:
    1. Keep the kick check only before the first write (keeps current semantics) or
    2. Service `pending` inside `appine_dbt_run` only by returning `APPINE_EXIT_KICK` after saving
       internal resume state (not recommended: leaks engine state across signal delivery / `ptrace`
       inspection), or
    3. Use a bounded slice size (e.g., 64 MiB is already ~5–15 ms on mobile DRAM) and only relax
       the cap for read-only ops or verify whether >64 MiB ops actually occur in target workloads
       before adding complexity.

---

## Non-goals / boundaries

Do **not** extend this mechanism to:

- **Compute-bound functions (`libm`, AES/SHA inner rounds, FFT)**: same-ISA DBT already executes
  FP/NEON/Crypto instructions verbatim. Exiting to host C adds overhead and risks `FPCR`/`FPSR`
  edge-case divergence.
- **Stateful or callback-bearing library calls (`malloc`/`free`, `pthread_mutex_*`, `qsort`)**:
  `exec_memop` runs inside `engine`'s host fault guard (must not allocate host memory or call back
  into the kernel layer), and intercepting guest allocator/lock internals couples correctness to
  private libc data structures and signal/fork semantics.

---

## Suggested execution order & verification

1. **Profile first**: measure soft-TLB time and `memop` hit/miss distribution across key workloads
   (`go build`, Maven/JVM, GCC `cc1`, Node/Python) to rank Direction 1 vs. 2A vs. 2B by real gain.
2. **Recommended sequence**:
   - **Step 1 (Direction 2A)** if profiling shows heavy 32–128 B musl memops or high `R_INTERP`
     exit cost; or **Step 1 (Direction 1 — Go runtime / extra musl ops)** if Go or `strnlen`/`bzero`
     dominates.
   - **Step 2 (Direction 2B — tight-loop idiom recognition)** once profiling confirms JVM JIT stubs
     or inlined loops are the top remaining soft-TLB bottleneck.
   - **Step 3 (Direction 3)** only if real workloads regularly exceed 64 MiB or suffer kick latency.
3. **Verification checklist for any change**:
   - Unit & integration tests in `core/engine/tests/dbt.rs` covering: `< threshold`, `== threshold`,
     `> threshold`, unaligned/cross-page boundaries, overlapping `memmove` (both directions),
     read-only/unmapped tail pages (precise fault with zero partial writes), TBI tagged pointers,
     active watchpoints (`nwatch != 0`), shadow return stack alignment (`dbt_rs`), code-group
     shared vs. private blocks, concurrent `set_memops` updates, and instance pool reuse.
   - Differential lockstep tests (`tests/lockstep`) and both `Default` and `Go` register sets.
   - Update `.agents/maps/engine/dbt.md` via the `map-module` skill once implemented.
