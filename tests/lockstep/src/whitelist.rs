//! 白名单对照（设计文档 §4.4）：Tier 0 认为能执行的编码，必须恰好是 LLVM 在
//! 我们所宣告的特性下能解码的编码。
//!
//! lockstep 用硬件检查语义，但硬件分不清一条编码属于 ARMv8.0 基础集还是我们
//! 没有宣告的扩展（比如 FP16）。LLVM 的反汇编器只解码启用了的特性，正好补上
//! 这一块。

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use appine_engine::{Cpu, Exit, Mmu, hwcap, hwcap2};

use crate::rng::Rng;

/// 一个被抽样的编码空间。
pub struct Space {
    /// 名字。
    pub name: &'static str,
    /// 固定为 1 或 0 的位。
    pub mask: u32,
    /// 这些位的取值。
    pub value: u32,
}

/// 全部抽样空间。
pub const SPACES: &[Space] = &[
    Space {
        name: "simd_fp_dp",
        mask: 0x0e00_0000,
        value: 0x0e00_0000, // bits 27:25 = 111
    },
    Space {
        name: "simd_ldst",
        mask: 0x0e00_0000,
        value: 0x0c00_0000, // bits 27:25 = 110：bit 26 = 1 的访存
    },
    Space {
        name: "exclusive",
        mask: 0x3f00_0000,
        value: 0x0800_0000,
    },
    Space {
        name: "lse",
        mask: 0x3f20_0c00,
        value: 0x3820_0000,
    },
    Space {
        name: "dp_2src",
        mask: 0x5fe0_0000,
        value: 0x1ac0_0000,
    },
];

/// 一处分歧。
#[derive(Debug, Clone)]
pub struct Disagreement {
    /// 编码空间。
    pub space: &'static str,
    /// 指令编码。
    pub insn: u32,
    /// Tier 0 是否认为能执行。
    pub tier0: bool,
    /// LLVM 是否能解码（在宣告的特性下）。
    pub llvm: bool,
    /// LLVM 打开全部特性时的反汇编，用来辨认是哪条指令。
    pub text: String,
}

