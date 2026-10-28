//! The Inspector's field rows: the decorators a component declares and the
//! control each row draws.
//!
//! # Responsibilities
//!
//! - `inspector_field_row`: one component field's row, with the decorators a component declares (`[Header]`, `[Space]`, `[Tooltip]`, `[Range]`, `[Min]`, the multi-line boxes, `[HideInInspector]`) applied to it.
//! - `inspector_row` and `is_scalar_field`: which control a field gets.
//! - `inspector_asset_row` and `inspector_entity_row`: a reference row - the
//!   named target, its picker, its clear button and a drop target for an
//!   asset or entity dragged out of another tab.
//! - `remember_inspector_row` and `field_widget_name`: the once-per-row logging the suite asserts on.
//! - `remember_asset_row_text`, `remember_asset_row_rect` and the entity row's
//!   twins: a row's current text (logged per change) and its rectangle (logged
//!   per placement), which the editor suite waits on.
//!
//! The globs below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// One row of a component section, drawn from the Inspector's row model: a
/// separator line or gap a field's decorators declared, a single widget, a
/// vector row (side-by-side fields with axis labels in the gizmo's colours),
/// or a read-only value.
// The parameters are the borrows one row draws from; a grouping struct would
// have to be rebuilt for every row of every component.
#[allow(clippy::too_many_arguments)]
pub fn inspector_field_row(
    ui: &mut egui::Ui,
    entity: Entity,
    component: &ComponentView,
    row: &inspector::FieldRow,
    targets: &[Entity],
    details: &[EntityDetail],
    assets: &[AssetListing],
    engine_assets: &[EngineAsset],
    scene: &[EntityListEntry],
    pending: &mut PendingEditorCommands,
    session: &mut InspectorSession,
    overlay_regions: &mut Vec<ShellRect>,
) {
    let type_name = component.type_name.as_str();
    match row {
        inspector::FieldRow::Header { text } => {
            if remember_inspector_row(session, type_name, "header", &[text.as_str()]) {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    component = type_name,
                    text = text.as_str(),
                    "[editor] inspector row kind=header"
                );
            }
            ui.add_space(6.0);
            ui.label(egui::RichText::new(text).strong());
            ui.add_space(2.0);
        }
        inspector::FieldRow::Space { pixels } => {
            if remember_inspector_row(session, type_name, "space", &[&pixels.to_string()]) {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    component = type_name,
                    pixels = *pixels,
                    "[editor] inspector row kind=space"
                );
            }
            ui.add_space(*pixels);
        }
        inspector::FieldRow::Single {
            index,
            field,
            widget,
            tooltip,
        } => {
            let Some(value) = component.values.get(*index) else {
                return;
            };
            let tooltip = tooltip.as_deref();
            if row_is_mixed(details, type_name, *index, value)
                && remember_inspector_row(session, type_name, "mixed", &[field.as_str()])
            {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    component = type_name,
                    field = field.as_str(),
                    entities = targets.len(),
                    "[editor] inspector row kind=mixed"
                );
            }
            if remember_inspector_row(
                session,
                type_name,
                "field",
                &[
                    field.as_str(),
                    field_widget_name(widget),
                    tooltip.unwrap_or_default(),
                ],
            ) {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    component = type_name,
                    field = field.as_str(),
                    widget = field_widget_name(widget),
                    tooltip = tooltip.unwrap_or_default(),
                    "[editor] inspector row kind=field"
                );
            }
            match value {
                FieldValue::Text(current) => {
                    let mut edited = current.clone();
                    let lines = match widget {
                        inspector::FieldWidget::MultilineText { lines } => Some(*lines),
                        _ => None,
                    };
                    inspector_row(
                        ui,
                        &row_label(field, details, type_name, *index, value),
                        tooltip,
                        &mut |ui| {
                            let text_edit = match lines {
                                Some(lines) => egui::TextEdit::multiline(&mut edited)
                                    .desired_rows(lines)
                                    .desired_width(f32::INFINITY),
                                None => egui::TextEdit::singleline(&mut edited)
                                    .desired_width(f32::INFINITY),
                            };
                            let response = ui.add(text_edit);
                            if response.lost_focus() && edited != *current {
                                let committed = FieldValue::Text(edited.clone());
                                queue_field_edit(
                                    pending, targets, entity, type_name, field, committed,
                                );
                            }
                        },
                    );
                }
                other => {
                    inspector_row(
                        ui,
                        &row_label(field, details, type_name, *index, other),
                        tooltip,
                        &mut |ui| match scalar_edit_decorated(ui, other, widget) {
                            Some(edited) => {
                                queue_field_edit(
                                    pending, targets, entity, type_name, field, edited,
                                );
                            }
                            None => {
                                ui.label(format_field_value(other));
                            }
                        },
                    );
                }
            }
        }
        inspector::FieldRow::Vector {
            index,
            field,
            elements,
            tooltip,
            ..
        } => {
            let Some(FieldValue::Array(values)) = component.values.get(*index) else {
                return;
            };
            if row_is_mixed(
                details,
                type_name,
                *index,
                &FieldValue::Array(values.clone()),
            ) && remember_inspector_row(session, type_name, "mixed", &[field.as_str()])
            {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    component = type_name,
                    field = field.as_str(),
                    entities = targets.len(),
                    "[editor] inspector row kind=mixed"
                );
            }
            if remember_inspector_row(
                session,
                type_name,
                "vector",
                &[field.as_str(), tooltip.as_deref().unwrap_or_default()],
            ) {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    component = type_name,
                    field = field.as_str(),
                    tooltip = tooltip.as_deref().unwrap_or_default(),
                    "[editor] inspector row kind=vector"
                );
            }
            // One row that wraps: a wide panel shows X/Y/Z side by side, a
            // narrow dock moves the overflowing fields to the next line
            // instead of clipping them past the panel's edge.
            ui.horizontal_wrapped(|ui| {
                let label = ui.add_sized(
                    egui::vec2(ROW_LABEL_WIDTH, ROW_HEIGHT),
                    egui::Label::new(egui::RichText::new(field).color(style::text_muted()))
                        .truncate(),
                );
                if let Some(text) = tooltip.as_deref() {
                    label.on_hover_text(text);
                }
                for (element_index, element) in values.iter().take(*elements).enumerate() {
                    // One axis letter with its field as a unit, so a wrap
                    // never separates a label from its widget. The letter's
                    // slot and the field's width are both fixed, so a long
                    // value is cropped inside its field instead of widening
                    // the row and shifting its neighbours.
                    ui.allocate_ui_with_layout(
                        egui::vec2(INSPECTOR_VECTOR_FIELD_WIDTH, INSPECTOR_VECTOR_FIELD_HEIGHT),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            axis_slot(ui, element_index);
                            if let Some(edited) = fixed_width_field(ui, element, None) {
                                queue_element_edit(
                                    pending,
                                    targets,
                                    entity,
                                    type_name,
                                    field,
                                    element_index,
                                    edited,
                                );
                            }
                        },
                    );
                    ui.add_space(2.0);
                }
            });
        }
        inspector::FieldRow::Color {
            leaves,
            field,
            tooltip,
        } => {
            let values = leaf_values(component, leaves);
            if remember_inspector_row(
                session,
                type_name,
                "color",
                &[field.as_str(), tooltip.as_deref().unwrap_or_default()],
            ) {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    component = type_name,
                    field = field.as_str(),
                    channels = leaves.len(),
                    tooltip = tooltip.as_deref().unwrap_or_default(),
                    "[editor] inspector row kind=color"
                );
            }
            inspector_row(ui, field, tooltip.as_deref(), &mut |ui| {
                let Some(mut channels) = color_channels(&values) else {
                    // Channels the picker cannot read (integer kinds) stay
                    // editable one by one rather than becoming uneditable.
                    for (axis, value) in values.iter().enumerate() {
                        ui.label(
                            egui::RichText::new(inspector::axis_label(axis))
                                .color(inspector::axis_color(axis))
                                .small(),
                        );
                        if let Some(edited) = scalar_edit(ui, value) {
                            let leaf = &leaves[axis];
                            match leaf.element {
                                Some(element) => queue_element_edit(
                                    pending,
                                    targets,
                                    entity,
                                    type_name,
                                    &leaf.field,
                                    element,
                                    edited,
                                ),
                                None => queue_field_edit(
                                    pending,
                                    targets,
                                    entity,
                                    type_name,
                                    &leaf.field,
                                    edited,
                                ),
                            }
                        }
                    }
                    return;
                };
                // The picker writes linear channels, which is what the value
                // holds, so no conversion stands between the two.
                let response = ui.color_edit_button_rgba_unmultiplied(&mut channels);
                if response.changed() {
                    let edited: Vec<FieldValue> = leaves
                        .iter()
                        .zip(channels)
                        .map(|(leaf, channel)| match component.values.get(leaf.index) {
                            Some(FieldValue::F64(_)) => FieldValue::F64(channel as f64),
                            _ => FieldValue::F32(channel),
                        })
                        .collect();
                    let (commands, label, key) =
                        inspector::selection_leaf_edit(targets, entity, type_name, leaves, &edited);
                    pending.push_recorded(commands, label, Some(key));
                }
            });
        }
        inspector::FieldRow::VectorGroup {
            leaves,
            widget,
            field,
            tooltip,
        } => {
            if remember_inspector_row(
                session,
                type_name,
                "vector",
                &[field.as_str(), tooltip.as_deref().unwrap_or_default()],
            ) {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    component = type_name,
                    field = field.as_str(),
                    axes = leaves.len(),
                    tooltip = tooltip.as_deref().unwrap_or_default(),
                    "[editor] inspector row kind=vector"
                );
            }
            // The same row a `[f32; 3]` field draws, assembled from leaves
            // that live in fields of their own.
            ui.horizontal_wrapped(|ui| {
                let label = ui.add_sized(
                    egui::vec2(ROW_LABEL_WIDTH, ROW_HEIGHT),
                    egui::Label::new(egui::RichText::new(field).color(style::text_muted()))
                        .truncate(),
                );
                if let Some(text) = tooltip.as_deref() {
                    label.on_hover_text(text);
                }
                for (axis, leaf) in leaves.iter().enumerate() {
                    let Some(value) = component.values.get(leaf.index) else {
                        continue;
                    };
                    ui.allocate_ui_with_layout(
                        egui::vec2(INSPECTOR_VECTOR_FIELD_WIDTH, INSPECTOR_VECTOR_FIELD_HEIGHT),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            axis_slot(ui, axis);
                            if let Some(edited) = fixed_width_field(ui, value, Some(widget)) {
                                queue_field_edit(
                                    pending,
                                    targets,
                                    entity,
                                    type_name,
                                    &leaf.field,
                                    edited,
                                );
                            }
                        },
                    );
                    ui.add_space(2.0);
                }
            });
        }
        inspector::FieldRow::Asset {
            index,
            field,
            asset_type,
            tooltip,
        } => {
            inspector_asset_row(
                ui,
                AssetRowInput {
                    entity,
                    component,
                    index: *index,
                    field,
                    asset_type: asset_type.as_deref(),
                    tooltip: tooltip.as_deref(),
                    targets,
                    details,
                    assets,
                    engine_assets,
                },
                pending,
                session,
                overlay_regions,
            );
        }
        inspector::FieldRow::Entity {
            index,
            field,
            tooltip,
        } => {
            inspector_entity_row(
                ui,
                EntityRowInput {
                    entity,
                    component,
                    index: *index,
                    field,
                    tooltip: tooltip.as_deref(),
                    targets,
                    details,
                    scene,
                },
                pending,
                session,
                overlay_regions,
            );
        }
        inspector::FieldRow::ReadOnly {
            index,
            field,
            tooltip,
        } => {
            let Some(value) = component.values.get(*index) else {
                return;
            };
            if remember_inspector_row(
                session,
                type_name,
                "read-only",
                &[field.as_str(), tooltip.as_deref().unwrap_or_default()],
            ) {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    component = type_name,
                    field = field.as_str(),
                    tooltip = tooltip.as_deref().unwrap_or_default(),
                    "[editor] inspector row kind=read-only"
                );
            }
            inspector_row(ui, field, tooltip.as_deref(), &mut |ui| {
                ui.label(egui::RichText::new(format_field_value(value)).weak());
            });
        }
    }
}

