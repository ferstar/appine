//! `/sys/fs/cgroup`（设计文档 §8.2）：一个只读的 cgroup v2 视图，让运行时知道 App 的内存
//! 上限。
//!
//! JVM 的默认堆是物理内存的 1/4，Node.js 按物理内存给 V8 定堆的上限，而 guest 能用的只有
//! OOM killer 的上限（默认物理内存的一半，[`crate::oom`]），几个运行时一起用满就会被结束。
//! 它们在容器里改按 cgroup 的 `memory.max` 算：JVM 的容器支持（UseContainerSupport），Node.js
//! 经由 libuv 的 `uv_get_constrained_memory`。
//!
//! 所有 guest 进程都在同一个 cgroup 里，看起来与 docker 用 cgroup 命名空间的容器一样：
//! `/proc/<pid>/cgroup` 是 `0::/`，cgroup2 只读地挂在 `/sys/fs/cgroup`，控制器有 cpuset、cpu、
//! memory、pids。
//!
//! - `memory.max` 是 OOM killer 的上限，`memory.current` 是它量的 App 的内存（宿主的匿名内存，
//!   换出到 zram 的也算在里面，所以 `memory.swap.max` 是 0），`memory.stat` 把它分成匿名内存和
//!   共享内存，`memory.events` 记着超出上限和结束进程的次数；
//! - 没有 CPU 配额和权重：`cpu.max` 是 `max 100000`，cpuset 是 guest 看到的所有 CPU；
//! - `pids.current` 是 guest 的线程数，`pids.max` 不限（限制在 threads-max 和 `RLIMIT_NPROC`）。
//!
//! 只读：不能写这些文件，也不能建子 cgroup（EROFS），与 docker 默认的挂载一样。

use std::fmt::Write;

use crate::kernel::{INIT, Kernel};

/// `/sys/fs/cgroup` 下的文件，和它在 Linux 上是否可写（决定权限位）。
pub(crate) const FILES: [(&[u8], bool); 38] = [
    (b"cgroup.controllers", false),
    (b"cgroup.events", false),
    (b"cgroup.freeze", true),
    (b"cgroup.max.depth", true),
    (b"cgroup.max.descendants", true),
    (b"cgroup.procs", true),
    (b"cgroup.stat", false),
    (b"cgroup.subtree_control", true),
    (b"cgroup.threads", true),
    (b"cgroup.type", true),
    (b"cpu.idle", true),
    (b"cpu.max", true),
    (b"cpu.max.burst", true),
    (b"cpu.stat", false),
    (b"cpu.weight", true),
    (b"cpu.weight.nice", true),
    (b"cpuset.cpus", true),
    (b"cpuset.cpus.effective", false),
    (b"cpuset.cpus.partition", true),
    (b"cpuset.mems", true),
    (b"cpuset.mems.effective", false),
    (b"memory.current", false),
    (b"memory.events", false),
    (b"memory.events.local", false),
    (b"memory.high", true),
    (b"memory.low", true),
    (b"memory.max", true),
    (b"memory.min", true),
    (b"memory.oom.group", true),
    (b"memory.peak", false),
    (b"memory.stat", false),
    (b"memory.swap.current", false),
    (b"memory.swap.events", false),
    (b"memory.swap.high", true),
    (b"memory.swap.max", true),
    (b"pids.current", false),
    (b"pids.events", false),
    (b"pids.max", true),
];

/// `/proc/<pid>/cgroup`：都在 cgroup 命名空间的根上。
pub(crate) const PROC_CGROUP: &str = "0::/\n";

/// `/proc/cgroups`：cgroup v2 下各个控制器的层级都是 0（JVM 据此认出 cgroup v2）。cpuacct
/// 在 v2 里并进了 cpu，仍然列出。
pub(crate) fn proc_cgroups() -> String {
    let mut s = String::from("#subsys_name\thierarchy\tnum_cgroups\tenabled\n");
    for name in ["cpuset", "cpu", "cpuacct", "memory", "pids"] {
        writeln!(s, "{name}\t0\t1\t1").unwrap();
    }
    s
}

