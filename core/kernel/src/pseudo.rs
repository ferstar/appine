//! 合成文件系统（设计文档 §8.2）：`/proc`（procfs）和 `/dev/pts`（devpts）。
//!
//! 节点是 [`PseudoNode`]，目录结构经由 [`View`]（持有 BKL 时对内核状态的只读视图）
//! 实现 [`Pseudo`]。procfs 的文件在打开时生成一份完整的内容（[`content`]），
//! 之后的读都在这份快照上进行，与 Linux 的 seq_file 一次读完时的效果相同。

use std::cell::RefCell;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use crate::errno::Errno;
use crate::fd::{FileKind, OpenFile};
use crate::kernel::{INIT, Kernel, Tid};
use crate::process::{Signal, ThreadGroup};
use crate::signal::SigAction;
use crate::task::Task;
use crate::tty::{PTS_MAJOR, Tty};
use crate::vfs::dev::rdev;
use crate::vfs::store::{DirEntry, Stat, mode};
use crate::vfs::{Mount, Node, Pseudo, Vfs};

/// `/proc` 下的全局文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Global {
    Cpuinfo,
    Meminfo,
    Stat,
    Uptime,
    Loadavg,
    Version,
    Filesystems,
    Cmdline,
    /// 指向 `self/mounts` 的符号链接。
    Mounts,
}

const GLOBALS: [(&[u8], Global); 9] = [
    (b"cpuinfo", Global::Cpuinfo),
    (b"meminfo", Global::Meminfo),
    (b"stat", Global::Stat),
    (b"uptime", Global::Uptime),
    (b"loadavg", Global::Loadavg),
    (b"version", Global::Version),
    (b"filesystems", Global::Filesystems),
    (b"cmdline", Global::Cmdline),
    (b"mounts", Global::Mounts),
];

/// `/proc/<pid>` 下的项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Entry {
    Stat,
    Status,
    Statm,
    Cmdline,
    Comm,
    Environ,
    Maps,
    Mounts,
    Cwd,
    Exe,
    Root,
    Fd,
    Task,
    OomScoreAdj,
}

const ENTRIES: [(&[u8], Entry); 14] = [
    (b"stat", Entry::Stat),
    (b"status", Entry::Status),
    (b"statm", Entry::Statm),
    (b"cmdline", Entry::Cmdline),
    (b"comm", Entry::Comm),
    (b"environ", Entry::Environ),
    (b"maps", Entry::Maps),
    (b"mounts", Entry::Mounts),
    (b"cwd", Entry::Cwd),
    (b"exe", Entry::Exe),
    (b"root", Entry::Root),
    (b"fd", Entry::Fd),
    (b"task", Entry::Task),
    (b"oom_score_adj", Entry::OomScoreAdj),
];

/// `/proc/net`、`/proc/sys` 下固定的目录和文件（相对 `/proc` 的路径，目录以 `/` 结尾）。
const STATIC: [&[u8]; 18] = [
    b"net/",
    b"net/dev",
    b"sys/",
    b"sys/fs/",
    b"sys/fs/file-max",
    b"sys/kernel/",
    b"sys/kernel/cap_last_cap",
    b"sys/kernel/hostname",
    b"sys/kernel/ngroups_max",
    b"sys/kernel/osrelease",
    b"sys/kernel/ostype",
    b"sys/kernel/pid_max",
    b"sys/kernel/random/",
    b"sys/kernel/random/boot_id",
    b"sys/kernel/random/uuid",
    b"sys/kernel/version",
    b"sys/vm/",
    b"sys/vm/overcommit_memory",
];

/// 合成文件系统里的一个节点。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum PseudoNode {
    /// `/dev/pts`。
    PtsRoot,
    /// `/dev/pts/<n>`。
    Pts(u32),
    /// `/dev/pts/ptmx`。
    PtsPtmx,
    /// `/proc`。
    ProcRoot,
    /// `/proc` 下的全局文件。
    Global(Global),
    /// `/proc/self`。
    SelfLink,
    /// `/proc/thread-self`。
    ThreadSelf,
    /// `/proc/net`、`/proc/sys` 下的目录或文件，取值来自 [`STATIC`]。
    Static(&'static [u8]),
    /// `/proc/<pid>`；第二项是 `Some(tid)` 时是 `/proc/<pid>/task/<tid>`。
    Pid(Tid, Option<Tid>),
    /// `/proc/<pid>/<项>`（含 task 子目录里的）。
    Entry(Tid, Option<Tid>, Entry),
    /// `/proc/<pid>/fd/<n>`。
    Fd(Tid, i32),
    /// `/proc/<pid>/fd/<n>` 跟随之后得到的那个打开文件（不在仓库里的那些：管道、宿主 fd）。
    OpenFd(Tid, i32),
}

/// `/proc/<pid>/task/<tid>` 里的项。
const TASK_ENTRIES: [Entry; 3] = [Entry::Stat, Entry::Status, Entry::Comm];

