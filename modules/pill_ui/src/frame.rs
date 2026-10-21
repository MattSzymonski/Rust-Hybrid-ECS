//! A frame of UI output - re-exported from `pill_draw`.
//!
//! # Responsibilities
//!
//! - Keeps the historical `pill_ui` paths for [`UiFrame`], [`UiDraw`] and the
//!   versioned texture set ([`UiTextureSet`], [`UiTexture`]) the frame
//!   carries.
//!
//! # Design
//!
//! The types moved to `pill_draw` (W-0032) so the renderer contract's
//! `RenderFrame.ui` can name them without depending on this crate; this
//! module re-exports them unchanged.

pub use pill_draw::frame::*;
pub use pill_draw::texture::*;
