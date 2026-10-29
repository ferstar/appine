# M9 之前：`/proc/<pid>/fd` 和 maps 里的路径解析符号链接（2026-10-29）

← [M9 之前：调试器、valgrind 和 headless Chromium（2026-10-29）](92-debuggers-valgrind-chromium.md) · [索引](../milestones.md) · [M9 之前：`/proc/<pid>/task` 的链接数（2026-10-29）](94-task-dir-links.md) →

[上一轮](92-debuggers-valgrind-chromium.md)在 Debian 上跑 valgrind，memcheck 报了几千个"未初始化"的错误（`ERROR SUMMARY: 3421 errors from 313 contexts`，应该是 3 个），开头有两行 `When reading debug info from /lib/ld-linux-aarch64.so.1: can't open file to inspect ELF header`。跟进子进程的 sh、ls 和 python 也都报了一大堆。

- **原因**：valgrind 自己加载客户程序和解释器，记下映射的文件名时读 `/proc/self/fd/<n>`，之后读调试信息时按这个名字 stat，再带着 `O_NOFOLLOW` 打开。
  - Linux 的 `d_path` 给出解析完符号链接的路径 `/usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1`。appine 记的是打开时传进来的路径，只做词法规范化，得到的是 `/lib/ld-linux-aarch64.so.1`：Debian 的 `/lib` 是指向 `usr/lib` 的链接，最后一级也是链接。`O_NOFOLLOW` 停在链接上，结果是 ELOOP。
  - 读不到 ld.so 的符号，valgrind 就替换不了 ld.so 里按字读的 `strlen` 这些函数。它们会读到字符串末尾之后的字节，每次都报未初始化。
  - `/proc/<pid>/exe` 在 [M8 补测 JVM](11-m8-jvm.md) 时已经改成解析后的路径（`Vfs::real_path`），打开的文件和解释器的映射一直没改：[M3b](05-m3b.md) 记的推迟项当时只补了 exe。
- **改动**：
  - 打开文件时记下的路径（`/proc/<pid>/fd/<n>` 显示它，mmap 之后 maps 里的名字也取它）改成按查找路径时最后找到文件的目录和名字算，与 `real_path` 用同一个函数（`Vfs::found_path`）。具体有这几种情况：
    - 符号链接都解析；
    - `..` 按实际的目录回退（原来按词法，链接后面的 `..` 回到链接所在的目录）；
    - `O_PATH | O_NOFOLLOW` 停在链接本身时，是链接的路径；
    - 沿着悬空的链接新建的文件，是链接指向的路径；
    - 经由 `/proc/<pid>/fd/<n>` 这种链接到达、不知道名字的文件，仍按词法规范化的路径，与原来相同。
  - execve 时解释器在 maps 里的名字原来是 PT_INTERP 原样的字符串，现在也解析符号链接。起第一个程序（`Machine`）和 execve 共用 `interp_label`，名字和 inode 只查找一次（原来为了 inode 另外查找一次）。
- **测试**：
  - cargo test 新增 `procfs_fd_paths_resolve_symlinks`，两层都跑：最后一级是链接、链接后面的 `..`、沿着悬空链接新建的文件、经由链接进入的当前目录下的相对路径。
  - 差分测试 compat 组新增一个 C 程序，在上面几种情况之外还检查：`O_PATH | O_NOFOLLOW`，经由链接打开的目录，相对路径里的 `..`，经由链接 mmap 的文件在 maps 里的名字，以及 PT_INTERP 指向链接（`-Wl,--dynamic-linker=/tmp/rp/ld`）的程序里解释器的映射。输出与原生逐行相同。
  - 变异检验：
    - 打开时不看找到的目录和名字，cargo test 的四个路径都退回原样（`D/link/flink` 等）；
    - 解释器用 PT_INTERP 原样的路径，差分测试里解释器那一行变成 `/tmp/rp/ld`。
  - 上一轮的 valgrind 负载：memcheck 正好 3 个错误（越界读、未初始化、泄漏 40 字节），helgrind 2 个；跟进子进程和 python 都是 0 个。
  - cargo test（215 个）、clippy（含 Android）、两层的 20 组差分测试通过。LTP 的 syscall 测试 958 个通过，与第 91 轮逐项相同，没有 BUG。这一轮和后面两轮的改动是一起跑的。
- **开销**：只在两处。打开文件时，每次多算一遍找到文件的那个目录的路径，查的是元数据缓存；execve 时，解释器少查找一次。用 open 的微基准（`target/mem/openbench.c`，同一个文件开关 20 万次）和 `target/mem/ab.sh`，与上一轮的二进制交替跑：
  - 绝对路径 0.374–0.390 → 0.392–0.394 秒，每次多约 0.05–0.1 µs；
  - 相对当前目录的路径 0.379–0.391 → 0.359–0.361 秒，原来要先算当前目录的路径、再做词法规范化；
  - 经由链接的路径 0.352–0.360 → 0.352–0.361 秒；
  - python 1.37–1.40 → 1.34–1.38 秒，gcc 2.25–2.30 → 2.20–2.26 秒，shell 起进程 0.18–0.19 秒、`git status` 0.07–0.08 秒不变；
  - `go build` 冷编译 6.27–6.30 → 6.14–6.22 秒，Maven 热构建 6.18–6.22 → 6.26–6.28 秒，在[第 89 轮](89-release-borrow-check.md)看到的波动范围（5.97–6.45 秒）里。

  A/B 的二进制也带着第 94、95 轮的改动，它们只碰 `/proc` 的 stat 和跟踪者退出。

**推迟项**：

下面三处原来就和 Linux 不同，都是因为 appine 在打开时就算好路径，Linux 是读的时候按 dentry 算：

- 打开之后删除的文件，Linux 的路径后面有 ` (deleted)`，这里没有（memfd 有）。
- 经由 `/proc/<pid>/fd/<n>` 重新打开的文件，Linux 显示原来那个文件的路径，这里显示 `/proc/<pid>/fd/<n>`。
- 打开之后文件或上级目录改了名，Linux 显示新的路径，这里仍是打开时的。
