//! Standalone binary: the engine's desktop frontend, headless or windowed.
//!
//! # Responsibilities
//!
//! - Install the engine report handler and the shared telemetry stack.
//! - Select what to run - the development host (`dev`) or the linked game
//!   (`shipping`) - and run it through the loops in [`runner`].
//! - Convert the final error into one styled miette report at the single
//!   reporting boundary.
//!
//! # Design
//!
//! This binary owns its event loop and its window; what runs inside it is
//! either `pill_host`'s development host or `pill_runtime` with the generated
//! shipping bundle, both driven through `pill_runtime::FrameDriver`. A shipping
//! build does not depend on `pill_host` at all, so development code cannot
//! reach a shipped binary. There is no GPU code here: the renderer attach lives
//! in the runtime, and the renderer itself in its own crate.

// Standard library
use std::path::PathBuf;

// External crates
use pill_core::error;
use pill_core::error::{engine_report, install_engine_report_handler};

// Current crate
use crate::frontend::RunError;

/// The standalone frontend's errors.
mod frontend;
/// What this binary runs: the development host, or a shipped game.
mod posture;
/// The headless and windowed run loops.
mod runner;

// Exactly one posture: without one there is nothing to run, and with both the
// development host would sit beside a linked project it never uses. Feature
// unification makes the second easy to trigger by accident - a `--workspace`
// build where another package asks for `pill_host` - so it is refused at
// compile time rather than discovered in a shipped binary.
#[cfg(not(any(feature = "dev", feature = "shipping")))]
compile_error!(
    "pill_standalone needs a posture: `dev` (the default) or `shipping` - build a shipped game as `--no-default-features --features shipping`."
);
#[cfg(all(feature = "dev", feature = "shipping"))]
compile_error!(
    "pill_standalone cannot be both `dev` and `shipping`: build a shipped game as `--no-default-features --features shipping` (and never with `hot_patch`)."
);

// =============================================================================
// Project
// =============================================================================

/// The project to run: resolved from `PROJECT_PATH` and its settings.
///
/// # Errors
///
/// Returns the configuration error when the project cannot be resolved.
#[cfg(feature = "dev")]
fn project() -> miette::Result<pill_host::HostConfig> {
    Ok(pill_host::HostConfig::from_environment()?)
}

/// The project to run: the one the generated shipping bundle links in.
///
/// The bundle is regenerated from the project's `project_settings.yaml` by
/// `devops/tools/generate_shipping_bundle.py` before a shipping build, so this
/// binary never names a project or module itself. The bundle also encodes the
/// backend - native Rust or managed C#.
#[cfg(feature = "shipping")]
fn project() -> miette::Result<pill_runtime::StaticProject> {
    Ok(pill_shipping_bundle::static_project())
}

// =============================================================================
// Telemetry
// =============================================================================

/// Install the shared telemetry stack before the run loop starts.
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
    if let Err(error) = pill_runtime::init_telemetry(file_directory) {
        // The one message that cannot go through the logger: it reports that
        // the logger itself did not install.
        eprintln!("[standalone] telemetry setup failed: {error}");
    }
}

// =============================================================================
// Reporting Boundary
// =============================================================================

/// Install the report handler once and report the final error once.
///
/// The telemetry stack is brought up before the run loop starts so that any
/// failure is captured on every active lane.
///
/// # Errors
///
/// Returns the styled [`engine_report`] when the run terminates with an error,
/// after also recording the failure on the tracing lane for correlation with
/// active spans and log files.
fn main() -> miette::Result<()> {
    // Step 1: install the miette report handler before anything can fail.
    install_engine_report_handler();
    // Step 2: bring up the shared telemetry stack (best-effort).
    init_telemetry();
    // Step 3: run the project and convert the error once.
    let outcome: Result<(), RunError> = runner::run(project()?);

    outcome.map_err(|error| {
        // Error correlation: the fatal failure also enters the tracing lane
        // so it appears inside any active spans and log files.
        error!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            error = %error,
            "host terminated with an error"
        );
        engine_report(error)
    })
}
