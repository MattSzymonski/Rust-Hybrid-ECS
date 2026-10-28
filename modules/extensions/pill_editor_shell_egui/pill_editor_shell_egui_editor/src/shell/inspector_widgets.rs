//! The Inspector's field controls.
//!
//! # Responsibilities
//!
//! - `scalar_edit_decorated`: one field's control, decorated and committed through the core's helper.
//! - The numeric editors (`scalar_edit`, `integer_slider`, `integer_drag`, `ascending`), the read-only row and the value formatters the component menu and the settings rows use.
//! - `axis_slot` and `fixed_width_field`: a vector component's axis letter and its editor, both at fixed widths so a long value is cropped inside its field instead of widening the row.
//!
//! The globs below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::tab_inspector::{
    INSPECTOR_VECTOR_FIELD_HEIGHT, INSPECTOR_VECTOR_LABEL_WIDTH,
};
use pill_editor_shell_egui_editor::shell::*;

/// The edit control for one scalar, honouring the widget the row model chose:
/// a slider over a `[Range]`, a drag value with a `[Min]` floor, and the plain
/// widget for every other kind.
///
/// A whole-number slider or floor runs through `f64` and rounds back, which is
/// exact for the 8- to 32-bit kinds and precise to the float mantissa above
/// that.
pub fn scalar_edit_decorated(
    ui: &mut egui::Ui,
    value: &FieldValue,
    widget: &inspector::FieldWidget,
) -> Option<FieldValue> {
    use inspector::FieldWidget as Row;
    match (value, widget) {
        (FieldValue::F32(current), Row::FloatSlider { min, max }) => {
            let mut edited = *current;
            let (low, high) = ascending(*min, *max);
            ui.add(egui::Slider::new(&mut edited, low..=high).max_decimals(4))
                .changed()
                .then_some(FieldValue::F32(edited))
        }
        (FieldValue::F32(current), Row::FloatDrag { min }) => {
            let mut edited = current.max(*min);
            ui.add(
                egui::DragValue::new(&mut edited)
                    .range(*min..=f32::MAX)
                    .speed(0.05),
            )
            .changed()
            .then_some(FieldValue::F32(edited))
        }
        (FieldValue::F64(current), Row::FloatSlider { min, max }) => {
            let mut edited = *current;
            let (low, high) = ascending(*min, *max);
            ui.add(egui::Slider::new(&mut edited, low as f64..=high as f64).max_decimals(9))
                .changed()
                .then_some(FieldValue::F64(edited))
        }
        (FieldValue::F64(current), Row::FloatDrag { min }) => {
            let mut edited = current.max(*min as f64);
            ui.add(
                egui::DragValue::new(&mut edited)
                    .range(*min as f64..=f64::MAX)
                    .speed(0.05)
                    .max_decimals(9),
            )
            .changed()
            .then_some(FieldValue::F64(edited))
        }
        // Every whole-number kind shares one editor: the decorator picks the
        // widget, the kind supplies its own bounds.
        (_, Row::IntegerSlider { min, max }) => {
            integer_edit(ui, &Number::of(value)?, Some((*min, *max)), None)
        }
        (_, Row::IntegerDrag { min }) => integer_edit(ui, &Number::of(value)?, None, Some(*min)),
        _ => scalar_edit(ui, value),
    }
}

/// One axis letter in its fixed slot: the slot keeps every component's editor
/// the same width, whatever the glyph's own advance is.
pub fn axis_slot(ui: &mut egui::Ui, axis: usize) {
    ui.add_sized(
        egui::vec2(INSPECTOR_VECTOR_LABEL_WIDTH, INSPECTOR_VECTOR_FIELD_HEIGHT),
        egui::Label::new(
            egui::RichText::new(inspector::axis_label(axis))
                .color(inspector::axis_color(axis))
                .small(),
        ),
    );
}

