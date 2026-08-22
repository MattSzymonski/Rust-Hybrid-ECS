//! Build, link, and hot-reload analytics: an in-process collector that times
//! every phase of the compile → stage → load → init → migrate pipeline and
//! prints a structured report to the host console.
//!
//! # Responsibilities
//!
//! - Time each module's build command, staging copy, DLL load, registration,
//!   and schema migration.
//! - Measure the host process and the cargo child process memory (working set
//!   and peak high-water mark).
//! - Inspect each loaded DLL: artifact size, PE exports, and imported DLLs.
//! - Read each module's direct cargo dependencies from the `.fingerprint` JSON.
//! - Parse cargo `--timings` HTML for per-crate compile+link wall time.
//! - Print a full startup report and a compact line per hot reload.
//!
//! # Design
//!
//! A process-wide collector is used because the build pipeline spans several
//! modules (`build_runner`, `native_library`, `optional_module`,
//! `project_module`) and no single one owns the whole transaction. Records are
//! keyed by module name, which is unique across the project and optional
//! modules. The collector is deliberately best-effort: a missing DLL, a PE
//! that cannot be parsed, or an absent cargo-timings report degrades the
//! report to `-` instead of failing the build.

// Standard library
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, UNIX_EPOCH};

// External crates
use serde_json::Value;

// =============================================================================
// Constants
// =============================================================================

/// Subdirectory, relative to the workspace root, where cargo writes its
/// `--timings` HTML report for every host-driven build.
const CARGO_TIMINGS_DIRECTORY: &str = "target/cargo-timings";

/// Subdirectory, relative to the workspace root, where cargo writes each
/// compiled crate's fingerprint JSON.
const CARGO_FINGERPRINT_DIRECTORY: &str = "target/debug/.fingerprint";

/// Whether a module was rebuilt from scratch or skipped by the up-to-date fast path.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuildStatus {
    /// The artifact was already newer than every input; cargo never ran.
    Fresh,
    /// Cargo ran and produced a new artifact.
    Built,
}

/// Which pipeline a module belongs to; used for the report's `kind` column.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModuleKind {
    /// An optional engine module loaded from its private hot-load copy.
    Optional,
    /// The active project module.
    Project,
}

impl ModuleKind {
    fn label(self) -> &'static str {
        match self {
            ModuleKind::Optional => "optional",
            ModuleKind::Project => "project",
        }
    }
}

// =============================================================================
// PE Inspection
// =============================================================================

/// The interesting parts of a Windows PE executable header.
pub(crate) struct PeInspection {
    /// Named exports from the export directory.
    pub exports: Vec<String>,
    /// DLL names referenced by the import directory.
    pub import_dlls: Vec<String>,
    /// `SizeOfImage` from the optional header, in bytes.
    pub image_size: u64,
}

