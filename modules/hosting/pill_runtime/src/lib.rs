//! Runs a game: the engine, its frames, and the statically linked project.
//!
//! # Responsibilities
//!
//! - Owns the [`pill_engine::Engine`] and runs frames ([`Runtime`]).
//! - Defines the static-link contract ([`StaticProject`], [`StaticModule`],
//!   [`StaticRenderer`], [`StaticProjectBackend`], [`ProjectBackend`]) and
//!   registers a linked project in the engine's order and owners.
//! - Names the owners every registering subject gets and runs an entry point
//!   under its scope ([`registration`]), for this crate and the development
//!   host alike.
//! - With `rendering`: attaches a renderer to a window given as handles and
//!   draws frames ([`AttachedRenderer`], [`RenderingRuntime`]).
//! - Installs the telemetry stack ([`init_telemetry`]) and defines
//!   [`FrameDriver`], what a frontend's loop needs.
//!
//! # Design
//!
//! Linked into every executable: the development host (`pill_host`) builds on
//! it, a shipping build and a web build use it alone. So it carries nothing a
//! shipped game does not do - no DLL loading, file watching, build tools, .NET
//! hosting, winit or YAML. A C# project reaches it as an external
//! [`ProjectBackend`] that `pill_csharp_bridge` implements.

// ===== Module Declarations =====

/// Errors of a windowed runtime.
#[cfg(feature = "rendering")]
mod error;
/// How modules, the project and the renderer register with the engine.
pub mod registration;
/// Renderer assets kept beside the engine.
#[cfg(feature = "rendering")]
mod render_assets;
/// Attaching a renderer to a frontend's window, through its handles as data.
#[cfg(feature = "rendering")]
mod render_window;
/// A renderer attached to a window, and the windowed shipping runtime.
#[cfg(feature = "rendering")]
mod rendering;
/// Engine ownership and the frame every build runs.
mod runtime;
/// Statically linked project and module registration.
mod static_project;

/// The start-up report of the machine the engine runs on.
mod system_specs;
/// Application telemetry bootstrap for every frontend.
mod telemetry;

// ===== Re-exports =====

/// The renderer contract's types that appear in this crate's API
/// ([`FrameDriver`], the windowed runtime), re-exported for frontends.
pub use pill_renderer_api::{RenderViewport, RendererError};
pub use runtime::{run_one_frame, setup, FrameDriver, FrameReport, Runtime};
pub use static_project::{
    ProjectBackend, StaticLogging, StaticModule, StaticProject, StaticProjectBackend,
    StaticRenderer, StaticRendererAttachFn,
};
pub use telemetry::{apply_logging_settings, init_telemetry, LoggingSettings};

#[cfg(feature = "rendering")]
pub use error::RenderingError;
#[cfg(feature = "rendering")]
pub use render_assets::NativeAssets;
#[cfg(feature = "rendering")]
pub use render_window::RendererWindow;
#[cfg(feature = "rendering")]
pub use rendering::{attach_renderer, setup_rendering, AttachedRenderer, RenderingRuntime};
