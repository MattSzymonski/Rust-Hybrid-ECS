//! Shared game-module host for every engine frontend.
//!
//! # Responsibilities
//!
//! - Creates and owns the [`pill_engine::Engine`] instance.
//! - Builds and loads native or C# game modules.
//! - Watches game sources and coordinates safe hot reloads.
//! - Exposes [`setup`] and [`run_one_frame`] to embedding frontends.
//! - Owns the standalone headless or windowed application runner.
//!
//! # Design
//!
//! The crate has no `main` function, but it owns the complete standalone run loop.
//! With `rendering` enabled that includes the window, event loop, and engine renderer.
//! Embedding frontends such as `editor` can instead provide their own window and
//! event loop through [`setup_rendering`].
//! Configuration is externalized in [`GameModuleConfig`],
//! keeping backend selection out of executable crates.

mod build_runner;
mod config;
mod csharp;
mod game_module;
mod native_library;
mod runner;
mod runtime;
mod watcher;

pub use config::{CSharpModuleConfig, GameModuleBackend, GameModuleConfig};
pub use runner::run;
pub use runtime::{run_one_frame, setup, FrameReport, Host};

#[cfg(feature = "rendering")]
pub use pill_engine::{RenderViewport, RendererError, VirtualResolution};

#[cfg(feature = "rendering")]
pub use runtime::{setup_rendering, RenderingHost};
