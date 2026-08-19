//! Build execution and output-path resolution for every reloadable module.
//!
//! Build processes inherit the host's standard streams so compiler progress
//! and diagnostics remain visible in the terminal that launched the host.
//!
//! # Responsibilities
//!
//! - Execute backend-specific project build commands from the workspace root.
//! - Execute the engine runtime build with the host's own feature set.
//! - Resolve each build's expected output artifact path.
//! - Validate that build artifacts exist before loading is attempted.
//!
//! # Design
//!
//! [`build_project_module`] and [`build_engine_runtime`] share one command
//! runner that treats a build as an opaque process: it never inspects compiler
//! output, and decides success from the child's exit status plus an
//! artifact-existence check. Cancellation is cooperative - the caller advances
//! a generation counter on newer source saves, and the watchdog loop aborts the
//! build when it observes the change.
//!
//! The engine build lives here rather than in the runtime because the runtime
//! is the artifact being produced: nothing that compiles the engine may itself
//! be part of the reload unit.

// Standard library
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// External crates
use pill_core::error::BuildError;
use pill_core::hot_reload::native_library_file_name;
use pill_core::info;

// Current crate
use crate::{ProjectModuleBackend, ProjectModuleConfig};

// =============================================================================
// Constants
// =============================================================================

/// Maximum wall-clock time a single build command may run.
const BUILD_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the build watchdog checks for completion and cancellation.
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Cargo package name of the hot-reloadable engine runtime.
pub(crate) const ENGINE_RUNTIME_PACKAGE: &str = "pill_runtime";

/// Cargo profile directory the engine runtime is built into.
const ENGINE_RUNTIME_PROFILE_DIRECTORY: &str = "debug";

// =============================================================================
// Free Functions
// =============================================================================

/// Build the selected project module and return its expected output artifact.
///
/// # Errors
///
/// Returns an error if the build command is empty, fails to spawn, exits with
/// a non-zero status, times out, is cancelled by a newer source change, or
/// the resolved output artifact does not exist at the configured path.
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

    let (program, arguments) = config
        .build_command
        .split_first()
        .ok_or(BuildError::EmptyCommand)?;
    run_build_command(
        workspace_root,
        &config.name,
        program,
        arguments,
        cancel_flag,
    )?;

    // Resolve the backend-specific output artifact path. The build command
    // itself is backend-agnostic, but each backend names and locates its
    // loadable artifact differently.
    let output_path = project_module_artifact_path(workspace_root, config);
    require_artifact(output_path)
}

/// Absolute path of the artifact the configured project backend produces.
///
/// Exposed separately from the build so a runtime generation can be created
/// with an already-built module without recompiling it.
pub(crate) fn project_module_artifact_path(
    workspace_root: &Path,
    config: &ProjectModuleConfig,
) -> PathBuf {
    match &config.backend {
        ProjectModuleBackend::NativeLibrary {
            library_name,
            output_subdirectory,
        } => workspace_root
            .join(output_subdirectory)
            .join(native_library_file_name(library_name)),
        ProjectModuleBackend::CSharp(config) => workspace_root
            .join(&config.project_output_subdirectory)
            .join(format!("{}.dll", config.project_assembly_name)),
    }
}

/// Build the hot-reloadable engine runtime and return its output artifact.
///
/// The runtime is compiled with exactly the feature set this host binary was
/// compiled with, because a feature difference changes the data both sides
/// exchange. The runtime rejects a mismatch at `create` time as well, so a
/// stale artifact built with other features cannot slip through.
///
/// # Errors
///
/// Returns an error if the build fails to spawn, exits with a non-zero status,
/// times out, is cancelled by a newer engine source change, or the produced
/// dynamic library does not appear in the workspace target directory.
pub(crate) fn build_engine_runtime(
    workspace_root: &Path,
    cancel_flag: Option<(&AtomicU64, u64)>,
) -> Result<PathBuf, BuildError> {
    info!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        package = ENGINE_RUNTIME_PACKAGE,
        features = engine_runtime_features().join(",").as_str(),
        "building engine runtime"
    );

    let mut arguments = vec![
        String::from("build"),
        String::from("--package"),
        String::from(ENGINE_RUNTIME_PACKAGE),
    ];
    let features = engine_runtime_features();
    if !features.is_empty() {
        arguments.push(String::from("--features"));
        arguments.push(features.join(","));
    }

    run_build_command(
        workspace_root,
        ENGINE_RUNTIME_PACKAGE,
        "cargo",
        &arguments,
        cancel_flag,
    )?;
    require_artifact(engine_runtime_artifact_path(workspace_root))
}

