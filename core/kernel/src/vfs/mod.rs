//! 虚拟文件系统（设计文档 §8）。
//!
//! 根文件系统是 inode 仓库，`/dev` 里的设备节点也存在仓库里（[`dev`]）。`/proc`
//! （procfs）和 `/dev/pts`（devpts）是挂在仓库目录上的合成文件系统：它们的内容来自
//! 内核状态，由内核层经由 [`Pseudo`] 提供。路径解析在整棵树里进行，符号链接最多
//! 跟随 40 层；经过的每个仓库目录都要有搜索权限。进程的根目录可以被 chroot 换掉：
//! 绝对路径从它开始，`..` 不会越过它。

pub mod dev;
pub mod import;
pub mod store;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use crate::cred::{Cred, MAY_EXEC};
use crate::errno::Errno;
pub(crate) use crate::pseudo::PseudoNode;
use store::{Ino, ROOT, Stat, Store, mode};

/// 跟随符号链接的最大次数，与 Linux 的 MAXSYMLINKS 一致。
pub(crate) const MAX_SYMLINKS: u32 = 40;
/// 路径组件的最大长度（`NAME_MAX`）。
const NAME_MAX: usize = 255;

/// 文件系统树里的一个节点。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Node {
    /// inode 仓库里的 inode。
    Store(Ino),
    /// 合成文件系统里的节点。
    Pseudo(PseudoNode),
}

impl Node {
    /// 根目录。
    pub const ROOT: Node = Node::Store(ROOT);
}

/// 挂在仓库目录上的合成文件系统。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Mount {
    /// `/proc`。
    Proc,
    /// `/dev/pts`。
    Pts,
    /// `/sys`。
    Sys,
    /// `/sys/fs/cgroup`（只读的 cgroup2）：挂在 sysfs 里，不在仓库的目录上。
    Cgroup,
}

impl Mount {
    /// 挂载点在仓库里的路径。
    const fn path(self) -> &'static [&'static [u8]] {
        match self {
            Mount::Proc => &[b"proc"],
            Mount::Pts => &[b"dev", b"pts"],
            Mount::Sys => &[b"sys"],
            Mount::Cgroup => &[b"sys", b"fs", b"cgroup"],
        }
    }
}

/// 合成文件系统的目录结构（内核层实现，内容来自内核状态）。
pub(crate) trait Pseudo {
    /// 文件系统的根目录。
    fn root(&self, fs: Mount) -> PseudoNode;
    /// 节点所在的文件系统。
    fn mount_of(&self, node: &PseudoNode) -> Mount;
    /// 上级目录；根目录返回 None。
    fn parent(&self, node: &PseudoNode) -> Option<PseudoNode>;
    /// 节点在上级目录里的名字。
    fn name(&self, node: &PseudoNode) -> Vec<u8>;
    /// 在目录里查找。
    fn lookup(&self, dir: &PseudoNode, name: &[u8]) -> Result<PseudoNode, Errno>;
    /// 文件类型位（`S_IFMT` 部分）。
    fn kind(&self, node: &PseudoNode) -> Result<u32, Errno>;
    /// 符号链接的内容。
    fn readlink(&self, node: &PseudoNode) -> Result<Vec<u8>, Errno>;
    /// "魔法链接"（`/proc/<pid>/fd/<n>` 等）跟随之后直接得到的节点，不按内容解析。
    fn follow(&self, node: &PseudoNode) -> Option<Node>;
}

/// 一个虚拟文件系统实例。
pub struct Vfs {
    store: Store,
    /// 挂载点：仓库里的目录和挂在上面的文件系统。
    mounts: Vec<(Ino, Mount)>,
    cached: Cached,
    /// 最近执行过的程序文件（[`Vfs::executable`]）。
    executables: crate::loader::ExeCache,
}

/// 不持 BKL 解析路径要用的部分（`syscall::fast`）：仓库的元数据缓存和挂载点。
#[derive(Clone)]
pub(crate) struct Cached {
    meta: Arc<store::Meta>,
    mounts: Arc<[Ino]>,
}

