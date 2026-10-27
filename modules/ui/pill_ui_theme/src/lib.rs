//! The UI theme pipeline's plain data.
//!
//! # Responsibilities
//!
//! - [`theme`]: the complete base [`Theme`], the neutral [`Theme::fallback`]
//!   and the [`Theme::brutalist`] recipe, plus the one pure merge.
//! - [`overrides`]: [`ThemeOverrides`], the sparse document a `.theme` file
//!   holds, and its JSON reader (bare body or standalone document).
//! - [`asset`]: the `.theme` standalone asset type.
//! - [`font`]: the `.ttf`/`.otf` font asset a layer names.
//! - [`register`](mod@register): registration, the game/editor path scopes and
//!   the handle-returning loaders.
//!
//! # Design
//!
//! This crate is a shared `dylib`: a project, `pill_project_ui`, the editor shell
//! and `pill_editor_core` share the `Theme`, `ThemeOverrides` and `FontAsset`
//! types, one asset column and one world resource. It is egui-free: the
//! translator lives in `pill_ui_kit`, compiled into each UI's link unit, so no
//! egui value crosses this boundary (two copies cannot share a link unit).
//!
//! A theme is always complete: a `.theme` file carries **only overrides**, and
//! merging never falls back to an egui default, so an absent item cannot change.

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
