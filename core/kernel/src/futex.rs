//! futex 的等待队列（设计文档 §6）。键是翻译后的宿主地址，所以 fork 之后共用同一段
//! 宿主内存的进程能互相唤醒；文件的共享映射以宿主文件和偏移为键，因为各个进程分别
//! 映射同一个文件（`sem_open` 的 `/dev/shm` 文件）时宿主地址各不相同。
//!
//! 等待者登记在队列里，然后走统一的等待机制；被唤醒的等待者从队列里移除，
//! 它重新检查时发现自己不在队列里了，就知道是被 FUTEX_WAKE 唤醒的。

use crate::kernel::request_wake;

/// futex 的键（[`crate::mm::Mm::futex_key`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    /// 宿主地址。
    Host(usize),
    /// 宿主文件（设备号、inode）里的偏移。
    File { dev: u64, ino: u64, offset: u64 },
}

struct Entry {
    id: u64,
    key: Key,
    bitset: u32,
}

/// 所有 futex 等待者，按登记顺序排列（唤醒时先到先得）。
#[derive(Default)]
pub(crate) struct Futexes {
    next_id: u64,
    waiting: Vec<Entry>,
}

impl Futexes {
    /// 登记一个等待者，返回它的编号。
    pub fn enqueue(&mut self, key: Key, bitset: u32) -> u64 {
        self.next_id += 1;
        self.waiting.push(Entry {
            id: self.next_id,
            key,
            bitset,
        });
        self.next_id
    }

    /// 等待者是否还在队列里（不在就是已经被唤醒）。
    pub fn is_queued(&self, id: u64) -> bool {
        self.waiting.iter().any(|e| e.id == id)
    }

    /// 移除等待者（超时、被信号打断）。
    pub fn remove(&mut self, id: u64) {
        self.waiting.retain(|e| e.id != id);
    }

    /// 唤醒最多 `n` 个等在 `key` 上、bitset 有交集的等待者，返回唤醒的个数。
    pub fn wake(&mut self, key: Key, n: u32, bitset: u32) -> u32 {
        let mut woken = 0;
        self.waiting.retain(|e| {
            if woken < n && e.key == key && e.bitset & bitset != 0 {
                woken += 1;
                false
            } else {
                true
            }
        });
        if woken > 0 {
            request_wake();
        }
        woken
    }

    /// FUTEX_REQUEUE：唤醒 `key` 上最多 `n_wake` 个等待者，再把最多 `n_requeue` 个
    /// 移到 `key2` 上。返回唤醒和移动的总数。
    pub fn requeue(&mut self, key: Key, n_wake: u32, key2: Key, n_requeue: u32) -> u32 {
        let woken = self.wake(key, n_wake, u32::MAX);
        let mut moved = 0;
        for e in self.waiting.iter_mut().filter(|e| e.key == key) {
            if moved == n_requeue {
                break;
            }
            e.key = key2;
            moved += 1;
        }
        woken + moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_and_requeue_in_order() {
        let (k1, k2) = (
            Key::Host(0x1000),
            Key::File {
                dev: 1,
                ino: 2,
                offset: 0,
            },
        );
        let mut f = Futexes::default();
        let a = f.enqueue(k1, u32::MAX);
        let b = f.enqueue(k1, 0b10);
        let c = f.enqueue(k1, u32::MAX);
        let d = f.enqueue(k2, u32::MAX);
        assert_eq!(f.wake(k1, 1, 0b01), 1);
        assert!(!f.is_queued(a) && f.is_queued(b));
        assert_eq!(f.requeue(k1, 1, k2, 5), 2);
        assert!(!f.is_queued(b) && f.is_queued(c));
        assert_eq!(f.wake(k2, 10, u32::MAX), 2);
        assert!(!f.is_queued(c) && !f.is_queued(d));
    }
}
