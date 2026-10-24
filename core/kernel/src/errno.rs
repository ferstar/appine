//! Linux errno（guest ABI 的编号，与宿主无关）。

use std::fmt;

/// 一个 Linux errno 值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Errno(pub i32);

impl Errno {
    /// 不允许的操作。
    pub const EPERM: Errno = Errno(1);
    /// I/O 错误。
    pub const EIO: Errno = Errno(5);
    /// 可执行文件格式错误。
    pub const ENOEXEC: Errno = Errno(8);
    /// fd 无效。
    pub const EBADF: Errno = Errno(9);
    /// 内存不足。
    pub const ENOMEM: Errno = Errno(12);
    /// guest 地址无效。
    pub const EFAULT: Errno = Errno(14);
    /// 参数无效。
    pub const EINVAL: Errno = Errno(22);
    /// 未实现的 syscall。
    pub const ENOSYS: Errno = Errno(38);

    /// syscall 返回给 guest 的值：`-errno`。
    pub fn to_syscall_return(self) -> u64 {
        (-(self.0 as i64)) as u64
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "errno {}", self.0)
    }
}

impl std::error::Error for Errno {}
