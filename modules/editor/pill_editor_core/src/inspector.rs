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
//! - The section model the tab draws: [`component_label`],
//!   [`component_icon`], [`widget_for`] and [`rows`] - what each component
//!   section holds and which widget each row asks for.
//! - Asset reference rows (W-0048): the [`EditorCommand::SetFieldAsset`]
//!   builders ([`asset_edit`], [`selection_asset_edit`]) and the assets a
//!   row's picker offers ([`asset_candidates`]).
//! - Asset settings as editable data: key paths (`a.b[0]`), leaf reads and
//!   patches, and the [`EditorCommand::SetAssetSettings`] builder.
//!
//! # Design
//!
//! Everything here is plain data and commands: the shell draws widgets and
//! queues batches, the executable applies them at the frame boundary, and
//! the history records them - the same split the Assets pipeline follows.

// External crates
use egui::Color32;
use pill_engine::component_registry::FieldDecorator;
use pill_engine::{Entity, FieldValue};
use serde_json::Value as JsonValue;

// Current crate
use crate::editor_state::{
    AssetListing, ComponentView, EditorCommand, EntityDetail, EntityListEntry,
};
use crate::icons::Icon;

/// The name after the last `::`, for labels and history entries.
pub fn short_component_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// The title a component section's header shows.
///
/// Unity names a component without its type suffix (`Transform`,
/// `Mesh Renderer`), so the engine's `TransformComponent` reads `Transform`;
/// a type without that suffix keeps its own short name.
pub fn component_label(type_name: &str) -> &str {
    let short = short_component_name(type_name);
    short.strip_suffix("Component").unwrap_or(short)
}

/// The duotone icon a component section's header draws, from its type name.
///
/// The engine's components carry no icon metadata, so the mark follows the
/// name's words: a transform points in all directions, a mesh or collider is
/// a cube (the collider an outlined one), a sprite or texture is an image, a
/// material a sphere, a light a power switch, a camera a monitor, a script or
/// behaviour a code mark, and anything else the property sliders.
pub fn component_icon(type_name: &str) -> Icon {
    let lower = type_name.to_lowercase();
    if lower.contains("transform") {
        Icon::ArrowsOutCardinal
    } else if lower.contains("collider") || lower.contains("collision") {
        Icon::CubeTransparent
    } else if lower.contains("mesh") || lower.contains("model") {
        Icon::Cube
    } else if lower.contains("sprite") || lower.contains("texture") || lower.contains("image") {
        Icon::Image
    } else if lower.contains("material") {
        Icon::Sphere
    } else if lower.contains("light") {
        Icon::Power
    } else if lower.contains("camera") {
        Icon::MonitorPlay
    } else if lower.contains("audio") || lower.contains("sound") {
        Icon::Play
    } else if lower.contains("script")
        || lower.contains("behaviour")
        || lower.contains("behavior")
        || lower.contains("system")
    {
        Icon::Code
    } else if lower.contains("rigid") || lower.contains("physics") || lower.contains("velocity") {
        Icon::ArrowsOut
    } else {
        Icon::SlidersHorizontal
    }
}

/// The widget a value's kind asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FieldWidget {
    /// A boolean: a checkbox.
    Checkbox,
    /// A whole number: a drag field that scrubs and accepts typed input.
    Integer,
    /// A floating point number: a drag field with decimals.
    Float,
    /// A string: a single-line text edit.
    Text,
    /// A value the engine can locate but not interpret: shown as text.
    ReadOnly,
    /// A `[Range]` on a floating point field: a slider over the range.
    FloatSlider {
        /// Lowest value the slider offers.
        min: f32,
        /// Highest value the slider offers.
        max: f32,
    },
    /// A `[Range]` on a whole-number field: a slider in whole steps.
    IntegerSlider {
        /// Lowest value the slider offers.
        min: f32,
        /// Highest value the slider offers.
        max: f32,
    },
    /// A `[Min]` on a floating point field: a drag value with a floor.
    FloatDrag {
        /// Lowest value the drag value accepts.
        min: f32,
    },
    /// A `[Min]` on a whole-number field: a drag value with a floor.
    IntegerDrag {
        /// Lowest value the drag value accepts.
        min: f32,
    },
    /// A `[TextArea]` or `[Multiline]` on a text field: a box of that many
    /// rows.
    MultilineText {
        /// Rows the box shows.
        lines: usize,
    },
}

/// The widget a field's current value asks for.
pub fn widget_for(value: &FieldValue) -> FieldWidget {
    match value {
        FieldValue::Bool(_) => FieldWidget::Checkbox,
        FieldValue::F32(_) | FieldValue::F64(_) => FieldWidget::Float,
        FieldValue::I8(_)
        | FieldValue::I16(_)
        | FieldValue::I32(_)
        | FieldValue::I64(_)
        | FieldValue::U8(_)
        | FieldValue::U16(_)
        | FieldValue::U32(_)
        | FieldValue::U64(_)
        | FieldValue::Usize(_)
        | FieldValue::Isize(_) => FieldWidget::Integer,
        FieldValue::Text(_) => FieldWidget::Text,
        FieldValue::Array(_)
        | FieldValue::List { .. }
        | FieldValue::Asset { .. }
        | FieldValue::Entity { .. }
        | FieldValue::Opaque { .. } => FieldWidget::ReadOnly,
    }
}

