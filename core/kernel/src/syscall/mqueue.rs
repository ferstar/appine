//! POSIX 消息队列：mq_open、mq_unlink、mq_timedsend、mq_timedreceive、mq_notify、
//! mq_getsetattr。
//!
//! 队列按名字登记在所在的 IPC 命名空间里（[`crate::ipc::Ipc::mqueues`]），打开得到一个可以 poll
//! 的 fd（有消息时可读，没满时可写），read 它得到与 Linux 相同的状态行。mqueue 文件系统
//! （`mount -t mqueue`，`crate::pseudo` 的 `MqRoot`、`Mq`）把同一张表列成目录：在里面
//! open(O_CREAT) 新建、unlink 删除、chmod、chown 的都是这些队列，打开得到的 fd 与 mq_open 的
//! 一样。消息按优先级
//! 排列，同一优先级先进先出。mq_notify 在空队列来了消息、又没有人在等着接收时通知一次：
//! 发信号，或者把登记时给的 32 字节 cookie 送到 netlink socket（musl 的 `SIGEV_THREAD`
//! 就是这样实现的）。上限按 `/proc/sys/fs/mqueue/*`。

use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::{SysResult, get, put, read_timespec};
use crate::cell::RefCell;
use crate::cred::{Cred, MAY_READ, MAY_WRITE, cap};
use crate::errno::Errno;
use crate::fd::{FileKind, OpenFile, oflags};
use crate::kernel::{Cx, Tid, request_wake_object, wake};
use crate::net::Socket;
use crate::process::Signal;
use crate::signal::{self, InfoFields, SigInfo};
use crate::uaccess::UserSlice;
use crate::vfs::store::{Stat, mode};

/// 优先级的上限（不含）。
const MQ_PRIO_MAX: u32 = 32768;
/// 有 `CAP_SYS_RESOURCE` 时的上限（Linux 的 HARD_MSGMAX、HARD_MSGSIZEMAX）。
const HARD_MSGMAX: u64 = 65536;
const HARD_MSGSIZEMAX: u64 = 16 << 20;
const NAME_MAX: usize = 255;

const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGEV_THREAD: i32 = 2;
/// 消息队列通知的 si_code。
const SI_MESGQ: i32 = -3;
/// netlink 通知 cookie 的最后一个字节。
const NOTIFY_WOKENUP: u8 = 1;

/// `/proc/sys/fs/mqueue/*`。
pub(crate) struct Limits {
    pub queues_max: u64,
    pub msg_max: u64,
    pub msgsize_max: u64,
    pub msg_default: u64,
    pub msgsize_default: u64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            queues_max: 256,
            msg_max: 10,
            msgsize_max: 8192,
            msg_default: 10,
            msgsize_default: 8192,
        }
    }
}

impl Limits {
    /// `/proc/sys/fs/mqueue/<name>` 的值。
    pub fn get(&self, name: &[u8]) -> Option<u64> {
        Some(match name {
            b"queues_max" => self.queues_max,
            b"msg_max" => self.msg_max,
            b"msgsize_max" => self.msgsize_max,
            b"msg_default" => self.msg_default,
            b"msgsize_default" => self.msgsize_default,
            _ => return None,
        })
    }

    /// 写 `/proc/sys/fs/mqueue/<name>`。
    pub fn slot(&mut self, name: &[u8]) -> Option<&mut u64> {
        Some(match name {
            b"queues_max" => &mut self.queues_max,
            b"msg_max" => &mut self.msg_max,
            b"msgsize_max" => &mut self.msgsize_max,
            b"msg_default" => &mut self.msg_default,
            b"msgsize_default" => &mut self.msgsize_default,
            _ => return None,
        })
    }
}

/// mq_notify 的登记。
enum Notify {
    Signal {
        tgid: Tid,
        sig: Signal,
        value: u64,
    },
    /// 什么也不做，只是占住通知（`SIGEV_NONE`）。
    None {
        tgid: Tid,
    },
    /// 把 cookie 送到 netlink socket（`SIGEV_THREAD`）。
    Socket {
        tgid: Tid,
        sock: Weak<RefCell<Socket>>,
        cookie: [u8; 32],
    },
}

impl Notify {
    fn tgid(&self) -> Tid {
        match self {
            Notify::Signal { tgid, .. } | Notify::None { tgid } | Notify::Socket { tgid, .. } => {
                *tgid
            }
        }
    }
}