impl Cached {
    /// 只用元数据缓存解析路径并取得元数据：相当于凭据有 `CAP_DAC_READ_SEARCH`（不检查
    /// 搜索权限）时的 [`Vfs::resolve`] 加上 [`Store::stat`]。`root`、`cwd` 是仓库里的
    /// 目录。路径进入合成文件系统，或者要用的东西不在缓存里时返回 None，调用方改为持有
    /// BKL 正常解析（顺带填进缓存）。
    pub(crate) fn stat(
        &self,
        root: Ino,
        cwd: Ino,
        path: &[u8],
        follow: bool,
    ) -> Option<Result<Stat, Errno>> {
        let r = self.meta.reader()?;
        match self.walk(&r, root, cwd, path, follow)? {
            Ok(ino) => r.stat(ino).map(Ok),
            Err(e) => Some(Err(e)),
        }
    }

    /// 与 [`Vfs::walk_in`] 一一对应。
    fn walk(
        &self,
        r: &store::Reader<'_>,
        root: Ino,
        cwd: Ino,
        path: &[u8],
        follow: bool,
    ) -> Option<Result<Ino, Errno>> {
        if path.is_empty() {
            return Some(Err(Errno::ENOENT));
        }
        let mut dir = if path[0] == b'/' { root } else { cwd };
        // 还没处理的组件，栈顶是下一个；不分配内存，放不下时交给正常的解析
        let mut pending = Pending::default();
        pending.push(path)?;
        let mut links = 0;
        while let Some(comp) = pending.pop() {
            let last = pending.is_empty();
            if comp.len() > NAME_MAX {
                return Some(Err(Errno::ENAMETOOLONG));
            }
            if r.kind(dir)? != mode::S_IFDIR {
                return Some(Err(Errno::ENOTDIR));
            }
            if comp == b"." {
                continue;
            }
            if comp == b".." {
                if dir != root {
                    dir = r.parent(dir)?;
                }
                continue;
            }
            let Some(node) = r.lookup(dir, comp)? else {
                return Some(Err(Errno::ENOENT));
            };
            if self.mounts.contains(&node) {
                return None;
            }
            if r.kind(node)? == mode::S_IFLNK && (!last || follow) {
                links += 1;
                if links > MAX_SYMLINKS {
                    return Some(Err(Errno::ELOOP));
                }
                let target = r.readlink(node)?;
                if target.first() == Some(&b'/') {
                    dir = root;
                }
                pending.push(target)?;
                continue;
            }
            dir = node;
        }
        Some(Ok(dir))
    }
}

/// [`Cached::walk`] 还没处理的路径组件（[`components`] 的结果倒过来放）。
struct Pending<'p> {
    comps: [&'p [u8]; 64],
    len: usize,
}

impl Default for Pending<'_> {
    fn default() -> Self {
        Pending {
            comps: [&[]; 64],
            len: 0,
        }
    }
}

impl<'p> Pending<'p> {
    /// 把 `path` 的组件放到最前面；放不下时返回 None。
    fn push(&mut self, path: &'p [u8]) -> Option<()> {
        if path.ends_with(b"/") {
            self.push_one(b".")?;
        }
        for c in path.rsplit(|&b| b == b'/').filter(|c| !c.is_empty()) {
            self.push_one(c)?;
        }
        Some(())
    }

    fn push_one(&mut self, comp: &'p [u8]) -> Option<()> {
        *self.comps.get_mut(self.len)? = comp;
        self.len += 1;
        Some(())
    }

