//! Versioned dock-layout loading and recoverable writes.
//!
//! # Responsibilities
//!
//! - Load the saved dock, refusing documents this build cannot read.
//! - Write without destroying the previous file when a save goes wrong.
//! - Own [`layout_directory`], where the editor's state files live.

// Standard library
use std::io;
use std::path::{Path, PathBuf};

// External crates
use pill_core::error::EngineMessage;
use thiserror::Error;

// Current crate
use super::EditorDock;
use crate::error::LayoutPersistenceError;

/// The schema of the dock-layout document this build writes and accepts.
///
/// v2 adds the Scene and Assets tabs and the side/bottom default layout, so
/// documents written by the two-tab build are rejected and replaced by the
/// new default.
///
/// v3 adds the second viewport tab (`EditorTab::SecondScene`), so documents
/// written before it are rejected and replaced by the new default.
///
/// v4 adds the Inspector tab (`EditorTab::Inspector`) and its slot in the
/// default layout, so previous documents are rejected and replaced too.
///
/// v5 adds the Preferences tab (`EditorTab::Preferences`), tabbed with the
/// Systems list, so previous documents are rejected and replaced too.
///
/// v6 adds the Game tab (`EditorTab::Game`), tabbed with the scene
/// viewports, so previous documents are rejected and replaced too.
pub const LAYOUT_SCHEMA_VERSION: u32 = 6;

/// The application directory inside the per-user state directory.
const APPLICATION_DIRECTORY: &str = "RustHybridEcs";

/// The dock document's file name, beside the `pill_ui` editor's layout file.
const LAYOUT_FILE: &str = "editor_egui_layout.json";

/// Failures the dock model reports.
///
/// Persistence wraps these in [`LayoutPersistenceError`]; the type stays
/// here because the shared error surface names it (`editor::layout`).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LayoutError {
    /// The saved document was written by a newer layout schema.
    #[error("invalid dock layout: UnsupportedVersion({0})")]
    UnsupportedVersion(u32),
}

/// Load the saved dock, falling back to the default on any invalid document.
pub fn load_or_default() -> EditorDock {
    let path = layout_directory().join(LAYOUT_FILE);
    // A missing file is the normal first-run state and stays silent.
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return EditorDock::new();
    };
    match serde_json::from_str::<EditorDock>(&contents) {
        Ok(dock) if dock.is_current() => dock,
        Ok(dock) => {
            warn_about(
                &path,
                &LayoutPersistenceError::Validation {
                    source: LayoutError::UnsupportedVersion(dock.schema_version()),
                },
            );
            EditorDock::new()
        }
        Err(source) => {
            warn_about(
                &path,
                &LayoutPersistenceError::Invalid {
                    path: path.display().to_string(),
                    source,
                },
            );
            EditorDock::new()
        }
    }
}

/// Persist `dock`; a failure leaves the previous document untouched.
pub fn save(dock: &EditorDock) {
    let path = layout_directory().join(LAYOUT_FILE);
    if let Err(error) = save_to_path(dock, path) {
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            "Cannot save dock layout: {}",
            error.to_plain_message()
        );
    }
}

/// Log one rejected dock document and continue with the default.
fn warn_about(path: &Path, error: &LayoutPersistenceError) {
    pill_core::warn!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        "Ignoring saved dock layout '{}': {}",
        path.display(),
        error.to_plain_message()
    );
}

/// Serialize and install `dock` at `path`.
fn save_to_path(dock: &EditorDock, path: PathBuf) -> Result<(), LayoutPersistenceError> {
    let document = serde_json::to_vec_pretty(&dock.settled())
        .map_err(|source| LayoutPersistenceError::Serialization { source })?;
    write_atomic(&path, &document)
}

/// Write `document` to `path` through a temporary file and a recoverable
/// replace, so a crash mid-write never corrupts the previous document.
fn write_atomic(path: &Path, document: &[u8]) -> Result<(), LayoutPersistenceError> {
    let parent = path
        .parent()
        .ok_or_else(|| LayoutPersistenceError::Filesystem {
            path: path.display().to_string(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "layout path has no parent"),
        })?;
    std::fs::create_dir_all(parent).map_err(|source| LayoutPersistenceError::Filesystem {
        path: parent.display().to_string(),
        source,
    })?;
    let temporary = path.with_extension("json.tmp");
    let backup = path.with_extension("json.bak");
    std::fs::write(&temporary, document).map_err(|source| LayoutPersistenceError::Filesystem {
        path: temporary.display().to_string(),
        source,
    })?;

    // Unix rename replaces atomically. Windows rename does not replace an
    // existing target, so keep the previous valid file as a short-lived
    // backup while installing the new document.
    if std::fs::rename(&temporary, path).is_err() {
        let _ = std::fs::remove_file(&backup);
        if path.exists() {
            std::fs::rename(path, &backup).map_err(|source| {
                LayoutPersistenceError::Filesystem {
                    path: backup.display().to_string(),
                    source,
                }
            })?;
        }
        if let Err(source) = std::fs::rename(&temporary, path) {
            let _ = std::fs::rename(&backup, path);
            return Err(LayoutPersistenceError::Filesystem {
                path: path.display().to_string(),
                source,
            });
        }
    }
    let _ = std::fs::remove_file(&backup);
    Ok(())
}

/// The directory the editor's layout files live in.
pub fn layout_directory() -> PathBuf {
    editor_state_directory().join(APPLICATION_DIRECTORY)
}

/// The per-user directory editor state belongs in.
///
/// Windows: `%LOCALAPPDATA%`. Elsewhere: `$XDG_STATE_HOME`, or
/// `~/.local/state` when it is unset, as the XDG base directory spec says.
/// The temp directory is the last resort for an environment with neither.
///
/// A browser has no environment and no filesystem; `std::env` panics there,
/// so the web posture reports the working directory and every file read or
/// write fails gracefully into the default dock instead.
#[cfg(not(target_arch = "wasm32"))]
fn editor_state_directory() -> PathBuf {
    if cfg!(windows) {
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local_app_data);
        }
    } else {
        if let Some(state_home) = std::env::var_os("XDG_STATE_HOME").filter(|path| !path.is_empty())
        {
            return PathBuf::from(state_home);
        }
        if let Some(home) = std::env::var_os("HOME").filter(|path| !path.is_empty()) {
            return PathBuf::from(home).join(".local").join("state");
        }
    }
    std::env::temp_dir()
}

/// The web posture's editor state directory; see the native function above.
#[cfg(target_arch = "wasm32")]
fn editor_state_directory() -> PathBuf {
    PathBuf::from("editor-state")
}
