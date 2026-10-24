//! 指令级 lockstep 测试（设计文档 §13.3 第 1 层）：宿主 CPU 就是标准答案。
//!
//! 每个随机用例（最多 4 条指令、随机的寄存器和 FP 状态、随机的数据区内容）分别
//! 在硬件、Tier 0 和 Tier 1 上执行一次，逐项比对 X0–X30、NZCV、V0–V31、FPCR、
//! FPSR、PC、SP 和数据区。原生执行端只能在 Linux aarch64 宿主上编译，其他宿主上
//! [`AVAILABLE`] 为 false。

pub mod classes;
#[cfg(lockstep_native)]
mod native;
pub mod rng;
#[cfg(lockstep_native)]
pub mod whitelist;

use std::fmt;

/// 当前宿主能否运行 lockstep。
pub const AVAILABLE: bool = cfg!(lockstep_native);

/// 被测指令执行前后的状态，布局与 oracle.c 的 `struct lockstep_state` 一致。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    /// X0–X30。
    pub x: [u64; 31],
    /// 条件标志，bit 31..28。
    pub nzcv: u64,
    /// V0–V31，低 64 位在前。
    pub v: [[u64; 2]; 32],
    /// 浮点控制寄存器。
    pub fpcr: u64,
    /// 浮点状态寄存器。
    pub fpsr: u64,
}

/// 一次运行的参数。
#[derive(Debug, Clone)]
pub struct Config {
    /// 每个类别生成多少个用例（再乘以类别的权重）。
    pub iterations: u64,
    /// 随机种子；同一个种子总是生成同样的测试序列。
    pub seed: u64,
    /// 只运行这个类别。
    pub class: Option<String>,
}

/// 一个类别的统计结果。
#[derive(Debug, Clone)]
pub struct ClassReport {
    /// 类别名。
    pub name: &'static str,
    /// 硬件和两层解释器都执行且结果一致的次数。
    pub executed: u64,
    /// 三方都判定为未定义指令的次数。
    pub both_undefined: u64,
    /// 硬件能执行、解释器按白名单拒绝的次数（只出现在模糊类别）。
    pub stricter: u64,
    /// Tier 1 退回 Tier 0 执行单条指令的次数（TLB 未命中、跨页或者没有翻译的指令）。
    pub tier1_fallback: u64,
    /// guest 特性集合缺少该类别需要的特性，没有运行。
    pub skipped: bool,
}

/// 一方执行后的结果。
#[derive(Debug, Clone)]
pub struct Outcome {
    /// 寄存器状态。
    pub state: State,
    /// 数据区内容。
    pub data: Vec<u8>,
}

