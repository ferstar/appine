//! seccomp（设计文档 §11.1）：线程在系统调用的入口按自己的 seccomp 模式检查这个系统调用。
//!
//! - 严格模式（`SECCOMP_MODE_STRICT`）只允许 read、write、exit、rt_sigreturn，别的系统调用
//!   让线程以 SIGKILL 结束（Linux 的 `do_exit(SIGKILL)`，只结束这个线程）。
//! - 过滤模式（`SECCOMP_MODE_FILTER`）在 `struct seccomp_data` 上运行线程的过滤器链里的每一个
//!   cBPF 程序（从最新装的开始），取动作最严格的结果（[`run_filters`]）：结束进程或线程、
//!   SIGSYS（TRAP）、返回错误码、交给跟踪者（`PTRACE_EVENT_SECCOMP`）、交给通知 fd 那头的
//!   监管进程（USER_NOTIF）、记一笔再执行、执行。
//!
//! 与 Linux 一样，过滤器属于线程：clone、fork 继承，execve 保留，`SECCOMP_FILTER_FLAG_TSYNC`
//! 同步给同一线程组的其他线程。fork 之后的进程共用祖先的过滤器，过滤器连成一棵树，每个
//! 线程看到的是从自己最新的那个往上的一条链（[`Chain`]）。检查在 PTRACE_SYSCALL 的入口停住
//! 之后（arm64 的 `syscall_trace_enter`），vDSO 里读时钟的存根不检查：Linux 上它们不进内核。
//!
//! 程序的检查与 Linux 相同：先按 cBPF 的规则（`bpf_check_classic`），再按 seccomp 的
//! （`seccomp_check_filter`，只能读 `seccomp_data` 里对齐的字，不能读包）。执行用一个简单
//! 的解释器，结果与 Linux 的 JIT 相同：移位按 32 取模，除以 0 时程序返回 0。
//!
//! 与 Linux 一样按编号缓存（`seccomp_cache`）：装过滤器时把程序当作只知道编号和架构来模拟
//! 一遍（[`const_allow`]），对哪些编号总是返回 ALLOW 记在位图里，与上一个过滤器的位图按位与。
//! 这些系统调用不再运行链上的程序。线程把位图另存一份原子的（[`publish`]），不持 BKL 的
//! 快速路径（`syscall::fast`）对这些编号照常走。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use appine_engine::Cpu;
use appine_hal::Event;

use crate::errno::Errno;
use crate::fd::{FileKind, OpenFile, oflags};
use crate::kernel::{Cx, Kernel, Tid, request_wake_object, signal_later, wake};
use crate::process::{ExitStatus, Signal};
use crate::signal::{self, InfoFields, SigInfo};
use crate::task::Task;
use crate::uaccess::UserSlice;

/// seccomp 的模式（`/proc/<pid>/status` 的 `Seccomp`、PR_GET_SECCOMP）。
pub(crate) mod mode {
    pub const DISABLED: u32 = 0;
    pub const STRICT: u32 = 1;
    pub const FILTER: u32 = 2;
    /// 过滤器要求结束这个线程之后（Linux 的 `SECCOMP_MODE_DEAD`）：再进内核就以 SIGKILL 结束。
    pub const DEAD: u32 = 3;
}

/// 过滤器的返回值：高 16 位是动作，低 16 位是附带的数据。动作按有符号数比较，越小越严格。
mod ret {
    pub const KILL_PROCESS: u32 = 0x8000_0000;
    pub const KILL_THREAD: u32 = 0;
    pub const TRAP: u32 = 0x0003_0000;
    pub const ERRNO: u32 = 0x0005_0000;
    pub const USER_NOTIF: u32 = 0x7fc0_0000;
    pub const TRACE: u32 = 0x7ff0_0000;
    pub const LOG: u32 = 0x7ffc_0000;
    pub const ALLOW: u32 = 0x7fff_0000;
    pub const ACTION_FULL: u32 = 0xffff_0000;
    pub const DATA: u32 = 0xffff;
}

/// `SECCOMP_FILTER_FLAG_*`。
mod flag {
    pub const TSYNC: u32 = 1;
    pub const LOG: u32 = 2;
    /// 不要预测执行的缓解措施（宿主上的，appine 管不了）：只检查。
    pub const SPEC_ALLOW: u32 = 4;
    pub const NEW_LISTENER: u32 = 8;
    pub const TSYNC_ESRCH: u32 = 16;
    pub const WAIT_KILLABLE_RECV: u32 = 32;
    pub const MASK: u32 =
        TSYNC | LOG | SPEC_ALLOW | NEW_LISTENER | TSYNC_ESRCH | WAIT_KILLABLE_RECV;
}

/// seccomp(2) 的操作。
const SET_MODE_STRICT: u32 = 0;
const SET_MODE_FILTER: u32 = 1;
const GET_ACTION_AVAIL: u32 = 2;
const GET_NOTIF_SIZES: u32 = 3;

/// `seccomp_data.arch`、SIGSYS 的 `si_arch`。
const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;
/// 一个程序最多的指令数（`BPF_MAXINSNS`）。
const MAX_INSNS: usize = 4096;
/// 一条链上转换成 eBPF 后的指令数加上旧过滤器各 4 条的额外开销（Linux 的
/// `MAX_INSNS_PER_PATH`）。arm64 Linux 先把 cBPF 转成 eBPF，再检查这个上限。
const MAX_INSNS_PER_PATH: usize = (1 << 18) / 8;
/// `struct seccomp_data` 的大小。
const DATA_SIZE: usize = 64;
/// 按编号缓存的位图覆盖多少个编号（Linux 是 arm64 的系统调用个数，这里取 512，appine 实现的
/// 编号都在里面），以及它有几个字。
const CACHED_NRS: usize = 512;
pub(crate) const CACHE_WORDS: usize = CACHED_NRS / 64;
/// SIGSYS 的 `si_code`。
const SYS_SECCOMP: i32 = 1;
/// ERRNO 的数据最大是 `MAX_ERRNO`。
const MAX_ERRNO: u32 = 4095;
/// 严格模式允许的系统调用：read、write、exit、rt_sigreturn。
const STRICT_ALLOWED: [u64; 4] = [63, 64, 93, 139];
/// `seccomp_notif_resp.flags`：让系统调用照常执行。
const USER_NOTIF_FLAG_CONTINUE: u32 = 1;

/// 通知 fd 的 ioctl。
const IOCTL_NOTIF_RECV: u32 = 0xc050_2100;
const IOCTL_NOTIF_SEND: u32 = 0xc018_2101;
const IOCTL_NOTIF_ID_VALID: u32 = 0x4008_2102;
/// 最早定义时方向写错了的 ID_VALID，Linux 仍然认。
const IOCTL_NOTIF_ID_VALID_WRONG_DIR: u32 = 0x8008_2102;
/// ADDFD 的长度可以变（结构体以后可能加长），比较时去掉方向和长度。
const IOCTL_NOTIF_ADDFD: u32 = 0x4018_2103;
const IOC_DIR_SIZE_MASK: u32 = 0xc000_0000 | 0x3fff_0000;
/// `struct seccomp_notif_addfd` 最初的长度。
const ADDFD_SIZE_VER0: usize = 24;
const ADDFD_FLAG_SETFD: u32 = 1;
const ADDFD_FLAG_SEND: u32 = 2;

/// cBPF 的操作码。
mod bpf {
    pub const LD: u16 = 0x00;
    pub const LDX: u16 = 0x01;
    pub const ST: u16 = 0x02;
    pub const STX: u16 = 0x03;
    pub const ALU: u16 = 0x04;
    pub const JMP: u16 = 0x05;
    pub const RET: u16 = 0x06;
    pub const MISC: u16 = 0x07;

    pub const W: u16 = 0x00;
    pub const H: u16 = 0x08;
    pub const B: u16 = 0x10;

    pub const IMM: u16 = 0x00;
    pub const ABS: u16 = 0x20;
    pub const IND: u16 = 0x40;
    pub const MEM: u16 = 0x60;
    pub const LEN: u16 = 0x80;
    pub const MSH: u16 = 0xa0;

    pub const ADD: u16 = 0x00;
    pub const SUB: u16 = 0x10;
    pub const MUL: u16 = 0x20;
    pub const DIV: u16 = 0x30;
    pub const OR: u16 = 0x40;
    pub const AND: u16 = 0x50;
    pub const LSH: u16 = 0x60;
    pub const RSH: u16 = 0x70;
    pub const NEG: u16 = 0x80;
    pub const MOD: u16 = 0x90;
    pub const XOR: u16 = 0xa0;

    pub const JA: u16 = 0x00;
    pub const JEQ: u16 = 0x10;
    pub const JGT: u16 = 0x20;
    pub const JGE: u16 = 0x30;
    pub const JSET: u16 = 0x40;

