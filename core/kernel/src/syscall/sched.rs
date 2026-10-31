//! 调度类 syscall：nice 值（getpriority、setpriority）和 `sched_*`。
//!
//! guest 线程就是普通的宿主线程，由宿主调度。nice 值与 Linux 一样属于线程，和 CPU
//! 亲和性一起设置到宿主线程上（[`apply_host`]），宿主据此分配 CPU；`SCHED_IDLE` 设成
//! 宿主的最低优先级。guest 的 CPU `i` 是宿主允许本进程用的第 `i` 个 CPU
//! （[`appine_hal::host_cpus`]）。实时策略和实时优先级只记录下来（procfs 和对应的 get
//! 调用能看到）。权限
//! 按 Linux 的规则检查：改别人的设置要求有效用户 ID 相同，提高优先级要求
//! `RLIMIT_NICE` 允许或者 `CAP_SYS_NICE`，实时策略要求 `CAP_SYS_NICE`。`SCHED_DEADLINE`
//! 的参数按 Linux 的规则检查后记录，不做带宽准入控制。

use std::cell::Cell;
use std::rc::Rc;

use super::{SysResult, get, put, write_pair};
use crate::cell::RefCell;
use crate::cred::{Cred, cap};
use crate::errno::Errno;
use crate::kernel::{Cx, INIT};
use crate::process::{RLIMIT_NICE, ThreadGroup};
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

/// getpriority、setpriority 作用的一个线程和它所在的进程。
type Target = (Option<Rc<Task>>, Rc<RefCell<ThreadGroup>>);

