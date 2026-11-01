//! cgroup v2（设计文档 §8.2）：`/sys/fs/cgroup` 里的一棵 cgroup 树。
//!
//! 起初是为了让运行时知道 App 的内存上限：JVM 的默认堆是物理内存的 1/4，Node.js 按物理内存
//! 给 V8 定堆的上限，而 guest 能用的只有 OOM killer 的上限（默认物理内存的一半，
//! [`crate::oom`]），几个运行时一起用满就会被结束。它们在容器里改按 cgroup 的 `memory.max`
//! 算：JVM 的容器支持（UseContainerSupport），Node.js 经由 libuv 的
//! `uv_get_constrained_memory`。
//!
//! 根 cgroup 看起来与 docker 用 cgroup 命名空间的容器的根一样，控制器 cpuset、cpu、memory、
//! pids 都开着，也只能写 `cgroup.procs`、`cgroup.threads`、`cgroup.subtree_control`（但它是
//! 真正的根，有进程时也能开控制器）：
//!
//! - `memory.max` 是 OOM killer 的上限，`memory.current` 是它量的 App 的内存（宿主的匿名内存，
//!   换出到 zram 的也算在里面，所以 `memory.swap.max` 是 0），`memory.stat` 把它分成匿名内存和
//!   共享内存，`memory.events` 记着超出上限和结束进程的次数；
//! - 没有 CPU 配额和权重：`cpu.max` 是 `max 100000`，cpuset 是 guest 看到的所有 CPU；
//! - `pids.current` 是 guest 的线程数，`pids.max` 不限（限制在 threads-max 和 `RLIMIT_NPROC`）。
//!
//! 下面可以建子 cgroup（dockerd、runc、crun、systemd 这样管容器和服务），进程经由
//! `cgroup.procs` 迁移、fork 时随父进程，`/proc/<pid>/cgroup` 是相对读者的 cgroup 命名空间的
//! 根的路径。子 cgroup 的控制器按父 cgroup 的 `cgroup.subtree_control`，规则与 Linux 6.1 相同
//! （没有内部进程、在用的不能关）。限制（`memory.max`、`pids.max`、`cpu.max`……）按 Linux 的格式
//! 检查后记下来，读出来是写进去的值，还不起作用；`cgroup.freeze` 还不能冻结；`cgroup.kill`
//! 结束整棵子树里的进程。

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::cred::Cred;
use crate::errno::Errno;
use crate::kernel::{INIT, Kernel, Tid};

/// 根 cgroup 的编号（也是它的 inode 号，与 Linux 一样是 1）。
pub(crate) const ROOT: u64 = 1;

/// Linux 的 cgroup v2 控制器（按 ssid 的顺序）和 appine 有没有：`cgroup.controllers` 按这个
/// 顺序列出，`subtree_control` 写别的名字是 EINVAL，写没有的是 ENOENT。
const CONTROLLERS: [(&str, bool); 8] = [
    ("cpuset", true),
    ("cpu", true),
    ("io", false),
    ("memory", true),
    ("hugetlb", false),
    ("pids", true),
    ("rdma", false),
    ("misc", false),
];

/// appine 有的控制器（[`CONTROLLERS`] 里的位）。
const AVAILABLE: u32 = 1 | 1 << 1 | 1 << 3 | 1 << 5;

/// cgroup 目录里的文件：名字、权限位、属于哪个控制器（None 是核心文件，每个 cgroup 都有）。
/// 按名字排好（readdir 的顺序）。
pub(crate) const FILES: [(&[u8], u32, Option<usize>); 39] = [
    (b"cgroup.controllers", 0o444, None),
    (b"cgroup.events", 0o444, None),
    (b"cgroup.freeze", 0o644, None),
    (b"cgroup.kill", 0o200, None),
    (b"cgroup.max.depth", 0o644, None),
    (b"cgroup.max.descendants", 0o644, None),
    (b"cgroup.procs", 0o644, None),
    (b"cgroup.stat", 0o444, None),
    (b"cgroup.subtree_control", 0o644, None),
    (b"cgroup.threads", 0o644, None),
    (b"cgroup.type", 0o644, None),
    (b"cpu.idle", 0o644, Some(1)),
    (b"cpu.max", 0o644, Some(1)),
    (b"cpu.max.burst", 0o644, Some(1)),
    (b"cpu.stat", 0o444, None),
    (b"cpu.weight", 0o644, Some(1)),
    (b"cpu.weight.nice", 0o644, Some(1)),
    (b"cpuset.cpus", 0o644, Some(0)),
    (b"cpuset.cpus.effective", 0o444, Some(0)),
    (b"cpuset.cpus.partition", 0o644, Some(0)),
    (b"cpuset.mems", 0o644, Some(0)),
    (b"cpuset.mems.effective", 0o444, Some(0)),
    (b"memory.current", 0o444, Some(3)),
    (b"memory.events", 0o444, Some(3)),
    (b"memory.events.local", 0o444, Some(3)),
    (b"memory.high", 0o644, Some(3)),
    (b"memory.low", 0o644, Some(3)),
    (b"memory.max", 0o644, Some(3)),
    (b"memory.min", 0o644, Some(3)),
    (b"memory.oom.group", 0o644, Some(3)),
    (b"memory.peak", 0o444, Some(3)),
    (b"memory.stat", 0o444, Some(3)),
    (b"memory.swap.current", 0o444, Some(3)),
    (b"memory.swap.events", 0o444, Some(3)),
    (b"memory.swap.high", 0o644, Some(3)),
    (b"memory.swap.max", 0o644, Some(3)),
    (b"pids.current", 0o444, Some(5)),
    (b"pids.events", 0o444, Some(5)),
    (b"pids.max", 0o644, Some(5)),
];

