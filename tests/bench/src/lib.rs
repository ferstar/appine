//! M2 性能探针用到的基准程序（设计文档 §14）。
//!
//! 程序位于 `programs/`，不依赖 libc，只用 `write`/`exit_group`，并用
//! `-mgeneral-regs-only` 禁止 SIMD，这样 M0 的内核层和 M2 的原型 Tier 1 都能
//! 运行它们。构建需要 Linux aarch64 宿主上的 gcc。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 一个基准程序。
#[derive(Debug, Clone, Copy)]
pub struct Program {
    /// 名字，也是 `programs/` 下的源文件名。
    pub name: &'static str,
    /// 测量时传入的轮数，选到让原生运行约 0.3 秒。
    pub rounds: u64,
    /// 正确性测试用的小轮数。
    pub test_rounds: u64,
    /// 主要考察的负载特征。
    pub workload: &'static str,
}

/// 全部基准程序。
pub const PROGRAMS: &[Program] = &[
    Program {
        name: "sha256",
        rounds: 4000,
        test_rounds: 4,
        workload: "32-bit ALU",
    },
    Program {
        name: "lz",
        rounds: 160,
        test_rounds: 1,
        workload: "loads, stores, branches",
    },
    Program {
        name: "sort",
        rounds: 24,
        test_rounds: 1,
        workload: "loads, stores, unpredictable branches",
    },
    Program {
        name: "crc32",
        rounds: 2000,
        test_rounds: 4,
        workload: "table lookups",
    },
    Program {
        name: "vm",
        rounds: 8_000_000,
        test_rounds: 20_000,
        workload: "indirect branches",
    },
];

/// 编译参数：静态、非 PIE、不依赖 libc、不使用 SIMD 和 PAC/BTI。
pub const CFLAGS: &[&str] = &[
    "-O2",
    "-std=c11",
    "-static",
    "-no-pie",
    "-fno-pie",
    "-nostdlib",
    "-ffreestanding",
    "-mgeneral-regs-only",
    "-fno-stack-protector",
    "-mbranch-protection=none",
    "-fno-tree-loop-distribute-patterns",
    "-fno-asynchronous-unwind-tables",
    "-Wall",
    "-Wextra",
    "-Werror",
    "-Wl,--build-id=none",
];

/// 源文件目录。
pub fn programs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("programs")
}

/// 把 `program` 编译到 `out_dir`，返回可执行文件路径。
pub fn build(program: &Program, out_dir: &Path) -> Result<PathBuf, String> {
    let dir = programs_dir();
    let out = out_dir.join(program.name);
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let output = Command::new(&cc)
        .args(CFLAGS)
        .arg("-o")
        .arg(&out)
        .arg(dir.join(format!("{}.c", program.name)))
        .arg(dir.join("rt.c"))
        .output()
        .map_err(|e| format!("failed to run {cc}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "building {} failed:\n{}",
            program.name,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(out)
}
