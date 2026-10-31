//! 进程内的 PTY 与行规程（设计文档 §9）。
//!
//! 一个 [`Tty`] 就是一对 PTY 的全部状态：从设备的 termios、窗口大小、输入队列
//! （规范模式下按行组织）、输出队列，以及作业控制需要的会话和前台进程组。主设备
//! 写入的是"键盘输入"，经过行规程处理（ICRNL、回显、行编辑、ISIG）进入输入队列；
//! 从设备写入的数据经过输出处理（ONLCR）进入输出队列，由主设备读走。
//!
//! 这里只有纯数据结构和行规程；阻塞、信号和作业控制的检查在 syscall 层完成。

use std::collections::VecDeque;
use std::rc::{Rc, Weak};

use crate::cell::RefCell;
use crate::kernel::{Tid, request_wake_object, wake};
use crate::process::Signal;

/// PTY 从设备的主设备号（`/dev/pts/<n>`）。
pub const PTS_MAJOR: u32 = 136;

/// tty 组：devpts 挂载时的 `gid=5`（mountinfo 里也这样写），新的终端都属于它。
pub const TTY_GID: u32 = 5;
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

    /// 字符宽度的几位。
    pub const CSIZE: u32 = 0o60;
    /// 8 位字符。
    pub const CS8: u32 = 0o60;
    /// 允许接收。
    pub const CREAD: u32 = 0o200;
    /// 奇偶校验。
    pub const PARENB: u32 = 0o400;
    /// 输出速率的几位：低 4 位是 B0–B38400，加上 `CBAUDEX` 是 B57600 起的 15 档。
    pub const CBAUD: u32 = 0o10017;
    /// `CBAUD` 里的扩展位。
    pub const CBAUDEX: u32 = 0o10000;
    /// 只有 `CBAUDEX` 时表示速率另写在 `c_ospeed`（输入速率的几位里是 `c_ispeed`）。
    pub const BOTHER: u32 = 0o10000;
    /// 输入速率在 `CBAUD` 左移这么多位的地方（`CIBAUD`），0 表示与输出相同。
    pub const IBSHIFT: u32 = 16;
    /// 地址位（RS-485），只能经由驱动改。
    pub const ADDRB: u32 = 0o4000000000;
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
    /// 行编辑由外部（主设备那边）做。
    pub const EXTPROC: u32 = 0o200000;
}

/// 包模式（TIOCPKT）下主设备读到的状态字节（`TIOCPKT_*`）。
pub mod pkt {
    /// 后面是数据。
    pub const DATA: u8 = 0;
    /// 从设备的输入被清空了。
    pub const FLUSHREAD: u8 = 1;
    /// 从设备的输出被清空了。
    pub const FLUSHWRITE: u8 = 2;
    /// 从设备的输出被停住了（^S、TCOOFF）。
    pub const STOP: u8 = 4;
    /// 恢复了。
    pub const START: u8 = 8;
    /// 不再是 ^S/^Q 流控。
    pub const NOSTOP: u8 = 16;
    /// 改成了 ^S/^Q 流控。
    pub const DOSTOP: u8 = 32;
    /// 设置了 EXTPROC 时 termios 变了。
    pub const IOCTL: u8 = 64;
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

/// 内核的 `ktermios`，即 asm-generic 的 `struct termios2`（44 字节）：`struct termios`
/// （36 字节，TCGETS、TCSETS 用）后面接着输入、输出速率（TCGETS2、TCSETS2 用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Termios {
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub line: u8,
    pub cc: [u8; NCCS],
    pub ispeed: u32,
    pub ospeed: u32,
}

/// `CBAUD` 各档的速率（Linux 的 `baud_table`，不是 sparc 的那份）：B0–B38400，之后是
/// `CBAUDEX` 加 1–15。
const BAUD_TABLE: [u32; 31] = [
    0, 50, 75, 110, 134, 150, 200, 300, 600, 1200, 1800, 2400, 4800, 9600, 19200, 38400, 57600,
    115200, 230400, 460800, 500000, 576000, 921600, 1000000, 1152000, 1500000, 2000000, 2500000,
    3000000, 3500000, 4000000,
];

/// 一组 `CBAUD` 位对应的速率；`BOTHER` 时是 `other`。
fn baud(cbaud: u32, other: u32) -> u32 {
    if cbaud == BOTHER {
        return other;
    }
    let i = if cbaud & CBAUDEX != 0 {
        (cbaud & !CBAUDEX) + 15
    } else {
        cbaud
    };
    BAUD_TABLE.get(i as usize).copied().unwrap_or(0)
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
            ispeed: 38400,
            ospeed: 38400,
        }
    }
}

