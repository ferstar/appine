# 8. 文件系统

## 8.1 rootfs：inode 仓库

- **SQLite** 保存完整的命名空间：目录项、inode、symlink 目标、xattr。使用 WAL 模式，SQLite 以 amalgamation 形式随 App 打包（`rusqlite` 的 `bundled`；NDK 不对外提供系统 SQLite）。
- 每个普通文件的数据存成一个宿主文件 `objects/<ino>`。目录、symlink、设备节点、FIFO、socket 文件只存在于数据库里。object 按 inode 编号相对 `objects` 目录打开（`openat` 等），不解析整个路径。
- 元数据的归属：普通文件的 **size、mtime 和 ctime 以宿主 object 文件为准**（`utimensat` 直接对 object 调用 `futimens`；数据库里改了普通文件的元数据时也 chmod 一下 object，让宿主改 ctime，同一个文件的时间戳只用一个时钟）。mode、uid/gid、xattr、nlink，以及别的 inode 的时间存在 SQLite 里。
- 硬链接就是多个目录项指向同一个 ino。每个元数据操作要么全部生效，要么全部不生效。
- 读写数据直接对宿主 fd 做 `pread`/`pwrite`。两个普通文件之间的 `sendfile`、`copy_file_range` 交给宿主的 `sendfile`。
- **推迟提交**：一次提交比操作本身贵得多，所以写事务打开之后最多推迟 5 毫秒提交，期间的每个操作是事务里的一个保存点，出错（或者内核层 panic）时只回滚到它自己的保存点。guest 的 `fsync`、`sync` 这类，宿主调用 `Machine::sync`、`trim_memory`，以及内核实例关闭时立即提交。App 被杀掉时最多丢失最后几毫秒的元数据修改，仓库仍然是某次提交时的完整状态。
  - 删除最后一个链接时，宿主 object 等提交之后再由后台线程删；inode 行以 nlink = 0 保留到下次开机清理，编号不会被提前重新分配。已经打开的宿主 fd 仍然可以读写。
  - 新建普通文件的 object 立即建好。提交之前被杀掉的话 object 会留下来，所以 `lock` 文件里记着还没提交的写事务新分配的 inode 编号范围，下次开机删掉范围里没有 inode 行的 object。
- **断电**：提交只把帧写进 WAL、不刷（`synchronous = NORMAL`），WAL 和 object 由宿主各自写回，先后没有保证。按原生 ext4（data=ordered、auto_da_alloc）的语义补三处顺序：
  - guest 要求落盘的真的落盘：fsync、fdatasync 先让宿主刷 object，再提交、等 WAL 落盘；sync、syncfs 提交之后对整个宿主文件系统 syncfs；`O_SYNC` 这类的写完了刷 object。刷盘时放开 BKL。
  - 改名覆盖别的文件的普通文件，在包含这次改名的提交之前先让它的数据落盘：apk 装文件、编辑器存盘都是写临时文件再改名、不调 fsync，原生 ext4 的 auto_da_alloc 替它们写回，appine 的改名宿主看不到。
  - 删除 object 之前先等删掉它最后一个链接的提交落盘。
- **检查点**（把 WAL 写回数据库文件，要 fsync）由仓库的后台线程用自己的连接做，不在多半持着 BKL 的提交里；一直有提交、WAL 长到 1 万页时才由提交的线程就地补上。
- **元数据缓存**：路径查找和 stat 走内存里的缓存（目录项，包括不存在的名字；inode 行；符号链接目标；目录列表）。自己的写入在改的时候就去掉受影响的条目，操作成功之后新建的条目直接放进缓存。缓存没命中时，一次路径解析里的所有查询共用一个读事务。缓存也可以不持 BKL 读（[kernel/process 的 map](../../.claude/maps/kernel/process.md)）：只有持 BKL 的线程修改缓存，修改时持有一把读写锁的写锁，别的线程持读锁读。
- **多个进程共用一个仓库**（App 的内核和"文件"App 的 DocumentsProvider，§11.3）：写事务一开始就取得写锁（`BEGIN IMMEDIATE`）；有别的进程也在用仓库时不推迟提交。仓库目录里的 `lock` 文件：每个进程持有它的共享锁，只有能独占时（相当于开机）才做开机的清理（nlink = 0 的 inode、上次留下的 tmpfs、`/dev/shm` 和 `/run`）。`lock` 文件开头由所有进程共享映射一个提交计数器，缓存发现它不是因为自己的提交而变了，就整体作废。

## 8.2 挂载表

开机时的挂载：

| 挂载点 | 文件系统 |
|---|---|
| `/` | inode 仓库 |
| `/dev` | tmpfs，相当于 devtmpfs：开机时在里面建好标准设备节点（`null`、`zero`、`full`、`random`/`urandom`、`kmsg`、`tty`、`console`、`ptmx`）和 `fd`、`stdin` 等链接，打开时按设备号找到进程内的实现 |
| `/dev/pts` | devpts：每个 PTY 一个 `/dev/pts/N`。属主是打开主设备的进程，属组是 tty，chown、chmod 的结果记在终端上（sshd 把终端交给登录的用户） |
| `/dev/shm`、`/dev/mqueue`、`/run` | 与 systemd 挂的一样：两个 tmpfs 和开机时的 IPC 命名空间的 mqueue（OpenRC 的 `init.sh` 看到已经挂着就不再挂） |
| `/proc` | procfs |
| `/sys` | sysfs |
| `/sys/fs/cgroup` | cgroup2 |
| `/tmp` | 仓库里的目录，开机时不挂 tmpfs |
| `/mnt/<名字>` | SAF 目录挂载（v1.x，见 §11.3） |