/// 队列的 inode 编号（Linux 的 `get_next_ino`），所有命名空间一起编号。
static NEXT_INO: AtomicU64 = AtomicU64::new(1);

/// mqueue 文件系统里一个目录项的大小、一个队列文件的大小（Linux 的 DIRENT_SIZE、
/// FILENT_SIZE）：根目录报告 2 + 队列数个目录项。
const DIRENT_SIZE: u64 = 20;
const FILENT_SIZE: u64 = 80;

/// 一个 IPC 命名空间里 mqueue 文件系统的根目录：属主是建立命名空间的进程（开机时的是
/// root），权限 01777，时间在新建、删除队列时更新。可以 chmod、chown。
pub(crate) struct MqDir {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub atime_ns: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl MqDir {
    pub fn new(uid: u32, gid: u32) -> MqDir {
        let now = crate::tty::wall_ns();
        MqDir {
            uid,
            gid,
            mode: mode::S_ISVTX | 0o777,
            atime_ns: now,
            mtime_ns: now,
            ctime_ns: now,
        }
    }

    /// stat：`queues` 是里面的队列数。
    pub fn stat(&self, queues: usize) -> Stat {
        Stat {
            ino: 0,
            mode: mode::S_IFDIR | self.mode,
            nlink: 2,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            size: (2 + queues as u64) * DIRENT_SIZE,
            atime_ns: self.atime_ns,
            mtime_ns: self.mtime_ns,
            ctime_ns: self.ctime_ns,
        }
    }

    fn touch(&mut self) {
        let now = crate::tty::wall_ns();
        (self.atime_ns, self.mtime_ns, self.ctime_ns) = (now, now, now);
    }
}

/// 命名空间里 inode 编号是 `ino` 的队列和它的名字（mqueue 文件系统里的节点）。
pub(crate) fn find(ipc: &crate::ipc::Ipc, ino: u64) -> Option<(&[u8], &Rc<RefCell<MessageQueue>>)> {
    ipc.mqueues
        .iter()
        .find(|(_, q)| q.borrow().ino == ino)
        .map(|(n, q)| (n.as_slice(), q))
}

impl Default for MqDir {
    fn default() -> MqDir {
        MqDir::new(0, 0)
    }
}

/// 一个消息队列。
pub struct MessageQueue {
    /// inode 编号：mqueue 文件系统里的节点按它找队列。
    pub ino: u64,
    /// 所在的 IPC 命名空间的编号（fstat 的设备号按它）。
    pub ns: u64,
    uid: u32,
    gid: u32,
    mode: u32,
    atime_ns: i64,
    mtime_ns: i64,
    ctime_ns: i64,
    maxmsg: usize,
    msgsize: usize,
    /// (优先级, 内容)，优先级高的在前，同一优先级先来的在前。
    messages: VecDeque<(u32, Vec<u8>)>,
    notify: Option<Notify>,
    /// 正在等着接收的线程数：有人在等时不发通知。
    receivers: usize,
    /// 发送、接收的消息数：与 Linux 一样，每次都让对方的 `EPOLLET` 再触发一次。
    sent: u64,
    received: u64,
}

impl MessageQueue {
    /// 发送、接收过的消息数（`syscall::poll` 的数据代数）。
    pub fn arrivals(&self) -> [u64; 2] {
        [self.sent, self.received]
    }

    /// poll 的就绪事件。
    pub fn readiness(&self) -> i16 {
        const POLLIN: i16 = 0x001;
        const POLLOUT: i16 = 0x004;
        const POLLRDNORM: i16 = 0x040;
        const POLLWRNORM: i16 = 0x100;
        let mut ev = 0;
        if !self.messages.is_empty() {
            ev |= POLLIN | POLLRDNORM;
        }
        if self.messages.len() < self.maxmsg {
            ev |= POLLOUT | POLLWRNORM;
        }
        ev
    }

    /// read 读到的状态行，与 Linux 的 mqueue 文件系统相同。
    pub fn status(&self) -> Vec<u8> {
        let qsize: usize = self.messages.iter().map(|(_, m)| m.len()).sum();
        let (notify, signo, pid) = match &self.notify {
            Some(Notify::Signal { tgid, sig, .. }) => (SIGEV_SIGNAL, sig.number(), *tgid),
            Some(Notify::None { tgid }) => (SIGEV_NONE, 0, *tgid),
            Some(Notify::Socket { tgid, .. }) => (SIGEV_THREAD, 0, *tgid),
            None => (0, 0, 0),
        };
        format!("QSIZE:{qsize:<10} NOTIFY:{notify:<5} SIGNO:{signo:<5} NOTIFY_PID:{pid:<6}\n")
            .into_bytes()
    }