/// 持有 BKL 时对内核状态的只读视图，`tid`/`tgid` 是 `self` 指向的线程。
pub(crate) struct View<'a> {
    pub k: &'a Kernel,
    pub vfs: &'a Vfs,
    pub tid: Tid,
    pub tgid: Tid,
}

/// 内核还没有启动时（解析第一个程序的路径）用的视图：合成文件系统只有空的根目录。
pub(crate) struct Offline;

impl Pseudo for Offline {
    fn root(&self, fs: Mount) -> PseudoNode {
        root_of(fs)
    }
    fn mount_of(&self, node: &PseudoNode) -> Mount {
        mount_of(node)
    }
    fn parent(&self, _: &PseudoNode) -> Option<PseudoNode> {
        None
    }
    fn name(&self, _: &PseudoNode) -> Vec<u8> {
        Vec::new()
    }
    fn lookup(&self, _: &PseudoNode, _: &[u8]) -> Result<PseudoNode, Errno> {
        Err(Errno::ENOENT)
    }
    fn kind(&self, _: &PseudoNode) -> Result<u32, Errno> {
        Ok(mode::S_IFDIR)
    }
    fn readlink(&self, _: &PseudoNode) -> Result<Vec<u8>, Errno> {
        Err(Errno::EINVAL)
    }
    fn follow(&self, _: &PseudoNode) -> Option<Node> {
        None
    }
}

fn root_of(fs: Mount) -> PseudoNode {
    match fs {
        Mount::Proc => PseudoNode::ProcRoot,
        Mount::Pts => PseudoNode::PtsRoot,
    }
}

fn mount_of(node: &PseudoNode) -> Mount {
    match node {
        PseudoNode::PtsRoot | PseudoNode::Pts(_) | PseudoNode::PtsPtmx => Mount::Pts,
        _ => Mount::Proc,
    }
}

fn parse_num(name: &[u8]) -> Option<i64> {
    if name.is_empty() || name.len() > 10 || (name.len() > 1 && name[0] == b'0') {
        return None;
    }
    std::str::from_utf8(name).ok()?.parse().ok()
}

/// 合成节点的 inode 编号：对节点取哈希（同一次运行里稳定），避开 0。
pub(crate) fn ino_of(node: &PseudoNode) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    node.hash(&mut h);
    h.finish() | 1
}

impl<'a> View<'a> {
    fn group(&self, pid: Tid) -> Option<Rc<RefCell<ThreadGroup>>> {
        self.k.groups.get(&pid).cloned()
    }

    /// 线程组的代表线程（主线程，它已退出时是任意一个活着的线程）。
    fn leader(&self, pid: Tid) -> Option<Rc<Task>> {
        let g = self.group(pid)?;
        let threads = g.borrow().threads.clone();
        threads
            .iter()
            .find(|&&t| t == pid)
            .or(threads.first())
            .and_then(|t| self.k.tasks.get(t).cloned())
    }

    /// `/proc/<pid>/task/<tid>` 用指定的线程，否则用代表线程。
    fn task(&self, pid: Tid, tid: Option<Tid>) -> Option<Rc<Task>> {
        match tid {
            Some(t) => self.k.tasks.get(&t).cloned().filter(|t| t.tgid == pid),
            None => self.leader(pid),
        }
    }

    fn pid_exists(&self, pid: Tid, tid: Option<Tid>) -> bool {
        match (self.group(pid), tid) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(g), Some(t)) => g.borrow().threads.contains(&t),
        }
    }

    /// 某个进程的打开文件。
    pub(crate) fn open_file(&self, pid: Tid, fd: i32) -> Option<crate::fd::FileRef> {
        let task = self.leader(pid)?;
        let res = task.res.borrow();
        res.fds.borrow().get(fd).ok()
    }

    fn pty(&self, n: u32) -> Option<Rc<RefCell<Tty>>> {
        self.k.ptys.get(&n).and_then(|w| w.upgrade())
    }
}

