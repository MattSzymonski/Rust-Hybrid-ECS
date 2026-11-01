//! Errors of the standalone frontend: its `winit` failures, and what a run
//! can end with.
//!
//! # Responsibilities
//!
//! - Declares [`FrontendError`], the `winit` event-loop and window failures.
//! - Declares [`RunError`], what a run can end with: a setup failure, a
//!   frontend failure, or a renderer failure.
//!
//! # Design
//!
//! Both types live in the crate that owns the event loop. `pill_core` is the
//! shared dylib every module, project and hot patch imports, so a `winit` type
//! there would put the windowing library in every one of their graphs for an
//! error only this binary constructs. `pill_player::RenderingError` composes
//! the windowed runtime's setup and renderer failures; [`RunError`] adds the
//! frontend arm, and a build without `rendering` has neither it nor the
//! renderer arm.

// Standard library
use std::convert::Infallible;

// External crates
use pill_core_macros::engine_error;

// =============================================================================
// Frontend Errors
// =============================================================================

/// Windowed-frontend failures produced by `winit`.
///
/// Raised while creating the event loop or the native standalone window.
#[cfg(feature = "rendering")]
#[engine_error(namespace = host::frontend, runtime = ::pill_core::error)]
pub enum FrontendError {
    /// The `winit` event loop could not be created.
    #[message("failed to create the event loop")]
    EventLoopCreation {
        #[source]
        source: winit::error::EventLoopError,
    },

    /// The native window could not be created.
    #[message("failed to create the standalone host window")]
    WindowCreation {
        #[source]
        source: winit::error::OsError,
    },
}

// =============================================================================
// Run Errors
// =============================================================================

/// Anything that can end a standalone run.
///
/// Transparent in every arm, so `?` carries the leaf error and its source
/// chain unchanged from wherever it was raised.
#[engine_error(namespace = standalone::run, runtime = ::pill_core::error)]
pub enum RunError {
    /// Setting up the host or runtime failed before any frame ran.
    #[transparent]
    Host(#[from] pill_core::error::HostError),

    /// The event loop or the window could not be created.
    #[cfg(feature = "rendering")]
    #[transparent]
    Frontend(#[from] FrontendError),

    /// The renderer failed: no GPU surface or device, a resource it needs
    /// missing, or a frame it could not draw.
    #[cfg(feature = "rendering")]
    #[transparent]
    Renderer(#[from] pill_player::RendererError),
}

#[cfg(feature = "rendering")]
impl From<pill_player::RenderingError> for RunError {
    /// The runtime's composition is this one without the frontend arm, so
    /// each of its arms maps onto the matching arm here.
    fn from(error: pill_player::RenderingError) -> Self {
        match error {
            pill_player::RenderingError::Host(error) => Self::Host(error),
            pill_player::RenderingError::Renderer(error) => Self::Renderer(error),
        }
    }
}

impl From<Infallible> for RunError {
    /// A headless driver's frame cannot fail, so this is what satisfies the
    /// `RunError: From<D::Error>` bound the headless loop takes.
    fn from(never: Infallible) -> Self {
        match never {}
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    // `code()` comes from the diagnostic trait `#[engine_error]` implements.
    use miette::Diagnostic as _;

    /// The transparent `Host` arm reports the code of the error it carries
    /// rather than one of its own.
    #[test]
    fn a_composed_arm_reports_the_leaf_code() {
        let host = RunError::from(pill_core::error::HostError::from(
            pill_core::error::ConfigError::EmptyModuleName,
        ));
        assert_eq!(
            host.code().map(|code| code.to_string()).as_deref(),
            Some("host::config::empty_module_name")
        );
    }
}
