//! 宿主一侧的控制台：宿主持有的进程内 PTY 主设备（kernel/tty 的 map）。
//!
//! [`crate::Machine`] 为每个终端会话开一个控制台，会话组长以它的从设备为 stdin/stdout/
//! stderr 和控制终端。宿主写入的字节经过行规程（^C 变成发给前台进程组的 SIGINT），
//! guest 的输出由宿主取走。宿主线程不是 guest 线程，但和它们一样只在持有 BKL 时访问
//! 内核对象，等待也走同一套机制：guest 写终端时请求唤醒，等输出的宿主线程就会醒来。
//!
//! `appine-run` 用 [`bridge`] 把宿主终端接到控制台上。

use std::rc::Rc;
use std::sync::Arc;

use appine_hal::Event;

use crate::cell::RefCell;
use crate::errno::Errno;
use crate::fd::{FileKind, FileRef, OpenFile, oflags};
use crate::kernel::{Kernel, Shared, Tid, block_for, request_wake, request_wake_object, wake};
use crate::process::Signal;
use crate::signal::{self, SigInfo};
use crate::tty::{PtyEnd, Tty, Winsize};

/// 接到宿主上的终端（[`RunOptions::terminal`](crate::RunOptions::terminal)）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Terminal {
    /// 读取键盘输入的宿主 fd。
    pub input: i32,
    /// 写出终端输出的宿主 fd。
    pub output: i32,
    /// 跟随它的窗口大小的宿主终端（TIOCGWINSZ）；没有时窗口大小是 24×80。
    pub size: Option<i32>,
    /// 窗口大小改变时变为可读的宿主 fd（比如 SIGWINCH 处理函数写的管道）。
    pub resize: Option<i32>,
}

/// 一个控制台。
pub(crate) struct Console {
    /// PTY 的主设备。
    pub master: PtyEnd,
    /// 已经有进程以它为控制终端：此后所有从设备都关闭就是会话结束。
    pub attached: bool,
}

const POLLIN: i16 = 1;
/// 一次从宿主读多少输入。
const READ_CHUNK: usize = 4096;

/// 宿主终端的窗口大小。
fn host_winsize(fd: i32) -> Option<Winsize> {
    const TIOCGWINSZ: u64 = 0x5413;
    let mut b = [0u8; 8];
    appine_hal::ioctl(fd, TIOCGWINSZ, &mut b).ok()?;
    let w = Winsize::decode(&b);
    (w.rows > 0 && w.cols > 0).then_some(w)
}

/// 新建一个控制台，返回 PTY 编号（`/dev/pts/<编号>`）。
pub(crate) fn open(k: &mut Kernel, size: Winsize) -> u32 {
    let fs = crate::tty::BOOT_PTS;
    let index = (0..).find(|i| !k.ptys.contains_key(&(fs, *i))).unwrap();
    let tty = Rc::new(RefCell::new(Tty::new(index)));
    {
        let mut t = tty.borrow_mut();
        t.locked = false;
        t.winsize = size;
    }
    k.ptys.insert((fs, index), Rc::downgrade(&tty));
    k.consoles.insert(
        index,
        Console {
            master: PtyEnd::new(tty, true),
            attached: false,
        },
    );
    index
}

/// 让 `leader` 成为控制台的会话组长：返回从设备的打开文件和 PTY。控制台已经属于
/// 一个会话时返回 EBUSY。
pub(crate) fn attach(
    k: &mut Kernel,
    id: u32,
    leader: Tid,
) -> Result<(FileRef, Rc<RefCell<Tty>>), Errno> {
    let console = k.consoles.get_mut(&id).ok_or(Errno::ENOENT)?;
    let tty = console.master.tty.clone();
    {
        let mut t = tty.borrow_mut();
        if t.session.is_some() {
            return Err(Errno::EBUSY);
        }
        t.session = Some(leader);
        t.fg = Some(leader);
    }
    console.attached = true;
    let path: Rc<[u8]> = Rc::from(format!("/dev/pts/{id}").into_bytes());
    let slave = OpenFile::with_path(
        FileKind::Pty(PtyEnd::new(tty.clone(), false)),
        oflags::O_RDWR | oflags::O_LARGEFILE,
        Some(path),
    );
    Ok((slave, tty))
}

fn tty_of(k: &Kernel, id: u32) -> Result<Rc<RefCell<Tty>>, Errno> {
    k.consoles
        .get(&id)
        .map(|c| c.master.tty.clone())
        .ok_or(Errno::ENOENT)
}

