//! 端到端测试：用宿主 `cc` 把 tests/guest/*.S 汇编成静态 ELF，再交给 appine-run 运行。
//!
//! 生成 Linux aarch64 ELF 需要 Linux arm64 宿主（设计文档 §13.3），其他宿主上整个文件不参与编译。
#![cfg(all(target_os = "linux", target_arch = "aarch64"))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

fn run(name: &str, args: &[&str]) -> Output {
    let program = build_guest(name);
    Command::new(env!("CARGO_BIN_EXE_appine-run"))
        .arg(&program)
        .args(args)
        .output()
        .expect("failed to run appine-run")
}

fn stdout(out: &Output) -> &str {
    std::str::from_utf8(&out.stdout).unwrap()
}

fn stderr(out: &Output) -> &str {
    std::str::from_utf8(&out.stderr).unwrap()
}

#[test]
fn hello_world() {
    let out = run("hello", &[]);
    assert_eq!(stdout(&out), "Hello from appine!\n");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
}

#[test]
fn exit_code_is_propagated() {
    let out = run("exit42", &[]);
    assert_eq!(out.status.code(), Some(42));
}

#[test]
fn fib_uses_stack_and_calls() {
    let out = run("fib", &[]);
    assert_eq!(stdout(&out), "fib(50) = 12586269025\n");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
}

#[test]
fn argv_reaches_guest() {
    let out = run("echo", &["hello", "appine", "world"]);
    assert_eq!(stdout(&out), "hello appine world\n");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
}

#[test]
fn unknown_syscall_returns_enosys() {
    let out = run("enosys", &[]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
}

#[test]
fn segfault_kills_guest() {
    let out = run("segv", &[]);
    assert_eq!(out.status.code(), Some(128 + 11));
    assert!(stderr(&out).contains("SIGSEGV"), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("addr=0x1000"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn undefined_instruction_kills_guest() {
    let out = run("undef", &[]);
    assert_eq!(out.status.code(), Some(128 + 4));
    assert!(stderr(&out).contains("SIGILL"), "stderr: {}", stderr(&out));
}

#[test]
fn missing_program_exits_127() {
    let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
        .arg("/nonexistent/program")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
}
