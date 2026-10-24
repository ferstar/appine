//! 虚拟文件系统（设计文档 §8）。
//!
//! M1b 只有一个根文件系统，就是 inode 仓库；devfs、procfs 等挂载点随后加入。
//! 路径解析完全在仓库的命名空间里进行，符号链接最多跟随 40 层。

pub mod import;
pub mod store;

use std::collections::VecDeque;
use std::path::Path;

use crate::errno::Errno;
use store::{Ino, ROOT, Stat, Store};

/// 跟随符号链接的最大次数，与 Linux 的 MAXSYMLINKS 一致。
const MAX_SYMLINKS: u32 = 40;

/// 一个虚拟文件系统实例。
pub struct Vfs {
    store: Store,
}

impl Vfs {
    /// 打开 `dir` 里的 inode 仓库作为根文件系统。
    pub fn open(dir: &Path) -> Result<Vfs, Errno> {
        Ok(Vfs {
            store: Store::open(dir)?,
        })
    }

    /// 底层的 inode 仓库。
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 解析路径。`follow` 为 false 时不跟随最后一个组件的符号链接（lstat、readlink 等）。
    pub fn resolve(&self, cwd: Ino, path: &[u8], follow: bool) -> Result<Ino, Errno> {
        if path.is_empty() {
            return Err(Errno::ENOENT);
        }
        let mut dir = if path[0] == b'/' { ROOT } else { cwd };
        let mut pending: VecDeque<Vec<u8>> = split(path).collect();
        let mut links = 0;
        // 路径以 / 结尾时，最后一个组件必须是目录，并且要跟随符号链接
        let trailing_slash = path.ends_with(b"/");
        while let Some(comp) = pending.pop_front() {
            let last = pending.is_empty();
            if comp == b".." {
                dir = self.store.parent(dir)?;
                continue;
            }
            if !self.store.stat(dir)?.is_dir() {
                return Err(Errno::ENOTDIR);
            }
            let ino = self.store.lookup(dir, &comp)?.ok_or(Errno::ENOENT)?;
            let st = self.store.stat(ino)?;
            if st.is_symlink() && (!last || follow || trailing_slash) {
                links += 1;
                if links > MAX_SYMLINKS {
                    return Err(Errno::ELOOP);
                }
                let target = self.store.readlink(ino)?;
                if target.first() == Some(&b'/') {
                    dir = ROOT;
                }
                for c in split(&target).rev() {
                    pending.push_front(c);
                }
                continue;
            }
            dir = ino;
        }
        if trailing_slash && !self.store.stat(dir)?.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        Ok(dir)
    }

    /// 解析路径的父目录，返回父目录和最后一个组件（去掉结尾的 `/`）。
    ///
    /// 最后一个组件可能是 `.` 或 `..`，由调用方决定怎么处理；路径是 `/` 时返回 `(ROOT, ".")`。
    pub fn resolve_parent<'p>(&self, cwd: Ino, path: &'p [u8]) -> Result<(Ino, &'p [u8]), Errno> {
        if path.is_empty() {
            return Err(Errno::ENOENT);
        }
        let end = path.iter().rposition(|&b| b != b'/').map_or(0, |i| i + 1);
        let trimmed = &path[..end];
        if trimmed.is_empty() {
            return Ok((ROOT, b"."));
        }
        let (dir, name) = match trimmed.iter().rposition(|&b| b == b'/') {
            Some(i) => (self.resolve(cwd, &trimmed[..=i], true)?, &trimmed[i + 1..]),
            None => (cwd, trimmed),
        };
        if !self.store.stat(dir)?.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        Ok((dir, name))
    }

    /// 目录的绝对路径（getcwd 用）。
    pub fn path_of(&self, dir: Ino) -> Result<Vec<u8>, Errno> {
        let mut names = Vec::new();
        let mut cur = dir;
        while cur != ROOT {
            let parent = self.store.parent(cur)?;
            // 已被删除的目录找不到名字，与 Linux 一样报 ENOENT
            names.push(self.store.name_of(parent, cur)?.ok_or(Errno::ENOENT)?);
            cur = parent;
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

    /// 解析路径并读取元数据。
    pub fn stat_path(&self, cwd: Ino, path: &[u8], follow: bool) -> Result<Stat, Errno> {
        let ino = self.resolve(cwd, path, follow)?;
        self.store.stat(ino)
    }
}

/// 拆分路径组件，去掉空组件和 `.`。
fn split(path: &[u8]) -> impl DoubleEndedIterator<Item = Vec<u8>> + '_ {
    path.split(|&b| b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
        .map(<[u8]>::to_vec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Vfs, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("appine-vfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = Store::create(&dir).unwrap();
        let bin = s.mkdir(ROOT, b"bin", 0o755, 0, 0).unwrap();
        let usr = s.mkdir(ROOT, b"usr", 0o755, 0, 0).unwrap();
        let ubin = s.mkdir(usr, b"bin", 0o755, 0, 0).unwrap();
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
        let busybox = vfs.resolve(ROOT, b"/bin/busybox", true).unwrap();
        assert_eq!(vfs.resolve(ROOT, b"/usr/bin/env", true).unwrap(), busybox);
        assert_eq!(vfs.resolve(ROOT, b"/usr/bin/rel", true).unwrap(), busybox);
        assert_eq!(
            vfs.resolve(ROOT, b"/bindir/busybox", false).unwrap(),
            busybox
        );
        assert_eq!(
            vfs.resolve(ROOT, b"//usr/./bin/../../bin/busybox", true)
                .unwrap(),
            busybox
        );
        let usr = vfs.resolve(ROOT, b"usr", true).unwrap();
        assert_eq!(vfs.resolve(usr, b"../bin/busybox", true).unwrap(), busybox);
        assert_eq!(vfs.resolve(ROOT, b"/..", true).unwrap(), ROOT);

        let env = vfs.resolve(ROOT, b"/usr/bin/env", false).unwrap();
        assert!(vfs.store().stat(env).unwrap().is_symlink());
        assert_eq!(vfs.resolve(ROOT, b"/loop", true), Err(Errno::ELOOP));
        assert_eq!(vfs.resolve(ROOT, b"/missing", true), Err(Errno::ENOENT));
        assert_eq!(
            vfs.resolve(ROOT, b"/bin/busybox/x", true),
            Err(Errno::ENOTDIR)
        );
        assert_eq!(
            vfs.resolve(ROOT, b"/bin/busybox/", true),
            Err(Errno::ENOTDIR)
        );
        assert_eq!(vfs.resolve(ROOT, b"", true), Err(Errno::ENOENT));

        let ubin = vfs.resolve(ROOT, b"/usr/bin", true).unwrap();
        assert_eq!(
            vfs.resolve_parent(ROOT, b"/usr/bin/new").unwrap(),
            (ubin, &b"new"[..])
        );
        assert_eq!(
            vfs.resolve_parent(usr, b"bin//").unwrap(),
            (usr, &b"bin"[..])
        );
        assert_eq!(vfs.resolve_parent(usr, b"///").unwrap(), (ROOT, &b"."[..]));
        assert_eq!(
            vfs.resolve_parent(ROOT, b"/bin/busybox/x"),
            Err(Errno::ENOTDIR)
        );
        assert_eq!(vfs.path_of(ubin).unwrap(), b"/usr/bin");
        assert_eq!(vfs.path_of(ROOT).unwrap(), b"/");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
