//! The Spline Tool panel (W-0038 Stage 12).
//!
//! # Responsibilities
//!
//! - [`spline_tool_panel`]: list the entities that carry a spline, select one,
//!   show its values, and edit them through the same recorded commands the
//!   Inspector uses.
//! - [`reseed_window`]: the re-seed confirmation, a modal the shell owns and
//!   this panel draws (W-0038 Stage 13).
//! - [`reseed_command`]: the write the confirmation applies, shared by the
//!   modal's answer and the scripted channel so a test drives the same code
//!   the button does.
//!
//! # Design
//!
//! The panel is the reference a module's editor crate is written against: it
//! names no crate's types. A spline is found by its component's *name*
//! (`archetype` is the joined component names the Entities tab already shows),
//! and its values arrive as text through [`PanelContext`], so nothing here can
//! outlive the module that registered it.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;

/// The panel's registry id.
pub const SPLINE_PANEL_ID: &str = "shell.spline_tool";
/// The component a spline entity carries, by registered name.
pub const SPLINE_COMPONENT: &str = "Spline";
/// The re-seed confirmation's window id.
pub const RESEED_WINDOW: &str = "spline.reseed";
/// How many control points a re-seed leaves in use.
pub const RESEED_POINTS: usize = 4;

/// The panel body: the same code draws the docked tab and the modal.
pub fn spline_tool_panel(ui: &mut egui::Ui, context: &mut PanelContext<'_>) {
    // The re-seed confirmation draws its own body and nothing else.
    if context.window == Some(RESEED_WINDOW) {
        reseed_body(ui, context);
        return;
    }
    ui.heading("Spline Tool");
    ui.label("Edits a spline through the editor's recorded commands.");
    ui.separator();
    let splines: Vec<PanelEntityRow> = context
        .entities
        .iter()
        .filter(|row| row.archetype.contains(SPLINE_COMPONENT))
        .cloned()
        .collect();
    if splines.is_empty() {
        ui.label("No entity carries a spline.");
        return;
    }
    for row in &splines {
        let selected = context.selected_entity == Some(row.entity);
        if ui
            .selectable_label(selected, format!("{} ({})", row.display, row.entity))
            .clicked()
        {
            context
                .commands
                .push(PanelCommand::SelectEntity(row.entity));
        }
    }
    let Some(detail) = context.detail else {
        ui.separator();
        ui.label("Select a spline to edit it.");
        return;
    };
    let Some(component) = detail
        .components
        .iter()
        .find(|component| component.component == SPLINE_COMPONENT)
    else {
        ui.separator();
        ui.label("The selected entity carries no spline.");
        return;
    };
    ui.separator();
    for field in &component.fields {
        ui.horizontal(|ui| {
            ui.label(field.name.as_str());
            ui.monospace(field.value.as_str());
        });
    }
    if ui.button("Re-seed...").clicked() {
        context
            .commands
            .push(PanelCommand::OpenWindow(reseed_window()));
    }
    // The answer the shell took from the modal, exactly once.
    if let Some(answer) = context.answer {
        if answer.window == RESEED_WINDOW && answer.accepted {
            context
                .commands
                .push(reseed_command(detail.entity).into_command());
        }
    }
}

/// The re-seed confirmation's body.
///
/// The shell draws the buttons and reads Enter and Escape; this only says what
/// the confirmation is for.
pub fn reseed_body(ui: &mut egui::Ui, context: &mut PanelContext<'_>) {
    ui.label(format!(
        "Re-seed the spline to {} control points?",
        RESEED_POINTS
    ));
    match context.selected_entity {
        Some(entity) => ui.monospace(format!("entity {entity}")),
        None => ui.label("no spline selected"),
    };
}

/// The window the panel asks for: a modal, so the editor waits for an answer.
pub fn reseed_window() -> PanelWindowRequest {
    PanelWindowRequest {
        id: RESEED_WINDOW.to_string(),
        title: "Re-seed spline".to_string(),
        width: 360.0,
        height: 140.0,
        modal: true,
    }
}

/// The write the confirmation applies: the recorded field edit that sets the
/// control-point count.
pub struct Reseed {
    /// The entity the spline belongs to.
    pub entity: Entity,
    /// The value written into the component's count field.
    pub value: usize,
}

impl Reseed {
    /// The panel command a confirmation produces.
    pub fn into_command(self) -> PanelCommand {
        PanelCommand::SetField {
            entity: self.entity,
            component: SPLINE_COMPONENT.to_string(),
            field: "control_point_count".to_string(),
            value: self.value.to_string(),
        }
    }
}

/// The re-seed write for one entity.
pub fn reseed_command(entity: Entity) -> Reseed {
    Reseed {
        entity,
        value: RESEED_POINTS,
    }
}
