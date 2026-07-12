//! Sequential and parallel iterators produced by [`Query`](super::Query).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rayon::prelude::*;

use crate::archetype::{Archetype, ArchetypeId};
use crate::component::Tick;
use crate::world::World;

use super::filter::QueryFilter;
use super::target::QueryTarget;
use super::FilteredArchetypeRange;

// ----------------------------------------------------------------------------
// Batch Statistics
// ----------------------------------------------------------------------------

/// Statistics about batch distribution during parallel iteration.
///
/// Returned by tracked parallel iteration to provide insight into
/// how Rayon distributed work across threads.
#[derive(Debug, Clone, Default)]
pub struct BatchStats {
    /// Number of threads in the Rayon thread pool
    pub num_threads: usize,
    /// Total number of batches that were executed
    pub batch_count: usize,
    /// Total number of entities that were processed
    pub total_entities: usize,
    /// Size of the smallest batch
    pub min_batch_size: usize,
    /// Size of the largest batch
    pub max_batch_size: usize,
    /// Average batch size (total_entities / batch_count)
    pub avg_batch_size: f64,
}

impl std::fmt::Display for BatchStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BatchStats {{ threads: {}, batches: {}, entities: {}, min: {}, max: {}, avg: {:.1} }}",
            self.num_threads,
            self.batch_count,
            self.total_entities,
            self.min_batch_size,
            self.max_batch_size,
            self.avg_batch_size
        )
    }
}

// ----------------------------------------------------------------------------
// Sequential Iterator
// ----------------------------------------------------------------------------

/// Sequential iterator for mutable queries.
pub struct QueryIterMut<'w, Q: QueryTarget, F: QueryFilter = ()> {
    world_ptr: *mut World,
    matching_archetypes: Vec<ArchetypeId>,
    current_archetype_idx: usize,
    current_entity_idx: usize,
    // Cache the current archetype length to avoid repeated lookups
    current_archetype_len: usize,
    // Cache component storage pointers (always Some during iteration)
    current_state: Option<Q::State>,
    // Cache filter state for the current archetype.
    current_filter_state: Option<F::State>,
    /// World tick captured at iterator construction; passed into each
    /// `Mut<T>` produced during iteration so mutations can be detected.
    this_run: Tick,
    /// Baseline tick used by the filter to detect changes since the
    /// owning system last ran.
    last_run: Tick,
    _phantom: std::marker::PhantomData<&'w mut (Q, F)>,
}

