//! Centralised configuration constants and the parallel-execution report.
//!
//! # Responsibilities
//!
//! - Defines all tunable constants (slice sizes, thread counts, profiling knobs).
//! - Provides `default_entities_per_slice()` for adaptive parallel work sizing.
//! - Reports the parallel-execution configuration at engine startup.
//!
//! # Design
//!
//! All magic numbers live here so they can be adjusted without hunting through
//! source files. The [`ParallelProcessingConfig`] struct groups related knobs;
//! free functions handle slice-size computation and the startup report.
//! Re-exported at the crate root via `crate::config`.
//!
//! The startup report (`print_parallel_config`) emits
//! through the tracing stack under `telemetry_target::SYSTEM` so they can be
//! filtered, redirected to the file lane, or silenced like any other engine
//! output. It is one multi-line event (`log_block`), which the
//! terminal formatter prints as a block below its time, level and target. The
//! system hardware report lives in `pill_runtime` (its `system_specs` module),
//! so `sysinfo` stays out of the shared engine dylib.

// External crates
use pill_core::info;
use pill_core::rayon;
use pill_core::telemetry::log_block;

// =============================================================================
// ProfilingConfig
// =============================================================================

/// Zero-sized struct grouping profiling-related configuration constants.
///
/// Holds the allocation-sampling frequency consumed by the Tracy profiler
/// integration; kept in its own type so callers can reference the knob via
/// `crate::config::ProfilingConfig::MEMORY_ALLOCATIONS_SAMPLING_FREQUENCY`.
pub struct ProfilingConfig;

impl ProfilingConfig {
    /// Sampling rate for `tracy_client::ProfiledAllocator`.
    ///
    /// `1` = track every allocation (complete picture, higher overhead).
    /// `10` = track 1 in 10 allocations (good balance).
    /// `100` = track 1 in 100 allocations (minimal overhead, statistical).
    pub const MEMORY_ALLOCATIONS_SAMPLING_FREQUENCY: u16 = 10;
}

// =============================================================================
// ParallelProcessingConfig
// =============================================================================

// Frame
//  └─ Scheduler BATCH ("run systems batch 1/2")
//      ├─ System: movement   ──┐
//      ├─ System: health_decay ─┤ these run concurrently in the batch
//      └─ System: cleanup    ──┘
//           │
//           └─ par_iter_mut().for_each()
//                │
//                ├─ iterator_slices (4096 entities each)  ← ITERATOR_PARALLEL_DEFAULT_ENTITIES_PER_SLICE
//                │   iterator_slice 0: entities 0..4096
//                │   iterator_slice 1: entities 4096..8192
//                │   ...
//                │
//                └─ iterator_work_groups (1 per rayon task)  ← ITERATOR_PARALLEL_TARGET_WORK_GROUP_DURATION
//                    iterator_work_group 0: iterator_slices [0,1,2,3]  → rayon task 0
//                    iterator_work_group 1: iterator_slices [4,5,6,7]  → rayon task 1

/// Zero-sized struct grouping configuration constants for the parallel iterator.
///
/// Access via `crate::config::ParallelProcessingConfig::<CONSTANT>`.
pub struct ParallelProcessingConfig;

impl ParallelProcessingConfig {
    /// Smoothing factor for the exponential moving average of system
    /// execution time.  `1/32 ≈ 0.031` gives a ~32-frame averaging window,
    /// damping frame-to-frame jitter.
    pub const SPLITTING_HINT_WINDOW: i64 = 32;

    /// Target wall-clock duration per parallel group (nanoseconds).
    ///
    /// The timing-feedback loop divides the system's average execution time
    /// by this value to determine how many Rayon tasks to spawn.  Larger
    /// values mean fewer, bigger groups - less wake-up scatter but also
    /// less parallelism.  50 µs is a sweet spot where OS thread wake-up
    /// latency (~10 µs) doesn't dominate.
    pub const TARGET_ITERATOR_WORK_GROUP_DURATION: u64 = 50_000;

    /// Most parallel work groups per pool thread.
    ///
    /// More groups than threads lets a long loop balance itself when the
    /// threads do not run equally fast (efficiency cores on a hybrid CPU, two
    /// hyperthreads on one core): the faster threads take the groups the
    /// slower ones have not reached yet. Measured on an i7-12700KF (8
    /// performance cores with hyperthreading plus 4 efficiency cores) with one
    /// group per thread, the threads were idle a third of every loop, waiting
    /// for the group on the slowest one.
    pub const WORK_GROUPS_PER_THREAD: usize = 4;

