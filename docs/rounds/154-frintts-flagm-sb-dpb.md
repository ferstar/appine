# M9 之前：FRINTTS、FlagM、SB 和 DPB（2026-11-01）

← [M9 之前：/proc/<pid>/exe 指向程序文件本身（2026-11-01）](153-exe-file.md) · [索引](../milestones.md) · [M9 之前：DIT（2026-11-01）](155-dit.md) →

[RDM、FCMA、FHM 和 BF16](142-rdm-fcma-fhm-bf16.md)那一轮的推迟项：宿主（开发用的 M2 Pro、常见的手机核心）有、appine 仍不宣告的 CPU 扩展（§4.4 表里的"还没做"）。这一轮做其中只是指令层面的几种：

- FRINTTS（`AT_HWCAP2` 的 `frint`）：FRINT32Z、FRINT32X、FRINT64Z、FRINT64X，舍入成 32、64 位整数范围里的整数值，超出范围的和 NaN 得到最小的那个整数，置 FPSR.IOC（JavaScript、C 的浮点转整数）。
- FlagM（`flagm`）：CFINV（C 取反）、RMIF（寄存器循环右移之后按掩码写进 NZCV）、SETF8、SETF16（按 8、16 位的有符号结果设 N、Z、V）。
- FlagM2（`AT_HWCAP2` 的 `flagm2`）：XAFLAG、AXFLAG，浮点比较的结果在 Arm 的 NZCV 和"外部格式"之间转换。
- SB（`sb`）：推测执行的屏障。
- DPB、DPB2（`dcpop`、`AT_HWCAP2` 的 `dcpodp`）：DC CVAP、DC CVADP，把缓存清到（深）持久化点。

DIT 是 PSTATE 里的一位，信号帧、ptrace 都要带上，[下一轮](155-dit.md)单独做。SM3/SM4 开发机的 M2 没有，与 EBF16 一样测不了，没做。LSE2 另外看。

## 做法

- **特性集合**（`features.c`）：HWCAP 第 16、27、29 位（DCPOP、FLAGM、SB），HWCAP2 第 0、7、8 位（DCPODP、FLAGM2、FRINT）。FRINTTS 是 SIMD&FP 的数据处理指令，和原来那些一样依赖原生指令桩；另外五种不用原生桩，宿主没有 FP、原生桩不可用时也宣告，但 Tier 1 原样执行它们，所以宿主要有。FlagM2、DPB2 只在 FlagM、DPB 上宣告（ID 寄存器里是同一个字段的 2）。ID_AA64ISAR0_EL1 的 TS（第 52–55 位，1 是 FlagM、2 是 FlagM2），ID_AA64ISAR1_EL1 的 DPB（第 0–3 位）、FRINTTS（第 32–35 位）、SB（第 36–39 位）。`/proc/cpuinfo` 的 HWCAP 名字表加到第 29 位（`flagm`、`ssbs`、`sb`）：原来只到第 26 位，宣告了 FlagM、SB 也不显示，`gcc -mcpu=native` 读出来是 `+flagm2 … +noflagm`、没有 `+sb`。
- **白名单**（`simd.c`）：标量一源操作数组 opcode 0x10–0x13（单、双精度，没有半精度）；两寄存器杂项组 U = 0 的 opcode 0x1e、0x1f（FRINT32Z、FRINT64Z）和 U = 1 的 0x1e、size 0x 的 0x1f（FRINT32X、FRINT64X，size 1x 的 0x1f 是原来的 FSQRT），双精度只有 128 位的形式。
- **Tier 0**（`interp.c`）：
  - RMIF、SETF8、SETF16 在 ADC 所在的组里（bits 15:10 不为 0），按 ARM ARM 的伪代码算 NZCV；
  - CFINV、XAFLAG、AXFLAG 是 MSR（立即数）里 op1 = 0、CRm = 0 的三个编码；CRm 不为 0 的、op2 是别的值的（UAO、PAN、SPSel 这些 EL0 用不了的）照旧未定义；
  - SB 是屏障组的 op2 = 7、CRm = 0，什么也不做；
  - DC CVAP、DC CVADP 与 DC CVAC 一样只按读访问检查地址：guest 内存就是宿主内存，宿主内存也不是持久化内存。
