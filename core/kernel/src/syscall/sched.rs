//! 调度类 syscall：nice 值（getpriority、setpriority）和 `sched_*`。
//!
//! guest 线程就是普通的宿主线程，由宿主调度。nice 值、调度策略、实时优先级和 CPU
//! 亲和性都只记录下来（procfs 和对应的 get 调用能看到），不影响宿主调度。权限按
//! Linux 的规则检查：改别人的设置要求有效用户 ID 相同，提高优先级、实时策略要求
//! `CAP_SYS_NICE`。`SCHED_DEADLINE` 的参数按 Linux 的规则检查后记录，不做带宽准入控制。

use std::cell::RefCell;
use std::rc::Rc;

use super::{SysResult, get, put, write_pair};
use crate::cred::{Cred, cap};
use crate::errno::Errno;
use crate::kernel::{Cx, INIT};
use crate::process::ThreadGroup;
use crate::task::Task;

const PRIO_PROCESS: i32 = 0;
const PRIO_PGRP: i32 = 1;
const PRIO_USER: i32 = 2;

const SCHED_OTHER: i32 = 0;
const SCHED_FIFO: i32 = 1;
const SCHED_RR: i32 = 2;
const SCHED_BATCH: i32 = 3;
const SCHED_IDLE: i32 = 5;
const SCHED_DEADLINE: i32 = 6;
const SCHED_RESET_ON_FORK: i32 = 0x4000_0000;
const SCHED_FLAG_RESET_ON_FORK: u64 = 1;

/// `struct sched_attr` 的大小：第一版，以及这里支持的版本（带 util_min/util_max）。
const SCHED_ATTR_SIZE_VER0: usize = 48;
const SCHED_ATTR_SIZE: usize = 56;

/// `SCHED_RR` 的时间片（Linux 的 `RR_TIMESLICE`，100 毫秒）。
const RR_TIMESLICE_NS: i64 = 100_000_000;

/// 能不能修改凭据为 `target` 的进程的调度参数（`check_same_owner`）。
fn same_owner(cred: &Cred, target: &Cred) -> bool {
    cred.euid == target.euid || cred.euid == target.ruid || cred.capable(cap::SYS_NICE)
}

/// getpriority、setpriority 作用的进程（不含僵尸进程）。
fn targets(cx: &Cx<'_>, which: i32, who: i32) -> Result<Vec<Rc<RefCell<ThreadGroup>>>, Errno> {
    let k = cx.kr();
    let live = |g: &&Rc<RefCell<ThreadGroup>>| !g.borrow().threads.is_empty();
    let found: Vec<_> = match which {
        PRIO_PROCESS => {
            let tgid = match who {
                0 => cx.tgid(),
                tid => k.tasks.get(&tid).map_or(tid, |t| t.tgid),
            };
            k.groups
                .get(&tgid)
                .filter(live)
                .cloned()
                .into_iter()
                .collect()
        }
        PRIO_PGRP => {
            let pgid = if who == 0 {
                cx.t.group.borrow().pgid
            } else {
                who
            };
            k.groups
                .values()
                .filter(live)
                .filter(|g| g.borrow().pgid == pgid)
                .cloned()
                .collect()
        }
        PRIO_USER => {
            let uid = if who == 0 { cx.cred().ruid } else { who as u32 };
            k.groups
                .values()
                .filter(live)
                .filter(|g| g.borrow().cred.ruid == uid)
                .cloned()
                .collect()
        }
        _ => return Err(Errno::EINVAL),
    };
    if found.is_empty() {
        return Err(Errno::ESRCH);
    }
    Ok(found)
}

pub(super) fn getpriority(cx: &mut Cx<'_>, which: i32, who: i32) -> SysResult {
    // 与 Linux 的 syscall 一样返回 20 - nice（1–40），libc 再换算回来
    let nice = targets(cx, which, who)?
        .iter()
        .map(|g| g.borrow().nice)
        .min()
        .unwrap();
    Ok((20 - nice) as u64)
}

