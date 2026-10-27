//! ptrace(2) 里跟踪者的一方，以及 process_vm_readv、process_vm_writev（设计文档 §7.6）。
//! 被跟踪的一方在 [`crate::ptrace`]。

use std::rc::Rc;
use std::sync::atomic::Ordering;

use appine_engine::PAGE_SIZE;

use super::proc::may_ptrace;
use super::{SysResult, get, put};
use crate::errno::Errno;
use crate::kernel::{Cx, INIT};
use crate::process::Signal;
use crate::ptrace::{
    BRK_SLOTS, EVENTMSG_SYSCALL_ENTRY, EVENTMSG_SYSCALL_EXIT, HwDebug, Mode, PSTATE_NZCV, Regs,
    Stop, Trace, WATCH_SLOTS, event, opt,
};
use crate::signal::{self, SigInfo};
use crate::task::Task;
use crate::uaccess::UserSlice;

const TRACEME: i64 = 0;
const PEEKTEXT: i64 = 1;
const PEEKDATA: i64 = 2;
const POKETEXT: i64 = 4;
const POKEDATA: i64 = 5;
const CONT: i64 = 7;
const KILL: i64 = 8;
const SINGLESTEP: i64 = 9;
const ATTACH: i64 = 16;
const DETACH: i64 = 17;
const SYSCALL: i64 = 24;
const SETOPTIONS: i64 = 0x4200;
const GETEVENTMSG: i64 = 0x4201;
const GETSIGINFO: i64 = 0x4202;
const SETSIGINFO: i64 = 0x4203;
const GETREGSET: i64 = 0x4204;
const SETREGSET: i64 = 0x4205;
const SEIZE: i64 = 0x4206;
const INTERRUPT: i64 = 0x4207;
const LISTEN: i64 = 0x4208;
const GETSIGMASK: i64 = 0x420a;
const SETSIGMASK: i64 = 0x420b;
const SECCOMP_GET_FILTER: i64 = 0x420c;
const SECCOMP_GET_METADATA: i64 = 0x420d;
const GET_SYSCALL_INFO: i64 = 0x420e;

/// regset 的类型（`NT_*`）。
const NT_PRSTATUS: u64 = 1;
const NT_PRFPREG: u64 = 2;
const NT_ARM_TLS: u64 = 0x401;
const NT_ARM_HW_BREAK: u64 = 0x402;
const NT_ARM_HW_WATCH: u64 = 0x403;
const NT_ARM_SYSTEM_CALL: u64 = 0x404;

/// `AUDIT_ARCH_AARCH64`（PTRACE_GET_SYSCALL_INFO 的 arch）。
const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;
/// 调试架构（`ID_AA64DFR0_EL1.DebugVer`）是 ARMv8。
const DEBUG_ARCH_V8: u32 = 6;

