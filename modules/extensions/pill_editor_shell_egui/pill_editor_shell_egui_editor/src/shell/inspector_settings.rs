//! The Inspector's asset import settings.
//!
//! # Responsibilities
//!
//! - `asset_settings_rows` and `asset_settings_row`: the selected asset's
//!   settings as a recursive row walk.
//! - `settings_commit`: the recorded command one committed settings value
//!   queues.
//! - The two shapes a material document needs rows of their own for: a colour
//!   parameter (`parameters.<slot>.Color`, an array of numbers) drawn as a
//!   picker, and a texture slot (`textures.<slot>`, a guid) drawn as a
//!   type-filtered picker with a drop target.
//!
//! # Design
//!
//! Every settings edit is one recorded `SetAssetSettings` step carrying the
//! whole patched document, so undo and the file's round trip are unchanged
//! whatever row wrote it. A row that references another asset writes that
//! asset's **guid**, which is what the file holds: each listing row carries
//! its own guid for exactly this.
//!
//! The globs below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// The last segment of a texture asset's registered type name: a slot's
/// picker filters by it without naming a renderer crate.
pub const TEXTURE_TYPE_SUFFIX: &str = "Texture";

/// The settings document's rows: objects and arrays nest their children,
/// every scalar leaf edits - unless `read_only`, when every row reads a module
/// resource that nothing may write.
///
/// `texture_slots` are the slots the asset's shader declares: each gets a row,
/// whether or not the document binds it yet.
#[allow(clippy::too_many_arguments)]
pub fn asset_settings_rows(
    ui: &mut egui::Ui,
    asset_path: &str,
    settings: &JsonValue,
    assets: &[AssetListing],
    pending: &mut PendingEditorCommands,
    session: &mut InspectorSession,
    overlay_regions: &mut Vec<ShellRect>,
    read_only: bool,
    texture_slots: &[String],
) {
    match settings {
        JsonValue::Object(entries) => {
            for (key, value) in entries {
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
            // A slot the shader declares but the document does not bind is
            // exactly the one a person wants to fill: without a row there is
            // no way to pick a texture for it.
            let bound = entries.get("textures");
            for slot in texture_slots {
                if bound.is_some_and(|textures| textures.get(slot.as_str()).is_some()) {
                    continue;
                }
                texture_slot_row(
                    ui,
                    asset_path,
                    settings,
                    &format!("textures.{slot}"),
                    &None,
                    assets,
                    pending,
                    session,
                    overlay_regions,
                );
            }
        }
        other => {
            read_only_row(ui, "settings", &other.to_string());
        }
    }
}

/// One settings row: a leaf gets the widget its JSON kind asks for (its
/// commit carries the whole patched document), a container nests - except the
/// two shapes that get a control of their own, a texture slot and a colour
/// parameter.
#[allow(clippy::too_many_arguments)]
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
    if key == "shader" || key == "shader.guid" {
        let reference = reference_guid(value);
        shader_row(
            ui,
            asset_path,
            root,
            "shader",
            &reference,
            assets,
            pending,
            session,
            overlay_regions,
        );
        return;
    }
    if let Some(reference) = texture_slot_value(&key, value) {
        texture_slot_row(
            ui,
            asset_path,
            root,
            &key,
            &reference,
            assets,
            pending,
            session,
            overlay_regions,
        );
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

/// The reference a texture slot holds, when `key` names one: the key is
/// `textures.<slot>` (or its `.guid` leaf) and the value is a guid string, a
/// nested object's `guid`, or `null` for an empty slot.
///
/// `Some(None)` is an empty slot; `None` is not a texture slot at all.
pub fn texture_slot_value(key: &str, value: &JsonValue) -> Option<Option<String>> {
    let leaf = key.strip_suffix(".guid").unwrap_or(key);
    let slot = leaf.strip_prefix("textures.")?;
    if slot.is_empty() {
        return None;
    }
    match value {
        JsonValue::Null => Some(None),
        JsonValue::String(guid) => Some(Some(guid.clone())),
        JsonValue::Object(entries) => match entries.get("guid") {
            Some(JsonValue::Null) | None => Some(None),
            Some(JsonValue::String(guid)) => Some(Some(guid.clone())),
            _ => None,
        },
        _ => None,
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

/// One texture slot: the referenced texture's name (or its guid, when the
/// listing no longer holds it), a clear button, a drop target and a
/// type-filtered picker that opens under the row.
///
/// Every choice writes the chosen asset's guid, which is what the file holds.
#[allow(clippy::too_many_arguments)]
pub fn texture_slot_row(
    ui: &mut egui::Ui,
    asset_path: &str,
    root: &JsonValue,
    key: &str,
    reference: &Option<String>,
    assets: &[AssetListing],
    pending: &mut PendingEditorCommands,
    session: &mut InspectorSession,
    overlay_regions: &mut Vec<ShellRect>,
) {
    let texture_key = key.strip_suffix(".guid").unwrap_or(key);
    let texture_path = reference
        .as_deref()
        .and_then(|guid| assets.iter().find(|row| row.guid.as_deref() == Some(guid)))
        .map(|row| row.path.clone());
    let text = match (&texture_path, reference) {
        (Some(path), _) => path.clone(),
        (None, Some(guid)) => format!("{}…", guid.chars().take(8).collect::<String>()),
        (None, None) => "(none)".to_string(),
    };
    let slot = texture_key
        .strip_prefix("textures.")
        .unwrap_or(texture_key)
        .to_string();
    // `Some(Some(guid))` points the slot at a texture, `Some(None)` clears it;
    // collected while drawing and queued once afterwards.
    let mut chosen: Option<Option<String>> = None;
    let mut button_rect = None;
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(slot.as_str()).color(style::text_muted()));
        let response = ui.add_sized(
            egui::vec2(
                (ui.available_width() - inspector_fields::ASSET_ROW_CLEAR_WIDTH).max(60.0),
                18.0,
            ),
            egui::Button::new(icons::rich(icons::Icon::Image, &text)).truncate(),
        );
        if response.dnd_hover_payload::<String>().is_some() {
            ui.painter().rect_stroke(
                response.rect,
                2.0,
                ui.visuals().selection.stroke,
                egui::StrokeKind::Inside,
            );
        }
        if let Some(path) = response.dnd_release_payload::<String>() {
            chosen = texture_guid_for(assets, &path).map(Some);
        }
        if response.clicked() {
            session.settings_picker = match session.settings_picker.as_deref() {
                Some(open) if open == texture_key => None,
                _ => Some(texture_key.to_string()),
            };
            session.settings_search.clear();
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                slot = slot.as_str(),
                "[editor] settings texture picker"
            );
        }
        button_rect = Some(response.rect);
        if ui
            .add_enabled(reference.is_some(), egui::Button::new("x").small())
            .on_hover_text("Clear the texture")
            .clicked()
        {
            chosen = Some(None);
        }
    });

    if session.settings_picker.as_deref() == Some(texture_key) {
        if let Some(rect) = button_rect {
            let picker_id = egui::Id::new((
                "pill_editor_shell_egui.settings_picker",
                texture_key.to_string(),
            ));
            floating_popup(
                ui,
                picker_id,
                rect,
                rect.width().max(180.0),
                overlay_regions,
                &mut |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut session.settings_search)
                            .hint_text("Search textures")
                            .desired_width(f32::INFINITY),
                    );
                    let search = session.settings_search.to_lowercase();
                    egui::ScrollArea::vertical()
                        .max_height(inspector_fields::ASSET_PICKER_HEIGHT)
                        .show(ui, |ui| {
                            if ui.selectable_label(reference.is_none(), "None").clicked() {
                                chosen = Some(None);
                            }
                            for candidate in texture_candidates(assets)
                                .into_iter()
                                .filter(|candidate| candidate.path.to_lowercase().contains(&search))
                            {
                                let current =
                                    texture_path.as_deref() == Some(candidate.path.as_str());
                                if ui.selectable_label(current, &candidate.path).clicked() {
                                    chosen = candidate.guid.clone().map(Some);
                                }
                            }
                        });
                },
            );
        }
    }

    if let Some(reference) = chosen {
        session.settings_picker = None;
        let value = match reference {
            Some(guid) => JsonValue::String(guid),
            None => JsonValue::Null,
        };
        settings_commit(pending, asset_path, root, texture_key, value);
    }
}

