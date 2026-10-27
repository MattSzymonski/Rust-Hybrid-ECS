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
//! - The immediate toolbar actions: [`queue_delete`], [`queue_write_meta`] and
//!   [`queue_move`], each a recorded [`EditorCommand`] batch.
//! - [`reveal_target`]: the absolute path a "show in the file manager"
//!   request opens, refused when it would leave the asset directory.
//!
//! # Design
//!
//! The dialogs are egui windows drawn by the shell; the file work is
//! [`EditorCommand`](crate::EditorCommand)'s asset commands, applied by the
//! executable at the frame boundary and recorded in the history. Nothing here
//! touches the engine: the batches land in [`PendingEditorCommands`] and the
//! executable's drain does the rest. A reveal is the one exception: nothing in
//! the world changes, so the shell queues it beside the batches and the
//! executable opens the folder.

// Standard library
use std::path::{Path, PathBuf};

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
        /// What confirming writes.
        kind: CreateKind,
        /// Set once the person confirmed an existing name: `Some(true)` keeps
        /// that file's guid, `Some(false)` writes a fresh one.
        overwrite: Option<bool>,
    },
    /// The rename dialog: the path being renamed and the typed new name.
    Rename {
        /// The asset's current path, relative to the asset directory.
        from: String,
        /// The typed new name; the file keeps its extension.
        name: String,
    },
}

/// What a create dialog writes when it is confirmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateKind {
    /// A standalone asset's default document.
    Asset,
    /// An empty scene: the world is cleared and its empty document written.
    EmptyScene,
    /// The current world, written as a new scene (Save Scene As).
    SaveWorld,
}

/// The selected asset's row, when the selection names one the snapshot
/// still holds.
pub fn selected_row<'a>(
    selection: &EditorSelection,
    assets: &'a [AssetListing],
) -> Option<&'a AssetListing> {
    let key = selection.asset.as_deref()?;
    assets.iter().find(|row| row.key() == key)
}

/// Whether `key` names an extension's resource tree - the extensions root
/// itself, one module's root row, or anything below them.
///
/// An extension's resources are listed and inspected, never written: the
/// toolbar, the grid's strip and the row menus ask this before they offer a
/// mutation. Everything under the root is read-only whatever rows the listing
/// happens to hold, so a module that just loaded or unloaded cannot open a
/// window where a write would be offered.
pub fn is_read_only_key(assets: &[AssetListing], key: &str) -> bool {
    if crate::browser::root_of(key).read_only {
        return true;
    }
    assets
        .iter()
        .filter(|row| row.read_only)
        .map(AssetListing::key)
        .any(|read_only_key| read_only_key == key || key.starts_with(&format!("{read_only_key}/")))
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
        .find(|row| Some(row.key().as_str()) == selection.asset.as_deref())
    {
        Some(row) if row.kind == "folder" => row.key(),
        Some(row) => {
            let key = row.key();
            key.rsplit_once('/')
                .map(|(parent, _)| parent.to_string())
                .unwrap_or_default()
        }
        None => String::new(),
    };
    // Nothing is created inside a module's resources: the dialog would offer a
    // file the drain refuses.
    if is_read_only_key(assets, &folder) {
        pill_core::warn!(
            target: telemetry_target::ECS,
            folder,
            "[editor] create refused: a module's resources are read-only"
        );
        return;
    }
    let type_name = types
        .first()
        .map(|info| info.type_name.clone())
        .unwrap_or_default();
    *dialog = Some(AssetDialog::Create {
        folder: folder.clone(),
        type_name,
        name: String::new(),
        kind: CreateKind::Asset,
        overwrite: None,
    });
    pill_core::info!(
        target: telemetry_target::ECS,
        dialog = "create",
        folder = folder.as_str(),
        "[editor] asset dialog opened"
    );
}