- **合成文件系统**（procfs、sysfs、cgroup2、devpts）根据内核层的结构实时合成，打开时生成内容，与 Linux 的 seq_file 一样从偏移 0 再读时重新生成。
  - procfs：smaps 的驻留页按宿主内存算（mincore），fork 之后写时复制共用的页算共享的；`/proc/meminfo` 和 sysinfo 照搬宿主的（与 docker 的容器一样），guest 能用的上限看 cgroup 的 `memory.max`。exe、`fd/<n>`、maps 里的路径与 Linux 的 `d_path` 一样，在 execve、打开文件时算好；exe 与 Linux 的 `mm->exe_file` 一样指向程序文件本身。
  - sysfs：只有 CPU 的拓扑（CPU 都一样，`cache/`、`cpufreq/` 照搬宿主的）和 `/sys/class/net`（读者所在的网络命名空间里的接口，Linux 上是指向 `/sys/devices` 的符号链接，这里是目录）。
  - cgroup2（`core/kernel/src/cgroup.rs`）：根像 docker 用 cgroup 命名空间的容器的根（`/proc/<pid>/cgroup` 是 `0::/`），控制器有 cpuset、cpu、memory、pids。`memory.max`、`memory.current` 是 §12 第 4 条的上限和用量，JVM、Node.js 据此定堆。下面可以建子 cgroup、迁移进程，规则照 Linux 6.1。起作用的限制只有 `pids.max`、`memory.max`（OOM killer 在那个 cgroup 里挑进程）、`cgroup.freeze`（进程在返回 guest 之前停下）和 eBPF 的设备过滤（bpf(2) 只有 `BPF_PROG_TYPE_CGROUP_DEVICE`，校验后解释执行，`core/kernel/src/bpf.rs`）；别的（`memory.high`、`cpu.max`……）只按格式记下。
- **mount(2)**（`core/kernel/src/syscall/mount.rs`）：合成文件系统、tmpfs、mqueue 可以挂到别的目录上，仓库里的和合成文件系统里的目录、文件都可以 bind；`MS_MOVE`、pivot_root、重新挂载、`MNT_DETACH`，新的挂载接口里的 open_tree、move_mount、mount_setattr。chroot 进别的发行版、bubblewrap、容器运行时要的都有。挂载的标志（ro、nosuid、nodev、noexec、nosymfollow）按经由的挂载算，顺序和错误码照 Linux 6.1。与 Linux 不同的地方：
  - 传播类型可以设置，但不模拟，都相当于 private。
  - 重新挂载只改这一个挂载（Linux 不带 bind 时还改共用的超级块）。
  - 别的文件系统是 ENODEV；fsopen、fsconfig、fsmount、fspick 是 ENOSYS（crun、util-linux 退回 mount(2)）；idmapped 挂载是 EINVAL。
- **节点带着经由的挂载**（`Node::Store(挂载编号, inode)`、`Node::Pseudo(挂载编号, 节点)`，相当于 Linux 的 `struct path`）：同一个目录挂在两处是两个节点，`..`、getcwd、`/proc/<pid>/{cwd,root,fd}` 经由各自的挂载点回到上一层。根文件系统也是挂载表里的一项（编号 1），挂在看不到的 rootfs 上。卸下了、还有人在用的挂载留一份记录，没人用了就丢掉。挂载记录全局按编号存，每个记着所属的挂载命名空间（[kernel/process 的 map](../../.claude/maps/kernel/process.md)）。
- **实例**：procfs、devpts、tmpfs、mqueue 每次挂是新的实例（devpts 有自己的一套终端，mqueue 对应挂的进程所在的 IPC 命名空间）；sysfs、cgroup2 只有一个实例。
- **tmpfs** 是仓库里不挂在根目录下面的一棵树：挂上时新建一个上级是它自己的目录（记在 `detached_roots` 表里），文件照常是宿主上的 object，不占 App 的内存；所有挂载都没了、也没人在用时连同里面的一起删掉，App 被杀掉留下的在下次开机时删。用量与 Linux 一样按页数和 inode 数限制，记在实例上，statfs 和重新挂载时把整棵树现数一遍。与 Linux 不同：普通文件按大小算页数（当作没有空洞），删掉最后一个名字时就释放（Linux 等最后一次关掉）。
- **openat2** 的 `RESOLVE_*`（`vfs::resolve`）照 Linux 6.1 加在同一个路径解析里。

## 8.3 inotify

所有文件系统修改都经过内核层，所以 inotify 不需要宿主参与：各个 syscall 改完仓库之后报告事件，报告的位置和顺序与 Linux 的 fsnotify 钩子相同。监视按仓库的 inode 登记；没有任何监视时，每个事件只多一次判空。

- 按 fd 的操作（读写、截断、关闭）用打开时经过的目录和名字：每个打开的仓库文件都记着它们，所以监视建立之前就打开的文件也报告。
- "文件"App（§11.3）另外打开仓库改的，它把改了什么（`vfs::browse::Change`）交给用着同一个仓库、正在运行的内核（`browse_changing`），内核持着 BKL 照对应的 syscall 报事件。
- procfs、devpts、sysfs 上可以建监视，但不会有事件。没有 fanotify。