/// The listing rows a texture slot's picker offers: the files whose type is a
/// texture, in listing order.
pub fn texture_candidates(assets: &[AssetListing]) -> Vec<&AssetListing> {
    assets
        .iter()
        .filter(|row| {
            row.kind
                .rsplit("::")
                .next()
                .is_some_and(|name| name == TEXTURE_TYPE_SUFFIX)
        })
        .collect()
}

/// The last segment of a shader asset's registered type name.
pub const SHADER_TYPE_SUFFIX: &str = "Shader";

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

/// The listing rows a shader reference's picker offers, in listing order.
pub fn shader_candidates(assets: &[AssetListing]) -> Vec<&AssetListing> {
    assets
        .iter()
        .filter(|row| {
            row.kind
                .rsplit("::")
                .next()
                .is_some_and(|name| name == SHADER_TYPE_SUFFIX)
        })
        .collect()
}

/// One shader reference: the shader's path (or its guid when the listing no
/// longer holds it), a clear button, a drop target and a picker over the
/// shaders the listing names.
///
/// Every choice writes the reference's object form, which is what a saved
/// `.material` holds.
#[allow(clippy::too_many_arguments)]
pub fn shader_row(
    ui: &mut egui::Ui,
    asset_path: &str,
    root: &JsonValue,
    key: &str,
    reference: &Option<String>,
    assets: &[AssetListing],
    pending: &mut PendingEditorCommands,
    session: &mut InspectorSession,
    overlay_regions: &mut Vec<ShellRect>,
) {
    let shader_path = reference
        .as_deref()
        .and_then(|guid| assets.iter().find(|row| row.guid.as_deref() == Some(guid)))
        .map(|row| row.path.clone());
    let text = match (&shader_path, reference) {
        (Some(path), _) => path.clone(),
        (None, Some(guid)) => format!("{}…", guid.chars().take(8).collect::<String>()),
        (None, None) => "(none)".to_string(),
    };
    let mut chosen: Option<Option<String>> = None;
    let mut button_rect = None;
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("shader").color(style::text_muted()));
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
                "[editor] settings shader picker"
            );
        }
        button_rect = Some(response.rect);
        if ui
            .add_enabled(reference.is_some(), egui::Button::new("x").small())
            .on_hover_text("Clear the shader")
            .clicked()
        {
            chosen = Some(None);
        }
    });

    if session.settings_picker.as_deref() == Some(key) {
        if let Some(rect) = button_rect {
            let picker_id =
                egui::Id::new(("pill_editor_shell_egui.shader_picker", key.to_string()));
            floating_popup(
                ui,
                picker_id,
                rect,
                rect.width().max(180.0),
                overlay_regions,
                &mut |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(inspector_fields::ASSET_PICKER_HEIGHT)
                        .show(ui, |ui| {
                            for candidate in shader_candidates(assets) {
                                let current =
                                    shader_path.as_deref() == Some(candidate.path.as_str());
                                if ui.selectable_label(current, &candidate.path).clicked() {
                                    chosen = candidate.guid.clone().map(Some);
                                }
                            }
                        });
                },
            );
        }
    }

    if let Some(reference) = chosen {
        session.settings_picker = None;
        let value = match reference {
            Some(guid) => serde_json::json!({ "guid": guid }),
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
    if let Err(message) = inspector::patch_settings(&mut document, &path, value) {
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

    /// A shader reference's picker offers shaders, and a texture slot's picker
    /// offers textures: neither lists the other's rows.
    #[test]
    fn the_pickers_offer_their_own_kind() {
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
        ];
        let shaders: Vec<&str> = shader_candidates(&assets)
            .iter()
            .map(|row| row.path.as_str())
            .collect();
        assert_eq!(shaders, vec!["shaders/pbr.hlsl"]);
        let textures: Vec<&str> = texture_candidates(&assets)
            .iter()
            .map(|row| row.path.as_str())
            .collect();
        assert_eq!(textures, vec!["textures/rock.png"]);
    }
}