/// 文件 `name` 的权限位；不是 cgroup 的文件时是 None。
pub(crate) fn file_mode(name: &[u8]) -> Option<u32> {
    FILES.iter().find(|f| f.0 == name).map(|f| f.1)
}

/// 一个 cgroup。
pub(crate) struct Cgroup {
    /// 在上级目录里的名字（根是空的）。
    pub name: Vec<u8>,
    pub parent: Option<u64>,
    /// 子 cgroup：名字 → 编号。
    pub children: BTreeMap<Vec<u8>, u64>,
    /// `cgroup.subtree_control`：给子 cgroup 开的控制器（[`CONTROLLERS`] 里的位）。
    pub subtree_control: u32,
    /// 目录和文件的属主（建立它的进程的 fsuid、fsgid，Linux 的 `cgroup_kn_set_ugid`）。
    pub uid: u32,
    pub gid: u32,
    /// 写过的接口文件（限制这些）的值，按 Linux 显示的格式；没写过的显示默认值。
    pub values: BTreeMap<&'static [u8], String>,
}

/// 所有的 cgroup，按编号。编号不重复使用。
pub(crate) struct Cgroups {
    pub map: BTreeMap<u64, Cgroup>,
    next: u64,
}

impl Default for Cgroups {
    fn default() -> Cgroups {
        let root = Cgroup {
            name: Vec::new(),
            parent: None,
            children: BTreeMap::new(),
            subtree_control: 0,
            uid: 0,
            gid: 0,
            values: BTreeMap::new(),
        };
        Cgroups {
            map: BTreeMap::from([(ROOT, root)]),
            next: ROOT + 1,
        }
    }
}

impl Cgroups {
    pub fn get(&self, id: u64) -> Option<&Cgroup> {
        self.map.get(&id)
    }

