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
use pill_engine::pill_module;
#[cfg(feature = "module-abi")]
use pill_engine::Engine;

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
    1133.0
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