/// What one asset reference row draws from: the field, its component and
/// the selection it writes to, and the project's assets the picker offers.
pub struct AssetRowInput<'a> {
    /// The primary entity the row describes.
    pub entity: Entity,
    /// The component section the row belongs to.
    pub component: &'a ComponentView,
    /// The field's position in the component's field list.
    pub index: usize,
    /// The field's registered name.
    pub field: &'a str,
    /// The registered asset type the field references, when recorded.
    pub asset_type: Option<&'a str>,
    /// The field's `[Tooltip]`, if it declared one.
    pub tooltip: Option<&'a str>,
    /// Every entity the edit reaches (the primary first).
    pub targets: &'a [Entity],
    /// The selected entities' details, for the mixed-value marker.
    pub details: &'a [EntityDetail],
    /// The project's assets, as the Assets tab lists them.
    pub assets: &'a [AssetListing],
    /// The store's own assets that no file backs: what the picker offers beside
    /// the listing's rows, badged as the engine's.
    pub engine_assets: &'a [EngineAsset],
}

/// One asset reference row: a button naming the referenced asset,
/// which opens a picker of every asset of the field's type, a clear button,
/// and a drop target that takes an asset dragged from the Assets tab.
///
/// Every choice queues one recorded `SetFieldAsset` step for the whole
/// selection; an unloaded asset is loaded when the step is applied.
pub fn inspector_asset_row(
    ui: &mut egui::Ui,
    input: AssetRowInput<'_>,
    pending: &mut PendingEditorCommands,
    session: &mut InspectorSession,
    overlay_regions: &mut Vec<ShellRect>,
) {
    let type_name = input.component.type_name.as_str();
    let label = input
        .component
        .asset_labels
        .get(input.index)
        .cloned()
        .flatten()
        .unwrap_or(AssetReferenceLabel::Missing);
    let text = label.text().to_owned();
    // One line per row and text: the row reports the asset it draws on its
    // first draw and on every change, so a suite can wait for a pick to show
    // up - and for the previous asset to show up again after an undo. The
    // entity is part of the key: two entities holding one asset are two rows.
    let key = format!("{}.{type_name}.{}", input.entity, input.field);
    if remember_asset_row_text(session, &key, &text) {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            entity = %input.entity,
            component = type_name,
            field = input.field,
            asset_type = input.asset_type.unwrap_or_default(),
            asset = text.as_str(),
            "[editor] inspector row kind=asset"
        );
    }

    // `Some(None)` clears the reference, `Some(Some(path))` points it at an
    // asset; collected while drawing and queued once afterwards.
    let mut chosen: Option<Option<String>> = None;
    let mut button_rect = None;
    let row_label = match input.component.values.get(input.index) {
        Some(value) => row_label(input.field, input.details, type_name, input.index, value),
        None => input.field.to_owned(),
    };
    inspector_row(ui, &row_label, input.tooltip, &mut |ui| {
        let width = (ui.available_width() - ASSET_ROW_CLEAR_WIDTH).max(40.0);
        let color = match label {
            AssetReferenceLabel::Named(_) => style::text(),
            _ => style::text_muted(),
        };
        let icon = assets_tiles::asset_type_icon(input.asset_type.unwrap_or_default());
        let response = ui.add_sized(
            egui::vec2(width, 18.0),
            egui::Button::new(icons::rich(icon, &text).color(color)).truncate(),
        );
        // A file dragged from the Assets tab lands here; hovering one marks
        // the row so the drop is visibly accepted.
        if response.dnd_hover_payload::<String>().is_some() {
            ui.painter().rect_stroke(
                response.rect,
                2.0,
                ui.visuals().selection.stroke,
                egui::StrokeKind::Inside,
            );
        }
        if let Some(path) = response.dnd_release_payload::<String>() {
            chosen = Some(Some((*path).clone()));
        }
        if response.clicked() {
            session.asset_picker = match session.asset_picker.as_deref() {
                Some(open) if open == key => None,
                _ => Some(key.clone()),
            };
            session.asset_search.clear();
        }
        button_rect = Some((response.rect, response.interact_rect));
        if ui
            .add_enabled(
                !matches!(label, AssetReferenceLabel::Unset),
                egui::Button::new("x").small(),
            )
            .on_hover_text("Clear the reference")
            .clicked()
        {
            chosen = Some(None);
        }
    });
    if let Some((rect, interact)) = button_rect {
        remember_asset_row_rect(session, &key, rect, interact.contains(rect.center()));
    }

    // The picker opens under its row: a search box, then the field's
    // candidates, with `None` first. It floats over the rows below and claims
    // its rectangle, so a click in it is the picker's.
    if session.asset_picker.as_deref() == Some(key.as_str()) {
        if let Some((rect, _)) = button_rect {
            let picker_id = egui::Id::new(("pill_editor_shell_egui.asset_picker", key.as_str()));
            let area =
                floating_popup(
                    ui,
                    picker_id,
                    rect,
                    rect.width().max(180.0),
                    overlay_regions,
                    &mut |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut session.asset_search)
                                .hint_text("Search assets")
                                .desired_width(f32::INFINITY),
                        );
                        let search = session.asset_search.to_lowercase();
                        let (files, built) = inspector::picker_candidates(
                            input.assets,
                            input.engine_assets,
                            input.asset_type,
                        );
                        egui::ScrollArea::vertical()
                            .max_height(ASSET_PICKER_HEIGHT)
                            .show(ui, |ui| {
                                if ui.selectable_label(false, "None").clicked() {
                                    chosen = Some(None);
                                }
                                for candidate in files.into_iter().filter(|candidate| {
                                    candidate.path.to_lowercase().contains(&search)
                                }) {
                                    let current = matches!(&label, AssetReferenceLabel::Named(name)
                                    if *name == candidate.path);
                                    ui.horizontal(|ui| {
                                        // A row under a module root is the engine's
                                        // own file, not the project's: the badge is
                                        // what tells the two apart, since the path
                                        // of a module's resource reads like any other.
                                        let from_module = !candidate.root.is_empty();
                                        if from_module {
                                            engine_badge(ui);
                                        }
                                        let mut row = ui.selectable_label(current, &candidate.path);
                                        if from_module {
                                            row = row.on_hover_text(format!(
                                                "Shipped with the {} module",
                                                candidate.root
                                            ));
                                        }
                                        if row.clicked() {
                                            chosen = Some(Some(candidate.path.clone()));
                                        }
                                        // The focus affordance the asset-settings
                                        // picker carries: showing a candidate in the
                                        // Assets panel keeps the picker open, so
                                        // looking one up does not throw the edit
                                        // away.
                                        if ui
                                            .small_button("o")
                                            .on_hover_text("Show this asset in the Assets panel")
                                            .clicked()
                                        {
                                            pending.focus_asset(candidate.key());
                                        }
                                    });
                                }
                                // The store's own assets: the ones built in code,
                                // which no directory walk finds. A row is picked by
                                // its guid when it has one and by the name it was
                                // added under otherwise, which the field write
                                // resolves through the store.
                                for candidate in built.into_iter().filter(|candidate| {
                                    candidate.name.to_lowercase().contains(&search)
                                }) {
                                    let current = matches!(&label, AssetReferenceLabel::Named(name)
                                    if *name == candidate.name);
                                    ui.horizontal(|ui| {
                                        engine_badge(ui);
                                        if ui.selectable_label(current, &candidate.name).clicked() {
                                            chosen = Some(Some(
                                                candidate
                                                    .guid
                                                    .clone()
                                                    .unwrap_or_else(|| candidate.name.clone()),
                                            ));
                                        }
                                    });
                                }
                            });
                    },
                );
            // Any press outside the picker and its row closes it, whatever
            // button it was: a picker that only closes by picking fights the
            // person who changed their mind. The press that opens the picker
            // is on the row, so it never closes what it just opened.
            let press =
                ui.input(|input| (input.pointer.any_pressed(), input.pointer.interact_pos()));
            if let (true, Some(at)) = press {
                if !rect.contains(at) && !area.response.rect.contains(at) {
                    session.asset_picker = None;
                }
            }
        }
    }

    if let Some(reference) = chosen {
        session.asset_picker = None;
        let (commands, label, coalesce) = inspector::selection_asset_edit(
            input.targets,
            input.entity,
            type_name,
            input.field,
            reference,
        );
        pending.push_recorded(commands, label, Some(coalesce));
    }
}

