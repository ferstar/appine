//! 合成文件系统（设计文档 §8.2）：procfs、devpts、sysfs（CPU 的拓扑、网络接口）和只读的
//! cgroup2，开机时挂在 `/proc`、`/dev/pts`、`/sys`、`/sys/fs/cgroup` 上。
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
use crate::pidns::PidNs;
use crate::process::{Signal, ThreadGroup};
use crate::signal::SigAction;
use crate::syscall::SocketRef;
use crate::task::Task;
use crate::tty::{PTS_MAJOR, Tty};
use crate::uaccess::UserSlice;
use crate::vfs::dev::rdev;
use crate::vfs::store::{DirEntry, Ino, Stat, Store, mode};
use crate::vfs::{Fs, MountId, Node, Pseudo, Vfs};

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
    /// 指向 `self/net` 的符号链接。
    Net,
    /// cgroup 的控制器（[`crate::cgroup`]）。
    Cgroups,
    /// syslog 的读（[`crate::klog`]）：内容不在打开时生成，读的时候取。
    Kmsg,
}

const GLOBALS: [(&[u8], Global); 12] = [
    (b"cpuinfo", Global::Cpuinfo),
    (b"meminfo", Global::Meminfo),
    (b"stat", Global::Stat),
    (b"uptime", Global::Uptime),
    (b"loadavg", Global::Loadavg),
    (b"version", Global::Version),
    (b"filesystems", Global::Filesystems),
    (b"cmdline", Global::Cmdline),
    (b"mounts", Global::Mounts),
    (b"net", Global::Net),
    (b"cgroups", Global::Cgroups),
    (b"kmsg", Global::Kmsg),
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
    /// 线程建的子进程（只在 `task/<tid>/` 下，Linux 的 `CONFIG_PROC_CHILDREN`）。
    Children,
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
    /// 网络命名空间的文件（[`PROC_NET`]），所有进程都在同一个命名空间里。
    Net,
    OomScoreAdj,
    CoredumpFilter,
    /// 所在的 cgroup（[`crate::cgroup`]）。
    Cgroup,
    /// 命名空间的目录（[`NsKind`]）。
    Ns,
    /// 用户命名空间的 ID 映射（user_namespaces(7)）：`uid_map`、`gid_map`、`projid_map`，
    /// 以及 `setgroups`。
    UidMap,
    GidMap,
    ProjidMap,
    Setgroups,
    /// 给子进程用的 time 命名空间的偏移（[`crate::nsproxy::TimeNs`]），只在 `/proc/<pid>` 下。
    TimensOffsets,
}

/// 命名空间的种类（`/proc/<pid>/ns` 里的项）。只有挂载命名空间可以新建，别的都是开机时的
/// 那个。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum NsKind {
    Cgroup,
    Ipc,
    Mnt,
    Net,
    Pid,
    PidForChildren,
    Time,
    TimeForChildren,
    User,
    Uts,
}

impl NsKind {
    /// 按名字排好的（`/proc/<pid>/ns` 的列表）。
    pub(crate) const ALL: [NsKind; 10] = [
        NsKind::Cgroup,
        NsKind::Ipc,
        NsKind::Mnt,
        NsKind::Net,
        NsKind::Pid,
        NsKind::PidForChildren,
        NsKind::Time,
        NsKind::TimeForChildren,
        NsKind::User,
        NsKind::Uts,
    ];

    /// `/proc/<pid>/ns` 里的名字。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            NsKind::Cgroup => "cgroup",
            NsKind::Ipc => "ipc",
            NsKind::Mnt => "mnt",
            NsKind::Net => "net",
            NsKind::Pid => "pid",
            NsKind::PidForChildren => "pid_for_children",
            NsKind::Time => "time",
            NsKind::TimeForChildren => "time_for_children",
            NsKind::User => "user",
            NsKind::Uts => "uts",
        }
    }

    /// 链接内容里的类型（`*_for_children` 与对应的相同）。
    pub(crate) const fn type_name(self) -> &'static str {
        match self {
            NsKind::PidForChildren => "pid",
            NsKind::TimeForChildren => "time",
            k => k.name(),
        }
    }

    /// setns 的 `nstype`（`CLONE_NEW*`）。
    pub(crate) const fn clone_flag(self) -> u64 {
        match self {
            NsKind::Cgroup => 0x0200_0000,
            NsKind::Ipc => 0x0800_0000,
            NsKind::Mnt => 0x0002_0000,
            NsKind::Net => 0x4000_0000,
            NsKind::Pid | NsKind::PidForChildren => 0x2000_0000,
            NsKind::Time | NsKind::TimeForChildren => 0x80,
            NsKind::User => 0x1000_0000,
            NsKind::Uts => 0x0400_0000,
        }
    }

    /// 线程 `task` 的这种命名空间的编号（inode 号）：挂载命名空间的按它的 `FsContext`，
    /// 用户命名空间的按它的凭据，pid 命名空间的按它的进程和线程，UTS、IPC 的按它的
    /// [`crate::nsproxy::NsProxy`]，别的是开机时的那个（与 Linux 常见的编号相同）。没有时
    /// （ENOENT）是 None。
    pub(crate) fn id_of(self, k: &Kernel, task: &Task) -> Option<u64> {
        let group = k.groups.get(&task.tgid);
        Some(match self {
            NsKind::Mnt => task.res.borrow().fs.borrow().mnt_ns.id,
            NsKind::User => group.map_or(crate::cred::INIT_USER_NS, |g| g.borrow().cred.user_ns.id),
            NsKind::Cgroup => task.ns.borrow().cgroup.id,
            NsKind::Ipc => task.ns.borrow().ipc.id,
            NsKind::Net => task.ns.borrow().net.id,
            NsKind::Pid => k.pid_ns_of(task.tgid).id,
            // unshare 了、还没有建子进程时没有（Linux 的 pidns_for_children_get 看
            // child_reaper）
            NsKind::PidForChildren => {
                let ns = task.pid_ns_children.borrow().clone();
                let ns = ns.unwrap_or_else(|| k.pid_ns_of(task.tgid));
                if ns.level > 0 && ns.reaper.get().is_none() {
                    return None;
                }
                ns.id
            }
            NsKind::Time => task.ns.borrow().time.id,
            NsKind::TimeForChildren => task.ns.borrow().time_for_children.id,
            NsKind::Uts => task.ns.borrow().uts.id,
        })
    }
}

const ENTRIES: [(&[u8], Entry); 32] = [
    (b"auxv", Entry::Auxv),
    (b"stat", Entry::Stat),
    (b"status", Entry::Status),
    (b"statm", Entry::Statm),
    (b"cmdline", Entry::Cmdline),
    (b"comm", Entry::Comm),
    (b"environ", Entry::Environ),
    (b"limits", Entry::Limits),
    (b"children", Entry::Children),
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
    (b"net", Entry::Net),
    (b"oom_score_adj", Entry::OomScoreAdj),
    (b"coredump_filter", Entry::CoredumpFilter),
    (b"cgroup", Entry::Cgroup),
    (b"ns", Entry::Ns),
    (b"uid_map", Entry::UidMap),
    (b"gid_map", Entry::GidMap),
    (b"projid_map", Entry::ProjidMap),
    (b"setgroups", Entry::Setgroups),
    (b"timens_offsets", Entry::TimensOffsets),
];

/// `/proc/<pid>/net` 下的文件。与 Linux 一样，`/proc/net` 是指向 `self/net` 的链接。
const PROC_NET: [&[u8]; 18] = [
    b"arp",
    b"dev",
    b"icmp",
    b"icmp6",
    b"if_inet6",
    b"ipv6_route",
    b"netstat",
    b"raw",
    b"raw6",
    b"route",
    b"snmp",
    b"sockstat",
    b"sockstat6",
    b"tcp",
    b"tcp6",
    b"udp",
    b"udp6",
    b"unix",
];

/// `/proc/sys`、`/proc/sysvipc` 下固定的目录和文件（相对 `/proc` 的路径，目录以 `/` 结尾）。
/// `sys/net` 下的按读者所在的网络命名空间（与 Linux 一样），`ipv4/conf`、`ipv6/conf` 下的
/// 目录是 [`PseudoNode::NetConf`]。
const STATIC: [&[u8]; 86] = [
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
    b"sys/fs/overflowgid",
    b"sys/fs/overflowuid",
    b"sys/fs/pipe-max-size",
    b"sys/fs/pipe-user-pages-hard",
    b"sys/fs/pipe-user-pages-soft",
    b"sys/kernel/",
    b"sys/kernel/cap_last_cap",
    b"sys/kernel/core_pattern",
    b"sys/kernel/core_uses_pid",
    b"sys/kernel/dmesg_restrict",
    b"sys/kernel/domainname",
    b"sys/kernel/hostname",
    b"sys/kernel/msgmax",
    b"sys/kernel/msgmnb",
    b"sys/kernel/msgmni",
    b"sys/kernel/ngroups_max",
    b"sys/kernel/ns_last_pid",
    b"sys/kernel/osrelease",
    b"sys/kernel/ostype",
    b"sys/kernel/overflowgid",
    b"sys/kernel/overflowuid",
    b"sys/kernel/pid_max",
    b"sys/kernel/printk",
    b"sys/kernel/printk_devkmsg",
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
    b"sys/net/",
    b"sys/net/core/",
    b"sys/net/core/somaxconn",
    b"sys/net/ipv4/",
    b"sys/net/ipv4/conf/",
    b"sys/net/ipv4/ip_forward",
    b"sys/net/ipv4/ip_local_port_range",
    b"sys/net/ipv4/ip_unprivileged_port_start",
    b"sys/net/ipv4/ping_group_range",
    b"sys/net/ipv6/",
    b"sys/net/ipv6/conf/",
    b"sys/net/unix/",
    b"sys/user/",
    b"sys/user/max_cgroup_namespaces",
    b"sys/user/max_ipc_namespaces",
    b"sys/user/max_mnt_namespaces",
    b"sys/user/max_net_namespaces",
    b"sys/user/max_pid_namespaces",
    b"sys/user/max_time_namespaces",
    b"sys/user/max_user_namespaces",
    b"sys/user/max_uts_namespaces",
    b"sys/vm/",
    b"sys/vm/max_map_count",
    b"sys/vm/overcommit_memory",
    b"sys/vm/overcommit_ratio",
    b"sysvipc/",
    b"sysvipc/msg",
    b"sysvipc/sem",
    b"sysvipc/shm",
];

/// `/sys` 下固定的目录和文件（相对 `/sys` 的路径，目录以 `/` 结尾）。CPU 的拓扑：busybox
/// 的 `nproc --all`、lscpu、glibc 的 get_nprocs 读它们，各个 `cpu<n>` 是
/// [`PseudoNode::SysCpu`]；网络接口（btop 读流量计数），各个接口是 [`PseudoNode::SysNet`]。
const SYS_STATIC: [&[u8]; 11] = [
    b"class/",
    b"class/net/",
    b"devices/",
    b"devices/system/",
    b"devices/system/cpu/",
    b"devices/system/cpu/offline",
    b"devices/system/cpu/online",
    b"devices/system/cpu/possible",
    b"devices/system/cpu/present",
    b"fs/",
    b"fs/cgroup/",
];

/// `/sys/class/net`：各个接口在它下面。
const SYS_NET_DIR: &[u8] = b"class/net/";

/// `/sys/class/net/<接口>` 下的目录和文件（相对接口目录的路径），取值见 [`net_file`]。
/// Linux 上的接口目录是指向 `/sys/devices` 下面的符号链接，这里是目录：宿主上的设备路径
/// 拿不到。
const NET_FILES: [&[u8]; 36] = [
    b"addr_len",
    b"address",
    b"broadcast",
    b"carrier",
    b"flags",
    b"ifindex",
    b"iflink",
    b"mtu",
    b"operstate",
    b"tx_queue_len",
    b"type",
    b"statistics/",
    b"statistics/collisions",
    b"statistics/multicast",
    b"statistics/rx_bytes",
    b"statistics/rx_compressed",
    b"statistics/rx_crc_errors",
    b"statistics/rx_dropped",
    b"statistics/rx_errors",
    b"statistics/rx_fifo_errors",
    b"statistics/rx_frame_errors",
    b"statistics/rx_length_errors",
    b"statistics/rx_missed_errors",
    b"statistics/rx_nohandler",
    b"statistics/rx_over_errors",
    b"statistics/rx_packets",
    b"statistics/tx_aborted_errors",
    b"statistics/tx_bytes",
    b"statistics/tx_carrier_errors",
    b"statistics/tx_compressed",
    b"statistics/tx_dropped",
    b"statistics/tx_errors",
    b"statistics/tx_fifo_errors",
    b"statistics/tx_heartbeat_errors",
    b"statistics/tx_packets",
    b"statistics/tx_window_errors",
];

/// bridge 的 `/sys/class/net/<接口>` 下另有的（`brif/<端口>` 是 [`PseudoNode::SysBrif`]）。
/// dockerd 读 `bridge/stp_state` 和端口的 `state`，等端口进入转发状态。
const BRIDGE_FILES: [&[u8]; 10] = [
    b"bridge/",
    b"bridge/ageing_time",
    b"bridge/bridge_id",
    b"bridge/forward_delay",
    b"bridge/hello_time",
    b"bridge/max_age",
    b"bridge/priority",
    b"bridge/root_id",
    b"bridge/stp_state",
    b"brif/",
];

