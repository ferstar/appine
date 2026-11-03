//! Guest pointer types (design doc §12 item 2).
//!
//! Addresses in syscall arguments are always wrapped in the types here and can be accessed only
//! through [`Mm`](crate::mm::Mm); the kernel layer never dereferences them as host pointers. A
//! typed `UserPtr<T>` will be added together with the first syscall that takes a struct pointer.

/// A range of bytes in a guest address space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserSlice {
    /// Start guest address.
    pub addr: u64,
    /// Length (bytes).
    pub len: usize,
}

impl UserSlice {
    /// Builds a description of a piece of guest memory, without any checks.
    pub fn new(addr: u64, len: usize) -> Self {
        UserSlice { addr, len }
    }
}
