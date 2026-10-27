//! The Inspector's asset import settings.
//!
//! # Responsibilities
//!
//! - `asset_settings_rows` and `asset_settings_row`: the selected asset's
//!   settings as a recursive row walk.
//! - `reference_row`: one row per reference field the asset's type declares -
//!   the same row for a material's shader, its texture slots and a chain's
//!   pass list, driven by the snapshot's answers rather than by knowing a
//!   renderer type.
//! - `settings_commit`: the recorded command one committed settings value
//!   queues.
//! - The colour parameter shape (`parameters.<slot>.Color`, an array of
//!   numbers) drawn as a swatch.
//!
//! # Design
//!
//! Every settings edit is one recorded `SetAssetSettings` step carrying the
//! whole patched document, so undo and the file's round trip are unchanged
//! whatever row wrote it. A row that references another asset writes that
//! asset's **guid**, which is what the file holds: each listing row carries
//! its own guid for exactly this. The first write is what creates a slot the
//! document does not hold yet.
//!
//! The globs below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// The settings document's rows: objects and arrays nest their children,
/// every scalar leaf edits - unless `read_only`, when every row reads a module
/// resource that nothing may write.
///
/// `references` are the reference fields the asset's type declares: each gets a
/// picker row, whether or not the document binds it yet, and the document key
/// behind it is left out of the walk so one field has one row.
#[allow(clippy::too_many_arguments)]
pub fn asset_settings_rows(
    ui: &mut egui::Ui,
    asset_path: &str,
    settings: &JsonValue,
    assets: &[AssetListing],
    engine_assets: &[EngineAsset],
    pending: &mut PendingEditorCommands,
    session: &mut InspectorSession,
    overlay_regions: &mut Vec<ShellRect>,
    read_only: bool,
    references: &[AssetReferenceField],
) {
    match settings {
        JsonValue::Object(entries) => {
            for (key, value) in entries {
                // A key a reference field owns is drawn by that field's row -
                // including a list family, whose entries arrive as
                // `passes[0]`, `passes[1]`, ... and whose array would
                // otherwise be walked as raw JSON, guids and all.
                let owned = references.iter().any(|field| {
                    field.key == *key
                        || field.key.starts_with(&format!("{key}."))
                        || reference_family(field) == Some(key.as_str())
                });
                if owned {
                    continue;
                }
                asset_settings_row(
                    ui,
                    asset_path,
                    settings,
                    value,
                    key.clone(),
                    assets,
                    pending,
                    session,
                    overlay_regions,
                    read_only,
                );
            }
            if read_only {
                return;
            }
            for field in references {
                reference_row(
                    ui,
                    asset_path,
                    settings,
                    field,
                    assets,
                    engine_assets,
                    pending,
                    session,
                    overlay_regions,
                );
            }
            reference_list_rows(
                ui,
                asset_path,
                settings,
                references,
                assets,
                engine_assets,
                pending,
                session,
                overlay_regions,
            );
        }
        other => {
            read_only_row(ui, "settings", &other.to_string());
        }
    }
}

/// The list families a reference field set names - `passes[0]`, `passes[1]`, ...
/// - as one editable list each: an append row, and a row of moves per entry.
///
/// A chain's pass list is the first such family; the moves rewrite the whole
/// array in one recorded step, so undo takes back the move and not the file.
#[allow(clippy::too_many_arguments)]
pub fn reference_list_rows(
    ui: &mut egui::Ui,
    asset_path: &str,
    root: &JsonValue,
    references: &[AssetReferenceField],
    assets: &[AssetListing],
    engine_assets: &[EngineAsset],
    pending: &mut PendingEditorCommands,
    session: &mut InspectorSession,
    overlay_regions: &mut Vec<ShellRect>,
) {
    let mut families: Vec<(String, String)> = references
        .iter()
        .filter_map(|field| {
            let family = reference_family(field)?;
            Some((family.to_owned(), field.target.clone()))
        })
        .collect();
    families.sort();
    families.dedup();
    for (family, target) in families {
        let entries = root
            .get(&family)
            .and_then(|list| list.as_array())
            .cloned()
            .unwrap_or_default();
        ui.separator();
        ui.label(
            egui::RichText::new(format!("{family} ({})", entries.len())).color(style::text_muted()),
        );
        // Appending is the picker row of the entry one past the end: the
        // inserting patch is what creates it.
        let append = AssetReferenceField {
            key: format!("{family}[{}]", entries.len()),
            target,
            guid: None,
            nested: true,
            label: None,
        };
        reference_row(
            ui,
            asset_path,
            root,
            &append,
            assets,
            engine_assets,
            pending,
            session,
            overlay_regions,
        );
        for (index, _) in entries.iter().enumerate() {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(format!("{family}[{index}]")).color(style::text_muted()),
                );
                if ui
                    .add_enabled(index > 0, egui::Button::new("Up").small())
                    .clicked()
                {
                    let mut moved = entries.clone();
                    moved.swap(index, index - 1);
                    settings_commit(pending, asset_path, root, &family, JsonValue::Array(moved));
                }
                if ui
                    .add_enabled(index + 1 < entries.len(), egui::Button::new("Down").small())
                    .clicked()
                {
                    let mut moved = entries.clone();
                    moved.swap(index, index + 1);
                    settings_commit(pending, asset_path, root, &family, JsonValue::Array(moved));
                }
                if ui
                    .add(egui::Button::new("Remove").small())
                    .on_hover_text("Drop this entry from the list")
                    .clicked()
                {
                    let mut fewer = entries.clone();
                    fewer.remove(index);
                    settings_commit(pending, asset_path, root, &family, JsonValue::Array(fewer));
                }
            });
        }
    }
}