    /// fstat：mqueue 文件系统里的普通文件。
    pub fn stat(&self) -> Stat {
        Stat {
            ino: self.ino,
            mode: mode::S_IFREG | self.mode,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            size: FILENT_SIZE,
            atime_ns: self.atime_ns,
            mtime_ns: self.mtime_ns,
            ctime_ns: self.ctime_ns,
        }
    }

    /// 读了状态行、登记或取消通知、改了标志：atime、ctime 是现在。
    pub fn touch_read(&mut self) {
        let now = crate::tty::wall_ns();
        (self.atime_ns, self.ctime_ns) = (now, now);
    }

    /// 发送、接收了一条消息：三个时间都是现在。
    fn touch_all(&mut self) {
        let now = crate::tty::wall_ns();
        (self.atime_ns, self.mtime_ns, self.ctime_ns) = (now, now, now);
    }

    /// chmod：只改权限位（文件类型、setuid 这些没有）。
    pub fn set_mode(&mut self, perm: u32) {
        self.mode = perm & 0o7777;
        self.ctime_ns = crate::tty::wall_ns();
    }

    /// chown：None 的不改。
    pub fn set_owner(&mut self, uid: Option<u32>, gid: Option<u32>) {
        if let Some(uid) = uid {
            self.uid = uid;
        }
        if let Some(gid) = gid {
            self.gid = gid;
        }
        self.ctime_ns = crate::tty::wall_ns();
    }

