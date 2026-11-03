# 11. 用户、权限与 Android 集成

## 11.1 凭据与权限

- 按 Linux 的 `struct cred` 实现（`core/kernel/src/cred.rs`）：ruid/euid/suid/fsuid、对应的四个 gid、附加组、umask。凭据放在线程组里（libc 的 setuid 系列本来就会让每个线程都调用一遍）。VFS 做 DAC 权限检查（`generic_permission`、粘滞位、setgid 目录），root 经由能力绕过。
- `fs.protected_symlinks`、`fs.protected_hardlinks` 默认打开（与 systemd、Android 设的一样），规则同 Linux 6.1 的 `may_follow_link`、`may_linkat`：粘滞位、所有人可写的目录里，别人的符号链接不跟随（root 也一样）；不是文件的属主、也没有 `CAP_FOWNER` 时只能硬链接能读写的普通文件，setuid、可执行的 setgid 文件不行。不持 BKL 的路径解析遇到这种链接交给正常的解析。
- **登录账号**：终端标签页由 assets 里的 `login.sh` 以 root 运行，按设置里的"登录账号"决定登录谁，再用 `su 账号 -c 'exec "$SHELL" -l'` 切过去（不用 `su -l`：它总是回家目录，恢复的标签页要从原来的目录开始，见 §11.2）。util-linux 的 su（Debian）对 `-c` 的命令另起会话、没有作业控制，所以有 `--session-command` 时用它。默认是"自动"：用最先建的普通用户（UID 1000–65533、有登录 shell 的里 UID 最小的），没有就是 root，像桌面 Linux 一样在命令行里 `adduser` 之后新标签页就以它登录；也可以明确选 root 或某个账号，选的账号不在了用 root。换发行版、重置 rootfs 之后回到自动。
  - 普通用户第一次成为登录账号时，照 Alpine 的 `setup-user -a` 让它能提权：Alpine 加进 wheel 组、写 `permit persist :wheel`、装 doas（App 带着这个包，离线也能装）；Debian 加进 sudo 组、联网装 sudo（显示 apt 的进度，按 Ctrl-C 跳过这一次，之后每次登录再试）。加过组的记在 `/var/lib/appine/admins`，用户自己移出组也不再加回去。
  - 提权的密码：设了密码的照发行版的惯例要输密码；没设的（`adduser -D`）不问。每次登录按当时的密码重写 `/etc/doas.d/30-appine.conf`（或 `/etc/sudoers.d/appine`）里免密的那几行。
- setuid/setgid 位在 execve 时生效（`PR_SET_NO_NEW_PRIVS` 之后、被没有 `CAP_SYS_PTRACE` 的跟踪者跟踪着时得不到新的特权），特权执行设置 `AT_SECURE`。写、截短普通文件时，没有 `CAP_FSETID` 的进程去掉它的 setuid 位和带着组执行位时的 setgid 位，chown 总是去掉（`file_remove_privs`、`chown_common`）。
- 支持 `chroot`（每个进程记录自己的根目录）。
- capabilities 按 capabilities(7) 的规则随用户 ID 变化：有效用户 ID 为 0 时拥有边界集内的全部能力，变成非 0 时清空有效集，三个用户 ID 都非 0 时清空允许集，文件系统能力跟着 fsuid；`capset`、`PR_CAPBSET_DROP` 可以缩小。execve 按 Linux 6.1 的 `cap_bprm_creds_from_file` 变换能力：程序文件的能力存在扩展属性 `security.capability` 里（v1、v2、v3，v3 只有 rootid 为 0 的起作用），环境能力在执行不带特权的程序时保留下来；securebits 和它们的锁与 Linux 相同。设置文件能力要 `CAP_SETFCAP`，写、截短、chown 时去掉。扩展属性的权限按 `xattr_permission` 检查：`trusted.*` 要 `CAP_SYS_ADMIN`，`user.*` 要文件的读写权限，写别的 `security.*` 要 `CAP_SYS_ADMIN`。
- guest 自己安装 seccomp 过滤器（`core/kernel/src/seccomp.rs`）：严格模式和 cBPF 过滤器，程序的检查、动作和优先级与 Linux 6.1 相同，在系统调用的入口（PTRACE_SYSCALL 的入口停住之后）由解释器执行。过滤器属于线程，clone、fork 继承，execve 保留，可以 TSYNC；支持 `PTRACE_EVENT_SECCOMP` 和通知 fd（USER_NOTIF、ADDFD）。与 Linux 一样按编号缓存过滤器总是放行的编号（`seccomp_cache`），这些系统调用不再运行程序，照常走不持 BKL 的快速路径（§6）。

## 11.2 生命周期