/// The widget a field's value asks for once its decorators are applied.
///
/// Decorators that cannot apply to the value - a `[Range]` on a string, a
/// `[TextArea]` on a number - are ignored rather than refused: the declaration
/// is presentation, and a wrong one must not hide the field it decorates. A
/// range wins over a minimum, since a slider already bounds below, and a text
/// area's first row count is the box height.
pub fn widget_for_decorated(value: &FieldValue, decorators: &[FieldDecorator]) -> FieldWidget {
    let mut widget = widget_for(value);
    let mut range: Option<(f32, f32)> = None;
    let mut minimum: Option<f32> = None;
    let mut lines: Option<usize> = None;
    for decorator in decorators {
        match decorator {
            FieldDecorator::Range { min, max } => range = Some((*min, *max)),
            FieldDecorator::Min(value) => minimum = Some(*value),
            FieldDecorator::TextArea { min_lines, .. } => lines = Some(*min_lines),
            FieldDecorator::Multiline(count) => lines = Some(*count),
            FieldDecorator::HideInInspector
            | FieldDecorator::Header(_)
            | FieldDecorator::Space(_)
            | FieldDecorator::Tooltip(_) => {}
        }
    }
    if let Some((min, max)) = range {
        widget = match widget {
            FieldWidget::Float => FieldWidget::FloatSlider { min, max },
            FieldWidget::Integer => FieldWidget::IntegerSlider { min, max },
            other => other,
        };
    } else if let Some(min) = minimum {
        widget = match widget {
            FieldWidget::Float => FieldWidget::FloatDrag { min },
            FieldWidget::Integer => FieldWidget::IntegerDrag { min },
            other => other,
        };
    }
    if let (FieldWidget::Text, Some(lines)) = (widget, lines) {
        widget = FieldWidget::MultilineText { lines };
    }
    widget
}

/// How many side-by-side elements a vector row holds at most: X, Y, Z, W.
pub const VECTOR_AXES_MAX: usize = 4;

/// One row of a component section.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldRow {
    /// A separator line with the text a field's `[Header]` declared.
    Header {
        /// Text the line shows.
        text: String,
    },
    /// A vertical gap a field's `[Space]` declared.
    Space {
        /// Gap height, in pixels.
        pixels: f32,
    },
    /// One field with the widget its kind asks for.
    Single {
        /// The field's position in the component's field list (the value's,
        /// too).
        index: usize,
        /// The field's registered name.
        field: String,
        /// The widget to draw.
        widget: FieldWidget,
        /// Text the field's `[Tooltip]` declared, shown on hover of the label.
        tooltip: Option<String>,
    },
    /// A short array of scalars: one row, one widget per element, labelled
    /// with the element's axis (`X`, `Y`, `Z`, `W`) - Unity's vector row.
    Vector {
        /// The array's position in the component's field list.
        index: usize,
        /// The field's registered name.
        field: String,
        /// The element widgets' kind.
        widget: FieldWidget,
        /// How many elements the row holds (2..=`VECTOR_AXES_MAX`).
        elements: usize,
        /// Text the field's `[Tooltip]` declared, shown on hover of the label.
        tooltip: Option<String>,
    },
    /// A reference to an asset: the asset it names, a picker of the assets
    /// of its type, a clear button and a drop target for the Assets tab.
    Asset {
        /// The field's position in the component's field list.
        index: usize,
        /// The field's registered name.
        field: String,
        /// The registered name of the asset type the field references, when
        /// recorded; the picker offers only assets of this type.
        asset_type: Option<String>,
        /// Text the field's `[Tooltip]` declared, shown on hover of the label.
        tooltip: Option<String>,
    },
    /// A reference to another entity (W-0048 Stage 6): the referenced
    /// entity's Hierarchy label, a picker over the scene's entities, a
    /// viewport pick, a clear button and a drop target for the Entities tab.
    Entity {
        /// The field's position in the component's field list.
        index: usize,
        /// The field's registered name.
        field: String,
        /// Text the field's `[Tooltip]` declared, shown on hover of the label.
        tooltip: Option<String>,
    },
    /// A field shown read-only (a long array, a list, an opaque value).
    ReadOnly {
        /// The field's position in the component's field list.
        index: usize,
        /// The field's registered name.
        field: String,
        /// Text the field's `[Tooltip]` declared, shown on hover of the label.
        tooltip: Option<String>,
    },
}

