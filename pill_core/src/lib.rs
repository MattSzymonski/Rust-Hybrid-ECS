//! Shared core definitions of the Pill engine.
//!
//! # Responsibilities
//!
//! - Own the semantic error system used by every workspace crate.
//! - Keep the `PillStyle` string-styling vocabulary for terminal output.
//! - Own the telemetry foundation: static targets, developer log macros,
//!   the terminal formatter, and the subscriber builder.
//! - Own the Tracy profiling API (`profile_scope!` and friends) with its
//!   feature gating and no-op fallbacks.
//! - Re-export the `tracing`, `metrics`, and profiling macros so users import
//!   everything from a single flat namespace: `pill_core`.
//!
//! # Design
//!
//! The error system lives in [`error`]: subsystem enums declared with
//! `#[engine_error]` compose transparently into [`error::HostError`], and
//! the diagnostics runtime renders one semantic message definition in
//! either plain or styled form. Crates import these types from here instead
//! of defining their own error-handling infrastructure.
//!
//! The telemetry system lives in [`telemetry`] and [`metrics`]: `tracing`
//! carries events and spans through three lanes (`engine::dev`, `engine::*`,
//! `profile::*`), and the `metrics` recorder keeps repeated numerical state.
//!
//! # Single Flat Namespace
//!
//! All telemetry entry-points re-export at the crate root, so there is exactly
//! one way to import each macro and no ambiguity with `tracing::*`:
//!
//! ```ignore
//! use pill_core::{debug, info, warn, error, trace, trace_span}; // permanent logging
//! use pill_core::{log, dev_warn, dev_error};                     // scratch debugging
//! use pill_core::{gauge, histogram, counter};                    // metrics
//! use pill_core::profile_scope;                                  // profiling
//! ```

pub mod error;
#[cfg(feature = "metrics")]
pub mod metrics;
pub mod profiling;
pub mod style;
pub mod telemetry;

pub use style::PillStyle;
pub use tracing;

pub mod color;
pub mod math;
pub mod utils;

// -----------------------------------------------------------------------------
// Flat re-exports: tracing, metrics, and tracy-client
// -----------------------------------------------------------------------------

// Permanent logging macros. `error` coexists with the `pub mod error` module:
// Rust keeps the macro and module namespaces separate, so both resolve.
pub use tracing::{debug, error, info, span, trace, trace_span, warn};

// Metrics macros (gated on the `metrics` feature). The leading `::` forces
// the external `metrics` crate rather than the local `pub mod metrics`.
#[cfg(feature = "metrics")]
pub use ::metrics::{counter, gauge, histogram};

// `tracy_client` is re-exported so the `profile_*` macros can reach it via
// `$crate::tracy_client::...` from any downstream crate without that crate
// declaring a direct dependency.
#[cfg(any(
    feature = "profiling",
    feature = "profiling-minimal",
    feature = "tracy"
))]
pub use tracy_client;

// =============================================================================
// Simple Developer Logging Macros
// =============================================================================

/// Simple developer scratch log at `DEBUG` level on the [`DEV_LOG_TARGET`]
/// lane (`engine::dev`).
///
/// Feature-gated behind `dev-logs`; compiles away when disabled. These macros
/// are for temporary developer diagnostics, never permanent telemetry.
///
/// [`DEV_LOG_TARGET`]: telemetry::DEV_LOG_TARGET
#[cfg(feature = "dev-logs")]
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::tracing::debug!(target: $crate::telemetry::DEV_LOG_TARGET, $($arg)*)
    };
}

/// Compile-time no-op of [`log!`] when the `dev-logs` feature is disabled.
#[cfg(not(feature = "dev-logs"))]
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {};
}

/// Simple developer scratch warning at `WARN` level on the `engine::dev`
/// lane. Feature-gated behind `dev-logs`.
#[cfg(feature = "dev-logs")]
#[macro_export]
macro_rules! dev_warn {
    ($($arg:tt)*) => {
        $crate::tracing::warn!(target: $crate::telemetry::DEV_LOG_TARGET, $($arg)*)
    };
}

/// Compile-time no-op of [`dev_warn!`] when the `dev-logs` feature is disabled.
#[cfg(not(feature = "dev-logs"))]
#[macro_export]
macro_rules! dev_warn {
    ($($arg:tt)*) => {};
}

/// Simple developer scratch error at `ERROR` level on the `engine::dev`
/// lane. Feature-gated behind `dev-logs`.
#[cfg(feature = "dev-logs")]
#[macro_export]
macro_rules! dev_error {
    ($($arg:tt)*) => {
        $crate::tracing::error!(target: $crate::telemetry::DEV_LOG_TARGET, $($arg)*)
    };
}

/// Compile-time no-op of [`dev_error!`] when the `dev-logs` feature is disabled.
#[cfg(not(feature = "dev-logs"))]
#[macro_export]
macro_rules! dev_error {
    ($($arg:tt)*) => {};
}
