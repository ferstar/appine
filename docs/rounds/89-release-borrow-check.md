# M9 之前：测试用的发布构建也检查借用（2026-10-29）

← [M9 之前：futex 等待者的清理，放开 BKL 时检查借用（2026-10-29）](88-futex-cleanup-borrow-check.md) · [索引](../milestones.md)

[上一轮](88-futex-cleanup-borrow-check.md)的推迟项：放开 BKL 时借着内核对象的检查只在调试构建里有，差分测试、LTP、普查用的发布构建都不查。

- **发布构建里也数借用**：
  - kernel crate 新增 `borrow-check` 特性，打开时与调试构建一样数着每个线程借着几个 `RefCell`，放开 BKL 时检查是 0（`core/kernel/src/cell.rs`）。
  - `appine-run` 默认打开它：差分测试、LTP、普查、手工测量用的都是 `appine-run` 的发布构建，不用记着另加参数。`--no-default-features` 可以去掉。
  - debug 版 App 也打开（与 `test-hooks` 一样由 `app/build.gradle.kts` 给 debug 变体加上），模拟器上的 instrumentation 测试也查。App 的发布版不打开，计数不存在。
  - 发布构建里撞上时与别的内核 bug 一样：panic 被兜住，结束这个进程（SIGSEGV），往 stderr 写 `appine: internal error in pid …`（§12 第 5 条）。
- **测试脚本认出内核层报告的内部错误**：发布构建只结束出问题的进程，测试未必因此失败（比如结束的是后台的辅助进程），所以三个脚本都去看 stderr 里有没有 `appine: internal error`：
  - `scripts/difftest` 原来把 `appine-run` 的 stderr 丢掉，现在写到 `<名字>.appine.err`，有内部错误时输出一致也算失败，失败时一起另存；
  - `scripts/ltp` 的结果多一种 `BUG`：日志里有内部错误，不论退出码；
  - `scripts/survey` 每个负载的那一行加上内部错误的条数。
- **`waits` 用例补上三种等待**（上一轮列出的 cargo test 没跑到的）：
  - **租约的打破**：父进程持着读租约，子进程以写方式打开时等它；父进程看到 `F_GETLEASE` 报告正在打破（子进程已经在等了）之后放弃租约。
  - **文件位置锁**（`f_pos_lock`）：两个线程同时从同一个打开文件各读 2 字节，检查读到的位置不重不漏。一个线程放开 BKL 读宿主文件时另一个等它；临时加日志数过，4000 次读里两层各等了 2000 多次。
  - **AIO 的完成**：管道上的 `IOCB_CMD_POLL` 由另一个线程写管道完成，`io_getevents` 不带超时一直等到它。
  - 上一轮列的另外两项，放开 BKL 时的 execve 和 DNS 存根，走的是 `Cx::unlocked`，进入时就检查，不必真的碰上并发：cargo test 里每次 execve 都查，DNS 存根由 CI 里需要外网的 apk 用例查。
- **变异检验**：
  - 在文件位置锁、租约打破的等待之前借着一个 `RefCell`，调试构建的 `waits` 用例分别在 `file position lock`、`lease break` 那一步报 `releasing the BKL while holding 1 RefCell borrow(s)`；
  - 文件位置锁的那个变异用发布构建跑，进程被结束，stderr 里是 `appine: internal error in pid 2 (waits), thread 14, …: kernel panic in syscall 63; killed the process with SIGSEGV`。
- **开销**：用 `target/mem/ab.sh` 拿这一轮之前的二进制和改动之后的交替跑两轮：
  - python 1.37–1.39 → 1.38 秒，gcc 2.28–2.33 → 2.25–2.28 秒，shell 起进程 0.18 → 0.19 秒，`git status` 0.06–0.08 → 0.07 秒，python 循环 0.70–0.73 → 0.71 秒；
  - `go build` 冷编译 6.13–6.15 → 6.21–6.31 秒，Maven 热构建 6.14–6.28 → 6.24–6.34 秒，都在上一轮两个二进制之间看到的波动范围里（6.03–6.37 秒、5.97–6.45 秒）。
- **测试**：
  - cargo test（208 个）、clippy（含 Android，打开和不打开这两个特性）、两层的 20 组差分测试通过，stderr 里都没有内部错误；debug 版 APK 构建通过，`libappine.so` 里有这项检查。模拟器上的 instrumentation 测试这一轮没有跑。
  - LTP 的 syscall 测试 957 个通过（上一轮 959），没有 BUG。futex_wait05、select02 从通过变成失败（睡得太久，[偶发项](../milestones.md#已知的偶发失败)），单独重跑、拿上一轮的二进制在同一时刻重跑也都失败；其余逐项相同。
  - 普查：15 个负载都正常退出，未实现的 syscall 与以前相同。语言的测试集和大项目：
    - CPython 437 个通过、第一遍 14 个失败：multiprocessing 的 9 组是工作进程在 2 GB 的上限处被 OOM killer 结束（[第二轮普查](70-survey-suites.md)的问题 5），其余 5 个是以前记过的 test_os（virtiofs 的时间戳）、test_build_details、test_ctypes（打包）和 test_urllib、test_urllib2（代理）；
    - PHP 0 个失败；Ruby 2 个失败、10 个错误，与原生相同；
    - Node 12 个失败：5 个与原生相同，另外 7 个被 OOM killer 结束（问题 5），[删除的目录里不能新建，socket 可以 fchmod](77-dead-dir-socket-fchmod.md)那一轮修好的 3 个不再失败；
    - Go 标准库 250 个包全部通过；ripgrep、Laravel、Rails 正常。
  - 以上全都没有报告内部错误。

**推迟项**：

- cargo test、差分测试、LTP、普查都没跑到的等待仍然不查；发布构建只在这些路径真的等待时才查得出来（比如文件位置锁要碰上争用）。
