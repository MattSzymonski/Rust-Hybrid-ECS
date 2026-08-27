//! Dummy optional engine module used to pad out module-loading tests.
//!
//! # Responsibilities
//!
//! - Defines the [`Adder`] struct with two dummy arithmetic methods.
//! - Exposes the [`double`] free function.
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
use pill_engine::pill_module;
use pill_engine::Engine;

// =============================================================================
// Struct
// =============================================================================

/// Dummy accumulator exercising basic arithmetic.
#[derive(Debug, Clone, Copy, Default)]
pub struct Adder {
    pub total: i32,
}

impl Adder {
    /// Adds `value` to the running total and returns the new total.
    pub fn add(&mut self, value: i32) -> i32 {
        self.total += value;
        self.total
    }

    /// Resets the running total to zero.
    pub fn reset(&mut self) {
        self.total = 0;
    }
}

// =============================================================================
// Free functions
// =============================================================================

/// Doubles the given value.
pub fn double(value: i32) -> i32 {
    value * 2
}

// =============================================================================
// Registration
// =============================================================================

/// Registers the module against the host engine. Returns zero on success.
///
/// Must be idempotent: the host calls it once per loaded generation and rolls
/// back to the previous library when it reports a non-zero status.
/// Public so a statically linked build can call it directly. With
/// `module-abi` on, `#[pill_module]` also exports it as
/// `pill_module_init` for the host to find in a loaded DLL; a shipping
/// build has no DLL and calls this function itself.
#[pill_module]
pub fn register(_engine: &mut Engine) -> u32 {
    0
}