    pub const K: u16 = 0x00;
    pub const X: u16 = 0x08;
    /// `BPF_RET | BPF_A`。
    pub const A: u16 = 0x10;

    pub const TAX: u16 = 0x00;
    pub const TXA: u16 = 0x80;

    /// 草稿内存的格数。
    pub const MEMWORDS: u32 = 16;

    /// seccomp 把读 `seccomp_data` 的 `LD | W | ABS` 改写成的操作码（Linux 也这样借用它）。
    pub const LOAD_DATA: u16 = LDX | W | ABS;
}

/// 一条 cBPF 指令（`struct sock_filter`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Insn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

impl Insn {
    fn decode(b: &[u8]) -> Insn {
        Insn {
            code: u16::from_le_bytes([b[0], b[1]]),
            jt: b[2],
            jf: b[3],
            k: u32::from_le_bytes(b[4..8].try_into().unwrap()),
        }
    }
}

/// cBPF 认得的操作码（Linux 的 `chk_code_allowed`）。
fn classic_code(code: u16) -> bool {
    use bpf::*;
    matches!(
        code,
        0x04 | 0x0c // ADD
            | 0x14 | 0x1c // SUB
            | 0x24 | 0x2c // MUL
            | 0x34 | 0x3c // DIV
            | 0x94 | 0x9c // MOD
            | 0x54 | 0x5c // AND
            | 0x44 | 0x4c // OR
            | 0xa4 | 0xac // XOR
            | 0x64 | 0x6c // LSH
            | 0x74 | 0x7c // RSH
            | 0x84 // NEG
    ) || [
        LD | W | ABS,
        LD | H | ABS,
        LD | B | ABS,
        LD | W | LEN,
        LD | W | IND,
        LD | H | IND,
        LD | B | IND,
        LD | IMM,
        LD | MEM,
        LDX | W | LEN,
        LDX | B | MSH,
        LDX | IMM,
        LDX | MEM,
        ST,
        STX,
        MISC | TAX,
        MISC | TXA,
        RET | K,
        RET | A,
        JMP | JA,
    ]
    .contains(&code)
        || conditional(code)
}

/// 条件跳转。
fn conditional(code: u16) -> bool {
    use bpf::*;
    code & 0x07 == JMP && code & 0xf0 != JA && code & 0xf0 <= JSET
}

/// 按 cBPF 的规则检查程序（Linux 的 `bpf_check_classic`）：操作码认得，除以常数 0、移位
/// 超过 31、草稿内存越界、跳出程序都不行，最后一条是返回，草稿内存先写后读。
fn check_classic(prog: &[Insn]) -> Result<(), Errno> {
    use bpf::*;
    let n = prog.len();
    for (pc, i) in prog.iter().enumerate() {
        if !classic_code(i.code) {
            return Err(Errno::EINVAL);
        }
        let bad = match i.code {
            c if c == ALU | DIV | K || c == ALU | MOD | K => i.k == 0,
            c if c == ALU | LSH | K || c == ALU | RSH | K => i.k >= 32,
            c if c == LD | MEM || c == LDX | MEM || c == ST || c == STX => i.k >= MEMWORDS,
            c if c == JMP | JA => i.k as u64 >= (n - pc - 1) as u64,
            c if conditional(c) => pc + i.jt as usize + 1 >= n || pc + i.jf as usize + 1 >= n,
            _ => false,
        };
        if bad {
            return Err(Errno::EINVAL);
        }
    }
    if !matches!(prog[n - 1].code, c if c == RET | K || c == RET | A) {
        return Err(Errno::EINVAL);
    }
    check_load_and_stores(prog)
}

/// 草稿内存在每条路径上都先写后读（Linux 的 `check_load_and_stores`）：跳转把到达时已经
/// 写过的格子传给目标，跳转之后的下一条只能从别处跳来。
fn check_load_and_stores(prog: &[Insn]) -> Result<(), Errno> {
    use bpf::*;
    let mut masks = vec![u16::MAX; prog.len()];
    let mut valid = 0u16;
    for (pc, i) in prog.iter().enumerate() {
        valid &= masks[pc];
        match i.code {
            c if c == ST || c == STX => valid |= 1 << i.k,
            c if c == LD | MEM || c == LDX | MEM => {
                if valid & 1 << i.k == 0 {
                    return Err(Errno::EINVAL);
                }
            }
            c if c == JMP | JA => {
                masks[pc + 1 + i.k as usize] &= valid;
                valid = u16::MAX;
            }
            c if conditional(c) => {
                masks[pc + 1 + i.jt as usize] &= valid;
                masks[pc + 1 + i.jf as usize] &= valid;
                valid = u16::MAX;
            }
            _ => {}
        }
    }
    Ok(())
}

/// 按 seccomp 的规则检查并改写程序（Linux 的 `seccomp_check_filter`）：读数据只能读
/// `seccomp_data` 里按 4 字节对齐的字，读长度变成常数 64，别的读包的指令和取模不行。
fn check_seccomp(prog: &mut [Insn]) -> Result<(), Errno> {
    use bpf::*;
    for i in prog {
        match i.code {
            c if c == LD | W | ABS => {
                if i.k as usize >= DATA_SIZE || i.k & 3 != 0 {
                    return Err(Errno::EINVAL);
                }
                i.code = LOAD_DATA;
            }
            c if c == LD | W | LEN => {
                i.code = LD | IMM;
                i.k = DATA_SIZE as u32;
            }
            c if c == LDX | W | LEN => {
                i.code = LDX | IMM;
                i.k = DATA_SIZE as u32;
            }
            0x06 | 0x16 // RET K、RET A
            | 0x04 | 0x0c | 0x14 | 0x1c | 0x24 | 0x2c | 0x34 | 0x3c // ADD、SUB、MUL、DIV
            | 0x54 | 0x5c | 0x44 | 0x4c | 0xa4 | 0xac // AND、OR、XOR
            | 0x64 | 0x6c | 0x74 | 0x7c | 0x84 // LSH、RSH、NEG
            | 0x00 | 0x01 // LD IMM、LDX IMM
            | 0x07 | 0x87 // TAX、TXA
            | 0x60 | 0x61 | 0x02 | 0x03 // LD MEM、LDX MEM、ST、STX
            | 0x05 | 0x15 | 0x1d | 0x35 | 0x3d | 0x25 | 0x2d | 0x45 | 0x4d => {} // 跳转
            _ => return Err(Errno::EINVAL),
        }
    }
    Ok(())
}

/// Linux `bpf_convert_filter` 转换后的长度，用于过滤器链的容量检查。这里仍解释执行
/// cBPF，只计转换后的长度；seccomp 已排除了包访问和取模指令。
fn converted_len(prog: &[Insn]) -> usize {
    use bpf::*;
    // 清零 A、X 并保存上下文指针。
    3 + prog
        .iter()
        .map(|i| match i.code {
            c if c == RET | K => 2,
            c if c == ALU | DIV | X => 5,
            c if conditional(c) => {
                let immediate = usize::from(c & X == 0 && i.k > i32::MAX as u32);
                let extra_jump = usize::from(i.jf != 0 && (i.jt != 0 || c & 0xf0 == JSET));
                1 + immediate + extra_jump
            }
            _ => 1,
        })
        .sum::<usize>()
}

/// 在 `data`（`struct seccomp_data`）上运行检查过的程序，返回它的返回值。
fn run(prog: &[Insn], data: &[u8; DATA_SIZE]) -> u32 {
    use bpf::*;
    let (mut a, mut x) = (0u32, 0u32);
    let mut mem = [0u32; MEMWORDS as usize];
    let mut pc = 0;
    loop {
        let i = prog[pc];
        pc += 1;
        let src = if i.code & X != 0 { x } else { i.k };
        match i.code {
            LOAD_DATA => {
                let k = i.k as usize;
                a = u32::from_le_bytes(data[k..k + 4].try_into().unwrap());
            }
            c if c == LD | IMM => a = i.k,
            c if c == LDX | IMM => x = i.k,
            c if c == LD | MEM => a = mem[i.k as usize],
            c if c == LDX | MEM => x = mem[i.k as usize],
            c if c == ST => mem[i.k as usize] = a,
            c if c == STX => mem[i.k as usize] = x,
            c if c == MISC | TAX => x = a,
            c if c == MISC | TXA => a = x,
            c if c == RET | K => return i.k,
            c if c == RET | A => return a,
            c if c == JMP | JA => pc += i.k as usize,
            c if c & 0x07 == ALU => match c & 0xf0 {
                ADD => a = a.wrapping_add(src),
                SUB => a = a.wrapping_sub(src),
                MUL => a = a.wrapping_mul(src),
                // 除以 0（只可能是 X）时 cBPF 程序返回 0
                DIV if src == 0 => return 0,
                DIV => a /= src,
                OR => a |= src,
                AND => a &= src,
                XOR => a ^= src,
                LSH => a = a.wrapping_shl(src),
                RSH => a = a.wrapping_shr(src),
                NEG => a = a.wrapping_neg(),
                _ => unreachable!("取模过不了检查"),
            },
            c => {
                let taken = match c & 0xf0 {
                    JEQ => a == src,
                    JGT => a > src,
                    JGE => a >= src,
                    _ => a & src != 0,
                };
                pc += if taken { i.jt } else { i.jf } as usize;
            }
        }
    }
}

