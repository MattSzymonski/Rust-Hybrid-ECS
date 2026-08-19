//! Standalone host binary — thin frontend over the shared [`pill_host`] runner.
//!
//! # Responsibilities
//!
//! - Install the engine report handler and the shared telemetry stack.
//! - Delegate the headless or windowed run loop to [`pill_host::run`].
//! - Convert the final error into one styled miette report at the single
//!   reporting boundary.
//!
//! # Design
//!
//! All hot-reload, module-loading, and rendering logic lives in the shared
//! [`pill_host`] crate. This binary selects the module configuration from the
//! environment, starts the loop, and reports failures exactly once. There is
//! no window, GPU, or event-loop code here: `pill_host::run` owns those behind the
//! `rendering` feature, and the engine itself lives in a dynamic library the
//! host loads and swaps without restarting this process.

// Standard library
use std::path::PathBuf;

// External crates
use pill_core::error;
use pill_host::{engine_report, install_engine_report_handler, ProjectModuleConfig};

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
/// to stderr without aborting the host.
fn init_telemetry() {
    // Step 1: resolve the optional log directory from the environment.
    let file_directory = std::env::var_os("ECS_LOG_DIR").map(PathBuf::from);
    // Step 2: install the stack, reporting setup failures to stderr.
    if let Err(error) = pill_host::init_telemetry(file_directory) {
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
/// Returns the styled [`engine_report`] when [`pill_host::run`] terminates with an
/// error, after also recording the failure on the tracing lane for
/// correlation with active spans and log files.
fn main() -> miette::Result<()> {
    // Step 1: install the miette report handler before anything can fail.
    install_engine_report_handler();
    // Step 2: bring up the shared telemetry stack (best-effort).
    init_telemetry();
    // Step 3: delegate to the shared run loop and convert the error once.
    pill_host::run(ProjectModuleConfig::from_environment()?).map_err(|error| {
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
