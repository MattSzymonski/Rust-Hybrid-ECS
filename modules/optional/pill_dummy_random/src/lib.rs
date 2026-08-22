//! Dummy optional engine module used to pad out module-loading tests.
//!
//! # Responsibilities
//!
//! - Defines the [`LcgRandom`] struct with two dummy pseudo-random methods.
//! - Exposes the [`seed_from_u32`] free function.
//! - Registers through the optional-module ABI when the host loads it.
//!
//! # Design
//!
//! The crate carries no ECS state; [`register`] is a no-op that only reports
//! success, kept as a plain Rust function so the same crate can also be linked
//! statically into a monolithic build. The generator is a linear congruential
//! generator: not suitable for real randomness, only for a deterministic dummy
//! sequence.

// External crates
// `pill_module` must resolve in every build (the attribute is applied to
// `register` in source); `Engine` is only needed by the module-abi build,
// where `register` is actually compiled in.
#[cfg(feature = "module-abi")]
use pill_engine::Engine;
use pill_engine::pill_module;

// =============================================================================
// Struct
// =============================================================================

/// Dummy linear congruential generator producing a deterministic sequence.
#[derive(Debug, Clone, Copy)]
pub struct LcgRandom {
    pub state: u32,
}

impl LcgRandom {
    /// Advances the generator and returns the next value in its sequence.
    pub fn next_u32(&mut self) -> u32 {
        // Numeric Recipes LCG constants; fine for a dummy deterministic stream.
        self.state = self.state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.state
    }

    /// Returns the next value mapped into `0.0..1.0`.
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u32() as f32) / (u32::MAX as f32)
    }
}

// =============================================================================
// Free functions
// =============================================================================

/// Builds a generator seeded from `seed`.
pub fn seed_from_u32(seed: u32) -> LcgRandom {
    LcgRandom { state: seed }
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

