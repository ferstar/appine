//! 合成文件系统（设计文档 §8.2）：`/proc`（procfs）、`/dev/pts`（devpts）和 `/sys`（sysfs，
//! 只有 CPU 的拓扑）。
//!
//! 节点是 [`PseudoNode`]，目录结构经由 [`View`]（持有 BKL 时对内核状态的只读视图）
//! 实现 [`Pseudo`]。procfs 的文件在打开时生成一份完整的内容（[`content`]），
//! 之后的读都在这份快照上进行，与 Linux 的 seq_file 一次读完时的效果相同。

use std::collections::HashSet;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use appine_engine::PAGE_SIZE;

use crate::cell::RefCell;
use crate::cred::{Cred, cap};
use crate::errno::Errno;
use crate::fd::{FileKind, OpenFile, oflags};
use crate::ipc;
use crate::kernel::{INIT, Kernel, Tid};
use crate::mm::Region;
use crate::net::{AF_INET, AF_INET6, Table};
use crate::process::{Signal, ThreadGroup};
use crate::signal::SigAction;
use crate::syscall::SocketRef;
use crate::task::Task;
use crate::tty::{PTS_MAJOR, Tty};
use crate::uaccess::UserSlice;
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
    /// cgroup 的控制器（[`crate::cgroup`]）。
    Cgroups,
}

const GLOBALS: [(&[u8], Global); 10] = [
    (b"cpuinfo", Global::Cpuinfo),
    (b"meminfo", Global::Meminfo),
    (b"stat", Global::Stat),
    (b"uptime", Global::Uptime),
    (b"loadavg", Global::Loadavg),
    (b"version", Global::Version),
    (b"filesystems", Global::Filesystems),
    (b"cmdline", Global::Cmdline),
    (b"mounts", Global::Mounts),
    (b"cgroups", Global::Cgroups),
];

/// `/proc/<pid>` 下的项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Entry {
    Auxv,
    Stat,
    Status,
    Statm,
    Cmdline,
    Comm,
    Environ,
    Limits,
    Maps,
    Mem,
    Smaps,
    SmapsRollup,
    Pagemap,
    Mounts,
    MountInfo,
    Cwd,
    Exe,
    Root,
    Fd,
    FdInfo,
    Task,
    OomScoreAdj,
    CoredumpFilter,
    /// 所在的 cgroup（[`crate::cgroup`]）。
    Cgroup,
}

const ENTRIES: [(&[u8], Entry); 24] = [
    (b"auxv", Entry::Auxv),
    (b"stat", Entry::Stat),
    (b"status", Entry::Status),
    (b"statm", Entry::Statm),
    (b"cmdline", Entry::Cmdline),
    (b"comm", Entry::Comm),
    (b"environ", Entry::Environ),
    (b"limits", Entry::Limits),
    (b"maps", Entry::Maps),
    (b"mem", Entry::Mem),
    (b"smaps", Entry::Smaps),
    (b"smaps_rollup", Entry::SmapsRollup),
    (b"pagemap", Entry::Pagemap),
    (b"mounts", Entry::Mounts),
    (b"mountinfo", Entry::MountInfo),
    (b"cwd", Entry::Cwd),
    (b"exe", Entry::Exe),
    (b"root", Entry::Root),
    (b"fd", Entry::Fd),
    (b"fdinfo", Entry::FdInfo),
    (b"task", Entry::Task),
    (b"oom_score_adj", Entry::OomScoreAdj),
    (b"coredump_filter", Entry::CoredumpFilter),
    (b"cgroup", Entry::Cgroup),
];

/// `/proc/net`、`/proc/sys` 下固定的目录和文件（相对 `/proc` 的路径，目录以 `/` 结尾）。
const STATIC: [&[u8]; 70] = [
    b"net/",
    b"net/dev",
    b"net/icmp",
    b"net/icmp6",
    b"net/if_inet6",
    b"net/raw",
    b"net/raw6",
    b"net/tcp",
    b"net/tcp6",
    b"net/udp",
    b"net/udp6",
    b"net/unix",
    b"sys/",
    b"sys/fs/",
    b"sys/fs/aio-max-nr",
    b"sys/fs/aio-nr",
    b"sys/fs/file-max",
    b"sys/fs/inotify/",
    b"sys/fs/inotify/max_queued_events",
    b"sys/fs/inotify/max_user_instances",
    b"sys/fs/inotify/max_user_watches",
    b"sys/fs/lease-break-time",
    b"sys/fs/mqueue/",
    b"sys/fs/mqueue/msg_default",
    b"sys/fs/mqueue/msg_max",
    b"sys/fs/mqueue/msgsize_default",
    b"sys/fs/mqueue/msgsize_max",
    b"sys/fs/mqueue/queues_max",
    b"sys/fs/nr_open",
    b"sys/fs/pipe-max-size",
    b"sys/fs/pipe-user-pages-hard",
    b"sys/fs/pipe-user-pages-soft",
    b"sys/kernel/",
    b"sys/kernel/cap_last_cap",
    b"sys/kernel/core_pattern",
    b"sys/kernel/core_uses_pid",
    b"sys/kernel/domainname",
    b"sys/kernel/hostname",
    b"sys/kernel/msgmax",
    b"sys/kernel/msgmnb",
    b"sys/kernel/msgmni",
    b"sys/kernel/ngroups_max",
    b"sys/kernel/ns_last_pid",
    b"sys/kernel/osrelease",
    b"sys/kernel/ostype",
    b"sys/kernel/pid_max",
    b"sys/kernel/random/",
    b"sys/kernel/random/boot_id",
    b"sys/kernel/random/entropy_avail",
    b"sys/kernel/random/poolsize",
    b"sys/kernel/random/uuid",
    b"sys/kernel/sched_rr_timeslice_ms",
    b"sys/kernel/seccomp/",
    b"sys/kernel/seccomp/actions_avail",
    b"sys/kernel/seccomp/actions_logged",
    b"sys/kernel/sem",
    b"sys/kernel/shmall",
    b"sys/kernel/shmmax",
    b"sys/kernel/shmmni",
    b"sys/kernel/tainted",
    b"sys/kernel/threads-max",
    b"sys/kernel/version",
    b"sys/vm/",
    b"sys/vm/max_map_count",
    b"sys/vm/overcommit_memory",
    b"sys/vm/overcommit_ratio",
    b"sysvipc/",
    b"sysvipc/msg",
    b"sysvipc/sem",
    b"sysvipc/shm",
];

/// `/sys` 下固定的目录和文件（相对 `/sys` 的路径，目录以 `/` 结尾）。只有 CPU 的拓扑：
/// busybox 的 `nproc --all`、lscpu、glibc 的 get_nprocs 读它们。各个 `cpu<n>` 是
/// [`PseudoNode::SysCpu`]。
const SYS_STATIC: [&[u8]; 8] = [
    b"devices/",
    b"devices/system/",
    b"devices/system/cpu/",
    b"devices/system/cpu/offline",
    b"devices/system/cpu/online",
    b"devices/system/cpu/possible",
    b"devices/system/cpu/present",
    b"fs/",
];

/// `/sys/devices/system/cpu`：各个 `cpu<n>` 在它下面。
const SYS_CPU_DIR: &[u8] = b"devices/system/cpu/";

/// `/sys/devices/system/cpu/cpu<n>` 下的目录和文件（相对 `cpu<n>` 的路径）。
const CPU_FILES: [&[u8]; 19] = [
    b"online",
    b"regs/",
    b"regs/identification/",
    b"regs/identification/midr_el1",
    b"regs/identification/revidr_el1",
    b"topology/",
    b"topology/cluster_cpus",
    b"topology/cluster_cpus_list",
    b"topology/cluster_id",
    b"topology/core_cpus",
    b"topology/core_cpus_list",
    b"topology/core_id",
    b"topology/core_siblings",
    b"topology/core_siblings_list",
    b"topology/package_cpus",
    b"topology/package_cpus_list",
    b"topology/physical_package_id",
    b"topology/thread_siblings",
    b"topology/thread_siblings_list",
];

