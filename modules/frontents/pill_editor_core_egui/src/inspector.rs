//! The Inspector's editing helpers: field edits, their labels and the asset
//! settings documents as editable rows.
//!
//! # Responsibilities
//!
//! - The `SetField`/`SetArrayElement` builders and their history labels (the
//!   native editor's spellings), so the shell's widgets and the scripted
//!   channel queue identical edits.
//! - [`parse_field_value`]: text into a value's own kind, the native
//!   inspector's parsing rule - used by the scripted channel and any text
//!   field.
//! - [`element_field`]: one element of an array value.
//! - Asset settings as editable data: key paths (`a.b[0]`), leaf reads and
//!   patches, and the [`EditorCommand::SetAssetSettings`] builder.
//!
//! # Design
//!
//! Everything here is plain data and commands: the shell draws widgets and
//! queues batches, the executable applies them at the frame boundary, and
//! the history records them - the same split the Assets pipeline follows.

// External crates
use pill_engine::{Entity, FieldValue};
use serde_json::Value as JsonValue;

// Current crate
use crate::editor_state::EditorCommand;

/// The name after the last `::`, for labels and history entries.
pub fn short_component_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// The recorded component attach, under the native label (`add {short}`):
/// the Component menu's picker and the scripted channel share it.
pub fn component_add(entity: Entity, component: &str) -> (EditorCommand, String) {
    (
        EditorCommand::AddComponent {
            entity,
            component: component.to_string(),
        },
        format!("add {}", short_component_name(component)),
    )
}

/// The recorded component detach, under the native label (`remove {short}`).
pub fn component_remove(entity: Entity, component: &str) -> (EditorCommand, String) {
    (
        EditorCommand::RemoveComponent {
            entity,
            component: component.to_string(),
        },
        format!("remove {}", short_component_name(component)),
    )
}

/// The recorded whole-field edit: `SetField` under the native label.
pub fn field_edit(
    entity: Entity,
    component: &str,
    field: &str,
    value: FieldValue,
) -> (EditorCommand, String) {
    (
        EditorCommand::SetField {
            entity,
            component: component.to_string(),
            field: field.to_string(),
            value,
        },
        format!("set {}.{}", short_component_name(component), field),
    )
}

/// The recorded array-element edit: `SetArrayElement` under the native
/// label (`set Component[index].field`).
pub fn element_edit(
    entity: Entity,
    component: &str,
    field: &str,
    index: usize,
    value: FieldValue,
) -> (EditorCommand, String) {
    (
        EditorCommand::SetArrayElement {
            entity,
            component: component.to_string(),
            field: field.to_string(),
            index,
            value,
        },
        format!(
            "set {}[{}].{}",
            short_component_name(component),
            index,
            field
        ),
    )
}

/// The continuous-edit key one inspector widget's commits share, so a drag
/// or a typing run merges into one undo step - the native widget id.
pub fn field_coalesce_key(entity: Entity, component: &str, field: &str) -> String {
    format!("inspector.{entity}.{component}.{field}")
}

/// Element `index` of an array value, when it is one and the index fits.
pub fn element_field(value: &FieldValue, index: usize) -> Option<FieldValue> {
    let FieldValue::Array(elements) = value else {
        return None;
    };
    elements.get(index).cloned()
}

/// Parse an edited text back into the kind `current` already holds.
///
/// The scripted channel and a text field both commit through this: a float
/// field takes a float, a text field the rest of the line, and a kind with
/// no editor refuses.
pub fn parse_field_value(text: &str, current: &FieldValue) -> Result<FieldValue, String> {
    let text = text.trim();
    let parsed = match current {
        FieldValue::F32(_) => FieldValue::F32(text.parse().map_err(|_| "not a float")?),
        FieldValue::F64(_) => FieldValue::F64(text.parse().map_err(|_| "not a float")?),
        FieldValue::I8(_) => FieldValue::I8(text.parse().map_err(|_| "not an integer")?),
        FieldValue::I16(_) => FieldValue::I16(text.parse().map_err(|_| "not an integer")?),
        FieldValue::I32(_) => FieldValue::I32(text.parse().map_err(|_| "not an integer")?),
        FieldValue::I64(_) => FieldValue::I64(text.parse().map_err(|_| "not an integer")?),
        FieldValue::U8(_) => FieldValue::U8(text.parse().map_err(|_| "not an unsigned")?),
        FieldValue::U16(_) => FieldValue::U16(text.parse().map_err(|_| "not an unsigned")?),
        FieldValue::U32(_) => FieldValue::U32(text.parse().map_err(|_| "not an unsigned")?),
        FieldValue::U64(_) => FieldValue::U64(text.parse().map_err(|_| "not an unsigned")?),
        FieldValue::Usize(_) => FieldValue::Usize(text.parse().map_err(|_| "not an unsigned")?),
        FieldValue::Bool(_) => FieldValue::Bool(text.parse().map_err(|_| "not a boolean")?),
        FieldValue::Text(_) => FieldValue::Text(text.to_string()),
        other => return Err(format!("`{other:?}` fields are not editable here")),
    };
    Ok(parsed)
}

