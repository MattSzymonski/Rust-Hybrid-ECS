//! A standalone wgpu sprite renderer: its components and their pipeline.
//!
//! # Responsibilities
//!
//! - Defines the renderer's data contract ([`Position`], [`Color`], [`Sprite`],
//!   [`RenderViewport`], [`VirtualResolution`], [`SpriteInstance`]).
//! - Creates and drives the window surface, adapter, device and queue
//!   ([`Renderer`]), owns the sprite pipeline and its buffers
//!   ([`SpriteRenderer`]), and declares the failure type ([`RendererError`]).
//!
//! # Design
//!
//! A sprite is a renderer concept, so its components live here with the code
//! that draws them, not in `pill_engine`.
//!
//! [`component`] names no `wgpu` type and reads the world through
//! [`World::archetypes_iter`](pill_engine::world::World::archetypes_iter) and
//! [`World::component_registry`](pill_engine::world::World::component_registry);
//! [`sprite`] holds the GPU pipeline and its own `bytemuck` upload record.
//!
//! It is a plain `rlib`, not a hot-loadable module: a renderer needs a live
//! window handle, per-frame `World` access and the frontend's event loop, which
//! the one-shot module ABI does not provide. The workspace excludes this
//! folder, so no workspace build compiles it.

/// The renderer's data contract: sprite components, viewports, instance data.
pub mod component;

/// Rendering initialization and presentation failures.
pub mod error;

/// Window surface, adapter, device and queue lifecycle.
pub mod renderer;

/// The sprite render pipeline and its GPU buffers.
pub mod sprite;

// The renderer's public surface, so callers name `pill_2d_renderer::Sprite`
// rather than reaching through the module that happens to declare it.
// `Position` and `Color` belong to the pipeline, not to the engine: a sprite's
// pixel placement and fill are this renderer's own contract, and re-exporting
// them here keeps `register_components` reading as one call.
pub use component::{
    register_components, sprite_instances, Color, Position, RenderViewport, Sprite, SpriteInstance,
    VirtualResolution,
};
pub use error::RendererError;
pub use renderer::{Renderer, RendererWindow};
pub use sprite::SpriteRenderer;
