# Optimizing CPU Performance for `llama.cpp`

> Handoff / future-work design note (Step 0 profiling and Step 1 implemented 2026-11-22; Step 2+ pending).
> Grounded in `core/engine/src/c/dbt.c`, `core/engine/include/appine/engine.h`,
> `core/engine/src/c/features.c`, `core/engine/src/c/mmu.c`, `core/kernel/src/mm.rs`,
> `core/kernel/src/syscall/mem.rs`, `core/kernel/src/syscall/fast.rs`,
> `core/kernel/src/syscall/sched.rs`, `docs/rounds/{27-interleaved-checks,30-translated-access,31-superblocks,141-fp16-jscvt}.md`,
> `docs/mobile-gpu.md`, `docs/dbt-memop-extensions.md`, and
> `.agents/maps/{engine,engine/dbt,kernel/mmu,kernel/futex,kernel/sysfs}.md`
> as of 2026-11-22.

## Scope & current baseline

This note covers optimization directions for **CPU-only `llama.cpp` inference** inside Appine
(Vulkan GPU offload is explicitly out of scope for now; see `docs/mobile-gpu.md` for current mobile
driver issues that leave CPU inference as the primary reliable path on phones). Every direction is
evaluated under a strict non-regression constraint: general workloads (`gcc`, `python`, `node`,
`go build`, `mvn-warm`, process spawning) must not regress.

### Existing measurements and bottlenecks

- **Prior ratios** (`docs/rounds/141-fp16-jscvt.md`, Apple Silicon VM, 4 threads, `llama-bench -p 128 -n 32`):
  - Qwen2.5-0.5B F16: prompt `1.64×` native, generation `1.22×` native.
  - Qwen2.5-0.5B Q4_0: prompt `1.38×` native, generation `1.21×` native.
  - Standalone 4096-element F16 dot-product microbenchmark (`ggml_vec_dot_f16`): `2.1×` slower than
    native (`0.069` vs. `0.0326` ns/element), attributed to inline soft-MMU checks. On narrower mobile
    cores (Cortex-A715/A720/X3/X4 vs. wide Apple Silicon), extra host instructions per load cost
    proportionally more.
- **Two distinct runtime regimes in `llama.cpp`**:
  1. **Prompt processing (prefill, `pp128`/`pp512`)** — compute- and L1/L2-bound tiled GEMM /
     `vec_dot` kernels (`SDOT`/`SMMLA`/`FMLA`/`BFDOT`). Overhead comes mainly from per-load inline
     soft-MMU checks in unrolled SIMD loops and from whether `ggml` selects an `I8MM`/`BF16` backend
     tier.
  2. **Token generation (decode, `tg32`/`tg128`)** — memory-streaming and barrier-bound. Every
     generated token streams through the entire model's weights (~350 MB for 0.5B Q4_0, ~900 MB for
     1.5B, ~2–4 GB for 3B–7B), whereas the per-thread data TLB covers only **16 MiB**
     (`APPINE_TLB_ENTRIES = 4096` × 4 KiB in `core/engine/include/appine/engine.h`). Decoding a
     1.5B–7B model therefore triggers hundreds of thousands to ~1M soft-TLB misses per token, plus
     frequent 4 KiB page crossings on unaligned quantized blocks (`block_q4_0` = 18 B,
     `block_q8_0` = 34 B, `block_q4_K` = 144 B).

### Current engine and kernel baseline

- **ISA features (`core/engine/src/c/features.c`)**: Appine advertises `FP`, `ASIMD`, `ASIMDDP`
  (DotProd), `FPHP`/`ASIMDHP` (FP16), `ASIMDFHM`, `ASIMDRDM`, `FCMA`, `I8MM`, `BF16`, `FRINTTS`,
  `ATOMICS` (LSE), `LRCPC`/`ILRCPC`, crypto/CRC, `FLAGM`/`FLAGM2`, `SB`, `DCPOP`/`DCPODP`, `DIT`,
  and `ECV`. It does **not** advertise `SVE`/`SVE2`, `SME`/`SME2`, `LSE2` (`uscat`), or `EBF16`.
- **Tier 1 translation (`core/engine/src/c/dbt.c`)**:
  - `MAX_BLOCK_INSNS = 64` (blocks also end at 4 KiB code page boundaries).
  - `plan_group` shares one TLB check across same-base accesses inside a block up to
    `GROUP_SPAN = 512` B, `GROUP_MAX = 16` accesses, and `GROUP_SLOTS = 4` concurrent base slots
    (`struct appine_cpu::dbt_groups[4]`).
  - `emit_refill` handles TLB misses and cross-page checks in translated code by saving `X0–X4`,
    checking `nwatch`, walking the 4-level radix page table (`MMU_LEVELS = 4`, `LEAF_SPAN = 2 MiB`),
    filling both read and write TLB halves, updating `tlb_used`, and returning via `br T1`.
  - `HINT` space (`0xd503201f`, including `YIELD` = `0xd503203f`) emits no instructions.