/// One segment of a settings key path: an object key or an array index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsKey {
    /// A key of a JSON object.
    Key(String),
    /// An index of a JSON array.
    Index(usize),
}

/// Parse a settings key path: dotted object keys with `[i]` array indexes,
/// like `waves[0].amplitude`.
pub fn parse_settings_key_path(text: &str) -> Result<Vec<SettingsKey>, String> {
    let mut path = Vec::new();
    let mut key = String::new();
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '.' => {
                if key.is_empty() {
                    // A dot right after an index is a separator (`a[0].b`);
                    // anywhere else it has no key to end.
                    match path.last() {
                        Some(SettingsKey::Index(_)) => {}
                        _ => return Err(format!("`{text}` has an empty key")),
                    }
                } else {
                    path.push(SettingsKey::Key(std::mem::take(&mut key)));
                }
            }
            '[' => {
                if !key.is_empty() {
                    path.push(SettingsKey::Key(std::mem::take(&mut key)));
                }
                let mut digits = String::new();
                for digit in characters.by_ref() {
                    if digit == ']' {
                        break;
                    }
                    digits.push(digit);
                }
                let index: usize = digits
                    .parse()
                    .map_err(|_| format!("`{text}` has a bad index `[{digits}]`"))?;
                path.push(SettingsKey::Index(index));
            }
            ']' => return Err(format!("`{text}` has an unbalanced `]`")),
            other => key.push(other),
        }
    }
    if !key.is_empty() {
        path.push(SettingsKey::Key(key));
    }
    if path.is_empty() {
        return Err(format!("`{text}` names no key"));
    }
    Ok(path)
}

/// The canonical text of a key path: what labels and coalescing keys use.
pub fn settings_key_text(path: &[SettingsKey]) -> String {
    let mut text = String::new();
    for segment in path {
        match segment {
            SettingsKey::Key(key) => {
                if !text.is_empty() {
                    text.push('.');
                }
                text.push_str(key);
            }
            SettingsKey::Index(index) => {
                text.push('[');
                text.push_str(&index.to_string());
                text.push(']');
            }
        }
    }
    text
}

/// The settings leaf a key path names, when the document holds it.
pub fn settings_field<'a>(document: &'a JsonValue, path: &[SettingsKey]) -> Option<&'a JsonValue> {
    let mut current = document;
    for segment in path {
        current = match segment {
            SettingsKey::Key(key) => current.get(key)?,
            SettingsKey::Index(index) => current.get(index)?,
        };
    }
    Some(current)
}

/// Replace the settings leaf a key path names; the containers along the way
/// must already exist (the editors only edit what they displayed).
pub fn patch_settings(
    document: &mut JsonValue,
    path: &[SettingsKey],
    value: JsonValue,
) -> Result<(), String> {
    let (last, parents) = path
        .split_last()
        .ok_or_else(|| "a settings key path cannot be empty".to_string())?;
    let mut current = document;
    for segment in parents {
        current = match segment {
            SettingsKey::Key(key) => current
                .get_mut(key)
                .ok_or_else(|| format!("`{key}` is not in the settings"))?,
            SettingsKey::Index(index) => current
                .get_mut(index)
                .ok_or_else(|| format!("element [{index}] is not in the settings"))?,
        };
    }
    let slot = match last {
        SettingsKey::Key(key) => current
            .get_mut(key)
            .ok_or_else(|| format!("`{key}` is not in the settings"))?,
        SettingsKey::Index(index) => current
            .get_mut(index)
            .ok_or_else(|| format!("element [{index}] is not in the settings"))?,
    };
    *slot = value;
    Ok(())
}

/// Parse an edited text back into the JSON kind `current` already holds.
pub fn parse_settings_value(text: &str, current: &JsonValue) -> Result<JsonValue, String> {
    let text = text.trim();
    let parsed = match current {
        JsonValue::Bool(_) => JsonValue::Bool(text.parse().map_err(|_| "not a boolean")?),
        JsonValue::Number(number) => {
            if number.is_i64() || number.is_u64() {
                let value: i64 = text.parse().map_err(|_| "not an integer")?;
                JsonValue::Number(value.into())
            } else {
                let value: f64 = text.parse().map_err(|_| "not a number")?;
                JsonValue::Number(serde_json::Number::from_f64(value).ok_or("not a finite number")?)
            }
        }
        JsonValue::String(_) => JsonValue::String(text.to_string()),
        other => return Err(format!("`{other}` settings are not editable here")),
    };
    Ok(parsed)
}

/// The recorded settings edit: `SetAssetSettings` under the label the
/// history and the suite read.
pub fn settings_edit(path: &str, settings: JsonValue, key_text: &str) -> (EditorCommand, String) {
    (
        EditorCommand::SetAssetSettings {
            path: path.to_string(),
            settings,
        },
        format!("set asset {key_text}"),
    )
}

