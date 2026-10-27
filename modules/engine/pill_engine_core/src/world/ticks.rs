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
//! A component's own ticks live in its column, beside the row they describe:
//! `added` and `changed`, the latter set to the current world tick by a write
//! through a mutable query. Nothing here tracks per-row metadata.
//!
//! What is left is the pair of numbers a filter needs: the tick this run was
//! stamped with, and the tick the calling system last ran at - the baseline
//! `Changed<T>` and `Added<T>` compare against.
//!
//! The baseline has two sources. Sequentially it is `World::system_last_run`,
//! one shared field. In a parallel batch two systems running at once need
//! different baselines and a single field would race, so the engine sets a
//! thread-local override before each system runs; every thread then reads that
//! for its own system without touching shared state.

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
    /// Called by the [`Engine`](crate::engine::Engine) once per frame and by
    /// mutable queries when they begin iteration, so mutations performed during
    /// the same frame can still be told apart by tick.
    ///
    /// A parallel batch runs every one of its systems against the same world,
    /// so bumping here would be an unsynchronized read-modify-write. The engine
    /// therefore allocates one tick per batch member on the dispatch thread and
    /// installs it as this thread's override; inside a system the reserved value
    /// is returned however often it is asked for, and the shared counter is left
    /// untouched.
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
    /// A per-thread override, when one is installed (parallel execution), wins
    /// over the shared world field (sequential execution or ad-hoc queries).
    #[inline]
    pub fn system_last_run(&self) -> Tick {
        if let Some(t) = per_thread_last_run_tick() {
            return t;
        }
        Tick::new(self.system_last_run)
    }

    /// Set the world-level baseline directly.
    ///
    /// The engine manages this itself; the setter exists for tests and one-off
    /// queries that compare against a tick they chose.
    #[inline]
    pub fn set_system_last_run(&mut self, tick: Tick) {
        self.system_last_run = tick.get();
    }
}
