//! 指令级 lockstep 测试（设计文档 §13.3 第 1 层）：宿主 CPU 就是标准答案。
//!
//! 对每条随机生成的指令和随机寄存器状态，分别在硬件上和解释器里执行一次，
//! 逐个比对 X0–X30、NZCV、PC 和 SP。原生执行端只能在 Linux aarch64 宿主上
//! 编译，其他宿主上 [`AVAILABLE`] 为 false。

pub mod classes;
#[cfg(lockstep_native)]
mod native;
pub mod rng;

use std::fmt;

/// 当前宿主能否运行 lockstep。
pub const AVAILABLE: bool = cfg!(lockstep_native);

/// 被测指令执行前后的寄存器状态。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    /// X0–X30。
    pub x: [u64; 31],
    /// 条件标志，bit 31..28。
    pub nzcv: u64,
}

/// 一次运行的参数。
#[derive(Debug, Clone)]
pub struct Config {
    /// 每个类别生成多少条指令。
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
    /// 两边都执行且结果一致的次数。
    pub executed: u64,
    /// 两边都判定为未定义指令的次数。
    pub both_undefined: u64,
    /// 硬件能执行、解释器按白名单拒绝的次数（只出现在模糊类别）。
    pub stricter: u64,
}

/// 第一处不一致。
#[derive(Debug, Clone)]
pub struct Mismatch {
    /// 类别名。
    pub class: &'static str,
    /// 指令编码。
    pub insn: u32,
    /// 本次运行的种子。
    pub seed: u64,
    /// 类别内的序号。
    pub iteration: u64,
    /// 不一致的原因。
    pub reason: String,
    /// 执行前的状态。
    pub before: State,
    /// 硬件执行后的状态。
    pub hardware: Option<State>,
    /// 解释器执行后的状态。
    pub interpreter: Option<State>,
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "lockstep mismatch in class `{}`: insn {:#010x}: {}",
            self.class, self.insn, self.reason
        )?;
        writeln!(
            f,
            "  reproduce: cargo run -p appine-lockstep -- --class {} --seed {}  (iteration {})",
            self.class, self.seed, self.iteration
        )?;
        let (Some(hw), Some(interp)) = (self.hardware, self.interpreter) else {
            return Ok(());
        };
        for i in 0..31 {
            if hw.x[i] != interp.x[i] {
                writeln!(
                    f,
                    "  x{i:<2} before={:#018x} hardware={:#018x} interpreter={:#018x}",
                    self.before.x[i], hw.x[i], interp.x[i]
                )?;
            }
        }
        if hw.nzcv != interp.nzcv {
            writeln!(
                f,
                "  nzcv before={:#x} hardware={:#x} interpreter={:#x}",
                self.before.nzcv >> 28,
                hw.nzcv >> 28,
                interp.nzcv >> 28
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
    use appine_engine::{Cpu, Exit, Mmu};

    use super::*;
    use crate::classes::{CLASSES, Class};
    use crate::native::Oracle;
    use crate::rng::Rng;

    /// 让各个类别的随机序列互不相关，同时可以单独复现。
    fn class_seed(seed: u64, name: &str) -> u64 {
        name.bytes().fold(seed ^ 0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }

    /// 偏向边界值的寄存器取值，提高标志位和溢出路径的覆盖率。
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

    fn random_state(g: &mut Rng) -> State {
        let mut x = [0; 31];
        for v in &mut x {
            *v = random_value(g);
        }
        State {
            x,
            nzcv: (g.bits(4) as u64) << 28,
        }
    }

    /// 解释器执行前把 SP 设成这个值；数据处理指令不应该改动它。
    const SP_SENTINEL: u64 = 0x0000_7fff_dead_bef0;

    pub(super) fn run(config: &Config) -> Result<Vec<ClassReport>, Box<Mismatch>> {
        let mut oracle = Oracle::new().expect("cannot create lockstep oracle");
        let mmu = Mmu::new();
        // SAFETY: mmu 比 cpu 先声明、后析构
        let mut cpu = unsafe { Cpu::new(&mmu) };
        let mut reports = Vec::new();
        for class in CLASSES {
            if config.class.as_deref().is_some_and(|c| c != class.name) {
                continue;
            }
            reports.push(run_class(class, config, &mut oracle, &mut cpu)?);
        }
        Ok(reports)
    }

    fn run_class(
        class: &'static Class,
        config: &Config,
        oracle: &mut Oracle,
        cpu: &mut Cpu,
    ) -> Result<ClassReport, Box<Mismatch>> {
        let mut g = Rng::new(class_seed(config.seed, class.name));
        let mut report = ClassReport {
            name: class.name,
            executed: 0,
            both_undefined: 0,
            stricter: 0,
        };
        let pc = oracle.slot_addr();

        for iteration in 0..config.iterations {
            let insn = (class.generate)(&mut g);
            let before = random_state(&mut g);
            let mismatch = |reason: String, hardware: Option<State>, interpreter: Option<State>| {
                Box::new(Mismatch {
                    class: class.name,
                    insn,
                    seed: config.seed,
                    iteration,
                    reason,
                    before,
                    hardware,
                    interpreter,
                })
            };

            let mut hw = before;
            let hw_result = oracle.run(insn, &mut hw);

            let s = cpu.state_mut();
            s.x = before.x;
            s.nzcv = before.nzcv;
            s.sp = SP_SENTINEL;
            s.pc = pc;
            let exit = cpu.exec_insn(insn);
            let s = cpu.state();
            let interp = State {
                x: s.x,
                nzcv: s.nzcv,
            };

            match (hw_result, exit) {
                (Err(4), Some(Exit::Undefined { .. })) => report.both_undefined += 1,
                (Err(4), other) => {
                    return Err(mismatch(
                        format!("hardware raised SIGILL, interpreter returned {other:?}"),
                        None,
                        None,
                    ));
                }
                (Err(sig), _) => {
                    return Err(mismatch(
                        format!(
                            "hardware raised signal {sig}; the generator must not emit such encodings"
                        ),
                        None,
                        None,
                    ));
                }
                (Ok(()), Some(Exit::Undefined { .. })) if !class.strict => report.stricter += 1,
                (Ok(()), Some(other)) => {
                    return Err(mismatch(
                        format!("hardware executed it, interpreter returned {other:?}"),
                        Some(hw),
                        None,
                    ));
                }
                (Ok(()), None) => {
                    if hw != interp {
                        return Err(mismatch("results differ".into(), Some(hw), Some(interp)));
                    }
                    if s.pc != pc + 4 || s.sp != SP_SENTINEL {
                        return Err(mismatch(
                            format!("interpreter left pc={:#x} sp={:#x}", s.pc, s.sp),
                            Some(hw),
                            Some(interp),
                        ));
                    }
                    report.executed += 1;
                }
            }
        }
        Ok(report)
    }
}