pub(super) fn setpriority(cx: &mut Cx<'_>, which: i32, who: i32, nice: i32) -> SysResult {
    let nice = nice.clamp(-20, 19);
    let cred = cx.cred();
    let mut result = Ok(0);
    for g in targets(cx, which, who)? {
        let mut g = g.borrow_mut();
        // 别人的进程要求有效用户 ID 相同，提高优先级（降低 nice）要求 CAP_SYS_NICE
        if !same_owner(&cred, &g.cred) {
            result = Err(Errno::EPERM);
        } else if nice < g.nice && !cred.capable(cap::SYS_NICE) {
            result = Err(Errno::EACCES);
        } else {
            g.nice = nice;
        }
    }
    result
}

/// `sched_*` 的目标线程：pid 0 是自己。内置的 init 没有线程，返回 None。
fn target(cx: &Cx<'_>, pid: i32) -> Result<Option<Rc<Task>>, Errno> {
    match pid {
        p if p < 0 => Err(Errno::EINVAL),
        0 => Ok(Some(cx.task.clone())),
        INIT => Ok(None),
        p => cx.kr().tasks.get(&p).cloned().map(Some).ok_or(Errno::ESRCH),
    }
}

/// 线程所在进程的凭据。
fn cred_of(cx: &Cx<'_>, task: Option<&Rc<Task>>) -> Cred {
    task.and_then(|t| cx.kr().groups.get(&t.tgid))
        .map_or_else(Cred::root, |g| g.borrow().cred.clone())
}

/// fork 出来的线程继承的调度策略：`SCHED_RESET_ON_FORK` 时回到普通策略。
pub(super) fn inherit((policy, priority): (i32, i32)) -> (i32, i32) {
    if policy & SCHED_RESET_ON_FORK != 0 {
        let base = policy & !SCHED_RESET_ON_FORK;
        if matches!(base, SCHED_FIFO | SCHED_RR) {
            return (SCHED_OTHER, 0);
        }
        return (base, priority);
    }
    (policy, priority)
}

/// 设置调度策略：检查策略、优先级和权限。
fn set_policy(
    cx: &Cx<'_>,
    task: Option<&Rc<Task>>,
    policy: i32,
    priority: i32,
) -> Result<(), Errno> {
    let base = policy & !SCHED_RESET_ON_FORK;
    let rt = match base {
        SCHED_FIFO | SCHED_RR => true,
        SCHED_OTHER | SCHED_BATCH | SCHED_IDLE => false,
        _ => return Err(Errno::EINVAL),
    };
    if rt && !(1..=99).contains(&priority) || !rt && priority != 0 {
        return Err(Errno::EINVAL);
    }
    let cred = cx.cred();
    if !same_owner(&cred, &cred_of(cx, task)) || rt && !cred.capable(cap::SYS_NICE) {
        return Err(Errno::EPERM);
    }
    if let Some(t) = task {
        t.sched.set((policy, priority));
    }
    Ok(())
}

/// 设成 `SCHED_DEADLINE`（`__checkparam_dl`）：runtime ≤ deadline ≤ period，runtime 至少
/// 1024 纳秒，period 为 0 时等于 deadline。
fn set_deadline(
    cx: &Cx<'_>,
    task: Option<&Rc<Task>>,
    priority: i32,
    (runtime, deadline, period): (u64, u64, u64),
) -> SysResult {
    let period = if period == 0 { deadline } else { period };
    if priority != 0
        || deadline == 0
        || runtime < 1024
        || (deadline | period) & (1 << 63) != 0
        || period < deadline
        || deadline < runtime
    {
        return Err(Errno::EINVAL);
    }
    let cred = cx.cred();
    if !same_owner(&cred, &cred_of(cx, task)) || !cred.capable(cap::SYS_NICE) {
        return Err(Errno::EPERM);
    }
    if let Some(t) = task {
        t.sched.set((SCHED_DEADLINE, 0));
        t.deadline.set((runtime, deadline, period));
    }
    Ok(0)
}

/// 读取 `struct sched_param` 的优先级。
fn read_priority(cx: &Cx<'_>, param: u64) -> Result<i32, Errno> {
    if param == 0 {
        return Err(Errno::EINVAL);
    }
    Ok(i32::from_le_bytes(get(cx, param)?))
}

pub(super) fn sched_setscheduler(cx: &mut Cx<'_>, pid: i32, policy: i32, param: u64) -> SysResult {
    if pid < 0 {
        return Err(Errno::EINVAL);
    }
    let priority = read_priority(cx, param)?;
    let task = target(cx, pid)?;
    set_policy(cx, task.as_ref(), policy, priority).map(|()| 0)
}

