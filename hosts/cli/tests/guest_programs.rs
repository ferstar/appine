//! End-to-end tests: tests/guest/*.S are assembled into static ELFs with the host `cc`, and
//! tests/guest/*.c are compiled into programs statically linked with musl by `musl-gcc`, then they
//! are run by appine-run. Each test case runs under Tier 0 and under Tier 1 separately, and the
//! results must be the same.
//!
//! Without musl-gcc (Debian/Ubuntu's musl-tools) the C programs are skipped; with
//! `APPINE_REQUIRE_MUSL=1` (as CI sets) that becomes a failure.
//!
//! Producing Linux aarch64 ELFs needs a Linux arm64 host (design doc §13.3); on other hosts the
//! whole file is not compiled.
#![cfg(all(target_os = "linux", target_arch = "aarch64"))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TIERS: [&str; 2] = ["0", "1"];

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
    // Mutexes, condition variables, atomics, thread-local storage, signals sent to a given thread,
    // the alternate signal stack, returning after fixing up a memory access fault, SA_RESTART, fork
    // in a multithreaded process, detached threads
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
fn thread_churn_keeps_the_host_heap_intact() {
    // 4 processes, each with 4 threads that keep creating and joining short-lived threads for 5
    // seconds: an exiting thread dropping an Rc weak reference outside the BKL corrupts the host
    // heap, and appine exits with SIGABRT (tests/guest/churn.c)
    let Some(program) = build_c_guest("churn") else {
        return;
    };
    for tier in TIERS {
        let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
            .args(["--tier", tier])
            .arg(&program)
            .arg("5")
            .output()
            .expect("failed to run appine-run");
        assert!(
            out.status.success() && stdout(&out).ends_with("all ok\n"),
            "tier {tier}: {:?} {}{}",
            out.status,
            stdout(&out),
            stderr(&out)
        );
    }
}

