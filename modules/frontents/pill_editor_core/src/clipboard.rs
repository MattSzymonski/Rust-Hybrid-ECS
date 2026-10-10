//! The editor clipboard's structured payloads (Stage 18, D28).
//!
//! # Responsibilities
//! - One versioned JSON document that names components by type name and
//!   fields by field name, values as token strings ([`format_value`] /
//!   [`parse_value`]) - never pointers, registry ids or `ComponentId`s, so a
//!   payload survives reloads and crosses processes (the cross-DLL rule).
//! - Encoding from the snapshot's detail tier, and decoding into the seed
//!   shapes paste needs (`CreateEntity`'s component seeds, `AddComponent` +
//!   `SetField` for a single component).
//!
//! The same text is what a plain text editor sees when the payload is
//! pasted outside the editor, which is why the document is JSON rather
//! than a binary blob.

// Standard library
use std::fmt::Write as _;

// External crates
use serde_json::{json, Value};

// Current crate
use crate::editor_state::EntityDetail;

// The shared dylib's value vocabulary.
use pill_engine::FieldValue;

/// The clipboard format name the structured payload is registered under
/// (Windows: `RegisterClipboardFormatW`), alongside the readable text.
pub const FORMAT_NAME: &str = "PillEditor.Object";

/// The payload document's version; a reader refuses anything else.
pub const VERSION: u64 = 1;

/// One decoded document.
#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    /// One entity: the components to seed a fresh entity with.
    Entity {
        /// Components with their carried field values.
        components: Vec<SeededComponent>,
    },
    /// One component: the shape `AddComponent` + `SetField`s rebuilds.
    Component {
        /// The component's type name and carried field values.
        component: SeededComponent,
    },
    /// One field value, carried as its token (the readable text a paste
    /// into a text field uses).
    Field {
        /// Registered component type name.
        component: String,
        /// Field name.
        field: String,
        /// The value's token ([`format_value`]'s form).
        value: String,
    },
    /// One asset reference, by project-relative path.
    Asset {
        /// Project-relative path, forward slashes.
        path: String,
    },
}

impl Payload {
    /// The document's kind tag, for messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Entity { .. } => "entity",
            Self::Component { .. } => "component",
            Self::Field { .. } => "field",
            Self::Asset { .. } => "asset",
        }
    }
}

/// One component in a payload: its type name and the fields it carried.
#[derive(Clone, Debug, PartialEq)]
pub struct SeededComponent {
    /// Registered component type name (the stable cross-reload key).
    pub type_name: String,
    /// Field name to token value, in the component's layout order.
    pub fields: Vec<(String, String)>,
}

/// The canonical token of a field value, or `None` for values the payload
/// does not carry (heap sequences, opaque bytes, struct arrays).
pub fn format_value(value: &FieldValue) -> Option<String> {
    Some(match value {
        FieldValue::F32(value) => format!("{value:?}f"),
        FieldValue::F64(value) => format!("{value:?}d"),
        FieldValue::I8(value) => format!("{value}i8"),
        FieldValue::I16(value) => format!("{value}i16"),
        FieldValue::I32(value) => format!("{value}i32"),
        FieldValue::I64(value) => format!("{value}i64"),
        FieldValue::U8(value) => format!("{value}u8"),
        FieldValue::U16(value) => format!("{value}u16"),
        FieldValue::U32(value) => format!("{value}u32"),
        FieldValue::U64(value) => format!("{value}u64"),
        FieldValue::Usize(value) => format!("{value}usize"),
        FieldValue::Isize(value) => format!("{value}isize"),
        FieldValue::Bool(value) => value.to_string(),
        FieldValue::Array(_)
        | FieldValue::List { .. }
        | FieldValue::Text(_)
        | FieldValue::Opaque { .. } => return None,
    })
}

/// One suffix parser: the digits before a type suffix, read as that type.
type SuffixParser = fn(&str) -> Option<FieldValue>;

