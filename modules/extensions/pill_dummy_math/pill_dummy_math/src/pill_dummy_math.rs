//! Placeholder extension with no ECS state.
//!
//! # Responsibilities
//!
//! - Defines the [`Adder`] struct with two arithmetic methods.
//! - Exposes the [`double`] free function.
//! - Registers through the extension ABI when the host loads it.
//!
//! # Design
//!
//! [`register`] does nothing except report success. It stays a plain Rust
//! function, so a build that links this crate statically can call it directly.

// External crates
// `pill_module` and `Engine` must resolve in every build: the attribute is
// applied to `register` in source, and `register` is compiled everywhere.
use pill_engine::pill_module;
use pill_engine::Engine;

// =============================================================================
// Struct
// =============================================================================

/// Dummy accumulator exercising basic arithmetic.
#[derive(Debug, Clone, Copy, Default)]
pub struct Adder {
    /// Sum of every amount added since the last reset.
    pub total: i32,
}

impl Adder {
    /// Adds `amount_to_add` to the running total and returns the new total.
    pub fn add(&mut self, amount_to_add: i32) -> i32 {
        self.total += amount_to_add;
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

/// Doubles the given number.
pub fn double(number_to_double: i32) -> i32 {
    number_to_double * 2
}

// =============================================================================
// Registration
// =============================================================================

/// Registers the module against the host engine. Returns zero on success.
///
/// The host calls it once each time it loads a new build of the module. If it
/// reports a non-zero status, the host goes back to the library that was
/// running before and calls that one's `register` again, so a second run must
/// restore what the first one set up.
///
/// It is public because a statically linked build calls it directly, and
/// because the generated `host_module_pill_dummy_math` wrapper calls it
/// through the `pill_module_init` entry point that wrapper exports.
#[pill_module]
pub fn register(_engine: &mut Engine) -> u32 {
    0
}
