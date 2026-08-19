//! The exported C-ABI surface of the engine runtime dynamic library.
//!
//! # Responsibilities
//!
//! - Export the single accessor the host resolves and the `'static` function
//!   table it returns.
//! - Translate every boundary call into a safe Rust call on [`Runtime`].
//! - Contain panics and report failures through the thread-local error slot.
//!
//! # Design
//!
//! A panic must never unwind across an `extern "C"` frame: doing so is
//! undefined behaviour and would corrupt the host that owns the window and the
//! event loop. Every entry point therefore runs its body inside
//! [`std::panic::catch_unwind`] and maps a caught panic to
//! [`PILL_ERR`](pill_runtime_api::PILL_ERR) plus a stored message. Catching
//! rather than aborting keeps the diagnostic: the host can report exactly
//! which call failed and roll back to the previous generation instead of
//! taking the whole process down.
//!
//! Failure messages live in a thread-local `CString` so a caller can read them
//! immediately after a failing call without any allocation crossing the
//! boundary. All calls happen on the host's main thread, so one slot per
//! thread is both sufficient and free of synchronisation.

// Standard library
use std::cell::RefCell;
use std::ffi::{c_char, c_void, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

// External crates
use pill_core::error::EngineMessage;
use pill_runtime_api::{
    feature_mask_difference, features_are_compatible, host_feature_mask_names, CapturedWorldState,
    FrameReport, PillRuntimeApiV1, PillRuntimeCreateArgsV1, PillWindowHandleV1, RenderViewport,
    RuntimeHandle, VirtualResolution, PILL_ERR, PILL_OK, PILL_RUNTIME_ABI_VERSION,
};

// Current crate
use crate::project::{decode_project_descriptor, optional_string};
use crate::runtime::Runtime;
use crate::state;

// =============================================================================
// Constants
// =============================================================================

/// Feature bits this dynamic library was compiled with.
///
/// Compared against the host's mask on `create`, because a feature difference
/// changes which subsystems exist on either side without changing any struct
/// layout that `struct_size` could catch.
const RUNTIME_FEATURE_MASK: u32 = {
    let mut mask = 0;
    if cfg!(feature = "rendering") {
        mask |= pill_runtime_api::PILL_RUNTIME_FEATURE_RENDERING;
    }
    if cfg!(feature = "metrics") {
        mask |= pill_runtime_api::PILL_RUNTIME_FEATURE_METRICS;
    }
    if cfg!(feature = "profiling") {
        mask |= pill_runtime_api::PILL_RUNTIME_FEATURE_PROFILING;
    }
    if cfg!(feature = "dev-logs") {
        mask |= pill_runtime_api::PILL_RUNTIME_FEATURE_DEV_LOGS;
    }
    mask
};

thread_local! {
    /// Message describing the most recent failure on the calling thread.
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

// =============================================================================
// Free Functions - error channel
// =============================================================================

/// Store a failure message for the host to read after a failing call.
fn set_last_error(message: impl Into<String>) {
    let message = message.into();
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = CString::new(message)
            .unwrap_or_else(|_| CString::new("runtime error").unwrap_or_default());
    });
}

/// Run one boundary body, mapping failures and panics onto a status code.
///
/// A panic is caught here rather than allowed to unwind into the host's C
/// frame. `AssertUnwindSafe` is sound because every failure path leaves the
/// runtime untouched or already reported: the host reacts to `PILL_ERR` by
/// keeping its previous generation rather than by continuing to use this one.
fn guarded<F>(operation: &str, body: F) -> i32
where
    F: FnOnce() -> Result<(), String>,
{
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(())) => PILL_OK,
        Ok(Err(message)) => {
            set_last_error(format!("{operation} failed: {message}"));
            PILL_ERR
        }
        Err(payload) => {
            set_last_error(format!("{operation} panicked: {}", panic_message(&payload)));
            PILL_ERR
        }
    }
}

