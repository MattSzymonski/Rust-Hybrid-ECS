//! Dummy optional engine module used to pad out module-loading tests.
//!
//! # Responsibilities
//!
//! - Defines the [`Tint`] struct with two dummy color-blending methods.
//! - Exposes the [`grayscale`] free function.
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
#[cfg(feature = "module-abi")]
use pill_engine::Engine;
use pill_engine::{pill_hot_fn, pill_module};

// SPIKE: the build script scans this crate and submits one address entry per
// function, so the host can resolve any of them by qualified path with nothing
// in this file annotated. One `include!` per crate replaces every attribute.
include!(concat!(env!("OUT_DIR"), "/function_inventory.rs"));

// =============================================================================
// Struct
// =============================================================================

/// Dummy RGB color used for blending demos.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tint {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

impl Tint {
    /// Linearly blends this color halfway towards `other`.
    pub fn mix(&self, other: Tint) -> Tint {
        Tint {
            r: (self.r + other.r) * 0.5,
            g: (self.g + other.g) * 0.5,
            b: (self.b + other.b) * 0.5,
        }
    }

    /// Inverts each channel, assuming values in the `0.0..=1.0` range.
    pub fn invert(&self) -> Tint {
        Tint {
            r: 1.0 - self.r,
            g: 1.0 - self.g,
            b: 1.0 - self.b,
        }
    }
}

// =============================================================================
// Free functions
// =============================================================================

/// Averages the channels of `tint` into a single gray value.
pub fn grayscale(tint: Tint) -> f32 {
    (tint.r + tint.g + tint.b) / 3.0
}

/// Dummy alpha channel: `Tint` carries no alpha, so this always reports fully
/// opaque, for other crates to call as a stand-in.
#[pill_hot_fn]
pub fn get_color_a() -> f32 {
    22221.0
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

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The function must be discoverable by its qualified path, which is how a
    /// host addresses it across the ABI.
    #[test]
    fn function_is_registered_under_its_qualified_path() {
        assert!(
            pill_engine::hot_patch::plain_function_names()
                .any(|name| name == "pill_dummy_color::get_color_a"),
            "declared functions: {:?}",
            pill_engine::hot_patch::plain_function_names().collect::<Vec<_>>()
        );
    }

    /// A signature that does not match is refused, leaving the original
    /// implementation in place. This is what stops a reshaped function being
    /// installed behind call sites compiled for the old shape.
    #[test]
    fn a_mismatched_signature_is_refused() {
        fn replacement() -> f32 {
            42.0
        }
        let result = pill_engine::hot_patch::install_plain_function(
            "pill_dummy_color::get_color_a",
            replacement as *const () as usize,
            "(some other shape)",
        );
        assert!(result.is_err(), "a changed signature must be refused");
    }

    /// The dispatcher forwards to the original body, an installed replacement
    /// redirects every caller of the public name, and a reset returns the
    /// function to its own code.
    ///
    /// One test rather than three, because the slot is a process-wide `static`
    /// and the test harness runs tests on several threads: separate tests would
    /// observe each other's installs in whatever order the threads happened to
    /// interleave. Asserting the sequence in one body is what makes it
    /// deterministic.
    #[test]
    fn the_slot_dispatches_installs_and_resets() {
        fn replacement() -> f32 {
            999.0
        }

        // Read rather than hardcode: this function's whole purpose is to have
        // its value edited, so asserting the literal made the test break every
        // time someone used it for what it is for.
        let original = get_color_a();

        // The recorded text, not a hand-written guess: the spelling comes from
        // `stringify!` inside the macro.
        let signature =
            pill_engine::hot_patch::plain_function_signature("pill_dummy_color::get_color_a")
                .expect("the function must be registered");

        pill_engine::hot_patch::install_plain_function(
            "pill_dummy_color::get_color_a",
            replacement as *const () as usize,
            signature,
        )
        .expect("install with the recorded signature must be accepted");
        assert_eq!(get_color_a(), 999.0, "callers must see the replacement");

        pill_engine::hot_patch::reset_plain_function("pill_dummy_color::get_color_a")
            .expect("reset must find the registered function");
        assert_eq!(
            get_color_a(),
            original,
            "a reset must return the function to its own body"
        );
    }
}
