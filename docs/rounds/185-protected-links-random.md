# M9 之前：Alpine 开机时写的 sysctl、protected_symlinks/hardlinks 和随机数的 ioctl（2026-11-02）

← [M9 之前：开机时挂好 /dev、/dev/shm、/dev/mqueue 和 /run（2026-11-02）](184-boot-tmpfs.md) · [索引](../milestones.md)

支持 OpenRC 的第二轮。开机运行级别里的两个服务在 appine 里报错：

- sysctl 服务执行 `/usr/lib/sysctl.d/00-alpine.conf`，其中 4 项在 appine 里没有：`kernel.panic`、`fs.protected_hardlinks`、`fs.protected_symlinks`、`kernel.unprivileged_bpf_disabled`。busybox 的 sysctl 用 `O_CREAT` 打开，procfs 里新建是 EACCES，报 "Permission denied"。
- seedrng（Alpine 默认在 boot 里）对 `/dev/urandom` 调 `RNDADDENTROPY`，把上次存下的种子加回熵池。appine 只实现了 `RNDGETENTCNT`，别的都是 ENOTTY，服务失败。

## 做法

### 随机数设备的 ioctl

按 Linux 6.1 的 `random_ioctl`（`syscall::fs::random_ioctl`）：

| 请求 | 行为 |
|---|---|
| `RNDGETENTCNT` | 256（不变） |
| `RNDADDTOENTCNT` | 要 `CAP_SYS_ADMIN`，负数是 EINVAL |
| `RNDADDENTROPY` | 要 `CAP_SYS_ADMIN`；熵的比特数是负数 EINVAL；数据要都读得到，否则 EFAULT（长度是负数也是 EFAULT） |
| `RNDZAPENTCNT`、`RNDCLEARPOOL`、`RNDRESEEDCRNG` | 要 `CAP_SYS_ADMIN`，别的什么也不做 |
| 别的（包括 `RNDGETPOOL`、tty 的 `TCGETS`） | EINVAL，与 Linux 一样不是 ENOTTY |

随机数取自宿主，熵池总是满的（与 Linux 5.18 起相同）。加进来的数据只读一遍、不用。

### 四个 sysctl

| 项 | 默认值 | 写入 |
|---|---|---|
| `kernel.panic` | 0 | int（`proc_dointvec`）。只记下：appine 的内核层没有 panic，内核 bug 只结束撞上它的进程（设计文档 §12） |
| `fs.protected_symlinks`、`fs.protected_hardlinks` | 1 | 0 或 1，别的 EINVAL |
| `kernel.unprivileged_bpf_disabled` | 2 | 要 `CAP_SYS_ADMIN`；0 到 2；改成 1 之后不能再改（EPERM），与 Linux 的 `bpf_unpriv_handler` 相同 |

- 四个文件的权限都是 0644。Linux 5.17 把 `protected_*` 挪进 fs/namei.c 之后从 0600 变成了 0644，6.1、6.8 都是 0644。
- 两个 `protected_*` 默认打开。Linux 内核自己的默认是 0，systemd、Android 的 init、Alpine 都设成 1，原生的虚拟机上也是 1。

### protected_symlinks、protected_hardlinks 的语义

只接受写入、不起作用，就是在安全设置上骗人。所以两个都按 Linux 6.1 实现了：

- **`may_follow_link`**（`Vfs::may_follow_link`）：
  - 只管路径的最后一个组件（`pick_link` 的 `WALK_TRAILING`），中间的不管。在 nosymfollow、`RESOLVE_NO_SYMLINKS` 的检查之前。
  - 链接的属主是跟随者（fsuid）就跟随。
  - 否则，粘滞位并且所有人可写的目录（/tmp）里的链接，只有属主与目录的属主相同时才跟随，不然 EACCES。root 也一样：这正是用来防 root 跟随别人放在 /tmp 里的链接。
  - `open(O_CREAT)` 经由悬空的链接新建，也在这里先被挡住。
