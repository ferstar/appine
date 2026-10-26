//! 不持 BKL 的系统调用（设计文档 §6）。
//!
//! 并行的 lstat（`git status` 预读索引、构建工具、语言服务器的线程池）原来要排队等
//! BKL，线程越多越慢。这里只处理不改状态、要用的东西都能无锁拿到的常见情况：凭据不必
//! 检查搜索权限、路径从根目录或当前目录开始、整条路径都在仓库的元数据缓存里
//! （[`Cached`]）。做不了时什么也不改，交给持有 BKL 的 [`dispatch`](super::dispatch)，
//! 它顺带把要用的东西填进缓存。
//!
//! 线程在持有 BKL 时记下自己的根目录、当前目录和凭据（[`Fast`]）。它们可能被别的线程
//! 改掉（`CLONE_FS` 共享的 chdir、线程组共享的凭据），改的一方让
//! [`Shared::paths_changed`] 的计数加一，记下的就作废了。
//!
//! 访问 guest 内存时线程仍然标记着在执行 guest 代码（`TaskSync::in_guest`）：解除映射
//! 的线程要等它退出才释放宿主内存（[`Mm::sync`](crate::mm::Mm::sync)），与 guest 代码
//! 自己访存一样。

use appine_engine::{Cpu, PAGE_SIZE, prot};

use super::fs::{
    AT_FDCWD, AT_SYMLINK_NOFOLLOW, ROOT_DEV, encode_stat, encode_statx, newfstatat_flags,
    statx_flags,
};
use super::{PATH_MAX, SysResult};
use crate::errno::Errno;
use crate::kernel::Shared;
use crate::task::TaskState;
use crate::vfs::store::{Ino, Stat};
use crate::vfs::{Cached, Node};

/// 线程为不持 BKL 的系统调用记下的状态。
pub(crate) struct Fast {
    /// 记下时的 [`Shared::paths_epoch`]。
    epoch: u64,
    /// 不能不持 BKL 解析路径（凭据要检查搜索权限、根目录不在仓库里、在跟踪系统调用）
    /// 时是 None。
    paths: Option<Paths>,
}

struct Paths {
    vfs: Cached,
    root: Ino,
    /// 当前目录不在仓库里（在 `/proc` 里）时是 None。
    cwd: Option<Ino>,
}

impl Fast {
    /// 记下线程现在的状态。持有 BKL 时调用。
    pub(crate) fn new(shared: &Shared, t: &TaskState) -> Fast {
        let epoch = shared.paths_epoch();
        let fs = t.fs.borrow();
        let paths = match (&t.vfs, &fs.root) {
            (Some(vfs), &Node::Store(root))
                if t.group.borrow().cred.searches_all() && !crate::debug::tracing() =>
            {
                Some(Paths {
                    vfs: vfs.cached().clone(),
                    root,
                    cwd: match fs.cwd {
                        Node::Store(ino) => Some(ino),
                        Node::Pseudo(_) => None,
                    },
                })
            }
            _ => None,
        };
        Fast { epoch, paths }
    }

    /// 记下的过时了就重新记下。持有 BKL 时调用。
    pub(crate) fn refresh(&mut self, shared: &Shared, t: &TaskState) {
        if shared.paths_epoch() != self.epoch {
            *self = Fast::new(shared, t);
        }
    }

    /// 不持 BKL 执行 `cpu` 停在的系统调用。做完了（结果写进了 x0）返回 true；返回 false
    /// 时什么也没改，由调用方持有 BKL 正常处理。
    pub(crate) fn syscall(&self, shared: &Shared, cpu: &mut Cpu) -> bool {
        let Some(p) = &self.paths else {
            return false;
        };
        let s = cpu.state();
        // 还有没作废的 ic ivau 时要先进内核作废（task::handle）
        if s.ivau_page != 0 || shared.paths_epoch() != self.epoch {
            return false;
        }
        let a = [s.x[0], s.x[1], s.x[2], s.x[3], s.x[4]];
        let r = match s.x[8] {
            79 => p.newfstatat(cpu, a[0] as i32, a[1], a[2], a[3] as u32),
            291 => p.statx(cpu, a[0] as i32, a[1], a[2] as u32, a[3] as u32, a[4]),
            _ => None,
        };
        let Some(r) = r else {
            return false;
        };
        cpu.state_mut().x[0] = match r {
            Ok(v) => v,
            Err(e) => e.to_syscall_return(),
        };
        true
    }
}

