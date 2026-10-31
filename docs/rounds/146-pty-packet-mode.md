# M9 之前：PTY 的包模式（2026-10-31）

← [M9 之前：termios2（2026-10-31）](145-termios2.md) · [索引](../milestones.md) · [M9 之前：/proc/<pid>/exe 的跟踪权限（2026-10-31）](147-proc-exe-access.md) →

[终端的流控和清空](75-pty-flow.md)那一轮记下包模式（TIOCPKT）整个没有实现。第 144 轮之后筛兼容性推迟项时读 GNU screen 的源码发现它离不开包模式：`src/window.c` 的 `OpenDevice` 给每个新窗口的主设备设 TIOCPKT，失败就关掉终端、不开这个窗口（"TIOCPKT ioctl"）；读主设备时（`win_readev_fn`）把第一个字节当作包头去掉，用 `TIOCPKT_DOSTOP`、`TIOCPKT_NOSTOP` 做自动流控。appine 原来回 ENOTTY，screen 一个窗口也开不了。另外用到它的有 inetutils 的 telnetd、rlogind。

## 做法

照 6.1 的 `drivers/tty/pty.c`、`n_tty.c`：

- **ioctl**（只在主设备上，从设备上是 ENOTTY）：`TIOCPKT` 打开、关掉包模式，打开时丢掉以前的状态（`pty_set_pktmode`）；`TIOCGPKT` 查询。顺带补上主设备上另外两个原来是 ENOTTY 的：`TIOCGPTLCK`（从设备锁没锁），`TIOCSIG`（给从设备的前台进程组发 SIGINT、SIGQUIT 或 SIGTSTP，别的是 EINVAL；与按键产生的一样是内核发的）。
- **状态**（`Tty::pktstatus`，Linux 记在从设备的 `ctrl.pktstatus`）：只在主设备是包模式时记，几种状态按位或起来，读的时候一起取走。
  - `FLUSHREAD`：从设备的输入被清空（从设备上的 TCIFLUSH，任一边的 TCSETSF，没设 NOFLSH 时 INTR、QUIT、SUSP 产生信号）。
  - `FLUSHWRITE`：从设备的输出被清空（从设备上的 TCOFLUSH，产生信号时）。从设备被 vhangup 时也清空了两边，但随后 `__tty_hangup` 丢掉了所有状态，主设备什么也读不到。主设备那边的清空不报告（Linux 的 `n_tty_packet_mode_flush`、`pty_flush_buffer` 都看对面是不是包模式）。
  - `STOP`、`START`：从设备的输出被 ^S、TCOOFF 停住，或者恢复，只在真的停住、恢复时报告，两者互相清掉（`pty_stop`、`pty_start`）。主设备上的 TCOOFF 不报告。
  - `NOSTOP`、`DOSTOP`：termios 改了之后，是不是 ^S/^Q 流控（IXON 而且 VSTOP、VSTART 是默认的 ^S、^Q）变了；`IOCTL`：改之前或之后设着 EXTPROC（`pty_set_termios`）。
- **读**（`n_tty_read`）：有状态时只读到那一个字节；否则有数据时先给一个 `TIOCPKT_DATA`（0），它也占读的长度（只读 1 个字节时只得到这个 0，数据留着）。没有数据、没有状态时照常阻塞或者 EAGAIN；从设备都关掉之后先读完状态、数据，再是 EIO。长度为 0 的读返回 0（原来会阻塞或者 EIO）。
- **poll**（`n_tty_poll`）：有状态时是 POLLPRI 加 POLLIN、POLLRDNORM，select 的异常集合、epoll 的 EPOLLPRI 随之可用；记状态也算一次数据到来（边沿触发）。

## 测试

- 差分测试 `pty` 组加上包模式：从设备上的这几个 ioctl 是 ENOTTY，坏的指针 EFAULT；打开前后的读、空读、长度 0 和 1 的读；从设备上的 TCIFLUSH、TCOFLUSH、TCIOFLUSH 的状态和 poll、select 的异常集合，主设备上的清空不报告；经由主设备的 TCSAFLUSH、不清空的 TCSADRAIN；TCOOFF、再 TCOOFF、TCOON，先停后恢复只剩 START；^S、^Q（经由主设备写进去）；主设备上的 TCOOFF、TCOON；关掉 IXON（再关一次）、经由主设备打开，改 VSTOP 再改回来；EXTPROC 打开、保持、关掉；^C 清空两边、NOFLSH 时不清；关掉包模式之后没有头、状态不记，再打开时没有旧状态；EPOLLPRI；从设备关掉之后的顺序；`TIOCSIG` 没有前台进程组时、非法信号，以及让 `setsid` 之后打开从设备的子进程收到 SIGINT。`vhangup` 组加上包模式下挂断从设备（第一版照 `tty_ldisc_hangup` 报告了两种清空，与原生比出来 Linux 随后清掉了状态）。原生上写进主设备的字节经由 flip buffer 异步处理，用例先等到 POLLIN 或 POLLPRI 再读；产生信号时 Linux 只清还在 flip buffer 里的输出（[流控和清空](75-pty-flow.md)的推迟项），用例里关掉回显，不比这一处。两个执行层下都与原生逐字相同。
- 变异检查：停住时不报告 STOP，用例在 TCOOFF 那一行起不同。
- 真的 GNU screen：在 Ubuntu 26.04 的仓库（第 145 轮）里用 apt 装上 screen 4.9.1，`screen -D -m sh -c '…'`：145 轮的二进制下 screen 退出码是 0，但窗口里的命令根本没有运行；现在窗口里的命令照常运行，`tty` 是 `/dev/pts/0`、`stty size` 是 `24 80`。顺带看到 Ubuntu 26.04 的 Rust 版 coreutils 的 `tty` 输出不带结尾的换行，原生上也一样。
- cargo test（282 个）、clippy（含 Android）、格式、两层的全部差分测试通过。LTP 的 syscall 测试 1010 个通过，与上次逐项相同。

**推迟项**：

- 主设备上的 `TIOCGPTPEER`（由主设备直接打开从设备）仍是 ENOTTY：glibc 的 openpty、systemd 在失败时退回按名字打开。
