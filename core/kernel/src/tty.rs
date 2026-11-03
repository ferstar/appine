//! In-process PTYs and the line discipline (the kernel/tty map).
//!
//! One [`Tty`] is the whole state of a PTY pair: the slave's termios, the window size, the input
//! queue (organized by lines in canonical mode), the output queue, and the session and foreground
//! process group job control needs. What the master writes is "keyboard input", which goes
//! through the line discipline (ICRNL, echo, line editing, ISIG) into the input queue; data the
//! slave writes goes through output processing (ONLCR) into the output queue, from which the
//! master reads it.
//!
//! Only plain data structures and the line discipline live here; blocking, signals and job
//! control checks are done in the syscall layer.

use std::collections::VecDeque;
use std::rc::{Rc, Weak};

use crate::cell::RefCell;
use crate::kernel::{Tid, request_wake_object, wake};
use crate::process::Signal;

/// The major number of PTY slaves (`/dev/pts/<n>`).
pub const PTS_MAJOR: u32 = 136;

/// The tty group: `gid=5` when devpts is mounted (also written so in mountinfo); every new
/// terminal belongs to it.
pub const TTY_GID: u32 = 5;

/// The device number of the devpts instance mounted at `/dev/pts` at boot
/// ([`crate::vfs::Fs::dev`]), which is also its id.
pub(crate) const BOOT_PTS: u64 = 0x18;
/// The most terminals one devpts instance can have (`NR_UNIX98_PTY_MAX`).
pub(crate) const PTY_MAX: u32 = 1 << 20;

/// Mount options of a devpts instance (Linux's `pts_mount_opts`): the owner and group of new
/// terminals (when not given, the fsuid and fsgid of the process creating it) and their mode, the
/// mode of ptmx, and the limit on the number of terminals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PtsOptions {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mode: u32,
    pub ptmxmode: u32,
    pub max: u32,
}

impl PtsOptions {
    /// The boot instance: the same as docker mounts for containers
    /// (`gid=5,mode=620,ptmxmode=666`).
    pub const BOOT: PtsOptions = PtsOptions {
        uid: None,
        gid: Some(TTY_GID),
        mode: 0o620,
        ptmxmode: 0o666,
        max: PTY_MAX,
    };

    /// Parses mount(2) options (`devpts_parse_param`): uid and gid are decimal and must be
    /// mappable in the caller's user namespace (`uid_in` converts to kernel IDs), mode and
    /// ptmxmode are octal; `newinstance` has had no effect since 4.7; anything else, or a bad
    /// format, is EINVAL. Options not given take the defaults (`mode=600,ptmxmode=000`).
    pub fn parse(
        data: &[u8],
        uid_in: impl Fn(u32) -> Option<u32>,
        gid_in: impl Fn(u32) -> Option<u32>,
    ) -> Result<PtsOptions, crate::errno::Errno> {
        use crate::errno::Errno;
        let mut o = PtsOptions {
            uid: None,
            gid: None,
            mode: 0o600,
            ptmxmode: 0,
            max: PTY_MAX,
        };
        let num = |v: &[u8], radix: u32| {
            std::str::from_utf8(v)
                .ok()
                .and_then(|s| u32::from_str_radix(s, radix).ok())
                .ok_or(Errno::EINVAL)
        };
        for opt in data.split(|&b| b == b',').filter(|o| !o.is_empty()) {
            let (key, value) = match opt.iter().position(|&b| b == b'=') {
                Some(i) => (&opt[..i], Some(&opt[i + 1..])),
                None => (opt, None),
            };
            match (key, value) {
                (b"uid", Some(v)) => o.uid = Some(uid_in(num(v, 10)?).ok_or(Errno::EINVAL)?),
                (b"gid", Some(v)) => o.gid = Some(gid_in(num(v, 10)?).ok_or(Errno::EINVAL)?),
                (b"mode", Some(v)) => o.mode = num(v, 8)? & 0o7777,
                (b"ptmxmode", Some(v)) => o.ptmxmode = num(v, 8)? & 0o7777,
                (b"max", Some(v)) => {
                    o.max = num(v, 10)?;
                    if o.max > PTY_MAX {
                        return Err(Errno::EINVAL);
                    }
                }
                (b"newinstance", None) => {}
                _ => return Err(Errno::EINVAL),
            }
        }
        Ok(o)
    }

