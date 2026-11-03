# M9 之前：setns 经由 pidfd（2026-10-31）

[挂载命名空间](118-mount-namespaces.md)那一轮起的推迟项：setns 只接受命名空间文件，pidfd 是 EINVAL。审推迟项时读 util-linux 的源码发现这会让 nsenter 用不了：2.41 起 `nsenter -t <pid>` 在内核版本高于 5.7 时用 pidfd_open 打开目标进程，再 setns(pidfd, 要进的那几种) 一次进去，失败了直接报 "reassociate to namespaces failed" 退出，不退回逐个打开 `/proc/<pid>/ns/*`。appine 报告 6.1、pidfd_open 也有，所以 Alpine 3.24（util-linux-misc 2.42.3）、Debian 13（2.41.5）上的 nsenter 进不了任何进程的命名空间，比如 bwrap 的沙箱；Ubuntu 24.04 的 2.39 和 busybox 的 nsenter 走路径，不受影响。

## 做法

按 Linux 6.1 的 `setns`、`prepare_nsset`、`validate_nsset`、`commit_nsset`（`core/kernel/src/syscall/ns.rs`），命名空间文件和 pidfd 走同一套：

- **要换哪些**：命名空间文件只有它那一种，`nstype` 是 0 或者与它相同；pidfd 的 `nstype` 是 `CLONE_NEW*` 的组合（含 `CLONE_NEWTIME`），0 或者有别的位是 EINVAL，别的 fd 也是 EINVAL。pidfd 取的是那个进程的命名空间的快照：进程已经被回收、主线程已经退出（Linux 上它的 nsproxy 没了）时 ESRCH，不能像 ptrace 那样读它（`PTRACE_MODE_READ_REALCREDS`，与 pidfd_getfd 用同一个检查）时 EPERM；pid 命名空间取它自己所在的那个，不是给子进程用的。
- **先检查、再一起换**：按用户、挂载、UTS、IPC、pid、cgroup、网络、time 的顺序检查每一种（各自的 `install`），一种不行就返回它的错误，前面检查过的也都不换；都行了再换（凭据，根目录和当前目录，IPC 换之前执行 SEM_UNDO 的撤销，UTS、IPC、网络、cgroup，给子进程用的 pid 命名空间）。原来各种命名空间的检查和修改写在一起，拆成了 `validate` 和 `commit`。
- **一起进用户命名空间**：后面几种要求"自己的用户命名空间里有 `CAP_SYS_ADMIN`"（`mntns_install` 还要 `CAP_SYS_CHROOT`）时，按进去之后的那个用户命名空间算（Linux 的 `nsset->cred`），能力仍看原来的凭据。所以建立那个用户命名空间的用户（属主）不用 `CAP_SYS_ADMIN` 也能一次进它和属于它的别的命名空间，只进 UTS 却是 EPERM。
- **挂载命名空间和共用的 fs**：只换挂载命名空间时，fs 与别的线程、进程共用是 EINVAL（原来就是这样）；连同别的一起换时 Linux 在临时复制的 fs 上检查（`prepare_nsset`），总是只有自己在用，所以多线程的进程也能换，这里照样不查。
- **time 命名空间**：只有开机时的那个，换过去什么也不做，但与 Linux 的 `timens_install` 一样多线程的进程是 EUSERS（原来经由命名空间文件时也成功，一并改了），再要在开机时的用户命名空间和自己的用户命名空间里有 `CAP_SYS_ADMIN`。新加了 `EUSERS`。

## 测试

- 新的差分测试组 `setns-pidfd`（docker 一侧加 `SYS_ADMIN`、`NET_ADMIN`，关掉 AppArmor，与 `nsfs` 组相同）：
  - Python：标志为 0、有别的位、对自己的 pidfd 进自己的各种命名空间（用户命名空间是 EINVAL、time 可以）、普通文件、坏的 fd、命名空间文件配上不同的标志；一次进另一个进程的挂载、UTS、IPC、网络、cgroup 命名空间之后各个链接、主机名、当前目录（换成根），只进挂载命名空间；多线程时只进挂载命名空间是 EINVAL、连同 UTS 一起可以，time 命名空间（pidfd 和文件）是 EUSERS；进新的 pid 命名空间之后自己的不变、给子进程用的是它的，子进程在里面是 2 号；从里层的 pid 命名空间进外层的是 EINVAL，连同 UTS 一起要求时 UTS 也不换；去掉有效集里的 `CAP_SYS_ADMIN` 之后只进 root 建的用户命名空间的 UTS 是 EPERM、连同用户命名空间一起可以（之后有全部能力），多线程时连同用户命名空间是 EINVAL、UTS 也不换；去掉 `CAP_SYS_PTRACE` 之后对不可 dump 的进程是 EPERM；僵尸进程和已经回收的是 ESRCH。
  - util-linux 的 nsenter：`-t` 进 UTS、网络（只有 lo），`-m -u -i -n` 和 `-a` 之后各种命名空间与目标相同；`unshare -p -f --mount-proc` 的 1 号进程的 pid、挂载命名空间里 `$$`；`unshare -U -r -u` 的用户命名空间里的主机名、用户 ID 和映射，`-a` 时也是；util-linux 的 `unshare -r`；目标退出之后失败。
  - 两个执行层下都与原生相同；改之前的 appine 上 nsenter 每一步都是 "reassociate to namespaces failed: Invalid argument"。
- 差分测试的软件包加了 util-linux-misc（它换掉 busybox 的 nsenter、unshare、setsid、taskset 等），`nsfs`、`namespaces` 组里原来测 busybox 的 `unshare`、`nsenter` 改成显式调用 `busybox unshare` 这样；本地的仓库要 `apk add util-linux-misc` 再用 `-r` 重新导出镜像。
- 变异检验：后面几种按原来的用户命名空间算能力时，"user and uts" 那一行变成 EPERM；连同别的一起换挂载命名空间时也查 fs 共用，"threaded mnt and uts" 变成 EINVAL。
- cargo test、clippy（含 Android）、两层的全部差分测试通过。LTP 的 syscall 测试 1000 个通过（上次 1000）：epoll_pwait03 由失败变成通过、futex_wait05 由通过变成失败，都是检查睡眠精度的[偶发项](../milestones.md#已知的偶发失败)，futex_wait05 与[上一轮](131-tmpfs-usage.md)的二进制交替单独跑各两次都失败；setns01、setns02 照常通过，其余逐项相同。

**推迟项**：

- 连同别的一起换挂载命名空间、fs 又和别的线程共用时，Linux 只换调用的线程的挂载命名空间（在它的 nsproxy 里），共用 fs 的线程的根目录、当前目录跟着变；appine 的挂载命名空间在 `FsContext` 里，那些线程的挂载命名空间也跟着换了（它们的 `/proc/<tid>/ns/mnt` 与 Linux 不同）。
- 进 time 命名空间时只看线程数：Linux 的 `current_is_single_threaded` 还看有没有别的进程共用地址空间（`CLONE_VM` 不带 `CLONE_THREAD`、vfork 出来还没 exec 的子进程）。
- 写测试时发现 `/proc/<pid>/task/<tid>/children` 没有（Linux 的 `CONFIG_PROC_CHILDREN`，列出这个线程的子进程），用例改用 `pgrep -P`。→ 已补上（M9 之前：task 目录的 children）