/// One settings row: a leaf gets the widget its JSON kind asks for (its
/// commit carries the whole patched document), a container nests - except the
/// two shapes that get a control of their own, a texture slot and a colour
/// parameter.
///
/// A leaf draws from its key and its value alone; the row's assets, session
/// and overlay regions exist for the rows below it, so the attributes are the
/// recursion's own shape rather than an unused argument.
#[allow(clippy::too_many_arguments, clippy::only_used_in_recursion)]
pub fn asset_settings_row(
    ui: &mut egui::Ui,
    asset_path: &str,
    root: &JsonValue,
    value: &JsonValue,
    key: String,
    assets: &[AssetListing],
    pending: &mut PendingEditorCommands,
    session: &mut InspectorSession,
    overlay_regions: &mut Vec<ShellRect>,
    read_only: bool,
) {
    // A module's resource is shown exactly as it is: containers keep their
    // shape and every leaf reads, with no picker to repoint it.
    if read_only {
        match value {
            JsonValue::Object(entries) => {
                ui.label(egui::RichText::new(key.as_str()).color(style::text_muted()));
                ui.indent(key.as_str(), |ui| {
                    for (child_key, child) in entries {
                        asset_settings_row(
                            ui,
                            asset_path,
                            root,
                            child,
                            format!("{key}.{child_key}"),
                            assets,
                            pending,
                            session,
                            overlay_regions,
                            true,
                        );
                    }
                });
            }
            other => read_only_row(ui, &key, &other.to_string()),
        }
        return;
    }
    if let Some(channels) = colour_parameter(&key, value) {
        colour_parameter_row(ui, asset_path, root, &key, channels, pending);
        return;
    }
    match value {
        JsonValue::Object(entries) => {
            ui.label(egui::RichText::new(key.as_str()).color(style::text_muted()));
            ui.indent(key.as_str(), |ui| {
                for (child_key, child) in entries {
                    asset_settings_row(
                        ui,
                        asset_path,
                        root,
                        child,
                        format!("{key}.{child_key}"),
                        assets,
                        pending,
                        session,
                        overlay_regions,
                        false,
                    );
                }
            });
        }
        JsonValue::Array(elements) => {
            ui.label(egui::RichText::new(key.as_str()).color(style::text_muted()));
            ui.indent(key.as_str(), |ui| {
                for (index, child) in elements.iter().enumerate() {
                    asset_settings_row(
                        ui,
                        asset_path,
                        root,
                        child,
                        format!("{key}[{index}]"),
                        assets,
                        pending,
                        session,
                        overlay_regions,
                        false,
                    );
                }
            });
        }
        JsonValue::Bool(current) => {
            let mut edited = *current;
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(key.as_str()).color(style::text_muted()));
                if ui.checkbox(&mut edited, "").changed() {
                    settings_commit(pending, asset_path, root, &key, JsonValue::Bool(edited));
                }
            });
        }
        JsonValue::Number(current) => {
            let mut integer_edit = current
                .as_i64()
                .or_else(|| current.as_u64().map(|value| value as i64));
            match integer_edit.as_mut() {
                Some(edited) => {
                    let mut changed = false;
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(key.as_str()).color(style::text_muted()));
                        changed = ui.add(egui::DragValue::new(edited)).changed();
                    });
                    if changed {
                        settings_commit(
                            pending,
                            asset_path,
                            root,
                            &key,
                            JsonValue::Number((*edited).into()),
                        );
                    }
                }
                None => {
                    let mut edited = current.as_f64().unwrap_or_default();
                    let mut changed = false;
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(key.as_str()).color(style::text_muted()));
                        changed = ui
                            .add(
                                egui::DragValue::new(&mut edited)
                                    .speed(0.05)
                                    .max_decimals(9),
                            )
                            .changed();
                    });
                    if changed {
                        if let Some(number) = serde_json::Number::from_f64(edited) {
                            settings_commit(
                                pending,
                                asset_path,
                                root,
                                &key,
                                JsonValue::Number(number),
                            );
                        }
                    }
                }
            }
        }
        JsonValue::String(current) => {
            let mut edited = current.clone();
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(key.as_str()).color(style::text_muted()));
                let response = ui.add(egui::TextEdit::singleline(&mut edited).desired_width(160.0));
                if response.lost_focus() && edited != *current {
                    settings_commit(pending, asset_path, root, &key, JsonValue::String(edited));
                }
            });
        }
        other => {
            read_only_row(ui, key.as_str(), &other.to_string());
        }
    }
}