impl Pseudo for View<'_> {
    fn root(&self, fs: Mount) -> PseudoNode {
        root_of(fs)
    }

    fn mount_of(&self, node: &PseudoNode) -> Mount {
        mount_of(node)
    }

    fn parent(&self, node: &PseudoNode) -> Option<PseudoNode> {
        use PseudoNode::*;
        Some(match node {
            PtsRoot | ProcRoot => return None,
            Pts(_) | PtsPtmx => PtsRoot,
            Global(_) | SelfLink | ThreadSelf | Pid(_, None) => ProcRoot,
            Static(path) => {
                let trimmed = path.strip_suffix(b"/").unwrap_or(path);
                match trimmed.iter().rposition(|&b| b == b'/') {
                    None => ProcRoot,
                    Some(i) => Static(STATIC.iter().find(|p| **p == &trimmed[..=i]).unwrap()),
                }
            }
            Pid(pid, Some(_)) => Entry(*pid, None, self::Entry::Task),
            Entry(pid, tid, _) => Pid(*pid, *tid),
            Fd(pid, _) | OpenFd(pid, _) => Entry(*pid, None, self::Entry::Fd),
        })
    }

    fn name(&self, node: &PseudoNode) -> Vec<u8> {
        use PseudoNode::*;
        match node {
            PtsRoot => b"pts".to_vec(),
            ProcRoot => b"proc".to_vec(),
            Pts(n) => n.to_string().into_bytes(),
            PtsPtmx => b"ptmx".to_vec(),
            Global(g) => GLOBALS.iter().find(|(_, x)| x == g).unwrap().0.to_vec(),
            SelfLink => b"self".to_vec(),
            ThreadSelf => b"thread-self".to_vec(),
            Static(path) => {
                let trimmed = path.strip_suffix(b"/").unwrap_or(path);
                let start = trimmed
                    .iter()
                    .rposition(|&b| b == b'/')
                    .map_or(0, |i| i + 1);
                trimmed[start..].to_vec()
            }
            Pid(pid, None) => pid.to_string().into_bytes(),
            Pid(_, Some(tid)) => tid.to_string().into_bytes(),
            Entry(_, _, e) => ENTRIES.iter().find(|(_, x)| x == e).unwrap().0.to_vec(),
            Fd(_, fd) | OpenFd(_, fd) => fd.to_string().into_bytes(),
        }
    }

    fn lookup(&self, dir: &PseudoNode, name: &[u8]) -> Result<PseudoNode, Errno> {
        use PseudoNode::*;
        let found = match dir {
            PtsRoot => {
                if name == b"ptmx" {
                    Some(PtsPtmx)
                } else {
                    parse_num(name)
                        .and_then(|n| u32::try_from(n).ok())
                        .filter(|&n| self.pty(n).is_some())
                        .map(Pts)
                }
            }
            ProcRoot => match name {
                b"self" => Some(SelfLink),
                b"thread-self" => Some(ThreadSelf),
                b"net" => Some(Static(STATIC[0])),
                b"sys" => Some(Static(STATIC[2])),
                _ => GLOBALS
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, g)| Global(*g))
                    .or_else(|| {
                        let pid = parse_num(name)? as Tid;
                        self.pid_exists(pid, None).then_some(Pid(pid, None))
                    }),
            },
            Static(path) => {
                let mut full = path.to_vec();
                full.extend_from_slice(name);
                STATIC
                    .iter()
                    .find(|p| p.strip_suffix(b"/").unwrap_or(p) == &full[..])
                    .map(|p| Static(p))
            }
            Pid(pid, tid) => {
                let allowed = |e: &self::Entry| tid.is_none() || TASK_ENTRIES.contains(e);
                ENTRIES
                    .iter()
                    .find(|(n, e)| *n == name && allowed(e))
                    .map(|(_, e)| Entry(*pid, *tid, *e))
                    .filter(|_| self.pid_exists(*pid, *tid))
            }
            Entry(pid, None, self::Entry::Fd) => parse_num(name)
                .map(|fd| fd as i32)
                .filter(|&fd| self.open_file(*pid, fd).is_some())
                .map(|fd| Fd(*pid, fd)),
            Entry(pid, None, self::Entry::Task) => parse_num(name)
                .map(|t| t as Tid)
                .filter(|&t| self.pid_exists(*pid, Some(t)))
                .map(|t| Pid(*pid, Some(t))),
            _ => return Err(Errno::ENOTDIR),
        };
        found.ok_or(Errno::ENOENT)
    }

    fn kind(&self, node: &PseudoNode) -> Result<u32, Errno> {
        use PseudoNode::*;
        Ok(match node {
            PtsRoot | ProcRoot | Pid(..) => mode::S_IFDIR,
            Static(p) if p.ends_with(b"/") => mode::S_IFDIR,
            Entry(_, _, self::Entry::Fd | self::Entry::Task) => mode::S_IFDIR,
            Pts(_) | PtsPtmx => mode::S_IFCHR,
            SelfLink | ThreadSelf | Global(self::Global::Mounts) | Fd(..) => mode::S_IFLNK,
            Entry(_, _, self::Entry::Cwd | self::Entry::Exe | self::Entry::Root) => mode::S_IFLNK,
            OpenFd(pid, fd) => {
                let file = self.open_file(*pid, *fd).ok_or(Errno::ENOENT)?;
                let f = file.borrow();
                match &f.kind {
                    FileKind::Pipe(_) => mode::S_IFIFO,
                    FileKind::Socket(_) => mode::S_IFSOCK,
                    FileKind::Host(fd) => {
                        appine_hal::fstat(*fd).map_err(Errno)?.st_mode as u32 & mode::S_IFMT
                    }
                    _ => mode::S_IFREG,
                }
            }
            _ => mode::S_IFREG,
        })
    }

    fn readlink(&self, node: &PseudoNode) -> Result<Vec<u8>, Errno> {
        use PseudoNode::*;
        match node {
            SelfLink => Ok(self.tgid.to_string().into_bytes()),
            ThreadSelf => Ok(format!("{}/task/{}", self.tgid, self.tid).into_bytes()),
            Global(self::Global::Mounts) => Ok(b"self/mounts".to_vec()),
            Entry(_, _, self::Entry::Root) => Ok(b"/".to_vec()),
            Entry(pid, _, self::Entry::Exe) => {
                let g = self.group(*pid).ok_or(Errno::ENOENT)?;
                let exe = g.borrow().exe.clone();
                if exe.is_empty() {
                    Err(Errno::ENOENT)
                } else {
                    Ok(exe)
                }
            }
            Entry(pid, tid, self::Entry::Cwd) => {
                let task = self.task(*pid, *tid).ok_or(Errno::ENOENT)?;
                let cwd = task.res.borrow().fs.borrow().cwd.clone();
                self.vfs.path_of(self, &cwd)
            }
            Fd(pid, fd) => {
                let file = self.open_file(*pid, *fd).ok_or(Errno::ENOENT)?;
                let name = describe(&file.borrow());
                Ok(name)
            }
            _ => Err(Errno::EINVAL),
        }
    }

    fn follow(&self, node: &PseudoNode) -> Option<Node> {
        let PseudoNode::Fd(pid, fd) = node else {
            return None;
        };
        let file = self.open_file(*pid, *fd)?;
        let f = file.borrow();
        Some(match &f.kind {
            FileKind::Regular { ino, .. }
            | FileKind::Device { ino, .. }
            | FileKind::Directory {
                node: Node::Store(ino),
                ..
            }
            | FileKind::Path(Node::Store(ino)) => Node::Store(*ino),
            FileKind::Directory { node, .. } | FileKind::Path(node) => node.clone(),
            FileKind::Proc { node, .. } => Node::Pseudo(node.clone()),
            FileKind::Pty(end) if !end.master => {
                Node::Pseudo(PseudoNode::Pts(end.tty.borrow().index))
            }
            _ => Node::Pseudo(PseudoNode::OpenFd(*pid, *fd)),
        })
    }
}

