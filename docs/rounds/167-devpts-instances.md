# M9 之前：devpts 的实例（2026-11-01）

← [M9 之前：cgroup 的设备过滤（eBPF）（2026-11-01）](166-bpf-devices.md) · [索引](../milestones.md)

[podman 和 docker 的普查](157-container-survey.md)的问题 11（[mount 的子集](105-mount-subset.md)的推迟项）：appine 的各个 devpts 挂载共用开机时的那一套终端。容器里 `tty` 是 `/dev/pts/1`，`ls /dev/pts` 看得到宿主的终端，到了容器里就是隔离上的漏洞。Linux 4.7 起每次 `mount -t devpts` 是一个新的实例，有自己的一套终端（编号从 0 开始）。照 Linux 6.1 的 `fs/devpts/inode.c` 做。

## 做法

- **实例**：每次挂 devpts 新建一个实例，有自己的设备号，设备号也就是实例的编号；开机时挂在 `/dev/pts` 的是 0:24。bind 挂载与源共用实例。节点带上实例：`PtsRoot(实例)`、`Pts(实例, n)`、`PtsPtmx(实例)`。终端表（`Kernel::ptys`）按 (实例, 编号) 记，编号是实例里最小的空着的，终端记着自己在哪个实例里。列目录、查找只看这个实例的终端。inode 号照 Linux：根是 1，ptmx 是 2，终端 n 是 n + 3。
- **选项**（`tty::PtsOptions`，`devpts_parse_param`）：`uid`、`gid`（十进制，要在调用者的用户命名空间里映射得到）、`mode`、`ptmxmode`（八进制）、`max`（不超过 2^20），`newinstance` 不起作用，别的、格式不对的都是 EINVAL。新实例默认 `mode=600,ptmxmode=000`。新终端的属主、属组按 `uid`、`gid`，没给时是打开 ptmx 的进程的 fsuid、fsgid，权限位按 `mode`；ptmx 的权限位按 `ptmxmode`；实例里的终端到了 `max` 个时再开是 ENOSPC。mountinfo 的超级块选项按实例显示（`devpts_show_options`）。开机时的实例与 docker 给容器挂的一样，是 `gid=5,mode=620,ptmxmode=666`；原来显示 `ptmxmode=000`，节点的权限却是 0666，现在两者一致。
- **ptmx 设备节点**（不在 devpts 里的 5:2，Linux 的 `devpts_acquire`）：用节点所在目录里的 `pts` 挂着的实例；没有 `pts` 是 ENOENT，有但不是 devpts 的根是 ENODEV。原来总是开机时的那个。devpts 里的 `ptmx` 用它所在的实例。
- **路径**：打开文件记着经由的 devpts 挂载（`PtyEnd::mnt`）。从设备的 fd 链接、ttyname 是那个挂载里的路径（bind 进来的就是 bind 的路径）。主设备的链接是打开它的路径，原来固定是 `/dev/ptmx`。从设备 fstat 的设备号是它所在实例的。
- **TIOCGPTPEER**（原来没有，Linux 4.13 起的 `ptm_open_peer`）：按主设备打开对应的从设备，经由主设备找到的那个 devpts 挂载，不经过路径、不查权限。参数是 open 的标志，O_CLOEXEC 给新的 fd；在从设备上、终端还锁着时是 EIO。containerd 的 console 包用它。
- **卸下**：经由 devpts 里的路径打开的终端（ptmx、`<n>`、TIOCGPTPEER 打开的）开着时，卸下那个挂载是 EBUSY；经由别处的设备节点打开的主设备不占（原来终端的打开文件都不算）。实例的选项在挂载都没了的时候丢掉。
- 顺带：`/dev/pts/<n>` 的设备号原来有两种算法：stat 按 136:n，设备过滤按 136 起的 8 个主设备号，n 不小于 256 时两边不一样。Linux 的 pts 驱动登记的是主设备号 136 的全部 2^20 个次设备号，现在都按 136:n，仓库里的终端设备节点也按它找终端。

## 测试

- 新的差分测试组 `devpts`（原生一侧是特权容器，容器的 `/dev/pts` 是它自己的实例，与 appine 开机时的一样都从没有终端开始）：
  - 新挂的实例、bind 挂载、默认选项的实例：设备号各不相同（bind 的相同）；根、ptmx 的 inode 号；各个实例里的编号都从 0 开始；ptmx 的权限按 `ptmxmode`。
  - ptmx 设备节点所在目录里的 `pts` 是 devpts、没有 `pts`、`pts` 只是目录三种情况，以及用它开着主设备时卸下 `pts` 可以。
  - 从设备的链接、设备号、inode、权限、属组；fstat 的设备号与路径的相同，与 `/dev/pts` 的不同；TIOCGPTPEER 打开的从设备的链接和 close-on-exec，在从设备上、锁着时是 EIO；经由 bind 打开的从设备；主设备的链接；ttyname。
  - 经由实例里的终端读写；`ls` 各个实例；终端节点的属主、权限；开着终端时卸下是 EBUSY；关掉之后节点消失。
  - 选项：写错的 `mode`、不认识的、写错的 `gid`、`max` 超出是 EINVAL；`ptmxmode=0777`、`uid=7,gid=8,mode=0640`（新终端的属主、权限）、`newinstance`、`max=1`（第二个是 ENOSPC）各自的 mountinfo。
  - 原生上 `/dev/ptmx` 是 docker 放的指向 `pts/ptmx` 的符号链接，appine 的是设备节点，用它开的主设备的链接不比。
- `mount` 组的设备号比较里加上新挂的 devpts（原来因为共用开机时的实例而不比），两边都是自己的设备号。
- 两个执行层下都与原生逐字相同。变异检查：新挂的 devpts 不建新实例，编号、ptmx 的权限、fstat 的设备号、链接都与原生不同。第一次做变异检查时用例卡住了：从 p1 的主设备读回写进去的数据，变异之后数据写到了别的终端，读就一直等。改成最多等 2 秒，整个程序再加 60 秒的 alarm。
- cargo test（302 个）、clippy（含 Android）、格式通过；两层的差分测试只有 Tier 1 的 `cgroup-freeze` 一次不同，是原生一侧：[cgroup.freeze](165-cgroup-freeze.md) 的用例要求睡 1 秒的进程总共睡够 1.5 秒（解冻就在 1.5 秒前后），原生这次差一点没到。不冻结时 1 秒就返回，阈值改成 1.25 秒，两层各跑两次都相同。LTP 的 syscall 测试 1011 个通过（上次 1010）：上次计时偶发失败的 select02 这次通过，其余逐项相同。
- `tty.rs` 的单元测试：选项的默认值、各种写法、错误（含映射不到的 uid），显示的格式。
- 在 appine 里实际跑：`docker run -t`、`podman run -t` 的容器里 `tty` 是 `/dev/pts/0`，`ls /dev/pts` 只有 `0` 和 `ptmx`，宿主的 `/dev/pts` 里看不到容器的终端。

**推迟项**：

- 不在 devpts 里的终端设备节点（mknod 的 136:n）打开的是开机时的实例里的终端；Linux 上这样打开从设备会失败。
- 内核参数 `kernel.pty.max`、`kernel.pty.reserve` 没有，只有每个实例的 `max`。
- 卸下了的实例里还开着的终端照常能用，但实例的选项已经丢了；之后没法再经由它开新的终端，影响不到什么。