/// getpriority、setpriority 作用的线程（不含僵尸进程）。与 Linux 一样，`PRIO_PROCESS`
/// 是一个线程，`PRIO_PGRP`、`PRIO_USER` 是这些进程的所有线程。内置的 init 没有线程，但
/// 它是 root 的进程：线程是 None，nice 值总是 0，改它没有效果。
fn targets(cx: &Cx<'_>, which: i32, who: i32) -> Result<Vec<Target>, Errno> {
    let k = cx.kr();
    let threads = |g: &Rc<RefCell<ThreadGroup>>| -> Vec<Target> {
        let gb = g.borrow();
        if gb.tgid == INIT && gb.threads.is_empty() {
            return vec![(None, g.clone())];
        }
        gb.threads
            .iter()
            .filter_map(|tid| k.tasks.get(tid))
            .map(|t| (Some(t.clone()), g.clone()))
            .collect()
    };
    let found: Vec<_> = match which {
        PRIO_PROCESS => match who {
            0 => vec![(Some(cx.task.clone()), cx.t.group.clone())],
            tid => match k.tasks.get(&tid) {
                Some(t) => k
                    .groups
                    .get(&t.tgid)
                    .map(|g| (Some(t.clone()), g.clone()))
                    .into_iter()
                    .collect(),
                None if tid == INIT => k.groups.get(&INIT).map(threads).unwrap_or_default(),
                None => Vec::new(),
            },
        },
        PRIO_PGRP => {
            let pgid = if who == 0 {
                cx.t.group.borrow().pgid
            } else {
                who
            };
            k.groups
                .values()
                .filter(|g| g.borrow().pgid == pgid)
                .flat_map(threads)
                .collect()
        }
        PRIO_USER => {
            let uid = if who == 0 { cx.cred().ruid } else { who as u32 };
            k.groups
                .values()
                .filter(|g| g.borrow().cred.ruid == uid)
                .flat_map(threads)
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
        .map(|(t, _)| t.as_ref().map_or(0, |t| t.nice.get()))
        .min()
        .unwrap();
    Ok((20 - nice) as u64)
}

pub(super) fn setpriority(cx: &mut Cx<'_>, which: i32, who: i32, nice: i32) -> SysResult {
    let nice = nice.clamp(-20, 19);
    let cred = cx.cred();
    let mut result = Ok(0);
    for (t, g) in targets(cx, which, who)? {
        let g = g.borrow();
        let current = t.as_ref().map_or(0, |t| t.nice.get());
        // 别人的进程要求有效用户 ID 相同；提高优先级（降低 nice）要在目标的
        // RLIMIT_NICE 之内，或者有 CAP_SYS_NICE（Linux 的 can_nice）
        if !same_owner(&cred, &g.cred) {
            result = Err(Errno::EPERM);
        } else if nice < current
            && (20 - nice) as u64 > g.rlimits[RLIMIT_NICE as usize].0
            && !cred.capable(cap::SYS_NICE)
        {
            result = Err(Errno::EACCES);
        } else if let Some(t) = t {
            t.nice.set(nice);
            apply_host(&t);
        }
    }
    result
}

// ---- IO 优先级（Linux 6.1 的 block/ioprio.c、blk-ioc.c） ----

const IOPRIO_WHO_PROCESS: i32 = 1;
const IOPRIO_WHO_PGRP: i32 = 2;
const IOPRIO_WHO_USER: i32 = 3;

const IOPRIO_CLASS_SHIFT: u32 = 13;
const IOPRIO_CLASS_NONE: u16 = 0;
const IOPRIO_CLASS_RT: u16 = 1;
const IOPRIO_CLASS_BE: u16 = 2;
const IOPRIO_CLASS_IDLE: u16 = 3;
/// RT、BE 的级别数（0 最高）。
const IOPRIO_NR_LEVELS: u16 = 8;

fn ioprio_class(ioprio: u16) -> u16 {
    (ioprio >> IOPRIO_CLASS_SHIFT) & 7
}

fn ioprio_value(class: u16, data: u16) -> u16 {
    (class << IOPRIO_CLASS_SHIFT) | data
}

/// ioprio_set、ioprio_get 作用的线程。与 Linux 一样，`IOPRIO_WHO_PROCESS` 是一个线程，
/// `IOPRIO_WHO_PGRP` 是进程组里所有进程的所有线程，`IOPRIO_WHO_USER` 是真实用户 ID 为它的
/// 所有线程（看不到的 pid 命名空间里的不算）。内置的 init 没有线程，是 None。找不到是
/// ESRCH。`uid` 是 `IOPRIO_WHO_USER` 时要找的内核用户 ID（set、get 对 who = 0 的理解不同，
/// 由调用者算好）。
fn ioprio_targets(
    cx: &Cx<'_>,
    which: i32,
    who: i32,
    uid: Option<u32>,
) -> Result<Vec<Target>, Errno> {
    let k = cx.kr();
    let threads = |g: &Rc<RefCell<ThreadGroup>>| -> Vec<Target> {
        let gb = g.borrow();
        if gb.tgid == INIT && gb.threads.is_empty() {
            return vec![(None, g.clone())];
        }
        gb.threads
            .iter()
            .filter_map(|tid| k.tasks.get(tid))
            .map(|t| (Some(t.clone()), g.clone()))
            .collect()
    };
    let found: Vec<_> = match which {
        IOPRIO_WHO_PROCESS => match who {
            0 => vec![(Some(cx.task.clone()), cx.t.group.clone())],
            tid => match k.tasks.get(&tid) {
                Some(t) => k
                    .groups
                    .get(&t.tgid)
                    .map(|g| (Some(t.clone()), g.clone()))
                    .into_iter()
                    .collect(),
                None if tid == INIT => k.groups.get(&INIT).map(threads).unwrap_or_default(),
                None => Vec::new(),
            },
        },
        IOPRIO_WHO_PGRP => {
            let pgid = if who == 0 {
                cx.t.group.borrow().pgid
            } else {
                who
            };
            k.groups
                .values()
                .filter(|g| g.borrow().pgid == pgid)
                .flat_map(threads)
                .collect()
        }
        IOPRIO_WHO_USER => match uid {
            Some(uid) => k
                .groups
                .values()
                .filter(|g| {
                    let g = g.borrow();
                    g.cred.ruid == uid && cx.pid_out(g.tgid) != 0
                })
                .flat_map(threads)
                .collect(),
            None => Vec::new(),
        },
        _ => return Err(Errno::EINVAL),
    };
    if found.is_empty() {
        return Err(Errno::ESRCH);
    }
    Ok(found)
}

/// ioprio_set 的值本身合不合法、调用者能不能设（`ioprio_check_cap`）：RT 要
/// `CAP_SYS_ADMIN` 或 `CAP_SYS_NICE`，RT、BE 的级别是 0–7，NONE 的级别只能是 0，IDLE 的
/// 级别不检查。
fn ioprio_check(cred: &Cred, ioprio: u16) -> Result<(), Errno> {
    let data = ioprio & ((1 << IOPRIO_CLASS_SHIFT) - 1);
    match ioprio_class(ioprio) {
        IOPRIO_CLASS_RT | IOPRIO_CLASS_BE => {
            if ioprio_class(ioprio) == IOPRIO_CLASS_RT
                && !cred.capable(cap::SYS_ADMIN)
                && !cred.capable(cap::SYS_NICE)
            {
                return Err(Errno::EPERM);
            }
            if data >= IOPRIO_NR_LEVELS {
                return Err(Errno::EINVAL);
            }
            Ok(())
        }
        IOPRIO_CLASS_IDLE => Ok(()),
        IOPRIO_CLASS_NONE if data == 0 => Ok(()),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn ioprio_set(cx: &mut Cx<'_>, which: i32, who: i32, ioprio: i32) -> SysResult {
    let cred = cx.cred();
    // io_context 里的是 unsigned short，高位丢掉（检查只看第 0–15 位）
    let ioprio = ioprio as u16;
    ioprio_check(&cred, ioprio)?;
    // Linux 在这里用 make_kuid(current_user_ns(), who)：who = 0 是 root，不是自己
    let uid = (which == IOPRIO_WHO_USER)
        .then(|| cred.uid_in(who as u32))
        .flatten();
    for (t, g) in ioprio_targets(cx, which, who, uid)? {
        // set_task_ioprio：目标的真实用户 ID 要等于自己的有效或真实用户 ID，或者有
        // CAP_SYS_NICE；进程组、用户的第一次失败就返回，前面的已经改了
        let target = g.borrow().cred.ruid;
        if target != cred.euid && target != cred.ruid && !cred.capable(cap::SYS_NICE) {
            return Err(Errno::EPERM);
        }
        if let Some(t) = t {
            set_task_ioprio(cx, &t, ioprio);
        }
    }
    Ok(0)
}

/// 记下线程 `t` 的 IO 优先级（没有 io_context 时建一个），交给宿主线程。`CLONE_IO` 共用
/// 这一份的线程也一起交给宿主。
fn set_task_ioprio(cx: &Cx<'_>, t: &Rc<Task>, ioprio: u16) {
    let shared = {
        let mut slot = t.ioprio.borrow_mut();
        let cell = slot.get_or_insert_with(|| Rc::new(Cell::new(0)));
        cell.set(ioprio);
        Rc::strong_count(cell) > 1
    };
    if shared {
        let mine = t.ioprio.borrow().clone();
        for other in cx.kr().tasks.values() {
            let same =
                matches!((&*other.ioprio.borrow(), &mine), (Some(a), Some(b)) if Rc::ptr_eq(a, b));
            if same {
                apply_host_ioprio(other, false);
            }
        }
    } else {
        apply_host_ioprio(t, false);
    }
}

/// 线程的原始 IO 优先级：没有设置过是 0（`IOPRIO_DEFAULT`，NONE 类）。
fn raw_ioprio(t: &Task) -> u16 {
    t.ioprio.borrow().as_ref().map_or(0, |c| c.get())
}

/// 线程实际的 IO 优先级（`__get_task_ioprio`）：NONE 类按调度策略和 nice 值推出来，
/// `SCHED_IDLE` 是 IDLE，实时策略是 RT，其余是 BE，级别是 (nice + 20) / 5。内置的 init
/// 是 BE 4。
fn effective_ioprio(t: Option<&Task>) -> u16 {
    let Some(t) = t else {
        return ioprio_value(IOPRIO_CLASS_BE, 4);
    };
    let raw = raw_ioprio(t);
    if ioprio_class(raw) != IOPRIO_CLASS_NONE {
        return raw;
    }
    let class = match t.sched.get().0 & !SCHED_RESET_ON_FORK {
        SCHED_IDLE => IOPRIO_CLASS_IDLE,
        SCHED_FIFO | SCHED_RR | SCHED_DEADLINE => IOPRIO_CLASS_RT,
        _ => IOPRIO_CLASS_BE,
    };
    ioprio_value(class, ((t.nice.get() + 20) / 5) as u16)
}

pub(super) fn ioprio_get(cx: &mut Cx<'_>, which: i32, who: i32) -> SysResult {
    let cred = cx.cred();
    // 这里 who = 0 是自己的真实用户（current_user()）
    let uid = (which == IOPRIO_WHO_USER)
        .then(|| {
            if who == 0 {
                Some(cred.ruid)
            } else {
                cred.uid_in(who as u32)
            }
        })
        .flatten();
    let targets = ioprio_targets(cx, which, who, uid)?;
    // 一个线程时是它原来设的值（`get_task_raw_ioprio`），进程组和用户是实际的优先级里
    // 最高的（数值最小，`ioprio_best`）
    let prio = if which == IOPRIO_WHO_PROCESS {
        targets[0].0.as_deref().map_or(0, raw_ioprio)
    } else {
        targets
            .iter()
            .map(|(t, _)| effective_ioprio(t.as_deref()))
            .min()
            .unwrap()
    };
    Ok(prio as u64)
}

/// 新线程的 IO 优先级（`copy_io`）：创建者有 io_context 时，`CLONE_IO` 共用它，否则类别
/// 不是 NONE 的复制一份；创建者没有的，新线程也没有。
pub(super) fn inherit_ioprio(parent: &Option<Rc<Cell<u16>>>, flags: u64) -> Option<Rc<Cell<u16>>> {
    const CLONE_IO: u64 = 0x8000_0000;
    let p = parent.as_ref()?;
    if flags & CLONE_IO != 0 {
        Some(p.clone())
    } else {
        (ioprio_class(p.get()) != IOPRIO_CLASS_NONE).then(|| Rc::new(Cell::new(p.get())))
    }
}

/// 按 guest 线程的 IO 优先级设置它的宿主线程的：RT 降成同一级的 BE（与 nice 值一样，
/// guest 不能让宿主线程的 IO 压过界面），NONE 让宿主按 nice 值推。`reset` 时没设置过的也
/// 写一次（线程池里复用的宿主线程还留着上一个 guest 线程的）。
fn apply_host_ioprio(t: &Task, reset: bool) {
    let tid = t.host_tid.get();
    if tid == 0 || (!reset && t.ioprio.borrow().is_none()) {
        return;
    }
    let raw = raw_ioprio(t);
    let host = if ioprio_class(raw) == IOPRIO_CLASS_RT {
        ioprio_value(IOPRIO_CLASS_BE, raw & ((1 << IOPRIO_CLASS_SHIFT) - 1))
    } else {
        raw
    };
    appine_hal::set_thread_ioprio(tid, host);
}

/// guest 提高优先级时，宿主线程的 nice 值最低到这里：不超过 Android 上与显示有关的
/// 线程（-4 到 -10），guest 的忙循环不会拖慢界面。
const HOST_NICE_FLOOR: i32 = -4;

/// 宿主线程开始运行 guest 线程 `t`：它继承的是创建它的宿主线程的优先级、IO 优先级和
/// 亲和性（`reused` 时是上一个 guest 线程的），换成 `t` 自己的。
pub(crate) fn host_started(t: &Task, reused: bool) {
    apply_host(t);
    apply_host_ioprio(t, reused);
    let mask = t.affinity.get() & online_cpus();
    // 线程池里复用的宿主线程还留着上一个 guest 线程的亲和性
    if mask != online_cpus() || reused {
        appine_hal::set_thread_affinity(t.host_tid.get(), mask);
    }
}

/// 按 guest 线程的 nice 值和调度策略设置它的宿主线程的 nice 值。`SCHED_IDLE` 用宿主
/// 最低的优先级。宿主线程还没有开始运行时，由它开始时设置。
fn apply_host(t: &Task) {
    let tid = t.host_tid.get();
    if tid == 0 {
        return;
    }
    let nice = if t.sched.get().0 & !SCHED_RESET_ON_FORK == SCHED_IDLE {
        19
    } else {
        t.nice.get().max(HOST_NICE_FLOOR)
    };
    appine_hal::set_thread_nice(tid, nice);
}

/// `sched_*` 的目标线程：pid 0 是自己。内置的 init 没有线程，返回 None。僵尸进程（和
/// 组长已经退出的进程）也是 None：Linux 上回收之前还能找到组长，按默认的调度参数处理。
fn target(cx: &Cx<'_>, pid: i32) -> Result<Option<Rc<Task>>, Errno> {
    match pid {
        p if p < 0 => Err(Errno::EINVAL),
        0 => Ok(Some(cx.task.clone())),
        INIT => Ok(None),
        p => match cx.kr().tasks.get(&p) {
            Some(t) => Ok(Some(t.clone())),
            None if cx.kr().groups.contains_key(&p) => Ok(None),
            None => Err(Errno::ESRCH),
        },
    }
}

/// 线程所在进程的凭据。
fn cred_of(cx: &Cx<'_>, task: Option<&Rc<Task>>) -> Cred {
    task.and_then(|t| cx.kr().groups.get(&t.tgid))
        .map_or_else(Cred::root, |g| g.borrow().cred.clone())
}

/// 新线程继承的调度策略和 nice 值：`SCHED_RESET_ON_FORK` 时实时策略回到普通策略，
/// 负的 nice 值回到 0。
pub(super) fn inherit((policy, priority): (i32, i32), nice: i32) -> ((i32, i32), i32) {
    if policy & SCHED_RESET_ON_FORK != 0 {
        let base = policy & !SCHED_RESET_ON_FORK;
        if matches!(base, SCHED_FIFO | SCHED_RR) {
            return ((SCHED_OTHER, 0), 0);
        }
        return ((base, priority), nice.max(0));
    }
    ((policy, priority), nice)
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
        apply_host(t);
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
        let tid = t.tid.get();
        setpriority(cx, PRIO_PROCESS, tid, nice)?;
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
    let nice = task.as_ref().map_or(0, |t| t.nice.get());
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
    let cpus = appine_hal::host_cpus().len();
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
        if t.host_tid.get() != 0 {
            appine_hal::set_thread_affinity(t.host_tid.get(), new);
        }
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
