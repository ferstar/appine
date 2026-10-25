//! inotify：inotify_init1、inotify_add_watch、inotify_rm_watch，以及对它的 read。
//!
//! 所有文件系统修改都经过内核层，事件由各个 syscall 在改完仓库之后报告，位置和顺序与
//! Linux 的 fsnotify 钩子相同。监视按仓库的 inode 登记在 [`Registry`] 里；没有任何监视
//! 时，每个事件只多一次判空。
//!
//! - 文件上的事件同时报给它所在目录的监视（带名字）。按路径操作时，有监视才再解析一次
//!   路径找出目录（[`watched_parent`]）。按 fd 操作（读写、截断、关闭）时用打开时经过的
//!   目录和名字：每个打开的仓库文件带一个 [`Opened`] 记着它们，相当于 Linux 打开文件
//!   引用的目录项。rename 移动目录项时一起改（[`renamed`]）；删除之后不变，与 Linux
//!   一样仍然报给原来的目录。
//! - `IN_CLOSE_*` 在打开文件的最后一个引用释放时报告（[`Opened`] 析构），与 Linux 的
//!   fput 相同。
//! - `IN_DELETE_SELF` 在最后一个链接删除时报告；还有经由被删除的那个名字打开着的文件
//!   时，等它们都关闭再报告（[`deleted`]），与 Linux 在目录项最后一个引用释放时报告
//!   相同。
//! - 目录上的事件带 `IN_ISDIR`；`IN_DELETE_SELF`、`IN_MOVE_SELF` 不带，inotify 一直
//!   如此（Linux 为兼容老程序特意去掉）。
//! - 合成文件系统（procfs、devpts）上可以建监视，但不会有事件。
//! - 与 Linux 相同：与队尾未读事件完全相同的事件合并；排队 `max_queued_events` 个事件
//!   之后丢弃新的，并排一个 `IN_Q_OVERFLOW`。每个用户（有效 uid）的实例数和监视数受
//!   `max_user_instances`、`max_user_watches` 限制，超过时分别是 EMFILE、ENOSPC。

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::{Rc, Weak};

use super::{SysResult, read_path};
use crate::cred::MAY_READ;
use crate::errno::Errno;
use crate::fd::{FileKind, OpenFile, oflags};
use crate::kernel::{Cx, Kernel, request_wake};
use crate::vfs::store::Ino;
use crate::vfs::{Node, Vfs};

pub(crate) const IN_ACCESS: u32 = 0x1;
pub(crate) const IN_MODIFY: u32 = 0x2;
pub(crate) const IN_ATTRIB: u32 = 0x4;
pub(crate) const IN_CLOSE_WRITE: u32 = 0x8;
pub(crate) const IN_CLOSE_NOWRITE: u32 = 0x10;
pub(crate) const IN_OPEN: u32 = 0x20;
pub(crate) const IN_MOVED_FROM: u32 = 0x40;
pub(crate) const IN_MOVED_TO: u32 = 0x80;
pub(crate) const IN_CREATE: u32 = 0x100;
pub(crate) const IN_DELETE: u32 = 0x200;
pub(crate) const IN_DELETE_SELF: u32 = 0x400;
pub(crate) const IN_MOVE_SELF: u32 = 0x800;
const IN_ALL_EVENTS: u32 = 0xfff;
const IN_Q_OVERFLOW: u32 = 0x4000;
const IN_IGNORED: u32 = 0x8000;
const IN_ONLYDIR: u32 = 0x0100_0000;
const IN_DONT_FOLLOW: u32 = 0x0200_0000;
const IN_EXCL_UNLINK: u32 = 0x0400_0000;
const IN_MASK_CREATE: u32 = 0x1000_0000;
const IN_MASK_ADD: u32 = 0x2000_0000;
pub(crate) const IN_ISDIR: u32 = 0x4000_0000;
const IN_ONESHOT: u32 = 0x8000_0000;
/// 只报给对象自己、不报给所在目录的事件。
const SELF_EVENTS: u32 = IN_DELETE_SELF | IN_MOVE_SELF;