/// Parse a Windows PE file's export and import directories.
///
/// Returns `None` for anything that is not a PE32/PE32+ image (including
/// `.so` and `.dylib` on other platforms). Pure byte parsing: no OS calls, so
/// it compiles and runs everywhere even though it only understands PE files.
pub(crate) fn inspect_pe(path: &Path) -> Option<PeInspection> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 0x40 || &data[0..2] != b"MZ" {
        return None;
    }
    let e_lfanew = u32::from_le_bytes(data[0x3c..0x40].try_into().ok()?) as usize;
    if e_lfanew + 24 > data.len() || &data[e_lfanew..e_lfanew + 4] != b"PE\0\0" {
        return None;
    }
    let coff = e_lfanew + 4;
    let number_of_sections = u16::from_le_bytes(data[coff + 2..coff + 4].try_into().ok()?) as usize;
    let size_of_optional = u16::from_le_bytes(data[coff + 16..coff + 18].try_into().ok()?) as usize;
    let opt = coff + 20;
    if opt + size_of_optional > data.len() {
        return None;
    }
    let magic = u16::from_le_bytes(data[opt..opt + 2].try_into().ok()?);
    // The data-directory table starts at a different offset in PE32 vs PE32+;
    // `SizeOfImage` sits at offset 56 in both.
    let data_directory_offset = match magic {
        0x10b => opt + 96,
        0x20b => opt + 112,
        _ => return None,
    };
    let image_size = u32::from_le_bytes(data[opt + 56..opt + 60].try_into().ok()?) as u64;
    if data_directory_offset + 16 > data.len() {
        return None;
    }
    let export_rva = u32::from_le_bytes(
        data[data_directory_offset..data_directory_offset + 4]
            .try_into()
            .ok()?,
    );
    let export_size = u32::from_le_bytes(
        data[data_directory_offset + 4..data_directory_offset + 8]
            .try_into()
            .ok()?,
    );
    let import_rva = u32::from_le_bytes(
        data[data_directory_offset + 8..data_directory_offset + 12]
            .try_into()
            .ok()?,
    );
    let import_size = u32::from_le_bytes(
        data[data_directory_offset + 12..data_directory_offset + 16]
            .try_into()
            .ok()?,
    );

    // Map an RVA to a file offset through the section table.
    let sections = coff + 20 + size_of_optional;
    let rva_to_offset = |rva: u32| -> Option<usize> {
        if rva == 0 {
            return None;
        }
        for index in 0..number_of_sections {
            let section = sections + index * 40;
            if section + 40 > data.len() {
                break;
            }
            let virtual_size = u32::from_le_bytes(data[section + 8..section + 12].try_into().ok()?);
            let virtual_address =
                u32::from_le_bytes(data[section + 12..section + 16].try_into().ok()?);
            let raw_size = u32::from_le_bytes(data[section + 16..section + 20].try_into().ok()?);
            let raw_pointer = u32::from_le_bytes(data[section + 20..section + 24].try_into().ok()?);
            let span = virtual_size.max(raw_size);
            if rva >= virtual_address && rva < virtual_address.saturating_add(span) {
                let offset = raw_pointer as usize + (rva - virtual_address) as usize;
                if offset < data.len() {
                    return Some(offset);
                }
            }
        }
        None
    };

    // Export directory: the number of named exports and the name pointer table.
    let exports = if export_rva != 0 && export_size != 0 {
        let mut names = Vec::new();
        if let Some(directory) = rva_to_offset(export_rva) {
            if directory + 40 <= data.len() {
                let number_of_names =
                    u32::from_le_bytes(data[directory + 24..directory + 28].try_into().ok()?);
                let address_of_names =
                    u32::from_le_bytes(data[directory + 32..directory + 36].try_into().ok()?);
                if let Some(name_table) = rva_to_offset(address_of_names) {
                    for index in 0..number_of_names {
                        let entry = name_table + index as usize * 4;
                        if entry + 4 > data.len() {
                            break;
                        }
                        let name_rva = u32::from_le_bytes(data[entry..entry + 4].try_into().ok()?);
                        if let Some(offset) = rva_to_offset(name_rva) {
                            let end = data[offset..]
                                .iter()
                                .position(|&byte| byte == 0)
                                .map(|position| offset + position)
                                .unwrap_or(offset);
                            if let Ok(name) = std::str::from_utf8(&data[offset..end]) {
                                names.push(name.to_string());
                            }
                        }
                    }
                }
            }
        }
        names
    } else {
        Vec::new()
    };

    // Import directory: a null-terminated list of image import descriptors,
    // each naming one imported DLL.
    let import_dlls = if import_rva != 0 && import_size != 0 {
        let mut dlls = Vec::new();
        if let Some(directory) = rva_to_offset(import_rva) {
            let mut index = 0usize;
            while index < 4096 {
                let descriptor = directory + index * 20;
                if descriptor + 20 > data.len() {
                    break;
                }
                let name_rva =
                    u32::from_le_bytes(data[descriptor + 12..descriptor + 16].try_into().ok()?);
                if name_rva == 0 {
                    break; // A null descriptor terminates the import table.
                }
                if let Some(offset) = rva_to_offset(name_rva) {
                    let end = data[offset..]
                        .iter()
                        .position(|&byte| byte == 0)
                        .map(|position| offset + position)
                        .unwrap_or(offset);
                    if let Ok(name) = std::str::from_utf8(&data[offset..end]) {
                        dlls.push(name.to_string());
                    }
                }
                index += 1;
            }
        }
        dlls
    } else {
        Vec::new()
    };

    Some(PeInspection {
        exports,
        import_dlls,
        image_size,
    })
}

// =============================================================================
// Process Memory
// =============================================================================