/// Run one infallible boundary body, containing any panic it raises.
///
/// Used by the calls whose contract has no status code, where the only
/// available response to a panic is to record it and return.
fn guarded_unit<F>(operation: &str, body: F)
where
    F: FnOnce(),
{
    if let Err(payload) = catch_unwind(AssertUnwindSafe(body)) {
        set_last_error(format!("{operation} panicked: {}", panic_message(&payload)));
    }
}

/// Extract a readable message from a caught panic payload.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    String::from("unknown panic payload")
}

/// Borrow a runtime handle as a mutable reference.
///
/// # Errors
///
/// Returns a message when the handle is null, which is the only invalid value
/// the contract allows a caller to produce.
///
/// # Safety
///
/// A non-null `handle` must be one returned by [`create`] and not yet
/// destroyed, and no other reference to the same runtime may be live.
unsafe fn runtime_from_handle<'a>(handle: RuntimeHandle) -> Result<&'a mut Runtime, String> {
    if handle.is_null() {
        return Err(String::from("the runtime handle is null"));
    }
    // SAFETY: The caller guarantees a non-null handle came from `create`, is
    // not yet destroyed, and is not aliased by another live reference.
    Ok(unsafe { &mut *(handle as *mut Runtime) })
}

// =============================================================================
// Free Functions - exported table
// =============================================================================

/// The `'static` table every host call goes through.
static RUNTIME_API: PillRuntimeApiV1 = PillRuntimeApiV1 {
    struct_size: std::mem::size_of::<PillRuntimeApiV1>() as u32,
    abi_version: PILL_RUNTIME_ABI_VERSION,
    abi_hash: 0,
    last_error_utf8,
    create,
    destroy,
    run_one_frame,
    take_frame_report,
    current_frame_report,
    resize,
    set_viewport,
    set_virtual_resolution,
    retarget_render_window,
    reload_project,
    capture_world_state,
    restore_world_state,
    release_world_state,
    state_byte_len,
    describe_state,
    is_exit_requested,
};

/// Hand the host this runtime's function table.
///
/// This is the only symbol the host resolves; everything else is reached
/// through the returned table, so the runtime exports no other public surface.
///
/// # Safety
///
/// Callers must treat the returned pointer as borrowed for as long as the
/// module stays mapped and must not free it.
#[no_mangle]
pub extern "C" fn get_pill_runtime_api_v1() -> *const PillRuntimeApiV1 {
    &RUNTIME_API
}

// =============================================================================
// Free Functions - diagnostics
// =============================================================================

/// Return the message describing the calling thread's most recent failure.
extern "C" fn last_error_utf8() -> *const c_char {
    LAST_ERROR.with(|slot| slot.borrow().as_ptr())
}

// =============================================================================
// Free Functions - lifecycle
// =============================================================================

