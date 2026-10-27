# M9 之前：signalfd 看调用的线程（2026-10-27）

← [M9 之前：起进程和设定时器不再叫醒所有人（2026-10-27）](51-spawn-timer-wakeups.md) · [索引](../milestones.md)

[上一轮](51-spawn-timer-wakeups.md)写差分测试时发现：Linux 的 signalfd 不属于哪个线程，poll、epoll 和读看的都是调用的线程自己和它的进程挂起的信号（`signalfd_poll`、`signalfd_dequeue` 用的是 `current`）；appine 读时看调用的线程，poll、epoll 时看的却是创建它的线程和线程组。只有多线程程序用 signalfd 等发给某个线程的信号（`pthread_kill`、`raise`、`SIGEV_THREAD_ID` 的定时器），或者 fork 之后在子进程里用，才会撞上：

- 发给创建者的线程定向信号：appine 对别的线程报告可读，它去读却得到 EAGAIN，水平触发的 epoll 一直返回，变成忙等；阻塞的读卡住。
- 发给别的线程的：Linux 对那个线程报告可读，appine 不报告，它在 poll、epoll 里一直等。
- fork 出来的子进程 poll 继承来的 signalfd：appine 看的是父进程挂起的信号，子进程收到的看不到。
- 创建它的线程退出之后：appine 只看线程组挂起的，调用的线程自己的看不到。

发给整个进程的信号（外面 kill、SIGCHLD、终端产生的）两边一样，signalfd 多半是这样用的；Go、Node.js、JVM、tokio、glib 都不用 signalfd。

- **调用者**：guest 线程在自己的宿主线程上一直执行到结束，线程开始时在线程局部记下它和它的线程组（`task::current`，弱引用；系统调用里 `Task::state` 已经被借走，拿不到线程组）。signalfd 的就绪状态和数据代数（`EPOLLET`）改为看调用的线程和它的线程组，不再记创建它的线程。不在 guest 线程上检查时（定时器线程替信号驱动的 I/O、`EPOLLEXCLUSIVE` 的独占门检查）只看创建它的线程组。
- **唤醒不用改**：发给线程的、被屏蔽的信号原来就叫醒所有等待者，各个 poll、epoll 醒来之后按自己重新判断。
- **测试**：差分测试 `compat` 新增一段：两个线程都屏蔽 SIGUSR1、SIGUSR2，线程 A 建 signalfd，线程 B 有自己的 epoll 关注它。发给 A 的信号只有 A 的 poll 报告、只有 A 读得到，B 的 poll、epoll 不报告、读到 EAGAIN；发给 B 的反过来；发给进程的两个线程都看得到，B 读走之后都不报告；B 阻塞在 poll 里时给它发信号，它醒来报告可读；fork 出来的子进程 poll 继承来的 signalfd，等到父进程发给它的信号；创建 signalfd 的线程退出之后，发给主线程的信号主线程 poll 得到、读得到。变异检验：上一轮的二进制上 5 行与原生不同（发给 A、发给 B、B 阻塞在 poll 里、fork 的子进程、创建者退出）。LTP 的 signalfd01、signalfd02、signalfd4_01、signalfd4_02（单线程）照常通过。
  - cargo test（192 个）、clippy、Android clippy、两层的 18 组差分测试通过。LTP 的 syscall 测试 956 个通过（上一轮 957）：nanosleep01 这次失败，单独重跑 3 次都失败，上一轮的二进制同样失败，量到睡 1 毫秒要 1.6 毫秒，是虚拟机这时的计时延迟（[偶发项](../milestones.md#已知的偶发失败)）。

**推迟项**：

- signalfd 的数据代数（`EPOLLET`）按调用的线程算：几个线程轮流在同一个 epoll 里以边沿触发等它时，换了线程计数就跟着变，可能多报告或者少报告一次。Linux 按进程的信号到达唤醒（`signalfd_wqh`）。
