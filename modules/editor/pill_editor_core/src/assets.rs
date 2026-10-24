//! The Assets pipeline: the dialogs' state and the recorded file batches
//! the toolbar, the dialogs and the scripted channel queue.
//!
//! # Responsibilities
//!
//! - [`AssetDialog`]: the state the create and rename dialogs edit while
//!   one is open, owned by the session so it survives shell reloads.
//! - The open, submit and cancel helpers every entry point shares - the
//!   toolbar buttons, the dialog windows and the scripted channel - so each
//!   action takes one path and logs one token set.
//! - The immediate toolbar actions: [`queue_delete`],
//!   [`queue_write_meta`] and [`queue_move`], each a recorded
//!   [`EditorCommand`] batch.
//!
//! # Design
//!
//! The dialogs are egui windows drawn by the shell; the file work is
//! [`EditorCommand`](crate::EditorCommand)'s asset commands, applied by the
//! executable at the frame boundary and recorded in the history - the exact
//! contract the Assets tab follows. Nothing here touches the engine:
//! the batches land in [`PendingEditorCommands`] and the executable's drain
//! does the rest.

// External crates
use pill_core::telemetry::telemetry_target;

// Current crate
use crate::commands::PendingEditorCommands;
use crate::editor_state::{AssetListing, EditorCommand, EditorSelection, StandaloneTypeInfo};

/// The asset dialog that is open, if one is.
///
/// The create dialog carries its choice, its name and its target folder;
/// the rename dialog the current path and the typed name. The state lives
/// in [`crate::session::EguiShellState::asset_dialog`], so a shell reload
/// keeps an open dialog exactly as it was.
#[derive(Clone, Debug, PartialEq)]
pub enum AssetDialog {
    /// The create dialog: a standalone type, a name and the destination
    /// folder (relative to the asset directory; empty for its root).
    Create {
        /// The destination folder, `/`-separated; empty for the root.
        folder: String,
        /// The chosen standalone type's registered name; empty until one is
        /// chosen.
        type_name: String,
        /// The typed name; the type's naming rules add the extension.
        name: String,
    },
    /// The rename dialog: the path being renamed and the typed new name.
    Rename {
        /// The asset's current path, relative to the asset directory.
        from: String,
        /// The typed new name; the file keeps its extension.
        name: String,
    },
}

/// The selected asset's row, when the selection names one the snapshot
/// still holds.
pub fn selected_row<'a>(
    selection: &EditorSelection,
    assets: &'a [AssetListing],
) -> Option<&'a AssetListing> {
    let path = selection.asset.as_deref()?;
    assets.iter().find(|row| row.path == path)
}

/// Open the create dialog: the target folder follows the selection (the
/// folder itself, the selected file's folder, or the root) and the first
/// standalone type stands preselected.
pub fn open_create(
    dialog: &mut Option<AssetDialog>,
    selection: &EditorSelection,
    assets: &[AssetListing],
    types: &[StandaloneTypeInfo],
) {
    let folder = match assets
        .iter()
        .find(|row| Some(row.path.as_str()) == selection.asset.as_deref())
    {
        Some(row) if row.kind == "folder" => row.path.clone(),
        Some(row) => row
            .path
            .rsplit_once('/')
            .map(|(parent, _)| parent.to_string())
            .unwrap_or_default(),
        None => String::new(),
    };
    let type_name = types
        .first()
        .map(|info| info.type_name.clone())
        .unwrap_or_default();
    *dialog = Some(AssetDialog::Create {
        folder: folder.clone(),
        type_name,
        name: String::new(),
    });
    pill_core::info!(
        target: telemetry_target::ECS,
        dialog = "create",
        folder = folder.as_str(),
        "[editor] asset dialog opened"
    );
}

/// Open the rename dialog for the selected file; the name field arrives
/// with the file's stem in it.
pub fn open_rename(
    dialog: &mut Option<AssetDialog>,
    selection: &EditorSelection,
    assets: &[AssetListing],
) {
    let Some(row) = selected_row(selection, assets).filter(|row| row.kind != "folder") else {
        pill_core::warn!(
            target: telemetry_target::ECS,
            "[editor] rename asset needs a selected file"
        );
        return;
    };
    let file_name = row.path.rsplit('/').next().unwrap_or(&row.path);
    let stem = file_name.split('.').next().unwrap_or(file_name);
    *dialog = Some(AssetDialog::Rename {
        from: row.path.clone(),
        name: stem.to_string(),
    });
    pill_core::info!(
        target: telemetry_target::ECS,
        dialog = "rename",
        from = row.path.as_str(),
        "[editor] asset dialog opened"
    );
}

