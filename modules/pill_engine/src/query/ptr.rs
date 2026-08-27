//! Thread-safe raw-pointer wrappers used internally by the query system.
//!
//! # Responsibilities
//!
//! - Provides [`SendPtr<T>`] and [`SendPtrMut<T>`] - raw-pointer wrappers
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

// =============================================================================
// SendPtr
// =============================================================================

/// Thread-safe wrapper around a `*const T` used to share read-only component
/// pointers across worker threads.
///
/// The wrapper holds a plain address with no ownership semantics; moving it
/// between threads is harmless and sharing `&SendPtr<T>` only allows copying
/// the address. Dereference requires `unsafe` and is protected by the query
/// scheduler's disjoint-access guarantee.
#[derive(Clone, Copy)]
pub struct SendPtr<T>(*const T);

// SAFETY: `SendPtr<T>` owns a raw `*const T` with no ownership semantics, so
// transferring it across threads moves only an address; no data race or
// aliasing violation can arise from the move itself.
//
// Bounded on `T` rather than unconditional. The wrapper alone would be sound
// for any `T`, because it never dereferences - but its whole purpose is to be
// dereferenced on a worker thread, and an unbounded impl would silently make
// `SendPtr<Rc<_>>` cross threads with nothing but an `unsafe` block between
// that and a data race. The bound puts the requirement where the type system
// can see it. Every current use is a `T: Component`, which is already `Send`.
unsafe impl<T: Send> Send for SendPtr<T> {}
// SAFETY: Shared access `&SendPtr<T>` only permits copying the stored address
// out; the wrapper never dereferences the pointer, so no `&mut` aliasing or
// data race is reachable through the wrapper itself. Bounded for the same
// reason as `Send` above.
unsafe impl<T: Sync> Sync for SendPtr<T> {}

impl<T> SendPtr<T> {
    /// Wraps a raw read-only pointer in a [`SendPtr`].
    pub fn new(ptr: *const T) -> Self {
        Self(ptr)
    }

    /// Returns the wrapped raw read-only pointer.
    pub fn as_ptr(&self) -> *const T {
        self.0
    }
}

// =============================================================================
// SendPtrMut
// =============================================================================

/// Thread-safe wrapper around a `*mut T` used to share mutable component
/// pointers across worker threads.
///
/// As with [`SendPtr`], the wrapper holds a plain address with no ownership
/// semantics. The `*mut` versus `*const` distinction matters only at the
/// dereference site, which requires `unsafe` and is guarded by the query
/// scheduler's disjoint-access guarantee.
#[derive(Clone, Copy)]
pub struct SendPtrMut<T>(*mut T);

// SAFETY: `SendPtrMut<T>` owns a raw `*mut T` and moving it between threads
// transfers only the address; the wrapper itself performs no dereference, so
// no data race is introduced by the move. Bounded on `T: Send` for the reason
// given on `SendPtr` above: the address exists to be dereferenced elsewhere.
unsafe impl<T: Send> Send for SendPtrMut<T> {}
// SAFETY: Sharing `&SendPtrMut<T>` only allows copying the stored `*mut T`
// address; dereference is deferred to `unsafe` blocks protected by the
// disjoint-access guarantee that `SystemAccess::conflicts_with` establishes,
// so no data race is reachable through the wrapper itself.
//
// `T: Send` rather than `T: Sync`: a `*mut T` is handed out for **exclusive**
// access to a disjoint row, which is a transfer to another thread, not sharing.
unsafe impl<T: Send> Sync for SendPtrMut<T> {}

impl<T> SendPtrMut<T> {
    /// Wraps a raw mutable pointer in a [`SendPtrMut`].
    pub fn new(ptr: *mut T) -> Self {
        Self(ptr)
    }

    /// Returns the wrapped raw mutable pointer.
    pub fn as_ptr(&self) -> *mut T {
        self.0
    }
}
