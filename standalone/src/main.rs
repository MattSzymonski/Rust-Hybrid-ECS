//! Standalone host binary — thin dispatcher that selects between headless
//! and windowed modes at compile time based on the `rendering` feature.
//!
//! # Responsibilities
//!
//! - Gate the entry point on `#[cfg(feature = "rendering")]`.
//! - Delegate to [`headless::run`] or [`windowed::run`].
//!
//! # Design
//!
//! All hot-reload logic lives in the shared [`host`] crate. This binary
//! only adds the execution loop: a plain console loop in `headless`, or a
//! `winit` + `wgpu` render loop in `windowed`.

#[cfg(not(feature = "rendering"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    headless::run()
}

#[cfg(feature = "rendering")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    windowed::run()
}

mod headless;

#[cfg(feature = "rendering")]
mod windowed;
