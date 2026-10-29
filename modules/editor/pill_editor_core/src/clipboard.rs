//! The editor clipboard's structured payloads.
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
use crate::editor_state::{
    ComponentSeed, EditorCommand, EditorSelection, EditorSnapshot, EntityDetail,
};

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
/// cannot carry (arrays, heap lists, text, opaque payloads, and asset or
/// entity references, which address a slot of this process only).
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
        | FieldValue::Asset { .. }
        | FieldValue::Entity { .. }
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

/// The clipboard commands the Edit menu and the shortcuts name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardAction {
    /// Put the selection on the clipboard.
    Copy,
    /// Put the selection on the clipboard and delete it.
    Cut,
    /// Read the clipboard and apply it.
    Paste,
}

/// What the executable should do with the OS clipboard.
///
/// The shell cannot touch the OS clipboard: it is a reloadable DLL, and the
/// Win32 half belongs to the executable (the windows-crate rule). It queues
/// one of these instead, and the executable carries it out at the frame
/// boundary - the same shape `PanelCommand` and `PendingEditorCommands`
/// follow.
#[derive(Clone, Debug, PartialEq)]
pub enum ClipboardRequest {
    /// Put this payload text on the clipboard, as the registered format and
    /// as plain text.
    Write(String),
    /// Read the clipboard and apply it as a paste.
    Read,
}

/// The shell's side of the clipboard: what it asked the executable to do, and
/// the text the executable read back.
#[derive(Default)]
pub struct EditorClipboard {
    /// The request waiting for the executable, taken at the frame boundary.
    pub request: Option<ClipboardRequest>,
    /// The last payload the clipboard carried, as the executable read it.
    pub text: String,
}

impl pill_engine::Resource for EditorClipboard {}

impl EditorClipboard {
    /// Take the waiting request, if one is.
    pub fn take_request(&mut self) -> Option<ClipboardRequest> {
        self.request.take()
    }
}

/// Build the payload text a copy cuts out of the current selection.
///
/// An entity wins over an asset when both slots hold - the entity is what the
/// Inspector shows - and an empty selection is `None`.
pub fn selection_payload(selection: &EditorSelection, snapshot: &EditorSnapshot) -> Option<String> {
    if let Some(detail) = snapshot.detail.as_ref() {
        return Some(encode_entity(detail));
    }
    if let Some(path) = selection.asset.as_deref() {
        return Some(encode_asset(path));
    }
    None
}

/// Whether the selection carries something a copy would write.
///
/// The boolean form of [`selection_payload`], and the one the Edit menu's Copy
/// and Cut rows read: the row is offered exactly when the copy it runs would
/// carry something. A selection whose detail has not been captured yet - the
/// entity is selected but the snapshot still describes the last one - has no
/// payload, so the row greys out instead of copying the asset the selection
/// replaced.
pub fn can_copy(selection: &EditorSelection, snapshot: &EditorSnapshot) -> bool {
    selection_payload(selection, snapshot).is_some()
}

/// Queue one clipboard command from the shell.
///
/// A copy writes the selection's payload; a cut writes it and queues the
/// delete as the same recorded step the Entities tab's trash button queues; a
/// paste asks the executable to read the clipboard. Returns the payload's
/// one-line summary for the caller's log, or `None` when there is nothing to
/// copy.
pub fn queue_action(
    clipboard: &mut EditorClipboard,
    action: ClipboardAction,
    selection: &EditorSelection,
    snapshot: &EditorSnapshot,
    pending: &mut crate::commands::PendingEditorCommands,
) -> Option<String> {
    let summary = queue_request(clipboard, action, selection, snapshot);
    if action == ClipboardAction::Cut {
        if let Some((commands, label)) = cut_commands(selection) {
            pending.push_recorded(commands, label, None);
        }
    }
    summary
}

/// Queue the request alone: the write a copy or a cut needs, or the read a
/// paste needs.
///
/// Split from [`queue_action`] because the executable reaches the two
/// resources behind separate borrows: the request belongs to the clipboard
/// resource, the recorded delete to the pending-command queue.
pub fn queue_request(
    clipboard: &mut EditorClipboard,
    action: ClipboardAction,
    selection: &EditorSelection,
    snapshot: &EditorSnapshot,
) -> Option<String> {
    match action {
        ClipboardAction::Copy | ClipboardAction::Cut => {
            let text = selection_payload(selection, snapshot)?;
            let summary = decode(&text)
                .map(|payload| summarize(&payload))
                .unwrap_or_else(|_| "payload".to_string());
            clipboard.request = Some(ClipboardRequest::Write(text));
            Some(summary)
        }
        ClipboardAction::Paste => {
            clipboard.request = Some(ClipboardRequest::Read);
            Some("read".to_string())
        }
    }
}

