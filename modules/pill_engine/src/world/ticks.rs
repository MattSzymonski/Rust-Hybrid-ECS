//! Change detection: the world's tick counter and the per-system baseline.
//!
//! # Responsibilities
//!
//! - Owns the monotonically increasing world tick every write is stamped with.
//! - Answers what "since my last run" means for the system currently running,
//!   which is what `Changed<T>` and `Added<T>` compare against.
//!
//! # Design
//!
//! A component's own ticks live in its column, beside the row they describe, so
//! nothing here tracks per-row metadata. What is left is the pair of numbers a
//! filter needs: the tick this run was stamped with, and the tick the system
//! last ran at. In parallel mode the second one is per-thread, because two
//! systems running at once need different baselines and a single field on
//! `World` would race.
//!
//! Every component and resource stores two tick values: `added` and `changed`.
//! The world bumps a global tick counter each frame.  When you write to a
//! component (through `&mut T` in a query, via `Mut<T>`), its `changed`
//! tick is set to the current world tick.
//!
//! Filters like `Changed<T>` and `Added<T>` compare each entity's ticks
//! against a *baseline* - the tick at which the calling system last ran.
//! If a component's `changed` tick is newer than that baseline, the entity
//! is yielded.  This is how "only process entities that were modified since
//! I last looked" works without any manual dirty flags.
//!
//! The baseline comes from one of two places:
//!
//!   SEQUENTIAL mode → world.system_last_run  (one shared field)
//!   PARALLEL  mode → per-thread override      (no sharing, no races)
//!
//! In parallel mode the Engine sets a thread-local override before each
//! system runs, so every thread sees the correct baseline for its own
//! system without touching shared state.

use super::*;

// =============================================================================
// World - Change Detection
// =============================================================================

impl World {
    /// Read the current world tick without modifying it.
    #[inline]
    pub fn change_tick(&self) -> Tick {
        Tick::new(self.change_tick)
    }

    /// Bump the world tick and return the new value.
    ///
    /// Called by the [`Engine`](crate::engine::Engine) once per frame and
    /// by mutable queries when they begin iteration so that mutations
    /// performed during the same frame can still be distinguished by tick.
    ///
    /// A parallel batch runs every one of its systems against the same world,
    /// so bumping here would be an unsynchronized read-modify-write. The
    /// engine therefore allocates one tick per batch member on the dispatch
    /// thread and installs it as this thread's override; inside a system the
    /// reserved value is returned however often it is asked for, and the
    /// shared counter is left untouched.
    #[inline]
    pub fn increment_change_tick(&mut self) -> Tick {
        if let Some(reserved) = per_thread_this_run_tick() {
            return reserved;
        }
        self.change_tick = self.change_tick.wrapping_add(1);
        Tick::new(self.change_tick)
    }

    /// What tick was current when the calling system last ran?
    ///
    /// If a per-thread override is active (parallel execution), that value
    /// wins.  Otherwise fall back to the shared world field (sequential
    /// execution or ad-hoc queries).
    #[inline]
    pub fn system_last_run(&self) -> Tick {
        if let Some(t) = per_thread_last_run_tick() {
            return t;
        }
        Tick::new(self.system_last_run)
    }

    /// Set the world-level baseline directly.  Prefer letting the Engine
    /// manage this - this method exists mainly for tests and one-off queries.
    #[inline]
    pub fn set_system_last_run(&mut self, tick: Tick) {
        self.system_last_run = tick.get();
    }
}
