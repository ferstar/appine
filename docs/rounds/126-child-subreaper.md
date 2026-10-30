# M9 之前：PR_SET_CHILD_SUBREAPER（2026-10-31）

← [M9 之前：raw ICMP 的模拟（2026-10-30）](125-raw-icmp.md) · [索引](../milestones.md) · [M9 之前：LTP 剩下的小差异（2026-10-31）](127-ltp-leftovers.md) →

[pid 命名空间](120-pid-namespaces.md)的推迟项。`PR_SET_CHILD_SUBREAPER` 让一个进程收养后代里的孤儿：子进程退出时，它的子进程不再交给 1 号进程，而是交给祖先里最近的 subreaper。tini、dumb-init、catatonit、s6 这类进程管理程序和 `systemd --user` 靠它回收自己起的服务留下的进程。原来 prctl 对这两个选项返回 EINVAL，这些程序要么报错退出，要么收养不到孤儿；LTP 的 prctl03 因此跳过。

## 做法

- 线程组上记一个 `child_subreaper`（`core/kernel/src/process.rs`）。`PR_SET_CHILD_SUBREAPER` 的参数非 0 就设上（与 Linux 一样不看别的参数），`PR_GET_CHILD_SUBREAPER` 往参数指的 int 里写 0 或 1（写不进去是 EFAULT）。fork 不继承，execve 保留。
- 进程结束时（`finish_group` 的第 2 步）先找 subreaper：从父进程往上，找第一个设了它、还有活着的线程的进程（Linux 的 `find_new_reaper`）。只在同一层 pid 命名空间里找：与 Linux 一样比 pid 的层数，往上走到外面那层就停，外面的 subreaper 不收养命名空间里的孤儿。找不到才照旧交给所在的 pid 命名空间的 1 号进程或内置的 init。命名空间的 1 号进程自己退出时（里面的进程都被结束）不找。
- 交出去的过程不变：孤儿记到新的父进程下，已经是僵尸的再通知它一次（`reparent_leader`），父进程退出时的 `PR_SET_PDEATHSIG`、孤儿进程组的判断都照旧。

## 测试

- 差分测试 `compat` 组新增 `subreaper.c`：默认值，设成 5 读出来是 1，设回 0，读到坏指针是 EFAULT；fork 出来的子进程没有，execve 之后还在；孤儿过继给 subreaper、由它回收，父进程退出时已经是僵尸的孙子进程也由它回收；过继给最近的 subreaper，它退出之后交给再往上的；清掉之后孤儿给 1 号进程。`namespaces` 组新增：外面的进程是 subreaper 时，新 pid 命名空间里的孤儿仍过继给里面的 1 号进程，里面的 subreaper 照常收养里面的孤儿。两个执行层下都与原生相同。
- 变异检验（第一层）：不找 subreaper，`compat` 组的收养和回收都对不上，`namespaces` 组里面的 subreaper 收养不到；找的时候不比 pid 命名空间的层数，命名空间里的孤儿过继给了外面的 subreaper，1 号进程退出时等它们被回收、外面的进程又只等 1 号进程，`namespaces` 组卡住。用例开发时还遇到两处用例自己的问题：fork 之前 stdout 缓冲着的内容被子进程又输出一遍（改成行缓冲）；最后用 `waitpid(-1, WNOHANG)` 查有没有剩下的子进程，原生一侧偶尔得到 0，去掉了。
- cargo test、clippy（含 Android）、两层的全部差分测试通过。LTP 的 syscall 测试 989 个通过（上次 989）：prctl03 由跳过变成通过；futex_wait05 这次失败（[偶发项](../milestones.md#已知的偶发失败)），其余逐项相同。

**推迟项**：无。
