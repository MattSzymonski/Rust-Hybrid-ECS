//! Inspector panel: components and editable fields of the selected entity.
//!
//! # Responsibilities
//!
//! - Render the selected entity's components with editable scalar and array
//!   element fields, driven entirely by the engine's generic field API.
//! - Offer add/remove component actions backed by the registered layouts.
//!
//! The snapshot's `detail` tier already carries the owned field descriptors
//! and current values; this panel only renders them and turns user edits into
//! [`EditorCommand`]s. Nothing here knows a concrete component type, and every
//! value round-trips through the engine's generic field API.

use std::sync::Arc;
use std::time::Duration;

use dioxus::prelude::*;
use pill_engine::{Entity, FieldValue};

use crate::editor_state::{EditorCommand, EditorSnapshot, RegisteredComponent};
use crate::EditorContext;

/// Poll interval for the local snapshot copy.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Human text for a field value, for read-only display.
fn field_value_text(value: &FieldValue) -> String {
    match value {
        FieldValue::F32(v) => format!("{v:.4}"),
        FieldValue::F64(v) => format!("{v:.4}"),
        FieldValue::I8(v) => v.to_string(),
        FieldValue::I16(v) => v.to_string(),
        FieldValue::I32(v) => v.to_string(),
        FieldValue::I64(v) => v.to_string(),
        FieldValue::U8(v) => v.to_string(),
        FieldValue::U16(v) => v.to_string(),
        FieldValue::U32(v) => v.to_string(),
        FieldValue::U64(v) => v.to_string(),
        FieldValue::Usize(v) => v.to_string(),
        FieldValue::Isize(v) => v.to_string(),
        FieldValue::Bool(v) => v.to_string(),
        FieldValue::Array(values) => {
            format!(
                "[{}]",
                values
                    .iter()
                    .map(field_value_text)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        FieldValue::Opaque { type_tag, bytes } => {
            format!("{type_tag} ({} bytes)", bytes.len())
        }
    }
}

/// Parse a text input into the scalar variant a tag names.
fn parse_scalar(tag: &str, text: &str) -> Option<FieldValue> {
    let text = text.trim();
    Some(match tag {
        "f32" => FieldValue::F32(text.parse().ok()?),
        "f64" => FieldValue::F64(text.parse().ok()?),
        "i8" => FieldValue::I8(text.parse().ok()?),
        "i16" => FieldValue::I16(text.parse().ok()?),
        "i32" => FieldValue::I32(text.parse().ok()?),
        "i64" => FieldValue::I64(text.parse().ok()?),
        "u8" => FieldValue::U8(text.parse().ok()?),
        "u16" => FieldValue::U16(text.parse().ok()?),
        "u32" => FieldValue::U32(text.parse().ok()?),
        "u64" => FieldValue::U64(text.parse().ok()?),
        "usize" => FieldValue::Usize(text.parse().ok()?),
        "isize" => FieldValue::Isize(text.parse().ok()?),
        _ => return None,
    })
}

/// Whether a tag gets an active numeric/bool editor in the generic inspector.
fn is_scalar_editable(tag: &str) -> bool {
    matches!(
        tag,
        "f32"
            | "f64"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "usize"
            | "isize"
            | "bool"
    )
}

/// The Inspector panel body.
#[component]
pub(crate) fn InspectorTab(editor: Arc<EditorContext>) -> Element {
    let mut snapshot = use_signal(EditorSnapshot::default);

    let poll_editor = Arc::clone(&editor);
    use_future(move || {
        let poll_editor = Arc::clone(&poll_editor);
        async move {
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                snapshot.set(poll_editor.snapshot());
            }
        }
    });

    // Registered component picker, refetched on the poll cadence.
    let mut registered = use_signal(Vec::<RegisteredComponent>::new);
    let register_editor = Arc::clone(&editor);
    use_future(move || {
        let register_editor = Arc::clone(&register_editor);
        async move {
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                registered.set(register_editor.registered_components());
            }
        }
    });

    let detail = snapshot.read().detail.clone();
    let Some(detail) = detail else {
        return rsx! {
            div { class: "editor-panel editor-placeholder", "Select an entity to inspect." }
        };
    };

    // One editable field row of the Inspector. Every flag/string is precomputed
    // so the rsx below stays declarative and closures capture only owned
    // clones. Array fields are flattened into one element sub-row per element,
    // each writing through `SetArrayElement`.
    #[derive(Clone)]
    struct FieldRow {
        key: String,
        name: String,
        entity: Entity,
        component: String,
        is_bool: bool,
        bool_value: bool,
        is_scalar: bool,
        scalar_text: String,
        scalar_tag: String,
        is_array_element: bool,
        array_field: String,
        element_index: usize,
        read_text: String,
    }

    // One component section: a header plus its field rows.
    struct Section {
        title: String,
        tags: String,
        has_layout: bool,
        rows: Vec<FieldRow>,
    }

    let sections: Vec<Section> = detail
        .components
        .iter()
        .map(|component| {
            let tags = if component.persistable && !component.editable {
                "persistable · read-only"
            } else if component.persistable {
                "persistable"
            } else if !component.editable {
                "read-only"
            } else {
                ""
            };
            let mut rows = Vec::new();
            if component.editable {
                for (index, field) in component.fields.iter().enumerate() {
                    let value = component.values.get(index).cloned();
                    let empty = FieldRow {
                        key: format!("field-{}-{}", component.type_name, field.name),
                        name: field.name.to_string(),
                        entity: detail.entity,
                        component: component.type_name.clone(),
                        is_bool: false,
                        bool_value: false,
                        is_scalar: false,
                        scalar_text: String::new(),
                        scalar_tag: String::new(),
                        is_array_element: false,
                        array_field: String::new(),
                        element_index: 0,
                        read_text: String::new(),
                    };
                    if let Some(inner) = field.type_tag.strip_prefix("array:") {
                        // One sub-row per element; empty arrays degrade to a
                        // read-only "[]" line.
                        match &value {
                            Some(FieldValue::Array(elements)) if !elements.is_empty() => {
                                for (element_index, element) in elements.iter().enumerate() {
                                    rows.push(FieldRow {
                                        name: format!("{}[{element_index}]", field.name),
                                        is_scalar: is_scalar_editable(inner),
                                        scalar_text: field_value_text(element),
                                        scalar_tag: inner.to_string(),
                                        is_array_element: true,
                                        array_field: field.name.to_string(),
                                        element_index,
                                        ..empty.clone()
                                    });
                                }
                            }
                            _ => {
                                rows.push(FieldRow {
                                    read_text: "[]".to_string(),
                                    ..empty.clone()
                                });
                            }
                        }
                    } else if field.type_tag == "bool" {
                        rows.push(FieldRow {
                            is_bool: true,
                            bool_value: matches!(value, Some(FieldValue::Bool(true))),
                            ..empty.clone()
                        });
                    } else if is_scalar_editable(field.type_tag) {
                        rows.push(FieldRow {
                            is_scalar: true,
                            scalar_text: value.as_ref().map(field_value_text).unwrap_or_default(),
                            scalar_tag: field.type_tag.to_string(),
                            ..empty.clone()
                        });
                    } else {
                        rows.push(FieldRow {
                            read_text: value.as_ref().map(field_value_text).unwrap_or_default(),
                            ..empty.clone()
                        });
                    }
                }
            }
            Section {
                title: component.type_name.clone(),
                tags: tags.to_string(),
                has_layout: component.editable,
                rows,
            }
        })
        .collect();

    let registered_options = registered.read().clone();
    rsx! {
        div {
            class: "editor-panel editor-inspector",
            div {
                class: "editor-panel-title",
                "Entity {detail.entity.id()}v{detail.entity.generation()}"
            }
            select {
                class: "editor-select",
                onchange: {
                    let editor = Arc::clone(&editor);
                    let entity = detail.entity;
                    move |event| {
                        let type_name = event.value();
                        if !type_name.is_empty() {
                            editor.push_command(EditorCommand::AddComponent {
                                entity,
                                component: type_name,
                            });
                        }
                    }
                },
                option { value: "", "Add component…" }
                for option_row in &registered_options {
                    option {
                        value: "{option_row.type_name}",
                        disabled: !option_row.addable,
                        title: if option_row.addable {
                            "Adds a zero-initialised component."
                        } else {
                            "No field layout; cannot be safely zero-initialised."
                        },
                        "{option_row.type_name}"
                    }
                }
            }
            for section in sections {
                div {
                    class: "editor-component",
                    key: "component-{section.title}",
                    div {
                        class: "editor-component-head",
                        "{section.title}",
                        if !section.tags.is_empty() {
                            span { class: "editor-tag", "{section.tags}" }
                        }
                        button {
                            class: "editor-icon-button",
                            title: "Remove component (removing the last one deletes the entity)",
                            onclick: {
                                let editor = Arc::clone(&editor);
                                let entity = detail.entity;
                                let component = section.title.clone();
                                move |_| {
                                    editor.push_command(EditorCommand::RemoveComponent {
                                        entity,
                                        component: component.clone(),
                                    });
                                }
                            },
                            "Remove"
                        }
                    }
                    if !section.has_layout {
                        div { class: "editor-placeholder", "No editable fields." }
                    } else {
                        for row in section.rows {
                            div {
                                class: "editor-field",
                                key: "{row.key}",
                                label { class: "editor-field-name", "{row.name}" }
                                if row.is_bool {
                                    input {
                                        class: "editor-checkbox",
                                        r#type: "checkbox",
                                        checked: row.bool_value,
                                        onchange: {
                                            let editor = Arc::clone(&editor);
                                            let entity = row.entity;
                                            let component = row.component.clone();
                                            let field = row.name.clone();
                                            let next = !row.bool_value;
                                            move |_| {
                                                editor.push_command(EditorCommand::SetField {
                                                    entity,
                                                    component: component.clone(),
                                                    field: field.clone(),
                                                    value: FieldValue::Bool(next),
                                                });
                                            }
                                        },
                                    }
                                } else if row.is_scalar {
                                    input {
                                        class: "editor-number",
                                        r#type: "text",
                                        value: row.scalar_text,
                                        onchange: {
                                            let editor = Arc::clone(&editor);
                                            let entity = row.entity;
                                            let component = row.component.clone();
                                            let field = row.name.clone();
                                            let array_field = row.array_field.clone();
                                            let element_index = row.element_index;
                                            let is_array_element = row.is_array_element;
                                            let type_tag = row.scalar_tag.clone();
                                            move |event| {
                                                if let Some(parsed) =
                                                    parse_scalar(&type_tag, &event.value())
                                                {
                                                    if is_array_element {
                                                        editor.push_command(
                                                            EditorCommand::SetArrayElement {
                                                                entity,
                                                                component: component.clone(),
                                                                field: array_field.clone(),
                                                                index: element_index,
                                                                value: parsed,
                                                            },
                                                        );
                                                    } else {
                                                        editor.push_command(EditorCommand::SetField {
                                                            entity,
                                                            component: component.clone(),
                                                            field: field.clone(),
                                                            value: parsed,
                                                        });
                                                    }
                                                }
                                            }
                                        },
                                    }
                                } else {
                                    span {
                                        class: "editor-field-value",
                                        "{row.read_text}"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