/// `/proc/<pid>/fd/<n>` 显示的目标：路径，或者 `pipe:[ino]` 这种描述。
pub(crate) fn describe(f: &OpenFile) -> Vec<u8> {
    if let Some(p) = &f.path {
        return p.to_vec();
    }
    match &f.kind {
        FileKind::Pipe(end) => format!("pipe:[{}]", end.pipe.borrow().ino).into_bytes(),
        FileKind::Pty(end) if end.master => b"/dev/ptmx".to_vec(),
        FileKind::Pty(end) => format!("/dev/pts/{}", end.tty.borrow().index).into_bytes(),
        FileKind::Socket(s) => format!("socket:[{}]", Rc::as_ptr(s) as u64 >> 4).into_bytes(),
        FileKind::Epoll(_) => b"anon_inode:[eventpoll]".to_vec(),
        FileKind::EventFd(_) => b"anon_inode:[eventfd]".to_vec(),
        FileKind::Host(fd) => std::fs::read_link(format!("/proc/self/fd/{fd}"))
            .map(|p| {
                use std::os::unix::ffi::OsStringExt;
                p.into_os_string().into_vec()
            })
            .unwrap_or_else(|_| format!("host:[{fd}]").into_bytes()),
        _ => b"anon_inode:[unknown]".to_vec(),
    }
}

/// 合成节点的元数据。
pub(crate) fn stat(ps: &View<'_>, node: &PseudoNode) -> Result<Stat, Errno> {
    let kind = ps.kind(node)?;
    let perm = match (kind, node) {
        (mode::S_IFDIR, _) => 0o555,
        (mode::S_IFLNK, _) => 0o777,
        (_, PseudoNode::Pts(_)) => 0o620,
        (_, PseudoNode::PtsPtmx) => 0o666,
        (_, n) if writable(n) => 0o644,
        _ => 0o444,
    };
    let rdev_value = match node {
        PseudoNode::Pts(n) => rdev(PTS_MAJOR, *n),
        PseudoNode::PtsPtmx => rdev(5, 2),
        _ => 0,
    };
    let now = appine_hal::clock_gettime(0)
        .map(|(s, ns)| s * 1_000_000_000 + ns)
        .unwrap_or(0);
    Ok(Stat {
        ino: ino_of(node),
        mode: kind | perm,
        nlink: if kind == mode::S_IFDIR { 2 } else { 1 },
        uid: 0,
        gid: 0,
        rdev: rdev_value,
        size: 0,
        mtime_ns: now,
        ctime_ns: now,
    })
}

/// 可以写入的 procfs 文件：写入的内容被接受但不起作用。
pub(crate) fn writable(node: &PseudoNode) -> bool {
    matches!(
        node,
        PseudoNode::Entry(_, None, Entry::OomScoreAdj) | PseudoNode::Static(b"sys/kernel/hostname")
    )
}

