//! PTY 的打开、读写和 ioctl，以及终端的作业控制（设计文档 §9）。
//!
//! 行规程在 [`crate::tty`] 里；这里负责阻塞和唤醒、会话与控制终端，以及后台进程组
//! 读写终端时的 SIGTTIN/SIGTTOU。

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use super::{SysResult, get, put};
use crate::errno::Errno;
use crate::fd::{FileKind, oflags};
use crate::kernel::{Cx, Kernel, request_wake};
use crate::process::Signal;
use crate::signal::{self, SigAction, SigInfo};
use crate::tty::{PtyEnd, Termios, Tty, Winsize, cc, flags};

const TCGETS: u32 = 0x5401;
const TCSETS: u32 = 0x5402;
const TCSETSW: u32 = 0x5403;
const TCSETSF: u32 = 0x5404;
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
const TIOCGPTN: u32 = 0x8004_5430;
const TIOCSPTLCK: u32 = 0x4004_5431;

/// 打开 `/dev/ptmx`：分配一对新的 PTY，返回主设备。
pub(crate) fn open_ptmx(k: &mut Kernel, owner: (u32, u32)) -> Result<FileKind, Errno> {
    k.ptys.retain(|_, w| w.strong_count() > 0);
    let index = (0..).find(|i| !k.ptys.contains_key(i)).unwrap();
    let tty = Rc::new(RefCell::new(Tty::new(index)));
    tty.borrow_mut().owner = owner;
    k.ptys.insert(index, Rc::downgrade(&tty));
    Ok(FileKind::Pty(PtyEnd::new(tty, true)))
}

/// 打开 `/dev/pts/<n>`。会话组长在没有控制终端时打开它（且没有 `O_NOCTTY`），它就成为
/// 控制终端，与 Linux 一样。
pub(crate) fn open_slave(cx: &mut Cx<'_>, index: u32, flags: u32) -> Result<FileKind, Errno> {
    let tty = cx
        .kr()
        .ptys
        .get(&index)
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
        }
    }
    Ok(FileKind::Pty(PtyEnd::new(tty, false)))
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