/// Current and peak working set (resident set) of a process, in bytes.
///
/// `pid` of `None` queries the host process itself. Returns `None` when the
/// OS exposes no usable counter (for example a missing `/proc` on Linux).
pub(crate) fn process_memory(pid: Option<u32>) -> Option<(u64, u64)> {
    #[cfg(target_os = "windows")]
    {
        // Windows process-memory counters without any static dependency. The
        // two functions needed are loaded at runtime from the OS-provided
        // `psapi.dll` and `kernel32.dll` through `libloading`, which the host
        // already links. This matters: adding a cargo crate that shares
        // features with `colored` (via `windows-sys`) changes the feature
        // unification of shared crates between the host build and the
        // optional-module builds, which silently splits the shared
        // `pill_core.dll` into two incompatible variants.
        use libloading::{Library, Symbol};

        // The leading fields of `PROCESS_MEMORY_COUNTERS`. `GetProcessMemoryInfo`
        // rejects a `cb` smaller than the documented struct, so the full
        // structure is declared (the trailing fields are never read).
        #[repr(C)]
        struct ProcessMemoryCounters {
            cb: u32,
            page_fault_count: u32,
            peak_working_set_size: usize,
            working_set_size: usize,
            quota_peak_paged_pool_usage: usize,
            quota_paged_pool_usage: usize,
            quota_peak_non_paged_pool_usage: usize,
            quota_non_paged_pool_usage: usize,
            pagefile_usage: usize,
            peak_pagefile_usage: usize,
            private_usage: usize,
        }

        type OpenProcessFn = unsafe extern "system" fn(u32, i32, u32) -> isize;
        type CloseHandleFn = unsafe extern "system" fn(isize) -> i32;
        type GetProcessMemoryInfoFn =
            unsafe extern "system" fn(isize, *mut ProcessMemoryCounters, u32) -> i32;

        // SAFETY: `kernel32.dll` and `psapi.dll` ship with every supported
        // Windows version, and the resolved symbols are cast to their
        // documented Win32 prototypes.
        unsafe {
            let kernel32 = Library::new("kernel32.dll").ok()?;
            let psapi = Library::new("psapi.dll").ok()?;
            let open_process: Symbol<OpenProcessFn> = kernel32.get(b"OpenProcess").ok()?;
            let close_handle: Symbol<CloseHandleFn> = kernel32.get(b"CloseHandle").ok()?;
            let get_process_memory_info: Symbol<GetProcessMemoryInfoFn> =
                psapi.get(b"GetProcessMemoryInfo").ok()?;

            // `GetCurrentProcess` is a pseudo-handle of `-1` that never needs
            // closing; `OpenProcess` returns a real handle closed below.
            const PROCESS_QUERY_INFORMATION: u32 = 0x0400;
            const PROCESS_VM_READ: u32 = 0x0010;
            let handle = match pid {
                Some(pid) => open_process(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid),
                None => -1isize,
            };
            if handle == 0 {
                return None;
            }

            let mut counters = ProcessMemoryCounters {
                cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
                page_fault_count: 0,
                peak_working_set_size: 0,
                working_set_size: 0,
                quota_peak_paged_pool_usage: 0,
                quota_paged_pool_usage: 0,
                quota_peak_non_paged_pool_usage: 0,
                quota_non_paged_pool_usage: 0,
                pagefile_usage: 0,
                peak_pagefile_usage: 0,
                private_usage: 0,
            };
            // SAFETY: `counters` is a valid out-parameter sized to the leading
            // fields and `handle` names the target process.
            let ok = get_process_memory_info(
                handle,
                &mut counters,
                std::mem::size_of::<ProcessMemoryCounters>() as u32,
            );
            if pid.is_some() {
                // SAFETY: `handle` came from `OpenProcess` and is closed exactly once.
                close_handle(handle);
            }
            if ok == 0 {
                return None;
            }
            Some((
                counters.working_set_size as u64,
                counters.peak_working_set_size as u64,
            ))
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Linux `/proc/<pid>/status` exposes `VmRSS` (current) and `VmHWM`
        // (peak high-water mark), both in kB.
        let status_path = match pid {
            Some(pid) => format!("/proc/{pid}/status"),
            None => "/proc/self/status".to_string(),
        };
        let content = std::fs::read_to_string(status_path).ok()?;
        let mut current_kb = None;
        let mut peak_kb = None;
        for line in content.lines() {
            if let Some(value) = line.strip_prefix("VmRSS:") {
                current_kb = value
                    .trim()
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse::<u64>().ok());
            } else if let Some(value) = line.strip_prefix("VmHWM:") {
                peak_kb = value
                    .trim()
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse::<u64>().ok());
            }
        }
        Some((current_kb? * 1024, peak_kb? * 1024))
    }
}

// =============================================================================
// Cargo `--timings` Parsing
// =============================================================================

/// Per-crate compile+link wall time from the newest cargo `--timings` report.
pub(crate) struct CargoTiming {
    /// Crate name → wall time in milliseconds, for crates that actually ran.
    pub crate_durations_ms: HashMap<String, u64>,
    /// The `DURATION = N;` constant: total wall time of that cargo invocation.
    pub total_seconds: f64,
}