/// Width the clear button takes beside an asset row's button.
pub const ASSET_ROW_CLEAR_WIDTH: f32 = 24.0;

/// Height an asset or entity picker's list scrolls within.
pub const ASSET_PICKER_HEIGHT: f32 = 180.0;

/// What one entity reference row draws from: the field, its component and the
/// selection it writes to, and the scene's entities the picker offers.
pub struct EntityRowInput<'a> {
    /// The primary entity the row describes.
    pub entity: Entity,
    /// The component section the row belongs to.
    pub component: &'a ComponentView,
    /// The field's position in the component's field list.
    pub index: usize,
    /// The field's registered name.
    pub field: &'a str,
    /// The field's `[Tooltip]`, if it declared one.
    pub tooltip: Option<&'a str>,
    /// Every entity the edit reaches (the primary first).
    pub targets: &'a [Entity],
    /// The selected entities' details, for the mixed-value marker.
    pub details: &'a [EntityDetail],
    /// The scene's entities, as the Entities tab lists them.
    pub scene: &'a [EntityListEntry],
}

/// One entity reference row: a button naming the referenced
/// entity, which opens a picker over the scene, a target button that arms a
/// scene click, a clear button, and a drop target that takes an entity dragged
/// from the Entities tab.
///
/// Every choice queues one recorded `SetFieldEntity` step for the whole
/// selection. A reference whose entity is gone reads `(missing)` rather than
/// disappearing: the field still holds the handle, and hiding that would leave
/// the row looking unset while the row's bytes say otherwise.
pub fn inspector_entity_row(
    ui: &mut egui::Ui,
    input: EntityRowInput<'_>,
    pending: &mut PendingEditorCommands,
    session: &mut InspectorSession,
    overlay_regions: &mut Vec<ShellRect>,
) {
    let type_name = input.component.type_name.as_str();
    let label = input
        .component
        .entity_labels
        .get(input.index)
        .cloned()
        .flatten()
        .unwrap_or(EntityReferenceLabel::Unset);
    let text = label.text().to_owned();
    let key = format!("{}.{type_name}.{}", input.entity, input.field);
    if remember_entity_row_text(session, &key, &text) {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            entity = %input.entity,
            component = type_name,
            field = input.field,
            reference = text.as_str(),
            "[editor] inspector row kind=entity"
        );
    }

    // `Some(None)` clears the reference, `Some(Some(entity))` points it at one;
    // collected while drawing and queued once afterwards.
    let mut chosen: Option<Option<Entity>> = None;
    let mut armed = false;
    let mut button_rect = None;
    let row_label = match input.component.values.get(input.index) {
        Some(value) => row_label(input.field, input.details, type_name, input.index, value),
        None => input.field.to_owned(),
    };
    inspector_row(ui, &row_label, input.tooltip, &mut |ui| {
        let width = (ui.available_width() - 2.0 * ASSET_ROW_CLEAR_WIDTH).max(40.0);
        let color = match label {
            EntityReferenceLabel::Live(_) => style::text(),
            _ => style::text_muted(),
        };
        let response = ui.add_sized(
            egui::vec2(width, 18.0),
            egui::Button::new(icons::rich(icons::Icon::TreeStructure, &text).color(color))
                .truncate(),
        );
        if response.dnd_hover_payload::<DraggedEntity>().is_some() {
            ui.painter().rect_stroke(
                response.rect,
                2.0,
                ui.visuals().selection.stroke,
                egui::StrokeKind::Inside,
            );
        }
        if let Some(dragged) = response.dnd_release_payload::<DraggedEntity>() {
            chosen = Some(Some(dragged.0));
        }
        if response.clicked() {
            session.entity_picker = match session.entity_picker.as_deref() {
                Some(open) if open == key => None,
                _ => Some(key.clone()),
            };
            session.entity_search.clear();
        }
        button_rect = Some((response.rect, response.interact_rect));
        if ui
            .add(egui::Button::new("+").small())
            .on_hover_text("Pick the reference by clicking it in the scene")
            .clicked()
        {
            armed = true;
        }
        if ui
            .add_enabled(
                !matches!(label, EntityReferenceLabel::Unset),
                egui::Button::new("x").small(),
            )
            .on_hover_text("Clear the reference")
            .clicked()
        {
            chosen = Some(None);
        }
    });
    if let Some((rect, interact)) = button_rect {
        remember_entity_row_rect(session, &key, rect, interact.contains(rect.center()));
    }

    if session.entity_picker.as_deref() == Some(key.as_str()) {
        if let Some((rect, _)) = button_rect {
            let picker_id = egui::Id::new(("pill_editor_shell_egui.entity_picker", key.as_str()));
            floating_popup(
                ui,
                picker_id,
                rect,
                rect.width().max(180.0),
                overlay_regions,
                &mut |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut session.entity_search)
                            .hint_text("Search entities")
                            .desired_width(f32::INFINITY),
                    );
                    let search = session.entity_search.to_lowercase();
                    egui::ScrollArea::vertical()
                        .max_height(ASSET_PICKER_HEIGHT)
                        .show(ui, |ui| {
                            if ui.selectable_label(false, "None").clicked() {
                                chosen = Some(None);
                            }
                            for candidate in inspector::entity_candidates(input.scene) {
                                if !candidate.display.to_lowercase().contains(&search) {
                                    continue;
                                }
                                let current = matches!(&label, EntityReferenceLabel::Live(name)
                                    if *name == candidate.display);
                                if ui.selectable_label(current, &candidate.display).clicked() {
                                    chosen = Some(Some(candidate.entity));
                                }
                            }
                        });
                },
            );
        }
    }

    if armed {
        // The scene's own click names the reference, so the request rides the
        // command queue to the executable that reads scene picks: the shell
        // draws rows, it does not own the scene.
        pending.push(vec![EditorCommand::ArmEntityPick {
            entity: input.entity,
            component: type_name.to_owned(),
            field: input.field.to_owned(),
        }]);
    }

    if let Some(reference) = chosen {
        session.entity_picker = None;
        let (commands, label, coalesce) = inspector::selection_entity_edit(
            input.targets,
            input.entity,
            type_name,
            input.field,
            reference,
        );
        pending.push_recorded(commands, label, Some(coalesce));
    }
}

