//! inode 仓库（设计文档 §8.1）。
//!
//! SQLite（WAL）保存完整的命名空间：目录项、inode、symlink 目标；普通文件的
//! 数据存成宿主文件 `objects/<ino>`。普通文件的 size 和 mtime 以宿主 object
//! 文件为准，其余元数据（mode、uid/gid、nlink、rdev、ctime）存在 SQLite 里。
//! 硬链接就是多个目录项指向同一个 ino。宿主侧只会出现 `objects/<数字>` 这种
//! 路径，永远不由 guest 输入拼出（§12 不变量 3）。
//!
//! 删除最后一个链接时，普通文件的 object 在这次修改提交之后删除（已经打开的宿主文件
//! 仍然可用），inode 行以 nlink = 0 留下来，让打开着它的 fd 还能 fstat；下次打开仓库时
//! 清理。
//!
//! 写事务推迟提交（[`COMMIT_DELAY`]）：每次提交都要把改了的页写进 WAL，比修改本身贵十
//! 倍，所以几毫秒里的元数据操作一起提交，每个操作是事务里的一个保存点，出错时只回滚
//! 它自己。App 被杀掉时最多丢失最后几毫秒的修改，仓库仍然是某个时刻的完整状态（Linux
//! 断电时丢失的更多）。别的 appine 进程也在用仓库时每个操作立即提交。
//!
//! 路径解析和 stat 走内存里的元数据缓存（目录项，包括不存在的名字；inode 行；符号
//! 链接目标），不逐级查询数据库。自己的写入只去掉受影响的条目。多个 appine 进程可以
//! 共用一个仓库，所以 `lock` 文件的前 8 字节是所有进程共享映射的计数器：每次提交之后
//! 加一；缓存发现它不是因为自己的提交而变了，就整体作废。
//!
//! 缓存也可以不持 BKL 读（[`Meta`]）：并行的 lstat（`git status`、构建工具）不必排队。

use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, RwLock, RwLockReadGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::errno::Errno;

/// inode 编号。
pub type Ino = u64;

/// 根目录的 inode。
pub const ROOT: Ino = 1;

/// 写事务最多推迟这么久提交（模块文档）。
pub const COMMIT_DELAY: Duration = Duration::from_millis(5);

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
    /// setuid 位。
    pub const S_ISUID: u32 = 0o4000;
    /// setgid 位。
    pub const S_ISGID: u32 = 0o2000;
    /// 粘滞位。
    pub const S_ISVTX: u32 = 0o1000;
}

/// 一个 inode 的元数据。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
    /// 访问时间（纳秒）。只有普通文件单独记录（宿主 object 的 atime），其余等于 mtime。
    pub atime_ns: i64,
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
    /// 元数据缓存、提交计数器和 `objects` 目录。
    meta: Arc<Meta>,
    /// 正在一个操作里（[`Store::transaction`]）：查到的可能是它回滚时会撤销的内容，不填
    /// 进缓存。缓存照样读：这次操作改过的条目改的时候就去掉了（[`Store::touch_name`]），
    /// 留下的都与数据库一致。
    writing: Cell<bool>,
    /// [`Store::snapshot`] 里：还没开始读事务时是 Some(None)，开始时的提交计数是
    /// Some(Some(n))。
    snapshot_at: Cell<Option<Option<u64>>>,
    /// 推迟提交的写事务开始的时刻；None 时没有打开的写事务。
    pending: Cell<Option<Instant>>,
    /// 最后一个链接已经删除、等写事务提交之后再删除的 object。
    doomed: RefCell<Vec<Ino>>,
    /// 开始推迟提交时调用：叫醒到点提交的线程（[`Store::commit_due`]）。
    waker: RefCell<Option<Box<dyn Fn()>>>,
    /// 删除 object 的后台线程（[`reap`]）。
    reaper: Option<JoinHandle<()>>,
    /// 这个操作成功之后放进缓存的内容（[`Fill`]）。
    fills: RefCell<Vec<Fill>>,
}

/// 操作里新建的目录项和 inode 行：内容在操作里就知道了，操作成功之后直接放进缓存，新建
/// 之后马上按路径 chmod、chown、stat 时不必再查数据库。之后同一个操作又改了它们的，
/// 改的时候作废（[`Store::touch_name`]）。
enum Fill {
    Name(Ino, Vec<u8>, Ino),
    Row(Stat),
}

impl Fill {
    /// 每个操作最多记这么多：导入 rootfs 整个是一个操作，作废时逐个比较会变成平方级。
    const MAX: usize = 64;
}

/// 元数据缓存，以及不持 BKL 读它（`syscall::fast`）要用的东西。
///
/// 只有 [`Store`] 的方法修改缓存，修改时持有写锁；它们读时不加锁，因为别的线程只会读：
/// Store 不是 Sync，同一时刻只有一个线程（持有 BKL 的那个）调用它的方法。不持 BKL 的
/// 线程只经由 [`Meta::reader`] 持读锁读。
pub(crate) struct Meta {
    lock: RwLock<()>,
    cache: UnsafeCell<Cache>,
    /// `lock` 文件开头的提交计数器（模块文档）。
    commits: Commits,
    /// `objects` 目录（宿主 `O_PATH`），按 inode 编号 stat object 不必解析整个路径。
    objects_dir: OwnedFd,
    /// 等着后台线程删除的 object（[`reap`]）。
    reaper: Reaper,
}

/// 提交之后要删除的 object 交给后台线程（[`reap`]）：宿主的 unlink 每个几微秒，还要拿
/// `objects` 目录的锁，`rm -rf` 时占了 guest 线程一成多的时间，而且是持着 BKL 做的。
/// 删除之前 App 被杀掉也没关系：inode 行以 nlink = 0 留着，开机时清理。
#[derive(Default)]
struct Reaper {
    state: Mutex<Reaping>,
    /// 有活干了，或者该退出了。
    work: Condvar,
    /// 队列空了、手上也没有在删的（测试等它）。
    idle: Condvar,
}

#[derive(Default)]
struct Reaping {
    queue: Vec<Ino>,
    busy: bool,
    quit: bool,
    /// 一共交给它多少个（测试用）。
    #[cfg(test)]
    handed: usize,
}

/// 删除 object 的后台线程，直到 Store 关闭；关闭之前删完队列里的。
fn reap(meta: Arc<Meta>) {
    let r = &meta.reaper;
    let mut st = r.state.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        if !st.queue.is_empty() {
            let inos = std::mem::take(&mut st.queue);
            st.busy = true;
            drop(st);
            for ino in inos {
                let _ = remove_object(&meta.objects_dir, ino);
            }
            st = r.state.lock().unwrap_or_else(PoisonError::into_inner);
            st.busy = false;
            continue;
        }
        r.idle.notify_all();
        if st.quit {
            return;
        }
        st = r.work.wait(st).unwrap_or_else(PoisonError::into_inner);
    }
}

