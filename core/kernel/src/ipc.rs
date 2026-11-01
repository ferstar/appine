//! System V IPC（svipc(7)）：消息队列、信号量集和共享内存段的内核对象。
//!
//! 三类对象各有一张表。id 是 `序号 * 32768 + 下标`，与 Linux 相同：下标重新分配出去
//! 之后，旧的 id 不再有效。权限按 `ipc_perm` 检查（有效用户 ID，`CAP_IPC_OWNER` 可以
//! 越过）。共享内存段是一段宿主内存（[`SharedMemory`]），shmat 把它映射进地址空间；
//! 所有 guest 进程都在同一个宿主进程里，看到的就是同一段内存。段被 shmat 了几次不
//! 单独记录，而是数各个地址空间里的映射，所以 fork、execve 和进程退出不需要额外处理。
//! syscall 见 `syscall::ipc`。

use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use crate::cred::{Cred, cap};
use crate::errno::Errno;
use crate::kernel::{Kernel, Tid, request_wake};
use crate::mm::SharedMemory;

pub(crate) const IPC_PRIVATE: i32 = 0;
pub(crate) const IPC_CREAT: i32 = 0o1000;
pub(crate) const IPC_EXCL: i32 = 0o2000;
pub(crate) const IPC_NOWAIT: i32 = 0o4000;
/// 共享内存段已被 IPC_RMID，等最后一个映射消失（记在 mode 里）。
pub(crate) const SHM_DEST: u32 = 0o1000;
/// 共享内存段被 SHM_LOCK 锁住（记在 mode 里）。
pub(crate) const SHM_LOCKED: u32 = 0o2000;
/// semop 的 `SEM_UNDO`：进程退出时撤销这次操作。
pub(crate) const SEM_UNDO: i16 = 0x1000;

/// 下标的个数（`IPCMNI`）。
const IPCMNI: i32 = 32768;

/// 固定的限制，与 Linux 相同。
pub(crate) mod limits {
    pub const SEMVMX: i32 = 32767;
    pub const SEMAEM: i32 = 32767;
    pub const SHMMIN: u64 = 1;
}

/// 可以经由 `/proc/sys/kernel` 修改的限制（`msgmax`、`sem` 等），默认值与 Linux 相同。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tunables {
    pub msgmax: u64,
    pub msgmnb: u64,
    pub msgmni: u64,
    /// `sem`：semmsl、semmns、semopm、semmni。
    pub sem: [u64; 4],
    pub shmmax: u64,
    pub shmall: u64,
    pub shmmni: u64,
}

impl Default for Tunables {
    fn default() -> Tunables {
        Tunables {
            msgmax: 8192,
            msgmnb: 16384,
            msgmni: 32000,
            sem: [32000, 1_024_000_000, 500, 32000],
            shmmax: u64::MAX - (1 << 24),
            shmall: u64::MAX - (1 << 24),
            shmmni: 4096,
        }
    }
}

impl Tunables {
    /// `/proc/sys/kernel/<name>` 的内容；不是这里的文件时返回 None。
    pub fn read(&self, name: &[u8]) -> Option<String> {
        Some(match name {
            b"msgmax" => format!("{}\n", self.msgmax),
            b"msgmnb" => format!("{}\n", self.msgmnb),
            b"msgmni" => format!("{}\n", self.msgmni),
            b"sem" => {
                let [a, b, c, d] = self.sem;
                format!("{a}\t{b}\t{c}\t{d}\n")
            }
            b"shmmax" => format!("{}\n", self.shmmax),
            b"shmall" => format!("{}\n", self.shmall),
            b"shmmni" => format!("{}\n", self.shmmni),
            _ => return None,
        })
    }

