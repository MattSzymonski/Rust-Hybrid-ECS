//! Standalone host binary — thin frontend over the shared [`host`] runner.
//!
//! # Responsibilities
//!
//! - Install the engine report handler and the shared telemetry stack.
//! - Delegate the headless or windowed run loop to [`host::run`].
//! - Convert the final error into one styled miette report at the single
//!   reporting boundary.
//!
//! # Design
//!
//! All hot-reload, module-loading, and rendering logic lives in the shared
//! [`host`] crate. This binary selects the module configuration from the
//! environment, starts the loop, and reports failures exactly once. There is
//! no window, GPU, or event-loop code here: `host::run` owns those behind the
//! `rendering` feature.

use host::{engine_report, install_engine_report_handler, GameModuleConfig};
use pill_core::error;

// =============================================================================
// Telemetry
// =============================================================================

/// Install the shared telemetry stack before the run loop starts.
///
/// Terminal logging is always active. A file lane is added when `ECS_LOG_DIR`
/// is set. When the `profiling` feature is enabled, `profile::*` spans are
/// routed to Tracy through an independent filter.
fn init_telemetry() {
    use std::path::PathBuf;
    let file_directory = std::env::var_os("ECS_LOG_DIR").map(PathBuf::from);
    if let Err(error) = host::init_telemetry(file_directory) {
        eprintln!("[standalone] telemetry setup failed: {error}");
    }
}

// =============================================================================
// Reporting Boundary
// =============================================================================

/// Install the report handler once and report the final error once.
fn main() -> miette::Result<()> {
    install_engine_report_handler();
    init_telemetry();
    host::run(GameModuleConfig::from_environment()).map_err(|error| {
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
