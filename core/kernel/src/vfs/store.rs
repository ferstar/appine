//! inode 仓库（设计文档 §8.1）。
//!
//! SQLite（WAL）保存完整的命名空间：目录项、inode、symlink 目标；普通文件的
//! 数据存成宿主文件 `objects/<ino>`。普通文件的 size 和 mtime 以宿主 object
//! 文件为准，其余元数据（mode、uid/gid、nlink、rdev、ctime）存在 SQLite 里。
//! 硬链接就是多个目录项指向同一个 ino。宿主侧只会出现 `objects/<数字>` 这种
//! 路径，永远不由 guest 输入拼出（§12 不变量 3）。

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::errno::Errno;

/// inode 编号。
pub type Ino = u64;

/// 根目录的 inode。
pub const ROOT: Ino = 1;

/// 文件类型位（`st_mode` 的高位）。
pub mod mode {
    /// 类型位掩码。
    pub const S_IFMT: u32 = 0o170000;
    /// 目录。
    pub const S_IFDIR: u32 = 0o040000;
    /// 普通文件。
    pub const S_IFREG: u32 = 0o100000;
    /// 符号链接。
    pub const S_IFLNK: u32 = 0o120000;
    /// 字符设备。
    pub const S_IFCHR: u32 = 0o020000;
    /// 块设备。
    pub const S_IFBLK: u32 = 0o060000;
    /// FIFO。
    pub const S_IFIFO: u32 = 0o010000;
    /// socket。
    pub const S_IFSOCK: u32 = 0o140000;
}

/// 一个 inode 的元数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    /// inode 编号。
    pub ino: Ino,
    /// 类型位与权限位。
    pub mode: u32,
    /// 硬链接数。
    pub nlink: u32,
    /// 属主。
    pub uid: u32,
    /// 属组。
    pub gid: u32,
    /// 设备号（设备节点）。
    pub rdev: u64,
    /// 字节数。
    pub size: u64,
    /// 修改时间（纳秒）。
    pub mtime_ns: i64,
    /// 状态改变时间（纳秒）。
    pub ctime_ns: i64,
}

impl Stat {
    /// 文件类型位。
    pub fn kind(&self) -> u32 {
        self.mode & mode::S_IFMT
    }

    /// 是不是目录。
    pub fn is_dir(&self) -> bool {
        self.kind() == mode::S_IFDIR
    }

    /// 是不是符号链接。
    pub fn is_symlink(&self) -> bool {
        self.kind() == mode::S_IFLNK
    }
}

/// 目录里的一项（不含 `.` 和 `..`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// 名字。
    pub name: Vec<u8>,
    /// inode 编号。
    pub ino: Ino,
    /// 类型位与权限位。
    pub mode: u32,
}

/// 一个 inode 仓库。
pub struct Store {
    db: Connection,
    objects: PathBuf,
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS inodes (
        ino      INTEGER PRIMARY KEY,
        mode     INTEGER NOT NULL,
        uid      INTEGER NOT NULL,
        gid      INTEGER NOT NULL,
        nlink    INTEGER NOT NULL,
        rdev     INTEGER NOT NULL DEFAULT 0,
        parent   INTEGER NOT NULL DEFAULT 0, -- 目录的上级目录（..）
        mtime_ns INTEGER NOT NULL DEFAULT 0, -- 没有 object 的 inode 才用这一列
        ctime_ns INTEGER NOT NULL DEFAULT 0,
        target   BLOB                        -- symlink 的目标
    );
    CREATE TABLE IF NOT EXISTS dentries (
        parent INTEGER NOT NULL,
        name   BLOB NOT NULL,
        ino    INTEGER NOT NULL,
        PRIMARY KEY (parent, name)
    ) WITHOUT ROWID;
";

fn db_err(e: rusqlite::Error) -> Errno {
    crate::debug::log(format_args!("inode store: {e}"));
    Errno::EIO
}

fn io_err(e: std::io::Error) -> Errno {
    crate::debug::log(format_args!("inode store: {e}"));
    e.raw_os_error().map(Errno).unwrap_or(Errno::EIO)
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn to_ns(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(e) => -(e.duration().as_nanos() as i64),
    }
}

impl Store {
    /// 在空目录（或不存在的目录）`dir` 里创建一个新仓库，只含根目录。
    pub fn create(dir: &Path) -> Result<Store, Errno> {
        fs::create_dir_all(dir.join("objects")).map_err(io_err)?;
        if dir.join("meta.db").exists() {
            return Err(Errno::EEXIST);
        }
        let store = Self::connect(dir)?;
        let now = now_ns();
        store
            .db
            .execute(
                "INSERT INTO inodes (ino, mode, uid, gid, nlink, parent, mtime_ns, ctime_ns)
                 VALUES (?1, ?2, 0, 0, 2, ?1, ?3, ?3)",
                params![ROOT, mode::S_IFDIR | 0o755, now],
            )
            .map_err(db_err)?;
        Ok(store)
    }

