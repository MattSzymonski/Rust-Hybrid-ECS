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
use std::time::{Duration, Instant};

use dioxus::prelude::*;
use pill_engine::component_registry::ComponentFieldDescriptor;
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

// ---------------------------------------------------------------------------
// Colour groups (renderer sprite tint as a colour picker)
// ---------------------------------------------------------------------------

/// One RGBA colour group discovered inside a component's scalar fields.
///
/// Either an embedded struct flattened with prefixed names (`color.r` …
/// `color.a`, exactly how the engine registers `Sprite`) or a bare `r/g/b/a`
/// set on a component that is itself a colour (`…::Color`). Each channel is an
/// `f32` in 0.0-1.0 written back through the generic scalar API, one
/// `SetField` per channel, so nothing here knows the concrete component type.
#[derive(Clone, PartialEq)]
struct ColorGroup {
    /// Display label, e.g. "color".
    label: String,
    /// Unique row key within the section.
    key: String,
    /// Engine field names backing each channel.
    r_field: String,
    g_field: String,
    b_field: String,
    a_field: Option<String>,
    /// Pre-formatted control values (swatch CSS, #rrggbb hex, alpha text).
    swatch_css: String,
    hex_value: String,
    alpha_text: String,
    /// Target entity and component for the write commands.
    entity: Entity,
    component: String,
}

/// Build a [`ColorGroup`] from channel entries and mark the consumed indices.
///
/// Missing red/green/blue entries (or non-`f32` values) disqualify the group;
/// alpha is optional and defaults to opaque.
fn color_group_from_channels(
    label: String,
    channels: &std::collections::BTreeMap<char, (usize, String)>,
    values: &[FieldValue],
    entity: Entity,
    component: &str,
    consumed: &mut std::collections::BTreeSet<usize>,
) -> Option<ColorGroup> {
    let channel = |name: char| -> Option<(f32, String)> {
        let (index, field_name) = channels.get(&name)?;
        let value = match values.get(*index) {
            Some(FieldValue::F32(value)) => *value,
            _ => return None,
        };
        Some((value, field_name.clone()))
    };
    let (r, r_field) = channel('r')?;
    let (g, g_field) = channel('g')?;
    let (b, b_field) = channel('b')?;
    let (a, a_field) = match channel('a') {
        Some((value, name)) => (value, Some(name)),
        None => (1.0, None),
    };
    for (index, _) in channels.values() {
        consumed.insert(*index);
    }
    let to_byte = |value: f32| -> u8 { (value.clamp(0.0, 1.0) * 255.0).round() as u8 };
    let red = to_byte(r);
    let green = to_byte(g);
    let blue = to_byte(b);
    Some(ColorGroup {
        label: label.clone(),
        key: format!("color-{label}"),
        r_field,
        g_field,
        b_field,
        a_field,
        swatch_css: format!(
            "background: rgba({red},{green},{blue},{:.3});",
            a.clamp(0.0, 1.0)
        ),
        hex_value: format!("#{red:02x}{green:02x}{blue:02x}"),
        alpha_text: format!("{:.2}", a.clamp(0.0, 1.0)),
        entity,
        component: component.to_string(),
    })
}