- **Kernel layer**:
  - Private `FUTEX_WAIT`/`FUTEX_WAKE` and `sched_yield` run on the no-BKL fast path
    (`core/kernel/src/syscall/fast.rs`), backed by a global `Futexes` mutex/linear list and
    per-thread `eventfd` + `poll()`.
  - `sched_setaffinity` is forwarded to host threads (`core/kernel/src/syscall/sched.rs`);
    `/sys/devices/system/cpu/` exposes synthesized `topology/` and host-copied `cache/` and
    `cpufreq/` (`.agents/maps/kernel/sysfs.md`).
  - `madvise` ignores `MADV_WILLNEED`, `MADV_SEQUENTIAL`, `MADV_RANDOM`, and `MADV_POPULATE_*`
    (`core/kernel/src/syscall/mem.rs`); mappings > 1 MiB populate soft page-table entries on demand
    in 2 MiB chunks via `Mm::fault_in` under the BKL (`core/kernel/src/mm.rs`).
  - Length-gated `memop` acceleration currently matches only the pinned Alpine musl SHA-256
    (`.agents/maps/kernel/exec.md`).

---

## Step 0 profiling baseline (2026-11-22)

Measured on the development ARM64 Linux VM (Apple Silicon host, 8 vCPUs, kernel `6.8.0-117-generic`)
using `llama.cpp` v0.8.0 multi-variant build (`GGML_CPU_ALL_VARIANTS=ON`, Alpine musl, release
`appine-run --tier 1` on `/bench/store` vs. native `appine-difftest:latest` Alpine container).

### 1. Native vs. Appine Tier 1 throughput (`libggml-cpu-armv8.2_2.so`)

| Model & workload | Native (t/s) | Appine Tier 1 (t/s) | Slowdown (Native / Appine) |
|---|---:|---:|---:|
| Qwen2.5-0.5B Q4_0 `pp512` `-t 1` | 285.42 ± 1.44 | 189.26 ± 1.31 | **1.51×** |
| Qwen2.5-0.5B Q4_0 `tg128` `-t 1` | 110.07 ± 0.11 | 81.95 ± 0.92 | **1.34×** |
| Qwen2.5-0.5B Q4_0 `pp128` `-t 4` | 1159.98 ± 0.41 | 847.70 ± 3.72 | **1.37×** |
| Qwen2.5-0.5B Q4_0 `pp512` `-t 4` | 1027.65 ± 3.47 | 707.10 ± 3.75 | **1.45×** |
| Qwen2.5-0.5B Q4_0 `tg32` `-t 4` | 226.78 ± 4.10 | 188.01 ± 10.04 | **1.21×** |
| Qwen2.5-0.5B Q4_0 `tg128` `-t 4` | 225.25 ± 0.46 | 177.91 ± 10.12 (186.74 in perf run) | **1.21–1.27×** |
| Qwen2.5-0.5B F16 `pp128` `-t 4` | 377.45 ± 2.07 | 225.43 ± 1.71 (230.02 in perf run) | **1.64–1.67×** |
| Qwen2.5-0.5B F16 `tg32` `-t 4` | 91.13 ± 0.52 | 72.09 ± 0.23 (80.74 in perf run) | **1.13–1.26×** |

### 2. CPU time breakdown (`APPINE_PERF_MAP=1 perf record -e task-clock -F 999` + `perf script`)

| Workload | `dbt_blocks` | `dbt_shared_routines` (`emit_refill` etc.) | Linux kernel | Host (`appine-run` / libc) |
|---|---:|---:|---:|---:|
| Q4_0 prefill (`pp512 -t 4 -r 3`, 12,087 samples) | **95.15%** | 2.57% | 1.42% | 0.85% |
| Q4_0 decode (`tg128 -t 4 -r 3`, 8,197 samples) | **95.21%** | **1.99%** | 1.68% | 1.12% |
| F16 prefill (`pp128 -t 4 -r 3`, 9,333 samples) | **92.75%** | 4.96% (`refill[1]` write cross-page = 3.33%) | 0.96% | 1.33% |
| F16 decode (`tg32 -t 4 -r 3`, 1.17 GiB, 5,151 samples) | **94.18%** | **2.62%** | 1.75% | 1.46% |

Key `--stats` and startup observations across all 7 baseline runs:

- **Tier 0 / syscall overhead is negligible**: `tier 0 fallback` is 38–40 instructions (`0.0%`),
  `exits: interp` is 47–263, `code cache flushes` is 0, `groups undone` is 3–7. `exits: svc` is
  virtually identical between `-t 1` (8,484 / 8,579) and `-t 4` (8,515 / 8,614), confirming that
  with `llama.cpp`'s default `--poll 50`, `futex`/`sched_yield` syscalls are not a bottleneck.
- **Static grouping coverage**: `memory (inline)` is ~34.0–34.5% of translated guest instructions,
  with `sharing a check` at 14.4–15.3% (~44% of memory instructions grouped).
- **`emit_refill` is minor even on 1.17 GiB streaming decode**: `dbt_shared_routines` totals only
  1.99% on Q4_0 decode and 2.62% on 1.17 GiB F16 decode (read refill ~1.8–2.3%). The 3.33% write
  refill (`+0x440..+0x57f`) in F16 prefill is not streaming weight misses; it comes from
  `tinyBLAS::gemm_bloc` spilling 384 B (`stp q0..q31, [sp, #0..352]`) on every inner iteration when
  that grouped stack frame straddles a 4 KiB page boundary.
