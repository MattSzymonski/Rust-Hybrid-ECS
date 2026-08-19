//! Project-module build execution and output-path resolution.
//!
//! Build processes inherit the host's standard streams so compiler progress
//! and diagnostics remain visible in the terminal that launched the host.
//!
//! # Responsibilities
//!
//! - Execute backend-specific build commands from the workspace root.
//! - Resolve each backend's expected output artifact path.
//! - Validate that build artifacts exist before loading is attempted.
//!
//! # Design
//!
//! [`build_project_module`] is the host's single entry point for compiling a
//! project module. It treats the build as an opaque process: it never inspects
//! compiler output, and instead decides success from the child's exit status
//! plus a backend-specific output-path resolution step. Cancellation is
//! cooperative: the caller advances a generation counter on newer source
//! saves, and the watchdog loop aborts the build when it observes the change.

// Standard library
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// External crates
use pill_core::error::BuildError;
use pill_core::info;

// Current crate
use crate::{OptionalModuleConfig, ProjectModuleBackend, ProjectModuleConfig};

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

/// Run one module's build command to completion.
///
/// Shared by the project module and by optional modules so both use the same
/// process handling, watchdog, cancellation, and failure reporting. Resolving
/// and validating the produced artifact is left to the caller, because each
/// module kind names and locates its output differently.
///
/// # Errors
///
/// Returns an error if the command is empty, fails to spawn, exits with a
/// non-zero status, times out, or is cancelled by a newer source change.
pub(crate) fn run_build_command(
    workspace_root: &Path,
    name: &str,
    build_command: &[String],
    build_environment: &[(String, String)],
    cancel_flag: Option<(&AtomicU64, u64)>,
) -> Result<(), BuildError> {
    // Step 1: Split the configured command into its executable and arguments.
    //
    // Module configuration stores commands as owned strings so callers can
    // define both Cargo and dotnet builds without shell-specific quoting. The
    // first item is always the executable; every remaining item is passed verbatim.
    let (program, arguments) = build_command
        .split_first()
        .ok_or(BuildError::EmptyCommand)?;

    // Step 2: Spawn the child process from the workspace root.
    //
    // Run from the workspace root because configured paths and Cargo package
    // selection are workspace-relative. The child inherits the host's stdout
    // and stderr instead of capturing them, which keeps compiler progress,
    // warnings, and errors visible during startup and hot reload. Configured
    // environment overrides are applied last so they win over anything the
    // host itself inherited.
    let mut child = Command::new(program)
        .args(arguments)
        .current_dir(workspace_root)
        .envs(build_environment.iter().map(|(key, value)| (key, value)))
        .spawn()
        .map_err(|source| BuildError::SpawnFailed {
            name: name.to_string(),
            source,
        })?;

    // Step 3: Poll for completion, cancellation, or timeout under a watchdog.
    //
    // The host frame loop must never block indefinitely on a hung compiler or
    // an interactive prompt, so the build is polled with a deadline and a
    // cancellation signal driven by newer source saves.
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
            name: name.to_string(),
            source,
        })? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(BuildError::TimedOut {
                name: name.to_string(),
                seconds: BUILD_TIMEOUT.as_secs(),
            });
        }
        std::thread::sleep(WATCHDOG_POLL_INTERVAL);
    };

    // Step 4: Reject a non-zero exit status.
    //
    // A failed compiler must stop the load transaction. During hot reload the
    // caller handles this error by leaving the current project module untouched.
    if !status.success() {
        return Err(BuildError::CommandFailed {
            name: name.to_string(),
            status,
        });
    }
    Ok(())
}

/// Build the selected project module and return its expected output artifact.
///
/// # Errors
///
/// Returns an error if the build fails for any of the reasons reported by
/// [`run_build_command`], or if the resolved output artifact does not exist at
/// the configured path.
pub(crate) fn build_project_module(
    workspace_root: &Path,
    config: &ProjectModuleConfig,
    cancel_flag: Option<(&AtomicU64, u64)>,
) -> Result<PathBuf, BuildError> {
    info!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        module = config.name.as_str(),
        "building project module"
    );

    run_build_command(
        workspace_root,
        &config.name,
        &config.build_command,
        &config.build_environment,
        cancel_flag,
    )?;

    // Step 5: Resolve the backend-specific output artifact path.
    //
    // The build command itself is backend-agnostic, but each backend names and
    // locates its loadable artifact differently. Native outputs use platform
    // naming conventions; managed outputs always use an assembly `.dll`.
    let output_path = match &config.backend {
        ProjectModuleBackend::NativeLibrary {
            library_name,
            output_subdirectory,
        } => workspace_root
            .join(output_subdirectory)
            .join(native_library_filename(library_name)),
        ProjectModuleBackend::CSharp(config) => workspace_root
            .join(&config.project_output_subdirectory)
            .join(format!("{}.dll", config.project_assembly_name)),
    };

    // Step 6: Confirm the resolved artifact exists before reporting success.
    //
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

/// Build one optional module and return its expected output artifact.
///
/// Optional modules are workspace members, so their output always follows the
/// platform's native-library naming inside the configured output directory.
///
/// # Errors
///
/// Returns an error if the build fails for any of the reasons reported by
/// [`run_build_command`], or if the built library is missing afterwards.
pub(crate) fn build_optional_module(
    workspace_root: &Path,
    config: &OptionalModuleConfig,
    cancel_flag: Option<(&AtomicU64, u64)>,
) -> Result<PathBuf, BuildError> {
    info!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        module = config.name.as_str(),
        "building optional module"
    );

    run_build_command(
        workspace_root,
        &config.name,
        &config.build_command,
        &[],
        cancel_flag,
    )?;

    let output_path = workspace_root
        .join(&config.output_subdirectory)
        .join(native_library_filename(&config.library_name));
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
