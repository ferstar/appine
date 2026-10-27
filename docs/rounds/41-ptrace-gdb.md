# M9 之前：ptrace 的第二步，gdb（2026-10-27）

← [M9 之前：ptrace 的第一步，strace（2026-10-27）](40-ptrace-strace.md) · [索引](../milestones.md) · [M9 之前：epoll 的边沿（2026-10-27）](42-epoll-edges.md) →

[上一步](40-ptrace-strace.md)之后 strace 能用了，gdb 还不行：下不了断点（`Cannot insert breakpoint 1. Cannot access memory at address 0x7e8`），也不能单步。这一轮补上 gdb 要的部分（§7.6）。

- **单步**（PTRACE_SINGLESTEP）：引擎新增 `appine_cpu_step`，用解释器执行 pc 处的一条指令，不理会 kick（`Cpu::step`）。跟踪者用 PTRACE_SINGLESTEP 让线程继续之后（`Task::stepping`，运行循环不持 BKL 时读它），运行循环每次只执行一条指令：
  - 执行完了（`ic ivau` 也算）投递 SIGTRAP，`si_code` 是 `TRAP_TRACE`，地址是下一条指令，与 arm64 Linux 的单步异常一样；访存出错、brk、未定义指令投递它们自己的信号。
  - 执行的是 svc：系统调用做完之后报告，这次的 SIGTRAP 是 `SI_USER`，没有发送者；下一次 PTRACE_SINGLESTEP 不执行指令，在原地再报告一次 `TRAP_TRACE`。这两点都是照原生 arm64 Linux 量出来的（硬件的单步状态在 svc 之后还挂着）。
  - 单步时系统调用不走不持 BKL 的路径。
- **改写被跟踪进程的内存**（PTRACE_POKETEXT/POKEDATA、`/proc/<pid>/mem`）：`Mm::write_force` 照 Linux 的 `FOLL_FORCE`：没有写权限的私有映射也能写（程序的代码段：下断点），写时复制和与别的地址空间共用的只读文件映射先复制成自己的一份（`write_kernel`）；没有写权限的共享映射不能写（EIO）。写过的地方作废翻译，用着这个地址空间的线程先刷新 TLB。写过的映射之后不再算作可信的翻译来源（`Mm::code_sources` 不要写过的），改过的代码不会经由翻译池带给下一个执行同一个程序的进程。
- **`/proc/<pid>/mem`**：位置就是地址，按页读写，碰到不能访问的页停下，一字节也没读写时是 EIO（Linux 的 `mem_rw`），写与 POKETEXT 相同。打开时要求能跟踪那个进程，权限 0600。gdb 读写被调试进程的内存都用它。
- **`/proc/<pid>/auxv`**：加载器把栈上的 auxv 留一份在地址空间里（`Mm::auxv`，fork 继承）。gdb 靠里面的 `AT_ENTRY`、`AT_PHDR` 算出 PIE 程序的加载地址，原来断点打在没有重定位的地址上。
- **`/proc/<pid>/task/<tid>/`** 下补上与进程相同的项（maps、smaps、mem、auxv、cmdline、environ、statm、mounts、mountinfo、cwd、exe、root）：gdb 读线程的 maps 和 mem。
- **硬件断点和观察点**：`NT_ARM_HW_BREAK`、`NT_ARM_HW_WATCH` 仍然报告 0 个槽，gdb 用软件的：断点写 brk，观察点每条指令单步之后比较。
- gdb 下断点、run、bt、print、next、finish、软件观察点、stepi、`x/i`、`catch syscall`、多线程程序的断点和 `info threads`，输出与原生 Linux 一致（差分测试，两个执行层）。另外在 appine 上试过（没有和原生对比）：`gdb -p` 挂到阻塞在系统调用里的 sleep、取 bt 再 detach，sleep 照常结束；python 收到 SIGSEGV 时 gdb 停下并给出栈。
- **性能**：上一轮的二进制和这一轮的交替跑，shell 起进程 5 次的总和 0.88–0.91 秒对 0.86–0.89 秒，`git status`、python 循环不变。
- **测试**：新的差分测试 `gdb`：一个循环程序上的断点、单步、看变量、软件观察点、stepi，先执行过（已经翻译）再下的断点，多线程程序上的断点，`catch syscall writev`（musl 的 stdio 用的是 writev）；加载地址、pid 归一化，两边都用软件观察点（`set can-use-hw-watchpoints 0`，原生有硬件观察点）。差分测试 `ptrace` 新增：PTRACE_POKETEXT 在已经翻译过的函数上写 brk，命中之后恢复原来的指令再单步四条，经由 `/proc/<pid>/mem` 读写断点，单步 svc，对只读的共享映射强制写入是 EIO，auxv 的 `AT_ENTRY`、`AT_PAGESZ`。变异检验：去掉强制写入之后的翻译作废，两组都失败（先执行过的函数上的断点不起作用）；最初的用例只在代码还没执行过时下断点，抓不到这个变异，所以加了"先执行过"的场景。guest 测试 `posix.c` 的 ptrace 一项新增单步。用 Ubuntu 24.04 的 gcc 13 按 build.rs 的参数编译引擎（-O3、-Werror）。cargo test（178 个）、clippy、Android clippy、两层的 16 组差分测试通过。LTP 的 syscall 测试 952 个通过（上一轮 950），变化的 futex_wait05、getrusage03、shmctl01（这次通过）和 mmap21_02（这次失败）都是[偶发项](../milestones.md#已知的偶发失败)。mmap21_02 这次的日志查出了原因：LTP 的库把结果放在 `/dev/shm/ltp_<程序名>_<pid>`，mmap21_01 和 _02 是同一个程序，在各自的 appine-run 里都是 pid 2，并发运行时共用仓库的 `/dev/shm`，一个删掉了另一个的文件；单独重跑 5 次都通过。

**推迟项**：

- 没有硬件断点和观察点：gdb 的观察点靠逐条指令单步，每条指令都要停下来让 gdb 比较一次。→ 已补上（M9 之前：硬件断点和观察点）：解释器检查，有它们的线程只用 Tier 0
- 单步进入信号处理函数时的行为没有和 Linux 对照过。→ 对照了，原来不同：Linux 停在处理函数的第一条指令上、不执行它，appine 执行了才停；已改成相同（M9 之前：ptrace 和 procfs 的细节）
- `/proc/<pid>/mem` 读写的是那个 pid 现在的地址空间；Linux 拿的是打开时的，进程 execve 之后旧的 fd 读不到新程序。`/proc/<pid>/auxv` 不检查权限（Linux 要求能以只读方式跟踪那个进程）。→ 都已补上（M9 之前：ptrace 和 procfs 的细节），maps、smaps、pagemap、environ 也一样要求能跟踪
- 仍然没有 core dump，gdb 不能事后分析。→ 已补上（M9 之前：core dump）