    /// 写 `/proc/sys/kernel/<name>`：值不是非负整数，或者 `*mni` 超过 IPCMNI 时 EINVAL。
    pub fn write(&mut self, name: &[u8], data: &[u8]) -> Result<(), Errno> {
        let text = std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?;
        let nums: Vec<u64> = text
            .split_whitespace()
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|_| Errno::EINVAL)?;
        let one = || match nums[..] {
            [v] => Ok(v),
            _ => Err(Errno::EINVAL),
        };
        let mni = |v: u64| {
            if v <= IPCMNI as u64 {
                Ok(v)
            } else {
                Err(Errno::EINVAL)
            }
        };
        match name {
            b"msgmax" => self.msgmax = one()?,
            b"msgmnb" => self.msgmnb = one()?,
            b"msgmni" => self.msgmni = mni(one()?)?,
            b"sem" => {
                let [a, b, c, d] = nums[..] else {
                    return Err(Errno::EINVAL);
                };
                self.sem = [a, b, c, mni(d)?];
            }
            b"shmmax" => self.shmmax = one()?,
            b"shmall" => self.shmall = one()?,
            b"shmmni" => self.shmmni = mni(one()?)?,
            _ => return Err(Errno::EINVAL),
        }
        Ok(())
    }
}

/// 当前的 CLOCK_REALTIME 秒数（`*_stime` 等时间戳）。
pub(crate) fn now() -> i64 {
    appine_hal::clock_gettime(0).map_or(0, |(s, _)| s)
}

/// `struct ipc_perm`。
#[derive(Clone, Debug)]
pub(crate) struct Perm {
    pub key: i32,
    pub uid: u32,
    pub gid: u32,
    pub cuid: u32,
    pub cgid: u32,
    /// 权限位（低 9 位）和对象自己的标志位（`SHM_DEST` 等）。
    pub mode: u32,
    pub seq: u16,
}

impl Perm {
    /// `ipcperms`：`flag` 是要求的权限（`S_IRUGO`、`S_IWUGO` 或者 msgget 等的权限位）。
    pub fn check(&self, cred: &Cred, flag: u32) -> Result<(), Errno> {
        let want = (flag >> 6 | flag >> 3 | flag) & 7;
        let granted = if cred.euid == self.cuid || cred.euid == self.uid {
            self.mode >> 6
        } else if cred.in_group(self.cgid) || cred.in_group(self.gid) {
            self.mode >> 3
        } else {
            self.mode
        };
        if want & !granted & 7 != 0 && !cred.capable(cap::IPC_OWNER) {
            return Err(Errno::EACCES);
        }
        Ok(())
    }

    /// IPC_SET、IPC_RMID：创建者、属主，或者有 `CAP_SYS_ADMIN`。
    pub fn may_change(&self, cred: &Cred) -> Result<(), Errno> {
        if cred.euid == self.cuid || cred.euid == self.uid || cred.capable(cap::SYS_ADMIN) {
            Ok(())
        } else {
            Err(Errno::EPERM)
        }
    }

    /// 按 asm-generic 的 `struct ipc64_perm`（48 字节）编码，ID 按调用者 `cred` 的用户命名空间
    /// 换算（Linux 的 `kernel_to_ipc64_perm`）。
    pub fn encode(&self, cred: &Cred) -> [u8; 48] {
        let mut b = [0u8; 48];
        for (i, v) in [
            self.key as u32,
            cred.uid_out(self.uid),
            cred.gid_out(self.gid),
            cred.uid_out(self.cuid),
            cred.gid_out(self.cgid),
            self.mode,
        ]
        .into_iter()
        .enumerate()
        {
            b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        b[24..26].copy_from_slice(&self.seq.to_le_bytes());
        b
    }

    /// IPC_SET：从 `struct ipc64_perm` 取属主、属组和权限位。属主、属组按调用者 `cred` 的用户
    /// 命名空间换成内核 ID，没有映射是 EINVAL（Linux 的 `ipc_update_perm`）。
    pub fn set(&mut self, b: &[u8], cred: &Cred) -> Result<(), Errno> {
        let word = |off: usize| u32::from_le_bytes(b[off..off + 4].try_into().unwrap());
        let (Some(uid), Some(gid)) = (cred.uid_in(word(4)), cred.gid_in(word(8))) else {
            return Err(Errno::EINVAL);
        };
        self.uid = uid;
        self.gid = gid;
        self.mode = (self.mode & !0o777) | (word(20) & 0o777);
        Ok(())
    }
}

/// 一类 IPC 对象的表。
pub(crate) struct Table<T> {
    slots: Vec<Option<(Perm, T)>>,
    /// 每个下标下一次分配时用的序号。
    seqs: Vec<u16>,
}

impl<T> Default for Table<T> {
    fn default() -> Table<T> {
        Table {
            slots: Vec::new(),
            seqs: Vec::new(),
        }
    }
}

impl<T> Table<T> {
    fn index_of(id: i32) -> Option<(usize, u16)> {
        (id >= 0).then_some(((id % IPCMNI) as usize, (id / IPCMNI) as u16))
    }

