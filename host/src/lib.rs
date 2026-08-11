//! Shared game-module host for every engine frontend.
//!
//! # Responsibilities
//!
//! - Creates and owns the [`ecs_hybrid::Engine`] instance.
//! - Builds and loads native or C# game modules.
//! - Watches game sources and coordinates safe hot reloads.
//! - Exposes [`setup`] and [`run_one_frame`] to frontend event loops.
//!
//! # Design
//!
//! The crate has no `main` function and owns no window. Frontends such as
//! `standalone` and `editor` own their event loops while this crate owns the
//! common engine and game-module lifecycle. Configuration is externalized in
//! [`GameModuleConfig`], keeping backend selection out of those frontends.

mod build;
mod config;
mod csharp;
mod game_module;
mod native_library;
mod runtime;
mod watcher;

pub use config::{CSharpModuleConfig, GameModuleBackend, GameModuleConfig};
pub use runtime::{run_one_frame, setup, FrameReport, Host};
