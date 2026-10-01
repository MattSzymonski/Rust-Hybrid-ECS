//! Errors of a windowed runtime.
//!
//! # Responsibilities
//!
//! - Declares [`RenderingError`], what bringing up or running a windowed
//!   runtime fails with: a setup failure or a renderer failure.
//!
//! # Design
//!
//! The engine cannot name a renderer error without depending on the renderer
//! contract, so the windowed boundary composes its failure sources here.
//! Window and event-loop failures belong to the frontend that owns the event
//! loop, which composes this type with its own.

// External crates
use pill_core_macros::engine_error;

// =============================================================================
// Rendering Errors
// =============================================================================

/// Anything that can go wrong bringing up or running a windowed runtime.
///
/// Transparent in every arm, so `?` carries the leaf error and its source
/// chain unchanged from wherever it was raised.
#[engine_error(namespace = runtime::rendering, runtime = ::pill_core::error)]
pub enum RenderingError {
    /// Setup failed before any window existed.
    #[transparent]
    Host(#[from] pill_core::error::HostError),

    /// The GPU surface, device, or a frame could not be obtained.
    #[transparent]
    Renderer(#[from] pill_renderer_api::RendererError),
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    // `code()` comes from the diagnostic trait `#[engine_error]` implements.
    use miette::Diagnostic as _;

    /// Every arm is transparent, so a composed error reports the code of the
    /// error it carries rather than one of its own.
    #[test]
    fn every_composed_arm_reports_the_leaf_code() {
        let renderer = RenderingError::Renderer(pill_renderer_api::RendererError::NoAlphaModes);
        assert_eq!(
            renderer.code().map(|code| code.to_string()).as_deref(),
            Some("engine::renderer::no_alpha_modes")
        );

        let host = RenderingError::from(pill_core::error::HostError::from(
            pill_core::error::ConfigError::EmptyModuleName,
        ));
        assert_eq!(
            host.code().map(|code| code.to_string()).as_deref(),
            Some("host::config::empty_module_name")
        );
    }
}