/// 目录的内容（不含 `.` 和 `..`）。
pub(crate) fn readdir(ps: &View<'_>, dir: &PseudoNode) -> Result<Vec<DirEntry>, Errno> {
    use PseudoNode::*;
    let mut nodes: Vec<PseudoNode> = Vec::new();
    match dir {
        PtsRoot => {
            nodes.push(PtsPtmx);
            nodes.extend(
                ps.k.ptys
                    .iter()
                    .filter(|(_, w)| w.upgrade().is_some())
                    .map(|(n, _)| Pts(*n)),
            );
        }
        ProcRoot => {
            nodes.extend([SelfLink, ThreadSelf, Static(STATIC[0]), Static(STATIC[2])]);
            nodes.extend(GLOBALS.iter().map(|(_, g)| Global(*g)));
            nodes.extend(ps.k.groups.keys().map(|&pid| Pid(pid, None)));
        }
        Static(path) => nodes.extend(
            STATIC
                .iter()
                .filter(|p| {
                    let trimmed = p.strip_suffix(b"/").unwrap_or(p);
                    trimmed.len() > path.len()
                        && trimmed.starts_with(path)
                        && !trimmed[path.len()..].contains(&b'/')
                })
                .map(|p| Static(p)),
        ),
        Pid(pid, tid) => nodes.extend(
            ENTRIES
                .iter()
                .filter(|(_, e)| tid.is_none() || TASK_ENTRIES.contains(e))
                .map(|(_, e)| Entry(*pid, *tid, *e)),
        ),
        Entry(pid, None, self::Entry::Fd) => {
            if let Some(task) = ps.leader(*pid) {
                let fds = task.res.borrow().fds.borrow().open_fds();
                nodes.extend(fds.into_iter().map(|fd| Fd(*pid, fd)));
            }
        }
        Entry(pid, None, self::Entry::Task) => {
            if let Some(g) = ps.group(*pid) {
                let threads = g.borrow().threads.clone();
                nodes.extend(threads.into_iter().map(|t| Pid(*pid, Some(t))));
            }
        }
        _ => return Err(Errno::ENOTDIR),
    }
    nodes
        .into_iter()
        .map(|n| {
            Ok(DirEntry {
                name: ps.name(&n),
                ino: ino_of(&n),
                mode: ps.kind(&n).unwrap_or(mode::S_IFREG),
            })
        })
        .collect()
}

// ---- 文件内容 ----

/// 时钟滴答（`sysconf(_SC_CLK_TCK)`）。
const HZ: u64 = crate::loader::CLOCK_TICKS;

fn ticks(ns: u64) -> u64 {
    ns / (1_000_000_000 / HZ)
}

/// 开机以来的纳秒数（CLOCK_BOOTTIME）。
pub(crate) fn uptime_ns() -> u64 {
    appine_hal::clock_gettime(appine_hal::CLOCK_BOOTTIME)
        .map(|(s, ns)| s as u64 * 1_000_000_000 + ns as u64)
        .unwrap_or(0)
}

fn cpus() -> usize {
    std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(64)
}

/// 线程组的 CPU 时间（纳秒）：活着的线程加上已经退出的线程。
/// 进程状态的字母：R 正在执行 guest 代码，S 在内核里或等待，T 停止，Z 僵尸。
fn state_of(ps: &View<'_>, g: &ThreadGroup, tid: Option<Tid>) -> (char, &'static str) {
    if g.zombie.is_some() {
        return ('Z', "zombie");
    }
    if g.stopped {
        return ('T', "stopped");
    }
    let running = g
        .threads
        .iter()
        .filter(|t| tid.is_none_or(|x| x == **t))
        .filter_map(|t| ps.k.tasks.get(t))
        .any(|t| t.sync.in_guest.load(std::sync::atomic::Ordering::Relaxed));
    if running {
        ('R', "running")
    } else {
        ('S', "sleeping")
    }
}

/// guest 内存里的一段（cmdline、environ），读不到时为空。
fn guest_bytes(task: &Task, range: fn(&crate::mm::Mm) -> (u64, u64)) -> Vec<u8> {
    let res = task.res.borrow();
    let mm = res.mm.borrow();
    let (start, end) = range(&mm);
    if end <= start || end - start > 1 << 20 {
        return Vec::new();
    }
    let mut buf = vec![0u8; (end - start) as usize];
    match mm.read(crate::uaccess::UserSlice::new(start, buf.len()), &mut buf) {
        Ok(()) => buf,
        Err(_) => Vec::new(),
    }
}

fn comm_str(task: Option<&Task>, pid: Tid) -> String {
    match task {
        Some(t) => {
            let c = t.comm.get();
            let n = c.iter().position(|&b| b == 0).unwrap_or(c.len());
            String::from_utf8_lossy(&c[..n]).into_owned()
        }
        None if pid == INIT => "init".to_owned(),
        None => String::new(),
    }
}

