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
    dispatch().map_err(engine_report)
}
