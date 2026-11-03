# 13. 工程

## 13.1 语言与接口

- **引擎**：C17 + aarch64 汇编，用 Clang（NDK）编译。
- **内核层**：Rust，可以参考 Fuchsia Starnix 和 Asterinas 的设计。
- **Android 外壳**：Kotlin。

引擎与内核层之间的接口很窄，采用**退出原因**模型：引擎从不回调内核层，`appine_cpu_run()`（Tier 0）或 `appine_dbt_run()`（Tier 1）一直执行到需要内核层介入的事件，返回一个退出原因；内核层处理完之后再调用它继续执行。这样 C 与 Rust 之间没有重入，FFI 边界最简单。完整定义见 `core/engine/include/appine/engine.h`，Rust 绑定见 `core/engine/src/lib.rs`。

```c
/* 运行：返回 SVC / UNDEF / BRK / FAULT / KICK / CODE / HOST_FAULT 之一 */
enum appine_exit appine_cpu_run(struct appine_cpu *cpu);                          /* Tier 0 */
enum appine_exit appine_dbt_run(struct appine_dbt *dbt, struct appine_cpu *cpu);  /* Tier 1 */
void appine_cpu_kick(struct appine_cpu *cpu);   /* 可从任意线程调用，让目标线程在下一个指令边界退出 */
void appine_cpu_tlb_flush(struct appine_cpu *cpu);

/* soft-MMU 页表：由内核层维护，修改后要刷新所有共用它的 CPU 的 TLB */
int appine_mmu_map(struct appine_mmu *mmu, uint64_t gva, void *host, uint64_t len, uint32_t prot);
int appine_mmu_unmap(struct appine_mmu *mmu, uint64_t gva, uint64_t len);
int appine_mmu_protect(struct appine_mmu *mmu, uint64_t gva, uint64_t len, uint32_t prot);
void *appine_mmu_translate(const struct appine_mmu *mmu, uint64_t gva, uint32_t required_prot);

/* Tier 1 的翻译不会自己失效：由内核层按代码失效日志作废，见 engine 的 map */
void appine_dbt_invalidate(struct appine_dbt *dbt, uint64_t start, uint64_t len);
void appine_dbt_flush(struct appine_dbt *dbt);
void appine_dbt_recheck(struct appine_dbt *dbt);  /* exec 之后：保留翻译，下次运行前逐块核对 */
/* 放回池里之前记下翻译所在的映射；从池里拿到之后，与记下的完全相同的映射里的块不必核对 */
void appine_dbt_set_sources(struct appine_dbt *dbt, const struct appine_dbt_source *s, size_t n);
void appine_dbt_trust(struct appine_dbt *dbt, const struct appine_dbt_source *s, size_t n);

/* 宿主侧的故障兜底（engine 的 map）：之后运行期间的 SIGSEGV/SIGBUS/SIGILL 变成 HOST_FAULT 退出 */
void appine_fault_guard_install(void);
```

- FAULT 退出时，pc 指向出错指令，`fault_addr`/`fault_access`/`fault_kind` 给出地址、访存类型以及页错误、对齐错误还是 SP 对齐错误；该指令不产生任何副作用。
- CODE 退出表示 guest 执行了 `ic ivau`：pc 已指向下一条指令，`fault_addr` 是它的地址，内核层把这一页记进代码失效日志。
- HOST_FAULT 退出表示引擎自己的 bug 让宿主收到了故障信号（[engine 的 map](../../.claude/maps/engine.md)）：guest 状态不可信，内核层结束这个进程，详情由 `appine_host_fault_last()` 给出。

guest 状态（`struct appine_cpu`）包含：`x[31]`、`sp`、`pc`、`nzcv`、`fpcr`、`fpsr`、`tpidr_el0`、`orig_x0`、fault 信息、`exit_info`、pending 标志、mmu 指针、独占监视器（地址、长度、读到的值）、Tier 1 内部用的几个字段（退出原因、跳转缓存、影子返回栈）、软件 TLB 和取指 TLB、`v[32]`（128 位）。Tier 1 生成的代码用立即数偏移直接寻址的字段放在 TLB 前面。Rust 侧逐字段镜像这个结构体，由 `appine_cpu_layout_check()` 在测试里校验布局。

## 13.2 仓库结构