/// 从设备的读：规范模式下读一行；非规范模式按 VMIN/VTIME。
fn slave_read(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    check_background(cx, tty, Signal::TTIN)?;
    let mut deadline: Option<Instant> = None;
    loop {
        {
            let mut t = tty.borrow_mut();
            let canonical = t.termios.lflag & flags::ICANON != 0;
            let vmin = t.termios.cc[cc::VMIN] as usize;
            let vtime = Duration::from_millis(100 * t.termios.cc[cc::VTIME] as u64);
            let expired = deadline.is_some_and(|d| Instant::now() >= d);
            if canonical {
                if let Some(n) = t.read(buf) {
                    request_wake();
                    return Ok(n);
                }
            } else {
                let avail = t.raw_len();
                let enough = avail >= vmin.min(buf.len()).max(1);
                if avail > 0 && (enough || expired || vmin == 0) {
                    let n = t.read(buf).unwrap_or(0);
                    request_wake();
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
        cx.wait(deadline, &mut [])?;
    }
}

/// 从设备的写：经过输出处理，输出队列满时等主设备读走。
fn slave_write(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    nonblock: bool,
    data: &[u8],
) -> Result<usize, Errno> {
    if tty.borrow().termios.lflag & flags::TOSTOP != 0 {
        check_background(cx, tty, Signal::TTOU)?;
    }
    let mut done = 0;
    loop {
        {
            let mut t = tty.borrow_mut();
            if t.masters == 0 {
                return if done > 0 { Ok(done) } else { Err(Errno::EIO) };
            }
            let n = t.output_room().min(data.len() - done);
            if n > 0 {
                t.write_output(&data[done..done + n]);
                done += n;
                request_wake();
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
        cx.wait(None, &mut [])?;
    }
}

/// 主设备的读：取走输出。从设备打开过又全部关闭之后返回 EIO。
fn master_read(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    loop {
        {
            let mut t = tty.borrow_mut();
            let out = t.take_output(buf.len());
            if !out.is_empty() {
                buf[..out.len()].copy_from_slice(&out);
                request_wake();
                return Ok(out.len());
            }
            if t.slaves == 0 && !t.locked {
                return Err(Errno::EIO);
            }
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait(None, &mut [])?;
    }
}

/// 主设备的写：作为终端输入交给行规程，产生的信号发给前台进程组。
fn master_write(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    nonblock: bool,
    data: &[u8],
) -> Result<usize, Errno> {
    let mut done = 0;
    loop {
        let signals = {
            let mut t = tty.borrow_mut();
            let n = t.input_room().min(data.len() - done);
            let s = t.write_input(&data[done..done + n]);
            done += n;
            if n > 0 {
                request_wake();
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
        cx.wait(None, &mut [])?;
    }
}

/// PTY 的一次读。
pub(crate) fn read(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    master: bool,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    if master {
        master_read(cx, tty, nonblock, buf)
    } else {
        slave_read(cx, tty, nonblock, buf)
    }
}

/// PTY 的一次写。
pub(crate) fn write(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    master: bool,
    nonblock: bool,
    data: &[u8],
) -> Result<usize, Errno> {
    if master {
        master_write(cx, tty, nonblock, data)
    } else {
        slave_write(cx, tty, nonblock, data)
    }
}

/// ppoll 的就绪状态。
pub(crate) fn readiness(tty: &Tty, master: bool) -> i16 {
    const POLLIN: i16 = 0x001;
    const POLLOUT: i16 = 0x004;
    const POLLHUP: i16 = 0x010;
    const POLLRDNORM: i16 = 0x040;
    const POLLWRNORM: i16 = 0x100;
    let mut ev = 0;
    if master {
        if tty.has_output() {
            ev |= POLLIN | POLLRDNORM;
        }
        if tty.input_room() > 0 {
            ev |= POLLOUT | POLLWRNORM;
        }
        if tty.slaves == 0 && !tty.locked {
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

/// PTY 的 ioctl。
pub(crate) fn ioctl(
    cx: &mut Cx<'_>,
    tty: &Rc<RefCell<Tty>>,
    master: bool,
    request: u32,
    arg: u64,
) -> SysResult {
    match request {
        TCGETS => put(cx, arg, tty.borrow().termios.encode()).map(|()| 0),
        TCSETS | TCSETSW | TCSETSF => {
            if !master {
                check_background(cx, tty, Signal::TTOU)?;
            }
            let t = Termios::decode(&get(cx, arg)?);
            let mut tt = tty.borrow_mut();
            if request == TCSETSF {
                tt.flush_input();
            }
            tt.set_termios(t);
            request_wake();
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
            let mut t = tty.borrow_mut();
            match arg {
                0 => t.flush_input(),
                1 => t.flush_output(),
                2 => {
                    t.flush_input();
                    t.flush_output();
                }
                _ => return Err(Errno::EINVAL),
            }
            request_wake();
            Ok(0)
        }
        TCXONC => {
            match arg {
                0 => tty.borrow_mut().stopped = true,
                1 => tty.borrow_mut().stopped = false,
                2 | 3 => {}
                _ => return Err(Errno::EINVAL),
            }
            request_wake();
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
            request_wake();
            Ok(0)
        }
        TIOCGPTN if master => put(cx, arg, tty.borrow().index.to_le_bytes()).map(|()| 0),
        TIOCSPTLCK if master => {
            let lock = u32::from_le_bytes(get(cx, arg)?) != 0;
            tty.borrow_mut().locked = lock;
            Ok(0)
        }
        TIOCGPGRP => {
            if !is_ctty(cx, tty) {
                return Err(Errno::ENOTTY);
            }
            let fg = tty.borrow().fg.unwrap_or(0);
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
        TIOCGSID => {
            if !is_ctty(cx, tty) {
                return Err(Errno::ENOTTY);
            }
            let sid = tty.borrow().session.ok_or(Errno::ENOTTY)?;
            put(cx, arg, sid.to_le_bytes()).map(|()| 0)
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
            cx.t.group.borrow_mut().ctty = Some(tty.clone());
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