/// 一个实例里最多排队的事件数（`max_queued_events` 的默认值）。
const MAX_QUEUED: usize = 16384;
/// 一个用户最多的实例数（`max_user_instances` 的默认值）。
const MAX_INSTANCES: usize = 128;
/// 一个用户最多的监视数（`max_user_watches` 的默认值）。
const MAX_WATCHES: usize = 1 << 20;

struct Watch {
    node: Node,
    mask: u32,
}

#[derive(PartialEq)]
struct Event {
    wd: i32,
    mask: u32,
    cookie: u32,
    name: Vec<u8>,
}

impl Event {
    /// read 给出的长度：16 字节的 `struct inotify_event`，名字补 NUL 到 16 字节的倍数。
    fn len(&self) -> usize {
        16 + if self.name.is_empty() {
            0
        } else {
            (self.name.len() + 1).next_multiple_of(16)
        }
    }
}

/// 一个 inotify 实例。
pub struct Inotify {
    watches: BTreeMap<i32, Watch>,
    last_wd: i32,
    queue: VecDeque<Event>,
    /// 队列里的事件一共多长（FIONREAD）。
    bytes: usize,
    /// 队列里有 `IN_Q_OVERFLOW`。
    overflowed: bool,
    /// 最多排队的事件数：与 Linux 一样取创建时的 `max_queued_events`。
    max_queued: usize,
    /// 创建者的有效 uid：实例和监视算在这个用户名下。
    uid: u32,
    /// 登记了监视的 [`Registry`]：实例关闭时从里面去掉自己的监视。
    registry: Weak<RefCell<Registry>>,
}

impl Default for Inotify {
    fn default() -> Inotify {
        Inotify {
            watches: BTreeMap::new(),
            last_wd: 0,
            queue: VecDeque::new(),
            bytes: 0,
            overflowed: false,
            max_queued: MAX_QUEUED,
            uid: 0,
            registry: Weak::new(),
        }
    }
}

impl Drop for Inotify {
    fn drop(&mut self) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let Ok(mut r) = registry.try_borrow_mut() else {
            return;
        };
        let user = r.users.entry(self.uid).or_default();
        user.0 = user.0.saturating_sub(1);
        user.1 = user.1.saturating_sub(self.watches.len());
        for w in self.watches.values() {
            if let Node::Store(ino) = w.node
                && let Some(list) = r.by_ino.get_mut(&ino)
            {
                // 正在析构的实例已经没有强引用，指向它的弱引用都失效了
                list.retain(|(weak, _)| weak.strong_count() > 0);
                if list.is_empty() {
                    r.by_ino.remove(&ino);
                }
            }
        }
    }
}

impl Inotify {
    /// 有没有事件可读。
    pub fn ready(&self) -> bool {
        !self.queue.is_empty()
    }

    /// 可读的字节数。
    pub fn pending(&self) -> usize {
        self.bytes
    }

    /// `/proc/<pid>/fdinfo/<fd>` 里每个监视一行，格式与 Linux 相同。仓库里的文件带上
    /// name_to_handle_at 给出的句柄。
    pub fn fdinfo(&self) -> String {
        let mut out = String::new();
        for (wd, w) in &self.watches {
            let (ino, sdev) = match &w.node {
                // 内核内部的设备号编码：(主 << 20) | 次，仓库是 8:1，procfs 是 0:4
                Node::Store(ino) => (*ino, 0x80_0001),
                Node::Pseudo(p) => (crate::pseudo::ino_of(p), 0x4),
            };
            out += &format!(
                "inotify wd:{wd:x} ino:{ino:x} sdev:{sdev:x} mask:{:x} ignored_mask:0",
                w.mask
            );
            if let Node::Store(ino) = w.node {
                let handle: String = ino
                    .to_le_bytes()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                out += &format!(" fhandle-bytes:8 fhandle-type:1 f_handle:{handle}");
            }
            out.push('\n');
        }
        out
    }