```
core/engine/     C 引擎（include/appine/engine.h、src/c/）及其 Rust 绑定；tests/ 是 Tier 0、Tier 1 的测试
core/kernel/     Rust 内核层（syscall/ 按类别分文件，vfs/ 是仓库和设备节点）
core/hal/        宿主抽象层（Android 与 Linux 共用一套实现）
hosts/cli/       appine-run（开发用 CLI 前端）及端到端测试（guest 程序、rootfs、基准程序）
hosts/android/   Android App 的原生库 libappine.so（JNI）
tests/guest/     guest 测试程序：手写的汇编，以及用 musl-gcc 编译的 C 程序
tests/lockstep/  指令级 lockstep：原生跳板、随机指令生成，同时比对 Tier 0 和 Tier 1；白名单与 llvm-mc 的对照
tests/bench/     性能探针：独立编译的基准程序和测量工具 appine-bench
tests/diff/      差分测试的脚本
tests/survey/    负载普查的脚本
scripts/         dev（在 Linux arm64 容器里运行命令）、fetch-rootfs、difftest、build-ltp、ltp、survey、release-sources
android/         Gradle 工程：Kotlin 外壳（app/）、vendored 终端库（terminal-emulator/、terminal-view/）
docs/            设计文档（design/，总览是 README.md，一节一篇），各轮的记录（rounds/，索引是 milestones.md）
```

构建：Cargo 工作区；引擎通过 `cc` crate 编译，C 代码在 gcc 和 clang 下都以 `-Wall -Wextra -Werror` 编译。Rust 工具链由 `rust-toolchain.toml` 锁定。App 外壳由 Gradle 构建，原生库由 cargo-ndk 构建。

## 13.3 开发环境与测试

- 主要的开发和测试宿主是 Mac 上的 **Linux arm64 虚拟机**（colima、OrbStack 或 Lima），90% 的工作不需要碰手机。`scripts/dev <命令>` 会在一个 arm64 Linux 容器里运行命令，比如 `scripts/dev cargo test --workspace`；Docker daemon 配了代理时，容器会自动沿用。
- **CI**（`.github/workflows`）有两个 job：
  - **Linux arm64**（GitHub 的 `ubuntu-24.04-arm` runner）：格式检查、clippy、`cargo test --workspace`（含三方 lockstep、白名单与 `llvm-mc` 的对照、guest 程序和 rootfs 用例、基准程序在两个执行层下的输出比对）、需要外网的 apk + git 用例、加长的 lockstep、性能探针、差分测试，以及用 clang 编译的引擎测试。
  - **Android APK**（x86_64 runner）：Android 目标的 clippy、`assembleDebug`、lint，APK 作为构件上传。