/// The channel values of a colour parameter, when `key` names one: a `Color`
/// leaf holding three or four numbers (the material document's
/// `parameters.<slot>.Color`).
pub fn colour_parameter(key: &str, value: &JsonValue) -> Option<Vec<f32>> {
    let leaf = key.rsplit('.').next().unwrap_or(key);
    if !leaf.eq_ignore_ascii_case("Color") {
        return None;
    }
    let JsonValue::Array(elements) = value else {
        return None;
    };
    if !(3..=4).contains(&elements.len()) {
        return None;
    }
    elements
        .iter()
        .map(|element| element.as_f64().map(|number| number as f32))
        .collect()
}

/// One colour parameter: a swatch whose popup holds the picker. A commit
/// writes the whole array back, as one recorded settings step.
pub fn colour_parameter_row(
    ui: &mut egui::Ui,
    asset_path: &str,
    root: &JsonValue,
    key: &str,
    channels: Vec<f32>,
    pending: &mut PendingEditorCommands,
) {
    let mut edited = [1.0f32; 4];
    for (channel, value) in edited.iter_mut().zip(&channels) {
        *channel = *value;
    }
    ui.horizontal_wrapped(|ui| {
        let label = key.rsplit('.').next().unwrap_or(key);
        ui.label(egui::RichText::new(label).color(style::text_muted()));
        if ui
            .color_edit_button_rgba_unmultiplied(&mut edited)
            .changed()
        {
            let numbers: Option<Vec<JsonValue>> = channels
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    serde_json::Number::from_f64(edited[index] as f64).map(JsonValue::Number)
                })
                .collect();
            if let Some(numbers) = numbers {
                settings_commit(pending, asset_path, root, key, JsonValue::Array(numbers));
            }
        }
    });
}

/// The guid a settings reference holds, in either spelling: the value itself
/// when the document writes a bare guid, or the object form's `guid`.
pub fn reference_guid(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::String(guid) => Some(guid.clone()),
        JsonValue::Object(entries) => entries
            .get("guid")
            .and_then(|guid| guid.as_str())
            .map(str::to_owned),
        _ => None,
    }
}

/// The listing's rows a reference's picker offers: the assets whose type is the
/// one the registry named for the field.
pub fn reference_candidates<'a>(assets: &'a [AssetListing], target: &str) -> Vec<&'a AssetListing> {
    inspector::picker_candidates(assets, &[], Some(target)).0
}

/// The list family a reference field's key belongs to: `passes[3]` is an entry
/// of `passes`, and a key without brackets is its own field.
pub fn reference_family(field: &AssetReferenceField) -> Option<&str> {
    field.key.split_once('[').map(|(family, _)| family)
}

