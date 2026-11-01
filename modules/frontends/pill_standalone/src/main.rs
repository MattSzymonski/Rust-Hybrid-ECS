//! Standalone binary: the engine's desktop frontend, headless or windowed.
//!
//! # Responsibilities
//!
//! - Install the engine report handler and the shared telemetry stack.
//! - Select what to run - the development host (`development`) or the linked project
//!   (`shipping`) - and run it through the windowed or headless loop, whichever
//!   this build compiled.
//! - Convert the run's failure into one styled miette report at the single
//!   reporting boundary.
//!
//! # Design
//!
//! This binary owns its event loop, and in a windowed build its window; what
//! runs inside it is either `pill_development_host`'s development host or
//! `pill_player` with the generated shipping bundle, both driven through
//! `pill_player::FrameDriver`. A shipping build does not depend on
//! `pill_development_host` at all, so development code cannot reach a shipped
//! binary. There is no GPU code here: the renderer attach lives in the runtime,
//! and the renderer itself in its own crate.

// Standard library
use std::path::PathBuf;

// External crates
use pill_core::error::{engine_report, install_engine_report_handler};

// Current crate
use crate::error::RunError;

// =============================================================================
// Modules
// =============================================================================

/// The standalone frontend's errors.
mod error;
/// The headless run loop.
#[cfg(not(feature = "rendering"))]
mod headless_standalone;
/// What this binary runs: the development host, or a shipped project.
mod posture;
/// The windowed run loop and its `winit` application.
#[cfg(feature = "rendering")]
mod windowed_standalone;

// =============================================================================
// Posture
// =============================================================================

// Exactly one posture: without one there is nothing to run, and with both the
// development host would sit beside a linked project it never uses. Feature
// unification makes the second easy to trigger by accident - a `--workspace`
// build where another package asks for `pill_development_host` - so it is
// refused at compile time rather than discovered in a shipped binary.
#[cfg(not(any(feature = "development", feature = "shipping")))]
compile_error!(
    "pill_standalone needs a posture: `development` (the default) or `shipping` - build a shipped project as `--no-default-features --features shipping`."
);
#[cfg(all(feature = "development", feature = "shipping"))]
compile_error!(
    "pill_standalone cannot be both `development` and `shipping`: build a shipped project as `--no-default-features --features shipping` (and never with `hot_patch`)."
);

// =============================================================================
// Telemetry
// =============================================================================

/// Installs the shared telemetry stack before the run loop starts.
///
/// Terminal logging is always active. A file lane is added when `ECS_LOG_DIR`
/// is set. When the `profiling` feature is enabled, `profile::*` spans are
/// routed to Tracy through an independent filter.
///
/// Setup is best-effort: a failure only degrades telemetry and is reported
/// to stderr without aborting the run.
fn init_telemetry() {
    // Step 1: resolve the optional log directory from the environment.
    let file_directory = std::env::var_os("ECS_LOG_DIR").map(PathBuf::from);
    // Step 2: install the stack, reporting setup failures to stderr.
    if let Err(error) = pill_player::init_telemetry(file_directory) {
        // The one message that cannot go through the logger: it reports that
        // the logger itself did not install.
        eprintln!("[standalone] telemetry setup failed: {error}");
    }
}

// =============================================================================
// Reporting Boundary
// =============================================================================

/// Installs the report handler, then runs the project and reports its failure.
///
/// The telemetry stack is brought up before the run loop starts so that any
/// failure is captured on every active lane.
///
/// # Errors
///
/// Returns the project's configuration report when it cannot be resolved, or
/// the styled [`engine_report`] of the failure the run terminated with, after
/// recording that failure on the tracing lane for correlation with active
/// spans and log files.
fn main() -> miette::Result<()> {
    // Step 1: install the miette report handler before anything can fail.
    install_engine_report_handler();
    // Step 2: bring up the shared telemetry stack (best-effort).
    init_telemetry();
    // Step 3: resolve the project this build runs. Development reads
    // `PROJECT_PATH` and the project's `project_settings.yaml`. Shipping asks
    // the generated bundle, regenerated from the same settings file by
    // `devops/tools/generate_shipping_bundle.py` before the build, so this
    // binary never names a project or module itself.
    #[cfg(feature = "development")]
    let project = pill_development_host::HostConfig::from_environment()?;
    #[cfg(feature = "shipping")]
    let project = pill_shipping_bundle::static_project();
    // Step 4: run the project and convert the error once.
    #[cfg(not(feature = "rendering"))]
    let run_result: Result<(), RunError> = headless_standalone::run(project);
    #[cfg(feature = "rendering")]
    let run_result: Result<(), RunError> = windowed_standalone::run(project);

    run_result.map_err(|error| {
        // Error correlation: the fatal failure also enters the tracing lane
        // so it appears inside any active spans and log files.
        pill_core::error!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            error = %error,
            "host terminated with an error"
        );
        engine_report(error)
    })
}