    /// `id` 和它的祖先，从它自己开始。
    fn ancestors(&self, id: u64) -> impl Iterator<Item = u64> + '_ {
        std::iter::successors(Some(id), |&c| self.map.get(&c).and_then(|g| g.parent))
    }

    /// `id` 在 `top` 下面（或者就是它）。
    pub fn is_within(&self, id: u64, top: u64) -> bool {
        self.ancestors(id).any(|c| c == top)
    }

    /// `id` 下面的 cgroup（不含它自己）的个数。
    fn descendants(&self, id: u64) -> usize {
        self.map.get(&id).map_or(0, |g| {
            g.children.values().map(|&c| 1 + self.descendants(c)).sum()
        })
    }

    /// `id` 的路径，相对 `root`（cgroup 命名空间的根）；不在它下面时像 Linux 的
    /// `kernfs_path_from_node` 那样先用 `..` 回到共同的祖先。
    pub fn path(&self, id: u64, root: u64) -> String {
        let up: Vec<u64> = self.ancestors(root).collect();
        let mut down = Vec::new();
        let mut c = id;
        while !up.contains(&c) {
            down.push(c);
            match self.map.get(&c).and_then(|g| g.parent) {
                Some(p) => c = p,
                None => break,
            }
        }
        let mut parts: Vec<String> = up
            .iter()
            .take_while(|&&a| a != c)
            .map(|_| "..".to_owned())
            .collect();
        parts.extend(
            down.iter()
                .rev()
                .filter_map(|d| self.map.get(d))
                .map(|g| String::from_utf8_lossy(&g.name).into_owned()),
        );
        format!("/{}", parts.join("/"))
    }

    /// cgroup `id` 开着的控制器（`cgroup.controllers`）：根的都开着，别的是父 cgroup 的
    /// `subtree_control`。
    fn controllers(&self, id: u64) -> u32 {
        match self.map.get(&id).and_then(|g| g.parent) {
            None => AVAILABLE,
            Some(p) => self.map.get(&p).map_or(0, |g| g.subtree_control),
        }
    }

    /// cgroup `id` 目录里有的文件：核心文件，加上开着的控制器的。
    pub fn files(&self, id: u64) -> impl Iterator<Item = &'static [u8]> + '_ {
        let on = self.controllers(id);
        FILES
            .iter()
            .filter(move |f| f.2.is_none_or(|c| on & 1 << c != 0))
            .map(|f| f.0)
    }

    /// 在 `parent` 下面建子 cgroup `name`（Linux 的 `cgroup_mkdir`），属主是 `cred` 的
    /// fsuid、fsgid。已经有同名的是 EEXIST；名字里有换行是 EINVAL；超出祖先的
    /// `cgroup.max.depth`、`cgroup.max.descendants` 是 EAGAIN。
    pub fn mkdir(&mut self, parent: u64, name: &[u8], cred: &Cred) -> Result<u64, Errno> {
        let p = self.map.get(&parent).ok_or(Errno::ENOENT)?;
        if p.children.contains_key(name) || self.files(parent).any(|f| f == name) {
            return Err(Errno::EEXIST);
        }
        if name.contains(&b'\n') {
            return Err(Errno::EINVAL);
        }
        // 每个祖先 a：新 cgroup 在它下面的深度、它下面的 cgroup 数都不能超过它的上限
        for (depth, a) in self.ancestors(parent).enumerate() {
            let limit = |key: &[u8]| {
                self.map[&a]
                    .values
                    .get(key)
                    .and_then(|v| v.parse::<usize>().ok())
            };
            if limit(b"cgroup.max.depth").is_some_and(|m| depth + 1 > m)
                || limit(b"cgroup.max.descendants").is_some_and(|m| self.descendants(a) + 1 > m)
            {
                return Err(Errno::EAGAIN);
            }
        }
        let id = self.next;
        self.next += 1;
        self.map.insert(
            id,
            Cgroup {
                name: name.to_vec(),
                parent: Some(parent),
                children: BTreeMap::new(),
                subtree_control: 0,
                uid: cred.fsuid,
                gid: cred.fsgid,
                values: BTreeMap::new(),
            },
        );
        self.map
            .get_mut(&parent)
            .unwrap()
            .children
            .insert(name.to_vec(), id);
        Ok(id)
    }
}

/// 删除 cgroup `id`（Linux 的 `cgroup_rmdir`）：还有进程在里面、还有子 cgroup 时是 EBUSY，
/// 根删不了（EBUSY）。
pub(crate) fn rmdir(k: &mut Kernel, id: u64) -> Result<(), Errno> {
    let g = k.cgroups.map.get(&id).ok_or(Errno::ENOENT)?;
    let Some(parent) = g.parent else {
        return Err(Errno::EBUSY);
    };
    if !g.children.is_empty() || populated(k, id) {
        return Err(Errno::EBUSY);
    }
    let name = g.name.clone();
    k.cgroups.map.remove(&id);
    if let Some(p) = k.cgroups.map.get_mut(&parent) {
        p.children.remove(&name);
    }
    Ok(())
}

/// 还活着（有线程）的进程所在的 cgroup。
fn members(k: &Kernel) -> impl Iterator<Item = (Tid, u64)> + '_ {
    k.groups.iter().filter_map(|(&tgid, g)| {
        let g = g.borrow();
        (tgid != INIT && !g.threads.is_empty()).then_some((tgid, g.cgroup))
    })
}

/// 进程 `tgid` 所在的 cgroup。
fn group_cgroup(k: &Kernel, tgid: Tid) -> Option<u64> {
    k.groups.get(&tgid).map(|g| g.borrow().cgroup)
}

/// cgroup `id` 或者它下面有活着的进程（`cgroup.events` 的 populated）。
fn populated(k: &Kernel, id: u64) -> bool {
    members(k).any(|(_, c)| k.cgroups.is_within(c, id))
}

/// `/proc/<pid>/cgroup`：进程所在的 cgroup，相对读者的 cgroup 命名空间的根 `root`。
pub(crate) fn proc_cgroup(k: &Kernel, cgroup: u64, root: u64) -> String {
    format!("0::{}\n", k.cgroups.path(cgroup, root))
}