/// Bring up one runtime generation from the host's creation arguments.
extern "C" fn create(args: *const PillRuntimeCreateArgsV1, out_runtime: *mut RuntimeHandle) -> i32 {
    guarded("create", || {
        if args.is_null() || out_runtime.is_null() {
            return Err(String::from("create received a null argument pointer"));
        }

        // SAFETY: The contract requires `args` to point to a live struct for
        // the duration of this call; the null case returned above.
        let args = unsafe { &*args };

        // Step 1: Reject a host built against a different contract or feature
        // set before anything is allocated or any subsystem is started.
        if args.struct_size as usize != std::mem::size_of::<PillRuntimeCreateArgsV1>() {
            return Err(format!(
                "create args layout mismatch: runtime expects {} bytes, host reports {} bytes",
                std::mem::size_of::<PillRuntimeCreateArgsV1>(),
                args.struct_size,
            ));
        }
        if args.abi_version != PILL_RUNTIME_ABI_VERSION {
            return Err(format!(
                "create args ABI version mismatch: runtime expects {PILL_RUNTIME_ABI_VERSION}, host reports {}",
                args.abi_version,
            ));
        }
        if !features_are_compatible(args.features_mask, RUNTIME_FEATURE_MASK) {
            let (missing, unexpected) =
                feature_mask_difference(args.features_mask, RUNTIME_FEATURE_MASK);
            return Err(format!(
                "feature mismatch: host built with [{}], runtime built with [{}] (runtime is missing [{missing}], runtime unexpectedly has [{unexpected}])",
                host_feature_mask_names(args.features_mask),
                host_feature_mask_names(RUNTIME_FEATURE_MASK),
            ));
        }

        // Step 2: Route this module's telemetry into the host's pipeline
        // before any subsystem can log, so startup diagnostics are not lost.
        // SAFETY: The contract requires the host to keep both sinks valid for
        // at least the lifetime of this generation.
        unsafe { crate::telemetry::install(args.log_sink, args.metrics_sink) };

        // Step 3: Decode the project half of the arguments.
        // SAFETY: Every string pointer in `args` is a NUL-terminated buffer
        // the host keeps valid for the duration of this call.
        let workspace_root =
            unsafe { optional_string(args.workspace_root_utf8, "workspace_root")? }.ok_or_else(
                || String::from("create args field 'workspace_root' must not be null"),
            )?;
        // SAFETY: Same contract as above for the project sub-struct.
        let descriptor = unsafe { decode_project_descriptor(args)? };

        // Step 4: Build the generation, then bind it to the host's window.
        let mut runtime = Box::new(
            Runtime::new(PathBuf::from(workspace_root), descriptor)
                .map_err(|error| error.to_plain_message())?,
        );
        attach_window(&mut runtime, args)?;

        // SAFETY: The contract requires `out_runtime` to address a writable
        // handle slot for the duration of this call; the null case returned
        // above.
        unsafe { *out_runtime = Box::into_raw(runtime) as RuntimeHandle };
        Ok(())
    })
}

/// Bind a freshly created generation to the host's window, when it has one.
///
/// Split out so the headless build compiles without any window handling at
/// all rather than carrying a permanently ignored branch.
#[cfg(feature = "rendering")]
fn attach_window(runtime: &mut Runtime, args: &PillRuntimeCreateArgsV1) -> Result<(), String> {
    if args.window.is_null() {
        return Ok(());
    }

    // SAFETY: The contract requires a non-null window descriptor to point to a
    // live struct for the duration of the `create` call.
    let window = unsafe { &*args.window };
    if !window.has_expected_layout() {
        return Err(format!(
            "window descriptor layout mismatch: runtime expects {} bytes, host reports {} bytes",
            std::mem::size_of::<PillWindowHandleV1>(),
            window.struct_size,
        ));
    }

    // SAFETY: The contract requires the host to keep the described window
    // alive for at least the lifetime of this runtime generation.
    unsafe { runtime.attach_renderer(window, args.width, args.height) }
}

/// A headless runtime never binds a surface, so a supplied window is ignored.
#[cfg(not(feature = "rendering"))]
fn attach_window(_runtime: &mut Runtime, _args: &PillRuntimeCreateArgsV1) -> Result<(), String> {
    Ok(())
}

/// Tear down one runtime generation.
extern "C" fn destroy(handle: RuntimeHandle) {
    guarded_unit("destroy", || {
        if handle.is_null() {
            return;
        }
        // SAFETY: The contract requires `handle` to be a live handle produced
        // by `create` and destroyed exactly once. Reconstructing the box runs
        // the drop order the runtime declares: renderer and engine first, then
        // the project module whose code their drop glue may still call.
        drop(unsafe { Box::from_raw(handle as *mut Runtime) });
    });
}

// =============================================================================
// Free Functions - frame and rendering
// =============================================================================

/// Run one frame of the loaded generation.
extern "C" fn run_one_frame(handle: RuntimeHandle) -> i32 {
    guarded("run_one_frame", || {
        // SAFETY: The contract requires a live, unaliased handle.
        unsafe { runtime_from_handle(handle)? }.run_one_frame()
    })
}

