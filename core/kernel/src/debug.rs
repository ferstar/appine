//! 本地调试日志（设计文档 §7.5、§12）：设置环境变量 `APPINE_LOG=1` 后输出到宿主 stderr。

use std::fmt;
use std::sync::OnceLock;

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var_os("APPINE_LOG").is_some_and(|v| !v.is_empty() && v != "0"))
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