impl<'w, Q: QueryTarget, F: QueryFilter> QueryIterMut<'w, Q, F> {
    /// Construct a new sequential iterator. Used by [`Query::iter_mut`].
    ///
    /// SAFETY contract for the caller: `world_ptr` must remain valid and
    /// be exclusively borrowed for the entirety of the iterator's
    /// lifetime `'w`, and every `ArchetypeId` in `matching_archetypes`
    /// must be present in that world.
    pub(crate) fn new(
        world_ptr: *mut World,
        matching_archetypes: Vec<ArchetypeId>,
        this_run: Tick,
        last_run: Tick,
    ) -> Self {
        Self {
            world_ptr,
            matching_archetypes,
            current_archetype_idx: 0,
            current_entity_idx: 0,
            current_archetype_len: 0,
            current_state: None,
            current_filter_state: None,
            this_run,
            last_run,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Advance to the next archetype (cold path, separated for better branch prediction)
    #[inline(never)]
    fn advance_archetype(&mut self) -> Option<()> {
        // Check if all archetypes have been exhausted
        if self.current_archetype_idx >= self.matching_archetypes.len() {
            return None;
        }

        // SAFETY: This function is safe because:
        // 1. world_ptr was created from a valid &mut World reference in iter_mut()
        // 2. The QueryIterMut holds exclusive access to World through its lifetime 'w
        // 3. We never yield references that outlive the iterator itself
        // 4. Each archetype_id comes from matching_archetypes which was populated from valid archetypes
        // 5. The HashMap lookup can fail (returning None) but that's handled by the ? operator
        // 6. init_state() caches raw pointers to component storage, which remain valid because:
        //    - We hold exclusive access to World
        //    - Archetypes are not moved/reallocated during iteration
        //    - Component storage vectors maintain stable addresses while we iterate
        unsafe {
            let world = &mut *self.world_ptr;
            let archetype_id = self.matching_archetypes[self.current_archetype_idx];
            let archetype = world.archetypes.get_mut(&archetype_id)?;

            // Cache archetype length and component storage pointers
            self.current_archetype_len = archetype.len();
            let arch_ptr = archetype as *mut Archetype;
            self.current_state = Some(Q::init_state(&mut *arch_ptr, self.this_run));
            self.current_filter_state =
                Some(F::init_state(&mut *arch_ptr, self.last_run, self.this_run));
            self.current_entity_idx = 0;
            self.current_archetype_idx += 1;
        }

        Some(())
    }
}

impl<'w, Q: QueryTarget, F: QueryFilter> Iterator for QueryIterMut<'w, Q, F> {
    type Item = Q::Item<'w>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Hot path: iterate within current archetype.
            while self.current_entity_idx < self.current_archetype_len {
                let index = self.current_entity_idx;
                self.current_entity_idx += 1;

                // SAFETY: both states are Some during iteration in the hot path.
                let state = unsafe { self.current_state.as_ref().unwrap_unchecked() };
                let filter_state = unsafe { self.current_filter_state.as_ref().unwrap_unchecked() };

                if !F::matches(filter_state, index) {
                    continue;
                }
                return Some(Q::fetch_with_state(state, index));
            }

            // Cold path: advance to next archetype.
            self.advance_archetype()?;
        }
    }
}

// ----------------------------------------------------------------------------
// Parallel Iterator
// ----------------------------------------------------------------------------

/// Parallel iterator for queries using Rayon.
///
/// Supports method chaining for configuration:
/// - `.with_batch_size(n)` - Set minimum batch size
/// - `.tracked()` - Enable batch statistics collection
/// - `.for_each(f)` - Execute closure on each entity
pub struct ParQueryIter<'w, Q: QueryTarget, F: QueryFilter = ()> {
    archetype_ranges: Vec<FilteredArchetypeRange<Q::State, F::State>>,
    min_batch_size: Option<usize>,
    tracked: bool,
    _phantom: std::marker::PhantomData<&'w mut (Q, F)>,
}

// SAFETY: ParQueryIter can be sent between threads because:
// - archetype_ranges contains owned data (Vec) and raw pointers in Q::State
// - The raw pointers in Q::State point to component storage that remains valid
//   for the lifetime of the query (exclusive World access)
// - Each thread accesses different entity indices, so no data races occur
unsafe impl<'w, Q: QueryTarget, F: QueryFilter> Send for ParQueryIter<'w, Q, F>
where
    Q::State: Send,
    F::State: Send,
{
}
unsafe impl<'w, Q: QueryTarget, F: QueryFilter> Sync for ParQueryIter<'w, Q, F>
where
    Q::State: Sync,
    F::State: Sync,
{
}