#[test]
fn sockets_polling_and_timers() {
    // AF_UNIX (socketpair, SCM_RIGHTS, SO_PEERCRED, abstract addresses), TCP/UDP loopback, netlink,
    // epoll/eventfd/select, setitimer; none of them needs outside network
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
    // A kernel-layer panic (on entering the kernel, while the BKL is released, in a no-BKL system
    // call, when a thread starts and exits, on the timer thread) or the engine making the host
    // fault (simulated with the test hooks of debug builds) ends only the guest process that hits
    // it, and everything else in the app keeps running
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
        // 0xa991 is the test hook's syscall number
        for report in [
            "kernel panic in syscall 43409",
            "kernel panic in syscall 113 without the BKL",
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
fn clock_jumps_wake_wall_clock_waiters() {
    // A jump of the host's wall clock (simulated with a test hook of debug builds): timerfds with
    // TFD_TIMER_CANCEL_ON_SET read ECANCELED, and waits on wall-clock deadlines look at them again
    // and still end on time
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(program) = build_c_guest("clockjump") else {
        return;
    };
    for tier in TIERS {
        let out = Command::new(env!("CARGO_BIN_EXE_appine-run"))
            .args(["--tier", tier])
            .env("APPINE_TEST_HOOKS", "1")
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
fn host_calls_survive_kernel_bugs() {
    // A kernel-layer panic while the host calls a Machine method (simulated by a test hook): only
    // that call fails, a call on a console also hangs up that console, and the Machine stays usable
    // afterwards
    use appine_kernel::errno::Errno;
    use appine_kernel::{ExitStatus, KillReason, Machine, MachineOptions, RunError, Stdio, Tier};
    use std::os::unix::ffi::OsStrExt;
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(program) = build_c_guest("bugs") else {
        return;
    };
    // The kernel layer reads APPINE_ON_BUG only when it first hits a bug: rerun this test case in
    // a child process that has it and APPINE_TEST_HOOKS set
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
            low_port_offset: None,
            always_map_low_ports: false,
        })
        .unwrap();
        // An error in spawn: spawn fails
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
        // An error while reading the console: the console is hung up, and the session leader
        // receives SIGHUP
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
        // Usable as usual afterwards
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
        // A process killed for hitting a bug leaves a report for the app (take_kills): it carries
        // the console's number, and once taken it is gone
        assert!(machine.take_kills().is_empty(), "{tier:?}");
        let id = machine.open_console(24, 80).unwrap();
        let pid = machine
            .spawn(
                &path(&program),
                &[b"bugs".to_vec()],
                &[],
                Stdio::Console(id),
            )
            .unwrap();
        let (out, _) = read_all(&machine, id);
        assert!(out.ends_with("all ok\r\n"), "{tier:?}: {out}");
        assert_eq!(machine.wait(pid), ExitStatus::Exited(0), "{tier:?}");
        let kills = machine.take_kills();
        assert!(kills.len() >= 6, "{tier:?}: {kills:?}");
        for k in &kills {
            assert_eq!(
                (k.comm.as_str(), k.console),
                ("guest-bugs", Some(id)),
                "{tier:?}: {k:?}"
            );
            assert!(
                matches!(&k.reason, KillReason::Bug { report, .. } if report.contains(&format!("pid {}", k.pid))),
                "{tier:?}: {k:?}"
            );
        }
        // The report holds the panic's message and location (bug::install_panic_hook), not just
        // "panic in a syscall"
        assert!(
            kills.iter().any(|k| matches!(&k.reason, KillReason::Bug { report, .. } if report.contains(
                "kernel panic in syscall 43409: panic injected by the test hook at core/kernel/src/bug.rs:"
            ))),
            "{tier:?}: {kills:?}"
        );
        // The report carries the kernel log, as `dmesg` writes it: the report itself last, older
        // records before it (here the report of the previous bug)
        let reports: Vec<(&str, &[String])> = kills
            .iter()
            .map(|k| match &k.reason {
                KillReason::Bug { report, log } => (report.as_str(), log.as_slice()),
                other => panic!("{tier:?}: {other:?}"),
            })
            .collect();
        for (i, (report, log)) in reports.iter().enumerate() {
            assert!(
                log.iter().all(|l| l.starts_with('[') && l.contains("] ")),
                "{tier:?}: {log:?}"
            );
            assert!(
                log.last()
                    .is_some_and(|l| l.ends_with(report.lines().last().unwrap())),
                "{tier:?}: {report} {log:?}"
            );
            if i > 0 {
                let before = reports[i - 1].0.lines().next().unwrap();
                assert!(
                    log.iter().any(|l| l.ends_with(before)),
                    "{tier:?}: {before} {log:?}"
                );
            }
        }
        assert!(machine.take_kills().is_empty(), "{tier:?}");
    }
}

#[test]
fn memory_limit_and_thread_limits() {
    // RLIMIT_NPROC's default value and checks (root is not limited by it), and over the app's
    // memory limit the guest process using the most is killed
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
fn main_stack_grows_up_to_rlimit_stack() {
    // Page faults, syscall buffers and signal frames all make the main thread's stack grow, its
    // size limited by the current value of RLIMIT_STACK; beyond that a page fault gets SIGSEGV and
    // a kernel access gets EFAULT
    let Some(program) = build_c_guest("stack") else {
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
    // System V message queues, semaphores (SEM_UNDO) and shared memory across fork, POSIX timers
    // (overrun counts, SIGEV_THREAD, CPU clocks), clocks (no-BKL and locked ones agree), mremap,
    // credential rules after dropping privileges, affinity and real-time policies, RLIMIT_NOFILE,
    // ptrace single-stepping, syscall stops and process_vm_readv
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
    // Rewriting code as a JIT does: the same page rewritten again and again, W^X mprotect, a dual
    // memfd mapping, rewriting while another thread is running it, and cache maintenance on
    // unmapped addresses
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

/// Runs a guest program once under each tier, returning (tier, output).
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

/// Builds tests/guest/<name>.c statically with musl-gcc; returns None without musl-gcc.
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

/// The path of a built guest program, and the temporary file written while building it: several
/// test cases may build the same program at once; each writes its own and then renames it into
/// place, so that none runs a file another one has half written.
fn output(name: &str) -> (PathBuf, PathBuf) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let tmp = dir.join(format!("guest-{name}.{}.{n}", std::process::id()));
    (dir.join(format!("guest-{name}")), tmp)
}

fn stdout(out: &Output) -> &str {
    std::str::from_utf8(&out.stdout).unwrap()
}

fn stderr(out: &Output) -> &str {
    std::str::from_utf8(&out.stderr).unwrap()
}