/// 信号处理方式的位图：(忽略, 捕获)。
fn sig_dispositions(g: &ThreadGroup) -> (u64, u64) {
    let sh = g.sighand.borrow();
    let (mut ign, mut cgt) = (0, 0);
    for n in 1..=Signal::MAX {
        let sig = Signal::new(n).unwrap();
        match sh.get(sig).handler {
            SigAction::IGNORE => ign |= sig.bit(),
            SigAction::DEFAULT => {}
            _ => cgt |= sig.bit(),
        }
    }
    (ign, cgt)
}

/// 地址空间的 (总大小, 驻留页数)。
fn mem_of(task: Option<&Task>) -> (u64, u64) {
    let Some(t) = task else {
        return (0, 0);
    };
    let res = t.res.borrow();
    let mm = res.mm.borrow();
    (mm.size(), mm.resident_pages())
}

fn ctty_info(ps: &View<'_>, g: &ThreadGroup) -> (u64, i64) {
    match &g.ctty {
        Some(tty) => {
            let t = tty.borrow();
            let _ = ps;
            (rdev(PTS_MAJOR, t.index), t.fg.map_or(-1, |p| p as i64))
        }
        None => (0, -1),
    }
}

fn pid_stat(ps: &View<'_>, pid: Tid, tid: Option<Tid>) -> Result<String, Errno> {
    let g = ps.group(pid).ok_or(Errno::ENOENT)?;
    let g = g.borrow();
    let task = ps.task(pid, tid);
    let (state, _) = state_of(ps, &g, tid);
    let (tty_nr, tpgid) = ctty_info(ps, &g);
    let cpu = match (tid, &task) {
        (Some(_), Some(t)) => t.cpu_ns(),
        _ => g.cpu_ns(ps.k),
    };
    let (vsize, rss) = mem_of(task.as_deref());
    let (sigign, sigcgt) = sig_dispositions(&g);
    let (pending, blocked) = task
        .as_ref()
        .map_or((0, 0), |t| (t.pending.get() | g.pending, t.sigmask.get()));
    let (args, env) = task.as_ref().map_or(((0, 0), (0, 0)), |t| {
        let res = t.res.borrow();
        let mm = res.mm.borrow();
        (mm.args, mm.env)
    });
    let mut s = String::new();
    write!(
        s,
        "{} ({}) {} {} {} {} {} {} 4194304 0 0 0 0 {} 0 {} 0 {} {} {} 0 {} {} {} 18446744073709551615",
        tid.unwrap_or(pid),
        comm_str(task.as_deref(), pid),
        state,
        g.ppid,
        g.pgid,
        g.sid,
        tty_nr,
        tpgid,
        ticks(cpu),
        ticks(g.child_cpu_ns),
        20 + g.nice,
        g.nice,
        g.threads.len().max(1),
        g.start_ticks,
        vsize,
        rss,
    )
    .unwrap();
    writeln!(
        s,
        " 0 0 0 0 0 {pending} {blocked} {sigign} {sigcgt} 0 0 0 {} 0 0 0 0 0 0 0 0 0 {} {} {} {} 0",
        g.exit_signal, args.0, args.1, env.0, env.1
    )
    .unwrap();
    Ok(s)
}

fn pid_status(ps: &View<'_>, pid: Tid, tid: Option<Tid>) -> Result<String, Errno> {
    let g = ps.group(pid).ok_or(Errno::ENOENT)?;
    let g = g.borrow();
    let task = ps.task(pid, tid);
    let (state, state_name) = state_of(ps, &g, tid);
    let (vsize, rss) = mem_of(task.as_deref());
    let (sigign, sigcgt) = sig_dispositions(&g);
    let (umask, fdsize) = task.as_ref().map_or((0o022, 0), |t| {
        let res = t.res.borrow();
        let fds = res.fds.borrow().open_fds().len();
        (res.fs.borrow().umask, fds.next_multiple_of(64))
    });
    let (pending, blocked) = task
        .as_ref()
        .map_or((0, 0), |t| (t.pending.get(), t.sigmask.get()));
    let mut s = String::new();
    let comm = comm_str(task.as_deref(), pid);
    writeln!(s, "Name:\t{comm}").unwrap();
    writeln!(s, "Umask:\t{umask:04o}").unwrap();
    writeln!(s, "State:\t{state} ({state_name})").unwrap();
    writeln!(s, "Tgid:\t{pid}\nNgid:\t0\nPid:\t{}", tid.unwrap_or(pid)).unwrap();
    writeln!(s, "PPid:\t{}\nTracerPid:\t0", g.ppid).unwrap();
    writeln!(
        s,
        "Uid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\nFDSize:\t{fdsize}\nGroups:\t"
    )
    .unwrap();
    if task.is_some() {
        let kb = vsize / 1024;
        writeln!(s, "VmPeak:\t{kb:>8} kB\nVmSize:\t{kb:>8} kB").unwrap();
        writeln!(s, "VmRSS:\t{:>8} kB", rss * 4).unwrap();
    }
    writeln!(s, "Threads:\t{}", g.threads.len().max(1)).unwrap();
    writeln!(s, "SigQ:\t0/63432").unwrap();
    writeln!(s, "SigPnd:\t{pending:016x}\nShdPnd:\t{:016x}", g.pending).unwrap();
    writeln!(
        s,
        "SigBlk:\t{blocked:016x}\nSigIgn:\t{sigign:016x}\nSigCgt:\t{sigcgt:016x}"
    )
    .unwrap();
    writeln!(s, "CapInh:\t0000000000000000\nCapPrm:\t000001ffffffffff").unwrap();
    writeln!(s, "CapEff:\t000001ffffffffff\nCapBnd:\t000001ffffffffff").unwrap();
    writeln!(s, "CapAmb:\t0000000000000000\nNoNewPrivs:\t0\nSeccomp:\t0").unwrap();
    writeln!(s, "Cpus_allowed_list:\t0-{}", cpus() - 1).unwrap();
    writeln!(
        s,
        "voluntary_ctxt_switches:\t0\nnonvoluntary_ctxt_switches:\t0"
    )
    .unwrap();
    Ok(s)
}

