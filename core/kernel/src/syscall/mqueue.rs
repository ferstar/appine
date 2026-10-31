//! POSIX 消息队列：mq_open、mq_unlink、mq_timedsend、mq_timedreceive、mq_notify、
//! mq_getsetattr。
//!
//! 队列按名字登记在所在的 IPC 命名空间里（[`crate::ipc::Ipc::mqueues`]），打开得到一个可以 poll
//! 的 fd（有消息时可读，没满时可写），read 它得到与 Linux 相同的状态行。消息按优先级
//! 排列，同一优先级先进先出。mq_notify 在空队列来了消息、又没有人在等着接收时通知一次：
//! 发信号，或者把登记时给的 32 字节 cookie 送到 netlink socket（musl 的 `SIGEV_THREAD`
//! 就是这样实现的）。上限按 `/proc/sys/fs/mqueue/*`。

use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::time::Instant;

use super::{SysResult, get, put, read_timespec};
use crate::cell::RefCell;
use crate::cred::{MAY_READ, MAY_WRITE, cap};
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

/// 一个消息队列。
pub struct MessageQueue {
    uid: u32,
    gid: u32,
    mode: u32,
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
            ino: 0,
            mode: mode::S_IFREG | self.mode,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            size: 80,
            atime_ns: 0,
            mtime_ns: 0,
            ctime_ns: 0,
        }
    }
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
            let limits = &cx.ipc().mq_limits;
            let (mut maxmsg, mut msgsize) = (limits.msg_default, limits.msgsize_default);
            if attr != 0 {
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
                (maxmsg, msgsize) = (m as u64, s as u64);
            }
            if cx.ipc().mqueues.len() as u64 >= limits.queues_max && !privileged {
                return Err(Errno::ENOSPC);
            }
            Rc::new(RefCell::new(MessageQueue {
                uid: cred.fsuid,
                gid: cred.fsgid,
                mode: perm & 0o777 & !cx.umask(),
                maxmsg: maxmsg as usize,
                msgsize: msgsize as usize,
                messages: VecDeque::new(),
                notify: None,
                receivers: 0,
                sent: 0,
                received: 0,
            }))
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
    cx.ipc_mut().mqueues.entry(name).or_insert(queue);
    Ok(fd as u64)
}

pub(super) fn mq_unlink(cx: &mut Cx<'_>, name: u64) -> SysResult {
    let name = queue_name(cx, name)?;
    let q = cx.ipc().mqueues.get(&name).cloned().ok_or(Errno::ENOENT)?;
    // 删除名字要求属主或者有 CAP_FOWNER（mqueue 文件系统的根目录有粘滞位）
    let cred = cx.cred();
    if cred.fsuid != q.borrow().uid && !cred.capable(cap::FOWNER) {
        return Err(Errno::EACCES);
    }
    cx.ipc_mut().mqueues.remove(&name);
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
    }
    Ok(0)
}