/// Parse the newest `target/cargo-timings/cargo-timing-*.html` report.
///
/// Cargo embeds a JSON `UNIT_DATA` array at the end of the HTML: one entry per
/// unit with `name`, `mode`, `start`, `duration` (seconds) and `sections`.
/// Only crates that actually compiled or linked in that invocation carry a
/// non-zero duration, which is exactly what a rebuild report wants. Returns
/// `None` when no report exists yet or its embedded JSON cannot be parsed.
pub(crate) fn parse_latest_cargo_timings(workspace_root: &Path) -> Option<CargoTiming> {
    let timings_directory = workspace_root.join(CARGO_TIMINGS_DIRECTORY);
    let mut reports: Vec<PathBuf> = std::fs::read_dir(&timings_directory)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "html")
        })
        .collect();
    reports.sort_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH)
    });
    let newest = reports.last()?;
    let content = std::fs::read_to_string(newest).ok()?;

    // `DURATION = <seconds>;` is the total wall time of the cargo invocation.
    // The value ends at the first `;`; the text after it (the `UNIT_DATA`
    // script) must not be included in the parse.
    let total_seconds = content
        .find("DURATION = ")
        .map(|index| &content[index + "DURATION = ".len()..])
        .and_then(|tail| tail.split(';').next())
        .and_then(|value| value.trim().parse::<f64>().ok())
        .unwrap_or(0.0);

    // The `UNIT_DATA` array closes with `];`; the first `];` after the opening
    // bracket is the terminator because no interior value in the pretty-printed
    // JSON ever ends with a `]` immediately followed by `;`. The slice keeps
    // both brackets so serde sees a JSON array, not the bare contents.
    let Some(open) = content.find("const UNIT_DATA = [") else {
        return None;
    };
    let array_start = open + "const UNIT_DATA = [".len() - 1;
    let array_end = content[array_start..].find("];")? + array_start;
    let units: Vec<Value> = serde_json::from_str(&content[array_start..=array_end]).ok()?;

    let mut crate_durations_ms = HashMap::new();
    for unit in units {
        let Some(name) = unit.get("name").and_then(Value::as_str) else {
            continue;
        };
        let duration_seconds = unit.get("duration").and_then(Value::as_f64).unwrap_or(0.0);
        if duration_seconds > 0.0 {
            crate_durations_ms.insert(name.to_string(), (duration_seconds * 1000.0) as u64);
        }
    }
    Some(CargoTiming {
        crate_durations_ms,
        total_seconds,
    })
}

// =============================================================================
// Cargo Dependency Reading
// =============================================================================

/// Read a crate's direct dependencies from its fingerprint JSON.
///
/// Cargo writes `<target>/debug/.fingerprint/<crate>-<hash>/lib-<crate>.json`
/// for every compiled library, whose `deps` array names each direct
/// dependency. Returns an empty vector when the fingerprint is missing (for
/// example before the first build of a fresh workspace).
pub(crate) fn read_cargo_deps(workspace_root: &Path, crate_name: &str) -> Vec<String> {
    let fingerprint_directory = workspace_root.join(CARGO_FINGERPRINT_DIRECTORY);
    let Ok(entries) = std::fs::read_dir(&fingerprint_directory) else {
        return Vec::new();
    };
    let mut directories: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&format!("{crate_name}-")))
        })
        .collect();
    directories.sort();

    let mut deps = Vec::new();
    for directory in directories {
        let lib_json = directory.join(format!("lib-{crate_name}.json"));
        if !lib_json.exists() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&lib_json) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        if let Some(dependency_list) = value.get("deps").and_then(Value::as_array) {
            for dependency in dependency_list {
                // Cargo's fingerprint `deps` is an array of arrays, each one
                // `[fingerprint, name, is_public, local_fingerprint]`.
                let name = dependency
                    .as_array()
                    .and_then(|entry| entry.get(1))
                    .and_then(Value::as_str);
                if let Some(name) = name {
                    if !deps.iter().any(|existing: &String| existing == name) {
                        deps.push(name.to_string());
                    }
                }
            }
        }
        break;
    }
    deps
}

// =============================================================================
// Collector
// =============================================================================

/// One module's cumulative analytics across startup and every reload.
struct ModuleAnalytics {
    name: String,
    kind: ModuleKind,
    status: BuildStatus,
    build_wall_ms: u64,
    /// Sub-second phase timings carry fractional milliseconds so fast
    /// operations (load, init) do not round to zero.
    stage_ms: f64,
    load_ms: f64,
    init_ms: f64,
    migrate_ms: f64,
    artifact_bytes: u64,
    image_size: u64,
    exports: Vec<String>,
    import_dlls: Vec<String>,
    cargo_deps: Vec<String>,
    cargo_unit_ms: Option<u64>,
    reloads: u32,
}

impl ModuleAnalytics {
    fn new(name: &str, kind: ModuleKind) -> Self {
        Self {
            name: name.to_string(),
            kind,
            status: BuildStatus::Built,
            build_wall_ms: 0,
            stage_ms: 0.0,
            load_ms: 0.0,
            init_ms: 0.0,
            migrate_ms: 0.0,
            artifact_bytes: 0,
            image_size: 0,
            exports: Vec::new(),
            import_dlls: Vec::new(),
            cargo_deps: Vec::new(),
            cargo_unit_ms: None,
            reloads: 0,
        }
    }
}