/// Find colour groups in one component's fields.
///
/// Returns the groups plus the set of field indices they consume so the caller
/// can skip rendering those fields as plain scalar rows.
fn detect_color_groups(
    type_name: &str,
    fields: &[ComponentFieldDescriptor],
    values: &[FieldValue],
    entity: Entity,
    component: String,
) -> (Vec<ColorGroup>, std::collections::BTreeSet<usize>) {
    use std::collections::{BTreeMap, BTreeSet};

    let channel_name = |name: &str| -> Option<char> {
        match name {
            "r" => Some('r'),
            "g" => Some('g'),
            "b" => Some('b'),
            "a" => Some('a'),
            _ => None,
        }
    };

    let mut groups = Vec::new();
    let mut consumed = BTreeSet::new();

    // Pass 1: prefixed groups such as `color.r` … `color.a`, i.e. an embedded
    // struct flattened by the engine layout (the `Sprite` case).
    let mut by_prefix: BTreeMap<String, BTreeMap<char, (usize, String)>> = BTreeMap::new();
    for (index, field) in fields.iter().enumerate() {
        if field.type_tag != "f32" {
            continue;
        }
        let Some((prefix, channel)) = field.name.rsplit_once('.') else {
            continue;
        };
        if prefix.is_empty() {
            continue;
        }
        let Some(channel) = channel_name(channel) else {
            continue;
        };
        by_prefix
            .entry(prefix.to_string())
            .or_default()
            .insert(channel, (index, field.name.to_string()));
    }
    for (prefix, channels) in by_prefix {
        if let Some(group) =
            color_group_from_channels(prefix, &channels, values, entity, &component, &mut consumed)
        {
            groups.push(group);
        }
    }

    // Pass 2: a component that is itself a colour exposes bare `r/g/b/a`
    // scalars (registered `…::Color`); treat them as one group.
    if type_name.ends_with("::Color") && groups.is_empty() {
        let mut channels = BTreeMap::new();
        for (index, field) in fields.iter().enumerate() {
            if field.type_tag == "f32" {
                if let Some(channel) = channel_name(field.name) {
                    channels.insert(channel, (index, field.name.to_string()));
                }
            }
        }
        if let Some(group) = color_group_from_channels(
            "color".to_string(),
            &channels,
            values,
            entity,
            &component,
            &mut consumed,
        ) {
            groups.push(group);
        }
    }

    (groups, consumed)
}

/// Parse a `#rrggbb` hex string into normalized 0.0-1.0 RGB channels.
fn hex_to_rgb(hex: &str) -> Option<(f32, f32, f32)> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((
        red as f32 / 255.0,
        green as f32 / 255.0,
        blue as f32 / 255.0,
    ))
}

// ---------------------------------------------------------------------------
// Field editors with an edit guard
// ---------------------------------------------------------------------------

/// Window after committing an edit during which the field keeps showing the
/// user's value while the engine acknowledges the write. If a running system
/// overwrites the field instead, the guard expires and the engine value is
/// shown again.
const COMMIT_ACKNOWLEDGE_WINDOW: Duration = Duration::from_millis(400);

/// One editable field row of the Inspector. Every flag/string is precomputed
/// so the rsx stays declarative and handlers capture only owned clones. Array
/// fields are flattened into one element sub-row per element, each writing
/// through `SetArrayElement`.
#[derive(Clone, PartialEq)]
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

/// Route one committed scalar value to the engine write matching the row's
/// shape (a whole field or one fixed-array element).
fn push_scalar_commit(editor: &EditorContext, row: &FieldRow, value: FieldValue) {
    if row.is_array_element {
        editor.push_command(EditorCommand::SetArrayElement {
            entity: row.entity,
            component: row.component.clone(),
            field: row.array_field.clone(),
            index: row.element_index,
            value,
        });
    } else {
        editor.push_command(EditorCommand::SetField {
            entity: row.entity,
            component: row.component.clone(),
            field: row.name.clone(),
            value,
        });
    }
}

/// Whether the engine's canonical field text equals the canonical text of the
/// user's committed draft, i.e. the engine has acknowledged the write.
fn commit_matches_engine(row: &FieldRow, draft_text: &str) -> bool {
    match parse_scalar(&row.scalar_tag, draft_text) {
        Some(value) => field_value_text(&value) == row.scalar_text,
        None => false,
    }
}

