# M9 之前：LRCPC2（2026-10-31）

[LRCPC](136-lrcpc.md)那一轮的推迟项。LRCPC2（ARMv8.4 起必有，cpuinfo 里是 `ilrcpc`）给 RCpc 的读和 release 的写加上 9 位有符号偏移：LDAPUR、LDAPURH、LDAPURB（补零），LDAPURSB、LDAPURSH（按符号扩展到 32 或 64 位）、LDAPURSW，STLUR、STLURH、STLURB。LLVM 在 v8.4 以上（`+rcpc-immo`）把带偏移的 acquire 读、release 写编成它们，省掉一条算地址的加法；这样编译的程序在宿主有、appine 不宣告时一碰到就 SIGILL。

## 做法

- **编码**：`size 011001 opc 0 imm9 00 Rn Rt`。opc 00 是写，01 读后补零，10 读后按符号扩展到 64 位，11 扩展到 32 位；size 11 没有带符号扩展的读，size 10 没有扩展到 32 位的，这三种未定义（与 LLVM 相同）。
- **Tier 0**（`core/engine/src/c/atomic.c`）：`atomic_target` 加上偏移（基址是 SP 时照旧先查 SP 对齐，再加偏移），照 LDAR 用 acquire 读、照 STLR 用 release 写，要自然对齐，没对齐是对齐错误。宿主有 LSE2（cpuinfo 的 `uscat`）时这类访问在 16 字节以内可以不对齐，appine 不宣告 LSE2，与 LDAR 一样都要求对齐。
- **Tier 1**（`dbt.c`）：这片编码算作原子访问（`is_atomic`），不与别的访存共用 TLB 检查。`emit_atomic_address` 加上偏移参数：基址加偏移之后检查对齐和软件 MMU（读按读、写按写），宿主地址算好之后把指令里的偏移清零，在宿主地址上原样执行。
- **宣告**：宿主的 `AT_HWCAP` 有第 26 位、而且也宣告了 LRCPC 时才宣告；`ID_AA64ISAR1_EL1` 的 LRCPC 字段是 2；`/proc/cpuinfo` 的名字表补到第 26 位。宿主还有 LRCPC3（Linux 报 3）时这里仍是 2。

## 测试

- lockstep 新类别 `lrcpc2`：四种宽度、各种 opc，偏移在 -256 到 255 之间按宽度对齐，基址加偏移指向数据区里对齐的地方。20 万轮的加长 lockstep 里执行 200000 条，全部一致，Tier 1 没有退回 Tier 0。变异检验：Tier 1 不加偏移，第一轮就对不上。
- 白名单与 llvm-mc 的对照加上 `+rcpc-immo` 和新的抽样空间 `rcpc_unscaled`。20 万个样本里 162073 个能执行（16 种 size、opc 的组合里 13 种合法），与 LLVM 一致；空间可以声明要的特性，宿主没有 LRCPC2 时不要求它有能执行的样本。变异检验：去掉 `+rcpc-immo` 之后报出各种宽度的分歧。
- 单元测试 `lrcpc2_follows_the_feature_set`：LDAPUR、两种扩展宽度的 LDAPURSB、STLUR 的结果，没对齐是对齐错误，宿主没有时是未定义指令；`id_registers_match_hwcap` 的 LRCPC 字段按 LRCPC、LRCPC2 是 1、2；`unimplemented_encodings_are_undefined` 里的 LDAPUR 换成 size 11、opc 10 的未分配编码。
- 差分测试 `c-programs` 组新增 `lrcpc2.c`（汇编器不一定认这些助记符，用 `.inst` 写指令字）：HWCAP、ID 寄存器的字段至少是 2、cpuinfo 的 `ilrcpc`；各种宽度、补零和符号扩展的读，正负偏移，`.rodata` 上的读，三种宽度的写。两个执行层下都与原生相同。
- 没有要测的收益：运行时按 HWCAP 选它的程序很少，这一轮是让按 v8.4 编译的程序能跑。
- cargo test、clippy（含 Android）、两层的全部差分测试通过。LTP 的 syscall 测试 1001 个通过（上次 1000）：mmap21_02 由失败变回通过（[偶发项](../milestones.md#已知的偶发失败)），其余逐项相同。

**推迟项**：

- LRCPC3（LDIAPP、STILP、带回写的 LDAPR、STLR 这些）不做。
- LSE2（16 字节以内不对齐的原子访问、`uscat`）不宣告，appine 里这些访问仍要求对齐。