/// The recorded step a cut queues: the same delete the Entities tab's trash
/// button queues, so a cut undoes back to the entity.
pub fn cut_commands(selection: &EditorSelection) -> Option<(Vec<EditorCommand>, String)> {
    let entity = selection.primary()?;
    Some(crate::entities::entity_delete(
        entity,
        selection.asset.clone(),
    ))
}

/// The recorded step one paste produces, and what it could not carry.
#[derive(Clone, Debug, PartialEq)]
pub struct PastePlan {
    /// The commands, in the order the batch applies them.
    pub commands: Vec<EditorCommand>,
    /// The history label the step is recorded under.
    pub label: String,
    /// Component type names the paste skipped because nothing in this build
    /// registers them; the caller reports them by name.
    pub skipped: Vec<String>,
    /// A note the step leaves behind for the caller (an asset paste that only
    /// moved the selection).
    pub note: String,
}

/// Build the recorded step a paste applies: one `CreateEntity` for an entity
/// payload, `AddComponent` plus `SetField`s for a component, one `SetField`
/// for a field, and no command for an asset (it only moves the selection).
///
/// A component this build does not register (or cannot add) is skipped and
/// named in [`PastePlan::skipped`]; a field the live layout does not carry is
/// skipped the same way, because a paste across two versions of a component
/// must not write garbage into a reordered image.
pub fn paste_plan(
    payload: &Payload,
    selection: &EditorSelection,
    snapshot: &EditorSnapshot,
) -> Result<PastePlan, String> {
    let registered = |type_name: &str| {
        snapshot
            .components
            .iter()
            .any(|entry| entry.type_name == type_name && entry.addable)
    };
    let mut skipped = Vec::new();
    match payload {
        Payload::Entity { components } => {
            let mut seeds = Vec::with_capacity(components.len());
            for component in components {
                if !registered(&component.type_name) {
                    skipped.push(component.type_name.clone());
                    continue;
                }
                seeds.push(seeded_component(component, &mut skipped));
            }
            Ok(PastePlan {
                commands: vec![EditorCommand::CreateEntity { components: seeds }],
                label: "paste entity".to_string(),
                skipped,
                note: String::new(),
            })
        }
        Payload::Component { component } => {
            if !registered(&component.type_name) {
                return Err(format!(
                    "no component named `{}` is loaded",
                    component.type_name
                ));
            }
            let Some(entity) = selection.primary() else {
                return Err("paste a component onto a selected entity".to_string());
            };
            // An entity that already carries the component takes the payload's
            // values over it; the `AddComponent` is only for one that does not,
            // because adding a component twice fails and would take the whole
            // step's recording with it.
            let carries = snapshot
                .selected_details
                .iter()
                .chain(snapshot.detail.iter())
                .find(|detail| detail.entity == entity)
                .is_some_and(|detail| {
                    detail
                        .components
                        .iter()
                        .any(|view| view.type_name == component.type_name)
                });
            let mut commands = Vec::new();
            if !carries {
                commands.push(EditorCommand::AddComponent {
                    entity,
                    component: component.type_name.clone(),
                });
            }
            for (field, token) in &component.fields {
                let Some(value) = parse_value(token) else {
                    skipped.push(format!("{}.{}", component.type_name, field));
                    continue;
                };
                commands.push(EditorCommand::SetField {
                    entity,
                    component: component.type_name.clone(),
                    field: field.clone(),
                    value,
                });
            }
            Ok(PastePlan {
                commands,
                label: format!("paste {}", component.type_name),
                skipped,
                note: String::new(),
            })
        }
        Payload::Field {
            component,
            field,
            value,
        } => {
            let Some(entity) = selection.primary() else {
                return Err("paste a value onto a selected entity".to_string());
            };
            let Some(value) = parse_value(value) else {
                return Err(format!("`{value}` is not a value this build parses"));
            };
            Ok(PastePlan {
                commands: vec![EditorCommand::SetField {
                    entity,
                    component: component.clone(),
                    field: field.clone(),
                    value,
                }],
                label: format!("paste {component}.{field}"),
                skipped,
                note: String::new(),
            })
        }
        Payload::Asset { path } => Ok(PastePlan {
            commands: Vec::new(),
            label: "paste asset".to_string(),
            skipped,
            note: format!("asset {path}"),
        }),
    }
}

