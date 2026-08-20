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

// Standard library
#[cfg(feature = "module-abi")]
use std::ffi::c_char;
#[cfg(feature = "module-abi")]
use std::panic::{catch_unwind, AssertUnwindSafe};

// External crates
#[cfg(feature = "module-abi")]
use pill_engine::*;

// =============================================================================
// Constants
// =============================================================================

/// Optional-module ABI revision this crate was built against.
#[cfg(feature = "module-abi")]
const MODULE_ABI_VERSION: u32 = 1;

/// Name reported to the host for diagnostics; null terminated for the C ABI.
#[cfg(feature = "module-abi")]
const MODULE_NAME: &[u8] = b"pill_dummy_random\0";

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
#[cfg(feature = "module-abi")]
pub fn register(_engine: &mut Engine) -> u32 {
    0
}

// =============================================================================
// Optional-module ABI exports
// =============================================================================
//
// Gated behind `module-abi` (on by default) so a crate linked directly into
// another binary, such as the project, can disable it: two crates exporting
// the same `#[no_mangle]` symbol names into one binary is a linker error. The
// standalone build the host hot-loads keeps the feature enabled.

/// Module ABI revision, checked by the host before anything else is called.
#[cfg(feature = "module-abi")]
#[no_mangle]
pub extern "C" fn pill_module_abi_version() -> u32 {
    MODULE_ABI_VERSION
}

/// Human-readable module name used in host log messages.
#[cfg(feature = "module-abi")]
#[no_mangle]
pub extern "C" fn pill_module_name() -> *const c_char {
    MODULE_NAME.as_ptr() as *const c_char
}

/// Registers the module against the host engine; returns zero on success.
///
/// # Safety
///
/// `api` must be a valid [`EngineApi`] pointer owned by the host and kept alive
/// for the whole duration of this call.
#[cfg(feature = "module-abi")]
#[no_mangle]
pub unsafe extern "C" fn pill_module_init(api: *const EngineApi) -> u32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: The host guarantees `api` points at a live `EngineApi` whose
        // `engine_handle` addresses the single engine instance, and that both
        // outlive this call.
        let api = unsafe { &*api };
        let engine = unsafe { &mut *(api.engine_handle as *mut Engine) };
        register(engine)
    }));
    result.unwrap_or(u32::MAX)
}
