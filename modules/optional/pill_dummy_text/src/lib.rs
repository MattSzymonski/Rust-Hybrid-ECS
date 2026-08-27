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
use pill_engine::pill_module;
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
/// Public so a statically linked build can call it directly. With
/// `module-abi` on, `#[pill_module]` also exports it as
/// `pill_module_init` for the host to find in a loaded DLL; a shipping
/// build has no DLL and calls this function itself.
#[pill_module]
pub fn register(_engine: &mut Engine) -> u32 {
    0
}
