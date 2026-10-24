//! 调度类 syscall：nice 值（getpriority、setpriority）和 `sched_*`。
//!
//! guest 线程就是普通的宿主线程，由宿主调度。nice 值只记录下来（`/proc/<pid>/stat`
//! 和 getpriority 能看到），不影响宿主调度；实时调度策略不支持（EPERM）。

use std::cell::RefCell;
use std::rc::Rc;

use super::{SysResult, put, write_pair};
use crate::errno::Errno;
use crate::kernel::Cx;
use crate::process::ThreadGroup;

const PRIO_PROCESS: i32 = 0;
const PRIO_PGRP: i32 = 1;
const PRIO_USER: i32 = 2;

const SCHED_OTHER: i32 = 0;
const SCHED_FIFO: i32 = 1;
const SCHED_RR: i32 = 2;
const SCHED_BATCH: i32 = 3;
const SCHED_IDLE: i32 = 5;
const SCHED_RESET_ON_FORK: i32 = 0x4000_0000;

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
        // 所有进程都属于 root
        PRIO_USER if who == 0 => k.groups.values().filter(live).cloned().collect(),
        PRIO_USER => Vec::new(),
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
    for g in targets(cx, which, who)? {
        g.borrow_mut().nice = nice;
    }
    Ok(0)
}

/// 检查 `sched_*` 的 pid 参数：0 是自己，否则必须是活着的线程。
fn check_pid(cx: &Cx<'_>, pid: i32) -> Result<(), Errno> {
    if pid < 0 {
        return Err(Errno::EINVAL);
    }
    if pid != 0 && !cx.kr().tasks.contains_key(&pid) {
        return Err(Errno::ESRCH);
    }
    Ok(())
}

/// 读取 `struct sched_param` 的优先级。
fn read_priority(cx: &Cx<'_>, param: u64) -> Result<i32, Errno> {
    if param == 0 {
        return Err(Errno::EINVAL);
    }
    Ok(i32::from_le_bytes(super::get(cx, param)?))
}

pub(super) fn sched_setscheduler(cx: &mut Cx<'_>, pid: i32, policy: i32, param: u64) -> SysResult {
    check_pid(cx, pid)?;
    let priority = read_priority(cx, param)?;
    match policy & !SCHED_RESET_ON_FORK {
        SCHED_OTHER | SCHED_BATCH | SCHED_IDLE if priority == 0 => Ok(0),
        SCHED_OTHER | SCHED_BATCH | SCHED_IDLE => Err(Errno::EINVAL),
        SCHED_FIFO | SCHED_RR => Err(Errno::EPERM),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn sched_setparam(cx: &mut Cx<'_>, pid: i32, param: u64) -> SysResult {
    check_pid(cx, pid)?;
    match read_priority(cx, param)? {
        0 => Ok(0),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn sched_getscheduler(cx: &mut Cx<'_>, pid: i32) -> SysResult {
    check_pid(cx, pid)?;
    Ok(SCHED_OTHER as u64)
}

pub(super) fn sched_getparam(cx: &mut Cx<'_>, pid: i32, param: u64) -> SysResult {
    check_pid(cx, pid)?;
    put(cx, param, 0i32.to_le_bytes()).map(|()| 0)
}

/// 线程由宿主调度：接受亲和性设置，但不起作用。
pub(super) fn sched_setaffinity(cx: &mut Cx<'_>, pid: i32) -> SysResult {
    check_pid(cx, pid).map(|()| 0)
}

pub(super) fn sched_priority_range(policy: i32, max: bool) -> SysResult {
    match policy {
        SCHED_FIFO | SCHED_RR => Ok(if max { 99 } else { 1 }),
        SCHED_OTHER | SCHED_BATCH | SCHED_IDLE => Ok(0),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn sched_rr_get_interval(cx: &mut Cx<'_>, pid: i32, ts: u64) -> SysResult {
    check_pid(cx, pid)?;
    // 不是 SCHED_RR 的线程没有时间片
    write_pair(cx, ts, 0, 0).map(|()| 0)
}
