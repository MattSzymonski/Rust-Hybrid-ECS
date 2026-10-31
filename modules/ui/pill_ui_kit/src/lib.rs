//! The UI kit: the token-to-egui translator and the scoped style variants.
//!
//! # Responsibilities
//!
//! - [`style`]: [`build_style`] (tokens to `egui::Style`), [`font_definitions`]
//!   and [`install`] (fonts, then the style, then the dark theme pinned), and
//!   the caller-held [`FontSignature`].
//! - [`variants`]: `primary`, `secondary` and `danger` scoped looks for stock
//!   widgets.
//! - [`widgets`]: the one look stock egui cannot draw - [`ChunkyButton`], a
//!   face with a hard offset shadow.
//!
//! # Design
//!
//! An `rlib`, embedded into both UI dylibs (`pill_editor_core` and
//! `pill_project_ui`), so the translator runs inside the link unit that owns
//! the egui `Context` it writes to and no egui value crosses a DLL boundary.
//! Only those two crates may depend on it; each re-exports it as `kit`. Its
//! dependencies stay exactly `egui` and `pill_ui_theme`: a widget that needs
//! more belongs in a separate crate, not in this one.

/// The translator and the font installer.
pub mod style;
/// The scoped stock-widget variants.
pub mod variants;
/// The themed widgets stock egui has no equivalent for.
pub mod widgets;

pub use style::{
    build_style, font_definitions, install, FontSignature, ThemeFonts, DISPLAY_FAMILY,
};
pub use widgets::ChunkyButton;
pub use widgets::SHADOW_OFFSET;
