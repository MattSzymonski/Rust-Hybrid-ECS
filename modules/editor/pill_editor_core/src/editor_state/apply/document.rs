//! The document family's applier.
//!
//! The scene documents and the preferences.
//!
//! # Responsibilities
//!
//! - Apply the scene document commands (new, open, save, save as) and reset
//!   the unsaved marker they own.
//! - Apply a preferences change.
//!
//! # Design
//!
//! One `match` over the family's own variants, called from
//! [`EditorCommand::apply_one`](super::EditorCommand). Every arm ends in a
//! [`CommandOutcome`]: the inverse the history records, or `plain()` for a
//! command that cannot be undone.

use pill_engine::Engine;

use super::CommandOutcome;
use super::*;

/// Apply one document command.
pub(super) fn apply(
    engine: &mut Engine,
    command: &EditorCommand,
) -> Result<CommandOutcome, String> {
    match command {
        EditorCommand::SetPreferences { values } => {
            // Read-modify-write, so the per-project gizmo entry and any
            // key another writer owns survive; blanks write nothing, so
            // an untouched row keeps the consumer's own default.
            let directory = crate::layout::layout_directory();
            let mut document = crate::preferences::read_document(&directory)
                .map_err(|message| format!("{command:?}: {message}"))?;
            let mut written = 0usize;
            for (key, text) in values {
                if text.trim().is_empty() {
                    continue;
                }
                written += 1;
                crate::preferences::set_key(
                    &mut document,
                    key,
                    crate::preferences::parse_value(text),
                )
                .map_err(|message| format!("{command:?}: {message}"))?;
            }
            crate::preferences::write_document(&directory, &document)
                .map_err(|message| format!("{command:?}: {message}"))?;
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                keys = written,
                "[editor] preferences saved"
            );
            Ok(CommandOutcome::plain())
        }

        EditorCommand::SceneNew => {
            let removed = crate::scene_workflow::new_scene(engine);
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                entities = removed,
                "[editor] scene new"
            );
            Ok(CommandOutcome::plain())
        }

        EditorCommand::SceneOpen { path } => {
            crate::scene_workflow::open_scene(engine, path)
                .map_err(|error| format!("{command:?}: {error}"))?;
            Ok(CommandOutcome::plain())
        }

        EditorCommand::SceneSave => {
            crate::scene_workflow::write_scene(engine, None)
                .map_err(|error| format!("{command:?}: {error}"))?;
            Ok(CommandOutcome::plain())
        }

        EditorCommand::SceneSaveAs { path } => {
            let path = crate::scene_workflow::scene_file_name(path);
            crate::scene_workflow::write_scene(engine, Some(&path))
                .map_err(|error| format!("{command:?}: {error}"))?;
            Ok(CommandOutcome::plain())
        }

        other => unreachable!("{:?} is not a document command", other.family()),
    }
}
