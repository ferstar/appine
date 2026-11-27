//! Sharded futex wait queues (the kernel/futex map). Keys follow Linux's `get_futex_key`: private
//! memory is keyed by address space and address, so a waiter is still found after the memory at
//! its address has been replaced (a `MAP_FIXED` mmap, or munmap and mmap again); shared anonymous
//! memory (shared mappings, System V shared memory) by its host address, so processes that share
//! one piece of host memory after fork can wake each other; shared file mappings by host file and
//! offset, because processes that each map the same file (the `/dev/shm` file of `sem_open`) see
//! different host addresses. With `FUTEX_PRIVATE_FLAG` every futex is keyed as private memory.
//!
//! Waiters are hashed across [`BUCKETS`] cache-line-aligned buckets so operations on distinct
//! keys never contend. Multi-bucket operations (`requeue`, `wake_op`, `futex_waitv`) lock buckets
//! in strictly ascending bucket-index order to prevent deadlock, and lock order is always BKL then
//! bucket lock (never the reverse). Each waiter carries a shared [`WaiterSlot`] whose `woken` flag
//! lets a woken thread skip re-locking its bucket in [`Enqueued::leave`] and whose `bucket` tracks
//! moves by `FUTEX_REQUEUE`.
//!
//! A wake notifies only the threads it wakes (their events were recorded when they registered),
//! and futex waiters listen on no wake channel or object ([`crate::kernel::Cx::wait_alone`]), so
//! no change elsewhere wakes them: the JVM and the Go runtime have many threads parked on locks
//! and condition variables, and waking all of them on every change elsewhere would be costly.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use appine_engine::prot;
use appine_hal::Event;

use crate::kernel::{FutexesRef, Shared, Tid};
use crate::mm::Mm;
use crate::uaccess::UserSlice;

/// Number of sharded futex wait-queue buckets (power of two).
const BUCKETS: usize = 256;

/// A futex key ([`crate::mm::Mm::futex_key`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    /// Private memory, or any memory with `FUTEX_PRIVATE_FLAG`: the address space
    /// ([`crate::mm::Mm::id`]) and the address. `shared` is an operation without the flag
    /// (Linux's `FUT_OFF_MMSHARED`): private and shared operations on one address never meet.
    Private { mm: u64, addr: u64, shared: bool },
    /// Shared anonymous memory: its host address.
    Host(usize),
    /// Offset in a host file (device number, inode).
    File { dev: u64, ino: u64, offset: u64 },
}

/// Fast hash mapping a [`Key`] to one of [`BUCKETS`].
fn bucket_index(key: Key) -> usize {
    let h = match key {
        Key::Private { mm, addr, shared } => {
            mm.wrapping_mul(0x9e37_79b9_7f4a_7c15)
                ^ (addr >> 2).wrapping_mul(0xbf58_476d_1ce4_e5b9)
                ^ u64::from(shared)
        }
        Key::Host(addr) => ((addr as u64) >> 2).wrapping_mul(0xbf58_476d_1ce4_e5b9),
        Key::File { dev, ino, offset } => {
            dev.wrapping_mul(0x9e37_79b9_7f4a_7c15)
                ^ ino.wrapping_mul(0xbf58_476d_1ce4_e5b9)
                ^ (offset >> 2).wrapping_mul(0x94d0_49bb_1331_11eb)
        }
    };
    let mixed = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    ((mixed ^ (mixed >> 27)) as usize) & (BUCKETS - 1)
}

/// Shared state between a queued [`Entry`] and the waiting thread's [`Waiter`] handle.
struct WaiterSlot {
    /// Set to `true` (Release) while holding the entry's bucket lock as the waker dequeues it,
    /// allowing [`Enqueued::leave`] to return without re-locking any bucket.
    woken: AtomicBool,
    /// Current bucket index holding this waiter's entry; updated under both source and
    /// destination bucket locks during `FUTEX_REQUEUE`.
    bucket: AtomicUsize,
}

/// Handle to one registered futex waiter.
#[derive(Clone)]
pub(crate) struct Waiter {
    id: u64,
    slot: Arc<WaiterSlot>,
}