/// `/proc/cgroups`：cgroup v2 下各个控制器的层级都是 0（JVM 据此认出 cgroup v2），
/// num_cgroups 是 cgroup 的个数。cpuacct 在 v2 里并进了 cpu，仍然列出。
pub(crate) fn proc_cgroups(k: &Kernel) -> String {
    let mut s = String::from("#subsys_name\thierarchy\tnum_cgroups\tenabled\n");
    let n = k.cgroups.map.len();
    for name in ["cpuset", "cpu", "cpuacct", "memory", "pids"] {
        writeln!(s, "{name}\t0\t{n}\t1").unwrap();
    }
    s
}

/// 控制器的位掩码按 [`CONTROLLERS`] 的顺序列出名字，以空格分开。
fn names(mask: u32) -> String {
    let names: Vec<&str> = CONTROLLERS
        .iter()
        .enumerate()
        .filter(|(i, _)| mask & 1 << i != 0)
        .map(|(_, (n, _))| *n)
        .collect();
    names.join(" ")
}

/// cgroup `id` 里的 `<name>` 的内容；不是它的文件时是 None。
pub(crate) fn file(k: &Kernel, id: u64, name: &[u8]) -> Option<String> {
    let g = k.cgroups.get(id)?;
    if !k.cgroups.files(id).any(|f| f == name) {
        return None;
    }
    let root = g.parent.is_none();
    let max = |v: u64| {
        if v == u64::MAX {
            "max".to_owned()
        } else {
            v.to_string()
        }
    };
    let cpus = appine_hal::host_cpus().len();
    // 子树里的进程、线程
    let in_tree = |c: u64| k.cgroups.is_within(c, id);
    let procs = || members(k).filter(|&(_, c)| c == id).map(|(t, _)| t);
    let tasks = || {
        k.tasks
            .iter()
            .filter(move |(_, t)| t.tgid != INIT && group_cgroup(k, t.tgid).is_some_and(in_tree))
    };
    if let Some(v) = g.values.get(name) {
        return Some(format!("{v}\n"));
    }
    let s = match name {
        b"cgroup.controllers" => names(k.cgroups.controllers(id)),
        // 控制器的列表只在列出了名字时才换行（Linux 的 cgroup_print_ss_mask）
        b"cgroup.subtree_control" if g.subtree_control == 0 => return Some(String::new()),
        b"cgroup.subtree_control" => names(g.subtree_control),
        b"cgroup.type" => "domain".to_owned(),
        b"cgroup.procs" => list(procs()),
        b"cgroup.threads" => list(
            k.tasks
                .iter()
                .filter(|(_, t)| t.tgid != INIT && group_cgroup(k, t.tgid) == Some(id))
                .map(|(&tid, _)| tid),
        ),
        b"cgroup.events" => format!("populated {}\nfrozen 0", populated(k, id) as u8),
        b"cgroup.freeze" | b"cpu.idle" | b"cpu.max.burst" | b"cpu.weight.nice" => "0".to_owned(),
        b"memory.min" | b"memory.low" | b"memory.oom.group" | b"memory.swap.current" => {
            "0".to_owned()
        }
        b"cgroup.max.depth"
        | b"cgroup.max.descendants"
        | b"memory.high"
        | b"memory.swap.high"
        | b"pids.max" => "max".to_owned(),
        b"cgroup.stat" => format!(
            "nr_descendants {}\nnr_dying_descendants 0",
            k.cgroups.descendants(id)
        ),
        b"cpu.max" => "max 100000".to_owned(),
        b"cpu.weight" => "100".to_owned(),
        b"cpu.stat" => {
            let cpu = if root {
                k.tasks.values().fold(k.cpu, |sum, t| sum + t.cpu())
            } else {
                tasks().fold(Default::default(), |sum, (_, t)| sum + t.cpu())
            };
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
        // 根的内存是 OOM killer 量的整个 App 的；子 cgroup 的是里面的进程的驻留内存之和
        b"memory.current" if root => k.oom.measure().to_string(),
        b"memory.peak" if root => {
            k.oom.measure();
            k.oom.peak.get().to_string()
        }
        b"memory.current" | b"memory.peak" => subtree_rss(k, id).to_string(),
        b"memory.max" if root => max(k.oom.limit),
        b"memory.swap.max" if root => "0".to_owned(),
        b"memory.max" | b"memory.swap.max" => "max".to_owned(),
        b"memory.events" | b"memory.events.local" if root => {
            let e = k.oom.events.get();
            format!(
                "low 0\nhigh 0\nmax {}\noom {}\noom_kill {}\noom_group_kill 0",
                e.max, e.oom, e.oom_kill
            )
        }
        b"memory.events" | b"memory.events.local" => {
            "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\noom_group_kill 0".to_owned()
        }
        b"memory.stat" if root => memory_stat(),
        b"memory.stat" => child_memory_stat(subtree_rss(k, id)),
        b"memory.swap.events" => "high 0\nmax 0\nfail 0".to_owned(),
        b"pids.current" if root => k.tasks.len().to_string(),
        b"pids.current" => tasks().count().to_string(),
        b"pids.events" => "max 0".to_owned(),
        // 只能写的（cgroup.kill）
        _ => return None,
    };
    Some(s + "\n")
}

/// cgroup `id` 子树里的进程的驻留内存之和（字节）：子 cgroup 的 `memory.current`。
fn subtree_rss(k: &Kernel, id: u64) -> u64 {
    members(k)
        .filter(|&(_, c)| k.cgroups.is_within(c, id))
        .filter_map(|(tgid, _)| {
            let g = k.groups.get(&tgid)?.borrow();
            let task = k.tasks.get(g.threads.first()?)?;
            let pages = task.res.borrow().mm.borrow().resident_pages();
            Some(pages * 4096)
        })
        .sum()
}

/// 子 cgroup 的 `memory.stat`：只知道驻留内存，都算匿名内存。
fn child_memory_stat(rss: u64) -> String {
    memory_stat_with(rss, 0)
}

/// 写的进程：线程、所在的 pid 命名空间（cgroup.procs 里的编号按它，0 是它自己）、所在的
/// cgroup 命名空间的根。
pub(crate) struct Writer<'a> {
    pub tid: Tid,
    pub pid_ns: &'a crate::pidns::PidNs,
    pub ns_root: u64,
}

