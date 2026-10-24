//! Per-thread change ticks of the system running on the current thread.
//!
//! # Responsibilities
//!
//! - Hold, for each thread, the "last ran at" tick and the reserved "this
//!   run" tick the scheduler installs before it runs a system in a parallel
//!   batch.
//! - Give every loaded artifact the same copy of these values.
//!
//! # Design
//!
//! The scheduler sets these in the host's engine code, and a system reads them
//! in the code of the artifact that declares it: `Query` is generic, so its
//! tick reads compile into the project or extension DLL. Kept in `pill_engine`
//! (an rlib embedded in every DLL), each DLL had its own copy, so a module's
//! system never saw the value the scheduler installed. Here they exist once
//! per process, like the Rayon pool.
//!
//! The values are raw `u32` ticks; `pill_engine` wraps them in its `Tick`.
//! Every accessor is `#[inline(never)]` so its thread-local is always the one
//! inside `pill_core.dll`, never a copy inlined into a caller.

// Standard library
use std::cell::Cell;

thread_local! {
    /// The tick at which the system running on this thread last ran. `None`
    /// outside a parallel batch, where the world's own field applies.
    static LAST_RUN_TICK: Cell<Option<u32>> = const { Cell::new(None) };

    /// The tick reserved for the system running on this thread. `None` means
    /// "bump the world's shared counter".
    static THIS_RUN_TICK: Cell<Option<u32>> = const { Cell::new(None) };
}

/// The last-run tick installed for the system running on this thread.
#[inline(never)]
pub fn last_run_tick() -> Option<u32> {
    LAST_RUN_TICK.with(Cell::get)
}

/// Install this thread's last-run tick; returns the previous value so the
/// caller can restore it once the system finishes.
#[inline(never)]
pub fn replace_last_run_tick(value: Option<u32>) -> Option<u32> {
    LAST_RUN_TICK.with(|cell| cell.replace(value))
}

/// The tick reserved for the system running on this thread.
#[inline(never)]
pub fn this_run_tick() -> Option<u32> {
    THIS_RUN_TICK.with(Cell::get)
}

/// Install this thread's reserved tick; returns the previous value so the
/// caller can restore it once the system finishes.
#[inline(never)]
pub fn replace_this_run_tick(value: Option<u32>) -> Option<u32> {
    THIS_RUN_TICK.with(|cell| cell.replace(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Replacing returns the earlier value, and each thread starts empty.
    #[test]
    fn values_are_per_thread_and_restorable() {
        assert_eq!(replace_this_run_tick(Some(7)), None);
        assert_eq!(replace_last_run_tick(Some(3)), None);
        assert_eq!(this_run_tick(), Some(7));
        assert_eq!(last_run_tick(), Some(3));
        let other_thread = std::thread::spawn(|| (this_run_tick(), last_run_tick()))
            .join()
            .expect("thread runs");
        assert_eq!(other_thread, (None, None));
        assert_eq!(replace_this_run_tick(None), Some(7));
        assert_eq!(replace_last_run_tick(None), Some(3));
    }
}