pub(super) fn ptrace(cx: &mut Cx<'_>, request: i64, pid: i32, addr: u64, data: u64) -> SysResult {
    match request {
        TRACEME => return traceme(cx),
        ATTACH | SEIZE => return attach(cx, pid, request == SEIZE, addr, data),
        _ => {}
    }
    let task = cx.kr().tasks.get(&pid).cloned().ok_or(Errno::ESRCH)?;
    // 只能对自己跟踪的线程发请求；除了 PTRACE_KILL、PTRACE_INTERRUPT，它还得停在
    // ptrace-stop 里（Linux 的 ptrace_check_attach）
    {
        let t = task.trace.borrow();
        if t.tracer != Some(cx.tgid()) || !(matches!(request, KILL | INTERRUPT) || t.stopped()) {
            return Err(Errno::ESRCH);
        }
    }
    match request {
        PEEKTEXT | PEEKDATA => peek(cx, &task, addr, data),
        POKETEXT | POKEDATA => {
            // 与 Linux 的 FOLL_FORCE 一样可以写没有写权限的私有映射：下断点
            let mm = task.res.borrow().mm.clone();
            mm.borrow_mut()
                .write_force(addr, &data.to_le_bytes(), &cx.task.sync)
                .map_err(|_| Errno::EIO)?;
            Ok(0)
        }
        CONT => resume(&task, Mode::Cont, data),
        SYSCALL => resume(&task, Mode::Syscall, data),
        SINGLESTEP => resume(&task, Mode::Step, data),
        DETACH => {
            if data > Signal::MAX as u64 {
                return Err(Errno::EIO);
            }
            crate::ptrace::detach(&task, Some(data as i32));
            Ok(0)
        }
        KILL => {
            signal::send_to_task(cx.kr(), &task, Signal::KILL, SigInfo::kernel());
            Ok(0)
        }
        SETOPTIONS => {
            if data & !(opt::MASK as u64) != 0 {
                return Err(Errno::EINVAL);
            }
            check_options(cx, data as u32)?;
            task.trace.borrow_mut().options = data as u32;
            Ok(0)
        }
        GETEVENTMSG => {
            let message = task.trace.borrow().message;
            put(cx, data, message.to_le_bytes()).map(|()| 0)
        }
        GETSIGINFO => {
            let info = task.trace.borrow().siginfo.ok_or(Errno::EINVAL)?;
            cx.mm().write(UserSlice::new(data, 128), &info)?;
            Ok(0)
        }
        SETSIGINFO => {
            let mut info = [0u8; 128];
            cx.mm().read(UserSlice::new(data, 128), &mut info)?;
            let mut t = task.trace.borrow_mut();
            let slot = t.siginfo.as_mut().ok_or(Errno::EINVAL)?;
            *slot = info;
            Ok(0)
        }
        GETREGSET | SETREGSET => regset(cx, &task, request == SETREGSET, addr, data),
        INTERRUPT => {
            let mut t = task.trace.borrow_mut();
            if !t.seized {
                return Err(Errno::EIO);
            }
            // 在运行的线程马上进内核，在等待的从系统调用里出来（signal::interrupted），
            // 停着的在继续之后停；PTRACE_LISTEN 的马上重新报告
            t.trap = true;
            drop(t);
            signal::wake(&task);
            Ok(0)
        }
        LISTEN => {
            let mut t = task.trace.borrow_mut();
            if !t.seized || !matches!(t.stop, Some(Stop::Event(event::STOP, _))) {
                return Err(Errno::EIO);
            }
            t.listening = true;
            drop(t);
            // 停下来之后 group-stop 已经结束了的话，马上重新报告
            crate::kernel::signal_later(&task.event);
            Ok(0)
        }
        GETSIGMASK | SETSIGMASK => {
            if addr != 8 {
                return Err(Errno::EINVAL);
            }
            // 与 Linux 一样：sigsuspend、ppoll 换掉了屏蔽字时读到的是原来的那个；设置之后不再
            // 恢复原来的（clear_tsk_restore_sigmask），处理函数返回后是跟踪者设的
            if request == GETSIGMASK {
                let mask = task.saved_sigmask.get().unwrap_or(task.sigmask.get());
                put(cx, data, mask.to_le_bytes()).map(|()| 0)
            } else {
                let mask = u64::from_le_bytes(get(cx, data)?);
                task.sigmask.set(mask & !signal::UNBLOCKABLE);
                task.saved_sigmask.set(None);
                Ok(0)
            }
        }
        GET_SYSCALL_INFO => syscall_info(cx, &task, addr, data),
        SECCOMP_GET_FILTER => crate::seccomp::get_filter(cx, &task, addr, data),
        SECCOMP_GET_METADATA => crate::seccomp::get_metadata(cx, &task, addr, data),
        // PEEKUSR、POKEUSR、GETREGS 这些在 arm64 上本来就没有
        _ => Err(Errno::EIO),
    }
}

/// PTRACE_TRACEME：由父进程跟踪。父进程是内置的 init 时没有谁能跟踪它。
fn traceme(cx: &mut Cx<'_>) -> SysResult {
    let ppid = cx.t.group.borrow().ppid;
    if ppid == INIT || !cx.kr().groups.contains_key(&ppid) {
        return Err(Errno::EPERM);
    }
    let mut t = cx.task.trace.borrow_mut();
    if t.tracer.is_some() {
        return Err(Errno::EPERM);
    }
    t.tracer = Some(ppid);
    Ok(0)
}

