//! The engine's UI library: input, text, icons and a draw list any backend
//! can draw.
//!
//! # Responsibilities
//!
//! - Defines the plain-data contracts every UI user shares: [`input`] (what
//!   the platform delivered, as UI reads it), [`draw`] (what a caller wants
//!   drawn), and [`text`] with [`atlas`] and [`icons`] (fonts, glyphs and
//!   icons, rasterized on the CPU).
//! - Owns the process's one copy of the font database, the glyph and icon
//!   atlases and the rasterization caches (shared `dylib`, plan decision
//!   D17): the editor executable, the reloadable editor shell module, editor
//!   crates, the GPU module and projects all import this crate's DLL.
//! - Never links a window system or a GPU API: positions, colours, texture
//!   regions and atlas updates are plain values; a backend crate draws them.
//!
//! # Design
//!
//! Everything a caller stores or hands across a module boundary is owned
//! plain data: [`UiInput`] remembers positions and flags, [`DrawList`] holds
//! vertices, indices, clip rectangles and draw commands, and [`UiState`] can
//! live in a world resource. The state that cannot be plain data - the font
//! database, the swash cache, the atlas - lives in [`TextSystem`] and
//! [`IconCache`], which are shared per process and never keep a module's
//! pointers.

/// The shared texture atlas holding rasterized glyphs and icons.
pub mod atlas;
/// The draw list a backend consumes, and the value types it carries.
pub mod draw;
/// SVG icons, rasterized on the CPU into the atlas.
pub mod icons;
/// The engine's input events, kept the way UI reads them.
pub mod input;
/// The UI's persistent state, as plain data.
pub mod state;
/// Fonts, shaping, line breaking and glyph rasterization.
pub mod text;

pub use atlas::{Atlas, Region};
pub use draw::{Color, DrawCommand, DrawList, Rect, TextureId, UiVertex};
pub use icons::{IconCache, IconId};
pub use input::UiInput;
pub use state::UiState;
pub use text::{ShapedGlyph, ShapedText, TextStyle, TextSystem};
