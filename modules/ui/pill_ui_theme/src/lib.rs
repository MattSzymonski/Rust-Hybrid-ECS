//! The UI theme pipeline's plain data.
//!
//! # Responsibilities
//!
//! - [`theme`]: the complete base [`Theme`], the neutral [`Theme::fallback`]
//!   and the research document's [`Theme::brutalist`] recipe, plus the one pure
//!   merge.
//! - [`overrides`]: [`ThemeOverrides`], the sparse document a `.theme` file
//!   holds, and its JSON reader (bare body or standalone document).
//! - [`asset`]: the `.theme` standalone asset type.
//! - [`font`]: the `.ttf`/`.otf` font asset a layer names.
//! - [`register`](mod@register): registration, the game/editor path scopes and
//!   the handle-returning loaders.
//!
//! # Design
//!
//! This crate is a shared `dylib` (W-0127), like the other cross-artifact type
//! crates: a project, `pill_project_ui`, the reloadable editor shell and
//! `pill_editor_core` all name the same `Theme`, `ThemeOverrides` and
//! `FontAsset` types, so one asset column and one world resource serve them
//! all. It is egui-free on purpose: the translator from tokens to egui's
//! `Style` lives in `pill_ui_kit`, compiled into each UI's own link unit, so no
//! egui value ever crosses this crate's boundary (one link unit cannot hold two
//! copies of egui in different formats).
//!
//! A theme is always complete here. A `.theme` file carries **only overrides**
//! (the items it changes), and merging never falls back to an egui default, so
//! an absent item cannot change by accident.

/// The `.theme` standalone asset.
pub mod asset;
/// The `.ttf`/`.otf` font asset a theme layer names.
pub mod font;
/// The sparse override document and its JSON reader.
pub mod overrides;
/// Registration, path scopes and the loader.
pub mod register;
/// The complete theme tokens and the pure merge.
pub mod theme;

pub use asset::THEME_SHARED_NAME;
pub use font::{FontAsset, FontImportSettings, FONT_SHARED_NAME};
pub use overrides::ThemeOverrides;
pub use register::{load_font, load_overrides, register, LoadedFont, ThemePathScope};
pub use theme::{
    FontFamilyName, Palette, Rgba, Shadow, Shape, Spacing, Stroke, TextStyle, Theme, ThemeError,
    ThemeFontPaths, WidgetStates, WidgetStyle,
};