    fn pop(&mut self) -> Option<&'p [u8]> {
        self.len = self.len.checked_sub(1)?;
        Some(self.comps[self.len])
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Vfs {
    /// 打开 `dir` 里的 inode 仓库作为根文件系统，补齐标准的设备节点，挂上 procfs、devpts
    /// 和 sysfs。
    /// 开机（没有别的进程在用这个仓库）时清空 Linux 上是 tmpfs 的 /dev/shm 和 /run：
    /// 共享内存、命名信号量和 pid 文件不能留到下次。
    pub fn open(dir: &Path) -> Result<Vfs, Errno> {
        let store = Store::open(dir, &[b"dev/shm", b"run"])?;
        if let Err(e) = dev::ensure_dev_nodes(&store) {
            crate::debug::log(format_args!("cannot create the device nodes: {e}"));
        }
        if let Err(e) = ensure_resolv_conf(&store) {
            crate::debug::log(format_args!("cannot create /etc/resolv.conf: {e}"));
        }
        let mut mounts = Vec::new();
        for m in [Mount::Proc, Mount::Pts, Mount::Sys] {
            let mut ino = Some(ROOT);
            for name in m.path() {
                ino = ino.and_then(|i| store.lookup(i, name).ok().flatten());
            }
            match ino {
                Some(ino) if store.stat(ino)?.is_dir() => mounts.push((ino, m)),
                _ => crate::debug::log(format_args!("no mount point for {m:?}")),
            }
        }
        let cached = Cached {
            meta: store.meta().clone(),
            mounts: mounts.iter().map(|&(ino, _)| ino).collect(),
        };
        Ok(Vfs {
            store,
            mounts,
            cached,
            executables: Default::default(),
        })
    }

    /// 打开仓库里的普通文件 `ino` 作为要执行的文件（[`crate::loader::ExeCache`]）。
    pub(crate) fn executable(&self, ino: Ino) -> Result<crate::loader::Executable, Errno> {
        self.executables
            .open(ino, || self.store.open_object(ino, false))
    }

    /// 丢掉记着的程序文件（宿主内存紧张时）。
    pub(crate) fn trim(&self) {
        self.executables.clear();
    }

    /// 底层的 inode 仓库。
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 不持 BKL 解析路径要用的部分。
    pub(crate) fn cached(&self) -> &Cached {
        &self.cached
    }

    fn mount_point(&self, m: Mount) -> Option<Ino> {
        self.mounts.iter().find(|(_, mm)| *mm == m).map(|(i, _)| *i)
    }

    /// 仓库里的 inode；如果它是挂载点，换成挂在上面的文件系统的根。
    fn mounted(&self, ps: &dyn Pseudo, ino: Ino) -> Node {
        match self.mounts.iter().find(|(i, _)| *i == ino) {
            Some(&(_, m)) => Node::Pseudo(ps.root(m)),
            None => Node::Store(ino),
        }
    }

    /// 节点的文件类型位。
    pub(crate) fn kind(&self, ps: &dyn Pseudo, node: &Node) -> Result<u32, Errno> {
        match node {
            Node::Store(ino) => self.store.kind(*ino),
            Node::Pseudo(p) => ps.kind(p),
        }
    }

    /// 是不是目录。
    pub(crate) fn is_dir(&self, ps: &dyn Pseudo, node: &Node) -> Result<bool, Errno> {
        Ok(self.kind(ps, node)? == mode::S_IFDIR)
    }

    /// 在目录里查找一个名字（不是 `.` 或 `..`）。
    fn lookup(&self, ps: &dyn Pseudo, dir: &Node, name: &[u8]) -> Result<Node, Errno> {
        match dir {
            Node::Store(ino) => {
                let child = self.store.lookup(*ino, name)?.ok_or(Errno::ENOENT)?;
                Ok(self.mounted(ps, child))
            }
            Node::Pseudo(p) => ps.lookup(p, name).map(Node::Pseudo),
        }
    }

    /// 目录的上级目录；合成文件系统的根的上级是挂载点的上级。
    pub(crate) fn parent(&self, ps: &dyn Pseudo, dir: &Node) -> Result<Node, Errno> {
        match dir {
            Node::Store(ino) => Ok(Node::Store(self.store.parent(*ino)?)),
            Node::Pseudo(p) => match ps.parent(p) {
                Some(up) => Ok(Node::Pseudo(up)),
                None => {
                    let point = self.mount_point(ps.mount_of(p)).ok_or(Errno::ENOENT)?;
                    Ok(Node::Store(self.store.parent(point)?))
                }
            },
        }
    }

    /// 符号链接的内容。
    pub(crate) fn readlink(&self, ps: &dyn Pseudo, node: &Node) -> Result<Vec<u8>, Errno> {
        match node {
            Node::Store(ino) => self.store.readlink(*ino),
            Node::Pseudo(p) => ps.readlink(p),
        }
    }

    /// 凭据为 `cred` 的进程能不能在目录 `dir` 里查找。合成文件系统的目录总是可以。
    fn may_search(&self, cred: &Cred, dir: &Node) -> Result<(), Errno> {
        match dir {
            Node::Store(ino) if !cred.searches_all() => {
                cred.permission(&self.store.stat(*ino)?, MAY_EXEC)
            }
            _ => Ok(()),
        }
    }

    /// 以凭据 `cred`、根目录 `root` 解析路径。`follow` 为 false 时不跟随最后一个组件的
    /// 符号链接（lstat、readlink 等）。
    pub(crate) fn resolve(
        &self,
        ps: &dyn Pseudo,
        cred: &Cred,
        root: &Node,
        cwd: &Node,
        path: &[u8],
        follow: bool,
    ) -> Result<Node, Errno> {
        self.walk(ps, cred, root, cwd, path, follow)
            .map(|(node, _)| node)
    }

    /// 路径指向的文件的绝对路径，符号链接全部解析（`/proc/<pid>/exe`，与 Linux 的
    /// `d_path` 相同）。经由 `/proc/<pid>/fd/<n>` 到达、不知道名字的文件报 ENOENT。
    pub(crate) fn real_path(
        &self,
        ps: &dyn Pseudo,
        cred: &Cred,
        root: &Node,
        cwd: &Node,
        path: &[u8],
    ) -> Result<Vec<u8>, Errno> {
        match self.walk(ps, cred, root, cwd, path, true)? {
            (_, Some((dir, name))) => {
                let mut p = self.path_of(ps, root, &dir)?;
                if p != b"/" {
                    p.push(b'/');
                }
                p.extend_from_slice(&name);
                Ok(p)
            }
            (node, None) if self.is_dir(ps, &node)? => self.path_of(ps, root, &node),
            _ => Err(Errno::ENOENT),
        }
    }

    /// [`resolve`](Self::resolve)，另外返回最后找到结果的目录和名字；结果是经由 `.`、
    /// `..`、根目录或者合成文件系统的链接得到的时候没有。
    pub(crate) fn walk(
        &self,
        ps: &dyn Pseudo,
        cred: &Cred,
        root: &Node,
        cwd: &Node,
        path: &[u8],
        follow: bool,
    ) -> Result<(Node, Option<Dentry>), Errno> {
        self.store
            .snapshot(|_| self.walk_in(ps, cred, root, cwd, path, follow))
    }

    fn walk_in(
        &self,
        ps: &dyn Pseudo,
        cred: &Cred,
        root: &Node,
        cwd: &Node,
        path: &[u8],
        follow: bool,
    ) -> Result<(Node, Option<Dentry>), Errno> {
        if path.is_empty() {
            return Err(Errno::ENOENT);
        }
        let mut dir = if path[0] == b'/' {
            root.clone()
        } else {
            cwd.clone()
        };
        let mut found = None;
        let mut pending = components(path);
        let mut links = 0;
        while let Some(comp) = pending.pop_front() {
            let last = pending.is_empty();
            if comp.len() > NAME_MAX {
                return Err(Errno::ENAMETOOLONG);
            }
            // 每个组件（包括 . 和 ..）之前的节点都必须是目录：file/. 和 file/ 是 ENOTDIR
            if !self.is_dir(ps, &dir)? {
                return Err(Errno::ENOTDIR);
            }
            self.may_search(cred, &dir)?;
            if comp == b"." {
                continue;
            }
            if comp == b".." {
                if dir != *root {
                    dir = self.parent(ps, &dir)?;
                }
                found = None;
                continue;
            }
            let node = self.lookup(ps, &dir, &comp)?;
            if self.kind(ps, &node)? == mode::S_IFLNK && (!last || follow) {
                links += 1;
                if links > MAX_SYMLINKS {
                    return Err(Errno::ELOOP);
                }
                if let Node::Pseudo(p) = &node
                    && let Some(target) = ps.follow(p)
                {
                    dir = target;
                    found = None;
                    continue;
                }
                let target = self.readlink(ps, &node)?;
                if target.first() == Some(&b'/') {
                    dir = root.clone();
                    found = None;
                }
                for c in components(&target).into_iter().rev() {
                    pending.push_front(c);
                }
                continue;
            }
            found = Some((std::mem::replace(&mut dir, node), comp));
        }
        Ok((dir, found))
    }

    /// 解析路径的父目录，返回父目录和最后一个组件（去掉结尾的 `/`）。
    ///
    /// 最后一个组件可能是 `.` 或 `..`，由调用方决定怎么处理；路径是 `/` 时返回 `(ROOT, ".")`。
    pub(crate) fn resolve_parent<'p>(
        &self,
        ps: &dyn Pseudo,
        cred: &Cred,
        root: &Node,
        cwd: &Node,
        path: &'p [u8],
    ) -> Result<(Node, &'p [u8]), Errno> {
        if path.is_empty() {
            return Err(Errno::ENOENT);
        }
        let end = path.iter().rposition(|&b| b != b'/').map_or(0, |i| i + 1);
        let trimmed = &path[..end];
        if trimmed.is_empty() {
            return Ok((root.clone(), b"."));
        }
        let (dir, name) = match trimmed.iter().rposition(|&b| b == b'/') {
            Some(i) => (
                self.resolve(ps, cred, root, cwd, &trimmed[..=i], true)?,
                &trimmed[i + 1..],
            ),
            None => (cwd.clone(), trimmed),
        };
        if name.len() > NAME_MAX {
            return Err(Errno::ENAMETOOLONG);
        }
        if !self.is_dir(ps, &dir)? {
            return Err(Errno::ENOTDIR);
        }
        Ok((dir, name))
    }

