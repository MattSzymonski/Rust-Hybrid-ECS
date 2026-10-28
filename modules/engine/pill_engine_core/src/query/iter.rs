//! Sequential and parallel iterators produced by [`Query`](super::Query).
//!
//! # Responsibilities
//!
//! - Implements [`QueryIterMut`] for sequential, single-threaded iteration.
//! - Implements [`ParQueryIter`] for Rayon-based parallel iteration with work stealing.
//! - Provides [`BatchStats`] for inspecting parallel work distribution across threads.
//!
//! # Design
//!
//! Both iterators walk the query's cached matching-archetype list. Sequential
//! iteration visits archetypes in order; parallel iteration slices the entity
//! range into Rayon work units. Per-archetype component pointers are cached
//! as raw pointers to avoid repeated HashMap lookups during iteration.

// Standard library
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// External crates
use pill_core::rayon;

// Current crate
use super::filter::QueryFilter;
use super::target::QueryTarget;
use super::FilteredArchetypeRange;
use crate::archetype::ArchetypeId;
use crate::component::Tick;
use crate::config;
use crate::world::World;

// =============================================================================
// BatchStats
// =============================================================================

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

// =============================================================================
// QueryIterMut (Sequential)
// =============================================================================

/// Sequential iterator for mutable queries.
///
/// Walks the query's cached matching-archetype list in order, visiting every
/// row of each archetype while the filter admits it. Component storage
/// pointers and filter state are cached per archetype to avoid repeated
/// lookups during the hot iteration loop.
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
            [(
                "Matching archetype ranges to iterate: {}",
                matching_archetypes.len()
            )]
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

    /// Advance to the next matching archetype, caching its component storage
    /// pointers and filter state.
    ///
    /// Returns `None` once every matching archetype has been visited. Kept
    /// cold via `#[inline(never)]` so the hot path in [`Iterator::next`]
    /// stays small for better branch prediction.
    #[inline(never)]
    fn advance_archetype(&mut self) -> Option<()> {
        // Step 1: check whether every matching archetype has been visited.
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

        // SAFETY: `world_ptr` was created from a valid `&mut World` in
        // [`Query::iter_mut`] and this iterator holds exclusive access to that
        // world for its lifetime `'w`, so the reference derived here is valid
        // and never aliased. Every `archetype_id` comes from the cached
        // matching-archetype list populated from live archetypes, so the
        // unchecked indexing is in bounds; the `HashMap` lookup may still fail
        // and is handled by `?`. `init_state` caches raw pointers to component
        // storage that remain valid because the world is exclusively borrowed
        // and archetypes (and their storage vectors) are neither moved nor
        // reallocated while the iterator is alive. References yielded from
        // these cached pointers never outlive the iterator itself.
        unsafe {
            let _zone_pointers = crate::profile_scope!(
                "get world and archetype id",
                [(
                    "Advancing to archetype: {}/{}",
                    self.current_archetype_idx,
                    self.matching_archetypes.len()
                )]
            );
            let world = &mut *self.world_ptr;
            let archetype_id = self.matching_archetypes[self.current_archetype_idx];
            drop(_zone_pointers);

            let _zone_archetype = crate::profile_scope!("get archetype");
            let archetype = world.archetypes.get_mut(&archetype_id)?;
            _zone_archetype.text_lazy(|| archetype.get_archetype_info(&world.component_registry));
            drop(_zone_archetype);

            let _zone_cache = crate::profile_scope!(
                "cache storage pointers",
                [("Entities in this archetype: {}", archetype.len())]
            );

            // Cache archetype length and component storage pointers. The
            // target takes the archetype mutably (it may look up a mutable
            // column) and the filter takes it shared; the two calls run one
            // after the other, so reborrowing `archetype` covers both.
            self.current_archetype_len = archetype.len();
            self.current_state = Some(Q::init_state(archetype, self.this_run));
            self.current_filter_state =
                Some(F::init_state(archetype, self.last_run, self.this_run));
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
            // Step 1: hot path - iterate rows within the current archetype.
            while self.current_entity_idx < self.current_archetype_len {
                let index = self.current_entity_idx;
                self.current_entity_idx += 1;

                // SAFETY: `current_state` is always `Some` while iterating in
                // the hot path; it is only reset when advancing archetypes,
                // which happens on the cold path below.
                let state = unsafe { self.current_state.as_ref().unwrap_unchecked() };

                if !F::ACCEPTS_ALL {
                    // SAFETY: `current_filter_state` is `Some` whenever
                    // `current_state` is, because both are initialized together
                    // in `advance_archetype` and only ever cleared there.
                    let filter_state =
                        unsafe { self.current_filter_state.as_ref().unwrap_unchecked() };
                    if !F::matches(filter_state, index) {
                        continue;
                    }
                }

                return Some(Q::fetch_with_state(state, index));
            }

            // Step 2: cold path - advance to the next archetype.
            self.advance_archetype()?;
        }
    }
}