/// 写 cgroup `id` 里的 `<name>`（Linux 6.1 的 `cgroup_file_write` 和各个 cftype 的 `write`）。
pub(crate) fn write(
    k: &mut Kernel,
    w: &Writer<'_>,
    (id, name): (u64, &[u8]),
    data: &[u8],
) -> Result<(), Errno> {
    // 命名空间的根上只能写 cgroup.procs、cgroup.threads、cgroup.subtree_control，别的是 EPERM
    // （nsdelegate）。appine 的根看起来是容器的根，也照这条：写它的 memory.max 这些不改变
    // OOM killer 的上限，与容器里一样不让写
    if (id == w.ns_root || id == ROOT)
        && !matches!(
            name,
            b"cgroup.procs" | b"cgroup.threads" | b"cgroup.subtree_control"
        )
    {
        return Err(Errno::EPERM);
    }
    // 没有写的回调的（只读的文件）
    if file_mode(name).is_none_or(|m| m & 0o200 == 0) {
        return Err(Errno::EINVAL);
    }
    let text = std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?;
    let word = text.trim();
    match name {
        b"cgroup.procs" | b"cgroup.threads" => {
            let n: i32 = word.parse().map_err(|_| Errno::EINVAL)?;
            let tid = match n {
                0 => w.tid,
                n if n < 0 => return Err(Errno::EINVAL),
                n => w.pid_ns.find(n).ok_or(Errno::ESRCH)?,
            };
            migrate(k, tid, (id, w.ns_root), name == b"cgroup.procs")
        }
        b"cgroup.subtree_control" => subtree_control(k, id, text),
        // 整数，不是 1 是 ERANGE（Linux 的 cgroup_kill_write）
        b"cgroup.kill" => match word.parse::<i32>().map_err(|_| Errno::EINVAL)? {
            1 => {
                kill(k, id);
                Ok(())
            }
            _ => Err(Errno::ERANGE),
        },
        b"cgroup.freeze" => match word.parse::<i32>().map_err(|_| Errno::EINVAL)? {
            0 => Ok(()),
            // 还不能冻结
            1 => Err(Errno::EOPNOTSUPP),
            _ => Err(Errno::ERANGE),
        },
        b"cgroup.type" => match word {
            "threaded" => Err(Errno::EOPNOTSUPP),
            _ => Err(Errno::EINVAL),
        },
        _ => {
            let value = parse_value(name, word)?;
            let g = k.cgroups.map.get_mut(&id).ok_or(Errno::ENOENT)?;
            let key = FILES.iter().find(|f| f.0 == name).ok_or(Errno::EINVAL)?.0;
            g.values.insert(key, value);
            Ok(())
        }
    }
}