/// 找到可用的 llvm-mc：环境变量 `APPINE_LLVM_MC`，否则在 PATH 里找。
pub fn find_llvm_mc() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("APPINE_LLVM_MC") {
        return Some(PathBuf::from(p));
    }
    let names = [
        "llvm-mc",
        "llvm-mc-19",
        "llvm-mc-18",
        "llvm-mc-17",
        "llvm-mc-16",
    ];
    names.iter().map(PathBuf::from).find(|name| {
        Command::new(name)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// guest 特性集合对应的 LLVM `-mattr`。
fn mattr(caps: u64, caps2: u64) -> String {
    let mut attrs = vec!["-fullfp16".to_string()];
    let flag = |on: bool, name: &str| format!("{}{name}", if on { '+' } else { '-' });
    attrs.push(flag(caps & hwcap::FP != 0, "fp-armv8"));
    attrs.push(flag(caps & hwcap::ASIMD != 0, "neon"));
    attrs.push(flag(caps & (hwcap::AES | hwcap::PMULL) != 0, "aes"));
    attrs.push(flag(caps & (hwcap::SHA1 | hwcap::SHA2) != 0, "sha2"));
    attrs.push(flag(caps & hwcap::CRC32 != 0, "crc"));
    attrs.push(flag(caps & hwcap::ATOMICS != 0, "lse"));
    attrs.push(flag(caps & hwcap::ASIMDDP != 0, "dotprod"));
    // LLVM 的 sha3 同时管 SHA3 和 SHA512，guest 也总是一起宣告
    attrs.push(flag(caps & hwcap::SHA3 != 0, "sha3"));
    attrs.push(flag(caps2 & hwcap2::I8MM != 0, "i8mm"));
    attrs.push(flag(caps & hwcap::LRCPC != 0, "rcpc"));
    attrs.join(",")
}

/// 用 llvm-mc 反汇编一批编码，返回每条是否合法以及反汇编文本。
fn disassemble(llvm_mc: &Path, insns: &[u32], mattr: &str, dir: &Path) -> Vec<(bool, String)> {
    let input = dir.join(format!("whitelist-{}.txt", std::process::id()));
    let mut text = String::new();
    for insn in insns {
        let b = insn.to_le_bytes();
        writeln!(
            text,
            "{:#04x} {:#04x} {:#04x} {:#04x}",
            b[0], b[1], b[2], b[3]
        )
        .unwrap();
    }
    std::fs::write(&input, text).expect("cannot write llvm-mc input");
    let out = Command::new(llvm_mc)
        .args([
            "--disassemble",
            "-triple=aarch64",
            &format!("-mattr={mattr}"),
        ])
        .arg(&input)
        .output()
        .expect("cannot run llvm-mc");
    let _ = std::fs::remove_file(&input);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let invalid: HashSet<usize> = stderr
        .lines()
        .filter(|l| l.contains("invalid instruction encoding"))
        .filter_map(|l| l.split(':').nth(1)?.parse().ok())
        .collect();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut decoded = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('.'));
    (1..=insns.len())
        .map(|line| {
            if invalid.contains(&line) {
                (false, String::new())
            } else {
                (true, decoded.next().unwrap_or("?").to_string())
            }
        })
        .collect()
}

/// 一个编码空间的抽样统计。
#[derive(Debug, Clone)]
pub struct SpaceReport {
    /// 编码空间。
    pub space: &'static str,
    /// 样本数。
    pub samples: usize,
    /// Tier 0 认为能执行的样本数。
    pub defined: usize,
}

/// 抽样比对 Tier 0 与 LLVM。返回各空间的统计和分歧列表（最多 `limit` 条）。
pub fn check(
    llvm_mc: &Path,
    samples: usize,
    seed: u64,
    limit: usize,
) -> (Vec<SpaceReport>, Vec<Disagreement>) {
    let caps = appine_engine::guest_hwcap();
    let caps2 = appine_engine::guest_hwcap2();
    let dir = std::env::temp_dir();
    let mmu = Mmu::new();
    // SAFETY: mmu 比 cpu 先声明、后析构
    let mut cpu = unsafe { Cpu::new(&mmu) };
    let mut found = Vec::new();
    let mut reports = Vec::new();
    for (i, space) in SPACES.iter().enumerate() {
        let mut g = Rng::new(seed ^ (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let insns: Vec<u32> = (0..samples)
            .map(|_| (g.next_u64() as u32 & !space.mask) | space.value)
            .collect();
        let restricted = disassemble(llvm_mc, &insns, &mattr(caps, caps2), &dir);
        let mut disagree = Vec::new();
        let mut defined = 0;
        for (k, &insn) in insns.iter().enumerate() {
            cpu.state_mut().pc = 0x1000;
            cpu.state_mut().sp = 0;
            let tier0 = !matches!(cpu.exec_insn(insn), Some(Exit::Undefined { .. }));
            defined += tier0 as usize;
            if tier0 != restricted[k].0 {
                disagree.push((insn, tier0, restricted[k].0));
            }
        }
        reports.push(SpaceReport {
            space: space.name,
            samples,
            defined,
        });
        if disagree.is_empty() {
            continue;
        }
        // 用全部特性再反汇编一次，辨认分歧的编码是哪条指令
        let codes: Vec<u32> = disagree.iter().map(|d| d.0).collect();
        let full = disassemble(llvm_mc, &codes, "+all", &dir);
        for ((insn, tier0, llvm), (_, text)) in disagree.into_iter().zip(full) {
            if found.len() < limit {
                found.push(Disagreement {
                    space: space.name,
                    insn,
                    tier0,
                    llvm,
                    text,
                });
            }
        }
    }
    (reports, found)
}