- **性能探针**：`cargo build --release -p appine-run -p appine-bench` 之后运行 `appine-bench`，它分别以原生、Tier 0、Tier 1 运行 `tests/bench/programs/` 下的程序，要求输出逐字相同，并打印耗时比。
- **调试**：`APPINE_LOG=1` 输出本地调试日志；`APPINE_STRACE=1` 打印每个 syscall 的参数、结果、时间和耗时；`APPINE_PERF_MAP=1` 让 Tier 1 写 `/tmp/perf-<pid>.map` 供 perf 使用；`appine-run --stats` 打印翻译统计。内核层和引擎的 bug 在 debug 构建下默认直接终止宿主进程，`APPINE_ON_BUG=kill` 改成只结束撞上它的 guest 进程（§12 第 5 条）；`APPINE_TEST_HOOKS=1` 时一个 Linux 编号之外的 syscall（`0xa991`）可以模拟各种内核 bug，供端到端测试使用（Android 的 debug 版 App 靠 `test-hooks` 特性带上它）。`scripts/difftest`、`scripts/ltp`、`scripts/survey` 都看 `appine-run` 的 stderr，内核层报告了内部错误时差分测试算失败、LTP 记为 `BUG`、普查标出条数。
- 测试分为五层：
  1. **指令级 lockstep**：用宿主 CPU 当标准答案。生成随机指令和随机寄存器值，分别原生执行和在解释器/DBT 中执行，逐条比对结果。这同时也是对解码器的 fuzz。被测的 guest 地址和原生一侧的宿主地址相同，所以访存用错了 addend 它发现不了，这类错误靠差分测试和真实程序。
  2. **LTP 的 syscall 测试**：`scripts/build-ltp` 在 `alpine` 容器里原生编译，`scripts/ltp` 每个测试起一个新的 `appine-run`，8 路并发，在本地运行，不在 CI 里。按上次的耗时，最长的先跑；`LTP_TIMEOUT_MUL=5`，`LTP_RUNTIME_MUL=0.1`（跑满规定时长的竞争测试只跑十分之一）。仓库放在虚拟机本地的 docker 卷上：经由 virtiofs 时八路同时写 SQLite 等锁超时。sigtimedwait01、sigwaitinfo01 默认跳过（musl 的 sigtimedwait 遇到 EINTR 自己重试，原生的 Alpine 上一样跑满超时）。已知的偶发失败见 [milestones.md](../milestones.md#已知的偶发失败)。musl libc-test 没有接入。
  3. **端到端场景**：`hosts/cli/tests/rootfs_programs.rs` 里的 busybox、交互式 shell、`apk add`、`git clone` 等用例。`scripts/survey` 在 appine 里跑常见的服务和工具（`tests/survey/`）、Debian 里的 glibc 程序（`glibc/`）、语言自带的测试集和大项目（`suites/`）、AI 编程 CLI（`ai/`，模拟服务冒充模型 API），`-n` 在由仓库导出的原生镜像里跑同样的负载对照；输出要人看，要联网安装软件包，不在 CI 里。
  4. **差分测试**：同一条命令分别在 appine 和真正的 arm64 Linux 中运行，比对输出；信号测试比对 `ucontext` 的内容。`scripts/difftest` 先在 appine 里把测试仓库的根文件系统打包、导入成 docker 镜像，再把 `tests/diff/` 下的每个脚本分别交给 docker 容器和 `appine-run` 执行，比对输出和退出码（脚本不能超过 128 KB）。`appine-run` 也在容器里运行，两边看到同一种网络，路由、接口、地址类的用例才比得了；随内核版本变的地方用例不比。macOS 上默认 4 组同时跑，每个并发通道用一份仓库的 APFS 克隆，上次耗时长的组先开始。
  5. **Android 专项**：用 arm64 Android 模拟器跑 instrumentation 测试（`MachineTest`），覆盖 4K 和 16K 页，包括用测试钩子在 App 进程里模拟内核层和引擎的 bug。GitHub 的 runner 没有 KVM，这一层在本地运行，CI 只构建 APK 并运行 lint。

## 13.4 许可证

appine 按 **AGPL-3.0-or-later** 发布。原因：App 里 vendored 的 Termux 终端库是 GPLv3-only，发布 APK 本来就要按 GPLv3 提供整个 App 的源码；GPLv2 与它、与 Apache-2.0 的代码（jackpal、AndroidX、Kotlin 标准库）都不兼容；AGPLv3 都兼容，而且网络条款覆盖有人拿改过的核心做在线服务（比如代码执行的沙箱）的情形。

- **依赖**：Rust crate 都是宽松许可证，都能选 MIT；Android 库都是 Apache-2.0；SQLite 是公有领域。都与 AGPLv3 兼容。
- **声明**：设置里的"开源许可"列出 App 里所有第三方的东西和许可证全文，构建时生成（`NoticesTask`，见 `android/README.md`）：Rust crate 从 `cargo metadata` 找链接进 `libappine.so` 的，各自发布的许可证文件原样附上；Android 库取运行时的依赖图，不在已知是 Apache-2.0 的组里时构建失败。设置页的这一行、`appine-run --version` 显示 AGPL 第 5 条要的法律声明。
- **APK 里的 Alpine minirootfs**：与 appine 只是放在一起（mere aggregation），不受 appine 的许可证影响；但 busybox、apk-tools、alpine-baselayout、scanelf 是 GPL-2.0-only，musl-utils 有一部分是 GPL-2.0-or-later，发布 APK 时要提供它们的源码。声明里列出每个包的 aports 提交。设置里下载的 Debian 由用户从 Docker 官方镜像的来源下载，不经 appine 分发。
- **源码**：GPLv3、AGPLv3 都要求随 APK 提供完整的对应源码（包括静态链接的 crate）。仓库现在是私有的，发布之前要公开（或者附上提供源码的书面承诺）。`scripts/release-sources` 生成发布时一起放出的三份：appine 自己（`git archive HEAD`，工作区有改动时拒绝）、`cargo vendor`、Alpine minirootfs 的 GPL 包（按安装数据库里的 origin 和 aports 提交从 GitLab 取 aports 的目录，上游源码从 Alpine 的 distfiles 镜像取并按 sha512sums 校验，不执行 APKBUILD）。
