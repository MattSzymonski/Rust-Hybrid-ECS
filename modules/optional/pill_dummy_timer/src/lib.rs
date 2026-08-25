//! Dummy optional engine module used to pad out module-loading tests.
//!
//! # Responsibilities
//!
//! - Defines the [`Stopwatch`] struct with two dummy time-tracking methods.
//! - Exposes the [`seconds_to_millis`] free function.
//! - Registers through the optional-module ABI when the host loads it.
//!
//! # Design
//!
//! The crate carries no ECS state; [`register`] is a no-op that only reports
//! success, kept as a plain Rust function so the same crate can also be linked
//! statically into a monolithic build.

// External crates
// `pill_module` must resolve in every build (the attribute is applied to
// `register` in source); `Engine` is only needed by the module-abi build,
// where `register` is actually compiled in.
use pill_engine::pill_module;
#[cfg(feature = "module-abi")]
use pill_engine::Engine;

// =============================================================================
// Struct
// =============================================================================

/// Dummy stopwatch accumulating elapsed seconds.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stopwatch {
    pub elapsed_seconds: f32,
}

impl Stopwatch {
    /// Advances the stopwatch by `delta_seconds`.
    pub fn tick(&mut self, delta_seconds: f32) {
        self.elapsed_seconds += delta_seconds;
    }

    /// Resets the stopwatch back to zero.
    pub fn reset(&mut self) {
        self.elapsed_seconds = 0.0;
    }
}

// =============================================================================
// Free functions
// =============================================================================

/// Converts seconds to whole milliseconds.
pub fn seconds_to_millis(seconds: f32) -> u64 {
    (seconds * 1000.0) as u64
}

// =============================================================================
// Registration
// =============================================================================

/// Registers the module against the host engine. Returns zero on success.
///
/// Must be idempotent: the host calls it once per loaded generation and rolls
/// back to the previous library when it reports a non-zero status.
#[pill_module]
fn register(_engine: &mut Engine) -> u32 {
    0
}
