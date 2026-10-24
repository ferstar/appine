//! 虚拟文件系统（设计文档 §8）。
//!
//! 根文件系统是 inode 仓库，`/dev` 里的设备节点也存在仓库里（[`dev`]）。`/proc`
//! （procfs）和 `/dev/pts`（devpts）是挂在仓库目录上的合成文件系统：它们的内容来自
//! 内核状态，由内核层经由 [`Pseudo`] 提供。路径解析在整棵树里进行，符号链接最多
//! 跟随 40 层。

pub mod dev;
pub mod import;
pub mod store;

use std::collections::VecDeque;
use std::path::Path;

use crate::errno::Errno;
pub(crate) use crate::pseudo::PseudoNode;
use store::{Ino, ROOT, Store, mode};

/// 跟随符号链接的最大次数，与 Linux 的 MAXSYMLINKS 一致。
const MAX_SYMLINKS: u32 = 40;

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
}

impl Mount {
    /// 挂载点在仓库里的路径。
    const fn path(self) -> &'static [&'static [u8]] {
        match self {
            Mount::Proc => &[b"proc"],
            Mount::Pts => &[b"dev", b"pts"],
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
}

impl Vfs {
    /// 打开 `dir` 里的 inode 仓库作为根文件系统，补齐标准的设备节点，挂上 procfs 和 devpts。
    pub fn open(dir: &Path) -> Result<Vfs, Errno> {
        let store = Store::open(dir)?;
        if let Err(e) = dev::ensure_dev_nodes(&store) {
            crate::debug::log(format_args!("cannot create the device nodes: {e}"));
        }
        let mut mounts = Vec::new();
        for m in [Mount::Proc, Mount::Pts] {
            let mut ino = Some(ROOT);
            for name in m.path() {
                ino = ino.and_then(|i| store.lookup(i, name).ok().flatten());
            }
            match ino {
                Some(ino) if store.stat(ino)?.is_dir() => mounts.push((ino, m)),
                _ => crate::debug::log(format_args!("no mount point for {m:?}")),
            }
        }
        Ok(Vfs { store, mounts })
    }

    /// 底层的 inode 仓库。
    pub fn store(&self) -> &Store {
        &self.store
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
            Node::Store(ino) => Ok(self.store.stat(*ino)?.kind()),
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

    /// 解析路径。`follow` 为 false 时不跟随最后一个组件的符号链接（lstat、readlink 等）。
    pub(crate) fn resolve(
        &self,
        ps: &dyn Pseudo,
        cwd: &Node,
        path: &[u8],
        follow: bool,
    ) -> Result<Node, Errno> {
        if path.is_empty() {
            return Err(Errno::ENOENT);
        }
        let mut dir = if path[0] == b'/' {
            Node::ROOT
        } else {
            cwd.clone()
        };
        let mut pending: VecDeque<Vec<u8>> = split(path).collect();
        let mut links = 0;
        // 路径以 / 结尾时，最后一个组件必须是目录，并且要跟随符号链接
        let trailing_slash = path.ends_with(b"/");
        while let Some(comp) = pending.pop_front() {
            let last = pending.is_empty();
            if comp == b".." {
                dir = self.parent(ps, &dir)?;
                continue;
            }
            if !self.is_dir(ps, &dir)? {
                return Err(Errno::ENOTDIR);
            }
            let node = self.lookup(ps, &dir, &comp)?;
            if self.kind(ps, &node)? == mode::S_IFLNK && (!last || follow || trailing_slash) {
                links += 1;
                if links > MAX_SYMLINKS {
                    return Err(Errno::ELOOP);
                }
                if let Node::Pseudo(p) = &node
                    && let Some(target) = ps.follow(p)
                {
                    dir = target;
                    continue;
                }
                let target = self.readlink(ps, &node)?;
                if target.first() == Some(&b'/') {
                    dir = Node::ROOT;
                }
                for c in split(&target).rev() {
                    pending.push_front(c);
                }
                continue;
            }
            dir = node;
        }
        if trailing_slash && !self.is_dir(ps, &dir)? {
            return Err(Errno::ENOTDIR);
        }
        Ok(dir)
    }

    /// 解析路径的父目录，返回父目录和最后一个组件（去掉结尾的 `/`）。
    ///
    /// 最后一个组件可能是 `.` 或 `..`，由调用方决定怎么处理；路径是 `/` 时返回 `(ROOT, ".")`。
    pub(crate) fn resolve_parent<'p>(
        &self,
        ps: &dyn Pseudo,
        cwd: &Node,
        path: &'p [u8],
    ) -> Result<(Node, &'p [u8]), Errno> {
        if path.is_empty() {
            return Err(Errno::ENOENT);
        }
        let end = path.iter().rposition(|&b| b != b'/').map_or(0, |i| i + 1);
        let trimmed = &path[..end];
        if trimmed.is_empty() {
            return Ok((Node::ROOT, b"."));
        }
        let (dir, name) = match trimmed.iter().rposition(|&b| b == b'/') {
            Some(i) => (
                self.resolve(ps, cwd, &trimmed[..=i], true)?,
                &trimmed[i + 1..],
            ),
            None => (cwd.clone(), trimmed),
        };
        if !self.is_dir(ps, &dir)? {
            return Err(Errno::ENOTDIR);
        }
        Ok((dir, name))
    }

    /// 目录的绝对路径（getcwd、`/proc/<pid>/cwd`）。
    pub(crate) fn path_of(&self, ps: &dyn Pseudo, dir: &Node) -> Result<Vec<u8>, Errno> {
        let mut names = Vec::new();
        let mut cur = dir.clone();
        loop {
            match cur {
                Node::Store(ROOT) => break,
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
        if names.is_empty() {
            return Ok(b"/".to_vec());
        }
        let mut out = Vec::new();
        for name in names.iter().rev() {
            out.push(b'/');
            out.extend_from_slice(name);
        }
        Ok(out)
    }
}

/// 拆分路径组件，去掉空组件和 `.`。
fn split(path: &[u8]) -> impl DoubleEndedIterator<Item = Vec<u8>> + '_ {
    path.split(|&b| b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
        .map(<[u8]>::to_vec)
}

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
            }
        }
        fn mount_of(&self, node: &PseudoNode) -> Mount {
            match node {
                PseudoNode::PtsRoot => Mount::Pts,
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

    fn sample() -> (Vfs, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("appine-vfs-{}", std::process::id()));
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
        drop(s);
        (Vfs::open(&dir).unwrap(), dir)
    }

    #[test]
    fn resolves_paths_and_symlinks() {
        let (vfs, dir) = sample();
        let ps = &Empty;
        let r = |cwd: &Node, p: &[u8], follow| vfs.resolve(ps, cwd, p, follow);
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
        assert_eq!(r(&root, b"", true), Err(Errno::ENOENT));

        let ubin = r(&root, b"/usr/bin", true).unwrap();
        assert_eq!(
            vfs.resolve_parent(ps, &root, b"/usr/bin/new").unwrap(),
            (ubin.clone(), &b"new"[..])
        );
        assert_eq!(
            vfs.resolve_parent(ps, &usr, b"bin//").unwrap(),
            (usr.clone(), &b"bin"[..])
        );
        assert_eq!(
            vfs.resolve_parent(ps, &usr, b"///").unwrap(),
            (root.clone(), &b"."[..])
        );
        assert_eq!(
            vfs.resolve_parent(ps, &root, b"/bin/busybox/x"),
            Err(Errno::ENOTDIR)
        );
        assert_eq!(vfs.path_of(ps, &ubin).unwrap(), b"/usr/bin");
        assert_eq!(vfs.path_of(ps, &root).unwrap(), b"/");

        // /proc 是挂载点：进去是合成文件系统的根，`..` 回到仓库的根目录
        let proc = r(&root, b"/proc", true).unwrap();
        assert_eq!(proc, Node::Pseudo(PseudoNode::ProcRoot));
        assert_eq!(r(&proc, b"..", true).unwrap(), root);
        assert_eq!(vfs.path_of(ps, &proc).unwrap(), b"/proc");
        // 设备节点和 /dev/pts 由 Vfs::open 补齐
        let pts = r(&root, b"/dev/pts", true).unwrap();
        assert_eq!(pts, Node::Pseudo(PseudoNode::PtsRoot));
        assert_eq!(vfs.path_of(ps, &pts).unwrap(), b"/dev/pts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalizes_paths_lexically() {
        assert_eq!(normalize(b"/a/./b//../c"), b"/a/c");
        assert_eq!(normalize(b"/../.."), b"/");
        assert_eq!(normalize(b"/tmp/x/"), b"/tmp/x");
    }
}
