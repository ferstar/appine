# M9 之前：跟踪停住的进程、跟踪者先取走结束（2026-10-27）

[ptrace 的第一步](40-ptrace-strace.md)留下的两处与 Linux 不同的细节，都出在用 `strace -p`、gdb 的 attach 跟踪别人的进程时（§7.6）。

- **跟踪已经停住的进程**：线程组被 SIGTSTP 这类停止信号停住、父进程已经用 wait 取走了停止事件之后，再 PTRACE_ATTACH 或者 SEIZE 它，原来 group-stop 报告成 SIGSTOP。原因是停止信号只记在"还没被父进程取走的停止事件"里。Linux 的每个线程在 `JOBCTL_STOP_SIGMASK` 里记着停住它的信号，跟父进程取没取走无关。现在线程组另记一份（`ThreadGroup::stop_signal`）。

  在原生上量了顺序，ATTACH 时还有一处不同：Linux 先报告 group-stop（SIGTSTP，没有 siginfo），再报告 ATTACH 发的 SIGSTOP。`get_signal` 每取一个信号之前先处理 `JOBCTL_TRAP_*`。appine 原来是线程在 group-stop 里被叫醒之后，直接去取挂起的信号，先报告了 SIGSTOP。

  现在 `signal::deliver` 的循环与 `get_signal` 一样，每取一个信号之前先等线程组的停止、再看 ptrace 要不要它停（group-stop、PTRACE_INTERRUPT）。原来这两步只在循环之前做一次，停止信号的默认动作另外写了一遍。
- **跟踪者不是父进程**：原来被跟踪的进程结束时，同时交给父进程（变成僵尸进程、发 SIGCHLD）和跟踪者，父进程马上就能回收它。Linux 的僵尸进程还被跟踪着时只有跟踪者看得见：
  - 只给跟踪者发 SIGCHLD；
  - 父进程的 wait 接着等（WNOHANG 返回 0），不是 ECHILD；
  - 跟踪者用 wait 取走之后（`wait_task_zombie` 的 `EXIT_TRACE`），才交给父进程、给它发 `exit_signal`；
  - 跟踪者没取走就退出时（`__ptrace_detach`），也交给父进程。

  现在 `ptrace::group_finished` 在这种情况下推迟交给父进程：进程是僵尸，记下 `exit_tracer`，父进程的 wait 跳过它。跟踪者 wait 取走（WNOWAIT 不算）或者退出时，再交给父进程（`process::tracer_released`）。父进程先退出了的话，这个僵尸进程交给 init，跟踪者取走之后由 init 回收。跟踪者 wait 取走整个进程的结束时，也与 Linux 一样报告它的资源用量（原来是 0）。
- **测试**：差分测试 `ptrace` 新增两段。
  - 子进程在自己的进程组里 raise(SIGTSTP)，父进程用 WUNTRACED 取走停止事件之后跟踪它。ATTACH 时先看到 SIGTSTP 的 group-stop（GETSIGINFO 是 EINVAL），再看到 SIGSTOP，不投递它，子进程照常执行到退出。SEIZE 时是 `PTRACE_EVENT_STOP`、siginfo 的 signo 是 SIGTSTP，PTRACE_LISTEN、SIGCONT 之后再报告一次，然后是 SIGCONT 的信号投递停止。
  - 祖父进程跟踪孙进程（Yama 也允许祖先跟踪），孙进程退出。跟踪者用 waitid 的 WNOWAIT 看到它结束时，父进程 WNOHANG 得到 0、SIGCHLD 没有挂起；跟踪者取走之后，父进程阻塞的 waitpid 才返回，SIGCHLD 挂起。另一个祖父进程看到孙进程结束、没取走就退出了，父进程阻塞的 waitpid 随之返回。

  变异检验：
  - 上一轮的二进制上，两段的每一项都与原生不同：报告 SIGSTOP；父进程先回收了，跟踪者退出之后父进程的 waitpid 是 ECHILD；
  - 只记停止信号、不改投递循环：ATTACH 那一项先看到 SIGSTOP、后看到 SIGTSTP。

  cargo test（193 个）、clippy、Android clippy、两层的 18 组差分测试通过。LTP 的 syscall 测试 957 个通过（上一轮 958），少的 getrusage03 这次跳过了，是[偶发项](../milestones.md#已知的偶发失败)（全量并发时可用内存不够）。

**推迟项**：

- 跟踪 pid 1 仍是 ESRCH，跟踪者记的仍是进程（[ptrace 的第一步](40-ptrace-strace.md)的推迟项）。
- 被跟踪的线程（不是主线程）退出之后，Linux 让它作为僵尸等跟踪者取走，取走之前父进程回收不了整个进程（`delay_group_leader`），appine 不等。只有跟踪者不是父进程、又不马上取走线程的结束时才看得出来。