    /// 目录相对根目录 `root` 的绝对路径（getcwd、`/proc/<pid>/cwd`）。不在 `root` 下面
    /// 的目录与 Linux 一样以 `(unreachable)` 开头。
    pub(crate) fn path_of(
        &self,
        ps: &dyn Pseudo,
        root: &Node,
        dir: &Node,
    ) -> Result<Vec<u8>, Errno> {
        let mut names = Vec::new();
        let mut cur = dir.clone();
        let mut reached = true;
        loop {
            if cur == *root {
                break;
            }
            match cur {
                Node::Store(ROOT) => {
                    reached = false;
                    break;
                }
                Node::Store(ino) => {
                    let parent = self.store.parent(ino)?;
                    // 已被删除的目录找不到名字，与 Linux 一样报 ENOENT
                    names.push(self.store.name_of(parent, ino)?.ok_or(Errno::ENOENT)?);
                    cur = Node::Store(parent);
                }
                Node::Pseudo(p) => match ps.parent(&p) {
                    Some(up) => {
                        names.push(ps.name(&p));
                        cur = Node::Pseudo(up);
                    }
                    None => {
                        let point = self.mount_point(ps.mount_of(&p)).ok_or(Errno::ENOENT)?;
                        cur = Node::Store(point);
                    }
                },
            }
        }
        let mut out = if reached {
            Vec::new()
        } else {
            b"(unreachable)".to_vec()
        };
        if names.is_empty() {
            out.push(b'/');
            return Ok(out);
        }
        for name in names.iter().rev() {
            out.push(b'/');
            out.extend_from_slice(name);
        }
        Ok(out)
    }
}

