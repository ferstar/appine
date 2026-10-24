//! 进程与线程：clone、execve、exit、wait4，进程组和会话。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use appine_engine::Cpu;

use super::{Flow, SysResult, put, read_path};
use crate::errno::Errno;
use crate::kernel::{Cx, Tid};
use crate::loader::{self, Args, LoadError};
use crate::mm::Mm;
use crate::process::{self, ExitStatus, Signal, ThreadGroup};
use crate::signal::{self, AltStack, SigInfo};
use crate::task::{self, FsContext, Task, TaskState};
use crate::uaccess::UserSlice;

const CLONE_VM: u64 = 0x100;
const CLONE_FS: u64 = 0x200;
const CLONE_FILES: u64 = 0x400;
const CLONE_SIGHAND: u64 = 0x800;
const CLONE_PIDFD: u64 = 0x1000;
const CLONE_VFORK: u64 = 0x4000;
const CLONE_PARENT: u64 = 0x8000;
const CLONE_THREAD: u64 = 0x10000;
const CLONE_SETTLS: u64 = 0x80000;
const CLONE_PARENT_SETTID: u64 = 0x100000;
const CLONE_CHILD_CLEARTID: u64 = 0x200000;
const CLONE_CHILD_SETTID: u64 = 0x1000000;
/// 各种 `CLONE_NEW*`：不支持 namespace（设计文档 §6）。
const CLONE_NEW_MASK: u64 = 0x0002_0000 | 0x7e00_0000;

const WNOHANG: u32 = 1;
const WUNTRACED: u32 = 2;
const WEXITED: u32 = 4;
const WCONTINUED: u32 = 8;
const WNOWAIT: u32 = 0x0100_0000;
const WNOTHREAD: u32 = 0x2000_0000;
const WALL: u32 = 0x4000_0000;
const WCLONE: u32 = 0x8000_0000;

const P_ALL: u32 = 0;
const P_PID: u32 = 1;
const P_PGID: u32 = 2;

/// execve 的参数和环境变量最多占用的字节数（与初始栈的上限一致）。
const MAX_ARG_BYTES: usize = (loader::STACK_SIZE / 4) as usize;
/// 单个参数的最大长度（Linux 的 `MAX_ARG_STRLEN`）。
const MAX_ARG_STRLEN: usize = 32 * 4096;
/// `#!` 解释器最多嵌套的层数，与 Linux 一样超过时返回 ELOOP。
const MAX_INTERP_DEPTH: usize = 4;
/// `#!` 行最多读多少字节（Linux 的 `BINPRM_BUF_SIZE`）。
const SHEBANG_MAX: usize = 256;

pub(super) fn exit(cx: &mut Cx<'_>, code: i32) -> Flow {
    if cx.tid() == cx.tgid() {
        cx.t.group.borrow_mut().leader_code = code & 0xff;
    }
    Flow::Exit
}

pub(super) fn exit_group(cx: &mut Cx<'_>, code: i32) -> Flow {
    let group = cx.t.group.clone();
    signal::group_exit(cx.k(), &group, ExitStatus::Exited(code & 0xff));
    Flow::Exit
}