    /// The superblock options in mountinfo (`devpts_show_options`).
    pub fn show(&self) -> String {
        let mut s = String::new();
        if let Some(u) = self.uid {
            s += &format!(",uid={u}");
        }
        if let Some(g) = self.gid {
            s += &format!(",gid={g}");
        }
        s += &format!(",mode={:03o},ptmxmode={:03o}", self.mode, self.ptmxmode);
        if self.max < PTY_MAX {
            s += &format!(",max={}", self.max);
        }
        s
    }
}
/// The input queue capacity (Linux's `N_TTY_BUF_SIZE`).
pub const INPUT_MAX: usize = 4096;
/// The output queue capacity: once the slave has filled it, writes block until the master reads.
pub const OUTPUT_MAX: usize = 64 * 1024;

/// termios flag bits (asm-generic values).
pub mod flags {
    /// Strip the 8th bit.
    pub const ISTRIP: u32 = 0o40;
    /// Translate NL to CR.
    pub const INLCR: u32 = 0o100;
    /// Ignore CR.
    pub const IGNCR: u32 = 0o200;
    /// Translate CR to NL.
    pub const ICRNL: u32 = 0o400;
    /// ^S/^Q flow control.
    pub const IXON: u32 = 0o2000;
    /// Any character restarts output stopped by ^S.
    pub const IXANY: u32 = 0o4000;
    /// Input is UTF-8 (erase deletes by character).
    pub const IUTF8: u32 = 0o40000;

    /// Output processing.
    pub const OPOST: u32 = 0o1;
    /// Translate NL to CR NL on output.
    pub const ONLCR: u32 = 0o4;
    /// Translate CR to NL on output.
    pub const OCRNL: u32 = 0o10;

    /// The character size bits.
    pub const CSIZE: u32 = 0o60;
    /// 8-bit characters.
    pub const CS8: u32 = 0o60;
    /// Enable the receiver.
    pub const CREAD: u32 = 0o200;
    /// Parity.
    pub const PARENB: u32 = 0o400;
    /// The output speed bits: the low 4 bits are B0–B38400, and with `CBAUDEX` 15 more steps from
    /// B57600.
    pub const CBAUD: u32 = 0o10017;
    /// The extension bit within `CBAUD`.
    pub const CBAUDEX: u32 = 0o10000;
    /// `CBAUDEX` alone means the speed is given separately in `c_ospeed` (in `c_ispeed` for the
    /// input speed bits).
    pub const BOTHER: u32 = 0o10000;
    /// The input speed is at `CBAUD` shifted left by this many bits (`CIBAUD`); 0 means the same as
    /// the output.
    pub const IBSHIFT: u32 = 16;
    /// The address bit (RS-485), changeable only through the driver.
    pub const ADDRB: u32 = 0o4000000000;
    /// Baud rate 38400.
    pub const B38400: u32 = 0o17;

    /// INTR, QUIT and SUSP generate signals.
    pub const ISIG: u32 = 0o1;
    /// Canonical mode (line editing).
    pub const ICANON: u32 = 0o2;
    /// Echo.
    pub const ECHO: u32 = 0o10;
    /// ERASE echoes as backspace.
    pub const ECHOE: u32 = 0o20;
    /// Echo a newline after KILL.
    pub const ECHOK: u32 = 0o40;
    /// Echo NL even when ECHO is off.
    pub const ECHONL: u32 = 0o100;
    /// Do not flush the queues when generating signals.
    pub const NOFLSH: u32 = 0o200;
    /// Background processes writing the terminal get SIGTTOU.
    pub const TOSTOP: u32 = 0o400;
    /// Echo control characters as `^X`.
    pub const ECHOCTL: u32 = 0o1000;
    /// KILL erases the whole line character by character.
    pub const ECHOKE: u32 = 0o4000;
    /// Extended functions (LNEXT, WERASE, REPRINT).
    pub const IEXTEN: u32 = 0o100000;
    /// Line editing is done externally (on the master side).
    pub const EXTPROC: u32 = 0o200000;
}

