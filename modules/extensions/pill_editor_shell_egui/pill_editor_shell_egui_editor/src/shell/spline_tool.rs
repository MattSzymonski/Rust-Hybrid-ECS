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

/// The component field the control points live in.
pub const SPLINE_POINTS_FIELD: &str = "control_points";

/// How many control-point rows the panel draws: a spline may carry more than a
/// person wants to scroll past, and the rest stay reachable in the Inspector.
pub const POINTS_SHOWN: usize = 8;

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
    // The control points. Stage 15's expanded layout turns an element of a
    // struct array into one scalar row per leaf (`control_points[0].x`), so a
    // point is three ordinary fields and the same recorded write path serves
    // it - no struct value ever has to cross this crate's boundary.
    let points = spline_point_count(component);
    if points > 0 {
        ui.label(format!("{points} control points"));
        for index in 0..points.min(POINTS_SHOWN) {
            ui.horizontal(|ui| {
                ui.label(format!("point {index}"));
                for axis in 0..3 {
                    let Some(field) = point_leaf(component, index, axis) else {
                        continue;
                    };
                    // The row's value is the text the shell rendered it as; a
                    // leaf that is not a float has no drag value.
                    let Ok(mut value) = field.value.parse::<f32>() else {
                        continue;
                    };
                    let name = field.name.clone();
                    if ui
                        .add(egui::DragValue::new(&mut value).speed(0.05))
                        .changed()
                    {
                        context
                            .commands
                            .push(point_write(detail.entity, name.as_str(), value));
                    }
                }
            });
        }
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

/// How many control points the selected spline's rows describe.
///
/// The count comes from the rows themselves: the expanded layout names every
/// element `control_points[i].<axis>`, so the highest index seen plus one is
/// what the panel can offer.
pub fn spline_point_count(component: &PanelComponentValues) -> usize {
    let mut count = 0;
    for field in &component.fields {
        if let Some(index) = field_element_index(field.name.as_str()) {
            count = count.max(index + 1);
        }
    }
    count
}

/// The leaf of one control point's axis (0 = x, 1 = y, 2 = z).
pub fn point_leaf(
    component: &PanelComponentValues,
    index: usize,
    axis: usize,
) -> Option<&PanelFieldValue> {
    let suffix = ["x", "y", "z"].get(axis)?;
    component
        .fields
        .iter()
        .find(|field| field.name == format!("{SPLINE_POINTS_FIELD}[{index}].{suffix}"))
}

/// The write one control-point row queues when its drag value changes.
///
/// `field` is the expanded leaf's name (`control_points[0].x`), so the write is
/// an ordinary recorded scalar edit - the same one an Inspector row queues.
pub fn point_write(entity: Entity, field: &str, value: f32) -> PanelCommand {
    PanelCommand::SetField {
        entity,
        component: SPLINE_COMPONENT.to_string(),
        field: field.to_string(),
        value: format!("{value}"),
    }
}

/// The element index a dotted element row names (`control_points[3].y` -> 3).
///
/// Public because the function inventory names every function by path.
pub fn field_element_index(name: &str) -> Option<usize> {
    let rest = name.strip_prefix(SPLINE_POINTS_FIELD)?.strip_prefix('[')?;
    let (index, _) = rest.split_once(']')?;
    index.parse().ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The component rows the engine's expanded layout hands a panel for a
    /// two-point spline.
    fn points_component() -> PanelComponentValues {
        let mut fields = vec![PanelFieldValue {
            name: "control_point_count".to_string(),
            value: "2".to_string(),
        }];
        for index in 0..2 {
            for axis in ["x", "y", "z"] {
                fields.push(PanelFieldValue {
                    name: format!("control_points[{index}].{axis}"),
                    value: format!("{}.{}", index + 1, axis.len()),
                });
            }
        }
        PanelComponentValues {
            component: SPLINE_COMPONENT.to_string(),
            fields,
        }
    }

    /// A row per point, each pointing at that point's own expanded leaves.
    #[test]
    fn control_points_become_one_row_per_point() {
        let component = points_component();
        assert_eq!(spline_point_count(&component), 2);
        let y_of_second = point_leaf(&component, 1, 1).expect("second point's y leaf");
        assert_eq!(y_of_second.name, "control_points[1].y");
        // A point past the rows the panel draws is simply absent, not stale.
        assert!(point_leaf(&component, 2, 0).is_none());
    }

    /// A row's write is the recorded scalar edit an Inspector row queues.
    #[test]
    fn a_control_point_row_queues_a_recorded_field_write() {
        let entity = Entity::from_raw(7, 0);
        match point_write(entity, "control_points[0].x", 2.5) {
            PanelCommand::SetField {
                entity: written,
                component,
                field,
                value,
            } => {
                assert_eq!(written, entity);
                assert_eq!(component, SPLINE_COMPONENT);
                assert_eq!(field, "control_points[0].x");
                assert_eq!(value, "2.5");
            }
            other => panic!("a control-point row queued {other:?}"),
        }
    }

    /// The element index the panel counts rows from.
    #[test]
    fn element_indices_come_from_the_expanded_leaf_names() {
        assert_eq!(field_element_index("control_points[3].y"), Some(3));
        assert_eq!(field_element_index("control_point_count"), None);
        assert_eq!(field_element_index("points[0].x"), None);
    }
}