/// A snapshot of one completed hot reload, printed on the next frame.
struct ReloadEvent {
    name: String,
    build_ms: u64,
    stage_ms: f64,
    load_ms: f64,
    init_ms: f64,
    migrate_ms: f64,
    artifact_bytes: u64,
    exports: usize,
    reload_count: u32,
    /// Crates that actually compiled or linked in this reload's cargo
    /// invocation(s), with their wall times, sorted by time descending. This
    /// is how a dependent crate that was relinked shows up (e.g. editing
    /// `pill_dummy_color` also relinks `pill_spline`).
    cargo_crates: Vec<(String, u64)>,
}

/// Process-wide analytics state, guarded because the build watchdog polls
/// child memory from the main frame thread.
struct Analytics {
    started: Instant,
    modules: Vec<ModuleAnalytics>,
    pending_reload_events: Vec<ReloadEvent>,
    /// Per-crate compile+link times accumulated from the cargo `--timings`
    /// reports of the builds in the current reload transaction. Snapshot and
    /// cleared by [`record_reload`], so each reload line shows exactly which
    /// crates its own build actually rebuilt.
    pending_cargo_crates: HashMap<String, u64>,
    host_current_bytes: u64,
    host_peak_bytes: u64,
    cargo_child_peak_bytes: u64,
    builds: u64,
    skips: u64,
    /// Total wall time of the newest cargo `--timings` report, in seconds.
    last_cargo_total_seconds: f64,
}

/// The process-wide collector, initialized on first use.
fn analytics() -> &'static Mutex<Analytics> {
    static ANALYTICS: OnceLock<Mutex<Analytics>> = OnceLock::new();
    ANALYTICS.get_or_init(|| {
        Mutex::new(Analytics {
            started: Instant::now(),
            modules: Vec::new(),
            pending_reload_events: Vec::new(),
            pending_cargo_crates: HashMap::new(),
            host_current_bytes: 0,
            host_peak_bytes: 0,
            cargo_child_peak_bytes: 0,
            builds: 0,
            skips: 0,
            last_cargo_total_seconds: 0.0,
        })
    })
}

/// Find a module's entry by name, creating it (with a default kind) on first
/// use. The kind is corrected by [`record_module_artifact`], which always runs
/// before any load/init/migrate record for a module.
fn find_or_create(collector: &mut Analytics, name: &str, kind: ModuleKind) -> usize {
    if let Some(index) = collector
        .modules
        .iter()
        .position(|module| module.name == name)
    {
        return index;
    }
    collector.modules.push(ModuleAnalytics::new(name, kind));
    collector.modules.len() - 1
}

/// Record the outcome of one module's build/stage step.
///
/// Called by `build_optional_module` and `build_project_module` from both the
/// fast-path skip branch and the real-build branch, with the artifact path
/// being the DLL the host actually loads (the hot copy for optional modules).
/// Also inspects the DLL's PE exports and imports, reads the crate's direct
/// cargo dependencies, and — after a real build — pulls the per-crate
/// compile+link time from the newest cargo `--timings` report.
pub(crate) fn record_module_artifact(
    name: &str,
    kind: ModuleKind,
    status: BuildStatus,
    stage_ms: f64,
    workspace_root: &Path,
    artifact_path: &Path,
) {
    let mut collector = analytics()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let index = find_or_create(&mut collector, name, kind);
    let module = &mut collector.modules[index];
    module.kind = kind;
    module.status = status;
    module.stage_ms = stage_ms;
    if let Ok(metadata) = std::fs::metadata(artifact_path) {
        module.artifact_bytes = metadata.len();
    }
    if let Some(inspection) = inspect_pe(artifact_path) {
        module.exports = inspection.exports;
        module.import_dlls = inspection.import_dlls;
        module.image_size = inspection.image_size;
    }
    module.cargo_deps = read_cargo_deps(workspace_root, name);
    if status == BuildStatus::Built {
        if let Some(timing) = parse_latest_cargo_timings(workspace_root) {
            module.cargo_unit_ms = timing.crate_durations_ms.get(name).copied();
            collector.last_cargo_total_seconds = timing.total_seconds;
            // Remember every crate this build actually ran so the next reload
            // report can show the full recompile/relink set, not just the
            // module whose transaction this was.
            for (crate_name, duration_ms) in timing.crate_durations_ms {
                collector
                    .pending_cargo_crates
                    .entry(crate_name)
                    .or_insert(duration_ms);
            }
        }
    }
    match status {
        BuildStatus::Built => collector.builds += 1,
        BuildStatus::Fresh => collector.skips += 1,
    }
}

