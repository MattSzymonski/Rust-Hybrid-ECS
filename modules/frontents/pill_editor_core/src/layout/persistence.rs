//! Versioned editor-layout loading and recoverable writes.
//!
//! # Responsibilities
//!
//! - Load a saved layout, refusing versions this build cannot read.
//! - Write without destroying the previous file when a save goes wrong.
//! - Own the recovery document (Stage 28): the editor state an unclean exit
//!   leaves behind beside the layout file, which a clean exit removes.

use std::path::{Path, PathBuf};

use pill_core::error::EngineMessage;
use serde::{Deserialize, Serialize};

use super::LayoutModel;
use crate::error::LayoutPersistenceError;

const APPLICATION_DIRECTORY: &str = "RustHybridEcs";
const LAYOUT_FILE: &str = "editor_layout.json";

/// The recovery file's name, beside the layout file.
pub const RECOVERY_FILE: &str = "editor_recovery.json";

/// The schema of the recovery document this build writes and accepts.
pub const RECOVERY_SCHEMA_VERSION: u32 = 1;

/// Load the saved workspace, falling back safely on any invalid document.
pub fn load_or_default() -> LayoutModel {
    let path = layout_path();
    // A missing file is the normal first-run state and stays silent.
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return LayoutModel::default_editor();
    };
    let model = match serde_json::from_str::<LayoutModel>(&contents) {
        Ok(model) => model,
        Err(source) => {
            report_load_failure(
                &path,
                LayoutPersistenceError::Invalid {
                    path: path.display().to_string(),
                    source,
                },
            );
            return LayoutModel::default_editor();
        }
    };
    if let Err(source) = model.validate() {
        report_load_failure(&path, LayoutPersistenceError::Validation { source });
        return LayoutModel::default_editor();
    }
    // A layout saved by an older editor schema may not contain the panels this
    // build knows about (for example `Systems`); refuse it loudly instead of
    // silently rendering a workspace without a way to open them.
    if model.schema_version != super::model::LAYOUT_SCHEMA_VERSION {
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            "Ignoring saved layout '{}': schema v{} is stale (this build is v{})",
            path.display(),
            model.schema_version,
            super::model::LAYOUT_SCHEMA_VERSION
        );
        return LayoutModel::default_editor();
    }
    model
}

/// Log one rejected layout document and continue with the default layout.
fn report_load_failure(path: &Path, error: LayoutPersistenceError) {
    pill_core::warn!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        "Ignoring invalid saved layout '{}': {}",
        path.display(),
        error.to_plain_message()
    );
}

/// Persist a validated model through a temporary file and recoverable replace.
pub fn save(model: &LayoutModel) {
    if let Err(error) = save_to_path(model, layout_path()) {
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            "Cannot save dock layout: {}",
            error.to_plain_message()
        );
    }
}

/// Install `model` at `path` without ever corrupting the previous document.
fn save_to_path(model: &LayoutModel, path: PathBuf) -> Result<(), LayoutPersistenceError> {
    model
        .validate()
        .map_err(|source| LayoutPersistenceError::Validation { source })?;
    let document = serde_json::to_vec_pretty(model)
        .map_err(|source| LayoutPersistenceError::Serialization { source })?;
    write_atomic(&path, &document)
}

/// Write `document` to `path` through a temporary file and a recoverable
/// replace, so a crash mid-write never corrupts the previous file.
fn write_atomic(path: &Path, document: &[u8]) -> Result<(), LayoutPersistenceError> {
    let parent = path
        .parent()
        .ok_or_else(|| LayoutPersistenceError::Filesystem {
            path: path.display().to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "layout path has no parent",
            ),
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
    // existing target, so retain the previous valid file as a short-lived
    // backup while installing the new document.
    if std::fs::rename(&temporary, &path).is_err() {
        let _ = std::fs::remove_file(&backup);
        if path.exists() {
            std::fs::rename(&path, &backup).map_err(|source| {
                LayoutPersistenceError::Filesystem {
                    path: backup.display().to_string(),
                    source,
                }
            })?;
        }
        if let Err(source) = std::fs::rename(&temporary, &path) {
            let _ = std::fs::rename(&backup, &path);
            return Err(LayoutPersistenceError::Filesystem {
                path: path.display().to_string(),
                source,
            });
        }
        let _ = std::fs::remove_file(backup);
    }
    Ok(())
}

/// Where the editor keeps its layout: the platform's per-user state directory,
/// so it survives temp-directory cleanup.
fn layout_path() -> PathBuf {
    layout_directory().join(LAYOUT_FILE)
}

// =============================================================================
// Recovery document (Stage 28)
// =============================================================================

/// The editor state a crash left behind: the dock layout, the shared
/// selection by stable identity, and the free-standing windows that were
/// open, all as owned plain data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecoveryState {
    /// The document's schema version; a mismatch is refused like a stale
    /// layout.
    pub schema_version: u32,
    /// The dock layout as the shell had it when last written.
    pub layout: LayoutModel,
    /// The shared selection, by stable identity.
    pub selection: RecoverySelection,
    /// The free-standing windows that were open, in open order.
    pub windows: Vec<RecoveryWindow>,
}

