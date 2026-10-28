//! String styling helpers for terminal output.
//!
//! # Responsibilities
//!
//! - Own the [`PillStyle`] trait that maps semantic output roles (module
//!   objects, general objects, specific objects, names, and severity
//!   levels) to ANSI-styled strings for terminal rendering.
//!
//! The `&str` implementations live in the platform layer
//! ([`crate::platform::styling`]), one per target: colored through the
//! `colored` crate on native, plain text (names still quoted) on `wasm32`.
//!
//! # Design
//!
//! The trait states the vocabulary; the platform layer picks the target's
//! implementation. The trait is re-exported at the crate root and is what
//! [`error`]'s terminal renderer styles through.
//!
//! [`error`]: crate::error

// =============================================================================
// PillStyle trait
// =============================================================================

/// Maps semantic output roles to ANSI-styled strings for terminal rendering.
///
/// Implementors decide how each role — module objects, general objects,
/// specific objects, names, and severity levels — is displayed. The crate
/// provides an implementation for `&str` that colors and bolds text on
/// native targets and returns plain text on `wasm32`.
///
/// # Examples
///
/// ```
/// use pill_core::PillStyle;
///
/// let styled = "Engine".module_object_style();
/// assert!(!styled.is_empty());
/// ```
pub trait PillStyle {
    /// Style a large module object (Engine, Renderer, Window, and so on).
    ///
    /// Changes the color and adds bold weight.
    fn module_object_style(self) -> String;

    /// Style a general object (Scene, Component, System, Resource, and so on).
    ///
    /// Changes the color.
    fn general_object_style(self) -> String;

    /// Style a specific object (CameraComponent, Texture, Mesh, and so on).
    ///
    /// Changes the color.
    fn specific_object_style(self) -> String;

    /// Style a name, changing the color and adding quotation marks.
    fn name_style(self) -> String;

    /// Style an error message, changing the color and adding bold weight.
    fn error_style(self) -> String;

    /// Style a warning message, changing the color and adding bold weight.
    fn warn_style(self) -> String;

    /// Style a debug message, changing the color and adding bold weight.
    fn debug_style(self) -> String;
}

// =============================================================================
// PillStyle for &str
// =============================================================================

// The `&str` implementations live in `platform::styling`, beside the other
// target splits; this module owns only the trait.