/// The status bytes the master reads in packet mode (TIOCPKT) (`TIOCPKT_*`).
pub mod pkt {
    /// Data follows.
    pub const DATA: u8 = 0;
    /// The slave's input was flushed.
    pub const FLUSHREAD: u8 = 1;
    /// The slave's output was flushed.
    pub const FLUSHWRITE: u8 = 2;
    /// The slave's output was stopped (^S, TCOOFF).
    pub const STOP: u8 = 4;
    /// Restarted.
    pub const START: u8 = 8;
    /// No longer ^S/^Q flow control.
    pub const NOSTOP: u8 = 16;
    /// Changed to ^S/^Q flow control.
    pub const DOSTOP: u8 = 32;
    /// termios changed while EXTPROC is set.
    pub const IOCTL: u8 = 64;
}

/// Indices into `c_cc`.
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
    /// The length of `c_cc` (asm-generic's `NCCS`).
    pub const NCCS: usize = 19;
}

use cc::*;
use flags::*;

/// The kernel's `ktermios`, that is asm-generic's `struct termios2` (44 bytes): `struct termios`
/// (36 bytes, used by TCGETS and TCSETS) followed by the input and output speeds (used by TCGETS2
/// and TCSETS2).
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

/// The speeds of the `CBAUD` steps (Linux's `baud_table`, not the sparc one): B0–B38400, then
/// `CBAUDEX` plus 1–15.
const BAUD_TABLE: [u32; 31] = [
    0, 50, 75, 110, 134, 150, 200, 300, 600, 1200, 1800, 2400, 4800, 9600, 19200, 38400, 57600,
    115200, 230400, 460800, 500000, 576000, 921600, 1000000, 1152000, 1500000, 2000000, 2500000,
    3000000, 3500000, 4000000,
];

/// The speed for a set of `CBAUD` bits; `other` for `BOTHER`.
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
    /// The initial settings of a PTY slave, the same as Linux's `tty_std_termios`.
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
    /// Encodes as the guest's `struct termios2`; the first 36 bytes are `struct termios`.
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

    /// Decodes from the guest's `struct termios2`.
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

    /// Fills in both speeds from `CBAUD` and `CIBAUD` (what Linux's `set_termios` does before
    /// applying the settings, `tty_termios_baud_rate`, `tty_termios_input_baud_rate`): the speed
    /// fields are always usable even when set the old `Bxxx` way, and with `BOTHER` the values in
    /// the fields are kept. When the input speed bits are 0 it is the same as the output.
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

    /// Whether `c` is a given special character (`_POSIX_VDISABLE`, i.e. 0, means disabled).
    fn is(&self, c: u8, which: usize) -> bool {
        self.cc[which] != 0 && self.cc[which] == c
    }
}

/// The window size (`struct winsize`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

impl Default for Winsize {
    /// The 24×80 the console uses when the host terminal's size is unknown.
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
    /// Encodes as the guest's `struct winsize`.
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

