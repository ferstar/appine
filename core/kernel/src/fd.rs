//! 打开文件与 fd 表。
//!
//! 一个打开文件（Linux 的 `struct file`）记录文件位置和状态标志，可以被多个
//! fd 共享（dup、将来的 fork）；close-on-exec 标志属于 fd 本身。

use std::cell::RefCell;
use std::fs::File;
use std::rc::Rc;

use crate::errno::Errno;
use crate::vfs::store::{DirEntry, Ino};

/// 每个进程最多打开的 fd 数，与 Linux 默认的 `RLIMIT_NOFILE` 软限制相同。
pub const MAX_FDS: usize = 1024;

/// open(2) 的标志位（arm64 的取值）。
pub mod oflags {
    /// 访问模式掩码。
    pub const O_ACCMODE: u32 = 0o3;
    /// 只读。
    pub const O_RDONLY: u32 = 0o0;
    /// 只写。
    pub const O_WRONLY: u32 = 0o1;
    /// 不存在时创建。
    pub const O_CREAT: u32 = 0o100;
    /// 与 `O_CREAT` 合用：已存在时失败。
    pub const O_EXCL: u32 = 0o200;
    /// 截断。
    pub const O_TRUNC: u32 = 0o1000;
    /// 追加写。
    pub const O_APPEND: u32 = 0o2000;
    /// 非阻塞。
    pub const O_NONBLOCK: u32 = 0o4000;
    /// 必须是目录。
    pub const O_DIRECTORY: u32 = 0o40000;
    /// 不跟随最后一个组件的符号链接。
    pub const O_NOFOLLOW: u32 = 0o100000;
    /// 大文件（64 位上恒为真）。
    pub const O_LARGEFILE: u32 = 0o400000;
    /// exec 时关闭。
    pub const O_CLOEXEC: u32 = 0o2000000;
    /// 只取路径。
    pub const O_PATH: u32 = 0o10000000;
    /// fcntl(F_SETFL) 能修改的标志。
    pub const SETFL_MASK: u32 = O_APPEND | O_NONBLOCK;
}

/// 打开文件的种类。
pub enum FileKind {
    /// 宿主 fd，`appine-run` 把宿主的 stdin/stdout/stderr 原样交给 guest。
    Host(i32),
    /// inode 仓库里的普通文件。
    Regular {
        /// inode。
        ino: Ino,
        /// 宿主 object 文件。
        file: File,
    },
    /// 目录。目录项在第一次 getdents 时取一份快照，lseek 到 0 时丢弃。
    Directory {
        /// inode。
        ino: Ino,
        /// 目录项快照（含 `.` 和 `..`）。
        entries: Option<Vec<DirEntry>>,
    },
    /// `O_PATH` 打开的文件：只能作为 `*at` 的起点或 fstat 的对象。
    Path(Ino),
}

/// 一个打开文件。
pub struct OpenFile {
    /// 种类。
    pub kind: FileKind,
    /// 访问模式和状态标志（不含 `O_CLOEXEC`）。
    pub flags: u32,
    /// 文件位置；目录是下一个目录项的下标。
    pub pos: u64,
}

impl OpenFile {
    /// 新建打开文件。
    pub fn new(kind: FileKind, flags: u32) -> FileRef {
        Rc::new(RefCell::new(OpenFile {
            kind,
            flags: flags & !oflags::O_CLOEXEC,
            pos: 0,
        }))
    }

    /// 是否可读。
    pub fn readable(&self) -> bool {
        self.flags & oflags::O_PATH == 0 && self.flags & oflags::O_ACCMODE != oflags::O_WRONLY
    }

    /// 是否可写。
    pub fn writable(&self) -> bool {
        self.flags & oflags::O_PATH == 0 && self.flags & oflags::O_ACCMODE != oflags::O_RDONLY
    }

    /// 对应的 inode（宿主 fd 没有）。
    pub fn ino(&self) -> Option<Ino> {
        match &self.kind {
            FileKind::Host(_) => None,
            FileKind::Regular { ino, .. }
            | FileKind::Directory { ino, .. }
            | FileKind::Path(ino) => Some(*ino),
        }
    }
}

/// 打开文件的共享引用。
pub type FileRef = Rc<RefCell<OpenFile>>;

