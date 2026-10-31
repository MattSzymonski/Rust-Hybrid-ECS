//! Versioned dock-layout loading and recoverable writes.
//!
//! # Responsibilities
//!
//! - Load the saved dock, falling back to the default for a document this
//!   build cannot read.
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
/// Any change to the tab set, or to what the document carries beside the dock
/// tree, bumps it: `EditorTab` is serialized into the document, and the tab
/// payloads plus the panel-id and pop-out lists are what a reader has to
/// understand.
///
/// A document `migrate` has a rule for is upgraded in place and stamped; any
/// other - older than the oldest rule, or newer than this build - is replaced
/// by the default layout with one log line.
pub const LAYOUT_SCHEMA_VERSION: u32 = 13;

/// The application directory inside the per-user state directory.
const APPLICATION_DIRECTORY: &str = "RustHybridEcs";

/// The dock document's file name in the layout directory.
const LAYOUT_FILE: &str = "editor_egui_layout.json";

/// Failures the dock model reports.
///
/// [`LayoutPersistenceError`] carries this type as its validation source; it
/// stays here because the shared error surface names it (`editor::layout`).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LayoutError {
    /// The saved document was written by a newer layout schema.
    #[error("invalid dock layout: UnsupportedVersion({0})")]
    UnsupportedVersion(u32),
}

/// Load the saved dock, falling back to the default on any invalid document.
pub fn load_or_default() -> EditorDock {
    load_from_path(&layout_directory().join(LAYOUT_FILE))
}

/// Load the dock document at `path`, falling back to the default on any
/// document this build cannot read.
///
/// An older document [`migrate`] has a rule for is upgraded and written back
/// first. The default is the answer for every remaining reason - a missing
/// file, an unreadable schema, a document that does not parse - so the editor
/// always opens. A reset is logged once, at info, as `[editor] layout reset`:
/// a suite tells a reset from a restored layout without reading the reason,
/// and a person reads the reason.
fn load_from_path(path: &Path) -> EditorDock {
    // A missing file is the normal first-run state and stays silent.
    let Ok(contents) = std::fs::read_to_string(path) else {
        return EditorDock::new();
    };
    match serde_json::from_str::<EditorDock>(&contents) {
        Ok(dock) if dock.is_current() => dock,
        Ok(mut dock) => match migrate(&mut dock) {
            Some(previous) => {
                // A document this build can upgrade in place: keep the layout
                // the user arranged and write the stamped version back.
                save(&dock);
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    path = %path.display(),
                    from = previous,
                    to = LAYOUT_SCHEMA_VERSION,
                    "[editor] layout migrated"
                );
                dock
            }
            None => {
                reset_layout(
                    path,
                    &format!(
                        "schema {} where this build writes {}",
                        dock.schema_version(),
                        LAYOUT_SCHEMA_VERSION
                    ),
                );
                EditorDock::new()
            }
        },
        Err(source) => {
            reset_layout(path, &format!("unreadable document: {source}"));
            EditorDock::new()
        }
    }
}

/// Upgrade an older document in place, returning the version it came from;
/// `None` when this build has no rule for that version.
///
/// Each chain entry names the newest schema that still lacks a tab, the anchor
/// it is appended beside and the tab itself. A document several versions old
/// gets every missing tab in one pass, chained in strip order, and nothing the
/// user arranged moves.
fn migrate(dock: &mut EditorDock) -> Option<u32> {
    const CHAIN: [(u32, super::EditorTab, super::EditorTab); 3] = [
        (7, super::EditorTab::Assets, super::EditorTab::Console),
        (8, super::EditorTab::Console, super::EditorTab::Problems),
        (9, super::EditorTab::Problems, super::EditorTab::Statistics),
    ];
    let previous = dock.schema_version();
    if !(7..LAYOUT_SCHEMA_VERSION).contains(&previous) {
        return None;
    }
    for (version, anchor, tab) in CHAIN {
        if previous <= version && !dock.contains(tab) {
            append_beside(dock, anchor, tab);
        }
    }
    dock.set_schema_version(LAYOUT_SCHEMA_VERSION);
    Some(previous)
}

/// Append `tab` to the leaf that holds `anchor`, or to the focused leaf when
/// the anchor is not docked - an unusual layout keeps the rest of itself.
fn append_beside(dock: &mut EditorDock, anchor: super::EditorTab, tab: super::EditorTab) {
    let leaf = dock
        .state_mut()
        .find_tab(&anchor)
        .map(|path| path.node_path());
    match leaf {
        Some(path) => {
            if let Ok(node) = dock.state_mut().leaf_mut(path) {
                node.append_tab(tab);
            }
        }
        None => dock.state_mut().push_to_focused_leaf(tab),
    }
}

