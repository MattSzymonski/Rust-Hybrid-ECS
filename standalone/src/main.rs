//! Standalone host binary — thin dispatcher that selects between headless
//! and windowed modes at compile time based on the `rendering` feature.
//!
//! # Responsibilities
//!
//! - Gate the execution path on `#[cfg(feature = "rendering")]`.
//! - Install the engine report handler and convert the final error into one
//!   styled miette report at the single reporting boundary.
//!
//! # Design
//!
//! All hot-reload logic lives in the shared [`host`] crate. This binary
//! only adds the execution loop: a plain console loop in `headless`, or a
//! `winit` + `wgpu` render loop in `windowed`. Errors propagate typed all
//! the way to `main`, where they are reported exactly once.

mod error;

use error::StandaloneError;
use host::{engine_report, install_engine_report_handler};

// =============================================================================
// Telemetry
// =============================================================================

/// Install the shared telemetry stack before the execution path starts.
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
// Headless Mode
// =============================================================================

#[cfg(not(feature = "rendering"))]
mod headless;

#[cfg(not(feature = "rendering"))]
fn dispatch() -> Result<(), StandaloneError> {
    headless::run()
}

// =============================================================================
// Windowed Mode
// =============================================================================

#[cfg(feature = "rendering")]
mod windowed;

#[cfg(feature = "rendering")]
fn dispatch() -> Result<(), StandaloneError> {
    windowed::run()
}

// =============================================================================
// Reporting Boundary
// =============================================================================

/// Install the report handler once and report the final error once.
fn main() -> miette::Result<()> {
    install_engine_report_handler();
    init_telemetry();
    dispatch().map_err(|error| {
        // Error correlation: the fatal failure also enters the tracing lane
        // so it appears inside any active spans and log files.
        tracing::error!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            error = %error,
            "host terminated with an error"
        );
        engine_report(error)
    })
}
