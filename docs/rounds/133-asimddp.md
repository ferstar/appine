# M9 之前：点积指令 ASIMDDP（2026-10-31）

设计文档 §4.4 把 ASIMDDP、LRCPC、SHA3、SHA512 放在 v1.x。ASIMDDP 是 Advanced SIMD 的 int8 点积（SDOT、UDOT，ARMv8.2 可选、v8.4 起必有，手机上 Cortex-A55、A75 起的核都有）：llama.cpp、onnxruntime、XNNPACK、libvpx、libaom、ffmpeg 等看 `HWCAP_ASIMDDP` 选点积的代码路径，没有时走慢的 NEON 路径；直接按 `+dotprod` 编译、不检测的预编译程序在 appine 里会 SIGILL。

## 做法

- **只改白名单**：Tier 0 的 SIMD&FP 数据处理指令本来就只做白名单判定、运算交给原生指令桩，Tier 1 原样拷贝（只改写被占用的通用寄存器操作数），两个执行层共用 `appine_simd_dp_decode`（`core/engine/src/c/simd.c`）。所以这类特性不用另写语义，判定放行即可：
  - 三寄存器扩展组（bit 21 = 0、bit 15 = 1、bit 10 = 1）原来整个拒绝，现在只放行 opcode 0010、size = 10 的 SDOT/UDOT (vector)；同一组里的 SQRDMLAH（RDM）、USDOT、SMMLA（I8MM）、FCMLA、FCADD（FCMA）、BFDOT（BF16）仍拒绝。
  - 按元素组的 opcode 1110 是 SDOT/UDOT (by element)，要 size = 10（别的 size 未定义）。
- **宿主有才宣告**（`features.c`）：宿主的 `AT_HWCAP` 有第 20 位才放进 guest 的集合。HWCAP、`ID_AA64ISAR0_EL1` 的 DP 字段（[47:44] = 1）、`/proc/cpuinfo` 的 `asimddp` 从同一份集合生成；cpuinfo 的名字表按 Linux 6.1 的 `hwcap_str` 补到第 20 位（`core/kernel/src/pseudo.rs`）。宿主没有点积时 guest 照旧看不到，这两组编码照旧 SIGILL。

## 测试

- lockstep 新类别 `dotprod`：随机的 SDOT/UDOT 向量形式和按元素形式（2S、4S，各种 H:L、M:Rm），解释器、Tier 1 与硬件逐条比对。20 万轮的加长 lockstep 里执行 200000 条，全部一致，Tier 1 没有退回 Tier 0；随机的 `fuzz_simd_fp` 也会碰到它们。
- 白名单与 llvm-mc 的对照按 guest 特性加 `+dotprod`。20 万个随机样本一致；变异检验：去掉 `+dotprod` 之后报出分歧，向量和按元素两种形式都抽到了。
- 单元测试：`dot_product_follows_the_feature_set`（宿主有时 SDOT、按元素的 UDOT 的结果，没有时是未定义指令），`id_registers_match_hwcap` 加上 DP 字段；`unimplemented_encodings_are_undefined` 里原来拿 SDOT 当没有实现的例子，换成 I8MM 的 USDOT 和 size 不对的按元素 SDOT。
- 差分测试 `c-programs` 组新增 `dotprod.c`（整个文件只加 `+dotprod`）：HWCAP、`ID_AA64ISAR0_EL1` 的 DP 字段、cpuinfo 的 `asimddp`；`vdotq_s32`、`vdotq_u32`、`vdot_s32` 和按元素的四个下标、`vdot_lane_s32`，累加值在溢出边缘；`-O3` 时编译器把 int8 的点积循环向量化成 SDOT，它的结果和汇编里 sdot 的条数。两个执行层下都与原生相同。
- **收益**：libvpx 1.15.2 的 `vpxenc`（Alpine 的 libvpx-utils）在运行时按 HWCAP 选 SAD、方差、卷积的点积版本，`VPX_SIMD_CAPS_MASK` 可以关掉（0x1 只有 NEON，0x3 加上点积）。VP9、`--good --cpu-used=2`、单线程、640x360 的 I420 合成画面 60 帧，输出 ivf 的 md5 在各种组合下都相同。原生、上一轮的二进制、这一轮的二进制交替各跑三次：

  | | 只有 NEON | NEON + 点积 |
  |---|---|---|
  | 原生 | 1.71–1.73s | 1.51–1.54s |
  | appine（上一轮，不宣告） | 3.59s | — |
  | appine（这一轮） | 3.58–3.60s | 3.42–3.48s |

  原生上点积省 11%，appine 里省 4.5%：点积的核心原样执行，省下的绝对时间差不多（0.17–0.2 秒），但 appine 里标量代码和访存检查占的比例更大。相对原生（都用点积）从 2.3× 到 2.2×。
- cargo test、clippy（含 Android）、两层的全部差分测试通过。LTP 的 syscall 测试 1000 个通过（上次 1000）：futex_wait05 由失败变成通过、select02 由通过变成失败，都是检查睡眠精度的[偶发项](../milestones.md#已知的偶发失败)，select02 与[上一轮](132-setns-pidfd.md)的二进制交替单独跑各两次都失败；其余逐项相同。

**推迟项**：

- LRCPC、SHA3、SHA512 仍在 v1.x。SHA3（EOR3、RAX1、XAR、BCAX）和 SHA512 也是数据处理指令，同样只改判定就行，用得到的是 OpenSSL、Go 的 `crypto/sha512`、`crypto/sha3` 这些；LRCPC 的 LDAPR 是访存，要在两个执行层里照 LDAR 实现。→ SHA3、SHA512 已补上（M9 之前：SHA3、SHA512 和 I8MM）；LRCPC 也补上了（M9 之前：LRCPC）
- 宿主有 RDM、I8MM、BF16、FHM、FCMA、JSCVT 这些时仍不宣告（不在 §4.4 的表里）；I8MM 和点积一样是数据处理指令，llama.cpp、libvpx 也用它。→ I8MM 已补上（M9 之前：SHA3、SHA512 和 I8MM）；JSCVT 已补上（M9 之前：FP16 和 JSCVT）；RDM、BF16、FHM、FCMA 已补上（M9 之前：RDM、FCMA、FHM 和 BF16）
