# 9. TTY 与终端

- **PTY 和行规程完全在进程内实现**（`core/kernel/src/tty.rs`）。借用宿主 PTY 的话，宿主的 `ISIG` 会把信号发给宿主进程组，而宿主根本不知道 guest 进程组的存在。实现照 Linux 的 PTY：termios（含 termios2；速率只记下不起作用）、包模式（`TIOCPKT`）、作业控制（`ISIG`、`setsid`/`TIOCSCTTY`/`tcsetpgrp`、`SIGTTIN`/`SIGTTOU`、`SIGWINCH`）、挂断（最后一个主设备关掉时照 `tty_vhangup`；`vhangup(2)`、`TIOCVHANGUP`）。关闭文件在 Drop 里拿不到内核，所以 `PtyEnd` 记下主设备都关掉了的终端，由放开 BKL 的线程在放开之前挂断。
- guest 可以通过 `/dev/ptmx` 自己申请 PTY，tmux、screen、`script`、sshd 都依赖这一点。
- App 里每个终端标签页对应一个 `/dev/pts/N`，在上面启动一个新会话和登录 shell。
- **终端 UI**：把 Termux 的 `terminal-emulator` + `terminal-view` vendor 进仓库，替换掉原来的 `TerminalSession`（它是 `final` 类，并且写死了 `JNI.createSubprocess`）。新实现经由 `Pty` 接口接到进程内 PTY 的主设备（`Machine` 的控制台）。扩展键栏属于 GPLv3 的 `termux-app` 主模块，所以重写了一份（`ExtraKeysView.kt`）。来源和改动记在 `android/README.md`。
- **开发用的 CLI 前端** `appine-run` 把宿主终端切到 raw 模式，把它当作 PTY master 使用，宿主的 SIGWINCH 变成 `TIOCSWINSZ`。`--pty` 在宿主 stdin 不是终端时也这样做，测试就是经由管道发送 ^C、^Z 这些字节；stdin 读到结束之后，与 `script` 一样等程序读完已经送进去的输入，再往控制台送一个 VEOF。