- **Tier 1**（`dbt.c`）：这五条 FlagM 指令只改 NZCV，guest 的 NZCV 在翻译后的代码里就是宿主的，原样执行；SB 走原来的屏障路径原样执行；DC CVAP、CVADP 与 DC CVAC 一样内联 TLB 检查。`dp_operands` 原来把 ADC 组里的编码都当作 ADC（Rd、Rn、Rm 三个寄存器），RMIF 的移位量（bits 20:15）会被当作 Rm、遇到被占用的寄存器编号时被改写，现在单独认出：只读 Rn、不写寄存器。

## 测试

- 引擎的单元测试：`flagm_sb_dpb_follow_the_feature_set`（CFINV、XAFLAG、AXFLAG 对 16 种 NZCV 与伪代码比，RMIF、SETF8、SETF16 的几个值，SB 不改状态，DC CVAP、CVADP 在只读页上成功、没有映射的地址上是读的缺页且 pc 不变）、`frintts_follows_the_feature_set`（3.7、3e10 的 FRINT32Z 和 IOC，2.5 的 FRINT64X，向量的 FRINT32X 含 NaN）；`id_registers_match_hwcap` 加上 TS、DPB、FRINTTS、SB 四个字段和"FlagM2、DPB2 只在前一级上宣告"；`unimplemented_encodings_are_undefined` 里原来的 FRINT32Z 换成半精度的 FRINT32Z，加上 CRm 不为 0 的 SB、CFINV 和 `msr ssbs, #1`。
- lockstep 新类别 `frint`（标量、向量，FPCR 的舍入方式随机）、`flagm`、`flagm2`（NZCV 的初值随机）、`sb`、`dcpop`、`dcpodp` 各跑 20 万条，原生、Tier 0、Tier 1 三方一致；`adc_sbc`、`nzcv_move`、`fuzz_dp` 也通过。
- 白名单与 `llvm-mc` 的对照：`-mattr` 加上 `fptoint`、`flagm`、`altnzcv`、`sb`；新加抽样空间 `adc_flags`（ADC、SBC、RMIF、SETF 所在的组）。屏障组没有加：LLVM 把未分配的屏障编码解码成通用的 `msr S0_3_C3_Cx_y, xzr`，CRm 不为 0 的 SB 也认成 `sb`，与"按编码判定"对不上。
- 差分测试 `c-programs` 加上 `ext2.c`（`-march=armv8.5-a`）：HWCAP、ID 寄存器的四个字段；六条 FlagM 指令对 16 种 NZCV、几个寄存器值的结果；ACLE 的 `__rint32zf` 等八个标量函数在 15 个值（含超出范围的、NaN、无穷）上、FPCR 的四种舍入方式下的结果和 IOC，向量的 `vrnd32xq_f32`、`vrnd64z_f32`、`vrnd64xq_f64`；SB；DC CVAP、DC CVADP 在可读、PROT_NONE、没有映射的地址上（后两种是 SIGSEGV 的 SEGV_ACCERR、SEGV_MAPERR）。两个执行层下都与原生逐字相同。
- 在 appine 里 `gcc -mcpu=native` 现在推出 `+flagm2 … +frintts … +sb`，定义 `__ARM_FEATURE_FRINT`，与原生相同（原生多一个 `+pauth`：PAC 按 §4.4 不宣告）。
- 变异检查：Tier 0 的 AXFLAG 不清 C，lockstep 的 `flagm2` 在第 20 次就与硬件不同。
- 引擎的单元测试、clippy（含 Android）、格式通过；全量测试与[第 155 轮](155-dit.md)合起来跑，结果记在那一篇。