/// Log one replaced dock document.
fn reset_layout(path: &Path, reason: &str) {
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        path = %path.display(),
        reason,
        "[editor] layout reset"
    );
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

/// Serialize and install `dock` at `path`.
fn save_to_path(dock: &EditorDock, path: PathBuf) -> Result<(), LayoutPersistenceError> {
    let document = serde_json::to_vec_pretty(&dock.settled())
        .map_err(|source| LayoutPersistenceError::Serialization { source })?;
    crate::editor_files::write_bytes(&path, &document).map_err(|error| match error {
        crate::editor_files::FileError::Write { path, source } => {
            LayoutPersistenceError::Filesystem { path, source }
        }
        other => LayoutPersistenceError::Filesystem {
            path: path.display().to_string(),
            source: io::Error::other(other.message()),
        },
    })
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
/// so the web posture answers a fixed relative directory and every load fails
/// into the default dock.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::EditorTab;

    /// A layout document in `egui_dock` 0.17's node model: a real v6
    /// document, not a hand-written sketch.
    const V6_LAYOUT: &str = include_str!("fixtures/layout_v6.json");

    /// A directory of this test's own under the temp directory.
    fn scratch_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("pill_layout_test_{name}"));
        std::fs::create_dir_all(&directory).expect("the temp directory is writable");
        directory
    }

    #[test]
    fn a_stored_v6_document_is_replaced_by_the_default_layout() {
        // The node models differ. Whether the document cannot be read at all
        // or parses and then fails the schema check, it must never be mistaken
        // for this build's own file.
        if let Ok(dock) = serde_json::from_str::<EditorDock>(V6_LAYOUT) {
            assert_eq!(dock.schema_version(), 6);
            assert!(!dock.is_current());
        }
        let directory = scratch_directory("v6");
        let path = directory.join(LAYOUT_FILE);
        std::fs::write(&path, V6_LAYOUT).expect("the fixture writes");

        let dock = load_from_path(&path);

        assert!(dock.is_current());
        for tab in EditorTab::ALL {
            assert!(dock.contains(tab), "{tab:?} is missing from the default");
        }
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_layout_written_by_this_build_loads_back_unchanged() {
        let directory = scratch_directory("round_trip");
        let path = directory.join(LAYOUT_FILE);
        let saved = EditorDock::new();
        save_to_path(&saved, path.clone()).expect("the layout saves");

        let loaded = load_from_path(&path);

        assert_eq!(loaded.schema_version(), LAYOUT_SCHEMA_VERSION);
        for tab in EditorTab::ALL {
            assert_eq!(
                saved.state().find_tab(&tab),
                loaded.state().find_tab(&tab),
                "{tab:?} moved between the file and the load"
            );
        }
        std::fs::remove_dir_all(&directory).ok();
    }

    /// Saving twice replaces the document: the rename the shared state-file
    /// rule relies on is a replace on this platform too, so the writer no
    /// longer keeps a `.bak` fallback for the opposite belief.
    #[test]
    fn a_second_save_replaces_the_first_layout() {
        let directory = scratch_directory("second_save");
        let path = directory.join(LAYOUT_FILE);
        save_to_path(&EditorDock::new(), path.clone()).expect("the first save");
        let mut moved = EditorDock::new();
        moved.pop_out(EditorTab::Console);
        save_to_path(&moved, path.clone()).expect("the second save");

        let loaded = load_from_path(&path);

        assert!(
            loaded.is_popped_out(EditorTab::Console),
            "the second save did not land"
        );
        assert!(
            !path.with_extension("json.bak").exists(),
            "the writer kept a backup file"
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_document_from_a_newer_schema_is_replaced_too() {
        let directory = scratch_directory("newer");
        let path = directory.join(LAYOUT_FILE);
        let mut document: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&EditorDock::new()).expect("serializes"))
                .expect("the document is JSON");
        document["schema_version"] = serde_json::Value::from(LAYOUT_SCHEMA_VERSION + 1);
        std::fs::write(&path, serde_json::to_vec(&document).expect("reserializes"))
            .expect("the document writes");

        let dock = load_from_path(&path);

        assert!(dock.is_current());
        for tab in EditorTab::ALL {
            assert!(dock.contains(tab), "{tab:?} is missing from the default");
        }
        std::fs::remove_dir_all(&directory).ok();
    }
}