    /// Decodes from the guest's `struct winsize`.
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

/// A PTY pair.
pub struct Tty {
    /// `/dev/pts/<index>`.
    pub index: u32,
    /// The devpts instance it is in (its device number, [`PtsOptions`]).
    pub fs: u64,
    pub termios: Termios,
    pub winsize: Winsize,
    /// The line being edited in canonical mode.
    editing: Vec<u8>,
    /// Lines completed in canonical mode and ready to be read; an empty line means EOF (^D at the
    /// start of a line).
    lines: VecDeque<Vec<u8>>,
    /// Bytes ready to be read in non-canonical mode.
    raw: VecDeque<u8>,
    /// Output waiting for the master to read it.
    output: VecDeque<u8>,
    /// The next character is entered literally (LNEXT).
    lnext: bool,
    /// While the slave's output is stopped (^S or TCOOFF, Linux's `flow.stopped`), the number of
    /// bytes written before the stop that the master can still read. While stopped, slave writes
    /// block; later echoes reach the master only when output restarts (Linux keeps them in the
    /// echo buffer).
    held_from: Option<usize>,
    /// The output was stopped by TCOOFF: ^Q cannot restart it, TCOON is needed (Linux's
    /// `flow.tco_stopped`).
    tco_stopped: bool,
    /// The master's output is stopped by TCOOFF: master writes wait until TCOON. The master's
    /// termios has no IXON, so only TCXONC can stop it.
    pub master_stopped: bool,
    /// The master is in packet mode (TIOCPKT): each read first returns a status byte ([`pkt`]),
    /// then data.
    pub packet: bool,
    /// Status not yet read in packet mode (Linux records it in the slave's `ctrl.pktstatus`).
    pub pktstatus: u8,
    /// The session that has it as controlling terminal.
    pub session: Option<Tid>,
    /// The foreground process group.
    pub fg: Option<Tid>,
    /// The numbers of open masters and slaves.
    pub masters: usize,
    pub slaves: usize,
    /// The slave has been opened and now all its ends are closed (Linux's `TTY_OTHER_CLOSED`):
    /// master reads are EIO and poll includes POLLHUP; cleared when a slave is opened again. A
    /// slave never opened does not count, and master reads block as usual.
    pub slave_closed: bool,
    /// Whether the slave is still locked (unlockpt).
    pub locked: bool,
    /// All masters are closed and it has been hung up (`syscall::tty::hang_up`): open slaves
    /// read 0, writes and ioctls are EIO, and poll is always ready with POLLERR and POLLHUP
    /// (Linux's `hung_up_tty_fops`).
    pub hung_up: bool,
    /// How many times the slave and master sides were hung up by vhangup or TIOCVHANGUP
    /// (`syscall::tty::vhangup`): files opened before that are hung up ([`Tty::hung`]), those
    /// opened later work as usual.
    pub vhangups: [u32; 2],
    /// This side has not been reopened since its hangup (Linux's `TTY_HUPPED`): hanging it up
    /// again does nothing.
    pub hupped: [bool; 2],
    /// The owner, group and mode of `/dev/pts/<index>`: as with Linux devpts mounted with
    /// `gid=5,mode=620`, the owner is the fsuid of the process that opened the master, the group
    /// is tty and the mode 0620; chown and chmod can change them, and the change lasts until the
    /// terminal is closed.
    pub owner: (u32, u32),
    pub mode: u32,
    /// How many times data arrived for input (read by the slave) and output (read by the master):
    /// as on Linux, each arrival makes the reader's `EPOLLET` fire again.
    pub arrivals: [u64; 2],
    /// The access, modification and status change times of `/dev/pts/<index>` (nanoseconds): the
    /// idle time of w and who comes from the access time ([`Tty::touch`]).
    pub atime_ns: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl Tty {
    /// Creates a PTY pair with neither end open yet.
    pub fn new(index: u32) -> Tty {
        let now = wall_ns();
        let fs = BOOT_PTS;
        Tty {
            index,
            termios: Termios::default(),
            // All 0, as for a terminal newly allocated by Linux; the console sets it separately
            // once created (`terminal::open`)
            winsize: Winsize {
                rows: 0,
                cols: 0,
                xpixel: 0,
                ypixel: 0,
            },
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
            slave_closed: false,
            locked: true,
            hung_up: false,
            vhangups: [0; 2],
            hupped: [false; 2],
            owner: (0, TTY_GID),
            mode: 0o620,
            fs,
            arrivals: [0; 2],
            atime_ns: now,
            mtime_ns: now,
            ctime_ns: now,
        }
    }

    /// The slave read data (`mtime` false) or wrote data: as with Linux 6.1's `tty_update_time`,
    /// only the seconds change, and only when they differ from the old value by 8 seconds or more;
    /// the exact moment would leak the rhythm of someone's typing.
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

    /// The owner, mode or times were changed (chmod, chown, utimensat): the status change time
    /// is now.
    pub fn changed(&mut self) {
        self.ctime_ns = wall_ns();
    }

    /// Whether in canonical mode (ICANON).
    pub fn canonical(&self) -> bool {
        self.termios.l(ICANON)
    }

    fn pending_input(&self) -> usize {
        self.editing.len() + self.lines.iter().map(Vec::len).sum::<usize>() + self.raw.len()
    }

    /// How many more bytes the input queue can take (master writes block by this). In canonical
    /// mode it still accepts input when the line is already full: the extra characters are dropped
    /// and only line terminators take effect, so it never waits forever for a newline.
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

    /// Whether the slave has data to read now (in canonical mode a complete line is needed).
    pub fn readable(&self) -> bool {
        if self.canonical() {
            !self.lines.is_empty()
        } else {
            !self.raw.is_empty()
        }
    }

    /// The number of bytes that have arrived in non-canonical mode (for VMIN).
    pub fn raw_len(&self) -> usize {
        self.raw.len()
    }

    /// One slave read: at most one line in canonical mode, returning 0 at EOF. Returns None when
    /// there is no data.
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

    /// The number of readable bytes in the input queue (FIONREAD).
    pub fn input_len(&self) -> usize {
        if self.canonical() {
            self.lines.iter().map(Vec::len).sum()
        } else {
            self.raw.len()
        }
    }

    /// The number of bytes in the output queue (TIOCOUTQ).
    pub fn output_len(&self) -> usize {
        self.output.len()
    }

    /// How many more bytes the output queue can take: none while output is stopped (Linux's
    /// `pty_write_room`).
    pub fn output_room(&self) -> usize {
        if self.held_from.is_some() {
            return 0;
        }
        OUTPUT_MAX.saturating_sub(self.output.len())
    }

    /// The number of bytes the master can read now ([`Tty::held_from`]).
    fn visible(&self) -> usize {
        self.held_from
            .map_or(self.output.len(), |v| v.min(self.output.len()))
    }

    /// One master read: takes output.
    pub fn take_output(&mut self, max: usize) -> Vec<u8> {
        let n = max.min(self.visible());
        if let Some(v) = self.held_from.as_mut() {
            *v = (*v).min(self.output.len()) - n;
        }
        self.output.drain(..n).collect()
    }

    /// Whether the master has output to read.
    pub fn has_output(&self) -> bool {
        self.visible() > 0
    }

    /// In packet mode, records status `set` and removes `clear`, which makes the master's readers
    /// ready (Linux records it only when the master is in packet mode).
    fn report(&mut self, set: u8, clear: u8) {
        if self.packet {
            self.pktstatus = self.pktstatus & !clear | set;
            self.arrivals[1] += 1;
        }
    }

    /// In packet mode, reports a flush on the slave side (Linux's `n_tty_packet_mode_flush` for
    /// input, `pty_flush_buffer` for output). Flushes on the master side are not reported.
    pub fn report_flush(&mut self, input: bool, output: bool) {
        let set = if input { pkt::FLUSHREAD } else { 0 } | if output { pkt::FLUSHWRITE } else { 0 };
        if set != 0 {
            self.report(set, 0);
        }
    }

    /// The status byte one master read returns in packet mode; takes it.
    pub fn take_pktstatus(&mut self) -> Option<u8> {
        if self.packet && self.pktstatus != 0 {
            Some(std::mem::take(&mut self.pktstatus))
        } else {
            None
        }
    }

    /// Stops the slave's output (^S, Linux's `__stop_tty`; `pty_stop` reports STOP).
    fn stop(&mut self) {
        if self.held_from.is_none() {
            self.held_from = Some(self.output.len());
            self.report(pkt::STOP, pkt::START);
        }
    }

    /// Restarts the slave's output (^Q, IXANY, when generating signals; Linux's `start_tty`,
    /// `pty_start` reports START): output stopped by TCOOFF is left alone.
    fn start(&mut self) {
        if !self.tco_stopped && self.held_from.take().is_some() {
            self.report(pkt::START, pkt::STOP);
        }
    }

    /// TCOOFF on the slave.
    pub fn stop_output(&mut self) {
        if !self.tco_stopped {
            self.tco_stopped = true;
            self.stop();
        }
    }

    /// TCOON on the slave: output stopped by ^S restarts too.
    pub fn start_output(&mut self) {
        if self.tco_stopped {
            self.tco_stopped = false;
            self.start();
        }
    }

    /// TCIOFF and TCION on the slave: sends the STOP or START character to the master, regardless
    /// of stopped output, ahead of the held echoes (Linux's `tty_send_xchar`).
    pub fn send_xchar(&mut self, c: u8) {
        let at = self.visible();
        self.output.insert(at, c);
        if let Some(v) = self.held_from.as_mut() {
            *v = at + 1;
        }
        self.arrivals[1] += 1;
    }

    /// Whether a file opened after this side's `born`-th vhangup (on the master when `master`) has
    /// been hung up: for the slave once all masters have closed, and for either side when it was
    /// hung up again by vhangup or TIOCVHANGUP after the open.
    pub fn hung(&self, master: bool, born: u32) -> bool {
        (!master && self.hung_up) || self.vhangups[master as usize] != born
    }

    /// Flushes the input queue (TCIFLUSH, when generating signals).
    pub fn flush_input(&mut self) {
        self.editing.clear();
        self.lines.clear();
        self.raw.clear();
        self.lnext = false;
    }

    /// Flushes the output queue (TCOFLUSH).
    pub fn flush_output(&mut self) {
        self.output.clear();
        if let Some(v) = self.held_from.as_mut() {
            *v = 0;
        }
    }

    /// Changes termios. The address bit stays (Linux's `tty_set_termios`), and a PTY is always 8
    /// bits, without parity, with the receiver enabled (`pty_set_termios`). When canonical mode is
    /// switched on or off, input already queued is moved between the two queues.
    pub fn set_termios(&mut self, mut t: Termios) {
        t.cflag ^= (t.cflag ^ self.termios.cflag) & ADDRB;
        t.cflag = t.cflag & !(CSIZE | PARENB) | CS8 | CREAD;
        // In packet mode report ^S/^Q flow control being switched, and with EXTPROC report that
        // termios changed (`pty_set_termios`)
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

    /// A slave write: goes through output processing into the output queue.
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

    /// Echoes a character: with ECHOCTL, control characters show as `^X`.
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

    /// Erases one character on the screen (ECHOE). Echoed control characters take two columns.
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

    /// Removes one character from the end of the line being edited (a whole character with
    /// UTF-8) and returns the removed bytes.
    fn pop_char(&mut self) -> Option<Vec<u8>> {
        let mut erased = vec![self.editing.pop()?];
        // UTF-8 continuation bytes are 10xxxxxx: keep deleting until the leading byte
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

    /// A master write (input from the terminal); returns the signals to send to the foreground
    /// process group. The caller guarantees `data` does not exceed [`Tty::input_room`].
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
                // First skip whitespace at the end of the line, then delete up to the next
                // whitespace
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
                // EOF at the start of a line makes the reader read 0; EOF in the middle of a line
                // hands what is there to the reader
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

    /// An ordinary character: in canonical mode it is added to the line being edited (dropped
    /// when the line is full), otherwise it is readable at once.
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

/// One end of a PTY, held by an open file; opening and closing maintain the counts of both ends.
pub struct PtyEnd {
    pub tty: Rc<RefCell<Tty>>,
    /// Whether this is the master.
    pub master: bool,
    /// The devpts mount it was opened through (where the terminal's instance is mounted): the
    /// slave's path (the fd's link, what TIOCGPTPEER opens) is computed from it.
    pub mnt: crate::vfs::MountId,
    /// The open file is in that mount (opened through a path in devpts, or by TIOCGPTPEER): while
    /// it is open, the mount cannot be unmounted (EBUSY). Opening through a device node elsewhere
    /// does not count.
    pub pin: bool,
    /// How many times this side had been vhangup'd at open ([`Tty::hung`]).
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
                t.slave_closed = false;
                // After a reopen it can be hung up again (Linux's tty_open clears TTY_HUPPED)
                t.hupped[0] = false;
            }
            t.vhangups[master as usize]
        };
        PtyEnd {
            tty,
            master,
            mnt: crate::vfs::PTS_MOUNT,
            pin: false,
            born,
        }
    }

    /// Opened through devpts mount `mnt`; with `pin`, the open file is in that mount.
    pub fn via(mut self, mnt: crate::vfs::MountId, pin: bool) -> PtyEnd {
        self.mnt = mnt;
        self.pin = pin;
        self
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
                t.slave_closed = t.slaves == 0;
            }
            self.master && t.masters == 0 && !t.hung_up
        };
        if closed {
            CLOSED.with_borrow_mut(|c| c.push(Rc::downgrade(&self.tty)));
        }
        // The other end must see the hangup
        request_wake_object(wake::of(&self.tty));
    }
}

