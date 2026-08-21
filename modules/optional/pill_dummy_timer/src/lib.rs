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
const MODULE_NAME: &[u8] = b"pill_dummy_timer\0";

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