/// 等控制台 `id` 的输出、输入队列的空间时登记的唤醒对象：它的 PTY。关闭控制台请求
/// 整个通道的唤醒。控制台不存在时是 None。
pub(crate) fn wake_objects(k: &Kernel, id: u32) -> Option<Vec<wake::Object>> {
    tty_of(k, id).ok().map(|t| vec![wake::of(&t)])
}

/// 取走最多 `max` 字节的输出。没有输出时返回 None；会话已经结束（所有从设备都关闭了）
/// 并且输出已经取完时返回空的 Vec。
pub(crate) fn take_output(k: &Kernel, id: u32, max: usize) -> Result<Option<Vec<u8>>, Errno> {
    let tty = tty_of(k, id)?;
    let out = tty.borrow_mut().take_output(max);
    if !out.is_empty() {
        // 输出队列空出来了，等着写终端的线程可以继续
        request_wake_object(wake::of(&tty));
        return Ok(Some(out));
    }
    let ended = k.consoles[&id].attached && tty.borrow().slaves == 0;
    Ok(ended.then(Vec::new))
}

/// 输入队列还能放下多少字节。
pub(crate) fn input_room(k: &Kernel, id: u32) -> Result<usize, Errno> {
    Ok(tty_of(k, id)?.borrow().input_room())
}

/// 宿主的输入交给行规程，产生的信号发给前台进程组。
pub(crate) fn feed(k: &Kernel, id: u32, data: &[u8]) -> Result<(), Errno> {
    let tty = tty_of(k, id)?;
    let signals = tty.borrow_mut().write_input(data);
    request_wake_object(wake::of(&tty));
    let fg = tty.borrow().fg;
    if let Some(fg) = fg {
        for sig in signals {
            signal::send_to_pgrp(k, fg, sig, SigInfo::kernel());
        }
    }
    Ok(())
}

/// 控制台前台进程组的当前目录（App 被杀之后恢复标签页用，设计文档 §11.2）：组长的，组长
/// 已经退出时取组里的任何一个进程；路径按那个进程的根目录算（与它的 `/proc/<pid>/cwd` 一样）。
/// 没有前台进程组（shell 已经退出）是 ESRCH。
pub(crate) fn fg_cwd(k: &Kernel, id: u32) -> Result<Vec<u8>, Errno> {
    let fg = tty_of(k, id)?.borrow().fg.ok_or(Errno::ESRCH)?;
    let leader = k.groups.get(&fg).filter(|g| g.borrow().pgid == fg);
    let group = leader
        .or_else(|| k.groups.values().find(|g| g.borrow().pgid == fg))
        .ok_or(Errno::ESRCH)?;
    let (tgid, tid) = {
        let g = group.borrow();
        (g.tgid, *g.threads.first().ok_or(Errno::ESRCH)?)
    };
    let task = k.tasks.get(&tid).ok_or(Errno::ESRCH)?;
    let (root, cwd) = {
        let res = task.res.borrow();
        let fs = res.fs.borrow();
        (fs.root.clone(), fs.cwd.clone())
    };
    let vfs = k.vfs.as_ref().ok_or(Errno::ENOENT)?;
    let view = crate::pseudo::View { k, vfs, tid, tgid };
    vfs.path_of(&view, &root, &cwd)
}

/// 窗口大小变了：更新 PTY，给前台进程组发 SIGWINCH。
pub(crate) fn resize(k: &Kernel, id: u32, w: Winsize) -> Result<(), Errno> {
    let tty = tty_of(k, id)?;
    let (changed, fg) = {
        let mut t = tty.borrow_mut();
        (std::mem::replace(&mut t.winsize, w) != w, t.fg)
    };
    if let (true, Some(fg)) = (changed, fg) {
        signal::send_to_pgrp(k, fg, Signal::WINCH, SigInfo::kernel());
    }
    Ok(())
}

/// 关闭控制台：与 Linux 挂断终端时一样，给会话组长发 SIGHUP 和 SIGCONT。从设备之后
/// 读到 EOF、写入失败。
pub(crate) fn close(k: &mut Kernel, id: u32) -> Result<(), Errno> {
    let console = k.consoles.remove(&id).ok_or(Errno::ENOENT)?;
    let tty = console.master.tty.clone();
    drop(console);
    crate::syscall::hang_up(k, &tty);
    request_wake();
    Ok(())
}

/// 宿主的输入结束之后交给程序的 EOF（[`send_eof`]）。
#[derive(Clone, Copy, PartialEq)]
enum Eof {
    /// 输入还没结束。
    Open,
    /// 输入结束了，等程序读完已经送进去的输入。
    Pending,
    /// 在规范模式下送了：VEOF 是行尾的 EOF 标记。
    Canonical,
    Sent,
}

