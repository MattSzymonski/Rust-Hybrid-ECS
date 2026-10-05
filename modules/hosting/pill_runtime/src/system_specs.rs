//! The start-up report of the machine the engine runs on.
//!
//! # Responsibilities
//!
//! - Report CPU, memory, storage and OS through the telemetry stack, once
//!   per runtime.
//!
//! # Design
//!
//! This lives in the runtime, which links only into executables, rather
//! than in the engine core: `sysinfo` reaches the system through the
//! `windows` crate, which declares its functions as `raw-dylib` imports of
//! about seventy system DLLs. In the shared `pill_engine_core.dll` that made
//! every hot patch generate an import library for each of them at link time.

// External crates
use pill_core::info;
use pill_core::telemetry::log_block;

/// Report a summary of detected system hardware through the telemetry stack.
///
/// Includes CPU, core/thread count, RAM, swap, disks, OS, and uptime.
/// GPU detection requires a rendering backend.
///
/// Emitted under `telemetry_target::SYSTEM` as one multi-line event, so the
/// report can be filtered or redirected like any other engine output and keeps
/// its box-drawing layout as a single block.
///
/// Called by [`Runtime::new`](crate::Runtime::new), before it creates the
/// engine.
pub(crate) fn print_system_specs() {
    use sysinfo::{Disks, System};

    // Step 1: Snapshot system state and derive RAM, swap, and CPU figures.
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

    let physical_cores = sysinfo::System::physical_core_count().unwrap_or(0);
    let logical_threads = system.cpus().len();

    let uptime_secs = System::uptime();
    let uptime_str = format_uptime(uptime_secs);

    // Step 2: Emit the hardware report as one multi-line entry.
    let mut lines = vec![
        format!("├─ CPU: {cpu_name}"),
        format!("│  └─ Cores: {physical_cores} physical, {logical_threads} logical threads"),
        "├─ Memory".to_string(),
        format!("│  ├─ RAM: {used_ram_gb:.1} / {total_ram_gb:.1} GiB used ({ram_pct:.0}%)"),
    ];
    if total_swap_gb > 0.0 {
        lines.push(format!(
            "│  └─ Swap: {used_swap_gb:.1} / {total_swap_gb:.1} GiB used ({swap_pct:.0}%)"
        ));
    } else {
        lines.push("│  └─ Swap: none".to_string());
    }
    lines.extend(disk_info_lines(&disks));
    lines.push(format!("├─ OS: {}", os_pretty_name()));
    lines.push(format!("│  └─ Uptime: {uptime_str}"));
    lines.push("└─ GPU: use external tools (dxdiag / lspci)".to_string());
    info!(
        target: pill_core::telemetry::telemetry_target::SYSTEM,
        "{}",
        log_block("System specs", lines)
    );
}

/// The `Storage` branch of the system report: one line per detected storage
/// device, or a single "no disks detected" line when the list is empty.
///
/// Each device line shows the mount point, used/total space, usage percentage,
/// and disk kind (SSD/HDD).
fn disk_info_lines(disks: &sysinfo::Disks) -> Vec<String> {
    if disks.is_empty() {
        return vec!["├─ Storage: no disks detected".to_string()];
    }
    let mut lines = vec!["├─ Storage".to_string()];
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
        lines.push(format!(
            "│  {branch} {mount}  {used_gb:.0} / {total_gb:.0} GiB used ({pct:.0}%)  [{kind}]"
        ));
    }
    lines
}

/// Format a duration in seconds as a compact `Nd Nh Nm` human-readable string.
///
/// Omits the days component when it is zero and returns `"unknown"` for a
/// zero-second input, matching `sysinfo`'s behaviour on unsupported systems.
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