/// 没有 `/etc/resolv.conf` 时写一个指向 DNS 存根的（设计文档 §10）；已有的保持不动。
fn ensure_resolv_conf(store: &Store) -> Result<(), Errno> {
    let Some(etc) = store.lookup(ROOT, b"etc")? else {
        return Ok(());
    };
    if store.lookup(etc, b"resolv.conf")?.is_some() {
        return Ok(());
    }
    write_file(
        store,
        b"/etc/resolv.conf",
        b"nameserver 127.0.0.53\n",
        0o644,
        false,
    )
}

/// 在仓库里写一个普通文件（宿主准备配置文件用）：父目录不存在时一并建好（0755），
/// 路径里的符号链接不跟随。文件已经存在时，`overwrite` 为 false 就保持不动，否则
/// 替换内容（权限位不变）。
pub fn write_file(
    store: &Store,
    path: &[u8],
    data: &[u8],
    mode: u32,
    overwrite: bool,
) -> Result<(), Errno> {
    use std::io::Write;
    let comps: Vec<Vec<u8>> = split(path).collect();
    let (name, dirs) = comps.split_last().ok_or(Errno::EINVAL)?;
    let mut dir = ROOT;
    for c in dirs {
        dir = match store.lookup(dir, c)? {
            Some(ino) if store.kind(ino)? == mode::S_IFDIR => ino,
            Some(_) => return Err(Errno::ENOTDIR),
            None => store.mkdir(dir, c, 0o755, 0, 0)?,
        };
    }
    let mut file = match store.lookup(dir, name)? {
        Some(_) if !overwrite => return Ok(()),
        Some(ino) if store.kind(ino)? == mode::S_IFREG => {
            store.truncate(ino, 0)?;
            store.open_object(ino, true)?
        }
        Some(_) => return Err(Errno::EEXIST),
        None => store.create_file(dir, name, mode & 0o7777, 0, 0)?.1,
    };
    file.write_all(data)
        .map_err(|e| Errno(e.raw_os_error().unwrap_or(Errno::EIO.0)))
}