/// One reference field: the referenced asset's path (or its guid, when the
/// listing no longer holds it), a clear button, a drop target and a picker over
/// the rows of the type the field accepts.
///
/// Every choice writes the chosen asset's guid in the shape the document
/// already uses - the object form for a named reference, the bare string for a
/// texture slot - and a field the document does not hold yet writes the object
/// form. The same row serves a material's shader, its declared slots and a
/// chain's pass list, because what differs between them is data.
#[allow(clippy::too_many_arguments)]
pub fn reference_row(
    ui: &mut egui::Ui,
    asset_path: &str,
    root: &JsonValue,
    field: &AssetReferenceField,
    assets: &[AssetListing],
    engine_assets: &[EngineAsset],
    pending: &mut PendingEditorCommands,
    session: &mut InspectorSession,
    overlay_regions: &mut Vec<ShellRect>,
) {
    let key = field.key.as_str();
    let label = key
        .split_once('.')
        .map(|(_, leaf)| leaf.to_owned())
        .unwrap_or_else(|| key.to_owned());
    let current_path = field
        .guid
        .as_deref()
        .and_then(|guid| assets.iter().find(|row| row.guid.as_deref() == Some(guid)))
        .map(|row| row.path.clone());
    // What the row reads: the label the executable resolved - a listing path,
    // or the name the store loaded the asset under when it has no file - and
    // only a guid's head when neither knows it.
    let text = match (field.label.as_deref(), field.guid.as_deref()) {
        (Some(label), _) => label.to_owned(),
        (None, Some(guid)) => format!("{}...", guid.chars().take(8).collect::<String>()),
        (None, None) => "(none)".to_string(),
    };
    let mut chosen: Option<Option<String>> = None;
    let mut button_rect = None;
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(&label).color(style::text_muted()));
        let response = ui.add_sized(
            egui::vec2(
                (ui.available_width() - inspector_fields::ASSET_ROW_CLEAR_WIDTH).max(60.0),
                18.0,
            ),
            egui::Button::new(icons::rich(icons::Icon::Code, &text)).truncate(),
        );
        if let Some(dropped) = response.dnd_release_payload::<String>() {
            chosen = assets
                .iter()
                .find(|row| row.key() == *dropped)
                .and_then(|row| row.guid.clone())
                .map(Some);
        }
        if response.clicked() {
            session.settings_picker = Some(key.to_string());
            session.settings_search.clear();
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                slot = key,
                target = field.target.as_str(),
                "[editor] settings reference picker"
            );
        }
        button_rect = Some(response.rect);
        if ui
            .add_enabled(field.guid.is_some(), egui::Button::new("x").small())
            .on_hover_text("Clear the reference")
            .clicked()
        {
            chosen = Some(None);
        }
    });

    if session.settings_picker.as_deref() == Some(key) {
        if let Some(rect) = button_rect {
            let picker_id =
                egui::Id::new(("pill_editor_shell_egui.reference_picker", key.to_string()));
            let area = floating_popup(
                ui,
                picker_id,
                rect,
                rect.width().max(180.0),
                overlay_regions,
                &mut |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(inspector_fields::ASSET_PICKER_HEIGHT)
                        .show(ui, |ui| {
                            if ui.selectable_label(field.guid.is_none(), "None").clicked() {
                                chosen = Some(None);
                            }
                            let (files, built) =
                                inspector::picker_candidates(assets, engine_assets, Some(&field.target));
                            for candidate in files {
                                let current =
                                    current_path.as_deref() == Some(candidate.path.as_str());
                                ui.horizontal(|ui| {
                                    let response = ui.selectable_label(current, &candidate.path);
                                    if response.clicked() {
                                        chosen = candidate.guid.clone().map(Some);
                                    }
                                    // The focus affordance: a single click shows the
                                    // candidate in the Assets panel - the selection
                                    // moves and its folder opens - while the picker
                                    // stays open, so browsing does not throw the edit
                                    // away; a double click also makes it the
                                    // Inspector's subject and closes the picker.
                                    let focus = ui
                                        .small_button("o")
                                        .on_hover_text("Show this asset in the Assets panel");
                                    if focus.double_clicked() {
                                        pending.focus_asset(candidate.key());
                                        session.settings_picker = None;
                                        pill_core::info!(
                                            target: pill_core::telemetry::telemetry_target::ENGINE,
                                            path = candidate.path.as_str(),
                                            "[editor] asset focused"
                                        );
                                    } else if focus.clicked() {
                                        pending.focus_asset(candidate.key());
                                    }
                                });
                            }
                            // The engine's own assets: they have no file, so no
                            // folder to reveal, and choosing one writes its guid.
                            // A candidate the store holds by name alone has no
                            // guid to write, so it reads but cannot be chosen.
                            for candidate in built {
                                let current = field.guid.as_deref() == candidate.guid.as_deref();
                                let pickable = candidate.guid.is_some();
                                ui.horizontal(|ui| {
                                    engine_badge(ui);
                                    let row = ui.add_enabled_ui(pickable, |ui| {
                                        ui.selectable_label(current, &candidate.name).clicked()
                                    });
                                    if row.inner {
                                        chosen = candidate.guid.clone().map(Some);
                                    }
                                    if !pickable {
                                        row.response.on_hover_text(
                                            "The store holds it by name only, so no reference can name it",
                                        );
                                    }
                                });
                            }
                        });
                },
            );
            // Any press outside the picker and its button closes it, whatever
            // button it was: a picker that only closes by picking fights the
            // person who changed their mind. The press that opens the picker is
            // on the button, so it never closes what it just opened.
            let press =
                ui.input(|input| (input.pointer.any_pressed(), input.pointer.interact_pos()));
            if let (true, Some(at)) = press {
                let on_button = rect.contains(at);
                if !on_button && !area.response.rect.contains(at) {
                    session.settings_picker = None;
                }
            }
        }
    }

    if let Some(reference) = chosen {
        session.settings_picker = None;
        let value = match reference {
            Some(guid) if field.nested => serde_json::json!({ "guid": guid }),
            Some(guid) => JsonValue::String(guid),
            None => JsonValue::Null,
        };
        settings_commit(pending, asset_path, root, key, value);
    }
}