/// Record the entity an entity row draws, returning whether it changed since
/// the last frame that drew the row.
pub fn remember_entity_row_text(session: &mut InspectorSession, key: &str, text: &str) -> bool {
    if let Some((_, logged)) = session
        .entity_row_texts
        .iter_mut()
        .find(|(logged_key, _)| logged_key == key)
    {
        if logged == text {
            return false;
        }
        logged.clear();
        logged.push_str(text);
        return true;
    }
    session
        .entity_row_texts
        .push((key.to_owned(), text.to_owned()));
    true
}

/// Log an entity row's rectangle once per placement, keyed by
/// `entity.component.field`, so a suite can drop an entity onto it.
///
/// The placement is recorded either way - the shell's own tests read it - but
/// the line is a scripted run's: it is there to aim a drop, and a person's
/// console gains nothing from it.
pub fn remember_entity_row_rect(
    session: &mut InspectorSession,
    key: &str,
    rect: egui::Rect,
    visible: bool,
) {
    if !visible {
        return;
    }
    let Some(rect) = record_row_placement(&mut session.entity_row_rects, key, rect) else {
        return;
    };
    if !session.log_rects {
        return;
    }
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        row = key,
        min_x = rect.min.x,
        min_y = rect.min.y,
        max_x = rect.max.x,
        max_y = rect.max.y,
        "[editor] inspector entity row rect"
    );
}

