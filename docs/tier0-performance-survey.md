# Pure Tier 0 performance survey

Measured at `16914003` on 2026-11-17. All guest runs used `appine-run --tier 0`.
Reusing the current code page in the interpreter loop reduced median wall time on all six
workloads by 9.6–13.0% with GCC and 4.5–6.3% with Clang. The initial survey measured each
prototype separately.

- The **fetch prototype** kept the last guest code page and its host address offset in local
  variables. A page hit reused the offset, and a page change consulted the execute TLB.
  It read the instruction word from memory each time, checked for a kick before each instruction
  and called the existing step path for a misaligned PC or an armed hardware breakpoint.
  GCC disassembly shows the fast path also saves a call to the step function; Clang already
  inlined it. The timings include both effects. [Interpreter loop](../core/engine/src/c/cpu.c)
- The **arithmetic prototype** split the non-flag-setting ADD/SUB immediate, shifted-register
  and extended-register cases before calling `add_with_carry`. It computed the width-limited
  result directly and preserved the SP/ZR destination rules. [Integer decoder](../core/engine/src/c/interp.c)

Linux arm64 Colima VM on Apple silicon, release builds, GCC 14.2 and Clang 19.1, vCPU 2.
The five [integer benchmarks](../tests/bench/src/lib.rs) used the same GCC-built guest binaries
for both host compilers. The sixth workload ran Alpine 3.24.3 BusyBox
`gzip -n -c /bin/busybox` using an imported minirootfs. For each workload and compiler, the three builds ran
once to warm up, then in all six possible orders. All output matched a native run byte
for byte, including the compressed stream. Times include starting the CLI.
Negative deltas indicate a lower median wall time than the same compiler’s baseline.

| Workload | Guest rounds or input | GCC baseline (ms) | Fetch | Arithmetic | Clang baseline (ms) | Fetch | Arithmetic |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| sha256 | 100 | 471.5 | -13.0% | -5.9% | 455.5 | -6.3% | -4.8% |
| lz | 6 | 413 | -10.8% | -3.5% | 422 | -5.1% | -3.1% |
| sort | 1 | 302 | -9.6% | -3.8% | 288 | -4.5% | -4.0% |
| crc32 | 100 | 279 | -12.2% | -1.1% | 259 | -5.6% | -0.4% |
| vm | 200000 | 496 | -10.9% | -5.0% | 503 | -4.8% | -4.0% |
| gzip | 919,304 bytes | 1345.5 | -10.7% | +0.7% | 1409.5 | -6.0% | -2.3% |

A separate SIMD probe used the unmodified engine. A loop read the same aligned 16 bytes
four million times (eight identical loads per iteration), then wrote the final vector to stdout.
Only the encoding changed. All three variants produced the same 16 bytes as native; medians
below are from the same six-order rotation.

| Load | GCC (ms) | Clang (ms) |
| --- | ---: | ---: |
| `ld1 {v0.16b}, [x0]` | 254 | 242 |
| `ld1 {v0.2d}, [x0]` | 80 | 66.5 |
| `ldr q0, [x0]` | 51.5 | 53.5 |

At the baseline revision, the interpreter checked the full span, then used the generic memory
helper per lane even for contiguous LD1/ST1. The byte form made 16 calls per vector and the
doubleword form made two.

## Combined implementation

The final Tier 0 loop keeps the current code page's address offset for one run. Contiguous
LD1/ST1 copy whole registers, and ordinary ADD/SUB bypasses the flag calculation at the two
decode sites. The comparison below used the same guest inputs, machine and six-order rotation
as the survey. The three builds were the baseline, fetch plus SIMD, and all three changes.
Each SIMD loop executed four million instructions of the indicated type. Outputs matched native
execution byte for byte.

| Workload | GCC base (ms) | GCC final | Change | Clang base (ms) | Clang final | Change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| sha256 | 490.5 | 399.5 | -18.6% | 455.5 | 407 | -10.6% |
| lz | 428 | 369 | -13.8% | 422 | 389 | -7.8% |
| sort | 313 | 272 | -13.1% | 288.5 | 267 | -7.5% |
| crc32 | 287.5 | 256.5 | -10.8% | 258 | 244 | -5.4% |
| vm | 511 | 432 | -15.5% | 502.5 | 464 | -7.7% |
| gzip | 1383.5 | 1253 | -9.4% | 1405 | 1296 | -7.8% |
| LD1 .16B | 253.5 | 66 | -74.0% | 248 | 58 | -76.6% |
| ST1 .16B | 288 | 65.5 | -77.3% | 247 | 57 | -76.9% |
| LD2 .16B | 442 | 435 | -1.6% | 436 | 434.5 | -0.3% |

Relative to fetch plus SIMD, adding the arithmetic change reduced SHA-256, lz, sort and vm time
by 2.5–6.2% with GCC and 3.0–4.8% with Clang. GCC crc32 and gzip took 1.2% and 0.2% longer.
An earlier version with a shared ADD/SUB helper also made Clang sort 4.5% slower than fetch
plus SIMD. Keeping the branch in the decoders made Clang sort 3.3% faster on that comparison.

The final builds executed across a page boundary, observed a rewritten instruction, and faulted
when the next page lost execute permission. In a two-thread guest, revoking execute permission
while the other thread spun inside that page produced SIGSEGV at the loop instruction in five
of five runs per compiler. Interpreter tests cover the same-page hardware breakpoint and
the fault and watchpoint checks before a multi-register store. All timings are from the Linux VM.
