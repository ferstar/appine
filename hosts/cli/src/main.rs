//! appine-run：在宿主终端里直接运行一个 guest 程序（开发用前端，设计文档 §13.3）。
//!
//! 不带 `--rootfs` 时，`<program>` 是宿主路径，guest 看不到任何文件系统；带上
//! `--rootfs` 时，`<program>` 是 inode 仓库里的 guest 路径，动态链接的程序经由
//! 仓库里的 `ld-musl` 加载。guest 以 `argv[0] = <program>` 启动，环境变量只有
//! `--env` 给出的那些。退出码沿用 shell 的约定：正常退出时是 guest 的退出码，
//! 被信号终止时是 128 + 信号编号，程序无法启动时是 127（找不到）或 126（不能执行）。
//!
//! stdin 和 stdout 都是终端时，guest 运行在一个进程内的 PTY 上（设计文档 §9）：宿主
//! 终端切到 raw 模式，按键原样交给 guest 的行规程，^C、^Z 由它变成发给前台进程组的
//! 信号，窗口大小跟随宿主终端。`--pty` 在没有终端时也这样做（stdin 和 stdout 可以是
//! 管道，测试用），`--no-pty` 让 guest 直接使用宿主的 stdin/stdout/stderr。

use std::env;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;

use appine_kernel::errno::Errno;
use appine_kernel::{DbtStats, ExitStatus, RunError, RunOptions, Terminal, Tier};

const USAGE: &str = "usage: appine-run [options] [--] <program> [args...]
       appine-run --rootfs <dir> --import <rootfs.tar.gz>

  --tier 0             run in the interpreter (default)
  --tier 1             run in the dynamic binary translator (aarch64 Linux hosts only)
  --rootfs <dir>       resolve <program> in the inode store at <dir>
  --import <tar.gz>    create the inode store at <dir> from a rootfs tarball, then exit
  --env NAME=VALUE     add a guest environment variable (repeatable)
  --pty                run on an in-process terminal even if stdin is not a tty
  --no-pty             give the guest the host stdin/stdout/stderr directly
  --memory-limit <n>   kill a guest process when appine's anonymous memory exceeds n
                       bytes (K, M, G suffixes; \"none\" for no limit; default: half the RAM)
  --stats              print Tier 1 statistics to stderr on exit";

fn main() -> ExitCode {
    let mut args: Vec<OsString> = env::args_os().skip(1).collect();
    let mut options = RunOptions::default();
    let mut stats = false;
    let mut import = None;
    let mut pty = None;
    let mut envp: Vec<Vec<u8>> = Vec::new();
    while let Some(arg) = args.first().and_then(|a| a.to_str()).map(str::to_owned) {
        if !arg.starts_with('-') {
            break;
        }
        args.remove(0);
        // 带值的选项既可以写成 `--opt value`，也可以写成 `--opt=value`
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_owned(), Some(OsString::from(v))),
            _ => (arg.clone(), None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| (!args.is_empty()).then(|| args.remove(0)))
        };
        match name.as_str() {
            "--" => break,
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--version" => {
                println!("appine-run {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "--stats" => stats = true,
            "--pty" => pty = Some(true),
            "--no-pty" => pty = Some(false),
            "--tier" => {
                options.tier = match value().as_ref().and_then(|v| v.to_str()) {
                    Some("0") => Tier::Interpreter,
                    Some("1") => Tier::Dbt,
                    _ => return usage_error(),
                };
            }
            "--rootfs" => match value() {
                Some(v) => options.rootfs = Some(PathBuf::from(v)),
                None => return usage_error(),
            },
            "--import" => match value() {
                Some(v) => import = Some(PathBuf::from(v)),
                None => return usage_error(),
            },
            "--env" => match value() {
                Some(v) if v.as_bytes().contains(&b'=') => envp.push(v.as_bytes().to_vec()),
                _ => return usage_error(),
            },
            "--memory-limit" => match value().as_ref().and_then(|v| v.to_str()).map(parse_size) {
                Some(Some(n)) => options.memory_limit = Some(n),
                _ => return usage_error(),
            },
            _ => return usage_error(),
        }
    }

    if let Some(tarball) = import {
        let Some(store) = &options.rootfs else {
            return usage_error();
        };
        return match appine_kernel::import_rootfs(store, &tarball) {
            Ok(r) => {
                eprintln!(
                    "appine-run: imported {} files, {} directories, {} symlinks, {} hard links, {} device nodes",
                    r.files, r.dirs, r.symlinks, r.hardlinks, r.nodes
                );
                ExitCode::SUCCESS
            }
            Err(err) => {
                let reason = if err == Errno::EEXIST {
                    "the inode store already exists".to_owned()
                } else {
                    err.to_string()
                };
                eprintln!("appine-run: cannot import {}: {reason}", tarball.display());
                ExitCode::from(1)
            }
        };
    }
    let Some(program) = args.first().cloned() else {
        return usage_error();
    };

    let argv: Vec<Vec<u8>> = args.iter().map(|a| a.as_bytes().to_vec()).collect();
    let on_tty = appine_hal::isatty(0) && appine_hal::isatty(1);
    let mut raw = None;
    if pty.unwrap_or(on_tty) {
        if on_tty {
            match appine_hal::RawMode::enter(0) {
                Ok(r) => raw = Some(r),
                Err(e) => {
                    eprintln!("appine-run: cannot switch the terminal to raw mode: errno {e}")
                }
            }
        }
        options.terminal = Some(Terminal {
            input: 0,
            output: 1,
            size: on_tty.then_some(1),
            resize: on_tty
                .then(appine_hal::sigwinch_notifier)
                .and_then(Result::ok),
        });
    }
    let result = appine_kernel::run(&program, &argv, &envp, &options);
    // 先恢复宿主终端，再打印消息
    drop(raw);
    let report = match result {
        Ok(report) => report,
        Err(err) => {
            eprintln!("appine-run: {}: {err}", program.to_string_lossy());
            return match err {
                RunError::Io(io) if io.kind() == ErrorKind::NotFound => ExitCode::from(127),
                RunError::Exec(errno) if errno == Errno::ENOENT => ExitCode::from(127),
                RunError::Rootfs(_) => ExitCode::from(1),
                _ => ExitCode::from(126),
            };
        }
    };
    if let (true, Some(s)) = (stats, report.dbt_stats) {
        print_stats(&s);
    }
    match report.status {
        ExitStatus::Exited(code) => ExitCode::from(code as u8),
        ExitStatus::Signaled {
            signal,
            pc,
            fault_addr,
        } => {
            match (pc, fault_addr) {
                (Some(pc), Some(addr)) => {
                    eprintln!("appine-run: guest killed by {signal} (pc={pc:#x}, addr={addr:#x})")
                }
                (Some(pc), None) => eprintln!("appine-run: guest killed by {signal} (pc={pc:#x})"),
                _ => eprintln!("appine-run: guest killed by {signal}"),
            }
            ExitCode::from((128 + signal.number()) as u8)
        }
    }
}

