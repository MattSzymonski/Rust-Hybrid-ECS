//! Terminal styling for both targets.
//!
//! # Responsibilities
//!
//! - Implement [`PillStyle`](crate::style::PillStyle) for `&str` on native
//!   targets (colored and bold through the `colored` crate) and on `wasm32`
//!   (plain text, names still quoted).
//!
//! # Design
//!
//! The two implementations are chosen at compile time, which is why they live
//! here beside the other target splits rather than next to the trait: the
//! platform layer is the one place a `cfg(target_arch)` gate exists, so a
//! reader looking for per-target behaviour looks here and nowhere else.

// =============================================================================
// PillStyle for &str (native, colored)
// =============================================================================

/// Applies ANSI styling through the `colored` crate on native targets.
///
/// Each method colors the string for its semantic role; module objects,
/// errors, warnings, and debug messages also gain bold weight.
#[cfg(not(target_arch = "wasm32"))]
impl crate::style::PillStyle for &str {
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
    /// changes the color.
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
impl crate::style::PillStyle for &str {
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
