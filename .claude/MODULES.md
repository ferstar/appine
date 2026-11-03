# Modules

Registry of module IDs used by `.claude/maps/`, `.claude/decisions/` and `MODULE:` Decision blocks.

## Structural modules

- `engine` — CPU engine: `core/engine/` except the page table and TLB fill/flush (Tier 0 interpreter `interp.c`/`simd.c`/`atomic.c`/`native.c`/`cpu.c`, Tier 1 translator `dbt.c`/`emit.h`, `features.c`, `guard.c`, `include/appine/engine.h`, Rust bindings `src/lib.rs`) plus `tests/lockstep`; instruction execution, translation, code cache and sharing, FP state, CPU features, host fault guard.
- `kernel/process` — processes, threads and scheduling: `core/kernel/src/task.rs`, `process.rs`, `kernel.rs` (BKL, `Cx`, wake-ups), `cell.rs`, `syscall/proc.rs`, `syscall/fast.rs`, `futex.rs`, `loader.rs`, `machine.rs` (timer thread, boot/shutdown), `itimer.rs`, `posix_timer.rs`, `loadavg.rs`, `inittab.rs`, `syscall/sched.rs`, `pidns.rs`, `nsproxy.rs`, `syscall/ns.rs`, user namespaces in `cred.rs`; task lifecycle, execve and the ELF loader, BKL and fast path, waiting and futexes, timers, PID 1, namespaces.
- `kernel/signal` — signals, ptrace and the user-visible ABI: `core/kernel/src/signal.rs`, `syscall/sig.rs`, `vdso.rs`, `ptrace.rs`, `syscall/ptrace.rs`, `coredump.rs`, `klog.rs`, signalfd, and in `syscall/misc.rs` uname/syslog/adjtimex/clock setting/restart_syscall; signal generation and delivery, frames and sigreturn, fault-to-signal mapping, the vDSO, ptrace, core dumps, the kernel log, the 6.1 ABI contract and ENOSYS policy.
- `kernel/mm` — guest memory (soft-MMU): `core/kernel/src/mm.rs`, `core/kernel/src/syscall/mem.rs`, the page table in `core/engine/src/c/mmu.c` and the `Mmu` wrapper, the software TLB in `core/engine/src/c/cpu.c` and `internal.h`, and the page-fault branch of the task run loop; VMAs, host backing, demand paging, TLB shootdown, fork copy-on-write, and memory introspection.

## Cross-cutting concerns