    fn push(&mut self, wd: i32, mask: u32, cookie: u32, name: &[u8]) {
        let event = Event {
            wd,
            mask,
            cookie,
            name: name.to_vec(),
        };
        if self.queue.back() == Some(&event) {
            return;
        }
        let event = if self.queue.len() < self.max_queued {
            event
        } else if self.overflowed {
            return;
        } else {
            self.overflowed = true;
            Event {
                wd: -1,
                mask: IN_Q_OVERFLOW,
                cookie: 0,
                name: Vec::new(),
            }
        };
        self.bytes += event.len();
        self.queue.push_back(event);
    }
}

/// 监视某个 inode 的实例和 wd。
type Watcher = (Weak<RefCell<Inotify>>, i32);

/// 所有监视，按被监视的 inode。
pub struct Registry {
    /// 每个 inode 上的 (实例, wd)。实例关闭之后弱引用失效，下次碰到时清掉。
    by_ino: HashMap<Ino, Vec<Watcher>>,
    last_cookie: u32,
    /// 监视所在的仓库：`IN_EXCL_UNLINK` 要查子项还有没有链接。
    vfs: Weak<Vfs>,
    /// 每个用户（有效 uid）的实例数和监视数。
    users: HashMap<u32, (usize, usize)>,
    /// `/proc/sys/fs/inotify/` 下的三个限制。
    pub max_queued: usize,
    pub max_instances: usize,
    pub max_watches: usize,
    /// 最后一个链接已经删除，但还有经由那个名字打开着的文件的 inode：这些文件都关闭
    /// 之后报告 `IN_DELETE_SELF`。
    doomed: HashMap<Ino, Vec<Weak<RefCell<OpenFile>>>>,
}

impl Default for Registry {
    fn default() -> Registry {
        Registry {
            by_ino: HashMap::new(),
            last_cookie: 0,
            vfs: Weak::new(),
            users: HashMap::new(),
            max_queued: MAX_QUEUED,
            max_instances: MAX_INSTANCES,
            max_watches: MAX_WATCHES,
            doomed: HashMap::new(),
        }
    }
}

impl Registry {
    /// 把事件交给 `ino` 上关心它的监视；返回有没有排进事件。`child` 是目录收到子项
    /// 自己的事件时那个子项：带 `IN_EXCL_UNLINK` 的监视不要已经删除的子项的事件。
    fn deliver(
        &mut self,
        ino: Ino,
        mask: u32,
        cookie: u32,
        name: &[u8],
        child: Option<Ino>,
    ) -> bool {
        let vfs = self.vfs.clone();
        let Some(list) = self.by_ino.get_mut(&ino) else {
            return false;
        };
        let mut unlinked = None;
        let mut is_unlinked = |child: Ino| {
            *unlinked.get_or_insert_with(|| {
                vfs.upgrade()
                    .is_some_and(|v| v.store().stat(child).map_or(true, |st| st.nlink == 0))
            })
        };
        let mut queued = false;
        let mut gone = Vec::new();
        list.retain(|(weak, wd)| {
            let Some(inotify) = weak.upgrade() else {
                return false;
            };
            let Ok(mut inotify) = inotify.try_borrow_mut() else {
                return true;
            };
            let Some(wmask) = inotify.watches.get(wd).map(|w| w.mask) else {
                return false;
            };
            let excluded = wmask & IN_EXCL_UNLINK != 0 && child.is_some_and(&mut is_unlinked);
            let wanted = mask & wmask & IN_ALL_EVENTS != 0 && !excluded;
            if wanted {
                inotify.push(*wd, mask, cookie, name);
                queued = true;
            }
            // 对象没了，或者一次性的监视已经报告过：去掉监视，报告 IN_IGNORED
            if mask & IN_DELETE_SELF != 0 || wanted && wmask & IN_ONESHOT != 0 {
                inotify.watches.remove(wd);
                inotify.push(*wd, IN_IGNORED, 0, b"");
                gone.push(inotify.uid);
                queued = true;
                return false;
            }
            true
        });
        if list.is_empty() {
            self.by_ino.remove(&ino);
        }
        for uid in gone {
            self.uncharge(uid);
        }
        queued
    }