fn maps(task: &Task) -> String {
    let res = task.res.borrow();
    let mm = res.mm.borrow();
    let mut s = String::new();
    for r in mm.regions() {
        let perms = format!(
            "{}{}{}{}",
            if r.prot & 1 != 0 { 'r' } else { '-' },
            if r.prot & 2 != 0 { 'w' } else { '-' },
            if r.prot & 4 != 0 { 'x' } else { '-' },
            if r.shared { 's' } else { 'p' },
        );
        let line = format!("{:08x}-{:08x} {perms} 00000000 00:00 0", r.start, r.end);
        match &r.name {
            Some(name) => writeln!(s, "{line:<73}{}", String::from_utf8_lossy(name)).unwrap(),
            None => writeln!(s, "{line}").unwrap(),
        }
    }
    s
}

const MOUNTS: &str = "appine / ext4 rw,relatime 0 0\n\
    proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0\n\
    devpts /dev/pts devpts rw,nosuid,noexec,relatime,gid=5,mode=620,ptmxmode=000 0 0\n";

const HWCAP_NAMES: [&str; 12] = [
    "fp", "asimd", "evtstrm", "aes", "pmull", "sha1", "sha2", "crc32", "atomics", "fphp",
    "asimdhp", "cpuid",
];

fn cpuinfo() -> String {
    let hwcap = appine_engine::guest_hwcap();
    let features: Vec<&str> = HWCAP_NAMES
        .iter()
        .enumerate()
        .filter(|(i, _)| hwcap & (1 << i) != 0)
        .map(|(_, n)| *n)
        .collect();
    let midr = appine_engine::guest_midr();
    let mut s = String::new();
    for cpu in 0..cpus() {
        writeln!(s, "processor\t: {cpu}\nBogoMIPS\t: 100.00").unwrap();
        writeln!(s, "Features\t: {}", features.join(" ")).unwrap();
        writeln!(s, "CPU implementer\t: {:#04x}", (midr >> 24) & 0xff).unwrap();
        writeln!(s, "CPU architecture: 8").unwrap();
        writeln!(s, "CPU variant\t: {:#x}", (midr >> 20) & 0xf).unwrap();
        writeln!(s, "CPU part\t: {:#05x}", (midr >> 4) & 0xfff).unwrap();
        writeln!(s, "CPU revision\t: {}\n", midr & 0xf).unwrap();
    }
    s
}

fn meminfo() -> String {
    let (total, free) = appine_hal::memory_info();
    let mut s = String::new();
    for (name, bytes) in [
        ("MemTotal", total),
        ("MemFree", free),
        ("MemAvailable", free),
        ("Buffers", 0),
        ("Cached", 0),
        ("SwapCached", 0),
        ("Active", 0),
        ("Inactive", 0),
        ("SwapTotal", 0),
        ("SwapFree", 0),
        ("Dirty", 0),
        ("AnonPages", total - free),
        ("Mapped", 0),
        ("Shmem", 0),
        ("Slab", 0),
        ("SReclaimable", 0),
    ] {
        writeln!(s, "{:<16}{:>8} kB", format!("{name}:"), bytes / 1024).unwrap();
    }
    s
}

