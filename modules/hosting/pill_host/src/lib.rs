//! The development host every engine frontend runs a project through.
//!
//! # Responsibilities
//!
//! - Builds and loads native or C# project modules and the extensions.
//! - Watches project sources and coordinates safe hot reloads, around the
//!   frames the wrapped [`pill_runtime::Runtime`] runs.
//! - Exposes [`setup`], [`run_one_frame`] and, windowed, [`setup_rendering`] /
//!   [`attach_renderer`] to frontends, which own their event loops and drive
//!   the host through [`FrameDriver`].
//!
//! # Design
//!
//! Configuration is externalized in [`ProjectModuleConfig`], keeping backend
//! selection out of executable crates. Running the game - the engine, the
//! frame, the statically linked project, the renderer's window - is
//! `pill_runtime`'s, which a shipping build links without this crate; the
//! host exists only with `hot_reload`, and without it this crate is little
//! more than its configuration types.

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
/// C# development tooling over `pill_csharp_bridge`: mirror generation and the
/// in-process compiler, plus the bridge items the host names.
///
/// A shipped C# project needs none of it: it starts through the bridge's
/// `CSharpBackend`, which the runtime runs.
#[cfg(feature = "hot_reload")]
mod csharp;
/// Per-function hot patching: classify, generate, compile and activate a patch.
#[cfg(feature = "hot_patch")]
mod hot_patch;

/// One entry in a patched function's history, as returned by
/// [`DevHost::patch_generations`](runtime::DevHost::patch_generations).
#[cfg(feature = "hot_patch")]
pub use hot_patch::PatchGeneration;

/// Lifecycle management for extensions.
mod extension;
/// Native project-library loading and Windows-safe temporary-copy handling.
#[cfg(feature = "hot_reload")]
mod native_library;
/// Lifecycle management for the active native or managed project module.
mod project_module;
/// The sequence every reload runs once its replacement image is loaded.
#[cfg(feature = "hot_reload")]
mod reload;

/// The GPU renderer as a loaded module: starting it and driving its backend.
#[cfg(all(feature = "rendering", feature = "hot_reload"))]
mod renderer_module;
/// The development host: reloads around the runtime's frames.
#[cfg(feature = "hot_reload")]
mod runtime;
/// Source-tree watching and reload signalling for the main thread.
#[cfg(feature = "hot_reload")]
mod watcher;

/// The project's `res`, watched so edited assets are reimported in place.
#[cfg(feature = "hot_reload")]
mod asset_watcher;

/// The project's assets as a tool sees them: tree, settings, moves.
#[cfg(feature = "hot_reload")]
mod asset_browser;

#[cfg(all(feature = "rendering", feature = "hot_reload"))]
mod shader_watcher;

// ===== Re-exports =====

// Local host modules and the shared crate-root error surface.
pub use config::{ExtensionConfig, HostConfig, ProjectModuleBackend, ProjectModuleConfig};
pub use extension::EXTENSION_ABI_VERSION;
pub use pill_core::error::{
    engine_report, install_engine_report_handler, BuildError, CSharpError, ConfigError,
    EngineMessage, EngineReportHandler, HostError, LibraryError, MessageRenderer, ModuleError,
    PlainMessageRenderer, SemanticRole, StyledDiagnosticProxy, TerminalMessageRenderer,
    WatcherError,
};
/// Where a managed project's assemblies are, as [`ProjectModuleBackend`]
/// names them; defined by the C# bridge.
pub use pill_csharp_bridge::CSharpModuleConfig;
// The frame driver frontends run the host through, and what a frame reports.
pub use pill_runtime::{FrameDriver, FrameReport};

// The development host.
#[cfg(feature = "hot_reload")]
pub use asset_browser::{standalone_file_name, AssetEntry, StandaloneType};
#[cfg(feature = "hot_reload")]
pub use runtime::{run_one_frame, setup, DevHost};

// `EngineError` has no rendering variants, so it is available to headless
// frontends too.
pub use pill_engine::EngineError;

// Rendering-only: the renderer contract's data and errors, re-exported so
// frontends never name a renderer crate and stay free of a wgpu dependency of
// their own. The viewport describes where a renderer draws, so a headless build
// has nothing to point it at.
#[cfg(feature = "rendering")]
pub use pill_renderer_api::{RenderViewport, RendererError};
#[cfg(feature = "rendering")]
pub use pill_runtime::{RendererWindow, RenderingError};

// Rendering-only entry points: attaching the renderer to a frontend's window.
#[cfg(all(feature = "rendering", feature = "hot_reload"))]
pub use runtime::{attach_renderer, setup_rendering, RenderingHost};