/// The [`FieldValue`] a token denotes, or `None` when it is not a token
/// this payload understands.
pub fn parse_value(token: &str) -> Option<FieldValue> {
    match token {
        "true" => return Some(FieldValue::Bool(true)),
        "false" => return Some(FieldValue::Bool(false)),
        _ => {}
    }
    // Longest suffixes first; every other suffix is unambiguous on its own
    // (`usize`/`isize` end in 'e', so no shorter suffix can shadow them).
    const SUFFIXED: [(&str, SuffixParser); 12] = [
        ("usize", |digits| digits.parse().ok().map(FieldValue::Usize)),
        ("isize", |digits| digits.parse().ok().map(FieldValue::Isize)),
        ("u64", |digits| digits.parse().ok().map(FieldValue::U64)),
        ("u32", |digits| digits.parse().ok().map(FieldValue::U32)),
        ("u16", |digits| digits.parse().ok().map(FieldValue::U16)),
        ("u8", |digits| digits.parse().ok().map(FieldValue::U8)),
        ("i64", |digits| digits.parse().ok().map(FieldValue::I64)),
        ("i32", |digits| digits.parse().ok().map(FieldValue::I32)),
        ("i16", |digits| digits.parse().ok().map(FieldValue::I16)),
        ("i8", |digits| digits.parse().ok().map(FieldValue::I8)),
        ("f", |digits| digits.parse().ok().map(FieldValue::F32)),
        ("d", |digits| digits.parse().ok().map(FieldValue::F64)),
    ];
    for (suffix, parse) in SUFFIXED {
        if let Some(digits) = token.strip_suffix(suffix) {
            if !digits.is_empty() && digits != "-" {
                if let Some(value) = parse(digits) {
                    return Some(value);
                }
            }
        }
    }
    None
}

/// The payload document for one entity's detail: every editable component
/// with the fields the payload can carry.
pub fn encode_entity(detail: &EntityDetail) -> String {
    let mut components: Vec<Value> = Vec::new();
    for component in &detail.components {
        if !component.editable {
            continue;
        }
        components.push(component_document(
            &component.type_name,
            &component_field_pairs(component),
        ));
    }
    let document = json!({
        "pill": VERSION,
        "kind": "entity",
        "components": components,
    });
    document.to_string()
}

/// The payload document for one component's current values.
pub fn encode_component(type_name: &str, fields: &[(String, FieldValue)]) -> String {
    let document = json!({
        "pill": VERSION,
        "kind": "component",
        "component": component_document(type_name, fields),
    });
    document.to_string()
}

/// The payload document for one field value.
pub fn encode_field(component: &str, field: &str, value: &FieldValue) -> Option<String> {
    let token = format_value(value)?;
    let document = json!({
        "pill": VERSION,
        "kind": "field",
        "component": component,
        "field": field,
        "value": token,
    });
    Some(document.to_string())
}

/// The payload document for one asset reference.
pub fn encode_asset(path: &str) -> String {
    let document = json!({
        "pill": VERSION,
        "kind": "asset",
        "path": path,
    });
    document.to_string()
}

/// Decode a payload document; an error names what did not parse.
pub fn decode(text: &str) -> Result<Payload, String> {
    let document: Value =
        serde_json::from_str(text).map_err(|error| format!("payload is not JSON: {error}"))?;
    let version = document.get("pill").and_then(Value::as_u64);
    if version != Some(VERSION) {
        return Err(format!(
            "payload version {} is not {VERSION}",
            version.map_or("missing".to_string(), |value| value.to_string())
        ));
    }
    let kind = document
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "payload has no kind".to_string())?;
    match kind {
        "entity" => {
            let components = document
                .get("components")
                .and_then(Value::as_array)
                .ok_or_else(|| "entity payload has no components".to_string())?;
            let mut seeds = Vec::with_capacity(components.len());
            for component in components {
                seeds.push(decode_component(component)?);
            }
            Ok(Payload::Entity { components: seeds })
        }
        "component" => {
            let component = document
                .get("component")
                .ok_or_else(|| "component payload has no component".to_string())?;
            Ok(Payload::Component {
                component: decode_component(component)?,
            })
        }
        "field" => {
            let component = string_field(&document, "component")?;
            let field = string_field(&document, "field")?;
            let value = string_field(&document, "value")?;
            Ok(Payload::Field {
                component,
                field,
                value,
            })
        }
        "asset" => Ok(Payload::Asset {
            path: string_field(&document, "path")?,
        }),
        other => Err(format!("unknown payload kind `{other}`")),
    }
}

/// The field pairs a payload carries for one component view.
pub fn component_field_pairs(
    component: &crate::editor_state::ComponentView,
) -> Vec<(String, FieldValue)> {
    component
        .fields
        .iter()
        .zip(component.values.iter())
        .map(|(descriptor, value)| (descriptor.name.to_string(), value.clone()))
        .collect()
}

/// The JSON object of one component: type name and the carryable fields.
fn component_document(type_name: &str, fields: &[(String, FieldValue)]) -> Value {
    let mut carried: Vec<Value> = Vec::new();
    for (name, value) in fields {
        if let Some(token) = format_value(value) {
            carried.push(json!([name, token]));
        }
    }
    json!({ "type": type_name, "fields": carried })
}

