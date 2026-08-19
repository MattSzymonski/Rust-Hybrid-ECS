//! Thin host loader for the hot-reloadable engine runtime.
//!
//! # Responsibilities
//!
//! - Build the engine runtime and the project module, and keep both current.
//! - Load the engine runtime dynamic library and drive it through its C ABI.
//! - Watch project, engine, and shared-core sources and run reload transactions.
//! - Route runtime logs and metrics into the host's telemetry pipeline.
//! - Own the standalone headless or windowed application runner.
//!
//! # Design
//!
//! The host never links the engine. Everything an engine rebuild invalidates -
//! the ECS, the renderer, the project module - lives in the `pill_runtime`
//! dynamic library, and the host reaches it only through the `#[repr(C)]`
//! function table declared by `pill_runtime_api`. That boundary is what makes
//! engine hot reload possible at all: the host binary, its window, its event
//! loop, and its telemetry survive a swap because nothing in them depends on
//! the engine's code or layout.
//!
//! [`EngineSession`] is the whole public surface for embedding frontends. It
//! owns the loaded generation, the build and watch subsystems, and both reload
//! transactions. Frontends supply a window through [`describe_window`], forward
//! resize events, and step frames; the editor additionally moves the surface
//! between windows.
//!
//! Configuration is externalized in [`ProjectModuleConfig`], keeping backend
//! selection out of executable crates.

// ===== Module Declarations =====

/// Build execution and output-path resolution for every reloadable module.
mod build_runner;
/// Project-module configuration shared by every host frontend.
mod config;
/// Host-side ownership of the engine runtime and its reload transactions.
mod engine_session;
/// Complete standalone application runner owned by the host crate.
mod runner;
/// Typed host-side access to one loaded engine runtime generation.
mod runtime_client;
/// Host-side receivers for telemetry produced inside the engine runtime.
mod sink;
/// Application telemetry bootstrap for every host frontend.
mod telemetry;
/// Source-tree watching and reload signalling for the main thread.
mod watcher;

// ===== Imports =====

// Standard library
use std::path::{Path, PathBuf};

// ===== Re-exports =====

// Local host modules and the shared crate-root error surface.
pub use config::{CSharpModuleConfig, ProjectModuleBackend, ProjectModuleConfig};
pub use engine_session::EngineSession;
#[cfg(feature = "rendering")]
pub use pill_core::error::FrontendError;
// The composed `HostError` is the single error type every frontend reports.
pub use pill_core::error::{
    engine_report, install_engine_report_handler, BuildError, CSharpError, ConfigError,
    EngineMessage, EngineReportHandler, HostError, LibraryError, MessageRenderer,
    PlainMessageRenderer, RuntimeError, SemanticRole, StyledDiagnosticProxy,
    TerminalMessageRenderer, WatcherError,
};
// Standalone runner and telemetry bootstrap.
pub use runner::run;
pub use telemetry::init_telemetry;

// Boundary types shared with the engine runtime. Frontends import them from
// here so they never depend on the contract crate directly.
pub use pill_runtime_api::{FrameReport, PillWindowHandleV1, RenderViewport, VirtualResolution};

// =============================================================================
// Constants
// =============================================================================

/// Feature bits this host binary was compiled with.
///
/// Passed to every `create` call so the runtime can reject a dylib built with
/// a different feature set. Features change which subsystems exist on either
/// side without changing any struct layout, so no `struct_size` guard would
/// catch the difference.
pub(crate) fn host_feature_mask() -> u32 {
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
}

// =============================================================================
// Free Functions
// =============================================================================

/// Resolve the workspace root every build and staging path is relative to.
///
/// # Errors
///
/// Returns [`HostError::WorkspaceRootUndetermined`] when the manifest
/// directory has no parent, which would mean the crate was moved out of its
/// workspace.
pub(crate) fn workspace_root() -> Result<PathBuf, HostError> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or(HostError::WorkspaceRootUndetermined)
        .map(Path::to_path_buf)
}

/// Describe a frontend's native window for the engine runtime.
///
/// Only the platform handles cross the boundary; the frontend keeps owning the
/// window. That is what lets one contract describe both the standalone
/// runner's `winit` window and the editor's `tao` windows, and it means no
/// reference count is transferred that a reload could leak or release twice.
///
/// # Errors
///
/// Returns [`RuntimeError::CallFailed`] when the platform handles cannot be
/// read, or when this contract revision cannot describe the platform.
///
/// # Safety note
///
/// The caller must keep the window alive for as long as any engine generation
/// renders into it. [`EngineSession`] is dropped before its frontend's window
/// in every runner in this workspace.
#[cfg(feature = "rendering")]
pub fn describe_window<W>(window: &W) -> Result<PillWindowHandleV1, HostError>
where
    W: pill_runtime_api::rwh::HasWindowHandle + pill_runtime_api::rwh::HasDisplayHandle,
{
    let window_handle = window
        .window_handle()
        .map_err(|source| RuntimeError::CallFailed {
            operation: String::from("describe_window"),
            details: format!("the frontend window has no usable handle: {source}"),
        })?;
    let display_handle = window
        .display_handle()
        .map_err(|source| RuntimeError::CallFailed {
            operation: String::from("describe_window"),
            details: format!("the frontend display has no usable handle: {source}"),
        })?;

    PillWindowHandleV1::from_raw_handles(window_handle.as_raw(), display_handle.as_raw())
        .ok_or_else(|| {
            HostError::from(RuntimeError::CallFailed {
                operation: String::from("describe_window"),
                details: String::from(
                    "this platform's window handles are not described by the runtime ABI",
                ),
            })
        })
}
