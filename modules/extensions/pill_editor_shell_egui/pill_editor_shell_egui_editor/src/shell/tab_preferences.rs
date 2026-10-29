//! The Preferences tab: the editor-wide rows and the project's gizmo state.
//!
//! # Responsibilities
//!
//! - `preferences_tab`: the editor-wide rows (seeded into the session's draft
//!   buffers, saved as one batch) and the project's gizmo rows.
//!
//! The globs below are absolute paths: a generated patch crate replays this
//! file's top-level `use` lines, so a `super::`-relative glob would not
//! resolve there.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// The Preferences tab: the editor-wide rows seeded from the preferences file
/// and saved as one read-modify-write batch, then the project's remembered
/// gizmo choices - the same `SetGizmoTool` commands the shortcuts, the menus
/// and the scripted channel queue.
#[pill_hot_fn]
pub fn preferences_tab(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    draft: &mut Option<preferences::PreferencesDraft>,
    pending: &mut PendingEditorCommands,
) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        let view = &snapshot.preferences;
        // The buffers seed from the file once and stay until a save;
        // `None` means "re-seed from the file" and covers the first frame
        // and the frame after a save.
        let buffers = draft.get_or_insert_with(|| view.seed_buffers());
        ui.heading("Editor");
        ui.label(
            egui::RichText::new("Preferences file: blank keeps the default, Save writes the rows.")
                .weak(),
        );
        for field in preferences::PREFERENCE_FIELDS {
            let buffer = buffers.entry(field.key.to_string()).or_default();
            // The previews switch is a real checkbox - a boolean row whose
            // state the person can see - written back as the text the
            // document's parser reads. It applies the moment it is ticked: the
            // tiles on screen start rendering without a Save, while every other
            // row still waits for one.
            if field.key == pill_editor_core::previews::RENDER_PREVIEWS_KEY {
                let mut enabled = buffer.trim().eq_ignore_ascii_case("true");
                let mut changed = false;
                row_in(ui, WIDE_ROW_LABEL_WIDTH, field.label, None, &mut |ui| {
                    changed = ui.checkbox(&mut enabled, "").changed();
                });
                if changed {
                    *buffer = if enabled { "true" } else { "false" }.to_string();
                    pending.push(vec![EditorCommand::SetPreferences {
                        values: vec![(field.key.to_string(), buffer.clone())],
                    }]);
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ENGINE,
                        enabled,
                        "[editor] preference applied"
                    );
                }
                continue;
            }
            row_in(ui, WIDE_ROW_LABEL_WIDTH, field.label, None, &mut |ui| {
                ui.add(egui::TextEdit::singleline(buffer).desired_width(ui.available_width()));
            });
        }
        let mut save = false;
        ui.horizontal(|ui| {
            save = ui
                .add(icons::IconButton::new(
                    icons::Icon::FloppyDisk,
                    "Save preferences",
                ))
                .clicked();
        });
        if save {
            let values = preferences::PREFERENCE_FIELDS
                .iter()
                .map(|field| {
                    (
                        field.key.to_string(),
                        buffers.get(field.key).cloned().unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>();
            pending.push(vec![EditorCommand::SetPreferences { values }]);
            // Re-seed from the file next frame, so the rows show what the
            // save actually wrote.
            *draft = None;
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                "[editor] preferences save queued"
            );
        }
        ui.separator();
        ui.heading("Project");
        value_row_in(
            ui,
            WIDE_ROW_LABEL_WIDTH,
            "project",
            view.project.as_deref().unwrap_or("(unknown)"),
        );
        if view.project_remembered {
            value_row_in(
                ui,
                WIDE_ROW_LABEL_WIDTH,
                "remembered mode",
                view.project_gizmo.mode.map_or("(unset)", gizmo_mode_label),
            );
            value_row_in(
                ui,
                WIDE_ROW_LABEL_WIDTH,
                "remembered space",
                view.project_gizmo
                    .space
                    .map_or("(unset)", gizmo_space_label),
            );
            value_row_in(
                ui,
                WIDE_ROW_LABEL_WIDTH,
                "remembered snap",
                &view
                    .project_gizmo
                    .snap
                    .map_or("(unset)".to_string(), |snap| snap.to_string()),
            );
        } else {
            row_in(ui, WIDE_ROW_LABEL_WIDTH, "remembered", None, &mut |ui| {
                ui.weak("none yet - a tool choice is remembered per project.");
            });
        }
        value_row_in(
            ui,
            WIDE_ROW_LABEL_WIDTH,
            "active mode",
            gizmo_mode_label(snapshot.gizmo.mode),
        );
        value_row_in(
            ui,
            WIDE_ROW_LABEL_WIDTH,
            "active space",
            gizmo_space_label(snapshot.gizmo.space),
        );
        value_row_in(
            ui,
            WIDE_ROW_LABEL_WIDTH,
            "active snap",
            &snapshot.gizmo.snap.to_string(),
        );
        row_in(ui, WIDE_ROW_LABEL_WIDTH, "gizmo", None, &mut |ui| {
            let state = toolbox::ToolboxState {
                gizmo: snapshot.gizmo,
                grid: snapshot.grid_enabled,
            };
            for item in toolbox::TOOL_ROWS {
                let button = icons::IconButton::new(item.icon(&state), item.label(&state))
                    .active(item.active(&state));
                if ui.add(button).clicked() {
                    pending.push(vec![item.command(&state)]);
                }
            }
            // The Preferences row is narrow, so the spaces use their short
            // names here rather than the menu's.
            for row in toolbox::SPACE_ROWS {
                let button = icons::IconButton::new(row.icon, row.short)
                    .active(snapshot.gizmo.space == row.space);
                if ui.add(button).clicked() {
                    pending.push(vec![toolbox::space_command(row.space)]);
                }
            }
            let snap = icons::IconButton::new(
                toolbox::ToolboxItem::Snap.icon(&state),
                toolbox::ToolboxItem::Snap.label(&state),
            )
            .active(toolbox::ToolboxItem::Snap.active(&state));
            if ui.add(snap).clicked() {
                pending.push(vec![toolbox::ToolboxItem::Snap.command(&state)]);
            }
        });
    });
}
