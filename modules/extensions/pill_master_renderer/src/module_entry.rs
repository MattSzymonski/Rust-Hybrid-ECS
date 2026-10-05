//! The C ABI a host drives this renderer through when it loads it as a module.
//!
//! # Responsibilities
//!
//! - Export `pill_renderer_attach`, which builds a renderer on a window the
//!   host describes as [`RawWindowData`], and `pill_renderer_detach`, which
//!   drops one inside this image.
//! - Keep panics and failures from crossing the boundary: both exports catch
//!   unwinds, and a failed attach is logged here and returned as null.
//!
//! # Design
//!
//! One named export per operation rather than one taking an operation number,
//! so a missing export fails at load with its name. The backend crosses as a
//! `Box<Box<dyn PillRenderer>>` turned into a raw pointer: the host calls the
//! trait's methods through it and hands it back to `pill_renderer_detach` to
//! drop, so the renderer is freed by the image that allocated it and its drop
//! glue runs while that image is mapped. Calling through `dyn PillRenderer`
//! across the boundary relies on host and module compiling the same
//! `pill_renderer_api` in one dependency graph - the rule the workspace
//! already enforces for `pill_core.dll`.
//!
//! The functions here are plain library code. The `#[no_mangle]` symbols come
//! from `__pill_renderer_entry_points!`, which the renderer's wrapper crate
//! expands. The library itself cannot carry them unmangled: every renderer
//! declares the same two names, and this crate is linked into the host's
//! dependency graph.

// Standard library
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};

// External crates
use pill_core::telemetry::telemetry_target;
use pill_renderer_api::{PillRenderer, RawWindowData};

// Current crate
use crate::renderer::Renderer;

/// Build a renderer on the window `window` describes.
///
/// Returns a `Box<Box<dyn PillRenderer>>` as a raw pointer, to be driven by
/// the host and released with [`pill_renderer_detach`], or null when the
/// window data is missing or the renderer could not be created; the reason is
/// logged here.
///
/// # Safety
///
/// `window`, when non-null, points to a valid [`RawWindowData`] naming a live
/// window, and the caller keeps that window alive until it has passed the
/// returned pointer to [`detach_backend`].
pub unsafe extern "C" fn attach_to_window(
    window: *const RawWindowData,
    width: u32,
    height: u32,
) -> *mut c_void {
    if window.is_null() {
        pill_core::error!(target: telemetry_target::ENGINE, "renderer attach was given no window");
        return std::ptr::null_mut();
    }
    // SAFETY: non-null and valid for reads by this function's contract; the
    // data is plain `Copy` integers.
    let window = unsafe { *window };
    let attached = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the caller keeps the window alive until detach, which is
        // after the renderer built here is dropped.
        unsafe { Renderer::new(window, width, height) }
    }));
    match attached {
        Ok(Ok(renderer)) => {
            let backend: Box<dyn PillRenderer> = Box::new(renderer);
            Box::into_raw(Box::new(backend)).cast()
        }
        Ok(Err(error)) => {
            pill_core::error!(
                target: telemetry_target::ENGINE,
                "the renderer could not attach to the window: {error}"
            );
            std::ptr::null_mut()
        }
        Err(_) => {
            pill_core::error!(
                target: telemetry_target::ENGINE,
                "the renderer panicked while attaching to the window"
            );
            std::ptr::null_mut()
        }
    }
}

/// Drop a renderer returned by [`attach_to_window`], inside this image.
///
/// A null `backend` is ignored.
///
/// # Safety
///
/// `backend` is null or a pointer [`attach_to_window`] of this same image
/// returned, not already detached, and not used again afterwards.
pub unsafe extern "C" fn detach_backend(backend: *mut c_void) {
    if backend.is_null() {
        return;
    }
    // SAFETY: by this function's contract the pointer came from
    // `Box::into_raw` in `pill_renderer_attach` and is released exactly once.
    let backend = unsafe { Box::from_raw(backend.cast::<Box<dyn PillRenderer>>()) };
    if catch_unwind(AssertUnwindSafe(|| drop(backend))).is_err() {
        pill_core::error!(
            target: telemetry_target::ENGINE,
            "the renderer panicked while detaching; its GPU state may have leaked"
        );
    }
}

/// Expands to this crate's renderer ABI exports, `pill_renderer_attach` and
/// `pill_renderer_detach`.
///
/// A wrapper crate compiles the renderer as a plain library and is itself the
/// artifact the host loads, so the symbols must land there: this library is in
/// the host's dependency graph, and every renderer declares the same two
/// names.
#[macro_export]
macro_rules! __pill_renderer_entry_points {
    () => {
        /// Builds a renderer on the window the host describes; the loadable
        /// counterpart of `module_entry::attach_to_window`, whose contract
        /// this function carries.
        ///
        /// # Safety
        ///
        /// `window`, when non-null, points to a valid `RawWindowData` naming a
        /// live window, and the caller keeps that window alive until it has
        /// passed the returned pointer to `pill_renderer_detach`.
        #[no_mangle]
        pub unsafe extern "C" fn pill_renderer_attach(
            window: *const $crate::RawWindowData,
            width: u32,
            height: u32,
        ) -> *mut ::core::ffi::c_void {
            // SAFETY: this function's contract is the one `attach_to_window`
            // states; the call forwards it.
            unsafe { $crate::module_entry::attach_to_window(window, width, height) }
        }

        /// Drops a renderer built by `pill_renderer_attach`; the loadable
        /// counterpart of `module_entry::detach_backend`.
        ///
        /// # Safety
        ///
        /// `backend` is null or a pointer `pill_renderer_attach` of this same
        /// image returned, not already detached, and not used again
        /// afterwards.
        #[no_mangle]
        pub unsafe extern "C" fn pill_renderer_detach(backend: *mut ::core::ffi::c_void) {
            // SAFETY: this function's contract is the one `detach_backend`
            // states; the call forwards it.
            unsafe { $crate::module_entry::detach_backend(backend) }
        }
    };
}
