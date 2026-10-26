//! futex 的等待队列（设计文档 §6）。键是翻译后的宿主地址，所以 fork 之后共用同一段
//! 宿主内存的进程能互相唤醒；文件的共享映射以宿主文件和偏移为键，因为各个进程分别
//! 映射同一个文件（`sem_open` 的 `/dev/shm` 文件）时宿主地址各不相同。
//!
//! 等待者登记在队列里，然后等待；被唤醒的等待者从队列里移除，它重新检查时发现自己
//! 不在队列里了，就知道是被 FUTEX_WAKE 唤醒的。唤醒只通知被唤醒的那几个线程（登记时
//! 记下了它们的事件），不像别的等待那样叫醒所有在等的线程：JVM、Go 运行时里停在锁和
//! 条件变量上的线程很多，别处的每次修改都把它们全部叫醒一遍，代价很大。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use appine_engine::prot;
use appine_hal::Event;

use crate::mm::Mm;
use crate::uaccess::UserSlice;

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
    /// 等待线程的事件（[`crate::task::Task::event`]）。
    event: Arc<Event>,
}

/// 所有 futex 等待者，按登记顺序排列（唤醒时先到先得）。
#[derive(Default)]
pub(crate) struct Futexes {
    next_id: u64,
    waiting: Vec<Entry>,
}

impl Futexes {
    /// 登记一个等待者，返回它的编号。`event` 是唤醒它时要通知的事件；等待者要用
    /// [`crate::kernel::Cx::wait_alone`] 等待。
    pub fn enqueue(&mut self, key: Key, bitset: u32, event: Arc<Event>) -> u64 {
        self.next_id += 1;
        self.waiting.push(Entry {
            id: self.next_id,
            key,
            bitset,
            event,
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
                crate::kernel::signal_later(&e.event);
                false
            } else {
                true
            }
        });
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

const FUTEX_WAITERS: u32 = 0x8000_0000;
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
const FUTEX_TID_MASK: u32 = 0x3fff_ffff;
/// 最多处理的链表项（`ROBUST_LIST_LIMIT`），防止 guest 构造出环。
const ROBUST_LIST_LIMIT: usize = 2048;

/// 线程退出或 execve 时处理它的 robust futex 链表（set_robust_list），与 Linux 的
/// `exit_robust_list` 相同：它还持有的锁标上 `FUTEX_OWNER_DIED`，有等待者时唤醒一个，
/// 等待者因此拿到 EOWNERDEAD。`head` 是 `struct robust_list_head` 的地址。
pub(crate) fn exit_robust_list(futexes: &mut Futexes, mm: &Mm, head: u64, tid: u32) {
    if head == 0 {
        return;
    }
    let word = |addr: u64| {
        let mut b = [0u8; 8];
        mm.read(UserSlice::new(addr, 8), &mut b)
            .ok()
            .map(|()| u64::from_le_bytes(b))
    };
    // 链表项的最低位表示 PI futex
    let (Some(mut entry), Some(offset), Some(pending)) =
        (word(head), word(head + 8), word(head + 16))
    else {
        return;
    };
    for _ in 0..ROBUST_LIST_LIMIT {
        if entry & !1 == head {
            break;
        }
        let Some(next) = word(entry & !1) else {
            return;
        };
        // 正在加锁或解锁的那一项最后单独处理
        if entry & !1 != pending & !1 {
            owner_died(
                futexes,
                mm,
                (entry & !1).wrapping_add(offset),
                tid,
                entry & 1 != 0,
                false,
            );
        }
        entry = next;
    }
    if pending & !1 != 0 {
        let addr = (pending & !1).wrapping_add(offset);
        owner_died(futexes, mm, addr, tid, pending & 1 != 0, true);
    }
}

/// Linux 的 `handle_futex_death`：`addr` 处的锁由 `tid` 持有时标上 `FUTEX_OWNER_DIED`
/// （保留 `FUTEX_WAITERS`），有等待者就唤醒一个。别的线程可能正在 guest 代码里用原子
/// 操作改它，所以用比较交换。
fn owner_died(futexes: &mut Futexes, mm: &Mm, addr: u64, tid: u32, pi: bool, pending: bool) {
    if !addr.is_multiple_of(4) {
        return;
    }
    let Some(host) = mm.mmu().translate(addr, prot::READ | prot::WRITE) else {
        return;
    };
    // SAFETY: 4 字节对齐、可写的 guest 内存，宿主内存由地址空间持有
    let lock = unsafe { AtomicU32::from_ptr(host.as_ptr().cast()) };
    let mut cur = lock.load(Ordering::SeqCst);
    let wake = |futexes: &mut Futexes| {
        if let Some(key) = mm.futex_key(addr) {
            futexes.wake(key, 1, u32::MAX);
        }
    };
    // 解锁之后、唤醒等待者之前退出的：锁已经是 0，直接唤醒一个，免得它们永远等下去
    if pending && !pi && cur == 0 {
        wake(futexes);
        return;
    }
    loop {
        if cur & FUTEX_TID_MASK != tid {
            return;
        }
        let new = (cur & FUTEX_WAITERS) | FUTEX_OWNER_DIED;
        match lock.compare_exchange(cur, new, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break,
            Err(v) => cur = v,
        }
    }
    if !pi && cur & FUTEX_WAITERS != 0 {
        wake(futexes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> Arc<Event> {
        Arc::new(Event::new().unwrap())
    }

    fn signaled(e: &Event) -> bool {
        crate::kernel::flush_signals();
        let mut p = [(e.fd(), 1, 0)];
        appine_hal::poll(&mut p, Some(std::time::Duration::ZERO)).unwrap();
        p[0].2 & 1 != 0
    }

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
        let (ea, eb) = (event(), event());
        let a = f.enqueue(k1, u32::MAX, ea.clone());
        let b = f.enqueue(k1, 0b10, eb.clone());
        let c = f.enqueue(k1, u32::MAX, event());
        let d = f.enqueue(k2, u32::MAX, event());
        assert_eq!(f.wake(k1, 1, 0b01), 1);
        assert!(!f.is_queued(a) && f.is_queued(b));
        // 只通知被唤醒的那个线程
        assert!(signaled(&ea) && !signaled(&eb));
        assert_eq!(f.requeue(k1, 1, k2, 5), 2);
        assert!(!f.is_queued(b) && f.is_queued(c));
        assert_eq!(f.wake(k2, 10, u32::MAX), 2);
        assert!(!f.is_queued(c) && !f.is_queued(d));
    }
}
