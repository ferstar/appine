# M9 之前：嵌套 epoll 的边沿（2026-10-27）

← [M9 之前：mremap 和 sysfs（2026-10-27）](46-mremap-sysfs.md) · [索引](../milestones.md)

[epoll 的边沿](42-epoll-edges.md)那一轮留下的推迟项：一个 epoll 放在另一个 epoll 里（外层以 `EPOLLET` 关注内层），内层关注的文件已经就绪时又来了事件，Linux 的外层再报告一次，appine 不报告。对照下来，内层水平或边沿触发关注 eventfd、管道、TCP、AF_UNIX，以及两层嵌套，原生都是"第一次 1、再来一次 1、没有新事件 0"，appine 第二次都是 0。Linux 里内层 epoll 每被唤醒一次（`ep_poll_callback`，按关注的事件过滤）都唤醒等它的 `poll_wait`，即使那个关注已经在就绪链表上（§10）。

- **内层 epoll 的数据代数**（`epoll_arrivals`）：各个关注从加入起、在关注的方向上（读、写分开，与 Linux 按唤醒的事件过滤一样）来过事件的次数之和。去掉的关注（DEL、MOD 换掉的、文件关闭之后清掉的）的份额记进 `Epoll::base`，所以只增不减；加入、修改时已经就绪也算一次（Linux 的 `ep_insert`、`ep_modify` 那时唤醒 `poll_wait`）。`EPOLLONESHOT` 报告过、停用的关注份额停住，与 Linux 停用的不唤醒一样。再往里嵌套的按层数递归，不超过 `EP_MAX_NESTS`。外层的边沿触发照 42 轮的规则用这个数判断"又来了事件"。
- **宿主 socket**：外层以边沿触发关注内层时（`mark_edge_watched`），内层和再往里嵌套的 epoll 关注的宿主 socket 也建记录新数据的宿主 epoll（`HostEdges`），之后加进这些 epoll 的也建；已有的关注重新取一次起点，刚建立的记录不算新事件。
- **等待**：外层只等内层的新事件时（报告过可读），原来把内层各个关注的宿主 fd 按它们的事件放进宿主 poll，宿主 fd 已经可读时立刻返回、变成忙等；现在改为等内层宿主 socket 的宿主 epoll 和 timerfd 的到期（`add_epoll_edges`），进程内的对象来了事件时内核叫醒边沿触发的等待者。
- **开销**：只在嵌套的 epoll 上算；加入关注时的起点用已经取的数据代数推出来，"加入时是否就绪"只在这个 epoll 被边沿触发关注着时才问宿主，Go 这类每个连接都 `EPOLL_CTL_ADD` 的程序不多系统调用。
- **测试**：差分测试 `epoll` 新增上面六种嵌套（eventfd 的内层水平和边沿触发、管道、TCP、AF_UNIX、两层），以及外层报告过、在 epoll_wait 里等着时另一个进程往管道和 TCP 连接里再写，外层被叫醒。单元测试新增内层每来一次事件外层都再触发、内层去掉关注不算新事件。变异检验：改动之前的二进制上八种都是第二次 0（阻塞的两种等到超时）。cargo test（188 个）、clippy、Android clippy、两层的 17 组差分测试通过。LTP 的 syscall 测试 957 个通过：shmctl01 这次通过，select02 这次失败——单独重跑仍失败，但第 42 轮之前的二进制此刻也一样（睡 1 毫秒量出 1.6 毫秒，虚拟机的计时延迟偏大），是[偶发项](../milestones.md#已知的偶发失败)。

**推迟项**：

- 内层 epoll 关注的宿主 stdio 是普通文件时建不了宿主 epoll，与 42 轮一样仍只看程序有没有用完数据。
