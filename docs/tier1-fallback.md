# Tier 1 interpreter exits

Measured at `26ceb37c` on 2026-11-17, release build in the Linux arm64 development
container on Apple silicon. These measurements distinguish two counters in
`appine-run --tier 1 --stats`: **tier 0 fallback** counts instructions the translator
declined, while **exits: interp** counts trips from translated code to the interpreter,
including failed grouped memory checks. Neither measures time.

| Workload | Interpreter exits | What caused them |
| --- | ---: | --- |
| `appine-bench --quick --skip-tier0 --runs 3 --stats` | 1 across five programs | One exit in sort; none in sha256, lz, crc32 or vm. All five outputs matched native. |
| Python: `sum(i*i for i in range(500000))` | 8 | All eight were group leaders in the traced run. The empty-range control also had eight exits. |
| OpenJDK 21: `java -version` | 1,656 | 1,048 DSB with an outstanding code page, 225 IC IVAU with another page pending, 283 FPCR writes, 8 FPSR writes, 92 other. |
| Node: sum the integers from 0 to 199,999 in a loop | 80 | 26 DSB, 4 IC IVAU, 10 FPCR writes, 40 other. |

Python, Java and Node used the Alpine differential-test store. For attribution, a
temporary linker wrapper around `appine_interp_step` read the executable word at
the guest PC and recorded the DBT exit reason, whether a code page was pending, and
the interpreter result. These runs were for counts only. The uninstrumented Java
and Node runs reported 1,651 and 81 exits respectively. In a second traced run all
283 Java FPCR writes and all 12 Node FPCR writes supplied the existing value.

**Tagged data reads.** A static assembly guest loaded argc from the initial stack
2,097,152 times with `ldr x1, [x0]; subs x2, x2, #1; b.ne`. Both variants copied SP
to x0 and used MOVK to set bits 63:48, to either 0 or 0xab00; both checked that
the final load returned 1. The uninstrumented CLI, including startup, ran each
variant six times in alternating order:

| Top byte | Static fallbacks | Interpreter exits | Median wall time |
| --- | ---: | ---: | ---: |
| 0 | 0 | 0 | 5 ms |
| 0xab | 0 | 2,097,152 | 53 ms |

At the baseline revision the inline TLB check compared the full page number and
the refill rejected a tagged address. Tier 0 ignored the top byte for data.

## Implementation and measurements

Tier 1 now checks FPCR writes against the saved guest value after applying Tier 0's
write mask. An equal write continues in the same block. A changed write still enters
Tier 0. DSB and ISB execute in translated code. When a code page is pending, the
dispatcher invalidates it and looks up the following PC. That exit still saves and
reloads registers, though it no longer counts as an interpreter exit.

For tagged data, the refill uses bits 47:0 for the page-table walk and checks that
bits 55:48 are zero at both ends of the access. The data TLB entry retains the top byte
in the page number and subtracts the tagged page base in its addend. Repeated accesses
with that tag hit the existing inline check. Different tags of one page compete for
the same slot. Exclusive monitors and code invalidation use untagged addresses.

The baseline and changed GCC release binaries ran on vCPU 2 in the same Linux arm64
container. Each workload had one warm-up with statistics, then eight timed runs per
binary in alternating order. Times are median wall time including CLI startup.
The FPCR loops checked the final FPCR against native execution. One wrote the initial
value every time, and the other toggled rounding-mode bit 22 after every write.
Interpreter exit counts come from the warm-up. Each pair produced the same output.
The five integer benchmarks used half their default round counts.

| Workload | Baseline (ms) | Changed (ms) | Interpreter exits, baseline → changed |
| --- | ---: | ---: | ---: |
| Stack load, untagged, 2,097,152 reads | 5.75 | 5.85 | 0 → 0 |
| Stack load, top byte 0xab, 2,097,152 reads | 55.14 | 6.43 | 2,097,152 → 0 |
| FPCR, 2,097,152 equal writes | 54.25 | 5.98 | 2,097,152 → 0 |
| FPCR, 2,097,152 writes alternating rounding modes | 85.70 | 85.74 | 2,097,152 → 2,097,151 |
| OpenJDK 21 startup | 128.12 | 129.40 | 1,600 → 273 |
| Node startup and integer loop | 233.68 | 234.13 | 81 → 45 |
| Python sum of squares | 153.13 | 154.17 | 8 → 8 |
| sha256 | 177.59 | 178.14 | 0 → 0 |
| lz | 241.05 | 241.16 | 0 → 0 |
| sort | 252.86 | 251.10 | 1 → 1 |
| crc32 | 360.80 | 362.39 | 0 → 0 |
| vm | 283.66 | 286.07 | 0 → 0 |

The tagged load loop took 88% less time and the equal FPCR write loop took 89% less time. Java and Node had fewer interpreter
exits with overlapping startup times. Across the five integer workloads, median
changes ranged from −0.7% to +0.8%. Application use of tagged pointers and
Android device timings remain unmeasured.