/// The continuous-edit key one settings widget's commits share.
pub fn settings_coalesce_key(asset_path: &str, key_text: &str) -> String {
    format!("inspector.asset.{asset_path}.{key_text}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn labels_shorten_the_component_name() {
        let entity = Entity::from_raw(3, 0);
        let (command, label) = field_edit(
            entity,
            "pill_master_renderer::component::TransformComponent",
            "speed",
            FieldValue::F32(1.0),
        );
        assert_eq!(label, "set TransformComponent.speed");
        assert!(matches!(command, EditorCommand::SetField { .. }));
        let (_, label) = element_edit(
            entity,
            "pill_master_renderer::component::TransformComponent",
            "translation",
            2,
            FieldValue::F32(1.0),
        );
        assert_eq!(label, "set TransformComponent[2].translation");
        assert_eq!(
            field_coalesce_key(entity, "Component", "speed"),
            format!("inspector.{entity}.Component.speed")
        );
    }

    #[test]
    fn parse_follows_the_current_kind() {
        assert_eq!(
            parse_field_value(" 4.25 ", &FieldValue::F32(0.0)),
            Ok(FieldValue::F32(4.25))
        );
        assert!(parse_field_value("true", &FieldValue::F32(0.0)).is_err());
        assert_eq!(
            parse_field_value("true", &FieldValue::Bool(false)),
            Ok(FieldValue::Bool(true))
        );
        assert_eq!(
            parse_field_value("hello world", &FieldValue::Text(String::new())),
            Ok(FieldValue::Text("hello world".to_string()))
        );
        assert!(parse_field_value("1", &FieldValue::Array(Vec::new())).is_err());
    }

    #[test]
    fn element_field_reads_arrays_only() {
        let value = FieldValue::Array(vec![
            FieldValue::F32(1.0),
            FieldValue::F32(2.0),
            FieldValue::F32(3.0),
        ]);
        assert_eq!(element_field(&value, 1), Some(FieldValue::F32(2.0)));
        assert_eq!(element_field(&value, 9), None);
        assert_eq!(element_field(&FieldValue::F32(1.0), 0), None);
    }

    #[test]
    fn key_paths_parse_and_render() {
        let path = parse_settings_key_path("waves[0].amplitude").expect("parses");
        assert_eq!(
            path,
            vec![
                SettingsKey::Key("waves".to_string()),
                SettingsKey::Index(0),
                SettingsKey::Key("amplitude".to_string()),
            ]
        );
        assert_eq!(settings_key_text(&path), "waves[0].amplitude");
        assert!(parse_settings_key_path("").is_err());
        assert!(parse_settings_key_path("a..b").is_err());
        assert!(parse_settings_key_path("a[x]").is_err());
    }

    #[test]
    fn settings_leafs_read_and_patch() {
        let mut document = json!({
            "flip_v": false,
            "waves": [{ "amplitude": 1.0 }],
        });
        let path = parse_settings_key_path("waves[0].amplitude").expect("parses");
        assert_eq!(
            settings_field(&document, &path),
            Some(&JsonValue::from(1.0))
        );
        patch_settings(&mut document, &path, json!(2.5)).expect("patches");
        assert_eq!(
            settings_field(&document, &path),
            Some(&JsonValue::from(2.5))
        );
        patch_settings(&mut document, &path, json!(7)).expect("patches");
        assert_eq!(settings_field(&document, &path), Some(&json!(7)));
        let missing = parse_settings_key_path("nope").expect("parses");
        assert!(patch_settings(&mut document, &missing, json!(1)).is_err());
    }

    #[test]
    fn settings_values_parse_following_the_current_kind() {
        assert_eq!(parse_settings_value("true", &json!(false)), Ok(json!(true)));
        assert_eq!(parse_settings_value("2", &json!(1)), Ok(json!(2)));
        assert_eq!(parse_settings_value("2.5", &json!(1.0)), Ok(json!(2.5)));
        assert_eq!(
            parse_settings_value("Normal", &json!("Color")),
            Ok(json!("Normal"))
        );
        assert!(parse_settings_value("2", &json!("Color")).is_ok());
        assert!(parse_settings_value("nope", &json!(1)).is_err());
        assert!(parse_settings_value("1", &json!({"a": 1})).is_err());
    }

    #[test]
    fn settings_edits_label_and_coalesce() {
        let (command, label) = settings_edit(
            "textures/rock.png",
            json!({"texture_type": "Normal"}),
            "texture_type",
        );
        assert_eq!(label, "set asset texture_type");
        assert!(matches!(command, EditorCommand::SetAssetSettings { .. }));
        assert_eq!(
            settings_coalesce_key("textures/rock.png", "texture_type"),
            "inspector.asset.textures/rock.png.texture_type"
        );
    }

    #[test]
    fn component_add_and_remove_record_the_native_labels() {
        let entity = Entity::from_raw(3, 0);
        let (add, add_label) = component_add(
            entity,
            "pill_master_renderer::component::TransformComponent",
        );
        assert_eq!(add_label, "add TransformComponent");
        assert!(matches!(add, EditorCommand::AddComponent { .. }));
        let (remove, remove_label) =
            component_remove(entity, "italian_brainrot::TagAlphaComponent");
        assert_eq!(remove_label, "remove TagAlphaComponent");
        assert!(matches!(remove, EditorCommand::RemoveComponent { .. }));
    }
}