thread_local! {
    /// Terminals whose masters are all closed but which are not hung up yet. Open files are
    /// closed in Drop, which cannot reach the kernel, so the thread releasing the BKL hangs them
    /// up before releasing it ([`take_closed`]).
    #[allow(clippy::missing_const_for_thread_local)] // already const; Android clippy false positive
    static CLOSED: std::cell::RefCell<Vec<Weak<RefCell<Tty>>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Takes the terminals whose last master this thread closed and which are not hung up yet
/// (recorded by [`PtyEnd`]), for `syscall::tty::hang_up_closed` to hang up.
pub(crate) fn take_closed() -> Vec<Rc<RefCell<Tty>>> {
    if CLOSED.with_borrow(Vec::is_empty) {
        return Vec::new();
    }
    let closed = CLOSED.with_borrow_mut(std::mem::take);
    closed.iter().filter_map(Weak::upgrade).collect()
}

/// The control characters echoed as `^X`: the C0 control characters except TAB and NL, and DEL.
const NS: i64 = 1_000_000_000;

/// The current wall-clock time (nanoseconds).
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

    /// devpts options: defaults, the various spellings, errors, and the mountinfo form, as on
    /// Linux.
    #[test]
    fn devpts_options() {
        let id = |x: u32| Some(x);
        let parse = |d: &[u8]| PtsOptions::parse(d, id, id);
        let o = parse(b"").unwrap();
        assert_eq!(o.show(), ",mode=600,ptmxmode=000");
        let o = parse(b"newinstance,ptmxmode=0666,mode=0620,gid=5").unwrap();
        assert_eq!(o.show(), ",gid=5,mode=620,ptmxmode=666");
        let o = parse(b"uid=7,gid=8,max=1").unwrap();
        assert_eq!((o.uid, o.gid, o.max), (Some(7), Some(8), 1));
        assert_eq!(o.show(), ",uid=7,gid=8,mode=600,ptmxmode=000,max=1");
        for bad in [
            &b"mode=xyz"[..],
            b"bogus",
            b"gid=abc",
            b"max=1048577",
            b"mode",
        ] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        // A uid unmappable in the user namespace
        assert!(PtsOptions::parse(b"uid=7", |_| None, id).is_err());
        assert_eq!(PtsOptions::BOOT.show(), ",gid=5,mode=620,ptmxmode=666");
    }

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
        // ^D at the start of a line is EOF, ^D in the middle of a line hands over what is there
        t.write_input(b"ab\x04\x04");
        assert_eq!(read_all(&mut t), b"ab");
        assert_eq!(t.read(&mut [0; 8]), Some(0));
        assert!(!t.readable());
        // ^U erases the whole line, ^V makes the next character literal
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
        // With ISIG off, ^C is an ordinary character
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
        // A time long ago: changed to the current second, nanoseconds unchanged
        t.atime_ns = 100 * NS + 123;
        t.touch(false);
        assert!((t.atime_ns.div_euclid(NS) - sec).abs() <= 1);
        assert_eq!(t.atime_ns.rem_euclid(NS), 123);
        // Unchanged within the same 8 seconds
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
        // What was written before the stop can still be read, the slave cannot write any more,
        // and later echoes wait until restart
        assert_eq!(t.output_room(), 0);
        assert_eq!(t.take_output(1), b"a");
        t.write_input(b"x");
        assert_eq!(t.take_output(16), b"b");
        assert!(!t.has_output());
        t.write_input(b"\x11");
        assert!(t.output_room() > 0);
        assert_eq!(t.take_output(16), b"x");
        // ^Q cannot restart output stopped by TCOOFF, TCOON is needed; characters sent by TCIOFF
        // are not held by the stop
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
        // B115200 (CBAUDEX plus 2) output, B1200 input
        t.cflag = t.cflag & !CBAUD & !(CBAUD << IBSHIFT) | 0o10002 | (0o11 << IBSHIFT);
        t.load_speeds();
        assert_eq!((t.ispeed, t.ospeed), (1200, 115200));
        // BOTHER uses the speed in the field; with B0 it is 0 too
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
