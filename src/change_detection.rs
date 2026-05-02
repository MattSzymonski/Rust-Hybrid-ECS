// ============================================================================
// Change Detection - Mut<T> Smart Pointer
// ============================================================================
//! Smart-pointer wrapper that records mutation ticks on `DerefMut`.
//!
//! Queries that request mutable component access (`&mut T`) yield `Mut<'a, T>`
//! values instead of bare `&'a mut T`. The wrapper transparently dereferences
//! to the underlying component, so existing code that does
//! `transform.x += vel.x` continues to compile unchanged.
//!
//! When a `Mut<T>` is dereferenced through `DerefMut`, the associated
//! `ComponentTicks::changed` field is bumped to the current world tick.
//! This per-row update requires no synchronization for parallel queries
//! because the scheduler guarantees that no two threads observe the same
//! component row mutably.

use std::ops::{Deref, DerefMut};

use crate::component::{ComponentTicks, Tick};

/// Mutable, change-tracking access to a component (or resource) instance.
///
/// `Mut<'a, T>` carries a borrow of both the value and its
/// [`ComponentTicks`], plus the current world tick (`this_run`). Mutating
/// through `DerefMut` updates `ticks.changed = this_run` so that future
/// systems can detect the modification.
///
/// # Example
///
/// ```ignore
/// fn movement(mut q: Query<(&mut Transform, &Velocity)>) {
///     for (mut transform, vel) in q.iter_mut() {
///         transform.x += vel.x; // DerefMut bumps the changed tick
///     }
/// }
/// ```
pub struct Mut<'a, T: ?Sized> {
    pub(crate) value: &'a mut T,
    pub(crate) ticks: &'a mut ComponentTicks,
    pub(crate) this_run: Tick,
}

impl<'a, T: ?Sized> Mut<'a, T> {
    /// Build a new `Mut` from its parts.
    #[inline]
    pub fn new(value: &'a mut T, ticks: &'a mut ComponentTicks, this_run: Tick) -> Self {
        Self {
            value,
            ticks,
            this_run,
        }
    }

    /// Read access to the change-detection ticks for this row.
    #[inline]
    pub fn ticks(&self) -> &ComponentTicks {
        self.ticks
    }

    /// Tick at which this component was added to its entity.
    #[inline]
    pub fn last_added(&self) -> Tick {
        self.ticks.added
    }

    /// Tick at which this component was most recently mutated.
    #[inline]
    pub fn last_changed(&self) -> Tick {
        self.ticks.changed
    }

    /// The current world tick that mutations through this `Mut` will record.
    #[inline]
    pub fn this_run(&self) -> Tick {
        self.this_run
    }

    /// Mutably access the value WITHOUT bumping the change tick.
    ///
    /// Useful for read-modify-write paths that want to suppress change
    /// detection (e.g., internal bookkeeping that user systems should not
    /// observe as a change).
    #[inline]
    pub fn bypass_change_detection(&mut self) -> &mut T {
        self.value
    }

    /// Force-mark this row as changed at the current tick, even if the
    /// underlying value is not actually mutated.
    #[inline]
    pub fn set_changed(&mut self) {
        self.ticks.changed = self.this_run;
    }
}

impl<T: ?Sized> Deref for Mut<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        self.value
    }
}

impl<T: ?Sized> DerefMut for Mut<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // Per-row write - no atomics needed because the query/scheduler
        // guarantees disjoint access across threads.
        self.ticks.changed = self.this_run;
        self.value
    }
}

impl<T: ?Sized + std::fmt::Debug> std::fmt::Debug for Mut<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mut")
            .field("value", &self.value)
            .field("ticks", &self.ticks)
            .field("this_run", &self.this_run)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deref_does_not_bump_changed() {
        let mut value = 42_i32;
        let mut ticks = ComponentTicks::new(Tick::new(1));
        let m = Mut::new(&mut value, &mut ticks, Tick::new(5));
        // Immutable deref
        let _ = *m;
        assert_eq!(m.last_changed(), Tick::new(1));
    }

    #[test]
    fn deref_mut_bumps_changed() {
        let mut value = 42_i32;
        let mut ticks = ComponentTicks::new(Tick::new(1));
        let mut m = Mut::new(&mut value, &mut ticks, Tick::new(5));
        *m += 1;
        assert_eq!(m.last_changed(), Tick::new(5));
        assert_eq!(*m, 43);
    }

    #[test]
    fn bypass_skips_tick_bump() {
        let mut value = 0_i32;
        let mut ticks = ComponentTicks::new(Tick::new(1));
        let mut m = Mut::new(&mut value, &mut ticks, Tick::new(7));
        *m.bypass_change_detection() = 99;
        assert_eq!(m.last_changed(), Tick::new(1));
        assert_eq!(*m, 99);
    }

    #[test]
    fn set_changed_marks_row() {
        let mut value = 0_i32;
        let mut ticks = ComponentTicks::new(Tick::new(1));
        let mut m = Mut::new(&mut value, &mut ticks, Tick::new(9));
        m.set_changed();
        assert_eq!(m.last_changed(), Tick::new(9));
    }
}