- **Startup / `fault_in`**: End-to-end wall time for `-p 1 -n 0 -t 4 -r 1` on the 1.17 GiB F16
  model is 392–401 ms (cold vs. warm), with host userspace (`appine-run` including `fault_in`,
  `dbt_run`, `prepare`, `sqlite3`) totaling 0.85–1.46% across the benchmark runs.

### 3. Hot translated block disassembly (`libggml-cpu-armv8.2_2.so`, guest base `0x7fffbe594000`)

Over **75–85%** of total runtime concentrates in 7 concrete inner loops:

1. **Q4_0 prefill #1 — `ggml_gemm_q4_0_4x4_q8_0` (`0xba094..0xba490`, 49.2% of total CPU)**:
   - One **256-instruction** straight-line inner loop (`subs x24, x24, #1; b.gt ba094`), currently
     split into **5 blocks** (`ba094` 14.3%, `ba194` 11.7%, `ba294` 10.3%, `ba394` 12.7%, `ba494`
     0.7%) by `MAX_BLOCK_INSNS = 64`, severing `[x23]`, `[x22]`, and `[x21]` groups across block
     boundaries.
   - Hits **all four** Direction 1 limits simultaneously:
     - 256-insn body > `MAX_BLOCK_INSNS` (64);
     - 5 live base pointers (`x28`, `x25`, `x23`, `x22`, `x21`) > `GROUP_SLOTS` (4);
     - Activation pointer lives in stolen register **`x28` (`CTX`)** (`ldr q, [x28, #0/16/32/48]`),
       preventing grouping and forcing 5 redundant `ctx->x[28]` loads + TLB checks;
     - Scale load uses a derived pointer **`sub x20, x28, #8; ldr d17, [x20]`**.
   - Pure read-only constant-stride self-loop (`x28 += 0x48`, `x25/x23/x22/x21 += 0x88`).
2. **Q4_0 prefill #2 — `ggml_compute_forward_flash_attn_ext_tiled` (28.7% total)**:
   - Three compact 10–11 instruction strided self-loops: `0x6f260` (12.7%, stolen `x17` post-index
     `ldp q28, q31, [x17], #256` + `M_REG` `ldr s23, [x2, x28, lsl #2]` with stolen `x28` +
     `ld1r [x2]`), `0x6f5a0` (8.8%, `add x4, x4, x26` with stolen `x26`), and `0x6ed64` (4.2%,
     `M_REG` `ldr h31, [x1, x6, lsl #1]` + `str s31, [x2, x6, lsl #2]`).
   - Too short and heterogeneous for `plan_group` alone; requires **Direction 4**.
3. **Q4_0 decode #1 — `ggml_gemv_q4_0_4x4_q8_0` (`0xb5c00..0xb5c84`, 39.7% of total CPU)**:
   - 34-instruction single-block self-loop (`x2 += 34` with 3 loads, `x0 += 72` with 5 loads,
     `cmp w6, w5; b.gt`).
   - **Already fully grouped today** (2 bases, no stolen regs, non-interleaved, <64 insns).
     Direction 1 gives essentially zero gain here; remaining overhead (~16 extra host insns per
     34-insn iteration from 2 group TLB checks + 1 kick check) requires **Direction 4**.
