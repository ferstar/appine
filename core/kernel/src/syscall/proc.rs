//! 进程与线程：clone、execve、exit、wait4，进程组和会话。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use appine_engine::Cpu;

use super::{Flow, SysResult, put, read_path};
use crate::cred::cap;
use crate::errno::Errno;
use crate::fd::{FileKind, FileRef, OpenFile, Pidfd, oflags};
use crate::kernel::{Cx, Tid};
use crate::loader::{self, Args, LoadError};
use crate::mm::Mm;
use crate::process::{self, ExitStatus, Signal, ThreadGroup};
use crate::signal::{self, AltStack, SigInfo, code};
use crate::task::{self, FsContext, Task, TaskState};
use crate::uaccess::UserSlice;
use crate::vfs::store::mode;
use crate::vfs::{Node, normalize};

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
/// 各种 `CLONE_NEW*`（含 clone3 才能用的 `CLONE_NEWTIME`）：不支持 namespace（设计文档 §6）。
const CLONE_NEW_MASK: u64 = 0x80 | CLONE_NEWNS | 0x7e00_0000;
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
}

pub(super) fn clone(
    cx: &mut Cx<'_>,
    cpu: &Cpu,
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
    };
    do_clone(cx, cpu, args)
}

/// clone3：参数在 `struct clone_args` 里，比 clone 多了 pidfd、栈的大小和 set_tid。
pub(super) fn clone3(cx: &mut Cx<'_>, cpu: &Cpu, uargs: u64, size: usize) -> SysResult {
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
        || set_tid_size > 32
        || (set_tid == 0) != (set_tid_size == 0);
    if invalid {
        return Err(Errno::EINVAL);
    }
    // 不能指定 pid（要 CAP_CHECKPOINT_RESTORE，CRIU 用），也没有 cgroup
    if set_tid_size != 0 || flags & CLONE_INTO_CGROUP != 0 {
        return Err(Errno::EINVAL);
    }
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
    };
    do_clone(cx, cpu, args)
}