/// Record `rect` as `key`'s placement, returning it when that is a placement
/// the log has not named yet.
///
/// Rectangles compare in whole points: a panel that re-lays itself out with a
/// fraction of a point of jitter sits in the same place, and a line per frame
/// of that is what turned the row tokens into noise. A row that leaves the
/// visible area keeps its last placement, so it does not log again when it
/// scrolls back to where it was.
pub fn record_row_placement(
    logged: &mut Vec<(String, egui::Rect)>,
    key: &str,
    rect: egui::Rect,
) -> Option<egui::Rect> {
    let quantized = egui::Rect::from_min_max(
        egui::pos2(rect.min.x.round(), rect.min.y.round()),
        egui::pos2(rect.max.x.round(), rect.max.y.round()),
    );
    if let Some((_, previous)) = logged.iter_mut().find(|(logged_key, _)| logged_key == key) {
        if *previous == quantized {
            return None;
        }
        *previous = quantized;
        return Some(quantized);
    }
    logged.push((key.to_owned(), quantized));
    Some(quantized)
}

/// Record the asset an asset row draws, returning whether it changed since
/// the last frame that drew the row: the row logs its current asset on its
/// first draw and on every change, so a suite can wait for a pick to show up
/// and for an undo to bring the previous asset back.
pub fn remember_asset_row_text(session: &mut InspectorSession, key: &str, text: &str) -> bool {
    if let Some((_, logged)) = session
        .asset_row_texts
        .iter_mut()
        .find(|(logged_key, _)| logged_key == key)
    {
        if logged == text {
            return false;
        }
        logged.clear();
        logged.push_str(text);
        return true;
    }
    session
        .asset_row_texts
        .push((key.to_owned(), text.to_owned()));
    true
}

