//! Shared core definitions of the Pill engine: the one library every DLL links.
//!
//! # Responsibilities
//!
//! - Own the semantic error system used by every workspace crate.
//! - Own the telemetry foundation: static targets, developer log macros, the
//!   terminal formatter, the subscriber builder and the metrics recorder.
//! - Own the platform layer ([`platform`]): clock, log output and futures, the
//!   only code that differs between native and the web.
//! - Own the Tracy profiling API (`profile_scope!` and friends) with its
//!   feature gating and no-op fallbacks.
//! - Own the state that must exist once per process: the Rayon pool, the
//!   native buffer allocation service and the per-thread run ticks.
//! - Provide the plain-data and helper types the artifacts share: inline
//!   strings, native-backed buffers, slot maps, `PillStyle`, colour palettes,
//!   math aliases and small utilities.
//! - Re-export the `tracing` and `metrics` macros, so every telemetry entry
//!   point imports from a single flat namespace: `pill_core`.
//!
//! # Design
//!
//! The crate is both a `dylib` and an `rlib`: every DLL links the one shared
//! copy, so process-global state has a single instance across the boundary,
//! while a monolithic release build can still embed it.
//!
//! The error system lives in [`error`]: subsystem enums declared with
//! `#[engine_error]` compose transparently into [`error::HostError`], and the
//! renderers turn one semantic message definition into plain or styled output.
//! The telemetry system lives in [`telemetry`] and [`metrics`].

// =============================================================================
// Modules
// =============================================================================

/// Color palettes for debug visualization.
pub mod color;
/// Growable runs of plain elements in engine-owned native memory.
pub mod dynamic_buffer;
/// Semantic error system shared by every workspace crate.
pub mod error;
/// Inline, fixed-capacity strings for plain-data structs.
pub mod fixed_string;
/// Math type aliases over `glam` and the direction vocabulary.
pub mod math;
/// Metrics recording foundation, gated on the `metrics` feature.
#[cfg(feature = "metrics")]
pub mod metrics;
/// Engine-owned native buffer allocation, shared by every loaded artifact.
pub mod native_buffer;
/// The platform layer: clock, log output and futures on native and the web.
pub mod platform;
/// Tracy profiling API with feature gating and no-op fallbacks.
pub mod profiling;
/// Generational slot arena for values addressed by stable handles.
pub mod slot_map;
/// `PillStyle` string-styling vocabulary for terminal output.
pub mod style;
/// Per-thread change ticks of the running system, shared by every artifact.
pub mod system_ticks;
/// Telemetry foundation: static targets, formatter, and subscriber builder.
pub mod telemetry;
/// Small general-purpose helpers shared by Pill crates.
pub mod utils;

// =============================================================================
// Flat Re-exports: tracing, metrics, and tracy-client
// =============================================================================

/// The engine-owned dynamic buffer, re-exported at the crate root.
pub use dynamic_buffer::DynamicBuffer;
/// Inline fixed-capacity strings, re-exported at the crate root.
pub use fixed_string::{FixedString64, FIXED_STRING_64_CAPACITY};
/// The `PillStyle` string-styling vocabulary, re-exported at the crate root.
pub use style::PillStyle;
/// The `tracing` crate, re-exported so downstream crates import telemetry
/// macros from `pill_core` alone.
pub use tracing;

/// The `rayon` crate, re-exported so every DLL runs parallel work on the one
/// global thread pool inside `pill_core.dll`, which no reload unmaps. Use
/// it through this path. A module must not build a pool of its own
/// (`ThreadPoolBuilder::build`): its threads would park in the module's
/// code, which a reload unmaps.
pub use rayon;

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
