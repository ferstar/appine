//! PTY 的打开、读写和 ioctl，以及终端的作业控制（kernel/tty 的 map）。
//!
//! 行规程在 [`crate::tty`] 里；这里负责阻塞和唤醒、会话与控制终端，以及后台进程组
//! 读写终端时的 SIGTTIN/SIGTTOU。

use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use super::{SysResult, get, put};
use crate::cell::RefCell;
use crate::cred::cap;
use crate::errno::Errno;
use crate::fd::{FileKind, oflags};
use crate::kernel::{Cx, Kernel, Tid, request_wake_object, wake};
use crate::process::Signal;
use crate::signal::{self, SigAction, SigInfo};
use crate::tty::{PtyEnd, Termios, Tty, Winsize, cc, flags, pkt};

const TCGETS: u32 = 0x5401;
const TCSETS: u32 = 0x5402;
const TCSETSW: u32 = 0x5403;
const TCSETSF: u32 = 0x5404;
/// 老式的 `struct termio`：标志只有低 16 位，控制字符只有 8 个。
const TCGETA: u32 = 0x5405;
const TCSETA: u32 = 0x5406;
const TCSETAW: u32 = 0x5407;
const TCSETAF: u32 = 0x5408;
const TCSBRK: u32 = 0x5409;
const TCXONC: u32 = 0x540a;
const TCFLSH: u32 = 0x540b;
const TIOCEXCL: u32 = 0x540c;
const TIOCNXCL: u32 = 0x540d;
const TIOCSCTTY: u32 = 0x540e;
const TIOCGPGRP: u32 = 0x540f;
const TIOCSPGRP: u32 = 0x5410;
const TIOCOUTQ: u32 = 0x5411;
const TIOCSTI: u32 = 0x5412;
const TIOCGWINSZ: u32 = 0x5413;
const TIOCSWINSZ: u32 = 0x5414;
const FIONREAD: u32 = 0x541b;
const TIOCNOTTY: u32 = 0x5422;
const TIOCSETD: u32 = 0x5423;
const TIOCGETD: u32 = 0x5424;
const TCSBRKP: u32 = 0x5425;
const TIOCGSID: u32 = 0x5429;
const TIOCVHANGUP: u32 = 0x5437;
/// `struct termios2`：多了输入、输出速率（glibc 2.42 起 tcgetattr、tcsetattr 只用这几个）。
const TCGETS2: u32 = 0x802c_542a;
const TCSETS2: u32 = 0x402c_542b;
const TCSETSW2: u32 = 0x402c_542c;
const TCSETSF2: u32 = 0x402c_542d;
/// 主设备的包模式：设置、查询（[`crate::tty::pkt`]）。
const TIOCPKT: u32 = 0x5420;
const TIOCGPKT: u32 = 0x8004_5438;
const TIOCGPTN: u32 = 0x8004_5430;
const TIOCSPTLCK: u32 = 0x4004_5431;
const TIOCGPTLCK: u32 = 0x8004_5439;
/// 经由主设备给从设备的前台进程组发信号（只能是 SIGINT、SIGQUIT、SIGTSTP）。
const TIOCSIG: u32 = 0x4004_5436;

/// 在 devpts 实例 `fs`（设备号）里分配一对新的 PTY（Linux 的 `devpts_new_index`、
/// `devpts_pty_new`）：编号是实例里最小的空着的，到了实例的 `max` 是 ENOSPC；从设备的属主、
/// 属组、权限位按实例的选项，没给 uid、gid 时是调用者的 fsuid、fsgid。
pub(crate) fn new_pty(
    k: &mut Kernel,
    fs: u64,
    (fsuid, fsgid): (u32, u32),
) -> Result<Rc<RefCell<Tty>>, Errno> {
    k.ptys.retain(|_, w| w.strong_count() > 0);
    let opts = k
        .vfs
        .as_ref()
        .map_or(crate::tty::PtsOptions::BOOT, |v| v.devpts_options(fs));
    let used = k.ptys.range((fs, 0)..=(fs, u32::MAX)).count();
    if used >= opts.max as usize {
        return Err(Errno::ENOSPC);
    }
    let index = (0..).find(|i| !k.ptys.contains_key(&(fs, *i))).unwrap();
    let tty = Rc::new(RefCell::new(Tty::new(index)));
    {
        let mut t = tty.borrow_mut();
        t.fs = fs;
        t.owner = (opts.uid.unwrap_or(fsuid), opts.gid.unwrap_or(fsgid));
        t.mode = opts.mode;
    }
    k.ptys.insert((fs, index), Rc::downgrade(&tty));
    Ok(tty)
}

