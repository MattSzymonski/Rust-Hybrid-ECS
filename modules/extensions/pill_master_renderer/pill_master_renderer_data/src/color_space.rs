//! The sRGB transfer curve, one channel at a time.
//!
//! # Responsibilities
//!
//! - [`srgb_to_linear`]: decode an sRGB-encoded channel to linear light, the
//!   way texture import reads an 8-bit image.
//! - [`linear_to_srgb`]: encode a linear channel as sRGB, which is how the GPU
//!   module prepares a linear debug label colour for the UI painter that
//!   decodes sRGB vertex colours.
//!
//! # Design
//!
//! Plain arithmetic with no GPU behind it, so it lives in the data crate and
//! both the importer here and the GPU module use the one copy. The shaders
//! carry their own WGSL version of the same curve.

/// Converts one sRGB-encoded channel in `[0, 1]` to linear light.
pub fn srgb_to_linear(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// Converts one linear-light channel in `[0, 1]` to its sRGB encoding.
pub fn linear_to_srgb(value: f32) -> f32 {
    if value <= 0.0031308 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_curve_keeps_black_and_white() {
        assert_eq!(srgb_to_linear(0.0), 0.0);
        assert_eq!(linear_to_srgb(0.0), 0.0);
        assert!((srgb_to_linear(1.0) - 1.0).abs() < 1e-6);
        assert!((linear_to_srgb(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_midtone_decodes_darker_and_round_trips() {
        let linear = srgb_to_linear(0.5);
        assert!(
            (linear - 0.2140).abs() < 1e-3,
            "0.5 in sRGB is about 0.214 linear"
        );
        for step in 0..=20 {
            let value = step as f32 / 20.0;
            assert!(
                (linear_to_srgb(srgb_to_linear(value)) - value).abs() < 1e-5,
                "{value} survives the round trip"
            );
        }
    }
}