    /// id 对应的对象；不存在（包括已经删除、下标被重新分配）时 EINVAL。
    pub fn get(&self, id: i32) -> Result<&(Perm, T), Errno> {
        let (i, seq) = Self::index_of(id).ok_or(Errno::EINVAL)?;
        self.slots
            .get(i)
            .and_then(Option::as_ref)
            .filter(|(p, _)| p.seq == seq)
            .ok_or(Errno::EINVAL)
    }

    pub fn get_mut(&mut self, id: i32) -> Result<&mut (Perm, T), Errno> {
        let (i, seq) = Self::index_of(id).ok_or(Errno::EINVAL)?;
        self.slots
            .get_mut(i)
            .and_then(Option::as_mut)
            .filter(|(p, _)| p.seq == seq)
            .ok_or(Errno::EINVAL)
    }

    /// 按下标找（`*_STAT` 命令），返回完整的 id。
    pub fn by_index(&self, index: i32) -> Result<(i32, &(Perm, T)), Errno> {
        let slot = usize::try_from(index)
            .ok()
            .and_then(|i| self.slots.get(i))
            .and_then(Option::as_ref)
            .ok_or(Errno::EINVAL)?;
        Ok((slot.0.seq as i32 * IPCMNI + index, slot))
    }

    /// 用到的最大下标（`IPC_INFO` 等的返回值）。
    pub fn max_index(&self) -> i32 {
        self.slots.iter().rposition(Option::is_some).unwrap_or(0) as i32
    }

    fn find(&self, key: i32) -> Option<i32> {
        self.iter()
            .find(|(_, p, _)| p.key == key)
            .map(|(id, _, _)| id)
    }

    fn insert(&mut self, mut perm: Perm, obj: T) -> Result<i32, Errno> {
        let i = match self.slots.iter().position(Option::is_none) {
            Some(i) => i,
            None if self.slots.len() < IPCMNI as usize => {
                self.slots.push(None);
                self.seqs.push(0);
                self.slots.len() - 1
            }
            None => return Err(Errno::ENOSPC),
        };
        perm.seq = self.seqs[i];
        // 序号要能放进 id：(序号 * 32768 + 下标) 不能溢出 i32
        self.seqs[i] = (self.seqs[i] + 1) % (i32::MAX / IPCMNI) as u16;
        let id = perm.seq as i32 * IPCMNI + i as i32;
        self.slots[i] = Some((perm, obj));
        Ok(id)
    }

    pub fn remove(&mut self, id: i32) -> Option<(Perm, T)> {
        self.get(id).ok()?;
        let (i, _) = Self::index_of(id)?;
        request_wake();
        self.slots[i].take()
    }

