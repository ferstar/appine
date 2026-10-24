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

use appine_kernel::{ExitStatus, RunError};

const USAGE: &str = "usage: appine-run [--] <program> [args...]";

fn main() -> ExitCode {
    let mut args: Vec<OsString> = env::args_os().skip(1).collect();
    match args.first().and_then(|a| a.to_str()) {
        None => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
        Some("-h" | "--help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some("--version") => {
            println!("appine-run {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("--") => {
            args.remove(0);
        }
        _ => {}
    }
    let Some(program) = args.first().cloned() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };

    let argv: Vec<Vec<u8>> = args.iter().map(|a| a.as_bytes().to_vec()).collect();
    let path = Path::new(&program);
    match appine_kernel::run(path, &argv, &[]) {
        Ok(ExitStatus::Exited(code)) => ExitCode::from(code as u8),
        Ok(ExitStatus::Signaled {
            signal,
            pc,
            fault_addr,
        }) => {
            match fault_addr {
                Some(addr) => eprintln!(
                    "appine-run: guest killed by {} (pc={pc:#x}, addr={addr:#x})",
                    signal.name()
                ),
                None => eprintln!("appine-run: guest killed by {} (pc={pc:#x})", signal.name()),
            }
            ExitCode::from((128 + signal.number()) as u8)
        }
        Err(err) => {
            eprintln!("appine-run: {}: {err}", path.display());
            match err {
                RunError::Io(io) if io.kind() == ErrorKind::NotFound => ExitCode::from(127),
                _ => ExitCode::from(126),
            }
        }
    }
}