    /// utimensat：None 的不改，ctime 是现在。
    pub fn set_times(&mut self, atime: Option<i64>, mtime: Option<i64>) {
        if let Some(t) = atime {
            self.atime_ns = t;
        }
        if let Some(t) = mtime {
            self.mtime_ns = t;
        }
        self.ctime_ns = crate::tty::wall_ns();
    }
}

/// 在 IPC 命名空间 `ns`（对象是 `ipc`）里新建队列 `name`（Linux 的 `mqueue_create_attr`）：
/// 属主是调用者的 fsuid、fsgid，`perm` 已经去掉了 umask；`size` 是 (消息数, 消息长度) 的上限，
/// None 时用 `msg_default`、`msgsize_default`。队列数到了 `queues_max` 时没有
/// `CAP_SYS_RESOURCE` 是 ENOSPC。还没有登记进名字表（mq_open 装好 fd 之后才登记）。
pub(crate) fn new_queue(
    ipc: &crate::ipc::Ipc,
    ns: u64,
    cred: &Cred,
    perm: u32,
    size: Option<(u64, u64)>,
) -> Result<Rc<RefCell<MessageQueue>>, Errno> {
    let limits = &ipc.mq_limits;
    if ipc.mqueues.len() as u64 >= limits.queues_max && !cred.capable(cap::SYS_RESOURCE) {
        return Err(Errno::ENOSPC);
    }
    let (maxmsg, msgsize) = size.unwrap_or((limits.msg_default, limits.msgsize_default));
    let now = crate::tty::wall_ns();
    Ok(Rc::new(RefCell::new(MessageQueue {
        ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
        ns,
        uid: cred.fsuid,
        gid: cred.fsgid,
        mode: perm & 0o777,
        atime_ns: now,
        mtime_ns: now,
        ctime_ns: now,
        maxmsg: maxmsg as usize,
        msgsize: msgsize as usize,
        messages: VecDeque::new(),
        notify: None,
        receivers: 0,
        sent: 0,
        received: 0,
    })))
}

/// 把新建的队列登记进名字表，根目录的时间随之更新。
pub(crate) fn insert_queue(ipc: &mut crate::ipc::Ipc, name: Vec<u8>, q: Rc<RefCell<MessageQueue>>) {
    ipc.mqueues.entry(name).or_insert(q);
    ipc.mq_dir.borrow_mut().touch();
}

/// 删除队列 `name`（mq_unlink 和 mqueue 文件系统里的 unlink，都是 Linux 的 `vfs_unlink`）：
/// 根目录有粘滞位，要是队列或根目录的属主，或者对队列有 `CAP_FOWNER`，否则 EPERM（libc 的
/// mq_unlink 把它换成 EACCES）。
pub(crate) fn remove_queue(
    ipc: &mut crate::ipc::Ipc,
    cred: &Cred,
    name: &[u8],
) -> Result<(), Errno> {
    let q = ipc.mqueues.get(name).ok_or(Errno::ENOENT)?;
    let st = q.borrow().stat();
    let dir_uid = ipc.mq_dir.borrow().uid;
    if cred.fsuid != st.uid && cred.fsuid != dir_uid && !cred.capable_wrt(&st, cap::FOWNER) {
        return Err(Errno::EPERM);
    }
    ipc.mqueues.remove(name);
    ipc.mq_dir.borrow_mut().touch();
    Ok(())
}

/// 名字去掉开头的 `/`（musl 已经去掉了），检查长度和字符。
fn queue_name(cx: &Cx<'_>, addr: u64) -> Result<Vec<u8>, Errno> {
    let name = cx.mm().read_cstr(addr, 4096)?;
    let name = name.strip_prefix(b"/").unwrap_or(&name).to_vec();
    if name.len() > NAME_MAX {
        return Err(Errno::ENAMETOOLONG);
    }
    if name.is_empty() {
        return Err(Errno::ENOENT);
    }
    if name.contains(&b'/') || name == b"." || name == b".." {
        return Err(Errno::EACCES);
    }
    Ok(name)
}

pub(super) fn mq_open(cx: &mut Cx<'_>, name: u64, flags: u32, perm: u32, attr: u64) -> SysResult {
    let name = queue_name(cx, name)?;
    let accmode = flags & oflags::O_ACCMODE;
    if accmode == oflags::O_ACCMODE {
        return Err(Errno::EINVAL);
    }
    let cred = cx.cred();
    let existing = cx.ipc().mqueues.get(&name).cloned();
    let queue = match existing {
        Some(_)
            if flags & (oflags::O_CREAT | oflags::O_EXCL) == oflags::O_CREAT | oflags::O_EXCL =>
        {
            return Err(Errno::EEXIST);
        }
        Some(q) => {
            let st = q.borrow().stat();
            let mask = match accmode {
                oflags::O_RDONLY => MAY_READ,
                oflags::O_WRONLY => MAY_WRITE,
                _ => MAY_READ | MAY_WRITE,
            };
            cred.permission(&st, mask)?;
            q
        }
        None if flags & oflags::O_CREAT == 0 => return Err(Errno::ENOENT),
        None => {
            let privileged = cred.capable(cap::SYS_RESOURCE);
            let mut size = None;
            if attr != 0 {
                let limits = &cx.ipc().mq_limits;
                let b: [u8; 64] = get(cx, attr)?;
                let field = |i: usize| i64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
                let (m, s) = (field(1), field(2));
                let (cap_m, cap_s) = if privileged {
                    (HARD_MSGMAX, HARD_MSGSIZEMAX)
                } else {
                    (limits.msg_max, limits.msgsize_max)
                };
                if m <= 0 || s <= 0 || m as u64 > cap_m || s as u64 > cap_s {
                    return Err(Errno::EINVAL);
                }
                size = Some((m as u64, s as u64));
            }
            let ns = cx.ns().ipc.id;
            new_queue(cx.ipc(), ns, &cred, perm & !cx.umask(), size)?
        }
    };
    let path: Rc<[u8]> = Rc::from([b"/".as_slice(), &name].concat());
    let file = OpenFile::with_path(
        FileKind::MessageQueue(queue.clone()),
        accmode | (flags & oflags::O_NONBLOCK),
        Some(path),
    );
    // 与 Linux 一样总是 close-on-exec；fd 用完时（EMFILE）不留下新建的队列
    let fd = cx.fds_mut().install(file, true, 0)?;
    insert_queue(cx.ipc_mut(), name, queue);
    Ok(fd as u64)
}

pub(super) fn mq_unlink(cx: &mut Cx<'_>, name: u64) -> SysResult {
    let name = queue_name(cx, name)?;
    let cred = cx.cred();
    remove_queue(cx.ipc_mut(), &cred, &name)?;
    Ok(0)
}

/// mq fd 对应的队列和打开文件的访问方式。
fn queue_of(cx: &Cx<'_>, fd: i32) -> Result<(Rc<RefCell<MessageQueue>>, u32), Errno> {
    let file = cx.fds().get(fd)?;
    let f = file.borrow();
    match &f.kind {
        FileKind::MessageQueue(q) => Ok((q.clone(), f.flags)),
        _ => Err(Errno::EBADF),
    }
}

/// 绝对超时（`CLOCK_REALTIME`）；0 表示一直等。
fn deadline(cx: &Cx<'_>, timeout: u64) -> Result<Option<Instant>, Errno> {
    if timeout == 0 {
        return Ok(None);
    }
    let t = read_timespec(cx, timeout)?;
    super::misc::deadline_from_clock(0, t).map(Some)
}

pub(super) fn mq_timedsend(
    cx: &mut Cx<'_>,
    fd: i32,
    msg: u64,
    len: usize,
    prio: u32,
    timeout: u64,
) -> SysResult {
    let (q, flags) = queue_of(cx, fd)?;
    if flags & oflags::O_ACCMODE == oflags::O_RDONLY {
        return Err(Errno::EBADF);
    }
    if prio >= MQ_PRIO_MAX {
        return Err(Errno::EINVAL);
    }
    if len > q.borrow().msgsize {
        return Err(Errno::EMSGSIZE);
    }
    let mut data = vec![0u8; len];
    cx.mm().read(UserSlice::new(msg, len), &mut data)?;
    let deadline = deadline(cx, timeout)?;
    loop {
        if q.borrow().messages.len() < q.borrow().maxmsg {
            break;
        }
        if flags & oflags::O_NONBLOCK != 0 {
            return Err(Errno::EAGAIN);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Errno::ETIMEDOUT);
        }
        if cx.interrupted() {
            return Err(Errno::EINTR);
        }
        // 收走消息时请求这个队列
        cx.wait_objects(deadline, &mut [], wake::FILE, &[wake::of(&q)])?;
    }
    let notify = {
        let mut q = q.borrow_mut();
        let was_empty = q.messages.is_empty();
        let at = q
            .messages
            .iter()
            .position(|(p, _)| *p < prio)
            .unwrap_or(q.messages.len());
        q.messages.insert(at, (prio, data));
        q.sent += 1;
        q.touch_all();
        // 空队列来了消息、又没有人在等着接收：通知一次，登记随之取消
        if was_empty && q.receivers == 0 {
            q.notify.take()
        } else {
            None
        }
    };
    match notify {
        Some(Notify::Signal { tgid, sig, value }) => {
            // si_pid、si_uid 是登记通知的进程看到的（Linux 的 __do_notify 用登记时的用户命名
            // 空间，这里用它现在的），之后与别的信号一样当作发送者发的再换算一次
            let cred = cx.cred();
            let uid = cx
                .kr()
                .groups
                .get(&tgid)
                .map_or(cred.ruid, |g| g.borrow().cred.uid_out(cred.ruid));
            let info = SigInfo {
                code: SI_MESGQ,
                fields: InfoFields::Queue {
                    pid: cx.kr().pid_seen_by(tgid, cx.tgid()),
                    uid,
                    value,
                },
                sender: cx.tgid(),
                ..SigInfo::default()
            }
            .sent_by(&cred);
            signal::send_to_group(cx.kr(), tgid, sig, info);
        }
        Some(Notify::Socket {
            sock, mut cookie, ..
        }) => {
            if let Some(sock) = sock.upgrade()
                && let Socket::Netlink(n) = &mut *sock.borrow_mut()
            {
                cookie[31] = NOTIFY_WOKENUP;
                n.queue.push_back(cookie.to_vec());
                n.arrivals += 1;
                request_wake_object(wake::of(&sock));
            }
        }
        _ => {}
    }
    // 只叫醒等这个队列的（poll、epoll、阻塞的接收）：原来叫醒所有等待者
    request_wake_object(wake::of(&q));
    Ok(0)
}

