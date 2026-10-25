//! 文件租约（`F_SETLEASE`、`F_GETLEASE`）。
//!
//! 租约挂在打开文件上，按 inode 登记在 [`Kernel::leases`](crate::kernel::Kernel)。别的打开
//! 文件要以冲突的方式打开或者截短这个 inode 时，租约被打破：持有者收到信号（SIGIO，
//! 或者 `F_SETSIG` 设置的信号），打开的一方等它降级或者放弃，最多等
//! `/proc/sys/fs/lease-break-time` 秒，到时强制降级。写方式打开与所有租约冲突（要降到
//! 没有），读方式打开只与写租约冲突（降到读租约）。

use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use crate::cred::cap;
use crate::errno::Errno;
use crate::fd::{FileKind, FileRef, OpenFile};
use crate::kernel::{Cx, Kernel, request_wake};
use crate::process::Signal;
use crate::signal::{InfoFields, SigInfo};
use crate::vfs::store::Ino;

pub(super) const F_RDLCK: i16 = 0;
pub(super) const F_WRLCK: i16 = 1;
pub(super) const F_UNLCK: i16 = 2;

/// 一个租约。
pub(crate) struct Lease {
    file: Weak<std::cell::RefCell<OpenFile>>,
    /// 设置租约时用的 fd：`F_SETSIG` 的信号把它放在 si_fd 里。
    fd: i32,
    /// `F_RDLCK` 或 `F_WRLCK`。
    ty: i16,
    /// 正在被打破：要降成的类型（`F_RDLCK` 或 `F_UNLCK`）和最后期限。
    breaking: Option<(i16, Instant)>,
}

impl Lease {
    fn is(&self, file: &FileRef) -> bool {
        self.file.as_ptr() == Rc::as_ptr(file)
    }
}

/// 普通文件的 inode。
fn ino_of(f: &OpenFile) -> Option<Ino> {
    match f.kind {
        FileKind::Regular { ino, .. } => Some(ino),
        _ => None,
    }
}

/// 除了 `except` 之外，`ino` 有多少个以读、以写方式打开的打开文件（不分进程）。
fn opens(k: &Kernel, ino: Ino, except: &FileRef) -> (usize, usize) {
    let mut files: Vec<FileRef> = Vec::new();
    for f in k.open_files() {
        if !Rc::ptr_eq(&f, except) && !files.iter().any(|g| Rc::ptr_eq(g, &f)) {
            files.push(f);
        }
    }
    let mine: Vec<_> = files
        .iter()
        .map(|f| f.borrow())
        .filter(|f| ino_of(f) == Some(ino))
        .map(|f| f.writable())
        .collect();
    let writers = mine.iter().filter(|&&w| w).count();
    (mine.len() - writers, writers)
}

/// 去掉已经关闭的打开文件的租约。
fn prune(k: &mut Kernel, ino: Ino) {
    if let Some(list) = k.leases.get_mut(&ino) {
        list.retain(|l| l.file.strong_count() > 0);
        if list.is_empty() {
            k.leases.remove(&ino);
        }
    }
}

/// fcntl(`F_SETLEASE`)。
pub(super) fn set(cx: &mut Cx<'_>, file: &FileRef, fd: i32, arg: i16) -> Result<(), Errno> {
    let (ino, writable) = {
        let f = file.borrow();
        (ino_of(&f).ok_or(Errno::EINVAL)?, f.writable())
    };
    if !matches!(arg, F_RDLCK | F_WRLCK | F_UNLCK) {
        return Err(Errno::EINVAL);
    }
    // 只有文件的属主（或者有 CAP_LEASE）能设置租约
    let st = cx.vfs()?.store().stat_open(
        ino,
        match &file.borrow().kind {
            FileKind::Regular { file, .. } => file,
            _ => unreachable!("checked above"),
        },
    )?;
    let cred = cx.cred();
    if cred.fsuid != st.uid && !cred.capable(cap::LEASE) {
        return Err(Errno::EACCES);
    }
    prune(cx.k(), ino);
    let k = cx.k();
    let list = k.leases.entry(ino).or_default();
    let mine = list.iter().position(|l| l.is(file));
    if arg == F_UNLCK {
        let i = mine.ok_or(Errno::EAGAIN)?;
        list.remove(i);
        prune(k, ino);
        request_wake();
        return Ok(());
    }
    let others = list.iter().filter(|l| !l.is(file)).count();
    let (readers, writers) = opens(cx.kr(), ino, file);
    // 读租约：自己不能是可写的，也不能有别人以写方式打开着；写租约：除了自己没有别的
    // 打开，也没有别的租约
    // 正在被写方式的打开打破时不能降成读租约：Linux 上打开的一方这时已经拿到了写
    // 权限，算作"有人以写方式打开着"
    let breaking_to_none = mine.is_some_and(|i| {
        cx.kr().leases[&ino][i]
            .breaking
            .is_some_and(|(t, _)| t == F_UNLCK)
    });
    let conflict = match arg {
        F_RDLCK => {
            writable
                || writers > 0
                || breaking_to_none
                || cx.kr().leases[&ino]
                    .iter()
                    .any(|l| !l.is(file) && l.ty == F_WRLCK)
        }
        _ => readers + writers > 0 || others > 0,
    };
    if conflict {
        return Err(Errno::EAGAIN);
    }
    let tgid = cx.tgid();
    let k = cx.k();
    let list = k.leases.get_mut(&ino).unwrap();
    match mine {
        // 改类型：降级（比如正在被读方式打开打破的写租约降成读租约）就算完成了打破
        Some(i) => {
            list[i].ty = arg;
            list[i].fd = fd;
            if list[i].breaking.is_some_and(|(target, _)| arg <= target) {
                list[i].breaking = None;
            }
            request_wake();
        }
        None => list.push(Lease {
            file: Rc::downgrade(file),
            fd,
            ty: arg,
            breaking: None,
        }),
    }
    // 与 Linux 一样，打破时的信号发给设置租约的进程
    file.borrow_mut().owner = (1, tgid);
    Ok(())
}

