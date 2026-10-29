//! The asset family's applier.
//!
//! The asset-file commands: import, remove, create, delete, restore, move,
//! rename and settings - the family that touches the filesystem and the asset
//! store, with its own rollback rules.
//!
//! # Responsibilities
//!
//! - Apply the asset-file commands: import, remove, create, delete, restore,
//!   move, rename, write a sidecar and change a file's settings.
//! - Take a failed command's own work back, and name the files a rollback
//!   could not remove.
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

/// Apply one asset command.
pub(super) fn apply(
    engine: &mut Engine,
    command: &EditorCommand,
) -> Result<CommandOutcome, String> {
    match command {
        EditorCommand::ImportFiles {
            sources,
            destination,
        } => {
            // Validate every source before copying anything, so one bad
            // path in a multi-file drop changes nothing.
            for source in sources {
                if !std::path::Path::new(source).is_file() {
                    return Err(format!("{command:?}: `{source}` is not a file"));
                }
            }
            std::fs::create_dir_all(destination)
                .map_err(|error| format!("{command:?}: {error}"))?;
            let mut created: Vec<String> = Vec::new();
            for source in sources {
                let file_name = std::path::Path::new(source)
                    .file_name()
                    .ok_or_else(|| format!("{command:?}: `{source}` has no file name"))?;
                let target = std::path::Path::new(destination).join(file_name);
                // A source that already is the destination file - a drop
                // of something the project already has - is nothing to
                // copy; copying a file onto itself would at best fail and
                // at worst truncate it.
                let source_path = std::path::Path::new(source);
                let same = match (source_path.canonicalize(), target.canonicalize()) {
                    (Ok(source), Ok(target)) => source == target,
                    _ => source_path == target,
                };
                if same {
                    continue;
                }
                if let Err(error) = std::fs::copy(source, &target) {
                    // The command's own copies go back; a removal that fails
                    // names the file it left, so the failure is the whole
                    // story rather than half of it.
                    let left_behind = roll_back_files(&created);
                    return Err(with_rollback_paths(
                        format!("{command:?}: {error}"),
                        &left_behind,
                    ));
                }
                created.push(target.to_string_lossy().into_owned());
            }
            if created.is_empty() {
                // Every source was already in place: nothing changed, and
                // nothing is recorded.
                return Ok(CommandOutcome::plain());
            }
            Ok(CommandOutcome::inverse(EditorCommand::RemoveFiles {
                paths: created,
                sources: sources.clone(),
                destination: destination.clone(),
            }))
        }

        EditorCommand::RemoveFiles {
            paths,
            sources,
            destination,
        } => {
            for path in paths {
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    // Already gone is the state undo wanted anyway.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(format!("{command:?}: {error}")),
                }
            }
            Ok(CommandOutcome::inverse(EditorCommand::ImportFiles {
                sources: sources.clone(),
                destination: destination.clone(),
            }))
        }

        EditorCommand::CreateStandaloneAsset {
            type_name,
            folder,
            name,
            overwrite,
        } => {
            let root = asset_authoring_root()?;
            let registry = engine
                .world()
                .get_resource::<pill_engine::ImportRegistry>()
                .cloned()
                .ok_or_else(|| format!("{command:?}: no asset types are registered"))?;
            // The store is taken before the file is written: an engine
            // without one must refuse the create, not leave a file no
            // asset manager will ever hold.
            let assets = engine
                .world_mut()
                .get_resource_mut::<pill_engine::AssetManager>()
                .ok_or_else(|| format!("{command:?}: the engine has no AssetManager"))?;
            let extension = registry
                .standalone_types()
                .into_iter()
                .find(|(registered, _)| registered == type_name)
                .map(|(_, extension)| extension)
                .ok_or_else(|| {
                    format!("{command:?}: `{type_name}` is not a standalone asset type")
                })?;
            let folder = folder.trim().trim_matches('/');
            let relative_folder = if folder.is_empty() {
                PathBuf::new()
            } else {
                pill_engine::asset_metadata::checked_relative(folder)
                    .map_err(|message| format!("{command:?}: {message}"))?
            };
            let directory = root.join(&relative_folder);
            if !directory.is_dir() {
                // A project whose asset directory does not exist yet gets
                // its root made on demand; a named folder must exist, the
                // same refusal the host's browser states.
                if relative_folder.as_os_str().is_empty() {
                    std::fs::create_dir_all(&directory)
                        .map_err(|error| format!("{command:?}: {error}"))?;
                } else {
                    return Err(format!(
                        "{command:?}: the folder `{}` does not exist",
                        asset_slash(&relative_folder)
                    ));
                }
            }
            let file_name = pill_engine::asset_metadata::standalone_file_name(name, &extension)
                .map_err(|message| format!("{command:?}: {message}"))?;
            let relative = relative_folder.join(&file_name);
            refuse_module_resource(command, &relative)?;
            let destination = root.join(&relative);
            // An existing file is replaced only when the dialog said so,
            // and the dialog says which guid survives.
            let existing = destination.exists();
            if existing && overwrite.is_none() {
                return Err(format!(
                    "{command:?}: `{}` already exists",
                    asset_slash(&relative)
                ));
            }
            // What the replace overwrites, so a write that fails to load
            // can be taken back byte for byte.
            let previous = if existing {
                Some(std::fs::read(&destination).map_err(|error| {
                    format!(
                        "{command:?}: cannot read `{}`: {error}",
                        asset_slash(&relative)
                    )
                })?)
            } else {
                None
            };
            // The store's identity comes first: a path bound to a guid is
            // what every reference names, so a file whose own header
            // disagrees with it cannot take its place.
            let loaded_guid = registry.guid_by_path(assets, &relative);
            let kept_guid = match overwrite {
                Some(true) => loaded_guid.or_else(|| previous.as_deref().and_then(document_guid)),
                _ => None,
            };
            let document = registry
                .default_document(type_name)
                .map_err(|error| format!("{command:?}: {error}"))?;
            let guid = match kept_guid {
                Some(guid) => guid,
                None => pill_engine::AssetGuid::random().map_err(|error| {
                    format!("{command:?}: no random source for a guid: {error}")
                })?,
            };
            // A stale asset under this path would refuse the import, and
            // the file is the truth now: a fresh guid unloads it, a kept
            // one reloads it from the file written below.
            let reloads_loaded = loaded_guid == Some(guid);
            if loaded_guid.is_some() && !reloads_loaded {
                registry.remove_loaded(assets, &relative);
            }
            let file = serde_json::json!({
                "format_version": pill_engine::asset_metadata::METADATA_FORMAT_VERSION,
                "asset_type": type_name,
                "guid": guid.to_string(),
                "asset": document,
            });
            let mut bytes = serde_json::to_vec_pretty(&file)
                .map_err(|error| format!("{command:?}: {error}"))?;
            bytes.push(b'\n');
            let sidecar = pill_engine::asset_metadata::metadata_path_for(&relative);
            let sidecar_existed = sidecar
                .as_ref()
                .is_some_and(|meta| root.join(meta).exists());
            if existing {
                pill_engine::asset_store::write_replacing(&destination, &bytes)
                    .map_err(|error| format!("{command:?}: {error}"))?;
            } else {
                pill_engine::asset_store::write_new(&destination, &bytes)
                    .map_err(|error| format!("{command:?}: {error}"))?;
            }
            // An asset that was loaded keeps its handle, so the file is
            // read again through the value - an import would answer with
            // the handle it already has and decode nothing.
            let loaded = if reloads_loaded {
                registry
                    .reimport(assets, &relative)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            } else {
                registry
                    .import(
                        assets,
                        &relative,
                        pill_engine::MetadataPolicy::ReadIfPresent,
                    )
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            };
            if let Err(error) = loaded {
                // The file is only an asset once it loads, so a refusal
                // takes the write back rather than leaving a file the
                // project cannot read: a replaced file gets its previous
                // bytes again, a new one goes.
                let mut left_behind = Vec::new();
                match previous {
                    Some(previous) => {
                        let restored =
                            pill_engine::asset_store::write_replacing(&destination, &previous);
                        if restored.is_err() {
                            left_behind.push(destination.display().to_string());
                        }
                    }
                    None => {
                        let destination = destination.display().to_string();
                        left_behind.extend(roll_back_files(&[destination]));
                        if let Some(meta) = sidecar.filter(|_| !sidecar_existed) {
                            let meta = root.join(meta).display().to_string();
                            left_behind.extend(roll_back_files(&[meta]));
                        }
                    }
                }
                return Err(with_rollback_paths(
                    format!(
                        "{command:?}: `{}` did not load: {error}",
                        asset_slash(&relative)
                    ),
                    &left_behind,
                ));
            }
            Ok(CommandOutcome::inverse(EditorCommand::DeleteAssetFile {
                path: asset_slash(&relative),
            }))
        }

        EditorCommand::DeleteAssetFile { path } => {
            let root = asset_authoring_root()?;
            let relative = pill_engine::asset_metadata::checked_relative(path)
                .map_err(|message| format!("{command:?}: {message}"))?;
            refuse_module_resource(command, &relative)?;
            let file = root.join(&relative);
            let bytes = std::fs::read(&file)
                .map_err(|error| format!("{command:?}: cannot read `{path}`: {error}"))?;
            let meta_relative = pill_engine::asset_metadata::metadata_path_for(&relative);
            let meta = match &meta_relative {
                Some(meta) => match std::fs::read(root.join(meta)) {
                    Ok(bytes) => Some(bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => return Err(format!("{command:?}: {error}")),
                },
                None => None,
            };
            std::fs::remove_file(&file)
                .map_err(|error| format!("{command:?}: cannot delete `{path}`: {error}"))?;
            if let Some(meta) = meta_relative {
                match std::fs::remove_file(root.join(&meta)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        // The asset file is already gone; put it back, so a
                        // delete that cannot finish leaves nothing behind.
                        return Err(match write_asset_bytes(&file, &bytes) {
                            Ok(()) => format!(
                                "{command:?}: cannot delete `{path}`'s sidecar, so nothing was deleted: {error}"
                            ),
                            Err(restore) => format!(
                                "{command:?}: cannot delete `{path}`'s sidecar ({error}) and cannot restore the asset: {restore}"
                            ),
                        });
                    }
                }
            }
            // The file is gone, so the asset it held must go with it: a
            // re-created path would otherwise collide with the loaded
            // value on the guid the create writes.
            if let Some(registry) = engine
                .world()
                .get_resource::<pill_engine::ImportRegistry>()
                .cloned()
            {
                if let Some(assets) = engine
                    .world_mut()
                    .get_resource_mut::<pill_engine::AssetManager>()
                {
                    registry.remove_loaded(&mut *assets, &relative);
                }
            }
            Ok(CommandOutcome::inverse(EditorCommand::RestoreAssetFile {
                path: path.clone(),
                bytes,
                meta,
            }))
        }

        EditorCommand::RestoreAssetFile { path, bytes, meta } => {
            let root = asset_authoring_root()?;
            let relative = pill_engine::asset_metadata::checked_relative(path)
                .map_err(|message| format!("{command:?}: {message}"))?;
            refuse_module_resource(command, &relative)?;
            write_asset_bytes(&root.join(&relative), bytes)
                .map_err(|message| format!("{command:?}: {message}"))?;
            if let Some(meta_relative) = pill_engine::asset_metadata::metadata_path_for(&relative) {
                let meta_path = root.join(&meta_relative);
                match meta {
                    Some(meta) => write_asset_bytes(&meta_path, meta)
                        .map_err(|message| format!("{command:?}: {message}"))?,
                    // A file that had no sidecar restores without one;
                    // anything sitting there now is an orphan, and a
                    // removal that fails says which one.
                    None => {
                        let meta = meta_path.display().to_string();
                        warn_left_behind(&roll_back_files(&[meta]));
                    }
                }
            }
            Ok(CommandOutcome::inverse(EditorCommand::DeleteAssetFile {
                path: path.clone(),
            }))
        }

        EditorCommand::MoveAssetFile { from, to } => {
            let root = asset_authoring_root()?;
            let from_relative = pill_engine::asset_metadata::checked_relative(from)
                .map_err(|message| format!("{command:?}: {message}"))?;
            let to_relative = pill_engine::asset_metadata::checked_relative(to)
                .map_err(|message| format!("{command:?}: {message}"))?;
            refuse_module_resource(command, &from_relative)?;
            refuse_module_resource(command, &to_relative)?;
            let destination = root.join(&to_relative);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).map_err(|error| format!("{command:?}: {error}"))?;
            }
            if destination.exists() {
                return Err(format!(
                    "{command:?}: `{}` already exists",
                    asset_slash(&to_relative)
                ));
            }
            pill_assets::move_asset(&root.join(&from_relative), &destination)
                .map_err(|error| format!("{command:?}: {error}"))?;
            Ok(CommandOutcome::inverse(EditorCommand::MoveAssetFile {
                from: to.clone(),
                to: from.clone(),
            }))
        }

        EditorCommand::WriteAssetMeta { path } => {
            let root = asset_authoring_root()?;
            let relative = pill_engine::asset_metadata::checked_relative(path)
                .map_err(|message| format!("{command:?}: {message}"))?;
            refuse_module_resource(command, &relative)?;
            let Some(meta_relative) = pill_engine::asset_metadata::metadata_path_for(&relative)
            else {
                return Err(format!("{command:?}: `{path}` cannot have a sidecar"));
            };
            let meta_path = root.join(&meta_relative);
            if meta_path.is_file() {
                // A sidecar that carries a guid is not missing: nothing
                // changes, nothing is recorded. One left empty or
                // unreadable by an interrupted write counts as missing
                // and is replaced; the undo removes the replacement.
                let usable = std::fs::read_to_string(&meta_path)
                    .ok()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                    .and_then(|document| {
                        document
                            .get("guid")
                            .and_then(|guid| guid.as_str())
                            .map(str::to_owned)
                    })
                    .is_some();
                if usable {
                    return Ok(CommandOutcome::plain());
                }
                std::fs::remove_file(&meta_path)
                    .map_err(|error| format!("{command:?}: {error}"))?;
            }
            let registry = engine
                .world()
                .get_resource::<pill_engine::ImportRegistry>()
                .cloned()
                .ok_or_else(|| format!("{command:?}: no asset types are registered"))?;
            let assets = engine
                .world_mut()
                .get_resource_mut::<pill_engine::AssetManager>()
                .ok_or_else(|| format!("{command:?}: the engine has no AssetManager"))?;
            registry
                .ensure_metadata(assets, &relative)
                .map_err(|error| format!("{command:?}: {error}"))?;
            Ok(CommandOutcome::inverse(EditorCommand::DeleteAssetFile {
                path: asset_slash(&meta_relative),
            }))
        }

        EditorCommand::SetAssetSettings { path, settings } => {
            let root = asset_authoring_root()?;
            let relative = pill_engine::asset_metadata::checked_relative(path)
                .map_err(|message| format!("{command:?}: {message}"))?;
            refuse_module_resource(command, &relative)?;
            let registry = engine
                .world()
                .get_resource::<pill_engine::ImportRegistry>()
                .cloned()
                .ok_or_else(|| format!("{command:?}: no asset types are registered"))?;
            let extension = relative
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or_default()
                .to_owned();
            let type_name = registry
                .type_for_extension(&extension)
                .ok_or_else(|| format!("{command:?}: no asset type imports `.{extension}` files"))?
                .to_owned();
            let normalized = registry
                .normalize_settings_json(&type_name, settings.clone())
                .map_err(|error| format!("{command:?}: {error}"))?;
            let previous = registry
                .settings_json(&relative)
                .map_err(|error| format!("{command:?}: {error}"))?;
            let standalone = registry.is_standalone_extension(&extension);
            let body_field = if standalone { "asset" } else { "settings" };
            let file = if standalone {
                root.join(&relative)
            } else {
                let meta_relative = pill_engine::asset_metadata::metadata_path_for(&relative)
                    .ok_or_else(|| format!("{command:?}: `{path}` cannot have a sidecar"))?;
                let file = root.join(&meta_relative);
                if !file.is_file() {
                    // The import writes the sidecar with the type's
                    // defaults and a guid, which the write below then
                    // replaces.
                    let assets = engine
                        .world_mut()
                        .get_resource_mut::<pill_engine::AssetManager>()
                        .ok_or_else(|| format!("{command:?}: the engine has no AssetManager"))?;
                    registry
                        .import(
                            assets,
                            &relative,
                            pill_engine::MetadataPolicy::CreateIfMissing,
                        )
                        .map_err(|error| format!("{command:?}: {error}"))?;
                }
                file
            };
            let text = std::fs::read_to_string(&file).map_err(|error| {
                format!("{command:?}: cannot read `{}`: {error}", file.display())
            })?;
            let mut document: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
                format!(
                    "{command:?}: `{}` is not valid JSON: {error}",
                    file.display()
                )
            })?;
            let object = document
                .as_object_mut()
                .ok_or_else(|| format!("{command:?}: `{}` is not a JSON object", file.display()))?;
            object.insert(body_field.to_owned(), normalized);
            let mut bytes = serde_json::to_vec_pretty(&document)
                .map_err(|error| format!("{command:?}: {error}"))?;
            bytes.push(b'\n');
            write_asset_bytes(&file, &bytes)
                .map_err(|message| format!("{command:?}: {message}"))?;
            Ok(CommandOutcome::inverse(EditorCommand::SetAssetSettings {
                path: path.clone(),
                settings: previous,
            }))
        }

        other => unreachable!("{:?} is not an asset command", other.family()),
    }
}