| 项目 | 做法 |
|---|---|
| 前台服务 | 只要还有终端会话或 guest 进程在运行，就保持一个**前台服务**（`TerminalService`），类型为 `specialUse`。通知栏显示"N 个会话运行中"，没有会话时是"在后台运行"，附带"全部退出"按钮。关掉最后一个标签页只关界面：服务每 2 秒看一次，没有会话、界面不在、也没有 guest 进程了才退出 |
| 开机、关机 | 内核启动之后在单独的线程上按 `/etc/inittab` 开机（§6，Alpine 上是 OpenRC 的服务），不挡标签页。"全部退出"、guest 的 `poweroff`/`halt` 先关机，完了才停止服务，这期间通知显示"正在停止服务"；guest 的 `reboot` 关机之后启动一个新的内核、开一个新的标签页。被系统杀掉时来不及关机 |
| 生命周期归属 | 所有会话和 guest 内核的生命周期都**挂在 Service 上，不挂在 Activity 上** |
| 熄屏 | 熄屏后继续运行依赖 `PARTIAL_WAKE_LOCK`，由用户手动开启（设置页的"熄屏时保持运行"、通知上的"保持唤醒"），默认关闭。开着时内核运行期间一直持有。App 有前台服务，Doze 里这个锁照样有效、网络也不断（AOSP 只对进程状态在 `BOUND_FOREGROUND_SERVICE` 之后的 App 禁用），不申请电池优化的豁免 |
| 内存紧张 | 丢掉可以重建的缓存：池里保留的翻译、共用的只读文件映射、dentry/inode 缓存（`Machine::trim_memory`），并提交推迟的写事务。界面停下 1 分钟之后还没回来就释放：Android 14 起有前台服务的进程连 `UI_HIDDEN` 也收不到，只能自己看 Activity 在不在。`onTrimMemory` 的 `RUNNING_LOW`、`RUNNING_CRITICAL`（Android 14 之前）和 `BACKGROUND` 以上立即释放 |
| 进程被杀 | 只恢复标签页布局、每个标签的 cwd 和滚动回看记录，并启动新的 shell，同时提示"上次会话因 App 被系统终止而结束"；**不恢复进程**。内核运行期间在 `files/tabs/` 留着"在运行"的标记，每个标签页的登录账号、前台进程组的当前目录（`Machine::console_cwd`）和回看记录每分钟存一次，界面隐藏、内存紧张、开关标签页时也存；正常退出时连标记一起删掉。服务启动时标记还在就是被杀了：每个标签页以原来的账号、从原来的目录起新的 shell，先显示原来的回看记录和一行提示 |
| 进程被结束、服务没起来 | 内核层结束 guest 进程（bug、引擎故障、OOM killer）时记下报告（`Machine::take_kills`：pid、进程名、控制台、原因，OOM 的还有哪个上限、用量）。App 每秒取一次：控制终端是某个标签页时在那里用黄色说明原因，插在光标所在行上面；没有控制终端、在全屏程序里或者界面不在时发通知。开机完成后 `/var/log/boot.log` 里有 OpenRC 的 `failed to start` 时发通知。appine 自己的 bug 总是发通知，上面有"复制报告"：App 的版本、设备、发行版、执行层、内核层的报告（带 panic 的消息和位置），以及内核日志里它前面的 10 条 |
| 网络服务 | guest 第一次在非回环地址上 `listen` 某个端口时发通知（§12 第 7 条） |
| 开机自启 | v1 不做（只看 App 冷启动） |

## 11.3 共享存储

- **v1：DocumentsProvider**（`Documents.kt`）。把登录账号的家目录注册为系统"文件" App 里的一个根，设置里的开关（默认关）可以把整个 `/` 以只读方式暴露。用户在"文件" App 里用"复制到…"即可导入或导出文件。
  - 不经过内核，直接在仓库里按 guest 路径查（`vfs::browse`：符号链接跟随，只给目录和普通文件，挂载看不到），每次调用另外打开一次仓库，文件以只读打开宿主上的 object 交出去。类型按扩展名猜，猜不出的看开头 4 KB（没有 NUL、是 UTF-8）认出文本。文档 ID 是 guest 的绝对路径，只认规范的、在显示着的根里面的。
  - 家目录里面能新建（属于登录账号，0644/0755，名字有了时换成 `a (1).txt`）、写、删除、改名、移动，按真实路径限制：经由符号链接走到家目录外面的不行；整个 `/` 只读。
  - 双向的通知：修改经由仓库的事务和提交计数器，运行中的内核马上看得到，也报 inotify 事件（§8.3）；为写交出去的文件带上关闭的回调，对方关闭时报 `IN_MODIFY`、`IN_CLOSE_WRITE`。反过来，guest 改了目录项的目录由仓库记下，服务定时取走（`Machine::take_changed_dirs`），通知"文件"App 刷新。
