//! Stable C-ABI contract between the thin host loader and the reloadable
//! engine runtime dynamic library.
//!
//! # Responsibilities
//!
//! - Declare the versioned [`PillRuntimeApiV1`] function-pointer table the
//!   host calls and the runtime exports.
//! - Own every type that crosses the boundary: frame reports, viewport
//!   geometry, window descriptors, telemetry sinks, and the captured world
//!   state envelope.
//! - Provide the layout and version guards both sides run at load time.
//!
//! # Design
//!
//! This crate is the single source of truth for the host↔runtime boundary and
//! therefore depends on nothing from the engine, the host, or any windowing or
//! telemetry framework. Its only optional dependencies are `libloading` (a
//! host-side loading helper) and `raw-window-handle` (window descriptor
//! translation); both are off by default so the contract itself stays
//! dependency free.
//!
//! Every boundary struct is `#[repr(C)]` and built from explicit fixed-width
//! integers, pointers, and nullable function pointers - never `usize`, `bool`,
//! or Rust enums. Each struct carries a `struct_size` field so a mismatched
//! build is rejected instead of silently reinterpreting memory. Changes after
//! v1 must be additive; anything structural bumps
//! [`PILL_RUNTIME_ABI_VERSION`].
//!
//! ## Ownership rules
//!
//! - The runtime allocates [`CapturedWorldState`] in
//!   [`PillRuntimeApiV1::capture_world_state`] and frees it in
//!   [`PillRuntimeApiV1::release_world_state`]. Both calls must target the
//!   same loaded module, because the allocation belongs to that module's
//!   allocator.
//! - [`PillRuntimeApiV1::restore_world_state`] only reads the envelope, so a
//!   host may hand the same state to several runtimes while rolling back.
//! - Strings crossing the boundary are NUL-terminated UTF-8 owned by the
//!   caller for the duration of the call, except `last_error_utf8` and
//!   `describe_state`, which return runtime-owned buffers that stay valid
//!   until the next call on the same module.

// ===== Module Declarations =====

/// Telemetry sinks that route runtime logs and metrics back to the host.
mod sink;
/// Plain data types shared by the host and the runtime.
mod types;
/// Version constants, status codes, and feature-parity bit flags.
mod version;
/// The function-pointer table, its creation arguments, and layout guards.
mod vtable;

/// Host-side helpers for loading and validating a runtime dynamic library.
#[cfg(feature = "loader")]
pub mod loader;

// ===== Re-exports =====

pub use sink::{
    LogSink, LogSinkEmitFn, MetricsSink, MetricsSinkRecordFn, PILL_LOG_KIND_EVENT,
    PILL_LOG_KIND_SPAN_ENTER, PILL_LOG_KIND_SPAN_EXIT, PILL_LOG_KIND_SPAN_RECORD,
    PILL_LOG_LEVEL_DEBUG, PILL_LOG_LEVEL_ERROR, PILL_LOG_LEVEL_INFO, PILL_LOG_LEVEL_TRACE,
    PILL_LOG_LEVEL_WARN, PILL_METRIC_KIND_COUNTER, PILL_METRIC_KIND_GAUGE,
    PILL_METRIC_KIND_HISTOGRAM,
};
pub use types::{
    FrameReport, PillWindowHandleV1, RenderViewport, VirtualResolution, PILL_WINDOW_BACKEND_APPKIT,
    PILL_WINDOW_BACKEND_NONE, PILL_WINDOW_BACKEND_WAYLAND, PILL_WINDOW_BACKEND_WIN32,
    PILL_WINDOW_BACKEND_XCB, PILL_WINDOW_BACKEND_XLIB,
};
pub use version::{
    feature_mask_difference, features_are_compatible, host_feature_mask_names, PILL_ERR, PILL_OK,
    PILL_PROJECT_BACKEND_CSHARP, PILL_PROJECT_BACKEND_NATIVE, PILL_PROJECT_BACKEND_NONE,
    PILL_RUNTIME_ABI_VERSION, PILL_RUNTIME_FEATURE_DEV_LOGS, PILL_RUNTIME_FEATURE_METRICS,
    PILL_RUNTIME_FEATURE_PROFILING, PILL_RUNTIME_FEATURE_RENDERING,
    PILL_RUNTIME_STATE_FORMAT_VERSION,
};
pub use vtable::{
    validate_api_table, ApiValidationError, CapturedWorldState, PillCSharpProjectV1,
    PillRuntimeApiV1, PillRuntimeCreateArgsV1, RuntimeHandle, PILL_RUNTIME_API_SYMBOL,
};

/// The exact `raw-window-handle` release this contract translates, re-exported
/// so the host and the runtime cannot drift onto different versions.
#[cfg(feature = "window-handle")]
pub use raw_window_handle as rwh;