impl Waiter {
    /// Lock-free check whether a waker has already dequeued this waiter.
    #[inline]
    pub fn is_woken(&self) -> bool {
        self.slot.woken.load(Ordering::Acquire)
    }
}

struct Entry {
    id: u64,
    key: Key,
    bitset: u32,
    /// The waiting thread's event ([`crate::task::Task::event`]).
    event: Arc<Event>,
    /// Shared woken/bucket state with the waiting thread's [`Waiter`].
    slot: Arc<WaiterSlot>,
    /// A `FUTEX_LOCK_PI` waiter: its thread, which the lock is handed to
    /// ([`BucketGuard::first_pi`]). Plain wakes and requeues leave such waiters alone.
    pi: Option<PiWaiter>,
}

impl Entry {
    /// Marks the waiter woken (Release) before scheduling its event wake-up so the woken thread
    /// observes `is_woken() == true` without re-locking its bucket.
    fn wake(&self) {
        self.slot.woken.store(true, Ordering::Release);
        crate::kernel::signal_later(&self.event);
    }
}

/// A thread blocked in `FUTEX_LOCK_PI`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PiWaiter {
    /// Its global tid.
    pub tid: Tid,
    /// The address of the lock word in its own address space.
    pub addr: u64,
}

#[repr(align(64))]
struct Bucket {
    entries: Mutex<Vec<Entry>>,
    pi_count: AtomicUsize,
}

impl Bucket {
    fn new() -> Self {
        Bucket {
            entries: Mutex::new(Vec::new()),
            pi_count: AtomicUsize::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Guard over a single locked bucket for compound PI futex operations (`futex_lock_pi`,
/// `futex_unlock_pi`).
pub(crate) struct BucketGuard<'a> {
    futexes: &'a Futexes,
    index: usize,
    entries: MutexGuard<'a, Vec<Entry>>,
}

impl BucketGuard<'_> {
    /// The first `FUTEX_LOCK_PI` waiter on `key` (Linux's top waiter; without priorities, the one
    /// that came first): its id and thread.
    pub fn first_pi(&self, key: Key) -> Option<(u64, PiWaiter)> {
        self.entries
            .iter()
            .find(|e| e.key == key && e.pi.is_some())
            .and_then(|e| e.pi.map(|w| (e.id, w)))
    }

    /// Whether `FUTEX_LOCK_PI` waiters other than `except` are queued on `key`.
    pub fn has_pi(&self, key: Key, except: Option<u64>) -> bool {
        self.entries
            .iter()
            .any(|e| e.key == key && e.pi.is_some() && Some(e.id) != except)
    }

    /// Registers a `FUTEX_LOCK_PI` waiter on `key`, which gets the lock when it is woken
    /// ([`BucketGuard::first_pi`], [`BucketGuard::hand_over`]).
    pub fn enqueue_pi(&mut self, key: Key, event: Arc<Event>, waiter: PiWaiter) -> Waiter {
        debug_assert_eq!(bucket_index(key), self.index);
        self.futexes.buckets[self.index]
            .pi_count
            .fetch_add(1, Ordering::Relaxed);
        self.futexes.pi_count.fetch_add(1, Ordering::Release);
        self.futexes.push_locked(
            &mut self.entries,
            self.index,
            key,
            u32::MAX,
            event,
            Some(waiter),
        )
    }

    /// Wakes PI waiter `id`, the new owner of the lock it waited for.
    pub fn hand_over(&mut self, id: u64) {
        if let Some(i) = self.entries.iter().position(|e| e.id == id) {
            let e = self.entries.remove(i);
            if e.pi.is_some() {
                self.futexes.dec_pi_count(self.index);
            }
            e.wake();
        }
    }

    /// The first `FUTEX_LOCK_PI` waiter for each key present in this bucket.
    fn pi_heads(&self) -> Vec<(u64, PiWaiter)> {
        let mut keys = Vec::new();
        let mut heads = Vec::new();
        for e in self.entries.iter() {
            if let Some(w) = e.pi
                && !keys.contains(&e.key)
            {
                keys.push(e.key);
                heads.push((e.id, w));
            }
        }
        heads
    }
}

/// One or two bucket locks acquired in canonical ascending index order (`first` corresponds to
/// `key`, `second` to `key2`).
enum TwoBuckets<'a> {
    Same(MutexGuard<'a, Vec<Entry>>),
    Distinct(MutexGuard<'a, Vec<Entry>>, MutexGuard<'a, Vec<Entry>>),
}

impl TwoBuckets<'_> {
    fn first_mut(&mut self) -> &mut Vec<Entry> {
        match self {
            Self::Same(g) | Self::Distinct(g, _) => g,
        }
    }

    fn second_mut(&mut self) -> &mut Vec<Entry> {
        match self {
            Self::Same(g) | Self::Distinct(_, g) => g,
        }
    }
}

/// All futex waiters, sharded across [`BUCKETS`] hash buckets (FIFO per key).
pub(crate) struct Futexes {
    next_id: AtomicU64,
    /// Total queued `FUTEX_LOCK_PI` waiters across all buckets so [`pi_owners_exited`] skips
    /// scanning buckets when none exist.
    pi_count: AtomicUsize,
    buckets: Box<[Bucket; BUCKETS]>,
}

impl Default for Futexes {
    fn default() -> Self {
        Self::new()
    }
}

impl Futexes {
    pub fn new() -> Self {
        let vec: Vec<Bucket> = (0..BUCKETS).map(|_| Bucket::new()).collect();
        let buckets: Box<[Bucket; BUCKETS]> = vec
            .into_boxed_slice()
            .try_into()
            .ok()
            .expect("BUCKETS entries");
        Futexes {
            next_id: AtomicU64::new(0),
            pi_count: AtomicUsize::new(0),
            buckets,
        }
    }

