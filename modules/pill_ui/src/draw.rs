//! What a panel wants drawn, as plain data - re-exported from `pill_draw`.
//!
//! # Responsibilities
//!
//! - Keeps the historical `pill_ui` paths for the draw vocabulary
//!   ([`DrawList`], [`UiVertex`], [`Color`], [`Rect`], [`TextureId`],
//!   [`DrawCommand`]).
//!
//! # Design
//!
//! The types moved to `pill_draw` (W-0032) so the renderer contract's
//! `RenderFrame.ui` can name them without depending on this crate; this
//! module re-exports them unchanged, and every existing path keeps
//! resolving.

pub use pill_draw::draw::*;