/// Log an asset row's rectangle once per placement, keyed by
/// `entity.component.field`, so a suite can drop an asset onto it.
///
/// Only a row whose centre is inside its visible area is drawn at all: a row
/// scrolled out of the panel cannot be clicked or dropped on, so a visible row
/// is the one whose rectangle reaches the log. The line itself is a scripted
/// run's.
pub fn remember_asset_row_rect(
    session: &mut InspectorSession,
    key: &str,
    rect: egui::Rect,
    visible: bool,
) {
    if !visible {
        return;
    }
    let Some(rect) = record_row_placement(&mut session.asset_row_rects, key, rect) else {
        return;
    };
    if !session.log_rects {
        return;
    }
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        row = key,
        min_x = rect.min.x,
        min_y = rect.min.y,
        max_x = rect.max.x,
        max_y = rect.max.y,
        "[editor] inspector asset row rect"
    );
}

/// The current values of a grouped row's leaves, in the row's own order: a
/// leaf inside an array value reads its element.
pub fn leaf_values(component: &ComponentView, leaves: &[inspector::RowLeaf]) -> Vec<FieldValue> {
    leaves
        .iter()
        .filter_map(|leaf| match leaf.element {
            Some(element) => component
                .values
                .get(leaf.index)
                .and_then(|value| inspector::element_field(value, element)),
            None => component.values.get(leaf.index).cloned(),
        })
        .collect()
}