    /// 用户 `uid` 少了一个监视。
    fn uncharge(&mut self, uid: u32) {
        let user = self.users.entry(uid).or_default();
        user.1 = user.1.saturating_sub(1);
    }

    /// 打开文件关闭了：它是经由已经删除的名字打开的最后一个文件时报告
    /// `IN_DELETE_SELF`。
    fn closed(&mut self, ino: Ino) {
        let Some(list) = self.doomed.get_mut(&ino) else {
            return;
        };
        // 正在析构的打开文件已经没有强引用
        list.retain(|f| f.strong_count() > 0);
        if list.is_empty() {
            self.doomed.remove(&ino);
            self.notify(ino, None, IN_DELETE_SELF);
        }
    }

    /// `ino` 自己的事件：先报给所在目录 `parent` 的监视（带名字），再报给它自己的。
    fn notify(&mut self, ino: Ino, parent: Option<(Ino, &[u8])>, mask: u32) {
        let mut queued = false;
        if let Some((dir, name)) = parent
            && mask & SELF_EVENTS == 0
        {
            queued |= self.deliver(dir, mask, 0, name, Some(ino));
        }
        queued |= self.deliver(ino, mask, 0, b"", None);
        if queued {
            request_wake();
        }
    }
}

/// 打开的仓库文件带着它：按 fd 的事件据此报给所在的目录，最后一个引用释放时报告
/// `IN_CLOSE_WRITE` 或 `IN_CLOSE_NOWRITE`。
pub struct Opened {
    registry: Weak<RefCell<Registry>>,
    ino: Ino,
    parent: Option<(Ino, Vec<u8>)>,
    close: u32,
}

impl Drop for Opened {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade()
            && let Ok(mut r) = registry.try_borrow_mut()
            && !r.by_ino.is_empty()
        {
            let parent = self.parent.as_ref().map(|(d, n)| (*d, n.as_slice()));
            r.notify(self.ino, parent, self.close);
            r.closed(self.ino);
        }
    }
}

/// 有没有任何监视（没有时不必为事件做任何事）。
fn idle(k: &Kernel) -> bool {
    k.inotify.borrow().by_ino.is_empty()
}

/// 有监视：调用方据此决定要不要为报告事件多查一次仓库。
pub(crate) fn active(k: &Kernel) -> bool {
    !idle(k)
}

/// 按路径操作的对象 `ino` 自己的事件；`parent` 是它所在的目录和名字。
pub(crate) fn path_event(k: &Kernel, ino: Ino, parent: Option<(Ino, &[u8])>, mask: u32) {
    if !idle(k) {
        k.inotify.borrow_mut().notify(ino, parent, mask);
    }
}

/// 按 fd 操作的打开文件上的事件。
pub(crate) fn file_event(k: &Kernel, f: &OpenFile, mask: u32) {
    if idle(k) {
        return;
    }
    let (ino, mask) = match &f.kind {
        FileKind::Regular { ino, .. } | FileKind::Device { ino, .. } => (*ino, mask),
        FileKind::Directory {
            node: Node::Store(ino),
            ..
        } => (*ino, mask | IN_ISDIR),
        _ => return,
    };
    let parent = f
        .watch
        .as_ref()
        .and_then(|w| w.parent.as_ref())
        .map(|(d, n)| (*d, n.as_slice()));
    k.inotify.borrow_mut().notify(ino, parent, mask);
}

