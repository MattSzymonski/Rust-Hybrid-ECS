//! Renderer failures, and the capture that turns wgpu's errors into them.
//!
//! # Responsibilities
//!
//! - Re-export [`RendererError`], [`Result`] and [`ErrorContext`] from
//!   `pill_renderer_api`, where the host can name them without wgpu.
//! - Log the GPU errors nothing captured ([`report_uncaptured_errors`]),
//!   instead of wgpu's default of panicking.
//! - Latch a lost device ([`latch_device_loss`]) and hand it to the frame path
//!   ([`device_loss`]), so a driver reset ends the run with a reason instead of
//!   a window that never updates again.
//! - Capture the errors a block of GPU work provoked: awaited while the
//!   renderer is built ([`captured`]), and, in a native development build,
//!   blocked on for work done during a frame (`capturing_validation`).
//!
//! # Design
//!
//! Waiting for an error scope is asynchronous, and a browser only answers it
//! once control returns to its event loop, so a frame cannot block on one
//! there. Blocking capture is therefore a development capability: it lets an
//! edited shader the driver refuses be reported and skipped instead of reaching
//! the error handler, which in every other build logs the refusal.
//!
//! A lost device reaches neither path: wgpu answers every call on it with
//! success, so only the callback [`latch_device_loss`] installs sees the loss,
//! and it has to be carried to the frame path rather than logged and forgotten.

// Standard library
use std::sync::{Mutex, MutexGuard, PoisonError};

// External crates
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
    device.on_uncaptured_error(std::sync::Arc::new(|error| {
        pill_core::error!(
            target: pill_core::telemetry::telemetry_target::RENDERING,
            "GPU error: {error}"
        );
    }));
}

/// The device loss this process has seen, if any, in the words the frame path
/// reports.
///
/// Module state rather than a renderer field: the loss arrives on a driver
/// thread, and the renderer that has to act on it is the one this image built.
/// A reload gives a fresh image with a fresh static, which is the right answer -
/// the replacement renderer creates its own device.
static DEVICE_LOSS: Mutex<Option<String>> = Mutex::new(None);

/// The latch, taken without a panic path: a poisoned lock still holds the last
/// thing written, which is all a reader wants.
fn latched_loss() -> MutexGuard<'static, Option<String>> {
    DEVICE_LOSS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Report a lost device, and latch it for the frame path.
///
/// A lost device accepts everything and draws nothing: writes, submits and
/// polls all return success, and no uncaptured error is raised, so nothing in
/// the frame path can tell that the picture stopped moving. The callback wgpu
/// fires for the loss is therefore the only witness, and it runs on a driver
/// thread; the latch carries its answer to [`device_loss`], which the next
/// frame reports.
///
/// Installed beside [`report_uncaptured_errors`] when the device is created,
/// which also clears a loss the previous device of this image recorded.
pub(crate) fn latch_device_loss(device: &wgpu::Device) {
    *latched_loss() = None;
    device.set_device_lost_callback(|reason, message| {
        let detail = if message.is_empty() {
            format!("the GPU device was lost ({reason:?})")
        } else {
            format!("the GPU device was lost ({reason:?}): {message}")
        };
        pill_core::error!(
            target: pill_core::telemetry::telemetry_target::RENDERING,
            "{detail}"
        );
        *latched_loss() = Some(detail);
    });
}

/// The reason to stop drawing, when the device this renderer built has been
/// lost since it was created.
pub(crate) fn device_loss() -> Option<String> {
    latched_loss().clone()
}

/// Run `make` with wgpu's errors captured, and await what it provoked.
///
/// For work done while the renderer is built, which is already asynchronous:
/// every target can await the scopes there.
pub(crate) async fn captured<T>(
    device: &wgpu::Device,
    make: impl FnOnce() -> T,
) -> (T, CapturedErrors) {
    let validation_scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let out_of_memory_scope = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let internal_scope = device.push_error_scope(wgpu::ErrorFilter::Internal);
    let value = make();

    // Scopes are a stack: the one pushed last is the one popped first.
    let internal = internal_scope.pop().await;
    let out_of_memory = out_of_memory_scope.pop().await;
    let validation = validation_scope.pop().await;
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
/// with no return value to check. Capturing turns the refusal into a message
/// the caller can attach to whatever asked for the work, which is the
/// difference between "this frame failed" and "this pass is not drawn, and here
/// is why". Where the build cannot capture, the block's value is returned and
/// the handler logs the refusal.
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

#[cfg(test)]
mod tests {
    // Current crate
    use super::{device_loss, latch_device_loss};

    /// A device without a surface, preferring the platform's software adapter.
    async fn windowless_device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            flags: wgpu::InstanceFlags::from_build_config(),
            backend_options: wgpu::BackendOptions::default(),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: None,
                force_fallback_adapter: true,
                apply_limit_buckets: false,
            })
            .await
            .ok()?;
        adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("device loss test device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::default(),
                trace: wgpu::Trace::default(),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
            })
            .await
            .ok()
    }

    /// A destroyed device accepts every call and reports nothing, so the frame
    /// path can only learn of the loss through the latch the callback fills.
    #[test]
    fn a_lost_device_is_latched_and_reported() {
        let Some((device, _queue)) = pill_core::platform::futures::block_on(windowless_device())
        else {
            eprintln!("no GPU adapter available; skipped");
            return;
        };
        // The real handler logs; a test wants to read the latch instead, so it
        // installs the same callback through the same entry point and then
        // checks what the frame path would see.
        latch_device_loss(&device);
        assert_eq!(device_loss(), None, "a fresh device reports no loss");

        device.destroy();
        // A loss is only reported once the queue has drained.
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();

        let reported = device_loss().expect("the destroyed device is latched as lost");
        assert!(
            reported.contains("lost"),
            "the report says what happened: {reported}"
        );
    }
}
