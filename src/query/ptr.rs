//! Thread-safe raw-pointer wrappers used internally by the query system.
//!
//! These wrappers allow caching pointers to archetype-owned storage
//! across thread boundaries during parallel iteration. Safety relies on
//! the scheduler guaranteeing exclusive `World` access for the lifetime
//! of the query and disjoint per-row access between threads.

/// A wrapper for `*const T` that implements `Send` and `Sync`.
///
/// SAFETY:
///
/// This type is safe to use when:
/// 1. The pointer points to valid data for the lifetime of the query
/// 2. Different threads access different indices (no aliasing)
/// 3. The World has exclusive access during iteration
#[derive(Clone, Copy)]
pub struct SendPtr<T>(*const T);

unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

impl<T> SendPtr<T> {
    pub fn new(ptr: *const T) -> Self {
        Self(ptr)
    }

    pub fn as_ptr(&self) -> *const T {
        self.0
    }
}

/// A wrapper for `*mut T` that implements `Send` and `Sync`.
///
/// SAFETY:
///
/// This type is safe to use when:
/// 1. The pointer points to valid data for the lifetime of the query
/// 2. Different threads access different indices (no aliasing)
/// 3. The World has exclusive access during iteration
#[derive(Clone, Copy)]
pub struct SendPtrMut<T>(*mut T);

unsafe impl<T> Send for SendPtrMut<T> {}
unsafe impl<T> Sync for SendPtrMut<T> {}

impl<T> SendPtrMut<T> {
    pub fn new(ptr: *mut T) -> Self {
        Self(ptr)
    }

    pub fn as_ptr(&self) -> *mut T {
        self.0
    }
}