/// One vector component's editor at the width left in its group: the width is
/// claimed exactly, and whatever the editor paints past it - a long value, the
/// scrub surface of a drag value - is cropped instead of widening the row, so
/// every component of a vector keeps the same width whatever its value.
///
/// The crop is the child Ui's clip rect, which also bounds the child's
/// interaction, so a press past the field's edge never lands on this editor.
/// `widget` is the decorator the row model chose, or `None` for the plain
/// editor.
pub fn fixed_width_field(
    ui: &mut egui::Ui,
    value: &FieldValue,
    widget: Option<&inspector::FieldWidget>,
) -> Option<FieldValue> {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), INSPECTOR_VECTOR_FIELD_HEIGHT),
        egui::Sense::hover(),
    );
    let mut field = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    field.set_clip_rect(rect.intersect(ui.clip_rect()));
    match widget {
        Some(widget) => scalar_edit_decorated(&mut field, value, widget),
        None => scalar_edit(&mut field, value),
    }
}

/// Two bounds in ascending order, so a swapped decorator pair still draws a
/// usable slider.
pub fn ascending(min: f32, max: f32) -> (f32, f32) {
    if min <= max {
        (min, max)
    } else {
        (max, min)
    }
}

/// One whole number seen through `f64`, with the way back into its own
/// `FieldValue`, so the slider and the drag value are written once rather
/// than once per kind. Each kind carries its own bounds: a `[Range]` wider
/// than the kind clamps to the kind's limit.
///
/// Public like every item here: the build script's inventory names it at the
/// crate root, and a patch crate reaches it through the shell's re-exports.
pub struct Number {
    /// The value, as the editors read and write it.
    pub current: f64,
    /// The kind's own lower bound.
    pub kind_min: f64,
    /// The kind's own upper bound.
    pub kind_max: f64,
    /// The kind's `FieldValue` constructor for an edited value.
    pub rebuild: fn(f64) -> FieldValue,
}

impl Number {
    /// The number inside `value` with its kind's own bounds, or `None` for a
    /// kind without a numeric editor.
    pub fn of(value: &FieldValue) -> Option<Number> {
        Some(match value {
            FieldValue::I8(current) => Number {
                current: *current as f64,
                kind_min: i8::MIN as f64,
                kind_max: i8::MAX as f64,
                rebuild: |edited| FieldValue::I8(edited as i8),
            },
            FieldValue::I16(current) => Number {
                current: *current as f64,
                kind_min: i16::MIN as f64,
                kind_max: i16::MAX as f64,
                rebuild: |edited| FieldValue::I16(edited as i16),
            },
            FieldValue::I32(current) => Number {
                current: *current as f64,
                kind_min: i32::MIN as f64,
                kind_max: i32::MAX as f64,
                rebuild: |edited| FieldValue::I32(edited as i32),
            },
            FieldValue::I64(current) => Number {
                current: *current as f64,
                kind_min: i64::MIN as f64,
                kind_max: i64::MAX as f64,
                rebuild: |edited| FieldValue::I64(edited as i64),
            },
            FieldValue::U8(current) => Number {
                current: *current as f64,
                kind_min: u8::MIN as f64,
                kind_max: u8::MAX as f64,
                rebuild: |edited| FieldValue::U8(edited as u8),
            },
            FieldValue::U16(current) => Number {
                current: *current as f64,
                kind_min: u16::MIN as f64,
                kind_max: u16::MAX as f64,
                rebuild: |edited| FieldValue::U16(edited as u16),
            },
            FieldValue::U32(current) => Number {
                current: *current as f64,
                kind_min: u32::MIN as f64,
                kind_max: u32::MAX as f64,
                rebuild: |edited| FieldValue::U32(edited as u32),
            },
            FieldValue::U64(current) => Number {
                current: *current as f64,
                kind_min: u64::MIN as f64,
                kind_max: u64::MAX as f64,
                rebuild: |edited| FieldValue::U64(edited as u64),
            },
            FieldValue::Usize(current) => Number {
                current: *current as f64,
                kind_min: usize::MIN as f64,
                kind_max: usize::MAX as f64,
                rebuild: |edited| FieldValue::Usize(edited as usize),
            },
            FieldValue::Isize(current) => Number {
                current: *current as f64,
                kind_min: isize::MIN as f64,
                kind_max: isize::MAX as f64,
                rebuild: |edited| FieldValue::Isize(edited as isize),
            },
            _ => return None,
        })
    }

    /// `current` clamped into `low..=high` and rounded, ready for a
    /// whole-number widget.
    pub fn rounded(&self, low: f64, high: f64) -> f64 {
        self.current.clamp(low, high).round()
    }
}