/// `PTRACE_O_SUSPEND_SECCOMP`（checkpoint/restore 用）：要求 `CAP_SYS_ADMIN`，跟踪者自己
/// 没有 seccomp、也没有被这样跟踪着（Linux 的 `check_ptrace_options`）。
fn check_options(cx: &Cx<'_>, options: u32) -> Result<(), Errno> {
    if options & opt::SUSPEND_SECCOMP != 0 {
        let suspended = {
            let t = cx.task.trace.borrow();
            t.tracer.is_some() && t.options & opt::SUSPEND_SECCOMP != 0
        };
        if !cx.cred().capable(crate::cred::cap::SYS_ADMIN)
            || cx.task.seccomp.borrow().mode != crate::seccomp::mode::DISABLED
            || suspended
        {
            return Err(Errno::EPERM);
        }
    }
    Ok(())
}

/// PTRACE_ATTACH、PTRACE_SEIZE（`options` 在 `data` 里）。ATTACH 给线程发一个 SIGSTOP，
/// SEIZE 不打扰它。
fn attach(cx: &mut Cx<'_>, pid: i32, seize: bool, addr: u64, data: u64) -> SysResult {
    let options = if seize {
        // 与 Linux 一样，SEIZE 不认得的选项是 EIO，SETOPTIONS 的是 EINVAL
        if addr != 0 || data & !(opt::MASK as u64) != 0 {
            return Err(Errno::EIO);
        }
        check_options(cx, data as u32)?;
        data as u32
    } else {
        0
    };
    let task = cx.kr().tasks.get(&pid).cloned().ok_or(Errno::ESRCH)?;
    let group = cx
        .kr()
        .groups
        .get(&task.tgid)
        .cloned()
        .ok_or(Errno::ESRCH)?;
    if task.tgid == cx.tgid() || !may_ptrace(cx, &group.borrow()) {
        return Err(Errno::EPERM);
    }
    if group.borrow().exiting.is_some() || task.killed.get() {
        return Err(Errno::EPERM);
    }
    {
        let mut t = task.trace.borrow_mut();
        if t.tracer.is_some() {
            return Err(Errno::EPERM);
        }
        *t = Trace {
            tracer: Some(cx.tgid()),
            seized: seize,
            options,
            ..Trace::default()
        };
        // 线程组已经被停住了：马上作为 group-stop 停下来报告（Linux 的 JOBCTL_TRAP_STOP），
        // 报告的是停住它的信号，父进程取走了停止事件也一样
        let g = group.borrow();
        if g.stopped {
            t.group_stop = Some(g.stop_signal);
        }
    }
    if !seize {
        signal::send_to_task(cx.kr(), &task, Signal::STOP, SigInfo::kernel());
    }
    // 在 group-stop 里等着的线程要改为停在 ptrace-stop 里
    signal::wake(&task);
    Ok(0)
}

/// PTRACE_CONT、PTRACE_SYSCALL、PTRACE_SINGLESTEP：让停着的线程继续，`data` 是要注入的
/// 信号。
fn resume(task: &Task, mode: Mode, data: u64) -> SysResult {
    if data > Signal::MAX as u64 {
        return Err(Errno::EIO);
    }
    {
        let mut t = task.trace.borrow_mut();
        t.mode = mode;
        t.resume = Some(data as i32);
    }
    task.syscall_traced
        .store(mode == Mode::Syscall, Ordering::Relaxed);
    task.stepping.store(mode == Mode::Step, Ordering::Relaxed);
    if mode != Mode::Step {
        task.step_again.store(false, Ordering::Relaxed);
    }
    // 它停在内核里，只等自己的事件，不必打断
    crate::kernel::signal_later(&task.event);
    Ok(0)
}