/// Take the periodic console report produced by the last frame.
extern "C" fn take_frame_report(handle: RuntimeHandle, out_report: *mut FrameReport) -> i32 {
    let mut produced = 0;
    let status = guarded("take_frame_report", || {
        if out_report.is_null() {
            return Err(String::from(
                "take_frame_report received a null report slot",
            ));
        }
        // SAFETY: The contract requires a live, unaliased handle.
        if let Some(report) = unsafe { runtime_from_handle(handle)? }.take_frame_report() {
            // SAFETY: The contract requires `out_report` to address a writable
            // `FrameReport` for the duration of this call.
            unsafe { *out_report = report };
            produced = 1;
        }
        Ok(())
    });

    // A failure reports "no report" rather than a status code, because this
    // call's contract is a presence flag; the message is still readable.
    if status == PILL_OK {
        produced
    } else {
        0
    }
}

/// Read live frame statistics without consuming the periodic report.
extern "C" fn current_frame_report(handle: RuntimeHandle, out_report: *mut FrameReport) -> i32 {
    guarded("current_frame_report", || {
        if out_report.is_null() {
            return Err(String::from(
                "current_frame_report received a null report slot",
            ));
        }
        // SAFETY: The contract requires a live, unaliased handle.
        let report = unsafe { runtime_from_handle(handle)? }.current_frame_report();
        // SAFETY: The contract requires `out_report` to address a writable
        // `FrameReport` for the duration of this call.
        unsafe { *out_report = report };
        Ok(())
    })
}

/// Forward a physical window resize.
extern "C" fn resize(handle: RuntimeHandle, width: u32, height: u32) {
    guarded_unit("resize", || {
        // SAFETY: The contract requires a live, unaliased handle.
        if let Ok(runtime) = unsafe { runtime_from_handle(handle) } {
            runtime.resize(width, height);
        }
    });
}

/// Restrict drawing to a physical region, or restore the full surface.
extern "C" fn set_viewport(handle: RuntimeHandle, viewport: *const RenderViewport) -> i32 {
    guarded("set_viewport", || {
        // SAFETY: The contract requires a live, unaliased handle.
        let runtime = unsafe { runtime_from_handle(handle)? };
        // SAFETY: The contract requires a non-null viewport to point to a live
        // value for the duration of this call; null means "full surface".
        let viewport = (!viewport.is_null()).then(|| unsafe { *viewport });
        apply_viewport(runtime, viewport);
        Ok(())
    })
}

/// Map a stable project coordinate space into the current viewport.
extern "C" fn set_virtual_resolution(
    handle: RuntimeHandle,
    resolution: *const VirtualResolution,
) -> i32 {
    guarded("set_virtual_resolution", || {
        // SAFETY: The contract requires a live, unaliased handle.
        let runtime = unsafe { runtime_from_handle(handle)? };
        // SAFETY: The contract requires a non-null resolution to point to a
        // live value for the duration of this call; null means "one to one".
        let resolution = (!resolution.is_null()).then(|| unsafe { *resolution });
        apply_virtual_resolution(runtime, resolution);
        Ok(())
    })
}

/// Move rendering to another native window.
extern "C" fn retarget_render_window(
    handle: RuntimeHandle,
    window: *const PillWindowHandleV1,
    width: u32,
    height: u32,
) -> i32 {
    guarded("retarget_render_window", || {
        // SAFETY: The contract requires a live, unaliased handle.
        let runtime = unsafe { runtime_from_handle(handle)? };
        if window.is_null() {
            return Err(String::from(
                "retarget_render_window received a null window descriptor",
            ));
        }
        // SAFETY: The contract requires a non-null window descriptor to point
        // to a live struct for the duration of this call.
        let window = unsafe { &*window };
        if !window.has_expected_layout() {
            return Err(format!(
                "window descriptor layout mismatch: runtime expects {} bytes, host reports {} bytes",
                std::mem::size_of::<PillWindowHandleV1>(),
                window.struct_size,
            ));
        }
        apply_retarget(runtime, window, width, height)
    })
}

/// Apply a viewport override to a rendering build.
#[cfg(feature = "rendering")]
fn apply_viewport(runtime: &mut Runtime, viewport: Option<RenderViewport>) {
    runtime.set_viewport(viewport);
}

/// A headless build has no surface to restrict.
#[cfg(not(feature = "rendering"))]
fn apply_viewport(_runtime: &mut Runtime, _viewport: Option<RenderViewport>) {}

