//! 进程内的 PTY 与行规程（设计文档 §9）。
//!
//! 一个 [`Tty`] 就是一对 PTY 的全部状态：从设备的 termios、窗口大小、输入队列
//! （规范模式下按行组织）、输出队列，以及作业控制需要的会话和前台进程组。主设备
//! 写入的是"键盘输入"，经过行规程处理（ICRNL、回显、行编辑、ISIG）进入输入队列；
//! 从设备写入的数据经过输出处理（ONLCR）进入输出队列，由主设备读走。
//!
//! 这里只有纯数据结构和行规程；阻塞、信号和作业控制的检查在 syscall 层完成。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::kernel::{Tid, request_wake};
use crate::process::Signal;

/// PTY 从设备的主设备号（`/dev/pts/<n>`）。
pub const PTS_MAJOR: u32 = 136;
/// 输入队列的容量（Linux 的 `N_TTY_BUF_SIZE`）。
pub const INPUT_MAX: usize = 4096;
/// 输出队列的容量：从设备写满之后阻塞，直到主设备读走。
pub const OUTPUT_MAX: usize = 64 * 1024;

/// termios 的标志位（asm-generic 的取值）。
pub mod flags {
    /// 去掉第 8 位。
    pub const ISTRIP: u32 = 0o40;
    /// NL 转成 CR。
    pub const INLCR: u32 = 0o100;
    /// 忽略 CR。
    pub const IGNCR: u32 = 0o200;
    /// CR 转成 NL。
    pub const ICRNL: u32 = 0o400;
    /// ^S/^Q 流控。
    pub const IXON: u32 = 0o2000;
    /// 任意字符都能恢复被 ^S 停止的输出。
    pub const IXANY: u32 = 0o4000;
    /// 输入是 UTF-8（退格按字符删除）。
    pub const IUTF8: u32 = 0o40000;

    /// 输出处理。
    pub const OPOST: u32 = 0o1;
    /// 输出时 NL 转成 CR NL。
    pub const ONLCR: u32 = 0o4;
    /// 输出时 CR 转成 NL。
    pub const OCRNL: u32 = 0o10;

    /// 8 位字符。
    pub const CS8: u32 = 0o60;
    /// 允许接收。
    pub const CREAD: u32 = 0o200;
    /// 波特率 38400。
    pub const B38400: u32 = 0o17;

    /// INTR、QUIT、SUSP 产生信号。
    pub const ISIG: u32 = 0o1;
    /// 规范模式（行编辑）。
    pub const ICANON: u32 = 0o2;
    /// 回显。
    pub const ECHO: u32 = 0o10;
    /// ERASE 回显成退格。
    pub const ECHOE: u32 = 0o20;
    /// KILL 之后回显换行。
    pub const ECHOK: u32 = 0o40;
    /// 关闭 ECHO 时仍然回显 NL。
    pub const ECHONL: u32 = 0o100;
    /// 产生信号时不清空队列。
    pub const NOFLSH: u32 = 0o200;
    /// 后台进程写终端时产生 SIGTTOU。
    pub const TOSTOP: u32 = 0o400;
    /// 控制字符回显成 `^X`。
    pub const ECHOCTL: u32 = 0o1000;
    /// KILL 逐个字符擦掉整行。
    pub const ECHOKE: u32 = 0o4000;
    /// 扩展功能（LNEXT、WERASE、REPRINT）。
    pub const IEXTEN: u32 = 0o100000;
}

/// `c_cc` 的下标。
pub mod cc {
    pub const VINTR: usize = 0;
    pub const VQUIT: usize = 1;
    pub const VERASE: usize = 2;
    pub const VKILL: usize = 3;
    pub const VEOF: usize = 4;
    pub const VTIME: usize = 5;
    pub const VMIN: usize = 6;
    pub const VSTART: usize = 8;
    pub const VSTOP: usize = 9;
    pub const VSUSP: usize = 10;
    pub const VEOL: usize = 11;
    pub const VREPRINT: usize = 12;
    pub const VWERASE: usize = 14;
    pub const VLNEXT: usize = 15;
    pub const VEOL2: usize = 16;
    /// `c_cc` 的长度（asm-generic 的 `NCCS`）。
    pub const NCCS: usize = 19;
}

