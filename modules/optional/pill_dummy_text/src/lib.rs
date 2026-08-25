//! Dummy optional engine module used to pad out module-loading tests.
//!
//! # Responsibilities
//!
//! - Defines the [`Greeter`] struct with two dummy text-formatting methods.
//! - Exposes the [`shout`] free function.
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

/// Dummy formatter that greets a fixed name.
#[derive(Debug, Clone)]
pub struct Greeter {
    pub name: String,
}

impl Greeter {
    /// Builds a friendly greeting for the stored name.
    pub fn greeting(&self) -> String {
        format!("Hello, {}!", self.name)
    }

    /// Builds a farewell for the stored name.
    pub fn farewell(&self) -> String {
        format!("Goodbye, {}!", self.name)
    }
}

// =============================================================================
// Free functions
// =============================================================================

/// Uppercases the given text.
pub fn shout(text: &str) -> String {
    text.to_uppercase()
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
