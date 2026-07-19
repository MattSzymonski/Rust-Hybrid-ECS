// ----------------------------------------------------------------------------
// Centralised Configuration Constants
// ----------------------------------------------------------------------------
//! All tunable constants live here so they can be adjusted without hunting
//! through source files.  Re-exported at the crate root so module paths
//! stay short (e.g. `crate::config::ParallelProcessingConfig::DEFAULT_ITERATOR_SLICE_SIZE`).

// ----------------------------------------------------------------------------
// Parallel slice size
// ----------------------------------------------------------------------------

/// Default number of entities per parallel work slice, clamped by component size.
///
/// For small components (≤8 B): uses the full default (4096).
/// For large components: scales down to keep the working set reasonable,
/// with a floor of `MINIMUM_SLICE_SIZE` (256) to avoid per-slice overhead.
///
/// Set the `ECS_SLICE_SIZE` environment variable to override at runtime.
pub fn default_entities_per_slice(bytes_per_entity: usize) -> usize {
    if let Ok(val) = std::env::var("ECS_SLICE_SIZE") {
        if let Ok(n) = val.parse::<usize>() {
            if n > 0 {
                return n;
            }
        }
    }
    let default = ParallelProcessingConfig::DEFAULT_ITERATOR_SLICE_SIZE;
    let min = ParallelProcessingConfig::MINIMUM_SLICE_SIZE;
    // Scale: keep total data per slice constant (~32 KiB for 8 B baseline).
    // bytes_per_entity is already clamped to at least 8 in the caller.
    (default * 8 / bytes_per_entity).clamp(min, default)
}

// ----------------------------------------------------------------------------
// System hardware detection
// ----------------------------------------------------------------------------

