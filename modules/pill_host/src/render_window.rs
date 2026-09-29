//! Attaching a renderer to a frontend's window.
//!
//! # Responsibilities
//!
//! - Define [`RendererWindow`], the bound a frontend's window type meets: any
//!   window that hands out `raw-window-handle` 0.6 handles (winit, tao).
//! - Attach a renderer to such a window from its handles as plain data
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
use pill_renderer_api::raw_window::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use pill_renderer_api::{PillRenderer, RawWindowData, RendererError};

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

/// Attach a renderer to `window` through `attach`, returning the renderer, the
/// window it must not outlive, and the window's handles as data - which a
/// caller keeps to attach again later on the same window.
///
/// `attach` receives the window's handles as data and builds the renderer on
/// them. The caller stores both results and drops the renderer first - in a
/// struct, by declaring the renderer field before the window field - which is
/// what makes it sound for `attach` to build a surface on the handles.
///
/// # Errors
///
/// Returns a [`RendererError`] when the window cannot give its handles or
/// `attach` fails.
pub(crate) fn attach_window<W: RendererWindow>(
    window: W,
    attach: impl FnOnce(RawWindowData) -> Result<Box<dyn PillRenderer>, RendererError>,
) -> Result<(Box<dyn PillRenderer>, AttachedWindow, RawWindowData), RendererError> {
    let window_data = RawWindowData::from_window(&window)?;
    let renderer = attach(window_data)?;
    Ok((renderer, Box::new(window), window_data))
}
