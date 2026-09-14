//! The engine's renderer: the sprite components and the wgpu pipeline drawing them.
//!
//! # Responsibilities
//!
//! - Defines the renderer's data contract ([`Position`], [`Color`], [`Sprite`],
//!   [`RenderViewport`], [`VirtualResolution`], [`SpriteInstance`]).
//! - Creates and drives the window surface, adapter, device and queue
//!   ([`Renderer`]).
//! - Owns the sprite render pipeline and its GPU buffers ([`SpriteRenderer`]).
//! - Declares the rendering failure type ([`RendererError`]).
//!
//! # Design
//!
//! A sprite is a renderer concept, so the components that describe one live
//! here, with the code that draws them, rather than in `pill_engine`. The ECS
//! core defines storage and scheduling; it does not define what a quad is. A
//! project that wants sprites depends on this crate and calls
//! [`component::register_components`].
//!
//! The crate is split in two halves. [`component`] is pure data: it names no
//! `wgpu` type and reads the world through the two read-only seams `pill_engine`
//! exposes, [`World::archetypes_iter`](pill_engine::world::World::archetypes_iter)
//! and [`World::component_registry`](pill_engine::world::World::component_registry).
//! [`sprite`] and [`renderer`] are the GPU half. The split is what lets
//! `sprite.rs` keep its `bytemuck` upload record separate from the component
//! definitions while asserting at compile time that the two layouts agree.
//!
//! It is a plain `rlib` linked by `pill_host` under its `rendering` feature,
//! not a hot-loadable module: a renderer needs a live window handle, per-frame
//! `World` access and the frontend's event loop, none of which the one-shot
//! module ABI provides.
//!
//! # Cost note
//!
//! This crate carries `wgpu`, and a project links it to name `Sprite`. That
//! puts wgpu, naga and the `windows` bindings into every project `cdylib` and
//! every hot patch: measured on a patch of one function, the linker pulls 278
//! archive members it then discards, about 215 ms of the compile. That is the
//! accepted price of keeping renderer components out of the ECS core. If patch
//! latency ever matters more than this layering, the fix is to split
//! [`component`] into its own dependency-free crate that projects depend on
//! instead, leaving `wgpu` reachable only from the host.

// Current crate

/// The renderer's data contract: sprite components, viewports, instance data.
pub mod component;

/// Rendering initialization and presentation failures.
pub mod error;

/// Window surface, adapter, device and queue lifecycle.
pub mod renderer;

/// The sprite render pipeline and its GPU buffers.
pub mod sprite;

// The renderer's public surface, so callers name `pill_master_renderer::Sprite`
// rather than reaching through the module that happens to declare it.
pub use component::{
    register_components, sprite_instances, Color, Position, RenderViewport, Sprite, SpriteInstance,
    VirtualResolution,
};
pub use error::RendererError;
pub use renderer::{Renderer, RendererWindow};
pub use sprite::SpriteRenderer;