// =============================================================================
// ParQueryIter (Parallel)
// =============================================================================

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
    /// Sum of all queried component sizes - the per-entity byte cost the
    /// default slice size is derived from.
    total_components_size: usize,
    /// Optional label for Tracy zones - set via `.label("system_name")`.
    label: Option<&'static str>,
    /// Per-label splitting hint timing map, shared via Arc<Mutex<>> for thread-safe access.
    iterator_timings: std::sync::Arc<std::sync::Mutex<crate::world::IteratorTimings>>,
    _phantom: std::marker::PhantomData<&'w mut (Q, F)>,
}

// SAFETY: `ParQueryIter` can be moved between threads because
// `archetype_ranges` holds owned data plus the raw pointers cached in
// `Q::State` and `F::State`, which point into component storage that stays
// valid for the query's lifetime (the iterator holds exclusive `World`
// access, so archetypes and their storage are neither reallocated nor
// aliased). Parallel execution assigns each thread a disjoint entity range,
// and the `Send` bounds below require the pointed-to data to be movable
// between threads, so no data race is possible.
//
// That the *systems* sharing a batch are themselves disjoint is established by
// `SystemAccess::conflicts_with`, not here; see its contract for the
// mask-completeness rule that makes it trustworthy.
unsafe impl<'w, Q: QueryTarget, F: QueryFilter> Send for ParQueryIter<'w, Q, F>
where
    Q::State: Send,
    F::State: Send,
{
}
// SAFETY: `ParQueryIter` is `Sync` because the raw pointers in `Q::State` and
// `F::State` reference component storage that is exclusively borrowed from the
// `World` for the iterator's lifetime, and the `Sync` bounds below ensure the
// pointed-to data is safe to share across threads. Each thread reads disjoint
// entity indices, so concurrent access through the shared state never races.
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
        total_components_size: usize,
    ) -> Self {
        let _zone = crate::profile_scope!(
            "create parallel query iterator",
            [(
                "Matching archetype ranges for parallel iteration: {}",
                archetype_ranges.len()
            )]
        );
        Self {
            archetype_ranges,
            min_batch_size: None,
            tracked: false,
            total_components_size,
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
            .map(|range| range.entity_count)
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
    ///
    /// The batch size is the divisor the executors split work slices with, so
    /// a zero becomes one entity per slice rather than a division by zero: a
    /// batch size computed from a frame's entity count can legitimately come
    /// out as zero, and that must not end the process mid-frame.
    pub fn with_batch_size(mut self, size: usize) -> Self {
        self.min_batch_size = Some(size.max(1));
        self
    }

    /// Enable batch statistics tracking.
    pub fn tracked(mut self) -> Self {
        self.tracked = true;
        self
    }

    /// Execute closure on each entity in parallel.
    ///
    /// Returns [`BatchStats`] when `.tracked()` was called, `None` otherwise.
    pub fn for_each<Func>(self, f: Func) -> Option<BatchStats>
    where
        Func: Fn(Q::Item<'_>) + Send + Sync,
    {
        // Step 1: resolve the per-label splitting hint recorded by prior runs.
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

        // Step 2: capture shared state before `self` is moved into the execute_* methods.
        let label = self.label;
        let iterator_timings = std::sync::Arc::clone(&self.iterator_timings);

        // Step 3: plan the dispatch once, then run it - sequentially below the
        // parallel threshold, or as the plan's groups across the pool.
        let started = pill_core::platform::Instant::now();
        let stats = match self.plan(hint_ns) {
            WorkPlan::Sequential {
                total_entities,
                threshold,
            } => {
                let processed = self.run_sequential(&f, total_entities, threshold);
                self.tracked.then(|| BatchStats {
                    num_threads: rayon::current_num_threads(),
                    batch_count: 1,
                    total_entities,
                    min_batch_size: processed,
                    max_batch_size: processed,
                    avg_batch_size: processed as f64,
                })
            }
            WorkPlan::Parallel(plan) => {
                if self.tracked {
                    Some(self.execute_tracked(&f, plan))
                } else {
                    self.execute_untracked(&f, plan);
                    None
                }
            }
        };
        let elapsed_ns = started.elapsed().as_nanos() as u64;

        // Step 4: record the measured duration as the splitting hint for this
        // label, and track duplicate labels for diagnostics.
        if let Some(label) = label {
            if let Ok(mut timing) = iterator_timings.lock() {
                // Looked up by the borrowed text and stored as a copy: the
                // label lives in the calling module's DLL, the timings in
                // the world, which outlives it (see `IteratorTimings`).
                let durations = &mut timing.per_iterator_label_average_duration;
                let entry = match durations.get_mut(label) {
                    Some(entry) => entry,
                    None => durations.entry(label.to_owned()).or_insert(elapsed_ns),
                };
                let delta = elapsed_ns as i64 - *entry as i64;
                *entry = (*entry as i64
                    + delta / config::ParallelProcessingConfig::SPLITTING_HINT_WINDOW)
                    as u64;
                if timing
                    .visited_iterator_labels
                    .iter()
                    .any(|visited| visited == label)
                {
                    timing
                        .visited_duplicated_iterator_labels
                        .push(label.to_owned());
                } else {
                    timing.visited_iterator_labels.push(label.to_owned());
                }
            }
        }

        stats
    }

    /// Decides how one dispatch runs.
    ///
    /// The one place the parallel threshold, the slice size and the work-group
    /// count are decided: below `pool threads × MINIMUM_SLICE_SIZE` the rows run
    /// on the calling thread, otherwise they become equal flat slices grouped
    /// into contiguous ranges that [`rayon::scope`] spawns all at once. Both
    /// executors read the plan this returns, so they cannot disagree about the
    /// split.
    fn plan(&self, hint_ns: u64) -> WorkPlan {
        let pool_threads = rayon::current_num_threads();
        let total_entities: usize = self
            .archetype_ranges
            .iter()
            .map(|range| range.entity_count)
            .sum();
        let threshold = pool_threads * config::ParallelProcessingConfig::MINIMUM_SLICE_SIZE;

        // Adaptive fallback - run sequentially for tiny workloads, where the
        // overhead of spawning Rayon tasks exceeds the work itself, especially
        // with many tiny archetypes.
        if total_entities < threshold {
            return WorkPlan::Sequential {
                total_entities,
                threshold,
            };
        }

        let _zone = crate::profile_scope!(
            "prepare batches",
            [
                ("Total entities to process: {}", total_entities),
                ("Rayon thread pool size: {}", pool_threads)
            ]
        );

        // Step 1: build flat work slices.
        //
        // Pre-build equal-sized entity-range slices, then group them into
        // contiguous chunks. Spawning all chunks at once through rayon::scope
        // lets every thread pull tasks from a shared pool at the same time - no
        // work-stealing cascade, no head-start for the owning thread, no
        // late-arriving outliers.
        let min_len = self
            .min_batch_size
            .unwrap_or(config::default_entities_per_slice(
                self.total_components_size,
            ));

        // Each flat work slice is an (archetype_index, start_entity, end_entity)
        // range; precompute the capacity so the Vec never reallocates on push.
        let slice_count: usize = self
            .archetype_ranges
            .iter()
            .map(|range| range.entity_count.div_ceil(min_len))
            .sum();
        let mut slices: Vec<(usize, usize, usize)> = Vec::with_capacity(slice_count);
        for (arch_idx, range) in self.archetype_ranges.iter().enumerate() {
            let mut chunk_start = 0;
            while chunk_start < range.entity_count {
                let chunk_end = range.entity_count.min(chunk_start + min_len);
                slices.push((arch_idx, chunk_start, chunk_end));
                chunk_start = chunk_end;
            }
        }

        // Step 2: choose the work-group count from timing feedback when
        // available, then pre-assign contiguous groups of slices so every
        // thread processes ALL its work back-to-back. Groups are stored as
        // (start_index, count) ranges into the flat slice list instead of
        // copying slices with to_vec().
        let group_count = iterator_work_group_count(hint_ns, pool_threads, slices.len());
        let base = slices.len() / group_count;
        let remainder = slices.len() % group_count;
        let mut groups: Vec<(usize, usize)> = Vec::with_capacity(group_count);
        let mut offset = 0;
        for group_index in 0..group_count {
            let count = if group_index < remainder {
                base + 1
            } else {
                base
            };
            if count == 0 {
                break;
            }
            groups.push((offset, count));
            offset += count;
        }

        crate::profile_message!(
            "rayon parallel iteration prepared: {} pool threads, {} archetype ranges totalling {} entities, sliced into {} work items, grouped into {} thread assignments (system splitting hint {} ns)",
            pool_threads,
            self.archetype_ranges.len(),
            total_entities,
            slices.len(),
            groups.len(),
            hint_ns,
        );

        WorkPlan::Parallel(ParallelPlan {
            slices,
            groups,
            total_entities,
            pool_threads,
        })
    }

    /// Runs the rows on the calling thread - the fallback below the parallel
    /// threshold - and returns how many rows it visited.
    ///
    /// Both modes come through here, so the fallback is reported the same way
    /// whichever result the caller asked for.
    #[cfg_attr(not(feature = "profiling"), allow(unused_variables))]
    fn run_sequential<Func>(&self, f: &Func, total_entities: usize, threshold: usize) -> usize
    where
        Func: Fn(Q::Item<'_>),
    {
        let _zone = crate::profile_scope!("sequential iteration below the parallel threshold");
        crate::profile_message!(
            "rayon workload below parallel threshold: {} entities is less than {} ({} threads x {}), falling back to sequential iteration on the calling thread",
            total_entities,
            threshold,
            rayon::current_num_threads(),
            crate::config::ParallelProcessingConfig::MINIMUM_SLICE_SIZE,
        );

        // The same row walk the parallel groups use, one whole-archetype slice
        // at a time, so the two paths cannot diverge on filtering or fetching.
        let mut visited = 0;
        for (arch_idx, range) in self.archetype_ranges.iter().enumerate() {
            visited += run_group::<Q, F, _>(
                &self.archetype_ranges,
                &[(arch_idx, 0, range.entity_count)],
                f,
            );
        }
        visited
    }

    /// Execute the closure on every matching entity (untracked).
    ///
    /// Runs the plan [`plan`](Self::plan) produced: equal flat work slices -
    /// `with_batch_size` entities each, or else `default_entities_per_slice`,
    /// which `ECS_SLICE_SIZE` overrides and which otherwise clamps to
    /// 256..4096 - grouped into chunks that [`rayon::scope`] spawns all at
    /// once, so every thread pulls work from the shared pool.
    fn execute_untracked<Func>(&self, f: &Func, plan: ParallelPlan)
    where
        Func: Fn(Q::Item<'_>) + Send + Sync,
    {
        let ranges_ref = &self.archetype_ranges;
        let func_ref = f;
        let scope_label = self.label;
        let iterator_slices_ref = &plan.slices;

        // Step 1: wrap the scope itself so the whole parallel section appears
        // as a single named zone in Tracy.
        let _zone_scope = if let Some(sys) = scope_label {
            crate::profile_scope!(
                "{} parallel scope",
                sys;
                [("Total entities to process: {}", plan.total_entities), ("Rayon thread pool size: {}", plan.pool_threads)]
            )
        } else {
            crate::profile_scope!(
                "parallel scope",
                [
                    ("Total entities to process: {}", plan.total_entities),
                    ("Rayon thread pool size: {}", plan.pool_threads)
                ]
            )
        };

        rayon::scope(|scope| {
            let iterator_work_group_total = plan.groups.len();
            for (iterator_work_group_index, &(start_idx, count)) in plan.groups.iter().enumerate() {
                scope.spawn(move |_| {
                    let iterator_work_group_slice =
                        &iterator_slices_ref[start_idx..start_idx + count];
                    // One zone per work group, on the thread that runs it, so
                    // the work shows up on the Rayon workers in Tracy rather
                    // than only as the caller waiting inside the parallel
                    // scope. The name is only formatted while a profiler is
                    // connected.
                    let _zone = if let Some(sys) = scope_label {
                        crate::profile_scope!(
                            "{} group {}/{}",
                            sys,
                            iterator_work_group_index + 1,
                            iterator_work_group_total;
                            [("{} entities in this group", iterator_work_group_slice.iter().map(|(_, start, end)| end - start).sum::<usize>())]
                        )
                    } else {
                        crate::profile_scope!(
                            "thread group {}/{}",
                            iterator_work_group_index + 1,
                            iterator_work_group_total;
                            [("{} entities in this group", iterator_work_group_slice.iter().map(|(_, start, end)| end - start).sum::<usize>())]
                        )
                    };
                    run_group::<Q, F, _>(ranges_ref, iterator_work_group_slice, func_ref);
                });
            }
        });

        crate::profile_message!(
            "rayon parallel iteration finished: {} entities processed across all work groups",
            plan.total_entities,
        );
    }

    /// Execute the closure on every matching entity, collecting
    /// [`BatchStats`] about how Rayon distributed the work.
    ///
    /// Runs the same plan [`execute_untracked`](Self::execute_untracked) runs;
    /// tracking adds per-group atomics (count, min, max) so the caller can
    /// inspect load distribution, and each spawned work group reports one stat.
    fn execute_tracked<Func>(&self, f: &Func, plan: ParallelPlan) -> BatchStats
    where
        Func: Fn(Q::Item<'_>) + Send + Sync,
    {
        let batch_count = Arc::new(AtomicUsize::new(0));
        let min_batch = Arc::new(AtomicUsize::new(usize::MAX));
        let max_batch = Arc::new(AtomicUsize::new(0));
        let _zone_dist = crate::profile_scope!(
            "start distribution",
            [
                ("Work slices prepared: {}", plan.slices.len()),
                ("Thread groups assigned: {}", plan.groups.len())
            ]
        );

        let ranges_ref = &self.archetype_ranges;
        let func_ref = f;
        let system_label = self.label;
        let iterator_slices_ref = &plan.slices;

        // Wrap the scope itself so the whole parallel section appears as a
        // single named zone in Tracy.
        let _zone_scope = if let Some(sys) = system_label {
            crate::profile_scope!(
                "{} parallel scope",
                sys;
                [("Total entities to process: {}", plan.total_entities), ("Rayon thread pool size: {}", plan.pool_threads)]
            )
        } else {
            crate::profile_scope!(
                "parallel scope",
                [
                    ("Total entities to process: {}", plan.total_entities),
                    ("Rayon thread pool size: {}", plan.pool_threads)
                ]
            )
        };

        rayon::scope(|scope| {
            let iterator_work_group_total = plan.groups.len();
            for (iterator_work_group_index, &(start_idx, count)) in plan.groups.iter().enumerate() {
                let batch_count = &batch_count;
                let min_batch = &min_batch;
                let max_batch = &max_batch;
                let iterator_work_group_slice = &iterator_slices_ref[start_idx..start_idx + count];
                scope.spawn(move |_| {
                    // One zone per thread-group - all its slices run
                    // contiguously, no inter-chunk gaps on the same thread.
                    let zone = if let Some(sys) = system_label {
                        crate::profile_scope!(
                            "{} group {}/{}",
                            sys,
                            iterator_work_group_index + 1,
                            iterator_work_group_total;
                            [("{} entities in this group", iterator_work_group_slice.iter().map(|(_, s, e)| e - s).sum::<usize>())]
                        )
                    } else {
                        crate::profile_scope!(
                            "thread group {}/{}",
                            iterator_work_group_index + 1,
                            iterator_work_group_total;
                            [("{} entities in this group", iterator_work_group_slice.iter().map(|(_, s, e)| e - s).sum::<usize>())]
                        )
                    };
                    zone.text(format_args!("thread {:?}", std::thread::current().id()));

                    let processed = run_group::<Q, F, _>(ranges_ref, iterator_work_group_slice, func_ref);

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
            plan.total_entities,
            batch_count,
            plan.pool_threads,
            min_batch_size,
            max_batch_size,
            if batch_count > 0 {
                plan.total_entities as f64 / batch_count as f64
            } else {
                0.0
            }
        );

        BatchStats {
            num_threads: plan.pool_threads,
            batch_count,
            total_entities: plan.total_entities,
            min_batch_size: if batch_count > 0 { min_batch_size } else { 0 },
            max_batch_size,
            avg_batch_size: if batch_count > 0 {
                plan.total_entities as f64 / batch_count as f64
            } else {
                0.0
            },
        }
    }
}

// =============================================================================
// Work planning
// =============================================================================

/// How one parallel dispatch runs, decided once per `for_each`.
enum WorkPlan {
    /// Below the parallel threshold: walk the rows on the calling thread.
    Sequential {
        /// Every row in every matching archetype.
        total_entities: usize,
        /// `pool threads × MINIMUM_SLICE_SIZE`, the parallel threshold.
        threshold: usize,
    },
    /// Split the rows into flat slices and run the groups on the pool.
    Parallel(ParallelPlan),
}

/// One parallel dispatch's slices and the contiguous groups of them.
struct ParallelPlan {
    /// Each slice is an `(archetype index, start row, end row)` range.
    slices: Vec<(usize, usize, usize)>,
    /// Each group is `(offset into `slices`, slice count)`; all its slices run
    /// back-to-back on one Rayon task.
    groups: Vec<(usize, usize)>,
    /// Every row in every matching archetype, before the row filter.
    total_entities: usize,
    /// Pool size the plan was sized for.
    pool_threads: usize,
}

/// Walks one work group's slices and returns how many rows it visited.
///
/// The row loop every dispatch path shares: resolve the archetype's cached
/// states by index, then fetch each row the filter admits.
fn run_group<Q, F, Func>(
    ranges: &[FilteredArchetypeRange<Q::State, F::State>],
    slices: &[(usize, usize, usize)],
    f: &Func,
) -> usize
where
    Q: QueryTarget,
    F: QueryFilter,
    Func: Fn(Q::Item<'_>),
{
    let mut visited = 0;
    for &(arch_idx, start, end) in slices {
        let range = &ranges[arch_idx];
        for index in start..end {
            if F::matches(&range.filter_state, index) {
                f(Q::fetch_with_state(&range.target_state, index));
            }
            visited += 1;
        }
    }
    visited
}

/// How many Rayon tasks to split a parallel iteration into.
///
/// One task per `TARGET_ITERATOR_WORK_GROUP_DURATION` of the iteration's
/// measured time, so short loops are not split into tasks that cost more to
/// schedule than they save. At most `WORK_GROUPS_PER_THREAD` per pool thread
/// and one per slice. Without a measurement yet, one per thread.
///
/// More tasks than threads is what keeps a long loop balanced on a CPU whose
/// threads do not run equally fast: a hybrid CPU's efficiency cores, or two
/// hyperthreads sharing one core. With exactly one equal task per thread, the
/// loop waits for whichever task landed on the slowest thread; with several,
/// the faster threads take the tasks the slower ones have not reached.
fn iterator_work_group_count(hint_ns: u64, pool_threads: usize, slice_count: usize) -> usize {
    let most = pool_threads * config::ParallelProcessingConfig::WORK_GROUPS_PER_THREAD;
    let target = if hint_ns > 0 {
        (hint_ns / config::ParallelProcessingConfig::TARGET_ITERATOR_WORK_GROUP_DURATION) as usize
    } else {
        pool_threads
    };
    target.clamp(1, most.max(1)).min(slice_count).max(1)
}

#[cfg(test)]
mod work_group_count_tests {
    use super::*;

    #[test]
    fn a_long_loop_splits_into_several_tasks_per_thread() {
        // 30 ms of work on 20 threads: as many tasks as allowed, so the fast
        // threads can take over the slow ones' share.
        let most = 20 * config::ParallelProcessingConfig::WORK_GROUPS_PER_THREAD;
        assert_eq!(iterator_work_group_count(30_000_000, 20, 1_000), most);
    }

    #[test]
    fn a_short_loop_keeps_its_tasks_near_the_target_duration() {
        // 200 us of work is four 50 us tasks, however many threads there are.
        assert_eq!(iterator_work_group_count(200_000, 20, 1_000), 4);
        assert_eq!(iterator_work_group_count(10_000, 20, 1_000), 1);
    }

    #[test]
    fn tasks_never_outnumber_slices_and_start_at_one_per_thread() {
        assert_eq!(iterator_work_group_count(30_000_000, 20, 50), 50);
        assert_eq!(iterator_work_group_count(0, 20, 1_000), 20);
        assert_eq!(iterator_work_group_count(0, 20, 0), 1);
    }
}
