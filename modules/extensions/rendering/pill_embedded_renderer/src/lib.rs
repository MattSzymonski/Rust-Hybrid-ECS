//! Software sprite renderer for display-controller targets, with no GPU.
//!
//! # Responsibilities
//!
//! - Defines the renderer's data contract ([`Position`], [`Color`], [`Sprite`],
//!   [`RenderViewport`], [`VirtualResolution`], [`SpriteInstance`]).
//! - Rasterizes one frame from an [`Engine`](pill_engine::engine::Engine) into
//!   an RGB565 framebuffer and hands over its bytes ([`SpriteRasterizer`],
//!   [`Framebuffer`], [`Renderer`]).
//!
//! # Design
//!
//! The same renderer contract as `pill_2d_renderer`, in software, for machines
//! that have a display but no usable GPU - a Raspberry Pi driving an SPI panel,
//! say. The crate ends at the framebuffer: it owns no display, no SPI bus and
//! no GPIO pin, and depends on no hardware crate.
//!
//! [`component`] duplicates `pill_2d_renderer`'s module rather than sharing a
//! dependency, so a display target never links wgpu to name `Sprite`, and the
//! two `#[repr(C)]` copies must not drift. The methods match the wgpu backend's;
//! construction takes dimensions rather than a window handle, and `render` is
//! infallible.

/// The renderer's data contract: sprite components, viewports, instance data.
pub mod component;

/// The RGB565 pixel buffer and its colour packing.
pub mod framebuffer;

/// The software rasterizer that turns sprite instances into pixels.
pub mod rasterizer;

/// Framebuffer ownership and the per-frame draw entry point.
pub mod renderer;

// The renderer's public surface, so callers name
// `pill_embedded_renderer::Sprite` rather than reaching through the module that
// happens to declare it. `Position` and `Color` are the engine's - every
// renderer and most projects want the same two, so they are defined once in
// `pill_engine` rather than copied into each pipeline - and re-exporting them
// here keeps `register_components` reading as one contract.
pub use component::{
    register_components, sprite_instances, RenderViewport, Sprite, SpriteInstance,
    VirtualResolution,
};
pub use framebuffer::{pack_rgb565, unpack_rgb565, Framebuffer, BYTES_PER_PIXEL};
pub use pill_engine::common_components::{Color, Position};
pub use rasterizer::SpriteRasterizer;
pub use renderer::Renderer;

// NOTE: there is no `RendererError` and no `RendererWindow` here, unlike in
// `pill_2d_renderer`.
//
// That `RendererError` wraps surface, adapter, device and frame-acquisition
// failures, most variants holding a `wgpu` type. Rasterizing into an owned
// buffer has no such step, so an enum here would be empty and every signature
// would carry a `Result` that is always `Ok`. Presentation is the one part that
// can fail, and it belongs to the caller's display driver, which reports
// failures in its own terms.
//
// `RendererWindow` abstracts a window handle for wgpu to bind a surface to.
// This backend has no surface and no window: it targets a panel described by
// its pixel dimensions.