/// 删除 `objects` 目录里的 object；它已经不在时不算错误。
fn remove_object(objects_dir: &OwnedFd, ino: Ino) -> Result<(), Errno> {
    let mut name = [0u8; 21];
    match appine_hal::unlink_at(objects_dir.as_raw_fd(), object_name(ino, &mut name)) {
        Err(e) if e != libc::ENOENT => Err(object_err(ino, e)),
        _ => Ok(()),
    }
}

// SAFETY: 缓存按上面的约定同步；提交计数器是共享映射里的原子变量，映射在 Meta 存续
// 期间一直有效
unsafe impl Send for Meta {}
unsafe impl Sync for Meta {}

impl Meta {
    /// 不持 BKL 读缓存。缓存是最新的（提交计数器没变）时返回读者，持有读锁直到它被丢掉。
    pub(crate) fn reader(&self) -> Option<Reader<'_>> {
        let guard = self.lock.read().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: 持有读锁，修改缓存的线程要等它释放
        let cache = unsafe { &*self.cache.get() };
        (cache.generation == self.commits.get()).then_some(Reader {
            _guard: guard,
            cache,
            objects_dir: &self.objects_dir,
        })
    }
}

/// 不持 BKL 读缓存的读者（[`Meta::reader`]）。查询没有命中时返回 None，调用方改为持有
/// BKL 正常查询（会填进缓存）。
pub(crate) struct Reader<'a> {
    _guard: RwLockReadGuard<'a, ()>,
    cache: &'a Cache,
    objects_dir: &'a OwnedFd,
}

impl Reader<'_> {
    /// [`Store::lookup`]。
    pub fn lookup(&self, parent: Ino, name: &[u8]) -> Option<Option<Ino>> {
        self.cache.dentries.get(&parent)?.get(name).copied()
    }

    /// [`Store::kind`]。
    pub fn kind(&self, ino: Ino) -> Option<u32> {
        Some(self.cache.rows.get(&ino)?.kind())
    }

    /// [`Store::readlink`]：不是符号链接时返回 None。
    pub fn readlink(&self, ino: Ino) -> Option<&[u8]> {
        self.cache.targets.get(&ino).map(Vec::as_slice)
    }

    /// [`Store::parent`]。
    pub fn parent(&self, dir: Ino) -> Option<Ino> {
        self.cache.parents.get(&dir).copied()
    }

    /// [`Store::stat`]。持有读锁时 stat object：inode 编号被删除后重新使用的话，结果
    /// 不会一半来自旧文件、一半来自新文件。宿主上出错时返回 None。
    pub fn stat(&self, ino: Ino) -> Option<Stat> {
        let mut st = *self.cache.rows.get(&ino)?;
        if st.kind() == mode::S_IFREG {
            fill_from_host(&mut st, self.objects_dir, ino).ok()?;
        }
        Some(st)
    }
}

/// 普通文件的 size 和时间取自宿主 object 文件。
fn fill_from_host(st: &mut Stat, objects_dir: &OwnedFd, ino: Ino) -> Result<(), Errno> {
    let mut name = [0u8; 21];
    let o =
        appine_hal::stat_at(objects_dir.as_raw_fd(), object_name(ino, &mut name)).map_err(Errno)?;
    st.size = o.st_size as u64;
    st.mtime_ns = o.st_mtime * 1_000_000_000 + o.st_mtime_nsec;
    st.atime_ns = o.st_atime * 1_000_000_000 + o.st_atime_nsec;
    Ok(())
}

/// object 的文件名（inode 编号的十进制表示），不分配内存。
fn object_name(mut ino: Ino, buf: &mut [u8; 21]) -> &std::ffi::CStr {
    let mut i = buf.len() - 1;
    buf[i] = 0;
    loop {
        i -= 1;
        buf[i] = b'0' + (ino % 10) as u8;
        ino /= 10;
        if ino == 0 {
            return std::ffi::CStr::from_bytes_with_nul(&buf[i..]).unwrap();
        }
    }
}

/// 缓存的哈希：键是 inode 号和文件名，不必抵御哈希碰撞攻击，用 FxHash（rustc 的做法）。
/// 默认的 SipHash 在 `ls`、`git status` 的 stat 路径上占 4–5%。
#[derive(Default, Clone, Copy)]
struct FxHasher {
    hash: u64,
}

impl FxHasher {
    fn add(&mut self, word: u64) {
        const SEED: u64 = 0x517c_c1b7_2722_0a95;
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl std::hash::Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut word = [0u8; 8];
            word[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(word));
        }
    }

    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    fn finish(&self) -> u64 {
        self.hash
    }
}

type Map<K, V> = HashMap<K, V, std::hash::BuildHasherDefault<FxHasher>>;

/// 元数据缓存。条目对应提交计数为 `generation` 时数据库的内容。
#[derive(Default)]
struct Cache {
    generation: u64,
    entries: usize,
    /// 目录 → 名字 → inode（None 表示不存在）。
    dentries: Map<Ino, Map<Vec<u8>, Option<Ino>>>,
    rows: Map<Ino, Stat>,
    targets: Map<Ino, Vec<u8>>,
    parents: Map<Ino, Ino>,
    /// 目录 → inode → 目录里指向它的一个名字（[`Store::name_of`]，None 表示没有）。
    names: Map<Ino, Map<Ino, Option<Vec<u8>>>>,
    /// 目录的完整列表（readdir）。
    listings: Map<Ino, Vec<DirEntry>>,
}

impl Cache {
    /// 条目数的上限：超过就整体清空。
    const MAX_ENTRIES: usize = 1 << 17;

    fn clear(&mut self, generation: u64) {
        *self = Cache {
            generation,
            ..Cache::default()
        };
    }
}

/// `lock` 文件开头所有进程共享映射的几个字：提交次数、正在使用仓库的进程数、还没提交的
/// 写事务新分配的 inode 编号范围。
struct Commits {
    map: appine_hal::FileMapping,
}

impl Commits {
    fn open(lock: &File) -> Result<Commits, Errno> {
        if lock.metadata().map_err(io_err)?.len() < 32 {
            lock.set_len(32).map_err(io_err)?;
        }
        let map = appine_hal::map_file(lock.as_raw_fd(), 0, 32, true, true).map_err(Errno)?;
        Ok(Commits { map })
    }

    fn word(&self, i: usize) -> &AtomicU64 {
        // SAFETY: 映射按页对齐、至少 32 字节，存续期间一直有效
        unsafe { AtomicU64::from_ptr(self.map.ptr.as_ptr().cast::<u64>().add(i)) }
    }

    fn get(&self) -> u64 {
        self.word(0).load(Ordering::SeqCst)
    }

    /// 加一，返回原来的值。
    fn bump(&self) -> u64 {
        self.word(0).fetch_add(1, Ordering::SeqCst)
    }

