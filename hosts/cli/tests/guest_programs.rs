//! 端到端测试：用宿主 `cc` 把 tests/guest/*.S 汇编成静态 ELF，再交给 appine-run 运行。
//! 每个用例都分别在 Tier 0 和 Tier 1 下运行，结果必须相同。
//!
//! 生成 Linux aarch64 ELF 需要 Linux arm64 宿主（设计文档 §13.3），其他宿主上整个文件不参与编译。
#![cfg(all(target_os = "linux", target_arch = "aarch64"))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TIERS: [&str; 2] = ["0", "1"];

fn build_guest(name: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/guest")
        .join(format!("{name}.S"));
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("guest-{name}"));
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let status = Command::new(&cc)
        .args([
            "-nostdlib",
            "-static",
            "-no-pie",
            "-Wl,--build-id=none",
            "-o",
        ])
        .arg(&out)
        .arg(&src)
        .status()
        .unwrap_or_else(|e| panic!("failed to run {cc}: {e}"));
    assert!(status.success(), "assembling {} failed", src.display());
    out
}

/// 在每个执行层下运行一次 guest 程序，返回 (执行层, 输出)。
fn run(name: &str, args: &[&str]) -> Vec<(&'static str, Output)> {
    let program = build_guest(name);
    TIERS
        .iter()
        .map(|&tier| {
            let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
                .args(["--tier", tier])
                .arg(&program)
                .args(args)
                .output()
                .expect("failed to run appine-run");
            (tier, out)
        })
        .collect()
}

fn stdout(out: &Output) -> &str {
    std::str::from_utf8(&out.stdout).unwrap()
}

fn stderr(out: &Output) -> &str {
    std::str::from_utf8(&out.stderr).unwrap()
}

#[test]
fn hello_world() {
    for (tier, out) in run("hello", &[]) {
        assert_eq!(stdout(&out), "Hello from appine!\n", "tier {tier}");
        assert_eq!(out.status.code(), Some(0), "tier {tier}: {}", stderr(&out));
    }
}

#[test]
fn exit_code_is_propagated() {
    for (tier, out) in run("exit42", &[]) {
        assert_eq!(out.status.code(), Some(42), "tier {tier}");
    }
}

#[test]
fn fib_uses_stack_and_calls() {
    for (tier, out) in run("fib", &[]) {
        assert_eq!(stdout(&out), "fib(50) = 12586269025\n", "tier {tier}");
        assert_eq!(out.status.code(), Some(0), "tier {tier}: {}", stderr(&out));
    }
}

#[test]
fn argv_reaches_guest() {
    for (tier, out) in run("echo", &["hello", "appine", "world"]) {
        assert_eq!(stdout(&out), "hello appine world\n", "tier {tier}");
        assert_eq!(out.status.code(), Some(0), "tier {tier}: {}", stderr(&out));
    }
}

#[test]
fn unknown_syscall_returns_enosys() {
    for (tier, out) in run("enosys", &[]) {
        assert_eq!(out.status.code(), Some(0), "tier {tier}: {}", stderr(&out));
    }
}

#[test]
fn segfault_kills_guest() {
    for (tier, out) in run("segv", &[]) {
        assert_eq!(out.status.code(), Some(128 + 11), "tier {tier}");
        assert!(
            stderr(&out).contains("SIGSEGV"),
            "tier {tier}: {}",
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("addr=0x1000"),
            "tier {tier}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn undefined_instruction_kills_guest() {
    for (tier, out) in run("undef", &[]) {
        assert_eq!(out.status.code(), Some(128 + 4), "tier {tier}");
        assert!(
            stderr(&out).contains("SIGILL"),
            "tier {tier}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn missing_program_exits_127() {
    let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
        .arg("/nonexistent/program")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
}