    fn push_locked(
        &self,
        entries: &mut Vec<Entry>,
        bucket: usize,
        key: Key,
        bitset: u32,
        event: Arc<Event>,
        pi: Option<PiWaiter>,
    ) -> Waiter {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let slot = Arc::new(WaiterSlot {
            woken: AtomicBool::new(false),
            bucket: AtomicUsize::new(bucket),
        });
        entries.push(Entry {
            id,
            key,
            bitset,
            event,
            slot: slot.clone(),
            pi,
        });
        Waiter { id, slot }
    }

    fn dec_pi_count(&self, bucket: usize) {
        self.buckets[bucket]
            .pi_count
            .fetch_sub(1, Ordering::Relaxed);
        self.pi_count.fetch_sub(1, Ordering::Release);
    }

    fn lock_bucket(&self, index: usize) -> BucketGuard<'_> {
        BucketGuard {
            futexes: self,
            index,
            entries: self.buckets[index].lock(),
        }
    }

    /// Locks buckets `b1` and `b2` in strictly ascending index order (`min` then `max`) to prevent
    /// deadlock, returning handles ordered as `(b1, b2)`.
    fn lock_two_buckets(&self, b1: usize, b2: usize) -> TwoBuckets<'_> {
        if b1 == b2 {
            TwoBuckets::Same(self.buckets[b1].lock())
        } else if b1 < b2 {
            let g1 = self.buckets[b1].lock();
            let g2 = self.buckets[b2].lock();
            TwoBuckets::Distinct(g1, g2)
        } else {
            let g2 = self.buckets[b2].lock();
            let g1 = self.buckets[b1].lock();
            TwoBuckets::Distinct(g1, g2)
        }
    }

    /// Locks the bucket currently holding `waiter` (retrying if a concurrent `requeue` moves it),
    /// returning `None` immediately if a waker has already marked `waiter` woken.
    fn lock_waiter_bucket(&self, waiter: &Waiter) -> Option<(usize, MutexGuard<'_, Vec<Entry>>)> {
        loop {
            if waiter.is_woken() {
                return None;
            }
            let b = waiter.slot.bucket.load(Ordering::Acquire);
            let entries = self.buckets[b].lock();
            if waiter.is_woken() {
                return None;
            }
            // A concurrent `requeue` updates `slot.bucket` while holding both old and new bucket
            // locks, so observing `slot.bucket == b` under `buckets[b]` guarantees `waiter` cannot
            // move until this lock is released.
            if waiter.slot.bucket.load(Ordering::Acquire) != b {
                continue;
            }
            return Some((b, entries));
        }
    }

    /// Runs `f` while holding the bucket lock for `key`.
    pub fn with_bucket<R>(&self, key: Key, f: impl FnOnce(&mut BucketGuard<'_>) -> R) -> R {
        let mut guard = self.lock_bucket(bucket_index(key));
        f(&mut guard)
    }

    /// Registers a waiter and returns its handle. `event` is the event to signal when it is
    /// woken; the waiter must wait with [`crate::kernel::Cx::wait_alone`] or fast sleep.
    #[cfg(test)]
    pub fn enqueue(&self, key: Key, bitset: u32, event: Arc<Event>) -> Waiter {
        let b = bucket_index(key);
        let mut entries = self.buckets[b].lock();
        self.push_locked(&mut entries, b, key, bitset, event, None)
    }

    /// Locks `key`'s bucket, runs `check()`, and enqueues a waiter iff `check()` returns
    /// `Ok(true)`. Keeps the value check and enqueue atomic against concurrent wakes.
    pub fn enqueue_if<E>(
        &self,
        key: Key,
        bitset: u32,
        event: Arc<Event>,
        check: impl FnOnce() -> Result<bool, E>,
    ) -> Result<Option<Waiter>, E> {
        let b = bucket_index(key);
        let mut entries = self.buckets[b].lock();
        if !check()? {
            return Ok(None);
        }
        Ok(Some(self.push_locked(
            &mut entries,
            b,
            key,
            bitset,
            event,
            None,
        )))
    }

    /// Locks every unique bucket needed by `keys` in strictly ascending index order, runs
    /// `check()`, and enqueues all waiters atomically iff `check()` returns `Ok(true)`.
    pub fn enqueue_many_if<E>(
        &self,
        keys: &[Key],
        event: &Arc<Event>,
        check: impl FnOnce() -> Result<bool, E>,
    ) -> Result<Option<Vec<Waiter>>, E> {
        let mut indices: Vec<usize> = keys.iter().map(|&k| bucket_index(k)).collect();
        indices.sort_unstable();
        indices.dedup();
        let mut guards: Vec<(usize, MutexGuard<'_, Vec<Entry>>)> = indices
            .into_iter()
            .map(|b| (b, self.buckets[b].lock()))
            .collect();
        if !check()? {
            return Ok(None);
        }
        let mut waiters = Vec::with_capacity(keys.len());
        for &key in keys {
            let b = bucket_index(key);
            let pos = guards
                .binary_search_by_key(&b, |&(idx, _)| idx)
                .expect("bucket locked");
            let w = self.push_locked(&mut guards[pos].1, b, key, u32::MAX, event.clone(), None);
            waiters.push(w);
        }
        Ok(Some(waiters))
    }

    /// Wakes at most `n` waiters on `key` with an intersecting bitset, and returns how many were
    /// woken.
    pub fn wake(&self, key: Key, n: u32, bitset: u32) -> u32 {
        let b = bucket_index(key);
        let mut entries = self.buckets[b].lock();
        wake_in_bucket(&mut entries, key, n, bitset)
    }

    /// FUTEX_REQUEUE: wakes at most `n_wake` waiters on `key`, then moves at most `n_requeue`
    /// more to `key2`. Returns the total woken and moved.
    #[cfg(test)]
    pub fn requeue(&self, key: Key, n_wake: u32, key2: Key, n_requeue: u32) -> u32 {
        self.requeue_if::<()>(key, n_wake, key2, n_requeue, || Ok(true))
            .unwrap()
            .unwrap()
    }

    /// Locks the buckets for `key` and `key2` in canonical ascending order, runs `check()`, and
    /// if `Ok(true)` wakes up to `n_wake` waiters on `key` and moves up to `n_requeue` more to
    /// `key2`. Returns `Ok(None)` when `check()` returns `Ok(false)`.
    pub fn requeue_if<E>(
        &self,
        key: Key,
        n_wake: u32,
        key2: Key,
        n_requeue: u32,
        check: impl FnOnce() -> Result<bool, E>,
    ) -> Result<Option<u32>, E> {
        let (b1, b2) = (bucket_index(key), bucket_index(key2));
        let mut buckets = self.lock_two_buckets(b1, b2);
        if !check()? {
            return Ok(None);
        }
        let woken = wake_in_bucket(buckets.first_mut(), key, n_wake, u32::MAX);
        let n_moved = match &mut buckets {
            TwoBuckets::Same(entries) => {
                // Append moved entries to the tail to preserve FIFO order behind existing `key2`
                // waiters in the same bucket.
                let mut moved = Vec::new();
                let n = move_entries(entries, &mut moved, key, key2, None, n_requeue);
                entries.extend(moved);
                n
            }
            TwoBuckets::Distinct(src, dst) => {
                move_entries(src, dst, key, key2, Some(b2), n_requeue)
            }
        };
        Ok(Some(woken + n_moved))
    }

    /// `FUTEX_WAKE_OP`: locks `key` and `key2`'s buckets in canonical order, runs `op()` (which
    /// performs the atomic word update and returns whether the comparison matched), wakes up to
    /// `n1` waiters on `key`, and if `op()` returned `Ok(true)`, wakes up to `n2` on `key2`.
    pub fn wake_op<E>(
        &self,
        key: Key,
        n1: u32,
        key2: Key,
        n2: u32,
        op: impl FnOnce() -> Result<bool, E>,
    ) -> Result<u32, E> {
        let mut buckets = self.lock_two_buckets(bucket_index(key), bucket_index(key2));
        let matched = op()?;
        let mut woken = wake_in_bucket(buckets.first_mut(), key, n1, u32::MAX);
        if matched {
            woken += wake_in_bucket(buckets.second_mut(), key2, n2, u32::MAX);
        }
        Ok(woken)
    }

    /// Whether `waiter` is still queued (if not, it has been woken or removed).
    #[cfg(test)]
    pub fn is_queued(&self, waiter: &Waiter) -> bool {
        self.lock_waiter_bucket(waiter)
            .is_some_and(|(_, entries)| entries.iter().any(|e| e.id == waiter.id))
    }

    /// Removes a waiter (timed out, interrupted by a signal, or unwinding). Returns `true` if a
    /// waker had already dequeued this waiter (so the wake is not lost), or `false` if this call
    /// removed it from its bucket.
    pub fn remove(&self, waiter: &Waiter) -> bool {
        let Some((b, mut entries)) = self.lock_waiter_bucket(waiter) else {
            return true;
        };
        if let Some(pos) = entries.iter().position(|e| e.id == waiter.id) {
            let e = entries.remove(pos);
            if e.pi.is_some() {
                self.dec_pi_count(b);
            }
            return false;
        }
        true
    }
}

fn wake_in_bucket(entries: &mut Vec<Entry>, key: Key, n: u32, bitset: u32) -> u32 {
    let mut woken = 0;
    entries.retain(|e| {
        if woken < n && e.key == key && e.bitset & bitset != 0 && e.pi.is_none() {
            woken += 1;
            e.wake();
            false
        } else {
            true
        }
    });
    woken
}

/// Moves up to `limit` non-PI waiters on `from` out of `src` and into `dst` under key `to`,
/// updating `WaiterSlot::bucket` when migrating across distinct buckets (`dst_bucket.is_some()`).
fn move_entries(
    src: &mut Vec<Entry>,
    dst: &mut Vec<Entry>,
    from: Key,
    to: Key,
    dst_bucket: Option<usize>,
    limit: u32,
) -> u32 {
    let mut moved = 0u32;
    let mut i = 0;
    while i < src.len() && moved < limit {
        if src[i].key == from && src[i].pi.is_none() {
            let mut e = src.remove(i);
            e.key = to;
            if let Some(b) = dst_bucket {
                e.slot.bucket.store(b, Ordering::Release);
            }
            dst.push(e);
            moved += 1;
        } else {
            i += 1;
        }
    }
    moved
}

/// Waiters registered in the queue ([`Futexes::enqueue_if`]); when done waiting they leave with
/// [`Enqueued::leave`]. On a panic midway (a kernel-layer bug, [`crate::bug`]) the destructor
/// removes those still queued: a leftover entry has nobody waiting on it, a later FUTEX_WAKE
/// would count against it, and the real waiter would not be woken; when the key is a host
/// address, that memory may later be given to another process.
pub(crate) struct Enqueued<'s, I: AsRef<[Waiter]>> {
    shared: &'s Shared,
    waiters: I,
    left: bool,
}

