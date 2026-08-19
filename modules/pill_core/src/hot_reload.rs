//! Filesystem layout shared by every hot-reloadable module in a workspace.
//!
//! # Responsibilities
//!
//! - Own the per-process directory that holds temporary copies of mapped
//!   dynamic libraries.
//! - Hand out collision-free temporary paths for one loaded generation.
//! - Own the staging directory where freshly built runtime dylibs land.
//! - Remove temporary directories left behind by crashed host processes.
//! - Name a native dynamic library the way the platform's linker does.
//!
//! # Design
//!
//! A mapped dynamic library is locked by the operating system on Windows, so
//! neither the project module nor the engine runtime is ever loaded from its
//! build output: each generation is copied to a unique path first, leaving the
//! compiler free to replace the original at any time.
//!
//! Both the thin host loader and the reloadable runtime need to agree on that
//! layout - the host stages runtime dylibs while the runtime stages project
//! dylibs, into the same per-process directory - so the layout lives here, in
//! the crate both sides link, rather than being restated on each side. It is
//! deliberately *not* part of the host↔runtime ABI: no path type crosses the
//! boundary, only the already-resolved absolute paths the contract carries as
//! plain strings.

// Standard library
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

// =============================================================================
// Constants
// =============================================================================

/// Directory beneath the workspace root holding temporary library copies.
pub const TEMPORARY_DIRECTORY: &str = "pill_standalone_temp";

/// Sub-directory holding staged engine runtime dylibs.
pub const RUNTIME_STAGING_DIRECTORY: &str = "runtime";

/// File-name stem of a staged engine runtime generation.
pub const RUNTIME_STAGED_FILE_PREFIX: &str = "pill_runtime_hot_reloaded_";

/// Monotonic suffix ensuring temporary copies never collide, even when the
/// system clock repeats or moves backwards.
static TEMPORARY_COPY_COUNTER: AtomicU64 = AtomicU64::new(0);

// =============================================================================
// Free Functions
// =============================================================================

/// Directory used by this process for temporary dynamic-library copies.
///
/// Scoping the directory per process id means one host instance can never
/// delete or overwrite the copies of another instance running against the
/// same workspace.
pub fn process_temporary_directory(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(TEMPORARY_DIRECTORY)
        .join(std::process::id().to_string())
}

/// Directory this process stages built engine runtime dylibs into.
///
/// Staged files are generation-numbered rather than overwritten, so the
/// watcher observing this directory can tell an externally produced build
/// apart from the one the host just staged itself.
pub fn runtime_staging_directory(workspace_root: &Path) -> PathBuf {
    process_temporary_directory(workspace_root).join(RUNTIME_STAGING_DIRECTORY)
}

/// File name of the staged runtime dylib for one generation index.
pub fn runtime_staged_file_name(generation: u64, extension: &str) -> String {
    format!("{RUNTIME_STAGED_FILE_PREFIX}{generation}.{extension}")
}

/// Parse the generation index out of a staged runtime dylib file name.
///
/// Returns `None` for any file that was not produced by
/// [`runtime_staged_file_name`], which lets a watcher ignore unrelated
/// entries such as debug symbol files.
pub fn parse_runtime_staged_generation(file_name: &str) -> Option<u64> {
    let remainder = file_name.strip_prefix(RUNTIME_STAGED_FILE_PREFIX)?;
    let digits = remainder.split('.').next()?;
    digits.parse().ok()
}

/// Build a collision-free temporary path inside this process's directory.
///
/// The name combines a caller-supplied prefix, a wall-clock timestamp, and a
/// process-wide counter, so concurrent loads and a repeating or backwards
/// system clock all still produce distinct paths.
///
/// # Errors
///
/// Returns the underlying failure when the temporary directory cannot be
/// created.
pub fn unique_temporary_module_path(
    workspace_root: &Path,
    prefix: &str,
    extension: &str,
) -> Result<PathBuf, std::io::Error> {
    let directory = process_temporary_directory(workspace_root);
    std::fs::create_dir_all(&directory)?;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let counter = TEMPORARY_COPY_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(directory.join(format!("{prefix}_{timestamp}_{counter}.{extension}")))
}

