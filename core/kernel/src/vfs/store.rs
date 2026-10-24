//! inode 仓库（设计文档 §8.1）。
//!
//! SQLite（WAL）保存完整的命名空间：目录项、inode、symlink 目标；普通文件的
//! 数据存成宿主文件 `objects/<ino>`。普通文件的 size 和 mtime 以宿主 object
//! 文件为准，其余元数据（mode、uid/gid、nlink、rdev、ctime）存在 SQLite 里。
//! 硬链接就是多个目录项指向同一个 ino。宿主侧只会出现 `objects/<数字>` 这种
//! 路径，永远不由 guest 输入拼出（§12 不变量 3）。
//!
//! 删除最后一个链接时，普通文件的 object 立即删除（已经打开的宿主文件仍然可用），
//! inode 行以 nlink = 0 留下来，让打开着它的 fd 还能 fstat；下次打开仓库时清理。

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
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
    /// 仓库目录里的 `lock` 文件。每个使用仓库的进程持有它的共享锁，只有独占使用时
    /// 才清理已删除的 inode：别的进程可能还打开着它们。
    lock: File,
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
    CREATE TABLE IF NOT EXISTS xattrs (
        ino   INTEGER NOT NULL,
        name  BLOB NOT NULL,
        value BLOB NOT NULL,
        PRIMARY KEY (ino, name)
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

fn fill_from_object(st: &mut Stat, meta: &fs::Metadata) {
    st.size = meta.len();
    st.mtime_ns = meta.modified().map(to_ns).unwrap_or(st.mtime_ns);
}

/// 删除 object 文件；它已经不在时不算错误。
fn remove_object(path: &Path) -> Result<(), Errno> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io_err(e)),
        _ => Ok(()),
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

    /// 打开已有的仓库。没有别的进程在使用它时，清理上次运行留下的已删除 inode。
    pub fn open(dir: &Path) -> Result<Store, Errno> {
        if !dir.join("meta.db").exists() {
            return Err(Errno::ENOENT);
        }
        let store = Self::connect(dir)?;
        let fd = store.lock.as_raw_fd();
        if appine_hal::flock(fd, true, false).is_ok() {
            store.remove_orphans()?;
        }
        appine_hal::flock(fd, false, true).map_err(Errno)?;
        Ok(store)
    }

    fn remove_orphans(&self) -> Result<(), Errno> {
        let orphans: Vec<(Ino, u32)> = {
            let mut stmt = self
                .db
                .prepare("SELECT ino, mode FROM inodes WHERE nlink = 0")
                .map_err(db_err)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(db_err)?;
            rows.collect::<Result<_, _>>().map_err(db_err)?
        };
        for (ino, m) in orphans {
            if m & mode::S_IFMT == mode::S_IFREG {
                remove_object(&self.object_path(ino))?;
            }
            self.db
                .prepare_cached("DELETE FROM inodes WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![ino]))
                .map_err(db_err)?;
            self.db
                .prepare_cached("DELETE FROM xattrs WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![ino]))
                .map_err(db_err)?;
        }
        Ok(())
    }

    fn connect(dir: &Path) -> Result<Store, Errno> {
        let db = Connection::open(dir.join("meta.db")).map_err(db_err)?;
        db.pragma_update(None, "journal_mode", "WAL")
            .map_err(db_err)?;
        db.pragma_update(None, "synchronous", "NORMAL")
            .map_err(db_err)?;
        // 多个 appine 进程可以同时使用同一个仓库：等别人的写事务结束，而不是立即失败
        db.busy_timeout(Duration::from_secs(10)).map_err(db_err)?;
        // 元数据操作很频繁：所有语句都只编译一次
        db.set_prepared_statement_cache_capacity(64);
        db.execute_batch(SCHEMA).map_err(db_err)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("lock"))
            .map_err(io_err)?;
        appine_hal::flock(lock.as_raw_fd(), false, true).map_err(Errno)?;
        Ok(Store {
            db,
            objects: dir.join("objects"),
            lock,
        })
    }

    /// 在一个事务里执行 `f`；`f` 出错时回滚。一开始就取得写锁（`IMMEDIATE`）：先读后写
    /// 的事务在别的进程提交之后无法升级成写事务，那时 busy_timeout 也帮不上忙。
    /// 已经在事务里时（导入 rootfs 时整个导入是一个事务）直接并入外层事务。
    pub fn transaction<T>(&self, f: impl FnOnce(&Store) -> Result<T, Errno>) -> Result<T, Errno> {
        if !self.db.is_autocommit() {
            return f(self);
        }
        self.db.execute_batch("BEGIN IMMEDIATE").map_err(db_err)?;
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
            .prepare_cached("SELECT ino FROM dentries WHERE parent = ?1 AND name = ?2")
            .and_then(|mut st| st.query_row(params![parent, name], |row| row.get(0)))
            .optional()
            .map_err(db_err)
    }

    /// 目录 `parent` 里指向 `ino` 的一个名字（getcwd 用）。
    pub fn name_of(&self, parent: Ino, ino: Ino) -> Result<Option<Vec<u8>>, Errno> {
        self.db
            .prepare_cached(
                "SELECT name FROM dentries WHERE parent = ?1 AND ino = ?2 ORDER BY name LIMIT 1",
            )
            .and_then(|mut st| st.query_row(params![parent, ino], |row| row.get(0)))
            .optional()
            .map_err(db_err)
    }

    /// 读取元数据。普通文件的 size 和 mtime 取自宿主 object 文件。
    pub fn stat(&self, ino: Ino) -> Result<Stat, Errno> {
        let mut st = self.stat_row(ino)?;
        if st.kind() == mode::S_IFREG {
            let meta = fs::metadata(self.object_path(ino)).map_err(io_err)?;
            fill_from_object(&mut st, &meta);
        }
        Ok(st)
    }

    /// 已打开的普通文件的元数据：size 和 mtime 取自打开的 object，文件被删除之后也能用。
    pub fn stat_open(&self, ino: Ino, file: &File) -> Result<Stat, Errno> {
        let mut st = self.stat_row(ino)?;
        fill_from_object(&mut st, &file.metadata().map_err(io_err)?);
        Ok(st)
    }

    /// 文件类型位（不需要读 object，删除之后仍然打开着的文件也能用）。
    pub fn kind(&self, ino: Ino) -> Result<u32, Errno> {
        Ok(self.stat_row(ino)?.kind())
    }

    /// 数据库里的元数据（普通文件的 size、mtime 还没有填）。
    fn stat_row(&self, ino: Ino) -> Result<Stat, Errno> {
        let row = self
            .db
            .prepare_cached(
                "SELECT mode, nlink, uid, gid, rdev, mtime_ns, ctime_ns, length(target)
                 FROM inodes WHERE ino = ?1",
            )
            .and_then(|mut st| {
                st.query_row(params![ino], |r| {
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
                })
            })
            .optional()
            .map_err(db_err)?
            .ok_or(Errno::ENOENT)?;
        let (mode, nlink, uid, gid, rdev, mtime_ns, ctime_ns, target_len) = row;
        let size = target_len.unwrap_or(0);
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
            .prepare_cached("SELECT target FROM inodes WHERE ino = ?1")
            .and_then(|mut st| st.query_row(params![ino], |r| r.get::<_, Option<Vec<u8>>>(0)))
            .optional()
            .map_err(db_err)?
            .ok_or(Errno::ENOENT)?
            .ok_or(Errno::EINVAL)
    }

    /// 目录的上级目录；根目录的上级是它自己。
    pub fn parent(&self, dir: Ino) -> Result<Ino, Errno> {
        self.db
            .prepare_cached("SELECT parent FROM inodes WHERE ino = ?1")
            .and_then(|mut st| st.query_row(params![dir], |r| r.get(0)))
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
            .prepare_cached(
                "INSERT INTO inodes (mode, uid, gid, nlink, parent, mtime_ns, ctime_ns)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            )
            .and_then(|mut st| st.execute(params![mode, uid, gid, nlink, parent, now]))
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
            .prepare_cached("INSERT INTO dentries (parent, name, ino) VALUES (?1, ?2, ?3)")
            .and_then(|mut st| st.execute(params![parent, name, ino]))
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
        self.transaction(|s| {
            if s.lookup(parent, name)?.is_some() {
                return Err(Errno::EEXIST);
            }
            let ino = s.new_inode(mode::S_IFDIR | (perm & 0o7777), uid, gid, 2, parent)?;
            s.add_dentry(parent, name, ino)?;
            s.db.prepare_cached("UPDATE inodes SET nlink = nlink + 1 WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![parent]))
                .map_err(db_err)?;
            Ok(ino)
        })
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
        self.transaction(|s| {
            if s.lookup(parent, name)?.is_some() {
                return Err(Errno::EEXIST);
            }
            let ino = s.new_inode(mode::S_IFREG | (perm & 0o7777), uid, gid, 1, 0)?;
            s.add_dentry(parent, name, ino)?;
            // 最后建 object：失败时整个事务回滚，不留下没有 object 的 inode
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(s.object_path(ino))
                .map_err(io_err)?;
            Ok((ino, file))
        })
    }

    /// 新建一个没有名字的普通文件（memfd_create）：链接数为 0，object 一开始就删除，
    /// 关闭之后数据随之消失，inode 在下次打开仓库时清理。
    pub fn create_anonymous(&self, perm: u32) -> Result<(Ino, File), Errno> {
        let ino = self.new_inode(mode::S_IFREG | (perm & 0o7777), 0, 0, 0, 0)?;
        let path = self.object_path(ino);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io_err)?;
        remove_object(&path)?;
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
        self.transaction(|s| {
            if s.lookup(parent, name)?.is_some() {
                return Err(Errno::EEXIST);
            }
            let ino = s.new_inode(mode::S_IFLNK | 0o777, uid, gid, 1, 0)?;
            s.db.prepare_cached("UPDATE inodes SET target = ?1 WHERE ino = ?2")
                .and_then(|mut st| st.execute(params![target, ino]))
                .map_err(db_err)?;
            s.add_dentry(parent, name, ino)?;
            Ok(ino)
        })
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
        self.transaction(|s| {
            if s.lookup(parent, name)?.is_some() {
                return Err(Errno::EEXIST);
            }
            let ino = s.new_inode(mode, uid, gid, 1, 0)?;
            s.db.prepare_cached("UPDATE inodes SET rdev = ?1 WHERE ino = ?2")
                .and_then(|mut st| st.execute(params![rdev, ino]))
                .map_err(db_err)?;
            s.add_dentry(parent, name, ino)?;
            Ok(ino)
        })
    }

    /// 给已有的非目录 inode 增加一个硬链接。
    pub fn link(&self, parent: Ino, name: &[u8], ino: Ino) -> Result<(), Errno> {
        self.transaction(|s| {
            if s.stat_row(ino)?.is_dir() {
                return Err(Errno::EPERM);
            }
            s.add_dentry(parent, name, ino)?;
            s.db.prepare_cached(
                "UPDATE inodes SET nlink = nlink + 1, ctime_ns = ?2 WHERE ino = ?1",
            )
            .and_then(|mut st| st.execute(params![ino, now_ns()]))
            .map_err(db_err)?;
            Ok(())
        })
    }

    /// 修改权限位（保留类型位）。
    pub fn set_mode(&self, ino: Ino, perm: u32) -> Result<(), Errno> {
        let n = self
            .db
            .prepare_cached(
                "UPDATE inodes SET mode = (mode & ?2) | ?3, ctime_ns = ?4 WHERE ino = ?1",
            )
            .and_then(|mut st| st.execute(params![ino, mode::S_IFMT, perm & 0o7777, now_ns()]))
            .map_err(db_err)?;
        if n == 0 { Err(Errno::ENOENT) } else { Ok(()) }
    }

    /// 修改属主和属组；`None` 表示不变。普通文件同时清除 setuid 位，组可执行时还清除
    /// setgid 位（Linux 对 root 也这样做）。
    pub fn set_owner(&self, ino: Ino, uid: Option<u32>, gid: Option<u32>) -> Result<(), Errno> {
        let st = self.stat_row(ino)?;
        let mut perm = st.mode & 0o7777;
        if !st.is_dir() {
            perm &= !0o4000;
            if perm & 0o010 != 0 {
                perm &= !0o2000;
            }
        }
        self.set_owner_mode(ino, perm, uid.unwrap_or(st.uid), gid.unwrap_or(st.gid))
    }

    /// 目录是否为空。
    fn is_empty_dir(&self, dir: Ino) -> Result<bool, Errno> {
        let n: i64 = self
            .db
            .prepare_cached("SELECT count(*) FROM dentries WHERE parent = ?1")
            .and_then(|mut st| st.query_row(params![dir], |r| r.get(0)))
            .map_err(db_err)?;
        Ok(n == 0)
    }

    /// 删除一个指向非目录的目录项；最后一个链接消失时普通文件的 object 一起删除。
    fn remove_link(&self, parent: Ino, name: &[u8], ino: Ino) -> Result<(), Errno> {
        self.db
            .prepare_cached("DELETE FROM dentries WHERE parent = ?1 AND name = ?2")
            .and_then(|mut st| st.execute(params![parent, name]))
            .map_err(db_err)?;
        self.db
            .prepare_cached("UPDATE inodes SET nlink = nlink - 1, ctime_ns = ?2 WHERE ino = ?1")
            .and_then(|mut st| st.execute(params![ino, now_ns()]))
            .map_err(db_err)?;
        let st = self.stat_row(ino)?;
        if st.nlink == 0 && st.kind() == mode::S_IFREG {
            remove_object(&self.object_path(ino))?;
        }
        Ok(())
    }

    /// 删除一个空目录的目录项。
    fn remove_dir(&self, parent: Ino, name: &[u8], ino: Ino) -> Result<(), Errno> {
        if !self.is_empty_dir(ino)? {
            return Err(Errno::ENOTEMPTY);
        }
        self.db
            .prepare_cached("DELETE FROM dentries WHERE parent = ?1 AND name = ?2")
            .and_then(|mut st| st.execute(params![parent, name]))
            .map_err(db_err)?;
        self.db
            .prepare_cached("UPDATE inodes SET nlink = 0 WHERE ino = ?1")
            .and_then(|mut st| st.execute(params![ino]))
            .map_err(db_err)?;
        self.db
            .prepare_cached("UPDATE inodes SET nlink = nlink - 1 WHERE ino = ?1")
            .and_then(|mut st| st.execute(params![parent]))
            .map_err(db_err)?;
        Ok(())
    }

    /// unlink(2)：删除一个非目录的目录项。
    pub fn unlink(&self, parent: Ino, name: &[u8]) -> Result<(), Errno> {
        let ino = self.lookup(parent, name)?.ok_or(Errno::ENOENT)?;
        if self.stat_row(ino)?.is_dir() {
            return Err(Errno::EISDIR);
        }
        self.transaction(|s| s.remove_link(parent, name, ino))
    }

    /// rmdir(2)：删除一个空目录。
    pub fn rmdir(&self, parent: Ino, name: &[u8]) -> Result<(), Errno> {
        let ino = self.lookup(parent, name)?.ok_or(Errno::ENOENT)?;
        if !self.stat_row(ino)?.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        if ino == ROOT {
            return Err(Errno::EBUSY);
        }
        self.transaction(|s| s.remove_dir(parent, name, ino))
    }

    /// `ancestor` 是不是 `dir` 本身或者它的上级目录。
    fn is_ancestor(&self, ancestor: Ino, mut dir: Ino) -> Result<bool, Errno> {
        loop {
            if dir == ancestor {
                return Ok(true);
            }
            if dir == ROOT {
                return Ok(false);
            }
            dir = self.parent(dir)?;
        }
    }

    /// 把目录 `dir` 从 `from` 挪到 `to` 下面时更新 `..` 和两边的链接数。
    fn reparent_dir(&self, dir: Ino, from: Ino, to: Ino) -> Result<(), Errno> {
        if from == to {
            return Ok(());
        }
        for (ino, delta) in [(from, -1i64), (to, 1)] {
            self.db
                .prepare_cached("UPDATE inodes SET nlink = nlink + ?2 WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![ino, delta]))
                .map_err(db_err)?;
        }
        self.db
            .prepare_cached("UPDATE inodes SET parent = ?2 WHERE ino = ?1")
            .and_then(|mut st| st.execute(params![dir, to]))
            .map_err(db_err)?;
        Ok(())
    }

    fn set_dentry(&self, parent: Ino, name: &[u8], ino: Ino) -> Result<(), Errno> {
        self.db
            .prepare_cached(
                "INSERT OR REPLACE INTO dentries (parent, name, ino) VALUES (?1, ?2, ?3)",
            )
            .and_then(|mut st| st.execute(params![parent, name, ino]))
            .map_err(db_err)?;
        Ok(())
    }

    /// rename(2)/renameat2(2)：`noreplace` 是 `RENAME_NOREPLACE`，`exchange` 是
    /// `RENAME_EXCHANGE`。整个操作在一个事务里完成。
    pub fn rename(
        &self,
        old_dir: Ino,
        old_name: &[u8],
        new_dir: Ino,
        new_name: &[u8],
        noreplace: bool,
        exchange: bool,
    ) -> Result<(), Errno> {
        self.transaction(|s| {
            let src = s.lookup(old_dir, old_name)?.ok_or(Errno::ENOENT)?;
            let dst = s.lookup(new_dir, new_name)?;
            let src_dir = s.stat_row(src)?.is_dir();
            if exchange {
                let dst = dst.ok_or(Errno::ENOENT)?;
                if src == dst {
                    return Ok(());
                }
                let dst_dir = s.stat_row(dst)?.is_dir();
                if (src_dir && s.is_ancestor(src, new_dir)?)
                    || (dst_dir && s.is_ancestor(dst, old_dir)?)
                {
                    return Err(Errno::EINVAL);
                }
                s.set_dentry(old_dir, old_name, dst)?;
                s.set_dentry(new_dir, new_name, src)?;
                if src_dir {
                    s.reparent_dir(src, old_dir, new_dir)?;
                }
                if dst_dir {
                    s.reparent_dir(dst, new_dir, old_dir)?;
                }
                return Ok(());
            }
            if let Some(d) = dst {
                if noreplace {
                    return Err(Errno::EEXIST);
                }
                // 两个名字指向同一个 inode（硬链接）：什么都不做
                if d == src {
                    return Ok(());
                }
                let dst_dir = s.stat_row(d)?.is_dir();
                match (src_dir, dst_dir) {
                    (true, false) => return Err(Errno::ENOTDIR),
                    (false, true) => return Err(Errno::EISDIR),
                    _ => {}
                }
            }
            if src_dir && s.is_ancestor(src, new_dir)? {
                return Err(Errno::EINVAL);
            }
            if let Some(d) = dst {
                if src_dir {
                    s.remove_dir(new_dir, new_name, d)?;
                } else {
                    s.remove_link(new_dir, new_name, d)?;
                }
            }
            s.db.prepare_cached("DELETE FROM dentries WHERE parent = ?1 AND name = ?2")
                .and_then(|mut st| st.execute(params![old_dir, old_name]))
                .map_err(db_err)?;
            s.set_dentry(new_dir, new_name, src)?;
            if src_dir {
                s.reparent_dir(src, old_dir, new_dir)?;
            }
            s.db.prepare_cached("UPDATE inodes SET ctime_ns = ?2 WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![src, now_ns()]))
                .map_err(db_err)?;
            Ok(())
        })
    }

    /// 截断普通文件。
    pub fn truncate(&self, ino: Ino, len: u64) -> Result<(), Errno> {
        self.open_object(ino, true)?.set_len(len).map_err(io_err)
    }

    /// 仓库所在的宿主文件系统的容量（statfs 报告给 guest）。
    pub fn fs_stats(&self) -> Result<appine_hal::FsStats, Errno> {
        appine_hal::statvfs(&self.objects).map_err(Errno)
    }

    /// 修改权限位、属主和属组（保留类型位）。
    pub fn set_owner_mode(&self, ino: Ino, perm: u32, uid: u32, gid: u32) -> Result<(), Errno> {
        self.db
            .prepare_cached("UPDATE inodes SET mode = (mode & ?2) | ?3, uid = ?4, gid = ?5, ctime_ns = ?6 WHERE ino = ?1")
            .and_then(|mut st| st.execute(params![ino, mode::S_IFMT, perm & 0o7777, uid, gid, now_ns()]))
            .map_err(db_err)?;
        Ok(())
    }

    /// 设置扩展属性。`create`（`XATTR_CREATE`）时已经存在则失败，`replace`
    /// （`XATTR_REPLACE`）时不存在则失败。
    pub fn set_xattr(
        &self,
        ino: Ino,
        name: &[u8],
        value: &[u8],
        create: bool,
        replace: bool,
    ) -> Result<(), Errno> {
        self.transaction(|s| {
            let exists = s.get_xattr(ino, name).is_ok();
            if create && exists {
                return Err(Errno::EEXIST);
            }
            if replace && !exists {
                return Err(Errno::ENODATA);
            }
            s.db.prepare_cached(
                "INSERT OR REPLACE INTO xattrs (ino, name, value) VALUES (?1, ?2, ?3)",
            )
            .and_then(|mut st| st.execute(params![ino, name, value]))
            .map_err(db_err)?;
            s.touch_ctime(ino)
        })
    }

    /// 读取扩展属性；没有时返回 ENODATA。
    pub fn get_xattr(&self, ino: Ino, name: &[u8]) -> Result<Vec<u8>, Errno> {
        self.db
            .prepare_cached("SELECT value FROM xattrs WHERE ino = ?1 AND name = ?2")
            .and_then(|mut st| st.query_row(params![ino, name], |r| r.get(0)))
            .optional()
            .map_err(db_err)?
            .ok_or(Errno::ENODATA)
    }

    /// 所有扩展属性的名字。
    pub fn list_xattr(&self, ino: Ino) -> Result<Vec<Vec<u8>>, Errno> {
        let mut stmt = self
            .db
            .prepare_cached("SELECT name FROM xattrs WHERE ino = ?1 ORDER BY name")
            .map_err(db_err)?;
        let rows = stmt.query_map(params![ino], |r| r.get(0)).map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)
    }

    /// 删除扩展属性；没有时返回 ENODATA。
    pub fn remove_xattr(&self, ino: Ino, name: &[u8]) -> Result<(), Errno> {
        self.transaction(|s| {
            let n =
                s.db.prepare_cached("DELETE FROM xattrs WHERE ino = ?1 AND name = ?2")
                    .and_then(|mut st| st.execute(params![ino, name]))
                    .map_err(db_err)?;
            if n == 0 {
                return Err(Errno::ENODATA);
            }
            s.touch_ctime(ino)
        })
    }

    fn touch_ctime(&self, ino: Ino) -> Result<(), Errno> {
        self.db
            .prepare_cached("UPDATE inodes SET ctime_ns = ?2 WHERE ino = ?1")
            .and_then(|mut st| st.execute(params![ino, now_ns()]))
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
            .prepare_cached("UPDATE inodes SET mtime_ns = ?2 WHERE ino = ?1")
            .and_then(|mut st| st.execute(params![ino, mtime_ns]))
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

    #[test]
    fn unlink_rmdir_and_rename() {
        let (s, dir) = temp_store("rename");
        let a = s.mkdir(ROOT, b"a", 0o755, 0, 0).unwrap();
        let b = s.mkdir(a, b"b", 0o755, 0, 0).unwrap();
        let (f, mut file) = s.create_file(a, b"f", 0o644, 0, 0).unwrap();
        file.write_all(b"data").unwrap();
        s.link(ROOT, b"g", f).unwrap();

        // 目录不能挪进自己的子目录；非目录不能覆盖目录
        assert_eq!(
            s.rename(ROOT, b"a", b, b"x", false, false),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            s.rename(ROOT, b"g", a, b"b", false, false),
            Err(Errno::EISDIR)
        );
        assert_eq!(
            s.rename(ROOT, b"g", a, b"f", true, false),
            Err(Errno::EEXIST)
        );
        // 同一个 inode 的两个名字之间 rename 什么都不做
        s.rename(ROOT, b"g", a, b"f", false, false).unwrap();
        assert_eq!(s.stat(f).unwrap().nlink, 2);

        // 目录换了上级：.. 和两边的链接数都要更新
        s.rename(a, b"b", ROOT, b"b", false, false).unwrap();
        assert_eq!(s.parent(b).unwrap(), ROOT);
        assert_eq!(
            (s.stat(a).unwrap().nlink, s.stat(ROOT).unwrap().nlink),
            (2, 4)
        );
        s.rename(ROOT, b"b", a, b"f", false, true).unwrap();
        assert_eq!(s.lookup(a, b"f").unwrap(), Some(b));
        assert_eq!(s.lookup(ROOT, b"b").unwrap(), Some(f));

        // 删除最后一个链接：打开的文件仍然可读、可 fstat，重新打开仓库后才真正清理
        assert_eq!(s.rmdir(ROOT, b"a"), Err(Errno::ENOTEMPTY));
        s.unlink(ROOT, b"g").unwrap();
        s.unlink(ROOT, b"b").unwrap();
        assert_eq!(s.lookup(ROOT, b"b").unwrap(), None);
        assert_eq!(s.stat_open(f, &file).unwrap().size, 4);
        s.rmdir(a, b"f").unwrap();
        s.rmdir(ROOT, b"a").unwrap();
        assert_eq!(s.stat(ROOT).unwrap().nlink, 2);
        assert_eq!(s.unlink(ROOT, b"a"), Err(Errno::ENOENT));
        drop(s);
        let s = Store::open(&dir).unwrap();
        assert_eq!(s.stat(f), Err(Errno::ENOENT));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn xattrs_and_anonymous_files() {
        let (s, dir) = temp_store("xattr");
        let (f, _) = s.create_file(ROOT, b"f", 0o644, 0, 0).unwrap();
        assert_eq!(s.get_xattr(f, b"user.a"), Err(Errno::ENODATA));
        s.set_xattr(f, b"user.b", b"2", false, false).unwrap();
        s.set_xattr(f, b"user.a", b"1", true, false).unwrap();
        assert_eq!(
            s.set_xattr(f, b"user.a", b"x", true, false),
            Err(Errno::EEXIST)
        );
        assert_eq!(
            s.set_xattr(f, b"user.c", b"x", false, true),
            Err(Errno::ENODATA)
        );
        s.set_xattr(f, b"user.a", b"one", false, true).unwrap();
        assert_eq!(s.get_xattr(f, b"user.a").unwrap(), b"one");
        assert_eq!(
            s.list_xattr(f).unwrap(),
            vec![b"user.a".to_vec(), b"user.b".to_vec()]
        );
        s.remove_xattr(f, b"user.b").unwrap();
        assert_eq!(s.remove_xattr(f, b"user.b"), Err(Errno::ENODATA));

        let (anon, mut file) = s.create_anonymous(0o777).unwrap();
        file.write_all(b"data").unwrap();
        let st = s.stat_open(anon, &file).unwrap();
        assert_eq!((st.nlink, st.size), (0, 4));
        s.set_xattr(anon, b"user.x", b"1", false, false).unwrap();
        // 别的进程还在使用仓库时不清理：它可能还打开着这个 inode
        let other = Store::open(&dir).unwrap();
        assert!(other.stat_row(anon).is_ok());
        drop(other);
        // 独占打开仓库时清理掉没有名字的 inode 和它的扩展属性
        drop((file, s));
        let s = Store::open(&dir).unwrap();
        assert!(s.stat_row(anon).is_err());
        assert_eq!(s.list_xattr(anon).unwrap(), Vec::<Vec<u8>>::new());
        assert_eq!(s.get_xattr(f, b"user.a").unwrap(), b"one");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_connections_wait_for_each_other() {
        // 多个 appine 进程同时使用一个仓库：写操作等别人的事务结束，不会失败
        let (s, dir) = temp_store("concurrent");
        let dirs: Vec<Ino> = (0..4)
            .map(|i| {
                s.mkdir(ROOT, format!("d{i}").as_bytes(), 0o755, 0, 0)
                    .unwrap()
            })
            .collect();
        drop(s);
        let workers: Vec<_> = dirs
            .into_iter()
            .map(|d| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    let s = Store::open(&dir).unwrap();
                    for i in 0..100 {
                        let name = format!("f{i}");
                        s.create_file(d, name.as_bytes(), 0o644, 0, 0).unwrap();
                        s.rename(d, name.as_bytes(), d, b"g", false, false).unwrap();
                        s.unlink(d, b"g").unwrap();
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