/// The create dialog's Create button: queue the recorded create command and
/// close the dialog; a missing type or name warns and keeps it open.
pub fn submit_create(
    dialog: &mut Option<AssetDialog>,
    pending: &mut PendingEditorCommands,
) -> bool {
    let Some(AssetDialog::Create {
        folder,
        type_name,
        name,
    }) = dialog.as_ref()
    else {
        pill_core::warn!(
            target: telemetry_target::ECS,
            "[editor] create asset without its dialog"
        );
        return false;
    };
    if type_name.is_empty() || name.trim().is_empty() {
        pill_core::warn!(
            target: telemetry_target::ECS,
            "[editor] create asset needs a type and a name"
        );
        return false;
    }
    let folder = folder.clone();
    let type_name = type_name.clone();
    let name = name.clone();
    pending.push_recorded(
        vec![EditorCommand::CreateStandaloneAsset {
            type_name: type_name.clone(),
            folder,
            name: name.clone(),
        }],
        format!("create asset {name}"),
        None,
    );
    pill_core::info!(
        target: telemetry_target::ECS,
        type_name = type_name.as_str(),
        name = name.as_str(),
        "[editor] asset create queued"
    );
    close(dialog, "create");
    true
}

/// The rename dialog's Rename button: build the new path from the typed
/// name - the extension stays - queue the recorded move and close the
/// dialog.
pub fn submit_rename(
    dialog: &mut Option<AssetDialog>,
    pending: &mut PendingEditorCommands,
) -> bool {
    let Some(AssetDialog::Rename { from, name }) = dialog.as_ref() else {
        pill_core::warn!(
            target: telemetry_target::ECS,
            "[editor] rename asset without its dialog"
        );
        return false;
    };
    let from = from.clone();
    let typed = name.trim().to_string();
    if from.is_empty() || typed.is_empty() {
        pill_core::warn!(
            target: telemetry_target::ECS,
            "[editor] rename asset needs a name"
        );
        return false;
    }
    if typed.contains(['/', '\\']) {
        pill_core::warn!(
            target: telemetry_target::ECS,
            name = typed.as_str(),
            "[editor] rename asset refuses a path separator"
        );
        return false;
    }
    // The extension stays with the file unless the typed name already ends
    // with it.
    let file_name = from.rsplit('/').next().unwrap_or(&from);
    let extension = file_name
        .split_once('.')
        .map(|(_, extension)| extension.to_owned());
    let new_file = match &extension {
        Some(extension)
            if !typed
                .to_ascii_lowercase()
                .ends_with(&extension.to_ascii_lowercase()) =>
        {
            format!("{typed}.{extension}")
        }
        _ => typed.clone(),
    };
    let folder = from
        .rsplit_once('/')
        .map(|(folder, _)| folder.to_string())
        .unwrap_or_default();
    let to = if folder.is_empty() {
        new_file.clone()
    } else {
        format!("{folder}/{new_file}")
    };
    pending.push_recorded(
        vec![EditorCommand::MoveAssetFile {
            from: from.clone(),
            to: to.clone(),
        }],
        format!("rename asset {new_file}"),
        None,
    );
    pill_core::info!(
        target: telemetry_target::ECS,
        from = from.as_str(),
        to = to.as_str(),
        "[editor] asset rename queued"
    );
    close(dialog, "rename");
    true
}

/// Close the open dialog, whatever it is; `false` when none was open.
pub fn cancel(dialog: &mut Option<AssetDialog>) -> bool {
    let Some(current) = dialog.as_ref() else {
        return false;
    };
    let kind = match current {
        AssetDialog::Create { .. } => "create",
        AssetDialog::Rename { .. } => "rename",
    };
    *dialog = None;
    pill_core::info!(
        target: telemetry_target::ECS,
        dialog = kind,
        "[editor] asset dialog closed"
    );
    true
}

/// Queue the recorded delete for `path` (the file and its `.meta` sidecar).
pub fn queue_delete(pending: &mut PendingEditorCommands, path: &str) {
    pending.push_recorded(
        vec![EditorCommand::DeleteAssetFile {
            path: path.to_string(),
        }],
        format!("delete asset {path}"),
        None,
    );
    pill_core::info!(
        target: telemetry_target::ECS,
        path,
        "[editor] asset delete queued"
    );
}

/// Queue the recorded `.meta` write for `path`.
pub fn queue_write_meta(pending: &mut PendingEditorCommands, path: &str) {
    pending.push_recorded(
        vec![EditorCommand::WriteAssetMeta {
            path: path.to_string(),
        }],
        format!("write meta {path}"),
        None,
    );
    pill_core::info!(
        target: telemetry_target::ECS,
        path,
        "[editor] asset meta queued"
    );
}