    /// Default entities per parallel work slice.
    ///
    /// This is an arbitrary but well-tested starting point.  Benchmarked
    /// values between 256 and 50000 show no measurable difference for
    /// standard workloads - the streaming access pattern and hardware
    /// prefetching make the exact number non-critical.  The clamped
    /// formula (see [`default_entities_per_slice`]) scales this down
    /// per-query for unusually large components.
    pub const DEFAULT_ITERATOR_SLICE_SIZE: usize = 4096;

    /// Minimum entities per thread before parallel execution kicks in.
    ///
    /// Below `num_threads × MINIMUM_SLICE_SIZE` total entities, the
    /// iterator falls back to a sequential loop - Rayon task-spawning
    /// overhead would dominate the actual work.
    pub const MINIMUM_SLICE_SIZE: usize = 256;
}

// =============================================================================
// EntityBuilderConfig
// =============================================================================

/// Zero-sized struct grouping configuration constants for entity builders.
pub struct EntityBuilderConfig;

impl EntityBuilderConfig {
    /// Initial `Vec::with_capacity` for the component list in
    /// `EntityBuilder` and `DeferredEntityBuilder`.
    ///
    /// Most entities carry 3–8 components.  Pre-allocating avoids
    /// reallocation during chained `.with()` calls.
    pub const DEFAULT_COMPONENTS_CAPACITY: usize = 8;
}

// =============================================================================
// QueryConfig
// =============================================================================

/// Zero-sized struct grouping configuration constants for query internals.
pub struct QueryConfig;

impl QueryConfig {
    /// Initial `Vec::with_capacity` for component-ID and filter-pair
    /// collections inside query-target and filter-tuple macros.
    ///
    /// Typical queries use 1–4 components/filters, so 4 avoids
    /// reallocation for the common case.
    pub const DEFAULT_TUPLE_COMPONENT_IDS_CAPACITY: usize = 4;

    /// Initial `Vec::with_capacity` for filter-pair combinations in
    /// `Or` filter expansions.
    pub const DEFAULT_FILTER_PAIRS_CAPACITY: usize = 4;
}

// =============================================================================
// Free Functions
// =============================================================================

/// Default number of entities per parallel work slice, clamped by component size.
///
/// Returns a value between [`MINIMUM_SLICE_SIZE`] (256) and
/// [`DEFAULT_ITERATOR_SLICE_SIZE`] (4096).  The slice scales inversely
/// with the total bytes per entity so that larger components get smaller
/// slices, keeping per-slice overhead bounded.
///
/// Set the `ECS_SLICE_SIZE` environment variable to override at runtime.
///
/// # Examples
///
/// ```
/// use pill_engine::config::default_entities_per_slice;
///
/// // Larger components yield smaller slices, keeping per-slice byte volume bounded.
/// let small_components = default_entities_per_slice(8);
/// let large_components = default_entities_per_slice(64);
/// assert!(large_components <= small_components);
/// ```
pub fn default_entities_per_slice(bytes_per_entity: usize) -> usize {
    // Step 1: Honour a runtime override from the `ECS_SLICE_SIZE` environment variable.
    if let Ok(val) = std::env::var("ECS_SLICE_SIZE") {
        if let Ok(n) = val.parse::<usize>() {
            if n > 0 {
                return n;
            }
        }
    }

    // Step 2: Fall back to the size-scaled default, clamped to the configured range.
    let default = ParallelProcessingConfig::DEFAULT_ITERATOR_SLICE_SIZE;
    let min = ParallelProcessingConfig::MINIMUM_SLICE_SIZE;
    // Scale: keep total data per slice constant (~32 KiB for 8 B baseline).
    // bytes_per_entity is already clamped to at least 8 in the caller.
    (default * 8 / bytes_per_entity).clamp(min, default)
}

/// Report the active parallel-iterator configuration through the telemetry stack.
///
/// Shows the Rayon thread-pool size and every tunable in
/// [`ParallelProcessingConfig`], under the same `telemetry_target::SYSTEM`
/// target as the runtime's system-specs report.
///
/// Called during [`Engine::new`](crate::Engine::new).
pub fn print_parallel_config() {
    let threads = rayon::current_num_threads();
    let lines = [
        format!("├─ Rayon threads: {threads}"),
        format!(
            "├─ Target work-group duration: {} µs",
            ParallelProcessingConfig::TARGET_ITERATOR_WORK_GROUP_DURATION / 1000
        ),
        format!(
            "├─ Splitting-hint averaging window: {} frames",
            ParallelProcessingConfig::SPLITTING_HINT_WINDOW
        ),
        format!(
            "├─ Default entities per slice: {}",
            default_entities_per_slice(8)
        ),
        format!(
            "└─ Minimum slice size: {}",
            ParallelProcessingConfig::MINIMUM_SLICE_SIZE
        ),
    ];
    info!(
        target: pill_core::telemetry::telemetry_target::SYSTEM,
        "{}",
        log_block("Parallel execution config", lines)
    );
}