impl<'s, I: AsRef<[Waiter]>> Enqueued<'s, I> {
    pub fn new(shared: &'s Shared, waiters: I) -> Self {
        Enqueued {
            shared,
            waiters,
            left: false,
        }
    }

    /// Lock-free check returning the index of the first waiter already woken, if any.
    #[inline]
    pub fn woken_index(&self) -> Option<usize> {
        self.waiters.as_ref().iter().position(Waiter::is_woken)
    }

    /// Leaves the queue: skips locking any bucket for waiters already marked woken, removes those
    /// still queued, and returns the index of the first one woken. A wake that coincides with a
    /// timeout or a signal interruption counts as a wake, as in Linux's `futex_unqueue`, so that
    /// wake is not lost.
    pub fn leave(mut self) -> Option<usize> {
        self.left = true;
        let waiters = self.waiters.as_ref();
        // Fast path for the common single-futex wait: when woken, zero bucket locks are taken.
        if let [only] = waiters
            && only.is_woken()
        {
            return Some(0);
        }
        let futexes = self.shared.futexes();
        let mut woken = None;
        for (i, w) in waiters.iter().enumerate() {
            let was_woken = w.is_woken() || futexes.remove(w);
            if was_woken && woken.is_none() {
                woken = Some(i);
            }
        }
        woken
    }
}