impl Termios {
    /// 编码成 guest 的 `struct termios2`；前 36 字节就是 `struct termios`。
    pub fn encode(&self) -> [u8; 44] {
        let mut b = [0u8; 44];
        for (i, v) in [self.iflag, self.oflag, self.cflag, self.lflag]
            .iter()
            .enumerate()
        {
            b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        b[16] = self.line;
        b[17..36].copy_from_slice(&self.cc);
        b[36..40].copy_from_slice(&self.ispeed.to_le_bytes());
        b[40..].copy_from_slice(&self.ospeed.to_le_bytes());
        b
    }

    /// 从 guest 的 `struct termios2` 解码。
    pub fn decode(b: &[u8; 44]) -> Termios {
        let word = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
        let mut c = [0u8; NCCS];
        c.copy_from_slice(&b[17..36]);
        Termios {
            iflag: word(0),
            oflag: word(1),
            cflag: word(2),
            lflag: word(3),
            line: b[16],
            cc: c,
            ispeed: word(9),
            ospeed: word(10),
        }
    }

    /// 按 `CBAUD`、`CIBAUD` 填好两个速率（Linux 的 `set_termios` 在设置之前做的，
    /// `tty_termios_baud_rate`、`tty_termios_input_baud_rate`）：用老的 `Bxxx` 写法设置时
    /// 速率字段也总是可用的，`BOTHER` 时沿用字段里的值。输入速率的几位是 0 时与输出相同。
    pub fn load_speeds(&mut self) {
        let ospeed = baud(self.cflag & CBAUD, self.ospeed);
        let input = (self.cflag >> IBSHIFT) & CBAUD;
        self.ispeed = if input == 0 {
            ospeed
        } else {
            baud(input, self.ispeed)
        };
        self.ospeed = ospeed;
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
    /// 从设备的输出被停住（^S 或 TCOOFF，Linux 的 `flow.stopped`）时，停住之前写出的、
    /// 主设备照样能读走的字节数。停住期间从设备的写阻塞；之后的回显留到恢复时才给主设备
    /// （Linux 把它们留在回显缓冲区里）。
    held_from: Option<usize>,
    /// 输出是被 TCOOFF 停住的：^Q 恢复不了，要 TCOON（Linux 的 `flow.tco_stopped`）。
    tco_stopped: bool,
    /// 主设备的输出被 TCOOFF 停住：主设备的写等到 TCOON。主设备的 termios 没有 IXON，
    /// 只有 TCXONC 能停住它。
    pub master_stopped: bool,
    /// 主设备在包模式（TIOCPKT）：每次读先给一个状态字节（[`pkt`]），之后是数据。
    pub packet: bool,
    /// 包模式下还没读走的状态（Linux 记在从设备的 `ctrl.pktstatus`）。
    pub pktstatus: u8,
    /// 以它为控制终端的会话。
    pub session: Option<Tid>,
    /// 前台进程组。
    pub fg: Option<Tid>,
    /// 打开着的主设备、从设备个数。
    pub masters: usize,
    pub slaves: usize,
    /// 从设备是否还没有解锁（unlockpt）。
    pub locked: bool,
    /// 主设备都关掉了，已经挂断（`syscall::tty::hang_up`）：打开着的从设备读返回 0，写和
    /// ioctl 是 EIO，poll 总是就绪，带着 POLLERR、POLLHUP（Linux 的 `hung_up_tty_fops`）。
    pub hung_up: bool,
    /// 从设备、主设备这一边被 vhangup、TIOCVHANGUP 挂断过几次（`syscall::tty::vhangup`）：
    /// 在那之前打开的文件都挂断了（[`Tty::hung`]），之后打开的照常能用。
    pub vhangups: [u32; 2],
    /// 这一边挂断之后还没有重新打开过（Linux 的 `TTY_HUPPED`）：再挂断什么也不做。
    pub hupped: [bool; 2],
    /// `/dev/pts/<index>` 的属主、属组和权限位：与 Linux 按挂载选项 `gid=5,mode=620` 的
    /// devpts 一样，属主是打开主设备的进程的 fsuid，属组是 tty，权限 0620；chown、chmod
    /// 可以修改，记到终端关掉为止。
    pub owner: (u32, u32),
    pub mode: u32,
    /// 输入（从设备读的）、输出（主设备读的）各来过几次数据：与 Linux 一样，每来一次
    /// 都让读的一方的 `EPOLLET` 再触发一次。
    pub arrivals: [u64; 2],
    /// `/dev/pts/<index>` 的访问、修改、状态改变时间（纳秒）：w、who 的空闲时间看访问
    /// 时间（[`Tty::touch`]）。
    pub atime_ns: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl Tty {
    /// 新建一对 PTY，两端都还没有打开。
    pub fn new(index: u32) -> Tty {
        let now = wall_ns();
        Tty {
            index,
            termios: Termios::default(),
            winsize: Winsize::default(),
            editing: Vec::new(),
            lines: VecDeque::new(),
            raw: VecDeque::new(),
            output: VecDeque::new(),
            lnext: false,
            held_from: None,
            tco_stopped: false,
            master_stopped: false,
            packet: false,
            pktstatus: 0,
            session: None,
            fg: None,
            masters: 0,
            slaves: 0,
            locked: true,
            hung_up: false,
            vhangups: [0; 2],
            hupped: [false; 2],
            owner: (0, TTY_GID),
            mode: 0o620,
            arrivals: [0; 2],
            atime_ns: now,
            mtime_ns: now,
            ctime_ns: now,
        }
    }

    /// 从设备读到了数据（`mtime` 为 false）或者写出了数据：与 Linux 6.1 的 `tty_update_time`
    /// 一样只改秒，并且只在和原来相差 8 秒以上时才改，精确的时刻会泄露别人敲键盘的节奏。
    pub fn touch(&mut self, mtime: bool) {
        let sec = wall_ns().div_euclid(NS);
        let t = if mtime {
            &mut self.mtime_ns
        } else {
            &mut self.atime_ns
        };
        if (sec ^ t.div_euclid(NS)) & !7 != 0 {
            *t = sec * NS + t.rem_euclid(NS);
        }
    }

    /// 修改了属主、权限或者时间（chmod、chown、utimensat）：状态改变时间是现在。
    pub fn changed(&mut self) {
        self.ctime_ns = wall_ns();
    }

    /// 是不是规范模式（ICANON）。
    pub fn canonical(&self) -> bool {
        self.termios.l(ICANON)
    }

    fn pending_input(&self) -> usize {
        self.editing.len() + self.lines.iter().map(Vec::len).sum::<usize>() + self.raw.len()
    }

    /// 输入队列还能接收多少字节（主设备的写入据此阻塞）。规范模式下这一行已经写满时
    /// 仍然接收：多出来的字符被丢弃，只有行结束符有效，避免永远等不到换行。
    pub fn input_room(&self) -> usize {
        if self.master_stopped {
            return 0;
        }
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

    /// 输出队列还能接收多少字节：输出被停住时不接收（Linux 的 `pty_write_room`）。
    pub fn output_room(&self) -> usize {
        if self.held_from.is_some() {
            return 0;
        }
        OUTPUT_MAX.saturating_sub(self.output.len())
    }

    /// 主设备现在能读走的字节数（[`Tty::held_from`]）。
    fn visible(&self) -> usize {
        self.held_from
            .map_or(self.output.len(), |v| v.min(self.output.len()))
    }

    /// 主设备的一次读：取走输出。
    pub fn take_output(&mut self, max: usize) -> Vec<u8> {
        let n = max.min(self.visible());
        if let Some(v) = self.held_from.as_mut() {
            *v = (*v).min(self.output.len()) - n;
        }
        self.output.drain(..n).collect()
    }

    /// 主设备有没有可读的输出。
    pub fn has_output(&self) -> bool {
        self.visible() > 0
    }

    /// 包模式下记下状态 `set`、去掉 `clear`，主设备的读者随之可读（Linux 只在主设备
    /// 是包模式时才记）。
    fn report(&mut self, set: u8, clear: u8) {
        if self.packet {
            self.pktstatus = self.pktstatus & !clear | set;
            self.arrivals[1] += 1;
        }
    }

    /// 包模式下报告从设备这一边的清空（Linux 的 `n_tty_packet_mode_flush` 是输入，
    /// `pty_flush_buffer` 是输出）。主设备那边的清空不报告。
    pub fn report_flush(&mut self, input: bool, output: bool) {
        let set = if input { pkt::FLUSHREAD } else { 0 } | if output { pkt::FLUSHWRITE } else { 0 };
        if set != 0 {
            self.report(set, 0);
        }
    }

    /// 包模式下主设备的一次读要给的状态字节，取走它。
    pub fn take_pktstatus(&mut self) -> Option<u8> {
        if self.packet && self.pktstatus != 0 {
            Some(std::mem::take(&mut self.pktstatus))
        } else {
            None
        }
    }

    /// 停住从设备的输出（^S，Linux 的 `__stop_tty`，`pty_stop` 报告 STOP）。
    fn stop(&mut self) {
        if self.held_from.is_none() {
            self.held_from = Some(self.output.len());
            self.report(pkt::STOP, pkt::START);
        }
    }

    /// 恢复从设备的输出（^Q、IXANY、产生信号时，Linux 的 `start_tty`，`pty_start` 报告
    /// START）：TCOOFF 停住的不管。
    fn start(&mut self) {
        if !self.tco_stopped && self.held_from.take().is_some() {
            self.report(pkt::START, pkt::STOP);
        }
    }

    /// 从设备上的 TCOOFF。
    pub fn stop_output(&mut self) {
        if !self.tco_stopped {
            self.tco_stopped = true;
            self.stop();
        }
    }

    /// 从设备上的 TCOON：^S 停住的也一起恢复。
    pub fn start_output(&mut self) {
        if self.tco_stopped {
            self.tco_stopped = false;
            self.start();
        }
    }

    /// 从设备上的 TCIOFF、TCION：把 STOP、START 字符发给主设备，不受输出停住的限制，
    /// 排在暂存的回显前面（Linux 的 `tty_send_xchar`）。
    pub fn send_xchar(&mut self, c: u8) {
        let at = self.visible();
        self.output.insert(at, c);
        if let Some(v) = self.held_from.as_mut() {
            *v = at + 1;
        }
        self.arrivals[1] += 1;
    }

    /// 在这一边第 `born` 次 vhangup 之后打开的文件（主设备是 `master`）已经挂断：从设备在
    /// 主设备都关掉之后，两边在打开之后又被 vhangup、TIOCVHANGUP 挂断过时。
    pub fn hung(&self, master: bool, born: u32) -> bool {
        (!master && self.hung_up) || self.vhangups[master as usize] != born
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
        if let Some(v) = self.held_from.as_mut() {
            *v = 0;
        }
    }

    /// 修改 termios。地址位不变（Linux 的 `tty_set_termios`），PTY 总是 8 位、不校验、
    /// 允许接收（`pty_set_termios`）。规范模式开关时，在两种队列之间搬运已有的输入。
    pub fn set_termios(&mut self, mut t: Termios) {
        t.cflag ^= (t.cflag ^ self.termios.cflag) & ADDRB;
        t.cflag = t.cflag & !(CSIZE | PARENB) | CS8 | CREAD;
        // 包模式下报告 ^S/^Q 流控的开关，EXTPROC 时报告 termios 变了（`pty_set_termios`）
        let flow = |t: &Termios| t.i(IXON) && t.cc[VSTOP] == 0o23 && t.cc[VSTART] == 0o21;
        let (old_flow, new_flow) = (flow(&self.termios), flow(&t));
        let extproc = (self.termios.lflag | t.lflag) & EXTPROC != 0;
        if old_flow != new_flow {
            let set = if new_flow { pkt::DOSTOP } else { pkt::NOSTOP };
            self.report(set, pkt::DOSTOP | pkt::NOSTOP);
        }
        if extproc {
            self.report(pkt::IOCTL, 0);
        }
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
        self.arrivals[1] += 1;
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
        let echoed = self.output.len();
        for &c in data {
            if let Some(sig) = self.input_char(c) {
                signals.push(sig);
            }
        }
        self.arrivals[0] += 1;
        if self.output.len() != echoed {
            self.arrivals[1] += 1;
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
                self.stop();
                return None;
            }
            if t.is(c, VSTART) {
                self.start();
                return None;
            }
            if t.i(IXANY) {
                self.start();
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
                    self.flush_output();
                    self.report_flush(true, true);
                }
                if t.i(IXON) {
                    self.start();
                }
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
    /// 打开时这一边被 vhangup 过几次（[`Tty::hung`]）。
    pub born: u32,
}

impl PtyEnd {
    pub fn new(tty: Rc<RefCell<Tty>>, master: bool) -> PtyEnd {
        let born = {
            let mut t = tty.borrow_mut();
            if master {
                t.masters += 1;
            } else {
                t.slaves += 1;
                // 重新打开之后又能挂断了（Linux 的 tty_open 清掉 TTY_HUPPED）
                t.hupped[0] = false;
            }
            t.vhangups[master as usize]
        };
        PtyEnd { tty, master, born }
    }
}

impl Drop for PtyEnd {
    fn drop(&mut self) {
        let closed = {
            let mut t = self.tty.borrow_mut();
            if self.master {
                t.masters -= 1;
            } else {
                t.slaves -= 1;
            }
            self.master && t.masters == 0 && !t.hung_up
        };
        if closed {
            CLOSED.with_borrow_mut(|c| c.push(Rc::downgrade(&self.tty)));
        }
        // 另一端要看到挂断
        request_wake_object(wake::of(&self.tty));
    }
}

thread_local! {
    /// 主设备都关掉了、还没有挂断的终端。关闭打开文件是在 Drop 里，拿不到内核，由放开
    /// BKL 的线程在放开之前挂断（[`take_closed`]）。
    #[allow(clippy::missing_const_for_thread_local)] // 已经是 const，Android 上的 clippy 误报
    static CLOSED: std::cell::RefCell<Vec<Weak<RefCell<Tty>>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// 取走这个线程关掉了最后一个主设备、还没有挂断的终端（[`PtyEnd`] 记下的），由
/// `syscall::tty::hang_up_closed` 挂断。
pub(crate) fn take_closed() -> Vec<Rc<RefCell<Tty>>> {
    if CLOSED.with_borrow(Vec::is_empty) {
        return Vec::new();
    }
    let closed = CLOSED.with_borrow_mut(std::mem::take);
    closed.iter().filter_map(Weak::upgrade).collect()
}

/// 回显时显示成 `^X` 的控制字符：除 TAB、NL 之外的 C0 控制字符和 DEL。
const NS: i64 = 1_000_000_000;

/// 现在的墙上时间（纳秒）。
pub(crate) fn wall_ns() -> i64 {
    appine_hal::clock_gettime(0)
        .map(|(s, ns)| s * NS + ns)
        .unwrap_or(0)
}

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
    fn reads_and_writes_touch_the_times_every_8_seconds() {
        let mut t = Tty::new(0);
        let sec = wall_ns().div_euclid(NS);
        // 很久以前的时间：改成现在的秒，纳秒不变
        t.atime_ns = 100 * NS + 123;
        t.touch(false);
        assert!((t.atime_ns.div_euclid(NS) - sec).abs() <= 1);
        assert_eq!(t.atime_ns.rem_euclid(NS), 123);
        // 同一个 8 秒里不改
        t.mtime_ns = (sec & !7) * NS + 456;
        let before = t.mtime_ns;
        if wall_ns().div_euclid(NS) & !7 == sec & !7 {
            t.touch(true);
            assert_eq!(t.mtime_ns, before);
        }
    }

    #[test]
    fn stopped_output_blocks_the_slave_but_keeps_what_was_written() {
        let mut t = Tty::new(0);
        t.write_output(b"ab");
        t.write_input(b"\x13");
        // 停住之前写出的照样能读，从设备不能再写，之后的回显留到恢复
        assert_eq!(t.output_room(), 0);
        assert_eq!(t.take_output(1), b"a");
        t.write_input(b"x");
        assert_eq!(t.take_output(16), b"b");
        assert!(!t.has_output());
        t.write_input(b"\x11");
        assert!(t.output_room() > 0);
        assert_eq!(t.take_output(16), b"x");
        // TCOOFF 停住的 ^Q 恢复不了，要 TCOON；TCIOFF 发的字符不受停住的限制
        t.stop_output();
        t.write_input(b"\x11");
        assert_eq!(t.output_room(), 0);
        t.send_xchar(0x13);
        assert_eq!(t.take_output(16), b"\x13");
        t.start_output();
        assert!(t.output_room() > 0);
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
    fn speeds_follow_the_baud_bits() {
        let mut t = Termios::default();
        t.load_speeds();
        assert_eq!((t.ispeed, t.ospeed), (38400, 38400));
        // B115200（CBAUDEX 加 2）输出、B1200 输入
        t.cflag = t.cflag & !CBAUD & !(CBAUD << IBSHIFT) | 0o10002 | (0o11 << IBSHIFT);
        t.load_speeds();
        assert_eq!((t.ispeed, t.ospeed), (1200, 115200));
        // BOTHER 用字段里的速率；B0 时也是 0
        t.cflag = t.cflag & !CBAUD & !(CBAUD << IBSHIFT) | BOTHER | (BOTHER << IBSHIFT);
        (t.ispeed, t.ospeed) = (12345, 54321);
        t.load_speeds();
        assert_eq!((t.ispeed, t.ospeed), (12345, 54321));
        t.cflag &= !CBAUD & !(CBAUD << IBSHIFT);
        t.load_speeds();
        assert_eq!((t.ispeed, t.ospeed), (0, 0));
    }

    #[test]
    fn pty_keeps_eight_bits_and_the_address_bit() {
        let mut tty = Tty::new(0);
        let mut t = tty.termios;
        t.cflag = t.cflag & !(CSIZE | CREAD) | PARENB | ADDRB;
        tty.set_termios(t);
        assert_eq!(
            tty.termios.cflag & (CSIZE | CREAD | PARENB | ADDRB),
            CS8 | CREAD
        );
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