/// 打开 ptmx：在 devpts 实例 `fs`（经由挂载 `mnt` 找到的）里分配一对新的 PTY，返回主设备。
/// `pin` 是经由那个挂载里的 ptmx 打开的。
pub(crate) fn open_ptmx(
    k: &mut Kernel,
    (fs, mnt, pin): (u64, crate::vfs::MountId, bool),
    owner: (u32, u32),
) -> Result<FileKind, Errno> {
    let tty = new_pty(k, fs, owner)?;
    Ok(FileKind::Pty(PtyEnd::new(tty, true).via(mnt, pin)))
}

/// 打开 `/dev/pts/<n>`。会话组长在没有控制终端时打开它（且没有 `O_NOCTTY`），它就成为
/// 控制终端，与 Linux 一样。
pub(crate) fn open_slave(
    cx: &mut Cx<'_>,
    (fs, index, mnt, pin): (u64, u32, crate::vfs::MountId, bool),
    flags: u32,
) -> Result<FileKind, Errno> {
    let tty = cx
        .kr()
        .ptys
        .get(&(fs, index))
        .and_then(Weak::upgrade)
        .ok_or(Errno::ENOENT)?;
    {
        let t = tty.borrow();
        if t.locked || t.masters == 0 {
            return Err(Errno::EIO);
        }
    }
    if flags & oflags::O_NOCTTY == 0 {
        let mut g = cx.t.group.borrow_mut();
        if g.sid == g.tgid && g.ctty.is_none() && tty.borrow().session.is_none() {
            let mut t = tty.borrow_mut();
            t.session = Some(g.sid);
            t.fg = Some(g.pgid);
            g.ctty = Some(tty.clone());
            g.tty_old_pgrp = None;
        }
    }
    Ok(FileKind::Pty(PtyEnd::new(tty, false).via(mnt, pin)))
}

/// 按主设备打开从设备（`TIOCGPTPEER`）。
pub(crate) const TIOCGPTPEER: u32 = 0x5441;

/// TIOCGPTPEER（Linux 4.13 起的 `ptm_open_peer`）：按主设备打开对应的从设备，经由主设备
/// 找到的那个 devpts 挂载，不经过路径、不查权限；`flags` 是 open 的标志，O_CLOEXEC 给新的
/// fd。在从设备上是 EIO。
pub(crate) fn open_peer(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    (master, born): (bool, u32),
    mnt: crate::vfs::MountId,
    flags: u32,
) -> SysResult {
    if !master || tty.borrow().hung(master, born) {
        return Err(Errno::EIO);
    }
    let (fs, index) = {
        let t = tty.borrow();
        (t.fs, t.index)
    };
    let kind = open_slave(cx, (fs, index, mnt, true), flags)?;
    let keep = oflags::O_ACCMODE | oflags::O_APPEND | oflags::O_NONBLOCK;
    // 路径是那个 devpts 挂载里的 <编号>（fd 的链接、ttyname 用它）
    let vfs = cx.vfs()?;
    let node = crate::vfs::Node::Pseudo(mnt, crate::pseudo::PseudoNode::Pts(fs, index));
    let path = vfs.path_of(&cx.view(&vfs), &cx.root(), &node).ok();
    let file = crate::fd::OpenFile::with_path(
        kind,
        flags & keep | oflags::O_LARGEFILE,
        path.map(Rc::from),
    );
    Ok(cx
        .fds_mut()
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)? as u64)
}

/// 打开 `/dev/tty`：调用者的控制终端。
pub(crate) fn open_ctty(cx: &mut Cx<'_>) -> Result<FileKind, Errno> {
    let tty = cx.t.group.borrow().ctty.clone().ok_or(Errno::ENXIO)?;
    Ok(FileKind::Pty(PtyEnd::new(tty, false)))
}

