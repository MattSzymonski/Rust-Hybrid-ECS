//! Editor error composition around the shared semantic error system.
//!
//! # Responsibilities
//!
//! - Compose every embedded host failure the editor can hit into
//!   [`EditorError`].
//! - Declare the dock-layout persistence failures that only the editor
//!   produces ([`LayoutPersistenceError`]).
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

// Standard library
use std::io;

// External crates
use pill_core_macros::engine_error;

// Current crate
use crate::layout::LayoutError;

// =============================================================================
// EditorError
// =============================================================================

/// Failures unique to the editor frontend on top of the embedded host.
#[engine_error(namespace = editor, runtime = ::pill_core::error)]
pub enum EditorError {
    /// Host setup or GPU surface creation failed while starting the editor.
    ///
    /// Wraps the full composed [`pill_host::EngineError`], so the styled report
    /// keeps the entire cause chain from configuration down to the GPU.
    #[transparent]
    Host(#[from] pill_host::EngineError),

    /// The engine surface could not be moved to another native window.
    #[message("failed to move the engine surface between windows")]
    Retarget {
        #[source]
        source: pill_host::RendererError,
    },

    /// One editor frame failed while presenting the rendered world.
    #[message("failed to present one editor frame")]
    Frame {
        #[source]
        source: pill_host::RendererError,
    },
}

// =============================================================================
// LayoutPersistenceError
// =============================================================================

/// Failures while loading or saving the persisted dock layout.
///
/// Loading failures are recovered by falling back to the default layout;
/// saving failures leave the previously installed document untouched.
#[engine_error(namespace = editor::layout, runtime = ::pill_core::error)]
pub enum LayoutPersistenceError {
    /// The saved document is not valid JSON for the current layout schema.
    #[message("failed to read the saved dock layout from ", name_style(path))]
    Invalid {
        path: String,
        #[source]
        source: serde_json::Error,
    },

    /// The loaded model violates the editor's layout invariants.
    #[message("the saved dock layout is invalid")]
    Validation {
        #[source]
        source: LayoutError,
    },

    /// The model could not be serialized before writing.
    #[message("failed to serialize the dock layout")]
    Serialization {
        #[source]
        source: serde_json::Error,
    },

    /// A filesystem operation failed while installing the layout document.
    #[message("failed to write the dock layout to ", name_style(path))]
    Filesystem {
        path: String,
        #[source]
        source: io::Error,
    },
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use miette::Diagnostic;
    use pill_core::error::EngineMessage;
    use std::error::Error;

    /// Plain rendering keeps dynamic paths verbatim with no semantic styling.
    #[test]
    fn layout_persistence_messages_render_plain_paths() {
        let error = LayoutPersistenceError::Filesystem {
            path: String::from("C:\\tmp\\editor_layout.json"),
            source: io::Error::other("disk full"),
        };
        assert_eq!(
            error.to_plain_message(),
            "failed to write the dock layout to C:\\tmp\\editor_layout.json"
        );
        assert!(error.source().is_some());
    }

    /// Diagnostic codes derive from the namespace and variant name.
    #[test]
    fn layout_persistence_codes_derive_from_namespace_and_variant() {
        let error = LayoutPersistenceError::Serialization {
            source: serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
        };
        let code = error.code().expect("generated code");
        assert_eq!(code.to_string(), "editor::layout::serialization");
    }

    /// Validation failures keep the underlying [`LayoutError`] in the chain.
    #[test]
    fn layout_validation_keeps_the_invariant_failure_in_the_chain() {
        let error = LayoutPersistenceError::Validation {
            source: LayoutError::UnsupportedVersion(99),
        };
        assert_eq!(error.to_plain_message(), "the saved dock layout is invalid");
        let source = error.source().expect("validation keeps its source");
        assert_eq!(
            source.to_string(),
            "invalid dock layout: UnsupportedVersion(99)"
        );
    }
}