/// Record the wall time and peak memory of one cargo invocation.
///
/// Called by `run_build_command` on success. The peak working set is sampled
/// from the cargo child process by the build watchdog; the host process memory
/// is captured at the same time so a heavy compile's effect on the host shows
/// up in the report.
pub(crate) fn record_build_command(name: &str, elapsed_ms: u64, cargo_peak_bytes: u64) {
    let mut collector = analytics()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let index = find_or_create(&mut collector, name, ModuleKind::Optional);
    collector.modules[index].build_wall_ms = elapsed_ms;
    collector.cargo_child_peak_bytes = collector.cargo_child_peak_bytes.max(cargo_peak_bytes);
    if let Some((current, peak)) = process_memory(None) {
        collector.host_current_bytes = current;
        collector.host_peak_bytes = collector.host_peak_bytes.max(peak);
    }
}

/// Record the DLL mapping time of one module.
pub(crate) fn record_load(name: &str, load_ms: f64) {
    let mut collector = analytics()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let index = find_or_create(&mut collector, name, ModuleKind::Optional);
    collector.modules[index].load_ms = load_ms;
}

/// Record the registration (`init`) time of one module.
pub(crate) fn record_init(name: &str, init_ms: f64) {
    let mut collector = analytics()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let index = find_or_create(&mut collector, name, ModuleKind::Optional);
    collector.modules[index].init_ms = init_ms;
}

/// Record the schema-migration time of one module reload.
pub(crate) fn record_migrate(name: &str, migrate_ms: f64) {
    let mut collector = analytics()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let index = find_or_create(&mut collector, name, ModuleKind::Optional);
    collector.modules[index].migrate_ms = migrate_ms;
}

/// Record a completed hot reload and queue its console line.
pub(crate) fn record_reload(name: &str) {
    let mut collector = analytics()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let index = find_or_create(&mut collector, name, ModuleKind::Optional);
    let reloads = {
        let module = &mut collector.modules[index];
        module.reloads += 1;
        module.reloads
    };
    // Snapshot the module's latest phase timings and the crates this reload's
    // builds actually rebuilt, then clear the pending set so the next reload
    // transaction starts fresh.
    let mut cargo_crates: Vec<(String, u64)> = collector
        .pending_cargo_crates
        .iter()
        .map(|(crate_name, duration_ms)| (crate_name.clone(), *duration_ms))
        .collect();
    cargo_crates.sort_by(|left, right| right.1.cmp(&left.1));
    collector.pending_cargo_crates.clear();
    let snapshot = {
        let module = &collector.modules[index];
        ReloadEvent {
            name: name.to_string(),
            build_ms: module.build_wall_ms,
            stage_ms: module.stage_ms,
            load_ms: module.load_ms,
            init_ms: module.init_ms,
            migrate_ms: module.migrate_ms,
            artifact_bytes: module.artifact_bytes,
            exports: module.exports.len(),
            reload_count: reloads,
            cargo_crates,
        }
    };
    collector.pending_reload_events.push(snapshot);
}

/// Record the host process's current and peak memory (called at setup end).
pub(crate) fn record_host_memory() {
    let mut collector = analytics()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((current, peak)) = process_memory(None) {
        collector.host_current_bytes = current;
        collector.host_peak_bytes = collector.host_peak_bytes.max(peak);
    }
}

// =============================================================================
// Report Rendering
// =============================================================================

/// Format a byte count as a compact human-readable string.
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    }
}

