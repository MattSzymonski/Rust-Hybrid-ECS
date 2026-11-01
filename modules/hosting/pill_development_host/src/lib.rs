//! The development host: it builds a project and its extensions as loadable
//! modules and reloads them while the project runs.
//!
//! # Responsibilities
//!
//! - Builds and loads native or C# project modules and the extensions.
//! - Watches project sources and coordinates safe hot reloads, around the
//!   frames the wrapped [`pill_player::Runtime`] runs.
//! - Exposes [`setup`], [`run_one_frame`] and, windowed, [`setup_rendering`] /
//!   [`attach_renderer`] to frontends, which own their event loops and drive
//!   the host through [`FrameDriver`].
//!
//! # Design
//!
//! Configuration is externalized in [`ProjectModuleConfig`], keeping backend
//! selection out of executable crates. Running the game - the engine, the
//! frame, the statically linked project, the renderer's window - is
//! `pill_player`'s, which a shipping build links without this crate. The
//! reload path compiles only with `hot_reload`; without it this crate is
//! little more than its configuration types.

// ===== Module Declarations =====

/// Build, link, and hot-reload analytics collector and console reports.
///
/// Every measurement it collects is about building, loading or reloading, so a
/// statically linked build has nothing to report.
#[cfg(feature = "hot_reload")]
mod analytics;
/// The startup build plan: the modules about to be built, announced up front
/// and counted off as each build starts.
#[cfg(feature = "hot_reload")]
mod build_progress;
/// Project-module build execution and output-path resolution.
#[cfg(feature = "hot_reload")]
mod build_runner;
/// Project-module configuration shared by every host frontend.
mod config;
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

/// Lifecycle management for extensions.
mod extension;
/// Native project-library loading and Windows-safe temporary-copy handling.
#[cfg(feature = "hot_reload")]
mod native_library;
/// Reloads, patches, failures and rollbacks, queued for an attached editor.
#[cfg(feature = "hot_reload")]
mod notices;
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

/// Re-cooks edited renderer shaders on a worker thread and queues the results.
#[cfg(all(feature = "rendering", feature = "hot_reload"))]
mod shader_watcher;

/// Test-only scratch directories and explicit file dates.
#[cfg(test)]
mod scratch;

// ===== Re-exports =====

/// The editor-only settings file's name in the project root.
pub use config::EDITOR_SETTINGS_FILE;
/// The editor's own settings file: read by the editor executable
/// only, never by a game build.
pub use config::{read_editor_settings, resolve_editor_theme, EditorSettings};
/// The host's configuration: the project module, the extensions to load and
/// the selected renderer.
pub use config::{ExtensionConfig, HostConfig, ProjectModuleBackend, ProjectModuleConfig};
/// The module ABI revision an extension must report before the host calls
/// into it.
pub use extension::EXTENSION_ABI_VERSION;
/// The notices an editor frontend drains each frame; see [`HostNotice`].
#[cfg(feature = "hot_reload")]
pub use notices::{take as take_notices, HostNotice};
/// The error surface frontends report through: the host's error types, the
/// message renderers and the installed report handler.
pub use pill_core::error::{
    engine_report, install_engine_report_handler, BuildError, CSharpError, ConfigError,
    EngineMessage, EngineReportHandler, HostError, LibraryError, MessageRenderer, ModuleError,
    PlainMessageRenderer, SemanticRole, StyledDiagnosticProxy, TerminalMessageRenderer,
    WatcherError,
};
/// Where a managed project's assemblies are, as [`ProjectModuleBackend`]
/// names them; defined by the C# bridge.
pub use pill_csharp_bridge::CSharpModuleConfig;
/// What a frontend's loop needs from whatever runs the game; the development
/// host implements it, as the shipping runtimes do.
pub use pill_player::FrameDriver;

/// The project's assets as an editor tool sees them, and the standalone types
/// a new asset can be created for.
#[cfg(feature = "hot_reload")]
pub use asset_browser::{standalone_file_name, standalone_types, AssetEntry, StandaloneType};
/// One entry in a patched function's history, as returned by
/// [`DevHost::patch_generations`](runtime::DevHost::patch_generations).
#[cfg(feature = "hot_patch")]
pub use hot_patch::PatchGeneration;
/// The development host and the headless entry points that drive one.
#[cfg(feature = "hot_reload")]
pub use runtime::{run_one_frame, setup, DevHost};

/// `EngineError` has no rendering variants, so it is available to headless
/// frontends too.
pub use pill_engine::EngineError;

// Rendering-only: the renderer contract's data and errors, re-exported so
// frontends never name a renderer crate and stay free of a wgpu dependency of
// their own. The viewport describes where a renderer draws, so a headless build
// has nothing to point it at.
#[cfg(feature = "rendering")]
pub use pill_player::{RendererWindow, RenderingError};
#[cfg(feature = "rendering")]
pub use pill_renderer_api::{RenderViewport, RendererError};

/// Rendering-only entry points: the windowed host and attaching the renderer
/// to a frontend's window.
#[cfg(all(feature = "rendering", feature = "hot_reload"))]
pub use runtime::{attach_renderer, setup_rendering, RenderingHost};