/// 目录 `dir` 里名为 `name` 的子项被创建、删除、移入或移出。
pub(crate) fn dir_event(k: &Kernel, dir: Ino, name: &[u8], mask: u32, cookie: u32) {
    if !idle(k)
        && k.inotify
            .borrow_mut()
            .deliver(dir, mask, cookie, name, None)
    {
        request_wake();
    }
}

/// rename 的两个事件共用的 cookie。
pub(crate) fn cookie(k: &Kernel) -> u32 {
    let mut r = k.inotify.borrow_mut();
    r.last_cookie = r.last_cookie.wrapping_add(1);
    r.last_cookie
}

/// 新打开的仓库文件要带的 [`Opened`]。`parent` 是打开时经过的目录和名字。
pub(crate) fn opened(k: &Kernel, f: &OpenFile, parent: Option<(Ino, Vec<u8>)>) -> Option<Opened> {
    let ino = match &f.kind {
        FileKind::Regular { ino, .. } | FileKind::Device { ino, .. } => *ino,
        FileKind::Directory {
            node: Node::Store(ino),
            ..
        } => *ino,
        _ => return None,
    };
    let dir = if matches!(f.kind, FileKind::Directory { .. }) {
        IN_ISDIR
    } else {
        0
    };
    Some(Opened {
        registry: Rc::downgrade(&k.inotify),
        ino,
        parent,
        close: dir
            | if f.writable() {
                IN_CLOSE_WRITE
            } else {
                IN_CLOSE_NOWRITE
            },
    })
}

/// rename 把目录项 `from` 移到了 `to`：经由它打开的文件，之后按 fd 的事件报给新的目录
/// 和名字（Linux 的 d_move）。`moved` 查出被移动的对象：同名的已经删除的文件不跟着改。
pub(crate) fn renamed(
    k: &Kernel,
    from: (Ino, &[u8]),
    to: (Ino, &[u8]),
    moved: impl FnOnce() -> Option<Ino>,
) {
    let at = |f: &OpenFile| {
        f.watch
            .as_ref()
            .and_then(|w| w.parent.as_ref())
            .is_some_and(|(d, n)| *d == from.0 && n == from.1)
    };
    let files: Vec<_> = k
        .open_files()
        .into_iter()
        .filter(|f| at(&f.borrow()))
        .collect();
    if files.is_empty() {
        return;
    }
    let Some(ino) = moved() else {
        return;
    };
    for f in files {
        let mut f = f.borrow_mut();
        if let Some(w) = f.watch.as_mut().filter(|w| w.ino == ino) {
            w.parent = Some((to.0, to.1.to_vec()));
        }
    }
}

/// `ino` 在目录 `dir` 里的最后一个链接 `name` 删除了：报告 `IN_DELETE_SELF`。还有经由
/// 这个名字打开着的文件时，等它们都关闭再报告。
pub(crate) fn deleted(k: &Kernel, ino: Ino, dir: Ino, name: &[u8]) {
    if !k.inotify.borrow().by_ino.contains_key(&ino) {
        return;
    }
    let open: Vec<_> = k
        .open_files()
        .iter()
        .filter(|f| {
            f.borrow().watch.as_ref().is_some_and(|w| {
                w.ino == ino
                    && w.parent
                        .as_ref()
                        .is_some_and(|(d, n)| *d == dir && n == name)
            })
        })
        .map(Rc::downgrade)
        .collect();
    if open.is_empty() {
        path_event(k, ino, None, IN_DELETE_SELF);
    } else {
        k.inotify
            .borrow_mut()
            .doomed
            .entry(ino)
            .or_default()
            .extend(open);
    }
}