/// 只知道编号 `nr` 和架构时，程序是不是一定返回 ALLOW（Linux 的 `seccomp_is_const_allow`）：
/// 只读编号和架构，只有与常数比较的跳转、无条件跳转和与常数按位与。读别的数据、用别的指令的
/// 都不算，返回值要正好是 ALLOW（带数据的不算）。
fn const_allow(prog: &[Insn], nr: u32) -> bool {
    use bpf::*;
    let mut a = 0u32;
    let mut pc = 0;
    while let Some(i) = prog.get(pc) {
        pc += 1;
        match i.code {
            LOAD_DATA => match i.k {
                0 => a = nr,
                4 => a = AUDIT_ARCH_AARCH64,
                _ => return false,
            },
            c if c == RET | K => return i.k == ret::ALLOW,
            c if c == JMP | JA => pc += i.k as usize,
            c if conditional(c) && c & X == 0 => {
                let taken = match c & 0xf0 {
                    JEQ => a == i.k,
                    JGT => a > i.k,
                    JGE => a >= i.k,
                    _ => a & i.k != 0,
                };
                pc += if taken { i.jt } else { i.jf } as usize;
            }
            c if c == ALU | AND | K => a &= i.k,
            _ => return false,
        }
    }
    false
}

/// 新过滤器的位图（Linux 的 `seccomp_cache_prepare_bitmap`）：上一个过滤器的位图里有（没有
/// 上一个时是所有编号）、这个程序也总是返回 ALLOW 的编号。
fn cache(prog: &[Insn], prev: Option<&[u64; CACHE_WORDS]>) -> [u64; CACHE_WORDS] {
    let mut allow = prev.copied().unwrap_or([u64::MAX; CACHE_WORDS]);
    for nr in 0..CACHED_NRS {
        let bit = 1 << (nr % 64);
        if allow[nr / 64] & bit != 0 && !const_allow(prog, nr as u32) {
            allow[nr / 64] &= !bit;
        }
    }
    allow
}

/// 位图（每个字由 `word` 给出）里有没有编号 `nr`。
fn cached(word: impl Fn(usize) -> u64, nr: u64) -> bool {
    nr < CACHED_NRS as u64 && word(nr as usize / 64) >> (nr % 64) & 1 != 0
}

/// `struct seccomp_data`：编号、架构、svc 之后的 pc 和六个参数（第一个是原来的 x0）。
fn seccomp_data(nr: i32, pc: u64, args: [u64; 6]) -> [u8; DATA_SIZE] {
    let mut d = [0u8; DATA_SIZE];
    d[..4].copy_from_slice(&nr.to_le_bytes());
    d[4..8].copy_from_slice(&AUDIT_ARCH_AARCH64.to_le_bytes());
    d[8..16].copy_from_slice(&pc.to_le_bytes());
    for (i, a) in args.iter().enumerate() {
        d[16 + i * 8..24 + i * 8].copy_from_slice(&a.to_le_bytes());
    }
    d
}

/// 一个装好的过滤器（Linux 的 `struct seccomp_filter`），装好之后不再变（通知 fd 那一头
/// 和用户数除外）。
pub(crate) struct Filter {
    /// 检查、改写过的程序。
    prog: Box<[Insn]>,
    /// Linux 转换成 eBPF 后的长度（链容量计数用）。
    converted_len: usize,
    /// 按编号缓存（Linux 的 `seccomp_cache`）：这条链（这个过滤器和它往上的）对哪些编号总是
    /// 返回 ALLOW（[`cache`]）。
    allow: [u64; CACHE_WORDS],
    /// 原样的程序（PTRACE_SECCOMP_GET_FILTER 读的）。
    orig: Box<[u8]>,
    /// `SECCOMP_FILTER_FLAG_LOG`：除了 ALLOW 以外的动作都记一笔。
    log: bool,
    /// `SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV`：通知被取走之后，等回答时只被致命信号打断。
    wait_killable_recv: bool,
    /// 装它时线程原来的过滤器。
    prev: Option<Rc<Filter>>,
    /// 直接用着它的线程，加上以它为 `prev` 的、还有用户的过滤器（Linux 的 `users`）：降到 0
    /// 之后通知 fd 报告 POLLHUP。
    users: Cell<usize>,
    /// `SECCOMP_FILTER_FLAG_NEW_LISTENER` 时通知 fd 那一头的状态；没有通知 fd、或者它关闭了
    /// 时是 None，这时 USER_NOTIF 的系统调用返回 ENOSYS。
    notif: RefCell<Option<Notif>>,
}

impl Filter {
    /// 通知 fd 的唤醒对象：来了通知、通知被取走、没有用户了。
    fn object(&self) -> wake::Object {
        wake::of(&self.notif)
    }

    /// 这条链对编号 `nr` 总是返回 ALLOW。
    fn always_allows(&self, nr: u64) -> bool {
        cached(|i| self.allow[i], nr)
    }

    /// 从自己往上的一条链。
    fn chain(&self) -> impl Iterator<Item = &Filter> {
        std::iter::successors(Some(self), |f| f.prev.as_deref())
    }
}

/// 线程用着的过滤器链，[`Chain::filter`] 是最新装的那个。持有它算作那个过滤器的一个用户：
/// 复制时加一，丢掉时减一，减到 0 的过滤器不再是它的 `prev` 的用户，依次往上（Linux 的
/// `__seccomp_filter_orphan`）。
pub(crate) struct Chain(Option<Rc<Filter>>);

impl Chain {
    fn filter(&self) -> &Rc<Filter> {
        self.0
            .as_ref()
            .expect("a chain holds a filter until it is dropped")
    }

    /// 交出过滤器，不减它的用户数：它成为新过滤器的 `prev`，这个用户转给了新过滤器。
    fn into_filter(mut self) -> Rc<Filter> {
        self.0
            .take()
            .expect("a chain holds a filter until it is dropped")
    }
}

impl Clone for Chain {
    fn clone(&self) -> Chain {
        let f = self.filter();
        f.users.set(f.users.get() + 1);
        Chain(Some(f.clone()))
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        let Some(top) = self.0.take() else {
            return;
        };
        for f in top.chain() {
            let users = f.users.get() - 1;
            f.users.set(users);
            if users != 0 {
                break;
            }
            if let Some(n) = f.notif.borrow_mut().as_mut() {
                n.arrivals[0] += 1;
                request_wake_object(f.object());
            }
        }
    }
}

/// 线程的 seccomp 状态（[`Task::seccomp`]），只在持有 BKL 时访问。clone、fork 时复制。
#[derive(Clone, Default)]
pub(crate) struct Seccomp {
    /// [`mode`]。
    pub mode: u32,
    /// 过滤模式时的过滤器链。
    chain: Option<Chain>,
}

impl Seccomp {
    /// 链上的过滤器个数（`/proc/<pid>/status` 的 `Seccomp_filters`）。
    pub fn filters(&self) -> usize {
        self.chain
            .as_ref()
            .map_or(0, |c| c.filter().chain().count())
    }
}

/// 通知 fd 那一头（Linux 的 `struct notification`）。
struct Notif {
    next_id: u64,
    /// 还没被 RECV 取走的通知个数（Linux 的信号量）。发通知的线程被打断、拿掉了自己的
    /// 通知时不减，下一次 RECV 因此可能是 ENOENT，与 Linux 相同。
    requests: u64,
    /// 正在等回答的通知，按先后。
    list: Vec<Rc<RefCell<Knotif>>>,
    /// 读、写两个方向来过几次事件（`EPOLLET` 的数据代数）：新通知和没有用户了算读的
    /// 方向，通知被取走算写的方向。
    arrivals: [u64; 2],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// 还没被 RECV 取走。
    Init,
    /// 取走了，等回答。
    Sent,
    /// 回答了（或者通知 fd 关闭了）。
    Replied,
}

/// 一个通知（Linux 的 `struct seccomp_knotif`）：发出它的线程等着它被回答。
struct Knotif {
    id: u64,
    tid: Tid,
    /// 发出它的线程的事件：回答时叫醒它。
    event: Arc<Event>,
    state: State,
    data: [u8; DATA_SIZE],
    error: i32,
    val: i64,
    flags: u32,
}

/// 通知 fd 的打开文件里的东西（[`FileKind::SeccompNotify`]）：最后一个引用关闭时，等着
/// 回答的通知都得到 ENOSYS，之后 USER_NOTIF 的系统调用也是 ENOSYS（Linux 的
/// `seccomp_notify_detach`）。
pub(crate) struct Listener(Rc<Filter>);

