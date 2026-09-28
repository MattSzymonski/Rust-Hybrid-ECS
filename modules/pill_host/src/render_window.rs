//! Attaching a renderer to a frontend's window.
//!
//! # Responsibilities
//!
//! - Define [`RendererWindow`], the bound a frontend's window type meets: any
//!   window that hands out `raw-window-handle` 0.6 handles (winit, tao).
//! - Build a renderer on such a window from its handles as plain data
//!   ([`attach_window`]), and keep the window alive beside it.
//!
//! # Design
//!
//! The renderer never sees a window type, only [`RawWindowData`]: that keeps a
//! windowing crate out of the renderer, so the standalone host's winit window
//! and the editor's tao window attach the same way. The handles do not keep
//! the window alive, so the window is returned with the renderer as an
//! [`AttachedWindow`], and whoever stores the pair drops the renderer first.

// Standard library
use std::any::Any;

// External crates
use pill_master_renderer::{PillRenderer, Renderer, RendererError};
use pill_renderer_api::raw_window::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use pill_renderer_api::RawWindowData;

/// A window the renderer can be attached to.
///
/// Blanket-implemented for every type that hands out `raw-window-handle` 0.6
/// window and display handles - winit's and tao's windows, and an `Arc` of
/// either - so a frontend passes its own window type.
pub trait RendererWindow: HasWindowHandle + HasDisplayHandle + 'static {}
impl<T> RendererWindow for T where T: HasWindowHandle + HasDisplayHandle + 'static {}

/// A frontend window held for the lifetime of the renderer drawn on it.
///
/// Opaque on purpose: it exists only to be kept, and dropped after the
/// renderer it belongs to.
pub(crate) type AttachedWindow = Box<dyn Any>;

/// Build a renderer on `window`, returning the renderer and the window it must
/// not outlive.
///
/// The caller stores both and drops the renderer first - in a struct, by
/// declaring the renderer field before the window field.
///
/// # Errors
///
/// Returns a [`RendererError`] when the window cannot give its handles or
/// renderer creation fails.
pub(crate) fn attach_window<W: RendererWindow>(
    window: W,
    width: u32,
    height: u32,
) -> Result<(Box<dyn PillRenderer>, AttachedWindow), RendererError> {
    let window_data = RawWindowData::from_window(&window)?;
    // SAFETY: the window named by `window_data` is returned beside the
    // renderer as its `AttachedWindow`, which every caller drops after the
    // renderer, so the window outlives the surface built on its handles.
    let renderer = unsafe { Renderer::new(window_data, width, height) }?;
    Ok((Box::new(renderer), Box::new(window)))
}
