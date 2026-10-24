//! 端到端测试：用宿主 `cc` 把 tests/guest/*.S 汇编成静态 ELF，用 `musl-gcc` 把
//! tests/guest/*.c 编译成静态链接 musl 的程序，再交给 appine-run 运行。每个用例都
//! 分别在 Tier 0 和 Tier 1 下运行，结果必须相同。
//!
//! 找不到 musl-gcc（Debian/Ubuntu 的 musl-tools）时跳过 C 程序；设置
//! `APPINE_REQUIRE_MUSL=1`（CI 这样做）时改为失败。
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

/// 用 musl-gcc 静态编译 tests/guest/<name>.c；没有 musl-gcc 时返回 None。
fn build_c_guest(name: &str) -> Option<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/guest")
        .join(format!("{name}.c"));
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("guest-{name}"));
    let status = Command::new("musl-gcc")
        .args([
            "-static", "-O2", "-Wall", "-Wextra", "-Werror", "-pthread", "-o",
        ])
        .arg(&out)
        .arg(&src)
        .status();
    let Ok(status) = status else {
        assert!(
            std::env::var_os("APPINE_REQUIRE_MUSL").is_none_or(|v| v != "1"),
            "musl-gcc not found; install musl-tools"
        );
        eprintln!("skipping {name}: musl-gcc not found");
        return None;
    };
    assert!(status.success(), "compiling {} failed", src.display());
    Some(out)
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
fn threads_signals_and_fork() {
    // 互斥锁、条件变量、原子操作、线程局部存储、发给指定线程的信号、备用信号栈、
    // 访存出错后修复返回、SA_RESTART、多线程进程里的 fork、分离的线程
    let Some(program) = build_c_guest("threads") else {
        return;
    };
    for tier in TIERS {
        let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
            .args(["--tier", tier])
            .arg(&program)
            .output()
            .expect("failed to run appine-run");
        assert_eq!(
            out.status.code(),
            Some(0),
            "tier {tier}: {}{}",
            stdout(&out),
            stderr(&out)
        );
        assert!(
            stdout(&out).ends_with("all ok\n"),
            "tier {tier}: {}",
            stdout(&out)
        );
    }
}

#[test]
fn sockets_polling_and_timers() {
    // AF_UNIX（socketpair、SCM_RIGHTS、SO_PEERCRED、抽象地址）、TCP/UDP 回环、netlink、
    // epoll/eventfd/select、setitimer；都不需要外网
    let Some(program) = build_c_guest("net") else {
        return;
    };
    for tier in TIERS {
        let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
            .args(["--tier", tier])
            .arg(&program)
            .output()
            .expect("failed to run appine-run");
        assert_eq!(
            out.status.code(),
            Some(0),
            "tier {tier}: {}{}",
            stdout(&out),
            stderr(&out)
        );
        assert!(
            stdout(&out).ends_with("all ok\n"),
            "tier {tier}: {}",
            stdout(&out)
        );
    }
}

#[test]
fn ipc_timers_credentials_and_scheduling() {
    // System V 消息队列、信号量（SEM_UNDO）、共享内存跨 fork，POSIX 定时器（超限次数、
    // SIGEV_THREAD、CPU 时钟），mremap，降低权限之后的凭据规则，亲和性和实时策略，
    // RLIMIT_NOFILE
    let Some(program) = build_c_guest("posix") else {
        return;
    };
    for tier in TIERS {
        let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
            .args(["--tier", tier])
            .arg(&program)
            .output()
            .expect("failed to run appine-run");
        assert_eq!(
            out.status.code(),
            Some(0),
            "tier {tier}: {}{}",
            stdout(&out),
            stderr(&out)
        );
        assert!(
            stdout(&out).ends_with("all ok\n"),
            "tier {tier}: {}",
            stdout(&out)
        );
    }
}

#[test]
fn self_modifying_code() {
    // 像 JIT 一样改写代码：同一页反复改写、W^X 的 mprotect、memfd 双映射、另一个线程
    // 正在执行时改写，以及对未映射地址做缓存维护
    let Some(program) = build_c_guest("jit") else {
        return;
    };
    for tier in TIERS {
        let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
            .args(["--tier", tier])
            .arg(&program)
            .output()
            .expect("failed to run appine-run");
        assert_eq!(
            out.status.code(),
            Some(0),
            "tier {tier}: {}{}",
            stdout(&out),
            stderr(&out)
        );
        assert!(
            stdout(&out).ends_with("all ok\n"),
            "tier {tier}: {}",
            stdout(&out)
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