/// `/sys/devices/system/cpu/cpu<n>` 下照搬宿主的目录和文件：缓存（`cache/`）和频率
/// （`cpufreq/`）。宿主对应的 CPU 有才有（[`host_cpu_has`]），内容也是宿主的，里面的 CPU
/// 编号换成 guest 的。lscpu、hwloc、OpenBLAS 读缓存的大小，Node.js 的 `os.cpus()` 读
/// `cpufreq/scaling_cur_freq`；没有 cpufreq 的宿主（虚拟机）上它们也没有，与 Linux 一样。
const HOST_CPU_FILES: [&[u8]; 68] = [
    b"cache/",
    b"cache/index0/",
    b"cache/index0/allocation_policy",
    b"cache/index0/coherency_line_size",
    b"cache/index0/id",
    b"cache/index0/level",
    b"cache/index0/number_of_sets",
    b"cache/index0/physical_line_partition",
    b"cache/index0/shared_cpu_list",
    b"cache/index0/shared_cpu_map",
    b"cache/index0/size",
    b"cache/index0/type",
    b"cache/index0/ways_of_associativity",
    b"cache/index0/write_policy",
    b"cache/index1/",
    b"cache/index1/allocation_policy",
    b"cache/index1/coherency_line_size",
    b"cache/index1/id",
    b"cache/index1/level",
    b"cache/index1/number_of_sets",
    b"cache/index1/physical_line_partition",
    b"cache/index1/shared_cpu_list",
    b"cache/index1/shared_cpu_map",
    b"cache/index1/size",
    b"cache/index1/type",
    b"cache/index1/ways_of_associativity",
    b"cache/index1/write_policy",
    b"cache/index2/",
    b"cache/index2/allocation_policy",
    b"cache/index2/coherency_line_size",
    b"cache/index2/id",
    b"cache/index2/level",
    b"cache/index2/number_of_sets",
    b"cache/index2/physical_line_partition",
    b"cache/index2/shared_cpu_list",
    b"cache/index2/shared_cpu_map",
    b"cache/index2/size",
    b"cache/index2/type",
    b"cache/index2/ways_of_associativity",
    b"cache/index2/write_policy",
    b"cache/index3/",
    b"cache/index3/allocation_policy",
    b"cache/index3/coherency_line_size",
    b"cache/index3/id",
    b"cache/index3/level",
    b"cache/index3/number_of_sets",
    b"cache/index3/physical_line_partition",
    b"cache/index3/shared_cpu_list",
    b"cache/index3/shared_cpu_map",
    b"cache/index3/size",
    b"cache/index3/type",
    b"cache/index3/ways_of_associativity",
    b"cache/index3/write_policy",
    b"cpufreq/",
    b"cpufreq/affected_cpus",
    b"cpufreq/cpuinfo_cur_freq",
    b"cpufreq/cpuinfo_max_freq",
    b"cpufreq/cpuinfo_min_freq",
    b"cpufreq/cpuinfo_transition_latency",
    b"cpufreq/related_cpus",
    b"cpufreq/scaling_available_frequencies",
    b"cpufreq/scaling_available_governors",
    b"cpufreq/scaling_cur_freq",
    b"cpufreq/scaling_driver",
    b"cpufreq/scaling_governor",
    b"cpufreq/scaling_max_freq",
    b"cpufreq/scaling_min_freq",
    b"cpufreq/scaling_setspeed",
];

/// 固定的目录树（[`STATIC`] 这样相对树根的路径，目录以 `/` 结尾）里，目录 `dir`（树根是
/// 空的）下名为 `name` 的项。
fn tree_lookup(tree: &[&'static [u8]], dir: &[u8], name: &[u8]) -> Option<&'static [u8]> {
    tree.iter().copied().find(|p| {
        let p = p.strip_suffix(b"/").unwrap_or(p);
        p.len() == dir.len() + name.len() && p.starts_with(dir) && p.ends_with(name)
    })
}

/// 固定的目录树里目录 `dir` 下直接的项。
fn tree_children<'t>(
    tree: &'t [&'static [u8]],
    dir: &'t [u8],
) -> impl Iterator<Item = &'static [u8]> + 't {
    tree.iter().copied().filter(move |p| {
        let p = p.strip_suffix(b"/").unwrap_or(p);
        p.len() > dir.len() && p.starts_with(dir) && !p[dir.len()..].contains(&b'/')
    })
}

/// 固定的目录树里的上级目录；直接在树根下的是 None。
fn tree_parent(tree: &[&'static [u8]], path: &[u8]) -> Option<&'static [u8]> {
    let p = path.strip_suffix(b"/").unwrap_or(path);
    let i = p.iter().rposition(|&b| b == b'/')?;
    tree.iter().copied().find(|q| *q == &p[..=i])
}

/// 路径的最后一个组件。
fn tree_name(path: &[u8]) -> &[u8] {
    let p = path.strip_suffix(b"/").unwrap_or(path);
    &p[p.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1)..]
}

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
    /// `/proc/<pid>/fdinfo/<n>`。
    FdInfo(Tid, i32),
    /// `/sys`。
    SysRoot,
    /// `/sys` 下的目录或文件，取值来自 [`SYS_STATIC`]。
    Sys(&'static [u8]),
    /// `/sys/devices/system/cpu/cpu<n>`（路径为空），或者它下面的目录和文件（路径取值来自
    /// [`CPU_FILES`]）。
    SysCpu(u32, &'static [u8]),
    /// `/sys/fs/cgroup`（名字为空），或者它下面的文件（名字取值来自 [`crate::cgroup::FILES`]）。
    Cgroup(&'static [u8]),
}

/// `/proc/<pid>/task/<tid>` 里的项：线程自己的，以及与进程相同的（调试器读线程的 maps、
/// mem）。
const TASK_ENTRIES: [Entry; 18] = [
    Entry::Stat,
    Entry::Status,
    Entry::Statm,
    Entry::Comm,
    Entry::Cmdline,
    Entry::Environ,
    Entry::Limits,
    Entry::Auxv,
    Entry::Mem,
    Entry::Maps,
    Entry::Smaps,
    Entry::SmapsRollup,
    Entry::Mounts,
    Entry::MountInfo,
    Entry::Cwd,
    Entry::Exe,
    Entry::Root,
    Entry::Cgroup,
];

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
        Mount::Sys => PseudoNode::SysRoot,
        Mount::Cgroup => PseudoNode::Cgroup(b""),
    }
}

pub(crate) fn mount_of(node: &PseudoNode) -> Mount {
    match node {
        PseudoNode::PtsRoot | PseudoNode::Pts(_) | PseudoNode::PtsPtmx => Mount::Pts,
        PseudoNode::SysRoot | PseudoNode::Sys(_) | PseudoNode::SysCpu(..) => Mount::Sys,
        PseudoNode::Cgroup(_) => Mount::Cgroup,
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

/// stat 和 readdir 给出的 inode：`/proc/<pid>` 和它下面的项还取决于是哪个进程，与 Linux
/// 一样，pid 被新进程重新使用之后 inode 不同。
pub(crate) fn node_ino(k: &Kernel, node: &PseudoNode) -> u64 {
    let pid = match node {
        PseudoNode::Pid(pid, _)
        | PseudoNode::Entry(pid, ..)
        | PseudoNode::Fd(pid, _)
        | PseudoNode::OpenFd(pid, _)
        | PseudoNode::FdInfo(pid, _) => *pid,
        _ => return ino_of(node),
    };
    let Some(g) = k.groups.get(&pid) else {
        return ino_of(node);
    };
    let mut h = std::hash::DefaultHasher::new();
    node.hash(&mut h);
    g.borrow().serial.hash(&mut h);
    h.finish() | 1
}

impl<'a> View<'a> {
    /// 读者（`self` 指向的线程）的根目录：procfs 里的路径相对它显示。
    fn root(&self) -> Node {
        self.k
            .tasks
            .get(&self.tid)
            .map_or(Node::ROOT, |t| t.res.borrow().fs.borrow().root.clone())
    }

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
            PtsRoot | ProcRoot | SysRoot => return None,
            Pts(_) | PtsPtmx => PtsRoot,
            Global(_) | SelfLink | ThreadSelf | Pid(_, None) => ProcRoot,
            Static(path) => tree_parent(&STATIC, path).map_or(ProcRoot, Static),
            Sys(path) => tree_parent(&SYS_STATIC, path).map_or(SysRoot, Sys),
            SysCpu(_, b"") => Sys(SYS_CPU_DIR),
            SysCpu(n, path) => tree_parent(&CPU_FILES, path)
                .or_else(|| tree_parent(&HOST_CPU_FILES, path))
                .map_or(SysCpu(*n, b""), |p| SysCpu(*n, p)),
            Cgroup(b"") => Sys(b"fs/"),
            Cgroup(_) => Cgroup(b""),
            Pid(pid, Some(_)) => Entry(*pid, None, self::Entry::Task),
            Entry(pid, tid, _) => Pid(*pid, *tid),
            Fd(pid, _) | OpenFd(pid, _) => Entry(*pid, None, self::Entry::Fd),
            FdInfo(pid, _) => Entry(*pid, None, self::Entry::FdInfo),
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
            Static(path) | Sys(path) => tree_name(path).to_vec(),
            SysRoot => b"sys".to_vec(),
            SysCpu(n, b"") => format!("cpu{n}").into_bytes(),
            SysCpu(_, path) => tree_name(path).to_vec(),
            Cgroup(b"") => b"cgroup".to_vec(),
            Cgroup(name) => name.to_vec(),
            Pid(pid, None) => pid.to_string().into_bytes(),
            Pid(_, Some(tid)) => tid.to_string().into_bytes(),
            Entry(_, _, e) => ENTRIES.iter().find(|(_, x)| x == e).unwrap().0.to_vec(),
            Fd(_, fd) | OpenFd(_, fd) | FdInfo(_, fd) => fd.to_string().into_bytes(),
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
                _ if tree_lookup(&STATIC, b"", name).is_some() => {
                    tree_lookup(&STATIC, b"", name).map(Static)
                }
                _ => GLOBALS
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, g)| Global(*g))
                    .or_else(|| {
                        let pid = parse_num(name)? as Tid;
                        self.pid_exists(pid, None).then_some(Pid(pid, None))
                    }),
            },
            Static(path) => tree_lookup(&STATIC, path, name).map(Static),
            SysRoot => tree_lookup(&SYS_STATIC, b"", name).map(Sys),
            Sys(path) => tree_lookup(&SYS_STATIC, path, name).map(Sys).or_else(|| {
                if *path == b"fs/" && name == b"cgroup" {
                    return Some(Cgroup(b""));
                }
                let n = name.strip_prefix(b"cpu").and_then(parse_num)?;
                let n = u32::try_from(n).ok().filter(|&n| (n as usize) < cpus())?;
                (*path == SYS_CPU_DIR).then_some(SysCpu(n, b""))
            }),
            Cgroup(b"") => crate::cgroup::FILES
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(n, _)| Cgroup(n)),
            SysCpu(n, path) => tree_lookup(&CPU_FILES, path, name)
                .or_else(|| {
                    tree_lookup(&HOST_CPU_FILES, path, name).filter(|p| host_cpu_has(*n, p))
                })
                .map(|p| SysCpu(*n, p)),
            Pid(pid, tid) => {
                let allowed = |e: &self::Entry| tid.is_none() || TASK_ENTRIES.contains(e);
                ENTRIES
                    .iter()
                    .find(|(n, e)| *n == name && allowed(e))
                    .map(|(_, e)| Entry(*pid, *tid, *e))
                    .filter(|_| self.pid_exists(*pid, *tid))
            }
            Entry(pid, None, e @ (self::Entry::Fd | self::Entry::FdInfo)) => parse_num(name)
                .map(|fd| fd as i32)
                .filter(|&fd| self.open_file(*pid, fd).is_some())
                .map(|fd| match e {
                    self::Entry::Fd => Fd(*pid, fd),
                    _ => FdInfo(*pid, fd),
                }),
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
            PtsRoot | ProcRoot | Pid(..) | SysRoot => mode::S_IFDIR,
            Static(p) | Sys(p) if p.ends_with(b"/") => mode::S_IFDIR,
            SysCpu(_, p) if p.is_empty() || p.ends_with(b"/") => mode::S_IFDIR,
            Cgroup(b"") => mode::S_IFDIR,
            Entry(_, _, self::Entry::Fd | self::Entry::FdInfo | self::Entry::Task) => mode::S_IFDIR,
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
            Entry(pid, tid, self::Entry::Root) => {
                let task = self.task(*pid, *tid).ok_or(Errno::ENOENT)?;
                let root = task.res.borrow().fs.borrow().root.clone();
                self.vfs.path_of(self, &self.root(), &root)
            }
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
                self.vfs.path_of(self, &self.root(), &cwd)
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
            // 已经删除的文件没法按 inode 重新打开（仓库里的 object 已经删了）：指向打开文件
            // 本身，重新打开时复制宿主文件
            FileKind::Regular { ino, file }
                if self
                    .vfs
                    .store()
                    .stat_open(*ino, file)
                    .is_ok_and(|st| st.nlink == 0) =>
            {
                Node::Pseudo(PseudoNode::OpenFd(*pid, *fd))
            }
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
        FileKind::Socket(s) => format!("socket:[{}]", socket_ino(s)).into_bytes(),
        FileKind::Epoll(_) => b"anon_inode:[eventpoll]".to_vec(),
        FileKind::EventFd(_) => b"anon_inode:[eventfd]".to_vec(),
        FileKind::Inotify(_) => b"anon_inode:inotify".to_vec(),
        FileKind::Pidfd(_) => b"anon_inode:[pidfd]".to_vec(),
        FileKind::SeccompNotify(_) => b"anon_inode:seccomp notify".to_vec(),
        FileKind::TimerFd(_) => b"anon_inode:[timerfd]".to_vec(),
        FileKind::SignalFd(_) => b"anon_inode:[signalfd]".to_vec(),
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
    let sysfs = matches!(mount_of(node), Mount::Sys | Mount::Cgroup);
    let perm = match (kind, node) {
        (mode::S_IFDIR, _) if sysfs => 0o755,
        (_, PseudoNode::Cgroup(name)) if crate::cgroup::FILES.contains(&(name, true)) => 0o644,
        (mode::S_IFDIR, _) => 0o555,
        (mode::S_IFLNK, _) => 0o777,
        (_, PseudoNode::Pts(n)) => pty(ps.k, *n).map_or(0o620, |t| t.borrow().mode),
        (_, PseudoNode::PtsPtmx) => 0o666,
        (_, PseudoNode::Entry(_, _, Entry::Mem)) => 0o600,
        (_, n) if writable(n) => 0o644,
        (_, PseudoNode::Entry(_, _, Entry::Pagemap | Entry::Auxv)) => 0o400,
        _ => 0o444,
    };
    let rdev_value = match node {
        PseudoNode::Pts(n) => rdev(PTS_MAJOR, *n),
        PseudoNode::PtsPtmx => rdev(5, 2),
        _ => 0,
    };
    let now = crate::tty::wall_ns();
    let times = match node {
        PseudoNode::Pts(n) => pty(ps.k, *n).map(|t| {
            let t = t.borrow();
            (t.atime_ns, t.mtime_ns, t.ctime_ns)
        }),
        _ => None,
    };
    let (atime_ns, mtime_ns, ctime_ns) = times.unwrap_or((now, now, now));
    let (uid, gid) = owner_of(ps.k, node);
    Ok(Stat {
        ino: node_ino(ps.k, node),
        mode: kind | perm,
        nlink: if kind == mode::S_IFDIR { 2 } else { 1 },
        uid,
        gid,
        rdev: rdev_value,
        // sysfs 的文件与 Linux 一样报告一页
        size: if sysfs && kind == mode::S_IFREG {
            4096
        } else {
            0
        },
        atime_ns,
        mtime_ns,
        ctime_ns,
    })
}

