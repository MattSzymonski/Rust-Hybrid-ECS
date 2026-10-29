//! Editor-state recovery.
//!
//! # Responsibilities
//!
//! - [`RecoveryDocument`]: the dock, the selection and the version stamp an
//!   abrupt exit leaves behind, as a JSON document beside the layout file.
//! - [`write`] / [`read`] / [`remove`]: the executable's side of that file.
//!
//! # Design
//!
//! The layout file is written on a **clean** quit and describes a state the
//! person chose to keep. The recovery file is written while the editor runs
//! and removed on a clean exit, so its presence at startup means the last run
//! did not finish: the document is then what the editor restores, and the
//! person sees the arrangement they had rather than the default one.
//!
//! The selection travels as display handles and names - strings, never
//! handles or type ids - because a recovery document outlives the process
//! that wrote it (the cross-DLL rule the clipboard payload follows too).

// External crates
use serde::{Deserialize, Serialize};

// Current crate
use crate::editor_state::EditorSelection;
use crate::layout::EditorDock;

/// The recovery document's version; a document from another version is
/// ignored rather than half-applied.
pub const RECOVERY_VERSION: u32 = 1;

/// The recovery document's file name, beside the layout document.
pub const RECOVERY_FILE: &str = "editor_egui_recovery.json";

/// What a run leaves behind so an unclean exit can be undone.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoveryDocument {
    /// The document's version ([`RECOVERY_VERSION`]).
    pub version: u32,
    /// The dock as it stood at the last autosave.
    pub dock: EditorDock,
    /// The selected entities' display handles, in selection order.
    pub entities: Vec<String>,
    /// The selected asset's project-relative path, when one was selected.
    pub asset: Option<String>,
    /// The selected resource's stable name, when one was selected.
    pub resource: Option<String>,
}

impl RecoveryDocument {
    /// The document for the current state.
    ///
    /// The dock is settled first: a leaf the frame has not laid out yet holds
    /// non-finite floats, which `serde_json` writes as `null` and cannot read
    /// back - the same reason the layout document is written settled.
    pub fn capture(dock: &EditorDock, selection: &EditorSelection) -> Self {
        Self {
            version: RECOVERY_VERSION,
            dock: dock.settled(),
            entities: selection
                .entities()
                .iter()
                .map(|entity| entity.to_string())
                .collect(),
            asset: selection.asset.clone(),
            resource: selection.resource.clone(),
        }
    }

    /// Whether this document was written by a build this one understands.
    pub fn is_current(&self) -> bool {
        self.version == RECOVERY_VERSION
    }
}

/// The recovery document's path in the editor's state directory.
pub fn recovery_path() -> std::path::PathBuf {
    crate::layout::layout_directory().join(RECOVERY_FILE)
}

/// Write the document, replacing the previous one.
///
/// Returns whether it was written; a failure is logged and the editor keeps
/// running, because losing recoverability must never stop the editor.
pub fn write(document: &RecoveryDocument) -> bool {
    let path = recovery_path();
    let written = crate::editor_files::write_json(&path, document).map_err(|error| error.message());
    match written {
        Ok(()) => true,
        Err(message) => {
            pill_core::warn!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                reason = message.as_str(),
                "[editor] recovery write failed"
            );
            false
        }
    }
}

/// The document a previous run left behind, when it left one this build can
/// use; `None` when the last run exited cleanly or the document is unusable.
pub fn read() -> Option<RecoveryDocument> {
    let path = recovery_path();
    let contents = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<RecoveryDocument>(&contents) {
        Ok(document) if document.is_current() => Some(document),
        Ok(document) => {
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                version = document.version,
                "[editor] recovery document ignored"
            );
            let _ = std::fs::remove_file(&path);
            None
        }
        Err(error) => {
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                reason = %error,
                "[editor] recovery document ignored"
            );
            let _ = std::fs::remove_file(&path);
            None
        }
    }
}

/// Remove the document: the run it described is ending cleanly, so there is
/// nothing left to recover.
pub fn remove() {
    let path = recovery_path();
    match std::fs::remove_file(&path) {
        Ok(()) => pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            "[editor] recovery document removed"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            reason = %error,
            "[editor] recovery document could not be removed"
        ),
    }
}