fn global_stat(ps: &View<'_>) -> String {
    let n = cpus() as u64;
    let up = ticks(uptime_ns());
    let busy = ticks(ps.k.cpu_ns + ps.k.tasks.values().map(|t| t.cpu_ns()).sum::<u64>());
    let idle = (up * n).saturating_sub(busy);
    let running =
        ps.k.tasks
            .values()
            .filter(|t| t.sync.in_guest.load(std::sync::atomic::Ordering::Relaxed))
            .count();
    let (now, _) = appine_hal::clock_gettime(0).unwrap_or((0, 0));
    let btime = now as u64 - uptime_ns() / 1_000_000_000;
    let mut s = String::new();
    writeln!(s, "cpu  {busy} 0 0 {idle} 0 0 0 0 0 0").unwrap();
    for i in 0..n {
        writeln!(s, "cpu{i} {} 0 0 {} 0 0 0 0 0 0", busy / n, idle / n).unwrap();
    }
    writeln!(s, "intr 0\nctxt 0\nbtime {btime}\nprocesses {}", ps.k.forks).unwrap();
    writeln!(
        s,
        "procs_running {}\nprocs_blocked 0\nsoftirq 0",
        running.max(1)
    )
    .unwrap();
    s
}

/// `/proc/net/dev`：宿主接口的列表，流量计数都是 0。
fn net_dev() -> String {
    let mut s = String::from(
        "Inter-|   Receive                                                |  Transmit\n \
         face |bytes    packets errs drop fifo frame compressed multicast|\
         bytes    packets errs drop fifo colls carrier compressed\n",
    );
    for i in appine_hal::net::interfaces() {
        writeln!(s, "{:>6}: {}", i.name, ["0"; 16].join(" ")).unwrap();
    }
    s
}

fn random_uuid() -> String {
    let mut b = [0u8; 16];
    let _ = appine_hal::fill_random(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format_uuid(&b)
}

pub(crate) fn format_uuid(b: &[u8; 16]) -> String {
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// 打开 procfs 文件时生成的内容。
pub(crate) fn content(ps: &View<'_>, node: &PseudoNode) -> Result<Vec<u8>, Errno> {
    use PseudoNode::*;
    let s = match node {
        Global(g) => match g {
            self::Global::Cpuinfo => cpuinfo(),
            self::Global::Meminfo => meminfo(),
            self::Global::Stat => global_stat(ps),
            self::Global::Uptime => {
                let up = uptime_ns() as f64 / 1e9;
                format!("{up:.2} {:.2}\n", up * cpus() as f64)
            }
            self::Global::Loadavg => format!(
                "0.00 0.00 0.00 1/{} {}\n",
                ps.k.tasks.len(),
                ps.k.groups.keys().max().unwrap_or(&1)
            ),
            self::Global::Version => {
                "Linux version 6.1.0-appine (appine@localhost) #1 SMP PREEMPT appine\n".to_owned()
            }
            self::Global::Filesystems => "\text4\nnodev\tproc\nnodev\tdevpts\n".to_owned(),
            self::Global::Cmdline => "appine\n".to_owned(),
            self::Global::Mounts => return Err(Errno::EINVAL),
        },
        Static(path) => match &path[..] {
            b"net/dev" => net_dev(),
            b"sys/fs/file-max" => "9223372036854775807\n".to_owned(),
            b"sys/kernel/cap_last_cap" => "40\n".to_owned(),
            b"sys/kernel/hostname" => "localhost\n".to_owned(),
            b"sys/kernel/ngroups_max" => "65536\n".to_owned(),
            b"sys/kernel/osrelease" => "6.1.0-appine\n".to_owned(),
            b"sys/kernel/ostype" => "Linux\n".to_owned(),
            b"sys/kernel/pid_max" => "4194304\n".to_owned(),
            b"sys/kernel/random/boot_id" => format!("{}\n", format_uuid(&ps.k.boot_id)),
            b"sys/kernel/random/uuid" => format!("{}\n", random_uuid()),
            b"sys/kernel/version" => "#1 SMP PREEMPT appine\n".to_owned(),
            b"sys/vm/overcommit_memory" => "0\n".to_owned(),
            _ => return Err(Errno::EISDIR),
        },
        Entry(pid, tid, e) => {
            let task = ps.task(*pid, *tid);
            match e {
                self::Entry::Stat => pid_stat(ps, *pid, *tid)?,
                self::Entry::Status => pid_status(ps, *pid, *tid)?,
                self::Entry::Statm => {
                    let (vsize, rss) = mem_of(task.as_deref());
                    format!("{} {rss} 0 0 0 {rss} 0\n", vsize / 4096)
                }
                self::Entry::Comm => format!("{}\n", comm_str(task.as_deref(), *pid)),
                self::Entry::Cmdline => {
                    return Ok(task.map_or(Vec::new(), |t| guest_bytes(&t, |m| m.args)));
                }
                self::Entry::Environ => {
                    return Ok(task.map_or(Vec::new(), |t| guest_bytes(&t, |m| m.env)));
                }
                self::Entry::Maps => task.map_or(String::new(), |t| maps(&t)),
                self::Entry::Mounts => MOUNTS.to_owned(),
                self::Entry::OomScoreAdj => "0\n".to_owned(),
                _ => return Err(Errno::EINVAL),
            }
        }
        _ => return Err(Errno::EINVAL),
    };
    Ok(s.into_bytes())
}
