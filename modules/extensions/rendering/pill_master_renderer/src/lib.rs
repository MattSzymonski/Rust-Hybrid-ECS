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
//! - Declares [`GpuTexture`], a GPU texture the engine holds as a resource.
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
#[cfg(feature = "gpu")]
pub mod error;

/// Window surface, adapter, device and queue lifecycle.
#[cfg(feature = "gpu")]
pub mod renderer;

/// The sprite render pipeline and its GPU buffers.
#[cfg(feature = "gpu")]
pub mod sprite;

/// A GPU texture stored in the world as an engine resource.
#[cfg(feature = "gpu")]
pub mod texture;

// The renderer's public surface, so callers name `pill_master_renderer::Sprite`
// rather than reaching through the module that happens to declare it.
// `Position` and `Color` are the engine's - every renderer and most
// projects want the same two, so they are defined once in
// `pill_engine` rather than copied into each pipeline. Re-exported
// here so a caller that already names them through this crate keeps
// working, and so `register_components` reads as one contract.
pub use component::{
    register_components, sprite_instances, RenderViewport, Sprite, SpriteInstance,
    VirtualResolution,
};
pub use pill_engine::common_components::{Color, Position};

// The GPU half's public surface, absent when the `gpu` feature is off.
#[cfg(feature = "gpu")]
pub use error::RendererError;
#[cfg(feature = "gpu")]
pub use renderer::{Renderer, RendererWindow};
#[cfg(feature = "gpu")]
pub use sprite::SpriteRenderer;
#[cfg(feature = "gpu")]
pub use texture::GpuTexture;