pub(super) fn set_tid_address(cx: &mut Cx<'_>, addr: u64) -> SysResult {
    cx.t.clear_child_tid = addr;
    Ok(cx.tid() as u64)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn clone(
    cx: &mut Cx<'_>,
    cpu: &Cpu,
    flags: u64,
    stack: u64,
    ptid: u64,
    tls: u64,
    ctid: u64,
) -> SysResult {
    if flags & CLONE_NEW_MASK != 0 {
        return Err(Errno::EPERM);
    }
    if (flags & CLONE_THREAD != 0 && flags & CLONE_SIGHAND == 0)
        || (flags & CLONE_SIGHAND != 0 && flags & CLONE_VM == 0)
        || flags & CLONE_PIDFD != 0
    {
        return Err(Errno::EINVAL);
    }
    let thread = flags & CLONE_THREAD != 0;
    let tid = cx.k().alloc_tid()?;

    let mm = if flags & CLONE_VM != 0 {
        cx.t.mm.clone()
    } else {
        // 先让同一地址空间里的其他线程停下来，复制出来的才是 fork 那一刻的快照
        cx.mm().quiesce(&cx.task.sync);
        Rc::new(RefCell::new(cx.mm().fork()?))
    };
    let fds = if flags & CLONE_FILES != 0 {
        cx.t.fds.clone()
    } else {
        Rc::new(RefCell::new(cx.fds().clone()))
    };
    let fs = if flags & CLONE_FS != 0 {
        cx.t.fs.clone()
    } else {
        let f = cx.t.fs.borrow();
        Rc::new(RefCell::new(FsContext {
            cwd: f.cwd,
            umask: f.umask,
        }))
    };
    let group = if thread {
        cx.t.group.clone()
    } else {
        let parent = cx.t.group.borrow();
        let sighand = if flags & CLONE_SIGHAND != 0 {
            parent.sighand.clone()
        } else {
            Rc::new(RefCell::new(parent.sighand.borrow().clone()))
        };
        let ppid = if flags & CLONE_PARENT != 0 {
            parent.ppid
        } else {
            parent.tgid
        };
        let mut g = ThreadGroup::new(tid, ppid, sighand);
        g.pgid = parent.pgid;
        g.sid = parent.sid;
        g.rlimits = parent.rlimits;
        g.exit_signal = (flags & 0xff) as i32;
        Rc::new(RefCell::new(g))
    };

    // 子线程从 clone 返回处开始执行，x0 = 0；线程（CLONE_VM 但不是 vfork）不继承备用栈
    // SAFETY: mm 由子线程的 TaskState 持有
    let mut child_cpu = unsafe { Cpu::new(mm.borrow().mmu()) };
    {
        let (p, c) = (cpu.state(), child_cpu.state_mut());
        c.x = p.x;
        c.x[0] = 0;
        c.sp = if stack != 0 { stack } else { p.sp };
        c.pc = p.pc;
        c.nzcv = p.nzcv;
        c.fpcr = p.fpcr;
        c.fpsr = p.fpsr;
        c.v = p.v;
        c.tpidr_el0 = if flags & CLONE_SETTLS != 0 {
            tls
        } else {
            p.tpidr_el0
        };
    }
    let altstack = if flags & (CLONE_VM | CLONE_VFORK) == CLONE_VM {
        AltStack::default()
    } else {
        cx.t.altstack
    };
    let vfork_done = (flags & CLONE_VFORK != 0).then(|| Rc::new(Cell::new(false)));
    let state = TaskState {
        mm: mm.clone(),
        fds,
        fs,
        group: group.clone(),
        vfs: cx.t.vfs.clone(),
        altstack,
        clear_child_tid: if flags & CLONE_CHILD_CLEARTID != 0 {
            ctid
        } else {
            0
        },
        saved_sigmask: None,
        vfork_done: vfork_done.clone(),
    };
    let tgid = if thread { cx.tgid() } else { tid };
    let task = Task::new(
        tid,
        tgid,
        &child_cpu,
        state,
        cx.task.sigmask.get(),
        cx.task.comm.get(),
    )?;

    // 写 tid 之前先确认能启动线程，失败时不留下任何痕迹
    if flags & CLONE_PARENT_SETTID != 0 {
        put(cx, ptid, tid.to_le_bytes())?;
    }
    if flags & CLONE_CHILD_SETTID != 0 {
        mm.borrow()
            .write(UserSlice::new(ctid, 4), &tid.to_le_bytes())?;
    }
    let shared = cx.shared;
    task::start(shared, cx.k(), task, child_cpu)?;
    if !thread {
        let ppid = group.borrow().ppid;
        let k = cx.k();
        k.groups.insert(tid, group);
        if let Some(parent) = k.groups.get(&ppid) {
            parent.borrow_mut().children.push(tid);
        }
    }

    // vfork：等子进程 execve 或退出；只有致命信号能打断（Linux 的 TASK_KILLABLE）
    if let Some(done) = vfork_done {
        while !done.get() && !cx.task.killed.get() {
            cx.wait(None, &mut [])?;
        }
    }
    Ok(tid as u64)
}

/// 读取 execve 的 argv 或 envp：以 NULL 结尾的字符串指针数组。
fn read_strings(cx: &Cx<'_>, addr: u64, total: &mut usize) -> Result<Vec<Vec<u8>>, Errno> {
    let mut out = Vec::new();
    if addr == 0 {
        return Ok(out);
    }
    for i in 0.. {
        let p = u64::from_le_bytes(super::get(cx, addr.wrapping_add(i * 8))?);
        if p == 0 {
            break;
        }
        let s = cx.mm().read_cstr(p, MAX_ARG_STRLEN - 1).map_err(|e| {
            if e == Errno::ENAMETOOLONG {
                Errno::E2BIG
            } else {
                e
            }
        })?;
        *total += s.len() + 1 + 8;
        if *total > MAX_ARG_BYTES {
            return Err(Errno::E2BIG);
        }
        out.push(s);
    }
    Ok(out)
}

/// `#!` 行：(解释器, 可选的一个参数)。不是脚本时返回 None。
fn parse_shebang(file: &[u8]) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    let head = file.strip_prefix(b"#!")?;
    let line = &head[..head.len().min(SHEBANG_MAX - 2)];
    let line = line.split(|&b| b == b'\n').next().unwrap_or(line);
    let is_space = |b: &u8| *b == b' ' || *b == b'\t';
    let line = line.trim_ascii();
    let end = line.iter().position(is_space).unwrap_or(line.len());
    let interp = &line[..end];
    if interp.is_empty() {
        return None;
    }
    let rest = line[end..].trim_ascii();
    Some((interp.to_vec(), (!rest.is_empty()).then(|| rest.to_vec())))
}