/// One payload component as the seed a create or an add rebuilds.
///
/// A field whose token does not parse is left out and named in `skipped`,
/// exactly as the single-component paste names it: a paste across two versions
/// of a component creates the entity with the fields it could read and the
/// component's defaults for the rest, and the person can see which.
fn seeded_component(component: &SeededComponent, skipped: &mut Vec<String>) -> ComponentSeed {
    let mut fields = Vec::with_capacity(component.fields.len());
    for (field, token) in &component.fields {
        match parse_value(token) {
            Some(value) => fields.push((field.clone(), value)),
            None => skipped.push(format!("{}.{}", component.type_name, field)),
        }
    }
    ComponentSeed {
        type_name: component.type_name.clone(),
        fields,
    }
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
    use pill_engine::Entity;

    fn descriptor(name: &'static str) -> ComponentFieldDescriptor {
        ComponentFieldDescriptor {
            name,
            type_tag: "f32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 1,
            decorators: std::borrow::Cow::Borrowed(&[]),
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
                    asset_labels: Vec::new(),
                    entity_labels: Vec::new(),
                    type_name: "Position".to_string(),
                    persistable: true,
                    editable: true,
                    fields: vec![descriptor("x"), descriptor("y")],
                    values: vec![FieldValue::F32(1.5), FieldValue::F32(-2.25)],
                },
                ComponentView {
                    asset_labels: Vec::new(),
                    entity_labels: Vec::new(),
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

    /// A snapshot whose registry knows `names` and nothing else.
    fn snapshot_with(names: &[&str]) -> EditorSnapshot {
        EditorSnapshot {
            components: names
                .iter()
                .map(|name| crate::editor_state::RegisteredComponent {
                    type_name: name.to_string(),
                    addable: true,
                })
                .collect(),
            ..EditorSnapshot::default()
        }
    }

    /// A selection holding exactly one entity.
    fn selection_with(entity: Entity) -> EditorSelection {
        let mut selection = EditorSelection::default();
        selection.select_only(entity);
        selection
    }

    /// Pasting an entity creates one, seeded with what the payload carried, and
    /// names the components this build does not register.
    #[test]
    fn an_entity_paste_seeds_a_create_and_names_what_it_skipped() {
        let text = concat!(
            r#"{"pill": 1, "kind": "entity", "components": ["#,
            r#"{"type": "Position", "fields": [["x", "1.5f"]]},"#,
            r#"{"type": "Gone", "fields": []}]}"#
        );
        let payload = decode(text).expect("a well-formed payload");
        let plan = paste_plan(
            &payload,
            &selection_with(Entity::from_raw(1, 0)),
            &snapshot_with(&["Position"]),
        )
        .expect("the paste plans");
        assert_eq!(plan.skipped, vec!["Gone".to_string()]);
        assert_eq!(plan.commands.len(), 1);
        match &plan.commands[0] {
            EditorCommand::CreateEntity { components } => {
                assert_eq!(components.len(), 1, "the unregistered component is skipped");
                assert_eq!(components[0].type_name, "Position");
                assert_eq!(
                    components[0].fields,
                    vec![("x".to_string(), FieldValue::F32(1.5))]
                );
            }
            other => panic!("an entity paste queued {other:?}"),
        }
    }

    /// An entity paste whose component carries a token this build cannot parse
    /// names the field it dropped, exactly as the single-component paste does:
    /// a lossy paste says what it lost.
    #[test]
    fn an_entity_paste_names_the_field_it_could_not_read() {
        let text = concat!(
            r#"{"pill": 1, "kind": "entity", "components": ["#,
            r#"{"type": "Demo", "fields": [["first", "1.5f"], ["second", "nonsense"]]}]}"#
        );
        let payload = decode(text).expect("a well-formed payload");
        let plan = paste_plan(
            &payload,
            &selection_with(Entity::from_raw(1, 0)),
            &snapshot_with(&["Demo"]),
        )
        .expect("the paste plans");
        assert_eq!(plan.skipped, vec!["Demo.second".to_string()]);
        match &plan.commands[0] {
            EditorCommand::CreateEntity { components } => {
                assert_eq!(
                    components[0].fields,
                    vec![("first".to_string(), FieldValue::F32(1.5))],
                    "the unparsable field is left out of the seed"
                );
            }
            other => panic!("an entity paste queued {other:?}"),
        }
    }

    /// A component paste is `AddComponent` plus one `SetField` per field: one
    /// recorded step, undone in one.
    #[test]
    fn a_component_paste_is_one_step() {
        let entity = Entity::from_raw(2, 0);
        let text = encode_component(
            "Position",
            &[
                ("x".to_string(), FieldValue::F32(4.0)),
                ("y".to_string(), FieldValue::F32(5.0)),
            ],
        );
        let payload = decode(&text).expect("a well-formed payload");
        let plan = paste_plan(
            &payload,
            &selection_with(entity),
            &snapshot_with(&["Position"]),
        )
        .expect("the paste plans");
        assert_eq!(plan.label, "paste Position");
        assert_eq!(plan.commands.len(), 3);
        assert!(matches!(
            plan.commands[0],
            EditorCommand::AddComponent { entity: queued, .. } if queued == entity
        ));
        let fields: Vec<&str> = plan
            .commands
            .iter()
            .skip(1)
            .filter_map(|command| match command {
                EditorCommand::SetField { field, .. } => Some(field.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(fields, vec!["x", "y"]);
    }

    /// An entity that already carries the component takes the values over it:
    /// no `AddComponent`, which would fail and cost the step its recording.
    #[test]
    fn a_component_paste_over_a_carried_component_only_writes_fields() {
        let entity = Entity::from_raw(5, 0);
        let mut snapshot = snapshot_with(&["Position"]);
        snapshot.detail = Some(EntityDetail {
            entity,
            components: vec![ComponentView {
                asset_labels: Vec::new(),
                entity_labels: Vec::new(),
                type_name: "Position".to_string(),
                persistable: true,
                editable: true,
                fields: vec![descriptor("x")],
                values: vec![FieldValue::F32(1.0)],
            }],
        });
        let text = encode_component("Position", &[("x".to_string(), FieldValue::F32(9.0))]);
        let payload = decode(&text).expect("a well-formed payload");
        let plan =
            paste_plan(&payload, &selection_with(entity), &snapshot).expect("the paste plans");
        assert_eq!(plan.commands.len(), 1);
        assert!(matches!(
            plan.commands[0],
            EditorCommand::SetField { entity: queued, .. } if queued == entity
        ));
    }

    /// A component paste needs a selected entity, and says so.
    #[test]
    fn a_component_paste_without_a_selection_is_refused() {
        let text = encode_component("Position", &[("x".to_string(), FieldValue::F32(1.0))]);
        let payload = decode(&text).expect("a well-formed payload");
        let error = paste_plan(
            &payload,
            &EditorSelection::default(),
            &snapshot_with(&["Position"]),
        )
        .expect_err("an empty selection cannot take a component");
        assert!(error.contains("selected entity"), "{error}");
    }

    /// A field paste writes the primary alone; an asset paste only moves the
    /// selection, so it queues no command.
    #[test]
    fn a_field_paste_writes_one_field_and_an_asset_paste_writes_none() {
        let entity = Entity::from_raw(3, 0);
        let text = encode_field("Position", "y", &FieldValue::F32(-1.0)).expect("carried");
        let payload = decode(&text).expect("a well-formed payload");
        let plan = paste_plan(
            &payload,
            &selection_with(entity),
            &snapshot_with(&["Position"]),
        )
        .expect("the paste plans");
        assert!(matches!(
            plan.commands.as_slice(),
            [EditorCommand::SetField { entity: queued, field, .. }] if *queued == entity && field == "y"
        ));

        let asset = decode(&encode_asset("res/models/fox.gltf")).expect("a well-formed payload");
        let plan = paste_plan(&asset, &selection_with(entity), &snapshot_with(&[]))
            .expect("the paste plans");
        assert!(plan.commands.is_empty());
        assert!(plan.note.contains("res/models/fox.gltf"), "{}", plan.note);
    }
}
