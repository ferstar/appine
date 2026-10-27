# M9 之前：被屏蔽的信号只叫醒等它的线程（2026-10-27）

← [M9 之前：宿主 stdio 上的阻塞读只等宿主 fd（2026-10-27）](55-host-stdio.md) · [索引](../milestones.md) · [M9 之前：跟踪停住的进程、跟踪者先取走结束（2026-10-27）](57-ptrace-stopped-reaped.md) →

[按对象唤醒](49-targeted-wakeups.md)起一直留着的推迟项：发给进程的信号被所有线程屏蔽（或者发给线程的信号被它屏蔽）时，叫醒所有等待者，为的是让 sigtimedwait 里的线程重新检查。[上一轮](55-host-stdio.md)查出它很常见：musl 的 fork 在 clone 前后屏蔽所有信号，fork 的那一刻正好有子进程退出，SIGCHLD 就是被所有线程屏蔽的。shell 起管道、构建工具并行起进程时都会碰到，每次都把系统里所有 poll、epoll、阻塞的读写、等锁的线程叫醒一遍。微基准（`target/herd/sockherd.c`）加了两项，8 个进程空等，epoll 和 recv 两种空等方式各量两次，每次操作的微秒数和空等的进程被叫醒的次数：

| 操作 | 原生 | 之前 | 之后 |
|---|---|---|---|
| 屏蔽着 SIGUSR1 给自己发一个，再用 sigwaitinfo 取走 | 0.7 | 8.8–16.3（每次叫醒 2.8–3.2 次） | 0.4–0.5（0 次） |
| 屏蔽着 SIGCHLD 时 fork，子进程退出，回收 | 56.8 | 99–120（7.7–8.5 次） | 86–104（0 次） |

- **信号的通道**（`wake::SIGNAL`）：被屏蔽的信号挂起时只请求这个通道。听它的只有三类：sigtimedwait；signalfd 的阻塞读（另外登记 signalfd 自己，改掩码时请求它）；poll、epoll（包括嵌套的 epoll，以及外层边沿触发、只等新事件的 epoll）关注了 signalfd 的等待者，与关注了 pidfd 才听子进程的通道一样。没有被屏蔽的信号照旧直接叫醒挑中的线程。
- 仍请求所有通道的：SIGCONT 让停住的进程继续、SysV IPC、AIO 完成、关闭控制台。
- **效果**：见上表。标准基准与上一轮的二进制交替各跑两次，都在噪声范围内：shell 0.23–0.24 秒对 0.23–0.24 秒，gcc 2.21–2.22 对 2.25–2.42，python 1.33–1.35 对 1.35–1.64，go build 6.01–6.02 对 6.01–6.13，Maven、Gradle 的热构建 5.83–5.89、1.35–1.39 对 5.81–5.86、1.33–1.38（前面是这一轮；上一轮的 2.42、1.64 都出在第一次）。
- **测试**：差分测试 `wakeups` 的空闲部分，别的进程的动作加了两项：给自己发被屏蔽的 SIGUSR1 再用 sigwaitinfo 取走 200 次，屏蔽着 SIGCHLD 起子进程 30 次。

  叫醒部分加了 8 项：
  - sigtimedwait 分别被另一个进程的 kill、本进程另一个线程的 pthread_kill（发给线程、被它屏蔽）、POSIX 定时器的信号、子进程退出的 SIGCHLD 叫醒；
  - 另一个进程 kill 之后，signalfd 的阻塞读、epoll、嵌套的 epoll 醒来；
  - 外层以边沿触发关注内层 epoll，内层关注 signalfd，已经报告过一次之后又来了一个信号，再报告一次。

  父进程先屏蔽这些信号，让子进程继承，免得父进程发来的信号赶在子进程屏蔽之前到达。

  变异检验：
  - 上一轮的二进制上，12 个空闲的进程都被叫醒；
  - sigtimedwait 和 signalfd 的读不听信号的通道、只等新事件的嵌套 epoll 不看 signalfd：四项 sigtimedwait、signalfd 的读、边沿那一项等不到；
  - poll、epoll 有 signalfd 时也不听：epoll 的三项等不到；
  - 发给线程、被它屏蔽的信号不请求：pthread_kill 那一项等不到；
  - 被所有线程屏蔽的进程信号不请求：其余七项等不到。
  - cargo test（193 个）、clippy、Android clippy、两层的 18 组差分测试通过。LTP 的 syscall 测试 958 个通过（上一轮 957），多出的 shmctl01 是[偶发项](../milestones.md#已知的偶发失败)。

**推迟项**：

- 信号的通道是全局的：任何进程里被屏蔽的信号都叫醒系统里所有 sigtimedwait、signalfd 的等待者，包括别的进程的。可以按线程组、线程登记对象，只叫醒目标进程里的。
