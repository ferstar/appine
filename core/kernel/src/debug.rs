//! 本地调试日志（设计文档 §7.5、§12）：设置环境变量 `APPINE_LOG=1` 后输出到宿主 stderr。
//! `APPINE_STRACE=1` 时另外打印每个返回的 syscall（编号、参数和结果），类似 strace。

use std::fmt;
use std::sync::OnceLock;

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var_os("APPINE_LOG").is_some_and(|v| !v.is_empty() && v != "0"))
}

fn tracing() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("APPINE_STRACE").is_some_and(|v| !v.is_empty() && v != "0")
    })
}

/// `APPINE_STRACE`：打印一个 syscall 和它的结果。
pub(crate) fn trace_syscall(
    tid: i32,
    nr: u64,
    args: &[u64; 6],
    result: &Result<u64, crate::errno::Errno>,
) {
    if tracing() {
        match result {
            Ok(v) => eprintln!("[{tid}] {nr}({:x?}) = {v:#x}", args),
            Err(e) => eprintln!("[{tid}] {nr}({:x?}) = -{}", args, e.0),
        }
    }
}

/// 记录一次未实现的 syscall，作为决定下一步补哪些 syscall 的依据。
pub(crate) fn unimplemented_syscall(nr: u64) {
    if enabled() {
        eprintln!("appine: unimplemented syscall {nr}");
    }
}

/// 记录一条调试日志。
pub(crate) fn log(args: fmt::Arguments<'_>) {
    if enabled() {
        eprintln!("appine: {args}");
    }
}