/// `/sys/fs/cgroup/<name>` 的内容。
pub(crate) fn file(k: &Kernel, name: &[u8]) -> Option<String> {
    let max = |v: u64| {
        if v == u64::MAX {
            "max".to_owned()
        } else {
            v.to_string()
        }
    };
    let cpus = appine_hal::host_cpus().len();
    let s = match name {
        b"cgroup.controllers" => "cpuset cpu memory pids".to_owned(),
        // 控制器的列表只在列出了名字时才换行（Linux 的 cgroup_print_ss_mask）
        b"cgroup.subtree_control" => return Some(String::new()),
        b"cgroup.type" => "domain".to_owned(),
        b"cgroup.procs" => list(
            k.groups
                .iter()
                .filter(|&(&tgid, g)| tgid != INIT && !g.borrow().threads.is_empty())
                .map(|(&tgid, _)| tgid),
        ),
        b"cgroup.threads" => list(k.tasks.keys().copied()),
        b"cgroup.events" => format!("populated {}\nfrozen 0", !k.tasks.is_empty() as u8),
        b"cgroup.freeze" | b"cpu.idle" | b"cpu.max.burst" | b"cpu.weight.nice" => "0".to_owned(),
        b"memory.min" | b"memory.low" | b"memory.oom.group" | b"memory.swap.current" => {
            "0".to_owned()
        }
        b"cgroup.max.depth"
        | b"cgroup.max.descendants"
        | b"memory.high"
        | b"memory.swap.high"
        | b"pids.max" => "max".to_owned(),
        b"cgroup.stat" => "nr_descendants 0\nnr_dying_descendants 0".to_owned(),
        b"cpu.max" => "max 100000".to_owned(),
        b"cpu.weight" => "100".to_owned(),
        b"cpu.stat" => {
            let cpu = k.tasks.values().fold(k.cpu, |sum, t| sum + t.cpu());
            format!(
                "usage_usec {}\nuser_usec {}\nsystem_usec {}\nnr_periods 0\nnr_throttled 0\n\
                 throttled_usec 0\nnr_bursts 0\nburst_usec 0",
                cpu.total_ns / 1000,
                cpu.user_ns() / 1000,
                cpu.sys_ns / 1000
            )
        }
        b"cpuset.cpus" | b"cpuset.mems" => String::new(),
        b"cpuset.cpus.effective" if cpus > 1 => format!("0-{}", cpus - 1),
        b"cpuset.cpus.effective" | b"cpuset.mems.effective" => "0".to_owned(),
        b"cpuset.cpus.partition" => "member".to_owned(),
        b"memory.current" => k.oom.measure().to_string(),
        b"memory.peak" => {
            k.oom.measure();
            k.oom.peak.get().to_string()
        }
        b"memory.max" => max(k.oom.limit),
        b"memory.swap.max" => "0".to_owned(),
        b"memory.events" | b"memory.events.local" => {
            let e = k.oom.events.get();
            format!(
                "low 0\nhigh 0\nmax {}\noom {}\noom_kill {}\noom_group_kill 0",
                e.max, e.oom, e.oom_kill
            )
        }
        b"memory.stat" => memory_stat(),
        b"memory.swap.events" => "high 0\nmax 0\nfail 0".to_owned(),
        b"pids.current" => k.tasks.len().to_string(),
        b"pids.events" => "max 0".to_owned(),
        _ => return None,
    };
    Some(s + "\n")
}

/// `memory.stat`，按 Linux 6.1 的 `memory_stat_format`（有 swap 和透明大页、没有 zswap 的
/// 配置），单位是字节。只量得到宿主进程的匿名内存（与 `memory.current` 相同）：私有的和换出的
/// 算 anon，共享的算 shmem（Linux 的 file 包括 shmem），都在 active_anon 里；没有可以回收的
/// 页缓存，inactive_file 是 0。.NET 的 GC 按 `memory.current` 减去 inactive_file 算用量，没有
/// 这个文件就只看自己进程的 RSS。
fn memory_stat() -> String {
    let u = appine_hal::memory_usage().unwrap_or_default();
    let anon = u.anon + u.swap;
    let mut s = String::new();
    let mut put = |name: &str, v: u64| writeln!(s, "{name} {v}").unwrap();
    for (name, v) in [("anon", anon), ("file", u.shmem)] {
        put(name, v);
    }
    for name in [
        "kernel",
        "kernel_stack",
        "pagetables",
        "sec_pagetables",
        "percpu",
        "sock",
        "vmalloc",
    ] {
        put(name, 0);
    }
    put("shmem", u.shmem);
    for name in [
        "file_mapped",
        "file_dirty",
        "file_writeback",
        "swapcached",
        "anon_thp",
        "file_thp",
        "shmem_thp",
        "inactive_anon",
    ] {
        put(name, 0);
    }
    put("active_anon", anon + u.shmem);
    for name in [
        "inactive_file",
        "active_file",
        "unevictable",
        "slab_reclaimable",
        "slab_unreclaimable",
        "slab",
        "workingset_refault_anon",
        "workingset_refault_file",
        "workingset_activate_anon",
        "workingset_activate_file",
        "workingset_restore_anon",
        "workingset_restore_file",
        "workingset_nodereclaim",
        "pgscan",
        "pgsteal",
        "pgscan_kswapd",
        "pgscan_direct",
        "pgsteal_kswapd",
        "pgsteal_direct",
        "pgfault",
        "pgmajfault",
        "pgrefill",
        "pgactivate",
        "pgdeactivate",
        "pglazyfree",
        "pglazyfreed",
        "thp_fault_alloc",
        "thp_collapse_alloc",
    ] {
        put(name, 0);
    }
    s.pop();
    s
}

/// 每行一个编号，从小到大。
fn list(ids: impl Iterator<Item = crate::kernel::Tid>) -> String {
    let mut ids: Vec<_> = ids.collect();
    ids.sort_unstable();
    let lines: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
    lines.join("\n")
}
