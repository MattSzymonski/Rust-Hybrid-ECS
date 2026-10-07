//! Parallel dispatch for managed systems: the host's half of
//! `query.ForEachParallel`.
//!
//! # Responsibilities
//!
//! - Run a managed callback for every index in `0..count` across the shared
//!   [`pill_core::rayon`] pool, joining before returning.
//! - Refuse calls made without an active managed invocation, or made from
//!   inside a parallel callback.
//!
//! # Design
//!
//! A scheduled managed system calls in on the frame thread; Rayon workers run
//! the callbacks (the calling thread participates in the join, so a work item
//! may also land there). Workers carry no managed scope, so every engine
//! entry point a worker called would refuse on its own - the checks here only
//! cover the calling thread and the nesting case, and they run before any
//! dispatch so the answers stay deterministic whichever thread Rayon picks.

// Standard library
use std::cell::Cell;
use std::ffi::c_void;

// External crates
use pill_core::rayon::prelude::*;

// Current crate
use crate::context::{active_scope_token, guard_managed_callback};

// =============================================================================
// Constants
// =============================================================================

/// Every index ran.
pub(super) const PARALLEL_OK: u8 = 0;
/// No managed system is scheduled on the calling thread.
pub(super) const PARALLEL_NO_SCOPE: u8 = 3;
/// The call came from inside a parallel callback.
pub(super) const PARALLEL_NESTED: u8 = 4;
/// A panic escaped the dispatch itself; reported by the guard.
const PARALLEL_INTERNAL_FAILURE: u8 = 5;

// =============================================================================
// Types + Impls
// =============================================================================

/// The managed trampoline's signature: the `[UnmanagedCallersOnly]` static
/// invoked once per work item.
pub(super) type ParallelCallback = extern "C" fn(*mut c_void, u32);

thread_local! {
    /// Whether this thread is inside a parallel callback right now.
    ///
    /// Set around every callback invocation on whichever thread runs it, so a
    /// nested dispatch is refused deterministically even when Rayon hands the
    /// work to the (scoped) calling thread.
    static IN_PARALLEL_CALLBACK: Cell<bool> = const { Cell::new(false) };
}

/// The managed caller's state pointer, allowed to travel to Rayon workers.
#[derive(Clone, Copy)]
struct SendState(*mut c_void);

impl SendState {
    /// The wrapped pointer, passed straight back to the managed callback.
    ///
    /// Accessed through a method on purpose: a field projection inside a
    /// dispatch closure would capture the raw pointer alone (disjoint closure
    /// captures), which is neither `Send` nor `Sync`; a method call captures
    /// the whole wrapper.
    fn get(self) -> *mut c_void {
        self.0
    }
}

// SAFETY: the state pointer belongs to the managed caller, which keeps it
// alive until the dispatch returns; workers only pass it back to the callback
// unchanged and never dereference it.
unsafe impl Send for SendState {}
// SAFETY: shared access only reads the pointer value; no worker dereferences
// it, and the managed caller's GCHandle keeps the target alive through the
// join.
unsafe impl Sync for SendState {}

// =============================================================================
// Free Functions
// =============================================================================

/// Run `run(index)` for every index in `0..count` on the shared pool,
/// returning only after every invocation finished.
///
/// Statuses: [`PARALLEL_OK`], [`PARALLEL_NO_SCOPE`], [`PARALLEL_NESTED`].
pub(super) fn parallel_for(count: u32, run: impl Fn(u32) + Send + Sync) -> u8 {
    // Step 1: Refuse nesting and off-scope calls before any dispatch; the
    // flag is set on whichever thread runs a callback, so both answers are
    // deterministic even on the calling thread itself.
    if IN_PARALLEL_CALLBACK.with(|flag| flag.get()) {
        return PARALLEL_NESTED;
    }
    if active_scope_token() == 0 {
        return PARALLEL_NO_SCOPE;
    }
    if count == 0 {
        return PARALLEL_OK;
    }

    // Step 2: Fan out. `for_each` joins before returning, so the caller
    // resumes only when every callback finished - the invariant the managed
    // side (world borrows, scope teardown, assembly unloadability) rests on.
    (0..count).into_par_iter().for_each(|index| {
        with_callback_flag(|| run(index));
    });
    PARALLEL_OK
}

/// Run `body` with this thread's in-callback flag set, restoring it after.
fn with_callback_flag<R>(body: impl FnOnce() -> R) -> R {
    IN_PARALLEL_CALLBACK.with(|flag| {
        let previous = flag.replace(true);
        let result = body();
        flag.set(previous);
        result
    })
}

/// Run the managed trampoline for every index in `0..count`; the entry the
/// managed runtime calls through the API table.
///
/// `state` is opaque: it travels back to `callback` untouched.
pub(super) extern "C" fn ffi_parallel_for(
    callback: ParallelCallback,
    state: *mut c_void,
    count: u32,
) -> u8 {
    guard_managed_callback("ffi_parallel_for", PARALLEL_INTERNAL_FAILURE, || {
        let state = SendState(state);
        parallel_for(count, move |index| callback(state.get(), index))
    })
}
