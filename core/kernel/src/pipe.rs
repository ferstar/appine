//! 进程内的管道：pipe2 的匿名管道，以及仓库里的命名管道（FIFO，同一个 inode 的所有
//! 打开共用一个管道对象）。
//!
//! 缓冲区大小默认与 Linux 一样是 64 KiB，可以用 `F_SETPIPE_SZ` 修改；不超过
//! `PIPE_BUF` 的写入是原子的。
//! 读写两端各自计数打开文件的个数：最后一个写端关闭后读端读到 EOF，最后一个
//! 读端关闭后写入失败并产生 SIGPIPE。阻塞和唤醒走 [`crate::kernel`] 的统一等待机制。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::kernel::request_wake_files;
use crate::vfs::store::Ino;

/// 管道的默认容量。
pub const PIPE_CAPACITY: usize = 64 * 1024;
/// `/proc/sys/fs/pipe-max-size` 的默认值：没有 `CAP_SYS_RESOURCE` 时管道的最大容量。
pub const PIPE_MAX_SIZE: usize = 1024 * 1024;
/// 不超过这个长度的写入不会和其他写入交错。
pub const PIPE_BUF: usize = 4096;

/// 一个管道。
pub struct Pipe {
    /// 缓冲的数据。
    pub buf: VecDeque<u8>,
    /// 容量。
    pub capacity: usize,
    /// 打开的读端个数。
    pub readers: usize,
    /// 打开的写端个数。
    pub writers: usize,
    /// fstat 报告的 inode 编号。
    pub ino: u64,
    /// 命名管道在仓库里的 inode；匿名管道是 None。
    pub fifo: Option<Ino>,
    /// 读端、写端被打开过的次数：打开 FIFO 时等对端，据此知道对端来过（即使已经关掉）。
    pub reader_opens: u64,
    pub writer_opens: u64,
}

impl Pipe {
    /// 剩余空间。
    pub fn space(&self) -> usize {
        self.capacity.saturating_sub(self.buf.len())
    }

    fn new(ino: u64, fifo: Option<Ino>, capacity: usize) -> Rc<RefCell<Pipe>> {
        Rc::new(RefCell::new(Pipe {
            buf: VecDeque::new(),
            capacity,
            readers: 0,
            writers: 0,
            ino,
            fifo,
            reader_opens: 0,
            writer_opens: 0,
        }))
    }

    /// 命名管道 `ino` 的管道对象，还没有任何一端。
    pub fn fifo(ino: Ino, capacity: usize) -> Rc<RefCell<Pipe>> {
        Pipe::new(ino, Some(ino), capacity)
    }
}

/// 管道的一端，由一个打开文件持有。以读写方式打开的 FIFO 同时是读端和写端。
pub struct PipeEnd {
    /// 管道。
    pub pipe: Rc<RefCell<Pipe>>,
    /// 是不是读端。
    pub read: bool,
    /// 是不是写端。
    pub write: bool,
}

impl PipeEnd {
    /// 新建容量为 `capacity` 的匿名管道，返回 (读端, 写端)。
    pub fn pair(capacity: usize) -> (PipeEnd, PipeEnd) {
        static NEXT_INO: AtomicU64 = AtomicU64::new(1);
        let pipe = Pipe::new(NEXT_INO.fetch_add(1, Ordering::Relaxed), None, capacity);
        (
            PipeEnd::open(&pipe, true, false),
            PipeEnd::open(&pipe, false, true),
        )
    }

    /// 打开管道的一端（FIFO 的 open，或者经由 `/proc/<pid>/fd/<n>` 再打开）。
    pub fn open(pipe: &Rc<RefCell<Pipe>>, read: bool, write: bool) -> PipeEnd {
        {
            let mut p = pipe.borrow_mut();
            if read {
                p.readers += 1;
                p.reader_opens += 1;
            }
            if write {
                p.writers += 1;
                p.writer_opens += 1;
            }
        }
        // 打开 FIFO 时等着对端的线程可以继续了
        request_wake_files();
        PipeEnd {
            pipe: pipe.clone(),
            read,
            write,
        }
    }
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        let mut p = self.pipe.borrow_mut();
        if self.read {
            p.readers -= 1;
        }
        if self.write {
            p.writers -= 1;
        }
        // 等在另一端的线程要看到 EOF 或 EPIPE
        request_wake_files();
    }
}