/// PTRACE_PEEKTEXT、PTRACE_PEEKDATA：读被跟踪进程的一个字，写到 `data`。
fn peek(cx: &mut Cx<'_>, task: &Task, addr: u64, data: u64) -> SysResult {
    let mm = task.res.borrow().mm.clone();
    let mut word = [0u8; 8];
    mm.borrow()
        .read(UserSlice::new(addr, 8), &mut word)
        .map_err(|_| Errno::EIO)?;
    put(cx, data, word).map(|()| 0)
}

/// regset 的（元素大小，元素个数）。
fn regset_shape(nt: u64) -> Result<(usize, usize), Errno> {
    Ok(match nt {
        // user_pt_regs：x0–x30、sp、pc、pstate
        NT_PRSTATUS => (8, 34),
        // user_fpsimd_state：v0–v31、fpsr、fpcr、两个保留的字
        NT_PRFPREG => (4, 132),
        NT_ARM_TLS => (8, 1),
        // user_hwdebug_state：dbg_info、一个空字、16 组（地址、控制、空字）
        NT_ARM_HW_BREAK | NT_ARM_HW_WATCH => (4, 66),
        NT_ARM_SYSTEM_CALL => (4, 1),
        // 没有 SVE、PAC、MTE、SME
        _ => return Err(Errno::EINVAL),
    })
}

/// regset 的内容。
pub(crate) fn encode_regset(nt: u64, r: &Regs, syscallno: i32, hw: &HwDebug) -> Vec<u8> {
    let mut b = Vec::new();
    match nt {
        NT_PRSTATUS => {
            for v in r.x.iter().chain(&[r.sp, r.pc, r.nzcv]) {
                b.extend_from_slice(&v.to_le_bytes());
            }
        }
        NT_PRFPREG => {
            for v in &r.v {
                b.extend_from_slice(&v[0].to_le_bytes());
                b.extend_from_slice(&v[1].to_le_bytes());
            }
            b.extend_from_slice(&(r.fpsr as u32).to_le_bytes());
            b.extend_from_slice(&(r.fpcr as u32).to_le_bytes());
            b.extend_from_slice(&[0; 8]);
        }
        NT_ARM_TLS => b.extend_from_slice(&r.tpidr.to_le_bytes()),
        NT_ARM_HW_BREAK | NT_ARM_HW_WATCH => {
            // dbg_info：调试架构和槽数，然后是 16 组（地址、控制字、空字）
            let (slots, n) = if nt == NT_ARM_HW_BREAK {
                (&hw.brk, BRK_SLOTS)
            } else {
                (&hw.watch, WATCH_SLOTS)
            };
            b.extend_from_slice(&(DEBUG_ARCH_V8 << 8 | n as u32).to_le_bytes());
            b.extend_from_slice(&[0; 4]);
            for &(addr, ctrl) in slots {
                b.extend_from_slice(&addr.to_le_bytes());
                b.extend_from_slice(&ctrl.to_le_bytes());
                b.extend_from_slice(&[0; 4]);
            }
        }
        _ => b.extend_from_slice(&syscallno.to_le_bytes()),
    }
    b
}

/// 跟踪者写来的 regset 内容。
fn decode_regset(
    nt: u64,
    b: &[u8],
    r: &mut Regs,
    syscallno: &mut i32,
    hw: &mut HwDebug,
) -> Result<(), Errno> {
    let u64_at = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
    let u32_at = |off: usize| u32::from_le_bytes(b[off..off + 4].try_into().unwrap());
    match nt {
        NT_PRSTATUS => {
            for (i, x) in r.x.iter_mut().enumerate() {
                *x = u64_at(i);
            }
            r.sp = u64_at(31);
            r.pc = u64_at(32);
            r.nzcv = u64_at(33) & PSTATE_NZCV;
        }
        NT_PRFPREG => {
            for (i, v) in r.v.iter_mut().enumerate() {
                *v = [u64_at(2 * i), u64_at(2 * i + 1)];
            }
            r.fpsr = u64::from(u32_at(512));
            r.fpcr = u64::from(u32_at(516));
        }
        NT_ARM_TLS => r.tpidr = u64_at(0),
        NT_ARM_HW_BREAK | NT_ARM_HW_WATCH => {
            let watch = nt == NT_ARM_HW_WATCH;
            let (mut slots, n) = if watch {
                (hw.watch, WATCH_SLOTS)
            } else {
                (hw.brk, BRK_SLOTS)
            };
            for (i, slot) in slots.iter_mut().enumerate() {
                let addr = u64_at(1 + 2 * i);
                let ctrl = u32_at(16 + 16 * i);
                // 超出槽数的只能是零（Linux 建不出断点，ENOSPC）
                *slot = if i >= n {
                    if (addr, ctrl) != (0, 0) {
                        return Err(Errno::ENOSPC);
                    }
                    (0, 0)
                } else {
                    hw_slot(watch, addr, ctrl)?
                };
            }
            if watch {
                hw.watch = slots;
            } else {
                hw.brk = slots;
            }
        }
        _ => *syscallno = u32_at(0) as i32,
    }
    Ok(())
}