    /// 所有对象：(id, 权限, 对象)。
    pub fn iter(&self) -> impl Iterator<Item = (i32, &Perm, &T)> {
        self.slots.iter().enumerate().filter_map(|(i, s)| {
            s.as_ref()
                .map(|(p, o)| (p.seq as i32 * IPCMNI + i as i32, p, o))
        })
    }

    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// msgget、semget、shmget 的公共部分：按 key 找已有的对象（`existing` 做对象自己的
    /// 检查），或者新建一个（`create`）；最多同时存在 `max` 个（`msgmni` 等）。
    pub fn get_or_create(
        &mut self,
        key: i32,
        flags: i32,
        cred: &Cred,
        max: u64,
        existing: impl FnOnce(&T) -> Result<(), Errno>,
        create: impl FnOnce() -> Result<T, Errno>,
    ) -> Result<i32, Errno> {
        if key != IPC_PRIVATE {
            if let Some(id) = self.find(key) {
                if flags & IPC_CREAT != 0 && flags & IPC_EXCL != 0 {
                    return Err(Errno::EEXIST);
                }
                let (perm, obj) = self.get(id)?;
                existing(obj)?;
                perm.check(cred, flags as u32 & 0o777)?;
                return Ok(id);
            }
            if flags & IPC_CREAT == 0 {
                return Err(Errno::ENOENT);
            }
        }
        if self.len() as u64 >= max {
            return Err(Errno::ENOSPC);
        }
        let perm = Perm {
            key,
            uid: cred.euid,
            gid: cred.egid,
            cuid: cred.euid,
            cgid: cred.egid,
            mode: flags as u32 & 0o777,
            seq: 0,
        };
        let obj = create()?;
        self.insert(perm, obj)
    }
}

/// 消息队列里的一条消息。
pub(crate) struct Msg {
    pub mtype: i64,
    pub text: Vec<u8>,
}

/// 消息队列。
pub(crate) struct MsgQueue {
    pub msgs: VecDeque<Msg>,
    /// 队列容量（字节）。
    pub qbytes: u64,
    /// 队列里消息的总字节数。
    pub cbytes: u64,
    pub stime: i64,
    pub rtime: i64,
    pub ctime: i64,
    /// 最后一次 msgsnd、msgrcv 的进程。
    pub lspid: Tid,
    pub lrpid: Tid,
}

impl MsgQueue {
    /// 新的队列，容量是 `msgmnb`。
    pub fn new(qbytes: u64) -> MsgQueue {
        MsgQueue {
            msgs: VecDeque::new(),
            qbytes,
            cbytes: 0,
            stime: 0,
            rtime: 0,
            ctime: now(),
            lspid: 0,
            lrpid: 0,
        }
    }

    /// 还能不能放下一条 `len` 字节的消息（`msg_fits_queue`）。
    pub fn fits(&self, len: usize) -> bool {
        self.cbytes + len as u64 <= self.qbytes && (self.msgs.len() as u64) < self.qbytes
    }

    /// msgrcv 选中的消息：`mtype` 为 0 是第一条；大于 0 是第一条类型等于它的（`except`
    /// 时是不等于它的）；小于 0 是类型不超过 `-mtype` 的消息里类型最小的第一条。
    pub fn select(&self, mtype: i64, except: bool) -> Option<usize> {
        match mtype {
            0 => (!self.msgs.is_empty()).then_some(0),
            t if t > 0 => self.msgs.iter().position(|m| (m.mtype == t) != except),
            t => self
                .msgs
                .iter()
                .enumerate()
                .filter(|(_, m)| m.mtype <= t.saturating_neg())
                .min_by_key(|&(i, m)| (m.mtype, i))
                .map(|(i, _)| i),
        }
    }
}

/// 信号量集。
pub(crate) struct SemSet {
    pub vals: Vec<i32>,
    /// 最后一次操作每个信号量的进程。
    pub pids: Vec<Tid>,
    /// 在等每个信号量变大（semncnt）、变成 0（semzcnt）的线程数。
    pub ncnt: Vec<u32>,
    pub zcnt: Vec<u32>,
    pub otime: i64,
    pub ctime: i64,
}

/// semop 的一个操作（`struct sembuf`）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct SemOp {
    pub num: u16,
    pub op: i16,
    pub flg: i16,
}

impl SemSet {
    pub fn new(n: usize) -> SemSet {
        SemSet {
            vals: vec![0; n],
            pids: vec![0; n],
            ncnt: vec![0; n],
            zcnt: vec![0; n],
            otime: 0,
            ctime: now(),
        }
    }

