//! The engine's wgpu renderer, and the only crate in the workspace that owns wgpu.
//!
//! # Responsibilities
//!
//! - Creates and drives the window surface, adapter, device and queue
//!   ([`Renderer`]).
//! - Owns the sprite render pipeline and its GPU buffers ([`SpriteRenderer`]).
//! - Declares the rendering failure type ([`RendererError`]).
//!
//! # Design
//!
//! This crate exists so that `wgpu` is reachable from the **host** and from
//! nowhere else. `pill_engine` is an rlib compiled into the host, into every
//! hot-loaded module and into every hot patch, so a single wgpu type reachable
//! from the engine puts the entire graphics stack - wgpu, naga, the `windows`
//! bindings - into all of them. Measured on a patch of one function, that
//! closure is ~215 ms of the compile: the linker pulls 278 archive members it
//! then discards, purely because generic instantiations resolve into them.
//!
//! The invariant to preserve: **`wgpu` appears in exactly one `Cargo.toml`
//! under `modules/`, this one**, and no source file outside this crate names a
//! `wgpu::` type.
//!
//! It is a plain `rlib` linked by `pill_host` under its `rendering` feature,
//! not a hot-loadable module: a renderer needs a live window handle, per-frame
//! `World` access and the frontend's event loop, none of which the one-shot
//! module ABI provides. It reads the world through the one public seam
//! `pill_engine` exposes for it,
//! [`World::sprite_instances`](pill_engine::world::World::sprite_instances).

// Current crate

/// Rendering initialization and presentation failures.
pub mod error;

/// Window surface, adapter, device and queue lifecycle.
pub mod renderer;

/// The sprite render pipeline and its GPU buffers.
pub mod sprite;

// The renderer's public surface, so callers name `pill_wgpu_renderer::Renderer`
// rather than reaching through the module that happens to declare it.
pub use error::RendererError;
pub use renderer::{Renderer, RendererWindow};
pub use sprite::SpriteRenderer;