/// The channels a colour row's values hold, as the picker's unmultiplied
/// linear floats, or `None` when a value is not a float.
///
/// Three channels stay opaque; four carry their alpha. A colour of any other
/// length is not one the picker draws.
pub fn color_channels(values: &[FieldValue]) -> Option<[f32; 4]> {
    if !(3..=4).contains(&values.len()) {
        return None;
    }
    let mut channels = [1.0f32; 4];
    for (channel, value) in channels.iter_mut().zip(values) {
        *channel = match value {
            FieldValue::F32(number) => *number,
            FieldValue::F64(number) => *number as f32,
            _ => return None,
        };
    }
    Some(channels)
}

/// The label a row draws, marked when the selection disagrees on its value.
pub fn row_label(
    field: &str,
    details: &[EntityDetail],
    component: &str,
    index: usize,
    value: &FieldValue,
) -> String {
    if row_is_mixed(details, component, index, value) {
        format!("{field} (mixed)")
    } else {
        field.to_string()
    }
}

/// Queue one field edit for **every** selected entity the component exists on,
/// as one recorded step.
///
/// `targets` is [`super::tab_inspector::inspector_targets`]'s answer: the
/// primary first, then the other selected entities carrying the component. One
/// batch means one undo step however many entities the row wrote, and the
/// coalesce key is the primary's, so a drag across a multi-selection still
/// collapses into the same single step.
pub fn queue_field_edit(
    pending: &mut PendingEditorCommands,
    targets: &[Entity],
    entity: Entity,
    type_name: &str,
    field: &str,
    value: FieldValue,
) {
    let (commands, label, key) =
        inspector::selection_field_edit(targets, entity, type_name, field, value);
    pending.push_recorded(commands, label, Some(key));
}