fn load_error(e: LoadError) -> Errno {
    match e {
        LoadError::NotElf | LoadError::Unsupported(_) | LoadError::Malformed(_) => Errno::ENOEXEC,
        LoadError::ArgumentsTooLong => Errno::E2BIG,
        LoadError::Memory(e) | LoadError::Interpreter(_, e) => e,
    }
}

pub(super) fn execve(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    path: u64,
    argv: u64,
    envp: u64,
) -> Result<(), Errno> {
    let path = read_path(cx, path)?;
    let mut total = 0;
    let mut argv = read_strings(cx, argv, &mut total)?;
    let envp = read_strings(cx, envp, &mut total)?;
    let vfs = cx.vfs()?;
    let cwd = cx.cwd();

    // `#!` 脚本：换成解释器，argv 变成 [解释器, 参数, 脚本路径, 原来的 argv[1..]]
    let mut file_path = path.clone();
    let mut file = process::read_executable(&vfs, cwd, &file_path)?;
    for depth in 0.. {
        let Some((interp, arg)) = parse_shebang(&file) else {
            break;
        };
        if depth == MAX_INTERP_DEPTH {
            return Err(Errno::ELOOP);
        }
        let mut new_argv = vec![interp.clone()];
        new_argv.extend(arg);
        new_argv.push(file_path);
        new_argv.extend(argv.into_iter().skip(1));
        argv = new_argv;
        file = process::read_executable(&vfs, cwd, &interp)?;
        file_path = interp;
    }

    let mut mm = Mm::new();
    let args = Args {
        argv: &argv,
        envp: &envp,
        execfn: &path,
    };
    let mut read_interp = |p: &[u8]| process::read_executable(&vfs, cwd, p);
    let image = loader::load(&mut mm, &file, &args, &mut read_interp).map_err(load_error)?;

    // 从这里开始不能失败：结束同一线程组里的其他线程，然后换掉地址空间
    kill_other_threads(cx)?;
    if Rc::strong_count(&cx.t.fds) > 1 {
        let copy = cx.fds().clone();
        cx.t.fds = Rc::new(RefCell::new(copy));
    }
    cx.fds_mut().close_on_exec();
    {
        let group = cx.t.group.clone();
        let mut g = group.borrow_mut();
        if Rc::strong_count(&g.sighand) > 1 {
            let copy = g.sighand.borrow().clone();
            g.sighand = Rc::new(RefCell::new(copy));
        }
        g.sighand.borrow_mut().reset_on_exec();
    }
    let task = cx.task.clone();
    let (k, t) = cx.parts();
    task::release_mm(k, &task, t);
    let mm = Rc::new(RefCell::new(mm));
    mm.borrow_mut().add_user(task.sync.clone());
    // SAFETY: 新的地址空间由 TaskState 持有；旧的可能随这次赋值释放，cpu 已经不再使用它
    unsafe { cpu.reset(mm.borrow().mmu()) };
    cx.t.mm = mm;
    cx.t.altstack = AltStack::default();
    cx.t.saved_sigmask = None;
    cx.task.comm.set(process::comm_of(&path));
    let st = cpu.state_mut();
    st.pc = image.entry;
    st.sp = image.sp;
    // 新程序开始执行之前先处理已经挂起的信号
    if cx.interrupted() {
        // SAFETY: cpu 属于当前线程，正在运行
        unsafe { cx.task.sync.kicker.kick() };
    }
    Ok(())
}

