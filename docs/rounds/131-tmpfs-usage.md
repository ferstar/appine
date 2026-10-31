# M9 之前：tmpfs 的用量和限制（2026-10-31）

← [M9 之前：vhangup（2026-10-31）](130-vhangup.md) · [索引](../milestones.md) · [M9 之前：setns 经由 pidfd（2026-10-31）](132-setns-pidfd.md) →

[tmpfs](117-tmpfs.md)那一轮的推迟项：`size=` 只影响 statfs 报告的总量，写满了也不会 ENOSPC，inode 数不限，用量不记，`df` 看到的已用总是 0；重新挂载时不看选项。bubblewrap 的 `--tmpfs`（Codex、Claude Code 的沙箱）、`--dev` 和手动挂的 tmpfs 都是这样。

Linux 的 tmpfs 在内存里，按页记用量（`shmem_inode_acct_block`），按 inode 数限制（`shmem_reserve_inode`）。appine 的 tmpfs 是仓库里的一棵树，文件的数据在宿主的文件里，普通文件的写不持 BKL、直接写宿主的文件，大小也不经过仓库，所以用量要在各处自己记。

## 做法

- **按什么算**（`core/kernel/src/vfs/mod.rs` 的 `tmpfs_usage`）：与 Linux 相同，根目录和每个名字各占一个 inode（硬链接也是，Linux 的 `shmem_link` 每个链接另占一个），普通文件按页，目标有 128 字节以上的符号链接一页（`SHORT_SYMLINK_LEN`），目录、设备、FIFO、socket 不占页。页数按文件的大小算，当作没有空洞，与 appine 报告的 `st_blocks` 一致；Linux 只算分到了的页。
- **限制**：默认与 Linux 一样都是物理内存的一半的页数，`size=`、`nr_blocks=`、`nr_inodes=` 可以改，0 是不限。新建文件、目录、设备节点、符号链接、硬链接、AF_UNIX 的 socket 节点之前看放不放得下，放不下是 ENOSPC。写普通文件写到满为止（Linux 一页页地要，要不到时已经写了的照样返回），一页也写不了是 ENOSPC，写在原来的大小以内的不受限制；sendfile、copy_file_range、splice 写进 tmpfs 的文件时不交给宿主拷贝，改走逐块的读写，同样写到满为止。fallocate 要的页放不下是 ENOSPC（打洞除外）。截短（truncate、ftruncate、`O_TRUNC`）与 Linux 一样不限制，按新的大小记账。
- **记账**：实例上记着用量，新建、删除（unlink、rmdir、被 rename 覆盖）、写、截短、fallocate 时按变化加减；statfs 和重新挂载时把整棵树现数一遍（`count_tmpfs`），共享映射里写的这类不经过记账的变化也就算进去了。statfs 的块、inode 按 Linux 的 `shmem_statfs`：块大小 4096，总量是限制，不限的那一项都是 0。
- **重新挂载**（`syscall/mount.rs` 的 `remount_tmpfs`，按 Linux 6.1 的 `shmem_reconfigure`）：不带 `MS_BIND` 的重新挂载改超级块：给了非 0 的 `size=`、`nr_inodes=` 时，原来不限的不能改成限制、不能小于已经用掉的，都是 EINVAL，这时挂载的标志也不改；给了的大小、inode 数（0 是改成不限）和 `inode32`/`inode64` 照改，mountinfo 的超级块选项跟着变；`mode=`、`uid=`、`gid=` 与 Linux 一样解析但不变。原来的选项解析分成"给了哪些"（`parse_tmpfs`）和新挂时补上默认值两步。

## 测试

- 差分测试 `tmpfs` 组新增（放在照 bubblewrap 换根的那一段前面：那一段最后 `exec` 别的程序，后面的不会执行）：`size=64k,nr_inodes=8` 的 tmpfs，空的、建了目录、两页多的文件和它的硬链接、短的和长的符号链接之后的 statfs；写 100000 字节写进 49152 字节后 ENOSPC；文件里面的 pwrite 照常，追加写只填满最后一页；fallocate 放不下是 ENOSPC；截短之后空出来；mknod 到 inode 用完，之后 mkdir、link、symlink、open 新建都是 ENOSPC；删掉一个硬链接、最后一个链接、被 rename 覆盖之后空出来；sendfile 拷进来写到满为止；inode 用完时 AF_UNIX 的 bind 是 ENOSPC；删掉之后不经过 statfs 直接新建、写。重新挂载改大小、改 inode 数、小于用量、`mode=`、`inode32`、改成不限、不限的改成限制，每次之后的 mountinfo 超级块选项；不限的 tmpfs 的 statfs。两个执行层下都与原生相同。原生一侧删掉之后还开着的文件要等关掉才释放，用例先关掉它们。
- 单元测试 `tmpfs_counts_pages_and_inodes`：数用量（硬链接的页只算一次、名字各算一个 inode）、放不放得下、文件还能用多少页、记账、重新挂载不能小于用量和改了之后的超级块选项。
- 变异检验：新建之前不检查，inode 用完之后照样能建；删除时不记账，删掉之后（不经过 statfs）新建是 ENOSPC。第一版用例每一步之后都调 statfs，它把记下的用量纠正过来了，删除不记账查不出来，所以补了"删掉之后不经过 statfs 直接新建、写"那一步。
- cargo test、clippy（含 Android）、两层的全部差分测试通过。LTP 的 syscall 测试 1000 个通过（上次 1001）：epoll_pwait03 这次失败，单独重跑也失败，1 毫秒的睡眠量出的中位数是 1.6 毫秒，[上一轮](130-vhangup.md)的二进制在同一时刻交替跑结果相同，是虚拟机当时的计时精度（[偶发项](../milestones.md#已知的偶发失败)）；其余逐项相同。

**推迟项**：

- 页数按文件大小算，当作没有空洞：`truncate -s` 把文件加长、`lseek` 跳过一段再写，在 Linux 上不占空出来的那些页，这里都算用掉了，之后可能比 Linux 先 ENOSPC。`FALLOC_FL_KEEP_SIZE` 的预留不改大小，Linux 算页，这里不算。
- 删掉最后一个名字时就释放它的页和 inode；Linux 上还开着的文件要等最后一次关掉才释放。
- 几个线程同时不持 BKL 写同一个 tmpfs 里的文件、都把它加长时，记账可能多算或少算，到下一次 statfs 或重新挂载时纠正；按记下的用量判断放不放得下，在那之前可能多放进一点或者早一点 ENOSPC。
- 重新挂载时 `huge=`、`mpol=` 仍只解析不用（Linux 照改，appine 的 tmpfs 没有大页和 NUMA 策略）。