/// The shared selection as the recovery document stores it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RecoverySelection {
    /// The selected entity's stable handle, when one was selected.
    pub entity: Option<RecoveryEntity>,
    /// The selected asset's project-relative path, when one was selected.
    pub asset: Option<String>,
}

/// An entity handle by its two stable numbers, so it survives a restart and
/// can be checked against the world it lands in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryEntity {
    /// The slot identifier.
    pub id: u64,
    /// The generation that disambiguates a recycled slot.
    pub generation: u32,
}

/// One free-standing window as the recovery document stores it; enough of a
/// window request's fields to ask for it again (the shell rebuilds the
/// request from these).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecoveryWindow {
    /// The window's identity.
    pub id: String,
    /// `"tool"` or `"dialog"`; the shell maps it back to its kind.
    pub kind: String,
    /// The title bar text.
    pub title: String,
    /// The window's width in logical pixels.
    pub width: f32,
    /// The window's height in logical pixels.
    pub height: f32,
    /// The registered panel the window shows.
    pub panel: String,
    /// A modal's buttons, in draw order.
    pub buttons: Vec<RecoveryWindowButton>,
    /// The window's outer top-left on the desktop, in logical pixels, when
    /// the platform reported one (Stage 29); missing in documents written
    /// before the field existed, and where no position exists (Wayland,
    /// D23).
    #[serde(default)]
    pub position: Option<(f32, f32)>,
}

/// One button of a recovered modal dialog.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecoveryWindowButton {
    /// The button's label.
    pub label: String,
    /// The command its activation executes, empty for a dismissal.
    pub command: String,
}

/// Where the recovery document lives: beside the layout file.
pub fn recovery_path() -> PathBuf {
    layout_directory().join(RECOVERY_FILE)
}

/// Load the recovery document, if a crash left one this build can read.
///
/// A missing file is the normal clean state and stays silent; an unreadable,
/// invalid or stale one is reported and ignored, which then behaves exactly
/// like a clean start.
pub fn load_recovery() -> Option<RecoveryState> {
    load_recovery_from(&recovery_path())
}

/// [`load_recovery`] against an explicit path, for tests.
fn load_recovery_from(path: &Path) -> Option<RecoveryState> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return None;
    };
    let state = match serde_json::from_str::<RecoveryState>(&contents) {
        Ok(state) => state,
        Err(source) => {
            report_load_failure(
                path,
                LayoutPersistenceError::Invalid {
                    path: path.display().to_string(),
                    source,
                },
            );
            return None;
        }
    };
    if state.schema_version != RECOVERY_SCHEMA_VERSION {
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            "Ignoring recovery document '{}': schema v{} is stale (this build is v{})",
            path.display(),
            state.schema_version,
            RECOVERY_SCHEMA_VERSION
        );
        return None;
    }
    if let Err(source) = state.layout.validate() {
        report_load_failure(path, LayoutPersistenceError::Validation { source });
        return None;
    }
    Some(state)
}

/// Write the recovery document. A failed write is reported by the caller:
/// recovery is a courtesy, never something a session depends on.
pub fn save_recovery(state: &RecoveryState) -> Result<(), LayoutPersistenceError> {
    save_recovery_to(state, &recovery_path())
}

/// [`save_recovery`] against an explicit path, for tests.
fn save_recovery_to(state: &RecoveryState, path: &Path) -> Result<(), LayoutPersistenceError> {
    let document = serde_json::to_vec_pretty(state)
        .map_err(|source| LayoutPersistenceError::Serialization { source })?;
    write_atomic(path, &document)
}

/// Remove the recovery document a clean exit makes unnecessary; a missing
/// file is the ordinary case and stays silent.
pub fn discard_recovery() {
    discard_recovery_at(&recovery_path());
}

