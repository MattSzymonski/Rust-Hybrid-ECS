//! The Inspector's resource section.
//!
//! # Responsibilities
//!
//! - [`resource_section`]: draw one editable resource's rows with the same
//!   widgets the component rows use, and queue each commit as a recorded
//!   `SetResourceField`.
//!
//! # Design
//!
//! The rows come from the snapshot, which read them through the resource's
//! curated table (`pill_editor_core::resources`), so the tab cannot offer
//! a field the write path would refuse. Every commit is one recorded command,
//! which is what makes a resource edit undoable like any other edit.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// The resource section: a heading, then one row per editable field.
pub fn resource_section(
    ui: &mut egui::Ui,
    view: &ResourceView,
    pending: &mut PendingEditorCommands,
) {
    ui.heading(view.label.as_str());
    ui.label("A world resource. Every edit here is one undo step.");
    ui.separator();
    for field in &view.fields {
        row(ui, field.label.as_str(), None, &mut |ui| {
            // The shell's one scalar editor decides the control, so every
            // numeric kind a resource exposes is editable - the hand match this
            // replaces covered a boolean and an `f32` and rendered the rest as
            // text.
            if let Some(edited) = scalar_edit(ui, &field.value) {
                queue_edit(pending, view, field, edited);
                return;
            }
            // What has no widget - an enum's discriminant, an opaque blob -
            // reads as text.
            ui.monospace(inspector::value_text(&field.value));
        });
    }
}

/// Queue one field write as a recorded `SetResourceField`.
pub fn queue_edit(
    pending: &mut PendingEditorCommands,
    view: &ResourceView,
    field: &ResourceFieldView,
    value: FieldValue,
) {
    pending.push_recorded(
        vec![EditorCommand::SetResourceField {
            resource: view.name.clone(),
            field: field.field.clone(),
            value,
        }],
        format!("set {}.{}", view.name, field.field),
        Some(format!("resource.{}.{}", view.name, field.field)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rows hand every value to one control, so every kind that control
    /// edits is editable in a resource: the hand match this replaced covered a
    /// boolean and an `f32` and painted the rest.
    #[test]
    fn every_numeric_kind_a_resource_carries_is_editable() {
        for value in [
            FieldValue::Bool(true),
            FieldValue::F32(1.0),
            FieldValue::F64(1.0),
            FieldValue::I8(1),
            FieldValue::I32(1),
            FieldValue::U16(1),
            FieldValue::U64(1),
            FieldValue::Isize(1),
        ] {
            assert_ne!(
                inspector::widget_for(&value),
                inspector::FieldWidget::ReadOnly,
                "{value:?} reaches a control, so the row routes it through `scalar_edit`"
            );
        }
        assert_eq!(
            inspector::widget_for(&FieldValue::Text("x".to_string())),
            inspector::FieldWidget::ReadOnly,
            "text keeps the painted fallback"
        );
    }
}