use cc::*;
use flags::*;

/// asm-generic 的 `struct termios`（36 字节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Termios {
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub line: u8,
    pub cc: [u8; NCCS],
}

impl Default for Termios {
    /// PTY 从设备的初始设置，与 Linux 的 `tty_std_termios` 相同。
    fn default() -> Self {
        let mut c = [0u8; NCCS];
        c[..17].copy_from_slice(
            b"\x03\x1c\x7f\x15\x04\x00\x01\x00\x11\x13\x1a\x00\x12\x0f\x17\x16\x00",
        );
        Termios {
            iflag: ICRNL | IXON,
            oflag: OPOST | ONLCR,
            cflag: B38400 | CS8 | CREAD,
            lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
            line: 0,
            cc: c,
        }
    }
}

impl Termios {
    /// 编码成 guest 的 `struct termios`。
    pub fn encode(&self) -> [u8; 36] {
        let mut b = [0u8; 36];
        for (i, v) in [self.iflag, self.oflag, self.cflag, self.lflag]
            .iter()
            .enumerate()
        {
            b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        b[16] = self.line;
        b[17..].copy_from_slice(&self.cc);
        b
    }

    /// 从 guest 的 `struct termios` 解码。
    pub fn decode(b: &[u8; 36]) -> Termios {
        let word = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
        let mut c = [0u8; NCCS];
        c.copy_from_slice(&b[17..]);
        Termios {
            iflag: word(0),
            oflag: word(1),
            cflag: word(2),
            lflag: word(3),
            line: b[16],
            cc: c,
        }
    }

    fn l(&self, f: u32) -> bool {
        self.lflag & f != 0
    }

    fn i(&self, f: u32) -> bool {
        self.iflag & f != 0
    }

    /// `c` 是不是某个特殊字符（`_POSIX_VDISABLE` 即 0 表示禁用）。
    fn is(&self, c: u8, which: usize) -> bool {
        self.cc[which] != 0 && self.cc[which] == c
    }
}

/// 窗口大小（`struct winsize`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

impl Default for Winsize {
    fn default() -> Self {
        Winsize {
            rows: 24,
            cols: 80,
            xpixel: 0,
            ypixel: 0,
        }
    }
}

impl Winsize {
    /// 编码成 guest 的 `struct winsize`。
    pub fn encode(&self) -> [u8; 8] {
        let mut b = [0u8; 8];
        for (i, v) in [self.rows, self.cols, self.xpixel, self.ypixel]
            .iter()
            .enumerate()
        {
            b[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    /// 从 guest 的 `struct winsize` 解码。
    pub fn decode(b: &[u8; 8]) -> Winsize {
        let h = |i: usize| u16::from_le_bytes([b[i * 2], b[i * 2 + 1]]);
        Winsize {
            rows: h(0),
            cols: h(1),
            xpixel: h(2),
            ypixel: h(3),
        }
    }
}

/// 一对 PTY。
pub struct Tty {
    /// `/dev/pts/<index>`。
    pub index: u32,
    pub termios: Termios,
    pub winsize: Winsize,
    /// 规范模式下正在编辑的一行。
    editing: Vec<u8>,
    /// 规范模式下已经完成、可以读走的行；空行表示 EOF（行首的 ^D）。
    lines: VecDeque<Vec<u8>>,
    /// 非规范模式下可以读走的字节。
    raw: VecDeque<u8>,
    /// 等主设备读走的输出。
    output: VecDeque<u8>,
    /// 下一个字符按字面输入（LNEXT）。
    lnext: bool,
    /// 输出被 ^S 停止。
    pub stopped: bool,
    /// 以它为控制终端的会话。
    pub session: Option<Tid>,
    /// 前台进程组。
    pub fg: Option<Tid>,
    /// 打开着的主设备、从设备个数。
    pub masters: usize,
    pub slaves: usize,
    /// 从设备是否还没有解锁（unlockpt）。
    pub locked: bool,
}

impl Tty {
    /// 新建一对 PTY，两端都还没有打开。
    pub fn new(index: u32) -> Tty {
        Tty {
            index,
            termios: Termios::default(),
            winsize: Winsize::default(),
            editing: Vec::new(),
            lines: VecDeque::new(),
            raw: VecDeque::new(),
            output: VecDeque::new(),
            lnext: false,
            stopped: false,
            session: None,
            fg: None,
            masters: 0,
            slaves: 0,
            locked: true,
        }
    }

    fn canonical(&self) -> bool {
        self.termios.l(ICANON)
    }

    fn pending_input(&self) -> usize {
        self.editing.len() + self.lines.iter().map(Vec::len).sum::<usize>() + self.raw.len()
    }

    /// 输入队列还能接收多少字节（主设备的写入据此阻塞）。规范模式下这一行已经写满时
    /// 仍然接收：多出来的字符被丢弃，只有行结束符有效，避免永远等不到换行。
    pub fn input_room(&self) -> usize {
        let room = INPUT_MAX.saturating_sub(self.pending_input());
        if room == 0 && self.canonical() && self.lines.is_empty() {
            1
        } else {
            room
        }
    }

    /// 从设备现在有没有可读的数据（规范模式下要有完整的一行）。
    pub fn readable(&self) -> bool {
        if self.canonical() {
            !self.lines.is_empty()
        } else {
            !self.raw.is_empty()
        }
    }

    /// 非规范模式下已经到达的字节数（VMIN 用）。
    pub fn raw_len(&self) -> usize {
        self.raw.len()
    }

    /// 从设备的一次读：规范模式下最多一行，EOF 时返回 0。没有数据时返回 None。
    pub fn read(&mut self, buf: &mut [u8]) -> Option<usize> {
        if self.canonical() {
            let line = self.lines.front_mut()?;
            let n = buf.len().min(line.len());
            buf[..n].copy_from_slice(&line[..n]);
            line.drain(..n);
            if line.is_empty() {
                self.lines.pop_front();
            }
            Some(n)
        } else {
            if self.raw.is_empty() {
                return None;
            }
            let n = buf.len().min(self.raw.len());
            for (d, s) in buf.iter_mut().zip(self.raw.drain(..n)) {
                *d = s;
            }
            Some(n)
        }
    }

    /// 输入队列里可读的字节数（FIONREAD）。
    pub fn input_len(&self) -> usize {
        if self.canonical() {
            self.lines.iter().map(Vec::len).sum()
        } else {
            self.raw.len()
        }
    }

    /// 输出队列里的字节数（TIOCOUTQ）。
    pub fn output_len(&self) -> usize {
        self.output.len()
    }

    /// 输出队列还能接收多少字节。
    pub fn output_room(&self) -> usize {
        OUTPUT_MAX.saturating_sub(self.output.len())
    }

    /// 主设备的一次读：取走输出（被 ^S 停止时没有）。
    pub fn take_output(&mut self, max: usize) -> Vec<u8> {
        if self.stopped {
            return Vec::new();
        }
        let n = max.min(self.output.len());
        self.output.drain(..n).collect()
    }

    /// 主设备有没有可读的输出。
    pub fn has_output(&self) -> bool {
        !self.stopped && !self.output.is_empty()
    }

    /// 清空输入队列（TCIFLUSH、产生信号时）。
    pub fn flush_input(&mut self) {
        self.editing.clear();
        self.lines.clear();
        self.raw.clear();
        self.lnext = false;
    }

    /// 清空输出队列（TCOFLUSH）。
    pub fn flush_output(&mut self) {
        self.output.clear();
    }

    /// 修改 termios。规范模式开关时，在两种队列之间搬运已有的输入。
    pub fn set_termios(&mut self, t: Termios) {
        let was = self.canonical();
        self.termios = t;
        match (was, self.canonical()) {
            (true, false) => {
                let lines: Vec<u8> = self.lines.drain(..).flatten().collect();
                self.raw.extend(lines);
                self.raw.extend(self.editing.drain(..));
            }
            (false, true) => self.editing.extend(self.raw.drain(..)),
            _ => {}
        }
    }

    /// 从设备的写入：经过输出处理放进输出队列。
    pub fn write_output(&mut self, data: &[u8]) {
        for &c in data {
            self.put_output(c);
        }
    }

    fn put_output(&mut self, c: u8) {
        let t = &self.termios;
        if t.oflag & OPOST != 0 {
            if c == b'\n' && t.oflag & ONLCR != 0 {
                self.output.extend(b"\r\n");
                return;
            }
            if c == b'\r' && t.oflag & OCRNL != 0 {
                self.output.push_back(b'\n');
                return;
            }
        }
        self.output.push_back(c);
    }

    /// 回显一个字符：ECHOCTL 时控制字符显示成 `^X`。
    fn echo(&mut self, c: u8) {
        if !self.termios.l(ECHO) {
            return;
        }
        if self.termios.l(ECHOCTL) && is_ctrl(c) {
            self.output.extend([b'^', c ^ 0x40]);
        } else {
            self.put_output(c);
        }
    }

    /// 擦掉屏幕上的一个字符（ECHOE）。控制字符回显时占两列。
    fn echo_erase(&mut self, erased: &[u8]) {
        if !self.termios.l(ECHO) {
            return;
        }
        if !self.termios.l(ECHOE) {
            if let Some(&c) = erased.first() {
                self.echo(c);
            }
            return;
        }
        let width = match erased {
            [c] if self.termios.l(ECHOCTL) && is_ctrl(*c) => 2,
            _ => 1,
        };
        for _ in 0..width {
            self.output.extend(b"\x08 \x08");
        }
    }

    /// 从正在编辑的行尾去掉一个字符（UTF-8 时去掉整个字符），返回去掉的字节。
    fn pop_char(&mut self) -> Option<Vec<u8>> {
        let mut erased = vec![self.editing.pop()?];
        // UTF-8 的后续字节是 10xxxxxx：一直删到首字节为止
        if self.termios.i(IUTF8) {
            while erased[0] & 0xc0 == 0x80 {
                match self.editing.pop() {
                    Some(b) => erased.insert(0, b),
                    None => break,
                }
            }
        }
        Some(erased)
    }

    /// 主设备的写入（来自终端的输入），返回要发给前台进程组的信号。
    /// 调用方保证 `data` 不超过 [`Tty::input_room`]。
    pub fn write_input(&mut self, data: &[u8]) -> Vec<Signal> {
        let mut signals = Vec::new();
        for &c in data {
            if let Some(sig) = self.input_char(c) {
                signals.push(sig);
            }
        }
        signals
    }

    fn input_char(&mut self, mut c: u8) -> Option<Signal> {
        let t = self.termios;
        if t.i(ISTRIP) {
            c &= 0x7f;
        }
        if self.lnext {
            self.lnext = false;
            self.receive(c);
            return None;
        }
        if c == b'\r' {
            if t.i(IGNCR) {
                return None;
            }
            if t.i(ICRNL) {
                c = b'\n';
            }
        } else if c == b'\n' && t.i(INLCR) {
            c = b'\r';
        }
        if t.i(IXON) {
            if t.is(c, VSTOP) {
                self.stopped = true;
                return None;
            }
            if t.is(c, VSTART) {
                self.stopped = false;
                return None;
            }
            if t.i(IXANY) {
                self.stopped = false;
            }
        }
        if t.l(ISIG) {
            let sig = if t.is(c, VINTR) {
                Some(Signal::INT)
            } else if t.is(c, VQUIT) {
                Some(Signal::QUIT)
            } else if t.is(c, VSUSP) {
                Some(Signal::TSTP)
            } else {
                None
            };
            if let Some(sig) = sig {
                if !t.l(NOFLSH) {
                    self.flush_input();
                    self.output.clear();
                }
                self.stopped = false;
                self.echo(c);
                return Some(sig);
            }
        }
        if t.l(ICANON) {
            if t.l(IEXTEN) && t.is(c, VLNEXT) {
                self.lnext = true;
                if t.l(ECHO) && t.l(ECHOCTL) {
                    self.output.extend(b"^\x08");
                }
                return None;
            }
            if t.is(c, VERASE) {
                if let Some(erased) = self.pop_char() {
                    self.echo_erase(&erased);
                }
                return None;
            }
            if t.l(IEXTEN) && t.is(c, VWERASE) {
                // 先跳过行尾的空白，再删到下一个空白
                while self.editing.last().is_some_and(|b| b.is_ascii_whitespace()) {
                    let e = self.pop_char().unwrap();
                    self.echo_erase(&e);
                }
                while self
                    .editing
                    .last()
                    .is_some_and(|b| !b.is_ascii_whitespace())
                {
                    let e = self.pop_char().unwrap();
                    self.echo_erase(&e);
                }
                return None;
            }
            if t.is(c, VKILL) {
                if t.l(ECHOKE) {
                    while let Some(e) = self.pop_char() {
                        self.echo_erase(&e);
                    }
                } else {
                    self.editing.clear();
                    self.echo(c);
                    if t.l(ECHOK) {
                        self.put_output(b'\n');
                    }
                }
                return None;
            }
            if t.l(IEXTEN) && t.is(c, VREPRINT) {
                self.echo(c);
                self.put_output(b'\n');
                let line = self.editing.clone();
                for b in line {
                    self.echo(b);
                }
                return None;
            }
            if t.is(c, VEOF) {
                // 行首的 EOF 让读者读到 0；行中间的 EOF 把已有的内容交给读者
                let line = std::mem::take(&mut self.editing);
                self.lines.push_back(line);
                return None;
            }
            let eol = c == b'\n' || t.is(c, VEOL) || (t.l(IEXTEN) && t.is(c, VEOL2));
            if eol {
                self.editing.push(c);
                if t.l(ECHO) || (c == b'\n' && t.l(ECHONL)) {
                    self.put_output(c);
                }
                let line = std::mem::take(&mut self.editing);
                self.lines.push_back(line);
                return None;
            }
        }
        self.receive(c);
        None
    }

    /// 普通字符：规范模式下加进正在编辑的行（满了就丢弃），否则直接可读。
    fn receive(&mut self, c: u8) {
        if self.canonical() {
            if self.editing.len() < INPUT_MAX - 1 {
                self.editing.push(c);
                self.echo(c);
            }
        } else {
            self.raw.push_back(c);
            self.echo(c);
        }
    }
}

/// PTY 的一端，由一个打开文件持有；打开和关闭时维护两端的计数。
pub struct PtyEnd {
    pub tty: Rc<RefCell<Tty>>,
    /// 是不是主设备。
    pub master: bool,
}

impl PtyEnd {
    pub fn new(tty: Rc<RefCell<Tty>>, master: bool) -> PtyEnd {
        {
            let mut t = tty.borrow_mut();
            if master {
                t.masters += 1;
            } else {
                t.slaves += 1;
            }
        }
        PtyEnd { tty, master }
    }
}

impl Drop for PtyEnd {
    fn drop(&mut self) {
        let mut t = self.tty.borrow_mut();
        if self.master {
            t.masters -= 1;
        } else {
            t.slaves -= 1;
        }
        // 另一端要看到挂断
        request_wake();
    }
}

/// 回显时显示成 `^X` 的控制字符：除 TAB、NL 之外的 C0 控制字符和 DEL。
fn is_ctrl(c: u8) -> bool {
    (c < 0x20 && c != b'\t' && c != b'\n') || c == 0x7f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(t: &mut Tty) -> Vec<u8> {
        let mut buf = [0u8; 256];
        let n = t.read(&mut buf).unwrap_or(0);
        buf[..n].to_vec()
    }

    #[test]
    fn canonical_mode_edits_lines_and_echoes() {
        let mut t = Tty::new(0);
        assert!(t.write_input(b"helo\x7flo wor\x17world\r").is_empty());
        assert!(t.readable());
        assert_eq!(read_all(&mut t), b"hello world\n");
        assert_eq!(
            t.take_output(1024),
            b"helo\x08 \x08lo wor\x08 \x08\x08 \x08\x08 \x08world\r\n"
        );
        // 行首 ^D 是 EOF，行中间的 ^D 交出已有的内容
        t.write_input(b"ab\x04\x04");
        assert_eq!(read_all(&mut t), b"ab");
        assert_eq!(t.read(&mut [0; 8]), Some(0));
        assert!(!t.readable());
        // ^U 擦掉整行，^V 让下一个字符按字面输入
        t.write_input(b"junk\x15\x16\x03\n");
        assert_eq!(read_all(&mut t), b"\x03\n");
    }

    #[test]
    fn signals_flush_input_and_echo() {
        let mut t = Tty::new(0);
        t.write_input(b"partial");
        t.take_output(1024);
        assert_eq!(t.write_input(b"\x03"), vec![Signal::INT]);
        assert_eq!(t.take_output(1024), b"^C");
        assert_eq!(t.write_input(b"\x1a\x1c"), vec![Signal::TSTP, Signal::QUIT]);
        t.write_input(b"x\n");
        assert_eq!(read_all(&mut t), b"x\n");
        // 关掉 ISIG 之后 ^C 就是普通字符
        let mut raw = t.termios;
        raw.lflag &= !(ISIG | ICANON | ECHO);
        t.set_termios(raw);
        assert!(t.write_input(b"\x03\r").is_empty());
        assert_eq!(read_all(&mut t), b"\x03\n");
    }

    #[test]
    fn switching_modes_keeps_pending_input() {
        let mut t = Tty::new(0);
        t.write_input(b"one\ntw");
        let mut raw = t.termios;
        raw.lflag &= !ICANON;
        t.set_termios(raw);
        assert_eq!(read_all(&mut t), b"one\ntw");
        let mut buf = [0u8; 4];
        t.write_input(b"abcdef");
        assert_eq!(t.read(&mut buf), Some(4));
        assert_eq!(t.raw_len(), 2);
    }

    #[test]
    fn output_processing_and_flow_control() {
        let mut t = Tty::new(0);
        t.write_output(b"a\nb");
        assert_eq!(t.take_output(16), b"a\r\nb");
        t.write_input(b"\x13");
        t.write_output(b"x");
        assert!(!t.has_output() && t.take_output(16).is_empty());
        t.write_input(b"\x11");
        assert_eq!(t.take_output(16), b"x");
    }

    #[test]
    fn utf8_erase_removes_whole_characters() {
        let mut t = Tty::new(0);
        let mut tio = t.termios;
        tio.iflag |= IUTF8;
        t.set_termios(tio);
        t.write_input("aé\x7f\n".as_bytes());
        assert_eq!(read_all(&mut t), b"a\n");
    }

    #[test]
    fn termios_round_trip() {
        let t = Termios::default();
        assert_eq!(Termios::decode(&t.encode()), t);
        assert_eq!(t.cc[VINTR], 3);
        assert_eq!(t.cc[VMIN], 1);
        let w = Winsize {
            rows: 50,
            cols: 132,
            xpixel: 0,
            ypixel: 0,
        };
        assert_eq!(Winsize::decode(&w.encode()), w);
    }
}
