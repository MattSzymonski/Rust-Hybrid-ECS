//! Renderer failures, and the capture that turns wgpu's errors into them.
//!
//! # Responsibilities
//!
//! - Re-export [`RendererError`], [`Result`] and [`ErrorContext`] from
//!   `pill_renderer_api`, where the host can name them without wgpu.
//! - Log the GPU errors nothing captured ([`report_uncaptured_errors`]),
//!   instead of wgpu's default of panicking.
//! - Capture the errors a block of GPU work provoked: awaited while the
//!   renderer is built ([`captured`]), and, in a native development build,
//!   blocked on for work done during a frame (`capturing_validation`).
//!
//! # Design
//!
//! Waiting for an error scope is asynchronous, and a browser only answers it
//! once control returns to its event loop, so a frame cannot block on one
//! there. Blocking capture is therefore a development capability: it is what
//! lets an edited shader that the driver refuses be reported and skipped
//! instead of reaching the error handler. Every other build reports such an
//! error through that handler, which logs it; shipped content has already been
//! through development, where the capture checked it.

pub use pill_renderer_api::error::{ErrorContext, RendererError, Result};

/// The errors a block of GPU work provoked, one per filter class.
#[derive(Debug, Default)]
pub(crate) struct CapturedErrors {
    /// A validation error: a descriptor the device refused.
    pub(crate) validation: Option<wgpu::Error>,
    /// The device ran out of memory.
    pub(crate) out_of_memory: Option<wgpu::Error>,
    /// An internal error in the driver or wgpu.
    pub(crate) internal: Option<wgpu::Error>,
}

/// Route GPU errors that no scope captured to the log.
///
/// wgpu panics on them by default, which takes the whole process down over a
/// refused pipeline or a lost surface. Logged, the frame that provoked one is
/// lost instead, and the log says why.
pub(crate) fn report_uncaptured_errors(device: &wgpu::Device) {
    device.on_uncaptured_error(Box::new(|error| {
        pill_core::error!(
            target: pill_core::telemetry::telemetry_target::RENDERING,
            "GPU error: {error}"
        );
    }));
}

/// Run `make` with wgpu's errors captured, and await what it provoked.
///
/// For work done while the renderer is built, which is already asynchronous:
/// every target can await the scopes there.
pub(crate) async fn captured<T>(
    device: &wgpu::Device,
    make: impl FnOnce() -> T,
) -> (T, CapturedErrors) {
    device.push_error_scope(wgpu::ErrorFilter::Validation);
    device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    device.push_error_scope(wgpu::ErrorFilter::Internal);
    let value = make();

    // Scopes are a stack: the one pushed last is the one popped first.
    let internal = device.pop_error_scope().await;
    let out_of_memory = device.pop_error_scope().await;
    let validation = device.pop_error_scope().await;
    let errors = CapturedErrors {
        validation,
        out_of_memory,
        internal,
    };
    (value, errors)
}

/// Run `make` with wgpu's errors captured, blocking until they are known.
///
/// Native development builds only: only a native build can block on a frame's
/// error scope, and only development needs to survive a refused edit.
#[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
pub(crate) fn captured_now<T>(
    device: &wgpu::Device,
    make: impl FnOnce() -> T,
) -> (T, CapturedErrors) {
    pill_core::platform::futures::block_on(captured(device, make))
}

/// Run `make` without capturing: elsewhere an error it provokes goes to the
/// handler [`report_uncaptured_errors`] installed.
#[cfg(any(not(debug_assertions), target_arch = "wasm32"))]
pub(crate) fn captured_now<T>(
    _device: &wgpu::Device,
    make: impl FnOnce() -> T,
) -> (T, CapturedErrors) {
    (make(), CapturedErrors::default())
}

/// Runs a block of GPU work with wgpu's errors captured rather than delivered
/// to its uncaptured-error handler, where the build can capture them.
///
/// wgpu reports a refused pipeline, bind group or texture through that handler,
/// with no return value to check. Capturing the errors here turns them into a
/// message the caller can attach to whatever asked for the work - the pass,
/// the shader, the material - which is the difference between "this frame
/// failed" and "this pass is not drawn, and here is why". Elsewhere the
/// block's value is always returned and a refusal is logged by the handler
/// instead.
///
/// # Errors
///
/// Returns the captured validation error if the block provoked one, otherwise
/// the captured out-of-memory or internal error; when none was reported, the
/// block's value is returned as `Ok`.
pub(crate) fn capturing_validation<T>(
    device: &wgpu::Device,
    make: impl FnOnce() -> T,
) -> std::result::Result<T, String> {
    let (value, errors) = captured_now(device, make);
    match errors
        .validation
        .or(errors.out_of_memory)
        .or(errors.internal)
    {
        Some(error) => Err(error.to_string()),
        None => Ok(value),
    }
}