impl Listener {
    /// 通知所属的过滤器。ioctl 拿它的 Rc，等通知时不借着打开文件（`syscall::fs::ioctl`）。
    pub(crate) fn filter(&self) -> Rc<Filter> {
        self.0.clone()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let Some(n) = self.0.notif.borrow_mut().take() else {
            return;
        };
        for k in n.list {
            let mut k = k.borrow_mut();
            if k.state != State::Replied {
                k.state = State::Replied;
                k.error = -(Errno::ENOSYS.0);
                k.val = 0;
                signal_later(&k.event);
            }
        }
    }
}

impl Listener {
    /// 就绪的事件：有没取走的通知可读，有取走了等回答的可写，没有用户了是 POLLHUP。
    pub fn readiness(&self) -> i16 {
        // poll(2) 的事件位
        const POLLIN: i16 = 0x001;
        const POLLOUT: i16 = 0x004;
        const POLLHUP: i16 = 0x010;
        const POLLRDNORM: i16 = 0x040;
        const POLLWRNORM: i16 = 0x100;
        let mut ev = 0;
        if let Some(n) = self.0.notif.borrow().as_ref() {
            for k in &n.list {
                match k.borrow().state {
                    State::Init => ev |= POLLIN | POLLRDNORM,
                    State::Sent => ev |= POLLOUT | POLLWRNORM,
                    State::Replied => {}
                }
            }
        }
        if self.0.users.get() == 0 {
            ev |= POLLHUP;
        }
        ev
    }

    /// 读、写两个方向来过几次事件（[`Notif::arrivals`]）。
    pub fn arrivals(&self) -> [u64; 2] {
        self.0
            .notif
            .borrow()
            .as_ref()
            .map_or([0, 0], |n| n.arrivals)
    }

    /// 唤醒对象（[`Filter::object`]）。
    pub fn object(&self) -> wake::Object {
        self.0.object()
    }
}

/// 系统调用入口的检查结果。
pub(crate) enum Verdict {
    /// 照常执行编号是这个的系统调用（跟踪者可能改了编号）。
    Run(u64),
    /// 不执行，x0 已经是返回值（或者已经挂起了 SIGSYS）。
    Skip,
    /// 线程结束了。
    Exit,
}

/// 系统调用的入口（在 PTRACE_SYSCALL 的入口停住之后）：按线程的 seccomp 模式检查编号
/// 为 `nr` 的系统调用。
pub(crate) fn check(cx: &mut Cx<'_>, cpu: &mut Cpu, nr: u64) -> Verdict {
    let (mode, top) = {
        let s = cx.task.seccomp.borrow();
        (s.mode, s.chain.as_ref().map(|c| c.filter().clone()))
    };
    // PTRACE_O_SUSPEND_SECCOMP（checkpoint/restore 用）；vDSO 里读时钟的存根在 Linux 上不进内核
    let suspended = {
        let t = cx.task.trace.borrow();
        t.tracer.is_some() && t.options & crate::ptrace::opt::SUSPEND_SECCOMP != 0
    };
    if mode == mode::DISABLED || suspended || crate::vdso::clock_stub(cx, cpu.state().pc, nr) {
        return Verdict::Run(nr);
    }
    match (mode, top) {
        (mode::FILTER, Some(top)) => filter(cx, cpu, &top, nr, false),
        (mode::STRICT, _) if STRICT_ALLOWED.contains(&nr) => Verdict::Run(nr),
        (mode::STRICT, _) => {
            log(cx, nr, Signal::KILL.number(), ret::KILL_THREAD, true);
            cx.task.seccomp.borrow_mut().mode = mode::DEAD;
            publish(cx.task);
            exit_thread(cx, cpu, Signal::KILL)
        }
        // 已经被要求结束的线程又进了内核（Linux 也不应该走到这里）
        _ => exit_thread(cx, cpu, Signal::KILL),
    }
}

/// 只结束这个线程，结束方式是被信号 `sig` 终止（Linux 的 `do_exit(sig)`）。
fn exit_thread(cx: &mut Cx<'_>, cpu: &mut Cpu, sig: Signal) -> Verdict {
    let status = ExitStatus::Signaled {
        signal: sig,
        pc: None,
        fault_addr: None,
        core_dumped: false,
    };
    cx.task.exit_status.set(status);
    if cx.tid() == cx.tgid() {
        cx.t.group.borrow_mut().leader_status = status;
    }
    crate::ptrace::exit(cx, cpu, status);
    Verdict::Exit
}

/// SIGSYS 的 siginfo：`si_errno` 是过滤器给的数据，`si_call_addr` 是 svc 之后的 pc。
fn sigsys(cpu: &Cpu, nr: u64, data: u32) -> SigInfo {
    SigInfo {
        code: SYS_SECCOMP,
        fields: InfoFields::Sys {
            errno: data as i32,
            call_addr: cpu.state().pc,
            syscall: nr as i32,
            arch: AUDIT_ARCH_AARCH64,
        },
        esr: 0,
    }
}

/// 过滤模式：在链上运行过滤器，执行结果的动作（Linux 的 `__seccomp_filter`）。`recheck`
/// 是跟踪者处理过 TRACE 之后的重新检查：这次 TRACE 当作允许。
fn filter(cx: &mut Cx<'_>, cpu: &mut Cpu, top: &Rc<Filter>, nr: u64, recheck: bool) -> Verdict {
    // 缓存说总是 ALLOW 的不运行程序（Linux 的 seccomp_cache_check_allow）
    if top.always_allows(nr) {
        return Verdict::Run(nr);
    }
    let data = {
        let s = cpu.state();
        let args = [s.orig_x0, s.x[1], s.x[2], s.x[3], s.x[4], s.x[5]];
        seccomp_data(nr as i32, s.pc, args)
    };
    let (r, matched) = run_filters(top, &data);
    let (action, value) = (r & ret::ACTION_FULL, r & ret::DATA);
    // 不执行这个系统调用：过滤器设了 SECCOMP_FILTER_FLAG_LOG 的才记
    let requested = matched.as_ref().is_some_and(|m| m.log);
    let skip = |cx: &Cx<'_>| {
        log(cx, nr, 0, action, requested);
        Verdict::Skip
    };
    match action {
        ret::ERRNO => {
            cpu.state_mut().x[0] = (-i64::from(value.min(MAX_ERRNO))) as u64;
            skip(cx)
        }
        ret::TRAP => {
            // 处理函数看到的是原来的寄存器
            cpu.state_mut().x[0] = cpu.state().orig_x0;
            signal::force(cx, Signal::SYS, sigsys(cpu, nr, value));
            skip(cx)
        }
        ret::TRACE if recheck => Verdict::Run(nr),
        ret::TRACE => {
            if !cx
                .task
                .trace
                .borrow()
                .enabled(crate::ptrace::event::SECCOMP)
            {
                cpu.state_mut().x[0] = Errno::ENOSYS.to_syscall_return();
                return skip(cx);
            }
            // 跟踪者可以改编号（-1 是跳过）和参数，之后重新检查
            let Some(nr) = crate::ptrace::seccomp_stop(cx, cpu, nr, value) else {
                return Verdict::Exit;
            };
            if nr as i64 >= 0 {
                filter(cx, cpu, top, nr, true)
            } else {
                skip(cx)
            }
        }
        ret::USER_NOTIF => match matched {
            Some(m) => notify(cx, cpu, &m, data, nr),
            None => Verdict::Run(nr),
        },
        ret::LOG => {
            log(cx, nr, 0, action, true);
            Verdict::Run(nr)
        }
        ret::ALLOW => Verdict::Run(nr),
        // KILL_PROCESS、KILL_THREAD 和不认得的动作：结束。只剩这一个线程时与结束进程一样，
        // 投递不能处理、不能被跟踪者拦下的 SIGSYS，写 core
        _ => {
            log(cx, nr, Signal::SYS.number(), action, true);
            cx.task.seccomp.borrow_mut().mode = mode::DEAD;
            publish(cx.task);
            if action != ret::KILL_THREAD || cx.t.group.borrow().threads.len() == 1 {
                cpu.state_mut().x[0] = cpu.state().orig_x0;
                signal::force_fatal(cx, Signal::SYS, sigsys(cpu, nr, value));
                Verdict::Skip
            } else {
                exit_thread(cx, cpu, Signal::SYS)
            }
        }
    }
}

