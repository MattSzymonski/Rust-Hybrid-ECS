//! Shared project-module host for every engine frontend.
//!
//! # Responsibilities
//!
//! - Creates and owns the [`pill_engine::Engine`] instance.
//! - Builds and loads native or C# project modules.
//! - Watches project sources and coordinates safe hot reloads.
//! - Exposes [`setup`] and [`run_one_frame`] to embedding frontends.
//! - Owns the standalone headless or windowed application runner.
//!
//! # Design
//!
//! The crate has no `main` function, but it owns the complete standalone run loop.
//! With `rendering` enabled that includes the window, event loop, and engine renderer.
//! Embedding frontends such as `editor` can instead provide their own window and
//! event loop through [`setup_rendering`].
//! Configuration is externalized in [`ProjectModuleConfig`],
//! keeping backend selection out of executable crates.

// ===== Module Declarations =====

/// Build, link, and hot-reload analytics collector and console reports.
mod analytics;
/// Project-module build execution and output-path resolution.
mod build_runner;
/// Project-module configuration shared by every host frontend.
mod config;
/// ANSI console helpers for the hot-reload log (colors, VT enabling).
mod console;
/// Scheduler-aware C# backend for the native project host.
mod csharp;
/// Native project-library loading and Windows-safe temporary-copy handling.
mod native_library;
/// Lifecycle management for optional engine modules.
mod optional_module;
/// Lifecycle management for the active native or managed project module.
mod project_module;
/// Complete standalone application runner owned by the host crate.
mod runner;
/// Engine ownership and frontend-facing frame orchestration.
mod runtime;
/// Application telemetry bootstrap for every host frontend.
mod telemetry;
/// Source-tree watching and reload signalling for the main thread.
mod watcher;

// ===== Re-exports =====

// Local host modules and the shared crate-root error surface.
pub use config::{
    CSharpModuleConfig, HostConfig, OptionalModuleConfig, ProjectModuleBackend, ProjectModuleConfig,
};
pub use optional_module::OPTIONAL_MODULE_ABI_VERSION;
#[cfg(feature = "rendering")]
pub use pill_core::error::FrontendError;
pub use pill_core::error::{
    engine_report, install_engine_report_handler, BuildError, CSharpError, ConfigError,
    EngineMessage, EngineReportHandler, HostError, LibraryError, MessageRenderer, ModuleError,
    PlainMessageRenderer, SemanticRole, StyledDiagnosticProxy, TerminalMessageRenderer,
    WatcherError,
};
// Standalone runner, frame orchestration, and telemetry bootstrap.
pub use runner::run;
pub use runtime::{run_one_frame, setup, FrameReport, Host};
pub use telemetry::init_telemetry;

// Rendering-only engine surface: error and viewport types.
#[cfg(feature = "rendering")]
pub use pill_engine::{EngineError, RenderViewport, RendererError, VirtualResolution};

// Rendering-only frontend entry points: window and event-loop setup.
#[cfg(feature = "rendering")]
pub use runtime::{attach_renderer, setup_rendering, RenderingHost};