/// 第一处不一致。
#[derive(Debug, Clone)]
pub struct Mismatch {
    /// 类别名。
    pub class: &'static str,
    /// 被测指令。
    pub insns: Vec<u32>,
    /// 本次运行的种子。
    pub seed: u64,
    /// 类别内的序号。
    pub iteration: u64,
    /// 不一致的原因。
    pub reason: String,
    /// 执行前的状态。
    pub before: State,
    /// 硬件执行后的结果。
    pub hardware: Option<Outcome>,
    /// 出现分歧的那一层解释器执行后的结果。
    pub interpreter: Option<Outcome>,
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let insns: Vec<String> = self.insns.iter().map(|i| format!("{i:#010x}")).collect();
        writeln!(
            f,
            "lockstep mismatch in class `{}`: insns [{}]: {}",
            self.class,
            insns.join(", "),
            self.reason
        )?;
        writeln!(
            f,
            "  reproduce: cargo run -p appine-lockstep -- --class {} --seed {}  (iteration {})",
            self.class, self.seed, self.iteration
        )?;
        let (Some(hw), Some(interp)) = (&self.hardware, &self.interpreter) else {
            return Ok(());
        };
        let (b, h, i) = (&self.before, &hw.state, &interp.state);
        for r in 0..31 {
            if h.x[r] != i.x[r] {
                writeln!(
                    f,
                    "  x{r:<2} before={:#018x} hardware={:#018x} interpreter={:#018x}",
                    b.x[r], h.x[r], i.x[r]
                )?;
            }
        }
        for r in 0..32 {
            if h.v[r] != i.v[r] {
                let q = |v: [u64; 2]| format!("{:016x}_{:016x}", v[1], v[0]);
                writeln!(
                    f,
                    "  v{r:<2} before={} hardware={} interpreter={}",
                    q(b.v[r]),
                    q(h.v[r]),
                    q(i.v[r])
                )?;
            }
        }
        for (name, bv, hv, iv) in [
            ("nzcv", b.nzcv, h.nzcv, i.nzcv),
            ("fpcr", b.fpcr, h.fpcr, i.fpcr),
            ("fpsr", b.fpsr, h.fpsr, i.fpsr),
        ] {
            if hv != iv {
                writeln!(
                    f,
                    "  {name} before={bv:#x} hardware={hv:#x} interpreter={iv:#x}"
                )?;
            }
        }
        let diff: Vec<usize> = (0..hw.data.len().min(interp.data.len()))
            .filter(|&k| hw.data[k] != interp.data[k])
            .collect();
        if let Some(&first) = diff.first() {
            writeln!(
                f,
                "  data: {} bytes differ, first at offset {first:#x} (hardware {:#04x}, interpreter {:#04x})",
                diff.len(),
                hw.data[first],
                interp.data[first]
            )?;
        }
        Ok(())
    }
}

/// 运行 lockstep，返回各类别的统计；遇到第一处不一致时返回它。
#[cfg(lockstep_native)]
pub fn run(config: &Config) -> Result<Vec<ClassReport>, Box<Mismatch>> {
    runner::run(config)
}

/// 当前宿主不支持 lockstep（[`AVAILABLE`] 为 false），调用会 panic。
#[cfg(not(lockstep_native))]
pub fn run(_config: &Config) -> Result<Vec<ClassReport>, Box<Mismatch>> {
    panic!("lockstep needs a Linux aarch64 host")
}

#[cfg(lockstep_native)]
mod runner {
    use std::ptr::NonNull;

    use appine_engine::{Cpu, Dbt, Exit, Mmu, guest_hwcap, prot};

    use super::*;
    use crate::classes::{CLASSES, Case, Class, DATA_BYTES, Env, Gen};
    use crate::native::Oracle;
    use crate::rng::Rng;

    /// 解释器执行前把 SP 设成这个值；被测指令不应该改动它。
    const SP_SENTINEL: u64 = 0x0000_7fff_dead_bef0;
    const NOP: u32 = 0xd503_201f;
    const SVC: u32 = 0xd400_0001;
    /// 独占访问的用例在硬件上可能偶发失败，不一致时最多重跑这么多次。
    const FLAKY_RETRIES: usize = 8;

