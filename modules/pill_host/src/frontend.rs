//! Windowed-frontend errors owned by the host.
//!
//! # Responsibilities
//!
//! - Declares [`FrontendError`], the `winit` event-loop and window failures.
//! - Declares [`RenderingError`], the composition every windowed entry point
//!   returns: a host-setup failure, a frontend failure, or a renderer failure.
//!
//! # Design
//!
//! Both types live here, in the crate that actually owns the event loop and
//! the renderer, rather than further down the stack.
//!
//! `FrontendError` used to live in `pill_core` for one reason only: so that
//! `pill_engine::EngineError` could wrap it. But `pill_core` is the shared
//! dylib every module and every hot patch imports, so keeping `winit` in it
//! taxed all of them for a type only the windowed host ever constructs.
//!
//! `RenderingError` replaces that `EngineError` composition. The engine cannot
//! name a renderer error without depending on the renderer crate, which would
//! be a cycle, so the windowed boundary composes its three failure sources
//! here instead. `EngineError` is left with a layout that no longer depends on
//! whether rendering is enabled - which is what lets a module drop the
//! `rendering` feature mirror entirely.

// External crates
use pill_core_macros::engine_error;

// =============================================================================
// Frontend Errors
// =============================================================================

/// Windowed-frontend failures produced by `winit`.
///
/// Raised while creating the event loop or the native standalone window.
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
// Rendering Errors
// =============================================================================

/// Anything that can go wrong bringing up or running a windowed host.
///
/// Transparent in every arm, so `?` carries the leaf error and its source
/// chain unchanged from wherever it was raised.
#[engine_error(namespace = host::rendering, runtime = ::pill_core::error)]
pub enum RenderingError {
    /// Host setup failed before any window existed.
    #[transparent]
    Host(#[from] pill_core::error::HostError),

    /// The event loop or the window could not be created.
    #[transparent]
    Frontend(#[from] FrontendError),

    /// The GPU surface, device, or a frame could not be obtained.
    #[transparent]
    Renderer(#[from] pill_wgpu_renderer::RendererError),
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
    /// error it carries rather than one of its own. That is what keeps a
    /// diagnostic identical whether it surfaced through this boundary or was
    /// raised directly.
    ///
    /// `FrontendError`'s own variants are not exercised here because both wrap
    /// a `winit` error that cannot be constructed outside `winit`; its
    /// namespace is pinned by the `host::frontend` attribute above, unchanged
    /// from when the enum lived in `pill_core`.
    #[test]
    fn every_composed_arm_reports_the_leaf_code() {
        let renderer = RenderingError::Renderer(pill_wgpu_renderer::RendererError::NoAlphaModes);
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