/// 限制这些接口文件写进来的值，检查之后换成读出来的格式（Linux 的 page_counter_memparse、
/// cpu_max_write 这些）。
fn parse_value(name: &[u8], word: &str) -> Result<String, Errno> {
    const PAGE: u64 = 4096;
    let int = |s: &str| s.parse::<i64>().map_err(|_| Errno::EINVAL);
    // memparse：K、M、G、T 这些后缀
    let bytes = |s: &str| -> Result<u64, Errno> {
        let (num, shift) = match s.char_indices().last() {
            Some((i, c)) if c.is_ascii_alphabetic() => {
                let shift = match c.to_ascii_lowercase() {
                    'k' => 10,
                    'm' => 20,
                    'g' => 30,
                    't' => 40,
                    'p' => 50,
                    'e' => 60,
                    _ => return Err(Errno::EINVAL),
                };
                (&s[..i], shift)
            }
            _ => (s, 0),
        };
        let n: u64 = num.parse().map_err(|_| Errno::EINVAL)?;
        Ok(n.saturating_mul(1 << shift))
    };
    let pages = |s: &str| -> Result<String, Errno> {
        if s == "max" {
            return Ok("max".to_owned());
        }
        Ok((bytes(s)? / PAGE * PAGE).to_string())
    };
    Ok(match name {
        b"memory.max" | b"memory.high" | b"memory.swap.max" | b"memory.swap.high" => pages(word)?,
        b"memory.low" | b"memory.min" => {
            if word == "max" {
                "max".to_owned()
            } else {
                (bytes(word)? / PAGE * PAGE).to_string()
            }
        }
        b"memory.oom.group" | b"cpu.idle" => match int(word)? {
            v @ (0 | 1) => v.to_string(),
            _ => return Err(Errno::EINVAL),
        },
        b"pids.max" if word == "max" => "max".to_owned(),
        b"pids.max" => match int(word)? {
            v if (0..1 << 22).contains(&v) => v.to_string(),
            _ => return Err(Errno::EINVAL),
        },
        // 负数是 ERANGE（Linux 的 cgroup_max_depth_write）
        b"cgroup.max.depth" | b"cgroup.max.descendants" if word == "max" => "max".to_owned(),
        b"cgroup.max.depth" | b"cgroup.max.descendants" => match word.parse::<i32>() {
            Ok(v) if v >= 0 => v.to_string(),
            Ok(_) => return Err(Errno::ERANGE),
            Err(_) => return Err(Errno::EINVAL),
        },
        b"cpu.weight" => match int(word)? {
            v @ 1..=10000 => v.to_string(),
            _ => return Err(Errno::ERANGE),
        },
        b"cpu.weight.nice" => match int(word)? {
            v @ -20..=19 => v.to_string(),
            _ => return Err(Errno::ERANGE),
        },
        b"cpu.max.burst" => match int(word)? {
            v if v >= 0 => v.to_string(),
            _ => return Err(Errno::EINVAL),
        },
        // `$MAX $PERIOD`，MAX 是 max 或者微秒数，PERIOD 可以省略（不变）
        b"cpu.max" => {
            let mut it = word.split_ascii_whitespace();
            let quota = it.next().ok_or(Errno::EINVAL)?;
            let period = match it.next() {
                Some(p) => int(p)?,
                None => 100_000,
            };
            if it.next().is_some() || !(1000..=1_000_000).contains(&period) {
                return Err(Errno::EINVAL);
            }
            match quota {
                "max" => format!("max {period}"),
                q => match int(q)? {
                    q if q >= 1000 => format!("{q} {period}"),
                    _ => return Err(Errno::EINVAL),
                },
            }
        }
        b"cpuset.cpus" | b"cpuset.mems" => {
            let limit = if name == b"cpuset.cpus" {
                appine_hal::host_cpus().len() as i64
            } else {
                1
            };
            for part in word.split(',').filter(|p| !p.is_empty()) {
                let (a, b) = part.split_once('-').unwrap_or((part, part));
                let (a, b) = (int(a)?, int(b)?);
                if a > b || a < 0 || b >= limit {
                    return Err(Errno::EINVAL);
                }
            }
            word.to_owned()
        }
        b"cpuset.cpus.partition" => match word {
            "member" => "member".to_owned(),
            "root" | "isolated" => return Err(Errno::EINVAL),
            _ => return Err(Errno::EINVAL),
        },
        // 没有写的接口（cgroup.controllers、memory.current 这些）
        _ => return Err(Errno::EINVAL),
    })
}

