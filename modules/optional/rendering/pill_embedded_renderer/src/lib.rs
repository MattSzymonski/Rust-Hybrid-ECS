//! Software sprite renderer for display-controller targets, with no GPU.
//!
//! # Responsibilities
//!
//! - Defines the renderer's data contract ([`Position`], [`Color`], [`Sprite`],
//!   [`RenderViewport`], [`VirtualResolution`], [`SpriteInstance`]).
//! - Rasterizes sprites into an RGB565 framebuffer on the CPU
//!   ([`SpriteRasterizer`], [`Framebuffer`]).
//! - Draws one frame from an [`Engine`](pill_engine::engine::Engine) and hands
//!   it over as bytes ([`Renderer`]).
//!
//! # Design
//!
//! The same renderer contract as `pill_wgpu_renderer`, implemented in software
//! for machines that have a display but no usable GPU - a Raspberry Pi driving
//! an SPI panel being the case this was written for. A project depends on this
//! crate, calls [`register_components`], and the sprites it spawns are drawn by
//! a rasterizer instead of a shader.
//!
//! The crate ends at the framebuffer. It owns no display, no SPI bus and no
//! GPIO pin, and depends on no hardware crate: [`Renderer::render`] fills a
//! buffer and [`Renderer::frame_rgb565`] hands it over, and the caller writes
//! those bytes wherever they belong.
//!
//! ```ignore
//! let mut renderer = Renderer::new(240, 280);
//! renderer.set_virtual_resolution(Some(VirtualResolution::new(800.0, 600.0)));
//!
//! loop {
//!     run_one_frame(&mut host);
//!     renderer.render(host.engine_mut());
//!     display.draw(renderer.frame_rgb565())?;
//! }
//! ```
//!
//! Keeping the driver out is what makes the rasterizer testable: every module
//! here builds and runs its unit tests on an ordinary desktop, with no panel
//! attached. It also means one renderer serves any controller taking RGB565,
//! not just the ST7789 this was validated against.
//!
//! # Relationship to the other renderers
//!
//! [`component`] is a deliberate duplicate of the wgpu backend's module rather
//! than a shared dependency, so a project targeting a display never links wgpu
//! to name `Sprite`. The two copies are a shared ABI: both are `#[repr(C)]`,
//! both are resolved by stable type name and verified size, and the layouts
//! must not drift.
//!
//! The renderer surface matches the wgpu backend where it can - `resize`,
//! `set_viewport`, `set_virtual_resolution`, `render` - so a frontend can drive
//! either. Two differences are inherent: construction takes pixel dimensions
//! instead of a window handle, and `render` is infallible, because there is no
//! device to lose and no swapchain image to acquire.

// Current crate

/// The renderer's data contract: sprite components, viewports, instance data.
pub mod component;

/// The RGB565 pixel buffer and its colour packing.
pub mod framebuffer;

/// The software rasterizer that turns sprite instances into pixels.
pub mod rasterizer;

/// Framebuffer ownership and the per-frame draw entry point.
pub mod renderer;

// The renderer's public surface, so callers name `pill_embedded_renderer::Sprite`
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
pub use framebuffer::{pack_rgb565, unpack_rgb565, Framebuffer, BYTES_PER_PIXEL};
pub use pill_engine::common_components::{Color, Position};
pub use rasterizer::SpriteRasterizer;
pub use renderer::Renderer;

// NOTE: there is no `RendererError` and no `RendererWindow` here, unlike in
// `pill_wgpu_renderer`.
//
// `RendererError` existed to wrap surface, adapter, device and frame
// acquisition failures - every variant held a `wgpu` type. Rasterizing into an
// owned buffer has no such step, so the enum would be empty and every
// signature would carry a `Result` that is always `Ok`. Presentation is the
// one part that can fail, and it belongs to the caller's display driver, which
// reports failures in its own terms.
//
// `RendererWindow` abstracted a window handle for wgpu to bind a surface to.
// This backend has no surface and no window: it targets a panel described by
// its pixel dimensions.
