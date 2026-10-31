# M9 之前：/proc/<pid>/exe 的跟踪权限（2026-10-31）

← [M9 之前：PTY 的包模式（2026-10-31）](146-pty-packet-mode.md) · [索引](../milestones.md) · [M9 之前：用户命名空间里的 ID 换算（2026-10-31）](148-userns-ids.md) →

[挂载命名空间](118-mount-namespaces.md)那一轮记下 `/proc/<pid>/ns/*`、`root`、`cwd` 跟随时不检查能不能看那个进程，[用户命名空间](119-user-namespaces.md)那一轮补上了这三种，`exe` 一直没查：任何进程都能读别的用户的进程、不可 dump 的进程的程序路径，也能经由它打开那个程序。Linux 的 `proc_pid_readlink`、`proc_pid_get_link` 对这几种链接都先 `proc_fd_access_allowed`（`PTRACE_MODE_READ_FSCREDS`），看不了是 EACCES。

## 做法

- `Pseudo` 读 `exe` 链接时与 `root`、`cwd` 一样先 `may_read`（自己总可以；别的进程要能以 `PTRACE_MODE_READ_FSCREDS` 跟踪它：同一个用户、可以 dump，或者有 CAP_SYS_PTRACE）。appine 的 `exe` 按读出来的文字跟随，跟随也经过这里，所以 open、stat、execve 经由它都一样 EACCES；lstat 链接本身不受影响。

## 测试

- 差分测试 `namespaces` 组：setuid 成 nobody 的子进程读父进程（root）的 `exe`、经由它 open 是 EACCES，读自己的照常；它的子进程设成不可 dump 之后读自己的照常，它（同一个用户）读这个子进程的是 EACCES；新建用户命名空间之后读父进程的 `exe`、`cwd`、`root`，readlink 和 stat 是 EACCES、lstat 照常。
- 变异检查：去掉这个检查，用例在 nobody 读父进程的 `exe` 那一行起不同。
- cargo test（282 个）、clippy（含 Android）、格式、两层的全部差分测试通过。LTP 的 syscall 测试 1010 个通过：futex_wait05 这次通过、mmap21_02 这次失败，都是[偶发项](../milestones.md#已知的偶发失败)，其余逐项相同。

**推迟项**：

- `exe` 仍按读出来的文字跟随（[已删除的后缀](97-deleted-suffix.md)的推迟项）：程序文件改了名、被删除之后，经由它打开的不是原来的那个文件。