/// 一组硬件断点或观察点（`watch`）的地址和控制字，按 Linux 检查并规范化
/// （`ptrace_hbp_fill_attr_ctrl`、`hw_breakpoint_arch_parse`）：停用的原样记下；启用的
/// 字节选择（BAS）必须是连续的一段，观察点的种类是读、写或者两者，断点只能是 4 字节；
/// 起点是地址加上 BAS 的偏移，再按 8 字节（断点按 4 字节）对齐、把偏移挪回 BAS，
/// 特权级记成 EL0。
fn hw_slot(watch: bool, addr: u64, ctrl: u32) -> Result<(u64, u32), Errno> {
    const EL0: u32 = 2 << 1;
    if ctrl & 1 == 0 {
        return Ok((addr, ctrl));
    }
    let ty = ctrl >> 3 & 3;
    let bas = ctrl >> 5 & 0xff;
    let shift = bas.trailing_zeros();
    let run = bas.checked_shr(shift).unwrap_or(0);
    if bas == 0 || run & (run + 1) != 0 {
        return Err(Errno::EINVAL);
    }
    let len = run.count_ones() as u64;
    let start = addr.wrapping_add(u64::from(shift));
    if watch {
        let offset = start & 7;
        if ty == 0 || offset + len > 8 {
            return Err(Errno::EINVAL);
        }
        let bas = ((1u32 << len) - 1) << offset;
        Ok((start & !7, bas << 5 | ty << 3 | EL0 | 1))
    } else {
        if ty != 0 || len != 4 {
            return Err(Errno::EINVAL);
        }
        Ok((start & !3, 0xf << 5 | EL0 | 1))
    }
}

/// PTRACE_GETREGSET、PTRACE_SETREGSET：`data` 指向 iovec，长度可以比 regset 短（只读写
/// 前面的部分），读写了多少写回 iovec 的长度。
fn regset(cx: &mut Cx<'_>, task: &Task, set: bool, nt: u64, iov: u64) -> SysResult {
    let b: [u8; 16] = get(cx, iov)?;
    let base = u64::from_le_bytes(b[..8].try_into().unwrap());
    let len = u64::from_le_bytes(b[8..].try_into().unwrap());
    let (size, n) = regset_shape(nt)?;
    if !len.is_multiple_of(size as u64) {
        return Err(Errno::EINVAL);
    }
    let len = len.min((size * n) as u64) as usize;
    {
        let mut t = task.trace.borrow_mut();
        let Trace {
            regs,
            syscallno,
            hw,
            hw_changed,
            ..
        } = &mut *t;
        let regs = regs.as_mut().ok_or(Errno::ESRCH)?;
        let mut bytes = encode_regset(nt, regs, *syscallno, hw);
        if set {
            cx.mm().read(UserSlice::new(base, len), &mut bytes[..len])?;
            decode_regset(nt, &bytes, regs, syscallno, hw)?;
            *hw_changed |= nt == NT_ARM_HW_BREAK || nt == NT_ARM_HW_WATCH;
        } else {
            cx.mm().write(UserSlice::new(base, len), &bytes[..len])?;
        }
    }
    put(cx, iov + 8, (len as u64).to_le_bytes()).map(|()| 0)
}

