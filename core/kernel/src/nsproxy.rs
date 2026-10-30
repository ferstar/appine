//! 线程所在的 UTS、IPC 命名空间（Linux 的 `struct nsproxy` 里的这两种；挂载命名空间在
//! `FsContext` 里，pid 命名空间见 [`crate::pidns`]）。与 Linux 一样按线程记：unshare、setns
//! 只改调用的线程，clone 出来的线程、进程沿用父线程的。
//!
//! UTS 命名空间有自己的主机名和 NIS 域名。IPC 命名空间有自己的 System V IPC 对象、POSIX
//! 消息队列和它们的限制，内容放在 [`crate::kernel::Kernel::ipcs`] 里按编号存，最后一个
//! 用它的线程退出之后丢掉。

use std::rc::Rc;

use crate::cell::RefCell;
use crate::cred::UserNs;

/// 开机时的 UTS 命名空间的编号（`/proc/<pid>/ns/uts`）。
pub(crate) const INIT_UTS_NS: u64 = 4_026_531_838;
/// 开机时的 IPC 命名空间的编号。
pub(crate) const INIT_IPC_NS: u64 = 4_026_531_839;

/// 一个 UTS 命名空间。
#[derive(Debug)]
pub(crate) struct UtsNs {
    pub id: u64,
    /// 建立它时所在的用户命名空间：在这里有 `CAP_SYS_ADMIN` 才能改名字、setns 进来。
    pub owner: Rc<UserNs>,
    /// 主机名和 NIS 域名（uname、sethostname、`/proc/sys/kernel/hostname`），最长
    /// [`crate::kernel::UTS_LEN`] 字节。
    pub hostname: RefCell<Vec<u8>>,
    pub domainname: RefCell<Vec<u8>>,
}

impl UtsNs {
    /// 复制一份名字，属于用户命名空间 `owner`（Linux 的 `copy_utsname`）。
    pub fn copy(&self, owner: Rc<UserNs>) -> Rc<UtsNs> {
        Rc::new(UtsNs {
            id: crate::vfs::new_ns_id(),
            owner,
            hostname: RefCell::new(self.hostname.borrow().clone()),
            domainname: RefCell::new(self.domainname.borrow().clone()),
        })
    }
}

/// 一个 IPC 命名空间。里面的对象在 [`crate::kernel::Kernel::ipcs`] 里。
#[derive(Debug)]
pub(crate) struct IpcNs {
    pub id: u64,
    /// 建立它时所在的用户命名空间（权限检查用）。
    pub owner: Rc<UserNs>,
}

/// 线程的 UTS、IPC 命名空间。
#[derive(Clone, Debug)]
pub(crate) struct NsProxy {
    pub uts: Rc<UtsNs>,
    pub ipc: Rc<IpcNs>,
}

impl NsProxy {
    /// 开机时的命名空间（每个内核一份，所有开机时的进程共用）。
    pub fn init() -> NsProxy {
        NsProxy {
            uts: Rc::new(UtsNs {
                id: INIT_UTS_NS,
                owner: UserNs::init(),
                hostname: RefCell::new(b"localhost".to_vec()),
                domainname: RefCell::new(b"(none)".to_vec()),
            }),
            ipc: Rc::new(IpcNs {
                id: INIT_IPC_NS,
                owner: UserNs::init(),
            }),
        }
    }
}