#[derive(Clone)]
struct Slot {
    file: FileRef,
    cloexec: bool,
}

/// 一个进程的 fd 表。
#[derive(Clone, Default)]
pub struct FdTable {
    slots: Vec<Option<Slot>>,
}

impl FdTable {
    /// 空表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 0/1/2 分别是宿主的 stdin/stdout/stderr。
    pub fn with_host_stdio() -> Self {
        let mut t = Self::new();
        for (fd, flags) in [
            (0, oflags::O_RDONLY),
            (1, oflags::O_WRONLY),
            (2, oflags::O_WRONLY),
        ] {
            t.install(OpenFile::new(FileKind::Host(fd), flags), false, 0)
                .unwrap();
        }
        t
    }

    /// 取出 fd 对应的打开文件。
    pub fn get(&self, fd: i32) -> Result<FileRef, Errno> {
        usize::try_from(fd)
            .ok()
            .and_then(|i| self.slots.get(i))
            .and_then(Option::as_ref)
            .map(|s| s.file.clone())
            .ok_or(Errno::EBADF)
    }

    /// 放到不小于 `min` 的最小空闲 fd 上。
    pub fn install(&mut self, file: FileRef, cloexec: bool, min: usize) -> Result<i32, Errno> {
        if min >= MAX_FDS {
            return Err(Errno::EINVAL);
        }
        let fd = (min..MAX_FDS)
            .find(|&i| self.slots.get(i).is_none_or(Option::is_none))
            .ok_or(Errno::EMFILE)?;
        self.install_at(fd as i32, file, cloexec)?;
        Ok(fd as i32)
    }

    /// 放到指定的 fd 上，原来的文件被关闭（dup2/dup3）。
    pub fn install_at(&mut self, fd: i32, file: FileRef, cloexec: bool) -> Result<(), Errno> {
        let i = usize::try_from(fd)
            .ok()
            .filter(|&i| i < MAX_FDS)
            .ok_or(Errno::EBADF)?;
        if self.slots.len() <= i {
            self.slots.resize(i + 1, None);
        }
        self.slots[i] = Some(Slot { file, cloexec });
        Ok(())
    }

    /// 关闭 fd。
    pub fn close(&mut self, fd: i32) -> Result<(), Errno> {
        let slot = usize::try_from(fd)
            .ok()
            .and_then(|i| self.slots.get_mut(i))
            .ok_or(Errno::EBADF)?;
        slot.take().map(|_| ()).ok_or(Errno::EBADF)
    }

    /// 读取 close-on-exec 标志。
    pub fn cloexec(&self, fd: i32) -> Result<bool, Errno> {
        self.slot(fd).map(|s| s.cloexec)
    }

    /// 设置 close-on-exec 标志。
    pub fn set_cloexec(&mut self, fd: i32, cloexec: bool) -> Result<(), Errno> {
        let i = usize::try_from(fd).map_err(|_| Errno::EBADF)?;
        let slot = self
            .slots
            .get_mut(i)
            .and_then(Option::as_mut)
            .ok_or(Errno::EBADF)?;
        slot.cloexec = cloexec;
        Ok(())
    }

    fn slot(&self, fd: i32) -> Result<&Slot, Errno> {
        usize::try_from(fd)
            .ok()
            .and_then(|i| self.slots.get(i))
            .and_then(Option::as_ref)
            .ok_or(Errno::EBADF)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_lowest_free_fd() {
        let mut t = FdTable::with_host_stdio();
        let f = OpenFile::new(FileKind::Host(1), oflags::O_WRONLY);
        assert_eq!(t.install(f.clone(), false, 0), Ok(3));
        t.close(1).unwrap();
        assert_eq!(t.install(f.clone(), true, 0), Ok(1));
        assert_eq!(t.cloexec(1), Ok(true));
        assert_eq!(t.install(f.clone(), false, 10), Ok(10));
        assert_eq!(t.close(7), Err(Errno::EBADF));
        assert_eq!(t.get(-1).err(), Some(Errno::EBADF));
        // dup 出来的 fd 共享同一个打开文件
        f.borrow_mut().pos = 5;
        assert_eq!(t.get(10).unwrap().borrow().pos, 5);
    }
}
