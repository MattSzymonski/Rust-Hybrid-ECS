//! The executable's half of the Assets pipeline.
//!
//! # Responsibilities
//!
//! - Walk the asset tree into the listing the shell draws, and refresh it on
//!   its cadence or at once after a file command.
//! - Capture the selected asset's detail - its import settings and the
//!   reference fields its type declares - for the Inspector.
//! - Ask the renderer for the previews of the tiles that are on screen, and
//!   answer a drop with the folder or files it names.
//!
//! # Design
//!
//! The shell never touches the filesystem: it queues file commands and draws
//! what the snapshot carries. This module is the side that walks the tree,
//! reads a file's settings and keeps the listing honest, so a read failure
//! leaves the previous listing rather than an empty one.

// Current crate
use super::*;

impl EditorWindow {
    /// Move the previews the renderer finished into the shell's textures, then
    /// ask for the next tile's.
    ///
    /// One request at a time: the renderer draws one preview per frame and
    /// reads it back between frames, so a grid of tiles fills in over frames
    /// rather than stalling one. A tile only asks once - its image is kept in
    /// the session until the listing changes - and the queue is the grid's own
    /// list of what it drew, so scrolling costs work only for what is visible.
    pub(super) fn apply_asset_previews(&mut self) {
        let images = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource_mut::<pill_runtime::PreviewResults>()
                .map(|results| results.take())
                .unwrap_or_default()
        };
        if !images.is_empty() {
            let world = self.host.host_mut().engine_mut().world_mut();
            let Some(state) = world.get_resource_mut::<EguiShellState>() else {
                return;
            };
            for image in images {
                let Some(path) = self.preview_in_flight.remove(&image.id) else {
                    continue;
                };
                // A refusal carries no pixels: the tile keeps its type icon
                // and the grid moves on, rather than waiting on this file
                // forever.
                if image.is_empty() {
                    state.previews.mark_refused(&path);
                    info!(
                        target: telemetry_target::ENGINE,
                        path = path.as_str(),
                        id = image.id,
                        "[editor] preview refused"
                    );
                    continue;
                }
                let size = [image.width as usize, image.height as usize];
                let texture = state.context.load_texture(
                    format!("preview:{path}"),
                    egui::ColorImage::from_rgba_unmultiplied(size, &image.rgba),
                    egui::TextureOptions::LINEAR,
                );
                state.previews.mark_ready(&path, texture);
                info!(
                    target: telemetry_target::ENGINE,
                    path = path.as_str(),
                    width = image.width,
                    height = image.height,
                    "[editor] preview ready"
                );
            }
        }
        if !self.render_previews || !self.preview_in_flight.is_empty() {
            return;
        }
        // Every visible tile still waiting for an image, in draw order.
        let waiting: Vec<String> = {
            let world = self.host.host_mut().engine_mut().world_mut();
            let Some(state) = world.get_resource::<EguiShellState>() else {
                return;
            };
            state
                .previews
                .visible()
                .iter()
                .filter(|key| state.previews.wants(key))
                .cloned()
                .collect()
        };
        if waiting.is_empty() {
            return;
        }
        // A texture preview names its file, so the request needs the asset
        // root the listing's paths are relative to.
        let root = self
            .host
            .host_mut()
            .asset_directory()
            .map(std::path::Path::to_path_buf);
        let Some(root) = root else {
            return;
        };
        // The first tile the listing can name a preview for. A row with none -
        // a folder, a scene, a file type the renderer cannot draw - is
        // recorded as such, because a tile that is never answered would block
        // every tile drawn after it: the grid lists folders first, so one
        // folder used to starve a whole folder of files.
        let mut skipped = Vec::new();
        let mut next = None;
        for key in waiting {
            match previews::preview_asset_for(self.asset_entries.iter(), &key, &root) {
                Some(asset) => {
                    next = Some((key, asset));
                    break;
                }
                None => skipped.push(key),
            }
        }
        if !skipped.is_empty() {
            let world = self.host.host_mut().engine_mut().world_mut();
            if let Some(state) = world.get_resource_mut::<EguiShellState>() {
                for key in &skipped {
                    state.previews.mark_refused(key);
                }
            }
            for key in &skipped {
                info!(
                    target: telemetry_target::ENGINE,
                    path = key.as_str(),
                    "[editor] preview skipped"
                );
            }
        }
        let Some((path, asset)) = next else {
            return;
        };
        let id = {
            let world = self.host.host_mut().engine_mut().world_mut();
            pill_runtime::ensure_preview_resources(world);
            let Some(requests) = world.get_resource_mut::<pill_runtime::PreviewRequests>() else {
                return;
            };
            requests.request(asset)
        };
        self.preview_in_flight.insert(id, path.clone());
        info!(
            target: telemetry_target::ENGINE,
            path = path.as_str(),
            id,
            "[editor] preview requested"
        );
    }

    /// Refresh the asset browser's rows when the cadence is due or a file
    /// command marked them dirty, and pick up the live standalone types the
    /// create dialog offers. Returns whether the walk ran, so the asset detail
    /// follows it.
    pub(super) fn refresh_asset_listing(&mut self) -> bool {
        if !(self.frame_index.is_multiple_of(ASSET_REFRESH_FRAMES) || self.asset_listing_dirty) {
            return false;
        }
        // An edit asked for this pass, not the cadence: settings or a file
        // changed, so the tiles' images and the Inspector's capture both have
        // to follow.
        let edited = self.asset_listing_dirty;
        self.asset_listing_dirty = false;
        let entries: Vec<AssetListing> = self
            .host
            .host_mut()
            .asset_entries()
            .into_iter()
            .take(ASSET_ROWS_MAX)
            .map(|entry| AssetListing {
                root: entry.root,
                path: entry.path,
                read_only: entry.read_only,
                kind: if entry.is_directory {
                    AssetKind::Folder
                } else {
                    AssetKind::parse(&entry.type_name.unwrap_or_else(|| "file".to_string()))
                },
                standalone: entry.standalone,
                has_meta: entry.guid.is_some(),
                guid: entry.guid.map(|guid| guid.to_string()),
            })
            .collect();
        // The walk runs on a cadence, so most of its passes find the tree
        // exactly as it was. Only a moved tree - or an edit - makes a tile's
        // image stale; clearing the previews on every pass is what made them
        // blink back to their icons every couple of seconds.
        let tree_moved = entries != self.asset_walk;
        // A tree that moved keeps the images of the rows that did not: a
        // create or a delete changes one asset, and re-rendering the folder
        // for it is the same flicker in a smaller place. An edit that left the
        // tree alone names no asset, so every image may be stale.
        let unchanged: std::collections::BTreeSet<String> = if tree_moved {
            entries
                .iter()
                .filter(|row| {
                    self.asset_walk
                        .iter()
                        .any(|old| old.key() == row.key() && *old == **row)
                })
                .map(AssetListing::key)
                .collect()
        } else {
            std::collections::BTreeSet::new()
        };
        self.asset_walk = entries.clone();
        self.asset_entries = entries;
        if tree_moved || edited {
            let world = self.host.host_mut().engine_mut().world_mut();
            if let Some(state) = world.get_resource_mut::<EguiShellState>() {
                if tree_moved {
                    state.previews.retain_keys(&unchanged);
                } else {
                    self.preview_in_flight.clear();
                    state.previews.clear();
                }
            }
        }
        // Every module root needs its own row under the extensions root: the
        // walk lists what is inside a root, and the tree can only open a folder
        // it has a row for. The root itself is not a row - the panel draws it.
        let roots: std::collections::BTreeSet<String> = self
            .asset_entries
            .iter()
            .map(|row| row.root.clone())
            .filter(|root| !root.is_empty())
            .collect();
        for root in roots.into_iter().rev() {
            self.asset_entries.insert(
                0,
                AssetListing {
                    root,
                    path: String::new(),
                    read_only: true,
                    kind: AssetKind::Folder,
                    standalone: false,
                    has_meta: false,
                    guid: None,
                },
            );
        }
        let types = pill_host::standalone_types(self.host.host_mut().engine_mut().world());
        self.standalone_types = types
            .into_iter()
            .map(|info| StandaloneTypeInfo {
                type_name: info.type_name,
                display_name: info.display_name,
                extension: info.extension,
            })
            .collect();
        let changed = tree_moved || edited;
        if changed {
            info!(
                target: telemetry_target::ENGINE,
                count = self.asset_entries.len(),
                types = self.standalone_types.len(),
                "[editor] asset listing refreshed"
            );
        }
        changed
    }

    /// Capture the selected asset's detail for the Inspector's asset
    /// section, when the selection or the tree changed: reading the settings
    /// document is a disk read, so the capture follows the listing's cadence
    /// instead of running every frame. A settings write forgets the captured
    /// path, so the widgets see the document the command wrote.
    pub(super) fn refresh_asset_detail(&mut self, listing_refreshed: bool) {
        let selected = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource::<EditorSelection>()
                .and_then(|selection| selection.asset.clone())
        };
        if !listing_refreshed && selected == self.asset_detail_path {
            return;
        }
        self.asset_detail_path = selected.clone();
        self.asset_detail = selected.map(|key| {
            // A selection the listing no longer holds - an asset deleted
            // between two refreshes - reads as a file-shaped row at its own
            // path, which is what the Inspector drew before the detail moved.
            let row = self
                .asset_entries
                .iter()
                .find(|row| row.key() == key)
                .cloned()
                .unwrap_or_else(|| AssetListing {
                    root: String::new(),
                    path: key.clone(),
                    read_only: false,
                    kind: AssetKind::Type("file".to_string()),
                    standalone: false,
                    has_meta: false,
                    guid: None,
                });
            // The settings document is read by the path inside the root: the
            // store searches the module roots, so a module resource reads
            // like a project asset.
            let settings = if row.kind.is_folder() {
                None
            } else {
                self.read_asset_settings(&row.path)
            };
            let references = self.asset_reference_fields(row.kind.text(), &settings);
            AssetDetail {
                row,
                settings,
                references,
            }
        });
    }

    /// The reference fields the asset's type declares, expanded against its
    /// document: one entry per picker the Inspector draws, with the guid the
    /// document holds now.
    ///
    /// The registry names the fields and the type each points at - `shader`,
    /// `textures.*`, `passes.*` - and the families are expanded here, because
    /// only the document knows how many entries a family has: a material's
    /// texture slots come from the shader beside it, a chain's entries from its
    /// own list.
    pub(super) fn asset_reference_fields(
        &mut self,
        kind: &str,
        settings: &Option<serde_json::Value>,
    ) -> Vec<AssetReferenceField> {
        let Some(settings) = settings.as_ref() else {
            return Vec::new();
        };
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(registry) = world
            .get_resource::<pill_engine_core::ImportRegistry>()
            .cloned()
        else {
            return Vec::new();
        };
        let declared = registry.reference_fields(kind).to_vec();
        let mut fields = Vec::new();
        for (key, target) in declared {
            let Some(family) = key.strip_suffix(".*") else {
                let value = settings.get(&key);
                let guid = reference_guid(value);
                fields.push(AssetReferenceField {
                    key,
                    target,
                    label: self.asset_label(&guid),
                    guid,
                    nested: !matches!(value, Some(serde_json::Value::String(_))),
                });
                continue;
            };
            match family {
                "textures" => {
                    for slot in self.shader_texture_slots(&Some(settings.clone())) {
                        let value = settings.get("textures").and_then(|map| map.get(&slot));
                        let guid = reference_guid(value);
                        fields.push(AssetReferenceField {
                            key: format!("textures.{slot}"),
                            target: target.clone(),
                            label: self.asset_label(&guid),
                            guid,
                            nested: false,
                        });
                    }
                }
                "passes" => {
                    let entries = settings
                        .get(family)
                        .and_then(|list| list.as_array())
                        .cloned()
                        .unwrap_or_default();
                    for (index, entry) in entries.iter().enumerate() {
                        let guid = reference_guid(Some(entry));
                        fields.push(AssetReferenceField {
                            key: format!("passes[{index}]"),
                            target: target.clone(),
                            label: self.asset_label(&guid),
                            guid,
                            nested: true,
                        });
                    }
                }
                other => debug_assert!(false, "unexpanded reference family `{other}`"),
            }
        }
        fields
    }

    /// What a reference row reads for the asset `guid` names: the listing's
    /// path when the file is one, otherwise the name the store holds - a
    /// chain's built-in passes are built in code, so their guid is the only
    /// thing a file walk can know about them.
    pub(super) fn asset_label(&mut self, guid: &Option<String>) -> Option<String> {
        let guid = guid.as_deref()?;
        if let Some(path) = self
            .asset_entries
            .iter()
            .find(|row| row.guid.as_deref() == Some(guid))
            .map(|row| row.path.clone())
        {
            return Some(path);
        }
        let parsed = pill_engine_core::AssetGuid::parse(guid)?;
        let world = self.host.host_mut().engine_mut().world_mut();
        let registry = world
            .get_resource::<pill_engine_core::ImportRegistry>()
            .cloned()?;
        let assets = world.get_resource::<pill_engine_core::AssetManager>()?;
        registry
            .loaded_by_guid(assets, parsed)
            .map(|(_type_name, name, _index, _generation)| name)
    }

    /// The texture slots the shader a material document names declares.
    ///
    /// A material's rows are the shader's slots: `RendererMaterial` builds its
    /// bind group from them, so the Inspector asks the same source. The shader
    /// is found through its guid, which is what the document holds, and its
    /// declarations are read from the `.hlsl.meta` the listing row points at.
    pub(super) fn shader_texture_slots(
        &mut self,
        settings: &Option<serde_json::Value>,
    ) -> Vec<String> {
        let Some(reference) = settings
            .as_ref()
            .and_then(|document| document.get("shader"))
        else {
            return Vec::new();
        };
        let guid = match reference {
            serde_json::Value::String(guid) => guid.clone(),
            other => match other.get("guid").and_then(|guid| guid.as_str()) {
                Some(guid) => guid.to_owned(),
                None => return Vec::new(),
            },
        };
        let Some(path) = self
            .asset_entries
            .iter()
            .find(|row| row.guid.as_deref() == Some(guid.as_str()))
            .map(|row| row.path.clone())
        else {
            return Vec::new();
        };
        let Some(shader) = self.read_asset_settings(&path) else {
            return Vec::new();
        };
        shader
            .get("textures")
            .and_then(|slots| slots.as_array())
            .map(|slots| {
                slots
                    .iter()
                    .filter_map(|slot| slot.get("name")?.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The settings document an asset's editable rows build on, read through
    /// the import registry - the same reader the `SetAssetSettings` apply
    /// uses.
    pub(super) fn read_asset_settings(&mut self, path: &str) -> Option<serde_json::Value> {
        let world = self.host.host_mut().engine_mut().world_mut();
        let registry = world
            .get_resource::<pill_engine_core::ImportRegistry>()?
            .clone();
        let relative = pill_engine_core::asset_metadata::checked_relative(path).ok()?;
        registry.settings_json(&relative).ok()
    }

    /// Show what a drag from the OS is carrying: a toast
    /// naming the file and the folder it would land in, once per path, so the
    /// person sees where a drop goes before letting go.
    pub(super) fn preview_drop(&mut self, path: &std::path::Path) {
        if self.drop_preview.as_deref() == Some(path) {
            return;
        }
        self.drop_preview = Some(path.to_path_buf());
        let folder = self.drop_folder();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        info!(
            target: telemetry_target::ENGINE,
            path = %path.display(),
            folder = folder.as_str(),
            "[editor] drop preview"
        );
        self.push_toast(NoticeLevel::Note, format!("drop {name} into {folder}"));
    }

    /// A drag that left every window: the preview goes away.
    pub(super) fn cancel_drop_preview(&mut self) {
        if self.drop_preview.take().is_some() {
            info!(
                target: telemetry_target::ENGINE,
                "[editor] drop preview cancelled"
            );
        }
    }

    /// The Assets folder a drop lands in, as the grid shows it: the session's
    /// open folder, or the asset root itself.
    pub(super) fn drop_folder(&mut self) -> String {
        let world = self.host.host_mut().engine_mut().world_mut();
        let folder = world
            .get_resource::<pill_editor_core::session::EguiShellState>()
            .map(|state| state.browser.folder.clone())
            .unwrap_or_default();
        if folder.is_empty() {
            "res".to_string()
        } else {
            format!("res/{folder}")
        }
    }

    /// Import the paths a drop delivered.
    ///
    /// A folder is refused by name - the import copies files, and a directory
    /// would need a recursive walk with its own refusals - and the refusal is
    /// a toast as well as a log line. Files become one recorded
    /// `ImportFiles` step, so undo takes them off the disk again.
    pub(super) fn drop_files(&mut self, paths: Vec<std::path::PathBuf>) {
        self.drop_preview = None;
        let Some(asset_root) = self
            .host
            .host_mut()
            .asset_directory()
            .map(|directory| directory.to_path_buf())
        else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] drop refused: the host knows no asset directory"
            );
            self.push_toast(
                NoticeLevel::Warning,
                "drop refused: no asset directory".to_string(),
            );
            return;
        };
        let folder = self.drop_folder();
        let sub_folder = match pill_editor_core::browser::drop_folder_relative(&folder) {
            Ok(relative) => relative,
            Err(message) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    folder = folder.as_str(),
                    "[editor] drop refused: {message}"
                );
                self.push_toast(NoticeLevel::Warning, format!("drop refused: {message}"));
                return;
            }
        };
        let destination = if sub_folder.as_os_str().is_empty() {
            asset_root.clone()
        } else {
            asset_root.join(&sub_folder)
        };
        let mut sources = Vec::new();
        for path in paths {
            if path.is_dir() {
                info!(
                    target: telemetry_target::ENGINE,
                    path = %path.display(),
                    "[editor] drop refused: folder"
                );
                self.push_toast(
                    NoticeLevel::Warning,
                    format!(
                        "{} is a folder; drop its files instead",
                        path.file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.display().to_string())
                    ),
                );
                continue;
            }
            sources.push(path.display().to_string());
        }
        if sources.is_empty() {
            return;
        }
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(
                vec![EditorCommand::ImportFiles {
                    sources: sources.clone(),
                    destination: destination.display().to_string(),
                }],
                "import files",
                None,
            );
        }
        self.asset_listing_dirty = true;
        info!(
            target: telemetry_target::ENGINE,
            count = sources.len(),
            folder = folder.as_str(),
            "[editor] files dropped"
        );
        self.push_toast(
            NoticeLevel::Note,
            format!("importing {} file(s) into {folder}", sources.len()),
        );
    }
}