/// Commit the focused draft of a numeric row: send the write and keep the
/// draft on screen until the engine acknowledges it, or drop the edit and
/// revert to the engine value when the draft does not parse.
fn commit_numeric_draft(
    editor: &EditorContext,
    row: &FieldRow,
    mut draft: Signal<String>,
    mut focused: Signal<bool>,
    mut acknowledge_until: Signal<Option<Instant>>,
) {
    let text = draft.read().clone();
    match parse_scalar(&row.scalar_tag, &text) {
        Some(value) => {
            push_scalar_commit(editor, row, value);
            focused.set(false);
            acknowledge_until.set(Some(Instant::now() + COMMIT_ACKNOWLEDGE_WINDOW));
        }
        None => {
            // Unparseable edit: revert the field to the engine value.
            draft.set(row.scalar_text.clone());
            focused.set(false);
            acknowledge_until.set(None);
        }
    }
}

/// Numeric (scalar or array-element) editor whose engine-driven value is
/// paused while the user edits.
///
/// While the input is focused, snapshot refreshes never clobber the text being
/// typed. Committing on Enter or blur sends the write and keeps the typed
/// value visible for a short acknowledgement window; the canonical engine text
/// takes over as soon as the snapshot reflects the write, or after the window
/// when a system overwrote the field instead.
#[component]
fn NumericFieldEditor(row: FieldRow, editor: Arc<EditorContext>) -> Element {
    let mut draft = use_signal(|| row.scalar_text.clone());
    let mut focused = use_signal(|| false);
    let mut acknowledge_until = use_signal(|| None::<Instant>);

    let awaiting_acknowledgement = match *acknowledge_until.read() {
        Some(deadline) => {
            Instant::now() < deadline && !commit_matches_engine(&row, &draft.read().clone())
        }
        None => false,
    };
    let show_draft = *focused.read() || awaiting_acknowledgement;
    let display_text = if show_draft {
        draft.read().clone()
    } else {
        row.scalar_text.clone()
    };

    rsx! {
        input {
            class: "editor-number",
            r#type: "text",
            value: "{display_text}",
            onfocus: {
                let engine_text = row.scalar_text.clone();
                move |_| {
                    draft.set(engine_text.clone());
                    focused.set(true);
                    acknowledge_until.set(None);
                }
            },
            oninput: move |event| {
                draft.set(event.value());
                focused.set(true);
            },
            onchange: {
                let editor = Arc::clone(&editor);
                let row = row.clone();
                move |_| commit_numeric_draft(&editor, &row, draft, focused, acknowledge_until)
            },
            onblur: {
                let editor = Arc::clone(&editor);
                let row = row.clone();
                move |_| commit_numeric_draft(&editor, &row, draft, focused, acknowledge_until)
            },
        }
    }
}

/// Checkbox editor with the same edit guard: a click commits immediately and
/// the engine snapshot cannot visually revert the toggle before the write is
/// acknowledged.
#[component]
fn BoolFieldEditor(row: FieldRow, editor: Arc<EditorContext>) -> Element {
    let mut desired = use_signal(|| row.bool_value);
    let mut acknowledge_until = use_signal(|| None::<Instant>);

    let pending = match *acknowledge_until.read() {
        Some(deadline) => Instant::now() < deadline && *desired.read() != row.bool_value,
        None => false,
    };
    let display_checked = if pending {
        *desired.read()
    } else {
        row.bool_value
    };

    rsx! {
        input {
            class: "editor-checkbox",
            r#type: "checkbox",
            checked: display_checked,
            onchange: {
                let editor = Arc::clone(&editor);
                let row = row.clone();
                move |_| {
                    let next = !*desired.read();
                    desired.set(next);
                    editor.push_command(EditorCommand::SetField {
                        entity: row.entity,
                        component: row.component.clone(),
                        field: row.name.clone(),
                        value: FieldValue::Bool(next),
                    });
                    acknowledge_until
                        .set(Some(Instant::now() + COMMIT_ACKNOWLEDGE_WINDOW));
                }
            },
        }
    }
}

/// Whether the user's alpha draft, clamped and rounded like the inspector
/// renders it, equals the engine's current alpha text.
fn alpha_matches_engine(draft_text: &str, engine_text: &str) -> bool {
    match parse_scalar("f32", draft_text) {
        Some(FieldValue::F32(alpha)) => format!("{:.2}", alpha.clamp(0.0, 1.0)) == engine_text,
        _ => false,
    }
}