/// 按路径操作时对象所在的目录和名字（报给目录的监视用）。没有任何监视时不解析。
pub(crate) fn watched_parent(
    cx: &Cx<'_>,
    dirfd: i32,
    path: &[u8],
    follow: bool,
) -> Option<(Ino, Vec<u8>)> {
    if idle(cx.kr()) || path.is_empty() {
        return None;
    }
    let vfs = cx.vfs().ok()?;
    let start = super::fs::start_dir(cx, dirfd, path).ok()?;
    let (_, found) = vfs
        .walk(&cx.view(&vfs), &cx.cred(), &cx.root(), &start, path, follow)
        .ok()?;
    match found? {
        (Node::Store(dir), name) => Some((dir, name)),
        _ => None,
    }
}

pub(super) fn inotify_init1(cx: &mut Cx<'_>, flags: u32) -> SysResult {
    if flags & !(oflags::O_CLOEXEC | oflags::O_NONBLOCK) != 0 {
        return Err(Errno::EINVAL);
    }
    let uid = cx.cred().euid;
    let registry = cx.kr().inotify.clone();
    let mut r = registry.borrow_mut();
    if r.users.get(&uid).map_or(0, |u| u.0) >= r.max_instances {
        return Err(Errno::EMFILE);
    }
    r.users.entry(uid).or_default().0 += 1;
    let mut inotify = Inotify::default();
    inotify.max_queued = r.max_queued;
    inotify.uid = uid;
    inotify.registry = Rc::downgrade(&registry);
    drop(r);
    // 装不进 fd 表时实例在这里析构，计数随之退回
    let file = OpenFile::new(
        FileKind::Inotify(Rc::new(RefCell::new(inotify))),
        oflags::O_RDONLY | (flags & oflags::O_NONBLOCK),
    );
    cx.fds_mut()
        .install(file, flags & oflags::O_CLOEXEC != 0, 0)
        .map(|fd| fd as u64)
}

