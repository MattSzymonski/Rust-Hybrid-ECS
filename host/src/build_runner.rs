//! Game-module build execution and output-path resolution.
//!
//! Build processes inherit the host's standard streams so compiler progress
//! and diagnostics remain visible in the terminal that launched the host.
//!
//! # Responsibilities
//!
//! - Execute backend-specific build commands from the workspace root.
//! - Resolve each backend's expected output artifact path.
//! - Validate that build artifacts exist before loading is attempted.

// Standard library
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// Current crate
use crate::{GameModuleBackend, GameModuleConfig};

// External crates
use pill_core::error::BuildError;

// =============================================================================
// Constants
// =============================================================================

/// Maximum wall-clock time a single build command may run.
const BUILD_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the build watchdog checks for completion and cancellation.
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(100);

// =============================================================================
// Free Functions
// =============================================================================

/// Build the selected game module and return its expected output artifact.
///
/// # Errors
///
/// Returns an error if the build command is empty, fails to spawn, exits with
/// a non-zero status, times out, is cancelled by a newer source change, or
/// the resolved output artifact does not exist at the configured path.
pub(crate) fn build_game_module(
    workspace_root: &Path,
    config: &GameModuleConfig,
    cancel_flag: Option<(&AtomicU64, u64)>,
) -> Result<PathBuf, BuildError> {
    tracing::info!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        module = config.name,
        "building game module"
    );

    // GameModuleConfig stores commands as static slices so callers can define
    // both Cargo and dotnet builds without shell-specific quoting. The first
    // item is always the executable; every remaining item is passed verbatim.
    let (program, arguments) = config
        .build_command
        .split_first()
        .ok_or(BuildError::EmptyCommand)?;

    // Run from the workspace root because configured paths and Cargo package
    // selection are workspace-relative. The child inherits the host's stdout
    // and stderr instead of capturing them, which keeps compiler progress,
    // warnings, and errors visible during startup and hot reload.
    let mut child = Command::new(program)
        .args(arguments)
        .current_dir(workspace_root)
        .spawn()
        .map_err(|source| BuildError::SpawnFailed {
            name: config.name.to_string(),
            source,
        })?;

    // Wait for the compiler under a watchdog. The host frame loop must never
    // block indefinitely on a hung compiler or an interactive prompt, so the
    // build is polled with a deadline and a cancellation signal driven by
    // newer source saves.
    let deadline = Instant::now() + BUILD_TIMEOUT;
    let status = loop {
        // A newer save during the build advances the generation beyond the
        // baseline captured when the reload started, which cancels this
        // attempt. The caller keeps the old module and the next frame
        // rebuilds with the newer sources.
        if cancel_flag
            .is_some_and(|(generation, baseline)| generation.load(Ordering::Acquire) != baseline)
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(BuildError::Cancelled);
        }
        if let Some(status) = child.try_wait().map_err(|source| BuildError::WaitFailed {
            name: config.name.to_string(),
            source,
        })? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(BuildError::TimedOut {
                name: config.name.to_string(),
                seconds: BUILD_TIMEOUT.as_secs(),
            });
        }
        std::thread::sleep(WATCHDOG_POLL_INTERVAL);
    };

    // A failed compiler must stop the load transaction. During hot reload the
    // caller handles this error by leaving the current game module untouched.
    if !status.success() {
        return Err(BuildError::CommandFailed {
            name: config.name.to_string(),
            status,
        });
    }

    // The build command itself is backend-agnostic, but each backend names and
    // locates its loadable artifact differently. Native outputs use platform
    // naming conventions; managed outputs always use an assembly `.dll`.
    let output_path = match &config.backend {
        GameModuleBackend::NativeLibrary {
            library_name,
            output_subdirectory,
        } => workspace_root
            .join(output_subdirectory)
            .join(native_library_filename(library_name)),
        GameModuleBackend::CSharp(config) => workspace_root
            .join(config.game_output_subdirectory)
            .join(format!("{}.dll", config.game_assembly_name)),
    };

    // A successful process exit does not guarantee that configuration points
    // at the artifact it produced. Validate the resolved path here so loading
    // errors identify an output-directory mismatch rather than an opaque DLL
    // or managed-runtime failure later in the startup sequence.
    if !output_path.exists() {
        return Err(BuildError::OutputMissing {
            path: output_path.display().to_string(),
        });
    }

    Ok(output_path)
}

/// Return the platform-specific filename produced for a native library.
fn native_library_filename(library_name: &str) -> String {
    // Cargo follows each platform's conventional dynamic-library prefix and
    // extension. Keeping this mapping here prevents backend orchestration from
    // accumulating operating-system-specific branches.
    if cfg!(target_os = "windows") {
        format!("{library_name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{library_name}.dylib")
    } else {
        format!("lib{library_name}.so")
    }
}
