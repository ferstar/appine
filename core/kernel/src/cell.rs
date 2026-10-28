//! 内核对象用的 `RefCell`（设计文档 §6）：与标准库的相同，调试构建和打开 `borrow-check`
//! 特性的构建里另外数着每个线程借着几个。
//!
//! 内核对象由 BKL 保护，放开 BKL（等待、[`Cx::unlocked`](crate::kernel::Cx::unlocked)）
//! 时不能还借着它们：别的线程这时借同一个对象会 panic（already borrowed），它所在的进程
//! 就被当作撞上内核 bug 结束。经由 `cx` 拿到的借用编译器会拦住，从手里的 `Rc` 拿到的
//! 借用（`file.borrow()`）拦不住，`if let`、`match`、`let … else` 的临时借用又不显眼。
//! 放开 BKL 的地方调用 [`assert_none_held`]，每一次等待都检查，不用碰上并发也查得出来。
//! 调试构建（cargo test）总是检查；发布构建打开 `borrow-check` 特性时检查（`appine-run`
//! 默认打开，差分测试、LTP、普查用的就是它；debug 版 App 也打开），App 的发布版里计数
//! 不存在，与标准库的 `RefCell` 一样。
//!
//! 线程自己的 `TaskState`（只有它所属的线程会借，整个系统调用期间都借着）和线程局部
//! 变量仍用标准库的 `RefCell`。

use std::cell;
use std::fmt;
use std::ops::{Deref, DerefMut};

#[cfg(any(debug_assertions, feature = "borrow-check"))]
thread_local! {
    /// 这个线程借着的 [`RefCell`] 个数。
    #[allow(clippy::missing_const_for_thread_local)] // 已经是 const，Android 上的 clippy 误报
    static BORROWS: cell::Cell<usize> = const { cell::Cell::new(0) };
}

/// 一个活着的借用：检查的构建里创建时加一、丢掉时减一。
struct Held;

impl Held {
    #[inline]
    fn new() -> Held {
        #[cfg(any(debug_assertions, feature = "borrow-check"))]
        BORROWS.set(BORROWS.get() + 1);
        Held
    }
}

#[cfg(any(debug_assertions, feature = "borrow-check"))]
impl Drop for Held {
    fn drop(&mut self) {
        BORROWS.set(BORROWS.get() - 1);
    }
}

/// 要放开 BKL 了：检查的构建里检查这个线程没有借着任何 [`RefCell`]。
#[inline]
pub(crate) fn assert_none_held() {
    #[cfg(any(debug_assertions, feature = "borrow-check"))]
    {
        let n = BORROWS.get();
        assert!(
            n == 0,
            "releasing the BKL while holding {n} RefCell borrow(s)"
        );
    }
}

/// 标准库的 `RefCell`，借用由检查的构建数着（模块文档）。
#[derive(Default)]
pub(crate) struct RefCell<T: ?Sized>(cell::RefCell<T>);

/// [`RefCell::borrow`] 的借用。
pub(crate) struct Ref<'b, T: ?Sized> {
    inner: cell::Ref<'b, T>,
    _held: Held,
}

/// [`RefCell::borrow_mut`] 的借用。
pub(crate) struct RefMut<'b, T: ?Sized> {
    inner: cell::RefMut<'b, T>,
    _held: Held,
}

impl<T> RefCell<T> {
    pub const fn new(value: T) -> RefCell<T> {
        RefCell(cell::RefCell::new(value))
    }
}

impl<T: ?Sized> RefCell<T> {
    #[inline]
    #[track_caller]
    pub fn borrow(&self) -> Ref<'_, T> {
        Ref {
            inner: self.0.borrow(),
            _held: Held::new(),
        }
    }

    #[inline]
    #[track_caller]
    pub fn borrow_mut(&self) -> RefMut<'_, T> {
        RefMut {
            inner: self.0.borrow_mut(),
            _held: Held::new(),
        }
    }

    pub fn try_borrow(&self) -> Result<Ref<'_, T>, cell::BorrowError> {
        self.0.try_borrow().map(|inner| Ref {
            inner,
            _held: Held::new(),
        })
    }

    pub fn try_borrow_mut(&self) -> Result<RefMut<'_, T>, cell::BorrowMutError> {
        self.0.try_borrow_mut().map(|inner| RefMut {
            inner,
            _held: Held::new(),
        })
    }

    pub fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut()
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RefCell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: ?Sized> Deref for Ref<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized> Deref for RefMut<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized> DerefMut for RefMut<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}