    /// 正在使用仓库的进程数。异常退出的进程没有减掉自己，所以可能偏大（只会让别的进程
    /// 不推迟提交），独占打开仓库（相当于开机）时清零。
    fn users(&self) -> &AtomicU64 {
        self.word(1)
    }

    /// 还没提交的写事务新分配的 inode 编号的最小值和最大值（0 表示没有）。进程在提交
    /// 之前被杀掉时，这些 inode 随事务一起丢失，它们的 object 却已经建好，下次开机按这个
    /// 范围删掉；不删的话编号再分配给目录之类没有 object 的 inode 时就永远留下了。
    fn fresh(&self) -> (&AtomicU64, &AtomicU64) {
        (self.word(2), self.word(3))
    }
}

impl Drop for Commits {
    fn drop(&mut self) {
        // SAFETY: 映射来自 map_file，之后不再访问
        unsafe { appine_hal::unmap(self.map.base, self.map.len) }
    }
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
    st.atime_ns = meta.accessed().map(to_ns).unwrap_or(st.mtime_ns);
}

/// 宿主上操作 object 出错（宿主的错误码）。
fn object_err(ino: Ino, e: i32) -> Errno {
    crate::debug::log(format_args!("inode store: object {ino}: errno {e}"));
    Errno(e)
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
        store.meta.commits.users().fetch_add(1, Ordering::SeqCst);
        Ok(store)
    }

    /// 打开已有的仓库。没有别的进程在使用它时（相当于开机），清空 `volatile` 里的目录
    /// （相对根目录的路径，比如 `dev/shm`：Linux 上它们是 tmpfs），清理上次运行留下的
    /// 已删除 inode。
    pub fn open(dir: &Path, volatile: &[&[u8]]) -> Result<Store, Errno> {
        if !dir.join("meta.db").exists() {
            return Err(Errno::ENOENT);
        }
        let store = Self::connect(dir)?;
        let fd = store.lock.as_raw_fd();
        if appine_hal::flock(fd, true, false).is_ok() {
            store.meta.commits.users().store(0, Ordering::SeqCst);
            store.remove_lost_objects()?;
            for path in volatile {
                store.empty_dir(path)?;
            }
            store.transaction(|s| s.remove_orphans())?;
            store.commit()?;
        }
        appine_hal::flock(fd, false, true).map_err(Errno)?;
        // 别的进程也在用仓库：等它提交推迟的写事务（它看到这个计数之后不再推迟），之后
        // 才能看到它的修改
        if store.meta.commits.users().fetch_add(1, Ordering::SeqCst) > 0 {
            store.exec("BEGIN IMMEDIATE")?;
            store.exec("COMMIT")?;
        }
        Ok(store)
    }

    /// 删掉上次没来得及提交就丢失的 inode 留下的 object（[`Commits::fresh`]）。
    fn remove_lost_objects(&self) -> Result<(), Errno> {
        let (lo, hi) = self.meta.commits.fresh();
        let range = lo.load(Ordering::SeqCst)..=hi.load(Ordering::SeqCst);
        if *range.start() != 0 {
            for ino in range {
                if self.query_stat_row(ino) == Err(Errno::ENOENT) {
                    self.remove_object(ino)?;
                }
            }
            lo.store(0, Ordering::SeqCst);
            hi.store(0, Ordering::SeqCst);
        }
        Ok(())
    }

    /// 清空目录 `path` 的内容，目录本身保留。不存在或者不是目录（比如符号链接）时
    /// 什么也不做。
    fn empty_dir(&self, path: &[u8]) -> Result<(), Errno> {
        let mut dir = ROOT;
        for name in path.split(|&b| b == b'/') {
            match self.lookup(dir, name)? {
                Some(ino) => dir = ino,
                None => return Ok(()),
            }
        }
        if !self.stat_row(dir)?.is_dir() {
            return Ok(());
        }
        self.remove_contents(dir)
    }

    fn remove_contents(&self, dir: Ino) -> Result<(), Errno> {
        for e in self.readdir(dir)? {
            if e.mode & mode::S_IFMT == mode::S_IFDIR {
                self.remove_contents(e.ino)?;
                self.rmdir(dir, &e.name)?;
            } else {
                self.unlink(dir, &e.name)?;
            }
        }
        Ok(())
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
            // 不交给后台线程：行一删掉，编号就可能分给新建的文件，后台线程晚一点删的会是
            // 新文件的 object。提交之前删也没关系，这些行早就没有目录项指着了
            if m & mode::S_IFMT == mode::S_IFREG {
                self.remove_object(ino)?;
            }
            self.db
                .prepare_cached("DELETE FROM inodes WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![ino]))
                .map_err(db_err)?;
            self.touch_ino(ino);
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
        let commits = Commits::open(&lock)?;
        let objects_dir = appine_hal::open_dir(&dir.join("objects")).map_err(Errno)?;
        let meta = Arc::new(Meta {
            lock: RwLock::new(()),
            cache: UnsafeCell::new(Cache {
                generation: commits.get(),
                ..Cache::default()
            }),
            commits,
            objects_dir,
            reaper: Reaper::default(),
        });
        let reaper = {
            let meta = meta.clone();
            std::thread::Builder::new()
                .name("objects-reaper".into())
                .spawn(move || reap(meta))
                .ok()
        };
        Ok(Store {
            db,
            objects: dir.join("objects"),
            lock,
            meta,
            reaper,
            writing: Cell::new(false),
            snapshot_at: Cell::new(None),
            pending: Cell::new(None),
            doomed: RefCell::default(),
            waker: RefCell::new(None),
            fills: RefCell::default(),
        })
    }

    /// 执行一条不带参数的语句（BEGIN、COMMIT、保存点），只编译一次。
    fn exec(&self, sql: &str) -> Result<(), Errno> {
        self.db
            .prepare_cached(sql)
            .and_then(|mut st| st.execute([]))
            .map(drop)
            .map_err(db_err)
    }

    /// 把 `f` 作为一个元数据操作执行：要么全部生效，要么（`f` 出错或者 panic 时）全部
    /// 回滚。操作是推迟提交的写事务（模块文档）里的一个保存点；写事务一开始就取得写锁
    /// （`IMMEDIATE`）：先读后写的事务在别的进程提交之后无法升级成写事务，那时
    /// busy_timeout 也帮不上忙。已经在操作里时（导入 rootfs 时整个导入是一个操作）直接
    /// 并入外层。
    pub fn transaction<T>(&self, f: impl FnOnce(&Store) -> Result<T, Errno>) -> Result<T, Errno> {
        self.operation(true, f)
    }

    /// 只有一条写语句的操作（改权限、属主、时间）：语句本身是原子的，失败时 SQLite 只撤销
    /// 这一条，不需要保存点。
    fn statement<T>(&self, f: impl FnOnce(&Store) -> Result<T, Errno>) -> Result<T, Errno> {
        self.operation(false, f)
    }

    fn operation<T>(
        &self,
        savepoint: bool,
        f: impl FnOnce(&Store) -> Result<T, Errno>,
    ) -> Result<T, Errno> {
        // 读事务里（[`Store::snapshot`]）也直接执行：和原来一样在读事务里写
        if self.writing.get() || (self.pending.get().is_none() && !self.db.is_autocommit()) {
            return f(self);
        }
        if self.pending.get().is_none() {
            self.exec("BEGIN IMMEDIATE")?;
            self.pending.set(Some(Instant::now()));
            if self.defers()
                && let Some(wake) = &*self.waker.borrow()
            {
                wake();
            }
        }
        let r = if savepoint {
            self.exec("SAVEPOINT op")
        } else {
            Ok(())
        };
        let r = r.and_then(|()| {
            let op = Op {
                store: self,
                savepoint,
                doomed: self.doomed.borrow().len(),
                done: false,
            };
            self.writing.set(true);
            let r = f(self);
            let r = match r {
                Ok(v) if savepoint => self.exec("RELEASE op").map(|()| v),
                r => r,
            };
            op.finish(r.is_ok());
            r
        });
        // 别的进程也在用仓库时立即提交，免得它看不到；推迟够久了也提交
        let overdue = self
            .pending
            .get()
            .is_some_and(|t| t.elapsed() >= COMMIT_DELAY);
        if overdue || !self.defers() {
            self.commit()?;
        }
        r
    }

    /// 写事务可以推迟提交：没有别的进程在用仓库。
    fn defers(&self) -> bool {
        self.meta.commits.users().load(Ordering::Relaxed) <= 1
    }

    /// 推迟提交时叫醒 `wake`（[`Store::commit_due`]）。
    pub fn set_waker(&self, wake: Box<dyn Fn()>) {
        *self.waker.borrow_mut() = Some(wake);
    }

    /// 到点了就提交推迟的写事务；还没到点时返回该提交的时刻。
    pub fn commit_due(&self) -> Option<Instant> {
        let due = self.pending.get()? + COMMIT_DELAY;
        if Instant::now() < due {
            return Some(due);
        }
        let _ = self.commit();
        None
    }

    /// 提交推迟的写事务（fsync、sync、关闭时），然后删除等着它的 object。提交失败时
    /// 推迟的修改全部丢失：数据库回到上次提交时的状态，缓存整个清空。
    pub fn commit(&self) -> Result<(), Errno> {
        if self.pending.take().is_none() {
            return Ok(());
        }
        let r = self.exec("COMMIT");
        let doomed = std::mem::take(&mut *self.doomed.borrow_mut());
        if r.is_err() {
            if !self.db.is_autocommit() {
                let _ = self.exec("ROLLBACK");
            }
            self.update(|c| c.clear(c.generation));
            let _ = self.remove_lost_objects();
            return r;
        }
        let (lo, hi) = self.meta.commits.fresh();
        lo.store(0, Ordering::SeqCst);
        hi.store(0, Ordering::SeqCst);
        self.reap_later(doomed);
        // 计数器必须在提交之后加：之前加的话，别的进程可能赶在提交完成之前重新读到旧的
        // 内容，再记成新的计数。计数器在这之前被别人改过，自己的缓存也整体作废
        self.update(|c| {
            let before = self.meta.commits.bump();
            if before == c.generation {
                c.generation = before + 1;
            } else {
                c.clear(before + 1);
            }
        });
        Ok(())
    }

    /// 改了目录 `dir` 里的名字 `name`：从自己的缓存里去掉受影响的条目。别的进程的缓存
    /// 在提交时作废（[`Store::commit`]）。
    fn touch_name(&self, dir: Ino, name: &[u8]) {
        self.fills
            .borrow_mut()
            .retain(|f| !matches!(f, Fill::Name(d, n, _) if *d == dir && n == name));
        self.update(|c| {
            if let Some(d) = c.dentries.get_mut(&dir) {
                d.remove(name);
            }
            c.listings.remove(&dir);
            c.names.remove(&dir);
        });
    }

    /// 操作成功之后把 `fill` 放进缓存。
    fn fill(&self, fill: Fill) {
        let mut fills = self.fills.borrow_mut();
        if fills.len() < Fill::MAX {
            fills.push(fill);
        }
    }

    /// 改了 inode `ino` 的行（同上）。
    fn touch_ino(&self, ino: Ino) {
        self.fills
            .borrow_mut()
            .retain(|f| !matches!(f, Fill::Row(st) if st.ino == ino));
        self.update(|c| {
            c.rows.remove(&ino);
            c.targets.remove(&ino);
            c.parents.remove(&ino);
        });
    }

    /// 读缓存。只有 Store 的方法改缓存，它们自己读不必加锁（[`Meta`]）。
    fn cached<R>(&self, f: impl FnOnce(&Cache) -> R) -> R {
        // SAFETY: Store 不是 Sync，别的线程不会同时修改；f 不回调 Store 的方法，借用不会
        // 与 update 的重叠
        f(unsafe { &*self.meta.cache.get() })
    }

    /// 改缓存：持有写锁，等不持 BKL 读缓存的线程读完。
    fn update<R>(&self, f: impl FnOnce(&mut Cache) -> R) -> R {
        let _guard = self
            .meta
            .lock
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        // SAFETY: 持有写锁，没有别的线程在读；f 不回调 Store 的方法
        f(unsafe { &mut *self.meta.cache.get() })
    }

    /// 不持 BKL 读缓存要用的部分（[`Meta`]）。
    pub(crate) fn meta(&self) -> &Arc<Meta> {
        &self.meta
    }

    /// 缓存没命中、要查数据库之前调用：在 [`Store::snapshot`] 里时，第一次查询开始读事务。
    fn begin_read(&self) -> Result<(), Errno> {
        if self.snapshot_at.get() == Some(None) {
            self.snapshot_at.set(Some(Some(self.meta.commits.get())));
            self.exec("BEGIN")?;
        }
        Ok(())
    }

    /// 缓存是现在的（读缓存之前已经清空过旧的）：操作成功之后往里填它新建的东西用。
    fn cache_generation_any(&self) -> bool {
        self.cached(|c| c.generation) == self.meta.commits.get()
    }

    /// 读缓存之前调用：缓存是旧的就先清空。查到的能填进缓存时（不在操作里）返回现在的
    /// 提交计数。
    fn cache_generation(&self) -> Option<u64> {
        let now = self.meta.commits.get();
        if self.cached(|c| c.generation != now || c.entries > Cache::MAX_ENTRIES) {
            self.update(|c| c.clear(now));
        }
        (!self.writing.get()).then_some(now)
    }

    /// 刚查到的结果能不能放进缓存：读事务开始以来、以及这次查询期间都没有人提交过
    /// （否则查到的可能比 `generation` 旧）。
    fn may_fill(&self, generation: u64) -> bool {
        self.snapshot_at
            .get()
            .flatten()
            .is_none_or(|s| s == generation)
            && self.meta.commits.get() == generation
    }

    /// 在一个读事务里执行 `f`，里面的查询看到同一个快照，并且共用一次加锁：WAL 模式下
    /// 每条语句单独开、关读事务都要 fcntl 加解锁并检查文件长度，逐级解析一个路径时
    /// 这比查询本身还贵。读事务在缓存第一次没命中时才开始，全部命中时不碰数据库。
    /// 已经在事务里时直接执行。
    pub fn snapshot<T>(&self, f: impl FnOnce(&Store) -> Result<T, Errno>) -> Result<T, Errno> {
        if !self.db.is_autocommit() || self.snapshot_at.get().is_some() {
            return f(self);
        }
        self.snapshot_at.set(Some(None));
        let r = f(self);
        if self.snapshot_at.get().flatten().is_some() {
            // 只读的事务提交只是释放读锁
            let _ = self.exec(if r.is_ok() { "COMMIT" } else { "ROLLBACK" });
        }
        self.snapshot_at.set(None);
        r
    }

    /// 打开 object（相对 `objects` 目录，不解析整个路径），`flags` 是宿主 open 的标志。
    fn object(&self, ino: Ino, flags: i32) -> Result<File, Errno> {
        let mut name = [0u8; 21];
        let dir = self.meta.objects_dir.as_raw_fd();
        appine_hal::open_at(dir, object_name(ino, &mut name), flags, 0o666)
            .map_err(|e| object_err(ino, e))
    }

    /// 删除 object；它已经不在时不算错误。
    fn remove_object(&self, ino: Ino) -> Result<(), Errno> {
        remove_object(&self.meta.objects_dir, ino)
    }

    /// 把 object 交给后台线程删除；线程没有建起来时自己删。
    fn reap_later(&self, inos: Vec<Ino>) {
        if inos.is_empty() {
            return;
        }
        if self.reaper.is_none() {
            for ino in inos {
                let _ = self.remove_object(ino);
            }
            return;
        }
        let r = &self.meta.reaper;
        let mut st = r.state.lock().unwrap_or_else(PoisonError::into_inner);
        #[cfg(test)]
        {
            st.handed += inos.len();
        }
        st.queue.extend(inos);
        drop(st);
        r.work.notify_one();
    }

    /// 在目录 `parent` 里查找 `name`。
    pub fn lookup(&self, parent: Ino, name: &[u8]) -> Result<Option<Ino>, Errno> {
        let generation = self.cache_generation();
        if let Some(hit) = self.cached(|c| c.dentries.get(&parent)?.get(name).copied()) {
            return Ok(hit);
        }
        self.begin_read()?;
        let found = self
            .db
            .prepare_cached("SELECT ino FROM dentries WHERE parent = ?1 AND name = ?2")
            .and_then(|mut st| st.query_row(params![parent, name], |row| row.get(0)))
            .optional()
            .map_err(db_err)?;
        if generation.is_some_and(|g| self.may_fill(g)) {
            self.update(|c| {
                c.entries += 1;
                c.dentries
                    .entry(parent)
                    .or_default()
                    .insert(name.to_vec(), found);
            });
        }
        Ok(found)
    }

    /// 目录 `parent` 里指向 `ino` 的一个名字（getcwd 用；打开相对路径时也要算出绝对路径，
    /// 见 `syscall::fs` 的 `display_path`，所以要缓存）。
    pub fn name_of(&self, parent: Ino, ino: Ino) -> Result<Option<Vec<u8>>, Errno> {
        let generation = self.cache_generation();
        if let Some(hit) = self.cached(|c| c.names.get(&parent)?.get(&ino).cloned()) {
            return Ok(hit);
        }
        self.begin_read()?;
        let found: Option<Vec<u8>> = self
            .db
            .prepare_cached(
                "SELECT name FROM dentries WHERE parent = ?1 AND ino = ?2 ORDER BY name LIMIT 1",
            )
            .and_then(|mut st| st.query_row(params![parent, ino], |row| row.get(0)))
            .optional()
            .map_err(db_err)?;
        if generation.is_some_and(|g| self.may_fill(g)) {
            self.update(|c| {
                c.entries += 1;
                c.names
                    .entry(parent)
                    .or_default()
                    .insert(ino, found.clone());
            });
        }
        Ok(found)
    }

    /// 读取元数据。普通文件的 size 和 mtime 取自宿主 object 文件。
    pub fn stat(&self, ino: Ino) -> Result<Stat, Errno> {
        let mut st = self.stat_row(ino)?;
        if st.kind() == mode::S_IFREG {
            fill_from_host(&mut st, &self.meta.objects_dir, ino)?;
        }
        Ok(st)
    }

    /// 已打开的普通文件的元数据：size 和 mtime 取自打开的 object，文件被删除之后也能用。
    pub fn stat_open(&self, ino: Ino, file: &File) -> Result<Stat, Errno> {
        let mut st = self.stat_row(ino)?;
        fill_from_object(&mut st, &file.metadata().map_err(io_err)?);
        Ok(st)
    }

    /// 数据库里的元数据（不碰宿主，普通文件的 size、时间没有填）：打开文件时检查权限用。
    pub fn attrs(&self, ino: Ino) -> Result<Stat, Errno> {
        self.stat_row(ino)
    }

    /// 文件类型位（不需要读 object，删除之后仍然打开着的文件也能用）。
    pub fn kind(&self, ino: Ino) -> Result<u32, Errno> {
        Ok(self.stat_row(ino)?.kind())
    }

    /// 数据库里的元数据（普通文件的 size、mtime 还没有填）。
    fn stat_row(&self, ino: Ino) -> Result<Stat, Errno> {
        let generation = self.cache_generation();
        if let Some(hit) = self.cached(|c| c.rows.get(&ino).copied()) {
            return Ok(hit);
        }
        self.begin_read()?;
        let st = self.query_stat_row(ino)?;
        if generation.is_some_and(|g| self.may_fill(g)) {
            self.update(|c| {
                c.entries += 1;
                c.rows.insert(ino, st);
            });
        }
        Ok(st)
    }

    fn query_stat_row(&self, ino: Ino) -> Result<Stat, Errno> {
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
            atime_ns: mtime_ns,
            mtime_ns,
            ctime_ns,
        })
    }

    /// 读取符号链接的目标。
    pub fn readlink(&self, ino: Ino) -> Result<Vec<u8>, Errno> {
        let generation = self.cache_generation();
        if let Some(hit) = self.cached(|c| c.targets.get(&ino).cloned()) {
            return Ok(hit);
        }
        self.begin_read()?;
        let target = self
            .db
            .prepare_cached("SELECT target FROM inodes WHERE ino = ?1")
            .and_then(|mut st| st.query_row(params![ino], |r| r.get::<_, Option<Vec<u8>>>(0)))
            .optional()
            .map_err(db_err)?
            .ok_or(Errno::ENOENT)?
            .ok_or(Errno::EINVAL)?;
        if generation.is_some_and(|g| self.may_fill(g)) {
            self.update(|c| {
                c.entries += 1;
                c.targets.insert(ino, target.clone());
            });
        }
        Ok(target)
    }

    /// 目录的上级目录；根目录的上级是它自己。
    pub fn parent(&self, dir: Ino) -> Result<Ino, Errno> {
        let generation = self.cache_generation();
        if let Some(hit) = self.cached(|c| c.parents.get(&dir).copied()) {
            return Ok(hit);
        }
        self.begin_read()?;
        let parent = self
            .db
            .prepare_cached("SELECT parent FROM inodes WHERE ino = ?1")
            .and_then(|mut st| st.query_row(params![dir], |r| r.get(0)))
            .optional()
            .map_err(db_err)?
            .ok_or(Errno::ENOENT)?;
        if generation.is_some_and(|g| self.may_fill(g)) {
            self.update(|c| {
                c.entries += 1;
                c.parents.insert(dir, parent);
            });
        }
        Ok(parent)
    }

    /// 列出目录内容，按名字排序，不含 `.` 和 `..`。
    pub fn readdir(&self, dir: Ino) -> Result<Vec<DirEntry>, Errno> {
        let generation = self.cache_generation();
        if let Some(hit) = self.cached(|c| c.listings.get(&dir).cloned()) {
            return Ok(hit);
        }
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
        let list: Vec<DirEntry> = rows.collect::<Result<_, _>>().map_err(db_err)?;
        if generation.is_some_and(|g| self.may_fill(g)) {
            self.update(|c| {
                c.entries += list.len() + 1;
                c.listings.insert(dir, list.clone());
            });
        }
        Ok(list)
    }

    /// 打开普通文件的 object。
    pub fn open_object(&self, ino: Ino, write: bool) -> Result<File, Errno> {
        self.object(ino, if write { libc::O_RDWR } else { libc::O_RDONLY })
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
        let ino = self.db.last_insert_rowid() as Ino;
        self.touch_ino(ino);
        self.fill(Fill::Row(Stat {
            ino,
            mode,
            nlink,
            uid,
            gid,
            rdev: 0,
            size: 0,
            atime_ns: now,
            mtime_ns: now,
            ctime_ns: now,
        }));
        let (lo, hi) = self.meta.commits.fresh();
        if lo.load(Ordering::Relaxed) == 0 {
            lo.store(ino, Ordering::Relaxed);
        }
        hi.fetch_max(ino, Ordering::Relaxed);
        Ok(ino)
    }

    fn add_dentry(&self, parent: Ino, name: &[u8], ino: Ino) -> Result<(), Errno> {
        if self.lookup(parent, name)?.is_some() {
            return Err(Errno::EEXIST);
        }
        if !self.stat_row(parent)?.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        self.db
            .prepare_cached("INSERT INTO dentries (parent, name, ino) VALUES (?1, ?2, ?3)")
            .and_then(|mut st| st.execute(params![parent, name, ino]))
            .map_err(db_err)?;
        self.touch_name(parent, name);
        self.fill(Fill::Name(parent, name.to_vec(), ino));
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
            s.touch_ino(parent);
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
            // 最后建 object：失败时整个操作回滚，不留下没有 object 的 inode
            let file = s.new_object(ino)?;
            Ok((ino, file))
        })
    }

    /// 为新的 inode 建空的 object。已经有的话是没来得及提交就丢失的 inode 留下的
    /// （inode 编号又分配了出来），清空后接着用。
    fn new_object(&self, ino: Ino) -> Result<File, Errno> {
        self.object(ino, libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC)
    }

    /// 新建一个没有名字的普通文件（memfd_create）：链接数为 0，object 一开始就删除，
    /// 关闭之后数据随之消失，inode 在下次打开仓库时清理。
    pub fn create_anonymous(&self, perm: u32) -> Result<(Ino, File), Errno> {
        self.transaction(|s| {
            let ino = s.new_inode(mode::S_IFREG | (perm & 0o7777), 0, 0, 0, 0)?;
            let file = s.new_object(ino)?;
            s.remove_object(ino)?;
            Ok((ino, file))
        })
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
            s.touch_ino(ino);
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
            s.touch_ino(ino);
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
            s.touch_ino(ino);
            Ok(())
        })
    }

    /// 修改权限位（保留类型位）。
    pub fn set_mode(&self, ino: Ino, perm: u32) -> Result<(), Errno> {
        self.statement(|s| {
            let n =
                s.db.prepare_cached(
                    "UPDATE inodes SET mode = (mode & ?2) | ?3, ctime_ns = ?4 WHERE ino = ?1",
                )
                .and_then(|mut st| st.execute(params![ino, mode::S_IFMT, perm & 0o7777, now_ns()]))
                .map_err(db_err)?;
            s.touch_ino(ino);
            if n == 0 { Err(Errno::ENOENT) } else { Ok(()) }
        })
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
        self.touch_name(parent, name);
        self.touch_ino(ino);
        let st = self.stat_row(ino)?;
        if st.nlink == 0 && st.kind() == mode::S_IFREG {
            // 提交之后再删：提交之前 App 被杀掉的话，目录项还在，object 也得在
            self.doomed.borrow_mut().push(ino);
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
        self.touch_name(parent, name);
        self.touch_ino(ino);
        self.touch_ino(parent);
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
        for ino in [dir, from, to] {
            self.touch_ino(ino);
        }
        Ok(())
    }

    fn set_dentry(&self, parent: Ino, name: &[u8], ino: Ino) -> Result<(), Errno> {
        self.db
            .prepare_cached(
                "INSERT OR REPLACE INTO dentries (parent, name, ino) VALUES (?1, ?2, ?3)",
            )
            .and_then(|mut st| st.execute(params![parent, name, ino]))
            .map_err(db_err)?;
        self.touch_name(parent, name);
        self.fill(Fill::Name(parent, name.to_vec(), ino));
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
            s.touch_name(old_dir, old_name);
            s.set_dentry(new_dir, new_name, src)?;
            if src_dir {
                s.reparent_dir(src, old_dir, new_dir)?;
            }
            s.db.prepare_cached("UPDATE inodes SET ctime_ns = ?2 WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![src, now_ns()]))
                .map_err(db_err)?;
            s.touch_ino(src);
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
        self.statement(|s| {
            s.db.prepare_cached("UPDATE inodes SET mode = (mode & ?2) | ?3, uid = ?4, gid = ?5, ctime_ns = ?6 WHERE ino = ?1")
                .and_then(|mut st| st.execute(params![ino, mode::S_IFMT, perm & 0o7777, uid, gid, now_ns()]))
                .map_err(db_err)?;
            s.touch_ino(ino);
            Ok(())
        })
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
        self.touch_ino(ino);
        Ok(())
    }

    /// 设置修改时间：普通文件改宿主 object 的 mtime，其余改数据库。
    pub fn set_mtime(&self, ino: Ino, mtime_ns: i64) -> Result<(), Errno> {
        self.set_times(ino, None, Some(mtime_ns))
    }

    /// 设置访问时间和修改时间（None 表示不改）。普通文件改宿主 object 的时间；其余的
    /// 只记录修改时间，访问时间跟着它。
    pub fn set_times(
        &self,
        ino: Ino,
        atime_ns: Option<i64>,
        mtime_ns: Option<i64>,
    ) -> Result<(), Errno> {
        if self.kind(ino)? == mode::S_IFREG {
            let mut name = [0u8; 21];
            return appine_hal::set_times_at(
                self.meta.objects_dir.as_raw_fd(),
                object_name(ino, &mut name),
                atime_ns.map(split_ns),
                mtime_ns.map(split_ns),
            )
            .map_err(|e| object_err(ino, e));
        }
        if let Some(ns) = mtime_ns {
            self.statement(|s| {
                s.db.prepare_cached("UPDATE inodes SET mtime_ns = ?2 WHERE ino = ?1")
                    .and_then(|mut st| st.execute(params![ino, ns]))
                    .map_err(db_err)?;
                s.touch_ino(ino);
                Ok(())
            })?;
        }
        Ok(())
    }

    /// 经由打开的 object 设置普通文件 `ino` 的访问时间和修改时间：文件删除之后（object
    /// 已经删了）也能用。
    pub fn set_times_open(
        &self,
        ino: Ino,
        file: &File,
        atime_ns: Option<i64>,
        mtime_ns: Option<i64>,
    ) -> Result<(), Errno> {
        appine_hal::set_times_fd(
            file.as_raw_fd(),
            atime_ns.map(split_ns),
            mtime_ns.map(split_ns),
        )
        .map_err(|e| object_err(ino, e))
    }
}

/// 纳秒数拆成 (秒, 纳秒)。
fn split_ns(ns: i64) -> (i64, i64) {
    (ns.div_euclid(1_000_000_000), ns.rem_euclid(1_000_000_000))
}

/// [`Store::transaction`] 里的一个操作：没有正常结束（出错或者 panic）时回滚到它的
/// 保存点，撤销它要删除的 object。没有保存点的（[`Store::statement`]）只清掉"在操作里"。
struct Op<'a> {
    store: &'a Store,
    savepoint: bool,
    doomed: usize,
    done: bool,
}