/// Format a millisecond count as a compact human-readable string.
fn format_ms(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

/// Print the full startup analytics report.
///
/// Called once after every module (optional + project) has been built, staged,
/// loaded and initialized. Rows are one per module; the trailing lines break
/// each module's exports, imports and direct cargo dependencies.
pub(crate) fn print_startup_report() {
    let mut collector = analytics()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Startup builds accumulated their `--timings` crates into the pending
    // set; they belong to the startup report's `cargo` column only, so drop
    // them before any reload transaction starts accumulating.
    collector.pending_cargo_crates.clear();
    let elapsed_seconds = collector.started.elapsed().as_secs_f64();
    let total_reloads: u64 = collector
        .modules
        .iter()
        .map(|module| module.reloads as u64)
        .sum();

    println!();
    println!("==============================================================");
    println!(" BUILD / LINK / HOT-RELOAD ANALYTICS | startup report");
    println!("==============================================================");
    println!(
        " elapsed: {:.2}s    host RSS: current {} / peak {}",
        elapsed_seconds,
        format_bytes(collector.host_current_bytes),
        format_bytes(collector.host_peak_bytes)
    );
    println!(
        " cargo child peak RSS: {}    builds: {}    up-to-date skips: {}    reloads: {}",
        format_bytes(collector.cargo_child_peak_bytes),
        collector.builds,
        collector.skips,
        total_reloads
    );
    if collector.last_cargo_total_seconds > 0.0 {
        println!(
            " newest cargo --timings total: {:.2}s (per-crate compile+link in the `cargo` column)",
            collector.last_cargo_total_seconds
        );
    }
    println!();

    // The module column widens to the longest name; every other column has a
    // fixed width, so long names never mangle the table.
    let name_width = collector
        .modules
        .iter()
        .map(|module| module.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let pad = |value: &str| format!("{value:<width$}", width = name_width);
    let separator = "-".repeat(name_width + 108);

    println!(
        "{}  {:<8}  {:<6}  {:>9}  {:>8}  {:>8}  {:>8}  {:>8}  {:>8}  {:>7}  {:>7}  {:>9}",
        pad("module"),
        "kind",
        "status",
        "build",
        "stage",
        "load",
        "init",
        "migrate",
        "size",
        "exports",
        "imports",
        "cargo"
    );
    println!("{separator}");

    for module in &collector.modules {
        let status = match module.status {
            BuildStatus::Fresh => "fresh",
            BuildStatus::Built => "built",
        };
        println!(
            "{}  {:<8}  {:<6}  {:>9}  {:>8}  {:>8}  {:>8}  {:>8}  {:>8}  {:>7}  {:>7}  {:>9}",
            pad(&module.name),
            module.kind.label(),
            status,
            if module.build_wall_ms > 0 {
                format_ms(module.build_wall_ms)
            } else {
                "-".to_string()
            },
            if module.stage_ms > 0.0 {
                format!("{:.1}ms", module.stage_ms)
            } else {
                "-".to_string()
            },
            if module.load_ms > 0.0 {
                format!("{:.1}ms", module.load_ms)
            } else {
                "-".to_string()
            },
            if module.init_ms > 0.0 {
                format!("{:.1}ms", module.init_ms)
            } else {
                "-".to_string()
            },
            if module.migrate_ms > 0.0 {
                format!("{:.1}ms", module.migrate_ms)
            } else {
                "-".to_string()
            },
            if module.artifact_bytes > 0 {
                format_bytes(module.artifact_bytes)
            } else {
                "-".to_string()
            },
            if module.exports.is_empty() {
                "-".to_string()
            } else {
                module.exports.len().to_string()
            },
            if module.import_dlls.is_empty() {
                "-".to_string()
            } else {
                module.import_dlls.len().to_string()
            },
            if let Some(cargo_ms) = module.cargo_unit_ms {
                format_ms(cargo_ms)
            } else {
                "-".to_string()
            },
        );
    }
    println!("{separator}");

    // Per-module detail lines: export names, import DLLs and direct deps.
    for module in &collector.modules {
        let mut details = Vec::new();
        if module.image_size > 0 {
            details.push(format!("image={}", format_bytes(module.image_size)));
        }
        if !module.exports.is_empty() {
            // A shared engine dylib can export tens of thousands of symbols;
            // cap the printed list so the console stays readable.
            if module.exports.len() <= 16 {
                details.push(format!("exports={}", module.exports.join(",")));
            } else {
                details.push(format!("exports={} symbols", module.exports.len()));
            }
        }
        if !module.import_dlls.is_empty() {
            details.push(format!("imports={}", module.import_dlls.join(",")));
        }
        if !module.cargo_deps.is_empty() {
            details.push(format!("deps={}", module.cargo_deps.join(",")));
        }
        if !details.is_empty() {
            println!("  {}: {}", module.name, details.join("  |  "));
        }
    }
    println!("==============================================================");
    println!();
}

/// Print one line per completed hot reload, draining the pending events.
///
/// Called by the frame loop right after reloads are processed, so the console
/// shows the rebuild → stage → load → init → migrate breakdown as it happens.
pub(crate) fn print_reload_events() {
    let events = drain_reload_events();
    for event in events {
        println!(
            "[analytics] reload {} (reload #{}) | build={} | stage={:.1}ms | load={:.1}ms | \
             init={:.1}ms | migrate={:.1}ms | size={} | exports={}",
            event.name,
            event.reload_count,
            if event.build_ms > 0 {
                format_ms(event.build_ms)
            } else {
                "-".to_string()
            },
            event.stage_ms,
            event.load_ms,
            event.init_ms,
            event.migrate_ms,
            if event.artifact_bytes > 0 {
                format_bytes(event.artifact_bytes)
            } else {
                "-".to_string()
            },
            event.exports,
        );
        // The module line above is only the transaction's own timings; the
        // cargo `--timings` breakdown shows every crate the build actually
        // recompiled or relinked (dependents included).
        if !event.cargo_crates.is_empty() {
            let parts: Vec<String> = event
                .cargo_crates
                .iter()
                .map(|(crate_name, duration_ms)| {
                    format!("{crate_name} {}", format_ms(*duration_ms))
                })
                .collect();
            println!("    crates rebuilt by cargo: {}", parts.join(" | "));
        }
    }
}

/// Take and clear the pending reload events for the current frame.
fn drain_reload_events() -> Vec<ReloadEvent> {
    let mut collector = analytics()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::take(&mut collector.pending_reload_events)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    /// A synthetic cargo `--timings` HTML matching the format this parser
    /// targets: `DURATION = N;`, a `UNIT_DATA` JSON array whose executed units
    /// carry non-zero `duration` seconds, and a trailing `];`.
    const FIXTURE_HTML: &str = r#"<html><head><title>Cargo Build Timings</title></head>
<body><script>
DURATION = 3;
const UNIT_DATA = [
  {
    "i": 1,
    "name": "pill_core",
    "version": "0.1.0",
    "mode": "todo",
    "target": "",
    "features": [],
    "start": 0.1,
    "duration": 1.2,
    "unblocked_units": [],
    "unblocked_rmeta_units": [],
    "sections": null
  },
  {
    "i": 2,
    "name": "project",
    "version": "0.1.0",
    "mode": "todo",
    "target": "",
    "features": [],
    "start": 1.26,
    "duration": 1.09,
    "unblocked_units": [],
    "unblocked_rmeta_units": [],
    "sections": null
  },
  {
    "i": 3,
    "name": "serde",
    "version": "1.0.229",
    "mode": "todo",
    "target": "",
    "features": [],
    "start": 0.0,
    "duration": 0.0,
    "unblocked_units": [],
    "unblocked_rmeta_units": [],
    "sections": null
  }
];
const CONCURRENCY_DATA = [
  {
    "t": 0.0,
    "active": 0,
    "waiting": 0,
    "inactive": 3
  }
];
</script></body></html>"#;

    /// Build a throwaway workspace whose `target/cargo-timings` holds the
    /// fixture, and return the workspace root.
    fn fixture_workspace() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "pill_analytics_test_{}_{unique}",
            std::process::id()
        ));
        let timings = root.join(CARGO_TIMINGS_DIRECTORY);
        std::fs::create_dir_all(&timings).unwrap();
        std::fs::write(
            timings.join("cargo-timing-20260822T000000000Z-deadbeef.html"),
            FIXTURE_HTML,
        )
        .unwrap();
        root
    }

    /// The parser extracts per-crate durations and the total from the HTML.
    #[test]
    fn parses_cargo_timings_fixture() {
        let root = fixture_workspace();
        let timing = parse_latest_cargo_timings(&root).expect("fixture must parse");

        assert_eq!(timing.total_seconds, 3.0);
        assert_eq!(timing.crate_durations_ms.get("pill_core"), Some(&1200));
        assert_eq!(timing.crate_durations_ms.get("project"), Some(&1090));
        // Fresh units (duration 0.0) are excluded from the map.
        assert!(!timing.crate_durations_ms.contains_key("serde"));

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// An empty timings directory yields `None` instead of failing.
    #[test]
    fn missing_timings_directory_returns_none() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "pill_analytics_none_test_{}_{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();

        assert!(parse_latest_cargo_timings(&root).is_none());

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// When a real cargo `--timings` report exists in the workspace, the
    /// parser must find at least one executed crate (the host always builds
    /// with `--timings`, so any recent report has one). Skipped when the
    /// workspace has never produced a report (fresh CI checkout).
    #[test]
    fn parses_real_workspace_timings_when_present() {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let Some(timing) = parse_latest_cargo_timings(workspace_root) else {
            return;
        };
        assert!(
            !timing.crate_durations_ms.is_empty(),
            "a real --timings report must name at least one executed crate"
        );
    }

    /// The dependency reader extracts names from the fingerprint's
    /// array-of-arrays `deps` field.
    #[test]
    fn reads_cargo_deps_from_fingerprint() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "pill_analytics_deps_test_{}_{unique}",
            std::process::id()
        ));
        let fingerprint = root
            .join(CARGO_FINGERPRINT_DIRECTORY)
            .join("pill_dummy_math-0123456789abcdef");
        std::fs::create_dir_all(&fingerprint).unwrap();
        std::fs::write(
            fingerprint.join("lib-pill_dummy_math.json"),
            r#"{
                "rustc": 1,
                "features": "[]",
                "declared_features": "[]",
                "target": 1,
                "profile": 1,
                "path": 1,
                "deps": [
                    [11718648107452275942, "pill_engine", false, 17195598676378470079],
                    [12107043699172518021, "pill_core", false, 15008079515306957073]
                ],
                "local": [],
                "rustflags": [],
                "config": 1,
                "compile_kind": 0
            }"#,
        )
        .unwrap();

        let deps = read_cargo_deps(&root, "pill_dummy_math");
        assert_eq!(
            deps,
            vec!["pill_engine".to_string(), "pill_core".to_string()]
        );

        std::fs::remove_dir_all(&root).unwrap();
    }
}