/// 拆分路径组件，去掉空组件和 `.`。
/// 路径解析的组件：保留 `.`，以 `/` 结尾时再补一个 `.`，这样前面的节点必须是目录，
/// 最后一个组件是符号链接时也会被跟随（与 Linux 一样）。
fn components(path: &[u8]) -> VecDeque<Vec<u8>> {
    let mut comps: VecDeque<Vec<u8>> = path
        .split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    if path.ends_with(b"/") {
        comps.push_back(b".".to_vec());
    }
    comps
}

fn split(path: &[u8]) -> impl DoubleEndedIterator<Item = Vec<u8>> + '_ {
    path.split(|&b| b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
        .map(<[u8]>::to_vec)
}

/// 目录和其中的一个名字。
pub(crate) type Dentry = (Node, Vec<u8>);

/// 词法上规范化一个绝对路径：去掉 `.`、空组件，按 `..` 回退（不跟随符号链接）。
pub(crate) fn normalize(path: &[u8]) -> Vec<u8> {
    let mut comps: Vec<&[u8]> = Vec::new();
    for c in path.split(|&b| b == b'/') {
        match c {
            b"" | b"." => {}
            b".." => {
                comps.pop();
            }
            c => comps.push(c),
        }
    }
    if comps.is_empty() {
        return b"/".to_vec();
    }
    let mut out = Vec::new();
    for c in comps {
        out.push(b'/');
        out.extend_from_slice(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只有一个空根目录的合成文件系统。
    struct Empty;

    impl Pseudo for Empty {
        fn root(&self, fs: Mount) -> PseudoNode {
            match fs {
                Mount::Proc => PseudoNode::ProcRoot,
                Mount::Pts => PseudoNode::PtsRoot,
                Mount::Sys => PseudoNode::SysRoot,
                Mount::Cgroup => PseudoNode::Cgroup(b""),
            }
        }
        fn mount_of(&self, node: &PseudoNode) -> Mount {
            match node {
                PseudoNode::PtsRoot => Mount::Pts,
                PseudoNode::SysRoot => Mount::Sys,
                _ => Mount::Proc,
            }
        }
        fn parent(&self, _: &PseudoNode) -> Option<PseudoNode> {
            None
        }
        fn name(&self, _: &PseudoNode) -> Vec<u8> {
            Vec::new()
        }
        fn lookup(&self, _: &PseudoNode, _: &[u8]) -> Result<PseudoNode, Errno> {
            Err(Errno::ENOENT)
        }
        fn kind(&self, _: &PseudoNode) -> Result<u32, Errno> {
            Ok(mode::S_IFDIR)
        }
        fn readlink(&self, _: &PseudoNode) -> Result<Vec<u8>, Errno> {
            Err(Errno::EINVAL)
        }
        fn follow(&self, _: &PseudoNode) -> Option<Node> {
            None
        }
    }

    fn sample(name: &str) -> (Vfs, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("appine-vfs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = Store::create(&dir).unwrap();
        let bin = s.mkdir(ROOT, b"bin", 0o755, 0, 0).unwrap();
        let usr = s.mkdir(ROOT, b"usr", 0o755, 0, 0).unwrap();
        let ubin = s.mkdir(usr, b"bin", 0o755, 0, 0).unwrap();
        s.mkdir(ROOT, b"proc", 0o555, 0, 0).unwrap();
        s.create_file(bin, b"busybox", 0o755, 0, 0).unwrap();
        s.symlink(ubin, b"env", b"/bin/busybox", 0, 0).unwrap();
        s.symlink(ubin, b"rel", b"../../bin/busybox", 0, 0).unwrap();
        s.symlink(ROOT, b"loop", b"loop", 0, 0).unwrap();
        s.symlink(ROOT, b"bindir", b"bin", 0, 0).unwrap();
        s.symlink(ROOT, b"slash", b"bin/busybox/", 0, 0).unwrap();
        drop(s);
        (Vfs::open(&dir).unwrap(), dir)
    }

    #[test]
    fn resolves_paths_and_symlinks() {
        let (vfs, dir) = sample("resolve");
        let ps = &Empty;
        let root_cred = Cred::root();
        let cred = &root_cred;
        let r = |cwd: &Node, p: &[u8], follow| vfs.resolve(ps, cred, &Node::ROOT, cwd, p, follow);
        let root = Node::ROOT;
        let busybox = r(&root, b"/bin/busybox", true).unwrap();
        assert_eq!(r(&root, b"/usr/bin/env", true).unwrap(), busybox);
        assert_eq!(r(&root, b"/usr/bin/rel", true).unwrap(), busybox);
        assert_eq!(r(&root, b"/bindir/busybox", false).unwrap(), busybox);
        assert_eq!(
            r(&root, b"//usr/./bin/../../bin/busybox", true).unwrap(),
            busybox
        );
        let usr = r(&root, b"usr", true).unwrap();
        assert_eq!(r(&usr, b"../bin/busybox", true).unwrap(), busybox);
        assert_eq!(r(&root, b"/..", true).unwrap(), root);

        let env = r(&root, b"/usr/bin/env", false).unwrap();
        assert_eq!(vfs.kind(ps, &env).unwrap(), mode::S_IFLNK);
        assert_eq!(r(&root, b"/loop", true), Err(Errno::ELOOP));
        assert_eq!(r(&root, b"/missing", true), Err(Errno::ENOENT));
        assert_eq!(r(&root, b"/bin/busybox/x", true), Err(Errno::ENOTDIR));
        assert_eq!(r(&root, b"/bin/busybox/", true), Err(Errno::ENOTDIR));
        // . 和 .. 之前也必须是目录；符号链接的目标以 / 结尾时同样要求是目录
        assert_eq!(r(&root, b"/bin/busybox/.", true), Err(Errno::ENOTDIR));
        assert_eq!(r(&root, b"/bin/busybox/..", true), Err(Errno::ENOTDIR));
        assert_eq!(r(&root, b"/slash", true), Err(Errno::ENOTDIR));
        assert_eq!(
            vfs.kind(ps, &r(&root, b"/slash", false).unwrap()).unwrap(),
            mode::S_IFLNK
        );
        assert_eq!(
            r(&root, b"/bindir/", false).unwrap(),
            r(&root, b"/bin", true).unwrap()
        );
        assert_eq!(r(&root, b"/", true).unwrap(), root);
        assert_eq!(r(&root, b"", true), Err(Errno::ENOENT));

        let ubin = r(&root, b"/usr/bin", true).unwrap();
        assert_eq!(
            vfs.resolve_parent(ps, cred, &Node::ROOT, &root, b"/usr/bin/new")
                .unwrap(),
            (ubin.clone(), &b"new"[..])
        );
        assert_eq!(
            vfs.resolve_parent(ps, cred, &Node::ROOT, &usr, b"bin//")
                .unwrap(),
            (usr.clone(), &b"bin"[..])
        );
        assert_eq!(
            vfs.resolve_parent(ps, cred, &Node::ROOT, &usr, b"///")
                .unwrap(),
            (root.clone(), &b"."[..])
        );
        assert_eq!(
            vfs.resolve_parent(ps, cred, &Node::ROOT, &root, b"/bin/busybox/x"),
            Err(Errno::ENOTDIR)
        );
        assert_eq!(vfs.path_of(ps, &Node::ROOT, &ubin).unwrap(), b"/usr/bin");
        assert_eq!(vfs.path_of(ps, &Node::ROOT, &root).unwrap(), b"/");

        // 解析完符号链接之后的路径（/proc/<pid>/exe）
        let real = |cwd: &Node, p: &[u8]| vfs.real_path(ps, cred, &Node::ROOT, cwd, p);
        for p in [
            &b"/usr/bin/env"[..],
            b"/usr/bin/rel",
            b"/bindir/busybox",
            b"/bin/busybox",
        ] {
            assert_eq!(real(&root, p).unwrap(), b"/bin/busybox");
        }
        assert_eq!(real(&usr, b"bin/rel").unwrap(), b"/bin/busybox");
        assert_eq!(real(&usr, b"../bindir/").unwrap(), b"/bin");
        assert_eq!(real(&ubin, b".").unwrap(), b"/usr/bin");
        assert_eq!(real(&ubin, b"..").unwrap(), b"/usr");
        assert_eq!(real(&root, b"/").unwrap(), b"/");
        assert_eq!(real(&root, b"/missing"), Err(Errno::ENOENT));

        // /proc 是挂载点：进去是合成文件系统的根，`..` 回到仓库的根目录
        let proc = r(&root, b"/proc", true).unwrap();
        assert_eq!(proc, Node::Pseudo(PseudoNode::ProcRoot));
        assert_eq!(r(&proc, b"..", true).unwrap(), root);
        assert_eq!(vfs.path_of(ps, &Node::ROOT, &proc).unwrap(), b"/proc");
        // 设备节点和 /dev/pts 由 Vfs::open 补齐
        let pts = r(&root, b"/dev/pts", true).unwrap();
        assert_eq!(pts, Node::Pseudo(PseudoNode::PtsRoot));
        assert_eq!(vfs.path_of(ps, &Node::ROOT, &pts).unwrap(), b"/dev/pts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_from_the_cache_like_the_full_walk() {
        let (vfs, dir) = sample("cached");
        let ps = &Empty;
        let cred = &Cred::root();
        let cached = vfs.cached();
        let usr = match vfs.resolve(ps, cred, &Node::ROOT, &Node::ROOT, b"/usr", true) {
            Ok(Node::Store(ino)) => ino,
            other => panic!("{other:?}"),
        };
        let paths: [&[u8]; 20] = [
            b"/bin/busybox",
            b"/usr/bin/env",
            b"/usr/bin/rel",
            b"/bindir/busybox",
            b"//usr/./bin/../../bin/busybox",
            b"../bin/busybox",
            b"/..",
            b"/loop",
            b"/missing",
            b"/bin/missing/x",
            b"/bin/busybox/x",
            b"/bin/busybox/",
            b"/bin/busybox/.",
            b"/bin/busybox/..",
            b"/slash",
            b"/bindir/",
            b"/",
            b"bin",
            b"bin/rel",
            b"./bin/../../bindir/",
        ];
        for follow in [true, false] {
            for cwd in [ROOT, usr] {
                for p in paths {
                    let full = vfs
                        .resolve(ps, cred, &Node::ROOT, &Node::Store(cwd), p, follow)
                        .and_then(|n| match n {
                            Node::Store(ino) => vfs.store().stat(ino),
                            Node::Pseudo(_) => unreachable!(),
                        });
                    // 正常解析把要用的都填进了缓存，之后只用缓存也得到同样的结果
                    assert_eq!(
                        cached.stat(ROOT, cwd, p, follow),
                        Some(full),
                        "{} from {cwd}, follow {follow}",
                        String::from_utf8_lossy(p)
                    );
                }
            }
        }
        // 合成文件系统交给正常解析
        assert_eq!(cached.stat(ROOT, ROOT, b"/proc", true), None);
        assert_eq!(cached.stat(ROOT, ROOT, b"/dev/pts/0", true), None);
        // 没解析过的名字也交给正常解析
        assert_eq!(cached.stat(ROOT, ROOT, b"/bin/sh", true), None);
        // 改了之后不会用到旧的条目
        let bin = match vfs.resolve(ps, cred, &Node::ROOT, &Node::ROOT, b"/bin", true) {
            Ok(Node::Store(ino)) => ino,
            other => panic!("{other:?}"),
        };
        vfs.store().unlink(bin, b"busybox").unwrap();
        assert_eq!(cached.stat(ROOT, ROOT, b"/bin/busybox", true), None);
        assert_eq!(cached.stat(ROOT, usr, b"bin/env", true), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalizes_paths_lexically() {
        assert_eq!(normalize(b"/a/./b//../c"), b"/a/c");
        assert_eq!(normalize(b"/../.."), b"/");
        assert_eq!(normalize(b"/tmp/x/"), b"/tmp/x");
    }
}