/// [`discard_recovery`] against an explicit path, for tests.
fn discard_recovery_at(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// The directory the editor's layout files live in.
///
/// The classic editor's file and the native editor's own file sit side by
/// side here; the native editor reads the classic one once for migration.
pub fn layout_directory() -> PathBuf {
    editor_state_directory().join(APPLICATION_DIRECTORY)
}

/// The per-user directory editor state belongs in.
///
/// Windows: `%LOCALAPPDATA%`. Elsewhere: `$XDG_STATE_HOME`, or
/// `~/.local/state` when it is unset, as the XDG base directory spec says.
/// The temp directory is the last resort for an environment with neither.
pub fn editor_state_directory() -> PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_layout_round_trips_through_a_file() {
        let directory = std::env::temp_dir().join(format!(
            "ecs-layout-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("layout.json");
        let model = LayoutModel::default_editor();
        save_to_path(&model, path.clone()).unwrap();
        let restored: LayoutModel = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(restored, model);
        let _ = std::fs::remove_dir_all(directory);
    }

    /// A layout saved before module panels existed still reads: built-in
    /// panels keep their plain snake-case names on disk.
    #[test]
    fn old_layout_panel_names_still_load() {
        use crate::layout::PanelKind;
        assert_eq!(
            serde_json::from_str::<PanelKind>("\"hierarchy\"").unwrap(),
            PanelKind::Hierarchy
        );
        let saved = serde_json::to_string(&LayoutModel::default_editor()).unwrap();
        assert!(saved.contains("\"scene\""), "{saved}");
        let restored: LayoutModel = serde_json::from_str(&saved).unwrap();
        assert!(restored.validate().is_ok());
    }

    /// A tab holding a module panel survives a save and a load, so a layout
    /// keeps the tab while its editor crate is not loaded.
    #[test]
    fn module_panel_tab_round_trips_through_a_file() {
        use crate::layout::{LayoutAction, PanelKind};
        let directory = std::env::temp_dir().join(format!(
            "ecs-layout-module-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("layout.json");
        let mut model = LayoutModel::default_editor();
        let target_tabset = model.resolved_active_tabset().unwrap();
        model
            .apply(LayoutAction::OpenTab {
                panel: PanelKind::Module("x".to_string()),
                target_tabset,
            })
            .unwrap();
        save_to_path(&model, path.clone()).unwrap();
        let restored: LayoutModel = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(restored, model);
        assert!(restored.validate().is_ok());
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("\"module\": \"x\""));
        let _ = std::fs::remove_dir_all(directory);
    }

    /// A recovery document survives a round trip, and a clean exit's discard
    /// removes it; the next load then reports nothing.
    #[test]
    fn a_recovery_document_round_trips_and_discards() {
        let directory = std::env::temp_dir().join(format!(
            "ecs-recovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = directory.join(RECOVERY_FILE);
        let state = RecoveryState {
            schema_version: RECOVERY_SCHEMA_VERSION,
            layout: LayoutModel::default_editor(),
            selection: RecoverySelection {
                entity: Some(RecoveryEntity {
                    id: 7,
                    generation: 3,
                }),
                asset: Some(String::from("res/thing.material")),
            },
            windows: vec![RecoveryWindow {
                id: String::from("spline_tool"),
                kind: String::from("tool"),
                title: String::from("Spline Tool"),
                width: 320.0,
                height: 240.0,
                panel: String::from("spline_tool"),
                buttons: vec![RecoveryWindowButton {
                    label: String::from("OK"),
                    command: String::from("spline.confirm"),
                }],
                position: Some((120.0, 80.0)),
            }],
        };
        save_recovery_to(&state, &path).unwrap();
        assert!(path.is_file());
        let restored = load_recovery_from(&path).expect("the document reads back");
        assert_eq!(restored, state);

        discard_recovery_at(&path);
        assert!(!path.exists());
        assert!(load_recovery_from(&path).is_none());
        let _ = std::fs::remove_dir_all(directory);
    }

    /// A stale schema, unparseable bytes and a layout that fails validation
    /// are all ignored: the editor then starts exactly as it would cleanly.
    #[test]
    fn a_stale_or_broken_recovery_document_is_ignored() {
        let directory = std::env::temp_dir().join(format!(
            "ecs-recovery-broken-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(RECOVERY_FILE);

        let mut state = RecoveryState {
            schema_version: RECOVERY_SCHEMA_VERSION + 1,
            layout: LayoutModel::default_editor(),
            selection: RecoverySelection::default(),
            windows: Vec::new(),
        };
        save_recovery_to(&state, &path).unwrap();
        assert!(
            load_recovery_from(&path).is_none(),
            "a stale schema is refused"
        );

        state.schema_version = RECOVERY_SCHEMA_VERSION;
        state.layout.nodes.clear();
        save_recovery_to(&state, &path).unwrap();
        assert!(
            load_recovery_from(&path).is_none(),
            "an invalid layout is refused"
        );

        std::fs::write(&path, b"not json at all").unwrap();
        assert!(
            load_recovery_from(&path).is_none(),
            "unparseable bytes are refused"
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    /// Unwritable destinations surface as a typed filesystem error.
    #[test]
    fn save_reports_filesystem_failures_as_typed_errors() {
        let directory = std::env::temp_dir().join(format!(
            "ecs-layout-fail-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Occupying the directory path with a regular file makes
        // `create_dir_all` fail, which is the first filesystem step.
        std::fs::write(&directory, "not a directory").unwrap();
        let path = directory.join("layout.json");
        let model = LayoutModel::default_editor();
        let error = save_to_path(&model, path).unwrap_err();
        assert!(error
            .to_plain_message()
            .contains("failed to write the dock layout"));
        let _ = std::fs::remove_file(directory);
    }
}