impl Paths {
    fn newfstatat(
        &self,
        cpu: &Cpu,
        dirfd: i32,
        path: u64,
        buf: u64,
        flags: u32,
    ) -> Option<SysResult> {
        newfstatat_flags(flags).ok()?;
        let st = match self.stat(cpu, dirfd, path, flags)? {
            Ok(st) => st,
            Err(e) => return Some(Err(e)),
        };
        write(cpu, buf, &encode_stat(&st, ROOT_DEV))?;
        Some(Ok(0))
    }

    fn statx(
        &self,
        cpu: &Cpu,
        dirfd: i32,
        path: u64,
        flags: u32,
        mask: u32,
        buf: u64,
    ) -> Option<SysResult> {
        let flags = statx_flags(flags, mask).ok()?;
        let st = match self.stat(cpu, dirfd, path, flags)? {
            Ok(st) => st,
            Err(e) => return Some(Err(e)),
        };
        write(cpu, buf, &encode_statx(&encode_stat(&st, ROOT_DEV)))?;
        Some(Ok(0))
    }

    /// `fs::stat_at` 里路径不为空、从根目录或当前目录开始的情况。
    fn stat(&self, cpu: &Cpu, dirfd: i32, path: u64, flags: u32) -> Option<Result<Stat, Errno>> {
        let mut buf = [0u8; PATH_MAX];
        let path = read_path(cpu, path, &mut buf)?;
        let cwd = match path.first()? {
            b'/' => self.root,
            _ if dirfd == AT_FDCWD => self.cwd?,
            _ => return None,
        };
        self.vfs
            .stat(self.root, cwd, path, flags & AT_SYMLINK_NOFOLLOW == 0)
    }
}

/// 把 guest 内存里以 NUL 结尾的路径读进 `buf`（`syscall::read_path`）。读不了或者太长
/// 时返回 None。
fn read_path<'b>(cpu: &Cpu, addr: u64, buf: &'b mut [u8; PATH_MAX]) -> Option<&'b [u8]> {
    let mut done = 0;
    while done < buf.len() {
        let cur = addr.checked_add(done as u64)?;
        let chunk = ((PAGE_SIZE - cur % PAGE_SIZE) as usize).min(buf.len() - done);
        let host = cpu.translate(cur, prot::READ)?;
        // SAFETY: 线程标记着在执行 guest 代码，这一页的宿主内存在此期间不会被释放
        let bytes = unsafe { std::slice::from_raw_parts(host.as_ptr(), chunk) };
        if let Some(n) = bytes.iter().position(|&b| b == 0) {
            buf[done..done + n].copy_from_slice(&bytes[..n]);
            return Some(&buf[..done + n]);
        }
        buf[done..done + chunk].copy_from_slice(bytes);
        done += chunk;
    }
    None
}

/// 写 guest 内存，要求每页可写；写不了时返回 None（可能已经写了一部分，正常路径会
/// 重新写）。
fn write(cpu: &Cpu, addr: u64, data: &[u8]) -> Option<()> {
    let mut done = 0;
    while done < data.len() {
        let cur = addr.checked_add(done as u64)?;
        let chunk = ((PAGE_SIZE - cur % PAGE_SIZE) as usize).min(data.len() - done);
        let host = cpu.translate(cur, prot::WRITE)?;
        // SAFETY: 同 read_path
        unsafe { std::ptr::copy_nonoverlapping(data[done..].as_ptr(), host.as_ptr(), chunk) };
        done += chunk;
    }
    Some(())
}