/// Apply a logical-resolution override to a rendering build.
#[cfg(feature = "rendering")]
fn apply_virtual_resolution(runtime: &mut Runtime, resolution: Option<VirtualResolution>) {
    runtime.set_virtual_resolution(resolution);
}

/// A headless build has no projection to map.
#[cfg(not(feature = "rendering"))]
fn apply_virtual_resolution(_runtime: &mut Runtime, _resolution: Option<VirtualResolution>) {}

/// Rebuild the surface against another window in a rendering build.
#[cfg(feature = "rendering")]
fn apply_retarget(
    runtime: &mut Runtime,
    window: &PillWindowHandleV1,
    width: u32,
    height: u32,
) -> Result<(), String> {
    // SAFETY: The contract requires the host to keep the described window
    // alive for at least as long as this runtime renders into it.
    unsafe { runtime.retarget_render_window(window, width, height) }
}

/// A headless build cannot move a surface it never created.
#[cfg(not(feature = "rendering"))]
fn apply_retarget(
    _runtime: &mut Runtime,
    _window: &PillWindowHandleV1,
    _width: u32,
    _height: u32,
) -> Result<(), String> {
    Err(String::from(
        "this runtime was built without the rendering feature and has no surface to retarget",
    ))
}

// =============================================================================
// Free Functions - project module
// =============================================================================

/// Swap in a rebuilt project module, preserving world state.
extern "C" fn reload_project(handle: RuntimeHandle, project_module_path: *const c_char) -> i32 {
    guarded("reload_project", || {
        // SAFETY: The contract requires a live, unaliased handle.
        let runtime = unsafe { runtime_from_handle(handle)? };
        // SAFETY: The contract requires the path to be null or a live
        // NUL-terminated string for the duration of this call.
        let module_path = unsafe { optional_string(project_module_path, "project_module_path")? };
        runtime.reload_project(module_path.as_ref().map(PathBuf::from).as_deref());
        Ok(())
    })
}

// =============================================================================
// Free Functions - engine hot reload
// =============================================================================

/// Serialize the world into a new envelope owned by this runtime.
extern "C" fn capture_world_state(
    handle: RuntimeHandle,
    out_state: *mut *mut CapturedWorldState,
) -> i32 {
    guarded("capture_world_state", || {
        if out_state.is_null() {
            return Err(String::from(
                "capture_world_state received a null output slot",
            ));
        }
        // SAFETY: The contract requires a live, unaliased handle.
        let runtime = unsafe { runtime_from_handle(handle)? };
        let state = state::capture(runtime.engine())?;
        // SAFETY: The contract requires `out_state` to address a writable
        // pointer slot for the duration of this call.
        unsafe { *out_state = state };
        Ok(())
    })
}

/// Rebuild the world from an envelope written by any matching runtime.
extern "C" fn restore_world_state(
    handle: RuntimeHandle,
    captured: *const CapturedWorldState,
) -> i32 {
    guarded("restore_world_state", || {
        // SAFETY: The contract requires a live, unaliased handle.
        let runtime = unsafe { runtime_from_handle(handle)? };
        // SAFETY: The contract requires the envelope and its payload to stay
        // valid for the duration of this call.
        let report = unsafe { state::restore(runtime.engine_mut(), captured)? };
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            entities = report.restored_entity_count,
            resources = report.restored_resource_count,
            migrated_types = report.migrated_type_names.len(),
            dropped_types = report.dropped_type_names.len(),
            added_types = report.added_type_names.len(),
            "world state restored across the engine swap"
        );
        Ok(())
    })
}

/// Release an envelope this runtime allocated.
extern "C" fn release_world_state(captured: *mut CapturedWorldState) {
    guarded_unit("release_world_state", || {
        // SAFETY: The contract requires the envelope to have been produced by
        // this same module and to be released exactly once.
        unsafe { state::release(captured) };
    });
}

/// Payload size of an envelope, in bytes.
extern "C" fn state_byte_len(captured: *const CapturedWorldState) -> u64 {
    let mut length = 0;
    guarded_unit("state_byte_len", || {
        // SAFETY: The contract requires a non-null envelope to be live for the
        // duration of this call.
        length = unsafe { state::byte_len(captured) };
    });
    length
}