/// 后台进程组访问控制终端（`sig` 是 SIGTTIN 或 SIGTTOU）：按 Linux 的 `tty_check_change`，
/// 给自己的进程组发信号并让 syscall 重启；信号被忽略或屏蔽时，读返回 EIO，写和修改设置照常进行。
fn check_background(cx: &mut Cx<'_>, tty: &Rc<RefCell<Tty>>, sig: Signal) -> Result<(), Errno> {
    let (pgid, is_ctty) = {
        let g = cx.t.group.borrow();
        (g.pgid, g.ctty.as_ref().is_some_and(|c| Rc::ptr_eq(c, tty)))
    };
    let fg = tty.borrow().fg;
    if !is_ctty || fg.is_none_or(|fg| fg == pgid) {
        return Ok(());
    }
    let ignored = cx.t.group.borrow().sighand.borrow().get(sig).handler == SigAction::IGNORE;
    let blocked = cx.task.sigmask.get() & sig.bit() != 0;
    if ignored || blocked {
        return if sig == Signal::TTIN {
            Err(Errno::EIO)
        } else {
            Ok(())
        };
    }
    if signal::pgrp_orphaned(cx.kr(), pgid) {
        return Err(Errno::EIO);
    }
    signal::send_to_pgrp(cx.kr(), pgid, sig, SigInfo::kernel());
    Err(Errno::ERESTARTSYS)
}

/// 给终端的前台进程组发信号（行规程产生的 SIGINT 等、窗口大小改变）。
pub(crate) fn signal_fg(k: &Kernel, tty: &Rc<RefCell<Tty>>, sig: Signal) {
    if let Some(fg) = tty.borrow().fg {
        signal::send_to_pgrp(k, fg, sig, SigInfo::kernel());
    }
}