/// Commit the alpha draft of a colour group (clamped to 0.0-1.0).
fn commit_alpha_draft(
    editor: &EditorContext,
    group: &ColorGroup,
    mut draft: Signal<String>,
    mut focused: Signal<bool>,
    mut acknowledge_until: Signal<Option<Instant>>,
) {
    let text = draft.read().clone();
    match parse_scalar("f32", &text) {
        Some(FieldValue::F32(alpha)) => {
            let clamped = alpha.clamp(0.0, 1.0);
            if let Some(field) = group.a_field.clone() {
                editor.push_command(EditorCommand::SetField {
                    entity: group.entity,
                    component: group.component.clone(),
                    field,
                    value: FieldValue::F32(clamped),
                });
            }
            focused.set(false);
            acknowledge_until.set(Some(Instant::now() + COMMIT_ACKNOWLEDGE_WINDOW));
        }
        _ => {
            // Unparseable edit: revert the alpha to the engine value.
            draft.set(group.alpha_text.clone());
            focused.set(false);
            acknowledge_until.set(None);
        }
    }
}

/// CSS background for a swatch from a `#rrggbb` hex and an alpha text.
fn swatch_css_from_hex_alpha(hex: &str, alpha_text: &str) -> String {
    let alpha = match parse_scalar("f32", alpha_text) {
        Some(FieldValue::F32(alpha)) => alpha.clamp(0.0, 1.0),
        _ => 1.0,
    };
    let to_byte = |value: f32| -> u8 { (value.clamp(0.0, 1.0) * 255.0).round() as u8 };
    match hex_to_rgb(hex) {
        Some((r, g, b)) => format!(
            "background: rgba({},{},{},{:.3});",
            to_byte(r),
            to_byte(g),
            to_byte(b),
            alpha
        ),
        None => format!("background: rgba(255,255,255,{alpha:.3});"),
    }
}

