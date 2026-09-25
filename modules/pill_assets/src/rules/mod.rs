//! The cooking rules this crate ships.
//!
//! # Responsibilities
//!
//! - Re-export the rules that carry no dependency of their own.
//! - Assemble [`default_rules`], the set every pipeline runs.
//!
//! # Design
//!
//! Only dependency-free rules live here. A rule that has to decode a file
//! format - mesh, texture - belongs to the crate that already carries that
//! decoder, and is plugged into a [`crate::Pipeline`] through
//! [`crate::Pipeline::with_manifest`].

mod hlsl_to_wgsl;

pub use hlsl_to_wgsl::HlslToWgsl;

use crate::Rule;

/// The rules every pipeline runs.
///
/// Shader cooking is deliberately not optional here: the renderer `include_str!`s
/// the cooked WGSL, so a build that skipped the cook would have no shader at all
/// rather than a stale one.
pub fn default_rules() -> Vec<Box<dyn Rule>> {
    vec![Box::new(HlslToWgsl)]
}