/// The rows a component section draws, in field order.
///
/// A scalar array of two to four elements becomes one vector row (Unity draws
/// a `Vector3` as three fields on one line); anything longer stays one row per
/// element, drawn by the caller from the array's own value.
///
/// Decorators are applied here so the shell only draws what the model asks
/// for: `[HideInInspector]` drops the field together with its header and gap,
/// `[Header]` and `[Space]` emit their own rows above the field in declaration
/// order, and `[Tooltip]` rides on the field's row.
pub fn rows(component: &ComponentView) -> Vec<FieldRow> {
    let mut rows = Vec::new();
    for (index, (descriptor, value)) in component
        .fields
        .iter()
        .zip(component.values.iter())
        .enumerate()
    {
        // A hidden field takes its decorations with it: drawing a header over
        // nothing would be worse than drawing nothing at all.
        if descriptor
            .decorators
            .iter()
            .any(|decorator| matches!(decorator, FieldDecorator::HideInInspector))
        {
            continue;
        }
        let mut tooltip: Option<String> = None;
        for decorator in descriptor.decorators.iter() {
            match decorator {
                FieldDecorator::Header(text) => rows.push(FieldRow::Header {
                    text: text.to_string(),
                }),
                FieldDecorator::Space(pixels) => rows.push(FieldRow::Space { pixels: *pixels }),
                FieldDecorator::Tooltip(text) => tooltip = Some(text.to_string()),
                FieldDecorator::HideInInspector
                | FieldDecorator::Range { .. }
                | FieldDecorator::TextArea { .. }
                | FieldDecorator::Multiline(_)
                | FieldDecorator::Min(_) => {}
            }
        }
        let widget = widget_for_decorated(value, descriptor.decorators.as_ref());
        let row = match value {
            FieldValue::Array(elements)
                if (2..=VECTOR_AXES_MAX).contains(&elements.len())
                    && elements.iter().all(|element| {
                        matches!(
                            widget_for(element),
                            FieldWidget::Integer | FieldWidget::Float
                        )
                    }) =>
            {
                FieldRow::Vector {
                    index,
                    field: descriptor.name.to_string(),
                    // A range belongs to one scalar, so a vector row keeps the
                    // plain element widget and the tooltip.
                    widget: widget_for(elements.first().expect("the array is not empty")),
                    elements: elements.len(),
                    tooltip,
                }
            }
            FieldValue::Asset { asset_type, .. } => FieldRow::Asset {
                index,
                field: descriptor.name.to_string(),
                asset_type: asset_type.clone(),
                tooltip,
            },
            FieldValue::Entity { .. } => FieldRow::Entity {
                index,
                field: descriptor.name.to_string(),
                tooltip,
            },
            FieldValue::Array(_) | FieldValue::List { .. } | FieldValue::Opaque { .. } => {
                FieldRow::ReadOnly {
                    index,
                    field: descriptor.name.to_string(),
                    tooltip,
                }
            }
            _ => FieldRow::Single {
                index,
                field: descriptor.name.to_string(),
                widget,
                tooltip,
            },
        };
        rows.push(row);
    }
    rows
}

/// The axis label a vector row shows for element `index`: `X`, `Y`, `Z`, `W`.
pub fn axis_label(index: usize) -> &'static str {
    match index {
        0 => "X",
        1 => "Y",
        2 => "Z",
        _ => "W",
    }
}

/// The colour a vector row's axis label takes: the gizmo's X red, Y green,
/// Z blue (the guide's colours), W muted for a fourth element.
pub fn axis_color(index: usize) -> Color32 {
    match index {
        0 => Color32::from_rgb(214, 96, 96),
        1 => Color32::from_rgb(112, 190, 112),
        2 => Color32::from_rgb(104, 148, 232),
        _ => Color32::GRAY,
    }
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

/// The recorded asset reference edit: `SetFieldAsset` under the field edit's
/// label (`set Component.field`). `reference` is a path relative to `res` or
/// a guid; `None` clears the reference.
pub fn asset_edit(
    entity: Entity,
    component: &str,
    field: &str,
    reference: Option<String>,
) -> (EditorCommand, String) {
    (
        EditorCommand::SetFieldAsset {
            entity,
            component: component.to_string(),
            field: field.to_string(),
            reference,
        },
        format!("set {}.{}", short_component_name(component), field),
    )
}

/// [`asset_edit`] for every entity of `targets`, as one recorded step labelled
/// and coalesced by the primary's edit, like [`selection_field_edit`].
pub fn selection_asset_edit(
    targets: &[Entity],
    primary: Entity,
    component: &str,
    field: &str,
    reference: Option<String>,
) -> (Vec<EditorCommand>, String, String) {
    let commands = targets
        .iter()
        .map(|target| asset_edit(*target, component, field, reference.clone()).0)
        .collect();
    let (_command, label) = asset_edit(primary, component, field, reference);
    (
        commands,
        label,
        field_coalesce_key(primary, component, field),
    )
}

/// The recorded entity reference edit: `SetFieldEntity` under the field edit's
/// label (`set Component.field`). `reference` is the entity to point at;
/// `None` clears the reference.
pub fn entity_edit(
    entity: Entity,
    component: &str,
    field: &str,
    reference: Option<Entity>,
) -> (EditorCommand, String) {
    (
        EditorCommand::SetFieldEntity {
            entity,
            component: component.to_string(),
            field: field.to_string(),
            reference,
        },
        format!("set {}.{}", short_component_name(component), field),
    )
}

/// [`entity_edit`] for every entity of `targets`, as one recorded step labelled
/// and coalesced by the primary's edit, like [`selection_field_edit`].
pub fn selection_entity_edit(
    targets: &[Entity],
    primary: Entity,
    component: &str,
    field: &str,
    reference: Option<Entity>,
) -> (Vec<EditorCommand>, String, String) {
    let commands = targets
        .iter()
        .map(|target| entity_edit(*target, component, field, reference).0)
        .collect();
    let (_command, label) = entity_edit(primary, component, field, reference);
    (
        commands,
        label,
        field_coalesce_key(primary, component, field),
    )
}

/// The entities an entity row's picker offers: every entity the Hierarchy
/// lists, in its order.
///
/// The row's own entity stays in the list: a component that references its own
/// entity is odd but harmless, and hiding it would make the picker disagree
/// with the Hierarchy for no rule the user could see.
pub fn entity_candidates(entries: &[EntityListEntry]) -> Vec<&EntityListEntry> {
    entries.iter().collect()
}

/// The assets an asset row's picker offers: every file of the listing whose
/// type is the field's, in listing order. A field without a recorded type
/// offers nothing, rather than every asset of every type.
pub fn asset_candidates<'a>(
    listing: &'a [AssetListing],
    asset_type: Option<&str>,
) -> Vec<&'a AssetListing> {
    let Some(asset_type) = asset_type else {
        return Vec::new();
    };
    listing
        .iter()
        .filter(|entry| entry.kind == asset_type)
        .collect()
}

