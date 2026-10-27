# M9 之前：io_destroy 叫醒等事件的线程（2026-10-28）

← [M9 之前：smaps 里共用的页（2026-10-28）](65-smaps-shared.md) · [索引](../milestones.md)

[负载普查](62-workload-survey.md)的问题 4：`mariadb-install-db` 永远不结束，之后启动的 mariadbd 也因为拿不到数据文件的锁（还被卡住的那个占着）起不来。

- **原因**：InnoDB 用 Linux 原生 AIO，一个线程专门在 `io_getevents` 里等完成事件。关闭时主线程 `io_destroy`，再 join 这个线程：Linux 的 `kill_ioctx` 把上下文标成已销毁并叫醒等它的线程，`aio_read_events` 看到 `ctx->dead`，这次一个事件也没取到就返回 EINVAL，线程据此退出。appine 的 `io_getevents` 一开始取了上下文的引用就一直拿着循环等；`io_destroy` 只把上下文从进程的表里拿掉、叫醒所有等待者，等待者醒来看到的还是那个上下文，接着等，主线程的 join 就永远等下去。这一点从 [M9 之前：推迟项](19-deferred.md)那一轮实现 AIO 起就是这样。
- **改动**：上下文记一个 `dead`，`io_destroy` 设置它，并取消挂起的 `IOCB_CMD_POLL`（不产生完成事件，与 Linux 相同）。`io_getevents`、`io_pgetevents` 在等待中发现上下文已经销毁时，已经完成的事件照样取走，一个也没有就返回 EINVAL。
- **怎么找到的**：`APPINE_STRACE` 里 mariadbd 的主线程最后一个返回的系统调用是 `io_destroy`，等事件的线程停在 `io_getevents` 里；对照 Linux 6.1 的 `fs/aio.c` 就清楚了。
- **测试**：
  - 差分测试 `compat` 的 AIO 一节新增两个场景：别的线程在 `io_getevents` 里不限时地等着时销毁上下文，没有事件时它得到 EINVAL；已经完成一个、它要等两个时，得到那一个。等待者最多等 5 秒（`pthread_timedjoin_np`），超时就打印"仍在等"并退出，免得测试本身卡住。上一轮的二进制上等待者一直等下去。
  - 普查的 `mariadb`：install-db 结束，InnoDB 用原生 AIO（`innodb_use_native_aio` 是 ON）插入 5 万行、事务回滚，`mariadb-admin shutdown` 之后 mariadbd 退出。普查脚本改了两处：插入之前先 `use d`（`seq_1_to_50000` 是序列引擎的虚拟表，要有当前数据库），启动之前删掉旧的错误日志。
  - cargo test（200 个）、clippy（含 Android）、两层的 20 组差分测试通过。LTP 的 syscall 测试 958 个通过（上次 959）：只有[偶发项](../milestones.md#已知的偶发失败) shmctl01 这次失败，io_* 那几个测试照常通过。

**推迟项**：无。