impl<I: AsRef<[Waiter]>> Drop for Enqueued<'_, I> {
    fn drop(&mut self) {
        if !self.left {
            let futexes = self.shared.futexes();
            for w in self.waiters.as_ref() {
                if !w.is_woken() {
                    futexes.remove(w);
                }
            }
        }
    }
}

pub(crate) const FUTEX_WAITERS: u32 = 0x8000_0000;
pub(crate) const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
pub(crate) const FUTEX_TID_MASK: u32 = 0x3fff_ffff;
/// The most list entries processed (`ROBUST_LIST_LIMIT`), against a cycle built by the guest.
const ROBUST_LIST_LIMIT: usize = 2048;

/// Processes a thread's robust futex list (set_robust_list) when it exits or calls execve, the
/// same as Linux's `exit_robust_list`: locks it still holds are marked `FUTEX_OWNER_DIED`, and if
/// there are waiters one is woken, which thereby gets EOWNERDEAD. `head` is the address of the
/// `struct robust_list_head`.
#[allow(clippy::needless_pass_by_ref_mut)] // preserves caller signature in task.rs
pub(crate) fn exit_robust_list(futexes: &mut FutexesRef<'_>, mm: &Mm, head: u64, tid: u32) {
    if head == 0 {
        return;
    }
    let word = |addr: u64| {
        let mut b = [0u8; 8];
        mm.read(UserSlice::new(addr, 8), &mut b)
            .ok()
            .map(|()| u64::from_le_bytes(b))
    };
    // The lowest bit of a list entry marks a PI futex
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
        // The entry being locked or unlocked is handled separately at the end
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

/// Linux's `handle_futex_death`: if the lock at `addr` is held by `tid`, marks it
/// `FUTEX_OWNER_DIED` (keeping `FUTEX_WAITERS`), and wakes one waiter if there are any. Other
/// threads may be changing it with atomic operations in guest code, so a compare-and-swap is used.
fn owner_died(futexes: &Futexes, mm: &Mm, addr: u64, tid: u32, pi: bool, pending: bool) {
    if !addr.is_multiple_of(4) {
        return;
    }
    let Some(lock) = writable_word(mm, addr) else {
        return;
    };
    let mut cur = lock.load(Ordering::SeqCst);
    // As Linux's handle_futex_death, a shared wake (glibc's robust mutexes use shared operations
    // for this reason)
    let wake = |futexes: &Futexes| {
        if let Some(key) = mm.futex_key(addr, false) {
            futexes.wake(key, 1, u32::MAX);
        }
    };
    // Exited after unlocking but before waking the waiters: the lock is already 0, so wake one
    // directly, lest they wait forever
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

/// A thread has exited or called `execve`: each PI lock with waiters whose owner no longer exists
/// (or matches `released_tid`, the thread leaving its address space in `execve`) goes to its first
/// waiter (Linux's `exit_pi_state_list` handing the rt_mutex over, and the new owner's
/// `fixup_pi_state_owner`). The word becomes the waiter's tid with `FUTEX_WAITERS` and
/// `FUTEX_OWNER_DIED`, whether or not the lock was on the owner's robust list.
#[allow(clippy::needless_pass_by_ref_mut)] // preserves caller signature in task.rs and syscall/proc.rs
pub(crate) fn pi_owners_exited(
    k: &crate::kernel::Kernel,
    futexes: &mut FutexesRef<'_>,
    released_tid: Option<Tid>,
) {
    if futexes.pi_count.load(Ordering::Acquire) == 0 {
        return;
    }
    for (index, bucket) in futexes.buckets.iter().enumerate() {
        if bucket.pi_count.load(Ordering::Acquire) == 0 {
            continue;
        }
        let mut guard = futexes.lock_bucket(index);
        for (id, w) in guard.pi_heads() {
            let Some(task) = k.tasks.get(&w.tid) else {
                continue;
            };
            let ns = k.pid_ns_of(task.tgid);
            let Ok(res) = task.res.try_borrow() else {
                continue;
            };
            let Ok(mm) = res.mm.try_borrow() else {
                continue;
            };
            let (Some(word), Some(me)) = (writable_word(&mm, w.addr), ns.vnr(w.tid)) else {
                continue;
            };
            let cur = word.load(Ordering::SeqCst);
            let owner = cur & FUTEX_TID_MASK;
            if owner != 0
                && ns
                    .find(owner as Tid)
                    .is_some_and(|g| Some(g) != released_tid && k.tasks.contains_key(&g))
            {
                continue;
            }
            let new = FUTEX_OWNER_DIED | me as u32 | FUTEX_WAITERS;
            if word
                .compare_exchange(cur, new, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                guard.hand_over(id);
            }
        }
    }
}

/// The futex word at `addr` (4-byte aligned) for the kernel to change atomically while guest
/// threads may be changing it too, or None when it cannot be written. A copy-on-write page not
/// written since fork, or one without an entry yet, is made writable first, as for any kernel
/// write (`Mm::write`), and as Linux faults it in.
pub(crate) fn writable_word(mm: &Mm, addr: u64) -> Option<&AtomicU32> {
    let need = prot::READ | prot::WRITE;
    let host = match mm.translate(addr, need) {
        Some(host) => host,
        None if mm.cow_break(addr) || mm.fault_in(addr, need) => mm.translate(addr, need)?,
        None => return None,
    };
    // SAFETY: 4-byte-aligned, writable guest memory; the host memory is held by the address space,
    // which the caller borrows for as long as it uses the word
    Some(unsafe { AtomicU32::from_ptr(host.as_ptr().cast()) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::time::Duration;

    fn event() -> Arc<Event> {
        Arc::new(Event::new().unwrap())
    }

    fn signaled(e: &Event) -> bool {
        crate::kernel::flush_signals();
        let mut p = [(e.fd(), 1, 0)];
        appine_hal::poll(&mut p, Some(Duration::ZERO)).unwrap();
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
        let f = Futexes::default();
        let (ea, eb) = (event(), event());
        let a = f.enqueue(k1, u32::MAX, ea.clone());
        let b = f.enqueue(k1, 0b10, eb.clone());
        let c = f.enqueue(k1, u32::MAX, event());
        let d = f.enqueue(k2, u32::MAX, event());
        assert_eq!(f.wake(k1, 1, 0b01), 1);
        assert!(a.is_woken() && !f.is_queued(&a) && f.is_queued(&b));
        // Only the woken thread is notified
        assert!(signaled(&ea) && !signaled(&eb));
        assert_eq!(f.requeue(k1, 1, k2, 5), 2);
        assert!(b.is_woken() && !f.is_queued(&b) && f.is_queued(&c));
        assert_eq!(f.wake(k2, 10, u32::MAX), 2);
        assert!(c.is_woken() && d.is_woken() && !f.is_queued(&c) && !f.is_queued(&d));
    }

    #[test]
    fn requeue_same_and_distinct_buckets_updates_waiter_removal() {
        let f = Futexes::default();
        let k1 = Key::Private {
            mm: 1,
            addr: 0x1000,
            shared: false,
        };
        let b1 = bucket_index(k1);
        // Find a key in the same bucket and a key in a different bucket.
        let mut same = None;
        let mut diff = None;
        for step in 1u64..10_000 {
            let k = Key::Private {
                mm: 1,
                addr: 0x1000 + step * 4,
                shared: false,
            };
            let b = bucket_index(k);
            if b == b1 && same.is_none() {
                same = Some(k);
            } else if b != b1 && diff.is_none() {
                diff = Some(k);
            }
            if same.is_some() && diff.is_some() {
                break;
            }
        }
        let (k_same, k_diff) = (same.unwrap(), diff.unwrap());

        // Requeue to a different bucket, then remove (simulating timeout/signal after requeue).
        let w1 = f.enqueue(k1, u32::MAX, event());
        assert_eq!(f.requeue(k1, 0, k_diff, 1), 1);
        assert!(f.is_queued(&w1));
        assert!(!f.remove(&w1));
        assert!(!f.is_queued(&w1));
        assert_eq!(f.wake(k_diff, 1, u32::MAX), 0);

        // Requeue within the same bucket, then wake on destination key.
        let w2 = f.enqueue(k1, u32::MAX, event());
        assert_eq!(f.requeue(k1, 0, k_same, 1), 1);
        assert_eq!(f.wake(k1, 1, u32::MAX), 0);
        assert_eq!(f.wake(k_same, 1, u32::MAX), 1);
        assert!(w2.is_woken());
        crate::kernel::flush_signals();
    }

    #[test]
    fn concurrent_contention_requeue_and_timeout_stress() {
        let shared = Arc::new(Shared::new(crate::kernel::Kernel::new(
            crate::process::Tier::Interpreter,
        )));
        let keys: Vec<Key> = (0..16u64)
            .map(|i| Key::Private {
                mm: 1,
                addr: 0x10000 + i * 4,
                shared: false,
            })
            .collect();
        let rounds = 200;
        let n_waiters = 8;
        let barrier = Arc::new(Barrier::new(n_waiters + 2));

        std::thread::scope(|s| {
            // Waiter threads repeatedly enqueue, wait with short timeouts or wakes, and leave.
            for t in 0..n_waiters {
                let shared = shared.clone();
                let keys = &keys;
                let barrier = barrier.clone();
                s.spawn(move || {
                    let ev = event();
                    barrier.wait();
                    for r in 0..rounds {
                        let key = keys[(t + r) % keys.len()];
                        let w = shared.futexes().enqueue(key, u32::MAX, ev.clone());
                        let enq = Enqueued::new(&shared, [w]);
                        if enq.woken_index().is_none() {
                            let _ = ev.wait(Some(Duration::from_micros(50)));
                            ev.clear();
                        }
                        let _ = enq.leave();
                    }
                });
            }

            // Requeue thread moving waiters in both directions across buckets (tests lock order).
            {
                let shared = shared.clone();
                let keys = &keys;
                let barrier = barrier.clone();
                s.spawn(move || {
                    barrier.wait();
                    for r in 0..rounds * 2 {
                        let k1 = keys[r % keys.len()];
                        let k2 = keys[(r * 7 + 3) % keys.len()];
                        let _ = shared.futexes().requeue(k1, 1, k2, 2);
                        crate::kernel::flush_signals();
                        let _ = shared.futexes().requeue(k2, 1, k1, 2);
                        crate::kernel::flush_signals();
                    }
                });
            }

            // Waker thread continuously waking across all keys.
            {
                let shared = shared.clone();
                let keys = &keys;
                let barrier = barrier.clone();
                s.spawn(move || {
                    barrier.wait();
                    for r in 0..rounds * 4 {
                        let key = keys[r % keys.len()];
                        let _ = shared.futexes().wake(key, 2, u32::MAX);
                        crate::kernel::flush_signals();
                    }
                });
            }
        });

        // After all threads finish, every bucket must be completely empty (zero leaked entries).
        let futexes = shared.futexes();
        for b in futexes.buckets.iter() {
            assert!(b.lock().is_empty());
        }
    }
}
