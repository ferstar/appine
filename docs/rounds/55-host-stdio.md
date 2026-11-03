# M9 之前：宿主 stdio 上的阻塞读只等宿主 fd（2026-10-27）

[锁和宿主 socket](53-locks-host-sockets.md)那一轮留下的推迟项：`appine-run` 交给 guest 的宿主 stdio（不带 `--pty`、stdin 不是终端时，`FileKind::Host`）上的阻塞读，用的是听所有通道、不限对象的等待，系统里每一次管道、PTY 的读写都会叫醒它。只影响 `appine-run`（开发、cargo test 和差分测试都用它），App 里没有这种文件。量下来（`target/herd/stdinidle.c`）：子进程阻塞在 `read(0)` 里，stdin 是没有数据的宿主管道；父进程自己写、读一个管道 2000 次，子进程被叫醒 137–451 次；与另一个进程来回 2000 次，被叫醒约 2600 次。

- **只等宿主 fd**：等待时听文件类的通道，但不登记任何对象。对象上的修改不再叫醒它，请求所有通道的修改（比如 Machine 关闭）仍会叫醒它。宿主 fd 上的写不经过等待机制（放开 BKL 直接调用宿主的 write），不用改。
- **效果**：上面两项都降到 0 次。标准基准与上一轮的二进制交替各跑两次，都在噪声范围内（这些基准的 guest 也用宿主 stdio，但不读 stdin）。
- **测试**：rootfs 测试新增一个用例：guest 里 dd 阻塞在读宿主 stdin（dup 到 fd 3）上，shell 用内建的 `echo`、`read` 在一个 FIFO 上写读 200 次，dd 的自愿上下文切换次数不变；之后往 stdin 写一行，照常读到。上一轮的二进制上，dd 被叫醒 196–201 次。开始数之前先等 dd 停在读里：已经 exec 成 dd、状态是 S、切换次数 0.2 秒没变（全量测试并行时它可能还在启动）。读这些状态也只用 shell 的内建命令。中途改过两次：
  - 第一版用 busybox 的 `read` 内建命令读 stdin，改动前后都是 0 次：它先 poll，poll 的等待者登记的对象列表是空的，本来就不会被对象上的修改叫醒。所以改用直接调用 read 的 dd。
  - 第二版的流量是 50 次 `echo x | cat`。单独运行时是 0 次，全量 cargo test 时 Tier 0 下每次都有 1–7 次。原因是 musl 的 fork 在 clone 前后屏蔽所有信号（`APPINE_STRACE` 看到 clone 之前两次 `SIG_BLOCK`、之后两次 `SIG_SETMASK`，等子进程的 wait4 不屏蔽）：shell 起管道里的第二个进程时第一个正好退出，SIGCHLD 就被所有线程屏蔽，按规则叫醒所有等待者（见推迟项）。Tier 0 下 fork 慢，这个窗口大。`target/herd/stdinidle2.c` 证实了这一点：父进程屏蔽 SIGCHLD 时起 100 个子进程，阻塞在 stdin 上的读者被叫醒 100 次，装了处理函数、不屏蔽时 0 次。所以改成测量期间不起子进程的 FIFO 读写。
  - cargo test（193 个，最终的测试连续三次全量运行都通过）、clippy、Android clippy、两层的 18 组差分测试通过。LTP 的 syscall 测试 957 个通过，与上一轮逐项相同。

**推迟项**：

- 发给进程的信号被所有线程屏蔽时仍叫醒所有等待者（[按对象唤醒](49-targeted-wakeups.md)起的推迟项），这一轮发现它比原来估计的常见：musl 的 fork 在 clone 前后屏蔽所有信号，进程 fork 的那一刻正好有子进程退出就会触发。shell 起管道、构建工具并行起进程时都可能遇到，每次都把系统里所有 poll、epoll、阻塞的读写、等锁的线程叫醒一遍。→ 已改：只叫醒 sigtimedwait、signalfd 的等待者（M9 之前：被屏蔽的信号只叫醒等它的线程），仍是全局的通道
