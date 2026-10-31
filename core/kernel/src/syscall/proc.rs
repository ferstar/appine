//! 进程与线程：clone、execve、exit、wait4，进程组和会话。

use std::cell::Cell;
use std::rc::Rc;

use appine_engine::Cpu;

use super::{Flow, SysResult, put, read_path};
use crate::cell::RefCell;
use crate::cred::{Cred, FileCaps, XATTR_NAME_CAPS, cap};
use crate::errno::Errno;
use crate::fd::{FileKind, FileRef, OpenFile, Pidfd, oflags};
use crate::kernel::{Cx, Tid};
use crate::loader::{self, Args, LoadError};
use crate::mm::Mm;
use crate::process::{self, ExitStatus, Signal, ThreadGroup};
use crate::signal::{self, AltStack, SigInfo, code};
use crate::task::{self, FsContext, Task, TaskState};
use crate::uaccess::UserSlice;
use crate::vfs::store::{Ino, mode};
use crate::vfs::{Node, Pseudo, Vfs, normalize};

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
const CLONE_DETACHED: u64 = 0x400000;
const CLONE_CHILD_SETTID: u64 = 0x1000000;
const CLONE_SYSVSEM: u64 = 0x40000;
const CLONE_NEWNS: u64 = 0x0002_0000;
const CLONE_NEWUSER: u64 = 0x1000_0000;
const CLONE_NEWPID: u64 = 0x2000_0000;
const CLONE_NEWUTS: u64 = 0x0400_0000;
const CLONE_NEWIPC: u64 = 0x0800_0000;
const CLONE_NEWNET: u64 = 0x4000_0000;
const CLONE_NEWCGROUP: u64 = 0x0200_0000;
/// 各种 `CLONE_NEW*`（含 clone3 才能用的 `CLONE_NEWTIME`）：支持挂载、用户、pid、UTS、
/// IPC、网络、cgroup 命名空间（`syscall::ns`），time 命名空间是 EPERM（设计文档 §6）。
pub(super) const CLONE_NEW_MASK: u64 = 0x80 | CLONE_NEWNS | 0x7e00_0000;
/// 支持的 `CLONE_NEW*`。
const CLONE_NEW_DONE: u64 = CLONE_NEWNS
    | CLONE_NEWUSER
    | CLONE_NEWPID
    | CLONE_NEWUTS
    | CLONE_NEWIPC
    | CLONE_NEWNET
    | CLONE_NEWCGROUP;
/// 要在新进程的用户命名空间里有 `CAP_SYS_ADMIN` 的 `CLONE_NEW*`（Linux 的 copy_namespaces）。
const CLONE_NEW_ADMIN: u64 =
    CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWNET | CLONE_NEWCGROUP;
/// clone3：子进程的信号处理函数恢复默认。
const CLONE_CLEAR_SIGHAND: u64 = 0x1_0000_0000;
/// clone3：子进程放进 `cgroup` 指定的 cgroup。
const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;
/// 低 8 位：clone 的退出信号。
const CSIGNAL: u64 = 0xff;

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
const P_PIDFD: u32 = 3;

const AT_FDCWD: i32 = -100;
const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
const AT_EMPTY_PATH: u32 = 0x1000;

/// 单个参数的最大长度（Linux 的 `MAX_ARG_STRLEN`）。
const MAX_ARG_STRLEN: usize = 32 * 4096;
/// `#!` 解释器最多嵌套的层数，与 Linux 一样超过时返回 ELOOP。
const MAX_INTERP_DEPTH: usize = 4;
/// `#!` 行最多读多少字节（Linux 的 `BINPRM_BUF_SIZE`）。
const SHEBANG_MAX: usize = 256;

