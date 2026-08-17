//! Shared core definitions of the Pill engine.
//!
//! # Responsibilities
//!
//! - Own the semantic error system used by every workspace crate.
//! - Keep the `PillStyle` string-styling vocabulary for terminal output.
//! - Own the telemetry foundation: static targets, developer log macros,
//!   the terminal formatter, and the subscriber builder.
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

pub mod error;
#[cfg(feature = "metrics")]
pub mod metrics;
pub mod style;
pub mod telemetry;

pub use style::PillStyle;
pub use tracing;

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
macro_rules! warn {
    ($($arg:tt)*) => {
        $crate::tracing::warn!(target: $crate::telemetry::DEV_LOG_TARGET, $($arg)*)
    };
}

/// Compile-time no-op of [`warn!`] when the `dev-logs` feature is disabled.
#[cfg(not(feature = "dev-logs"))]
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {};
}

/// Simple developer scratch error at `ERROR` level on the `engine::dev`
/// lane. Feature-gated behind `dev-logs`.
#[cfg(feature = "dev-logs")]
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        $crate::tracing::error!(target: $crate::telemetry::DEV_LOG_TARGET, $($arg)*)
    };
}

/// Compile-time no-op of [`error!`] when the `dev-logs` feature is disabled.
#[cfg(not(feature = "dev-logs"))]
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {};
}