/// Whether a scripted or typed value names "no asset": the word that clears an
/// asset reference field.
pub fn is_cleared_reference(text: &str) -> bool {
    matches!(text.trim(), "" | "none" | "None")
}

/// The entity a scripted reference names: a Hierarchy label (`3v0`), a bare
/// slot id (`3`, read as generation 0), or `None` for the word that clears the
/// reference.
///
/// # Errors
///
/// A message naming the text when it is not an entity label.
pub fn parse_entity_reference(text: &str) -> Result<Option<Entity>, String> {
    if is_cleared_reference(text) {
        return Ok(None);
    }
    let trimmed = text.trim();
    let (id_text, generation_text) = match trimmed.split_once('v') {
        Some((id, generation)) => (id, generation),
        None => (trimmed, "0"),
    };
    let id: u64 = id_text
        .parse()
        .map_err(|_| format!("`{trimmed}` is not an entity label (`<id>v<generation>`)"))?;
    let generation: u32 = generation_text
        .parse()
        .map_err(|_| format!("`{trimmed}` is not an entity label (`<id>v<generation>`)"))?;
    Ok(Some(Entity::from_raw(id, generation)))
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

/// The selected entities one component's edit reaches (W-0038 Stage 16): the
/// primary first, then every other selected entity whose detail carries the
/// component.
///
/// A selection whose entities do not all carry a component edits the ones that
/// do - writing to an entity without it would fail the command and put an
/// error in the Console for a row that showed nothing.
pub fn selection_targets(
    details: &[EntityDetail],
    primary: Entity,
    component: &str,
) -> Vec<Entity> {
    let mut targets = vec![primary];
    for detail in details {
        if detail.entity == primary {
            continue;
        }
        if detail
            .components
            .iter()
            .any(|view| view.type_name == component)
        {
            targets.push(detail.entity);
        }
    }
    targets
}

/// One recorded batch writing `value` into `field` on every target, with its
/// label and coalesce key.
///
/// The label names the primary's component and field, so a step reads the same
/// whether one entity or ten were written; the coalesce key is the primary's,
/// so a drag across a multi-selection collapses into one undo step.
pub fn selection_field_edit(
    targets: &[Entity],
    primary: Entity,
    component: &str,
    field: &str,
    value: FieldValue,
) -> (Vec<EditorCommand>, String, String) {
    let mut commands = Vec::with_capacity(targets.len());
    for target in targets {
        let (command, _step_label) = field_edit(*target, component, field, value.clone());
        commands.push(command);
    }
    let (_command, label) = field_edit(primary, component, field, value);
    let key = field_coalesce_key(primary, component, field);
    (commands, label, key)
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

/// One field value in the text form the Inspector's widgets show and commit
/// (W-0038 Stage 12): what a module panel displays and what it writes back
/// through `PanelCommand::SetField`.
pub fn value_text(value: &FieldValue) -> String {
    match value {
        FieldValue::F32(value) => format!("{value}"),
        FieldValue::F64(value) => format!("{value}"),
        FieldValue::I8(value) => format!("{value}"),
        FieldValue::I16(value) => format!("{value}"),
        FieldValue::I32(value) => format!("{value}"),
        FieldValue::I64(value) => format!("{value}"),
        FieldValue::U8(value) => format!("{value}"),
        FieldValue::U16(value) => format!("{value}"),
        FieldValue::U32(value) => format!("{value}"),
        FieldValue::U64(value) => format!("{value}"),
        FieldValue::Usize(value) => format!("{value}"),
        FieldValue::Isize(value) => format!("{value}"),
        FieldValue::Bool(value) => format!("{value}"),
        FieldValue::Text(value) => value.clone(),
        FieldValue::Array(elements) => format!("[{}]", elements.len()),
        FieldValue::List { element_count, .. } => format!("[{element_count}]"),
        other => format!("{other:?}"),
    }
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

    /// A detail whose components are named by their short names, enough to
    /// answer which entities carry a component.
    fn detail(entity: Entity, components: &[&str]) -> EntityDetail {
        EntityDetail {
            entity,
            components: components
                .iter()
                .map(|name| ComponentView {
                    type_name: name.to_string(),
                    persistable: true,
                    editable: true,
                    fields: Vec::new(),
                    values: Vec::new(),
                    asset_labels: Vec::new(),
                    entity_labels: Vec::new(),
                })
                .collect(),
        }
    }

    /// A multi-selection edits the entities that carry the component, and skips
    /// the ones that do not (W-0038 Stage 16).
    #[test]
    fn selection_targets_skip_entities_without_the_component() {
        let primary = Entity::from_raw(1, 0);
        let carrier = Entity::from_raw(2, 0);
        let other = Entity::from_raw(3, 0);
        let details = vec![
            detail(primary, &["Transform"]),
            detail(carrier, &["Transform", "Health"]),
            detail(other, &["Health"]),
        ];
        assert_eq!(
            selection_targets(&details, primary, "Transform"),
            vec![primary, carrier]
        );
        assert_eq!(
            selection_targets(&details, primary, "Health"),
            vec![primary, carrier, other]
        );
    }

    /// One row's commit over a multi-selection is one recorded batch: one
    /// command per target, named and coalesced by the primary.
    #[test]
    fn a_selection_field_edit_batches_every_target() {
        let primary = Entity::from_raw(1, 0);
        let second = Entity::from_raw(2, 0);
        let (commands, label, key) = selection_field_edit(
            &[primary, second],
            primary,
            "Transform",
            "speed",
            FieldValue::F32(2.5),
        );
        assert_eq!(commands.len(), 2);
        assert_eq!(label, "set Transform.speed");
        assert_eq!(key, field_coalesce_key(primary, "Transform", "speed"));
        let entities: Vec<Entity> = commands
            .iter()
            .map(|command| match command {
                EditorCommand::SetField { entity, .. } => *entity,
                other => panic!("a selection edit queued {other:?}"),
            })
            .collect();
        assert_eq!(entities, vec![primary, second]);
    }

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

    /// A component view over a field/value pair list.
    fn component(name: &str, values: Vec<FieldValue>) -> ComponentView {
        /// Field names for the fixture (the descriptor's name is a `&'static
        /// str`), indexed by the value's position.
        const NAMES: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];
        ComponentView {
            asset_labels: Vec::new(),
            entity_labels: Vec::new(),
            type_name: name.to_string(),
            persistable: false,
            editable: true,
            fields: values
                .iter()
                .enumerate()
                .map(
                    |(index, _)| pill_engine::component_registry::ComponentFieldDescriptor {
                        name: NAMES[index],
                        type_tag: "f32",
                        offset: 0,
                        size: 4,
                        align: 4,
                        element_count: 0,
                        decorators: std::borrow::Cow::Borrowed(&[]),
                    },
                )
                .collect(),
            values,
        }
    }

    #[test]
    fn the_widget_follows_the_value_kind() {
        assert_eq!(widget_for(&FieldValue::Bool(true)), FieldWidget::Checkbox);
        assert_eq!(widget_for(&FieldValue::F32(1.0)), FieldWidget::Float);
        assert_eq!(widget_for(&FieldValue::F64(1.0)), FieldWidget::Float);
        for value in [
            FieldValue::I8(1),
            FieldValue::I16(1),
            FieldValue::I32(1),
            FieldValue::I64(1),
            FieldValue::U8(1),
            FieldValue::U16(1),
            FieldValue::U32(1),
            FieldValue::U64(1),
            FieldValue::Usize(1),
            FieldValue::Isize(1),
        ] {
            assert_eq!(widget_for(&value), FieldWidget::Integer);
        }
        assert_eq!(
            widget_for(&FieldValue::Text("x".to_string())),
            FieldWidget::Text
        );
        assert_eq!(
            widget_for(&FieldValue::Opaque {
                type_tag: "struct:x",
                bytes: vec![0; 4]
            }),
            FieldWidget::ReadOnly
        );
    }

    #[test]
    fn a_short_scalar_array_becomes_one_vector_row() {
        let transform = component(
            "TransformComponent",
            vec![
                FieldValue::Array(vec![
                    FieldValue::F32(1.0),
                    FieldValue::F32(2.0),
                    FieldValue::F32(3.0),
                ]),
                FieldValue::Bool(true),
            ],
        );
        assert_eq!(
            rows(&transform),
            vec![
                FieldRow::Vector {
                    index: 0,
                    field: "a".to_string(),
                    widget: FieldWidget::Float,
                    elements: 3,
                    tooltip: None,
                },
                FieldRow::Single {
                    index: 1,
                    field: "b".to_string(),
                    widget: FieldWidget::Checkbox,
                    tooltip: None,
                },
            ]
        );
    }

    #[test]
    fn a_long_or_non_scalar_array_stays_read_only() {
        let long = component(
            "Big",
            vec![FieldValue::Array(
                (0..8).map(|index| FieldValue::F32(index as f32)).collect(),
            )],
        );
        assert_eq!(
            rows(&long),
            vec![FieldRow::ReadOnly {
                index: 0,
                field: "a".to_string(),
                tooltip: None,
            }]
        );
        let nested = component(
            "Nested",
            vec![FieldValue::Array(vec![
                FieldValue::Array(vec![FieldValue::F32(1.0), FieldValue::F32(2.0)]),
                FieldValue::Array(vec![FieldValue::F32(3.0), FieldValue::F32(4.0)]),
            ])],
        );
        assert_eq!(
            rows(&nested),
            vec![FieldRow::ReadOnly {
                index: 0,
                field: "a".to_string(),
                tooltip: None,
            }]
        );
        let list = component(
            "List",
            vec![FieldValue::List {
                element_tag: "f32",
                element_count: 3,
            }],
        );
        assert_eq!(
            rows(&list),
            vec![FieldRow::ReadOnly {
                index: 0,
                field: "a".to_string(),
                tooltip: None,
            }]
        );
    }

    #[test]
    fn a_two_element_array_is_a_vector_too() {
        let value = component(
            "Vec2-ish",
            vec![FieldValue::Array(vec![
                FieldValue::I32(1),
                FieldValue::I32(2),
            ])],
        );
        assert_eq!(
            rows(&value),
            vec![FieldRow::Vector {
                index: 0,
                field: "a".to_string(),
                widget: FieldWidget::Integer,
                elements: 2,
                tooltip: None,
            }]
        );
    }

    /// A component whose fields carry the decorations at their position.
    fn decorated(values: Vec<FieldValue>, decorators: Vec<Vec<FieldDecorator>>) -> ComponentView {
        /// Field names for the fixture (the descriptor's name is a `&'static
        /// str`), indexed by the value's position.
        const NAMES: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];
        ComponentView {
            asset_labels: Vec::new(),
            entity_labels: Vec::new(),
            type_name: "Decorated".to_string(),
            persistable: false,
            editable: true,
            fields: values
                .iter()
                .enumerate()
                .map(
                    |(index, _)| pill_engine::component_registry::ComponentFieldDescriptor {
                        name: NAMES[index],
                        type_tag: "f32",
                        offset: 0,
                        size: 4,
                        align: 4,
                        element_count: 0,
                        decorators: std::borrow::Cow::Owned(decorators[index].clone()),
                    },
                )
                .collect(),
            values,
        }
    }

    #[test]
    fn header_and_space_decorators_become_rows_above_their_field() {
        let view = decorated(
            vec![FieldValue::F32(1.0), FieldValue::Bool(true)],
            vec![
                vec![
                    FieldDecorator::Header("Timing".into()),
                    FieldDecorator::Space(10.0),
                ],
                Vec::new(),
            ],
        );
        assert_eq!(
            rows(&view),
            vec![
                FieldRow::Header {
                    text: "Timing".to_string(),
                },
                FieldRow::Space { pixels: 10.0 },
                FieldRow::Single {
                    index: 0,
                    field: "a".to_string(),
                    widget: FieldWidget::Float,
                    tooltip: None,
                },
                FieldRow::Single {
                    index: 1,
                    field: "b".to_string(),
                    widget: FieldWidget::Checkbox,
                    tooltip: None,
                },
            ]
        );
    }

    #[test]
    fn a_hidden_field_takes_its_header_and_gap_with_it() {
        let view = decorated(
            vec![FieldValue::F32(1.0), FieldValue::F32(2.0)],
            vec![
                vec![
                    FieldDecorator::Header("Hidden".into()),
                    FieldDecorator::HideInInspector,
                ],
                vec![FieldDecorator::Tooltip("Kept".into())],
            ],
        );
        assert_eq!(
            rows(&view),
            vec![FieldRow::Single {
                index: 1,
                field: "b".to_string(),
                widget: FieldWidget::Float,
                tooltip: Some("Kept".to_string()),
            }]
        );
    }

    #[test]
    fn a_range_decorator_picks_the_slider_for_the_value_kind() {
        let view = decorated(
            vec![FieldValue::F32(50.0), FieldValue::I32(5)],
            vec![
                vec![FieldDecorator::Range {
                    min: 0.0,
                    max: 100.0,
                }],
                vec![FieldDecorator::Range {
                    min: 0.0,
                    max: 100.0,
                }],
            ],
        );
        assert_eq!(
            rows(&view),
            vec![
                FieldRow::Single {
                    index: 0,
                    field: "a".to_string(),
                    widget: FieldWidget::FloatSlider {
                        min: 0.0,
                        max: 100.0,
                    },
                    tooltip: None,
                },
                FieldRow::Single {
                    index: 1,
                    field: "b".to_string(),
                    widget: FieldWidget::IntegerSlider {
                        min: 0.0,
                        max: 100.0,
                    },
                    tooltip: None,
                },
            ]
        );
    }

    #[test]
    fn a_minimum_decorator_clamps_the_drag_value() {
        let view = decorated(
            vec![FieldValue::F32(1.0), FieldValue::I32(1)],
            vec![
                vec![FieldDecorator::Min(0.0)],
                vec![FieldDecorator::Min(-5.0)],
            ],
        );
        assert_eq!(
            rows(&view),
            vec![
                FieldRow::Single {
                    index: 0,
                    field: "a".to_string(),
                    widget: FieldWidget::FloatDrag { min: 0.0 },
                    tooltip: None,
                },
                FieldRow::Single {
                    index: 1,
                    field: "b".to_string(),
                    widget: FieldWidget::IntegerDrag { min: -5.0 },
                    tooltip: None,
                },
            ]
        );
    }

    #[test]
    fn text_decorators_make_a_multiline_box() {
        let view = decorated(
            vec![FieldValue::Text("a".into()), FieldValue::Text("b".into())],
            vec![
                vec![FieldDecorator::TextArea {
                    min_lines: 3,
                    max_lines: 10,
                }],
                vec![FieldDecorator::Multiline(5)],
            ],
        );
        assert_eq!(
            rows(&view),
            vec![
                FieldRow::Single {
                    index: 0,
                    field: "a".to_string(),
                    widget: FieldWidget::MultilineText { lines: 3 },
                    tooltip: None,
                },
                FieldRow::Single {
                    index: 1,
                    field: "b".to_string(),
                    widget: FieldWidget::MultilineText { lines: 5 },
                    tooltip: None,
                },
            ]
        );
    }

    #[test]
    fn decorators_that_cannot_apply_leave_the_widget_alone() {
        let numeric = decorated(
            vec![FieldValue::F32(1.0), FieldValue::Text("x".into())],
            vec![
                vec![FieldDecorator::TextArea {
                    min_lines: 3,
                    max_lines: 3,
                }],
                vec![FieldDecorator::Range { min: 0.0, max: 1.0 }],
            ],
        );
        assert_eq!(
            rows(&numeric),
            vec![
                FieldRow::Single {
                    index: 0,
                    field: "a".to_string(),
                    widget: FieldWidget::Float,
                    tooltip: None,
                },
                FieldRow::Single {
                    index: 1,
                    field: "b".to_string(),
                    widget: FieldWidget::Text,
                    tooltip: None,
                },
            ]
        );
        // A range belongs to one scalar: a vector row keeps the plain element
        // widget and the tooltip.
        let vector = decorated(
            vec![FieldValue::Array(vec![
                FieldValue::F32(1.0),
                FieldValue::F32(2.0),
            ])],
            vec![vec![
                FieldDecorator::Range { min: 0.0, max: 1.0 },
                FieldDecorator::Tooltip("Vec".into()),
            ]],
        );
        assert_eq!(
            rows(&vector),
            vec![FieldRow::Vector {
                index: 0,
                field: "a".to_string(),
                widget: FieldWidget::Float,
                elements: 2,
                tooltip: Some("Vec".to_string()),
            }]
        );
    }

    #[test]
    fn labels_drop_the_component_suffix_and_the_module_path() {
        assert_eq!(
            component_label("pill_master_renderer::component::TransformComponent"),
            "Transform"
        );
        assert_eq!(component_label("TransformComponent"), "Transform");
        assert_eq!(
            component_label("italian_brainrot::TagAlphaComponent"),
            "TagAlpha"
        );
        assert_eq!(component_label("pill_spline::Spline"), "Spline");
        assert_eq!(component_label("Weird"), "Weird");
    }

    #[test]
    fn icons_follow_the_component_name() {
        assert_eq!(
            component_icon("x::TransformComponent"),
            Icon::ArrowsOutCardinal
        );
        assert_eq!(component_icon("x::MeshRendererComponent"), Icon::Cube);
        assert_eq!(component_icon("x::BoxCollider"), Icon::CubeTransparent);
        assert_eq!(component_icon("x::SpriteComponent"), Icon::Image);
        assert_eq!(component_icon("x::TextureAsset"), Icon::Image);
        assert_eq!(component_icon("x::MaterialComponent"), Icon::Sphere);
        assert_eq!(component_icon("x::DirectionalLightComponent"), Icon::Power);
        assert_eq!(component_icon("x::CameraComponent"), Icon::MonitorPlay);
        assert_eq!(component_icon("x::AudioSource"), Icon::Play);
        assert_eq!(component_icon("x::HealthBehaviour"), Icon::Code);
        assert_eq!(component_icon("x::SomeScript"), Icon::Code);
        assert_eq!(component_icon("x::RigidbodyBody"), Icon::ArrowsOut);
        assert_eq!(component_icon("x::UnknownThing"), Icon::SlidersHorizontal);
    }

    #[test]
    fn vector_axes_are_named_and_coloured_like_the_gizmo_guides() {
        assert_eq!(
            (0..4).map(axis_label).collect::<Vec<_>>(),
            vec!["X", "Y", "Z", "W"]
        );
        let red = axis_color(0);
        let green = axis_color(1);
        let blue = axis_color(2);
        assert!(red.r() > red.g() && red.r() > red.b(), "X is red-ish");
        assert!(
            green.g() > green.r() && green.g() > green.b(),
            "Y is green-ish"
        );
        assert!(blue.b() > blue.r() && blue.b() > blue.g(), "Z is blue-ish");
        assert_eq!(axis_color(3), Color32::GRAY);
    }

    /// An asset value becomes an asset row carrying the field's asset type,
    /// not a read-only row (W-0048).
    #[test]
    fn an_asset_value_is_an_asset_row() {
        let view = component(
            "demo::Renderer",
            vec![FieldValue::Asset {
                asset_type: Some("demo::Mesh".to_owned()),
                index: 0,
                generation: 0,
            }],
        );
        assert_eq!(
            rows(&view),
            vec![FieldRow::Asset {
                index: 0,
                field: "a".to_owned(),
                asset_type: Some("demo::Mesh".to_owned()),
                tooltip: None,
            }]
        );
    }

    /// The picker offers the listing's files of the field's type only, and
    /// nothing for a field whose type is not recorded.
    #[test]
    fn asset_candidates_are_the_files_of_the_fields_type() {
        let listing = |path: &str, kind: &str| AssetListing {
            path: path.to_owned(),
            kind: kind.to_owned(),
            standalone: false,
            has_meta: true,
        };
        let assets = vec![
            listing("models", "folder"),
            listing("models/cube.obj", "demo::Mesh"),
            listing("textures/a.png", "demo::Texture"),
            listing("models/plane.obj", "demo::Mesh"),
        ];
        let paths: Vec<&str> = asset_candidates(&assets, Some("demo::Mesh"))
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        assert_eq!(paths, ["models/cube.obj", "models/plane.obj"]);
        assert!(asset_candidates(&assets, None).is_empty());
    }

    /// An entity value becomes an entity row, not a read-only one (W-0048
    /// Stage 6).
    #[test]
    fn an_entity_value_is_an_entity_row() {
        let view = component(
            "demo::Follower",
            vec![FieldValue::Entity {
                id: 7,
                generation: 2,
            }],
        );
        assert_eq!(
            rows(&view),
            vec![FieldRow::Entity {
                index: 0,
                field: "a".to_owned(),
                tooltip: None,
            }]
        );
    }

    /// The picker offers every listed entity in the Hierarchy's order, and a
    /// typed or scripted label reads back as the handle it names.
    #[test]
    fn entity_references_round_trip_through_their_labels() {
        let listed = EntityListEntry {
            entity: Entity::from_raw(9, 3),
            display: "9v3".to_owned(),
            component_count: 1,
            archetype_label: String::new(),
        };
        let entries = vec![listed];
        assert_eq!(entity_candidates(&entries).len(), 1);
        assert_eq!(
            parse_entity_reference(&EntityListEntry::display_of(Entity::from_raw(9, 3))),
            Ok(Some(Entity::from_raw(9, 3)))
        );
        assert_eq!(
            parse_entity_reference("9"),
            Ok(Some(Entity::from_raw(9, 0)))
        );
        assert_eq!(parse_entity_reference("none"), Ok(None));
        assert!(parse_entity_reference("not-an-entity").is_err());
    }

    /// The entity edit builders queue `SetFieldEntity` under the field edit's
    /// label, once per selected entity, and the clear names no entity.
    #[test]
    fn entity_edits_queue_set_field_entity_for_the_selection() {
        let first = Entity::from_raw(1, 0);
        let second = Entity::from_raw(2, 0);
        let target = Entity::from_raw(4, 1);
        let (commands, label, key) = selection_entity_edit(
            &[first, second],
            first,
            "demo::Follower",
            "follow",
            Some(target),
        );
        assert_eq!(label, "set Follower.follow");
        assert_eq!(key, field_coalesce_key(first, "demo::Follower", "follow"));
        assert_eq!(commands.len(), 2);
        assert!(matches!(
            &commands[1],
            EditorCommand::SetFieldEntity { entity, reference: Some(entity_ref), .. }
                if *entity == second && *entity_ref == target
        ));
        let (cleared, _label) = entity_edit(first, "demo::Follower", "follow", None);
        assert!(matches!(
            cleared,
            EditorCommand::SetFieldEntity {
                reference: None,
                ..
            }
        ));
    }

    /// The asset edit builders queue `SetFieldAsset` under the field edit's
    /// label, once per selected entity, and `none` reads as a clear.
    #[test]
    fn asset_edits_queue_set_field_asset_for_the_selection() {
        let first = Entity::from_raw(1, 0);
        let second = Entity::from_raw(2, 0);
        let (commands, label, _key) = selection_asset_edit(
            &[first, second],
            first,
            "demo::MeshRendererComponent",
            "mesh",
            Some("models/cube.obj".to_owned()),
        );
        assert_eq!(label, "set MeshRendererComponent.mesh");
        assert_eq!(commands.len(), 2);
        assert!(matches!(
            &commands[1],
            EditorCommand::SetFieldAsset { entity, reference: Some(path), .. }
                if *entity == second && path == "models/cube.obj"
        ));
        assert!(is_cleared_reference(" none "));
        assert!(!is_cleared_reference("models/cube.obj"));
    }
}
