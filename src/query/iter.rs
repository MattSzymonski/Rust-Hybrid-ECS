//! Sequential and parallel iterators produced by [`Query`](super::Query).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::archetype::{Archetype, ArchetypeId};
use crate::component::Tick;
use crate::world::World;

use super::filter::QueryFilter;
use super::target::QueryTarget;
use super::FilteredArchetypeRange;

// ----------------------------------------------------------------------------
// Parallel-group tuning constants
// ----------------------------------------------------------------------------

/// Target wall-clock duration per parallel group (nanoseconds).
///
/// The timing-feedback loop divides the system's average execution time
/// by this value to determine how many Rayon tasks to spawn.  Larger
/// values mean fewer, bigger groups — less wake-up scatter but also
/// less parallelism.  50 µs is a sweet spot where OS thread wake-up
/// latency (~10 µs) doesn't dominate.
const TARGET_GROUP_DURATION_NS: u64 = 50_000;

/// Smoothing factor for the exponential moving average of system
/// execution time.  `1/32 ≈ 0.031` gives a ~32-frame averaging window,
/// damping frame-to-frame jitter.
pub(crate) const EMA_ALPHA_DENOM: i64 = 32;

/// Default entities per work slice.  Sized so one slice fits in L1
/// data cache for components up to 8 bytes (32 KiB / 8 B = 4096).
/// For the common `f32` component this is half-filling L1 — plenty of
/// room for filter state and adjacent cache lines.
const DEFAULT_SLICE_ENTITIES: usize = 4096;

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
        let _zone = crate::profile_scope!(
            "create mutable query iterator",
            [("Matching archetype ranges to iterate: {}", matching_archetypes.len())]
        );
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

        let _zone = crate::profile_scope!(
            "advance to next archetype",
            [(
                "archetype {}/{}",
                self.current_archetype_idx + 1,
                self.matching_archetypes.len()
            )]
        );

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
            let _zone_pointers = crate::profile_scope!(
                "get world and archetype id",
                [("Advancing to archetype: {}/{}", self.current_archetype_idx, self.matching_archetypes.len())]
            );
            let world = &mut *self.world_ptr;
            let archetype_id = self.matching_archetypes[self.current_archetype_idx];
            drop(_zone_pointers);

            let _zone_archetype = crate::profile_scope!("get archetype");
            let archetype = world.archetypes.get_mut(&archetype_id)?;
            _zone_archetype.text(format_args!(
                "{}",
                archetype.get_archetype_info(&world.component_registry),
            ));
            drop(_zone_archetype);

            let _zone_cache = crate::profile_scope!(
                "cache storage pointers",
                [("Entities in this archetype: {}", archetype.len())]
            );

            // Cache archetype length and component storage pointers
            self.current_archetype_len = archetype.len();
            let archetype_ptr = archetype as *mut Archetype;
            self.current_state = Some(Q::init_state(&mut *archetype_ptr, self.this_run));
            self.current_filter_state = Some(F::init_state(
                &mut *archetype_ptr,
                self.last_run,
                self.this_run,
            ));
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

                if !F::ACCEPTS_ALL {
                    let filter_state =
                        unsafe { self.current_filter_state.as_ref().unwrap_unchecked() };
                    if !F::matches(filter_state, index) {
                        continue;
                    }
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
    /// Optional label for Tracy zones — set via `.label("system_name")`.
    label: Option<&'static str>,
    /// Per-label EMA timing map, shared via Arc<Mutex<>> for thread-safe access.
    iterator_timings: std::sync::Arc<std::sync::Mutex<crate::world::IteratorTimings>>,
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
    pub(crate) fn new(
        archetype_ranges: Vec<FilteredArchetypeRange<Q::State, F::State>>,
        iterator_timings: std::sync::Arc<std::sync::Mutex<crate::world::IteratorTimings>>,
    ) -> Self {
        let _zone = crate::profile_scope!(
            "create parallel query iterator",
            [("Matching archetype ranges for parallel iteration: {}", archetype_ranges.len())]
        );
        Self {
            archetype_ranges,
            min_batch_size: None,
            tracked: false,
            label: None,
            iterator_timings,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Attach a label (usually the system name) for Tracy zone identification.
    pub fn label(mut self, name: &'static str) -> Self {
        self.label = Some(name);
        self
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
        // Resolve timing hint from per-label EMA.
        let hint_ns = self
            .label
            .and_then(|label| {
                self.iterator_timings.lock().ok().and_then(|timing| {
                    timing
                        .per_iterator_label_average_duration
                        .get(label)
                        .copied()
                })
            })
            .unwrap_or(0);

        // Capture before self is moved into execute_*.
        let label = self.label;
        let iterator_timings = std::sync::Arc::clone(&self.iterator_timings);

        let started = std::time::Instant::now();
        let result = if self.tracked {
            ParForEachResult::Tracked(self.execute_tracked(f, hint_ns))
        } else {
            self.execute_untracked(f, hint_ns);
            ParForEachResult::Untracked
        };
        let elapsed_ns = started.elapsed().as_nanos() as u64;

        // Store per-label EMA and track seen labels for duplicate detection.
        if let Some(label) = label {
            if let Ok(mut timing) = iterator_timings.lock() {
                let entry = timing
                    .per_iterator_label_average_duration
                    .entry(label)
                    .or_insert(elapsed_ns);
                let delta = elapsed_ns as i64 - *entry as i64;
                *entry = (*entry as i64 + delta / EMA_ALPHA_DENOM) as u64;
                if timing.visited_iterator_labels.contains(&label) {
                    timing.visited_duplicated_iterator_labels.push(label);
                } else {
                    timing.visited_iterator_labels.push(label);
                }
            }
        }

        result
    }

    /// Execute the closure on every matching entity (untracked).
    ///
    /// Uses an adaptive fallback: if the total entity count is below
    /// `num_threads × 256`, the iteration runs sequentially - avoiding
    /// Rayon scheduling overhead for tiny workloads (common when many
    /// small archetypes exist).  Above the threshold, flat work slices
    /// (default 4096 entities each, matching L1 data cache) are grouped
    /// into chunks and spawned via [`rayon::scope`], so every thread
    /// pulls tasks from a shared pool simultaneously
    /// — no work-stealing cascade, no late-arriving outliers.
    fn execute_untracked<Func>(self, f: Func, hint_ns: u64)
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

        // Parallel path: flat work slices distributed via rayon::scope.
        //
        // We pre-build equal-sized entity-range slices, then group them
        // into num_threads × 2 chunks. Spawning all chunks at once
        // through rayon::scope lets every thread pull tasks from a shared
        // pool simultaneously — no work-stealing cascade, no head-start
        // for the owning thread, and no late-arriving outliers.
        let min_len = self.min_batch_size.unwrap_or(DEFAULT_SLICE_ENTITIES);

        // Build flat work slices: each is (archetype_index, start_entity, end_entity).
        let mut work_slices: Vec<(usize, usize, usize)> = Vec::new();
        for (arch_idx, (_, _, _, len)) in self.archetype_ranges.iter().enumerate() {
            let mut chunk_start = 0;
            while chunk_start < *len {
                let chunk_end = (*len).min(chunk_start + min_len);
                work_slices.push((arch_idx, chunk_start, chunk_end));
                chunk_start = chunk_end;
            }
        }

        let pool_threads = rayon::current_num_threads();

        // Choose group count from timing feedback when available.
        let num_groups = if hint_ns > 0 {
            let target =
                (hint_ns / TARGET_GROUP_DURATION_NS).clamp(1, pool_threads as u64) as usize;
            target.min(work_slices.len()).max(1)
        } else {
            pool_threads.min(work_slices.len()).max(1)
        };

        // Pre-assign contiguous groups of slices so every
        // thread processes ALL its work back-to-back.
        let base = work_slices.len() / num_groups;
        let remainder = work_slices.len() % num_groups;
        let mut thread_groups: Vec<Vec<(usize, usize, usize)>> = Vec::with_capacity(num_groups);
        let mut offset = 0;
        for group_idx in 0..num_groups {
            let count = if group_idx < remainder {
                base + 1
            } else {
                base
            };
            if count == 0 {
                break;
            }
            thread_groups.push(work_slices[offset..offset + count].to_vec());
            offset += count;
        }

        crate::profile_message!(
            "rayon parallel iteration prepared: {} pool threads, {} archetype ranges totalling {} entities, sliced into {} work items, grouped into {} thread assignments (system EMA hint {} ns)",
            pool_threads,
            self.archetype_ranges.len(),
            total,
            work_slices.len(),
            thread_groups.len(),
            hint_ns,
        );

        let ranges_ref = &self.archetype_ranges;
        let func_ref = &f;
        let scope_label = self.label;

        // Wrap the scope itself so the whole parallel section appears
        // as a single named zone in Tracy.
        let _zone_scope = if let Some(sys) = scope_label {
            crate::profile_scope!(
                "{} parallel scope",
                sys;
                [("Total entities to process: {}", total), ("Rayon thread pool size: {}", pool_threads)]
            )
        } else {
            crate::profile_scope!(
                "parallel scope",
                [("Total entities to process: {}", total), ("Rayon thread pool size: {}", pool_threads)]
            )
        };

        rayon::scope(|scope| {
            for group in thread_groups {
                scope.spawn(move |_| {
                    for (arch_idx, start, end) in group {
                        let (_, q_state, f_state, _) = &ranges_ref[arch_idx];
                        for index in start..end {
                            if F::matches(f_state, index) {
                                func_ref(Q::fetch_with_state(q_state, index));
                            }
                        }
                    }
                });
            }
        });

        crate::profile_message!(
            "rayon parallel iteration finished: {} entities processed across all work groups",
            total,
        );
    }

    /// Execute the closure on every matching entity, collecting
    /// [`BatchStats`] about how Rayon distributed the work.
    ///
    /// Same adaptive fallback as [`execute_untracked`]: sequential below
    /// `num_threads × 256` entities, flat work slices above.
    ///
    /// Tracking adds per-slice atomics (count, min, max) so the caller
    /// can inspect load distribution.  Each pre-built slice reports
    /// one stat, giving precise visibility into per-thread assignment.
    ///
    /// [`execute_untracked`]: Self::execute_untracked
    fn execute_tracked<Func>(self, f: Func, hint_ns: u64) -> BatchStats
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
            crate::profile_message!(
                "rayon workload below parallel threshold: {} entities is less than {} ({} threads x 256), falling back to sequential iteration on the calling thread",
                total_entities,
                threshold,
                num_threads,
            );
            return BatchStats {
                total_entities,
                batch_count: 1,
                min_batch_size: processed,
                max_batch_size: processed,
                avg_batch_size: processed as f64,
                num_threads,
            };
        }
        let _zone_prepare = crate::profile_scope!(
            "prepare batches",
            [("Total entities to process: {}", total_entities), ("Rayon thread pool size: {}", num_threads)]
        );

        // Parallel path: flat work slices distributed via rayon::scope.
        //
        // Pre-building equal-sized slices and spawning them as scope tasks
        // lets every thread pull work from a shared pool simultaneously,
        // eliminating the work-stealing cascade and late-arriving outliers.
        let min_len = self.min_batch_size.unwrap_or(DEFAULT_SLICE_ENTITIES);
        let system_label = self.label;

        // Build flat work slices: each is (archetype_index, start_entity, end_entity).
        let mut work_slices: Vec<(usize, usize, usize)> = Vec::new();
        for (arch_idx, (_, _, _, len)) in self.archetype_ranges.iter().enumerate() {
            let mut chunk_start = 0;
            while chunk_start < *len {
                let chunk_end = (*len).min(chunk_start + min_len);
                work_slices.push((arch_idx, chunk_start, chunk_end));
                chunk_start = chunk_end;
            }
        }

        // Choose group count from timing feedback when available.
        let num_groups = if hint_ns > 0 {
            let target = (hint_ns / TARGET_GROUP_DURATION_NS).clamp(1, num_threads as u64) as usize;
            target.min(work_slices.len()).max(1)
        } else {
            num_threads.min(work_slices.len()).max(1)
        };

        // Pre-assign contiguous groups so every thread processes
        // ALL its work back-to-back — no per-chunk queue contention,
        // no 90µs gaps between chunks on the same thread.
        let base = work_slices.len() / num_groups;
        let remainder = work_slices.len() % num_groups;
        let mut thread_groups: Vec<Vec<(usize, usize, usize)>> = Vec::with_capacity(num_groups);
        let mut offset = 0;
        for group_idx in 0..num_groups {
            let count = if group_idx < remainder {
                base + 1
            } else {
                base
            };
            if count == 0 {
                break;
            }
            thread_groups.push(work_slices[offset..offset + count].to_vec());
            offset += count;
        }

        let batch_count = Arc::new(AtomicUsize::new(0));
        let min_batch = Arc::new(AtomicUsize::new(usize::MAX));
        let max_batch = Arc::new(AtomicUsize::new(0));
        drop(_zone_prepare);
        let _zone_dist = crate::profile_scope!(
            "start distribution",
            [("Work slices prepared: {}", work_slices.len()), ("Thread groups assigned: {}", thread_groups.len())]
        );

        crate::profile_message!(
            "rayon tracked parallel iteration prepared: {} pool threads, {} total entities across {} work slices, grouped into {} thread assignments (system EMA hint {} ns)",
            num_threads,
            total_entities,
            work_slices.len(),
            thread_groups.len(),
            hint_ns,
        );

        let ranges_ref = &self.archetype_ranges;
        let func_ref = &f;

        // Wrap the scope itself so the whole parallel section appears
        // as a single named zone in Tracy.
        let _zone_scope = if let Some(sys) = system_label {
            crate::profile_scope!(
                "{} parallel scope",
                sys;
                [("Total entities to process: {}", total_entities), ("Rayon thread pool size: {}", num_threads)]
            )
        } else {
            crate::profile_scope!(
                "parallel scope",
                [("Total entities to process: {}", total_entities), ("Rayon thread pool size: {}", num_threads)]
            )
        };

        rayon::scope(|scope| {
            let groups_total = thread_groups.len();
            for (group_idx, group) in thread_groups.into_iter().enumerate() {
                let batch_count = &batch_count;
                let min_batch = &min_batch;
                let max_batch = &max_batch;
                scope.spawn(move |_| {
                    let group_total: usize = group.iter().map(|(_, s, e)| e - s).sum();
                    // One zone per thread-group — all its slices run
                    // contiguously, no inter-chunk gaps on the same thread.
                    let zone = if let Some(sys) = system_label {
                        crate::profile_scope!(
                            "{} group {}/{}",
                            sys,
                            group_idx + 1,
                            groups_total;
                            [("{} entities in this group", group_total)]
                        )
                    } else {
                        crate::profile_scope!(
                            "thread group {}/{}",
                            group_idx + 1,
                            groups_total;
                            [("{} entities in this group", group_total)]
                        )
                    };
                    zone.text(format_args!("thread {:?}", std::thread::current().id()));

                    let mut processed = 0usize;
                    for &(arch_idx, start, end) in &group {
                        let (_, q_state, f_state, _) = &ranges_ref[arch_idx];
                        for index in start..end {
                            if F::matches(f_state, index) {
                                func_ref(Q::fetch_with_state(q_state, index));
                            }
                            processed += 1;
                        }
                    }

                    zone.text(format_args!("{} processed", processed));

                    batch_count.fetch_add(1, Ordering::Relaxed);
                    min_batch.fetch_min(processed, Ordering::Relaxed);
                    max_batch.fetch_max(processed, Ordering::Relaxed);
                });
            }
        });

        let batch_count = batch_count.load(Ordering::Relaxed);
        let min_batch_size = min_batch.load(Ordering::Relaxed);
        let max_batch_size = max_batch.load(Ordering::Relaxed);

        crate::profile_message!(
            "rayon tracked parallel iteration completed: {} total entities distributed across {} rayon tasks (pool has {} threads), batch sizes min {} max {} avg {:.1} entities per task",
            total_entities,
            batch_count,
            num_threads,
            min_batch_size,
            max_batch_size,
            if batch_count > 0 {
                total_entities as f64 / batch_count as f64
            } else {
                0.0
            }
        );

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
