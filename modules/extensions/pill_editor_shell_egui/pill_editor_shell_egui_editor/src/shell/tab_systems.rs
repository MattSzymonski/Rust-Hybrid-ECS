//! The Systems tab: the engine's pause and single-step controls, then one
//! row per registered system with its recorded, undoable toggle.
//!
//! # Responsibilities
//!
//! - Draw [`systems_tab`]: the header's Pause/Resume and Step buttons, then
//!   the systems' rows.
//!
//! # Design
//!
//! A patch crate is this file's top-level `use` lines plus the crate's own
//! glob, so the two openings below are the whole rule: `shell::prelude::*`
//! reaches the shared model's vocabulary (icons, systems, the snapshot types)
//! and `shell::*` reaches the sibling modules this crate declares. A body that
//! names either by bare name compiles as a patch only while both are here.

// External crates
use pill_engine::pill_hot_fn;

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// The Systems tab: the engine's pause and single-step controls in a header
/// row, then one row per registered system - its name and owner, and a state
/// button (`on`/`off`) that queues the recorded, undoable system toggle.
#[pill_hot_fn]
pub fn systems_tab(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    pending: &mut PendingEditorCommands,
) {
    ui.horizontal(|ui| {
        let pause_icon = if snapshot.paused {
            icons::Icon::Play
        } else {
            icons::Icon::Pause
        };
        if ui
            .add(icons::IconButton::new(
                pause_icon,
                systems::pause_button_label(snapshot.paused),
            ))
            .clicked()
        {
            pending.push(vec![systems::pause_toggle_command(snapshot.paused)]);
        }
        if ui
            .add(icons::IconButton::new(icons::Icon::SkipForward, "Step"))
            .clicked()
        {
            pending.push(vec![EditorCommand::StepOnce]);
        }
        let enabled = snapshot.systems.iter().filter(|row| row.enabled).count();
        ui.label(
            egui::RichText::new(format!("{}/{} enabled", enabled, snapshot.systems.len())).weak(),
        );
    });
    ui.separator();
    egui::ScrollArea::vertical().show(ui, |ui| {
        for system in &snapshot.systems {
            // A system's own row label carries the module it belongs to, so a
            // long crate name is the one label the wide column can truncate;
            // the tooltip keeps the whole of it reachable.
            let name = systems::system_row_label(system);
            row_in(
                ui,
                WIDE_ROW_LABEL_WIDTH,
                &name,
                Some(name.as_str()),
                &mut |ui| {
                    if ui
                        .add(icons::IconButton::new(
                            icons::Icon::Power,
                            systems::system_state_label(system.enabled),
                        ))
                        .clicked()
                    {
                        let (command, label) =
                            systems::system_toggle(system.index, !system.enabled);
                        pending.push_recorded(vec![command], label, None);
                    }
                },
            );
        }
    });
}