- **v1.x：SAF 目录挂载到 `/mnt/<名字>`**。权限位是合成的（目录 0755、文件 0644）；`chmod`/`chown` 静默成功；`symlink`/`link` 返回 `EPERM`；目录列表在内存中缓存；不区分大小写这一点写进文档。
- 不申请 `MANAGE_EXTERNAL_STORAGE`，Play 不批准终端类 App 使用。

## 11.4 首次启动和设置

`android/app/src/main/java/.../Rootfs.kt`、`SettingsActivity.kt`：

1. 第一次运行时直接导入 Alpine Linux，不让用户选。Alpine 的 minirootfs（`rootfs.tgz`）在 APK 的 assets 里，由 `scripts/fetch-rootfs` 下载，下载时验官方的 GPG 签名，版本和 sha256 固定写在脚本里。Debian 13 在设置里换：debuerreotype 构建的 `debian:trixie`（Docker 官方镜像用的同一份，固定在一个提交上），选了才下载，长度和 sha256 固定写在 `Rootfs.kt` 里，只比 sha256（没有签名，摘要写在 App 里，验签也不会更可信）。它只有 GitHub 一个来源，暂不自己托管。有几个来源时先测速，从快的开始试，一个失败了换下一个；中途断开时带 `Range` 接着下。校验过的 tarball 留在缓存目录里，导入成功才删掉。
2. 以流的方式导入 `rootfs.tmp`，完成后（连同第 3 步写的配置）对整个文件系统 syncfs 一次再改名，改名之后让 `files` 目录也落盘（§8.1 的断电）；导入中途被杀时下次重来。装的是哪个发行版记在 App 的 SharedPreferences 里。
3. 写入包管理器的源（`/etc/apk/repositories` 或 apt 的 deb822 源，apt 用 http：rootfs 里没有 ca-certificates，软件包有签名）、`/etc/hostname`、`/etc/hosts`、`/etc/motd`，以及 `/etc/profile.d/appine.sh`（`TERM=xterm-256color`、`LANG=C.UTF-8`、`NMAP_UNPRIVILEGED=1`、彩色 `PS1`，显示 motd）。Alpine 的 `/etc/resolv.conf` 由内核层在打开仓库时补上（§10）；Debian 的改成指向 DNS 存根。
   - 每次启动内核之前，没有 `/etc/network/interfaces` 时写一个只有回环接口的：网络是宿主的，OpenRC 为要网络的服务拉起 networking，没有这个文件时它失败，服务也起不来。不用 rc.conf 的 `rc_provide`：它加到每一个服务上，容易成环。
   - 装了 OpenRC、hwclock/osclock/swclock 都不在任何运行级别里时，把 osclock 加进 boot：OpenRC 默认用 hwclock 提供 `clock`，而 appine 没有 RTC、也设不了宿主的时间（§7.5），hwclock 必然失败。用户放了哪个时钟服务就不动。
4. 终端标签页经由 `login.sh` 登录（§11.1 的登录账号），shell 取 `/etc/passwd` 里那个账号的：Alpine 是 ash，Debian 是 bash，chsh 改过之后照样。

- **设置页**：标签栏菜单里的"设置"。状态都在 `TerminalService` 里，设置页只显示和转达；有设置页或终端界面在，都算界面可见。
- **换发行版、重置 rootfs**：重置是换成装着的那个。确认之后先在后台下载并校验 tarball，这期间原来的系统照常运行，进度显示在设置页和通知里，两处都能取消；下载好了才结束所有会话、停下内核、删掉仓库、导入新的。下载失败时原来的系统不动；导入失败时退回 APK 里的 Alpine。内核启动失败时也能在设置里重置。一次只有一个 rootfs，换发行版就丢掉原来的文件；rootfs 导出和导入放到 v1.x。
- **镜像选择**：每个发行版内置一份镜像列表：官方的、清华、中科大、阿里云。导入之后在后台并发测速，选最快的写入包管理器的源，全部失败时保持官方的。设置里可以手动选一个，或者选回"自动"重新测速。写进源时只把内置镜像的地址换成选的那个，协议照旧，用户加的行都留着；源里一处内置镜像也没有时不动，提示用户手改。内核运行着也可以写：仓库的提交计数器让内核层的缓存作废（§8.1）。
- **执行层**：设置里可以换成 Tier 0（解释器，排查问题用），记下之后重启内核。
- **升级**：App 更新时不动用户已有的 rootfs。同一分支内由用户自己运行 `apk upgrade`、`apt upgrade`；跨分支升级由用户修改源。
