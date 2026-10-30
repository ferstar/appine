# M9 之前：vhangup（2026-10-31）

← [M9 之前：命名空间文件留住命名空间（2026-10-31）](129-namespace-files.md) · [索引](../milestones.md) · [M9 之前：tmpfs 的用量和限制（2026-10-31）](131-tmpfs-usage.md) →

[关掉主设备时挂断从设备](104-pty-hangup.md)那一轮的推迟项：`vhangup(2)`、`TIOCVHANGUP` 原来没有（ENOSYS、ENOTTY），程序自己不能挂断终端。util-linux 的 `login` 在登录之前调 vhangup 赶走终端上留下的进程（它不看返回值，没有的话旧进程照样能读写这个终端），agetty 的 `--hangup`、systemd 用 `TIOCVHANGUP` 做同样的事。App 自己登录不经过 login，只有用户自己运行 login、agetty 时才用得到。LTP 的 vhangup01、vhangup02 因此跳过。

## 做法

Linux 的 `tty_vhangup` 和关掉最后一个主设备时的挂断都走 `__tty_hangup`，区别在于后者还删掉 `/dev/pts/<n>`（`pty_close` 里的 `devpts_pty_kill`），终端从此不能再用；vhangup 只把挂断时打开着的文件换成 `hung_up_tty_fops`，节点还在，重新打开的照常能用。appine 原来用终端上的一个标志表示挂断（`Tty::hung_up`），对所有从设备的文件都有效，表示不了"之前打开的挂断、之后打开的不挂断"。

- **按打开文件判断**（`core/kernel/src/tty.rs`）：终端记着两边各被 vhangup 过几次（`Tty::vhangups`），打开文件记着打开时的次数（`PtyEnd::born`），两者不同就是挂断了（`Tty::hung`）；从设备在主设备都关掉之后照旧全部挂断。读写、ioctl、poll 都改成按打开文件看：读返回 0，写和 ioctl 是 EIO（TIOCSPGRP 是 ENOTTY），poll 总是就绪，与原来的从设备挂断相同；阻塞在读、写里的线程被叫醒之后也看得到。
- **`vhangup`**（`syscall/tty.rs`）：从设备这一边的次数加一，没读的输入丢掉（Linux 关掉行规程，重新打开时是新的），会话部分与原来的挂断共用（`detach`）：以它为控制终端的进程都失去控制终端，会话组长收到 SIGHUP 和 SIGCONT，记下前台进程组等它退出时再发。终端记着挂断之后还没有重新打开过（`Tty::hupped`，Linux 的 `TTY_HUPPED`），这时再挂断什么也不做；从设备重新打开（包括 `/dev/tty`）时清掉。
- vhangup(2) 要 `CAP_SYS_TTY_CONFIG`，`TIOCVHANGUP` 要 `CAP_SYS_ADMIN`，都按 Linux 的 `capable()` 只看开机时的用户命名空间。vhangup 挂断调用者的控制终端，没有控制终端时什么也不做；调用者是会话组长时自己也收到 SIGHUP、SIGCONT（login 先把 SIGHUP 设成忽略）。`TIOCVHANGUP` 挂断这个文件所在的那一边：主设备上的只挂断主设备的文件、丢掉从设备还没被读走的输出，主设备没有会话，从设备照常能用。

## 测试

- 新的差分测试组 `vhangup`（原生一侧加 `CAP_SYS_TTY_CONFIG`、`CAP_SYS_ADMIN`）：没有控制终端时 vhangup 什么也不做；会话组长打开从设备作控制终端，会话里另一个进程组也开着它，主设备写进一行不读，组长 vhangup：组长收到 SIGHUP、SIGCONT，另一个进程收不到，两个旧的文件读到 EOF、写和 `tcgetattr` 是 EIO、`tcsetpgrp` 是 ENOTTY、TIOCSCTTY 是 EIO、`isatty` 为假、poll 全部就绪，`/dev/tty` 是 ENXIO，再 vhangup 什么也不做；重新打开从设备：又成了控制终端，挂断之前的输入没了，之后写进来的读得到，旧文件仍是挂断的；在新文件上 TIOCVHANGUP 同样挂断，再来一次是 EIO；主设备读得到从设备写出的；之后不带 `O_NOCTTY` 再打开照常能用；普通用户的 vhangup、TIOCVHANGUP 是 EPERM；主设备上的 TIOCVHANGUP：主设备读到 0、写是 EIO、poll 全部就绪，从设备照常能写，新开的 `/dev/ptmx` 不受影响。两个执行层下都与原生相同。
- 变异检验：挂断时不丢掉没读的输入，重新打开之后读到的是挂断之前写进来的那一行；重新打开时不清掉 `hupped`，在新文件上的 TIOCVHANGUP 什么也没做，组长没有再收到信号（用例接着读那个没挂断的文件就卡住了，之后改成读之前先设成非阻塞）。
- cargo test、clippy（含 Android）、两层的全部差分测试通过。LTP 的 syscall 测试 1001 个通过（上次 998）：vhangup01、vhangup02 由跳过变成通过；mmap21_02 这次通过（[偶发项](../milestones.md#已知的偶发失败)），其余逐项相同。

**推迟项**：

- 从设备挂断之后、重新打开之前主设备写进来的输入照常进输入队列，马上回显；Linux 的行规程这时已经关掉，数据留在缓冲区里，重新打开之后才处理、回显。