    /// 尝试原子地完成所有操作（`perform_atomic_semop`）：全部完成返回 `Ok(None)`；
    /// 有一个要等待时什么都不改，返回它的下标；结果超出范围时 ERANGE。`undo` 是这个
    /// 进程对这个信号量集的撤销值，带 `SEM_UNDO` 的操作同时更新它。
    pub fn apply(
        &mut self,
        ops: &[SemOp],
        undo: &mut Vec<i32>,
        pid: Tid,
    ) -> Result<Option<usize>, Errno> {
        let mut vals = self.vals.clone();
        let mut adj = undo.clone();
        adj.resize(vals.len(), 0);
        for (i, o) in ops.iter().enumerate() {
            let v = &mut vals[o.num as usize];
            let op = o.op as i32;
            if op == 0 && *v != 0 || op < 0 && *v + op < 0 {
                return Ok(Some(i));
            }
            *v += op;
            if *v > limits::SEMVMX {
                return Err(Errno::ERANGE);
            }
            if o.flg & SEM_UNDO != 0 {
                let a = &mut adj[o.num as usize];
                *a -= op;
                if !(-limits::SEMAEM - 1..=limits::SEMAEM).contains(a) {
                    return Err(Errno::ERANGE);
                }
            }
        }
        self.vals = vals;
        *undo = adj;
        for o in ops {
            self.pids[o.num as usize] = pid;
        }
        self.otime = now();
        request_wake();
        Ok(None)
    }
}

/// 共享内存段。
pub(crate) struct ShmSeg {
    pub mem: SharedMemory,
    /// shmget 时的字节数（映射按页向上取整）。
    pub size: u64,
    pub atime: i64,
    pub dtime: i64,
    pub ctime: i64,
    /// 创建者和最后一次 shmat、shmdt 的进程。
    pub cpid: Tid,
    pub lpid: Tid,
}

/// 一个共享内存段：所在的 IPC 命名空间的编号和段的 id（各个命名空间的 id 各自编号）。
pub(crate) type ShmKey = (u64, i32);

/// 一个 IPC 命名空间里的所有对象（[`crate::nsproxy::IpcNs`]）：System V IPC 对象和 POSIX
/// 消息队列，以及它们的限制。
#[derive(Default)]
pub(crate) struct Ipc {
    pub msg: Table<MsgQueue>,
    pub sem: Table<SemSet>,
    pub shm: Table<ShmSeg>,
    /// SEM_UNDO 的撤销值：(进程, 信号量集) → 每个信号量的值。
    pub undo: BTreeMap<(Tid, i32), Vec<i32>>,
    pub tunables: Tunables,
    /// POSIX 消息队列，按名字（`syscall::mqueue`）。
    pub mqueues: BTreeMap<Vec<u8>, Rc<crate::cell::RefCell<crate::syscall::MessageQueue>>>,
    /// `/proc/sys/fs/mqueue/*`。
    pub mq_limits: crate::syscall::MqLimits,
    /// mqueue 文件系统的根目录的属主和时间。
    pub mq_dir: crate::cell::RefCell<crate::syscall::MqDir>,
}

impl Ipc {
    /// 信号量的值被 SETVAL、SETALL 直接设置：所有进程对它的撤销值清零。`num` 为 None
    /// 时是整个集合。
    pub fn clear_undo(&mut self, id: i32, num: Option<usize>) {
        for ((_, sid), adj) in self.undo.iter_mut() {
            if *sid == id {
                match num {
                    Some(n) => {
                        if let Some(a) = adj.get_mut(n) {
                            *a = 0;
                        }
                    }
                    None => adj.fill(0),
                }
            }
        }
    }

    /// 删除信号量集：同时丢掉它的撤销值。
    pub fn remove_sem(&mut self, id: i32) {
        self.sem.remove(id);
        self.undo.retain(|&(_, sid), _| sid != id);
    }

    /// 进程退出：按 SEM_UNDO 的撤销值调整信号量（结果限制在 0 到 SEMVMX 之间）。
    pub fn exit(&mut self, tgid: Tid) {
        let mine: Vec<(i32, Vec<i32>)> = self
            .undo
            .range((tgid, i32::MIN)..=(tgid, i32::MAX))
            .map(|(&(_, id), adj)| (id, adj.clone()))
            .collect();
        for (id, adj) in mine {
            self.undo.remove(&(tgid, id));
            if let Ok((_, set)) = self.sem.get_mut(id) {
                for (i, a) in adj.iter().enumerate().filter(|(_, a)| **a != 0) {
                    if let Some(v) = set.vals.get_mut(i) {
                        *v = (*v + a).clamp(0, limits::SEMVMX);
                        set.pids[i] = tgid;
                    }
                }
                set.otime = now();
                request_wake();
            }
        }
    }
}