/// Return the platform-specific filename produced for a native library.
///
/// Cargo follows each platform's conventional dynamic-library prefix and
/// extension. Keeping this mapping here prevents build orchestration and
/// module loading from accumulating operating-system-specific branches.
pub fn native_library_file_name(library_name: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{library_name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{library_name}.dylib")
    } else {
        format!("lib{library_name}.so")
    }
}

/// Extension a native dynamic library uses on the current platform.
pub fn native_library_extension() -> &'static str {
    if cfg!(target_os = "windows") {
        "dll"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    }
}

/// Whether a process with the given id is still running.
///
/// Linux probes `/proc` directly. Other platforms cannot probe process
/// liveness cheaply, so stale directories are detected by attempting the
/// removal: a live process keeps its mapped modules locked and the deletion
/// fails naturally.
fn process_is_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

/// Remove temporary copies left by earlier runs of the host process.
///
/// Directories belonging to other, still-running host instances are skipped;
/// stale directories from crashed processes are removed. Removal failures are
/// reported instead of swallowed. Must run before this process stages any
/// module of its own, because its own directory is also cleared.
pub fn cleanup_stale_temporary_files(workspace_root: &Path) {
    let temporary_root = workspace_root.join(TEMPORARY_DIRECTORY);
    let Ok(entries) = std::fs::read_dir(&temporary_root) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && process_is_alive(pid) {
            continue;
        }
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => {
                if pid != std::process::id() {
                    println!("[host] Cleaned up stale temporary files from process {pid}.");
                }
            }
            Err(error) => {
                if pid == std::process::id() {
                    eprintln!(
                        "[host] Could not remove temporary directory {}: {error}",
                        entry.path().display()
                    );
                } else {
                    // On platforms without process probing the removal may
                    // fail simply because another live host holds the files.
                    println!(
                        "[host] Temporary directory {} left in place (possibly still in use): {error}",
                        entry.path().display()
                    );
                }
            }
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Temporary copies are scoped to the current process id.
    #[test]
    fn temporary_directory_is_scoped_per_process() {
        let directory = process_temporary_directory(Path::new("/workspace"));
        assert!(directory.ends_with(std::process::id().to_string()));
        assert!(directory.to_string_lossy().contains(TEMPORARY_DIRECTORY));
    }

    /// Staged runtime names round-trip through their generation parser.
    #[test]
    fn staged_runtime_names_round_trip() {
        let name = runtime_staged_file_name(17, "dll");
        assert_eq!(name, "pill_runtime_hot_reloaded_17.dll");
        assert_eq!(parse_runtime_staged_generation(&name), Some(17));
    }

    /// Unrelated files in the staging directory are ignored by the parser.
    #[test]
    fn unrelated_staged_files_have_no_generation() {
        assert_eq!(parse_runtime_staged_generation("project_12_0.dll"), None);
        assert_eq!(
            parse_runtime_staged_generation("pill_runtime_hot_reloaded_.dll"),
            None
        );
    }

    /// Debug symbol files staged beside a dylib still report their generation,
    /// so a watcher never mistakes them for a newer build.
    #[test]
    fn staged_symbol_files_report_the_same_generation() {
        assert_eq!(
            parse_runtime_staged_generation("pill_runtime_hot_reloaded_4.pdb"),
            Some(4)
        );
    }

    /// Repeated temporary paths never collide within one process.
    #[test]
    fn temporary_module_paths_are_unique() {
        let workspace = std::env::temp_dir().join(format!("pill_layout_{}", std::process::id()));
        let first = unique_temporary_module_path(&workspace, "project", "dll").unwrap();
        let second = unique_temporary_module_path(&workspace, "project", "dll").unwrap();
        assert_ne!(first, second);
        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// Native library naming follows the host platform's linker convention.
    #[test]
    fn native_library_naming_matches_the_platform() {
        let name = native_library_file_name("project_rs");
        if cfg!(target_os = "windows") {
            assert_eq!(name, "project_rs.dll");
        } else if cfg!(target_os = "macos") {
            assert_eq!(name, "libproject_rs.dylib");
        } else {
            assert_eq!(name, "libproject_rs.so");
        }
        assert!(name.ends_with(native_library_extension()));
    }
}