/// execve 的 de_thread：结束线程组里的其他线程并等它们退出。不是主线程调用
/// execve 时，它接过主线程的 tid。
fn kill_other_threads(cx: &mut Cx<'_>) -> Result<(), Errno> {
    let me = cx.tid();
    let others: Vec<Tid> =
        cx.t.group
            .borrow()
            .threads
            .iter()
            .copied()
            .filter(|&t| t != me)
            .collect();
    if others.is_empty() {
        return Ok(());
    }
    for tid in &others {
        if let Some(t) = cx.kr().tasks.get(tid) {
            t.killed.set(true);
            signal::wake(t);
        }
    }
    while cx.t.group.borrow().threads.len() > 1 {
        if cx.task.killed.get() {
            // 别的线程同时在结束整个进程
            return Err(Errno::EINTR);
        }
        cx.wait(None, &mut [])?;
    }
    let tgid = cx.tgid();
    if me != tgid {
        let k = cx.k();
        let task = k.tasks.remove(&me).unwrap();
        task.tid.set(tgid);
        k.tasks.insert(tgid, task);
        cx.t.group.borrow_mut().threads = vec![tgid];
    }
    Ok(())
}

/// 等待的对象：pid、任意子进程或某个进程组。
#[derive(Clone, Copy)]
enum Who {
    Pid(Tid),
    Any,
    Pgid(Tid),
}

/// 找到一个符合条件的已结束子进程并（`reap` 时）回收它。没有子进程符合条件时返回 ECHILD。
fn find_zombie(cx: &mut Cx<'_>, who: Who, reap: bool) -> Result<Option<(Tid, ExitStatus)>, Errno> {
    let children = cx.t.group.borrow().children.clone();
    let mut any = false;
    for c in children {
        let Some(child) = cx.kr().groups.get(&c).cloned() else {
            continue;
        };
        let (pgid, zombie) = {
            let g = child.borrow();
            (g.pgid, g.zombie)
        };
        let matches = match who {
            Who::Pid(p) => p == c,
            Who::Any => true,
            Who::Pgid(g) => g == pgid,
        };
        if !matches {
            continue;
        }
        any = true;
        if let Some(status) = zombie {
            if reap {
                cx.k().groups.remove(&c);
                cx.t.group.borrow_mut().children.retain(|&x| x != c);
            }
            return Ok(Some((c, status)));
        }
    }
    if any { Ok(None) } else { Err(Errno::ECHILD) }
}