pub(super) fn sched_setparam(cx: &mut Cx<'_>, pid: i32, param: u64) -> SysResult {
    if pid < 0 {
        return Err(Errno::EINVAL);
    }
    let priority = read_priority(cx, param)?;
    let task = target(cx, pid)?;
    let policy = task.as_ref().map_or(SCHED_OTHER, |t| t.sched.get().0);
    set_policy(cx, task.as_ref(), policy, priority).map(|()| 0)
}

pub(super) fn sched_getscheduler(cx: &mut Cx<'_>, pid: i32) -> SysResult {
    let task = target(cx, pid)?;
    Ok(task.map_or(SCHED_OTHER, |t| t.sched.get().0) as u64)
}

pub(super) fn sched_getparam(cx: &mut Cx<'_>, pid: i32, param: u64) -> SysResult {
    if param == 0 || pid < 0 {
        return Err(Errno::EINVAL);
    }
    let task = target(cx, pid)?;
    let priority = task.map_or(0, |t| t.sched.get().1);
    put(cx, param, priority.to_le_bytes()).map(|()| 0)
}

pub(super) fn sched_setattr(cx: &mut Cx<'_>, pid: i32, attr: u64, flags: u32) -> SysResult {
    if attr == 0 || pid < 0 || flags != 0 {
        return Err(Errno::EINVAL);
    }
    let size = match u32::from_le_bytes(get(cx, attr)?) as usize {
        0 => SCHED_ATTR_SIZE_VER0,
        n => n,
    };
    if !(SCHED_ATTR_SIZE_VER0..=4096).contains(&size) {
        put(cx, attr, (SCHED_ATTR_SIZE as u32).to_le_bytes())?;
        return Err(Errno::E2BIG);
    }
    let mut b = vec![0u8; size];
    cx.mm()
        .read(crate::uaccess::UserSlice::new(attr, size), &mut b)?;
    // 比这里认识的版本长时，多出来的部分必须是 0
    if b.iter().skip(SCHED_ATTR_SIZE).any(|&x| x != 0) {
        put(cx, attr, (SCHED_ATTR_SIZE as u32).to_le_bytes())?;
        return Err(Errno::E2BIG);
    }
    let word = |off: usize| u32::from_le_bytes(b[off..off + 4].try_into().unwrap());
    let long = |off: usize| u64::from_le_bytes(b[off..off + 8].try_into().unwrap());
    let policy = word(4) as i32;
    let sched_flags = long(8);
    let nice = word(16) as i32;
    let priority = word(20) as i32;
    if policy < 0 {
        return Err(Errno::EINVAL);
    }
    let task = target(cx, pid)?;
    if sched_flags & !SCHED_FLAG_RESET_ON_FORK != 0 {
        return Err(Errno::EINVAL);
    }
    if policy == SCHED_DEADLINE {
        let (runtime, deadline, period) = (long(24), long(32), long(40));
        return set_deadline(cx, task.as_ref(), priority, (runtime, deadline, period));
    }
    let reset = if sched_flags & SCHED_FLAG_RESET_ON_FORK != 0 {
        SCHED_RESET_ON_FORK
    } else {
        0
    };
    set_policy(cx, task.as_ref(), policy | reset, priority)?;
    if !matches!(policy, SCHED_FIFO | SCHED_RR)
        && let Some(t) = &task
    {
        let tgid = t.tgid;
        setpriority(cx, PRIO_PROCESS, tgid, nice)?;
    }
    Ok(0)
}