fn inotify_of(cx: &Cx<'_>, fd: i32) -> Result<Rc<RefCell<Inotify>>, Errno> {
    match &cx.fds().get(fd)?.borrow().kind {
        FileKind::Inotify(i) => Ok(i.clone()),
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn inotify_add_watch(cx: &mut Cx<'_>, fd: i32, path: u64, mask: u32) -> SysResult {
    let inotify = inotify_of(cx, fd)?;
    let path = read_path(cx, path)?;
    if mask & IN_ALL_EVENTS == 0 || mask & IN_MASK_ADD != 0 && mask & IN_MASK_CREATE != 0 {
        return Err(Errno::EINVAL);
    }
    let node = super::fs::lookup_at(cx, super::fs::AT_FDCWD, &path, mask & IN_DONT_FOLLOW == 0)?;
    let vfs = cx.vfs()?;
    let view = cx.view(&vfs);
    if mask & IN_ONLYDIR != 0 && !vfs.is_dir(&view, &node)? {
        return Err(Errno::ENOTDIR);
    }
    if let Node::Store(ino) = node {
        cx.cred().permission(&vfs.store().stat(ino)?, MAY_READ)?;
    }
    let keep = IN_ALL_EVENTS | IN_EXCL_UNLINK | IN_ONESHOT;
    let mut i = inotify.borrow_mut();
    // 同一个对象上已经有监视：改掉它的掩码（IN_MASK_ADD 时合并）
    if let Some((&wd, w)) = i.watches.iter_mut().find(|(_, w)| w.node == node) {
        if mask & IN_MASK_CREATE != 0 {
            return Err(Errno::EEXIST);
        }
        w.mask = if mask & IN_MASK_ADD != 0 {
            w.mask | (mask & keep)
        } else {
            mask & keep
        };
        return Ok(wd as u64);
    }
    let registry = cx.kr().inotify.clone();
    let mut r = registry.borrow_mut();
    let user = r.users.get(&i.uid).map_or(0, |u| u.1);
    if user >= r.max_watches {
        return Err(Errno::ENOSPC);
    }
    i.last_wd = i.last_wd.checked_add(1).ok_or(Errno::ENOSPC)?;
    r.users.entry(i.uid).or_default().1 += 1;
    let wd = i.last_wd;
    i.watches.insert(
        wd,
        Watch {
            node: node.clone(),
            mask: mask & keep,
        },
    );
    if let Node::Store(ino) = node {
        r.vfs = Rc::downgrade(&vfs);
        r.by_ino
            .entry(ino)
            .or_default()
            .push((Rc::downgrade(&inotify), wd));
    }
    Ok(wd as u64)
}

pub(super) fn inotify_rm_watch(cx: &mut Cx<'_>, fd: i32, wd: i32) -> SysResult {
    let inotify = inotify_of(cx, fd)?;
    let mut i = inotify.borrow_mut();
    let w = i.watches.remove(&wd).ok_or(Errno::EINVAL)?;
    i.push(wd, IN_IGNORED, 0, b"");
    request_wake();
    let mut r = cx.kr().inotify.borrow_mut();
    r.uncharge(i.uid);
    if let Node::Store(ino) = w.node
        && let Some(list) = r.by_ino.get_mut(&ino)
    {
        list.retain(|(weak, w)| !(*w == wd && weak.ptr_eq(&Rc::downgrade(&inotify))));
        if list.is_empty() {
            r.by_ino.remove(&ino);
        }
    }
    Ok(0)
}

/// 读出排队的事件：放得下几个就给几个，第一个都放不下时 EINVAL；没有事件时阻塞。
pub(super) fn read(
    cx: &mut Cx<'_>,
    inotify: &Rc<RefCell<Inotify>>,
    nonblock: bool,
    buf: &mut [u8],
) -> Result<usize, Errno> {
    loop {
        {
            let mut i = inotify.borrow_mut();
            if let Some(first) = i.queue.front() {
                if first.len() > buf.len() {
                    return Err(Errno::EINVAL);
                }
                let mut n = 0;
                while let Some(e) = i.queue.front() {
                    let len = e.len();
                    if n + len > buf.len() {
                        break;
                    }
                    let out = &mut buf[n..n + len];
                    out.fill(0);
                    out[0..4].copy_from_slice(&e.wd.to_le_bytes());
                    out[4..8].copy_from_slice(&e.mask.to_le_bytes());
                    out[8..12].copy_from_slice(&e.cookie.to_le_bytes());
                    out[12..16].copy_from_slice(&((len - 16) as u32).to_le_bytes());
                    out[16..16 + e.name.len()].copy_from_slice(&e.name);
                    if e.mask == IN_Q_OVERFLOW {
                        i.overflowed = false;
                    }
                    i.bytes -= len;
                    i.queue.pop_front();
                    n += len;
                }
                return Ok(n);
            }
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if cx.interrupted() {
            return Err(Errno::ERESTARTSYS);
        }
        cx.wait(None, &mut [])?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch(r: &mut Registry, i: &Rc<RefCell<Inotify>>, ino: Ino, mask: u32) -> i32 {
        let mut inotify = i.borrow_mut();
        inotify.last_wd += 1;
        let wd = inotify.last_wd;
        inotify.watches.insert(
            wd,
            Watch {
                node: Node::Store(ino),
                mask,
            },
        );
        r.by_ino
            .entry(ino)
            .or_default()
            .push((Rc::downgrade(i), wd));
        wd
    }

    fn events(i: &Rc<RefCell<Inotify>>) -> Vec<(i32, u32, Vec<u8>)> {
        let mut i = i.borrow_mut();
        i.bytes = 0;
        i.queue.drain(..).map(|e| (e.wd, e.mask, e.name)).collect()
    }

    #[test]
    fn reports_to_the_file_and_its_directory() {
        let mut r = Registry::default();
        let i = Rc::new(RefCell::new(Inotify::default()));
        let dir = watch(&mut r, &i, 1, IN_MODIFY | IN_CREATE);
        let file = watch(&mut r, &i, 2, IN_MODIFY | IN_DELETE_SELF);
        r.notify(2, Some((1, b"f")), IN_MODIFY);
        // 只和队尾的事件比较：完全相同的才合并
        r.notify(2, None, IN_MODIFY);
        r.notify(2, Some((1, b"f")), IN_ATTRIB);
        r.notify(2, Some((1, b"f")), IN_DELETE_SELF);
        assert_eq!(
            events(&i),
            vec![
                (dir, IN_MODIFY, b"f".to_vec()),
                (file, IN_MODIFY, vec![]),
                (file, IN_DELETE_SELF, vec![]),
                (file, IN_IGNORED, vec![]),
            ]
        );
        // 文件的监视随 IN_DELETE_SELF 去掉，目录的还在
        assert!(!r.by_ino.contains_key(&2));
        assert!(r.deliver(1, IN_CREATE | IN_ISDIR, 0, b"d", None));
        assert_eq!(events(&i), vec![(dir, IN_CREATE | IN_ISDIR, b"d".to_vec())]);
    }

    #[test]
    fn oneshot_watches_and_closed_instances_go_away() {
        let mut r = Registry::default();
        let i = Rc::new(RefCell::new(Inotify::default()));
        let wd = watch(&mut r, &i, 5, IN_OPEN | IN_ONESHOT);
        r.notify(5, None, IN_OPEN);
        r.notify(5, None, IN_OPEN | IN_ISDIR);
        assert_eq!(
            events(&i),
            vec![(wd, IN_OPEN, vec![]), (wd, IN_IGNORED, vec![])]
        );
        assert!(r.by_ino.is_empty());
        watch(&mut r, &i, 6, IN_OPEN);
        drop(i);
        assert!(!r.deliver(6, IN_OPEN, 0, b"", None));
        assert!(r.by_ino.is_empty());
    }

    #[test]
    fn delete_self_waits_for_the_last_close() {
        let registry = Rc::new(RefCell::new(Registry::default()));
        let i = Rc::new(RefCell::new(Inotify::default()));
        let wd = watch(&mut registry.borrow_mut(), &i, 7, IN_ALL_EVENTS);
        let open = || {
            let file = OpenFile::new(FileKind::Path(Node::Store(7)), oflags::O_RDONLY);
            file.borrow_mut().watch = Some(Opened {
                registry: Rc::downgrade(&registry),
                ino: 7,
                parent: Some((1, b"f".to_vec())),
                close: IN_CLOSE_NOWRITE,
            });
            file
        };
        let (a, b) = (open(), open());
        registry
            .borrow_mut()
            .doomed
            .insert(7, vec![Rc::downgrade(&a), Rc::downgrade(&b)]);
        drop(a);
        assert_eq!(events(&i), vec![(wd, IN_CLOSE_NOWRITE, vec![])]);
        drop(b);
        assert_eq!(
            events(&i),
            vec![
                (wd, IN_CLOSE_NOWRITE, vec![]),
                (wd, IN_DELETE_SELF, vec![]),
                (wd, IN_IGNORED, vec![]),
            ]
        );
        assert!(registry.borrow().doomed.is_empty());
    }

    #[test]
    fn queue_overflows_once() {
        let mut i = Inotify::default();
        for n in 0..MAX_QUEUED + 10 {
            i.push(1, IN_MODIFY, n as u32, b"");
        }
        assert_eq!(i.queue.len(), MAX_QUEUED + 1);
        assert_eq!(i.queue.back().unwrap().mask, IN_Q_OVERFLOW);
        assert_eq!(i.pending(), (MAX_QUEUED + 1) * 16);
        assert_eq!(
            Event {
                wd: 1,
                mask: 0,
                cookie: 0,
                name: b"0123456789abcde".to_vec()
            }
            .len(),
            32
        );
    }
}