/// fcntl(`F_GETLEASE`)：正在被打破时是要降成的类型。
pub(super) fn get(cx: &mut Cx<'_>, file: &FileRef) -> i16 {
    let Some(ino) = ino_of(&file.borrow()) else {
        return F_UNLCK;
    };
    prune(cx.k(), ino);
    cx.kr()
        .leases
        .get(&ino)
        .and_then(|list| list.iter().find(|l| l.is(file)))
        .map_or(F_UNLCK, |l| l.breaking.map_or(l.ty, |(target, _)| target))
}

/// 要以写（`write`）或者读的方式打开或截短 `ino`：打破冲突的租约，等持有者让步。
/// `own` 是截短时自己的打开文件，它的租约不算冲突。`nonblock` 时只通知持有者，返回
/// EWOULDBLOCK（EAGAIN）。
pub(super) fn break_leases(
    cx: &mut Cx<'_>,
    ino: Ino,
    write: bool,
    nonblock: bool,
    own: Option<&FileRef>,
) -> Result<(), Errno> {
    if cx.kr().leases.is_empty() {
        return Ok(());
    }
    prune(cx.k(), ino);
    let target = if write { F_UNLCK } else { F_RDLCK };
    let conflicts = |k: &Kernel| {
        k.leases.get(&ino).is_some_and(|list| {
            list.iter()
                .any(|l| own.is_none_or(|f| !l.is(f)) && (write || l.ty == F_WRLCK))
        })
    };
    if !conflicts(cx.kr()) {
        return Ok(());
    }
    // 通知还没有在打破的持有者
    let secs = cx.kr().lease_break_time;
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut notify = Vec::new();
    if let Some(list) = cx.k().leases.get_mut(&ino) {
        for l in list.iter_mut() {
            if own.is_some_and(|f| l.is(f)) || !(write || l.ty == F_WRLCK) {
                continue;
            }
            match &mut l.breaking {
                Some((t, _)) => *t = (*t).max(target),
                None => {
                    l.breaking = Some((target, deadline));
                    notify.push((l.file.clone(), l.fd));
                }
            }
        }
    }
    for (file, fd) in notify {
        let Some(file) = file.upgrade() else {
            continue;
        };
        let ((ty, pid), sig) = {
            let f = file.borrow();
            (f.owner, f.sig)
        };
        let Some(signo) = Signal::new(if sig == 0 { 29 } else { sig }) else {
            continue;
        };
        // POLL_MSG，si_band 是 POLLIN | POLLRDNORM | POLLMSG
        let info = if sig == 0 {
            SigInfo::kernel()
        } else {
            SigInfo {
                code: 3,
                fields: InfoFields::Poll { band: 0x441, fd },
                esr: 0,
            }
        };
        super::poll::send_to_owner(cx.kr(), ty, pid, signo, info);
    }
    if nonblock {
        return Err(Errno::EAGAIN);
    }
    loop {
        prune(cx.k(), ino);
        if !conflicts(cx.kr()) {
            return Ok(());
        }
        // 到期的强制降级
        let now = Instant::now();
        let mut next = None::<Instant>;
        if let Some(list) = cx.k().leases.get_mut(&ino) {
            list.retain_mut(|l| match l.breaking {
                Some((t, d)) if d <= now => {
                    l.breaking = None;
                    if t == F_UNLCK {
                        return false;
                    }
                    l.ty = t;
                    true
                }
                Some((_, d)) => {
                    next = Some(next.map_or(d, |n| n.min(d)));
                    true
                }
                None => true,
            });
        }
        prune(cx.k(), ino);
        if !conflicts(cx.kr()) {
            return Ok(());
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait(next, &mut [])?;
    }
}