/// 共享内存段被 shmat 的次数：数所有地址空间里的映射（每个地址空间只数一次）。
pub(crate) fn nattch(k: &Kernel, id: ShmKey) -> u64 {
    let mut seen: Vec<*const crate::cell::RefCell<crate::mm::Mm>> = Vec::new();
    let mut n = 0;
    for t in k.tasks.values() {
        let mm = t.res.borrow().mm.clone();
        if seen.contains(&Rc::as_ptr(&mm)) {
            continue;
        }
        seen.push(Rc::as_ptr(&mm));
        n += mm.borrow().shm_attachments(id) as u64;
    }
    n
}

/// 释放已经标记删除、不再有映射的共享内存段（各个 IPC 命名空间里的）。
pub(crate) fn reap_shm(k: &mut Kernel) {
    let doomed: Vec<ShmKey> = k
        .ipcs
        .iter()
        .flat_map(|(&ns, (_, ipc))| {
            ipc.shm
                .iter()
                .filter(|(_, p, _)| p.mode & SHM_DEST != 0)
                .map(move |(id, _, _)| (ns, id))
        })
        .collect();
    for key in doomed {
        if nattch(k, key) == 0 {
            k.ipc_mut(key.0).shm.remove(key.1);
        }
    }
}

/// 进程退出：各个 IPC 命名空间里它的 SEM_UNDO 的撤销值（[`Ipc::exit`]）。
pub(crate) fn exit(k: &mut Kernel, tgid: Tid) {
    for (_, ipc) in k.ipcs.values_mut() {
        ipc.exit(tgid);
    }
}

