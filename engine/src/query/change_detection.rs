//! [`Mut<T>`] smart pointer that records component mutation ticks on `DerefMut`.
//!
//! # Responsibilities
//!
//! - Wraps `&mut T` in a [`Mut<T>`] that transparently dereferences to the inner value.
//! - Bumps the component's `changed` tick on every mutable dereference for
//!   frame-based change detection by [`Changed`] and [`Added`] filters.
//!
//! # Design
//!
//! Queries requesting `&mut T` yield `Mut<'a, T>` instead of bare `&'a mut T`.
//! The wrapper's `DerefMut` implementation updates the associated
//! [`ComponentTicks::changed`] field to the current world tick. This per-row
//! update requires no synchronization because the scheduler guarantees
//! disjoint mutable access per thread.

// Standard library
use std::ops::{Deref, DerefMut};

// Current crate
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
/// ```no_run
/// # use pill_engine::*;
/// # #[derive(Debug, Clone)] struct Transform { x: f32, y: f32 }
/// # impl Component for Transform {}
/// # #[derive(Debug, Clone)] struct Velocity { x: f32, y: f32 }
/// # impl Component for Velocity {}
/// fn movement(mut q: Query<(&mut Transform, &Velocity)>) {
///     for (mut transform, vel) in q.iter_mut() {
///         transform.x += vel.x; // DerefMut bumps the changed tick
///     }
/// }
/// ```
///
/// # Safety
///
/// The wrapper stores raw mutable pointers; safety relies on the scheduler's
/// guarantee that no two threads obtain `Mut<T>` for the same component row.

// =============================================================================
// Mut
// =============================================================================

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

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies that immutable `Deref` does not bump the changed tick.
    #[test]
    fn deref_does_not_bump_changed() {
        let mut value = 42_i32;
        let mut ticks = ComponentTicks::new(Tick::new(1));
        let m = Mut::new(&mut value, &mut ticks, Tick::new(5));
        // Immutable deref
        let _ = *m;
        assert_eq!(m.last_changed(), Tick::new(1));
    }

    /// Verifies that `DerefMut` bumps the changed tick to the current world tick.
    #[test]
    fn deref_mut_bumps_changed() {
        let mut value = 42_i32;
        let mut ticks = ComponentTicks::new(Tick::new(1));
        let mut m = Mut::new(&mut value, &mut ticks, Tick::new(5));
        *m += 1;
        assert_eq!(m.last_changed(), Tick::new(5));
        assert_eq!(*m, 43);
    }

    /// Verifies that `bypass()` provides raw access without bumping the changed tick.
    #[test]
    fn bypass_skips_tick_bump() {
        let mut value = 0_i32;
        let mut ticks = ComponentTicks::new(Tick::new(1));
        let mut m = Mut::new(&mut value, &mut ticks, Tick::new(7));
        *m.bypass_change_detection() = 99;
        assert_eq!(m.last_changed(), Tick::new(1));
        assert_eq!(*m, 99);
    }

    /// Verifies that `set_changed()` explicitly marks a row as changed at the given tick.
    #[test]
    fn set_changed_marks_row() {
        let mut value = 0_i32;
        let mut ticks = ComponentTicks::new(Tick::new(1));
        let mut m = Mut::new(&mut value, &mut ticks, Tick::new(9));
        m.set_changed();
        assert_eq!(m.last_changed(), Tick::new(9));
    }
}
