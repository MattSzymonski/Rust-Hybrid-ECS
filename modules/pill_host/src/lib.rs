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
///
/// Every measurement it collects is about building, loading or reloading, so a
/// statically linked build has nothing to report.
#[cfg(feature = "hot_reload")]
mod analytics;
/// Project-module build execution and output-path resolution.
#[cfg(feature = "hot_reload")]
mod build_runner;
/// Project-module configuration shared by every host frontend.
mod config;
/// ANSI console helpers for the hot-reload log (colors, VT enabling).
#[cfg(feature = "hot_reload")]
mod console;
/// Scheduler-aware C# backend for the native project host.
///
/// Compiled in both postures. A managed assembly is loaded by the .NET runtime
/// either way - there is no static equivalent - so a shipping build differs
/// only in what it does not do: no `dotnet build`, no generated C# mirrors, and
/// no watching the assembly for replacement.
mod csharp;
/// Per-function hot patching: classify, generate, compile and activate a patch.
#[cfg(feature = "hot_patch")]
mod hot_patch;

/// One entry in a patched function's history, as returned by
/// [`Host::patch_generations`](runtime::Host::patch_generations).
#[cfg(feature = "hot_patch")]
pub use hot_patch::PatchGeneration;

/// Native project-library loading and Windows-safe temporary-copy handling.
#[cfg(feature = "hot_reload")]
mod native_library;
/// Lifecycle management for optional engine modules.
mod optional_module;
/// Lifecycle management for the active native or managed project module.
mod project_module;
/// The sequence every reload runs once its replacement image is loaded.
#[cfg(feature = "hot_reload")]
mod reload;
/// Complete standalone application runner owned by the host crate.
mod runner;
/// Engine ownership and frontend-facing frame orchestration.
mod runtime;
/// Application telemetry bootstrap for every host frontend.
mod telemetry;
/// Source-tree watching and reload signalling for the main thread.
#[cfg(feature = "hot_reload")]
mod watcher;

/// Statically linked project and module registration, for shipping builds.
#[cfg(not(feature = "hot_reload"))]
mod static_link;

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
/// The project and modules a shipping build links in, in place of a
/// [`HostConfig`]: with `hot_reload` off nothing is built, watched or loaded.
#[cfg(not(feature = "hot_reload"))]
pub use static_link::{StaticModule, StaticProject, StaticProjectBackend};
// Standalone runner, frame orchestration, and telemetry bootstrap.
pub use runner::run;
pub use runtime::{run_one_frame, setup, FrameReport, Host, ProjectSource};
pub use telemetry::init_telemetry;

// Rendering-only engine surface: error and viewport types.
#[cfg(feature = "rendering")]
pub use pill_engine::{EngineError, RenderViewport, RendererError, VirtualResolution};

// Rendering-only frontend entry points: window and event-loop setup.
#[cfg(feature = "rendering")]
pub use runtime::{attach_renderer, setup_rendering, RenderingHost};