impl Op<'_> {
    fn finish(mut self, ok: bool) {
        self.done = ok;
    }
}

impl Drop for Op<'_> {
    fn drop(&mut self) {
        let s = self.store;
        s.writing.set(false);
        let fills = std::mem::take(&mut *s.fills.borrow_mut());
        if !self.done {
            if self.savepoint {
                let _ = s.exec("ROLLBACK TO op");
                let _ = s.exec("RELEASE op");
                s.doomed.borrow_mut().truncate(self.doomed);
            }
            return;
        }
        if !fills.is_empty() && s.cache_generation_any() {
            s.update(|c| {
                for f in fills {
                    c.entries += 1;
                    match f {
                        Fill::Name(dir, name, ino) => {
                            c.dentries.entry(dir).or_default().insert(name, Some(ino));
                        }
                        Fill::Row(st) => {
                            c.rows.insert(st.ino, st);
                        }
                    }
                }
            });
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        if let Err(e) = self.commit() {
            crate::debug::log(format_args!("inode store: final commit failed: {e:?}"));
        }
        if let Some(t) = self.reaper.take() {
            let r = &self.meta.reaper;
            r.state.lock().unwrap_or_else(PoisonError::into_inner).quit = true;
            r.work.notify_one();
            let _ = t.join();
        }
        self.meta.commits.users().fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    impl Store {
        /// 等后台线程删完交给它的 object。
        fn reaped(&self) {
            let r = &self.meta.reaper;
            let mut st = r.state.lock().unwrap();
            while !st.queue.is_empty() || st.busy {
                st = r.idle.wait(st).unwrap();
            }
        }

        /// 模拟进程在提交之前被杀掉：推迟的修改丢失，等着删除的 object 留着，`lock` 里
        /// 新分配的 inode 编号范围也留着。
        fn crash(self) {
            let _ = self.exec("ROLLBACK");
            self.pending.set(None);
            self.doomed.borrow_mut().clear();
        }
    }

    fn temp_store(name: &str) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("appine-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        (Store::create(&dir).unwrap(), dir)
    }

    #[test]
    fn cache_follows_own_and_other_processes_writes() {
        let (a, dir) = temp_store("cache");
        // 第二个连接相当于另一个 appine 进程
        let b = Store::open(&dir, &[]).unwrap();
        let names = |s: &Store| -> Vec<Vec<u8>> {
            s.readdir(ROOT)
                .unwrap()
                .into_iter()
                .map(|e| e.name)
                .collect()
        };
        // 自己的写入：缓存里的"不存在"、属性和目录列表随之更新
        assert_eq!(a.lookup(ROOT, b"f").unwrap(), None);
        assert!(names(&a).is_empty());
        let (f, _) = a.create_file(ROOT, b"f", 0o644, 0, 0).unwrap();
        assert_eq!(a.lookup(ROOT, b"f").unwrap(), Some(f));
        assert_eq!(names(&a), [b"f".to_vec()]);
        assert_eq!(a.stat(f).unwrap().mode & 0o777, 0o644);
        a.set_mode(f, 0o600).unwrap();
        assert_eq!(a.stat(f).unwrap().mode & 0o777, 0o600);
        a.rename(ROOT, b"f", ROOT, b"g", false, false).unwrap();
        assert_eq!(a.lookup(ROOT, b"f").unwrap(), None);
        assert_eq!(a.lookup(ROOT, b"g").unwrap(), Some(f));
        // 别的进程的写入：计数器变了，整个缓存作废
        assert_eq!(b.lookup(ROOT, b"g").unwrap(), Some(f));
        assert_eq!(b.stat(f).unwrap().nlink, 1);
        a.link(ROOT, b"h", f).unwrap();
        a.unlink(ROOT, b"g").unwrap();
        assert_eq!(b.lookup(ROOT, b"g").unwrap(), None);
        assert_eq!(b.lookup(ROOT, b"h").unwrap(), Some(f));
        assert_eq!(names(&b), [b"h".to_vec()]);
        let d = b.mkdir(ROOT, b"d", 0o755, 0, 0).unwrap();
        assert_eq!(a.lookup(ROOT, b"d").unwrap(), Some(d));
        assert_eq!(a.stat(ROOT).unwrap().nlink, 3);
        // 读事务里的查询同样走缓存，读事务之外的写入随后可见
        a.snapshot(|s| {
            assert_eq!(s.lookup(ROOT, b"h")?, Some(f));
            assert_eq!(s.parent(d)?, ROOT);
            Ok(())
        })
        .unwrap();
        b.rmdir(ROOT, b"d").unwrap();
        a.snapshot(|s| {
            assert_eq!(s.lookup(ROOT, b"d")?, None);
            Ok(())
        })
        .unwrap();
        drop((a, b));
        let _ = fs::remove_dir_all(&dir);
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
        let s = Store::open(&dir, &[]).unwrap();
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
        let s = Store::open(&dir, &[]).unwrap();
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
        // 定时器线程到点提交；不提交的话，下面打开仓库要等到超时
        s.commit().unwrap();
        // 别的进程还在使用仓库时不清理：它可能还打开着这个 inode
        let other = Store::open(&dir, &[]).unwrap();
        assert!(other.stat_row(anon).is_ok());
        drop(other);
        // 独占打开仓库时清理掉没有名字的 inode 和它的扩展属性
        drop((file, s));
        let s = Store::open(&dir, &[]).unwrap();
        assert!(s.stat_row(anon).is_err());
        assert_eq!(s.list_xattr(anon).unwrap(), Vec::<Vec<u8>>::new());
        assert_eq!(s.get_xattr(f, b"user.a").unwrap(), b"one");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn volatile_directories_are_emptied_on_first_open() {
        let (s, dir) = temp_store("volatile");
        let dev = s.mkdir(ROOT, b"dev", 0o755, 0, 0).unwrap();
        let shm = s.mkdir(dev, b"shm", 0o1777, 0, 0).unwrap();
        s.create_file(shm, b"seg", 0o600, 0, 0).unwrap();
        let run = s.mkdir(ROOT, b"run", 0o755, 0, 0).unwrap();
        let sub = s.mkdir(run, b"sub", 0o755, 0, 0).unwrap();
        s.create_file(sub, b"pid", 0o644, 0, 0).unwrap();
        s.symlink(ROOT, b"tmp", b"run", 0, 0).unwrap();
        let volatile: &[&[u8]] = &[b"dev/shm", b"run", b"tmp", b"missing/dir"];
        s.commit().unwrap();
        // 别的进程还在使用仓库：不是开机，什么都不删
        let other = Store::open(&dir, volatile).unwrap();
        assert!(other.lookup(shm, b"seg").unwrap().is_some());
        drop((other, s));
        let s = Store::open(&dir, volatile).unwrap();
        assert!(s.readdir(shm).unwrap().is_empty());
        assert!(s.readdir(run).unwrap().is_empty());
        assert_eq!(s.stat(shm).unwrap().mode & 0o7777, 0o1777);
        assert!(s.lookup(ROOT, b"tmp").unwrap().is_some());
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
                    let s = Store::open(&dir, &[]).unwrap();
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

    #[test]
    fn deferred_commits() {
        let (s, dir) = temp_store("defer");
        let (f, _) = s.create_file(ROOT, b"f", 0o644, 0, 0).unwrap();
        // 别的连接（直接读数据库）在提交之前看不到
        let raw = Connection::open(dir.join("meta.db")).unwrap();
        let count = |name: &[u8]| -> i64 {
            raw.query_row(
                "SELECT count(*) FROM dentries WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(count(b"f"), 0);
        // 失败的操作只回滚它自己：在普通文件下面建符号链接，插入 inode 之后才发现上级
        // 不是目录
        assert_eq!(s.symlink(f, b"x", b"t", 0, 0), Err(Errno::ENOTDIR));
        let max: Ino =
            s.db.query_row("SELECT max(ino) FROM inodes", [], |r| r.get(0))
                .unwrap();
        assert_eq!(max, f);
        assert_eq!(s.lookup(ROOT, b"f").unwrap(), Some(f));
        s.commit().unwrap();
        assert_eq!(count(b"f"), 1);
        // 最后一个链接删除之后，object 在提交时才删
        let object = dir.join("objects").join(f.to_string());
        s.unlink(ROOT, b"f").unwrap();
        assert!(object.exists());
        s.commit().unwrap();
        s.reaped();
        assert!(!object.exists());
        assert_eq!(count(b"f"), 0);
        drop((raw, s));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn inode_numbers_reused_after_boot_keep_their_objects() {
        let (s, dir) = temp_store("reuse");
        let inos: Vec<Ino> = (0..50)
            .map(|i| {
                let name = format!("f{i}");
                let (ino, _) = s.create_file(ROOT, name.as_bytes(), 0o644, 0, 0).unwrap();
                s.unlink(ROOT, name.as_bytes()).unwrap();
                ino
            })
            .collect();
        drop(s);
        // 开机清理删掉这些 inode 行，编号又分配给新建的文件。清理时自己删 object，不交给
        // 后台线程：它晚一点删的话，删掉的是新文件的
        let s = Store::open(&dir, &[]).unwrap();
        assert_eq!(s.meta.reaper.state.lock().unwrap().handed, 0);
        for i in 0..50 {
            s.create_file(ROOT, format!("g{i}").as_bytes(), 0o644, 0, 0)
                .unwrap();
        }
        s.commit().unwrap();
        s.reaped();
        for i in 0..50 {
            let ino = s.lookup(ROOT, format!("g{i}").as_bytes()).unwrap().unwrap();
            assert!(inos.contains(&ino));
            assert!(dir.join("objects").join(ino.to_string()).exists());
        }
        drop(s);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn killed_before_commit() {
        let (s, dir) = temp_store("killed");
        let (g, _) = s.create_file(ROOT, b"g", 0o644, 0, 0).unwrap();
        s.commit().unwrap();
        // 没提交就被杀掉：删除没有生效，新建的文件随之丢失，它的 object 开机时删掉
        s.unlink(ROOT, b"g").unwrap();
        let (f, _) = s.create_file(ROOT, b"f", 0o644, 0, 0).unwrap();
        let object = |ino: Ino| dir.join("objects").join(ino.to_string());
        assert!(object(f).exists());
        s.crash();
        let s = Store::open(&dir, &[]).unwrap();
        assert_eq!(s.lookup(ROOT, b"g").unwrap(), Some(g));
        assert!(object(g).exists());
        assert_eq!(s.lookup(ROOT, b"f").unwrap(), None);
        assert!(!object(f).exists());
        // 编号又分配了出来
        assert_eq!(s.mkdir(ROOT, b"d", 0o755, 0, 0).unwrap(), f);
        drop(s);
        let _ = fs::remove_dir_all(&dir);
    }
}