/// PTRACE_GET_SYSCALL_INFO：`size` 是缓冲区的长度。系统调用的入口和出口（要
/// `PTRACE_O_TRACESYSGOOD`）给出编号和参数、返回值，别的停住只有前面的公共部分。返回
/// 完整的长度。
fn syscall_info(cx: &mut Cx<'_>, task: &Task, size: u64, data: u64) -> SysResult {
    const OP_ENTRY: u8 = 1;
    const OP_EXIT: u8 = 2;
    const OP_SECCOMP: u8 = 3;
    let mut b = [0u8; 88];
    let actual = {
        let t = task.trace.borrow();
        let r = t.regs.as_ref().ok_or(Errno::ESRCH)?;
        b[4..8].copy_from_slice(&AUDIT_ARCH_AARCH64.to_le_bytes());
        b[8..16].copy_from_slice(&r.pc.to_le_bytes());
        b[16..24].copy_from_slice(&r.sp.to_le_bytes());
        let si_code = t
            .siginfo
            .map_or(0, |s| i32::from_le_bytes(s[8..12].try_into().unwrap()));
        let syscall = si_code == Signal::TRAP.number() | 0x80;
        let seccomp = si_code == Signal::TRAP.number() | (event::SECCOMP as i32) << 8;
        let entry = |b: &mut [u8; 88]| {
            b[24..32].copy_from_slice(&(t.syscallno as i64).to_le_bytes());
            let args = [r.orig_x0, r.x[1], r.x[2], r.x[3], r.x[4], r.x[5]];
            for (i, a) in args.iter().enumerate() {
                b[32 + i * 8..40 + i * 8].copy_from_slice(&a.to_le_bytes());
            }
        };
        match t.message {
            // seccomp 的 TRACE：编号和参数，加上过滤器给的数据
            data if seccomp => {
                b[0] = OP_SECCOMP;
                entry(&mut b);
                b[80..84].copy_from_slice(&(data as u32).to_le_bytes());
                84
            }
            EVENTMSG_SYSCALL_ENTRY if syscall => {
                b[0] = OP_ENTRY;
                entry(&mut b);
                80
            }
            EVENTMSG_SYSCALL_EXIT if syscall => {
                b[0] = OP_EXIT;
                let rval = r.x[0] as i64;
                b[24..32].copy_from_slice(&rval.to_le_bytes());
                b[32] = (-4095..0).contains(&rval) as u8;
                33
            }
            _ => 24,
        }
    };
    let n = size.min(actual) as usize;
    cx.mm().write(UserSlice::new(data, n), &b[..n])?;
    Ok(actual)
}

/// 读 `count` 个 iovec。
fn iovecs(cx: &Cx<'_>, addr: u64, count: u64) -> Result<Vec<(u64, u64)>, Errno> {
    let mut b = vec![0u8; count as usize * 16];
    cx.mm().read(UserSlice::new(addr, b.len()), &mut b)?;
    let mut total = 0u64;
    let mut out = Vec::with_capacity(count as usize);
    for c in b.chunks_exact(16) {
        let base = u64::from_le_bytes(c[..8].try_into().unwrap());
        let len = u64::from_le_bytes(c[8..].try_into().unwrap());
        total = total
            .checked_add(len)
            .filter(|&t| t <= i64::MAX as u64)
            .ok_or(Errno::EINVAL)?;
        out.push((base, len));
    }
    Ok(out)
}