/// `/dev/pts/<n>` 的终端，已经关掉的是 None。
pub(crate) fn pty(k: &Kernel, n: u32) -> Option<Rc<RefCell<Tty>>> {
    k.ptys.get(&n).and_then(std::rc::Weak::upgrade)
}

/// 节点的属主和属组：`/proc/<pid>` 下是进程的有效用户 ID 和组 ID，`/dev/pts/<n>`
/// 记在终端上（[`Tty::owner`]），其余是 root。
fn owner_of(k: &Kernel, node: &PseudoNode) -> (u32, u32) {
    match node {
        PseudoNode::Pid(pid, _) | PseudoNode::Entry(pid, ..) | PseudoNode::Fd(pid, _) => {
            k.groups.get(pid).map_or((0, 0), |g| {
                let c = &g.borrow().cred;
                (c.euid, c.egid)
            })
        }
        PseudoNode::Pts(n) => pty(k, *n).map_or((0, 0), |t| t.borrow().owner),
        _ => (0, 0),
    }
}

/// 可以写入的 procfs 文件。
pub(crate) fn writable(node: &PseudoNode) -> bool {
    matches!(
        node,
        PseudoNode::Entry(_, None, Entry::OomScoreAdj | Entry::CoredumpFilter)
            | PseudoNode::Entry(_, _, Entry::Mem)
            | PseudoNode::Static(
                b"sys/kernel/hostname"
                    | b"sys/kernel/domainname"
                    | b"sys/kernel/pid_max"
                    | b"sys/kernel/threads-max"
                    | b"sys/kernel/ns_last_pid"
                    | b"sys/kernel/core_pattern"
                    | b"sys/kernel/seccomp/actions_logged"
                    | b"sys/kernel/core_uses_pid"
                    | b"sys/kernel/msgmax"
                    | b"sys/kernel/msgmnb"
                    | b"sys/kernel/msgmni"
                    | b"sys/kernel/sem"
                    | b"sys/kernel/shmall"
                    | b"sys/kernel/shmmax"
                    | b"sys/kernel/shmmni"
                    | b"sys/fs/lease-break-time"
                    | b"sys/fs/aio-max-nr"
                    | b"sys/fs/mqueue/msg_default"
                    | b"sys/fs/mqueue/msg_max"
                    | b"sys/fs/mqueue/msgsize_default"
                    | b"sys/fs/mqueue/msgsize_max"
                    | b"sys/fs/mqueue/queues_max"
                    | b"sys/fs/nr_open"
                    | b"sys/fs/pipe-max-size"
                    | b"sys/fs/inotify/max_queued_events"
                    | b"sys/fs/inotify/max_user_instances"
                    | b"sys/fs/inotify/max_user_watches"
                    | b"sys/vm/max_map_count"
            )
    )
}