impl<'w, Q: QueryTarget, F: QueryFilter> ParQueryIter<'w, Q, F> {
    /// Construct a new parallel iterator with default settings. Used by
    /// [`Query::par_iter_mut`].
    pub(crate) fn new(archetype_ranges: Vec<FilteredArchetypeRange<Q::State, F::State>>) -> Self {
        Self {
            archetype_ranges,
            min_batch_size: None,
            tracked: false,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<'w, Q: QueryTarget, F: QueryFilter> ParQueryIter<'w, Q, F>
where
    Q::State: Send + Sync,
    F::State: Send + Sync,
    for<'a> Q::Item<'a>: Send,
{
    /// Get the number of threads available in Rayon's thread pool
    pub fn num_threads() -> usize {
        rayon::current_num_threads()
    }

    /// Get an upper bound on the number of entities that will be processed.
    ///
    /// This counts every row in every matching archetype, before the
    /// row-level filter is applied. The actual number of yielded items
    /// may be smaller when a non-trivial filter (e.g. `Changed<T>`) is in
    /// use.
    pub fn entity_count(&self) -> usize {
        self.archetype_ranges
            .iter()
            .map(|(_, _, _, len)| *len)
            .sum()
    }

    /// Set minimum batch size for parallel iteration.
    ///
    /// Larger batches reduce overhead but may cause load imbalance.
    /// Smaller batches improve load balancing but increase overhead.
    ///
    /// Guidelines:
    /// - Light work (simple math): larger batches (1000-10000)
    /// - Heavy work (complex calculations): smaller batches (10-100)
    pub fn with_batch_size(mut self, size: usize) -> Self {
        self.min_batch_size = Some(size);
        self
    }

    /// Enable batch statistics tracking.
    pub fn tracked(mut self) -> Self {
        self.tracked = true;
        self
    }

    /// Execute closure on each entity in parallel.
    ///
    /// Returns `BatchStats` if `.tracked()` was called, otherwise `()`.
    pub fn for_each<Func>(self, f: Func) -> ParForEachResult
    where
        Func: Fn(Q::Item<'_>) + Send + Sync,
    {
        if self.tracked {
            ParForEachResult::Tracked(self.execute_tracked(f))
        } else {
            self.execute_untracked(f);
            ParForEachResult::Untracked
        }
    }

    /// Execute the closure on every matching entity (untracked).
    ///
    /// Uses an adaptive fallback if the total entity count is below
    /// `num_threads × 256`, the iteration runs sequentially - avoiding
    /// Rayon scheduling overhead for tiny workloads (common when many
    /// small archetypes exist).  Above the threshold, the existing
    /// two-level `par_iter` (archetypes × rows) is used, which performs
    /// well for large entity counts.
    fn execute_untracked<Func>(self, f: Func)
    where
        Func: Fn(Q::Item<'_>) + Send + Sync,
    {
        // Sum precomputed entity counts - O(archetypes), negligible.
        let total: usize = self.archetype_ranges.iter().map(|(_, _, _, len)| len).sum();
        let threshold = rayon::current_num_threads() * 256;

        if total < threshold {
            // Adaptive fallback: sequential loop.  For small N the
            // overhead of spawning Rayon tasks exceeds the work itself,
            // especially with many tiny archetypes.
            for (_, q_state, f_state, len) in &self.archetype_ranges {
                for index in 0..*len {
                    if F::matches(f_state, index) {
                        f(Q::fetch_with_state(q_state, index));
                    }
                }
            }
            return;
        }

        // Parallel path: two-level par_iter (archetypes × rows).
        // Each outer task handles one archetype; the inner par_iter
        // distributes rows within that archetype across threads.
        //
        // Default batch size of 256 amortizes Rayon scheduling overhead
        // while maintaining good load balancing. For workloads where
        // per-row cost is extremely low (< 10 ns), larger values (512-1024)
        // yield better throughput.
        let min_len = self.min_batch_size.unwrap_or(256);

        self.archetype_ranges
            .into_par_iter()
            .for_each(|(_, q_state, f_state, len)| {
                (0..len)
                    .into_par_iter()
                    .with_min_len(min_len)
                    .for_each(|index| {
                        if F::matches(&f_state, index) {
                            f(Q::fetch_with_state(&q_state, index));
                        }
                    });
            });
    }

    /// Execute the closure on every matching entity, collecting
    /// [`BatchStats`] about how Rayon distributed the work.
    ///
    /// Same adaptive fallback as [`execute_untracked`]: sequential below
    /// `num_threads × 256` entities, two-level `par_iter` above.
    ///
    /// Tracking adds per-batch atomics (count, min, max) so the caller
    /// can inspect load distribution.  The atomic overhead is modest
    /// because each batch only bumps the counters once.
    ///
    /// [`execute_untracked`]: Self::execute_untracked
    fn execute_tracked<Func>(self, f: Func) -> BatchStats
    where
        Func: Fn(Q::Item<'_>) + Send + Sync,
    {
        let num_threads = rayon::current_num_threads();
        let total_entities: usize = self.archetype_ranges.iter().map(|(_, _, _, len)| len).sum();
        let threshold = num_threads * 256;

        // Adaptive fallback: sequential for tiny workloads.
        if total_entities < threshold {
            let mut processed = 0usize;
            for (_, q_state, f_state, len) in &self.archetype_ranges {
                for index in 0..*len {
                    if F::matches(f_state, index) {
                        f(Q::fetch_with_state(q_state, index));
                    }
                    processed += 1;
                }
            }
            return BatchStats {
                total_entities,
                batch_count: 1,
                min_batch_size: processed,
                max_batch_size: processed,
                avg_batch_size: processed as f64,
                num_threads,
            };
        }

        // Parallel path: two-level par_iter with per-batch atomics.
        // `fold_with` accumulates a per-thread count within each inner
        // batch; the `for_each` at the end pushes the count into shared
        // atomics for aggregated stats.
        let min_len = self.min_batch_size.unwrap_or(256);

        let batch_count = Arc::new(AtomicUsize::new(0));
        let min_batch = Arc::new(AtomicUsize::new(usize::MAX));
        let max_batch = Arc::new(AtomicUsize::new(0));

        self.archetype_ranges
            .into_par_iter()
            .for_each(|(_, q_state, f_state, len)| {
                let batch_count = Arc::clone(&batch_count);
                let min_batch = Arc::clone(&min_batch);
                let max_batch = Arc::clone(&max_batch);

                (0..len)
                    .into_par_iter()
                    .with_min_len(min_len)
                    .fold_with(0usize, |count, index| {
                        if F::matches(&f_state, index) {
                            f(Q::fetch_with_state(&q_state, index));
                        }
                        count + 1
                    })
                    .for_each(|size| {
                        batch_count.fetch_add(1, Ordering::Relaxed);
                        min_batch.fetch_min(size, Ordering::Relaxed);
                        max_batch.fetch_max(size, Ordering::Relaxed);
                    });
            });

        let batch_count = batch_count.load(Ordering::Relaxed);
        let min_batch_size = min_batch.load(Ordering::Relaxed);
        let max_batch_size = max_batch.load(Ordering::Relaxed);

        BatchStats {
            num_threads,
            batch_count,
            total_entities,
            min_batch_size: if batch_count > 0 { min_batch_size } else { 0 },
            max_batch_size,
            avg_batch_size: if batch_count > 0 {
                total_entities as f64 / batch_count as f64
            } else {
                0.0
            },
        }
    }
}

/// Result of parallel for_each execution.
#[derive(Debug, Clone, Default)]
pub enum ParForEachResult {
    #[default]
    Untracked,
    Tracked(BatchStats),
}

impl ParForEachResult {
    /// Get batch stats if tracking was enabled.
    pub fn stats(self) -> Option<BatchStats> {
        match self {
            ParForEachResult::Tracked(stats) => Some(stats),
            ParForEachResult::Untracked => None,
        }
    }

    /// Unwrap batch stats, panicking if not tracked.
    pub fn unwrap(self) -> BatchStats {
        match self {
            ParForEachResult::Tracked(stats) => stats,
            ParForEachResult::Untracked => panic!("for_each was not tracked"),
        }
    }
}

impl From<ParForEachResult> for Option<BatchStats> {
    fn from(result: ParForEachResult) -> Self {
        result.stats()
    }
}

impl std::fmt::Display for ParForEachResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParForEachResult::Tracked(stats) => write!(f, "{}", stats),
            ParForEachResult::Untracked => write!(f, "Untracked"),
        }
    }
}
