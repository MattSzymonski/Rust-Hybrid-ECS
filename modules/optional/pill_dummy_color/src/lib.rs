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
const MODULE_NAME: &[u8] = b"pill_dummy_color\0";

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
pub fn get_color_a() -> f32 {
    15263.0
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