/// Colour group editor: swatch, `#rrggbb` picker, and optional alpha box, each
/// with its own edit guard so the engine snapshot never clobbers an in-flight
/// pick while the user is choosing or just after committing it.
#[component]
fn ColorGroupEditor(group: ColorGroup, editor: Arc<EditorContext>) -> Element {
    let mut draft_hex = use_signal(|| group.hex_value.clone());
    let mut hex_until = use_signal(|| None::<Instant>);
    let mut draft_alpha = use_signal(|| group.alpha_text.clone());
    let mut alpha_focused = use_signal(|| false);
    let mut alpha_until = use_signal(|| None::<Instant>);

    let hex_waiting = match *hex_until.read() {
        Some(deadline) => Instant::now() < deadline && *draft_hex.read() != group.hex_value,
        None => false,
    };
    let display_hex = if hex_waiting {
        draft_hex.read().clone()
    } else {
        group.hex_value.clone()
    };

    let alpha_waiting = match *alpha_until.read() {
        Some(deadline) => {
            Instant::now() < deadline
                && !alpha_matches_engine(&draft_alpha.read().clone(), &group.alpha_text)
        }
        None => false,
    };
    let show_alpha_draft = *alpha_focused.read() || alpha_waiting;
    let display_alpha = if show_alpha_draft {
        draft_alpha.read().clone()
    } else {
        group.alpha_text.clone()
    };

    // While a pick or alpha edit is pending the swatch previews the draft so
    // the UI never flashes back to the pre-edit colour before the engine
    // acknowledges the change.
    let display_swatch = if hex_waiting || alpha_waiting {
        swatch_css_from_hex_alpha(&display_hex, &display_alpha)
    } else {
        group.swatch_css.clone()
    };

    rsx! {
        div {
            class: "editor-field editor-color",
            label { class: "editor-field-name", "{group.label}" }
            div {
                class: "editor-color-swatch",
                style: "{display_swatch}",
            }
            input {
                class: "editor-color-input",
                r#type: "color",
                value: "{display_hex}",
                title: "Pick a colour",
                onchange: {
                    let editor = Arc::clone(&editor);
                    let group = group.clone();
                    move |event| {
                        let hex = event.value();
                        if let Some((r, g, b)) = hex_to_rgb(&hex) {
                            draft_hex.set(hex);
                            let component = group.component.clone();
                            editor.push_command(EditorCommand::SetField {
                                entity: group.entity,
                                component: component.clone(),
                                field: group.r_field.clone(),
                                value: FieldValue::F32(r),
                            });
                            editor.push_command(EditorCommand::SetField {
                                entity: group.entity,
                                component: component.clone(),
                                field: group.g_field.clone(),
                                value: FieldValue::F32(g),
                            });
                            editor.push_command(EditorCommand::SetField {
                                entity: group.entity,
                                component: component.clone(),
                                field: group.b_field.clone(),
                                value: FieldValue::F32(b),
                            });
                            hex_until
                                .set(Some(Instant::now() + COMMIT_ACKNOWLEDGE_WINDOW));
                        }
                    }
                },
            }
            if group.a_field.is_some() {
                input {
                    class: "editor-number editor-color-alpha",
                    r#type: "text",
                    value: "{display_alpha}",
                    title: "Alpha (0.0-1.0)",
                    onfocus: {
                        let engine_text = group.alpha_text.clone();
                        move |_| {
                            draft_alpha.set(engine_text.clone());
                            alpha_focused.set(true);
                            alpha_until.set(None);
                        }
                    },
                    oninput: move |event| {
                        draft_alpha.set(event.value());
                        alpha_focused.set(true);
                    },
                    onchange: {
                        let editor = Arc::clone(&editor);
                        let group = group.clone();
                        move |_| {
                            commit_alpha_draft(
                                &editor,
                                &group,
                                draft_alpha,
                                alpha_focused,
                                alpha_until,
                            );
                        }
                    },
                    onblur: {
                        let editor = Arc::clone(&editor);
                        let group = group.clone();
                        move |_| {
                            commit_alpha_draft(
                                &editor,
                                &group,
                                draft_alpha,
                                alpha_focused,
                                alpha_until,
                            );
                        }
                    },
                }
            }
        }
    }
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

    // One component section: a header plus its field rows and any colour
    // groups (which render as swatch + picker instead of scalar rows).
    struct Section {
        title: String,
        tags: String,
        has_layout: bool,
        colors: Vec<ColorGroup>,
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
            let (colors, color_field_indices) = if component.editable {
                detect_color_groups(
                    &component.type_name,
                    &component.fields,
                    &component.values,
                    detail.entity,
                    component.type_name.clone(),
                )
            } else {
                (Vec::new(), std::collections::BTreeSet::new())
            };
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
                                    // Each element is its own sibling row, so
                                    // the key must include the index or Dioxus
                                    // rejects the duplicate sibling keys.
                                    rows.push(FieldRow {
                                        key: format!(
                                            "field-{}-{}-[{element_index}]",
                                            component.type_name, field.name
                                        ),
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
                    } else if is_scalar_editable(field.type_tag)
                        && !color_field_indices.contains(&index)
                    {
                        rows.push(FieldRow {
                            is_scalar: true,
                            scalar_text: value.as_ref().map(field_value_text).unwrap_or_default(),
                            scalar_tag: field.type_tag.to_string(),
                            ..empty.clone()
                        });
                    } else if color_field_indices.contains(&index) {
                        // Consumed by a colour group above; no plain row.
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
                colors,
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
                        for group in section.colors {
                            ColorGroupEditor {
                                key: "{group.key}-e{group.entity.id()}v{group.entity.generation()}",
                                group: group.clone(),
                                editor: Arc::clone(&editor),
                            }
                        }
                        for row in section.rows {
                            div {
                                class: "editor-field",
                                key: "{row.key}-e{row.entity.id()}v{row.entity.generation()}",
                                label { class: "editor-field-name", "{row.name}" }
                                if row.is_bool {
                                    BoolFieldEditor {
                                        key: "bool-{row.key}-e{row.entity.id()}v{row.entity.generation()}",
                                        row: row.clone(),
                                        editor: Arc::clone(&editor),
                                    }
                                } else if row.is_scalar {
                                    NumericFieldEditor {
                                        key: "num-{row.key}-e{row.entity.id()}v{row.entity.generation()}",
                                        row: row.clone(),
                                        editor: Arc::clone(&editor),
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

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::render::{Color, Sprite};
    use pill_engine::Engine;

    /// A `Sprite`-carrying entity surfaces width/height as ordinary scalar
    /// rows plus one colour group whose flattened `color.*` channels are
    /// consumed by the picker.
    #[test]
    fn sprite_fields_produce_one_color_group() {
        let mut engine = Engine::new();
        engine.world_mut().register_component::<Sprite>();
        let entity = engine
            .world_mut()
            .create_entity()
            .with(Sprite {
                width: 64.0,
                height: 32.0,
                color: Color::new(1.0, 0.5, 0.25, 1.0),
            })
            .build()
            .expect("entity builds");

        let detail = EditorSnapshot::capture_detail(&engine, entity).expect("detail");
        let sprite = detail
            .components
            .iter()
            .find(|view| view.type_name.ends_with("::Sprite"))
            .expect("sprite view present");
        assert!(sprite.editable, "sprite has a field layout");

        let (groups, consumed) = detect_color_groups(
            &sprite.type_name,
            &sprite.fields,
            &sprite.values,
            detail.entity,
            sprite.type_name.clone(),
        );

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].label, "color");
        assert_eq!(groups[0].r_field, "color.r");
        assert_eq!(groups[0].a_field.as_deref(), Some("color.a"));
        assert_eq!(groups[0].hex_value, "#ff8040");
        // The four colour channels are consumed; width and height stay rows.
        assert_eq!(consumed.len(), 4);
        assert!(consumed.contains(&2));
        assert!(consumed.contains(&5));
    }

    /// A component that is itself a colour (`…::Color`) groups its bare
    /// `r/g/b/a` scalars under the "color" label.
    #[test]
    fn color_component_groups_bare_channels() {
        let mut engine = Engine::new();
        engine.world_mut().register_component::<Color>();
        let entity = engine
            .world_mut()
            .create_entity()
            .with(Color::new(0.0, 0.5, 1.0, 1.0))
            .build()
            .expect("entity builds");

        let detail = EditorSnapshot::capture_detail(&engine, entity).expect("detail");
        let color = detail
            .components
            .iter()
            .find(|view| view.type_name.ends_with("::Color"))
            .expect("color view present");

        let (groups, consumed) = detect_color_groups(
            &color.type_name,
            &color.fields,
            &color.values,
            detail.entity,
            color.type_name.clone(),
        );

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].label, "color");
        assert_eq!(groups[0].r_field, "r");
        assert_eq!(groups[0].a_field.as_deref(), Some("a"));
        assert_eq!(groups[0].hex_value, "#0080ff");
        assert_eq!(consumed.len(), 4);
    }

    /// Hex parsing is strict about the `#rrggbb` shape the picker produces.
    #[test]
    fn hex_to_rgb_parses_only_full_hex_triplets() {
        assert_eq!(
            hex_to_rgb("#ff8040"),
            Some((1.0, 0x80 as f32 / 255.0, 0x40 as f32 / 255.0))
        );
        assert_eq!(hex_to_rgb("ff8040"), None);
        assert_eq!(hex_to_rgb("#fff"), None);
        assert_eq!(hex_to_rgb("#gggggg"), None);
        assert_eq!(hex_to_rgb(""), None);
    }
}