/// Queue the recorded move of `from` to `to` (the `.meta` sidecar travels
/// along, so the loaded asset keeps its handle and guid).
pub fn queue_move(pending: &mut PendingEditorCommands, from: &str, to: &str) {
    pending.push_recorded(
        vec![EditorCommand::MoveAssetFile {
            from: from.to_string(),
            to: to.to_string(),
        }],
        format!("move asset {from}"),
        None,
    );
    pill_core::info!(
        target: telemetry_target::ECS,
        from,
        to,
        "[editor] asset move queued"
    );
}

/// The toolbar's Delete Asset: needs a selected file; refuses (and warns)
/// otherwise, so the scripted path takes the same rule the button's
/// enablement does.
pub fn delete_selection(
    pending: &mut PendingEditorCommands,
    selection: &EditorSelection,
    assets: &[AssetListing],
) -> bool {
    let Some(row) = selected_row(selection, assets).filter(|row| row.kind != "folder") else {
        pill_core::warn!(
            target: telemetry_target::ECS,
            "[editor] delete asset needs a selected file"
        );
        return false;
    };
    queue_delete(pending, &row.path);
    true
}

/// The toolbar's Write Asset Meta: needs a selected sourced file that has
/// no sidecar (nor a standalone header) yet.
pub fn write_meta_selection(
    pending: &mut PendingEditorCommands,
    selection: &EditorSelection,
    assets: &[AssetListing],
) -> bool {
    let Some(row) = selected_row(selection, assets)
        .filter(|row| row.kind != "folder" && !row.standalone && !row.has_meta)
    else {
        pill_core::warn!(
            target: telemetry_target::ECS,
            "[editor] write asset meta needs a sourced file without one"
        );
        return false;
    };
    queue_write_meta(pending, &row.path);
    true
}

/// The destination a dragged file lands at inside `folder`: its own file
/// name under the folder's path; `None` when the file already lives there.
pub fn drop_destination(from: &str, folder: &str) -> Option<String> {
    let file_name = from.rsplit('/').next().unwrap_or(from);
    let to = if folder.is_empty() {
        file_name.to_string()
    } else {
        format!("{folder}/{file_name}")
    };
    (to != from).then_some(to)
}

