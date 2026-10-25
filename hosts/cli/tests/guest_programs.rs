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

/// 编译好的 guest 程序的路径，和编译时写的临时文件：几个用例可能同时编译同一个程序，
/// 各自写完再改名过去，免得运行到别人写了一半的文件。
fn output(name: &str) -> (PathBuf, PathBuf) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let tmp = dir.join(format!("guest-{name}.{}.{n}", std::process::id()));
    (dir.join(format!("guest-{name}")), tmp)
}

fn build_guest(name: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/guest")
        .join(format!("{name}.S"));
    let (out, tmp) = output(name);
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let status = Command::new(&cc)
        .args([
            "-nostdlib",
            "-static",
            "-no-pie",
            "-Wl,--build-id=none",
            "-o",
        ])
        .arg(&tmp)
        .arg(&src)
        .status()
        .unwrap_or_else(|e| panic!("failed to run {cc}: {e}"));
    assert!(status.success(), "assembling {} failed", src.display());
    std::fs::rename(&tmp, &out).unwrap();
    out
}

/// 用 musl-gcc 静态编译 tests/guest/<name>.c；没有 musl-gcc 时返回 None。
fn build_c_guest(name: &str) -> Option<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/guest")
        .join(format!("{name}.c"));
    let (out, tmp) = output(name);
    let status = Command::new("musl-gcc")
        .args([
            "-static", "-O2", "-Wall", "-Wextra", "-Werror", "-pthread", "-o",
        ])
        .arg(&tmp)
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
    std::fs::rename(&tmp, &out).unwrap();
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
fn bugs_kill_only_the_guest_process() {
    // 内核层 panic（进入内核时、线程开始和退出时、定时器线程上）、引擎让宿主出错（用
    // debug 构建的测试钩子模拟）都只结束撞上它的 guest 进程，App 里别的照常运行
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(program) = build_c_guest("bugs") else {
        return;
    };
    for tier in TIERS {
        let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
            .args(["--tier", tier])
            .env("APPINE_TEST_HOOKS", "1")
            .env("APPINE_ON_BUG", "kill")
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
        // 0xa991 是测试钩子的 syscall 编号
        for report in [
            "kernel panic in syscall 43409",
            "host signal 11",
            "kernel panic while the thread was exiting",
            "kernel panic while starting the thread",
            "kernel panic in its timers",
        ] {
            assert!(
                stderr(&out).contains(report),
                "tier {tier}: {}",
                stderr(&out)
            );
        }
    }
}

#[test]
fn host_calls_survive_kernel_bugs() {
    // 宿主调用 Machine 的方法时内核层 panic（测试钩子模拟）：只有这次调用失败，控制台上的
    // 调用还挂断那个控制台，Machine 之后照常可用
    use appine_kernel::errno::Errno;
    use appine_kernel::{ExitStatus, Machine, MachineOptions, RunError, Stdio, Tier};
    use std::os::unix::ffi::OsStrExt;
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(program) = build_c_guest("bugs") else {
        return;
    };
    // 内核层只在第一次撞上 bug 时读 APPINE_ON_BUG：在设置了它和 APPINE_TEST_HOOKS 的
    // 子进程里重新运行这个用例
    if std::env::var_os("APPINE_TEST_HOOKS").is_none() {
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "host_calls_survive_kernel_bugs", "--nocapture"])
            .env("APPINE_TEST_HOOKS", "1")
            .env("APPINE_ON_BUG", "kill")
            .output()
            .expect("failed to rerun the test");
        assert!(
            out.status.success() && stdout(&out).contains("1 passed"),
            "{}{}",
            stdout(&out),
            stderr(&out)
        );
        return;
    }
    let hello = build_guest("hello");
    let path = |p: &Path| p.as_os_str().as_bytes().to_vec();
    let argv = |mode: &str| [b"bugs".to_vec(), mode.as_bytes().to_vec()];
    let read_all = |machine: &Machine, id: u32| {
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        let end = loop {
            match machine.read_console(id, &mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) => break Err(e),
            }
        };
        (String::from_utf8(out).unwrap(), end)
    };
    for tier in [Tier::Interpreter, Tier::Dbt] {
        let machine = Machine::start(&MachineOptions {
            tier,
            rootfs: None,
            memory_limit: None,
        })
        .unwrap();
        // spawn 里出错：spawn 失败
        let pid = machine
            .spawn(
                &path(&program),
                &argv("host-call-then-exit"),
                &[],
                Stdio::Host,
            )
            .unwrap();
        assert_eq!(machine.wait(pid), ExitStatus::Exited(0), "{tier:?}");
        let r = machine.spawn(&path(&hello), &[b"hello".to_vec()], &[], Stdio::Host);
        assert!(matches!(r, Err(RunError::Internal)), "{tier:?}: {r:?}");
        // 读控制台时出错：控制台被挂断，会话组长收到 SIGHUP
        let id = machine.open_console(24, 80).unwrap();
        let pid = machine
            .spawn(
                &path(&program),
                &argv("host-call-then-read"),
                &[],
                Stdio::Console(id),
            )
            .unwrap();
        assert_eq!(read_all(&machine, id).1, Err(Errno::EIO), "{tier:?}");
        assert!(
            matches!(machine.wait(pid), ExitStatus::Signaled { signal, .. } if signal.number() == 1),
            "{tier:?}"
        );
        assert_eq!(read_all(&machine, id).1, Err(Errno::ENOENT), "{tier:?}");
        // 之后照常可用
        let id = machine.open_console(24, 80).unwrap();
        let pid = machine
            .spawn(&path(&hello), &[b"hello".to_vec()], &[], Stdio::Console(id))
            .unwrap();
        let (out, end) = read_all(&machine, id);
        assert_eq!(
            (out.as_str(), end),
            ("Hello from appine!\r\n", Ok(())),
            "{tier:?}"
        );
        assert_eq!(machine.wait(pid), ExitStatus::Exited(0), "{tier:?}");
    }
}

#[test]
fn memory_limit_and_thread_limits() {
    // RLIMIT_NPROC 的默认值和检查（root 不受它限制），超出 App 内存的上限时结束用得最多
    // 的 guest 进程
    let Some(program) = build_c_guest("limits") else {
        return;
    };
    for tier in TIERS {
        let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
            .args(["--tier", tier, "--memory-limit", "256M"])
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
        assert!(
            stderr(&out).contains("out of memory: killed process"),
            "tier {tier}: {}",
            stderr(&out)
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