/// Print a summary of detected system hardware to stdout.
///
/// Includes CPU, core/thread count, RAM, swap, disks, OS, and uptime.
/// GPU detection requires a rendering backend.
///
/// Called during [`Engine::new`](crate::Engine::new).
pub fn print_system_specs() {
    use sysinfo::{Disks, System};

    let system = System::new_all();
    let disks = Disks::new_with_refreshed_list();

    let total_ram_gb = system.total_memory() as f64 / (1024.0 * 1024.0 * 1024.0);
    let used_ram_gb =
        (system.total_memory() - system.available_memory()) as f64 / (1024.0 * 1024.0 * 1024.0);
    let ram_pct = if system.total_memory() > 0 {
        (used_ram_gb / total_ram_gb) * 100.0
    } else {
        0.0
    };

    let total_swap_gb = system.total_swap() as f64 / (1024.0 * 1024.0 * 1024.0);
    let used_swap_gb = system.used_swap() as f64 / (1024.0 * 1024.0 * 1024.0);
    let swap_pct = if total_swap_gb > 0.0 {
        (used_swap_gb / total_swap_gb) * 100.0
    } else {
        0.0
    };

    let cpu_name = system
        .cpus()
        .first()
        .map(|cpu| cpu.brand().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let physical_cores = system.physical_core_count().unwrap_or(0);
    let logical_threads = system.cpus().len();

    let uptime_secs = System::uptime();
    let uptime_str = format_uptime(uptime_secs);

    println!();
    println!("System specs");
    println!("├─ CPU: {}", cpu_name);
    println!(
        "│  └─ Cores: {} physical, {} logical threads",
        physical_cores, logical_threads
    );
    println!("├─ Memory");
    println!(
        "│  ├─ RAM: {:.1} / {:.1} GiB used ({:.0}%)",
        used_ram_gb, total_ram_gb, ram_pct
    );
    if total_swap_gb > 0.0 {
        println!(
            "│  └─ Swap: {:.1} / {:.1} GiB used ({:.0}%)",
            used_swap_gb, total_swap_gb, swap_pct
        );
    } else {
        println!("│  └─ Swap: none");
    }
    print_disk_info(&disks);
    println!("├─ OS: {}", os_pretty_name());
    println!("│  └─ Uptime: {}", uptime_str);
    println!("└─ GPU: use external tools (dxdiag / lspci)");
    println!();
}

// ----------------------------------------------------------------------------
// Parallel execution configuration
// ----------------------------------------------------------------------------

/// Print the active parallel-iterator configuration to stdout.
///
/// Shows the Rayon thread-pool size and every tunable in
/// [`ParallelProcessingConfig`].
///
/// Called after [`print_system_specs`] during [`Engine::new`](crate::Engine::new).
pub fn print_parallel_config() {
    let threads = rayon::current_num_threads();

    println!("Parallel execution config");
    println!("├─ Rayon threads: {}", threads);
    println!(
        "├─ Target work-group duration: {} µs",
        ParallelProcessingConfig::TARGET_ITERATOR_WORK_GROUP_DURATION / 1000
    );
    println!(
        "├─ Splitting-hint averaging window: {} frames",
        ParallelProcessingConfig::SPLITTING_HINT_WINDOW
    );
    println!(
        "├─ Default entities per slice: {}",
        default_entities_per_slice(8)
    );
    println!(
        "└─ Minimum slice size: {}",
        ParallelProcessingConfig::MINIMUM_SLICE_SIZE
    );
    println!();
}

// ----------------------------------------------------------------------------
// Memory / disk / uptime helpers
// ----------------------------------------------------------------------------

fn print_disk_info(disks: &sysinfo::Disks) {
    if disks.is_empty() {
        println!("├─ Storage: no disks detected");
        return;
    }
    println!("├─ Storage");
    let count = disks.len();
    for (i, disk) in disks.iter().enumerate() {
        let total_gb = disk.total_space() as f64 / (1024.0 * 1024.0 * 1024.0);
        let avail_gb = disk.available_space() as f64 / (1024.0 * 1024.0 * 1024.0);
        let used_gb = total_gb - avail_gb;
        let pct = if total_gb > 0.0 {
            (used_gb / total_gb) * 100.0
        } else {
            0.0
        };
        let kind = match disk.kind() {
            sysinfo::DiskKind::SSD => "SSD",
            sysinfo::DiskKind::HDD => "HDD",
            _ => "?",
        };
        let mount = disk.mount_point().to_string_lossy();
        // Disk name can be long; just show mount point.
        let branch = if i == count - 1 { "└─" } else { "├─" };
        println!(
            "│  {} {}  {:.0} / {:.0} GiB used ({:.0}%)  [{}]",
            branch, mount, used_gb, total_gb, pct, kind
        );
    }
}

fn format_uptime(seconds: u64) -> String {
    if seconds == 0 {
        return "unknown".to_string();
    }
    let days = seconds / 86400;
    let hours = (seconds % 86400) / 3600;
    let minutes = (seconds % 3600) / 60;
    if days > 0 {
        format!("{}d {}h {}m", days, hours, minutes)
    } else if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else {
        format!("{}m", minutes)
    }
}

/// Human-readable OS name.
fn os_pretty_name() -> String {
    let name = sysinfo::System::name().unwrap_or_else(|| "unknown".to_string());
    let version = sysinfo::System::os_version().unwrap_or_default();
    let arch = std::env::consts::ARCH;
    if version.is_empty() {
        format!("{name} ({arch})")
    } else {
        format!("{name} {version} ({arch})")
    }
}

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
    /// values mean fewer, bigger groups — less wake-up scatter but also
    /// less parallelism.  50 µs is a sweet spot where OS thread wake-up
    /// latency (~10 µs) doesn't dominate.
    pub const TARGET_ITERATOR_WORK_GROUP_DURATION: u64 = 50_000;

    /// Default entities per parallel work slice.  Sized so one slice fits
    /// in L1 data cache for components up to 8 bytes (32 KiB / 8 B = 4096).
    /// For the common `f32` component this is half-filling L1 — plenty of
    /// room for filter state and adjacent cache lines.
    pub const DEFAULT_ITERATOR_SLICE_SIZE: usize = 6144;

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
