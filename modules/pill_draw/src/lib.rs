//! The plain draw data the UI library and the renderer contract share.
//!
//! # Responsibilities
//!
//! - [`draw`]: the draw vocabulary a backend consumes - [`DrawList`] with
//!   vertices, indices, clips and commands, and the values they carry
//!   ([`UiVertex`], [`Color`], [`Rect`], [`TextureId`]).
//! - [`frame`]: a frame of UI output, [`UiFrame`] and [`UiDraw`] - the draw
//!   lists per surface plus the textures they sample.
//! - [`texture`]: the versioned texture set ([`UiTextureSet`], [`UiTexture`])
//!   a backend uploads by revision, so a rebuilt backend re-uploads instead of
//!   drawing nothing.
//! - [`atlas`]: the atlas update data ([`AtlasUpdate`], [`Region`]) a
//!   backend applies to its own texture.
//!
//! # Design
//!
//! These types cross artifact boundaries - a UI built in one DLL, a renderer
//! in another draws it (`pill_renderer_api::RenderFrame.ui`) - so they live
//! in their own shared crate: one type identity across every artifact that
//! builds or draws UI, without the renderer contract depending on the UI
//! library (W-0032). Everything here is owned plain data, and `pill_ui`
//! re-exports it under its historical paths.

/// The atlas update data a backend applies.
pub mod atlas;
/// The draw vocabulary a backend consumes.
pub mod draw;
/// A frame of UI output.
pub mod frame;
/// The versioned textures a frame carries.
pub mod texture;

pub use atlas::{AtlasRegionUpdate, AtlasUpdate, Region};
pub use draw::{Color, DrawCommand, DrawList, Rect, TextureId, UiVertex};
pub use frame::{UiDraw, UiFrame};
pub use texture::{UiTexture, UiTextureFilter, UiTextureSet, UiTextureUpload};
