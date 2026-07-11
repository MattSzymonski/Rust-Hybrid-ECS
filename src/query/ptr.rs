//! Thread-safe raw-pointer wrappers used internally by the query system.
//!
//! These wrappers allow caching pointers to archetype-owned storage
//! across thread boundaries during parallel iteration. Safety relies on
//! the scheduler guaranteeing exclusive `World` access for the lifetime
//! of the query and disjoint per-row access between threads.

/// A wrapper for `*const T` that implements [`Send`] and [`Sync`].
///
/// SAFETY:
///
/// Raw pointers carry no ownership - the wrapper just holds a plain address.  
/// Moving it between threads is harmless, and sharing `&SendPtr<T>`
/// only allows copying the address (dereference requires `unsafe`).
/// The standard library omits these impls as a lint; they are not fundamentally unsound.
/// The actual data-race-prevention obligation rests on the `unsafe` blocks that dereference the pointer,
/// which the query system enforces through the scheduler's disjoint-access guarantee.
#[derive(Clone, Copy)]
pub struct SendPtr<T>(*const T);

// SAFETY: Raw pointer - no ownership transferred when moved across threads.
unsafe impl<T> Send for SendPtr<T> {}
// SAFETY: `&SendPtr<T>` only grants access to a copyable address; no
// data race is possible through the wrapper itself.
unsafe impl<T> Sync for SendPtr<T> {}

impl<T> SendPtr<T> {
    pub fn new(ptr: *const T) -> Self {
        Self(ptr)
    }

    pub fn as_ptr(&self) -> *const T {
        self.0
    }
}

/// A wrapper for `*mut T` that implements [`Send`] and [`Sync`].
///
/// SAFETY: The same rationale as [`SendPtr`] applies - raw pointers carry no ownership.  
/// The `*mut` vs `*const` distinction only matters at the dereference site,
/// which requires `unsafe` and is guarded by the scheduler's disjoint-access guarantee.
#[derive(Clone, Copy)]
pub struct SendPtrMut<T>(*mut T);

// SAFETY: See [`SendPtr`].
unsafe impl<T> Send for SendPtrMut<T> {}
// SAFETY: See [`SendPtr`].
unsafe impl<T> Sync for SendPtrMut<T> {}

impl<T> SendPtrMut<T> {
    pub fn new(ptr: *mut T) -> Self {
        Self(ptr)
    }

    pub fn as_ptr(&self) -> *mut T {
        self.0
    }
}
