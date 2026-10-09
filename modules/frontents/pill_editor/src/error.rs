//! Editor error composition around the shared semantic error system.
//!
//! # Responsibilities
//!
//! - Compose every embedded host failure the editor can hit into
//!   [`EditorError`].
//! - The dock-layout persistence failures live in `pill_editor_core::error`
//!   with the layout model itself.
//!
//! # Design
//!
//! The editor owns no engine or GPU code; it delegates those failures to the
//! host crate's typed errors through a transparent wrapper, exactly like the
//! standalone frontend. The only editor-specific failure domains are moving
//! the engine surface between native windows and reading/writing the saved
//! dock layout. The Dioxus entry point converts the fatal startup error into
//! a styled miette report once; recoverable frame and persistence failures
//! are logged with their plain semantic messages and the UI continues.

// External crates
use pill_core_macros::engine_error;

// =============================================================================
// EditorError
// =============================================================================

/// Failures unique to the editor frontend on top of the embedded host.
#[engine_error(namespace = editor, runtime = ::pill_core::error)]
pub enum EditorError {
    /// Host setup or GPU surface creation failed while starting the editor.
    ///
    /// Wraps the full composed [`pill_host::RenderingError`], so the styled
    /// report keeps the entire cause chain from configuration down to the GPU.
    #[transparent]
    Host(#[from] pill_host::RenderingError),

    /// The engine surface could not be moved to another native window.
    #[message("failed to move the engine surface between windows")]
    Retarget {
        #[source]
        source: pill_host::RendererError,
    },

    /// A detached Scene window could not be given its native scene window
    /// (Linux only: elsewhere the scene shares the WebView's window).
    #[cfg(target_os = "linux")]
    #[message("failed to create a native window for the detached scene")]
    SceneWindow,

    /// One editor frame failed while presenting the rendered world.
    #[message("failed to present one editor frame")]
    Frame {
        #[source]
        source: pill_host::RendererError,
    },
}