/// 把线程 `tid` 所在的进程（`procs`）或者线程迁移到 cgroup `dst`（Linux 的
/// `__cgroup_procs_write`、`cgroup_attach_permissions`）：进程不存在是 ESRCH；原来的和目的地
/// 要都在写的进程的 cgroup 命名空间的根 `ns_root` 下面（ENOENT）；目的地开了控制器（不是根）
/// 时不能有进程（EBUSY）；domain cgroup 之间单独迁移线程是 EOPNOTSUPP。写 `cgroup.procs`
/// 要的写权限由打开文件时检查。
fn migrate(k: &mut Kernel, tid: Tid, (dst, ns_root): (u64, u64), procs: bool) -> Result<(), Errno> {
    let tgid = k.tasks.get(&tid).ok_or(Errno::ESRCH)?.tgid;
    if tgid == INIT {
        return Err(Errno::EINVAL);
    }
    let g = k.groups.get(&tgid).cloned().ok_or(Errno::ESRCH)?;
    let src = g.borrow().cgroup;
    if !k.cgroups.is_within(src, ns_root) || !k.cgroups.is_within(dst, ns_root) {
        return Err(Errno::ENOENT);
    }
    let dst_cg = k.cgroups.get(dst).ok_or(Errno::ENOENT)?;
    if !procs {
        return if src == dst {
            Ok(())
        } else {
            Err(Errno::EOPNOTSUPP)
        };
    }
    if dst_cg.parent.is_some() && dst_cg.subtree_control != 0 {
        return Err(Errno::EBUSY);
    }
    g.borrow_mut().cgroup = dst;
    Ok(())
}

/// 写 `cgroup.subtree_control`（Linux 的 `cgroup_subtree_control_write`）：`+名字`、`-名字`，
/// 以空白分开。不认识的控制器、没有 `+`/`-` 是 EINVAL；要开的不在 `cgroup.controllers` 里是
/// ENOENT；不是根、里面有进程时不能开（EBUSY）；子 cgroup 还开着的不能关（EBUSY）。
fn subtree_control(k: &mut Kernel, id: u64, text: &str) -> Result<(), Errno> {
    let (mut enable, mut disable) = (0u32, 0u32);
    for tok in text.split_ascii_whitespace() {
        let (sign, name) = tok.split_at(1);
        let i = CONTROLLERS
            .iter()
            .position(|(n, _)| *n == name)
            .ok_or(Errno::EINVAL)?;
        match sign {
            "+" => {
                enable |= 1 << i;
                disable &= !(1 << i);
            }
            "-" => {
                disable |= 1 << i;
                enable &= !(1 << i);
            }
            _ => return Err(Errno::EINVAL),
        }
    }
    let g = k.cgroups.get(id).ok_or(Errno::ENOENT)?;
    enable &= !g.subtree_control;
    disable &= g.subtree_control;
    if enable == 0 && disable == 0 {
        return Ok(());
    }
    if enable & !k.cgroups.controllers(id) != 0 {
        return Err(Errno::ENOENT);
    }
    let has_procs = members(k).any(|(_, c)| c == id);
    if enable != 0 && g.parent.is_some() && has_procs {
        return Err(Errno::EBUSY);
    }
    let children: Vec<u64> = g.children.values().copied().collect();
    if children.iter().any(|c| {
        k.cgroups
            .get(*c)
            .is_some_and(|c| c.subtree_control & disable != 0)
    }) {
        return Err(Errno::EBUSY);
    }
    let g = k.cgroups.map.get_mut(&id).unwrap();
    g.subtree_control = (g.subtree_control | enable) & !disable;
    Ok(())
}

/// `cgroup.kill`：给子树里的进程都发 SIGKILL（Linux 的 `cgroup_kill`）。
fn kill(k: &mut Kernel, id: u64) {
    let victims: Vec<Tid> = members(k)
        .filter(|&(_, c)| k.cgroups.is_within(c, id))
        .map(|(t, _)| t)
        .collect();
    for tgid in victims {
        crate::signal::send_to_group(
            k,
            tgid,
            crate::process::Signal::KILL,
            crate::signal::SigInfo::kernel(),
        );
    }
}

/// 能不能把从 cgroup `src` 里 fork 出来的子进程放进 cgroup `dst`（clone3 的
/// `CLONE_INTO_CGROUP`，Linux 的 `cgroup_css_set_fork`）：已经删了是 ENODEV；两个都要在调用者的
/// cgroup 命名空间的根 `ns_root` 下面（ENOENT）；开了控制器（不是根）的不能有进程（EBUSY）。
pub(crate) fn may_enter(k: &Kernel, src: u64, dst: u64, ns_root: u64) -> Result<(), Errno> {
    let g = k.cgroups.get(dst).ok_or(Errno::ENODEV)?;
    if !k.cgroups.is_within(src, ns_root) || !k.cgroups.is_within(dst, ns_root) {
        return Err(Errno::ENOENT);
    }
    if g.parent.is_some() && g.subtree_control != 0 {
        return Err(Errno::EBUSY);
    }
    Ok(())
}

/// `memory.stat`，按 Linux 6.1 的 `memory_stat_format`（有 swap 和透明大页、没有 zswap 的
/// 配置），单位是字节。只量得到宿主进程的匿名内存（与 `memory.current` 相同）：私有的和换出的
/// 算 anon，共享的算 shmem（Linux 的 file 包括 shmem），都在 active_anon 里；没有可以回收的
/// 页缓存，inactive_file 是 0。.NET 的 GC 按 `memory.current` 减去 inactive_file 算用量，没有
/// 这个文件就只看自己进程的 RSS。
fn memory_stat() -> String {
    let u = crate::oom::app_memory().unwrap_or_default();
    memory_stat_with(u.anon + u.swap, u.shmem)
}