    /// 打开已有的仓库。
    pub fn open(dir: &Path) -> Result<Store, Errno> {
        if !dir.join("meta.db").exists() {
            return Err(Errno::ENOENT);
        }
        Self::connect(dir)
    }

    fn connect(dir: &Path) -> Result<Store, Errno> {
        let db = Connection::open(dir.join("meta.db")).map_err(db_err)?;
        db.pragma_update(None, "journal_mode", "WAL")
            .map_err(db_err)?;
        db.pragma_update(None, "synchronous", "NORMAL")
            .map_err(db_err)?;
        db.execute_batch(SCHEMA).map_err(db_err)?;
        Ok(Store {
            db,
            objects: dir.join("objects"),
        })
    }

    /// 在一个事务里执行 `f`；`f` 出错时回滚。
    pub fn transaction<T>(&self, f: impl FnOnce(&Store) -> Result<T, Errno>) -> Result<T, Errno> {
        self.db.execute_batch("BEGIN").map_err(db_err)?;
        match f(self) {
            Ok(v) => {
                self.db.execute_batch("COMMIT").map_err(db_err)?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.db.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn object_path(&self, ino: Ino) -> PathBuf {
        self.objects.join(ino.to_string())
    }

    /// 在目录 `parent` 里查找 `name`。
    pub fn lookup(&self, parent: Ino, name: &[u8]) -> Result<Option<Ino>, Errno> {
        self.db
            .query_row(
                "SELECT ino FROM dentries WHERE parent = ?1 AND name = ?2",
                params![parent, name],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)
    }

    /// 目录 `parent` 里指向 `ino` 的一个名字（getcwd 用）。
    pub fn name_of(&self, parent: Ino, ino: Ino) -> Result<Option<Vec<u8>>, Errno> {
        self.db
            .query_row(
                "SELECT name FROM dentries WHERE parent = ?1 AND ino = ?2 ORDER BY name LIMIT 1",
                params![parent, ino],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)
    }

    /// 读取元数据。普通文件的 size 和 mtime 取自宿主 object 文件。
    pub fn stat(&self, ino: Ino) -> Result<Stat, Errno> {
        let row = self
            .db
            .query_row(
                "SELECT mode, nlink, uid, gid, rdev, mtime_ns, ctime_ns, length(target)
                 FROM inodes WHERE ino = ?1",
                params![ino],
                |r| {
                    Ok((
                        r.get::<_, u32>(0)?,
                        r.get::<_, u32>(1)?,
                        r.get::<_, u32>(2)?,
                        r.get::<_, u32>(3)?,
                        r.get::<_, u64>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, Option<u64>>(7)?,
                    ))
                },
            )
            .optional()
            .map_err(db_err)?
            .ok_or(Errno::ENOENT)?;
        let (mode, nlink, uid, gid, rdev, mut mtime_ns, ctime_ns, target_len) = row;
        let mut size = target_len.unwrap_or(0);
        if mode & mode::S_IFMT == mode::S_IFREG {
            let meta = fs::metadata(self.object_path(ino)).map_err(io_err)?;
            size = meta.len();
            mtime_ns = meta.modified().map(to_ns).unwrap_or(mtime_ns);
        }
        Ok(Stat {
            ino,
            mode,
            nlink,
            uid,
            gid,
            rdev,
            size,
            mtime_ns,
            ctime_ns,
        })
    }

    /// 读取符号链接的目标。
    pub fn readlink(&self, ino: Ino) -> Result<Vec<u8>, Errno> {
        self.db
            .query_row(
                "SELECT target FROM inodes WHERE ino = ?1",
                params![ino],
                |r| r.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()
            .map_err(db_err)?
            .ok_or(Errno::ENOENT)?
            .ok_or(Errno::EINVAL)
    }

    /// 目录的上级目录；根目录的上级是它自己。
    pub fn parent(&self, dir: Ino) -> Result<Ino, Errno> {
        self.db
            .query_row(
                "SELECT parent FROM inodes WHERE ino = ?1",
                params![dir],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?
            .ok_or(Errno::ENOENT)
    }

    /// 列出目录内容，按名字排序，不含 `.` 和 `..`。
    pub fn readdir(&self, dir: Ino) -> Result<Vec<DirEntry>, Errno> {
        let mut stmt = self
            .db
            .prepare_cached(
                "SELECT d.name, d.ino, i.mode FROM dentries d JOIN inodes i ON i.ino = d.ino
                 WHERE d.parent = ?1 ORDER BY d.name",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![dir], |r| {
                Ok(DirEntry {
                    name: r.get(0)?,
                    ino: r.get(1)?,
                    mode: r.get(2)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)
    }

    /// 打开普通文件的 object。
    pub fn open_object(&self, ino: Ino, write: bool) -> Result<File, Errno> {
        OpenOptions::new()
            .read(true)
            .write(write)
            .open(self.object_path(ino))
            .map_err(io_err)
    }

    fn new_inode(
        &self,
        mode: u32,
        uid: u32,
        gid: u32,
        nlink: u32,
        parent: Ino,
    ) -> Result<Ino, Errno> {
        let now = now_ns();
        self.db
            .execute(
                "INSERT INTO inodes (mode, uid, gid, nlink, parent, mtime_ns, ctime_ns)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                params![mode, uid, gid, nlink, parent, now],
            )
            .map_err(db_err)?;
        Ok(self.db.last_insert_rowid() as Ino)
    }

    fn add_dentry(&self, parent: Ino, name: &[u8], ino: Ino) -> Result<(), Errno> {
        if self.lookup(parent, name)?.is_some() {
            return Err(Errno::EEXIST);
        }
        if !self.stat(parent)?.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        self.db
            .execute(
                "INSERT INTO dentries (parent, name, ino) VALUES (?1, ?2, ?3)",
                params![parent, name, ino],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// 新建目录。
    pub fn mkdir(
        &self,
        parent: Ino,
        name: &[u8],
        perm: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Ino, Errno> {
        if self.lookup(parent, name)?.is_some() {
            return Err(Errno::EEXIST);
        }
        let ino = self.new_inode(mode::S_IFDIR | (perm & 0o7777), uid, gid, 2, parent)?;
        self.add_dentry(parent, name, ino)?;
        self.db
            .execute(
                "UPDATE inodes SET nlink = nlink + 1 WHERE ino = ?1",
                params![parent],
            )
            .map_err(db_err)?;
        Ok(ino)
    }

    /// 新建空的普通文件，返回可读写的 object。
    pub fn create_file(
        &self,
        parent: Ino,
        name: &[u8],
        perm: u32,
        uid: u32,
        gid: u32,
    ) -> Result<(Ino, File), Errno> {
        if self.lookup(parent, name)?.is_some() {
            return Err(Errno::EEXIST);
        }
        let ino = self.new_inode(mode::S_IFREG | (perm & 0o7777), uid, gid, 1, 0)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(self.object_path(ino))
            .map_err(io_err)?;
        self.add_dentry(parent, name, ino)?;
        Ok((ino, file))
    }

    /// 新建符号链接。
    pub fn symlink(
        &self,
        parent: Ino,
        name: &[u8],
        target: &[u8],
        uid: u32,
        gid: u32,
    ) -> Result<Ino, Errno> {
        if self.lookup(parent, name)?.is_some() {
            return Err(Errno::EEXIST);
        }
        let ino = self.new_inode(mode::S_IFLNK | 0o777, uid, gid, 1, 0)?;
        self.db
            .execute(
                "UPDATE inodes SET target = ?1 WHERE ino = ?2",
                params![target, ino],
            )
            .map_err(db_err)?;
        self.add_dentry(parent, name, ino)?;
        Ok(ino)
    }

    /// 新建设备节点、FIFO 或 socket 文件；它们只存在于数据库里。
    pub fn mknod(
        &self,
        parent: Ino,
        name: &[u8],
        mode: u32,
        rdev: u64,
        uid: u32,
        gid: u32,
    ) -> Result<Ino, Errno> {
        if self.lookup(parent, name)?.is_some() {
            return Err(Errno::EEXIST);
        }
        let ino = self.new_inode(mode, uid, gid, 1, 0)?;
        self.db
            .execute(
                "UPDATE inodes SET rdev = ?1 WHERE ino = ?2",
                params![rdev, ino],
            )
            .map_err(db_err)?;
        self.add_dentry(parent, name, ino)?;
        Ok(ino)
    }

    /// 给已有的非目录 inode 增加一个硬链接。
    pub fn link(&self, parent: Ino, name: &[u8], ino: Ino) -> Result<(), Errno> {
        if self.stat(ino)?.is_dir() {
            return Err(Errno::EPERM);
        }
        self.add_dentry(parent, name, ino)?;
        self.db
            .execute(
                "UPDATE inodes SET nlink = nlink + 1, ctime_ns = ?2 WHERE ino = ?1",
                params![ino, now_ns()],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// 修改权限位、属主和属组（保留类型位）。
    pub fn set_owner_mode(&self, ino: Ino, perm: u32, uid: u32, gid: u32) -> Result<(), Errno> {
        self.db
            .execute(
                "UPDATE inodes SET mode = (mode & ?2) | ?3, uid = ?4, gid = ?5, ctime_ns = ?6 WHERE ino = ?1",
                params![ino, mode::S_IFMT, perm & 0o7777, uid, gid, now_ns()],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// 设置修改时间：普通文件改宿主 object 的 mtime，其余改数据库。
    pub fn set_mtime(&self, ino: Ino, mtime_ns: i64) -> Result<(), Errno> {
        if self.stat(ino)?.kind() == mode::S_IFREG {
            let t = if mtime_ns >= 0 {
                UNIX_EPOCH + Duration::from_nanos(mtime_ns as u64)
            } else {
                UNIX_EPOCH
            };
            return self.open_object(ino, true)?.set_modified(t).map_err(io_err);
        }
        self.db
            .execute(
                "UPDATE inodes SET mtime_ns = ?2 WHERE ino = ?1",
                params![ino, mtime_ns],
            )
            .map_err(db_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_store(name: &str) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("appine-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        (Store::create(&dir).unwrap(), dir)
    }

    #[test]
    fn namespace_round_trip() {
        let (s, dir) = temp_store("ns");
        let bin = s.mkdir(ROOT, b"bin", 0o755, 0, 0).unwrap();
        let (busybox, mut f) = s.create_file(bin, b"busybox", 0o4755, 0, 0).unwrap();
        f.write_all(b"\x7fELF").unwrap();
        let ls = s.symlink(bin, b"ls", b"/bin/busybox", 0, 0).unwrap();
        s.link(bin, b"bb", busybox).unwrap();

        assert_eq!(s.lookup(ROOT, b"bin").unwrap(), Some(bin));
        assert_eq!(s.lookup(bin, b"missing").unwrap(), None);
        assert_eq!(s.parent(bin).unwrap(), ROOT);
        let st = s.stat(busybox).unwrap();
        assert_eq!((st.mode, st.size, st.nlink), (mode::S_IFREG | 0o4755, 4, 2));
        assert!(s.stat(ls).unwrap().is_symlink());
        assert_eq!(s.readlink(ls).unwrap(), b"/bin/busybox");
        assert_eq!(s.stat(ROOT).unwrap().nlink, 3);
        let names: Vec<Vec<u8>> = s
            .readdir(bin)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(
            names,
            vec![b"bb".to_vec(), b"busybox".to_vec(), b"ls".to_vec()]
        );
        assert_eq!(s.mkdir(ROOT, b"bin", 0o755, 0, 0), Err(Errno::EEXIST));

        drop(s);
        let s = Store::open(&dir).unwrap();
        assert_eq!(s.lookup(bin, b"busybox").unwrap(), Some(busybox));
        let _ = fs::remove_dir_all(&dir);
    }
}
