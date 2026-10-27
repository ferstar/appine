# M9 之前：硬件断点和观察点（2026-10-27）

← [M9 之前：core dump（2026-10-27）](43-coredump.md) · [索引](../milestones.md) · [M9 之前：ptrace 和 procfs 的细节（2026-10-27）](45-ptrace-procfs.md) →

[ptrace 的第二步](41-ptrace-gdb.md)让 `NT_ARM_HW_BREAK`、`NT_ARM_HW_WATCH` 报告 0 个槽，gdb 的 `watch` 只能退回软件观察点：每执行一条指令都停下来让 gdb 比较一次值，在真实程序上基本不能用；`hbreak` 也不能用。这一轮实现硬件断点和观察点（§7.6）。

- **槽和控制字**：与常见的 Cortex-A 核一样报告 6 个断点、4 个观察点（`dbg_info` 的调试架构仍是 ARMv8）。设置时按 Linux 的 `ptrace_hbp_fill_attr_ctrl`、`hw_breakpoint_arch_parse` 检查和规范化（`syscall::ptrace::hw_slot`）：停用的原样记下；启用的字节选择（BAS）必须是连续的一段，观察点的种类是读、写或者两者，断点只能是执行、4 字节，否则 EINVAL；起点是地址加上 BAS 的偏移，再按 8 字节（断点按 4 字节）对齐、把偏移挪回 BAS，特权级记成 EL0，读回的就是规范化之后的值。超出槽数的项不是零时 ENOSPC。
- **谁检查**：状态属于线程（`Trace::hw`），跟踪者让线程继续时交给 CPU（`HwDebug::apply` → `CpuState::set_hw_debug`，引擎的 `struct appine_cpu` 末尾多了断点地址和观察点的 [起点, 终点)、种类）。只有解释器检查：每条指令执行之前比较断点；每次访存在页表检查之后、产生副作用之前比较观察点（`watch_hit`），普通读写、原子操作、独占访问、LDAR/STLR、SIMD 的多寄存器访问、`dc zva` 都经过它，原子的读改写算读也算写，与硬件一样。有断点或观察点的线程只用 Tier 0 执行（运行循环看 `hw_debug_active`），同一进程的别的线程和别的进程照常用 Tier 1；没有设置时解释器每次访存多比较一次计数。
- **命中**：指令不执行，以新的故障类型（`FaultKind::Watchpoint`、`HwBreakpoint`）退出，内核投递 SIGTRAP，`si_code` 是 `TRAP_HWBKPT`，`si_addr` 是断点地址或者访问的起点（不对齐的一个字节的观察点报告那个字节，16 字节的 SIMD 存储碰到后一半时报告起点），没有 esr_context，与 arm64 Linux 的 `ptrace_hbptriggered` 一样。越过它由调试器负责：gdb 在 aarch64 上把观察点当作"不能单步"的，先停用、单步、再恢复。
- **生命周期**：fork、clone 出来的线程没有（新的 CPU 状态是清零的，复用的 CPU 在 `appine_cpu_init` 清掉），execve 时清掉（Linux 的 `flush_ptrace_hw_breakpoint`），PTRACE_DETACH 不清（与 Linux 相同）。core 文件里的 `NT_ARM_HW_BREAK`、`NT_ARM_HW_WATCH` 是线程当时的设置。
- **与原生对照**：本地 colima 虚拟机里的原生 gdb 用的是硬件观察点。gdb 会话（写观察点两次，结构体里不对齐的两个字节，读观察点，读写观察点，`hbreak`，另一个线程触发的观察点和 `info threads`）在两个执行层下都与原生逐行一致。
- **性能**：上一轮之前的二进制和这一轮的交替跑三次 `sha256sum` 加 `gzip`（2 MB）：Tier 0 2.80–2.96 秒对 2.75–2.82 秒，Tier 1 0.11 秒对 0.10–0.11 秒，没有变慢（`target/mem/hwdebug-ab.sh`）。
- **测试**：差分测试 `gdb` 新增上面的硬件观察点会话；`ptrace` 新增一个跟踪器直接读写 regset：槽数、三种非法的控制字、读回规范化的控制字，写、读、不对齐的一个字节的观察点停在访问之前（`PTRACE_PEEKDATA` 看到数据还没写）、`si_addr` 是访问的地址，停用之后单步完成访问，断点停在指令上，原子的读改写触发只读的观察点，SIMD 存储报告访问的起点。变异检验：改动之前的二进制上 gdb 报告的是软件观察点，跟踪器读到 0 个槽，两组都失败。单元测试新增控制字的检查和规范化、观察点换算成的范围。用 Ubuntu 24.04 的 gcc 13 按 build.rs 的参数编译引擎（-O3、-Werror）。
  - 跑全部差分测试时 `gdb` 组失败过一次，查下来是[第二步](41-ptrace-gdb.md)就有的竞争：程序最后打印的一行和 gdb 报告线程退出的 `[LWP N exited]` 写同一个管道，谁先谁后不一定（原生一侧同样可能），连跑几次 Tier 1 能遇到一次。threads 的会话改为 `set print thread-events off`，新的硬件观察点会话看完 `info threads` 就 kill，之后两个执行层连跑 11 次都通过。
  - cargo test（186 个）、clippy、Android clippy、两层的 17 组差分测试通过。LTP 的 syscall 测试 957 个通过（上一轮 955）：futex_wait05、mmap21_02、shmctl01 这次通过，times03 这次失败（全量运行期间并发跑了 gdb 组），单独重跑 3 次都通过，都是[偶发项](../milestones.md#已知的偶发失败)。

**推迟项**：

- 有断点或观察点的线程整个用解释器执行，比 Tier 1 慢一个数量级（仍比软件观察点快得多）。要在 Tier 1 里做，得让翻译后的代码在观察的页上走慢路径（TLB 不填这些页）、在断点处结束块。
- 观察点不支持地址掩码（控制字的 MASK 位，Linux 也不支持）；断点不支持上下文匹配、关联的断点。
- 内核替 guest 访问内存（`read`、`write` 的缓冲区等）不触发观察点，与 Linux 一样（Linux 只在用户态的访问上触发）。
