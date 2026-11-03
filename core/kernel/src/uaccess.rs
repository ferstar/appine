//! guest 指针类型（设计文档 §12 第 2 条）。
//!
//! syscall 参数里的地址一律包装成这里的类型，只能经由 [`Mm`](crate::mm::Mm)
//! 访问，内核层永远不把它们当作宿主指针解引用。带类型的 `UserPtr<T>` 会随
//! 第一个接收结构体指针的 syscall 一起加入。

/// guest 地址空间里的一段字节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserSlice {
    /// 起始 guest 地址。
    pub addr: u64,
    /// 长度（字节）。
    pub len: usize,
}

impl UserSlice {
    /// 构造一段 guest 内存的描述，不做任何检查。
    pub fn new(addr: u64, len: usize) -> Self {
        UserSlice { addr, len }
    }
}
