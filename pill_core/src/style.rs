//! String styling helpers for terminal output.
//!
//! # Responsibilities
//!
//! - Own the [`PillStyle`] trait that maps semantic output roles (module
//!   objects, general objects, specific objects, names, and severity
//!   levels) to ANSI-styled strings for terminal rendering.
//! - Provide a native `&str` implementation that colors and bolds text
//!   through the `colored` crate.
//! - Provide a `wasm32` no-op implementation so the same styling vocabulary
//!   compiles on targets without terminal capabilities.
//!
//! # Design
//!
//! The [`PillStyle`] trait is implemented for `&str` behind two
//! `#[cfg(target_arch = ...)]` gates. The native build emits colored, bold
//! text; the `wasm32` build returns the plain string (wrapping names in
//! quotation marks) so callers share one styling API across targets. The
//! trait is re-exported at the crate root and used by [`telemetry`] for
//! semantic styling decisions.
//!
//! [`telemetry`]: crate::telemetry

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
    /// Changes the color and adds bold weight.
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
// PillStyle for &str (native, colored)
// =============================================================================

/// Applies ANSI styling through the `colored` crate on native targets.
///
/// Each method colors the string for its semantic role; module objects,
/// errors, warnings, and debug messages also gain bold weight.
#[cfg(not(target_arch = "wasm32"))]
impl PillStyle for &str {
    /// Style a large module object (Engine, Renderer, Window, and so on):
    /// changes the color and adds bold weight.
    #[inline]
    fn module_object_style(self) -> String {
        use colored::Colorize;
        self.color(colored::Color::TrueColor {
            r: 180,
            g: 25,
            b: 100,
        })
        .bold()
        .to_string()
    }

    /// Style a general object (Scene, Component, System, Resource, and so on):
    /// changes the color and adds bold weight.
    #[inline]
    fn general_object_style(self) -> String {
        use colored::Colorize;
        self.color(colored::Color::BrightCyan).to_string()
    }

    /// Style a specific object (CameraComponent, Texture, Mesh, and so on):
    /// changes the color.
    #[inline]
    fn specific_object_style(self) -> String {
        use colored::Colorize;
        self.color(colored::Color::TrueColor {
            r: 95,
            g: 210,
            b: 90,
        })
        .to_string()
    }

    /// Style a name: changes the color and adds quotation marks.
    #[inline]
    fn name_style(self) -> String {
        use colored::Colorize;
        format!("\"{}\"", self)
            .color(colored::Color::TrueColor {
                r: 190,
                g: 220,
                b: 160,
            })
            .to_string()
    }

    /// Style an error message: changes the color and adds bold weight.
    #[inline]
    fn error_style(self) -> String {
        use colored::Colorize;
        self.color(colored::Color::Red).bold().to_string()
    }

    /// Style a warning message: changes the color and adds bold weight.
    #[inline]
    fn warn_style(self) -> String {
        use colored::Colorize;
        self.color(colored::Color::Yellow).bold().to_string()
    }

    /// Style a debug message: changes the color and adds bold weight.
    #[inline]
    fn debug_style(self) -> String {
        use colored::Colorize;
        self.color(colored::Color::Blue).bold().to_string()
    }
}

// =============================================================================
// PillStyle for &str (wasm32, plain)
// =============================================================================

/// No-op styling for targets without terminal capabilities.
///
/// Returns the string unchanged so the styling vocabulary compiles on
/// `wasm32`; names still gain quotation marks.
#[cfg(target_arch = "wasm32")]
impl PillStyle for &str {
    /// Style a large module object: returned unchanged.
    #[inline]
    fn module_object_style(self) -> String {
        self.to_string()
    }

    /// Style a general object: returned unchanged.
    #[inline]
    fn general_object_style(self) -> String {
        self.to_string()
    }

    /// Style a specific object: returned unchanged.
    #[inline]
    fn specific_object_style(self) -> String {
        self.to_string()
    }

    /// Style a name: adds quotation marks.
    #[inline]
    fn name_style(self) -> String {
        format!("\"{}\"", self)
    }

    /// Style an error message: returned unchanged.
    #[inline]
    fn error_style(self) -> String {
        self.to_string()
    }

    /// Style a warning message: returned unchanged.
    #[inline]
    fn warn_style(self) -> String {
        self.to_string()
    }

    /// Style a debug message: returned unchanged.
    #[inline]
    fn debug_style(self) -> String {
        self.to_string()
    }
}