/// wait4 和 waitid 的公共部分：等到有子进程结束。
fn wait_child(
    cx: &mut Cx<'_>,
    who: Who,
    nohang: bool,
    reap: bool,
) -> Result<Option<(Tid, ExitStatus)>, Errno> {
    loop {
        if let Some(found) = find_zombie(cx, who, reap)? {
            return Ok(Some(found));
        }
        if nohang {
            return Ok(None);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait(None, &mut [])?;
    }
}

pub(super) fn wait4(
    cx: &mut Cx<'_>,
    pid: i32,
    wstatus: u64,
    options: u32,
    rusage: u64,
) -> SysResult {
    if options & !(WNOHANG | WUNTRACED | WCONTINUED | WNOTHREAD | WALL | WCLONE) != 0 {
        return Err(Errno::EINVAL);
    }
    let who = match pid {
        -1 => Who::Any,
        0 => Who::Pgid(cx.t.group.borrow().pgid),
        p if p < 0 => Who::Pgid(-p),
        p => Who::Pid(p),
    };
    let Some((child, status)) = wait_child(cx, who, options & WNOHANG != 0, true)? else {
        return Ok(0);
    };
    if wstatus != 0 {
        put(cx, wstatus, status.wait_status().to_le_bytes())?;
    }
    if rusage != 0 {
        cx.mm().write(UserSlice::new(rusage, 144), &[0; 144])?;
    }
    Ok(child as u64)
}

pub(super) fn waitid(
    cx: &mut Cx<'_>,
    idtype: u32,
    id: i32,
    infop: u64,
    options: u32,
    rusage: u64,
) -> SysResult {
    if options & !(WNOHANG | WUNTRACED | WEXITED | WCONTINUED | WNOWAIT | WNOTHREAD | WALL | WCLONE)
        != 0
        || options & (WEXITED | WUNTRACED | WCONTINUED) == 0
    {
        return Err(Errno::EINVAL);
    }
    let who = match idtype {
        P_ALL => Who::Any,
        P_PID if id > 0 => Who::Pid(id),
        P_PGID if id > 0 => Who::Pgid(id),
        P_PGID if id == 0 => Who::Pgid(cx.t.group.borrow().pgid),
        _ => return Err(Errno::EINVAL),
    };
    // 停止和继续随作业控制一起实现，这里只等待结束
    if options & WEXITED == 0 {
        return Err(Errno::ECHILD);
    }
    let found = wait_child(cx, who, options & WNOHANG != 0, options & WNOWAIT == 0)?;
    if infop != 0 {
        let info = match found {
            Some((child, status)) => SigInfo::child(child, status).encode(Signal::CHLD),
            None => [0; 128],
        };
        cx.mm().write(UserSlice::new(infop, 128), &info)?;
    }
    if rusage != 0 {
        cx.mm().write(UserSlice::new(rusage, 144), &[0; 144])?;
    }
    Ok(0)
}

/// pid 参数为 0 时表示调用者自己。
fn group_of(cx: &Cx<'_>, pid: Tid) -> Result<Rc<RefCell<ThreadGroup>>, Errno> {
    if pid == 0 {
        return Ok(cx.t.group.clone());
    }
    cx.kr().groups.get(&pid).cloned().ok_or(Errno::ESRCH)
}

pub(super) fn getpgid(cx: &mut Cx<'_>, pid: i32) -> SysResult {
    Ok(group_of(cx, pid)?.borrow().pgid as u64)
}

pub(super) fn getsid(cx: &mut Cx<'_>, pid: i32) -> SysResult {
    Ok(group_of(cx, pid)?.borrow().sid as u64)
}

pub(super) fn setsid(cx: &mut Cx<'_>) -> SysResult {
    let tgid = cx.tgid();
    // 已经是进程组组长时不能新建会话
    if cx.kr().groups.values().any(|g| g.borrow().pgid == tgid) {
        return Err(Errno::EPERM);
    }
    let mut g = cx.t.group.borrow_mut();
    g.sid = tgid;
    g.pgid = tgid;
    Ok(tgid as u64)
}

pub(super) fn setpgid(cx: &mut Cx<'_>, pid: i32, pgid: i32) -> SysResult {
    if pgid < 0 {
        return Err(Errno::EINVAL);
    }
    let me = cx.tgid();
    let target = group_of(cx, pid)?;
    let (tgid, ppid, sid) = {
        let g = target.borrow();
        (g.tgid, g.ppid, g.sid)
    };
    let my_sid = cx.t.group.borrow().sid;
    if tgid != me && ppid != me {
        return Err(Errno::ESRCH);
    }
    if sid != my_sid {
        return Err(Errno::EPERM);
    }
    // 会话组长不能换进程组
    if sid == tgid {
        return Err(Errno::EPERM);
    }
    let pgid = if pgid == 0 { tgid } else { pgid };
    // 只能加入同一会话里已有的进程组，或者以自己为组长新建
    if pgid != tgid
        && !cx
            .kr()
            .groups
            .values()
            .any(|g| g.borrow().pgid == pgid && g.borrow().sid == my_sid)
    {
        return Err(Errno::EPERM);
    }
    target.borrow_mut().pgid = pgid;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shebang_lines() {
        assert_eq!(
            parse_shebang(b"#!/bin/sh\necho"),
            Some((b"/bin/sh".to_vec(), None))
        );
        assert_eq!(
            parse_shebang(b"#! /usr/bin/env  python3 -u \nx"),
            Some((b"/usr/bin/env".to_vec(), Some(b"python3 -u".to_vec())))
        );
        assert_eq!(parse_shebang(b"#!\n"), None);
        assert_eq!(parse_shebang(b"\x7fELF"), None);
    }

    #[test]
    fn wait_status_encoding() {
        assert_eq!(ExitStatus::Exited(3).wait_status(), 0x300);
        let killed = ExitStatus::Signaled {
            signal: Signal::TERM,
            pc: None,
            fault_addr: None,
        };
        assert_eq!(killed.wait_status(), 15);
    }
}