/// 从设备的读：规范模式下读一行；非规范模式按 VMIN/VTIME。`born` 见 [`Tty::hung`]。
fn slave_read(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    born: u32,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    if tty.borrow().hung(false, born) {
        return Ok(0);
    }
    check_background(cx, tty, Signal::TTIN)?;
    let mut deadline: Option<Instant> = None;
    loop {
        {
            let mut t = tty.borrow_mut();
            if t.hung(false, born) {
                return Ok(0);
            }
            let canonical = t.termios.lflag & flags::ICANON != 0;
            let vmin = t.termios.cc[cc::VMIN] as usize;
            let vtime = Duration::from_millis(100 * t.termios.cc[cc::VTIME] as u64);
            let expired = deadline.is_some_and(|d| Instant::now() >= d);
            if canonical {
                if let Some(n) = t.read(buf) {
                    request_wake_object(wake::of(tty));
                    return Ok(n);
                }
            } else {
                let avail = t.raw_len();
                let enough = avail >= vmin.min(buf.len()).max(1);
                if avail > 0 && (enough || expired || vmin == 0) {
                    let n = t.read(buf).unwrap_or(0);
                    request_wake_object(wake::of(tty));
                    return Ok(n);
                }
                if vmin == 0 && (vtime.is_zero() || expired) {
                    return Ok(0);
                }
                // VTIME：VMIN 为 0 时从开始读算起，否则从收到第一个字节算起
                if !vtime.is_zero() && deadline.is_none() && (vmin == 0 || avail > 0) {
                    deadline = Some(Instant::now() + vtime);
                }
            }
            if t.masters == 0 {
                return Err(Errno::EIO);
            }
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait_objects(deadline, &mut [], wake::FILE, &[wake::of(tty)])?;
    }
}

/// 从设备的写：经过输出处理，输出队列满时等主设备读走。
fn slave_write(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    born: u32,
    nonblock: bool,
    data: &[u8],
) -> Result<usize, Errno> {
    if tty.borrow().hung(false, born) {
        return Err(Errno::EIO);
    }
    if tty.borrow().termios.lflag & flags::TOSTOP != 0 {
        check_background(cx, tty, Signal::TTOU)?;
    }
    let mut done = 0;
    loop {
        {
            let mut t = tty.borrow_mut();
            if t.masters == 0 || t.hung(false, born) {
                return if done > 0 { Ok(done) } else { Err(Errno::EIO) };
            }
            let n = t.output_room().min(data.len() - done);
            if n > 0 {
                t.write_output(&data[done..done + n]);
                done += n;
                request_wake_object(wake::of(tty));
            }
        }
        if done == data.len() {
            return Ok(done);
        }
        if nonblock || cx.interrupted() {
            let e = if nonblock {
                Errno::EAGAIN
            } else {
                Errno::ERESTARTSYS
            };
            return if done > 0 { Ok(done) } else { Err(e) };
        }
        cx.wait_objects(None, &mut [], wake::FILE, &[wake::of(tty)])?;
    }
}

/// 主设备的读：取走输出。从设备打开过又全部关闭之后返回 EIO，主设备这一边被 TIOCVHANGUP
/// 挂断了是 0。包模式下（Linux 的 `n_tty_read`）有状态时只读到那一个状态字节，否则数据
/// 前面加一个 `TIOCPKT_DATA`（也占读的长度）。
fn master_read(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    born: u32,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    loop {
        {
            let mut t = tty.borrow_mut();
            if t.hung(true, born) {
                return Ok(0);
            }
            if buf.is_empty() {
                return Ok(0);
            }
            if let Some(status) = t.take_pktstatus() {
                buf[0] = status;
                return Ok(1);
            }
            let head = t.packet as usize;
            if t.has_output() {
                let out = t.take_output(buf.len() - head);
                if head == 1 {
                    buf[0] = pkt::DATA;
                }
                buf[head..head + out.len()].copy_from_slice(&out);
                request_wake_object(wake::of(tty));
                return Ok(head + out.len());
            }
            if t.slave_closed {
                return Err(Errno::EIO);
            }
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait_objects(None, &mut [], wake::FILE, &[wake::of(tty)])?;
    }
}

/// 主设备的写：作为终端输入交给行规程，产生的信号发给前台进程组。
fn master_write(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    born: u32,
    nonblock: bool,
    data: &[u8],
) -> Result<usize, Errno> {
    let mut done = 0;
    loop {
        let signals = {
            let mut t = tty.borrow_mut();
            if t.hung(true, born) {
                return if done > 0 { Ok(done) } else { Err(Errno::EIO) };
            }
            let n = t.input_room().min(data.len() - done);
            let s = t.write_input(&data[done..done + n]);
            done += n;
            if n > 0 {
                request_wake_object(wake::of(tty));
            }
            s
        };
        for sig in signals {
            signal_fg(cx.kr(), tty, sig);
        }
        if done == data.len() {
            return Ok(done);
        }
        if nonblock || cx.interrupted() {
            let e = if nonblock {
                Errno::EAGAIN
            } else {
                Errno::ERESTARTSYS
            };
            return if done > 0 { Ok(done) } else { Err(e) };
        }
        cx.wait_objects(None, &mut [], wake::FILE, &[wake::of(tty)])?;
    }
}

/// PTY 的一次读。`born` 是打开文件的 [`PtyEnd::born`]。
pub(crate) fn read(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    (master, born): (bool, u32),
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    if master {
        master_read(cx, tty, born, nonblock, buf)
    } else {
        let n = slave_read(cx, tty, born, nonblock, buf)?;
        if n > 0 {
            tty.borrow_mut().touch(false);
        }
        Ok(n)
    }
}

/// PTY 的一次写。`born` 是打开文件的 [`PtyEnd::born`]。
pub(crate) fn write(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    (master, born): (bool, u32),
    nonblock: bool,
    data: &[u8],
) -> Result<usize, Errno> {
    if master {
        master_write(cx, tty, born, nonblock, data)
    } else {
        let n = slave_write(cx, tty, born, nonblock, data)?;
        if n > 0 {
            tty.borrow_mut().touch(true);
        }
        Ok(n)
    }
}

/// 挂断终端（Linux 的 `__tty_hangup`）：会话组长收到 SIGHUP 和 SIGCONT，并且记下前台
/// 进程组，等它退出时再发给它们（`process::release_ctty`）；会话里以它为控制终端的进程都
/// 失去控制终端，打开着的从设备从此读到 EOF、写入失败（[`Tty::hung_up`]）。
/// `/dev/pts/<n>` 随之消失，编号可以给新的终端（Linux 的 `devpts_pty_kill`）。最后一个
/// 主设备关掉（Linux 的 `pty_close`，[`hang_up_closed`]）和关闭控制台
/// （[`crate::terminal::close`]）时调用；已经挂断的什么也不做。
pub(crate) fn hang_up(k: &mut Kernel, tty: &Rc<RefCell<Tty>>) {
    let (key, session, fg) = {
        let mut t = tty.borrow_mut();
        if t.hung_up {
            return;
        }
        t.hung_up = true;
        ((t.fs, t.index), t.session.take(), t.fg.take())
    };
    if k.ptys
        .get(&key)
        .is_some_and(|w| std::ptr::eq(w.as_ptr(), Rc::as_ptr(tty)))
    {
        k.ptys.remove(&key);
    }
    detach(k, tty, session, fg);
}

/// 挂断终端的一边（Linux 的 `tty_vhangup`，vhangup(2)、TIOCVHANGUP）：这一边打开着的文件
/// 都挂断（[`Tty::hung`]），没读的输入丢掉（行规程的缓冲区随之清空）。从设备这一边还与
/// [`hang_up`] 一样给会话组长发信号、让会话失去控制终端；与关掉主设备不同，
/// `/dev/pts/<n>` 还在，重新打开就照常能用（login、agetty 靠这个赶走终端上留下的进程）。
/// 主设备那边没有会话，只挂断主设备的文件、丢掉从设备还没被读走的输出。挂断之后还没有
/// 重新打开过的什么也不做（Linux 的 `TTY_HUPPED`）。
pub(crate) fn vhangup(k: &mut Kernel, tty: &Rc<RefCell<Tty>>, master: bool) {
    let side = master as usize;
    let session = {
        let mut t = tty.borrow_mut();
        if t.hupped[side] || (!master && t.hung_up) {
            return;
        }
        t.hupped[side] = true;
        t.vhangups[side] = t.vhangups[side].wrapping_add(1);
        if master {
            t.flush_output();
            None
        } else {
            // Linux 的 tty_ldisc_hangup 清空从设备的输入，之后 __tty_hangup 丢掉包模式的状态
            t.flush_input();
            t.pktstatus = 0;
            Some((t.session.take(), t.fg.take()))
        }
    };
    match session {
        Some((session, fg)) => detach(k, tty, session, fg),
        None => request_wake_object(wake::of(tty)),
    }
}

/// 挂断时的会话部分（Linux 的 `tty_signal_session_leader`）：以它为控制终端的进程都失去
/// 控制终端，会话组长收到 SIGHUP 和 SIGCONT，并且记下前台进程组 `fg`，等它退出时再发给
/// 它们（`process::release_ctty`）。叫醒等这个终端的线程。
fn detach(k: &mut Kernel, tty: &Rc<RefCell<Tty>>, session: Option<Tid>, fg: Option<Tid>) {
    for g in k.groups.values() {
        let mut g = g.borrow_mut();
        if g.ctty.as_ref().is_some_and(|c| Rc::ptr_eq(c, tty)) {
            g.ctty = None;
        }
    }
    if let Some(leader) = session {
        // 组长退出时没有控制终端了，给这时的前台进程组发 SIGHUP（Linux 的 tty_old_pgrp）
        if let Some(g) = k.groups.get(&leader) {
            g.borrow_mut().tty_old_pgrp = fg;
        }
        signal::send_to_group(k, leader, Signal::HUP, SigInfo::kernel());
        signal::send_to_group(k, leader, Signal::CONT, SigInfo::kernel());
    }
    request_wake_object(wake::of(tty));
}

/// vhangup(2)：挂断调用者的控制终端（[`vhangup`]），没有控制终端时什么也不做。要
/// `CAP_SYS_TTY_CONFIG`（Linux 的 capable()，只看开机时的用户命名空间）。调用者是会话组长
/// 时自己也收到 SIGHUP、SIGCONT（login 先忽略 SIGHUP）。
pub(super) fn vhangup_ctty(cx: &mut Cx<'_>) -> SysResult {
    if !cx.cred().capable(cap::SYS_TTY_CONFIG) {
        return Err(Errno::EPERM);
    }
    let ctty = cx.t.group.borrow().ctty.clone();
    if let Some(tty) = ctty {
        vhangup(cx.k(), &tty, false);
    }
    Ok(0)
}

/// 挂断这个线程关掉了最后一个主设备的终端（[`crate::tty::take_closed`]）。放开 BKL 之前
/// 调用（[`crate::kernel::Bkl`]）。
pub(crate) fn hang_up_closed(k: &mut Kernel) {
    for tty in crate::tty::take_closed() {
        hang_up(k, &tty);
    }
}

/// ppoll 的就绪状态。`born` 是打开文件的 [`PtyEnd::born`]。
pub(crate) fn readiness(tty: &Tty, master: bool, born: u32) -> i16 {
    const POLLIN: i16 = 0x001;
    const POLLOUT: i16 = 0x004;
    const POLLHUP: i16 = 0x010;
    const POLLRDNORM: i16 = 0x040;
    const POLLWRNORM: i16 = 0x100;
    let mut ev = 0;
    if tty.hung(master, born) {
        const POLLERR: i16 = 0x008;
        ev = POLLIN | POLLOUT | POLLERR | POLLHUP | POLLRDNORM | POLLWRNORM;
    } else if master {
        if tty.has_output() {
            ev |= POLLIN | POLLRDNORM;
        }
        if tty.packet && tty.pktstatus != 0 {
            const POLLPRI: i16 = 0x002;
            ev |= POLLPRI | POLLIN | POLLRDNORM;
        }
        if tty.input_room() > 0 {
            ev |= POLLOUT | POLLWRNORM;
        }
        if tty.slave_closed {
            ev |= POLLHUP;
        }
    } else {
        if tty.readable() {
            ev |= POLLIN | POLLRDNORM;
        }
        if tty.output_room() > 0 {
            ev |= POLLOUT | POLLWRNORM;
        }
        if tty.masters == 0 {
            ev |= POLLHUP | POLLIN;
        }
    }
    ev
}

/// 这个终端是不是调用者的控制终端。
fn is_ctty(cx: &Cx<'_>, tty: &Rc<RefCell<Tty>>) -> bool {
    cx.t.group
        .borrow()
        .ctty
        .as_ref()
        .is_some_and(|c| Rc::ptr_eq(c, tty))
}

/// PTY 的 ioctl。`born` 是打开文件的 [`PtyEnd::born`]。
pub(crate) fn ioctl(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    (master, born): (bool, u32),
    request: u32,
    arg: u64,
) -> SysResult {
    if tty.borrow().hung(master, born) {
        return Err(if request == TIOCSPGRP {
            Errno::ENOTTY
        } else {
            Errno::EIO
        });
    }
    match request {
        TCGETS => {
            let t = tty.borrow().termios.encode();
            put(cx, arg, <[u8; 36]>::try_from(&t[..36]).unwrap()).map(|()| 0)
        }
        TCGETS2 => put(cx, arg, tty.borrow().termios.encode()).map(|()| 0),
        TCGETA => {
            let t = tty.borrow().termios.encode();
            let mut b = [0u8; 18];
            for i in 0..4 {
                b[i * 2..i * 2 + 2].copy_from_slice(&t[i * 4..i * 4 + 2]);
            }
            b[8..17].copy_from_slice(&t[16..25]);
            put(cx, arg, b).map(|()| 0)
        }
        TCSETS | TCSETSW | TCSETSF | TCSETA | TCSETAW | TCSETAF | TCSETS2 | TCSETSW2 | TCSETSF2 => {
            if !master {
                check_background(cx, tty, Signal::TTOU)?;
            }
            // 在原来的设置上换掉给出的部分（Linux 的 set_termios）：termio 只换标志的低
            // 16 位和前 8 个控制字符，termios 不换速率字段
            let mut t = tty.borrow().termios.encode();
            match request {
                TCSETA | TCSETAW | TCSETAF => {
                    let b: [u8; 18] = get(cx, arg)?;
                    for i in 0..4 {
                        t[i * 4..i * 4 + 2].copy_from_slice(&b[i * 2..i * 2 + 2]);
                    }
                    t[16..25].copy_from_slice(&b[8..17]);
                }
                TCSETS | TCSETSW | TCSETSF => {
                    t[..36].copy_from_slice(&get::<36>(cx, arg)?);
                }
                _ => t = get(cx, arg)?,
            }
            let mut t = Termios::decode(&t);
            t.load_speeds();
            let mut tt = tty.borrow_mut();
            if matches!(request, TCSETSF | TCSETAF | TCSETSF2) {
                // 两边都是清空从设备的输入（Linux 在 real_tty 上 set_termios）
                tt.flush_input();
                tt.report_flush(true, false);
            }
            tt.set_termios(t);
            request_wake_object(wake::of(tty));
            Ok(0)
        }
        TIOCGWINSZ => put(cx, arg, tty.borrow().winsize.encode()).map(|()| 0),
        TIOCSWINSZ => {
            let w = Winsize::decode(&get(cx, arg)?);
            let changed = std::mem::replace(&mut tty.borrow_mut().winsize, w) != w;
            if changed {
                signal_fg(cx.kr(), tty, Signal::WINCH);
            }
            Ok(0)
        }
        TCFLSH => {
            let (input, output) = match arg {
                0 => (true, false),
                1 => (false, true),
                2 => (true, true),
                _ => return Err(Errno::EINVAL),
            };
            // 主设备和从设备是一对 tty，主设备的输入就是从设备的输出。主设备的 TCOFLUSH 在
            // Linux 上只清掉还没交给从设备行规程的数据（pty_flush_buffer），这里写进来就
            // 交给了，没有可清的
            let mut t = tty.borrow_mut();
            if master {
                if input {
                    t.flush_output();
                }
            } else {
                if input {
                    t.flush_input();
                }
                if output {
                    t.flush_output();
                }
                t.report_flush(input, output);
            }
            request_wake_object(wake::of(tty));
            Ok(0)
        }
        TCXONC => {
            let mut signals = Vec::new();
            {
                let mut t = tty.borrow_mut();
                match (arg, master) {
                    (0, false) => t.stop_output(),
                    (1, false) => t.start_output(),
                    (0, true) => t.master_stopped = true,
                    (1, true) => t.master_stopped = false,
                    // TCIOFF、TCION：发 STOP、START 字符（禁用的不发）。主设备发的是从设备的输入
                    (2 | 3, _) => {
                        let c = t.termios.cc[if arg == 2 { cc::VSTOP } else { cc::VSTART }];
                        if c != 0 && master {
                            signals = t.write_input(&[c]);
                        } else if c != 0 {
                            t.send_xchar(c);
                        }
                    }
                    _ => return Err(Errno::EINVAL),
                }
            }
            for sig in signals {
                signal_fg(cx.kr(), tty, sig);
            }
            request_wake_object(wake::of(tty));
            Ok(0)
        }
        TCSBRK | TCSBRKP | TIOCEXCL | TIOCNXCL | TIOCSETD => Ok(0),
        TIOCGETD => put(cx, arg, 0i32.to_le_bytes()).map(|()| 0),
        FIONREAD => {
            let t = tty.borrow();
            let n = if master {
                t.output_len()
            } else {
                t.input_len()
            };
            put(cx, arg, (n as i32).to_le_bytes()).map(|()| 0)
        }
        TIOCOUTQ => {
            let n = tty.borrow().output_len() as i32;
            put(cx, arg, n.to_le_bytes()).map(|()| 0)
        }
        TIOCSTI => {
            let c: [u8; 1] = get(cx, arg)?;
            let signals = tty.borrow_mut().write_input(&c);
            for sig in signals {
                signal_fg(cx.kr(), tty, sig);
            }
            request_wake_object(wake::of(tty));
            Ok(0)
        }
        TIOCGPTN if master => put(cx, arg, tty.borrow().index.to_le_bytes()).map(|()| 0),
        TIOCSPTLCK if master => {
            let lock = u32::from_le_bytes(get(cx, arg)?) != 0;
            tty.borrow_mut().locked = lock;
            Ok(0)
        }
        TIOCGPTLCK if master => {
            put(cx, arg, (tty.borrow().locked as i32).to_le_bytes()).map(|()| 0)
        }
        // 打开包模式时丢掉以前的状态（Linux 的 pty_set_pktmode）
        TIOCPKT if master => {
            let on = i32::from_le_bytes(get(cx, arg)?) != 0;
            let mut t = tty.borrow_mut();
            if on && !t.packet {
                t.pktstatus = 0;
            }
            t.packet = on;
            Ok(0)
        }
        TIOCGPKT if master => put(cx, arg, (tty.borrow().packet as i32).to_le_bytes()).map(|()| 0),
        // Linux 的 pty_signal：发给从设备的前台进程组，与按键产生的一样是内核发的
        TIOCSIG if master => {
            let sig = match arg {
                2 => Signal::INT,
                3 => Signal::QUIT,
                20 => Signal::TSTP,
                _ => return Err(Errno::EINVAL),
            };
            signal_fg(cx.kr(), tty, sig);
            Ok(0)
        }
        TIOCGPGRP => {
            if !is_ctty(cx, tty) {
                return Err(Errno::ENOTTY);
            }
            let fg = tty.borrow().fg.map_or(0, |p| cx.pid_out(p));
            put(cx, arg, fg.to_le_bytes()).map(|()| 0)
        }
        TIOCSPGRP => {
            if !is_ctty(cx, tty) {
                return Err(Errno::ENOTTY);
            }
            check_background(cx, tty, Signal::TTOU)?;
            let pgid = i32::from_le_bytes(get(cx, arg)?);
            if pgid < 0 {
                return Err(Errno::EINVAL);
            }
            let pgid = cx.pid_in(pgid);
            let sid = cx.t.group.borrow().sid;
            let exists = cx
                .kr()
                .groups
                .values()
                .any(|g| g.borrow().pgid == pgid && g.borrow().sid == sid);
            if !exists {
                return Err(Errno::EPERM);
            }
            tty.borrow_mut().fg = Some(pgid);
            Ok(0)
        }
        // Linux 的 tty_ioctl 用 capable()：只看开机时的用户命名空间
        TIOCVHANGUP => {
            if !cx.cred().capable(cap::SYS_ADMIN) {
                return Err(Errno::EPERM);
            }
            vhangup(cx.k(), tty, master);
            Ok(0)
        }
        TIOCGSID => {
            if !is_ctty(cx, tty) {
                return Err(Errno::ENOTTY);
            }
            let sid = tty.borrow().session.ok_or(Errno::ENOTTY)?;
            put(cx, arg, cx.pid_out(sid).to_le_bytes()).map(|()| 0)
        }
        TIOCSCTTY => {
            let (sid, pgid, tgid, has_ctty) = {
                let g = cx.t.group.borrow();
                (g.sid, g.pgid, g.tgid, g.ctty.is_some())
            };
            if is_ctty(cx, tty) {
                return Ok(0);
            }
            if sid != tgid || has_ctty {
                return Err(Errno::EPERM);
            }
            // 已经是别的会话的控制终端：只有 arg 为 1 时（root）才能抢过来
            if let Some(other) = tty.borrow().session {
                if arg != 1 {
                    return Err(Errno::EPERM);
                }
                for g in cx.kr().groups.values() {
                    let mut g = g.borrow_mut();
                    if g.sid == other {
                        g.ctty = None;
                    }
                }
            }
            {
                let mut t = tty.borrow_mut();
                t.session = Some(sid);
                t.fg = Some(pgid);
            }
            let mut g = cx.t.group.borrow_mut();
            g.ctty = Some(tty.clone());
            g.tty_old_pgrp = None;
            Ok(0)
        }
        TIOCNOTTY => {
            if !is_ctty(cx, tty) {
                return Err(Errno::ENOTTY);
            }
            let (sid, tgid) = {
                let g = cx.t.group.borrow();
                (g.sid, g.tgid)
            };
            if sid == tgid {
                // 会话组长放弃控制终端：整个会话都失去它，前台进程组收到 SIGHUP
                let fg = {
                    let mut t = tty.borrow_mut();
                    t.session = None;
                    t.fg.take()
                };
                for g in cx.kr().groups.values() {
                    let mut g = g.borrow_mut();
                    if g.sid == sid {
                        g.ctty = None;
                    }
                }
                if let Some(fg) = fg {
                    signal::send_to_pgrp(cx.kr(), fg, Signal::HUP, SigInfo::kernel());
                    signal::send_to_pgrp(cx.kr(), fg, Signal::CONT, SigInfo::kernel());
                }
            } else {
                cx.t.group.borrow_mut().ctty = None;
            }
            Ok(0)
        }
        _ => Err(Errno::ENOTTY),
    }
}
