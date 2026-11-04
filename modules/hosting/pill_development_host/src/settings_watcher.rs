//! Watches the project's `settings` directory and reports the managers whose
//! settings file changed.
//!
//! # Responsibilities
//!
//! - Watch `<project>/settings` and group a burst of events into one batch, the
//!   same way the asset and source watchers do.
//! - Report each changed manager by its settings name - the file stem without
//!   `_settings.yaml` - for the main thread to apply at a frame boundary.
//!
//! # Design
//!
//! The worker thread runs only host code and never calls into a module, so a
//! reload can unload a manager at any moment without leaving a thread running
//! inside code that is going away. The main thread applies the queue at the
//! frame boundary, after the reloads, against the managers that exist then.
//!
//! The watcher reports names, not decisions. Whether a name belongs to a
//! manager, and what to do when the file does not parse, are the main thread's
//! business - it holds the world.

// Standard library
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

// External crates
use notify::event::EventKind;
use pill_core::error::WatcherError;
use pill_core::info;

// Current crate
use crate::manager_settings::settings_name_of_file;
use crate::watcher::{is_relevant_event, spawn_settled_watcher};

// =============================================================================
// Types
// =============================================================================

/// A running settings watcher. Dropping it makes the worker stop when it next
/// reports a batch.
pub(crate) struct SettingsWatcher {
    /// Queued manager names, oldest batch first.
    changes: Receiver<Vec<String>>,
}

impl SettingsWatcher {
    /// Watches `settings_directory` and queues every batch of file changes.
    ///
    /// # Errors
    ///
    /// Returns a [`WatcherError`] when the watcher or its worker thread cannot
    /// be created, or the directory cannot be registered for watching.
    pub(crate) fn spawn(settings_directory: PathBuf) -> Result<Self, WatcherError> {
        // Step 1: Open the queue the worker fills and the main thread drains.
        let (change_sender, changes) = channel::<Vec<String>>();
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            directory = %settings_directory.display(),
            "[settings] watching the project's manager settings for edits"
        );

        // Step 2: Start the shared watcher worker. Every path passes through;
        // which of them names a manager is decided where the world is.
        spawn_settled_watcher(
            settings_directory,
            "pill settings watcher",
            "settings watcher error",
            |kind: &EventKind| is_relevant_event(kind) || is_any_rename(kind),
            |_path: &Path| true,
            move |root, touched| {
                let names = settings_names_of_batch(root, &touched);
                if names.is_empty() {
                    return true;
                }
                change_sender.send(names).is_ok()
            },
        )?;

        Ok(Self { changes })
    }

    /// Every batch queued since the last call, flattened into the manager names
    /// it touched, each once, oldest first.
    pub(crate) fn drain(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for batch in self.changes.try_iter() {
            for name in batch {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Whether the event is one side of a rename, which some backends report
/// instead of a create and a remove.
fn is_any_rename(kind: &EventKind) -> bool {
    matches!(kind, EventKind::Modify(notify::event::ModifyKind::Name(_)))
}

/// The manager names one batch of touched paths refers to.
///
/// The directory is not recursive in practice, but a path in a subdirectory is
/// ignored rather than turned into a name: a settings file lives directly in
/// `settings/`.
fn settings_names_of_batch(settings_directory: &Path, touched: &[PathBuf]) -> Vec<String> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    for path in touched {
        if path.is_dir() {
            continue;
        }
        let relative_path = path.strip_prefix(settings_directory).unwrap_or(path);
        // A nested path is not a settings file this host writes.
        if relative_path.components().count() != 1 {
            continue;
        }
        let Some(file_name) = relative_path.to_str() else {
            continue;
        };
        if file_name.starts_with('.') {
            continue;
        }
        if let Some(name) = settings_name_of_file(file_name) {
            names.insert(name);
        }
    }
    names.into_iter().collect()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    // Standard library
    use std::path::PathBuf;

    // Current crate
    use super::*;

    /// One batch names each manager once, ignores files that are not settings,
    /// nested paths and dot-files, and comes out sorted.
    #[test]
    fn a_batch_names_each_manager_once() {
        let directory = Path::new("project/settings");
        let touched = vec![
            PathBuf::from("project/settings/pill_audio_settings.yaml"),
            PathBuf::from("project/settings/pill_audio_settings.yaml"),
            PathBuf::from("project/settings/pill_physics_settings.yaml"),
            PathBuf::from("project/settings/.pill_audio_settings.yaml.tmp"),
            PathBuf::from("project/settings/notes.txt"),
            PathBuf::from("project/settings/nested/pill_audio_settings.yaml"),
        ];

        assert_eq!(
            settings_names_of_batch(directory, &touched),
            vec!["pill_audio".to_owned(), "pill_physics".to_owned()]
        );
    }

    /// A file that is not `*_settings.yaml` names no manager.
    #[test]
    fn only_settings_files_name_a_manager() {
        assert_eq!(
            settings_name_of_file("pill_audio_settings.yaml"),
            Some("pill_audio".to_owned())
        );
        assert_eq!(settings_name_of_file("pill_audio.yaml"), None);
        assert_eq!(settings_name_of_file("_settings.yaml"), None);
        assert_eq!(settings_name_of_file("settings.yaml"), None);
    }
}