/// `--memory-limit` 的值：字节数，可以带 K、M、G 后缀；`none` 表示不限制。
fn parse_size(s: &str) -> Option<u64> {
    if s == "none" {
        return Some(u64::MAX);
    }
    let (digits, shift) = match s.as_bytes().last()? {
        b'K' | b'k' => (&s[..s.len() - 1], 10),
        b'M' | b'm' => (&s[..s.len() - 1], 20),
        b'G' | b'g' => (&s[..s.len() - 1], 30),
        _ => (s, 0),
    };
    digits.parse::<u64>().ok()?.checked_mul(1 << shift)
}

fn usage_error() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn print_stats(s: &DbtStats) {
    let pct = |n: u64| 100.0 * n as f64 / s.insns.max(1) as f64;
    eprintln!("appine-run: tier 1 statistics");
    eprintln!("  blocks translated   {:>12}", s.blocks);
    eprintln!("    from templates    {:>12}", s.blocks_shared);
    eprintln!("  guest instructions  {:>12}", s.insns);
    eprintln!(
        "    copied            {:>12}  ({:.1}%)",
        s.insns_copied,
        pct(s.insns_copied)
    );
    eprintln!(
        "    remapped          {:>12}  ({:.1}%)",
        s.insns_remapped,
        pct(s.insns_remapped)
    );
    eprintln!(
        "    memory (inline)   {:>12}  ({:.1}%)",
        s.insns_memory,
        pct(s.insns_memory)
    );
    eprintln!(
        "    tier 0 fallback   {:>12}  ({:.1}%)",
        s.insns_fallback,
        pct(s.insns_fallback)
    );
    eprintln!(
        "  exits: chain {}, lookup {}, interp {}, svc {}, kick {}",
        s.exits_chain, s.exits_lookup, s.exits_interp, s.exits_svc, s.exits_kick
    );
    eprintln!("  code cache flushes  {:>12}", s.flushes);
    eprintln!("  blocks invalidated  {:>12}", s.invalidated);
}