/// Absolute path of the dynamic library the engine runtime build produces.
pub(crate) fn engine_runtime_artifact_path(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join("target")
        .join(ENGINE_RUNTIME_PROFILE_DIRECTORY)
        .join(native_library_file_name(ENGINE_RUNTIME_PACKAGE))
}

/// Cargo features the engine runtime must be built with.
///
/// Derived from this host binary's own compilation so the two halves of the
/// boundary always agree, which is the same set the feature mask encodes.
pub(crate) fn engine_runtime_features() -> Vec<String> {
    let mut features = Vec::new();
    if cfg!(feature = "rendering") {
        features.push(String::from("rendering"));
    }
    if cfg!(feature = "metrics") {
        features.push(String::from("metrics"));
    }
    if cfg!(feature = "profiling") {
        features.push(String::from("profiling"));
    }
    if cfg!(feature = "profiling-fine") {
        features.push(String::from("profiling-fine"));
    }
    if cfg!(feature = "dev-logs") {
        features.push(String::from("dev-logs"));
    }
    features
}

/// Spawn one build command and supervise it to completion.
///
/// # Errors
///
/// Returns an error if the command fails to spawn, exits with a non-zero
/// status, exceeds [`BUILD_TIMEOUT`], or is cancelled by a newer source change.
fn run_build_command(
    workspace_root: &Path,
    name: &str,
    program: &str,
    arguments: &[String],
    cancel_flag: Option<(&AtomicU64, u64)>,
) -> Result<(), BuildError> {
    // Step 1: Spawn the child process from the workspace root.
    //
    // Run from the workspace root because configured paths and Cargo package
    // selection are workspace-relative. The child inherits the host's stdout
    // and stderr instead of capturing them, which keeps compiler progress,
    // warnings, and errors visible during startup and hot reload.
    let mut child = Command::new(program)
        .args(arguments)
        .current_dir(workspace_root)
        .spawn()
        .map_err(|source| BuildError::SpawnFailed {
            name: name.to_string(),
            source,
        })?;

    // Step 2: Poll for completion, cancellation, or timeout under a watchdog.
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

    // Step 3: Reject a non-zero exit status.
    //
    // A failed compiler must stop the load transaction. During hot reload the
    // caller handles this error by leaving the current module untouched.
    if !status.success() {
        return Err(BuildError::CommandFailed {
            name: name.to_string(),
            status,
        });
    }
    Ok(())
}

/// Confirm a resolved artifact exists before reporting a build successful.
///
/// A successful process exit does not guarantee that configuration points at
/// the artifact it produced. Validating the resolved path here means loading
/// errors identify an output-directory mismatch rather than an opaque dynamic
/// library failure later in the startup sequence.
///
/// # Errors
///
/// Returns [`BuildError::OutputMissing`] naming the path that was expected.
fn require_artifact(output_path: PathBuf) -> Result<PathBuf, BuildError> {
    if !output_path.exists() {
        return Err(BuildError::OutputMissing {
            path: output_path.display().to_string(),
        });
    }
    Ok(output_path)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine runtime artifact follows the platform's library naming.
    #[test]
    fn engine_runtime_artifact_uses_platform_naming() {
        let path = engine_runtime_artifact_path(Path::new("/workspace"));
        assert!(path.ends_with(native_library_file_name(ENGINE_RUNTIME_PACKAGE)));
        assert!(path
            .to_string_lossy()
            .contains(ENGINE_RUNTIME_PROFILE_DIRECTORY));
    }

    /// The runtime is built with exactly this host binary's feature set.
    #[test]
    fn engine_runtime_features_track_the_host_build() {
        let features = engine_runtime_features();
        assert_eq!(
            features.iter().any(|feature| feature == "rendering"),
            cfg!(feature = "rendering")
        );
        assert_eq!(
            features.iter().any(|feature| feature == "metrics"),
            cfg!(feature = "metrics")
        );
    }

    /// A missing artifact is reported by path rather than silently accepted.
    #[test]
    fn missing_artifacts_are_reported_by_path() {
        let missing = PathBuf::from("/workspace/target/debug/definitely_absent.dll");
        let error = require_artifact(missing).expect_err("a missing artifact fails");
        assert!(matches!(error, BuildError::OutputMissing { .. }));
    }
}