/// IPC 命名空间 `ns` 的 `/proc/sysvipc/{msg,sem,shm}` 的内容，属主、属组按读者 `reader` 的用户
/// 命名空间换算（Linux 的 `seq_user_ns`）。
pub(crate) fn proc_listing(k: &Kernel, ns: u64, which: &[u8], reader: &Cred) -> String {
    use std::fmt::Write;
    let ipc = k.ipc(ns);
    let mut s = String::new();
    match which {
        b"msg" => {
            s.push_str("       key      msqid perms      cbytes       qnum lspid lrpid   uid   gid  cuid  cgid      stime      rtime      ctime\n");
            for (id, p, q) in ipc.msg.iter() {
                writeln!(
                    s,
                    "{:10} {:10} {:5o} {:11} {:10} {:5} {:5} {:5} {:5} {:5} {:5} {:10} {:10} {:10}",
                    p.key,
                    id,
                    p.mode & 0o777,
                    q.cbytes,
                    q.msgs.len(),
                    q.lspid,
                    q.lrpid,
                    reader.uid_out(p.uid),
                    reader.gid_out(p.gid),
                    reader.uid_out(p.cuid),
                    reader.gid_out(p.cgid),
                    q.stime,
                    q.rtime,
                    q.ctime
                )
                .unwrap();
            }
        }
        b"sem" => {
            s.push_str("       key      semid perms      nsems   uid   gid  cuid  cgid      otime      ctime\n");
            for (id, p, set) in ipc.sem.iter() {
                writeln!(
                    s,
                    "{:10} {:10} {:5o} {:10} {:5} {:5} {:5} {:5} {:10} {:10}",
                    p.key,
                    id,
                    p.mode & 0o777,
                    set.vals.len(),
                    reader.uid_out(p.uid),
                    reader.gid_out(p.gid),
                    reader.uid_out(p.cuid),
                    reader.gid_out(p.cgid),
                    set.otime,
                    set.ctime
                )
                .unwrap();
            }
        }
        _ => {
            s.push_str("       key      shmid perms                  size  cpid  lpid nattch   uid   gid  cuid  cgid      atime      dtime      ctime                   rss                  swap\n");
            for (id, p, seg) in ipc.shm.iter() {
                writeln!(
                    s,
                    "{:10} {:10} {:5o} {:21} {:5} {:5} {:6} {:5} {:5} {:5} {:5} {:10} {:10} {:10} {:21} {:21}",
                    p.key, id, p.mode & 0o1777, seg.size, seg.cpid, seg.lpid, nattch(k, (ns, id)),
                    reader.uid_out(p.uid), reader.gid_out(p.gid), reader.uid_out(p.cuid), reader.gid_out(p.cgid), seg.atime, seg.dtime, seg.ctime,
                    seg.mem.len(), 0
                )
                .unwrap();
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(uid: u32) -> Cred {
        let mut c = Cred::root();
        c.setresgid(Some(uid), Some(uid), Some(uid)).unwrap();
        c.setresuid(Some(uid), Some(uid), Some(uid)).unwrap();
        c
    }

    #[test]
    fn ids_are_reused_with_a_new_sequence() {
        let root = Cred::root();
        let mut t: Table<()> = Table::default();
        let a = t
            .get_or_create(IPC_PRIVATE, 0o600, &root, 4, |_| Ok(()), || Ok(()))
            .unwrap();
        assert_eq!(
            t.get_or_create(7, 0o600, &root, 4, |_| Ok(()), || Ok(())),
            Err(Errno::ENOENT)
        );
        let b = t
            .get_or_create(7, IPC_CREAT | 0o640, &root, 4, |_| Ok(()), || Ok(()))
            .unwrap();
        assert_eq!(
            t.get_or_create(7, IPC_CREAT | IPC_EXCL, &root, 4, |_| Ok(()), || Ok(())),
            Err(Errno::EEXIST)
        );
        assert_eq!(
            t.get_or_create(7, 0, &root, 4, |_| Ok(()), || Ok(())),
            Ok(b)
        );
        // 别的用户：没有读权限
        assert_eq!(
            t.get_or_create(7, 0o400, &user(1000), 4, |_| Ok(()), || Ok(())),
            Err(Errno::EACCES)
        );
        t.remove(a);
        assert_eq!(t.get(a).err(), Some(Errno::EINVAL));
        let c = t
            .get_or_create(IPC_PRIVATE, 0o600, &root, 4, |_| Ok(()), || Ok(()))
            .unwrap();
        assert_ne!(c, a);
        assert_eq!(c % IPCMNI, a % IPCMNI);
    }

    #[test]
    fn msgrcv_selects_by_type() {
        let mut q = MsgQueue::new(16384);
        for t in [3, 1, 2, 1] {
            q.msgs.push_back(Msg {
                mtype: t,
                text: Vec::new(),
            });
        }
        assert_eq!(q.select(0, false), Some(0));
        assert_eq!(q.select(2, false), Some(2));
        assert_eq!(q.select(3, true), Some(1));
        assert_eq!(q.select(-2, false), Some(1));
        assert_eq!(q.select(5, false), None);
    }

    #[test]
    fn semop_is_atomic_and_undone_at_exit() {
        let mut ipc = Ipc::default();
        let root = Cred::root();
        let id = ipc
            .sem
            .get_or_create(
                IPC_PRIVATE,
                0o600,
                &root,
                4,
                |_| Ok(()),
                || Ok(SemSet::new(2)),
            )
            .unwrap();
        let mut undo = Vec::new();
        let set = &mut ipc.sem.get_mut(id).unwrap().1;
        let up = |num, op| SemOp {
            num,
            op,
            flg: SEM_UNDO,
        };
        assert_eq!(set.apply(&[up(0, 2), up(1, 1)], &mut undo, 10), Ok(None));
        // 第二个操作要等：第一个也不生效
        assert_eq!(
            set.apply(&[up(0, -1), up(1, -2)], &mut undo, 10),
            Ok(Some(1))
        );
        assert_eq!(set.vals, vec![2, 1]);
        assert_eq!(
            set.apply(
                &[SemOp {
                    num: 0,
                    op: i16::MAX,
                    flg: 0
                }],
                &mut undo,
                10
            ),
            Err(Errno::ERANGE)
        );
        ipc.undo.insert((10, id), undo);
        ipc.exit(10);
        assert_eq!(ipc.sem.get(id).unwrap().1.vals, vec![0, 0]);
    }
}