/// Open the create dialog for a scene: the same window a material or a pass is
/// created in, with the scene type chosen and the folder following the
/// selection.
///
/// `kind` says what confirming writes - an empty scene (the Create menu's row)
/// or the current world (Save Scene As) - and nothing touches the world until
/// the step runs, so cancelling leaves the open scene as it was.
pub fn open_create_scene(
    dialog: &mut Option<AssetDialog>,
    selection: &EditorSelection,
    assets: &[AssetListing],
    types: &[StandaloneTypeInfo],
    kind: CreateKind,
) {
    let Some(scene_type) = types
        .iter()
        .find(|info| info.extension.eq_ignore_ascii_case("scene"))
    else {
        pill_core::warn!(
            target: telemetry_target::ECS,
            "[editor] no scene type is registered"
        );
        return;
    };
    open_create(dialog, selection, assets, types);
    if let Some(AssetDialog::Create {
        type_name,
        kind: chosen,
        ..
    }) = dialog.as_mut()
    {
        *type_name = scene_type.type_name.clone();
        *chosen = kind;
    }
    pill_core::info!(
        target: telemetry_target::ECS,
        dialog = "create scene",
        "[editor] asset dialog opened"
    );
}

/// The path a create would write, when the name already exists: the dialog
/// asks before it replaces anything.
///
/// `None` when the type or name is incomplete, or when nothing is in the way.
pub fn create_collision(
    dialog: &AssetDialog,
    types: &[StandaloneTypeInfo],
    assets: &[AssetListing],
) -> Option<String> {
    let AssetDialog::Create {
        folder,
        type_name,
        name,
        ..
    } = dialog
    else {
        return None;
    };
    if name.trim().is_empty() {
        return None;
    }
    let extension = types
        .iter()
        .find(|info| info.type_name == *type_name)
        .map(|info| info.extension.clone())?;
    let file = pill_engine::asset_metadata::standalone_file_name(name, &extension).ok()?;
    let path = if folder.is_empty() {
        file
    } else {
        format!("{folder}/{file}")
    };
    assets
        .iter()
        .any(|row| row.path == path && row.root.is_empty())
        .then_some(path)
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

/// The create dialog's Create button: queue the recorded step and close the
/// dialog; a missing type or name warns and keeps it open.
///
/// What the step is depends on what the dialog was opened for: a standalone
/// asset's default document, an empty scene, or the current world saved as a
/// scene. A scene step only touches the world once it runs, so cancelling the
/// dialog leaves the current scene exactly as it was.
pub fn submit_create(
    dialog: &mut Option<AssetDialog>,
    pending: &mut PendingEditorCommands,
) -> bool {
    let Some(AssetDialog::Create {
        folder,
        type_name,
        name,
        kind,
        overwrite,
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
    let kind = *kind;
    let overwrite = *overwrite;
    if kind == CreateKind::Asset {
        pending.push_recorded(
            vec![EditorCommand::CreateStandaloneAsset {
                type_name: type_name.clone(),
                folder,
                name: name.clone(),
                overwrite,
            }],
            format!("create asset {name}"),
            None,
        );
    } else {
        let path = scene_path(&folder, &name);
        let (commands, label) = match kind {
            CreateKind::EmptyScene => (
                vec![EditorCommand::SceneNew, EditorCommand::SceneSaveAs { path }],
                format!("create scene {name}"),
            ),
            CreateKind::SaveWorld => (
                vec![EditorCommand::SceneSaveAs { path }],
                format!("save scene as {name}"),
            ),
            CreateKind::Asset => unreachable!("handled above"),
        };
        pending.push_recorded(commands, label, None);
    }
    pill_core::info!(
        target: telemetry_target::ECS,
        type_name = type_name.as_str(),
        name = name.as_str(),
        "[editor] asset create queued"
    );
    close(dialog, "create");
    true
}

/// The path a scene create writes: the dialog's folder and the typed name with
/// the scene extension.
fn scene_path(folder: &str, name: &str) -> String {
    let file = crate::scene_workflow::scene_file_name(name);
    if folder.is_empty() {
        file
    } else {
        format!("{folder}/{file}")
    }
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
    if row.read_only {
        pill_core::warn!(
            target: telemetry_target::ECS,
            path = row.key().as_str(),
            "[editor] delete refused: a module's resources are read-only"
        );
        return false;
    }
    queue_delete(pending, &row.key());
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
    if row.read_only {
        pill_core::warn!(
            target: telemetry_target::ECS,
            path = row.key().as_str(),
            "[editor] meta refused: a module's resources are read-only"
        );
        return false;
    }
    queue_write_meta(pending, &row.key());
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

/// The absolute path a reveal opens, for a key that may name an extension's
/// resource.
///
/// A key under the extensions root (`extensions/<module>/<path>`) resolves
/// below that module's own directory, because a module's resources do not live
/// under the project's asset directory at all; every other key resolves the way
/// [`reveal_target`] does. An unknown module is a refusal that names it, since
/// a row for a module that has since unloaded must not open some other tree.
///
/// # Errors
///
/// A message naming the refusal, which the executable logs instead of opening
/// anything.
pub fn reveal_target_in(
    asset_root: &Path,
    module_roots: &[(String, PathBuf)],
    path: &str,
) -> Result<PathBuf, String> {
    let relative = path.trim().replace('\\', "/");
    let extensions_prefix = format!("{}/", crate::editor_state::EXTENSIONS_ROOT);
    let Some(rest) = relative.strip_prefix(extensions_prefix.as_str()) else {
        return reveal_target(asset_root, path);
    };
    let (module, below) = rest.split_once('/').unwrap_or((rest, ""));
    // A module may declare several roots (its `res`, its shader sources, the
    // images it ships); the listing merges them under the module's name, so a
    // reveal looks for the one root that actually holds the file.
    let directories: Vec<&PathBuf> = module_roots
        .iter()
        .filter(|(name, _)| name == module)
        .map(|(_, directory)| directory)
        .collect();
    if directories.is_empty() {
        return Err(format!("`{path}` names no loaded extension"));
    }
    if below.starts_with('/') || Path::new(below).is_absolute() {
        return Err(format!("`{path}` is not a path below the extension's root"));
    }
    if below.split('/').any(|segment| segment == "..") {
        return Err(format!("`{path}` climbs out of the extension's root"));
    }
    if below.is_empty() {
        return Ok(directories[0].clone());
    }
    for directory in directories {
        let target = directory.join(below);
        if target.exists() {
            return Ok(target);
        }
    }
    Err(format!("`{path}` is not on disk"))
}

/// The absolute path a reveal opens, for a project-relative asset path.
///
/// The empty path is the asset root itself - the Assets panel's open folder at
/// the top - and a nested path is joined below it. A path that climbs out of
/// the root, or one that names nothing on disk, is refused with a message
/// rather than opened, so a stale row never makes the OS open something else.
///
/// # Errors
///
/// A message naming the refusal, which the executable logs instead of opening
/// anything.
pub fn reveal_target(asset_root: &Path, path: &str) -> Result<PathBuf, String> {
    let relative = path.trim().replace('\\', "/");
    if relative.starts_with('/') || Path::new(&relative).is_absolute() {
        return Err(format!("`{path}` is not a path below the asset directory"));
    }
    if relative.split('/').any(|segment| segment == "..") {
        return Err(format!("`{path}` climbs out of the asset directory"));
    }
    let target = if relative.is_empty() {
        asset_root.to_path_buf()
    } else {
        asset_root.join(&relative)
    };
    if !target.exists() {
        return Err(format!("`{path}` is not on disk"));
    }
    Ok(target)
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
    use crate::editor_state::EXTENSIONS_ROOT;

    /// A scratch folder of this test's own, with one file in it.
    fn scratch_folder(label: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "pill-reveal-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(directory.join("models")).unwrap();
        std::fs::write(directory.join("models/cube.obj"), b"o cube").unwrap();
        directory
    }

    /// A reveal resolves below the asset root, refuses leaving it, and
    /// reports a path that is not on disk instead of opening its parent.
    #[test]
    fn a_reveal_resolves_below_the_asset_root_and_refuses_escapes() {
        let root = scratch_folder("targets");
        // The empty path is the root itself: the panel's open folder at the top.
        assert_eq!(reveal_target(&root, "").unwrap(), root);
        // A nested file and folder both resolve.
        assert_eq!(
            reveal_target(&root, "models/cube.obj").unwrap(),
            root.join("models/cube.obj")
        );
        assert_eq!(reveal_target(&root, "models").unwrap(), root.join("models"));
        // Backslashes read as separators, the way the panels may hand them over.
        assert_eq!(
            reveal_target(&root, "models\\cube.obj").unwrap(),
            root.join("models/cube.obj")
        );
        // Escapes, absolute paths and missing files are all refusals.
        assert!(reveal_target(&root, "../outside").is_err());
        assert!(reveal_target(&root, "models/../../outside").is_err());
        assert!(reveal_target(&root, "/etc").is_err());
        assert!(reveal_target(&root, "models/missing.obj").is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    /// A reveal of an extension's resource resolves below the module root that
    /// holds it - a module may declare several - and an unloaded module is
    /// refused by name.
    #[test]
    fn an_extension_reveal_resolves_below_its_module_root() {
        let project = scratch_folder("project");
        let first = scratch_folder("extension_res");
        let second = scratch_folder("extension_sources");
        std::fs::create_dir_all(second.join("shaders")).unwrap();
        std::fs::write(second.join("shaders/a.hlsl"), b"// shader").unwrap();
        let roots = vec![
            ("pill_demo".to_string(), first.clone()),
            ("pill_demo".to_string(), second.clone()),
        ];

        // The second root of the module is the one that holds the shader.
        assert_eq!(
            reveal_target_in(&project, &roots, "extensions/pill_demo/shaders/a.hlsl").unwrap(),
            second.join("shaders/a.hlsl")
        );
        // A file below the first root resolves there instead.
        assert_eq!(
            reveal_target_in(&project, &roots, "extensions/pill_demo/models/cube.obj").unwrap(),
            first.join("models/cube.obj")
        );
        // The module's own row opens its first root.
        assert_eq!(
            reveal_target_in(&project, &roots, "extensions/pill_demo").unwrap(),
            first
        );
        // A project path still resolves below the project's asset directory.
        assert_eq!(
            reveal_target_in(&project, &roots, "models/cube.obj").unwrap(),
            project.join("models/cube.obj")
        );
        // An unloaded module, an escape and a missing file are refusals.
        let unloaded = reveal_target_in(&project, &roots, "extensions/pill_gone/a.hlsl");
        assert!(unloaded.is_err());
        assert!(reveal_target_in(&project, &roots, "extensions/pill_demo/../../outside").is_err());
        assert!(reveal_target_in(&project, &roots, "extensions/pill_demo/missing.hlsl").is_err());
        std::fs::remove_dir_all(&project).ok();
        std::fs::remove_dir_all(&first).ok();
        std::fs::remove_dir_all(&second).ok();
    }

    fn listing(path: &str, kind: &str, standalone: bool, has_meta: bool) -> AssetListing {
        AssetListing {
            root: String::new(),
            path: path.to_string(),
            read_only: false,
            kind: kind.to_string(),
            standalone,
            has_meta,
            guid: None,
        }
    }

    /// A row of a module's read-only resource tree.
    fn module_row(root: &str, path: &str, kind: &str) -> AssetListing {
        AssetListing {
            root: root.to_string(),
            path: path.to_string(),
            read_only: true,
            kind: kind.to_string(),
            standalone: kind == "pill_master_renderer::Material",
            has_meta: kind == "pill_master_renderer::Material",
            guid: None,
        }
    }

    /// A module's resource tree is recognised by the extensions root and every
    /// key below it; a project key never is.
    #[test]
    fn a_module_tree_is_read_only() {
        let rows = vec![
            module_row("pill_master_renderer_data", "", "folder"),
            module_row("pill_master_renderer_data", "defaults", "folder"),
            module_row(
                "pill_master_renderer_data",
                "defaults/pill.pbr.material",
                "pill_master_renderer::Material",
            ),
            listing("textures/rock.png", "Texture", false, true),
            listing("pill_master_renderer_data", "folder", false, false),
        ];
        assert!(is_read_only_key(&rows, EXTENSIONS_ROOT));
        assert!(is_read_only_key(
            &rows,
            "extensions/pill_master_renderer_data"
        ));
        assert!(is_read_only_key(
            &rows,
            "extensions/pill_master_renderer_data/defaults"
        ));
        assert!(is_read_only_key(
            &rows,
            "extensions/pill_master_renderer_data/defaults/pill.pbr.material"
        ));
        // A project folder of the same name is its own key, not the module's.
        assert!(!is_read_only_key(&rows, "textures/rock.png"));
        assert!(!is_read_only_key(&rows, "textures"));
        assert!(!is_read_only_key(&rows, "pill_master_renderer_data"));
    }

    /// A module's resource is found by its key, and neither the toolbar's
    /// delete nor its meta write queues anything for it.
    #[test]
    fn a_module_resource_is_never_written() {
        let rows = vec![module_row(
            "pill_master_renderer_data",
            "defaults/pill.pbr.material",
            "pill_master_renderer::Material",
        )];
        let mut selection = EditorSelection::default();
        selection.asset =
            Some("extensions/pill_master_renderer_data/defaults/pill.pbr.material".to_string());
        assert!(selected_row(&selection, &rows).is_some(), "found by key");

        let mut pending = PendingEditorCommands::default();
        assert!(!delete_selection(&mut pending, &selection, &rows));
        assert!(
            pending.batches.is_empty(),
            "a refused delete queues nothing"
        );
        assert!(!write_meta_selection(&mut pending, &selection, &rows));
        assert!(pending.batches.is_empty());
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
                kind: CreateKind::Asset,
                overwrite: None,
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
            kind: CreateKind::Asset,
            overwrite: None,
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
                overwrite: None,
            }]
        );
    }

    /// A scene create is one recorded step that clears the world and writes it,
    /// and a save-as writes the world where the dialog said.
    #[test]
    fn submitting_a_scene_create_writes_the_chosen_path() {
        let mut dialog = Some(AssetDialog::Create {
            folder: "scenes".to_string(),
            type_name: "pill_engine::Scene".to_string(),
            name: "level two".to_string(),
            kind: CreateKind::EmptyScene,
            overwrite: None,
        });
        let mut pending = PendingEditorCommands::default();
        assert!(submit_create(&mut dialog, &mut pending));
        let batch = &batches(&pending)[0];
        assert_eq!(batch.label, "create scene level two");
        assert_eq!(
            batch.commands,
            vec![
                EditorCommand::SceneNew,
                EditorCommand::SceneSaveAs {
                    path: "scenes/level two.scene".to_string()
                },
            ]
        );

        let mut dialog = Some(AssetDialog::Create {
            folder: String::new(),
            type_name: "pill_engine::Scene".to_string(),
            name: "copy".to_string(),
            kind: CreateKind::SaveWorld,
            overwrite: None,
        });
        let mut pending = PendingEditorCommands::default();
        assert!(submit_create(&mut dialog, &mut pending));
        assert_eq!(
            batches(&pending)[0].commands,
            vec![EditorCommand::SceneSaveAs {
                path: "copy.scene".to_string()
            }]
        );
    }

    #[test]
    fn submitting_create_needs_a_name_and_keeps_the_dialog() {
        let mut dialog = Some(AssetDialog::Create {
            folder: String::new(),
            type_name: "pill_master_renderer::Material".to_string(),
            name: "   ".to_string(),
            kind: CreateKind::Asset,
            overwrite: None,
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