/// The guid a dropped asset writes: the listing row's own, or `None` when the
/// path names no asset that carries one.
///
/// The dropped payload is the row's key, so a module row is found the same way
/// a project one is.
pub fn texture_guid_for(assets: &[AssetListing], path: &str) -> Option<String> {
    assets
        .iter()
        .find(|row| row.key() == path)
        .and_then(|row| row.guid.clone())
}

/// Queue the recorded edit that puts `value` at `key` in the settings; the
/// command carries the whole patched document.
pub fn settings_commit(
    pending: &mut PendingEditorCommands,
    asset_path: &str,
    root: &JsonValue,
    key: &str,
    value: JsonValue,
) {
    let Ok(path) = inspector::parse_settings_key_path(key) else {
        return;
    };
    let mut document = root.clone();
    // The inserting rule: a slot or a reference the type declares but the file
    // does not hold yet is created by its first write, which is exactly the
    // unbound-texture case.
    if let Err(message) = inspector::patch_settings_inserting(&mut document, &path, value) {
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ECS,
            key,
            message = message.as_str(),
            "[editor] asset settings edit refused"
        );
        return;
    }
    let (command, label) = inspector::settings_edit(asset_path, document, key);
    pending.push_recorded(
        vec![command],
        label,
        Some(inspector::settings_coalesce_key(asset_path, key)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, kind: &str, guid: Option<&str>) -> AssetListing {
        AssetListing {
            root: String::new(),
            path: path.to_string(),
            read_only: false,
            kind: kind.to_string(),
            standalone: true,
            has_meta: guid.is_some(),
            guid: guid.map(str::to_owned),
        }
    }

    /// A reference reads both spellings the documents use: the object form this
    /// build writes, and the bare guid every older file holds.
    #[test]
    fn a_reference_reads_both_spellings() {
        assert_eq!(
            reference_guid(&serde_json::json!({ "guid": "abc" })).as_deref(),
            Some("abc")
        );
        assert_eq!(
            reference_guid(&serde_json::json!("abc")).as_deref(),
            Some("abc")
        );
        assert_eq!(reference_guid(&serde_json::Value::Null), None);
    }

    /// A picker offers the type its field accepts, and nothing else: the same
    /// filter serves a shader reference, a texture slot and a pass entry.
    #[test]
    fn a_reference_lists_the_type_its_field_accepts() {
        let assets = vec![
            row(
                "materials/a.material",
                "pill_master_renderer::assets::Material",
                Some("m"),
            ),
            row(
                "shaders/pbr.hlsl",
                "pill_master_renderer::assets::Shader",
                Some("s"),
            ),
            row(
                "textures/rock.png",
                "pill_master_renderer::assets::Texture",
                Some("t"),
            ),
            row(
                "passes/opaque.render_pass",
                "pill_master_renderer::assets::RenderPass",
                Some("p"),
            ),
        ];
        let shaders = |target: &str| -> Vec<String> {
            reference_candidates(&assets, target)
                .iter()
                .map(|row| row.path.clone())
                .collect()
        };
        assert_eq!(
            shaders("pill_master_renderer::assets::Shader"),
            vec!["shaders/pbr.hlsl"]
        );
        assert_eq!(shaders("Texture"), vec!["textures/rock.png"]);
        assert_eq!(shaders("RenderPass"), vec!["passes/opaque.render_pass"]);
    }
}