/// 宿主的输入结束之后，与 util-linux 的 script 一样往控制台的输入里送一个 VEOF（^D）：
/// 规范模式下行首的 VEOF 让 read 返回 0，shell、cat 这类读到 EOF 就结束的程序随之退出；
/// 不是规范模式时程序收到这个字符（shell 的行编辑当作 EOF）。与 script 一样等程序把已经
/// 送进去的输入读完再送；还没读完时返回 None，程序读走输入时控制台的 PTY 叫醒桥接线程。
/// 送了时返回送的时候是不是规范模式。VEOF 被禁用时什么也不送。
fn send_eof(k: &Kernel, id: u32) -> Option<bool> {
    let Ok(tty) = tty_of(k, id) else {
        return Some(false);
    };
    let (eof, canonical) = {
        let t = tty.borrow();
        if t.readable() {
            return None;
        }
        (t.termios.cc[crate::tty::cc::VEOF], t.canonical())
    };
    if eof != 0 {
        let _ = feed(k, id, &[eof]);
    }
    Some(canonical)
}

/// 在规范模式下送出的 EOF 之后的一步：程序没读到它就切到了非规范模式（shell 读完一行切回
/// 规范模式执行命令，桥接线程这时送了 VEOF，回到提示符时行编辑又切到非规范模式），EOF 标记
/// 就没了（Linux 上变成一个 NUL 字节，script 有同样的问题），这时再送一次，这次是字符。
fn eof_after(k: &Kernel, id: u32, eof: Eof) -> Eof {
    match eof {
        Eof::Pending => match send_eof(k, id) {
            Some(true) => Eof::Canonical,
            Some(false) => Eof::Sent,
            None => Eof::Pending,
        },
        Eof::Canonical if tty_of(k, id).is_ok_and(|t| !t.borrow().canonical()) => {
            match send_eof(k, id) {
                Some(_) => Eof::Sent,
                None => Eof::Canonical,
            }
        }
        e => e,
    }
}

/// 把数据全部写到宿主 fd；宿主那边已经关闭时丢弃。
fn write_all(fd: i32, mut data: &[u8]) {
    while !data.is_empty() {
        match appine_hal::write(fd, data) {
            Ok(0) | Err(_) => return,
            Ok(n) => data = &data[n..],
        }
    }
}

/// 宿主终端当前的窗口大小；不是终端时是 24×80。
pub(crate) fn initial_size(term: &Terminal) -> Winsize {
    term.size.and_then(host_winsize).unwrap_or_default()
}

/// `appine-run` 的桥接线程：把宿主终端接到控制台 `id` 上，直到进程 `pid` 结束、所有
/// guest 线程退出并且输出写完为止。
pub(crate) fn bridge(shared: Arc<Shared>, term: Terminal, id: u32, pid: Tid) {
    let event = Arc::new(Event::new().expect("cannot create an eventfd for the terminal"));
    let mut eof = Eof::Open;
    let mut buf = vec![0u8; READ_CHUNK];
    let mut k = shared.lock();
    loop {
        match take_output(&k, id, 64 * 1024) {
            Ok(Some(out)) if !out.is_empty() => {
                drop(k);
                write_all(term.output, &out);
                k = shared.lock();
                continue;
            }
            // 控制台已经关闭，或者所有从设备都关闭了并且输出取完了
            Err(_) | Ok(Some(_)) => break,
            Ok(None) => {}
        }
        if k.watched.get(&pid).is_some_and(Option::is_some) && k.tasks.is_empty() {
            break;
        }
        eof = eof_after(&k, id, eof);
        let room = input_room(&k, id).unwrap_or(0);
        let mut fds = Vec::new();
        if eof == Eof::Open && room > 0 {
            fds.push((term.input, POLLIN, 0));
        }
        if let Some(r) = term.resize {
            fds.push((r, POLLIN, 0));
        }
        let objects = wake_objects(&k, id);
        let (k2, _) = block_for(
            &shared,
            k,
            &event,
            None,
            &mut fds,
            wake::ALL,
            objects.as_deref(),
        );
        k = k2;
        for (fd, _, revents) in fds {
            if revents == 0 {
                continue;
            }
            if fd == term.input {
                drop(k);
                let n = appine_hal::read(fd, &mut buf[..room.min(READ_CHUNK)]);
                k = shared.lock();
                match n {
                    Ok(n) if n > 0 => {
                        let _ = feed(&k, id, &buf[..n]);
                    }
                    // 宿主的输入结束了（stdin 是管道或者文件）：不再读，等程序读完已经送进去
                    // 的输入之后交给它一个 EOF
                    _ => eof = Eof::Pending,
                }
            } else {
                appine_hal::drain(fd);
                if let Some(w) = term.size.and_then(host_winsize) {
                    let _ = resize(&k, id, w);
                }
            }
        }
    }
}