/// Write a human summary of an envelope to `out_summary`.
extern "C" fn describe_state(
    captured: *const CapturedWorldState,
    out_summary: *mut *const c_char,
) -> i32 {
    guarded("describe_state", || {
        if out_summary.is_null() {
            return Err(String::from("describe_state received a null output slot"));
        }
        // SAFETY: The contract requires a non-null envelope to be live for the
        // duration of this call.
        let summary = unsafe { state::describe(captured) };
        if summary.is_null() {
            return Err(String::from("the envelope carries no summary"));
        }
        // SAFETY: The contract requires `out_summary` to address a writable
        // pointer slot for the duration of this call.
        unsafe { *out_summary = summary };
        Ok(())
    })
}

// =============================================================================
// Free Functions - exit signal
// =============================================================================

/// Report whether the runtime asked the host to stop the frame loop.
extern "C" fn is_exit_requested(handle: RuntimeHandle) -> i32 {
    let mut requested = 0;
    guarded_unit("is_exit_requested", || {
        // SAFETY: The contract requires a live, unaliased handle.
        if let Ok(runtime) = unsafe { runtime_from_handle(handle) } {
            requested = i32::from(runtime.is_exit_requested());
        }
    });
    requested
}

// =============================================================================
// Free Functions - unused-import guards
// =============================================================================

/// Keep the opaque-pointer type named in this module's imports meaningful for
/// every feature combination.
#[allow(dead_code)]
fn assert_opaque_pointer_type(_: *mut c_void) {}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The exported table passes the contract's own load-time validation.
    #[test]
    fn exported_table_validates_against_the_contract() {
        let table = get_pill_runtime_api_v1();
        // SAFETY: `get_pill_runtime_api_v1` returns this module's `'static`
        // table, which stays valid for the whole process.
        assert!(unsafe { pill_runtime_api::validate_api_table(table) }.is_ok());
    }

    /// A failure recorded by one call is readable through the error channel.
    #[test]
    fn failures_are_reported_through_the_error_channel() {
        assert_eq!(run_one_frame(std::ptr::null_mut()), PILL_ERR);
        // SAFETY: `last_error_utf8` returns this thread's live error slot.
        let message = unsafe { std::ffi::CStr::from_ptr(last_error_utf8()) };
        let message = message.to_str().unwrap();
        assert!(message.contains("run_one_frame failed"), "{message}");
        assert!(message.contains("null"), "{message}");
    }

    /// A panic inside a boundary body is contained and reported, not unwound.
    #[test]
    fn panics_are_contained_and_reported() {
        let status = guarded("unit_test", || panic!("boom"));
        assert_eq!(status, PILL_ERR);
        // SAFETY: `last_error_utf8` returns this thread's live error slot.
        let message = unsafe { std::ffi::CStr::from_ptr(last_error_utf8()) };
        let message = message.to_str().unwrap();
        assert!(message.contains("unit_test panicked"), "{message}");
        assert!(message.contains("boom"), "{message}");
    }

    /// Every diagnostic entry point tolerates a null envelope.
    #[test]
    fn null_state_arguments_are_rejected_without_unwinding() {
        assert_eq!(state_byte_len(std::ptr::null()), 0);
        let mut summary: *const c_char = std::ptr::null();
        assert_eq!(
            describe_state(std::ptr::null(), &mut summary as *mut *const c_char),
            PILL_ERR
        );
        release_world_state(std::ptr::null_mut());
    }

    /// The runtime's feature mask reflects the features it was compiled with.
    #[test]
    fn runtime_feature_mask_matches_the_build() {
        assert_eq!(
            RUNTIME_FEATURE_MASK & pill_runtime_api::PILL_RUNTIME_FEATURE_RENDERING != 0,
            cfg!(feature = "rendering")
        );
        assert_eq!(
            RUNTIME_FEATURE_MASK & pill_runtime_api::PILL_RUNTIME_FEATURE_METRICS != 0,
            cfg!(feature = "metrics")
        );
    }
}