pub(super) fn sched_getattr(
    cx: &mut Cx<'_>,
    pid: i32,
    attr: u64,
    size: u32,
    flags: u32,
) -> SysResult {
    let size = size as usize;
    if attr == 0 || pid < 0 || flags != 0 || !(SCHED_ATTR_SIZE_VER0..=4096).contains(&size) {
        return Err(Errno::EINVAL);
    }
    let task = target(cx, pid)?;
    let (policy, priority) = task.as_ref().map_or((SCHED_OTHER, 0), |t| t.sched.get());
    let nice = task
        .as_ref()
        .and_then(|t| cx.kr().groups.get(&t.tgid))
        .map_or(0, |g| g.borrow().nice);
    let n = size.min(SCHED_ATTR_SIZE);
    let mut b = [0u8; SCHED_ATTR_SIZE];
    b[..4].copy_from_slice(&(n as u32).to_le_bytes());
    b[4..8].copy_from_slice(&(policy & !SCHED_RESET_ON_FORK).to_le_bytes());
    if policy & SCHED_RESET_ON_FORK != 0 {
        b[8..16].copy_from_slice(&SCHED_FLAG_RESET_ON_FORK.to_le_bytes());
    }
    b[16..20].copy_from_slice(&nice.to_le_bytes());
    b[20..24].copy_from_slice(&priority.to_le_bytes());
    if policy == SCHED_DEADLINE
        && let Some(t) = &task
    {
        let (runtime, deadline, period) = t.deadline.get();
        b[24..32].copy_from_slice(&runtime.to_le_bytes());
        b[32..40].copy_from_slice(&deadline.to_le_bytes());
        b[40..48].copy_from_slice(&period.to_le_bytes());
    }
    // util_max 默认是 1024（SCHED_CAPACITY_SCALE）
    b[52..56].copy_from_slice(&1024u32.to_le_bytes());
    cx.mm()
        .write(crate::uaccess::UserSlice::new(attr, n), &b[..n])
        .map(|()| 0)
}

/// 宿主的 CPU 个数对应的掩码（最多 64 个）。
fn online_cpus() -> u64 {
    let cpus = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(64);
    if cpus == 64 {
        u64::MAX
    } else {
        (1u64 << cpus) - 1
    }
}

/// 亲和性的目标线程：与 Linux 相同，负数的 pid 是找不到的进程（ESRCH），不是 EINVAL。
fn affinity_target(cx: &Cx<'_>, pid: i32) -> Result<Option<Rc<Task>>, Errno> {
    if pid < 0 {
        return Err(Errno::ESRCH);
    }
    target(cx, pid)
}

pub(super) fn sched_setaffinity(cx: &mut Cx<'_>, pid: i32, len: usize, mask: u64) -> SysResult {
    // 掩码短于内核的 cpumask（8 字节）时其余的位是 0，长出来的部分忽略
    let mut b = [0u8; 8];
    let n = len.min(8);
    cx.mm()
        .read(crate::uaccess::UserSlice::new(mask, n), &mut b[..n])?;
    let task = affinity_target(cx, pid)?;
    let new = u64::from_le_bytes(b) & online_cpus();
    if !same_owner(&cx.cred(), &cred_of(cx, task.as_ref())) {
        return Err(Errno::EPERM);
    }
    if new == 0 {
        return Err(Errno::EINVAL);
    }
    if let Some(t) = task {
        t.affinity.set(new);
    }
    Ok(0)
}

pub(super) fn sched_getaffinity(cx: &mut Cx<'_>, pid: i32, len: usize, mask: u64) -> SysResult {
    // 内核的 cpumask 按 unsigned long 对齐；guest 给的缓冲区至少要能放下它
    if len < 8 || !len.is_multiple_of(8) {
        return Err(Errno::EINVAL);
    }
    let task = affinity_target(cx, pid)?;
    let bits = task.map_or(u64::MAX, |t| t.affinity.get()) & online_cpus();
    put(cx, mask, bits.to_le_bytes())?;
    Ok(8)
}

pub(super) fn sched_priority_range(policy: i32, max: bool) -> SysResult {
    match policy {
        SCHED_FIFO | SCHED_RR => Ok(if max { 99 } else { 1 }),
        SCHED_OTHER | SCHED_BATCH | SCHED_IDLE | SCHED_DEADLINE => Ok(0),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn sched_rr_get_interval(cx: &mut Cx<'_>, pid: i32, ts: u64) -> SysResult {
    let task = target(cx, pid)?;
    // 只有 SCHED_RR 的线程有时间片
    let policy = task.map_or(SCHED_OTHER, |t| t.sched.get().0 & !SCHED_RESET_ON_FORK);
    let ns = if policy == SCHED_RR {
        RR_TIMESLICE_NS
    } else {
        0
    };
    write_pair(cx, ts, 0, ns).map(|()| 0)
}
