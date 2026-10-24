//! appine-run：在宿主终端里直接运行一个 guest 程序（开发用前端，设计文档 §13.3）。
//!
//! guest 以 `argv[0] = <program>` 启动，环境变量为空。退出码沿用 shell 的约定：
//! 正常退出时是 guest 的退出码，被信号终止时是 128 + 信号编号，程序无法启动时
//! 是 127（找不到）或 126（不能执行）。

use std::env;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::ExitCode;

use appine_kernel::{DbtStats, ExitStatus, RunError, RunOptions, Tier};

const USAGE: &str = "usage: appine-run [--tier 0|1] [--stats] [--] <program> [args...]

  --tier 0   run in the interpreter (default)
  --tier 1   run in the dynamic binary translator (aarch64 Linux hosts only)
  --stats    print Tier 1 statistics to stderr on exit";

fn main() -> ExitCode {
    let mut args: Vec<OsString> = env::args_os().skip(1).collect();
    let mut options = RunOptions::default();
    let mut stats = false;
    while let Some(arg) = args.first().and_then(|a| a.to_str()).map(str::to_owned) {
        if !arg.starts_with('-') {
            break;
        }
        args.remove(0);
        match arg.as_str() {
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
            "--tier" | "--tier=0" | "--tier=1" => {
                let value = match arg.strip_prefix("--tier=") {
                    Some(v) => Some(v.to_owned()),
                    None if args.is_empty() => None,
                    None => args.remove(0).into_string().ok(),
                };
                options.tier = match value.as_deref() {
                    Some("0") => Tier::Interpreter,
                    Some("1") => Tier::Dbt,
                    _ => return usage_error(),
                };
            }
            _ => return usage_error(),
        }
    }
    let Some(program) = args.first().cloned() else {
        return usage_error();
    };

    let argv: Vec<Vec<u8>> = args.iter().map(|a| a.as_bytes().to_vec()).collect();
    let path = Path::new(&program);
    let report = match appine_kernel::run(path, &argv, &[], &options) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("appine-run: {}: {err}", path.display());
            return match err {
                RunError::Io(io) if io.kind() == ErrorKind::NotFound => ExitCode::from(127),
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
            match fault_addr {
                Some(addr) => eprintln!(
                    "appine-run: guest killed by {} (pc={pc:#x}, addr={addr:#x})",
                    signal.name()
                ),
                None => eprintln!("appine-run: guest killed by {} (pc={pc:#x})", signal.name()),
            }
            ExitCode::from((128 + signal.number()) as u8)
        }
    }
}

fn usage_error() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn print_stats(s: &DbtStats) {
    let pct = |n: u64| 100.0 * n as f64 / s.insns.max(1) as f64;
    eprintln!("appine-run: tier 1 statistics");
    eprintln!("  blocks translated   {:>12}", s.blocks);
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
}