/// 运行链上的每一个过滤器，取动作最严格的（按有符号数最小的，同样严格时取最新的）结果，
/// 以及给出它的过滤器（Linux 的 `seccomp_run_filters`）。
fn run_filters(top: &Rc<Filter>, data: &[u8; DATA_SIZE]) -> (u32, Option<Rc<Filter>>) {
    let action = |r: u32| (r & ret::ACTION_FULL) as i32;
    let mut best = (ret::ALLOW, None);
    let mut f = Some(top);
    while let Some(cur) = f {
        let r = run(&cur.prog, data);
        if action(r) < action(best.0) {
            best = (r, Some(cur.clone()));
        }
        f = cur.prev.as_ref();
    }
    best
}

/// USER_NOTIF：给通知 fd 那头发一个通知，等它回答（Linux 的 `seccomp_do_user_notification`）。
/// 没有通知 fd 时返回 ENOSYS。被信号打断时返回 ERESTARTSYS，重新执行时再发一个新的通知；
/// 设了 `WAIT_KILLABLE_RECV` 的，通知被取走之后只被致命信号打断。
fn notify(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    filter: &Rc<Filter>,
    data: [u8; DATA_SIZE],
    nr: u64,
) -> Verdict {
    let knotif = {
        let mut slot = filter.notif.borrow_mut();
        let Some(n) = slot.as_mut() else {
            cpu.state_mut().x[0] = Errno::ENOSYS.to_syscall_return();
            return Verdict::Skip;
        };
        let k = Rc::new(RefCell::new(Knotif {
            id: n.next_id,
            tid: cx.tid(),
            event: cx.task.event.clone(),
            state: State::Init,
            data,
            error: 0,
            val: 0,
            flags: 0,
        }));
        n.next_id = n.next_id.wrapping_add(1);
        n.list.push(k.clone());
        n.requests += 1;
        n.arrivals[0] += 1;
        k
    };
    request_wake_object(filter.object());
    let outcome = loop {
        let state = knotif.borrow().state;
        if state == State::Replied {
            break Ok(());
        }
        if cx.task.killed.get() {
            break Err(None);
        }
        let killable = filter.wait_killable_recv && state == State::Sent;
        if !killable && cx.interrupted() {
            break Err(Some(Errno::ERESTARTSYS));
        }
        let _ = cx.wait_alone(None);
    };
    if let Some(n) = filter.notif.borrow_mut().as_mut() {
        n.list.retain(|k| !Rc::ptr_eq(k, &knotif));
    }
    let k = knotif.borrow();
    match outcome {
        Err(None) => Verdict::Exit,
        Err(Some(e)) => {
            cpu.state_mut().x[0] = e.to_syscall_return();
            Verdict::Skip
        }
        Ok(()) if k.flags & USER_NOTIF_FLAG_CONTINUE != 0 => Verdict::Run(nr),
        Ok(()) => {
            let v = if k.error != 0 {
                i64::from(k.error)
            } else {
                k.val
            };
            cpu.state_mut().x[0] = v as u64;
            Verdict::Skip
        }
    }
}

/// 记一笔（Linux 发 audit 记录，这里写调试日志）：结束、LOG 总记，TRAP、ERRNO、TRACE、
/// USER_NOTIF 只在过滤器设了 `SECCOMP_FILTER_FLAG_LOG` 时记，都要在 `actions_logged` 里。
fn log(cx: &Cx<'_>, nr: u64, sig: i32, action: u32, requested: bool) {
    let (bit, always) = match action {
        ret::ALLOW => return,
        ret::TRAP => (logged::TRAP, false),
        ret::ERRNO => (logged::ERRNO, false),
        ret::TRACE => (logged::TRACE, false),
        ret::USER_NOTIF => (logged::USER_NOTIF, false),
        ret::LOG => (logged::LOG, true),
        ret::KILL_THREAD => (logged::KILL_THREAD, true),
        _ => (logged::KILL_PROCESS, true),
    };
    if (always || requested) && cx.kr().seccomp_logged & bit != 0 {
        crate::debug::log(format_args!(
            "seccomp: pid {} tid {} syscall {nr} sig {sig} action {action:#x}",
            cx.tgid(),
            cx.tid(),
        ));
    }
}

/// `/proc/sys/kernel/seccomp/actions_logged` 的位（Linux 的 `SECCOMP_LOG_*`）。
pub(crate) mod logged {
    pub const KILL_PROCESS: u32 = 1;
    pub const KILL_THREAD: u32 = 2;
    pub const TRAP: u32 = 4;
    pub const ERRNO: u32 = 8;
    pub const TRACE: u32 = 16;
    pub const LOG: u32 = 32;
    pub const ALLOW: u32 = 64;
    pub const USER_NOTIF: u32 = 128;
    /// 默认记除了 ALLOW 以外的所有动作。
    pub const DEFAULT: u32 = KILL_PROCESS | KILL_THREAD | TRAP | ERRNO | USER_NOTIF | TRACE | LOG;
}

/// 动作的名字，按 `actions_avail` 的顺序。
const NAMES: [(&str, u32); 8] = [
    ("kill_process", logged::KILL_PROCESS),
    ("kill_thread", logged::KILL_THREAD),
    ("trap", logged::TRAP),
    ("errno", logged::ERRNO),
    ("user_notif", logged::USER_NOTIF),
    ("trace", logged::TRACE),
    ("log", logged::LOG),
    ("allow", logged::ALLOW),
];

/// `/proc/sys/kernel/seccomp/actions_avail`。
pub(crate) fn actions_avail() -> String {
    let names: Vec<_> = NAMES.iter().map(|(n, _)| *n).collect();
    format!("{}\n", names.join(" "))
}

/// `/proc/sys/kernel/seccomp/actions_logged`。
pub(crate) fn actions_logged(k: &Kernel) -> String {
    let names: Vec<_> = NAMES
        .iter()
        .filter(|(_, bit)| k.seccomp_logged & bit != 0)
        .map(|(n, _)| *n)
        .collect();
    format!("{}\n", names.join(" "))
}