/// bridge 的端口的 `/sys/class/net/<接口>` 下另有的。
const PORT_FILES: [&[u8]; 4] = [
    b"brport/",
    b"brport/path_cost",
    b"brport/priority",
    b"brport/state",
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
    /// devpts 实例（它的设备号，[`crate::tty::PtsOptions`]）的根目录，`/dev/pts`。
    PtsRoot(u64),
    /// 实例里的终端，`/dev/pts/<n>`。
    Pts(u64, u32),
    /// 实例里的 ptmx，`/dev/pts/ptmx`。
    PtsPtmx(u64),
    /// `/proc`。
    ProcRoot,
    /// `/proc` 下的全局文件。
    Global(Global),
    /// `/proc/self`。
    SelfLink,
    /// `/proc/thread-self`。
    ThreadSelf,
    /// `/proc/sys`、`/proc/sysvipc` 下的目录或文件，取值来自 [`STATIC`]。
    Static(&'static [u8]),
    /// `/proc/<pid>`；第二项是 `Some(tid)` 时是 `/proc/<pid>/task/<tid>`。
    Pid(Tid, Option<Tid>),
    /// `/proc/<pid>/<项>`（含 task 子目录里的）。
    Entry(Tid, Option<Tid>, Entry),
    /// `/proc/<pid>/net/<名字>`（含 task 子目录里的），名字取值来自 [`PROC_NET`]。
    Net(Tid, Option<Tid>, &'static [u8]),
    /// `/proc/<pid>/fd/<n>`。
    Fd(Tid, i32),
    /// `/proc/<pid>/fd/<n>` 跟随之后得到的那个打开文件（不在仓库里的那些：管道、宿主 fd）。
    OpenFd(Tid, i32),
    /// `/proc/<pid>/exe` 跟随之后得到的程序文件，已经删除、没法按 inode 打开时
    /// （[`crate::process::ExeFile`]）。
    OpenExe(Tid),
    /// `/proc/<pid>/fdinfo/<n>`。
    FdInfo(Tid, i32),
    /// `/sys`。
    SysRoot,
    /// `/sys` 下的目录或文件，取值来自 [`SYS_STATIC`]。
    Sys(&'static [u8]),
    /// `/sys/devices/system/cpu/cpu<n>`（路径为空），或者它下面的目录和文件（路径取值来自
    /// [`CPU_FILES`]）。
    SysCpu(u32, &'static [u8]),
    /// `/sys/class/net/<接口>`（路径为空），或者它下面的目录和文件（路径取值来自
    /// [`NET_FILES`]）；接口按宿主的接口编号记。
    SysNet(u32, &'static [u8]),
    /// `/sys/class/net/<bridge>/brif/<端口>`：指向 `../../<端口>/brport` 的链接（bridge 和
    /// 端口的接口编号）。
    SysBrif(u32, u32),
    /// cgroup 的目录（名字为空），或者它里面的文件（名字取值来自 [`crate::cgroup::FILES`]）：
    /// cgroup 的编号和文件名。
    Cgroup(u64, &'static [u8]),
    /// `/proc/<pid>/ns/<种类>`（含 task 子目录里的）：指向命名空间的魔法链接。
    Ns(Tid, Option<Tid>, NsKind),
    /// `/proc/<pid>/ns/<种类>` 跟随之后得到的命名空间（Linux 的 nsfs 里的文件），按编号。
    NsFile(NsKind, u64),
    /// mqueue 文件系统的根目录：IPC 命名空间（编号）的 POSIX 消息队列
    /// （[`crate::syscall::MessageQueue`]）。
    MqRoot(u64),
    /// mqueue 文件系统里的一个队列：IPC 命名空间和队列的 inode 编号。
    Mq(u64, u64),
    /// `/proc/sys/net/ipv4/conf/<目录>`（第一项是 false）或者 `ipv6/conf/<目录>`（名字为空），
    /// 或者它下面的一项（名字取值来自 [`crate::netconf`] 的表）；按读者所在的网络命名空间
    /// （[`crate::netdev::conf_read`]）。
    NetConf(bool, crate::netdev::ConfDir, &'static [u8]),
    /// `/proc/sys/net/` 下的一项 sysctl（[`crate::netconf::SYSCTLS`] 里的，路径如
    /// `ipv4/tcp_keepalive_time`），按读者所在的网络命名空间。
    NetSysctl(&'static str),
}

/// `ipv4/conf`、`ipv6/conf` 的目录。
const NET_CONF4: &[u8] = b"sys/net/ipv4/conf/";
const NET_CONF6: &[u8] = b"sys/net/ipv6/conf/";

/// `/proc/<pid>/task/<tid>` 里的项：线程自己的，以及与进程相同的（调试器读线程的 maps、
/// mem）。
const TASK_ENTRIES: [Entry; 27] = [
    Entry::Stat,
    Entry::Status,
    Entry::Statm,
    Entry::Comm,
    Entry::Cmdline,
    Entry::Environ,
    Entry::Limits,
    Entry::Children,
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
    Entry::Fd,
    Entry::FdInfo,
    Entry::Cgroup,
    Entry::Net,
    Entry::Ns,
    Entry::UidMap,
    Entry::GidMap,
    Entry::ProjidMap,
    Entry::Setgroups,
];

/// `/proc/<pid>`（`tid` 是 None）或者 `/proc/<pid>/task/<tid>` 里有没有 `e` 这一项：
/// `children` 只在后者里。
fn in_pid_dir(tid: Option<Tid>, e: &Entry) -> bool {
    match tid {
        None => *e != Entry::Children,
        Some(_) => TASK_ENTRIES.contains(e),
    }
}

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
    fn parent(&self, _: &PseudoNode) -> Option<PseudoNode> {
        None
    }
    fn name(&self, _: MountId, _: &PseudoNode) -> Vec<u8> {
        Vec::new()
    }
    fn lookup(&self, _: MountId, _: &PseudoNode, _: &[u8]) -> Result<PseudoNode, Errno> {
        Err(Errno::ENOENT)
    }
    fn kind(&self, _: &PseudoNode) -> Result<u32, Errno> {
        Ok(mode::S_IFDIR)
    }
    fn readlink(&self, _: MountId, _: &PseudoNode) -> Result<Vec<u8>, Errno> {
        Err(Errno::EINVAL)
    }
    fn follow(&self, _: MountId, _: &PseudoNode) -> Option<Result<Node, Errno>> {
        None
    }
}

/// 节点所在的文件系统。
pub(crate) fn fs_of(node: &PseudoNode) -> Fs {
    match node {
        PseudoNode::PtsRoot(_) | PseudoNode::Pts(..) | PseudoNode::PtsPtmx(_) => Fs::Pts,
        PseudoNode::SysRoot
        | PseudoNode::Sys(_)
        | PseudoNode::SysCpu(..)
        | PseudoNode::SysNet(..)
        | PseudoNode::SysBrif(..) => Fs::Sys,
        PseudoNode::Cgroup(..) => Fs::Cgroup,
        PseudoNode::NsFile(..) => Fs::Nsfs,
        PseudoNode::MqRoot(_) | PseudoNode::Mq(..) => Fs::Mqueue,
        _ => Fs::Proc,
    }
}

/// sysfs 里 cgroup2 的挂载点 `fs/cgroup`（没有挂东西时是空目录）。
pub(crate) const CGROUP_POINT: PseudoNode = PseudoNode::Sys(b"fs/cgroup/");

fn parse_num(name: &[u8]) -> Option<i64> {
    if name.is_empty() || name.len() > 10 || (name.len() > 1 && name[0] == b'0') {
        return None;
    }
    std::str::from_utf8(name).ok()?.parse().ok()
}

/// 合成节点的 inode 编号：对节点取哈希（同一次运行里稳定），避开 0。各个文件系统的根目录
/// 与 Linux 一样是 1（procfs 的 `PROC_ROOT_INO`、kernfs 的根、devpts 的根）：runc 打开
/// `/proc` 之后检查它（pathrs-lite 的 `verifyProcRoot`），不是 1 就当作被换掉的 procfs。
/// devpts 里的也照 Linux：ptmx 是 2，终端 n 是 n + 3。
pub(crate) fn ino_of(node: &PseudoNode) -> u64 {
    if matches!(
        node,
        PseudoNode::ProcRoot
            | PseudoNode::SysRoot
            | PseudoNode::PtsRoot(_)
            | PseudoNode::Cgroup(crate::cgroup::ROOT, b"")
    ) {
        return 1;
    }
    match node {
        PseudoNode::PtsPtmx(_) => return 2,
        PseudoNode::Pts(_, n) => return *n as u64 + 3,
        _ => {}
    }
    // 队列按它自己的编号（mq_open 得到的 fd 的 fstat 也是它）
    if let PseudoNode::Mq(_, ino) = node {
        return *ino;
    }
    let mut h = std::hash::DefaultHasher::new();
    node.hash(&mut h);
    h.finish() | 1
}

/// stat 和 readdir 给出的 inode：`/proc/<pid>` 和它下面的项还取决于是哪个进程，与 Linux
/// 一样，pid 被新进程重新使用之后 inode 不同。
pub(crate) fn node_ino(k: &Kernel, node: &PseudoNode) -> u64 {
    let pid = match node {
        // 命名空间的 inode 号就是它的编号
        PseudoNode::NsFile(_, id) => return *id,
        PseudoNode::Pid(pid, _)
        | PseudoNode::Entry(pid, ..)
        | PseudoNode::Net(pid, ..)
        | PseudoNode::Fd(pid, _)
        | PseudoNode::OpenFd(pid, _)
        | PseudoNode::OpenExe(pid)
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

    /// 挂载 `mnt`（procfs 的一个实例）的 pid 命名空间：目录名、链接和文件里的 pid 按它
    /// 算（Linux 的 `proc_pid_ns`）。
    pub(crate) fn pid_ns(&self, mnt: MountId) -> Rc<PidNs> {
        self.vfs.proc_pid_ns(mnt).unwrap_or_else(PidNs::init)
    }

    /// 读者所在的 pid 命名空间里最后分配的编号（`ns_last_pid`、`loadavg`，Linux 按读者的
    /// 命名空间，不按挂载的）。
    fn last_pid(&self) -> Tid {
        let ns = self.k.pid_ns_of(self.tgid);
        if ns.level == 0 {
            self.k.last_pid()
        } else {
            ns.last_pid()
        }
    }

    /// 读者（`self` 指向的线程）所在的 UTS、IPC 命名空间：`/proc/sys/kernel/hostname`、
    /// System V IPC 的限制和 `/proc/sysvipc` 按它。
    pub(crate) fn ns(&self) -> crate::nsproxy::NsProxy {
        self.k
            .tasks
            .get(&self.tid)
            .map_or_else(|| self.k.init_ns.clone(), |t| t.ns.borrow().clone())
    }

    /// 读者（`self` 指向的进程）的凭据：procfs 里的 ID 按它的用户命名空间换算。
    /// 读者所在网络命名空间里编号为 `index` 的接口（宿主的或者 lo，和虚拟设备）。
    fn interface(&self, index: u32) -> Option<appine_hal::net::Interface> {
        crate::netdev::interfaces(&self.ns().net)
            .into_iter()
            .find(|i| i.index == index)
    }

    /// 读者所在网络命名空间里编号为 `index` 的接口是不是 bridge、是不是 bridge 的端口。
    fn bridge_roles(&self, index: u32) -> (bool, bool) {
        crate::netdev::views(&self.ns().net)
            .into_iter()
            .find(|v| v.index == index)
            .map_or((false, false), |v| {
                (v.kind == crate::netdev::Kind::Bridge, v.master.is_some())
            })
    }

    /// `/proc/sys/net/ipv*/conf` 下的目录还在（接口没有删掉）。
    fn conf_dir_exists(&self, dir: crate::netdev::ConfDir) -> bool {
        match dir {
            crate::netdev::ConfDir::Dev(i) => self.interface(i).is_some(),
            _ => true,
        }
    }

    /// 读者所在的 cgroup 命名空间的根（cgroup 的编号）。
    pub(crate) fn cgroup_root(&self) -> u64 {
        self.k
            .tasks
            .get(&self.tid)
            .map_or(crate::cgroup::ROOT, |t| t.ns.borrow().cgroup.root)
    }

    /// 开机以来的纳秒数 `ns` 加上读者所在的 time 命名空间里 boottime 的偏移（Linux 的
    /// `timens_add_boottime_ns`：`/proc/uptime`、进程的启动时间），不小于 0。
    fn with_boot_offset(&self, ns: u64) -> u64 {
        let offset = self
            .k
            .tasks
            .get(&self.tid)
            .map_or(0, |t| t.ns.borrow().time.offset(7));
        (ns as i128 + offset as i128).clamp(0, u64::MAX as i128) as u64
    }

    pub(crate) fn reader(&self) -> Cred {
        self.group(self.tgid)
            .map_or_else(Cred::root, |g| g.borrow().cred.clone())
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

    fn pty(&self, fs: u64, n: u32) -> Option<Rc<RefCell<Tty>>> {
        self.k.ptys.get(&(fs, n)).and_then(|w| w.upgrade())
    }
}

impl Pseudo for View<'_> {
    fn parent(&self, node: &PseudoNode) -> Option<PseudoNode> {
        use PseudoNode::*;
        Some(match node {
            PtsRoot(_) | ProcRoot | SysRoot | MqRoot(_) => return None,
            // 子 cgroup 的上级是父 cgroup，根没有
            Cgroup(id, b"") => Cgroup(self.k.cgroups.get(*id)?.parent?, b""),
            Mq(ns, _) => MqRoot(*ns),
            Pts(fs, _) | PtsPtmx(fs) => PtsRoot(*fs),
            Global(_) | SelfLink | ThreadSelf | Pid(_, None) => ProcRoot,
            Static(path) => tree_parent(&STATIC, path).map_or(ProcRoot, Static),
            Sys(path) => tree_parent(&SYS_STATIC, path).map_or(SysRoot, Sys),
            SysCpu(_, b"") => Sys(SYS_CPU_DIR),
            SysCpu(n, path) => tree_parent(&CPU_FILES, path)
                .or_else(|| tree_parent(&HOST_CPU_FILES, path))
                .map_or(SysCpu(*n, b""), |p| SysCpu(*n, p)),
            SysNet(_, b"") => Sys(SYS_NET_DIR),
            SysNet(n, path) => tree_parent(&NET_FILES, path)
                .or_else(|| tree_parent(&BRIDGE_FILES, path))
                .or_else(|| tree_parent(&PORT_FILES, path))
                .map_or(SysNet(*n, b""), |p| SysNet(*n, p)),
            SysBrif(n, _) => SysNet(*n, b"brif/"),
            Cgroup(id, _) => Cgroup(*id, b""),
            NetConf(v6, _, b"") => Static(if *v6 { NET_CONF6 } else { NET_CONF4 }),
            NetSysctl(path) => {
                let dir = &path[..=path.find('/').unwrap_or(0)];
                tree_lookup(&STATIC, b"sys/net/", dir.trim_end_matches('/').as_bytes())
                    .map_or(ProcRoot, Static)
            }
            NetConf(v6, dir, _) => NetConf(*v6, *dir, b""),
            Pid(pid, Some(_)) => Entry(*pid, None, self::Entry::Task),
            Entry(pid, tid, _) => Pid(*pid, *tid),
            Net(pid, tid, _) => Entry(*pid, *tid, self::Entry::Net),
            Fd(pid, _) | OpenFd(pid, _) => Entry(*pid, None, self::Entry::Fd),
            OpenExe(pid) => Pid(*pid, None),
            FdInfo(pid, _) => Entry(*pid, None, self::Entry::FdInfo),
            Ns(pid, tid, _) => Entry(*pid, *tid, self::Entry::Ns),
            NsFile(..) => return None,
        })
    }

    fn name(&self, mnt: MountId, node: &PseudoNode) -> Vec<u8> {
        use PseudoNode::*;
        match node {
            PtsRoot(_) => b"pts".to_vec(),
            ProcRoot => b"proc".to_vec(),
            Pts(_, n) => n.to_string().into_bytes(),
            PtsPtmx(_) => b"ptmx".to_vec(),
            Global(g) => GLOBALS.iter().find(|(_, x)| x == g).unwrap().0.to_vec(),
            SelfLink => b"self".to_vec(),
            ThreadSelf => b"thread-self".to_vec(),
            Static(path) | Sys(path) => tree_name(path).to_vec(),
            SysRoot => b"sys".to_vec(),
            MqRoot(_) => b"mqueue".to_vec(),
            Mq(ns, ino) => self
                .k
                .ipcs
                .get(ns)
                .and_then(|(_, ipc)| crate::syscall::mqueue_find(ipc, *ino))
                .map_or_else(|| ino.to_string().into_bytes(), |(n, _)| n.to_vec()),
            SysCpu(n, b"") => format!("cpu{n}").into_bytes(),
            SysCpu(_, path) => tree_name(path).to_vec(),
            SysNet(n, b"") => self
                .interface(*n)
                .map_or_else(|| n.to_string(), |i| i.name)
                .into_bytes(),
            NetConf(_, dir, b"") => {
                crate::netdev::conf_dir_name(&self.ns().net, *dir).unwrap_or_else(|| b"?".to_vec())
            }
            NetConf(_, _, name) => name.to_vec(),
            NetSysctl(path) => tree_name(path.as_bytes()).to_vec(),
            SysNet(_, path) => tree_name(path).to_vec(),
            SysBrif(_, p) => self
                .interface(*p)
                .map_or_else(|| p.to_string(), |i| i.name)
                .into_bytes(),
            Cgroup(crate::cgroup::ROOT, b"") => b"cgroup".to_vec(),
            Cgroup(id, b"") => self
                .k
                .cgroups
                .get(*id)
                .map_or_else(|| id.to_string().into_bytes(), |g| g.name.clone()),
            Cgroup(_, name) => name.to_vec(),
            Pid(pid, None) => self
                .pid_ns(mnt)
                .vnr(*pid)
                .unwrap_or(0)
                .to_string()
                .into_bytes(),
            Pid(_, Some(tid)) => self
                .pid_ns(mnt)
                .vnr(*tid)
                .unwrap_or(0)
                .to_string()
                .into_bytes(),
            Entry(_, _, e) => ENTRIES.iter().find(|(_, x)| x == e).unwrap().0.to_vec(),
            Net(_, _, name) => name.to_vec(),
            Fd(_, fd) | OpenFd(_, fd) | FdInfo(_, fd) => fd.to_string().into_bytes(),
            OpenExe(_) => b"exe".to_vec(),
            Ns(_, _, k) => k.name().as_bytes().to_vec(),
            NsFile(k, id) => format!("{}:[{id}]", k.type_name()).into_bytes(),
        }
    }

    fn lookup(&self, mnt: MountId, dir: &PseudoNode, name: &[u8]) -> Result<PseudoNode, Errno> {
        use PseudoNode::*;
        // 名字里的 pid 是挂载的 pid 命名空间里的编号
        let global = |name: &[u8]| {
            let n = Tid::try_from(parse_num(name)?).ok()?;
            self.pid_ns(mnt).find(n)
        };
        let found = match dir {
            MqRoot(ns) => self
                .k
                .ipcs
                .get(ns)
                .and_then(|(_, ipc)| ipc.mqueues.get(name))
                .map(|q| Mq(*ns, q.borrow().ino)),
            PtsRoot(fs) => {
                if name == b"ptmx" {
                    Some(PtsPtmx(*fs))
                } else {
                    parse_num(name)
                        .and_then(|n| u32::try_from(n).ok())
                        .filter(|&n| self.pty(*fs, n).is_some())
                        .map(|n| Pts(*fs, n))
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
                        let pid = global(name)?;
                        self.pid_exists(pid, None).then_some(Pid(pid, None))
                    }),
            },
            Static(path) if *path == NET_CONF4 || *path == NET_CONF6 => {
                crate::netdev::conf_dirs(&self.ns().net)
                    .into_iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, d)| NetConf(*path == NET_CONF6, d, b""))
            }
            Static(path) => tree_lookup(&STATIC, path, name).map(Static).or_else(|| {
                let rest = path.strip_prefix(b"sys/net/")?;
                let full = [rest, name].concat();
                let init = self.ns().net.host_net.is_none();
                crate::netconf::sysctl(&full, init).map(|e| NetSysctl(e.0))
            }),
            NetConf(v6, dir, b"") => crate::netconf::table(*v6)
                .iter()
                .find(|(n, ..)| n.as_bytes() == name)
                .filter(|_| self.conf_dir_exists(*dir))
                .map(|(n, ..)| NetConf(*v6, *dir, n.as_bytes())),
            SysRoot => tree_lookup(&SYS_STATIC, b"", name).map(Sys),
            Sys(path) => tree_lookup(&SYS_STATIC, path, name).map(Sys).or_else(|| {
                if *path == SYS_NET_DIR {
                    return crate::netdev::interfaces(&self.ns().net)
                        .into_iter()
                        .find(|i| i.name.as_bytes() == name)
                        .map(|i| SysNet(i.index, b""));
                }
                let n = name.strip_prefix(b"cpu").and_then(parse_num)?;
                let n = u32::try_from(n).ok().filter(|&n| (n as usize) < cpus())?;
                (*path == SYS_CPU_DIR).then_some(SysCpu(n, b""))
            }),
            // 子 cgroup，或者这个 cgroup 有的文件
            Cgroup(id, b"") => self.k.cgroups.get(*id).and_then(|g| {
                g.children.get(name).map(|&c| Cgroup(c, b"")).or_else(|| {
                    self.k
                        .cgroups
                        .files(*id)
                        .find(|f| *f == name)
                        .map(|f| Cgroup(*id, f))
                })
            }),
            SysCpu(n, path) => tree_lookup(&CPU_FILES, path, name)
                .or_else(|| {
                    tree_lookup(&HOST_CPU_FILES, path, name).filter(|p| host_cpu_has(*n, p))
                })
                .map(|p| SysCpu(*n, p)),
            SysNet(n, b"brif/") => crate::netdev::views(&self.ns().net)
                .into_iter()
                .find(|v| v.master == Some(*n) && v.name == name)
                .map(|v| SysBrif(*n, v.index)),
            SysNet(n, path) => tree_lookup(&NET_FILES, path, name)
                .filter(|_| self.interface(*n).is_some())
                .or_else(|| {
                    let (bridge, port) = self.bridge_roles(*n);
                    tree_lookup(&BRIDGE_FILES, path, name)
                        .filter(|_| bridge)
                        .or_else(|| tree_lookup(&PORT_FILES, path, name).filter(|_| port))
                })
                .map(|p| SysNet(*n, p)),
            Pid(pid, tid) => {
                let allowed = |e: &self::Entry| in_pid_dir(*tid, e);
                ENTRIES
                    .iter()
                    .find(|(n, e)| *n == name && allowed(e))
                    .map(|(_, e)| Entry(*pid, *tid, *e))
                    .filter(|_| self.pid_exists(*pid, *tid))
            }
            // 线程目录里的 fd、fdinfo 也是进程的打开文件（fd 表按进程记）
            Entry(pid, _, e @ (self::Entry::Fd | self::Entry::FdInfo)) => parse_num(name)
                .map(|fd| fd as i32)
                .filter(|&fd| self.open_file(*pid, fd).is_some())
                .map(|fd| match e {
                    self::Entry::Fd => Fd(*pid, fd),
                    _ => FdInfo(*pid, fd),
                }),
            Entry(pid, None, self::Entry::Task) => global(name)
                .filter(|&t| self.pid_exists(*pid, Some(t)))
                .map(|t| Pid(*pid, Some(t))),
            Entry(pid, tid, self::Entry::Net) => PROC_NET
                .iter()
                .find(|n| **n == name)
                .filter(|_| self.pid_exists(*pid, *tid))
                .map(|n| Net(*pid, *tid, n)),
            Entry(pid, tid, self::Entry::Ns) => NsKind::ALL
                .into_iter()
                .find(|k| k.name().as_bytes() == name)
                .filter(|_| self.pid_exists(*pid, *tid))
                .map(|k| Ns(*pid, *tid, k)),
            _ => return Err(Errno::ENOTDIR),
        };
        found.ok_or(Errno::ENOENT)
    }

    fn kind(&self, node: &PseudoNode) -> Result<u32, Errno> {
        use PseudoNode::*;
        Ok(match node {
            PtsRoot(_) | ProcRoot | Pid(..) | SysRoot | MqRoot(_) => mode::S_IFDIR,
            Mq(..) => mode::S_IFREG,
            Static(p) | Sys(p) if p.ends_with(b"/") => mode::S_IFDIR,
            SysCpu(_, p) | SysNet(_, p) if p.is_empty() || p.ends_with(b"/") => mode::S_IFDIR,
            Cgroup(_, b"") => mode::S_IFDIR,
            NetSysctl(_) => mode::S_IFREG,
            NetConf(_, dir, name) => {
                if !self.conf_dir_exists(*dir) {
                    return Err(Errno::ENOENT);
                }
                if name.is_empty() {
                    mode::S_IFDIR
                } else {
                    mode::S_IFREG
                }
            }
            Entry(
                _,
                _,
                self::Entry::Fd
                | self::Entry::FdInfo
                | self::Entry::Task
                | self::Entry::Net
                | self::Entry::Ns,
            ) => mode::S_IFDIR,
            Pts(..) | PtsPtmx(_) => mode::S_IFCHR,
            SelfLink | ThreadSelf | Fd(..) | Ns(..) | SysBrif(..) => mode::S_IFLNK,
            Global(self::Global::Mounts | self::Global::Net) => mode::S_IFLNK,
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

    fn readlink(&self, mnt: MountId, node: &PseudoNode) -> Result<Vec<u8>, Errno> {
        use PseudoNode::*;
        // 读者在挂载的 pid 命名空间里看不到时没有 self、thread-self（ENOENT）
        let ns = || self.pid_ns(mnt);
        match node {
            SelfLink => Ok(ns()
                .vnr(self.tgid)
                .ok_or(Errno::ENOENT)?
                .to_string()
                .into_bytes()),
            ThreadSelf => {
                let ns = ns();
                let (tgid, tid) = (ns.vnr(self.tgid), ns.vnr(self.tid));
                let (Some(tgid), Some(tid)) = (tgid, tid) else {
                    return Err(Errno::ENOENT);
                };
                Ok(format!("{tgid}/task/{tid}").into_bytes())
            }
            Global(self::Global::Mounts) => Ok(b"self/mounts".to_vec()),
            Global(self::Global::Net) => Ok(b"self/net".to_vec()),
            Entry(pid, tid, self::Entry::Root) => {
                let task = self.task(*pid, *tid).ok_or(Errno::ENOENT)?;
                if !self.may_read(*pid) {
                    return Err(Errno::EACCES);
                }
                let root = task.res.borrow().fs.borrow().root.clone();
                self.vfs.path_of(self, &self.root(), &root)
            }
            // 与 root、cwd 一样要能看那个进程（Linux 的 proc_pid_readlink、proc_pid_get_link
            // 都先 proc_fd_access_allowed）。程序文件删除之后与 fd 一样带 ` (deleted)`
            Entry(pid, _, self::Entry::Exe) => {
                let g = self.group(*pid).ok_or(Errno::ENOENT)?;
                if !self.may_read(*pid) {
                    return Err(Errno::EACCES);
                }
                let exe = g.borrow().exe.clone().ok_or(Errno::ENOENT)?;
                let path = exe.path.to_vec();
                Ok(match exe.node {
                    Some((_, ino)) => mark_deleted(self.vfs.store(), ino, path),
                    None => path,
                })
            }
            Entry(pid, tid, self::Entry::Cwd) => {
                let task = self.task(*pid, *tid).ok_or(Errno::ENOENT)?;
                if !self.may_read(*pid) {
                    return Err(Errno::EACCES);
                }
                let cwd = task.res.borrow().fs.borrow().cwd.clone();
                self.vfs.path_of(self, &self.root(), &cwd)
            }
            Fd(pid, fd) => {
                let file = self.open_file(*pid, *fd).ok_or(Errno::ENOENT)?;
                let f = file.borrow();
                let name = describe(&f);
                Ok(match &f.kind {
                    FileKind::Regular { ino, .. } => mark_deleted(self.vfs.store(), *ino, name),
                    _ => name,
                })
            }
            Ns(pid, tid, k) => {
                let task = self.task(*pid, *tid).ok_or(Errno::ENOENT)?;
                if !self.may_read(*pid) {
                    return Err(Errno::EACCES);
                }
                let id = k.id_of(self.k, &task).ok_or(Errno::ENOENT)?;
                Ok(self.name(mnt, &NsFile(*k, id)))
            }
            SysBrif(_, p) => {
                let i = self.interface(*p).ok_or(Errno::ENOENT)?;
                Ok(format!("../../{}/brport", i.name).into_bytes())
            }
            _ => Err(Errno::EINVAL),
        }
    }

    fn follow_dentry(&self, node: &PseudoNode) -> Option<crate::vfs::Dentry> {
        if let PseudoNode::Entry(pid, _, Entry::Exe) = node {
            return self.group(*pid)?.borrow().exe.as_ref()?.dentry.clone();
        }
        let PseudoNode::Fd(pid, fd) = node else {
            return None;
        };
        let file = self.open_file(*pid, *fd)?;
        let f = file.borrow();
        // 只有不是目录的文件要它（目录的路径按上级目录算）
        let dir = match &f.kind {
            FileKind::Path(n) => self.vfs.is_dir(self, n).unwrap_or(true),
            FileKind::Directory { .. } => true,
            _ => false,
        };
        f.dentry.clone().filter(|_| !dir)
    }

    /// `/proc/<pid>/fd/<n>`、`ns/*`、`root`、`cwd`、`exe`（Linux 的 proc_pid_get_link、
    /// proc_ns_get_link 都经由 nd_jump_link）。`/proc/self`、`thread-self` 是普通的链接。
    fn magic(&self, node: &PseudoNode) -> bool {
        matches!(
            node,
            PseudoNode::Fd(..)
                | PseudoNode::Ns(..)
                | PseudoNode::Entry(_, _, Entry::Root | Entry::Cwd | Entry::Exe)
        )
    }

    fn follow(&self, mnt: MountId, node: &PseudoNode) -> Option<Result<Node, Errno>> {
        match node {
            PseudoNode::Ns(pid, tid, k) => {
                let task = self.task(*pid, *tid)?;
                if !self.may_read(*pid) {
                    return Some(Err(Errno::EACCES));
                }
                let Some(id) = k.id_of(self.k, &task) else {
                    return Some(Err(Errno::ENOENT));
                };
                return Some(Ok(Node::Pseudo(mnt, PseudoNode::NsFile(*k, id))));
            }
            // 根目录、当前目录直接指到那个进程的节点（Linux 的 proc_root_link、
            // proc_cwd_link）：经由它们能进到别的挂载命名空间、chroot 的外面
            PseudoNode::Entry(pid, tid, e @ (Entry::Root | Entry::Cwd)) => {
                let task = self.task(*pid, *tid)?;
                if !self.may_read(*pid) {
                    return Some(Err(Errno::EACCES));
                }
                let res = task.res.borrow();
                let fs = res.fs.borrow();
                return Some(Ok(match e {
                    Entry::Root => fs.root.clone(),
                    _ => fs.cwd.clone(),
                }));
            }
            // 程序文件本身（Linux 的 proc_exe_link）：改了名、被删除、被换掉之后仍是原来
            // 那个。删除了的仓库里的 object 也删了，经由记着的宿主文件打开（OpenExe）。没有
            // 仓库（开发用的宿主文件）时按读出来的路径跟随
            PseudoNode::Entry(pid, _, Entry::Exe) => {
                let g = self.group(*pid)?;
                if !self.may_read(*pid) {
                    return Some(Err(Errno::EACCES));
                }
                let Some(exe) = g.borrow().exe.clone() else {
                    return Some(Err(Errno::ENOENT));
                };
                let (m, ino) = exe.node?;
                let deleted = self.vfs.store().attrs(ino).map_or(true, |st| st.nlink == 0);
                return Some(Ok(if deleted && exe.host.is_some() {
                    Node::Pseudo(mnt, PseudoNode::OpenExe(*pid))
                } else {
                    Node::Store(m, ino)
                }));
            }
            _ => {}
        }
        self.follow_fd(mnt, node).map(Ok)
    }
}

impl View<'_> {
    /// 读者能不能看进程 `pid` 的根目录、当前目录、程序文件、命名空间（Linux 的
    /// `ptrace_may_access(PTRACE_MODE_READ_FSCREDS)`）。
    fn may_read(&self, pid: Tid) -> bool {
        pid == self.tgid
            || self.group(pid).is_some_and(|g| {
                let g = g.borrow();
                self.reader().may_ptrace(&g.cred, g.dumpable, true)
            })
    }

    /// `/proc/<pid>/fd/<n>` 跟随之后的节点（[`Pseudo::follow`]）。
    fn follow_fd(&self, mnt: MountId, node: &PseudoNode) -> Option<Node> {
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
                Node::Pseudo(mnt, PseudoNode::OpenFd(*pid, *fd))
            }
            FileKind::Regular { ino, .. } | FileKind::Device { ino, .. } => {
                Node::Store(f.mnt, *ino)
            }
            FileKind::Directory { node, .. } | FileKind::Path(node) => node.clone(),
            FileKind::Proc { mnt, node, .. } => Node::Pseudo(*mnt, node.clone()),
            // 从设备：经由的 devpts 挂载里的路径
            FileKind::Pty(end) if !end.master => {
                let t = end.tty.borrow();
                Node::Pseudo(end.mnt, PseudoNode::Pts(t.fs, t.index))
            }
            _ => Node::Pseudo(mnt, PseudoNode::OpenFd(*pid, *fd)),
        })
    }
}

/// 与 Linux 的 `d_path` 一样，已经删除的文件（仍然打开着、映射着）在路径后面加上
/// ` (deleted)`：`/proc/<pid>/fd/<n>`、maps、smaps。仓库里删掉最后一个名字之后 inode 的行
/// 以 nlink = 0 留到下次独占打开仓库时，所以按 nlink 判断。Linux 看的是打开时那个名字，
/// 别的硬链接还在、只删了这个名字时也加，这里不加。apt 升级时 dpkg 用 rename 换掉旧的库，
/// needrestart 按 maps 里的 ` (deleted)` 找出要重启的进程。
fn mark_deleted(store: &Store, ino: Ino, mut name: Vec<u8>) -> Vec<u8> {
    const DELETED: &[u8] = b" (deleted)";
    if !name.ends_with(DELETED) && store.attrs(ino).is_ok_and(|st| st.nlink == 0) {
        name.extend_from_slice(DELETED);
    }
    name
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
        FileKind::Bpf(_) => b"anon_inode:bpf-prog".to_vec(),
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
    // mqueue 文件系统：属主、权限、时间记在队列和命名空间里
    match node {
        PseudoNode::MqRoot(ns) => {
            let (_, ipc) = ps.k.ipcs.get(ns).ok_or(Errno::ENOENT)?;
            let mut st = ipc.mq_dir.borrow().stat(ipc.mqueues.len());
            st.ino = ino_of(node);
            return Ok(st);
        }
        PseudoNode::Mq(ns, ino) => {
            let (_, ipc) = ps.k.ipcs.get(ns).ok_or(Errno::ENOENT)?;
            let (_, q) = crate::syscall::mqueue_find(ipc, *ino).ok_or(Errno::ENOENT)?;
            return Ok(q.borrow().stat());
        }
        _ => {}
    }
    let kind = ps.kind(node)?;
    let sysfs = matches!(fs_of(node), Fs::Sys | Fs::Cgroup);
    let perm = match (kind, node) {
        (mode::S_IFDIR, _) if sysfs => 0o755,
        (_, PseudoNode::Cgroup(_, name)) if !name.is_empty() => {
            crate::cgroup::file_mode(name).unwrap_or(0o444)
        }
        (mode::S_IFDIR, _) => 0o555,
        (mode::S_IFLNK, _) => 0o777,
        (_, PseudoNode::Pts(fs, n)) => pty(ps.k, *fs, *n).map_or(0o620, |t| t.borrow().mode),
        // ptmx 的权限位按实例的 ptmxmode
        (_, PseudoNode::PtsPtmx(fs)) => {
            ps.k.vfs
                .as_ref()
                .map_or(0o666, |v| v.devpts_options(*fs).ptmxmode)
        }
        (_, PseudoNode::Entry(_, _, Entry::Mem)) => 0o600,
        (_, PseudoNode::NetSysctl(path)) => {
            u32::from(crate::netconf::sysctl(path.as_bytes(), true).map_or(0o444, |e| e.2))
        }
        (_, n) if writable(n) => 0o644,
        (_, PseudoNode::Entry(_, _, Entry::Pagemap | Entry::Auxv)) => 0o400,
        (_, PseudoNode::Global(Global::Kmsg)) => 0o400,
        _ => 0o444,
    };
    let rdev_value = match node {
        PseudoNode::Pts(_, n) => rdev(PTS_MAJOR, *n),
        PseudoNode::PtsPtmx(_) => rdev(5, 2),
        _ => 0,
    };
    let now = crate::tty::wall_ns();
    let times = match node {
        PseudoNode::Pts(fs, n) => pty(ps.k, *fs, *n).map(|t| {
            let t = t.borrow();
            (t.atime_ns, t.mtime_ns, t.ctime_ns)
        }),
        _ => None,
    };
    let (atime_ns, mtime_ns, ctime_ns) = times.unwrap_or((now, now, now));
    let (uid, gid) = owner_of(ps.k, node);
    // 与 Linux 的 proc_task_getattr 一样，task 目录的链接数加上线程数：Chromium 的沙箱按它
    // 判断自己是不是单线程，zygote fork 之前检查它至少是 3，不是就崩溃
    let nlink = match node {
        PseudoNode::Entry(pid, None, Entry::Task) => {
            2 + ps
                .group(*pid)
                .map_or(0, |g| g.borrow().threads.len() as u32)
        }
        _ if kind == mode::S_IFDIR => 2,
        _ => 1,
    };
    Ok(Stat {
        ino: node_ino(ps.k, node),
        mode: kind | perm,
        nlink,
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
pub(crate) fn pty(k: &Kernel, fs: u64, n: u32) -> Option<Rc<RefCell<Tty>>> {
    k.ptys.get(&(fs, n)).and_then(std::rc::Weak::upgrade)
}

/// 节点的属主和属组：`/proc/<pid>` 下是进程的有效用户 ID 和组 ID，`/dev/pts/<n>`
/// 记在终端上（[`Tty::owner`]），其余是 root。
fn owner_of(k: &Kernel, node: &PseudoNode) -> (u32, u32) {
    match node {
        // 不可 dump 的进程的文件属于它的用户命名空间里的 root（Linux 的 task_dump_owner）
        PseudoNode::Pid(pid, _) | PseudoNode::Entry(pid, ..) | PseudoNode::Fd(pid, _) => {
            k.groups.get(pid).map_or((0, 0), |g| {
                let g = g.borrow();
                let c = &g.cred;
                if g.dumpable {
                    (c.euid, c.egid)
                } else {
                    (c.uid_in(0).unwrap_or(0), c.gid_in(0).unwrap_or(0))
                }
            })
        }
        PseudoNode::Pts(fs, n) => pty(k, *fs, *n).map_or((0, 0), |t| t.borrow().owner),
        PseudoNode::Cgroup(id, _) => k.cgroups.get(*id).map_or((0, 0), |g| (g.uid, g.gid)),
        _ => (0, 0),
    }
}

/// 读不了的文件：cgroup 里只能写的（`cgroup.kill`），与 Linux 一样打开时不查，读是
/// EINVAL（`cgroup_seqfile_show` 没有读的回调）。
pub(crate) fn unreadable(node: &PseudoNode) -> bool {
    matches!(node, PseudoNode::Cgroup(_, name)
        if crate::cgroup::file_mode(name).is_some_and(|m| m & 0o444 == 0))
}

/// 可以写入的 procfs 文件。
pub(crate) fn writable(node: &PseudoNode) -> bool {
    if let PseudoNode::Cgroup(_, name) = node {
        return crate::cgroup::file_mode(name).is_some_and(|m| m & 0o200 != 0);
    }
    if let PseudoNode::NetConf(_, _, name) = node {
        return !name.is_empty() && !crate::netconf::read_only(name);
    }
    if let PseudoNode::NetSysctl(path) = node {
        return crate::netconf::sysctl(path.as_bytes(), true).is_some_and(|e| e.2 & 0o200 != 0);
    }
    matches!(
        node,
        PseudoNode::Entry(_, None, Entry::OomScoreAdj | Entry::CoredumpFilter)
            | PseudoNode::Entry(_, None, Entry::TimensOffsets)
            | PseudoNode::Entry(_, _, Entry::Mem)
            | PseudoNode::Entry(_, _, Entry::UidMap | Entry::GidMap | Entry::Setgroups)
            | PseudoNode::Static(
                b"sys/kernel/hostname"
                    | b"sys/kernel/domainname"
                    | b"sys/kernel/pid_max"
                    | b"sys/kernel/threads-max"
                    | b"sys/kernel/ns_last_pid"
                    | b"sys/kernel/core_pattern"
                    | b"sys/kernel/seccomp/actions_logged"
                    | b"sys/kernel/core_uses_pid"
                    | b"sys/kernel/dmesg_restrict"
                    | b"sys/kernel/printk"
                    | b"sys/kernel/printk_devkmsg"
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
                    | b"sys/net/core/somaxconn"
                    | b"sys/net/ipv4/ip_forward"
                    | b"sys/net/ipv4/ip_local_port_range"
                    | b"sys/net/ipv4/ip_unprivileged_port_start"
                    | b"sys/net/ipv4/ping_group_range"
            )
    ) || matches!(node, PseudoNode::Static(p) if p.starts_with(b"sys/user/max_"))
}

/// 写 procfs 文件：`oom_score_adj`、主机名、域名和 System V IPC 的限制记下新值，其余
/// 可写的文件接受写入但不起作用。`cred` 是写入者的凭据。
pub(crate) fn write(
    k: &mut Kernel,
    (tid, cred): (Tid, &Cred),
    (ns, pid_ns): (&crate::nsproxy::NsProxy, &PidNs),
    node: &PseudoNode,
    data: &[u8],
) -> Result<(), Errno> {
    match node {
        PseudoNode::Cgroup(id, name) => {
            let writer = crate::cgroup::Writer {
                tid,
                pid_ns,
                ns_root: ns.cgroup.root,
            };
            return crate::cgroup::write(k, &writer, (*id, name), data);
        }
        PseudoNode::Entry(pid, _, e @ (Entry::UidMap | Entry::GidMap)) => {
            return write_id_map(k, cred, *pid, *e == Entry::GidMap, data);
        }
        PseudoNode::Entry(pid, _, Entry::Setgroups) => return write_setgroups(k, cred, *pid, data),
        PseudoNode::Entry(pid, None, Entry::TimensOffsets) => {
            return write_timens_offsets(k, cred, *pid, data);
        }
        // 网络命名空间的 sysctl：写入者所在的那个（crate::netconf）
        PseudoNode::NetConf(v6, dir, name) => {
            return crate::netdev::conf_write(&ns.net, *v6, *dir, name, data);
        }
        PseudoNode::NetSysctl(path) => {
            return crate::netconf::sysctl_write(&mut ns.net.devs.borrow_mut().conf, path, data);
        }
        PseudoNode::Static(b"sys/net/ipv4/ip_forward") => {
            return crate::netdev::conf_write(
                &ns.net,
                false,
                crate::netdev::ConfDir::All,
                b"forwarding",
                data,
            );
        }
        PseudoNode::Static(b"sys/net/core/somaxconn") => {
            let v = crate::netconf::parse_int(data)?;
            if !(0..=i32::MAX as i64).contains(&v) {
                return Err(Errno::EINVAL);
            }
            ns.net.devs.borrow_mut().conf.somaxconn = v;
            return Ok(());
        }
        PseudoNode::Static(b"sys/net/ipv4/ip_local_port_range") => {
            let mut d = ns.net.devs.borrow_mut();
            d.conf.port_range = crate::netconf::parse_port_range(data, d.conf.prot_sock)?;
            return Ok(());
        }
        PseudoNode::Static(b"sys/net/ipv4/ip_unprivileged_port_start") => {
            let mut d = ns.net.devs.borrow_mut();
            d.conf.prot_sock = crate::netconf::parse_prot_sock(data, d.conf.port_range.0)?;
            return Ok(());
        }
        PseudoNode::Static(b"sys/net/ipv4/ping_group_range") => {
            let r = crate::netconf::parse_ping_group(data)?;
            ns.net.devs.borrow_mut().conf.ping_group = r;
            return Ok(());
        }
        _ => {}
    }
    if let PseudoNode::Static(path) = node
        && let Some(name) = path.strip_prefix(b"sys/kernel/")
        && k.ipc(ns.ipc.id).tunables.read(name).is_some()
    {
        return k.ipc_mut(ns.ipc.id).tunables.write(name, data);
    }
    if let PseudoNode::Static(path @ (b"sys/kernel/hostname" | b"sys/kernel/domainname")) = node {
        let name = data.strip_suffix(b"\n").unwrap_or(data);
        if name.len() > crate::kernel::UTS_LEN {
            return Err(Errno::EINVAL);
        }
        let mut slot = if *path == b"sys/kernel/hostname" {
            ns.uts.hostname.borrow_mut()
        } else {
            ns.uts.domainname.borrow_mut()
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
        && let Some(slot) = k.ipc_mut(ns.ipc.id).mq_limits.slot(name)
    {
        *slot = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|&v: &u64| v > 0)
            .ok_or(Errno::EINVAL)?;
    }
    // 与 Linux 一样在 0 到 INT_MAX 之间；只记下，不实际限制
    if let PseudoNode::Static(path) = node
        && path.starts_with(b"sys/user/max_")
    {
        let n: u64 = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|&n| n <= i32::MAX as u64)
            .ok_or(Errno::EINVAL)?;
        k.max_namespaces.insert(path.to_vec(), n);
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
    // 与 Linux 一样在 0 到 pid_max 之间（checkpoint/restore 用它让新进程拿到指定的 pid），
    // 改的是写入者所在的 pid 命名空间的
    if let PseudoNode::Static(b"sys/kernel/ns_last_pid") = node {
        let pid: i32 = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|&n| (0..=crate::kernel::PID_MAX).contains(&n))
            .ok_or(Errno::EINVAL)?;
        if pid_ns.level == 0 {
            k.set_last_pid(pid);
        } else {
            pid_ns.set_last_pid(pid);
        }
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
        let value = kstrto(proc_number(data)?, true)?;
        if !(-1000..=1000).contains(&value) {
            return Err(Errno::EINVAL);
        }
        let value = value as i32;
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
    // proc_dointvec_minmax_sysadmin：写要开机时的命名空间里的 CAP_SYS_ADMIN，只能是 0、1
    if let PseudoNode::Static(b"sys/kernel/dmesg_restrict") = node {
        if !cred.capable(cap::SYS_ADMIN) {
            return Err(Errno::EPERM);
        }
        k.dmesg_restrict = std::str::from_utf8(data)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .filter(|v| (0..=1).contains(v))
            .ok_or(Errno::EINVAL)?
            == 1;
    }
    // devkmsg_sysctl_set_loglvl：proc_dostring 取到换行或第 9 个字节为止，必须恰好是 on、off、
    // ratelimit 之一再加一个字节（echo 的换行），否则 EINVAL、保持原样
    if let PseudoNode::Static(b"sys/kernel/printk_devkmsg") = node {
        let value = &data[..data.len().min(9)];
        let value = &value[..value
            .iter()
            .position(|&b| b == 0 || b == b'\n')
            .unwrap_or(value.len())];
        let (mode, len) = if value.starts_with(b"on") {
            (crate::klog::Devkmsg::On, 2)
        } else if value.starts_with(b"off") {
            (crate::klog::Devkmsg::Off, 3)
        } else if value.starts_with(b"ratelimit") {
            (crate::klog::Devkmsg::Ratelimit, 9)
        } else {
            return Err(Errno::EINVAL);
        };
        if len + 1 != data.len() {
            return Err(Errno::EINVAL);
        }
        k.klog.borrow_mut().devkmsg = mode;
    }
    // proc_dointvec：按顺序写前几个，最多四个；第一个就不是数时 EINVAL
    if let PseudoNode::Static(b"sys/kernel/printk") = node {
        let text = std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?;
        let values: Vec<i32> = text
            .split_ascii_whitespace()
            .map_while(|t| t.parse().ok())
            .take(4)
            .collect();
        if values.is_empty() {
            return Err(Errno::EINVAL);
        }
        k.printk[..values.len()].copy_from_slice(&values);
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
        let value = kstrto(proc_number(data)?, false)?;
        let group = k.groups.get(pid).ok_or(Errno::ESRCH)?;
        group.borrow_mut().coredump_filter = value as u32 & 0x1ff;
    }
    Ok(())
}

/// 写 `node` 时写入的字节数：写数字的 `/proc/<pid>` 文件只收 [`proc_number`] 看的那 12 个
/// 字节（Linux 截短 count 之后返回它），别的都收下。
pub(crate) fn written(node: &PseudoNode, len: usize) -> usize {
    match node {
        PseudoNode::Entry(_, _, Entry::OomScoreAdj | Entry::CoredumpFilter) => len.min(12),
        _ => len,
    }
}

/// `/proc/<pid>` 里写数字的文件（oom_score_adj、coredump_filter）的缓冲区：与 Linux 的
/// `PROC_NUMBUF` 一样只看前 12 个字节、到 NUL 为止（runc 写进来的值结尾带着 NUL），去掉两头
/// 的空白（`strstrip`）。
fn proc_number(data: &[u8]) -> Result<&str, Errno> {
    let data = &data[..data.len().min(12)];
    let data = data.split(|&b| b == 0).next().unwrap_or(data);
    Ok(std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?.trim())
}

/// Linux 的 `kstrtoint`（`signed`）、`kstrtouint` 的基数 0：`0x` 开头是十六进制，`0` 开头是
/// 八进制，可以有 `+`，有符号的还可以有 `-`。不是数字是 EINVAL，超出 32 位是 ERANGE。
fn kstrto(s: &str, signed: bool) -> Result<i64, Errno> {
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) if signed => (true, rest),
        _ => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (digits, radix) = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) if h.starts_with(|c: char| c.is_ascii_hexdigit()) => (h, 16),
        _ if s.len() > 1 && s.starts_with('0') => (&s[1..], 8),
        _ => (s, 10),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Err(Errno::EINVAL);
    }
    let v = u64::from_str_radix(digits, radix).map_err(|_| Errno::ERANGE)?;
    match (signed, neg) {
        (true, true) if v <= 1 << 31 => Ok(-(v as i64)),
        (true, false) if v <= i32::MAX as u64 => Ok(v as i64),
        (false, _) if v <= u32::MAX as u64 => Ok(v as i64),
        _ => Err(Errno::ERANGE),
    }
}

/// 目录的内容（不含 `.` 和 `..`）。
pub(crate) fn readdir(
    ps: &View<'_>,
    mnt: MountId,
    dir: &PseudoNode,
) -> Result<Vec<DirEntry>, Errno> {
    use PseudoNode::*;
    let mut nodes: Vec<PseudoNode> = Vec::new();
    let ns = ps.pid_ns(mnt);
    match dir {
        MqRoot(ipc) => {
            if let Some((_, ipc_objects)) = ps.k.ipcs.get(ipc) {
                nodes.extend(
                    ipc_objects
                        .mqueues
                        .values()
                        .map(|q| Mq(*ipc, q.borrow().ino)),
                );
            }
        }
        PtsRoot(fs) => {
            nodes.push(PtsPtmx(*fs));
            nodes.extend(
                ps.k.ptys
                    .range((*fs, 0)..=(*fs, u32::MAX))
                    .filter(|(_, w)| w.upgrade().is_some())
                    .map(|((_, n), _)| Pts(*fs, *n)),
            );
        }
        ProcRoot => {
            nodes.extend([SelfLink, ThreadSelf]);
            nodes.extend(tree_children(&STATIC, b"").map(Static));
            nodes.extend(GLOBALS.iter().map(|(_, g)| Global(*g)));
            // 挂载的 pid 命名空间里看得到的进程，按那里的编号排
            let mut pids: Vec<(Tid, Tid)> =
                ps.k.groups
                    .keys()
                    .filter_map(|&pid| Some((ns.vnr(pid)?, pid)))
                    .collect();
            pids.sort_unstable();
            nodes.extend(pids.into_iter().map(|(_, pid)| Pid(pid, None)));
        }
        Static(path) if *path == NET_CONF4 || *path == NET_CONF6 => {
            let v6 = *path == NET_CONF6;
            nodes.extend(
                crate::netdev::conf_dirs(&ps.ns().net)
                    .into_iter()
                    .map(|(_, d)| NetConf(v6, d, b"")),
            );
        }
        Static(path) => {
            nodes.extend(tree_children(&STATIC, path).map(Static));
            // /proc/sys/net/core、ipv4、ipv6、unix 下的 sysctl
            if let Some(dir) = path.strip_prefix(b"sys/net/") {
                let init = ps.ns().net.host_net.is_none();
                nodes.extend(
                    crate::netconf::SYSCTLS
                        .iter()
                        .filter(|(p, _, _, only)| {
                            (init || !only)
                                && p.as_bytes().starts_with(dir)
                                && !p.as_bytes()[dir.len()..].contains(&b'/')
                        })
                        .map(|e| NetSysctl(e.0)),
                );
            }
        }
        NetConf(v6, dir, b"") => nodes.extend(
            crate::netconf::table(*v6)
                .iter()
                .map(|(n, ..)| NetConf(*v6, *dir, n.as_bytes())),
        ),
        SysRoot => nodes.extend(tree_children(&SYS_STATIC, b"").map(Sys)),
        Sys(path) => {
            nodes.extend(tree_children(&SYS_STATIC, path).map(Sys));
            if *path == SYS_CPU_DIR {
                nodes.extend((0..cpus() as u32).map(|n| SysCpu(n, b"")));
            }
            if *path == SYS_NET_DIR {
                nodes.extend(
                    crate::netdev::interfaces(&ps.ns().net)
                        .iter()
                        .map(|i| SysNet(i.index, b"")),
                );
            }
        }
        SysNet(n, b"brif/") => nodes.extend(
            crate::netdev::views(&ps.ns().net)
                .into_iter()
                .filter(|v| v.master == Some(*n))
                .map(|v| SysBrif(*n, v.index)),
        ),
        SysNet(n, path) => {
            nodes.extend(tree_children(&NET_FILES, path).map(|p| SysNet(*n, p)));
            let (bridge, port) = ps.bridge_roles(*n);
            if bridge {
                nodes.extend(tree_children(&BRIDGE_FILES, path).map(|p| SysNet(*n, p)));
            }
            if port {
                nodes.extend(tree_children(&PORT_FILES, path).map(|p| SysNet(*n, p)));
            }
        }
        Cgroup(id, b"") => {
            nodes.extend(ps.k.cgroups.files(*id).map(|f| Cgroup(*id, f)));
            if let Some(g) = ps.k.cgroups.get(*id) {
                nodes.extend(g.children.values().map(|&c| Cgroup(c, b"")));
            }
        }
        SysCpu(n, path) => {
            nodes.extend(tree_children(&CPU_FILES, path).map(|p| SysCpu(*n, p)));
            nodes.extend(
                tree_children(&HOST_CPU_FILES, path)
                    .filter(|p| host_cpu_has(*n, p))
                    .map(|p| SysCpu(*n, p)),
            );
        }
        Entry(pid, tid, self::Entry::Net) => {
            nodes.extend(PROC_NET.iter().map(|n| Net(*pid, *tid, n)));
        }
        Entry(pid, tid, self::Entry::Ns) => {
            nodes.extend(NsKind::ALL.iter().map(|k| Ns(*pid, *tid, *k)));
        }
        Pid(pid, tid) => nodes.extend(
            ENTRIES
                .iter()
                .filter(|(_, e)| in_pid_dir(*tid, e))
                .map(|(_, e)| Entry(*pid, *tid, *e)),
        ),
        Entry(pid, _, e @ (self::Entry::Fd | self::Entry::FdInfo)) => {
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
                name: ps.name(mnt, &n),
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

/// 控制终端的设备号和前台进程组（在 pid 命名空间 `ns` 里的编号）。
fn ctty_info(ns: &PidNs, g: &ThreadGroup) -> (u64, i64) {
    match &g.ctty {
        Some(tty) => {
            let t = tty.borrow();
            let fg = t.fg.map_or(-1, |p| ns.vnr(p).unwrap_or(0) as i64);
            (rdev(PTS_MAJOR, t.index), fg)
        }
        None => (0, -1),
    }
}

fn pid_stat(ps: &View<'_>, ns: &PidNs, pid: Tid, tid: Option<Tid>) -> Result<String, Errno> {
    let g = ps.group(pid).ok_or(Errno::ENOENT)?;
    let g = g.borrow();
    let task = ps.task(pid, tid);
    let nice = task.as_ref().map_or(0, |t| t.nice.get());
    let (state, _) = state_of(ps, &g, tid);
    let (tty_nr, tpgid) = ctty_info(ns, &g);
    let v = |p: Tid| ns.vnr(p).unwrap_or(0);
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
        v(tid.unwrap_or(pid)),
        comm_str(task.as_deref(), pid),
        state,
        v(g.ppid),
        v(g.pgid),
        v(g.sid),
        tty_nr,
        tpgid,
        ticks(cpu.user_ns()),
        ticks(cpu.sys_ns),
        ticks(g.child_cpu.user_ns()),
        ticks(g.child_cpu.sys_ns),
        20 + nice,
        nice,
        g.threads.len().max(1),
        ps.with_boot_offset(g.start_ticks * (1_000_000_000 / HZ)) / (1_000_000_000 / HZ),
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

fn pid_status(ps: &View<'_>, ns: &PidNs, pid: Tid, tid: Option<Tid>) -> Result<String, Errno> {
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
    let v = |p: Tid| ns.vnr(p).unwrap_or(0);
    let tid = tid.unwrap_or(pid);
    writeln!(s, "Tgid:\t{}\nNgid:\t0\nPid:\t{}", v(pid), v(tid)).unwrap();
    let tracer = task.as_ref().and_then(|t| t.trace.borrow().tracer);
    writeln!(
        s,
        "PPid:\t{}\nTracerPid:\t{}",
        v(g.ppid),
        tracer.map_or(0, v)
    )
    .unwrap();
    // ID 按读者的用户命名空间换算（Linux 的 seq_user_ns）
    let c = &g.cred;
    let r = ps.reader();
    let u = |k: u32| r.uid_out(k);
    let gi = |k: u32| r.gid_out(k);
    writeln!(
        s,
        "Uid:\t{}\t{}\t{}\t{}",
        u(c.ruid),
        u(c.euid),
        u(c.suid),
        u(c.fsuid)
    )
    .unwrap();
    writeln!(
        s,
        "Gid:\t{}\t{}\t{}\t{}",
        gi(c.rgid),
        gi(c.egid),
        gi(c.sgid),
        gi(c.fsgid)
    )
    .unwrap();
    let groups: Vec<String> = c.groups.iter().map(|&g| gi(g).to_string()).collect();
    writeln!(s, "FDSize:\t{fdsize}\nGroups:\t{}", groups.join(" ")).unwrap();
    // 从挂载的 pid 命名空间到进程所在的那个，每层一列
    let levels = g.pid_ns.levels_from(ns.level);
    let col = |p: Tid| -> String {
        levels
            .iter()
            .map(|n| format!("\t{}", n.vnr(p).unwrap_or(0)))
            .collect()
    };
    writeln!(
        s,
        "NStgid:{}\nNSpid:{}\nNSpgid:{}\nNSsid:{}",
        col(pid),
        col(tid),
        col(g.pgid),
        col(g.sid)
    )
    .unwrap();
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
    writeln!(s, "CapAmb:\t{:016x}", c.cap_ambient).unwrap();
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

fn maps(store: &Store, task: &Task) -> String {
    let res = task.res.borrow();
    let mm = res.mm.borrow();
    mm.regions().iter().map(|r| map_line(store, r)).collect()
}

/// maps 的一行。与 Linux 一样，没有名字的行末尾也有一个空格，名字从第 74 列开始。
fn map_line(store: &Store, r: &Region) -> String {
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
        Some(name) => {
            let name = match r.file {
                Some((_, ino)) => mark_deleted(store, ino, name.to_vec()),
                None => name.to_vec(),
            };
            format!("{line:<73}{}\n", String::from_utf8_lossy(&name))
        }
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
fn smaps(store: &Store, task: &Task, rollup: bool) -> String {
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
        s += &map_line(store, r);
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
        (r.may_exec, "me"),
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

/// `/proc/<pid>/mounts`（`info` 为 false）或 `mountinfo`：与 Linux 的 `show_vfsmnt`、
/// `show_mountinfo` 一样列出那个进程的挂载命名空间 `ns` 里的挂载，按挂上的先后，挂载点相对
/// 它的根目录 `root`，根目录以外的不列（chroot 进去之后看不到外面的挂载，包括根文件系统）。
/// 挂载编号与 fdinfo 的 `mnt_id`、设备号与 stat 的 `st_dev` 一致。
fn mounts(ps: &View<'_>, root: &Node, ns: u64, info: bool) -> Vec<u8> {
    let visible = |node: &Node| match ps.vfs.reach(ps, root, node) {
        Ok((path, true)) => Some(path),
        _ => None,
    };
    let mut out = Vec::new();
    for m in ps.vfs.mounts_in(ns) {
        let Some(point) = visible(&m.root) else {
            continue;
        };
        MountLine {
            id: m.id,
            // 挂载树的根挂在看不到的 rootfs 上（Linux 上它的编号不在列表里），写 0
            parent: m.parent().unwrap_or(0),
            dev: m.dev,
            fs_root: ps.vfs.root_path(ps, &m),
            point,
            flags: m.flags,
            fstype: m.fs.type_name(),
            source: if m.source.is_empty() {
                b"none"
            } else {
                &m.source
            },
            options: ps.vfs.sb_options(&m),
        }
        .write(&mut out, info);
    }
    out
}

/// mounts、mountinfo 里的一行。
struct MountLine<'a> {
    id: crate::vfs::MountId,
    parent: crate::vfs::MountId,
    dev: u64,
    /// 挂载看到的部分在文件系统里的路径。
    fs_root: Vec<u8>,
    point: Vec<u8>,
    flags: u64,
    fstype: &'static str,
    source: &'a [u8],
    /// 超级块的选项（[`crate::vfs::Vfs::sb_options`]）。
    options: String,
}

impl MountLine<'_> {
    fn write(&self, out: &mut Vec<u8>, info: bool) {
        let opts = mount_options(self.flags);
        let (fstype, options) = (self.fstype, &self.options);
        if info {
            let (id, parent) = (self.id, self.parent);
            let (major, minor) = (self.dev >> 8, self.dev & 0xff);
            out.extend_from_slice(format!("{id} {parent} {major}:{minor} ").as_bytes());
            out.extend(mangle(&self.fs_root));
            out.push(b' ');
            out.extend(mangle(&self.point));
            out.extend_from_slice(format!(" {opts} - {fstype} ").as_bytes());
            out.extend(mangle(self.source));
            out.extend_from_slice(format!(" rw{options}\n").as_bytes());
        } else {
            out.extend(mangle(self.source));
            out.push(b' ');
            out.extend(mangle(&self.point));
            out.extend_from_slice(format!(" {fstype} {opts}{options} 0 0\n").as_bytes());
        }
    }
}

/// 挂载的选项（mountinfo 的第六列，Linux 的 `show_mnt_opts`）。
fn mount_options(flags: u64) -> String {
    use crate::vfs::ms;
    let mut s = String::from(if flags & ms::RDONLY != 0 { "ro" } else { "rw" });
    for (bit, name) in [
        (ms::NOSUID, ",nosuid"),
        (ms::NODEV, ",nodev"),
        (ms::NOEXEC, ",noexec"),
        (ms::NOATIME, ",noatime"),
        (ms::NODIRATIME, ",nodiratime"),
        (ms::RELATIME, ",relatime"),
        (ms::NOSYMFOLLOW, ",nosymfollow"),
    ] {
        if flags & bit != 0 {
            s += name;
        }
    }
    s
}

/// 与 Linux 的 `mangle` 一样，把空格、制表符、换行和反斜杠写成八进制的转义。
fn mangle(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for &b in s {
        match b {
            b' ' | b'\t' | b'\n' | b'\\' => out.extend_from_slice(format!("\\{b:03o}").as_bytes()),
            _ => out.push(b),
        }
    }
    out
}

/// `/proc/cpuinfo` 里 HWCAP 各位的名字（Linux 6.1 的 `hwcap_str`），按位的顺序。
const HWCAP_NAMES: [&str; 30] = [
    "fp", "asimd", "evtstrm", "aes", "pmull", "sha1", "sha2", "crc32", "atomics", "fphp",
    "asimdhp", "cpuid", "asimdrdm", "jscvt", "fcma", "lrcpc", "dcpop", "sha3", "sm3", "sm4",
    "asimddp", "sha512", "sve", "asimdfhm", "dit", "uscat", "ilrcpc", "flagm", "ssbs", "sb",
];

/// 同上，HWCAP2 各位的名字，排在 HWCAP 的后面。
const HWCAP2_NAMES: [&str; 15] = [
    "dcpodp",
    "sve2",
    "sveaes",
    "svepmull",
    "svebitperm",
    "svesha3",
    "svesm4",
    "flagm2",
    "frint",
    "svei8mm",
    "svef32mm",
    "svef64mm",
    "svebf16",
    "i8mm",
    "bf16",
];

fn cpuinfo() -> String {
    let named = |names: &'static [&'static str], caps: u64| {
        names
            .iter()
            .enumerate()
            .filter(move |(i, _)| caps & (1 << i) != 0)
            .map(|(_, n)| *n)
    };
    let features: Vec<&str> = named(&HWCAP_NAMES, appine_engine::guest_hwcap())
        .chain(named(&HWCAP2_NAMES, appine_engine::guest_hwcap2()))
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
/// socket 所在的网络命名空间的编号。
fn socket_net(s: &crate::net::Socket) -> u64 {
    use crate::net::Socket;
    let net = match s {
        Socket::Inet(i) => i.net.as_ref(),
        Socket::Netlink(n) => n.net.as_ref(),
        Socket::Unix(u) => u.net.as_ref(),
    };
    net.map_or(crate::nsproxy::INIT_NET_NS, |n| n.id)
}

pub(crate) fn sockets(k: &Kernel) -> Vec<(SocketRef, u32)> {
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

/// `/proc/net/dev`：宿主的接口，计数是 guest 自己收发的流量（[`crate::traffic`]），格式与
/// Linux 的 `dev_seq_printf_stats` 相同。
/// `/proc/net/dev`：接口 `ifaces` 的流量计数（`counted` 为假时都是 0）。
fn net_dev(k: &Kernel, ifaces: &[appine_hal::net::Interface], counted: bool) -> String {
    let mut s = String::from(
        "Inter-|   Receive                                                |  Transmit\n \
         face |bytes    packets errs drop fifo frame compressed multicast|\
         bytes    packets errs drop fifo colls carrier compressed\n",
    );
    for i in ifaces {
        let st = if counted {
            k.traffic.get(&i.name)
        } else {
            Default::default()
        };
        writeln!(
            s,
            "{:>6}: {:>7} {:>7} {:>4} {:>4} {:>4} {:>5} {:>10} {:>9} {:>8} {:>7} {:>4} {:>4} {:>4} \
             {:>5} {:>7} {:>10}",
            i.name,
            st.rx_bytes,
            st.rx_packets,
            0,
            0,
            0,
            0,
            0,
            0,
            st.tx_bytes,
            st.tx_packets,
            0,
            0,
            0,
            0,
            0,
            0
        )
        .unwrap();
    }
    s
}

/// `/proc/net/if_inet6`：宿主接口上的 IPv6 地址，格式与 Linux 的 `if6_seq_show` 相同。
/// OpenJDK 靠它判断有没有 IPv6（`IPv6_supported`），没有这个文件或者是空的就整个关掉
/// IPv6。scope 按 Linux 的 `ipv6_addr_scope` 从地址看；标志与 netlink 的回答一样都是
/// `IFA_F_PERMANENT`（getifaddrs 分不出临时地址）。
fn net_if_inet6(ifaces: &[appine_hal::net::Interface]) -> String {
    let mut s = String::new();
    for i in ifaces {
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
pub(crate) fn content(ps: &View<'_>, mnt: MountId, node: &PseudoNode) -> Result<Vec<u8>, Errno> {
    use PseudoNode::*;
    let ns = ps.pid_ns(mnt);
    let s = match node {
        Global(g) => match g {
            self::Global::Cpuinfo => cpuinfo(),
            self::Global::Meminfo => meminfo(),
            self::Global::Stat => global_stat(ps),
            // 与 Linux 的 uptime_proc_show 一样截断到百分之一秒（四舍五入的话 x.996 秒会写成
            // 下一秒，比随后 sysinfo 报告的还大）
            self::Global::Uptime => {
                let hundredths =
                    |ns: u64| format!("{}.{:02}", ns / 1_000_000_000, ns / 10_000_000 % 100);
                // 空闲时间不受 time 命名空间影响（Linux 的 uptime_proc_show）
                let idle = uptime_ns() * cpus() as u64;
                let up = ps.with_boot_offset(uptime_ns());
                format!("{} {}\n", hundredths(up), hundredths(idle))
            }
            self::Global::Loadavg => crate::loadavg::proc_loadavg(ps.k, ps.last_pid()),
            self::Global::Version => {
                "Linux version 6.1.0-appine (appine@localhost) #1 SMP PREEMPT appine\n".to_owned()
            }
            self::Global::Filesystems => {
                "nodev\tsysfs\nnodev\ttmpfs\n\text4\nnodev\tproc\nnodev\tdevpts\nnodev\tcgroup2\nnodev\tmqueue\n"
                    .to_owned()
            }
            self::Global::Cmdline => "appine\n".to_owned(),
            self::Global::Cgroups => crate::cgroup::proc_cgroups(ps.k),
            self::Global::Kmsg => String::new(),
            self::Global::Mounts | self::Global::Net => return Err(Errno::EINVAL),
        },
        // 那个进程所在的网络命名空间的：接口是宿主的（新建的里面是 lo）加上虚拟设备
        // （crate::netdev），socket 只列建在里面的；新建的里面流量和协议的计数都是 0
        // （crate::netns）
        Net(pid, tid, name) => {
            let full = ps
                .task(*pid, *tid)
                .map_or_else(|| ps.k.init_ns.net.clone(), |t| t.ns.borrow().net.clone());
            let net = Some(full.clone()).filter(|n| n.host_net.is_some());
            let id = net.as_ref().map_or(crate::nsproxy::INIT_NET_NS, |n| n.id);
            // uid 列按读者的用户命名空间换算（Linux 的 seq_user_ns）
            let reader = ps.reader();
            let socks: Vec<(SocketRef, u32)> = sockets(ps.k)
                .into_iter()
                .filter(|(s, _)| socket_net(&s.borrow()) == id)
                .map(|(s, uid)| (s, reader.uid_out(uid)))
                .collect();
            let ifaces = crate::netdev::interfaces(&full);
            let world = ps.k.world();
            let quiet = crate::traffic::Protocols::default();
            let protocols = if net.is_some() {
                &quiet
            } else {
                &ps.k.traffic.protocols
            };
            let tables = || {
                let host = net.is_none().then(|| crate::route::host_routes(ps.k)).flatten();
                crate::netdev::tables(&full, host)
            };
            match &name[..] {
                b"arp" => crate::net::PROC_ARP.to_owned(),
                b"dev" => net_dev(ps.k, &ifaces, net.is_none()),
                b"netstat" => crate::net::proc_netstat(protocols),
                b"snmp" => crate::net::proc_snmp(&socks, protocols),
                b"if_inet6" => net_if_inet6(&ifaces),
                b"route" => crate::route::proc_route(&tables()),
                b"ipv6_route" => crate::route::proc_ipv6_route(&tables()),
                b"sockstat" => crate::net::sockstat(&socks, false),
                b"sockstat6" => crate::net::sockstat(&socks, true),
                b"tcp" => crate::net::proc_listing(&socks, AF_INET, Table::Tcp, &ps.k.portmap, &world),
                b"tcp6" => crate::net::proc_listing(&socks, AF_INET6, Table::Tcp, &ps.k.portmap, &world),
                b"udp" => crate::net::proc_listing(&socks, AF_INET, Table::Udp, &ps.k.portmap, &world),
                b"udp6" => crate::net::proc_listing(&socks, AF_INET6, Table::Udp, &ps.k.portmap, &world),
                b"icmp" => crate::net::proc_listing(&socks, AF_INET, Table::Icmp, &ps.k.portmap, &world),
                b"icmp6" => crate::net::proc_listing(&socks, AF_INET6, Table::Icmp, &ps.k.portmap, &world),
                b"raw" => crate::net::proc_listing(&socks, AF_INET, Table::Raw, &ps.k.portmap, &world),
                b"raw6" => crate::net::proc_listing(&socks, AF_INET6, Table::Raw, &ps.k.portmap, &world),
                b"unix" => crate::unix::proc_listing(&socks),
                _ => return Err(Errno::ENOENT),
            }
        }
        Static(path) => match &path[..] {
            b"sys/fs/file-max" => "9223372036854775807\n".to_owned(),
            b"sys/fs/lease-break-time" => format!("{}\n", ps.k.lease_break_time),
            b"sys/fs/aio-max-nr" => format!("{}\n", ps.k.aio_max_nr),
            b"sys/fs/aio-nr" => format!("{}\n", ps.k.aio_nr),
            p if p.starts_with(b"sys/fs/mqueue/") => {
                let ns = ps.ns().ipc.id;
                format!("{}\n", ps.k.ipc(ns).mq_limits.get(&p[14..]).unwrap_or(0))
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
            b"sys/net/core/somaxconn" => {
                format!("{}\n", ps.ns().net.devs.borrow().conf.somaxconn)
            }
            b"sys/net/ipv4/ip_forward" => format!("{}\n", crate::netdev::ip_forward(&ps.ns().net)),
            b"sys/net/ipv4/ip_local_port_range" => {
                let (lo, hi) = ps.ns().net.devs.borrow().conf.port_range;
                format!("{lo}\t{hi}\n")
            }
            b"sys/net/ipv4/ip_unprivileged_port_start" => {
                format!("{}\n", ps.ns().net.devs.borrow().conf.prot_sock)
            }
            b"sys/net/ipv4/ping_group_range" => {
                let (lo, hi) = ps.ns().net.devs.borrow().conf.ping_group;
                format!("{lo}\t{hi}\n")
            }
            b"sys/kernel/core_pattern" => {
                format!("{}\n", String::from_utf8_lossy(&ps.k.core_pattern))
            }
            b"sys/kernel/core_uses_pid" => format!("{}\n", ps.k.core_uses_pid as u8),
            b"sys/kernel/domainname" => {
                format!(
                    "{}\n",
                    String::from_utf8_lossy(&ps.ns().uts.domainname.borrow())
                )
            }
            b"sys/kernel/hostname" => {
                format!(
                    "{}\n",
                    String::from_utf8_lossy(&ps.ns().uts.hostname.borrow())
                )
            }
            b"sys/kernel/sched_rr_timeslice_ms" => "100\n".to_owned(),
            b"sys/kernel/msgmax" | b"sys/kernel/msgmnb" | b"sys/kernel/msgmni"
            | b"sys/kernel/sem" | b"sys/kernel/shmall" | b"sys/kernel/shmmax"
            | b"sys/kernel/shmmni" => {
                let ns = ps.ns().ipc.id;
                ps.k.ipc(ns).tunables.read(&path[11..]).unwrap()
            }
            b"sysvipc/msg" => ipc::proc_listing(ps.k, ps.ns().ipc.id, b"msg", &ps.reader()),
            b"sysvipc/sem" => ipc::proc_listing(ps.k, ps.ns().ipc.id, b"sem", &ps.reader()),
            b"sysvipc/shm" => ipc::proc_listing(ps.k, ps.ns().ipc.id, b"shm", &ps.reader()),
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
            b"sys/kernel/ns_last_pid" => format!("{}\n", ps.last_pid()),
            b"sys/kernel/seccomp/actions_avail" => crate::seccomp::actions_avail(),
            b"sys/kernel/seccomp/actions_logged" => crate::seccomp::actions_logged(ps.k),
            b"sys/kernel/osrelease" => "6.1.0-appine\n".to_owned(),
            b"sys/kernel/ostype" => "Linux\n".to_owned(),
            // 映射不到的用户、组 ID 显示成的 ID（Linux 的默认值，bubblewrap 启动时读它）。
            // 不能改
            b"sys/kernel/overflowuid"
            | b"sys/kernel/overflowgid"
            | b"sys/fs/overflowuid"
            | b"sys/fs/overflowgid" => "65534\n".to_owned(),
            b"sys/kernel/pid_max" => "4194304\n".to_owned(),
            b"sys/kernel/dmesg_restrict" => format!("{}\n", ps.k.dmesg_restrict as u8),
            b"sys/kernel/printk" => {
                let [a, b, c, d] = ps.k.printk;
                format!("{a}\t{b}\t{c}\t{d}\n")
            }
            b"sys/kernel/printk_devkmsg" => format!("{}\n", ps.k.klog.borrow().devkmsg.name()),
            // 每个用户能建的各种命名空间个数的上限：与 Linux 的默认值一样是 threads-max 的
            // 一半，只读，不实际限制
            p if p.starts_with(b"sys/user/max_") => {
                let v = ps.k.max_namespaces.get(p).copied();
                format!("{}\n", v.unwrap_or(ps.k.threads_max as u64 / 2))
            }
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
                self::Entry::Stat => pid_stat(ps, &ns, *pid, *tid)?,
                self::Entry::Status => pid_status(ps, &ns, *pid, *tid)?,
                self::Entry::Statm => {
                    let (vsize, rss) = mem_of(task.as_deref());
                    format!("{} {rss} 0 0 0 {rss} 0\n", vsize / 4096)
                }
                self::Entry::Comm => format!("{}\n", comm_str(task.as_deref(), *pid)),
                // 线程建的、还没有被回收的子进程，每个后面一个空格，没有换行（Linux 的
                // children_seq_show）
                self::Entry::Children => {
                    let g = ps.group(*pid).ok_or(Errno::ENOENT)?;
                    let me = tid.unwrap_or(*pid);
                    let mut s = String::new();
                    for c in &g.borrow().children {
                        if ps
                            .k
                            .groups
                            .get(c)
                            .is_some_and(|g| g.borrow().parent_thread == me)
                        {
                            write!(s, "{} ", ns.vnr(*c).unwrap_or(0)).unwrap();
                        }
                    }
                    s
                }
                // 相对读者所在的 cgroup 命名空间的根
                self::Entry::Cgroup => {
                    let g = ps.group(*pid).ok_or(Errno::ESRCH)?;
                    let cgroup = g.borrow().cgroup;
                    crate::cgroup::proc_cgroup(ps.k, cgroup, ps.cgroup_root())
                }
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
                self::Entry::Maps => task.map_or(String::new(), |t| maps(ps.vfs.store(), &t)),
                self::Entry::Smaps => {
                    task.map_or(String::new(), |t| smaps(ps.vfs.store(), &t, false))
                }
                self::Entry::SmapsRollup => {
                    task.map_or(String::new(), |t| smaps(ps.vfs.store(), &t, true))
                }
                // 二进制，按读写的位置现算（`read_pagemap`、`read_mem`、`write_mem`）
                self::Entry::Pagemap | self::Entry::Mem => String::new(),
                self::Entry::Auxv => {
                    let auxv =
                        task.map_or_else(Vec::new, |t| t.res.borrow().mm.borrow().auxv.to_vec());
                    return Ok(auxv.iter().flat_map(|w| w.to_le_bytes()).collect());
                }
                self::Entry::Mounts | self::Entry::MountInfo => {
                    let (root, ns) = task.map_or((Node::ROOT, crate::vfs::INIT_MNT_NS), |t| {
                        let res = t.res.borrow();
                        let fs = res.fs.borrow();
                        (fs.root.clone(), fs.mnt_ns.id)
                    });
                    return Ok(mounts(ps, &root, ns, *e == self::Entry::MountInfo));
                }
                self::Entry::OomScoreAdj => {
                    let g = ps.group(*pid).ok_or(Errno::ESRCH)?;
                    format!("{}\n", g.borrow().oom_score_adj)
                }
                // Linux 的 proc_timens_show_offsets：给子进程用的那个
                self::Entry::TimensOffsets => {
                    let t = ps.k.tasks.get(pid).ok_or(Errno::ESRCH)?;
                    let [mono, boot] = t.ns.borrow().time_for_children.offsets.get();
                    format!(
                        "{:<10} {:>10} {:>9}\n{:<10} {:>10} {:>9}\n",
                        "monotonic", mono.0, mono.1, "boottime", boot.0, boot.1
                    )
                }
                e @ (self::Entry::UidMap | self::Entry::GidMap | self::Entry::ProjidMap) => {
                    let g = ps.group(*pid).ok_or(Errno::ESRCH)?;
                    let ns = g.borrow().cred.user_ns.clone();
                    id_map(&ps.reader(), &ns, *e)
                }
                self::Entry::Setgroups => {
                    let g = ps.group(*pid).ok_or(Errno::ESRCH)?;
                    let allow = g.borrow().cred.user_ns.setgroups.get();
                    format!("{}\n", if allow { "allow" } else { "deny" })
                }
                self::Entry::CoredumpFilter => {
                    let g = ps.group(*pid).ok_or(Errno::ESRCH)?;
                    format!("{:08x}\n", g.borrow().coredump_filter)
                }
                _ => return Err(Errno::EINVAL),
            }
        }
        FdInfo(pid, fd) => fdinfo(ps, &ns, *pid, *fd)?,
        Sys(path) => match &path[..] {
            b"devices/system/cpu/online"
            | b"devices/system/cpu/possible"
            | b"devices/system/cpu/present" => cpu_list(0..cpus()),
            b"devices/system/cpu/offline" => "\n".to_owned(),
            _ => return Err(Errno::EISDIR),
        },
        SysCpu(n, path) => cpu_file(*n as usize, path)?,
        SysNet(n, path) => net_file(ps.k, &ps.ns().net, *n, path)?,
        NetConf(v6, dir, name) => {
            crate::netdev::conf_read(&ps.ns().net, *v6, *dir, name)?
        }
        NetSysctl(path) => crate::netconf::sysctl_read(&ps.ns().net.devs.borrow().conf, path),
        // 只能写的（cgroup.kill）读是 EINVAL
        Cgroup(_, b"") => return Err(Errno::EISDIR),
        Cgroup(id, name) => crate::cgroup::file(ps.k, *id, name).ok_or(Errno::EINVAL)?,
        // 打开命名空间只为 setns，读不出东西
        NsFile(..) => String::new(),
        _ => return Err(Errno::EINVAL),
    };
    Ok(s.into_bytes())
}

/// `/proc/<pid>/uid_map`、`gid_map`、`projid_map`（Linux 的 `uid_m_show`）：每段
/// "命名空间里的起点 上一层的起点 个数"，各占 10 列。第二列按读者的命名空间换算；读者就在
/// 这个命名空间里时按上一层的。映射不到写 4294967295。projid_map 只有开机时的命名空间有。
fn id_map(reader: &Cred, ns: &Rc<crate::cred::UserNs>, e: Entry) -> String {
    let lower = match &ns.parent {
        Some(parent) if reader.user_ns.id == ns.id => parent.clone(),
        _ => reader.user_ns.clone(),
    };
    let (map, lower_map) = match e {
        Entry::GidMap => (ns.gid_map.borrow().clone(), lower.gid_map.borrow().clone()),
        Entry::UidMap => (ns.uid_map.borrow().clone(), lower.uid_map.borrow().clone()),
        _ if ns.level == 0 => return format!("{:>10} {:>10} {:>10}\n", 0, 0, u32::MAX),
        _ => return String::new(),
    };
    map.extents()
        .iter()
        .map(|&(first, k, count)| {
            let shown = lower_map.up(k).unwrap_or(u32::MAX);
            format!("{first:>10} {shown:>10} {count:>10}\n")
        })
        .collect()
}

/// 写 `/proc/<pid>/uid_map`、`gid_map`（Linux 的 `map_write`）：只能写一次（EPERM），写的
/// 进程要在那个用户命名空间里有 `CAP_SYS_ADMIN`（EPERM），内容不对是 EINVAL；然后看
/// `new_idmap_permitted`：建立者把自己的有效 ID 映射成一段一个（gid_map 要先把 setgroups
/// 写成 deny），或者在上一层命名空间里有 `CAP_SETUID`/`CAP_SETGID`，否则 EPERM。第二列在
/// 上一层里映射不到也是 EPERM。Linux 按打开文件的凭据检查前两条，这里按写的进程的。
fn write_id_map(k: &Kernel, writer: &Cred, pid: Tid, gid: bool, data: &[u8]) -> Result<(), Errno> {
    let g = k.groups.get(&pid).ok_or(Errno::ESRCH)?;
    let ns = g.borrow().cred.user_ns.clone();
    let map = if gid { &ns.gid_map } else { &ns.uid_map };
    if !map.borrow().extents().is_empty() || !writer.ns_capable(&ns, cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    let new = crate::cred::IdMap::parse(data)?;
    let parent = ns.parent.clone().ok_or(Errno::EPERM)?;
    let parent_map = if gid {
        parent.gid_map.borrow().clone()
    } else {
        parent.uid_map.borrow().clone()
    };
    let own = match new.extents() {
        &[(_, lower, 1)] if ns.owner == writer.euid => {
            if gid {
                !ns.setgroups.get() && parent_map.down(lower) == Some(writer.egid)
            } else {
                parent_map.down(lower) == Some(writer.euid)
            }
        }
        _ => false,
    };
    let setid = if gid { cap::SETGID } else { cap::SETUID };
    if !own && !writer.ns_capable(&parent, setid) {
        return Err(Errno::EPERM);
    }
    let kernel = new.lower_to_kernel(&parent_map)?;
    *map.borrow_mut() = kernel;
    Ok(())
}

/// 写 `/proc/<pid>/timens_offsets`（Linux 6.1 的 `timens_offsets_write`、
/// `proc_timens_set_offset`）：一两行 `<时钟> <秒> <纳秒>`，时钟是 `monotonic`、`boottime` 或者
/// 它们的编号 1、7。写进那个进程给子进程用的 time 命名空间：要在它的属主里有 `CAP_SYS_TIME`
/// （EPERM，Linux 按打开文件时的凭据，这里按写的进程的），加上偏移之后的时钟要在
/// [0, KTIME_SEC_MAX / 2] 里（ERANGE），已经有进程进去过是 EACCES。
fn write_timens_offsets(k: &Kernel, cred: &Cred, pid: Tid, data: &[u8]) -> Result<(), Errno> {
    const KTIME_SEC_MAX: i64 = i64::MAX / 1_000_000_000;
    if data.len() >= 4096 {
        return Err(Errno::EINVAL);
    }
    let text = std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?;
    let mut offsets = Vec::new();
    for line in text.split_terminator('\n') {
        let mut words = line.split_ascii_whitespace();
        let (Some(clock), Some(sec), Some(nsec)) = (words.next(), words.next(), words.next())
        else {
            return Err(Errno::EINVAL);
        };
        let sec: i64 = sec.parse().map_err(|_| Errno::EINVAL)?;
        let nsec: u64 = nsec.parse().map_err(|_| Errno::EINVAL)?;
        if nsec >= 1_000_000_000 {
            return Err(Errno::EINVAL);
        }
        let index = match clock {
            "monotonic" | "1" => 0,
            "boottime" | "7" => 1,
            _ => return Err(Errno::EINVAL),
        };
        offsets.push((index, (sec, nsec as i64)));
        if offsets.len() == 2 {
            break;
        }
    }
    let task = k.tasks.get(&pid).ok_or(Errno::ESRCH)?;
    let ns = task.ns.borrow().time_for_children.clone();
    if !cred.ns_capable(&ns.owner, cap::SYS_TIME) {
        return Err(Errno::EPERM);
    }
    for &(index, (sec, nsec)) in &offsets {
        let clock = [1, appine_hal::CLOCK_BOOTTIME][index];
        let (now, now_ns) = appine_hal::clock_gettime(clock).map_err(Errno)?;
        if !(-KTIME_SEC_MAX..=KTIME_SEC_MAX).contains(&sec) {
            return Err(Errno::ERANGE);
        }
        let total = now + sec + (now_ns + nsec) / 1_000_000_000;
        if !(0..=KTIME_SEC_MAX / 2).contains(&total) {
            return Err(Errno::ERANGE);
        }
    }
    if ns.frozen.get() {
        return Err(Errno::EACCES);
    }
    let mut all = ns.offsets.get();
    for (index, value) in offsets {
        all[index] = value;
    }
    ns.offsets.set(all);
    Ok(())
}

/// 写 `/proc/<pid>/setgroups`（Linux 的 `proc_setgroups_write`）："allow" 或 "deny"，后面
/// 只能有空白（EINVAL）；要在那个用户命名空间里有 `CAP_SYS_ADMIN`。禁止了之后不能再允许，
/// gid_map 写过之后不能禁止（EPERM）。
fn write_setgroups(k: &Kernel, writer: &Cred, pid: Tid, data: &[u8]) -> Result<(), Errno> {
    let g = k.groups.get(&pid).ok_or(Errno::ESRCH)?;
    let ns = g.borrow().cred.user_ns.clone();
    if !writer.ns_capable(&ns, cap::SYS_ADMIN) {
        return Err(Errno::EPERM);
    }
    let (allow, rest) = if let Some(rest) = data.strip_prefix(b"allow") {
        (true, rest)
    } else if let Some(rest) = data.strip_prefix(b"deny") {
        (false, rest)
    } else {
        return Err(Errno::EINVAL);
    };
    if !rest.iter().all(u8::is_ascii_whitespace) {
        return Err(Errno::EINVAL);
    }
    if allow {
        if !ns.setgroups.get() {
            return Err(Errno::EPERM);
        }
    } else {
        if !ns.gid_map.borrow().extents().is_empty() {
            return Err(Errno::EPERM);
        }
        ns.setgroups.set(false);
    }
    Ok(())
}

/// `/sys/class/net/<接口>` 下的文件，接口按读者所在网络命名空间里的编号（与 Linux 不同：
/// Linux 看的是挂载 sysfs 的那个进程所在的）。接口的类型与 netlink 的回答一样只分回环和
/// 以太网；`flags` 与 Linux 一样是 `dev->flags`，不含 `IFF_RUNNING` 这些按状态算出来的位。
/// 虚拟设备（[`crate::netdev`]）的 `iflink` 是 veth 的对端，`operstate` 可以是
/// lowerlayerdown。
fn net_file(
    k: &Kernel,
    ns: &crate::nsproxy::NetNs,
    index: u32,
    path: &[u8],
) -> Result<String, Errno> {
    const IFF_UP: u32 = 0x1;
    const IFF_LOOPBACK: u32 = 0x8;
    const IFF_RUNNING: u32 = 0x40;
    // IFF_RUNNING、IFF_LOWER_UP、IFF_DORMANT、IFF_ECHO（dev_get_flags 才加上它们）
    const STATE_FLAGS: u32 = 0x40 | 0x1_0000 | 0x2_0000 | 0x4_0000;
    let virt = crate::netdev::views(ns)
        .into_iter()
        .find(|v| v.index == index);
    if let Some(v) = virt {
        let mac = |m: [u8; 6]| {
            let hex: Vec<String> = m.iter().map(|b| format!("{b:02x}")).collect();
            format!("{}\n", hex.join(":"))
        };
        return Ok(match path {
            b"addr_len" => "6\n".to_owned(),
            b"address" => mac(v.mac),
            b"broadcast" => mac([0xff; 6]),
            b"carrier" if v.flags & IFF_UP == 0 => return Err(Errno::EINVAL),
            b"carrier" => format!("{}\n", u8::from(v.carrier)),
            b"flags" => format!("{:#x}\n", v.flags & !STATE_FLAGS),
            b"ifindex" => format!("{index}\n"),
            b"iflink" => format!("{}\n", v.link.as_ref().map_or(index, |l| l.0)),
            b"mtu" => format!("{}\n", v.mtu),
            b"operstate" => match v.operstate {
                0 => "unknown\n",
                3 => "lowerlayerdown\n",
                6 => "up\n",
                _ => "down\n",
            }
            .to_owned(),
            b"tx_queue_len" => format!("{}\n", v.txqlen),
            b"type" => "1\n".to_owned(),
            // bridge 的参数都是默认值，STP 关着（crate::netdev::bridge_info）
            b"bridge/ageing_time" => "30000\n".to_owned(),
            b"bridge/forward_delay" => "1500\n".to_owned(),
            b"bridge/hello_time" => "200\n".to_owned(),
            b"bridge/max_age" => "2000\n".to_owned(),
            b"bridge/priority" => "32768\n".to_owned(),
            b"bridge/stp_state" => "0\n".to_owned(),
            b"bridge/bridge_id" | b"bridge/root_id" => {
                let hex: String = v.mac.iter().map(|b| format!("{b:02x}")).collect();
                format!("8000.{hex}\n")
            }
            b"brport/state" => format!("{}\n", v.port_state.unwrap_or(0)),
            b"brport/priority" => "32\n".to_owned(),
            b"brport/path_cost" => "2\n".to_owned(),
            p if p.starts_with(b"statistics/") && !p.ends_with(b"/") => "0\n".to_owned(),
            _ => return Err(Errno::EISDIR),
        });
    }
    // 接口没了：Linux 上目录随之消失，已经打开的文件读出 ENODEV
    let i = crate::netdev::base(ns)
        .into_iter()
        .find(|i| i.index == index)
        .ok_or(Errno::ENODEV)?;
    let loopback = i.flags & IFF_LOOPBACK != 0;
    let mac = |m: [u8; 6]| {
        let hex: Vec<String> = m.iter().map(|b| format!("{b:02x}")).collect();
        format!("{}\n", hex.join(":"))
    };
    let st = k.traffic.get(&i.name);
    Ok(match path {
        b"addr_len" => "6\n".to_owned(),
        b"address" => mac(i.mac.unwrap_or([0; 6])),
        b"broadcast" => mac(if loopback { [0; 6] } else { [0xff; 6] }),
        // Linux 上停用的接口读 carrier 是 EINVAL
        b"carrier" if i.flags & IFF_UP == 0 => return Err(Errno::EINVAL),
        b"carrier" => format!("{}\n", u8::from(i.flags & IFF_RUNNING != 0)),
        b"flags" => format!("{:#x}\n", i.flags & !STATE_FLAGS),
        b"ifindex" | b"iflink" => format!("{index}\n"),
        b"mtu" => format!("{}\n", i.mtu),
        b"operstate" => match crate::net::operstate(&i) {
            0 => "unknown\n",
            6 => "up\n",
            _ => "down\n",
        }
        .to_owned(),
        b"tx_queue_len" => format!(
            "{}\n",
            appine_hal::net::tx_queue_len(&i.name).unwrap_or(1000)
        ),
        b"type" => format!("{}\n", if loopback { 772 } else { 1 }),
        b"statistics/rx_bytes" => format!("{}\n", st.rx_bytes),
        b"statistics/rx_packets" => format!("{}\n", st.rx_packets),
        b"statistics/tx_bytes" => format!("{}\n", st.tx_bytes),
        b"statistics/tx_packets" => format!("{}\n", st.tx_packets),
        p if p.starts_with(b"statistics/") && !p.ends_with(b"/") => "0\n".to_owned(),
        _ => return Err(Errno::EISDIR),
    })
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

/// `/proc/<pid>/fdinfo/<n>`：位置、标志（加上 close-on-exec；open 打开的带着
/// `O_LARGEFILE`）、挂载编号、inode，以及 eventfd、inotify 的附加行。
fn fdinfo(ps: &View<'_>, ns: &PidNs, pid: Tid, fd: i32) -> Result<String, Errno> {
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
    let mnt_id = f.node().map_or(crate::vfs::ANON_MOUNT, |n| n.mnt());
    let mut s = format!(
        "pos:\t{}\nflags:\t0{:o}\nmnt_id:\t{mnt_id}\nino:\t{}\n",
        f.pos,
        f.flags | cloexec,
        f.ino().unwrap_or(0)
    );
    match &f.kind {
        FileKind::EventFd(e) => s += &format!("eventfd-count: {:16x}\n", e.borrow().count),
        FileKind::Inotify(i) => s += &i.borrow().fdinfo(),
        // 挂载的 pid 命名空间里的编号（看不到时是 0），NSpid 接着列出更下层的
        FileKind::Pidfd(p) => {
            let nr = if p.exited() {
                -1
            } else {
                ns.vnr(p.tgid).unwrap_or(0)
            };
            s += &format!("Pid:\t{nr}\nNSpid:\t{nr}");
            if nr > 0 {
                let levels = ps.k.pid_ns_of(p.tgid).levels_from(ns.level + 1);
                for n in levels {
                    s += &format!("\t{}", n.vnr(p.tgid).unwrap_or(0));
                }
            }
            s.push('\n');
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

    /// 写数字的 /proc 文件：到 NUL 为止、只看前 12 个字节，基数 0，超出 32 位是 ERANGE。
    #[test]
    fn proc_numbers() {
        let num = |d: &[u8], signed| kstrto(proc_number(d)?, signed);
        assert_eq!(num(b"0\0", true), Ok(0));
        assert_eq!(num(b"-500\0junk", true), Ok(-500));
        assert_eq!(num(b" 12\n", true), Ok(12));
        assert_eq!(num(b"+0x1f", false), Ok(31));
        assert_eq!(num(b"017", false), Ok(15));
        assert_eq!(num(b"0", false), Ok(0));
        assert_eq!(num(b"-1", false), Err(Errno::EINVAL));
        assert_eq!(num(b"-2147483648", true), Ok(-(1 << 31)));
        assert_eq!(num(b"2147483648", true), Err(Errno::ERANGE));
        assert_eq!(num(b"4294967296", false), Err(Errno::ERANGE));
        assert_eq!(num(b"08", false), Err(Errno::EINVAL));
        assert_eq!(num(b"0x", false), Err(Errno::EINVAL));
        assert_eq!(num(b"", true), Err(Errno::EINVAL));
        assert_eq!(num(b"1 2", true), Err(Errno::EINVAL));
        // 第 13 个字节起不看
        assert_eq!(num(b"000000000001x", false), Ok(1));
    }

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
