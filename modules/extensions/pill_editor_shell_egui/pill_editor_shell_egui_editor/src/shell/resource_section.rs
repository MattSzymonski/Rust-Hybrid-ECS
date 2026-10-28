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
            match &field.value {
                FieldValue::Bool(value) => {
                    let mut checked = *value;
                    if ui.checkbox(&mut checked, "").changed() {
                        queue_edit(pending, view, field, FieldValue::Bool(checked));
                    }
                }
                FieldValue::F32(value) => {
                    let mut number = *value;
                    if ui
                        .add(egui::DragValue::new(&mut number).speed(0.01))
                        .changed()
                    {
                        queue_edit(pending, view, field, FieldValue::F32(number));
                    }
                }
                // An enum field travels as its discriminant, so the row shows
                // the number.
                other => {
                    ui.monospace(inspector::value_text(other));
                }
            }
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