pub(super) fn exit(cx: &mut Cx<'_>, code: i32) -> Flow {
    let status = ExitStatus::Exited(code & 0xff);
    cx.task.exit_status.set(status);
    if cx.tid() == cx.tgid() {
        cx.t.group.borrow_mut().leader_status = status;
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

/// clone 和 clone3 的参数（Linux 的 `struct kernel_clone_args`）。
struct CloneArgs {
    flags: u64,
    /// 子进程退出时发给父进程的信号，0 表示不发。
    exit_signal: i32,
    /// 子线程的栈顶，0 表示沿用父线程的 sp。
    stack: u64,
    ptid: u64,
    tls: u64,
    ctid: u64,
    /// `CLONE_PIDFD`：新进程的 pidfd 写到哪里。
    pidfd: u64,
    /// clone3 指定的新进程在各层 pid 命名空间里的编号，从它所在的那层起往外。
    set_tid: Vec<Tid>,
}

pub(super) fn clone(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    flags: u64,
    stack: u64,
    ptid: u64,
    tls: u64,
    ctid: u64,
) -> SysResult {
    // 只看低 32 位；pidfd 写在 parent_tid 那里，所以不能再同时要 CLONE_PARENT_SETTID
    let flags = flags & 0xffff_ffff;
    if flags & CLONE_PIDFD != 0 && flags & CLONE_PARENT_SETTID != 0 {
        return Err(Errno::EINVAL);
    }
    let args = CloneArgs {
        flags: flags & !CSIGNAL,
        exit_signal: (flags & CSIGNAL) as i32,
        stack,
        ptid,
        tls,
        ctid,
        pidfd: ptid,
        set_tid: Vec::new(),
    };
    do_clone(cx, cpu, args)
}

/// clone3：参数在 `struct clone_args` 里，比 clone 多了 pidfd、栈的大小和 set_tid。
pub(super) fn clone3(cx: &mut Cx<'_>, cpu: &mut Cpu, uargs: u64, size: usize) -> SysResult {
    // CLONE_ARGS_SIZE_VER0 和已知的最大长度（VER2）
    const MIN: usize = 64;
    const KNOWN: usize = 88;
    if size < MIN {
        return Err(Errno::EINVAL);
    }
    if size > appine_engine::PAGE_SIZE as usize {
        return Err(Errno::E2BIG);
    }
    let mut b = vec![0u8; size];
    cx.mm().read(UserSlice::new(uargs, size), &mut b)?;
    // 新内核的程序传来更长的结构体：多出来的部分必须是零
    if b.get(KNOWN..)
        .is_some_and(|rest| rest.iter().any(|&x| x != 0))
    {
        return Err(Errno::E2BIG);
    }
    b.resize(KNOWN, 0);
    let w = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
    let (flags, pidfd, ctid, ptid, exit_signal) = (w(0), w(1), w(2), w(3), w(4));
    let (stack, stack_size, tls, set_tid, set_tid_size) = (w(5), w(6), w(7), w(8), w(9));
    let known = 0xffff_ffff | CLONE_CLEAR_SIGHAND | CLONE_INTO_CGROUP;
    let invalid = flags & !known != 0
        || flags & (CLONE_DETACHED | (CSIGNAL & !0x80)) != 0
        || flags & CLONE_SIGHAND != 0 && flags & CLONE_CLEAR_SIGHAND != 0
        || flags & (CLONE_THREAD | CLONE_PARENT) != 0 && exit_signal != 0
        || exit_signal > Signal::MAX as u64
        || (stack == 0) != (stack_size == 0)
        || set_tid_size > crate::pidns::MAX_PID_NS_LEVEL as u64
        || (set_tid == 0) != (set_tid_size == 0);
    if invalid {
        return Err(Errno::EINVAL);
    }
    // 没有 cgroup
    if flags & CLONE_INTO_CGROUP != 0 {
        return Err(Errno::EINVAL);
    }
    // 指定各层的 pid（CRIU 用；能不能指定在 Kernel::choose_pid 里看）
    let mut buf = vec![0u8; set_tid_size as usize * 4];
    cx.mm().read(UserSlice::new(set_tid, buf.len()), &mut buf)?;
    let set_tid: Vec<Tid> = buf
        .chunks_exact(4)
        .map(|b| Tid::from_le_bytes(b.try_into().unwrap()))
        .collect();
    if flags & CLONE_PIDFD != 0 && flags & CLONE_PARENT_SETTID != 0 && pidfd == ptid {
        return Err(Errno::EINVAL);
    }
    let args = CloneArgs {
        flags,
        exit_signal: exit_signal as i32,
        stack: if stack != 0 {
            stack.wrapping_add(stack_size)
        } else {
            0
        },
        ptid,
        tls,
        ctid,
        pidfd,
        set_tid,
    };
    do_clone(cx, cpu, args)
}

fn do_clone(cx: &mut Cx<'_>, cpu: &mut Cpu, args: CloneArgs) -> SysResult {
    let CloneArgs {
        flags,
        stack,
        ptid,
        tls,
        ctid,
        ..
    } = args;
    let both = |a: u64, b: u64| flags & (a | b) == a | b;
    if both(CLONE_NEWNS, CLONE_FS)
        || both(CLONE_NEWUSER, CLONE_FS)
        || both(CLONE_NEWUSER, CLONE_THREAD)
        || (flags & CLONE_THREAD != 0 && flags & CLONE_SIGHAND == 0)
        || (flags & CLONE_SIGHAND != 0 && flags & CLONE_VM == 0)
    {
        return Err(Errno::EINVAL);
    }
    // pid 命名空间的 1 号进程不能给自己建兄弟进程；线程要和自己在同一个 pid 命名空间里
    let active = cx.pid_ns();
    let for_children = cx.pid_ns_children();
    let thread = flags & CLONE_THREAD != 0;
    if flags & CLONE_PARENT != 0 && active.reaper.get() == Some(cx.tgid())
        || thread && (flags & CLONE_NEWPID != 0 || for_children.id != active.id)
        || (flags & CLONE_PIDFD != 0 && flags & (CLONE_DETACHED | CLONE_THREAD) != 0)
    {
        return Err(Errno::EINVAL);
    }
    if flags & CLONE_NEW_MASK & !CLONE_NEW_DONE != 0 {
        return Err(Errno::EPERM);
    }
    // 新的用户命名空间先建好，别的命名空间属于它、按它的能力检查（Linux 的 copy_creds 在
    // copy_namespaces 之前）
    let mut child_cred = cx.cred();
    if flags & CLONE_NEWUSER != 0 {
        super::ns::new_user_ns(&mut child_cred, super::ns::chrooted(cx))?;
    }
    if flags & CLONE_NEW_ADMIN != 0 && !child_cred.capable_here(cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    // 新的 IPC 命名空间里不能再共用原来的 SEM_UNDO 撤销值
    if both(CLONE_NEWIPC, CLONE_SYSVSEM) {
        return Err(Errno::EINVAL);
    }
    let pid_ns = if flags & CLONE_NEWPID != 0 {
        super::ns::new_pid_ns(&active, &for_children, child_cred.user_ns.clone())?
    } else {
        for_children
    };
    // 新的 UTS、IPC 命名空间属于新进程的用户命名空间
    let child_cred_owner = child_cred.user_ns.clone();
    // 线程数的限制（Linux 的 copy_process）：RLIMIT_NPROC 按真实用户 ID 数线程，root 和
    // 有 CAP_SYS_RESOURCE 或 CAP_SYS_ADMIN 的不受它限制；threads-max 对谁都有效
    {
        let cred = cx.cred();
        let nproc = cx.t.group.borrow().rlimits[process::RLIMIT_NPROC as usize].0;
        let k = cx.kr();
        if cred.ruid != 0
            && !cred.capable(cap::SYS_RESOURCE)
            && !cred.capable(cap::SYS_ADMIN)
            && user_threads(k, cred.ruid) >= nproc
        {
            return Err(Errno::EAGAIN);
        }
        if k.tasks.len() >= k.threads_max {
            return Err(Errno::EAGAIN);
        }
    }
    let cred = cx.cred();
    let tid = cx.k().choose_pid(&pid_ns, &cred, &args.set_tid)?;

    let mm = if flags & CLONE_VM != 0 {
        cx.t.mm.clone()
    } else {
        // 先让同一地址空间里的其他线程停下来，复制出来的才是 fork 那一刻的快照
        cx.mm().quiesce(&cx.task.sync);
        cx.mm_mut().fork()?
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
        let mut copy = FsContext {
            root: f.root.clone(),
            cwd: f.cwd.clone(),
            umask: f.umask,
            mnt_ns: f.mnt_ns.clone(),
        };
        if flags & CLONE_NEWNS != 0 {
            let owner = child_cred.user_ns.clone();
            super::ns::new_mnt_ns(cx.t.vfs.as_deref(), &mut copy, owner);
        }
        Rc::new(RefCell::new(copy))
    };
    let group = if thread {
        cx.t.group.clone()
    } else {
        let parent = cx.t.group.borrow();
        let sighand = if flags & CLONE_SIGHAND != 0 {
            parent.sighand.clone()
        } else {
            let mut h = parent.sighand.borrow().clone();
            // 与 execve 一样：装了处理函数的恢复默认，忽略的仍然忽略
            if flags & CLONE_CLEAR_SIGHAND != 0 {
                h.reset_on_exec();
            }
            Rc::new(RefCell::new(h))
        };
        let ppid = if flags & CLONE_PARENT != 0 {
            parent.ppid
        } else {
            parent.tgid
        };
        let mut g = ThreadGroup::new(tid, ppid, sighand);
        g.parent_thread = if flags & CLONE_PARENT != 0 {
            parent.parent_thread
        } else {
            cx.tid()
        };
        g.pgid = parent.pgid;
        g.sid = parent.sid;
        g.rlimits = parent.rlimits;
        g.dumpable = parent.dumpable;
        g.no_new_privs = parent.no_new_privs;
        g.oom_score_adj = parent.oom_score_adj;
        g.oom_score_adj_min = parent.oom_score_adj_min;
        g.cred = child_cred;
        g.personality = parent.personality;
        g.thp_disable = parent.thp_disable;
        g.coredump_filter = parent.coredump_filter;
        g.ctty = parent.ctty.clone();
        g.exe = parent.exe.clone();
        g.exit_signal = args.exit_signal;
        g.pid_ns = pid_ns.clone();
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
        vfork_done: vfork_done.clone().map(|d| (d, cx.task.event.clone())),
        restart: None,
        fast_sleep: None,
    };
    let tgid = if thread { cx.tgid() } else { tid };
    let mut ns = cx.ns();
    if flags & CLONE_NEWUTS != 0 {
        ns.uts = ns.uts.copy(child_cred_owner.clone());
    }
    if flags & CLONE_NEWIPC != 0 {
        ns.ipc = cx.k().new_ipc_ns(child_cred_owner.clone());
    }
    if flags & CLONE_NEWNET != 0 {
        ns.net = cx.k().new_net_ns(child_cred_owner.clone())?;
    }
    if flags & CLONE_NEWCGROUP != 0 {
        ns.cgroup = super::ns::new_cgroup_ns(child_cred_owner.clone());
    }
    let task = Task::new(
        tid,
        tgid,
        &child_cpu,
        state,
        ns,
        cx.task.sigmask.get(),
        cx.task.comm.get(),
    )?;
    task.affinity.set(cx.task.affinity.get());
    let (sched, nice) = super::sched::inherit(cx.task.sched.get(), cx.task.nice.get());
    task.sched.set(sched);
    task.nice.set(nice);
    *task.ioprio.borrow_mut() = super::sched::inherit_ioprio(&cx.task.ioprio.borrow(), flags);
    task.deadline.set(cx.task.deadline.get());
    let slack = cx.task.timerslack.get().0;
    task.timerslack.set((slack, slack));
    // seccomp 的模式和过滤器链与 Linux 一样继承
    let seccomp = cx.task.seccomp.borrow().clone();
    *task.seccomp.borrow_mut() = seccomp;
    crate::seccomp::publish(&task);
    // 被跟踪的线程 clone 出来的可能也被跟踪（crate::ptrace）
    let traced = crate::ptrace::clone(cx.task, &task, flags, args.exit_signal);

    // 在 pid 命名空间的各层分配编号：父进程看到的是它那层的，子线程自己看到的是它那层的
    cx.k().register_pid(&pid_ns, tid, &args.set_tid)?;
    let (vtid, child_vtid) = (cx.pid_out(tid), pid_ns.vnr(tid).unwrap_or(0));
    // 写 tid 之前先确认能启动线程，失败时不留下任何痕迹
    let started = (|| {
        if flags & CLONE_PARENT_SETTID != 0 {
            put(cx, ptid, vtid.to_le_bytes())?;
        }
        if flags & CLONE_CHILD_SETTID != 0 {
            mm.borrow()
                .write(UserSlice::new(ctid, 4), &child_vtid.to_le_bytes())?;
        }
        // CLONE_PIDFD：在父进程里打开新进程的 pidfd（没有 CLONE_FILES 时子进程看不到它）
        let pidfd = if flags & CLONE_PIDFD != 0 {
            let fd = cx.fds_mut().install(new_pidfd(tid, &group, 0), true, 0)?;
            if let Err(e) = put(cx, args.pidfd, fd.to_le_bytes()) {
                let _ = cx.fds_mut().close(fd);
                return Err(e);
            }
            Some(fd)
        } else {
            None
        };
        let shared = cx.shared;
        if let Err(e) = task::start(shared, cx.k(), task, child_cpu) {
            if let Some(fd) = pidfd {
                let _ = cx.fds_mut().close(fd);
            }
            return Err(e);
        }
        Ok(())
    })();
    if let Err(e) = started {
        cx.k().release_pid(tid);
        return Err(e);
    }
    if !thread {
        let ppid = group.borrow().ppid;
        let k = cx.k();
        k.forks += 1;
        k.groups.insert(tid, group);
        if let Some(parent) = k.groups.get(&ppid) {
            parent.borrow_mut().children.push(tid);
        }
    }

    // 跟踪者打开了这个事件：报告新线程（PTRACE_EVENT_FORK、VFORK、CLONE），编号是跟踪者
    // 看到的（Linux 的 ptrace_event_pid）
    if let Some(e) = traced
        && !crate::ptrace::event(cx, cpu, e, crate::ptrace::tracer_pid_of(cx, tid) as u64)
    {
        return Ok(vtid as u64);
    }
    // vfork：等子进程 execve 或退出；只有致命信号能打断（Linux 的 TASK_KILLABLE）。两者都
    // 直接叫醒这个线程（task::release_mm、signal::group_exit）
    if let Some(done) = vfork_done {
        while !done.get() && !cx.task.killed.get() {
            cx.wait_alone(None)?;
        }
        let pid = crate::ptrace::tracer_pid_of(cx, tid);
        crate::ptrace::event(cx, cpu, crate::ptrace::event::VFORK_DONE, pid as u64);
    }
    Ok(vtid as u64)
}

/// 真实用户 ID 是 `uid` 的线程数（`RLIMIT_NPROC`）。
fn user_threads(k: &crate::kernel::Kernel, uid: u32) -> u64 {
    k.groups
        .values()
        .map(|g| g.borrow())
        .filter(|g| g.cred.ruid == uid)
        .map(|g| g.threads.len() as u64)
        .sum()
}

/// 读取 execve 的 argv 或 envp：以 NULL 结尾的字符串指针数组。字符串和指针一共超过
/// `limit` 字节（[`loader::arg_limit`]）时返回 E2BIG。
fn read_strings(
    cx: &Cx<'_>,
    addr: u64,
    total: &mut usize,
    limit: usize,
) -> Result<Vec<Vec<u8>>, Errno> {
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
        if *total > limit {
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

/// 给程序和解释器的映射起名字（`/proc/<pid>/maps`）。`exe`、`interp` 是名字和在仓库里的
/// inode（不知道时是 0），解释器的见 [`interp_label`]。
pub(crate) fn label_image(
    mm: &mut Mm,
    image: &loader::Image,
    exe: (&Rc<[u8]>, u64),
    interp: Option<(Rc<[u8]>, u64)>,
) {
    mm.label(
        image.main.0,
        image.main.1 - image.main.0,
        Some(exe.0.clone()),
        exe.1,
    );
    if let (Some((_, start, end)), Some((name, ino))) = (&image.interp, interp) {
        mm.label(*start, end - start, Some(name), ino);
    }
}

/// 程序文件的能力（`security.capability`）；没有时是 None，格式不对时 execve 失败（EINVAL）。
fn file_caps(vfs: &Vfs, ino: Ino) -> Result<Option<FileCaps>, Errno> {
    let store = vfs.store();
    if !store.has_file_caps(ino)? {
        return Ok(None);
    }
    match store.get_xattr(ino, XATTR_NAME_CAPS) {
        Ok(v) => FileCaps::parse(&v),
        Err(Errno::ENODATA) => Ok(None),
        Err(e) => Err(e),
    }
}

/// 解释器在 maps 里的名字和 inode：名字与主程序一样解析完符号链接（Debian 的
/// `/lib/ld-linux-aarch64.so.1` 是链接）；找不到时是 PT_INTERP 原样的路径。
pub(crate) fn interp_label(
    vfs: Option<&Vfs>,
    ps: &dyn Pseudo,
    cred: &Cred,
    root: &Node,
    cwd: &Node,
    image: &loader::Image,
) -> Option<(Rc<[u8]>, u64)> {
    let (path, ..) = image.interp.as_ref()?;
    let walked = vfs.and_then(|vfs| Some((vfs, vfs.walk(ps, cred, root, cwd, path, true).ok()?)));
    let Some((vfs, (node, found))) = walked else {
        return Some((Rc::from(path.as_slice()), 0));
    };
    let name = vfs
        .found_path(ps, root, &node, found.as_ref())
        .unwrap_or_else(|_| path.clone());
    let ino = node.ino().unwrap_or(0);
    Some((Rc::from(name), ino))
}

fn load_error(e: LoadError) -> Errno {
    match e {
        LoadError::NotElf | LoadError::Unsupported(_) | LoadError::Malformed(_) => Errno::ENOEXEC,
        LoadError::ArgumentsTooLong => Errno::E2BIG,
        LoadError::Memory(e) | LoadError::Read(e) | LoadError::Interpreter(_, e) => e,
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
    exec(cx, cpu, AT_FDCWD, path, argv, envp, 0)
}

/// execveat：路径相对 `dirfd`；`AT_EMPTY_PATH` 且路径为空时执行 `dirfd` 本身（fexecve）。
pub(super) fn execveat(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    dirfd: i32,
    path: u64,
    (argv, envp): (u64, u64),
    flags: u32,
) -> Result<(), Errno> {
    if flags & !(AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW) != 0 {
        return Err(Errno::EINVAL);
    }
    let path = read_path(cx, path)?;
    exec(cx, cpu, dirfd, path, argv, envp, flags)
}

fn exec(
    cx: &mut Cx<'_>,
    cpu: &mut Cpu,
    dirfd: i32,
    path: Vec<u8>,
    argv: u64,
    envp: u64,
    flags: u32,
) -> Result<(), Errno> {
    let mut stack_limit = cx.t.group.borrow().rlimits[process::RLIMIT_STACK as usize].0;
    let limit = loader::arg_limit(stack_limit);
    let mut total = 0;
    let mut argv = read_strings(cx, argv, &mut total, limit)?;
    let envp = read_strings(cx, envp, &mut total, limit)?;
    // 与 Linux 5.18 起相同：argv 为空时补一个空字符串，新程序的 argc 至少是 1
    if argv.is_empty() {
        argv.push(Vec::new());
    }
    let vfs = cx.vfs()?;
    let files = cx.k().file_maps.clone();
    let cwd = cx.cwd();
    let view = cx.view(&vfs);
    let cred = cx.cred();

    // 新程序看到的路径（AT_EXECFN、comm、脚本解释器的参数）：路径不是从当前目录或者根
    // 目录出发时，与 Linux 一样是 /dev/fd/<dirfd>[/<路径>]
    let by_fd = dirfd != AT_FDCWD && !path.starts_with(b"/");
    let filename = match (by_fd, path.is_empty()) {
        (false, _) => path.clone(),
        (true, true) => format!("/dev/fd/{dirfd}").into_bytes(),
        (true, false) => [format!("/dev/fd/{dirfd}/").as_bytes(), &path].concat(),
    };
    // /proc/<pid>/exe 的路径从哪里算：(起点目录, 相对它的路径)
    let mut exe_at = (cwd.clone(), path.clone());
    // comm 取路径的最后一段；执行的是 fd 本身时取文件名，不是 fd 的编号
    let mut comm = filename.clone();
    let root = cx.root();
    let node = if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        if dirfd == AT_FDCWD {
            cwd.clone()
        } else {
            let file = cx.fds().get(dirfd)?;
            let f = file.borrow();
            exe_at.1 = f.path.as_deref().map(<[u8]>::to_vec).unwrap_or_default();
            if !exe_at.1.is_empty() {
                comm.clone_from(&exe_at.1);
            }
            f.node().ok_or(Errno::EACCES)?
        }
    } else {
        let start = super::fs::start_dir(cx, dirfd, &path)?;
        exe_at.0 = start.clone();
        let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
        vfs.resolve(&view, &cred, &root, &start, &path, follow)?
    };
    let (mut file, mut st, mut nosuid) = process::open_node(&vfs, &cred, &node)?;

    // `#!` 脚本：换成解释器，argv 变成 [解释器, 参数, 脚本路径, 原来的 argv[1..]]
    let mut file_path = filename.clone();
    for depth in 0.. {
        let Some((interp, arg)) = parse_shebang(file.head()) else {
            break;
        };
        if depth == MAX_INTERP_DEPTH {
            return Err(Errno::ELOOP);
        }
        // 解释器要按路径打开脚本，close-on-exec 的 dirfd 到时已经关了
        if depth == 0 && by_fd && cx.fds().cloexec(dirfd)? {
            return Err(Errno::ENOENT);
        }
        let mut new_argv = vec![interp.clone()];
        new_argv.extend(arg);
        new_argv.push(file_path);
        new_argv.extend(argv.into_iter().skip(1));
        argv = new_argv;
        (file, st, nosuid) = process::open_executable(&vfs, &view, &cred, &root, &cwd, &interp)?;
        exe_at = (cwd.clone(), interp.clone());
        file_path = interp;
    }
    // 以写方式打开着的程序不能执行
    if super::fs::open_for_write(cx, st.ino) {
        return Err(Errno::ETXTBSY);
    }
    // /proc/<pid>/exe 和 maps 里显示的绝对路径，符号链接全部解析：musl 按它展开主程序
    // 的 `$ORIGIN`，经由 /usr/bin/java 这种链接启动的程序靠它找到自己的库
    let (exe_dir, exe_path) = &exe_at;
    let exe: Rc<[u8]> = match vfs.real_path(&view, &cred, &root, exe_dir, exe_path) {
        Ok(p) => Rc::from(p),
        Err(_) if exe_path.starts_with(b"/") => Rc::from(normalize(exe_path)),
        Err(_) => {
            let mut p = vfs.path_of(&view, &root, exe_dir)?;
            p.push(b'/');
            p.extend_from_slice(exe_path);
            Rc::from(normalize(&p))
        }
    };

    // setuid、setgid 程序（`PR_SET_NO_NEW_PRIVS` 之后、nosuid 的挂载里不生效）和程序文件的
    // 能力；脚本看的是解释器。被没有 CAP_SYS_PTRACE 的跟踪者跟踪着时得不到新的特权
    let no_new_privs = cx.t.group.borrow().no_new_privs;
    // 文件的属主、属组在自己的用户命名空间里没有映射时也不生效（bprm_fill_uid）
    let mapped = cred.user_ns.uid_map.borrow().up(st.uid).is_some()
        && cred.user_ns.gid_map.borrow().up(st.gid).is_some();
    let setid = |bit: u32| st.mode & bit == bit && !no_new_privs && !nosuid && mapped;
    let fcaps = if nosuid {
        None
    } else {
        file_caps(&vfs, st.ino)?
    };
    let ptraced = {
        let t = cx.task.trace.borrow();
        t.tracer.is_some() && !t.privileged
    };
    let mut new_cred = cred.clone();
    let secure = new_cred.exec(
        setid(mode::S_ISUID).then_some(st.uid),
        setid(mode::S_ISGID | 0o010).then_some(st.gid),
        fcaps,
        no_new_privs,
        ptraced,
    )?;
    // 与 Linux 一样，特权执行的程序的栈大小上限不超过默认值，免得调用者用它影响新程序的
    // 地址空间布局；新的值在 execve 成功之后写回
    if secure {
        stack_limit = stack_limit.min(loader::STACK_SIZE);
    }
    let args = Args {
        argv: &argv,
        envp: &envp,
        execfn: &filename,
        ids: [new_cred.ruid, new_cred.euid, new_cred.rgid, new_cred.egid],
        secure,
        stack_limit,
    };
    let mut read_interp = |p: &[u8]| process::read_executable(&vfs, &view, &cred, &root, &cwd, p);
    let prepared = loader::prepare(&file, &mut read_interp).map_err(load_error)?;
    // 新的地址空间放开 BKL 建：映射程序的各段、构造初始栈占了 execve 持锁时间的大半，
    // 管道两头的进程、并行构建里同时 exec 的进程原来要排队。这期间只碰新的地址空间和
    // 所有地址空间共用的只读文件映射（FileMaps，自己有锁）
    let built = cx.unlocked(|| {
        let mut mm = Mm::new();
        mm.share_files(&files);
        loader::load_prepared(&mut mm, &prepared, &args).map(|image| (mm, image))
    });
    // 等 BKL 的时候被结束了（别的线程 exit_group、execve，或者致命信号）
    if cx.task.killed.get() {
        return Err(Errno::EINTR);
    }
    let (mut mm, image) = built.map_err(load_error)?;
    let view = cx.view(&vfs);
    let interp = interp_label(Some(&*vfs), &view, &cred, &root, &cwd, &image);
    label_image(&mut mm, &image, (&exe, st.ino), interp);
    mm.exe = Some(st.ino);

    // 从这里开始不能失败：结束同一线程组里的其他线程，然后换掉地址空间
    kill_other_threads(cx)?;
    if Rc::strong_count(&cx.t.fds) > 1 {
        let copy = cx.fds().clone();
        cx.set_fds(Rc::new(RefCell::new(copy)));
    }
    let closed = cx.fds_mut().close_on_exec();
    let fds = cx.t.fds.clone();
    crate::locks::closed(cx.k(), &fds, &closed);
    crate::unix::released(cx.k(), closed);
    {
        let group = cx.t.group.clone();
        let mut g = group.borrow_mut();
        // 驻留内存的峰值跨 execve 保留：换掉地址空间之前取一次样（在标记 execed 之前）
        g.sample_rss_at_exec(&cx.mm());
        g.exe = exe.to_vec();
        // 与 Linux 的 commit_creds 一样，换了有效 ID、得到了新的能力时不能 core dump，
        // 父进程退出时也不再发信号给它；特权执行同样不再发。别的执行恢复成可以 dump
        // （begin_new_exec），程序文件不让调用者读时除外（would_dump）
        let ids = |c: &Cred| (c.euid, c.egid, c.fsuid, c.fsgid);
        let gained = new_cred.cap_permitted & !g.cred.cap_permitted != 0;
        if (new_cred.euid, new_cred.egid) != (new_cred.ruid, new_cred.rgid)
            || ids(&new_cred) != ids(&g.cred)
            || gained
        {
            g.dumpable = false;
            cx.task.pdeathsig.set(0);
        } else {
            g.dumpable = cred.permission(&st, crate::cred::MAY_READ).is_ok();
        }
        if secure {
            cx.task.pdeathsig.set(0);
        }
        g.cred = new_cred;
        cx.shared.fast_changed();
        g.rlimits[process::RLIMIT_STACK as usize].0 = stack_limit;
        g.posix_timers.clear();
        // AIO 上下文的环在旧的地址空间里，随之消失
        let contexts = std::mem::take(&mut g.aio);
        drop(g);
        super::aio::released(cx.k(), contexts);
        let mut g = group.borrow_mut();
        g.execed = true;
        if Rc::strong_count(&g.sighand) > 1 {
            let copy = g.sighand.borrow().clone();
            g.sighand = Rc::new(RefCell::new(copy));
        }
        g.sighand.borrow_mut().reset_on_exec();
    }
    let task = cx.task.clone();
    task::release_mm(cx.shared, &task, cx.t);
    let mm = mm.into_shared();
    mm.borrow_mut().add_user(task.sync.clone());
    // SAFETY: 新的地址空间由 TaskState 持有；旧的可能随这次赋值释放，cpu 已经不再使用它
    unsafe { cpu.reset(mm.borrow().mmu()) };
    cx.set_mm(mm);
    cx.t.altstack = AltStack::default();
    cx.task.saved_sigmask.set(None);
    cx.task.comm.set(process::comm_of(&comm));
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
        k.release_pid(me);
        let children = {
            let mut g = cx.t.group.borrow_mut();
            g.threads = vec![tgid];
            // 原来的主线程已经被回收了（Linux 的 de_thread 里 release_task）
            g.leader_seccomp = None;
            g.children.clone()
        };
        // 别的线程建的子进程在它们退出时已经交给了自己，现在自己的编号变了
        for c in children {
            if let Some(cg) = cx.kr().groups.get(&c) {
                let mut cg = cg.borrow_mut();
                if cg.parent_thread == me {
                    cg.parent_thread = tgid;
                }
            }
        }
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

/// 子进程（或者被跟踪的线程）的状态变化。
#[derive(Clone, Copy)]
enum ChildEvent {
    Exited(ExitStatus),
    Stopped(Signal),
    Continued,
    /// 被跟踪的线程停在 ptrace-stop 里，带着报告给跟踪者的状态（`crate::ptrace`）。
    Trapped(i32),
}

impl ChildEvent {
    /// wait4 的状态字。
    fn wait_status(self) -> i32 {
        match self {
            ChildEvent::Exited(s) => s.wait_status(),
            ChildEvent::Stopped(sig) => sig.number() << 8 | 0x7f,
            ChildEvent::Continued => 0xffff,
            ChildEvent::Trapped(code) => code << 8 | 0x7f,
        }
    }

    /// waitid 的 siginfo。
    fn siginfo(self, pid: Tid) -> SigInfo {
        match self {
            ChildEvent::Exited(s) => SigInfo::child(pid, s),
            ChildEvent::Stopped(sig) => SigInfo::child_event(pid, code::CLD_STOPPED, sig.number()),
            ChildEvent::Continued => {
                SigInfo::child_event(pid, code::CLD_CONTINUED, Signal::CONT.number())
            }
            ChildEvent::Trapped(status) => SigInfo::child_event(pid, code::CLD_TRAPPED, status),
        }
    }
}

/// 要等哪些状态变化（`WEXITED`、`WUNTRACED`/`WSTOPPED`、`WCONTINUED`），是否取走事件，以及
/// 是否只等调用的线程建的子进程（`__WNOTHREAD`）。
#[derive(Clone, Copy)]
struct Wanted {
    exited: bool,
    stopped: bool,
    continued: bool,
    consume: bool,
    own_thread: bool,
}

/// 找到的子进程状态变化：(pid, 事件, 子进程和它回收过的子进程用掉的 CPU 时间)。
/// 子进程（调用者看到的编号）、它的状态变化和资源用量。
type Found = (Tid, ChildEvent, process::Rusage);

/// 找到一个符合条件的子进程状态变化，`consume` 时取走它（已结束的子进程同时被回收）。
/// 跟踪着的线程（不一定是子进程）的 ptrace-stop 和退出也算。没有子进程、跟踪着的线程
/// 符合条件时返回 ECHILD。
fn find_event(cx: &mut Cx<'_>, who: Who, want: Wanted) -> Result<Option<Found>, Errno> {
    let children = cx.t.group.borrow().children.clone();
    let me = cx.tgid();
    let matches = |pid: Tid, pgid: Tid| match who {
        Who::Pid(p) => p == pid,
        Who::Any => true,
        Who::Pgid(g) => g == pgid,
    };
    // 与 Linux 一样，子进程的用量包括它回收的子进程。找到事件才算：活着的线程的内核态
    // 时间要读宿主的 /proc，wait 每次被叫醒都会重新扫一遍子进程
    let usage = |g: &ThreadGroup, k: &crate::kernel::Kernel| process::Rusage {
        cpu: g.cpu(k) + g.child_cpu,
        maxrss_kb: g.maxrss_kb.max(g.child_maxrss_kb),
    };
    let mut any = false;
    let tid = cx.tid();
    for c in children {
        let Some(child) = cx.kr().groups.get(&c).cloned() else {
            continue;
        };
        if !matches(c, child.borrow().pgid) {
            continue;
        }
        // __WNOTHREAD：别的线程建的子进程看不见（Linux 的 do_wait 只看这个线程的 children）
        if want.own_thread && child.borrow().parent_thread != tid {
            continue;
        }
        any = true;
        // 主线程被自己跟踪着：停止和继续按 ptrace-stop 报告（下面），与 Linux 一样不重复报告
        let traced = cx
            .kr()
            .tasks
            .get(&c)
            .is_some_and(|t| t.trace.borrow().tracer == Some(me));
        let mut g = child.borrow_mut();
        // 结束了、跟踪者还没取走：只有跟踪者看得见（下面的 trace_exits），父进程接着等
        if g.exit_tracer.is_some() {
            continue;
        }
        // 回收之前换算：回收之后编号就不在 pid 命名空间里了
        let vpid = cx.pid_out(c);
        if let (true, Some(status)) = (want.exited, g.zombie) {
            let usage = usage(&g, cx.kr());
            if want.consume {
                drop(g);
                cx.k().groups.remove(&c);
                cx.k().release_pid(c);
                let mut me = cx.t.group.borrow_mut();
                me.children.retain(|&x| x != c);
                me.child_cpu += usage.cpu;
                me.child_maxrss_kb = me.child_maxrss_kb.max(usage.maxrss_kb);
            }
            return Ok(Some((vpid, ChildEvent::Exited(status), usage)));
        }
        if traced {
            continue;
        }
        if let (true, Some(sig)) = (want.stopped, g.stop_report) {
            if want.consume {
                g.stop_report = None;
            }
            return Ok(Some((vpid, ChildEvent::Stopped(sig), usage(&g, cx.kr()))));
        }
        if want.continued && g.cont_report {
            if want.consume {
                g.cont_report = false;
            }
            return Ok(Some((vpid, ChildEvent::Continued, usage(&g, cx.kr()))));
        }
    }
    // 跟踪着的线程：ptrace-stop 不要求 WUNTRACED（与 Linux 一样，也不要求 __WALL）
    let tracees: Vec<_> = cx
        .kr()
        .tasks
        .values()
        .filter(|t| t.trace.borrow().tracer == Some(me))
        .cloned()
        .collect();
    for t in tracees {
        let pgid = cx.kr().groups.get(&t.tgid).map_or(0, |g| g.borrow().pgid);
        if !matches(t.tid.get(), pgid) {
            continue;
        }
        any = true;
        let mut trace = t.trace.borrow_mut();
        if let (true, false, Some(code)) = (trace.stopped(), trace.reported, trace.exit_code()) {
            if want.consume {
                trace.reported = true;
            }
            // 与 Linux 一样报告它所在进程的用量（strace -c 用它算系统调用的时间）
            let usage = cx
                .kr()
                .groups
                .get(&t.tgid)
                .map_or_else(process::Rusage::default, |g| usage(&g.borrow(), cx.kr()));
            return Ok(Some((
                cx.pid_out(t.tid.get()),
                ChildEvent::Trapped(code),
                usage,
            )));
        }
    }
    // 跟踪着的线程退出了
    let exits = &cx.kr().trace_exits;
    if let Some(i) = exits
        .iter()
        .position(|e| e.tracer == me && matches(e.pid, e.pgid))
    {
        any = true;
        if want.exited {
            let (pid, seen, status) = (exits[i].pid, exits[i].seen, exits[i].status);
            // 整个进程的结束：与 Linux 一样报告它的用量（它还在，等着交给父进程）
            let usage = match cx.kr().groups.get(&pid) {
                Some(g) if exits[i].process => usage(&g.borrow(), cx.kr()),
                _ => process::Rusage::default(),
            };
            if want.consume {
                let e = cx.k().trace_exits.remove(i);
                crate::ptrace::released(cx.k(), &e);
            }
            return Ok(Some((seen, ChildEvent::Exited(status), usage)));
        }
    }
    if any { Ok(None) } else { Err(Errno::ECHILD) }
}

/// wait4 和 waitid 的公共部分：等到有子进程的状态变化。
fn wait_child(
    cx: &mut Cx<'_>,
    who: Who,
    nohang: bool,
    want: Wanted,
) -> Result<Option<Found>, Errno> {
    loop {
        if let Some(found) = find_event(cx, who, want)? {
            return Ok(Some(found));
        }
        if nohang {
            return Ok(None);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        // 只有子进程的状态变化（和直接通知的信号）会让这里的条件成立（crate::kernel::wake）
        cx.wait_on(None, &mut [], crate::kernel::wake::CHILD)?;
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
    // -INT_MIN 不能表示成进程组号
    if pid == i32::MIN {
        return Err(Errno::ESRCH);
    }
    let who = match pid {
        -1 => Who::Any,
        0 => Who::Pgid(cx.t.group.borrow().pgid),
        p if p < 0 => Who::Pgid(-p),
        p => Who::Pid(p),
    };
    let want = Wanted {
        exited: true,
        stopped: options & WUNTRACED != 0,
        continued: options & WCONTINUED != 0,
        consume: true,
        own_thread: options & WNOTHREAD != 0,
    };
    let Some((child, event, usage)) = wait_child(cx, who, options & WNOHANG != 0, want)? else {
        return Ok(0);
    };
    if wstatus != 0 {
        put(cx, wstatus, event.wait_status().to_le_bytes())?;
    }
    if rusage != 0 {
        put(cx, rusage, usage.encode())?;
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
    let mut nonblock = false;
    let who = match idtype {
        P_ALL => Who::Any,
        P_PID if id > 0 => Who::Pid(id),
        P_PGID if id > 0 => Who::Pgid(id),
        P_PGID if id == 0 => Who::Pgid(cx.t.group.borrow().pgid),
        // pidfd 指向的进程已经被回收、pid 分给了别人时，与 Linux 一样等不到它
        P_PIDFD if id >= 0 => {
            let file = cx.fds().get(id)?;
            let f = file.borrow();
            let FileKind::Pidfd(p) = &f.kind else {
                return Err(Errno::EBADF);
            };
            nonblock = f.nonblocking();
            match p.group(cx.kr()) {
                Some(_) => Who::Pid(p.tgid),
                None => return Err(Errno::ECHILD),
            }
        }
        _ => return Err(Errno::EINVAL),
    };
    let want = Wanted {
        exited: options & WEXITED != 0,
        stopped: options & WUNTRACED != 0,
        continued: options & WCONTINUED != 0,
        consume: options & WNOWAIT == 0,
        own_thread: options & WNOTHREAD != 0,
    };
    let nohang = options & WNOHANG != 0;
    let found = wait_child(cx, who, nohang || nonblock, want)?;
    // 非阻塞的 pidfd：没有事件时返回 EAGAIN，调用者自己要求 WNOHANG 时除外
    if found.is_none() && nonblock && !nohang {
        return Err(Errno::EAGAIN);
    }
    if infop != 0 {
        let info = match found {
            Some((child, event, _)) => event.siginfo(child).encode(Signal::CHLD),
            None => [0; 128],
        };
        cx.mm().write(UserSlice::new(infop, 128), &info)?;
    }
    if rusage != 0 {
        let usage = found.map_or(process::Rusage::default(), |f| f.2);
        put(cx, rusage, usage.encode())?;
    }
    Ok(0)
}

/// 指向线程组 `group`（pid 为 `tgid`）的新的 pidfd。
fn new_pidfd(tgid: Tid, group: &Rc<RefCell<ThreadGroup>>, flags: u32) -> FileRef {
    let p = Pidfd {
        tgid,
        group: Rc::downgrade(group),
    };
    OpenFile::new(FileKind::Pidfd(p), oflags::O_RDWR | flags)
}

/// pidfd_open：`flags` 只能是 `PIDFD_NONBLOCK`（6.9 起的 `PIDFD_THREAD` 不支持），
/// pid 必须是进程而不是线程。pidfd 总是 close-on-exec。
pub(super) fn pidfd_open(cx: &mut Cx<'_>, pid: i32, flags: u32) -> SysResult {
    if pid <= 0 || flags & !oflags::O_NONBLOCK != 0 {
        return Err(Errno::EINVAL);
    }
    let Some(group) = cx.kr().groups.get(&pid).cloned() else {
        let thread = cx.kr().tasks.contains_key(&pid);
        return Err(if thread { Errno::EINVAL } else { Errno::ESRCH });
    };
    let file = new_pidfd(pid, &group, flags);
    cx.fds_mut().install(file, true, 0).map(|fd| fd as u64)
}

/// 能不能像 ptrace 那样访问 `target`（`PTRACE_MODE_ATTACH_REALCREDS`）：是自己，或者
/// [`Cred::may_ptrace`]。
pub(super) fn may_ptrace(cx: &Cx<'_>, target: &ThreadGroup) -> bool {
    target.tgid == cx.tgid() || cx.cred().may_ptrace(&target.cred, target.dumpable, false)
}

/// pidfd_getfd：把 pidfd 指向的进程的 `target` 复制一份到调用者，带 close-on-exec。
pub(super) fn pidfd_getfd(cx: &mut Cx<'_>, pidfd: i32, target: i32, flags: u32) -> SysResult {
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let file = cx.fds().get(pidfd)?;
    let group = match &file.borrow().kind {
        FileKind::Pidfd(p) => p.group(cx.kr()).ok_or(Errno::ESRCH)?,
        _ => return Err(Errno::EBADF),
    };
    let leader = group
        .borrow()
        .threads
        .first()
        .copied()
        .ok_or(Errno::ESRCH)?;
    if !may_ptrace(cx, &group.borrow()) {
        return Err(Errno::EPERM);
    }
    let task = cx.kr().tasks.get(&leader).cloned().ok_or(Errno::ESRCH)?;
    let fds = task.res.borrow().fds.clone();
    let remote = fds.borrow().get(target)?;
    cx.fds_mut().install(remote, true, 0).map(|fd| fd as u64)
}

/// kcmp：两个线程的某种资源是不是同一个。相同返回 0，不同时按对象的地址返回 1 或 2
/// （Linux 的地址是打乱过的，只保证顺序一致）。
pub(super) fn kcmp(
    cx: &mut Cx<'_>,
    pid1: i32,
    pid2: i32,
    ty: i32,
    idx1: u64,
    idx2: u64,
) -> SysResult {
    const KCMP_FILE: i32 = 0;
    const KCMP_VM: i32 = 1;
    const KCMP_FILES: i32 = 2;
    const KCMP_FS: i32 = 3;
    const KCMP_SIGHAND: i32 = 4;
    const KCMP_IO: i32 = 5;
    const KCMP_SYSVSEM: i32 = 6;
    const KCMP_EPOLL_TFD: i32 = 7;
    let task = |pid: i32| cx.kr().tasks.get(&pid).cloned().ok_or(Errno::ESRCH);
    let (t1, t2) = (task(pid1)?, task(pid2)?);
    for t in [&t1, &t2] {
        let group = cx.kr().groups.get(&t.tgid).cloned().ok_or(Errno::ESRCH)?;
        if !may_ptrace(cx, &group.borrow()) {
            return Err(Errno::EPERM);
        }
    }
    let file = |t: &Task, fd: u64| -> Result<usize, Errno> {
        let fds = t.res.borrow().fds.clone();
        let f = fds
            .borrow()
            .get(i32::try_from(fd).map_err(|_| Errno::EBADF)?)?;
        Ok(Rc::as_ptr(&f) as usize)
    };
    let sighand = |t: &Task| -> usize {
        cx.kr()
            .groups
            .get(&t.tgid)
            .map_or(0, |g| Rc::as_ptr(&g.borrow().sighand) as usize)
    };
    let (a, b) = match ty {
        KCMP_FILE => (file(&t1, idx1)?, file(&t2, idx2)?),
        KCMP_VM => {
            let vm = |t: &Task| Rc::as_ptr(&t.res.borrow().mm) as usize;
            (vm(&t1), vm(&t2))
        }
        KCMP_FILES => {
            let files = |t: &Task| Rc::as_ptr(&t.res.borrow().fds) as usize;
            (files(&t1), files(&t2))
        }
        KCMP_FS => {
            let fs = |t: &Task| Rc::as_ptr(&t.res.borrow().fs) as usize;
            (fs(&t1), fs(&t2))
        }
        KCMP_SIGHAND => (sighand(&t1), sighand(&t2)),
        // 没有 IO 上下文；SysV 信号量的 undo 表不在进程之间共享，按两边都为空算
        KCMP_IO | KCMP_SYSVSEM => (0, 0),
        // idx2 指向 `struct kcmp_epoll_slot { efd, tfd, toff }`：t2 的 epoll 里第 toff 个
        // fd 为 tfd 的关注
        KCMP_EPOLL_TFD => {
            let slot: [u8; 12] = super::get(cx, idx2)?;
            let word = |i: usize| u32::from_le_bytes(slot[i * 4..i * 4 + 4].try_into().unwrap());
            let fds = t2.res.borrow().fds.clone();
            let ep = fds.borrow().get(word(0) as i32)?;
            let target = match &ep.borrow().kind {
                FileKind::Epoll(e) => e.borrow().target(word(1) as i32, word(2) as usize),
                _ => return Err(Errno::EINVAL),
            };
            let target = target.ok_or(Errno::ENOENT)?;
            (file(&t1, idx1)?, Rc::as_ptr(&target) as usize)
        }
        _ => return Err(Errno::EINVAL),
    };
    Ok(match a.cmp(&b) {
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Less => 1,
        std::cmp::Ordering::Greater => 2,
    })
}

/// unshare：不再和别的线程、进程共用某些资源。命名空间只支持挂载命名空间、用户命名空间，
/// 别的与 clone 一样返回 EPERM。与 Linux 一样，`CLONE_THREAD`、`CLONE_SIGHAND`、`CLONE_VM` 只在本来就
/// 没有共用时成功（什么也不用做）。
pub(super) fn unshare(cx: &mut Cx<'_>, flags: u64) -> SysResult {
    let known = CLONE_THREAD
        | CLONE_FS
        | CLONE_SIGHAND
        | CLONE_VM
        | CLONE_FILES
        | CLONE_SYSVSEM
        | CLONE_NEW_MASK;
    let mut flags = flags;
    if flags & CLONE_NEWUSER != 0 {
        flags |= CLONE_THREAD | CLONE_FS;
    }
    if flags & CLONE_VM != 0 {
        flags |= CLONE_SIGHAND;
    }
    if flags & CLONE_SIGHAND != 0 {
        flags |= CLONE_THREAD;
    }
    if flags & CLONE_NEWNS != 0 {
        flags |= CLONE_FS;
    }
    if flags & !known != 0 {
        return Err(Errno::EINVAL);
    }
    let group = cx.t.group.clone();
    let shared = (flags & (CLONE_THREAD | CLONE_SIGHAND | CLONE_VM) != 0
        && group.borrow().threads.len() > 1)
        || (flags & (CLONE_SIGHAND | CLONE_VM) != 0
            && Rc::strong_count(&group.borrow().sighand) > 1)
        || (flags & CLONE_VM != 0 && cx.mm().shared_with_others());
    if shared {
        return Err(Errno::EINVAL);
    }
    if flags & CLONE_NEW_MASK & !CLONE_NEW_DONE != 0 {
        return Err(Errno::EPERM);
    }
    // 先算好新的凭据和各种检查，都通过了才改（Linux 的 ksys_unshare 最后才提交）
    let mut new_cred = cx.cred();
    if flags & CLONE_NEWUSER != 0 {
        super::ns::new_user_ns(&mut new_cred, super::ns::chrooted(cx))?;
    }
    if flags & CLONE_NEW_ADMIN != 0 && !new_cred.capable_here(cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    // 新的 pid 命名空间只给以后的子进程用（自己不进去）；进去之前不能再 unshare 一次
    let pid_ns = if flags & CLONE_NEWPID != 0 {
        let owner = new_cred.user_ns.clone();
        Some(super::ns::new_pid_ns(
            &cx.pid_ns(),
            &cx.pid_ns_children(),
            owner,
        )?)
    } else {
        None
    };
    // 每个线程的 TaskState 和 TaskRes 各持有一份引用
    let others = |count: usize| count > 2;
    if flags & CLONE_FILES != 0 && others(Rc::strong_count(&cx.t.fds)) {
        // 与 Linux 的 dup_fd 一样：已经打开的最大的 fd（按 64 取整）超过 nr_open 时复制不了
        let highest = cx.fds().open_fds().last().map_or(0, |&fd| fd as usize + 1);
        if highest.next_multiple_of(64) > cx.kr().nr_open {
            return Err(Errno::EMFILE);
        }
        let copy = cx.fds().clone();
        cx.set_fds(Rc::new(RefCell::new(copy)));
    }
    if flags & CLONE_FS != 0 && others(Rc::strong_count(&cx.t.fs)) {
        let copy = {
            let f = cx.t.fs.borrow();
            FsContext {
                root: f.root.clone(),
                cwd: f.cwd.clone(),
                umask: f.umask,
                mnt_ns: f.mnt_ns.clone(),
            }
        };
        cx.set_fs(Rc::new(RefCell::new(copy)));
    }
    if flags & CLONE_NEWUSER != 0 {
        cx.t.group.borrow_mut().cred = new_cred.clone();
        cx.shared.fast_changed();
    }
    if flags & CLONE_NEWNS != 0 {
        let owner = new_cred.user_ns.clone();
        super::ns::new_mnt_ns(cx.t.vfs.as_deref(), &mut cx.t.fs.borrow_mut(), owner);
        cx.shared.fast_changed();
    }
    if pid_ns.is_some() {
        *cx.task.pid_ns_children.borrow_mut() = pid_ns;
    }
    // 与 Linux 的 exit_sem 一样，执行并丢掉 SysV 信号量的 undo；换 IPC 命名空间时也要做
    // （原来的命名空间里的信号量以后够不着了）
    if flags & (CLONE_SYSVSEM | CLONE_NEWIPC) != 0 {
        let tgid = cx.tgid();
        cx.ipc_mut().exit(tgid);
    }
    // 新的 UTS 命名空间复制原来的名字，新的 IPC 命名空间是空的，新的网络命名空间里只有
    // 关着的 lo
    if flags & (CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWNET | CLONE_NEWCGROUP) != 0 {
        let owner = new_cred.user_ns.clone();
        let mut ns = cx.ns();
        if flags & CLONE_NEWCGROUP != 0 {
            ns.cgroup = super::ns::new_cgroup_ns(owner.clone());
        }
        if flags & CLONE_NEWNET != 0 {
            ns.net = cx.k().new_net_ns(owner.clone())?;
        }
        if flags & CLONE_NEWUTS != 0 {
            ns.uts = ns.uts.copy(owner.clone());
        }
        if flags & CLONE_NEWIPC != 0 {
            ns.ipc = cx.k().new_ipc_ns(owner);
        }
        *cx.task.ns.borrow_mut() = ns;
        cx.k().collect_ipc();
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
    g.ctty = None;
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
    let (tgid, ppid, sid, execed) = {
        let g = target.borrow();
        (g.tgid, g.ppid, g.sid, g.execed)
    };
    let my_sid = cx.t.group.borrow().sid;
    if tgid != me && ppid != me {
        return Err(Errno::ESRCH);
    }
    if sid != my_sid {
        return Err(Errno::EPERM);
    }
    // 子进程 execve 之后，父进程不能再改它的进程组
    if tgid != me && execed {
        return Err(Errno::EACCES);
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
            core_dumped: false,
        };
        assert_eq!(killed.wait_status(), 15);
    }
}
