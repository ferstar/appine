//! 进程内的管道（pipe2）。
//!
//! 缓冲区大小与 Linux 默认的 64 KiB 相同，不超过 `PIPE_BUF` 的写入是原子的。
//! 读写两端各自计数打开文件的个数：最后一个写端关闭后读端读到 EOF，最后一个
//! 读端关闭后写入失败并产生 SIGPIPE。阻塞和唤醒走 [`crate::kernel`] 的统一等待机制。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::kernel::request_wake;

/// 管道容量。
pub const PIPE_CAPACITY: usize = 64 * 1024;
/// 不超过这个长度的写入不会和其他写入交错。
pub const PIPE_BUF: usize = 4096;

/// 一个管道。
pub struct Pipe {
    /// 缓冲的数据。
    pub buf: VecDeque<u8>,
    /// 打开的读端个数。
    pub readers: usize,
    /// 打开的写端个数。
    pub writers: usize,
    /// fstat 报告的 inode 编号。
    pub ino: u64,
}

impl Pipe {
    /// 剩余空间。
    pub fn space(&self) -> usize {
        PIPE_CAPACITY - self.buf.len()
    }
}

/// 管道的一端，由一个打开文件持有。
pub struct PipeEnd {
    /// 管道。
    pub pipe: Rc<RefCell<Pipe>>,
    /// 是不是写端。
    pub write: bool,
}

impl PipeEnd {
    /// 新建管道，返回 (读端, 写端)。
    pub fn pair() -> (PipeEnd, PipeEnd) {
        static NEXT_INO: AtomicU64 = AtomicU64::new(1);
        let pipe = Rc::new(RefCell::new(Pipe {
            buf: VecDeque::new(),
            readers: 1,
            writers: 1,
            ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
        }));
        (
            PipeEnd {
                pipe: pipe.clone(),
                write: false,
            },
            PipeEnd { pipe, write: true },
        )
    }
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        let mut p = self.pipe.borrow_mut();
        if self.write {
            p.writers -= 1;
        } else {
            p.readers -= 1;
        }
        // 等在另一端的线程要看到 EOF 或 EPIPE
        request_wake();
    }
}