/// Decode one component object into its seed; an unparseable field is an
/// error here (the caller reports it), because a silently dropped field
/// would make a paste quietly lossy.
fn decode_component(component: &Value) -> Result<SeededComponent, String> {
    let type_name = component
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "component has no type".to_string())?
        .to_string();
    let mut fields: Vec<(String, String)> = Vec::new();
    if let Some(entries) = component.get("fields").and_then(Value::as_array) {
        for entry in entries {
            let pair = entry
                .as_array()
                .filter(|pair| pair.len() == 2)
                .ok_or_else(|| format!("field entry of `{type_name}` is not a pair"))?;
            let name = pair[0]
                .as_str()
                .ok_or_else(|| format!("field entry of `{type_name}` has no name"))?
                .to_string();
            let token = pair[1]
                .as_str()
                .ok_or_else(|| format!("field `{name}` of `{type_name}` has no value"))?
                .to_string();
            fields.push((name, token));
        }
    }
    Ok(SeededComponent { type_name, fields })
}

/// One required string member of a document.
fn string_field(document: &Value, member: &str) -> Result<String, String> {
    document
        .get(member)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("payload has no `{member}`"))
}

/// A compact one-line summary of a payload, for log lines.
pub fn summarize(payload: &Payload) -> String {
    let mut summary = String::new();
    match payload {
        Payload::Entity { components } => {
            let _ = write!(summary, "entity with {} component(s)", components.len());
            for component in components {
                let _ = write!(summary, ", {}", component.type_name);
            }
        }
        Payload::Component { component } => {
            let _ = write!(
                summary,
                "component {} with {} field(s)",
                component.type_name,
                component.fields.len()
            );
        }
        Payload::Field {
            component,
            field,
            value,
        } => {
            let _ = write!(summary, "field {component}.{field} = {value}");
        }
        Payload::Asset { path } => {
            let _ = write!(summary, "asset {path}");
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_state::{ComponentView, EntityDetail};
    use pill_engine::component_registry::ComponentFieldDescriptor;

    fn descriptor(name: &'static str) -> ComponentFieldDescriptor {
        ComponentFieldDescriptor {
            name,
            type_tag: "f32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 1,
        }
    }

    fn detail() -> EntityDetail {
        // A real handle from a real world; the payload only carries its
        // components, but a test that invents handles tests nothing.
        let mut engine = pill_engine::Engine::new();
        let entity = engine
            .world_mut()
            .create_entity()
            .build()
            .expect("an empty entity builds");
        EntityDetail {
            entity,
            components: vec![
                ComponentView {
                    type_name: "Position".to_string(),
                    persistable: true,
                    editable: true,
                    fields: vec![descriptor("x"), descriptor("y")],
                    values: vec![FieldValue::F32(1.5), FieldValue::F32(-2.25)],
                },
                ComponentView {
                    type_name: "Opaque".to_string(),
                    persistable: false,
                    editable: false,
                    fields: Vec::new(),
                    values: Vec::new(),
                },
            ],
        }
    }

    #[test]
    fn an_entity_round_trips_through_the_payload() {
        let text = encode_entity(&detail());
        match decode(&text).expect("round trip") {
            Payload::Entity { components } => {
                assert_eq!(components.len(), 1, "only editable components are carried");
                assert_eq!(components[0].type_name, "Position");
                assert_eq!(
                    components[0].fields,
                    vec![
                        ("x".to_string(), "1.5f".to_string()),
                        ("y".to_string(), "-2.25f".to_string()),
                    ]
                );
            }
            other => panic!("unexpected payload {other:?}"),
        }
    }

    #[test]
    fn every_scalar_token_round_trips() {
        let values = [
            FieldValue::F32(1.5),
            FieldValue::F64(-0.125),
            FieldValue::I8(-8),
            FieldValue::I16(-16),
            FieldValue::I32(-32),
            FieldValue::I64(-64),
            FieldValue::U8(8),
            FieldValue::U16(16),
            FieldValue::U32(32),
            FieldValue::U64(64),
            FieldValue::Usize(4),
            FieldValue::Isize(-4),
            FieldValue::Bool(true),
            FieldValue::Bool(false),
        ];
        for value in values {
            let token = format_value(&value).expect("scalar formats");
            assert_eq!(parse_value(&token), Some(value.clone()), "token {token}");
        }
        for value in [
            FieldValue::Array(Vec::new()),
            FieldValue::List {
                element_tag: "f32",
                element_count: 3,
            },
            FieldValue::Text("hi".to_string()),
        ] {
            assert_eq!(format_value(&value), None);
        }
    }

    #[test]
    fn versions_and_kinds_are_refused_by_name() {
        assert!(decode("not json").is_err());
        assert!(decode(r#"{"pill": 99, "kind": "entity", "components": []}"#).is_err());
        assert!(decode(r#"{"pill": 1, "kind": "mystery"}"#).is_err());
        let field = encode_field("Position", "x", &FieldValue::F32(2.0)).expect("carried");
        assert!(matches!(decode(&field), Ok(Payload::Field { .. })));
    }
}
