//! 凭据类 syscall：用户和组 ID、附加组、能力（规则见 [`crate::cred`]）。

use super::{SysResult, get, put};
use crate::cred::{Cred, NGROUPS_MAX};
use crate::errno::Errno;
use crate::kernel::Cx;

/// capget、capset 的版本：v1 只有低 32 位，v2、v3 是两组 32 位。
const CAPABILITY_VERSION_1: u32 = 0x1998_0330;
const CAPABILITY_VERSION_2: u32 = 0x2007_1026;
const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// 不修改的 ID 参数（-1）是 `None`。
fn id_arg(id: u64) -> Option<u32> {
    (id as u32 != u32::MAX).then_some(id as u32)
}

/// 修改当前进程的凭据：`f` 失败时凭据保持不变。
fn update<T>(cx: &Cx<'_>, f: impl FnOnce(&mut Cred) -> Result<T, Errno>) -> Result<T, Errno> {
    let mut cred = cx.cred();
    let r = f(&mut cred)?;
    cx.t.group.borrow_mut().cred = cred;
    Ok(r)
}

pub(super) fn getuid(cx: &mut Cx<'_>, effective: bool) -> SysResult {
    let c = cx.cred();
    Ok(if effective { c.euid } else { c.ruid } as u64)
}

pub(super) fn getgid(cx: &mut Cx<'_>, effective: bool) -> SysResult {
    let c = cx.cred();
    Ok(if effective { c.egid } else { c.rgid } as u64)
}

pub(super) fn setuid(cx: &mut Cx<'_>, uid: u64) -> SysResult {
    let uid = id_arg(uid).ok_or(Errno::EINVAL)?;
    update(cx, |c| c.setuid(uid)).map(|()| 0)
}

pub(super) fn setgid(cx: &mut Cx<'_>, gid: u64) -> SysResult {
    let gid = id_arg(gid).ok_or(Errno::EINVAL)?;
    update(cx, |c| c.setgid(gid)).map(|()| 0)
}

pub(super) fn setreuid(cx: &mut Cx<'_>, r: u64, e: u64) -> SysResult {
    update(cx, |c| c.setreuid(id_arg(r), id_arg(e))).map(|()| 0)
}

pub(super) fn setregid(cx: &mut Cx<'_>, r: u64, e: u64) -> SysResult {
    update(cx, |c| c.setregid(id_arg(r), id_arg(e))).map(|()| 0)
}

pub(super) fn setresuid(cx: &mut Cx<'_>, r: u64, e: u64, s: u64) -> SysResult {
    update(cx, |c| c.setresuid(id_arg(r), id_arg(e), id_arg(s))).map(|()| 0)
}

pub(super) fn setresgid(cx: &mut Cx<'_>, r: u64, e: u64, s: u64) -> SysResult {
    update(cx, |c| c.setresgid(id_arg(r), id_arg(e), id_arg(s))).map(|()| 0)
}

/// getresuid、getresgid：三个 ID 分别写到三个地址。
pub(super) fn getres(cx: &mut Cx<'_>, gid: bool, addrs: [u64; 3]) -> SysResult {
    let c = cx.cred();
    let ids = if gid {
        [c.rgid, c.egid, c.sgid]
    } else {
        [c.ruid, c.euid, c.suid]
    };
    for (addr, id) in addrs.into_iter().zip(ids) {
        put(cx, addr, id.to_le_bytes())?;
    }
    Ok(0)
}

pub(super) fn setfsuid(cx: &mut Cx<'_>, uid: u64) -> SysResult {
    update(cx, |c| Ok(c.setfsuid(id_arg(uid)) as u64))
}

pub(super) fn setfsgid(cx: &mut Cx<'_>, gid: u64) -> SysResult {
    update(cx, |c| Ok(c.setfsgid(id_arg(gid)) as u64))
}

pub(super) fn getgroups(cx: &mut Cx<'_>, size: i32, list: u64) -> SysResult {
    let groups = cx.cred().groups;
    let size = usize::try_from(size).map_err(|_| Errno::EINVAL)?;
    if size == 0 {
        return Ok(groups.len() as u64);
    }
    if size < groups.len() {
        return Err(Errno::EINVAL);
    }
    for (i, gid) in groups.iter().enumerate() {
        put(cx, list + 4 * i as u64, gid.to_le_bytes())?;
    }
    Ok(groups.len() as u64)
}

pub(super) fn setgroups(cx: &mut Cx<'_>, size: i32, list: u64) -> SysResult {
    let size = usize::try_from(size)
        .ok()
        .filter(|&n| n <= NGROUPS_MAX)
        .ok_or(Errno::EINVAL)?;
    let mut groups = Vec::with_capacity(size);
    for i in 0..size {
        let gid = u32::from_le_bytes(get(cx, list + 4 * i as u64)?);
        groups.push(id_arg(gid as u64).ok_or(Errno::EINVAL)?);
    }
    update(cx, |c| c.setgroups(groups)).map(|()| 0)
}

/// capget、capset 的头部：检查版本（不认识时写回内核的版本并返回 EINVAL），返回
/// 数据的组数和 pid。
fn cap_header(cx: &Cx<'_>, hdr: u64) -> Result<(usize, i32), Errno> {
    let h: [u8; 8] = get(cx, hdr)?;
    let version = u32::from_le_bytes(h[..4].try_into().unwrap());
    let pid = i32::from_le_bytes(h[4..].try_into().unwrap());
    match version {
        CAPABILITY_VERSION_1 => Ok((1, pid)),
        CAPABILITY_VERSION_2 | CAPABILITY_VERSION_3 => Ok((2, pid)),
        _ => {
            put(cx, hdr, CAPABILITY_VERSION_3.to_le_bytes())?;
            Err(Errno::EINVAL)
        }
    }
}

pub(super) fn capget(cx: &mut Cx<'_>, hdr: u64, data: u64) -> SysResult {
    let (words, pid) = cap_header(cx, hdr)?;
    if pid < 0 {
        return Err(Errno::EINVAL);
    }
    let cred = if pid == 0 {
        cx.cred()
    } else {
        let k = cx.kr();
        let task = k.tasks.get(&pid).ok_or(Errno::ESRCH)?;
        let group = k.groups.get(&task.tgid).ok_or(Errno::ESRCH)?;
        group.borrow().cred.clone()
    };
    // 数据指针为空时只是探测版本
    if data == 0 {
        return Ok(0);
    }
    for i in 0..words {
        let mut b = [0u8; 12];
        for (j, set) in [cred.cap_effective, cred.cap_permitted, cred.cap_inheritable]
            .into_iter()
            .enumerate()
        {
            b[j * 4..j * 4 + 4].copy_from_slice(&((set >> (32 * i)) as u32).to_le_bytes());
        }
        put(cx, data + 12 * i as u64, b)?;
    }
    Ok(0)
}

pub(super) fn capset(cx: &mut Cx<'_>, hdr: u64, data: u64) -> SysResult {
    let (words, pid) = cap_header(cx, hdr)?;
    if pid != 0 && pid != cx.tid() {
        return Err(Errno::EPERM);
    }
    let mut sets = [0u64; 3];
    for i in 0..words {
        let b: [u8; 12] = get(cx, data + 12 * i as u64)?;
        for (j, set) in sets.iter_mut().enumerate() {
            let word = u32::from_le_bytes(b[j * 4..j * 4 + 4].try_into().unwrap());
            *set |= (word as u64) << (32 * i);
        }
    }
    let [effective, permitted, inheritable] = sets;
    update(cx, |c| c.capset(effective, permitted, inheritable)).map(|()| 0)
}
