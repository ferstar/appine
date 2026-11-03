# M9 之前：短的符号链接不占块（2026-11-01）

[Ubuntu 26.04 的普查](149-ubuntu26-survey.md)的问题 2：`ls -l` 一个有符号链接的目录，total 比原生多 4。appine 对所有文件按大小向上取整到 4 KB 报 `st_blocks`，符号链接也是；Linux 上短的符号链接存在 inode 里，不占数据块。

## 做法

- `encode_stat`：符号链接的目标短于这个长度时 `st_blocks` 是 0：仓库（对应 ext4）是 60 字节（`ext4_symlink`：连同结尾的 NUL 放得进 60 字节的 `i_data` 就是快速符号链接），tmpfs 这类别的是 128 字节（`shmem_symlink` 的 `SHORT_SYMLINK_LEN`）。长的照旧按一页算，与 [tmpfs 的用量](131-tmpfs-usage.md)里"长的符号链接一页"一致。

## 测试

- 差分测试 `compat2` 组：`/tmp`（原生是 ext4 上的 overlayfs）里目标 59、60 字节的符号链接的大小和块数（0、8）；`tmpfs` 组：挂上的 tmpfs 里目标 127、128 字节的（0、8）。两个执行层下都与原生相同。
- 变异检查：去掉符号链接这一条，两组都在短的那一个上不同。
- 与[第 150 轮](150-new-pty-state.md)合起来跑的全量测试：cargo test（283 个）、clippy（含 Android）、格式、两层的全部差分测试通过；LTP 的 syscall 测试 1010 个通过，futex_wait05 这次失败，是[偶发项](../milestones.md#已知的偶发失败)，其余逐项相同。