/// 写 `actions_logged`：空格分开的名字（读到换行或者空的名字为止），不能有 allow，要求
/// `CAP_SYS_ADMIN`（Linux 的 `write_actions_logged`）。
pub(crate) fn set_actions_logged(
    k: &mut Kernel,
    cred: &crate::cred::Cred,
    data: &[u8],
) -> Result<(), Errno> {
    if !cred.capable(crate::cred::cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    let line = data.split(|&c| c == b'\n' || c == 0).next().unwrap_or(&[]);
    let mut bits = 0;
    for name in line.split(|&c| c == b' ') {
        if name.is_empty() {
            break;
        }
        let (_, bit) = NAMES
            .iter()
            .find(|(n, _)| n.as_bytes() == name)
            .ok_or(Errno::EINVAL)?;
        bits |= bit;
    }
    if bits & logged::ALLOW != 0 {
        return Err(Errno::EINVAL);
    }
    k.seccomp_logged = bits;
    Ok(())
}

/// seccomp(2)。
pub(crate) fn seccomp(cx: &mut Cx<'_>, op: u32, flags: u32, args: u64) -> Result<u64, Errno> {
    match op {
        SET_MODE_STRICT if flags != 0 || args != 0 => Err(Errno::EINVAL),
        SET_MODE_STRICT => set_strict(cx),
        SET_MODE_FILTER => set_filter(cx, flags, args),
        GET_ACTION_AVAIL if flags != 0 => Err(Errno::EINVAL),
        GET_ACTION_AVAIL => {
            let b: [u8; 4] = get(cx, args)?;
            match u32::from_le_bytes(b) {
                ret::KILL_PROCESS
                | ret::KILL_THREAD
                | ret::TRAP
                | ret::ERRNO
                | ret::USER_NOTIF
                | ret::TRACE
                | ret::LOG
                | ret::ALLOW => Ok(0),
                _ => Err(Errno::EOPNOTSUPP),
            }
        }
        GET_NOTIF_SIZES if flags != 0 => Err(Errno::EINVAL),
        GET_NOTIF_SIZES => {
            // seccomp_notif、seccomp_notif_resp、seccomp_data 的大小
            let mut b = [0u8; 6];
            b[..2].copy_from_slice(&80u16.to_le_bytes());
            b[2..4].copy_from_slice(&24u16.to_le_bytes());
            b[4..].copy_from_slice(&(DATA_SIZE as u16).to_le_bytes());
            cx.mm().write(UserSlice::new(args, 6), &b)?;
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// prctl(PR_SET_SECCOMP)：没有标志；严格模式不看 `filter`。
pub(crate) fn prctl_set(cx: &mut Cx<'_>, mode: u64, filter: u64) -> Result<u64, Errno> {
    match mode {
        1 => set_strict(cx),
        2 => set_filter(cx, 0, filter),
        _ => Err(Errno::EINVAL),
    }
}

fn get<const N: usize>(cx: &Cx<'_>, addr: u64) -> Result<[u8; N], Errno> {
    let mut b = [0u8; N];
    cx.mm().read(UserSlice::new(addr, N), &mut b)?;
    Ok(b)
}

/// 进入严格模式。已经是过滤模式的不能改。
fn set_strict(cx: &mut Cx<'_>) -> Result<u64, Errno> {
    let mut s = cx.task.seccomp.borrow_mut();
    if s.mode != mode::DISABLED && s.mode != mode::STRICT {
        return Err(Errno::EINVAL);
    }
    s.mode = mode::STRICT;
    drop(s);
    publish(cx.task);
    Ok(0)
}

/// 装一个过滤器（Linux 的 `seccomp_set_mode_filter`）：检查标志和程序，要求
/// `no_new_privs` 或者 `CAP_SYS_ADMIN`；`NEW_LISTENER` 时返回通知 fd，链上已经有通知 fd 时是
/// EBUSY；`TSYNC` 时同一线程组的其他线程都要能同步过来，否则返回第一个不能的线程的 tid
/// （`TSYNC_ESRCH` 时是 ESRCH）。
fn set_filter(cx: &mut Cx<'_>, flags: u32, args: u64) -> Result<u64, Errno> {
    if flags & !flag::MASK != 0 {
        return Err(Errno::EINVAL);
    }
    // 两个都设时，成功返回的 fd 和失败返回的 tid 分不开，除非不要 tid
    if flags & flag::TSYNC != 0 && flags & flag::NEW_LISTENER != 0 && flags & flag::TSYNC_ESRCH == 0
    {
        return Err(Errno::EINVAL);
    }
    if flags & flag::WAIT_KILLABLE_RECV != 0 && flags & flag::NEW_LISTENER == 0 {
        return Err(Errno::EINVAL);
    }
    // struct sock_fprog：长度和指令的地址
    let b: [u8; 16] = get(cx, args)?;
    let len = u16::from_le_bytes([b[0], b[1]]) as usize;
    let addr = u64::from_le_bytes(b[8..].try_into().unwrap());
    if len == 0 || len > MAX_INSNS {
        return Err(Errno::EINVAL);
    }
    {
        let g = cx.t.group.borrow();
        if !g.no_new_privs && !g.cred.capable(crate::cred::cap::SYS_ADMIN) {
            return Err(Errno::EACCES);
        }
    }
    if addr == 0 {
        return Err(Errno::EINVAL);
    }
    let mut orig = vec![0u8; len * 8];
    cx.mm().read(UserSlice::new(addr, orig.len()), &mut orig)?;
    let mut prog: Vec<Insn> = orig.chunks_exact(8).map(Insn::decode).collect();
    check_classic(&prog)?;
    check_seccomp(&mut prog)?;

    // 通知 fd 的位子先找好：装上过滤器之后就不会失败（Linux 先 get_unused_fd_flags）
    let listener = flags & flag::NEW_LISTENER != 0;
    let fd = if listener {
        Some(cx.fds().free_slot(0)?)
    } else {
        None
    };
    let (mode, prev) = {
        let s = cx.task.seccomp.borrow();
        (s.mode, s.chain.as_ref().map(|c| c.filter().clone()))
    };
    if mode != mode::DISABLED && mode != mode::FILTER {
        return Err(Errno::EINVAL);
    }
    if listener
        && prev
            .iter()
            .any(|p| p.chain().any(|f| f.notif.borrow().is_some()))
    {
        return Err(Errno::EBUSY);
    }
    let converted_len = converted_len(&prog);
    let total = converted_len
        + prev
            .iter()
            .flat_map(|p| p.chain())
            .map(|f| f.converted_len + 4)
            .sum::<usize>();
    if total > MAX_INSNS_PER_PATH {
        return Err(Errno::ENOMEM);
    }
    // TSYNC：其他线程要么没有 seccomp，要么用的过滤器是自己现在的这条链上的
    let others: Vec<Rc<Task>> = {
        let g = cx.t.group.borrow();
        let me = cx.tid();
        g.threads
            .iter()
            .filter(|&&t| t != me)
            .filter_map(|t| cx.kr().tasks.get(t).cloned())
            .collect()
    };
    if flags & flag::TSYNC != 0 {
        let unsyncable = others.iter().find(|t| {
            let s = t.seccomp.borrow();
            match (s.mode, &s.chain) {
                (mode::DISABLED, _) => false,
                (mode::FILTER, Some(c)) => !prev
                    .as_ref()
                    .is_some_and(|p| p.chain().any(|f| std::ptr::eq(f, &**c.filter()))),
                _ => true,
            }
        });
        if let Some(t) = unsyncable {
            if flags & flag::TSYNC_ESRCH != 0 {
                return Err(Errno::ESRCH);
            }
            return Ok(t.tid.get() as u64);
        }
    }
    let allow = cache(&prog, prev.as_ref().map(|p| &p.allow));
    let filter = Rc::new(Filter {
        converted_len,
        allow,
        prog: prog.into_boxed_slice(),
        orig: orig.into_boxed_slice(),
        log: flags & flag::LOG != 0,
        wait_killable_recv: flags & flag::WAIT_KILLABLE_RECV != 0,
        prev: cx
            .task
            .seccomp
            .borrow_mut()
            .chain
            .take()
            .map(Chain::into_filter),
        users: Cell::new(1),
        notif: RefCell::new(listener.then(|| Notif {
            next_id: random_id(),
            requests: 0,
            list: Vec::new(),
            arrivals: [0, 0],
        })),
    });
    let chain = Chain(Some(filter.clone()));
    if flags & flag::TSYNC != 0 {
        for t in &others {
            {
                let mut s = t.seccomp.borrow_mut();
                s.mode = mode::FILTER;
                // 换下来的那条在 drop 时减掉它的用户数
                s.chain = Some(chain.clone());
            }
            publish(t);
        }
    }
    {
        let mut s = cx.task.seccomp.borrow_mut();
        s.mode = mode::FILTER;
        s.chain = Some(chain);
    }
    publish(cx.task);
    match fd {
        Some(fd) => {
            let file = OpenFile::new(FileKind::SeccompNotify(Listener(filter)), oflags::O_RDWR);
            cx.fds_mut().install_at(fd, file, true)?;
            Ok(fd as u64)
        }
        None => Ok(0),
    }
}

/// 线程的 seccomp 变了（装了过滤器、TSYNC、clone、要结束它）：更新不持 BKL 时看的
/// [`Task::seccomp_on`] 和放行位图 [`Task::seccomp_allow`]。只有过滤模式有位图。
pub(crate) fn publish(task: &Task) {
    let s = task.seccomp.borrow();
    let allow = match (s.mode, &s.chain) {
        (mode::FILTER, Some(c)) => c.filter().allow,
        _ => [0; CACHE_WORDS],
    };
    for (w, bits) in task.seccomp_allow.iter().zip(allow) {
        w.store(bits, Ordering::Relaxed);
    }
    task.seccomp_on
        .store(s.mode != mode::DISABLED, Ordering::Relaxed);
}

/// 不持 BKL 时：线程的过滤器对编号 `nr` 总是返回 ALLOW（[`publish`] 的位图）。别的线程 TSYNC
/// 时可能正在改它，读到的旧位是同步之前那条链的（新链只会更严），相当于这个系统调用发生在
/// 同步之前。
pub(crate) fn always_allowed(task: &Task, nr: u64) -> bool {
    cached(|i| task.seccomp_allow[i].load(Ordering::Relaxed), nr)
}

/// 通知的 id 从一个随机数开始（与 Linux 一样，让监管进程不能猜别的通知）。
fn random_id() -> u64 {
    let mut b = [0u8; 8];
    let _ = appine_hal::fill_random(&mut b);
    u64::from_le_bytes(b)
}

/// 通知 fd 的 ioctl。
pub(crate) fn ioctl(cx: &mut Cx<'_>, f: &Filter, request: u32, arg: u64) -> Result<u64, Errno> {
    match request {
        IOCTL_NOTIF_RECV => recv(cx, f, arg),
        IOCTL_NOTIF_SEND => send(cx, f, arg),
        IOCTL_NOTIF_ID_VALID | IOCTL_NOTIF_ID_VALID_WRONG_DIR => {
            let id = u64::from_le_bytes(get(cx, arg)?);
            match find(f, id) {
                Some(k) if k.borrow().state == State::Sent => Ok(0),
                _ => Err(Errno::ENOENT),
            }
        }
        r if r & !IOC_DIR_SIZE_MASK == IOCTL_NOTIF_ADDFD & !IOC_DIR_SIZE_MASK => {
            addfd(cx, f, arg, (r >> 16 & 0x3fff) as usize)
        }
        _ => Err(Errno::EINVAL),
    }
}

fn find(f: &Filter, id: u64) -> Option<Rc<RefCell<Knotif>>> {
    let n = f.notif.borrow();
    n.as_ref()?
        .list
        .iter()
        .find(|k| k.borrow().id == id)
        .cloned()
}

/// SECCOMP_IOCTL_NOTIF_RECV：等一个还没被取走的通知，取走它（`struct seccomp_notif` 要
/// 先清零）。与 6.1 一样，被信号打断是 EINTR，fd 设了 O_NONBLOCK 也照样等。
fn recv(cx: &mut Cx<'_>, f: &Filter, arg: u64) -> Result<u64, Errno> {
    let mut b = [0u8; 80];
    cx.mm().read(UserSlice::new(arg, b.len()), &mut b)?;
    if b.iter().any(|&c| c != 0) {
        return Err(Errno::EINVAL);
    }
    let knotif = loop {
        let got = {
            let mut slot = f.notif.borrow_mut();
            let n = slot.as_mut().ok_or(Errno::ENOENT)?;
            if n.requests > 0 {
                n.requests -= 1;
                let k = n
                    .list
                    .iter()
                    .find(|k| k.borrow().state == State::Init)
                    .cloned();
                if k.is_some() {
                    n.arrivals[1] += 1;
                }
                Some(k)
            } else {
                None
            }
        };
        match got {
            // 发出它的线程被打断、拿掉了它
            Some(None) => return Err(Errno::ENOENT),
            Some(Some(k)) => break k,
            None => {
                if cx.interrupted() {
                    return Err(Errno::EINTR);
                }
                cx.wait_objects(None, &mut [], wake::FILE, &[f.object()])?;
            }
        }
    };
    knotif.borrow_mut().state = State::Sent;
    request_wake_object(f.object());
    let (id, tid, data) = {
        let k = knotif.borrow();
        (k.id, k.tid, k.data)
    };
    b[..8].copy_from_slice(&id.to_le_bytes());
    b[8..12].copy_from_slice(&tid.to_le_bytes());
    b[16..].copy_from_slice(&data);
    if cx.mm().write(UserSlice::new(arg, b.len()), &b).is_err() {
        // 写不回去：通知放回去，发出它的线程不至于一直等（只等致命信号的叫醒它重新看）
        let mut slot = f.notif.borrow_mut();
        if let Some(n) = slot.as_mut()
            && n.list.iter().any(|k| Rc::ptr_eq(k, &knotif))
        {
            let mut k = knotif.borrow_mut();
            if f.wait_killable_recv && k.state == State::Sent {
                signal_later(&k.event);
            }
            k.state = State::Init;
            n.requests += 1;
            n.arrivals[0] += 1;
            request_wake_object(f.object());
        }
        return Err(Errno::EFAULT);
    }
    Ok(0)
}

/// SECCOMP_IOCTL_NOTIF_SEND：回答一个取走了的通知（`struct seccomp_notif_resp`）。
fn send(cx: &mut Cx<'_>, f: &Filter, arg: u64) -> Result<u64, Errno> {
    let b: [u8; 24] = get(cx, arg)?;
    let id = u64::from_le_bytes(b[..8].try_into().unwrap());
    let val = i64::from_le_bytes(b[8..16].try_into().unwrap());
    let error = i32::from_le_bytes(b[16..20].try_into().unwrap());
    let flags = u32::from_le_bytes(b[20..24].try_into().unwrap());
    if flags & !USER_NOTIF_FLAG_CONTINUE != 0
        || (flags & USER_NOTIF_FLAG_CONTINUE != 0 && (error != 0 || val != 0))
    {
        return Err(Errno::EINVAL);
    }
    let knotif = find(f, id).ok_or(Errno::ENOENT)?;
    let mut k = knotif.borrow_mut();
    // 只能回答一次
    if k.state != State::Sent {
        return Err(Errno::EINPROGRESS);
    }
    k.state = State::Replied;
    k.error = error;
    k.val = val;
    k.flags = flags;
    signal_later(&k.event);
    Ok(0)
}

/// SECCOMP_IOCTL_NOTIF_ADDFD：把自己的 fd `srcfd` 装进发出通知的线程的 fd 表（`SETFD` 时
/// 装在 `newfd` 上，像 dup2），返回那边的 fd；`SEND` 时同时用它回答通知。Linux 由那个线程
/// 自己去装；这里持有 BKL 直接装，效果相同，它被打断的那一刻也不会失败（Linux 返回 ESRCH）。
fn addfd(cx: &mut Cx<'_>, f: &Filter, arg: u64, size: usize) -> Result<u64, Errno> {
    if !(ADDFD_SIZE_VER0..appine_engine::PAGE_SIZE as usize).contains(&size) {
        return Err(Errno::EINVAL);
    }
    let mut b = vec![0u8; size];
    cx.mm().read(UserSlice::new(arg, size), &mut b)?;
    // 比现在的结构体长的部分要是零（copy_struct_from_user）
    if b[ADDFD_SIZE_VER0..].iter().any(|&c| c != 0) {
        return Err(Errno::E2BIG);
    }
    let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
    let id = u64::from_le_bytes(b[..8].try_into().unwrap());
    let (flags, srcfd, newfd, newfd_flags) = (u32_at(8), u32_at(12), u32_at(16), u32_at(20));
    if newfd_flags & !oflags::O_CLOEXEC != 0
        || flags & !(ADDFD_FLAG_SETFD | ADDFD_FLAG_SEND) != 0
        || (newfd != 0 && flags & ADDFD_FLAG_SETFD == 0)
    {
        return Err(Errno::EINVAL);
    }
    let file = cx.fds().get(srcfd as i32)?;
    let knotif = find(f, id).ok_or(Errno::ENOENT)?;
    if knotif.borrow().state != State::Sent {
        return Err(Errno::EINPROGRESS);
    }
    let tid = knotif.borrow().tid;
    let task = cx.kr().tasks.get(&tid).cloned().ok_or(Errno::ENOENT)?;
    let fds = task.res.borrow().fds.clone();
    let cloexec = newfd_flags & oflags::O_CLOEXEC != 0;
    let installed = if flags & ADDFD_FLAG_SETFD != 0 {
        let replaced = fds.borrow_mut().install_at(newfd as i32, file, cloexec);
        match replaced {
            Ok(old) => {
                let old: Vec<_> = old.into_iter().collect();
                crate::locks::closed(cx.k(), &fds, &old);
                crate::unix::released(cx.k(), old);
                Ok(newfd as i32)
            }
            Err(e) => Err(e),
        }
    } else {
        fds.borrow_mut().install(file, cloexec, 0)
    };
    let fd = installed?;
    if flags & ADDFD_FLAG_SEND != 0 {
        let mut k = knotif.borrow_mut();
        k.state = State::Replied;
        k.flags = 0;
        k.error = 0;
        k.val = i64::from(fd);
        signal_later(&k.event);
    }
    Ok(fd as u64)
}

/// PTRACE_SECCOMP_GET_FILTER：被跟踪的线程的第 `index` 个过滤器（0 是最早装的）的原样的
/// 程序，写到 `data`（为 0 时只返回指令数）。跟踪者要有 `CAP_SYS_ADMIN`、自己没有 seccomp。
pub(crate) fn get_filter(cx: &Cx<'_>, task: &Task, index: u64, data: u64) -> Result<u64, Errno> {
    let f = nth_filter(cx, task, index)?;
    if data != 0 {
        cx.mm().write(UserSlice::new(data, f.orig.len()), &f.orig)?;
    }
    Ok((f.orig.len() / 8) as u64)
}

/// PTRACE_SECCOMP_GET_METADATA：`data` 是 `struct seccomp_metadata`，跟踪者给出
/// `filter_off`，得到那个过滤器的标志（只有 `SECCOMP_FILTER_FLAG_LOG`）。返回写了多少字节。
pub(crate) fn get_metadata(cx: &Cx<'_>, task: &Task, size: u64, data: u64) -> Result<u64, Errno> {
    privileged_tracer(cx)?;
    let size = size.min(16) as usize;
    if size < 8 {
        return Err(Errno::EINVAL);
    }
    let index = u64::from_le_bytes(get(cx, data)?);
    let f = nth_filter(cx, task, index)?;
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&index.to_le_bytes());
    if f.log {
        b[8..].copy_from_slice(&u64::from(flag::LOG).to_le_bytes());
    }
    cx.mm().write(UserSlice::new(data, size), &b[..size])?;
    Ok(size as u64)
}

fn privileged_tracer(cx: &Cx<'_>) -> Result<(), Errno> {
    if !cx.cred().capable(crate::cred::cap::SYS_ADMIN)
        || cx.task.seccomp.borrow().mode != mode::DISABLED
    {
        return Err(Errno::EACCES);
    }
    Ok(())
}

fn nth_filter(cx: &Cx<'_>, task: &Task, index: u64) -> Result<Rc<Filter>, Errno> {
    privileged_tracer(cx)?;
    let s = task.seccomp.borrow();
    let top = match (s.mode, &s.chain) {
        (mode::FILTER, Some(c)) => c.filter(),
        _ => return Err(Errno::EINVAL),
    };
    let all: Vec<&Filter> = top.chain().collect();
    let i = usize::try_from(index)
        .ok()
        .filter(|&i| i < all.len())
        .ok_or(Errno::ENOENT)?;
    // 链从最新的开始，下标从最早的开始
    let mut f = top.clone();
    for _ in 0..all.len() - 1 - i {
        let prev = f.prev.clone().expect("the chain is that long");
        f = prev;
    }
    Ok(f)
}

/// 线程结束了：不是主线程的马上放下过滤器链；主线程的交给线程组，进程被回收时才放下（Linux
/// 在 `release_task` 时放下，僵尸进程仍是用户，通知 fd 那时才 POLLHUP）。
pub(crate) fn exited(task: &Task, group: &mut crate::process::ThreadGroup) {
    let chain = task.seccomp.borrow_mut().chain.take();
    if task.tid.get() == group.tgid {
        group.leader_seccomp = chain;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insn(code: u16, jt: u8, jf: u8, k: u32) -> Insn {
        Insn { code, jt, jf, k }
    }

    fn prepared(mut prog: Vec<Insn>) -> Result<Vec<Insn>, Errno> {
        check_classic(&prog)?;
        check_seccomp(&mut prog)?;
        Ok(prog)
    }

    #[test]
    fn programs_are_checked_like_linux() {
        use bpf::*;
        let ret = insn(RET | K, 0, 0, ret::ALLOW);
        assert!(prepared(vec![ret]).is_ok());
        // 读编号、按 4 对齐、不越过 seccomp_data
        assert!(prepared(vec![insn(LD | W | ABS, 0, 0, 0), ret]).is_ok());
        assert_eq!(
            prepared(vec![insn(LD | W | ABS, 0, 0, 2), ret]),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            prepared(vec![insn(LD | W | ABS, 0, 0, 64), ret]),
            Err(Errno::EINVAL)
        );
        // 读半字、取模是 cBPF 认得、seccomp 不认的
        assert_eq!(
            prepared(vec![insn(LD | H | ABS, 0, 0, 0), ret]),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            prepared(vec![insn(ALU | MOD | K, 0, 0, 3), ret]),
            Err(Errno::EINVAL)
        );
        // 最后一条不是返回、跳出程序、除以常数 0、移位太多
        assert_eq!(prepared(vec![insn(LD | IMM, 0, 0, 1)]), Err(Errno::EINVAL));
        assert_eq!(
            prepared(vec![insn(JMP | JA, 0, 0, 1), ret]),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            prepared(vec![insn(JMP | JEQ | K, 1, 0, 0), ret]),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            prepared(vec![insn(ALU | DIV | K, 0, 0, 0), ret]),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            prepared(vec![insn(ALU | LSH | K, 0, 0, 32), ret]),
            Err(Errno::EINVAL)
        );
        // 草稿内存先写后读：只在一条分支上写过的不能在汇合之后读
        let partial = vec![
            insn(JMP | JEQ | K, 0, 1, 0),
            insn(ST, 0, 0, 3),
            insn(LD | MEM, 0, 0, 3),
            ret,
        ];
        assert_eq!(prepared(partial), Err(Errno::EINVAL));
        let both = vec![
            insn(ST, 0, 0, 3),
            insn(JMP | JA, 0, 0, 0),
            insn(LD | MEM, 0, 0, 3),
            ret,
        ];
        assert!(prepared(both).is_ok());
        // 长度变成 seccomp_data 的大小
        let len = prepared(vec![insn(LD | W | LEN, 0, 0, 0), insn(RET | A, 0, 0, 0)]).unwrap();
        assert_eq!(run(&len, &[0; DATA_SIZE]), 64);
    }

    #[test]
    fn programs_run_like_the_jit() {
        use bpf::*;
        let data = seccomp_data(39, 0x1000, [7, 0, 0, 0, 0, 0]);
        // 编号是 39 时返回 ERRNO|3，否则 ALLOW
        let prog = prepared(vec![
            insn(LD | W | ABS, 0, 0, 0),
            insn(JMP | JEQ | K, 0, 1, 39),
            insn(RET | K, 0, 0, ret::ERRNO | 3),
            insn(RET | K, 0, 0, ret::ALLOW),
        ])
        .unwrap();
        assert_eq!(run(&prog, &data), ret::ERRNO | 3);
        // 第一个参数的低 32 位、架构
        let arg = prepared(vec![insn(LD | W | ABS, 0, 0, 16), insn(RET | A, 0, 0, 0)]).unwrap();
        assert_eq!(run(&arg, &data), 7);
        let arch = prepared(vec![insn(LD | W | ABS, 0, 0, 4), insn(RET | A, 0, 0, 0)]).unwrap();
        assert_eq!(run(&arch, &data), AUDIT_ARCH_AARCH64);
        // 除以 X 为 0 时返回 0（KILL_THREAD），移位按 32 取模
        let div = prepared(vec![
            insn(LD | IMM, 0, 0, 10),
            insn(ALU | DIV | X, 0, 0, 0),
            insn(RET | K, 0, 0, ret::ALLOW),
        ])
        .unwrap();
        assert_eq!(run(&div, &data), 0);
        let shift = prepared(vec![
            insn(LD | IMM, 0, 0, 1),
            insn(LDX | IMM, 0, 0, 33),
            insn(ALU | LSH | X, 0, 0, 0),
            insn(RET | A, 0, 0, 0),
        ])
        .unwrap();
        assert_eq!(run(&shift, &data), 2);
    }

    #[test]
    fn the_cache_keeps_only_constant_allows() {
        use bpf::*;
        let allowed = |allow: &[u64; CACHE_WORDS], nr: u64| cached(|i| allow[i], nr);
        // 先看架构，再按编号：98 返回 ERRNO，400 及以上 TRAP，别的 ALLOW
        let by_nr = prepared(vec![
            insn(LD | W | ABS, 0, 0, 4),
            insn(JMP | JEQ | K, 1, 0, AUDIT_ARCH_AARCH64),
            insn(RET | K, 0, 0, ret::KILL_PROCESS),
            insn(LD | W | ABS, 0, 0, 0),
            insn(JMP | JEQ | K, 2, 0, 98),
            insn(JMP | JGE | K, 2, 0, 400),
            insn(RET | K, 0, 0, ret::ALLOW),
            insn(RET | K, 0, 0, ret::ERRNO | 1),
            insn(RET | K, 0, 0, ret::TRAP),
        ])
        .unwrap();
        let c = cache(&by_nr, None);
        assert!(allowed(&c, 0) && allowed(&c, 97) && allowed(&c, 399));
        assert!(!allowed(&c, 98) && !allowed(&c, 400) && !allowed(&c, 511));
        // 编号以外的数据（这里是第一个参数）不是常数：98 要运行程序，别的仍是常数
        let by_arg = prepared(vec![
            insn(LD | W | ABS, 0, 0, 0),
            insn(JMP | JEQ | K, 0, 2, 98),
            insn(LD | W | ABS, 0, 0, 16),
            insn(JMP | JSET | K, 1, 0, 1),
            insn(RET | K, 0, 0, ret::ALLOW),
            insn(RET | K, 0, 0, ret::ERRNO | 1),
        ])
        .unwrap();
        let c = cache(&by_arg, None);
        assert!(!allowed(&c, 98) && allowed(&c, 99));
        // 按位与之后比较、无条件跳转
        let masked = prepared(vec![
            insn(LD | W | ABS, 0, 0, 0),
            insn(ALU | AND | K, 0, 0, 0xf),
            insn(JMP | JEQ | K, 1, 0, 3),
            insn(JMP | JA, 0, 0, 1),
            insn(RET | K, 0, 0, ret::LOG),
            insn(RET | K, 0, 0, ret::ALLOW),
        ])
        .unwrap();
        let c = cache(&masked, None);
        assert!(!allowed(&c, 3) && !allowed(&c, 19) && allowed(&c, 4));
        // 返回值要正好是 ALLOW；RET A、别的指令都不算
        let data = prepared(vec![insn(RET | K, 0, 0, ret::ALLOW | 1)]).unwrap();
        assert!(!allowed(&cache(&data, None), 0));
        let ret_a = prepared(vec![
            insn(LD | IMM, 0, 0, ret::ALLOW),
            insn(RET | A, 0, 0, 0),
        ])
        .unwrap();
        assert!(!allowed(&cache(&ret_a, None), 0));
        // 新的过滤器总是放行，也不放开上一个不放行的
        let all = prepared(vec![insn(RET | K, 0, 0, ret::ALLOW)]).unwrap();
        let c = cache(&all, Some(&cache(&by_nr, None)));
        assert!(!allowed(&c, 98) && allowed(&c, 97));
        // 位图之外的编号、负数都要运行程序
        assert!(
            !allowed(&cache(&all, None), CACHED_NRS as u64)
                && !allowed(&cache(&all, None), u64::MAX)
        );
    }
}