/// process_vm_readv（`write` 时是 process_vm_writev）：在自己的 `local` 和进程 `pid` 的
/// `remote` 之间拷贝，权限与 PTRACE_ATTACH 相同。远端按页拷贝，碰到不能访问的页就停在
/// 那里，返回已经拷了多少（一字节也没拷时是 EFAULT）。
pub(super) fn process_vm(
    cx: &mut Cx<'_>,
    pid: i32,
    (local, local_count): (u64, u64),
    (remote, remote_count): (u64, u64),
    flags: u64,
    write: bool,
) -> SysResult {
    const IOV_MAX: u64 = 1024;
    if flags != 0 || local_count > IOV_MAX || remote_count > IOV_MAX {
        return Err(Errno::EINVAL);
    }
    let local = iovecs(cx, local, local_count)?;
    if local.iter().all(|&(_, len)| len == 0) {
        return Ok(0);
    }
    let remote = iovecs(cx, remote, remote_count)?;
    let task = cx.kr().tasks.get(&pid).cloned().ok_or(Errno::ESRCH)?;
    let group = cx
        .kr()
        .groups
        .get(&task.tgid)
        .cloned()
        .ok_or(Errno::ESRCH)?;
    if !may_ptrace(cx, &group.borrow()) {
        return Err(Errno::EPERM);
    }
    let target: Rc<_> = task.res.borrow().mm.clone();
    let mut local = local.into_iter().filter(|&(_, len)| len > 0);
    let mut cur = (0, 0);
    let mut total = 0u64;
    let mut faulted = false;
    let mut buf = Vec::new();
    'remote: for (base, len) in remote {
        let mut done = 0;
        while done < len {
            if cur.1 == 0 {
                match local.next() {
                    Some(next) => cur = next,
                    None => break 'remote,
                }
            }
            let addr = base + done;
            let n = (len - done).min(cur.1).min(PAGE_SIZE - addr % PAGE_SIZE) as usize;
            buf.resize(n, 0);
            let ok = if write {
                cx.mm().read(UserSlice::new(cur.0, n), &mut buf).is_ok()
                    && target.borrow().write(UserSlice::new(addr, n), &buf).is_ok()
            } else {
                target
                    .borrow()
                    .read(UserSlice::new(addr, n), &mut buf)
                    .is_ok()
                    && cx.mm().write(UserSlice::new(cur.0, n), &buf).is_ok()
            };
            if !ok {
                faulted = true;
                break 'remote;
            }
            done += n as u64;
            total += n as u64;
            cur = (cur.0 + n as u64, cur.1 - n as u64);
        }
    }
    if faulted && total == 0 {
        return Err(Errno::EFAULT);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hw_slots_are_checked_and_normalized_like_linux() {
        const EL0: u32 = 2 << 1;
        let watch = |bas: u32, ty: u32| bas << 5 | ty << 3 | EL0 | 1;
        // 停用的原样记下，不检查
        assert_eq!(hw_slot(true, 0x1234, 0x40), Ok((0x1234, 0x40)));
        // 地址里的偏移和 BAS 里的偏移是一回事：都规范化成按 8 对齐的地址
        assert_eq!(
            hw_slot(true, 0x1000, watch(0xf0, 2)),
            Ok((0x1000, watch(0xf0, 2)))
        );
        assert_eq!(
            hw_slot(true, 0x1004, watch(0x0f, 2)),
            Ok((0x1000, watch(0xf0, 2)))
        );
        assert_eq!(
            hw_slot(true, 0x1003, watch(0x01, 1)),
            Ok((0x1000, watch(0x08, 1)))
        );
        // 越过 8 字节、BAS 不连续或为零、种类是执行：EINVAL
        assert_eq!(hw_slot(true, 0x1006, watch(0x0f, 2)), Err(Errno::EINVAL));
        assert_eq!(hw_slot(true, 0x1000, watch(0x05, 2)), Err(Errno::EINVAL));
        assert_eq!(hw_slot(true, 0x1000, watch(0, 2)), Err(Errno::EINVAL));
        assert_eq!(hw_slot(true, 0x1000, watch(0xff, 0)), Err(Errno::EINVAL));
        // 断点只能是执行、4 字节，地址按 4 对齐
        assert_eq!(
            hw_slot(false, 0x2000, watch(0x0f, 0)),
            Ok((0x2000, watch(0x0f, 0)))
        );
        assert_eq!(
            hw_slot(false, 0x2002, watch(0x0f, 0)),
            Ok((0x2000, watch(0x0f, 0)))
        );
        assert_eq!(hw_slot(false, 0x2000, watch(0x03, 0)), Err(Errno::EINVAL));
        assert_eq!(hw_slot(false, 0x2000, watch(0x0f, 1)), Err(Errno::EINVAL));
    }
}