    /// 让各个类别的随机序列互不相关，同时可以单独复现。
    fn class_seed(seed: u64, name: &str) -> u64 {
        name.bytes().fold(seed ^ 0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }

    /// 偏向边界值的通用寄存器取值，提高标志位和溢出路径的覆盖率。
    fn random_value(g: &mut Rng) -> u64 {
        const SPECIAL: &[u64] = &[
            0,
            1,
            2,
            0x7f,
            0x80,
            0xff,
            0x7fff_ffff,
            0x8000_0000,
            0xffff_ffff,
            0x1_0000_0000,
            0xffff_ffff_8000_0000,
            0x7fff_ffff_ffff_ffff,
            0x8000_0000_0000_0000,
            0xffff_ffff_ffff_fffe,
            0xffff_ffff_ffff_ffff,
        ];
        match g.below(8) {
            0 | 1 => g.pick(SPECIAL),
            2 => g.below(64) as u64,
            3 => (g.below(64) as i64 - 32) as u64,
            4 => g.next_u64() & 0xffff_ffff,
            5 => g.pick(SPECIAL) ^ (1 << g.below(64)),
            _ => g.next_u64(),
        }
    }

    /// 偏向浮点特殊值的 64 位向量通道：一个双精度数，或者两个单精度数。
    fn random_lane(g: &mut Rng) -> u64 {
        const F64: &[u64] = &[
            0,
            0x8000_0000_0000_0000, // -0
            0x3ff0_0000_0000_0000, // 1.0
            0xbff0_0000_0000_0000, // -1.0
            0x3fe0_0000_0000_0000, // 0.5
            0x7ff0_0000_0000_0000, // +inf
            0xfff0_0000_0000_0000, // -inf
            0x7ff8_0000_0000_0000, // qNaN
            0x7ff4_0000_0000_0001, // sNaN
            0x0000_0000_0000_0001, // 最小的非规格化数
            0x000f_ffff_ffff_ffff, // 最大的非规格化数
            0x0010_0000_0000_0000, // 最小的规格化数
            0x7fef_ffff_ffff_ffff, // 最大的有限数
            0x41e0_0000_0000_0000, // 2^31
            0x43e0_0000_0000_0000, // 2^63
        ];
        const F32: &[u32] = &[
            0,
            0x8000_0000,
            0x3f80_0000,
            0xbf80_0000,
            0x3f00_0000,
            0x7f80_0000,
            0xff80_0000,
            0x7fc0_0000,
            0x7fa0_0001,
            0x0000_0001,
            0x007f_ffff,
            0x0080_0000,
            0x7f7f_ffff,
            0x4f00_0000,
            0x5f00_0000,
        ];
        match g.below(4) {
            0 => g.pick(F64),
            1 => g.pick(F32) as u64 | (g.pick(F32) as u64) << 32,
            _ => g.next_u64(),
        }
    }

    fn random_state(g: &mut Rng) -> State {
        let mut x = [0; 31];
        for v in &mut x {
            *v = random_value(g);
        }
        let mut v = [[0; 2]; 32];
        for lanes in &mut v {
            *lanes = [random_lane(g), random_lane(g)];
        }
        State {
            x,
            nzcv: (g.bits(4) as u64) << 28,
            v,
            fpcr: g.next_u64() & 0x07c0_0000, // AHP、DN、FZ、RMode
            fpsr: g.next_u64() & 0x0800_009f, // QC 和累积异常位
        }
    }

    #[repr(C, align(4096))]
    struct Page([u8; 4096]);

    /// 解释器一侧的执行环境：被测指令所在的代码页和数据区，映射在与硬件相同的 guest 地址上。
    struct Machine {
        // 析构顺序：dbt → cpu → mmu → 页
        dbt: Option<Dbt>,
        cpu: Cpu,
        _mmu: Mmu,
        code: Box<Page>,
        data: Box<[Page; 2]>,
        slot: u64,
    }

    impl Machine {
        fn new(slot: u64, data_va: u64, tier1: bool) -> Self {
            let mut code = Box::new(Page([0; 4096]));
            let mut data = Box::new([Page([0; 4096]), Page([0; 4096])]);
            let mut mmu = Mmu::new();
            let code_host = NonNull::from(&mut code.0).cast::<u8>();
            let data_host = NonNull::from(&mut *data).cast::<u8>();
            // SAFETY: 两块内存都由 Machine 持有，比 mmu 活得久
            unsafe {
                mmu.map(slot & !0xfff, code_host, 4096, prot::READ | prot::EXEC)
                    .unwrap();
                mmu.map(data_va, data_host, DATA_BYTES, prot::READ | prot::WRITE)
                    .unwrap();
            }
            // SAFETY: mmu 由 Machine 持有，比 cpu 活得久
            let cpu = unsafe { Cpu::new(&mmu) };
            let dbt = tier1.then(|| Dbt::new().expect("tier 1 is unavailable on this host"));
            Machine {
                dbt,
                cpu,
                _mmu: mmu,
                code,
                data,
                slot,
            }
        }

        fn data(&self) -> &[u8] {
            // SAFETY: 两页在数组里连续存放
            unsafe {
                std::slice::from_raw_parts(self.data.as_ptr().cast::<u8>(), DATA_BYTES as usize)
            }
        }

        /// 与硬件相同地布置代码页和数据区，执行到第 5 个字处的 svc 为止。
        fn run(
            &mut self,
            page: &[u8],
            insns: &[u32],
            before: &State,
            data: &[u8],
        ) -> (Exit, State) {
            let off = (self.slot & 0xfff) as usize;
            self.code.0.copy_from_slice(page);
            for k in 0..4 {
                let insn = insns.get(k).copied().unwrap_or(NOP);
                self.code.0[off + 4 * k..off + 4 * k + 4].copy_from_slice(&insn.to_le_bytes());
            }
            self.code.0[off + 16..off + 20].copy_from_slice(&SVC.to_le_bytes());
            // SAFETY: 两页在数组里连续存放
            let dst = unsafe {
                std::slice::from_raw_parts_mut(
                    self.data.as_mut_ptr().cast::<u8>(),
                    DATA_BYTES as usize,
                )
            };
            dst.copy_from_slice(data);

            let s = self.cpu.state_mut();
            s.x = before.x;
            s.nzcv = before.nzcv;
            s.v = before.v;
            s.fpcr = before.fpcr;
            s.fpsr = before.fpsr;
            s.sp = SP_SENTINEL;
            s.pc = self.slot;
            let exit = match &mut self.dbt {
                Some(dbt) => {
                    dbt.flush();
                    dbt.run(&mut self.cpu)
                }
                None => self.cpu.run(),
            };
            let s = self.cpu.state();
            let after = State {
                x: s.x,
                nzcv: s.nzcv,
                v: s.v,
                fpcr: s.fpcr,
                fpsr: s.fpsr,
            };
            (exit, after)
        }
    }

    struct Harness {
        oracle: Oracle,
        page: Vec<u8>,
        tiers: [(&'static str, Machine); 2],
    }

    pub(super) fn run(config: &Config) -> Result<Vec<ClassReport>, Box<Mismatch>> {
        let oracle = Oracle::new().expect("cannot create lockstep oracle");
        let slot = oracle.slot_addr();
        let data_va = oracle.data_addr();
        let page = oracle.slot_page().to_vec();
        let mut h = Harness {
            oracle,
            page,
            tiers: [
                ("tier 0", Machine::new(slot, data_va, false)),
                ("tier 1", Machine::new(slot, data_va, true)),
            ],
        };
        let mut reports = Vec::new();
        for class in CLASSES {
            if config.class.as_deref().is_some_and(|c| c != class.name) {
                continue;
            }
            reports.push(run_class(class, config, &mut h)?);
        }
        Ok(reports)
    }

    fn run_class(
        class: &'static Class,
        config: &Config,
        h: &mut Harness,
    ) -> Result<ClassReport, Box<Mismatch>> {
        let mut report = ClassReport {
            name: class.name,
            executed: 0,
            both_undefined: 0,
            stricter: 0,
            tier1_fallback: 0,
            skipped: guest_hwcap() & class.requires != class.requires,
        };
        if report.skipped {
            return Ok(report);
        }
        let interp_exits = |h: &Harness| {
            h.tiers[1]
                .1
                .dbt
                .as_ref()
                .map_or(0, |d| d.stats().exits_interp)
        };
        let start_exits = interp_exits(h);
        let mut g = Rng::new(class_seed(config.seed, class.name));
        let slot = h.oracle.slot_addr();
        let data_va = h.oracle.data_addr();
        let env = Env {
            slot_offset: slot & 0xfff,
        };

        for iteration in 0..config.iterations * class.weight {
            let case = match class.generate {
                Gen::Insn(f) => Case {
                    insns: vec![f(&mut g)],
                    ..Case::default()
                },
                Gen::Case(f) => f(&mut g, &env),
            };
            let mut before = random_state(&mut g);
            let mut data = vec![0u8; DATA_BYTES as usize];
            if !case.pointers.is_empty() {
                for chunk in data.chunks_mut(8) {
                    chunk.copy_from_slice(&g.next_u64().to_le_bytes());
                }
            }
            for &(r, off) in &case.pointers {
                before.x[r as usize] = data_va.wrapping_add(off as u64);
            }
            for &(r, v) in &case.values {
                if r < 31 {
                    before.x[r as usize] = v;
                }
            }
            for &(r, off) in &case.loads {
                // 靠近数据区末尾时不足 8 字节，高位补 0（指令只用到低位）
                let mut bytes = [0u8; 8];
                let end = (off as usize + 8).min(data.len());
                bytes[..end - off as usize].copy_from_slice(&data[off as usize..end]);
                before.x[r as usize] = u64::from_le_bytes(bytes);
            }

            let mismatch =
                |reason: String, hardware: Option<Outcome>, interpreter: Option<Outcome>| {
                    Box::new(Mismatch {
                        class: class.name,
                        insns: case.insns.clone(),
                        seed: config.seed,
                        iteration,
                        reason,
                        before,
                        hardware,
                        interpreter,
                    })
                };

            let mut hw = run_hardware(&mut h.oracle, &case.insns, &before, &data);
            let mut kinds = Vec::new();
            for t in 0..2 {
                let name = h.tiers[t].0;
                let machine = &mut h.tiers[t].1;
                let (exit, state) = machine.run(&h.page, &case.insns, &before, &data);
                let pc = machine.cpu.state().pc;
                let sp = machine.cpu.state().sp;
                let interp = Outcome {
                    state,
                    data: machine.data().to_vec(),
                };
                let verdict = |hw: &(Result<(), i32>, Outcome)| -> Result<&'static str, String> {
                    match (hw.0, exit) {
                        (Err(4), Exit::Undefined { .. }) => Ok("undefined"),
                        (Err(4), other) => {
                            Err(format!("hardware raised SIGILL, {name} returned {other:?}"))
                        }
                        (Err(sig), _) => Err(format!(
                            "hardware raised signal {sig}; the generator must not emit such cases"
                        )),
                        (Ok(()), Exit::Undefined { .. }) if !class.strict => Ok("stricter"),
                        (Ok(()), Exit::Svc { .. }) => {
                            if hw.1.state != interp.state || hw.1.data != interp.data {
                                Err(format!("{name}: results differ"))
                            } else if pc != slot + 20 || sp != SP_SENTINEL {
                                Err(format!("{name} left pc={pc:#x} sp={sp:#x}"))
                            } else {
                                Ok("executed")
                            }
                        }
                        (Ok(()), other) => {
                            Err(format!("hardware executed it, {name} returned {other:?}"))
                        }
                    }
                };
                let mut result = verdict(&hw);
                // 独占访问在硬件上可能被中断清掉标记，重跑几次看能否得到相同结果
                let mut retries = 0;
                while result.is_err() && class.flaky && retries < FLAKY_RETRIES {
                    hw = run_hardware(&mut h.oracle, &case.insns, &before, &data);
                    result = verdict(&hw);
                    retries += 1;
                }
                match result {
                    Ok(kind) => kinds.push(kind),
                    Err(reason) => return Err(mismatch(reason, Some(hw.1.clone()), Some(interp))),
                }
            }
            if kinds[0] != kinds[1] {
                return Err(mismatch(
                    format!("tier 0 and tier 1 disagree: {} vs {}", kinds[0], kinds[1]),
                    None,
                    None,
                ));
            }
            match kinds[0] {
                "executed" => report.executed += 1,
                "undefined" => report.both_undefined += 1,
                _ => report.stricter += 1,
            }
        }
        report.tier1_fallback = interp_exits(h) - start_exits;
        Ok(report)
    }

    fn run_hardware(
        oracle: &mut Oracle,
        insns: &[u32],
        before: &State,
        data: &[u8],
    ) -> (Result<(), i32>, Outcome) {
        oracle.data().copy_from_slice(data);
        let mut state = *before;
        let result = oracle.run(insns, &mut state);
        (
            result,
            Outcome {
                state,
                data: oracle.data().to_vec(),
            },
        )
    }
}
