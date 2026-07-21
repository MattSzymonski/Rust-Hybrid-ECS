//! Thread-safe raw-pointer wrappers used internally by the query system.
//!
//! # Responsibilities
//!
//! - Provides [`SendPtr<T>`] and [`SendPtrMut<T>`] — raw-pointer wrappers
//!   that implement [`Send`] and [`Sync`] for cross-thread sharing.
//! - Caches pointers to archetype-owned storage so worker threads can
//!   access components without repeated HashMap lookups.
//!
//! # Design
//!
//! These wrappers hold plain addresses with no ownership semantics. Moving
//! them between threads is harmless; sharing `&SendPtr<T>` only allows
//! copying the address. Actual dereference requires `unsafe` blocks that
//! the query system protects through the scheduler's disjoint-access guarantee.

// Standard library

// =============================================================================
// SendPtr
// =============================================================================

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

// =============================================================================
// SendPtrMut
// =============================================================================

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