/// [`memory_stat`] 的格式：匿名内存 `anon`、共享内存 `shmem`（字节）。
fn memory_stat_with(anon: u64, shmem: u64) -> String {
    let mut s = String::new();
    let mut put = |name: &str, v: u64| writeln!(s, "{name} {v}").unwrap();
    for (name, v) in [("anon", anon), ("file", shmem)] {
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
    put("shmem", shmem);
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
    put("active_anon", anon + shmem);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 建子 cgroup：重名（含这个 cgroup 有的接口文件）是 EEXIST，换行是 EINVAL；子 cgroup
    /// 的文件按父 cgroup 的 subtree_control；路径相对命名空间的根，不在下面时先用 `..`。
    #[test]
    fn tree_and_paths() {
        let mut c = Cgroups::default();
        let cred = Cred::root();
        let t = c.mkdir(ROOT, b"t", &cred).unwrap();
        assert_eq!(c.mkdir(ROOT, b"t", &cred), Err(Errno::EEXIST));
        assert_eq!(c.mkdir(ROOT, b"cgroup.procs", &cred), Err(Errno::EEXIST));
        assert_eq!(c.mkdir(ROOT, b"a\nb", &cred), Err(Errno::EINVAL));
        // t 没开 memory，可以建叫 memory.max 的子 cgroup
        assert!(!c.files(t).any(|f| f == b"memory.max"));
        assert!(c.mkdir(t, b"memory.max", &cred).is_ok());
        c.map.get_mut(&ROOT).unwrap().subtree_control = 1 << 3;
        assert!(c.files(t).any(|f| f == b"memory.max"));
        assert!(!c.files(t).any(|f| f == b"pids.max"));
        let a = c.mkdir(t, b"a", &cred).unwrap();
        let b = c.mkdir(t, b"b", &cred).unwrap();
        assert_eq!(c.path(a, ROOT), "/t/a");
        assert_eq!(c.path(a, a), "/");
        assert_eq!(c.path(b, a), "/../b");
        assert_eq!(c.path(ROOT, a), "/../..");
        assert!(c.is_within(a, t) && !c.is_within(t, a));
        assert_eq!(c.descendants(ROOT), 4);
    }

    /// 祖先的 cgroup.max.depth、cgroup.max.descendants 限制新建的（EAGAIN）。
    #[test]
    fn depth_and_descendant_limits() {
        let mut c = Cgroups::default();
        let cred = Cred::root();
        let t = c.mkdir(ROOT, b"t", &cred).unwrap();
        c.map
            .get_mut(&t)
            .unwrap()
            .values
            .insert(b"cgroup.max.depth", "1".to_owned());
        let a = c.mkdir(t, b"a", &cred).unwrap();
        assert_eq!(c.mkdir(a, b"x", &cred), Err(Errno::EAGAIN));
        let g = c.map.get_mut(&t).unwrap();
        g.values.remove(&b"cgroup.max.depth"[..]);
        g.values.insert(b"cgroup.max.descendants", "2".to_owned());
        assert!(c.mkdir(a, b"x", &cred).is_ok());
        assert_eq!(c.mkdir(t, b"b", &cred), Err(Errno::EAGAIN));
    }

    /// 接口文件的值按 Linux 的格式检查、换算。
    #[test]
    fn values() {
        assert_eq!(parse_value(b"memory.max", "1M").unwrap(), "1048576");
        assert_eq!(parse_value(b"memory.max", "5000").unwrap(), "4096");
        assert_eq!(parse_value(b"memory.max", "max").unwrap(), "max");
        assert_eq!(parse_value(b"memory.max", "1x"), Err(Errno::EINVAL));
        assert_eq!(parse_value(b"pids.max", "-1"), Err(Errno::EINVAL));
        assert_eq!(parse_value(b"cgroup.max.depth", "-1"), Err(Errno::ERANGE));
        assert_eq!(parse_value(b"cpu.weight", "0"), Err(Errno::ERANGE));
        assert_eq!(parse_value(b"cpu.max", "50000").unwrap(), "50000 100000");
        assert_eq!(parse_value(b"cpu.max", "max 20000").unwrap(), "max 20000");
        assert_eq!(parse_value(b"cpu.max", "500"), Err(Errno::EINVAL));
        assert_eq!(parse_value(b"memory.current", "1"), Err(Errno::EINVAL));
    }
}