/// Queue one array-element edit for every target, as one recorded step.
pub fn queue_element_edit(
    pending: &mut PendingEditorCommands,
    targets: &[Entity],
    entity: Entity,
    type_name: &str,
    field: &str,
    index: usize,
    value: FieldValue,
) {
    let mut commands = Vec::with_capacity(targets.len());
    let mut label = String::new();
    for target in targets {
        let (command, step_label) =
            inspector::element_edit(*target, type_name, field, index, value.clone());
        if label.is_empty() {
            label = step_label;
        }
        commands.push(command);
    }
    pending.push_recorded(
        commands,
        label,
        Some(inspector::field_coalesce_key(
            entity,
            type_name,
            &format!("{field}[{index}]"),
        )),
    );
}

/// Whether the selected entities disagree on this row's value.
pub fn row_is_mixed(
    details: &[EntityDetail],
    component: &str,
    index: usize,
    value: &FieldValue,
) -> bool {
    details.iter().any(|detail| {
        detail
            .components
            .iter()
            .find(|view| view.type_name == component)
            .and_then(|view| view.values.get(index))
            .is_some_and(|other| other != value)
    })
}

/// Note one Inspector row's signature, answering whether it is new, so a
/// panel that redraws every frame does not flood the log with the same row.
///
/// The signature is built here from the component, the row's kind and its
/// distinguishing parts, so no call site joins a `|`-string by hand.
pub fn remember_inspector_row(
    session: &mut InspectorSession,
    type_name: &str,
    kind: &str,
    parts: &[&str],
) -> bool {
    let mut signature = format!("{type_name}|{kind}");
    for part in parts {
        signature.push('|');
        signature.push_str(part);
    }
    if session.rows_logged.contains(&signature) {
        return false;
    }
    session.rows_logged.push(signature);
    true
}

/// The wire name a row's widget logs under, so suites can assert what the
/// model chose without reading pixels.
pub fn field_widget_name(widget: &inspector::FieldWidget) -> &'static str {
    match widget {
        inspector::FieldWidget::Checkbox => "checkbox",
        inspector::FieldWidget::Integer => "integer",
        inspector::FieldWidget::Float => "float",
        inspector::FieldWidget::Text => "text",
        inspector::FieldWidget::ReadOnly => "read-only",
        inspector::FieldWidget::FloatSlider { .. } => "float-slider",
        inspector::FieldWidget::IntegerSlider { .. } => "integer-slider",
        inspector::FieldWidget::FloatDrag { .. } => "float-drag",
        inspector::FieldWidget::IntegerDrag { .. } => "integer-drag",
        inspector::FieldWidget::MultilineText { .. } => "multiline",
    }
}

/// The Inspector's field row: the shared label/control row ([`row`]), with
/// the field's label on the left and its widget in the control column. A
/// decorator's tooltip shows while the pointer rests on the label.
pub fn inspector_row(
    ui: &mut egui::Ui,
    label: &str,
    tooltip: Option<&str>,
    widget: &mut dyn FnMut(&mut egui::Ui),
) {
    row(ui, label, tooltip, widget);
}

/// Whether a value has its own widget kind.
pub fn is_scalar_field(value: &FieldValue) -> bool {
    matches!(
        value,
        FieldValue::F32(_)
            | FieldValue::F64(_)
            | FieldValue::I8(_)
            | FieldValue::I16(_)
            | FieldValue::I32(_)
            | FieldValue::I64(_)
            | FieldValue::U8(_)
            | FieldValue::U16(_)
            | FieldValue::U32(_)
            | FieldValue::U64(_)
            | FieldValue::Usize(_)
            | FieldValue::Bool(_)
    )
}
