// ----------------------------------------------------------------------------
// Centralised Configuration Constants
// ----------------------------------------------------------------------------
//! All tunable constants live here so they can be adjusted without hunting
//! through source files.  Re-exported at the crate root so module paths
//! stay short (e.g. `crate::config::ParallelIteratorConfig::DEFAULT_ENTITIES_PER_SLICE`).

// ----------------------------------------------------------------------------
// Tracy Profiler
// ----------------------------------------------------------------------------

pub struct ProfilingConfig;

impl ProfilingConfig {
    /// Sampling rate for `tracy_client::ProfiledAllocator`.
    ///
    /// `1` = track every allocation (complete picture, higher overhead).
    /// `10` = track 1 in 10 allocations (good balance).
    /// `100` = track 1 in 100 allocations (minimal overhead, statistical).
    pub const MEMORY_ALLOCATIONS_SAMPLING_FREQUENCY: u16 = 10;
}
// ----------------------------------------------------------------------------
// Parallel iteration — group sizing
// ----------------------------------------------------------------------------

/// Zero-sized struct grouping configuration constants for the parallel iterator.
///
/// Access via `crate::config::ParallelIteratorConfig::<CONSTANT>`.
pub struct ParallelIteratorConfig;

impl ParallelIteratorConfig {
    /// Target wall-clock duration per parallel group (nanoseconds).
    ///
    /// The timing-feedback loop divides the system's average execution time
    /// by this value to determine how many Rayon tasks to spawn.  Larger
    /// values mean fewer, bigger groups — less wake-up scatter but also
    /// less parallelism.  50 µs is a sweet spot where OS thread wake-up
    /// latency (~10 µs) doesn't dominate.
    pub const TARGET_WORK_GROUP_DURATION: u64 = 50_000;

    /// Smoothing factor for the exponential moving average of system
    /// execution time.  `1/32 ≈ 0.031` gives a ~32-frame averaging window,
    /// damping frame-to-frame jitter.
    pub const TIMING_EMA_WINDOW: i64 = 32;

    /// Default entities per parallel work slice.  Sized so one slice fits
    /// in L1 data cache for components up to 8 bytes (32 KiB / 8 B = 4096).
    /// For the common `f32` component this is half-filling L1 — plenty of
    /// room for filter state and adjacent cache lines.
    pub const DEFAULT_ENTITIES_PER_SLICE: usize = 4096;

    /// Minimum entities per thread before parallel execution kicks in.
    ///
    /// Below `num_threads × MINIMUM_SLICE_SIZE` total entities, the
    /// iterator falls back to a sequential loop — Rayon task-spawning
    /// overhead would dominate the actual work.
    pub const MINIMUM_SLICE_SIZE: usize = 256;
}

// ----------------------------------------------------------------------------
// Entity builder — pre-allocation
// ----------------------------------------------------------------------------

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

// ----------------------------------------------------------------------------
// Query internals — pre-allocation
// ----------------------------------------------------------------------------

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
