# M9 之前：task 目录的 children（2026-10-31）

← [M9 之前：点积指令 ASIMDDP（2026-10-31）](133-asimddp.md) · [索引](../milestones.md) · [M9 之前：SHA3、SHA512 和 I8MM（2026-10-31）](135-sha3-sha512-i8mm.md) →

[setns 经由 pidfd](132-setns-pidfd.md)那一轮写测试时发现没有 `/proc/<pid>/task/<tid>/children`（Linux 的 `CONFIG_PROC_CHILDREN`，3.5 起，Debian、Ubuntu 的内核都打开）。它列出这个线程的直接子进程，是为 CRIU 遍历进程树加的；进程在 fork、退出时它可能漏掉一些，所以 ps、pstree、`pgrep -P`、psutil 都扫 `/proc/*/stat` 的父进程号，差分测试镜像里的程序没有一个读它，用得到的主要是手写的脚本（`cat /proc/$pid/task/*/children` 找子进程、递归结束一棵进程树）。原来 appine 里是 ENOENT，这样的脚本会当作没有子进程。

## 做法

- **按线程记父进程**：Linux 的子进程挂在建立它的线程（`real_parent`）的 `children` 链表上，按线程列；appine 原来只按进程记子进程。`ThreadGroup` 加上 `parent_thread`（`core/kernel/src/process.rs`），在 Linux 改 `real_parent` 和链表的地方跟着改：
  - fork、clone 时是调用的线程；`CLONE_PARENT` 时是调用者的父线程（`copy_process`）。
  - 线程退出、进程还有别的线程时，它建的子进程交给进程里还活着、没有正在被结束的第一个线程（主线程在最前面，`find_alive_thread`），排到子进程列表的最后（`forget_original_parent` 的 `list_splice_tail_init`）。
  - 进程结束时孤儿过继给 subreaper、pid 命名空间的 1 号进程或者 init，父线程是那个进程里还活着的第一个线程。
  - 不是主线程调用 execve 时，别的线程退出时子进程已经交给了它，它接过主线程的编号，这些子进程的父线程跟着改。
- **文件**（`core/kernel/src/pseudo.rs`）：只在 `task/<tid>/` 下，`/proc/<pid>/children` 没有（与 Linux 相同）；权限 0444，不查跟踪权限。内容是父线程是它的子进程（含还没回收的僵尸进程，线程不算）的编号，按子进程列表的顺序，每个后面一个空格，没有换行（`children_seq_show`）；编号按 procfs 实例的 pid 命名空间算。

## 测试

- 差分测试 `compat2` 组新增 `children.c`：主线程建的两个、别的线程建的一个各自只在自己的文件里，线程不算，格式是"编号 空格"；`/proc/self/children` 是 ENOENT；建子进程的线程退出之后它的子进程排到主线程的最后；被结束、还没回收的僵尸进程照样列着，回收之后没了；`CLONE_PARENT` 建的孙进程算主线程的；设了 `PR_SET_CHILD_SUBREAPER` 之后收养的孤儿排在最后；非主线程 exec 之后它（接过了主线程的编号）的文件里先是它自己建的、再是原来主线程的。pid 换成名字比。`namespaces` 组加上 pid 命名空间里 1 号进程的 `task/1/children`（里面的编号）。两个执行层下都与原生相同。
- 第 132 轮装上 util-linux-misc 之后，`namespaces` 组里另外几处注释写着 busybox 的 `unshare`（pid、UTS、网络命名空间那几段）实际跑的是 util-linux 的，当时漏改了，这一轮改成显式的 `busybox unshare`（util-linux 的 unshare 在 `setns-pidfd` 组里测）。
- 变异检验：转给别的线程的子进程不排到最后，exec 之后那一行的顺序与原生不同；`CLONE_PARENT` 时记成调用者自己，孙进程不在主线程的文件里。
- cargo test、clippy（含 Android）、两层的全部差分测试通过。LTP 的 syscall 测试 1001 个通过（上次 1000）：select02 由失败变回通过（[偶发项](../milestones.md#已知的偶发失败)），其余逐项相同。

**推迟项**：

- `PR_SET_PDEATHSIG` 仍在父进程结束时才发；Linux 上"父进程"是建立它的线程，那个线程退出（比如 `pthread_exit`）就发，进程还在也一样（prctl(2)）。现在记着父线程了，可以照改。→ 已补上（M9 之前：PDEATHSIG 和 __WNOTHREAD）
- wait 系列的 `__WNOTHREAD` 接受但不起作用，仍等整个进程的子进程；Linux 只等调用的线程建的。→ 已补上（M9 之前：PDEATHSIG 和 __WNOTHREAD）；跟踪着的线程仍按进程算
