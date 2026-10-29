//! Linux errno（guest ABI 的编号，与宿主无关）。

use std::fmt;

/// 一个 Linux errno 值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Errno(pub i32);

impl Errno {
    /// 不允许的操作。
    pub const EPERM: Errno = Errno(1);
    /// 文件不存在。
    pub const ENOENT: Errno = Errno(2);
    /// 进程不存在。
    pub const ESRCH: Errno = Errno(3);
    /// 被信号打断。
    pub const EINTR: Errno = Errno(4);
    /// I/O 错误。
    pub const EIO: Errno = Errno(5);
    /// 设备或地址不存在。
    pub const ENXIO: Errno = Errno(6);
    /// 参数列表太长。
    pub const E2BIG: Errno = Errno(7);
    /// 可执行文件格式错误。
    pub const ENOEXEC: Errno = Errno(8);
    /// fd 无效。
    pub const EBADF: Errno = Errno(9);
    /// 没有子进程。
    pub const ECHILD: Errno = Errno(10);
    /// 暂时不可用。
    pub const EAGAIN: Errno = Errno(11);
    /// 内存不足。
    pub const ENOMEM: Errno = Errno(12);
    /// 权限不足。
    pub const EACCES: Errno = Errno(13);
    /// guest 地址无效。
    pub const EFAULT: Errno = Errno(14);
    /// 设备或资源忙。
    pub const EBUSY: Errno = Errno(16);
    /// 文件已存在。
    pub const EEXIST: Errno = Errno(17);
    /// 跨文件系统。
    pub const EXDEV: Errno = Errno(18);
    /// 设备不支持该操作。
    pub const ENODEV: Errno = Errno(19);
    /// 不是目录。
    pub const ENOTDIR: Errno = Errno(20);
    /// 是目录。
    pub const EISDIR: Errno = Errno(21);
    /// 参数无效。
    pub const EINVAL: Errno = Errno(22);
    /// fd 用尽。
    pub const EMFILE: Errno = Errno(24);
    /// 不是终端。
    pub const ENOTTY: Errno = Errno(25);
    /// 设备没有空间。
    pub const ENOSPC: Errno = Errno(28);
    /// 不能 seek。
    pub const ESPIPE: Errno = Errno(29);
    /// 只读文件系统。
    pub const EROFS: Errno = Errno(30);
    /// 管道的读端已全部关闭。
    pub const EPIPE: Errno = Errno(32);
    /// 结果超出范围。
    pub const ERANGE: Errno = Errno(34);
    /// 会死锁。
    pub const EDEADLK: Errno = Errno(35);
    /// 名字太长。
    pub const ENAMETOOLONG: Errno = Errno(36);
    /// 正在执行的程序文件不能写，以写方式打开着的文件不能执行。
    pub const ETXTBSY: Errno = Errno(26);
    /// 文件太大（semop 的信号量编号超出范围）。
    pub const EFBIG: Errno = Errno(27);
    /// 没有符合条件的消息（msgrcv）。
    pub const ENOMSG: Errno = Errno(42);
    /// IPC 对象已被删除。
    pub const EIDRM: Errno = Errno(43);
    /// 文件句柄已经失效。
    pub const ESTALE: Errno = Errno(116);
    /// 没有这个属性（xattr）。
    pub const ENODATA: Errno = Errno(61);
    /// 未实现的 syscall。
    pub const ENOSYS: Errno = Errno(38);
    /// 目录非空。
    pub const ENOTEMPTY: Errno = Errno(39);
    /// 符号链接层数太多。
    pub const ELOOP: Errno = Errno(40);
    /// 值溢出。
    pub const EOVERFLOW: Errno = Errno(75);
    /// 不是 socket。
    pub const ENOTSOCK: Errno = Errno(88);
    /// 消息太长。
    pub const EMSGSIZE: Errno = Errno(90);
    /// socket 类型与协议不符。
    pub const EPROTOTYPE: Errno = Errno(91);
    /// 不支持的 socket 选项。
    pub const ENOPROTOOPT: Errno = Errno(92);
    /// 不支持的协议。
    pub const EPROTONOSUPPORT: Errno = Errno(93);
    /// 不支持的 socket 类型。
    pub const ESOCKTNOSUPPORT: Errno = Errno(94);
    /// 不支持的操作。
    pub const EOPNOTSUPP: Errno = Errno(95);
    /// 不支持的地址族。
    pub const EAFNOSUPPORT: Errno = Errno(97);
    /// 地址已被占用。
    pub const EADDRINUSE: Errno = Errno(98);
    /// 网络不可达。
    pub const ENETUNREACH: Errno = Errno(101);
    /// 已经连接。
    pub const EISCONN: Errno = Errno(106);
    /// 没有连接。
    pub const ENOTCONN: Errno = Errno(107);
    /// 超时。
    pub const ETIMEDOUT: Errno = Errno(110);
    /// 连接被拒绝。
    pub const ECONNREFUSED: Errno = Errno(111);
    /// 主机不可达。
    pub const EHOSTUNREACH: Errno = Errno(113);
    /// 连接正在进行。
    pub const EINPROGRESS: Errno = Errno(115);

    // 以下是内核内部使用的 syscall 重启码（设计文档 §7.3），返回用户态之前一定会被
    // 换成重新执行 syscall 或者 EINTR，guest 永远看不到它们。

    /// 被信号打断：处理函数带 `SA_RESTART` 时重新执行，否则返回 EINTR。
    pub const ERESTARTSYS: Errno = Errno(512);
    /// 被信号打断：总是重新执行。
    pub const ERESTARTNOINTR: Errno = Errno(513);
    /// 被信号打断：有处理函数时返回 EINTR，否则重新执行。
    pub const ERESTARTNOHAND: Errno = Errno(514);
    /// 被信号打断：有处理函数时返回 EINTR，否则改成执行 `restart_syscall`，接着做剩下的
    /// 部分（线程的 [`crate::task::TaskState::restart`]）。
    pub const ERESTART_RESTARTBLOCK: Errno = Errno(516);

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
