//! 把宿主终端接到一个进程内的 PTY 上（设计文档 §9）。
//!
//! 第一个进程的 stdin/stdout/stderr 和控制终端是这个 PTY 的从设备。桥接线程把宿主的
//! 输入写进主设备（经过行规程，^C 变成发给前台进程组的 SIGINT），把主设备的输出写到
//! 宿主。它不是 guest 线程，但和它们一样只在持有 BKL 时访问内核对象，等待也走同一套
//! 机制：guest 写终端时请求唤醒，桥接线程就会醒来取走输出。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use appine_hal::Event;

use crate::fd::{FileKind, FileRef, OpenFile, oflags};
use crate::kernel::{Kernel, Shared, block, request_wake};
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

/// 建好会话用的 PTY：主设备留在 kernel 里给桥接线程用，返回从设备的打开文件和 PTY。
pub(crate) fn create(k: &mut Kernel, term: &Terminal) -> (FileRef, Rc<RefCell<Tty>>) {
    let index = (0..).find(|i| !k.ptys.contains_key(i)).unwrap();
    let tty = Rc::new(RefCell::new(Tty::new(index)));
    {
        let mut t = tty.borrow_mut();
        t.locked = false;
        if let Some(w) = term.size.and_then(host_winsize) {
            t.winsize = w;
        }
    }
    k.ptys.insert(index, Rc::downgrade(&tty));
    k.terminal = Some(PtyEnd::new(tty.clone(), true));
    let path: Rc<[u8]> = Rc::from(format!("/dev/pts/{index}").into_bytes());
    let slave = OpenFile::with_path(
        FileKind::Pty(PtyEnd::new(tty.clone(), false)),
        oflags::O_RDWR,
        Some(path),
    );
    (slave, tty)
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

/// 桥接线程：直到会话结束、所有 guest 线程退出并且输出写完为止。
pub(crate) fn bridge(shared: Arc<Shared>, term: Terminal) {
    let event = Arc::new(Event::new().expect("cannot create an eventfd for the terminal"));
    let mut input_open = true;
    let mut buf = vec![0u8; READ_CHUNK];
    let mut k = shared.lock();
    loop {
        let Some(tty) = k.terminal.as_ref().map(|m| m.tty.clone()) else {
            break;
        };
        let out = tty.borrow_mut().take_output(64 * 1024);
        let room = tty.borrow().input_room();
        drop(tty);
        if !out.is_empty() {
            // 输出队列空出来了，等着写终端的线程可以继续
            request_wake();
            drop(k);
            write_all(term.output, &out);
            k = shared.lock();
            continue;
        }
        if k.session.is_some() && k.tasks.is_empty() {
            break;
        }
        let mut fds = Vec::new();
        if input_open && room > 0 {
            fds.push((term.input, POLLIN, 0));
        }
        if let Some(r) = term.resize {
            fds.push((r, POLLIN, 0));
        }
        let (k2, _) = block(&shared, k, &event, None, &mut fds);
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
                    Ok(n) if n > 0 => feed(&k, &buf[..n]),
                    // 宿主的输入结束了：不再读，会话照常进行直到第一个进程退出
                    _ => input_open = false,
                }
            } else {
                appine_hal::drain(fd);
                if let Some(w) = term.size.and_then(host_winsize) {
                    resize(&k, w);
                }
            }
        }
    }
    // 在持有 BKL 时释放主设备
    k.terminal = None;
}

/// 宿主的输入交给行规程，产生的信号发给前台进程组。
fn feed(k: &Kernel, data: &[u8]) {
    let Some(tty) = k.terminal.as_ref().map(|m| m.tty.clone()) else {
        return;
    };
    let signals = tty.borrow_mut().write_input(data);
    request_wake();
    let fg = tty.borrow().fg;
    if let Some(fg) = fg {
        for sig in signals {
            signal::send_to_pgrp(k, fg, sig, SigInfo::kernel());
        }
    }
}

/// 宿主终端的窗口大小变了：更新 PTY，给前台进程组发 SIGWINCH。
fn resize(k: &Kernel, w: Winsize) {
    let Some(tty) = k.terminal.as_ref().map(|m| m.tty.clone()) else {
        return;
    };
    let (changed, fg) = {
        let mut t = tty.borrow_mut();
        (std::mem::replace(&mut t.winsize, w) != w, t.fg)
    };
    if let (true, Some(fg)) = (changed, fg) {
        signal::send_to_pgrp(k, fg, Signal::WINCH, SigInfo::kernel());
    }
}