pub(super) fn mq_timedreceive(
    cx: &mut Cx<'_>,
    fd: i32,
    msg: u64,
    len: usize,
    prio: u64,
    timeout: u64,
) -> SysResult {
    let (q, flags) = queue_of(cx, fd)?;
    if flags & oflags::O_ACCMODE == oflags::O_WRONLY {
        return Err(Errno::EBADF);
    }
    if len < q.borrow().msgsize {
        return Err(Errno::EMSGSIZE);
    }
    let deadline = deadline(cx, timeout)?;
    let (p, data) = loop {
        let mut queue = q.borrow_mut();
        if let Some(m) = queue.messages.pop_front() {
            queue.received += 1;
            queue.touch_all();
            break m;
        }
        drop(queue);
        if flags & oflags::O_NONBLOCK != 0 {
            return Err(Errno::EAGAIN);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Errno::ETIMEDOUT);
        }
        if cx.interrupted() {
            return Err(Errno::EINTR);
        }
        q.borrow_mut().receivers += 1;
        // 来了消息时请求这个队列
        let r = cx.wait_objects(deadline, &mut [], wake::FILE, &[wake::of(&q)]);
        q.borrow_mut().receivers -= 1;
        r?;
    };
    request_wake_object(wake::of(&q));
    cx.mm().write(UserSlice::new(msg, data.len()), &data)?;
    if prio != 0 {
        put(cx, prio, p.to_le_bytes())?;
    }
    Ok(data.len() as u64)
}