fn do_clone(cx: &mut Cx<'_>, cpu: &Cpu, args: CloneArgs) -> SysResult {
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
        || (flags & CLONE_THREAD != 0 && flags & CLONE_SIGHAND == 0)
        || (flags & CLONE_SIGHAND != 0 && flags & CLONE_VM == 0)
        || (flags & CLONE_PIDFD != 0 && flags & (CLONE_DETACHED | CLONE_THREAD) != 0)
    {
        return Err(Errno::EINVAL);
    }
    if flags & CLONE_NEW_MASK != 0 {
        return Err(Errno::EPERM);
    }
    let thread = flags & CLONE_THREAD != 0;
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
    let tid = cx.k().alloc_tid()?;

    let mm = if flags & CLONE_VM != 0 {
        cx.t.mm.clone()
    } else {
        // 先让同一地址空间里的其他线程停下来，复制出来的才是 fork 那一刻的快照
        cx.mm().quiesce(&cx.task.sync);
        Rc::new(RefCell::new(cx.mm_mut().fork()?))
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
            root: f.root.clone(),
            cwd: f.cwd.clone(),
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
        g.pgid = parent.pgid;
        g.sid = parent.sid;
        g.rlimits = parent.rlimits;
        g.dumpable = parent.dumpable;
        g.no_new_privs = parent.no_new_privs;
        g.oom_score_adj = parent.oom_score_adj;
        g.oom_score_adj_min = parent.oom_score_adj_min;
        g.cred = parent.cred.clone();
        g.personality = parent.personality;
        g.thp_disable = parent.thp_disable;
        g.ctty = parent.ctty.clone();
        g.exe = parent.exe.clone();
        g.exit_signal = args.exit_signal;
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
        restart: None,
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
    task.affinity.set(cx.task.affinity.get());
    let (sched, nice) = super::sched::inherit(cx.task.sched.get(), cx.task.nice.get());
    task.sched.set(sched);
    task.nice.set(nice);
    task.deadline.set(cx.task.deadline.get());
    let slack = cx.task.timerslack.get().0;
    task.timerslack.set((slack, slack));

    // 写 tid 之前先确认能启动线程，失败时不留下任何痕迹
    if flags & CLONE_PARENT_SETTID != 0 {
        put(cx, ptid, tid.to_le_bytes())?;
    }
    if flags & CLONE_CHILD_SETTID != 0 {
        mm.borrow()
            .write(UserSlice::new(ctid, 4), &tid.to_le_bytes())?;
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
    if !thread {
        let ppid = group.borrow().ppid;
        let k = cx.k();
        k.forks += 1;
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

/// 给程序和解释器的映射起名字（`/proc/<pid>/maps`）。`inos` 是它们在仓库里的 inode
/// （不知道时是 0）。
pub(crate) fn label_image(mm: &mut Mm, image: &loader::Image, exe: &Rc<[u8]>, inos: (u64, u64)) {
    mm.label(
        image.main.0,
        image.main.1 - image.main.0,
        Some(exe.clone()),
        inos.0,
    );
    if let Some((path, start, end)) = &image.interp {
        mm.label(*start, end - start, Some(Rc::from(path.as_slice())), inos.1);
    }
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
    let (mut file, mut st) = process::open_node(&vfs, &cred, &node)?;

    // `#!` 脚本：换成解释器，argv 变成 [解释器, 参数, 脚本路径, 原来的 argv[1..]]
    let mut file_path = filename.clone();
    for depth in 0.. {
        let Some((interp, arg)) = parse_shebang(file.bytes()) else {
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
        (file, st) = process::open_executable(&vfs, &view, &cred, &root, &cwd, &interp)?;
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

    // setuid、setgid 程序（`PR_SET_NO_NEW_PRIVS` 之后不再生效）；脚本看的是解释器
    let no_new_privs = cx.t.group.borrow().no_new_privs;
    let setid = |bit: u32| st.mode & bit == bit && !no_new_privs;
    let mut new_cred = cred.clone();
    new_cred.exec(
        setid(mode::S_ISUID).then_some(st.uid),
        setid(mode::S_ISGID | 0o010).then_some(st.gid),
    );
    // 与 Linux 一样，setuid、setgid 程序的栈大小上限不超过默认值，免得调用者用它影响
    // 新程序的地址空间布局；新的值在 execve 成功之后写回
    let secure = new_cred.ruid != new_cred.euid || new_cred.rgid != new_cred.egid;
    if secure {
        stack_limit = stack_limit.min(loader::STACK_SIZE);
    }
    let mut mm = Mm::new();
    let args = Args {
        argv: &argv,
        envp: &envp,
        execfn: &filename,
        ids: [new_cred.ruid, new_cred.euid, new_cred.rgid, new_cred.egid],
        stack_limit,
    };
    let mut read_interp = |p: &[u8]| process::read_executable(&vfs, &view, &cred, &root, &cwd, p);
    let image = loader::load(&mut mm, &file, &args, &mut read_interp).map_err(load_error)?;
    let interp_ino = image.interp.as_ref().and_then(|(p, ..)| {
        match vfs.resolve(&view, &cred, &root, &cwd, p, true) {
            Ok(Node::Store(ino)) => Some(ino),
            _ => None,
        }
    });
    label_image(&mut mm, &image, &exe, (st.ino, interp_ino.unwrap_or(0)));
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
        g.exe = exe.to_vec();
        if (new_cred.euid, new_cred.egid) != (new_cred.ruid, new_cred.rgid) {
            g.dumpable = false;
        }
        g.cred = new_cred;
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
    // 驻留内存的峰值跨 execve 保留：换掉地址空间之前取一次样
    let group = cx.t.group.clone();
    group.borrow_mut().sample_rss(&cx.mm());
    let task = cx.task.clone();
    let (k, t) = cx.parts();
    task::release_mm(k, &task, t);
    let mm = Rc::new(RefCell::new(mm));
    mm.borrow_mut().add_user(task.sync.clone());
    // SAFETY: 新的地址空间由 TaskState 持有；旧的可能随这次赋值释放，cpu 已经不再使用它
    unsafe { cpu.reset(mm.borrow().mmu()) };
    cx.set_mm(mm);
    cx.t.altstack = AltStack::default();
    cx.t.saved_sigmask = None;
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

/// 子进程的状态变化。
#[derive(Clone, Copy)]
enum ChildEvent {
    Exited(ExitStatus),
    Stopped(Signal),
    Continued,
}

impl ChildEvent {
    /// wait4 的状态字。
    fn wait_status(self) -> i32 {
        match self {
            ChildEvent::Exited(s) => s.wait_status(),
            ChildEvent::Stopped(sig) => sig.number() << 8 | 0x7f,
            ChildEvent::Continued => 0xffff,
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
        }
    }
}

/// 要等哪些状态变化（`WEXITED`、`WUNTRACED`/`WSTOPPED`、`WCONTINUED`），以及是否取走事件。
#[derive(Clone, Copy)]
struct Wanted {
    exited: bool,
    stopped: bool,
    continued: bool,
    consume: bool,
}

/// 找到的子进程状态变化：(pid, 事件, 子进程和它回收过的子进程用掉的 CPU 时间)。
type Found = (Tid, ChildEvent, process::Rusage);

/// 找到一个符合条件的子进程状态变化，`consume` 时取走它（已结束的子进程同时被回收）。
/// 没有子进程符合条件时返回 ECHILD。
fn find_event(cx: &mut Cx<'_>, who: Who, want: Wanted) -> Result<Option<Found>, Errno> {
    let children = cx.t.group.borrow().children.clone();
    let mut any = false;
    for c in children {
        let Some(child) = cx.kr().groups.get(&c).cloned() else {
            continue;
        };
        let matches = match who {
            Who::Pid(p) => p == c,
            Who::Any => true,
            Who::Pgid(g) => g == child.borrow().pgid,
        };
        if !matches {
            continue;
        }
        any = true;
        // 与 Linux 一样，子进程的用量包括它回收的子进程。找到事件才算：活着的线程的内核态
        // 时间要读宿主的 /proc，wait 每次被叫醒都会重新扫一遍子进程
        let usage = |g: &ThreadGroup, k: &crate::kernel::Kernel| process::Rusage {
            cpu: g.cpu(k) + g.child_cpu,
            maxrss_kb: g.maxrss_kb.max(g.child_maxrss_kb),
        };
        let mut g = child.borrow_mut();
        if let (true, Some(status)) = (want.exited, g.zombie) {
            let usage = usage(&g, cx.kr());
            if want.consume {
                drop(g);
                cx.k().groups.remove(&c);
                let mut me = cx.t.group.borrow_mut();
                me.children.retain(|&x| x != c);
                me.child_cpu += usage.cpu;
                me.child_maxrss_kb = me.child_maxrss_kb.max(usage.maxrss_kb);
            }
            return Ok(Some((c, ChildEvent::Exited(status), usage)));
        }
        if let (true, Some(sig)) = (want.stopped, g.stop_report) {
            if want.consume {
                g.stop_report = None;
            }
            return Ok(Some((c, ChildEvent::Stopped(sig), usage(&g, cx.kr()))));
        }
        if want.continued && g.cont_report {
            if want.consume {
                g.cont_report = false;
            }
            return Ok(Some((c, ChildEvent::Continued, usage(&g, cx.kr()))));
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
        // 子进程的状态变化不是打开文件上的修改（crate::kernel::wake）
        cx.wait_on(None, &mut [], crate::kernel::wake::OTHER)?;
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

/// 能不能像 ptrace 那样访问 `target`（`PTRACE_MODE_ATTACH_REALCREDS`）：是自己；或者
/// 真实用户和组等于目标的所有用户和组，并且目标可以 dump；或者有 `CAP_SYS_PTRACE`。
fn may_ptrace(cx: &Cx<'_>, target: &ThreadGroup) -> bool {
    let me = cx.cred();
    let t = &target.cred;
    let same = [t.ruid, t.euid, t.suid].iter().all(|&u| u == me.ruid)
        && [t.rgid, t.egid, t.sgid].iter().all(|&g| g == me.rgid);
    target.tgid == cx.tgid() || (same && target.dumpable) || me.capable(cap::SYS_PTRACE)
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

/// unshare：不再和别的线程、进程共用某些资源。命名空间不支持，与 clone 一样返回
/// EPERM。与 Linux 一样，`CLONE_THREAD`、`CLONE_SIGHAND`、`CLONE_VM` 只在本来就没有
/// 共用时成功（什么也不用做）。
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
    if flags & CLONE_NEW_MASK != 0 {
        return Err(Errno::EPERM);
    }
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
            }
        };
        cx.set_fs(Rc::new(RefCell::new(copy)));
    }
    // 与 Linux 的 exit_sem 一样，执行并丢掉 SysV 信号量的 undo
    if flags & CLONE_SYSVSEM != 0 {
        let tgid = cx.tgid();
        cx.k().ipc.exit(tgid);
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
        };
        assert_eq!(killed.wait_status(), 15);
    }
}