/// Close the dialog, logging it under the given kind.
fn close(dialog: &mut Option<AssetDialog>, kind: &str) {
    *dialog = None;
    pill_core::info!(
        target: telemetry_target::ECS,
        dialog = kind,
        "[editor] asset dialog closed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_state::EditorSelection;

    fn listing(path: &str, kind: &str, standalone: bool, has_meta: bool) -> AssetListing {
        AssetListing {
            path: path.to_string(),
            kind: kind.to_string(),
            standalone,
            has_meta,
        }
    }

    fn types() -> Vec<StandaloneTypeInfo> {
        vec![
            StandaloneTypeInfo {
                type_name: "pill_master_renderer::Material".to_string(),
                display_name: "Material".to_string(),
                extension: "material".to_string(),
            },
            StandaloneTypeInfo {
                type_name: "pill_master_renderer::Skybox".to_string(),
                display_name: "Skybox".to_string(),
                extension: "skybox".to_string(),
            },
        ]
    }

    fn batches(pending: &PendingEditorCommands) -> &[crate::commands::PendingCommandBatch] {
        &pending.batches
    }

    #[test]
    fn opening_create_seeds_the_folder_and_first_type() {
        let assets = vec![
            listing("textures", "folder", false, false),
            listing("textures/rock.png", "texture", false, true),
        ];
        let mut dialog = None;
        let mut selection = EditorSelection::default();
        open_create(&mut dialog, &selection, &assets, &types());
        // Nothing selected: the root.
        assert_eq!(
            dialog,
            Some(AssetDialog::Create {
                folder: String::new(),
                type_name: "pill_master_renderer::Material".to_string(),
                name: String::new(),
            })
        );
        // A selected folder is the target.
        selection.asset = Some("textures".to_string());
        open_create(&mut dialog, &selection, &assets, &types());
        match dialog.as_ref() {
            Some(AssetDialog::Create { folder, .. }) => assert_eq!(folder, "textures"),
            other => panic!("expected a create dialog, got {other:?}"),
        }
        // A selected file targets its folder.
        selection.asset = Some("textures/rock.png".to_string());
        open_create(&mut dialog, &selection, &assets, &types());
        match dialog.as_ref() {
            Some(AssetDialog::Create { folder, .. }) => assert_eq!(folder, "textures"),
            other => panic!("expected a create dialog, got {other:?}"),
        }
    }

    #[test]
    fn submitting_create_queues_the_batch_and_closes() {
        let mut dialog = Some(AssetDialog::Create {
            folder: "textures".to_string(),
            type_name: "pill_master_renderer::Material".to_string(),
            name: "rock".to_string(),
        });
        let mut pending = PendingEditorCommands::default();
        assert!(submit_create(&mut dialog, &mut pending));
        assert!(dialog.is_none());
        let batch = &batches(&pending)[0];
        assert!(batch.record);
        assert_eq!(batch.label, "create asset rock");
        assert_eq!(
            batch.commands,
            vec![EditorCommand::CreateStandaloneAsset {
                type_name: "pill_master_renderer::Material".to_string(),
                folder: "textures".to_string(),
                name: "rock".to_string(),
            }]
        );
    }

    #[test]
    fn submitting_create_needs_a_name_and_keeps_the_dialog() {
        let mut dialog = Some(AssetDialog::Create {
            folder: String::new(),
            type_name: "pill_master_renderer::Material".to_string(),
            name: "   ".to_string(),
        });
        let mut pending = PendingEditorCommands::default();
        assert!(!submit_create(&mut dialog, &mut pending));
        assert!(dialog.is_some());
        assert!(pending.batches.is_empty());
    }

    #[test]
    fn submitting_rename_builds_the_new_path() {
        let mut pending = PendingEditorCommands::default();
        let mut dialog = Some(AssetDialog::Rename {
            from: "textures/rock.png".to_string(),
            name: "stone".to_string(),
        });
        assert!(submit_rename(&mut dialog, &mut pending));
        assert!(dialog.is_none());
        let batch = &batches(&pending)[0];
        assert_eq!(batch.label, "rename asset stone.png");
        assert_eq!(
            batch.commands,
            vec![EditorCommand::MoveAssetFile {
                from: "textures/rock.png".to_string(),
                to: "textures/stone.png".to_string(),
            }]
        );
    }

    #[test]
    fn rename_keeps_a_typed_extension_and_refuses_separators() {
        let mut pending = PendingEditorCommands::default();
        let mut dialog = Some(AssetDialog::Rename {
            from: "rock.png".to_string(),
            name: "stone.PNG".to_string(),
        });
        assert!(submit_rename(&mut dialog, &mut pending));
        assert_eq!(
            batches(&pending)[0].commands,
            vec![EditorCommand::MoveAssetFile {
                from: "rock.png".to_string(),
                to: "stone.PNG".to_string(),
            }]
        );
        let mut dialog = Some(AssetDialog::Rename {
            from: "rock.png".to_string(),
            name: "a/b".to_string(),
        });
        assert!(!submit_rename(&mut dialog, &mut pending));
        assert_eq!(pending.batches.len(), 1);
        assert!(dialog.is_some());
    }

    #[test]
    fn toolbar_actions_check_the_selection() {
        let assets = vec![
            listing("textures", "folder", false, false),
            listing("textures/rock.png", "texture", false, true),
            listing("sourced.ini", "file", false, false),
        ];
        let mut pending = PendingEditorCommands::default();
        let mut selection = EditorSelection::default();
        // No selection: both refuse.
        assert!(!delete_selection(&mut pending, &selection, &assets));
        assert!(!write_meta_selection(&mut pending, &selection, &assets));
        // A folder refuses the delete.
        selection.asset = Some("textures".to_string());
        assert!(!delete_selection(&mut pending, &selection, &assets));
        // A file deletes; a file with a sidecar refuses the meta write.
        selection.asset = Some("textures/rock.png".to_string());
        assert!(delete_selection(&mut pending, &selection, &assets));
        assert!(!write_meta_selection(&mut pending, &selection, &assets));
        // A sourced file without a sidecar takes the meta write.
        selection.asset = Some("sourced.ini".to_string());
        assert!(write_meta_selection(&mut pending, &selection, &assets));
        let labels: Vec<&str> = pending
            .batches
            .iter()
            .map(|batch| batch.label.as_str())
            .collect();
        assert_eq!(
            labels,
            vec!["delete asset textures/rock.png", "write meta sourced.ini"]
        );
    }

    #[test]
    fn a_drop_destination_refuses_the_same_parent() {
        assert_eq!(
            drop_destination("rock.png", "textures"),
            Some("textures/rock.png".to_string())
        );
        assert_eq!(drop_destination("textures/rock.png", "textures"), None);
        assert_eq!(
            drop_destination("textures/rock.png", "models"),
            Some("models/rock.png".to_string())
        );
    }
}