- **不持 BKL 的路径解析**（`Cached::walk`）不知道凭据。遇到粘滞位、所有人可写的目录里属主与目录不同的链接，就交给正常的解析。
- **`may_linkat`**（`linkat`，在 EXDEV 之后、目录的写权限之前）：
  - 不是文件的属主、也没有 `CAP_FOWNER` 时，源要是普通文件，不是 setuid 的、不是可执行的 setgid 的，并且能读写。不然 EPERM。
  - 设备节点、FIFO 都不行。

### unprivileged_bpf_disabled 的作用

- 不是 0 时，与原来一样：没有 `CAP_BPF`/`CAP_SYS_ADMIN` 的进程，bpf(2) 一律 EPERM。
- 是 0 时，按 Linux 各个命令自己的检查：
  - PROG_LOAD 除了套接字过滤、cgroup skb 都要有能力。appine 没有这两种，是 EINVAL。
  - PROG_QUERY 要 `CAP_NET_ADMIN`，PROG_GET_FD_BY_ID 要 `CAP_SYS_ADMIN`（原来就有）。
  - PROG_ATTACH/DETACH 不另外检查。
  - OBJ_GET_INFO_BY_FD 不给指令和各种记录的个数（`bpf_prog_get_info_by_fd` 的 `goto done`）。

## 测试

- 新的差分测试组 `protected`（带 `CAP_SYS_ADMIN`），两个执行层都与原生逐字相同：
  - 四个 sysctl 的值和权限，nobody 也能读。
  - **protected_symlinks**：
    - root 跟随 /tmp 里 nobody 的链接是 EACCES，`stat -L` 也是。
    - 经由它到目录里的文件（中间的组件）可以，readlink、lstat 可以。
    - 经由 nobody 的悬空链接 `O_CREAT` 是 EACCES，什么也没建。
    - nobody 跟随自己的、root 的链接可以。
    - 不是粘滞位的目录里可以。
    - 粘滞位目录的属主与链接的属主相同时可以，换了目录的属主之后不行。
  - **protected_hardlinks**：nobody 链接 root 的 0600 文件、setuid、可执行的 setgid、设备节点、FIFO、只读的文件都是 EPERM；0666 的文件、不可执行的 setgid 文件、自己的 0000 文件可以，root 可以。
  - **随机数的 ioctl**：上表的每一项，加上数据伸进没有映射的页（EFAULT）；nobody 能读熵、改的都是 EPERM。
- 变异检查：
  - 用第 184 轮的二进制跑这一组：没有这几个文件，跟随、`stat -L`、经由悬空链接新建都成功了，链接也都建成了，ioctl 是 ENOTTY。
  - 只去掉"不持 BKL 的解析交给正常的解析"：`stat -L` 那一行成功了，与原生不同。
- rootfs 的集成测试新增一个（容器里 `/proc/sys` 只读，写入比不了）：
  - 照 00-alpine.conf 用 `sysctl -w` 写四项，读回来。
  - `unprivileged_bpf_disabled` 改成 1 之后改不回 0，`protected_symlinks` 写 2 是 EINVAL，nobody 写不了。
  - 关掉 `protected_symlinks` 之后 root 能跟随 nobody 的链接，打开之后不能。
- 手动（差分测试的仓库，Python 直接调 bpf(2)）：
  - `unprivileged_bpf_disabled` 是 2 时，nobody 的加载、查询都是 EPERM。
  - 是 0 时，nobody 加载设备程序是 EPERM、套接字过滤是 EINVAL、查询是 EPERM。
  - root 能加载设备程序。
- 手动（OpenRC，`target/openrc/devfs.sh`）：boot 里的 sysctl、seedrng 都是 `[ ok ]`，seedrng 打印 "Seeding 256 bits and crediting"。
- cargo test（333 个）、两个执行层的全部差分测试（52 组）通过。LTP 的 syscall 测试与第 184 轮逐项相同（1013 个通过，epoll_pwait03 仍是 colima 的计时问题）。
