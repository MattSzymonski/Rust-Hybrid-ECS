//! Renderer failures, and the capture that turns wgpu's errors into them.
//!
//! # Responsibilities
//!
//! - Re-export [`RendererError`], [`Result`] and [`ErrorContext`] from
//!   `pill_renderer_api`, where the host can name them without wgpu.
//! - Capture wgpu's uncaptured validation errors, which panic by default, and
//!   turn them into messages a caller can report (`capturing_validation`).

pub use pill_renderer_api::error::{ErrorContext, RendererError, Result};

/// Runs a block of GPU work with wgpu's errors captured rather than delivered
/// to its uncaptured-error handler.
///
/// wgpu reports a refused pipeline, bind group or texture through that handler,
/// which panics by default and takes the host down with it. There is no return
/// value to check, so a shader or a pass the driver will not accept arrives as a
/// crash rather than as a mistake to report. Capturing the errors here turns
/// them into a message the caller can attach to whatever asked for the work -
/// the pass, the shader, the material - which is the difference between "the
/// host died" and "this pass is not drawn, and here is why".
///
/// # Errors
///
/// Returns the captured validation error if the block provoked one, otherwise
/// the captured out-of-memory error; when neither was reported, the block's
/// value is returned as `Ok`.
pub(crate) fn capturing_validation<T>(
    device: &wgpu::Device,
    make: impl FnOnce() -> T,
) -> std::result::Result<T, String> {
    device.push_error_scope(wgpu::ErrorFilter::Validation);
    device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let value = make();

    // Scopes are a stack: the one pushed last is the one popped first.
    let out_of_memory = pollster::block_on(device.pop_error_scope());
    let validation = pollster::block_on(device.pop_error_scope());

    match validation.or(out_of_memory) {
        Some(error) => Err(error.to_string()),
        None => Ok(value),
    }
}