4. **Q4_0 decode #2 — `ggml_gemv_q8_0_4x4_q8_0` (`0xb9480..0xb94e4`, 21.9% of total CPU)**:
   - 26-instruction single-block self-loop (Qwen 0.5B's `output.weight` is `Q8_0`): `x0 += 0x22`,
     `x2 += 0x88`, with **`mov x5, x2`** aliasing `x2` for `ld1 {v28..v31}, [x5], #64` and
     `ld1 {v24..v27}, [x5]`.
   - Extending Direction 1.3 to track **`MOV Xd, Xn`** (`ORR Xd, XZR, Xn`, zero-offset affine
     alias) merges `[x2, #-8]` with both `[x5]` `ld1`s immediately; **Direction 4** removes the
     remaining per-iteration TLB and kick checks.
5. **All workloads — `ggml_barrier` (`0x152f8`, 14.3% Q4_0 decode, 5.3% F16 decode, 2.2% Q4_0
   prefill, 1.5% F16 prefill)**:
   - Exact 4-instruction spin loop: `152f8: yield; ldr w0, [x1]; cmp w3, w0; b.eq 152f8`. Dropping
     `yield` (`0xd503203f`) directly matches Direction 5.1.
6. **F16 prefill — `tinyBLAS::gemm_bloc<4, 6>` (`0x9dff0..0x9e104`, 65.1%) + `<4, 5>`
   (`0x9e600..0x9e6f0`, 12.9%), 78.0% total**:
   - 69-instruction inner loop (`9dff0..9e104`): split at the 4 KiB code page boundary `9e000`
     (2.6% + 56.9%) and again at `9e100` (5.7%, just `cmp x11, x3; b.gt`!) by `MAX_BLOCK_INSNS = 64`.
   - Loads 10 distinct matrix rows via `M_REG` (`ldr q, [x2, x3/x6/x5/x4]` and `[x1, x7/x3/x6/x5/x4]`,
     each advancing +16 B/iter via `x0 += 8` shifted `<< 1`, using stolen `x16`/`x17`/`x18`), plus
     12 `stp q, [sp, #0..352]` stores per iteration (triggering the 3.33% `refill[1]` cross-page
     overhead). Direction 1.1 fixes the 2-insn tail split; eliminating the 10 per-row TLB checks
     requires **Direction 4**.
7. **F16 decode — `ggml_vec_dot_f16` (`0x67de0..0x67e0c`, 72.1% of total CPU)**:
   - 12-instruction single-block self-loop (`ldp`×2 on `x2 += 64` interleaved with `ldp`×2 on
     `x4 += 64`, 4× `fmla v*.8h`, `cmp x6, x2; b.ne`).
   - Already grouped, yet interleaving forces `dbt_groups` spill/reload + 2 group TLB checks + kick
     check (~20 extra host instructions on top of 12 guest instructions). Ideal **Direction 4**
     target (up to 64 native iterations per 4 KiB page).

### 4. ISA tier (Direction 2) & real-device big.LITTLE audit (Direction 5.2)

- **ISA tier**: `GGML_BACKEND_DEBUG=1` confirms both native and Appine load
  `libggml-cpu-armv8.2_2.so`. In upstream `ggml/src/CMakeLists.txt` (`CMAKE_SYSTEM_NAME MATCHES
  "Linux"`), `armv8.6_1` requires `DOTPROD FP16_VECTOR_ARITHMETIC SVE MATMUL_INT8` (there is no
  non-SVE `MATMUL_INT8` Linux variant, whereas Android defines `android_armv8.6_1` without `SVE`).
  Neither the Apple Silicon Linux VM nor the tested Galaxy S23 (`SM-S9110`, Snapdragon 8 Gen 2)
  exposes `sve` in host `/proc/cpuinfo` (both expose `i8mm bf16` only). This confirms **no SVE
  implementation is needed**; unlocking `I8MM` is a build/packaging choice (`-DGGML_NATIVE=ON` or a
  non-SVE `I8MM` backend variant).
- **Android big.LITTLE (`SM-S9110`, Snapdragon 8 Gen 2)**: Host topology is 3× Cortex-A510
  (`cpu0–2`, `cpu_capacity=266`, 2.02 GHz) + 4× Cortex-A715/A710 (`cpu3–6`, `cpu_capacity=811`,
  2.80 GHz) + 1× Cortex-X3 (`cpu7`, `cpu_capacity=1024`, 3.36 GHz); `cpu_capacity` and
  `cpufreq/cpuinfo_max_freq` carry SELinux label `u:object_r:sysfs_devices_system_cpu:s0`.
  Upstream `llama.cpp` (`common/common.cpp`'s `common_cpu_get_num_math()`) only reads
  `/sys/devices/system/cpu/cpuN/topology/thread_siblings` on ARM Linux to count physical cores and
  does not read `cpu_capacity` or `midr_el1` to pin threads.

---

## Candidate optimization directions

### Direction 1: Relax Tier 1 block & `plan_group` limits for unrolled SIMD/GEMM loops (highest near-term ROI, zero regression)

Unrolled `ggml-cpu` dot-product and tiled GEMM inner loops routinely hit four artificial ceilings in
`core/engine/src/c/dbt.c` that rarely trigger on branchy integer code:

1. **Decouple block length from the hard `MAX_BLOCK_INSNS = 64` cap (`dbt.c:60, 3411`)**
   - Unrolled `vec_dot` and `4×4`/`4×8`/`8×8` GEMM inner loops often span 60–200 straight-line guest
     instructions, most of which are 1:1 verbatim NEON arithmetic instructions (`sdot`, `smmla`,
     `fmla`). Splitting a loop body at 64 guest instructions terminates every active `plan_group` and
     forces every live base pointer to re-run a full 6-instruction inline TLB check in the next
     block.
   - **Approach**: Stop translating a block based on remaining host-word / stub / fixup budget
     (keeping `MAX_BLOCK_WORDS` within `REFILL_RESUME_SHIFT` and `BRANCH_MASK`) and raise the guest
     instruction cap toward the `uint8_t block::nguest` limit (e.g., 192–255; review `WORDS_LIMIT`
     and `nogroup()`'s `4 * MAX_BLOCK_INSNS` span so memory bounds and strike-out windows stay sane).
   - **Non-regression**: Branchy workloads (`gcc`, `python`, `node`, `JVM`) have short basic blocks
     (typically 5–15 instructions) and are unaffected.

2. **Expand `GROUP_SLOTS` from 4 to 8 (`dbt.c:494`, `engine.h:156`)**
   - Tiled GEMM microkernels (`4×8`, `8×8`) interleave 4–8 weight row pointers plus activation and
     output pointers in a single loop body. Once 4 slots are occupied, the 5th+ base pointers cannot
     group at all and pay a full inline TLB check on every load.
   - **Layout verification**: In `struct appine_cpu` (`core/engine/include/appine/engine.h:115-159`),
     `offsetof(struct appine_cpu, tlb)` is currently **448 B** (`dbt_groups[4]` occupies bytes
     `416..448`). The `ldp` constraint in `dbt.c:170` is `TLB_OFF % 16 == 0 && TLB_OFF + 16 <= 504`,
     leaving **40 bytes of unused headroom**. Expanding to `uint64_t dbt_groups[8]` moves `TLB_OFF`
     to **480 B** (`480 + 16 = 496 <= 504`) with **zero change** to TLB addressing instructions
     (mirror the change in `core/engine/src/lib.rs` so `layout_matches` passes).
   - **Non-regression**: Workloads with <= 4 concurrent groups never touch the extra slots.

3. **Track affine derived base pointers (`ADD Xd, Xn, #imm`) in `plan_group` (`dbt.c:1564-1569, 1649-1656`)**
   - Quantized blocks such as `block_q4_0` (18 B) and `block_q8_0` (34 B) are not 16-byte aligned.
     Because AArch64's unsigned-offset `LDR Qt, [Xn, #uimm]` requires 16-byte alignment, `LDUR` only
     reaches `[-256, +255]`, and `LD1` has no immediate offset at all, compilers frequently materialize
     derived pointers (`add x9, x0, #18; ldr q0, [x9, #2]` or `add x9, x0, #2; ld1 {v0.16b}, [x9]`).
   - Currently `keeps_group` only tracks in-place updates (`Rd == Rn`). Writing `x9 = x0 + const`
     causes `plan_group` to treat `[x9]` as an unrelated base — emitting a redundant TLB check and
     marking `x0`'s group `clobbered` (triggering spill/reload through `dbt_groups`).
   - **Approach**: During `plan_group` lookahead, track simple block-local affine aliases
     (`r_derived = rn + const_imm`, cleared whenever `r_derived` is overwritten) and fold accesses
     through `r_derived` into `rn`'s group with adjusted `off`.
   - **Non-regression**: Pure translation-time analysis; emits fewer runtime instructions and also
     helps struct sub-pointer accesses in `gcc`/`python`.

4. **Extend grouping to `M_REG` and stolen-register bases (`dbt.c:1618, 1657`)**
   - Currently `lead->mode == M_REG`, `m.mode == M_REG`, and `is_stolen(rn)` cannot lead or join
     groups. Indexed vector loops (`ldr q0, [x0, x8]`) sharing the same `(base, index)` (or constant
     index step) and register-starved GEMM kernels that assign a pointer to `x16`/`x17`/`x26`/`x27`/`x28`
     lose all grouping. Allowing stolen bases to lead (loading `rn` once for the group check and
     reusing `T2`/`dbt_groups[slot]` while `rn` is unmodified) removes both redundant TLB checks and
     redundant `ctx->x[rn]` loads.

**Key files**: `core/engine/src/c/dbt.c`, `core/engine/include/appine/engine.h`,
`core/engine/src/lib.rs`, `core/engine/tests/dbt.rs`.

---

### Direction 2: Verify & unblock `ggml-cpu` ISA tier selection (`I8MM` / `BF16` vs. `SVE`)

- Appine already implements and advertises `ASIMDDP`, `FP16`, `FHM`, `I8MM` (`SMMLA`/`UMMLA`,
  roughly 2× `SDOT` throughput on Q4_0/Q4_K/Q8_0 prefill), and `BF16` (`BFDOT`/`BFMMLA`).
- However, `docs/rounds/141-fp16-jscvt.md` noted that in `GGML_CPU_ALL_VARIANTS=ON` multi-variant
  builds, the `I8MM` tier previously also required `SVE`, trapping Appine on `armv8.2_2` (DotProd +
  FP16 only) even when the host supported `I8MM`.
- **Action before writing code**: Run `GGML_BACKEND_DEBUG=1 llama-bench ...` (both on Alpine's
  packaged `llama.cpp` 0.0.9564 and on a local build) to check which `libggml-cpu-*.so` variant is
  actually selected.
  - If an `armv8.6` NEON+I8MM+BF16 variant is already selected, no ISA work is needed here.
  - If distro packages still fall back to `armv8.2_2` solely because `SVE` is absent, weigh
    rebuilding with `-DGGML_NATIVE=ON` / upstream variant configuration against the high cost of
    implementing SVE in Appine (see *Non-goals / deferred* below).

---

### Direction 3: Reduce streaming soft-TLB miss & cold-fault overhead (targeting Decode & TTFT)

**Hard constraint**: `docs/rounds/27-interleaved-checks.md` and `docs/rounds/30-translated-access.md`
proved that adding even **one instruction** to the ordinary inline 4 KiB TLB hit path regresses
`gcc` and `python`. All work in this direction must stay off the inline 4 KiB hit path.

#### 3A. Refill-path sequential TLB prefill + optional 2 MiB L2 superpage entries
- **Problem**: `emit_refill` (`dbt.c:3608-3673`) runs ~55–65 instructions plus a 4-level radix walk
  and `tlb_used` RMW on every 4 KiB miss. Streaming 0.5B–7B weights exceeds the 16 MiB TLB on every
  token, and unaligned quantized blocks / grouped checks (`144–512 B`) cross 4 KiB boundaries
  frequently, hitting the `cross` refill path even when both pages are warm.
- **Approach**:
  1. *Sequential batch prefill inside `emit_refill`*: Once the walk reaches the L3 leaf table
     (which already holds 512 adjacent 4 KiB PTEs covering 2 MiB in contiguous cache lines), check
     whether the preceding TLB slot `(idx - 1)` is valid with the **same `addend`** (indicating
     sequential streaming). Only when that signal holds, fill the next `k` (e.g., 4–8) contiguous
     same-addend, same-permission 4 KiB slots (16–32 KiB) whose slots are currently invalid or hold
     the same stream, updating `tlb_used` once per 64-bit word. This eliminates 75–87% of refill
     entries on linear weight scans while avoiding cache/TLB pollution on random pointer-chasing
     workloads (`gcc`, `python`).
  2. *L2 2 MiB superpage entries in `kernel/mmu`*: Since `LEAF_SPAN` is already 2 MiB (`mmu.c:27`)
     and large file/tensor mappings are backed by a single contiguous `HostRegion` with uniform
     permissions, an L2 entry covering a fully contiguous, uniform-prot, non-CoW 2 MiB span can be
     tagged (e.g., low bit in L2) as a 2 MiB leaf. `emit_refill` stops one level early, and
     cross-4 KiB checks within the same 2 MiB block verify contiguity without probing L3. (On any
     `mprotect`, `munmap`, or CoW split inside that 2 MiB range, demote the L2 entry to a normal
     512-entry L3 table under the BKL.)
- **Non-regression**: Zero change to the inline 4 KiB TLB hit sequence; random-access misses do not
  trigger batch prefill.

#### 3B. Forward `madvise` hints and widen read-only file `fault_in` windows
- **Problem**: `core/kernel/src/syscall/mem.rs:476-500` ignores `MADV_WILLNEED`, `MADV_SEQUENTIAL`,
  and `MADV_POPULATE_READ` (which `llama.cpp`'s `llama-mmap.cpp` issues on GGUF model mappings).
  Moreover, `Mm::fault_in` (`core/kernel/src/mm.rs:1157-1163`) fills only one 2 MiB leaf table per
  guest fault under the BKL, so 4–8 threads scanning a 1–4 GiB model on first inference serialize
  500–2000 guest page faults on the BKL.
- **Approach**:
  1. Forward `MADV_WILLNEED` / `MADV_SEQUENTIAL` / `MADV_RANDOM` to host `libc::madvise` **after
     releasing the BKL** (so host readahead never stalls other guest threads).
  2. For large read-only `PrivateFile` VMAs (or ranges explicitly hinted with `MADV_WILLNEED` /
     `MADV_POPULATE_READ`), widen `fault_in`'s soft-PTE fill span from 2 MiB to e.g. 8–16 MiB (soft
     PTEs only — does not touch host pages or inflate RSS, and leaves anonymous reservations like
     ASan's 2 MiB demand granularity untouched).
- **Non-regression**: Zero impact on anonymous allocations, `fork`, or small-file workloads; improves
  model cold-load and time-to-first-token (TTFT).

**Key files**: `core/engine/src/c/dbt.c`, `core/engine/src/c/mmu.c`, `core/kernel/src/mm.rs`,
`core/kernel/src/syscall/mem.rs`, `core/hal/src/lib.rs`.

---

### Direction 4: Strip-mined in-page / contiguous-range fast loops for strided SIMD self-loops (highest ceiling, zero regression)

Note: `docs/dbt-memop-extensions.md` (Direction 2B) measured that write-only tight self-loops account
for `<=0.25%` of dynamic TLB checks on `gcc`/`python`/`go`/`node`/`mvn-warm` and rightly deferred
tight-loop recognition for general integer workloads. **`llama.cpp` is the opposite regime**: a
handful of strided NEON compute loops (`ggml_vec_dot_*`, `ggml_gemm_*`, `rms_norm`, `silu`, `rope`,
`softmax`, `quantize_row_*`) dominate total execution time.

- **Approach**:
  - In `prepare()`, detect canonical single-block (or small 2-block) self-loops (`back-edge == x->pc`)
    whose memory operands all use constant-stride induction pointers (`base_i += stride_i`), with a
    simple monotonic trip counter (`subs`/`b.ne`, `cbnz`, `cmp`/`b.lo`), no calls/syscalls/atomics,
    and no aliased writes that could mutate control/pointer state.
  - Emit a guarded fast-loop prologue:
    1. If `cpu->nwatch != 0`, skip straight to the standard translated loop.
    2. For each induction base `base_i`, look up its TLB entry (or 2 MiB superpage entry if 3A is
       implemented) and compute how many iterations fit before crossing the verified 4 KiB (or 2 MiB)
       boundary: `iters_i = bytes_remaining_i / stride_i`.
    3. Compute `safe_iters = min(remaining_trips, min_i(iters_i), KICK_BUDGET)` where
       `KICK_BUDGET` (e.g., 128 or 256) caps time between `pending` checks so `quiesce`/signals
       remain responsive within microseconds.
    4. If `safe_iters >= 2`, add each `addend_i` into `base_i` (converting guest pointers to host
       pointers), execute the **native inner loop** with **zero TLB checks and zero kick checks** for
       `safe_iters` iterations, then subtract `addend_i` to restore exact guest pointers and fall
       through to the standard checked loop for page crossings / tail iterations / kicks.
- **Expected impact**: Inside matched `ggml` kernels, removes ~100% of soft-MMU and kick-check
  overhead, pushing inner-loop speed close to `1.0×` native across all quantization formats and
  `llama.cpp` versions (and also accelerating `ffmpeg`, `OpenBLAS`, `numpy`, `zlib`).
- **Non-regression**: Strictly zero code or runtime change for any block that does not match the
  static loop pattern.
- **Invariants to preserve**:
  - Precise exceptions (all accessed bytes across `safe_iters` are verified before the first
    iteration; any miss or fault falls back to the standard checked loop before mutating guest
    state).
  - Respect D6 (`X18` untouched, inserted prologue/epilogue code never clobbers live guest `NZCV`),
    both `APPINE_DBT_REGS_DEFAULT` and `APPINE_DBT_REGS_GO`, and template arena compatibility.
  - Aliasing safety: when the loop writes to memory, ensure write ranges do not overlap read ranges
    in a way that changes cross-iteration behavior (or verify that per-iteration order is identical,
    which holds for verbatim inner loops since only address translation is hoisted!).
  - `c->nwatch != 0` always bypasses the fast loop.

---

### Direction 5: Threadpool sync, `YIELD`, big.LITTLE topology, and glibc `memop` polish

1. **Emit host `YIELD` verbatim instead of dropping it (`dbt.c:2112`)**
   - Currently `(insn & 0xfffff01f) == 0xd503201f` emits nothing for all `HINT` encodings.
     `ggml_threadpool`'s spin-wait barrier executes `yield` (`0xd503203f`); erasing it turns
     spin-waits into tight zero-yield loops that contend for memory bandwidth and accelerate thermal
     throttling on phones (`docs/mobile-gpu.md:133` observed thermal status rising `0 -> 2` on S25
     Ultra during `llama-bench`).
   - **Approach**: Special-case `insn == 0xd503203f` to copy `yield` verbatim (1-line change, zero
     impact on non-spinning workloads).

2. **Audit big.LITTLE CPU topology visibility on real phones (`sysfs.md`, `features.c:81`)**
   - Because `ggml_threadpool` synchronizes all worker threads at every op barrier (hundreds per
     token), a single thread scheduled onto a LITTLE core drags down overall throughput 2–3×.
   - `sched_setaffinity` and host `cpufreq/`/`cache/` are already wired up, but `topology/` is
     synthesized (no `cluster_cpus` or `cpu_capacity`), `host_midr` is read once globally on a single
     core (`features.c:81`) so every `/sys/.../cpu<n>/regs/identification/midr_el1` returns the same
     value, and Android SELinux may deny reading host `cpufreq`. Verify on device what `llama.cpp`
     sees and whether worker threads stay on big/mid cores.

3. **Profile `futex` barrier overhead before changing `kernel/futex`**
   - Private `FUTEX_WAIT`/`FUTEX_WAKE` already bypass the BKL (`core/kernel/src/syscall/fast.rs`),
     but share a single `Shared::futexes` mutex + linear list and wake threads via per-thread
     `eventfd` writes + `poll()` (`.agents/maps/kernel/futex.md`).
   - With `llama.cpp`'s default `--poll 50` spinning, futex traffic may be minor; with OpenMP or
     `--poll 0`, barrier wakeups can be frequent. Only replace `eventfd`/global-list with bucketed
     queues + host `SYS_futex` if profiling shows `futex` is a material fraction of decode time.

4. **Note on `memop` coverage (`docs/dbt-memop-extensions.md`)**
   - Alpine guests already benefit from in-cache same-page `memcpy`/`memset`/`memmove` helpers;
     Debian/glibc guests currently do not (only the pinned Alpine musl SHA-256 is recognized).
   - Cross-page `exec_memop` (>4 KiB, e.g., KV-cache copies or weight repacking) currently walks the
     4-level page table twice per 4 KiB page (`memop_check_range` then the copy loop); if L2 2 MiB
     superpages (Direction 3A) are added, `exec_memop` can also copy contiguous spans in bulk.

---

## Non-goals / rejected directions

- **Modifying the ordinary inline 4 KiB TLB hit sequence**: Tested in Rounds 27 and 30; even +1
  instruction regresses `gcc` and `python`.
- **Implementing `SVE`/`SVE2` or `SME`/`SME2` as a first step**: Unlike NEON extensions (which reuse
  `V0–V31` and only need whitelisting in `simd.c`), SVE/SME add `Z0–Z31`, `P0–P15`, `FFR`, `ZA`,
  `prctl(PR_SVE_SET_VL)`, signal-frame `sve_context`, ptrace regsets, and predicated/gather-scatter
  soft-MMU translation. Unconditionally saving SVE state in `enter`/`exit` would also regress every
  syscall-heavy workload unless made lazy. Moreover, most mobile ARMv9 cores (Cortex-X3/X4/A715/A720)
  implement 128-bit SVE (same width as NEON). Revisit only if profiling confirms `SVE` is the sole
  blocker for `I8MM` in target distro packages and rebuilding/reconfiguring is not viable.
- **Symbol/SHA-256 native hooks for `ggml_vec_dot_*` kernels**: `llama.cpp` updates frequently,
  supports dozens of quantization formats, and relies heavily on compiler inlining. Generic Tier 1
  grouping + streaming loop optimization delivers durable gains across all formats and workloads.
- **Superblocks across calls and unconditional forward jumps**: Measured in Round 31
  (`docs/rounds/31-superblocks.md`) at `<=2%` gain with a warm-compile `gcc` regression.
- **Fixed-offset direct guest-to-host memory mapping (eliminating soft-MMU)**: Analyzed in Round 30
  as incompatible with Android GKI's 39-bit user VA (512 GiB shared across all guest processes),
  16 KiB host pages, and precise fault recovery.
- **Concurrent direct relinking of conditional branches (`nthreads > 1`)**: Prohibited by ARM CMODX
  rules (`dbt.c:5518`); backward edges of single-block loops already link directly at `link_block`
  time before publication.

---

## Recommended execution order & verification

1. **Step 0 — Profile first (completed 2026-11-22, see *Step 0 profiling baseline* above)**
   - Established that 92.8–95.2% of CPU time sits inside 7 translated hot loops, `emit_refill` is
     only 2.0–2.6% on decode (even for 1.17 GiB F16), `futex`/Tier 0 exits are negligible, and
     `libggml-cpu-armv8.2_2.so` is selected identically on native and Appine because host CPUs lack
     `sve`.
2. **Step 1 — Low-risk quick wins (completed 2026-11-22)**
   - Implemented Direction 5.1 (verbatim `yield`) and full Direction 1 (`MAX_BLOCK_INSNS = 256` with
     budget-based stopping and lazy 32/64-word guest snapshotting, `GROUP_SLOTS` 4→8 and `GROUP_MAX`
     16→32, block-local affine alias tracking for `ADD`/`SUB #imm` and `MOV Xd, Xm` before and after
     leaders, stolen-register base grouping, and `M_REG` grouping).
   - **Same-session alternating A/B results** (2 rounds × `-r 3`, `appine-run --tier 1` on
     `/bench/store`):
     - Static check-sharing jumped from ~44–45% to **48–51%** of inline memory instructions across
       all runs (`23,661→27,618` on Q4_0 `-t 1`, `28,796→34,911` on Q4_0 `-t 4`, `27,529→32,933` on
       F16 `-t 4`).
     - Qwen2.5-0.5B Q4_0 `pp512 -t 1`: `190.7 → 197.8 t/s` (**+3.7%**); `pp128 -t 4`: `849.9 → 871.8 t/s` (**+2.6%**); `pp512 -t 4`: `718.9 → 727.3 t/s` (**+1.2%**); `tg32 -t 4`: `180.6 → 184.2 t/s` (**+2.0%**); `tg128 -t 4`: `178.9 → 180.4 t/s` (**+0.8%**).
     - Qwen2.5-0.5B F16 `pp128 -t 4`: `224.5 → 228.6 t/s` (**+1.8%**); `tg32 -t 4`: flat (`73.9 → 72.9 t/s`, within run-to-run noise).
     - **Non-ML A/B**: `appine-bench` Tier 1 geomean `1.71× → 1.70×` (`vm` `1.89× → 1.80×`);
       `gcc-lua`, `python-loop`, `spawnbench`, and `gobuild` all noise-identical or slightly faster
       (lazy `fetch_guest`, `affine_valid == 0` fast return, and `active_groups`/`grouped_bits`
       eliminated cold-translation overhead on `gcc-lua`).
   - **Takeaway**: Confirms Step 0's diagnosis — on wide Apple Silicon cores, removing remaining
     grouping gaps inside compute-bound loops delivers modest (+1–4%) gains, and breaking past the
     remaining 1.2–1.6× gap requires **Step 2 (Direction 4: strip-mined in-page fast loops)**.
3. **Step 2 — High-ceiling core optimization: Direction 4 (strip-mined fast loops)**
   - Promoted ahead of Direction 3A based on Step 0: `ggml_vec_dot_f16` (72.1% F16 decode),
     `ggml_gemv_q4_0_4x4_q8_0` (39.7% Q4_0 decode), `ggml_gemv_q8_0_4x4_q8_0` (21.9% Q4_0 decode),
     `flash_attn_ext_tiled` (28.7% Q4_0 prefill), and `tinyBLAS::gemm_bloc` (78.0% F16 prefill)
     cannot be fully accelerated by `plan_group` alone, and all are constant-stride self-loops.
4. **Step 3 — Deferred / optional secondary work**
   - **Direction 3B** (unlocked `madvise` forwarding + widened read-only file `fault_in`): small
     independent change for cold-start TTFT on flash storage; can land anytime.
   - **Direction 3A** (sequential TLB prefill + L2 2 MiB superpages): deprioritized while
     `dbt_shared_routines` is only 2.0–2.6% on decode; revisit after Step 1+2 shrink in-block time
     (or when profiling 3B–7B models where streaming refill share grows).
5. **Verification & non-regression gates**
   - Unit/integration tests in `core/engine/tests/dbt.rs` (block length boundary, 5–8 interleaved
     groups, derived pointers, `M_REG`/stolen groups, cross-page faults, watchpoints, TBI tags,
     L2 superpage demotion on `mprotect`/`munmap`/CoW, strip-mined loop exact faults & kick
     responsiveness).
   - Lockstep comparison (`tests/lockstep`) under both `Default` and `Go` register sets.
   - Same-session alternating A/B benchmarks on `llama-bench` (`pp128`/`pp512`, `tg32`/`tg128`,
     Q4_0/Q4_K_M/F16, `-t 1` & `-t 4`) **and** non-ML regression suite (`appine-bench`, `gcc-lvm`,
     `python`, `node`, `mvn-warm`, `spawnbench`).
   - Update `.agents/maps/engine/dbt.md` (and `kernel/mmu.md` / `kernel/mm.md` if touched) via
     `map-module` once changes land.