pub(super) fn mq_notify(cx: &mut Cx<'_>, fd: i32, sev: u64) -> SysResult {
    // 与 Linux 一样先检查通知方式，再看 fd
    let sev = if sev != 0 {
        let b: [u8; 64] = get(cx, sev)?;
        let how = i32::from_le_bytes(b[12..16].try_into().unwrap());
        let signo = i32::from_le_bytes(b[8..12].try_into().unwrap());
        if !matches!(how, SIGEV_NONE | SIGEV_SIGNAL | SIGEV_THREAD)
            || how == SIGEV_SIGNAL && Signal::new(signo).is_none()
        {
            return Err(Errno::EINVAL);
        }
        Some(b)
    } else {
        None
    };
    let (q, _) = queue_of(cx, fd)?;
    let tgid = cx.tgid();
    let Some(b) = sev else {
        // 取消自己的登记
        let mut q = q.borrow_mut();
        if q.notify.as_ref().is_some_and(|n| n.tgid() == tgid) {
            q.notify = None;
            q.touch_read();
        }
        return Ok(0);
    };
    let value = u64::from_le_bytes(b[..8].try_into().unwrap());
    let signo = i32::from_le_bytes(b[8..12].try_into().unwrap());
    let how = i32::from_le_bytes(b[12..16].try_into().unwrap());
    let notify = match how {
        SIGEV_NONE => Notify::None { tgid },
        SIGEV_SIGNAL => Notify::Signal {
            tgid,
            sig: Signal::new(signo).ok_or(Errno::EINVAL)?,
            value,
        },
        // sigev_signo 是 netlink socket 的 fd，sigev_value 指向 32 字节的 cookie
        SIGEV_THREAD => {
            let file = cx.fds().get(signo)?;
            let sock = match &file.borrow().kind {
                FileKind::Socket(s) if matches!(&*s.borrow(), Socket::Netlink(_)) => {
                    Rc::downgrade(s)
                }
                FileKind::Socket(_) => return Err(Errno::EINVAL),
                _ => return Err(Errno::ENOTSOCK),
            };
            let cookie: [u8; 32] = get(cx, value)?;
            Notify::Socket { tgid, sock, cookie }
        }
        _ => return Err(Errno::EINVAL),
    };
    let mut q = q.borrow_mut();
    // 一个队列只能有一个进程登记；登记者已经退出的不算
    if let Some(n) = &q.notify
        && cx
            .kr()
            .groups
            .get(&n.tgid())
            .is_some_and(|g| g.borrow().zombie.is_none())
    {
        return Err(Errno::EBUSY);
    }
    q.notify = Some(notify);
    q.touch_read();
    Ok(0)
}

pub(super) fn mq_getsetattr(cx: &mut Cx<'_>, fd: i32, new: u64, old: u64) -> SysResult {
    let file = cx.fds().get(fd)?;
    let q = match &file.borrow().kind {
        FileKind::MessageQueue(q) => q.clone(),
        _ => return Err(Errno::EBADF),
    };
    let attr = if new != 0 {
        let b: [u8; 64] = get(cx, new)?;
        let flags = i64::from_le_bytes(b[..8].try_into().unwrap());
        if flags as u32 & !oflags::O_NONBLOCK != 0 {
            return Err(Errno::EINVAL);
        }
        Some(flags as u32)
    } else {
        None
    };
    if old != 0 {
        let q = q.borrow();
        let mut b = [0u8; 64];
        let fields = [
            (file.borrow().flags & oflags::O_NONBLOCK) as i64,
            q.maxmsg as i64,
            q.msgsize as i64,
            q.messages.len() as i64,
        ];
        for (i, v) in fields.into_iter().enumerate() {
            b[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        put(cx, old, b)?;
    }
    if let Some(flags) = attr {
        let mut f = file.borrow_mut();
        f.flags = (f.flags & !oflags::O_NONBLOCK) | flags;
        q.borrow_mut().touch_read();
    }
    Ok(0)
}