/// The whole-number editor for `number`: a slider over `range`, or a drag
/// value with `floor`, both clamped to the kind's own bounds.
pub fn integer_edit(
    ui: &mut egui::Ui,
    number: &Number,
    range: Option<(f32, f32)>,
    floor: Option<f32>,
) -> Option<FieldValue> {
    if let Some((low, high)) = range {
        let low = (low as f64).clamp(number.kind_min, number.kind_max);
        let high = (high as f64).clamp(number.kind_min, number.kind_max);
        let (low, high) = if low <= high {
            (low, high)
        } else {
            (high, low)
        };
        let mut current = number.rounded(low, high);
        return ui
            .add(
                egui::Slider::new(&mut current, low..=high)
                    .step_by(1.0)
                    .fixed_decimals(0),
            )
            .changed()
            .then(|| (number.rebuild)(current.round()));
    }
    let low = (floor? as f64).clamp(number.kind_min, number.kind_max);
    let mut current = number.rounded(low, number.kind_max);
    ui.add(
        egui::DragValue::new(&mut current)
            .range(low..=number.kind_max)
            .speed(1.0)
            .fixed_decimals(0),
    )
    .changed()
    .then(|| (number.rebuild)(current.round()))
}

/// The edit control for one scalar: a checkbox for booleans, a drag value
/// (drag to scrub, click to type) for numbers. `None` when the kind has no
/// widget, so the caller paints it read-only.
pub fn scalar_edit(ui: &mut egui::Ui, value: &FieldValue) -> Option<FieldValue> {
    match value {
        FieldValue::Bool(current) => {
            let mut edited = *current;
            ui.checkbox(&mut edited, "")
                .changed()
                .then_some(FieldValue::Bool(edited))
        }
        FieldValue::F32(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited).speed(0.05))
                .changed()
                .then_some(FieldValue::F32(edited))
        }
        FieldValue::F64(current) => {
            let mut edited = *current;
            ui.add(
                egui::DragValue::new(&mut edited)
                    .speed(0.05)
                    .max_decimals(9),
            )
            .changed()
            .then_some(FieldValue::F64(edited))
        }
        // Every whole-number kind shares one drag value here; a kind without
        // a numeric editor answers `None`.
        other => {
            let number = Number::of(other)?;
            let mut edited = number.current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then(|| (number.rebuild)(edited))
        }
    }
}

/// One read-only (name, value) row: the shared label column, then the value.
/// The label carries itself as a tooltip, because the column truncates a
/// long name before its widget would, and a value too long for the row is
/// truncated with its own tooltip rather than pushing the row wider.
pub fn read_only_row(ui: &mut egui::Ui, label: &str, value: &str) {
    row(ui, label, Some(label), &mut |ui| {
        ui.add(egui::Label::new(value).truncate())
            .on_hover_text(value);
    });
}

/// A field value as display text; the kinds without their own formatting
/// fall back to their debug form.
pub fn format_field_value(value: &FieldValue) -> String {
    match value {
        FieldValue::F32(number) => format!("{number}"),
        FieldValue::F64(number) => format!("{number}"),
        FieldValue::I8(number) => format!("{number}"),
        FieldValue::I16(number) => format!("{number}"),
        FieldValue::I32(number) => format!("{number}"),
        FieldValue::I64(number) => format!("{number}"),
        FieldValue::U8(number) => format!("{number}"),
        FieldValue::U16(number) => format!("{number}"),
        FieldValue::U32(number) => format!("{number}"),
        FieldValue::U64(number) => format!("{number}"),
        FieldValue::Usize(number) => format!("{number}"),
        FieldValue::Bool(flag) => format!("{flag}"),
        FieldValue::Text(text) => text.clone(),
        other => format!("{other:?}"),
    }
}

/// The Inspector's rows: one (type name, field rows) pair per component of
/// the detail, each field row a (name, formatted value) pair.
pub fn component_rows(detail: &EntityDetail) -> Vec<(String, Vec<(String, String)>)> {
    detail
        .components
        .iter()
        .map(|component| {
            let fields = component
                .fields
                .iter()
                .zip(&component.values)
                .map(|(descriptor, value)| (descriptor.name.to_string(), format_field_value(value)))
                .collect();
            (component.type_name.clone(), fields)
        })
        .collect()
}