/// 写 procfs 文件：`oom_score_adj`、主机名、域名和 System V IPC 的限制记下新值，其余
/// 可写的文件接受写入但不起作用。`cred` 是写入者的凭据。
pub(crate) fn write(
    k: &mut Kernel,
    cred: &Cred,
    node: &PseudoNode,
    data: &[u8],
) -> Result<(), Errno> {
    if let PseudoNode::Static(path) = node
        && let Some(name) = path.strip_prefix(b"sys/kernel/")
        && k.ipc.tunables.read(name).is_some()
    {
        return k.ipc.tunables.write(name, data);
    }
    if let PseudoNode::Static(path @ (b"sys/kernel/hostname" | b"sys/kernel/domainname")) = node {
        let name = data.strip_suffix(b"\n").unwrap_or(data);
        if name.len() > crate::kernel::UTS_LEN {
            return Err(Errno::EINVAL);
        }
        let slot = if *path == b"sys/kernel/hostname" {
            &mut k.hostname
        } else {
            &mut k.domainname
        };
        *slot = name.to_vec();
    }
    if let PseudoNode::Static(b"sys/vm/max_map_count") = node {
        let n: usize = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|&n| n <= i32::MAX as usize)
            .ok_or(Errno::EINVAL)?;
        crate::mm::MAX_MAP_COUNT.store(n, std::sync::atomic::Ordering::Relaxed);
    }
    if let PseudoNode::Static(path) = node
        && let Some(name) = path.strip_prefix(b"sys/fs/mqueue/")
        && let Some(slot) = k.mq_limits.slot(name)
    {
        *slot = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|&v: &u64| v > 0)
            .ok_or(Errno::EINVAL)?;
    }
    if let PseudoNode::Static(b"sys/fs/aio-max-nr") = node {
        k.aio_max_nr = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or(Errno::EINVAL)?;
    }
    if let PseudoNode::Static(path) = node
        && let Some(name) = path.strip_prefix(b"sys/fs/inotify/")
    {
        // 与 Linux 一样不接受负数
        let n: usize = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|&n| n <= i32::MAX as usize)
            .ok_or(Errno::EINVAL)?;
        let mut r = k.inotify.borrow_mut();
        match name {
            b"max_queued_events" => r.max_queued = n,
            b"max_user_instances" => r.max_instances = n,
            _ => r.max_watches = n,
        }
    }
    // 与 Linux 一样在 0 到 pid_max 之间（checkpoint/restore 用它让新进程拿到指定的 pid）
    if let PseudoNode::Static(b"sys/kernel/ns_last_pid") = node {
        let pid: i32 = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|&n| (0..=crate::kernel::PID_MAX).contains(&n))
            .ok_or(Errno::EINVAL)?;
        k.set_last_pid(pid);
    }
    // 与 Linux 一样在 MIN_THREADS 到 FUTEX_TID_MASK 之间；已经设置的 RLIMIT_NPROC 不变
    if let PseudoNode::Static(b"sys/kernel/threads-max") = node {
        k.threads_max = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|n| (crate::process::MIN_THREADS..=0x3fff_ffff).contains(n))
            .ok_or(Errno::EINVAL)?;
    }
    if let PseudoNode::Static(b"sys/fs/lease-break-time") = node {
        k.lease_break_time = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or(Errno::EINVAL)?;
    }
    // fd 表的容量固定：nr_open 可以调小，不能超过它
    if let PseudoNode::Static(b"sys/fs/nr_open") = node {
        k.nr_open = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|n| (64..=crate::fd::MAX_FDS).contains(n))
            .ok_or(Errno::EINVAL)?;
    }
    // 与 Linux 一样按页向上取整到 2 的幂，超过 2^31 是 EINVAL
    if let PseudoNode::Static(b"sys/fs/pipe-max-size") = node {
        let size: u64 = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|&n| n <= 1 << 31)
            .ok_or(Errno::EINVAL)?;
        k.pipe_max_size = (size as usize)
            .max(crate::pipe::PIPE_BUF)
            .next_power_of_two();
    }
    if let PseudoNode::Entry(pid, _, Entry::OomScoreAdj) = node {
        let value: i32 = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|v| (-1000..=1000).contains(v))
            .ok_or(Errno::EINVAL)?;
        let group = k.groups.get(pid).ok_or(Errno::ESRCH)?;
        let mut g = group.borrow_mut();
        // 与 Linux 一样：调到下限以下要求 CAP_SYS_RESOURCE，有它时写的值同时成为新的下限
        let privileged = cred.capable(cap::SYS_RESOURCE);
        if value < g.oom_score_adj_min && !privileged {
            return Err(Errno::EACCES);
        }
        g.oom_score_adj = value;
        if privileged {
            g.oom_score_adj_min = value;
        }
    }
    if let PseudoNode::Static(b"sys/kernel/seccomp/actions_logged") = node {
        crate::seccomp::set_actions_logged(k, cred, data)?;
    }
    // 与 Linux 的 proc_dostring 一样，长了截断（core_pattern 最长 127 个字节）
    if let PseudoNode::Static(b"sys/kernel/core_pattern") = node {
        let pattern = data.strip_suffix(b"\n").unwrap_or(data);
        let len = pattern.len().min(crate::coredump::PATTERN_MAX - 1);
        k.core_pattern = pattern[..len].to_vec();
    }
    if let PseudoNode::Static(b"sys/kernel/core_uses_pid") = node {
        let v: i64 = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or(Errno::EINVAL)?;
        k.core_uses_pid = v != 0;
    }
    // 与 Linux 一样按 C 的写法解析（0x 开头是十六进制，0 开头是八进制），只取 9 位
    if let PseudoNode::Entry(pid, _, Entry::CoredumpFilter) = node {
        let s = std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?.trim();
        let value = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            u32::from_str_radix(hex, 16)
        } else if s.len() > 1 && s.starts_with('0') {
            u32::from_str_radix(&s[1..], 8)
        } else {
            s.parse()
        }
        .map_err(|_| Errno::EINVAL)?;
        let group = k.groups.get(pid).ok_or(Errno::ESRCH)?;
        group.borrow_mut().coredump_filter = value & 0x1ff;
    }
    Ok(())
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
            nodes.extend([SelfLink, ThreadSelf]);
            nodes.extend(tree_children(&STATIC, b"").map(Static));
            nodes.extend(GLOBALS.iter().map(|(_, g)| Global(*g)));
            nodes.extend(ps.k.groups.keys().map(|&pid| Pid(pid, None)));
        }
        Static(path) => nodes.extend(tree_children(&STATIC, path).map(Static)),
        SysRoot => nodes.extend(tree_children(&SYS_STATIC, b"").map(Sys)),
        Sys(path) => {
            nodes.extend(tree_children(&SYS_STATIC, path).map(Sys));
            if *path == SYS_CPU_DIR {
                nodes.extend((0..cpus() as u32).map(|n| SysCpu(n, b"")));
            }
            if *path == b"fs/" {
                nodes.push(Cgroup(b""));
            }
        }
        Cgroup(b"") => nodes.extend(crate::cgroup::FILES.iter().map(|(n, _)| Cgroup(n))),
        SysCpu(n, path) => {
            nodes.extend(tree_children(&CPU_FILES, path).map(|p| SysCpu(*n, p)));
            nodes.extend(
                tree_children(&HOST_CPU_FILES, path)
                    .filter(|p| host_cpu_has(*n, p))
                    .map(|p| SysCpu(*n, p)),
            );
        }
        Pid(pid, tid) => nodes.extend(
            ENTRIES
                .iter()
                .filter(|(_, e)| tid.is_none() || TASK_ENTRIES.contains(e))
                .map(|(_, e)| Entry(*pid, *tid, *e)),
        ),
        Entry(pid, None, e @ (self::Entry::Fd | self::Entry::FdInfo)) => {
            if let Some(task) = ps.leader(*pid) {
                let fds = task.res.borrow().fds.borrow().open_fds();
                nodes.extend(fds.into_iter().map(|fd| match e {
                    self::Entry::Fd => Fd(*pid, fd),
                    _ => FdInfo(*pid, fd),
                }));
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
                ino: node_ino(ps.k, &n),
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
    appine_hal::host_cpus().len()
}

/// 线程组的 CPU 时间（纳秒）：活着的线程加上已经退出的线程。
/// 进程状态的字母：R 正在执行 guest 代码，S 在内核里或等待，T 停止，Z 僵尸。
fn state_of(ps: &View<'_>, g: &ThreadGroup, tid: Option<Tid>) -> (char, &'static str) {
    if g.zombie.is_some() {
        return ('Z', "zombie");
    }
    // 停在 ptrace-stop 里（PTRACE_LISTEN 时也是，与 Linux 一样）
    if let Some(t) = ps.task(g.tgid, tid)
        && t.trace.borrow().stop.is_some()
    {
        return ('t', "tracing stop");
    }
    if g.stopped {
        return ('T', "stopped");
    }
    // 与 Linux 相同，进程的状态是主线程（主线程已退出时是代表线程）的状态：在内核里
    // 等待是 S，其余（执行 guest 代码、处理 syscall、刚创建还没运行、被叫醒了还没跑起来）
    // 都是 R
    if ps.task(g.tgid, tid).is_some_and(|t| t.asleep()) {
        ('S', "sleeping")
    } else {
        ('R', "running")
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

pub(crate) fn comm_str(task: Option<&Task>, pid: Tid) -> String {
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
    let nice = task.as_ref().map_or(0, |t| t.nice.get());
    let (state, _) = state_of(ps, &g, tid);
    let (tty_nr, tpgid) = ctty_info(ps, &g);
    let cpu = match (tid, &task) {
        (Some(_), Some(t)) => t.cpu(),
        _ => g.cpu(ps.k),
    };
    let (vsize, rss) = mem_of(task.as_deref());
    let (sigign, sigcgt) = sig_dispositions(&g);
    let (pending, blocked) = task.as_ref().map_or((0, 0), |t| {
        (t.pending.borrow().set() | g.pending.set(), t.sigmask.get())
    });
    let (args, env) = task.as_ref().map_or(((0, 0), (0, 0)), |t| {
        let res = t.res.borrow();
        let mm = res.mm.borrow();
        (mm.args, mm.env)
    });
    let mut s = String::new();
    write!(
        s,
        "{} ({}) {} {} {} {} {} {} 4194304 0 0 0 0 {} {} {} {} {} {} {} 0 {} {} {} 18446744073709551615",
        tid.unwrap_or(pid),
        comm_str(task.as_deref(), pid),
        state,
        g.ppid,
        g.pgid,
        g.sid,
        tty_nr,
        tpgid,
        ticks(cpu.user_ns()),
        ticks(cpu.sys_ns),
        ticks(g.child_cpu.user_ns()),
        ticks(g.child_cpu.sys_ns),
        20 + nice,
        nice,
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
        .map_or((0, 0), |t| (t.pending.borrow().set(), t.sigmask.get()));
    let mut s = String::new();
    let comm = comm_str(task.as_deref(), pid);
    writeln!(s, "Name:\t{comm}").unwrap();
    writeln!(s, "Umask:\t{umask:04o}").unwrap();
    writeln!(s, "State:\t{state} ({state_name})").unwrap();
    writeln!(s, "Tgid:\t{pid}\nNgid:\t0\nPid:\t{}", tid.unwrap_or(pid)).unwrap();
    let tracer = task.as_ref().and_then(|t| t.trace.borrow().tracer);
    writeln!(s, "PPid:\t{}\nTracerPid:\t{}", g.ppid, tracer.unwrap_or(0)).unwrap();
    let c = &g.cred;
    writeln!(s, "Uid:\t{}\t{}\t{}\t{}", c.ruid, c.euid, c.suid, c.fsuid).unwrap();
    writeln!(s, "Gid:\t{}\t{}\t{}\t{}", c.rgid, c.egid, c.sgid, c.fsgid).unwrap();
    let groups: Vec<String> = c.groups.iter().map(u32::to_string).collect();
    writeln!(s, "FDSize:\t{fdsize}\nGroups:\t{}", groups.join(" ")).unwrap();
    if task.is_some() {
        let kb = vsize / 1024;
        writeln!(s, "VmPeak:\t{kb:>8} kB\nVmSize:\t{kb:>8} kB").unwrap();
        let locked = task
            .as_ref()
            .map_or(0, |t| t.res.borrow().mm.borrow().locked_bytes());
        writeln!(s, "VmLck:\t{:>8} kB\nVmPin:\t{:>8} kB", locked / 1024, 0).unwrap();
        let hwm = g.maxrss_kb.max(rss * 4);
        writeln!(s, "VmHWM:\t{hwm:>8} kB\nVmRSS:\t{:>8} kB", rss * 4).unwrap();
        // 不区分数据段、栈和代码；没有交换
        writeln!(s, "VmData:\t{kb:>8} kB\nVmStk:\t{:>8} kB", 0).unwrap();
        writeln!(s, "VmExe:\t{:>8} kB\nVmLib:\t{:>8} kB", 0, 0).unwrap();
        writeln!(s, "VmPTE:\t{:>8} kB\nVmSwap:\t{:>8} kB", 0, 0).unwrap();
    }
    writeln!(s, "Threads:\t{}", g.threads.len().max(1)).unwrap();
    let limit = g.rlimits[crate::process::RLIMIT_SIGPENDING as usize].0;
    writeln!(s, "SigQ:\t{}/{limit}", crate::signal::queued(ps.k, &g)).unwrap();
    writeln!(
        s,
        "SigPnd:\t{pending:016x}\nShdPnd:\t{:016x}",
        g.pending.set()
    )
    .unwrap();
    writeln!(
        s,
        "SigBlk:\t{blocked:016x}\nSigIgn:\t{sigign:016x}\nSigCgt:\t{sigcgt:016x}"
    )
    .unwrap();
    writeln!(s, "CapInh:\t{:016x}", c.cap_inheritable).unwrap();
    writeln!(s, "CapPrm:\t{:016x}", c.cap_permitted).unwrap();
    writeln!(s, "CapEff:\t{:016x}", c.cap_effective).unwrap();
    writeln!(s, "CapBnd:\t{:016x}", c.cap_bounding).unwrap();
    writeln!(s, "CapAmb:\t0000000000000000").unwrap();
    let (seccomp, filters) = task.as_ref().map_or((0, 0), |t| {
        let s = t.seccomp.borrow();
        (s.mode, s.filters())
    });
    writeln!(
        s,
        "NoNewPrivs:\t{}\nSeccomp:\t{seccomp}\nSeccomp_filters:\t{filters}",
        g.no_new_privs as u8
    )
    .unwrap();
    writeln!(s, "Cpus_allowed_list:\t0-{}", cpus() - 1).unwrap();
    // 宿主线程睡下（等待）一次是一次自愿切换：没被叫醒的线程不会变
    let (vol, nonvol) = task.as_ref().map_or((0, 0), |t| {
        let (v, n) = appine_hal::thread_ctxt_switches(t.host_tid.get()).unwrap_or((0, 0));
        let (bv, bn) = t.ctxt_base.get();
        (v.saturating_sub(bv), n.saturating_sub(bn))
    });
    writeln!(
        s,
        "voluntary_ctxt_switches:\t{vol}\nnonvoluntary_ctxt_switches:\t{nonvol}"
    )
    .unwrap();
    Ok(s)
}

/// `/proc/<pid>/limits` 各行的名字和单位，按资源编号排列（Linux 的 `lnames`）。
const LIMIT_NAMES: [(&str, &str); crate::process::RLIM_NLIMITS] = [
    ("Max cpu time", "seconds"),
    ("Max file size", "bytes"),
    ("Max data size", "bytes"),
    ("Max stack size", "bytes"),
    ("Max core file size", "bytes"),
    ("Max resident set", "bytes"),
    ("Max processes", "processes"),
    ("Max open files", "files"),
    ("Max locked memory", "bytes"),
    ("Max address space", "bytes"),
    ("Max file locks", "locks"),
    ("Max pending signals", "signals"),
    ("Max msgqueue size", "bytes"),
    ("Max nice priority", ""),
    ("Max realtime priority", ""),
    ("Max realtime timeout", "us"),
];

/// `/proc/<pid>/limits`：与 Linux 的 `proc_pid_limits` 格式相同，没有单位的行末尾也留着
/// 硬限制后面的空格。
fn limits(rlimits: &[(u64, u64); crate::process::RLIM_NLIMITS]) -> String {
    let value = |v: u64| match v {
        u64::MAX => "unlimited".to_owned(),
        v => v.to_string(),
    };
    let mut s = format!(
        "{:<25} {:<20} {:<20} {:<10}\n",
        "Limit", "Soft Limit", "Hard Limit", "Units"
    );
    for ((name, unit), &(soft, hard)) in LIMIT_NAMES.iter().zip(rlimits) {
        write!(s, "{name:<25} {:<20} {:<20} ", value(soft), value(hard)).unwrap();
        if !unit.is_empty() {
            write!(s, "{unit:<10}").unwrap();
        }
        s.push('\n');
    }
    s
}

fn maps(task: &Task) -> String {
    let res = task.res.borrow();
    let mm = res.mm.borrow();
    mm.regions().iter().map(map_line).collect()
}

/// maps 的一行。与 Linux 一样，没有名字的行末尾也有一个空格，名字从第 74 列开始。
fn map_line(r: &Region) -> String {
    let perms = format!(
        "{}{}{}{}",
        if r.prot & 1 != 0 { 'r' } else { '-' },
        if r.prot & 2 != 0 { 'w' } else { '-' },
        if r.prot & 4 != 0 { 'x' } else { '-' },
        if r.shared { 's' } else { 'p' },
    );
    // 文件映射的设备号是根文件系统的 8:1（与 stat 的 st_dev 相同）
    let (offset, dev, ino) = match r.file {
        Some((offset, ino)) => (offset, "08:01", ino),
        None => (0, "00:00", 0),
    };
    let line = format!(
        "{:08x}-{:08x} {perms} {offset:08x} {dev} {ino} ",
        r.start, r.end
    );
    match &r.name {
        Some(name) => format!("{line:<73}{}\n", String::from_utf8_lossy(name)),
        None => format!("{line}\n"),
    }
}

/// smaps 的计数，单位 kB。宿主看不出哪些页被写过：匿名内存的驻留页都算脏页，文件映射的
/// 都算干净页。匿名内存里 fork 之后和别的地址空间共用的页（写时复制）算共享的，Pss 按
/// 共用的个数分摊（[`crate::mm::Mm::cow_shared`]），其余都算私有的。
#[derive(Default)]
struct Usage {
    rss: u64,
    anon: u64,
    /// `anon` 里和别的地址空间共用的。
    shared: u64,
    /// `shared` 分摊之后的字节数。
    shared_pss: u64,
    file: u64,
    shmem: u64,
    locked: u64,
}

impl Usage {
    fn add(&mut self, o: &Usage) {
        self.rss += o.rss;
        self.anon += o.anon;
        self.shared += o.shared;
        self.shared_pss += o.shared_pss;
        self.file += o.file;
        self.shmem += o.shmem;
        self.locked += o.locked;
    }

    /// Linux 的 `__show_smap`：标签占 16 列，数字占 8 列。Pss 与 Linux 一样先按字节累加，
    /// 最后才换成 kB。
    fn write(&self, s: &mut String, rollup: bool) {
        let private = self.anon - self.shared;
        let pss_anon = private * 1024 + self.shared_pss;
        let pss_shmem = self.shmem * 1024;
        let dirty = private + self.shmem;
        // Private_Hugetlb 的标签多一列，数字少一列，总宽度不变
        let mut put = |label: &str, kb: u64| {
            let w = 24 - label.len().max(16);
            writeln!(s, "{label:<16}{kb:>w$} kB").unwrap()
        };
        put("Rss:", self.rss);
        put("Pss:", (pss_anon + pss_shmem) / 1024 + self.file);
        put("Pss_Dirty:", (pss_anon + pss_shmem) / 1024);
        if rollup {
            put("Pss_Anon:", pss_anon / 1024);
            put("Pss_File:", self.file);
            put("Pss_Shmem:", self.shmem);
        }
        put("Shared_Clean:", 0);
        put("Shared_Dirty:", self.shared);
        put("Private_Clean:", self.file);
        put("Private_Dirty:", dirty);
        put("Referenced:", self.rss);
        put("Anonymous:", self.anon);
        for label in [
            "LazyFree:",
            "AnonHugePages:",
            "ShmemPmdMapped:",
            "FilePmdMapped:",
        ] {
            put(label, 0);
        }
        put("Shared_Hugetlb:", 0);
        put("Private_Hugetlb:", 0);
        put("Swap:", 0);
        put("SwapPss:", 0);
        put("Locked:", self.locked);
    }
}

/// `/proc/<pid>/smaps`，或者 `rollup` 时的 `smaps_rollup`（所有映射的合计）。驻留的页
/// 按宿主内存算（mincore）。
fn smaps(task: &Task, rollup: bool) -> String {
    let res = task.res.borrow();
    let mm = res.mm.borrow();
    let regions = mm.regions();
    let mut s = String::new();
    let mut total = Usage::default();
    for r in &regions {
        let resident = mm.residency(r.start, r.end - r.start).unwrap_or_default();
        let rss = resident.iter().filter(|&&p| p).count() as u64 * 4;
        let mut u = Usage {
            rss,
            locked: if r.locked { rss } else { 0 },
            ..Usage::default()
        };
        match (r.file, r.shared) {
            (Some(_), _) => u.file = rss,
            (None, true) => u.shmem = rss,
            (None, false) => {
                let (pages, pss) = mm.cow_shared(r.start, r.end);
                u.anon = rss;
                // 借出的页一般都驻留；换出到 zram 的不算驻留，也就不算共用的
                u.shared = (pages * 4).min(rss);
                u.shared_pss = pss.min(u.shared * 1024);
            }
        }
        total.add(&u);
        if rollup {
            continue;
        }
        s += &map_line(r);
        let kb = (r.end - r.start) / 1024;
        writeln!(s, "{:<16}{kb:>8} kB", "Size:").unwrap();
        writeln!(s, "{:<16}{:>8} kB", "KernelPageSize:", 4).unwrap();
        writeln!(s, "{:<16}{:>8} kB", "MMUPageSize:", 4).unwrap();
        u.write(&mut s, false);
        writeln!(s, "THPeligible:    0").unwrap();
        s += &vm_flags(r);
    }
    if rollup && let (Some(first), Some(last)) = (regions.first(), regions.last()) {
        let line = format!(
            "{:08x}-{:08x} ---p 00000000 00:00 0 ",
            first.start, last.end
        );
        writeln!(s, "{line:<73}[rollup]").unwrap();
        total.write(&mut s, true);
    }
    s
}

/// smaps 的 VmFlags 行，按 Linux 的标志位顺序。
fn vm_flags(r: &Region) -> String {
    let flags = [
        (r.prot & 1 != 0, "rd"),
        (r.prot & 2 != 0, "wr"),
        (r.prot & 4 != 0, "ex"),
        (r.shared, "sh"),
        (true, "mr"),
        (r.may_write, "mw"),
        (true, "me"),
        (r.shared, "ms"),
        (r.name.as_deref() == Some(b"[stack]"), "gd"),
        (r.locked, "lo"),
        // 计入提交量：可写的私有匿名映射
        (r.file.is_none() && !r.shared && r.prot & 2 != 0, "ac"),
        (r.dontdump, "dd"),
    ];
    let mut s = String::from("VmFlags: ");
    for (on, name) in flags {
        if on {
            s += name;
            s.push(' ');
        }
    }
    s.push('\n');
    s
}

/// 读 `/proc/<pid>/pagemap`：每页 8 字节，位置和长度都要是 8 的整数倍。
pub(crate) fn read_pagemap(k: &Kernel, pid: Tid, buf: &mut [u8], pos: u64) -> Result<usize, Errno> {
    if !pos.is_multiple_of(8) || !buf.len().is_multiple_of(8) {
        return Err(Errno::EINVAL);
    }
    let task = k
        .groups
        .get(&pid)
        .and_then(|g| g.borrow().threads.first().copied())
        .and_then(|t| k.tasks.get(&t).cloned())
        .ok_or(Errno::ESRCH)?;
    // 用户地址空间是 48 位
    let pages_total = 1u64 << (48 - 12);
    let first = pos / 8;
    let n = (buf.len() as u64 / 8).min(pages_total.saturating_sub(first)) as usize;
    let entries = task.res.borrow().mm.borrow().pagemap(first * 4096, n);
    for (i, e) in entries.iter().enumerate() {
        buf[i * 8..i * 8 + 8].copy_from_slice(&e.to_le_bytes());
    }
    Ok(n * 8)
}

/// 进程 `pid` 的地址空间。
pub(crate) fn process_mm(k: &Kernel, pid: Tid) -> Result<Rc<RefCell<crate::mm::Mm>>, Errno> {
    let task = k
        .groups
        .get(&pid)
        .and_then(|g| g.borrow().threads.first().copied())
        .and_then(|t| k.tasks.get(&t).cloned())
        .ok_or(Errno::ESRCH)?;
    let mm = task.res.borrow().mm.clone();
    Ok(mm)
}

/// 读 `/proc/<pid>/mem`（`mm` 是打开时的地址空间，已经释放时读到 EOF）：位置就是地址。
/// 按页读，碰到不能读的页停下；一字节也没读到时是 EIO（Linux 的 `mem_rw`）。
pub(crate) fn read_mem(
    mm: Option<Rc<RefCell<crate::mm::Mm>>>,
    buf: &mut [u8],
    pos: u64,
) -> Result<usize, Errno> {
    let Some(mm) = mm else {
        return Ok(0);
    };
    let mm = mm.borrow();
    let mut done = 0;
    while done < buf.len() {
        let Some(addr) = pos.checked_add(done as u64) else {
            break;
        };
        let n = ((PAGE_SIZE - addr % PAGE_SIZE) as usize).min(buf.len() - done);
        if mm
            .read(UserSlice::new(addr, n), &mut buf[done..done + n])
            .is_err()
        {
            break;
        }
        done += n;
    }
    if done == 0 && !buf.is_empty() {
        return Err(Errno::EIO);
    }
    Ok(done)
}

/// 写 `/proc/<pid>/mem`：与 PTRACE_POKETEXT 一样强制写入（[`Mm::write_force`]），按页
/// 写，碰到不能写的页停下。`current` 是写入的线程。
///
/// [`Mm::write_force`]: crate::mm::Mm::write_force
pub(crate) fn write_mem(
    mm: Option<Rc<RefCell<crate::mm::Mm>>>,
    data: &[u8],
    pos: u64,
    current: &Arc<crate::task::TaskSync>,
) -> Result<usize, Errno> {
    let Some(mm) = mm else {
        return Ok(0);
    };
    let mut mm = mm.borrow_mut();
    let mut done = 0;
    while done < data.len() {
        let Some(addr) = pos.checked_add(done as u64) else {
            break;
        };
        let n = ((PAGE_SIZE - addr % PAGE_SIZE) as usize).min(data.len() - done);
        if mm
            .write_force(addr, &data[done..done + n], current)
            .is_err()
        {
            break;
        }
        done += n;
    }
    if done == 0 && !data.is_empty() {
        return Err(Errno::EIO);
    }
    Ok(done)
}

const MOUNTS: &str = "appine / ext4 rw,relatime 0 0\n\
    proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0\n\
    devpts /dev/pts devpts rw,nosuid,noexec,relatime,gid=5,mode=620,ptmxmode=000 0 0\n\
    sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0\n\
    cgroup /sys/fs/cgroup cgroup2 ro,nosuid,nodev,noexec,relatime,nsdelegate,memory_recursiveprot 0 0\n";

/// `/proc/<pid>/mountinfo`：挂载点的编号与 fdinfo 的 mnt_id 一致，设备号与 stat 的
/// st_dev 一致。
const MOUNTINFO: &str = "1 1 8:1 / / rw,relatime - ext4 appine rw\n\
    2 1 0:4 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw\n\
    3 1 0:24 / /dev/pts rw,nosuid,noexec,relatime - devpts devpts \
    rw,gid=5,mode=620,ptmxmode=000\n\
    4 1 0:22 / /sys rw,nosuid,nodev,noexec,relatime - sysfs sysfs rw\n\
    6 4 0:26 / /sys/fs/cgroup ro,nosuid,nodev,noexec,relatime - cgroup2 cgroup \
    rw,nsdelegate,memory_recursiveprot\n";

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

/// `/proc/meminfo`：照搬宿主的（与 docker 的容器一样），guest 能用的上限另见 cgroup 的
/// `memory.max`。原来按 sysinfo 算，`MemAvailable` 就是 `MemFree`，页缓存都算作已用。
fn meminfo() -> String {
    let m = appine_hal::host_meminfo();
    let mut s = String::new();
    for (name, bytes) in [
        ("MemTotal", m.total),
        ("MemFree", m.free),
        ("MemAvailable", m.available),
        ("Buffers", m.buffers),
        ("Cached", m.cached),
        ("SwapCached", m.swap_cached),
        ("Active", m.active),
        ("Inactive", m.inactive),
        ("SwapTotal", m.swap_total),
        ("SwapFree", m.swap_free),
        ("Dirty", m.dirty),
        ("AnonPages", m.anon_pages),
        ("Mapped", m.mapped),
        ("Shmem", m.shmem),
        ("Slab", m.slab),
        ("SReclaimable", m.sreclaimable),
    ] {
        writeln!(s, "{:<16}{:>8} kB", format!("{name}:"), bytes / 1024).unwrap();
    }
    s
}

fn global_stat(ps: &View<'_>) -> String {
    let n = cpus() as u64;
    let up = ticks(uptime_ns());
    let cpu = ps.k.tasks.values().fold(ps.k.cpu, |sum, t| sum + t.cpu());
    let (user, system) = (ticks(cpu.user_ns()), ticks(cpu.sys_ns));
    let idle = (up * n).saturating_sub(user + system);
    let running =
        ps.k.tasks
            .values()
            .filter(|t| t.sync.in_guest.load(std::sync::atomic::Ordering::Relaxed))
            .count();
    let (now, _) = appine_hal::clock_gettime(0).unwrap_or((0, 0));
    let btime = now as u64 - uptime_ns() / 1_000_000_000;
    let mut s = String::new();
    writeln!(s, "cpu  {user} 0 {system} {idle} 0 0 0 0 0 0").unwrap();
    for i in 0..n {
        writeln!(
            s,
            "cpu{i} {} 0 {} {} 0 0 0 0 0 0",
            user / n,
            system / n,
            idle / n
        )
        .unwrap();
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

/// 所有打开着的 socket（同一个只算一次）和打开它的进程的 fsuid，按 inode 排列。
/// 只在 SCM_RIGHTS 消息里、或者等着 accept 的 socket 不算。
fn sockets(k: &Kernel) -> Vec<(SocketRef, u32)> {
    let mut tables = HashSet::new();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for task in k.tasks.values() {
        let fds = task.res.borrow().fds.clone();
        if !tables.insert(Rc::as_ptr(&fds)) {
            continue;
        }
        let uid = k
            .groups
            .get(&task.tgid)
            .map_or(0, |g| g.borrow().cred.fsuid);
        let fds = fds.borrow();
        for file in fds.open_fds().into_iter().filter_map(|fd| fds.get(fd).ok()) {
            if let FileKind::Socket(s) = &file.borrow().kind
                && seen.insert(Rc::as_ptr(s))
            {
                out.push((s.clone(), uid));
            }
        }
    }
    // 只在 SCM_RIGHTS 消息里的
    for s in crate::unix::inflight_sockets(k) {
        if seen.insert(Rc::as_ptr(&s)) {
            out.push((s, 0));
        }
    }
    out.sort_by_key(|(s, _)| socket_ino(s));
    out
}

/// socket 的 inode 号（fstat、`/proc/<pid>/fd` 的 `socket:[n]`）。
pub(crate) fn socket_ino(s: &SocketRef) -> u64 {
    Rc::as_ptr(s) as u64 >> 4
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

/// `/proc/net/if_inet6`：宿主接口上的 IPv6 地址，格式与 Linux 的 `if6_seq_show` 相同。
/// OpenJDK 靠它判断有没有 IPv6（`IPv6_supported`），没有这个文件或者是空的就整个关掉
/// IPv6。scope 按 Linux 的 `ipv6_addr_scope` 从地址看；标志与 netlink 的回答一样都是
/// `IFA_F_PERMANENT`（getifaddrs 分不出临时地址）。
fn net_if_inet6() -> String {
    let mut s = String::new();
    for i in appine_hal::net::interfaces() {
        for (_, addr, prefix) in i.addrs.iter().filter(|(f, ..)| *f == AF_INET6) {
            let scope = match addr[..] {
                [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1] => 0x10,
                [0xfe, b, ..] if b & 0xc0 == 0x80 => 0x20,
                [0xfe, b, ..] if b & 0xc0 == 0xc0 => 0x40,
                _ => 0,
            };
            let hex: String = addr.iter().map(|b| format!("{b:02x}")).collect();
            writeln!(
                s,
                "{hex} {:02x} {prefix:02x} {scope:02x} 80 {:>8}",
                i.index, i.name
            )
            .unwrap();
        }
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
            self::Global::Loadavg => crate::loadavg::proc_loadavg(ps.k),
            self::Global::Version => {
                "Linux version 6.1.0-appine (appine@localhost) #1 SMP PREEMPT appine\n".to_owned()
            }
            self::Global::Filesystems => {
                "nodev\tsysfs\n\text4\nnodev\tproc\nnodev\tdevpts\nnodev\tcgroup2\n".to_owned()
            }
            self::Global::Cmdline => "appine\n".to_owned(),
            self::Global::Cgroups => crate::cgroup::proc_cgroups(),
            self::Global::Mounts => return Err(Errno::EINVAL),
        },
        Static(path) => match &path[..] {
            b"net/dev" => net_dev(),
            b"net/if_inet6" => net_if_inet6(),
            b"net/tcp" => crate::net::proc_listing(&sockets(ps.k), AF_INET, Table::Tcp),
            b"net/tcp6" => crate::net::proc_listing(&sockets(ps.k), AF_INET6, Table::Tcp),
            b"net/udp" => crate::net::proc_listing(&sockets(ps.k), AF_INET, Table::Udp),
            b"net/udp6" => crate::net::proc_listing(&sockets(ps.k), AF_INET6, Table::Udp),
            b"net/icmp" => crate::net::proc_listing(&sockets(ps.k), AF_INET, Table::Icmp),
            b"net/icmp6" => crate::net::proc_listing(&sockets(ps.k), AF_INET6, Table::Icmp),
            b"net/raw" => crate::net::proc_listing(&sockets(ps.k), AF_INET, Table::Raw),
            b"net/raw6" => crate::net::proc_listing(&sockets(ps.k), AF_INET6, Table::Raw),
            b"net/unix" => crate::unix::proc_listing(&sockets(ps.k)),
            b"sys/fs/file-max" => "9223372036854775807\n".to_owned(),
            b"sys/fs/lease-break-time" => format!("{}\n", ps.k.lease_break_time),
            b"sys/fs/aio-max-nr" => format!("{}\n", ps.k.aio_max_nr),
            b"sys/fs/aio-nr" => format!("{}\n", ps.k.aio_nr),
            p if p.starts_with(b"sys/fs/mqueue/") => {
                format!("{}\n", ps.k.mq_limits.get(&p[14..]).unwrap_or(0))
            }
            b"sys/fs/inotify/max_queued_events" => {
                format!("{}\n", ps.k.inotify.borrow().max_queued)
            }
            b"sys/fs/inotify/max_user_instances" => {
                format!("{}\n", ps.k.inotify.borrow().max_instances)
            }
            b"sys/fs/inotify/max_user_watches" => {
                format!("{}\n", ps.k.inotify.borrow().max_watches)
            }
            b"sys/fs/nr_open" => format!("{}\n", ps.k.nr_open),
            b"sys/fs/pipe-max-size" => format!("{}\n", ps.k.pipe_max_size),
            b"sys/fs/pipe-user-pages-hard" => "0\n".to_owned(),
            b"sys/fs/pipe-user-pages-soft" => "16384\n".to_owned(),
            b"sys/kernel/cap_last_cap" => "40\n".to_owned(),
            b"sys/kernel/core_pattern" => {
                format!("{}\n", String::from_utf8_lossy(&ps.k.core_pattern))
            }
            b"sys/kernel/core_uses_pid" => format!("{}\n", ps.k.core_uses_pid as u8),
            b"sys/kernel/domainname" => {
                format!("{}\n", String::from_utf8_lossy(&ps.k.domainname))
            }
            b"sys/kernel/hostname" => format!("{}\n", String::from_utf8_lossy(&ps.k.hostname)),
            b"sys/kernel/sched_rr_timeslice_ms" => "100\n".to_owned(),
            b"sys/kernel/msgmax" | b"sys/kernel/msgmnb" | b"sys/kernel/msgmni"
            | b"sys/kernel/sem" | b"sys/kernel/shmall" | b"sys/kernel/shmmax"
            | b"sys/kernel/shmmni" => ps.k.ipc.tunables.read(&path[11..]).unwrap(),
            b"sysvipc/msg" => ipc::proc_listing(ps.k, b"msg"),
            b"sysvipc/sem" => ipc::proc_listing(ps.k, b"sem"),
            b"sysvipc/shm" => ipc::proc_listing(ps.k, b"shm"),
            b"sys/kernel/tainted" => "0\n".to_owned(),
            b"sys/kernel/threads-max" => format!("{}\n", ps.k.threads_max),
            b"sys/vm/max_map_count" => {
                format!(
                    "{}\n",
                    crate::mm::MAX_MAP_COUNT.load(std::sync::atomic::Ordering::Relaxed)
                )
            }
            b"sys/vm/overcommit_ratio" => "50\n".to_owned(),
            b"sys/kernel/ngroups_max" => "65536\n".to_owned(),
            b"sys/kernel/ns_last_pid" => format!("{}\n", ps.k.last_pid()),
            b"sys/kernel/seccomp/actions_avail" => crate::seccomp::actions_avail(),
            b"sys/kernel/seccomp/actions_logged" => crate::seccomp::actions_logged(ps.k),
            b"sys/kernel/osrelease" => "6.1.0-appine\n".to_owned(),
            b"sys/kernel/ostype" => "Linux\n".to_owned(),
            b"sys/kernel/pid_max" => "4194304\n".to_owned(),
            b"sys/kernel/random/boot_id" => format!("{}\n", format_uuid(&ps.k.boot_id)),
            b"sys/kernel/random/entropy_avail" | b"sys/kernel/random/poolsize" => {
                format!("{}\n", crate::syscall::ENTROPY)
            }
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
                self::Entry::Cgroup => crate::cgroup::PROC_CGROUP.to_owned(),
                self::Entry::Cmdline => {
                    return Ok(task.map_or(Vec::new(), |t| guest_bytes(&t, |m| m.args)));
                }
                self::Entry::Environ => {
                    return Ok(task.map_or(Vec::new(), |t| guest_bytes(&t, |m| m.env)));
                }
                self::Entry::Limits => {
                    let g = ps.group(*pid).ok_or(Errno::ESRCH)?;
                    limits(&g.borrow().rlimits)
                }
                self::Entry::Maps => task.map_or(String::new(), |t| maps(&t)),
                self::Entry::Smaps => task.map_or(String::new(), |t| smaps(&t, false)),
                self::Entry::SmapsRollup => task.map_or(String::new(), |t| smaps(&t, true)),
                // 二进制，按读写的位置现算（`read_pagemap`、`read_mem`、`write_mem`）
                self::Entry::Pagemap | self::Entry::Mem => String::new(),
                self::Entry::Auxv => {
                    let auxv =
                        task.map_or_else(Vec::new, |t| t.res.borrow().mm.borrow().auxv.to_vec());
                    return Ok(auxv.iter().flat_map(|w| w.to_le_bytes()).collect());
                }
                self::Entry::Mounts => MOUNTS.to_owned(),
                self::Entry::MountInfo => MOUNTINFO.to_owned(),
                self::Entry::OomScoreAdj => {
                    let g = ps.group(*pid).ok_or(Errno::ESRCH)?;
                    format!("{}\n", g.borrow().oom_score_adj)
                }
                self::Entry::CoredumpFilter => {
                    let g = ps.group(*pid).ok_or(Errno::ESRCH)?;
                    format!("{:08x}\n", g.borrow().coredump_filter)
                }
                _ => return Err(Errno::EINVAL),
            }
        }
        FdInfo(pid, fd) => fdinfo(ps, *pid, *fd)?,
        Sys(path) => match &path[..] {
            b"devices/system/cpu/online"
            | b"devices/system/cpu/possible"
            | b"devices/system/cpu/present" => cpu_list(0..cpus()),
            b"devices/system/cpu/offline" => "\n".to_owned(),
            _ => return Err(Errno::EISDIR),
        },
        SysCpu(n, path) => cpu_file(*n as usize, path)?,
        Cgroup(name) => crate::cgroup::file(ps.k, name).ok_or(Errno::EISDIR)?,
        _ => return Err(Errno::EINVAL),
    };
    Ok(s.into_bytes())
}

/// `/sys/devices/system/cpu/cpu<n>` 下的文件。CPU 都一样（`/proc/cpuinfo` 也是），各自是
/// 一个核，没有超线程，都在一个封装里。
fn cpu_file(n: usize, path: &[u8]) -> Result<String, Errno> {
    let all = 0..cpus();
    let me = n..n + 1;
    Ok(match path {
        b"online" => "1\n".to_owned(),
        b"regs/identification/midr_el1" => format!("0x{:016x}\n", appine_engine::guest_midr()),
        b"regs/identification/revidr_el1" => format!("0x{:016x}\n", 0),
        b"topology/cluster_id" => "-1\n".to_owned(),
        b"topology/core_id" => format!("{n}\n"),
        b"topology/physical_package_id" => "0\n".to_owned(),
        b"topology/cluster_cpus" | b"topology/core_cpus" | b"topology/thread_siblings" => {
            cpu_mask(me)
        }
        b"topology/cluster_cpus_list"
        | b"topology/core_cpus_list"
        | b"topology/thread_siblings_list" => cpu_list(me),
        b"topology/core_siblings" | b"topology/package_cpus" => cpu_mask(all),
        b"topology/core_siblings_list" | b"topology/package_cpus_list" => cpu_list(all),
        _ if HOST_CPU_FILES.contains(&path) => return host_cpu_file(n, path),
        _ => return Err(Errno::EISDIR),
    })
}

/// 宿主上对应 guest CPU `n` 的那个 CPU。
fn host_cpu(n: u32) -> Option<usize> {
    appine_hal::host_cpus().get(n as usize).copied()
}

/// [`HOST_CPU_FILES`] 里的 `path`（相对 `cpu<n>`，目录以 `/` 结尾）宿主对应的 CPU 有没有。
fn host_cpu_has(n: u32, path: &[u8]) -> bool {
    let rel = String::from_utf8_lossy(path.strip_suffix(b"/").unwrap_or(path)).into_owned();
    host_cpu(n).is_some_and(|h| appine_hal::cpu_sysfs(h, &rel).is_some())
}

/// 照搬宿主的缓存、频率文件：CPU 列表和掩码里宿主的 CPU 编号换成 guest 的（guest 用不了的
/// CPU 去掉）。
fn host_cpu_file(n: usize, path: &[u8]) -> Result<String, Errno> {
    let rel = String::from_utf8_lossy(path).into_owned();
    let raw = host_cpu(n as u32)
        .and_then(|h| appine_hal::cpu_sysfs(h, &rel))
        .ok_or(Errno::ENOENT)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let guest = |host: Vec<usize>| -> Vec<usize> {
        let cpus = appine_hal::host_cpus();
        (0..cpus.len())
            .filter(|&i| host.contains(&cpus[i]))
            .collect()
    };
    Ok(match tree_name(path) {
        b"shared_cpu_list" => cpu_list_of(&guest(parse_cpu_list(text.trim()))),
        b"shared_cpu_map" => cpu_mask_of(&guest(parse_cpu_mask(text.trim()))),
        b"affected_cpus" | b"related_cpus" => {
            let host = text
                .split_whitespace()
                .filter_map(|w| w.parse().ok())
                .collect();
            let cpus: Vec<String> = guest(host).iter().map(usize::to_string).collect();
            format!("{}\n", cpus.join(" "))
        }
        _ => text,
    })
}

/// CPU 列表（`0-3,5`）里的 CPU。
fn parse_cpu_list(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in s.split(',').filter(|p| !p.is_empty()) {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
            out.extend(a..=b.min(a + 4096));
        }
    }
    out
}

/// CPU 掩码（十六进制，每 32 位一组用逗号隔开，高位在前）里的 CPU。
fn parse_cpu_mask(s: &str) -> Vec<usize> {
    let words: Vec<u32> = s
        .split(',')
        .filter_map(|w| u32::from_str_radix(w, 16).ok())
        .collect();
    let mut out = Vec::new();
    for (i, w) in words.iter().rev().enumerate() {
        out.extend((0..32).filter(|b| w >> b & 1 != 0).map(|b| i * 32 + b));
    }
    out
}

/// 按 CPU 列表的写法（[`cpu_list`]，`0-1,3`）写出一组 CPU（从小到大）。
fn cpu_list_of(cpus: &[usize]) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < cpus.len() {
        let mut j = i;
        while j + 1 < cpus.len() && cpus[j + 1] == cpus[j] + 1 {
            j += 1;
        }
        parts.push(if i == j {
            cpus[i].to_string()
        } else {
            format!("{}-{}", cpus[i], cpus[j])
        });
        i = j + 1;
    }
    format!("{}\n", parts.join(","))
}

/// 按 CPU 掩码的写法（[`cpu_mask`]）写出一组 CPU。
fn cpu_mask_of(cpus: &[usize]) -> String {
    let bits = self::cpus();
    let groups = bits.div_ceil(32);
    let mut s = String::new();
    for g in (0..groups).rev() {
        let width = if g == groups - 1 {
            (bits - g * 32).div_ceil(4)
        } else {
            8
        };
        let word = cpus
            .iter()
            .filter(|&&c| c / 32 == g)
            .fold(0u32, |w, &c| w | 1 << (c % 32));
        if g != groups - 1 {
            s.push(',');
        }
        write!(s, "{word:0width$x}").unwrap();
    }
    s.push('\n');
    s
}

/// CPU 列表的写法（`%*pbl`）：`0-3`，只有一个时是 `2`。
fn cpu_list(set: Range<usize>) -> String {
    cpu_list_of(&set.collect::<Vec<_>>())
}

/// CPU 掩码的写法（`%*pb`）：十六进制，每 32 位一组，用逗号隔开，最高的一组补零到写得下
/// CPU 总数的位数（4 个 CPU 时是 `f`，8 个时 cpu0 是 `01`）。
fn cpu_mask(set: Range<usize>) -> String {
    cpu_mask_of(&set.collect::<Vec<_>>())
}

/// `/proc/<pid>/fdinfo/<n>`：位置、标志（加上 close-on-exec 和 64 位上总有的
/// `O_LARGEFILE`）、挂载编号、inode，以及 eventfd、inotify 的附加行。
fn fdinfo(ps: &View<'_>, pid: Tid, fd: i32) -> Result<String, Errno> {
    const O_LARGEFILE: u32 = 0o400000;
    let task = ps.leader(pid).ok_or(Errno::ENOENT)?;
    let res = task.res.borrow();
    let fds = res.fds.borrow();
    let file = fds.get(fd).map_err(|_| Errno::ENOENT)?;
    let cloexec = if fds.cloexec(fd)? {
        oflags::O_CLOEXEC
    } else {
        0
    };
    let f = file.borrow();
    let mnt_id = match f.node() {
        Some(Node::Store(_)) => 1,
        Some(Node::Pseudo(p)) => match mount_of(&p) {
            Mount::Proc => 2,
            Mount::Pts => 3,
            Mount::Sys => 4,
            Mount::Cgroup => 6,
        },
        None => 5,
    };
    let mut s = format!(
        "pos:\t{}\nflags:\t0{:o}\nmnt_id:\t{mnt_id}\nino:\t{}\n",
        f.pos,
        f.flags | O_LARGEFILE | cloexec,
        f.ino().unwrap_or(0)
    );
    match &f.kind {
        FileKind::EventFd(e) => s += &format!("eventfd-count: {:16x}\n", e.borrow().count),
        FileKind::Inotify(i) => s += &i.borrow().fdinfo(),
        FileKind::Pidfd(p) => {
            let pid = if p.exited() { -1 } else { p.tgid };
            s += &format!("Pid:\t{pid}\nNSpid:\t{pid}\n");
        }
        FileKind::Epoll(e) => s += &e.borrow().fdinfo(),
        FileKind::TimerFd(t) => s += &t.borrow_mut().fdinfo(),
        FileKind::SignalFd(sfd) => s += &format!("sigmask:\t{:016x}\n", sfd.borrow().mask),
        _ => {}
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_lists_and_masks_round_trip() {
        assert_eq!(parse_cpu_list("0-2,5"), vec![0, 1, 2, 5]);
        assert_eq!(parse_cpu_list("7"), vec![7]);
        assert_eq!(parse_cpu_mask("00000001,0000000f"), vec![0, 1, 2, 3, 32]);
        assert_eq!(cpu_list_of(&[0, 1, 2, 5, 7, 8]), "0-2,5,7-8\n");
        assert_eq!(cpu_list_of(&[3]), "3\n");
        assert_eq!(cpu_list_of(&[]), "\n");
    }
}
