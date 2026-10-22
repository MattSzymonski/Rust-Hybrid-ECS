//! The Inspector tab: the selection's components and their rows.
//!
//! # Responsibilities
//!
//! - `inspector_tab` and its header: the selected entity's name and the Add Component popup.
//! - `entity_inspector` and its section headers: one collapsible section per component, expanded through the row builders.
//! - `addable_components`: the components the popup offers.
//!
//! Bodies unchanged from the shell's root file (W-0037 Stage 3); the globs
//! below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// The Inspector's label column width, in points: Unity's label/control
/// split leaves the control side most of a narrow panel.
pub const INSPECTOR_LABEL_WIDTH: f32 = 80.0;

/// One vector element's group width, in points: the axis letter and its
/// field together, so a wrap moves the pair and never separates them.
pub const INSPECTOR_VECTOR_FIELD_WIDTH: f32 = 58.0;

/// The Inspector tab: the selected entity's components as collapsible
/// sections (a caret, the component's duotone icon and its label), the Add
/// Component button in the tab's top-right, and the selected asset's import
/// settings as editable leaves below. Every commit is a recorded `SetField` /
/// `SetArrayElement` / `SetAssetSettings` edit, coalesced per widget so a
/// drag or a typing run is one undo step.
#[pill_hot_fn]
pub fn inspector_tab(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    selection: &EditorSelection,
    pending: &mut PendingEditorCommands,
    overlay_regions: &mut Vec<ShellRect>,
    session: &mut InspectorSession,
) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        // A selected resource takes the tab: one selection shows one thing
        // (W-0038 Stage 14).
        if let Some(view) = selection
            .resource
            .as_deref()
            .and_then(|name| snapshot.resources.iter().find(|view| view.name == name))
        {
            resource_section(ui, view, pending);
            return;
        }
        match snapshot.detail.as_ref() {
            Some(detail) => {
                inspector_header(ui, snapshot, detail, pending, overlay_regions, session);
                entity_inspector(ui, snapshot, detail, pending, overlay_regions, session);
            }
            None => match selection.entity {
                Some(entity) => {
                    ui.label(format!("entity {entity} is not alive"));
                }
                None => {
                    ui.label("Select an entity in the Entities tab.");
                }
            },
        }
        // The selected asset's section follows the entity's: its kind and
        // its import settings, editable leaf by leaf.
        let selected_asset = selection.asset.as_deref();
        if let Some(detail) = snapshot
            .asset_detail
            .as_ref()
            .filter(|detail| selected_asset == Some(detail.path.as_str()))
        {
            ui.add_space(8.0);
            ui.label(egui::RichText::new(format!("asset {}", detail.path)).strong());
            read_only_row(ui, "kind", &detail.kind);
            if detail.standalone {
                read_only_row(ui, "standalone", "yes");
            }
            ui.separator();
            match detail.settings.as_ref() {
                Some(settings) => asset_settings_rows(ui, &detail.path, settings, pending),
                None => {
                    ui.label(egui::RichText::new("no editable settings").weak().italics());
                }
            }
        }
        // The strip below the rows carries the component menu where no
        // heading is there to hang it on (a right-click there adds).
        if let Some(detail) = snapshot.detail.as_ref() {
            let empty = ui.allocate_response(empty_area(ui), egui::Sense::click());
            context_menu("inspector", &empty, overlay_regions, &mut |ui| {
                component_menu_items(ui, snapshot, detail, None, pending)
            });
        }
    });
}

/// The Inspector's top row: the selected entity's name and, right aligned,
/// the Add Component button with the components it can attach.
pub fn inspector_header(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    detail: &EntityDetail,
    pending: &mut PendingEditorCommands,
    overlay_regions: &mut Vec<ShellRect>,
    session: &mut InspectorSession,
) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("entity {}", detail.entity)).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let button = ui
                .add(
                    icons::IconButton::new(icons::Icon::Plus, "")
                        .fixed_size(egui::vec2(24.0, 18.0))
                        .active(session.add_open),
                )
                .on_hover_text("Add Component");
            if session.add_rect != Some(button.rect) {
                session.add_rect = Some(button.rect);
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    x = button.rect.min.x,
                    y = button.rect.min.y,
                    width = button.rect.width(),
                    height = button.rect.height(),
                    "[editor] inspector add rect"
                );
            }
            if button.clicked() {
                session.add_open = !session.add_open;
                if session.add_open {
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ENGINE,
                        "[editor] inspector add opened"
                    );
                }
            }
        });
    });
    if session.add_open {
        inspector_add_popup(ui, snapshot, detail, pending, overlay_regions, session);
    }
}

/// The Add Component popup: every registered addable component the entity
/// does not carry yet, under the `+` button. A pick queues the recorded
/// `add {short}` step the header menu and the scripted channel queue.
pub fn inspector_add_popup(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    detail: &EntityDetail,
    pending: &mut PendingEditorCommands,
    overlay_regions: &mut Vec<ShellRect>,
    session: &mut InspectorSession,
) {
    let Some(anchor) = session.add_rect else {
        return;
    };
    let addable = addable_components(snapshot, detail);
    let mut picked = None;
    let area = egui::Area::new(egui::Id::new("pill_editor_shell_egui.inspector_add"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(anchor.max.x - 220.0, anchor.max.y + 2.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(220.0);
                egui::ScrollArea::vertical()
                    .max_height(260.0)
                    .show(ui, |ui| {
                        if addable.is_empty() {
                            ui.label(egui::RichText::new("every component is attached").weak());
                        }
                        for type_name in &addable {
                            let response = ui.selectable_label(
                                false,
                                icons::rich(
                                    inspector::component_icon(type_name),
                                    inspector::component_label(type_name),
                                ),
                            );
                            if remember_moved_rect(
                                &mut session.add_entries,
                                type_name,
                                response.rect,
                            ) {
                                pill_core::info!(
                                    target: pill_core::telemetry::telemetry_target::ENGINE,
                                    component = type_name,
                                    x = response.rect.min.x,
                                    y = response.rect.min.y,
                                    width = response.rect.width(),
                                    height = response.rect.height(),
                                    "[editor] inspector add entry"
                                );
                            }
                            if response.clicked() {
                                picked = Some((*type_name).to_string());
                                ui.close();
                            }
                        }
                    });
            });
        });
    // The popup floats over the panels; its rectangle is the UI's.
    overlay_regions.push(shell_rect(area.response.rect));
    if let Some(type_name) = picked {
        let (command, label) = inspector::component_add(detail.entity, &type_name);
        pending.push_recorded(vec![command], label, None);
        session.add_open = false;
    } else if addable.is_empty() {
        // Nothing to pick: a second click on the button closes it.
    }
    // A press anywhere outside the button and the popup closes the popup.
    let outside = ui.ctx().input(|input| {
        input.pointer.any_pressed()
            && input.pointer.interact_pos().is_some_and(|position| {
                !anchor.contains(position) && !area.response.rect.contains(position)
            })
    });
    if outside {
        session.add_open = false;
    }
}

/// The components a section may attach: registered, addable and not already
/// on the entity.
pub fn addable_components<'a>(snapshot: &'a EditorSnapshot, detail: &EntityDetail) -> Vec<&'a str> {
    snapshot
        .components
        .iter()
        .filter(|registered| registered.addable)
        .filter(|registered| {
            !detail
                .components
                .iter()
                .any(|carried| carried.type_name == registered.type_name)
        })
        .map(|registered| registered.type_name.as_str())
        .collect()
}

/// One entity's components as collapsible sections: a header bar (caret,
/// duotone icon, label) that toggles the session's collapse state and carries
/// the component menu, then the rows the field model asks for.
pub fn entity_inspector(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    detail: &EntityDetail,
    pending: &mut PendingEditorCommands,
    overlay_regions: &mut Vec<ShellRect>,
    session: &mut InspectorSession,
) {
    for component in &detail.components {
        let collapsed = session.collapsed.contains(&component.type_name);
        let header = inspector_section_header(ui, component, collapsed, session);
        context_menu("inspector", &header, overlay_regions, &mut |ui| {
            component_menu_items(
                ui,
                snapshot,
                detail,
                Some(component.type_name.as_str()),
                pending,
            )
        });
        if header.clicked() {
            if collapsed {
                session.collapsed.remove(&component.type_name);
            } else {
                session.collapsed.insert(component.type_name.clone());
            }
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                component = component.type_name.as_str(),
                collapsed = !collapsed,
                "[editor] inspector section"
            );
        }
        if collapsed {
            continue;
        }
        if !component.editable {
            ui.label(egui::RichText::new("no editable fields").weak().italics());
            ui.add_space(4.0);
            continue;
        }
        for row in inspector::rows(component) {
            inspector_field_row(ui, detail.entity, component, &row, pending, session);
        }
        ui.add_space(6.0);
    }
}

/// One component section's header bar: the foldout caret, the component's
/// duotone icon and its label, on the panel's control fill. The bar is
/// clickable as a whole (a click toggles the section) and logs its rectangle
/// once per placement.
pub fn inspector_section_header(
    ui: &mut egui::Ui,
    component: &ComponentView,
    collapsed: bool,
    session: &mut InspectorSession,
) -> egui::Response {
    let caret = if collapsed {
        icons::Icon::CaretRight
    } else {
        icons::Icon::CaretDown
    };
    let frame = egui::Frame::new()
        .fill(style::CONTROL_FILL)
        .corner_radius(3.0)
        .inner_margin(egui::Margin::symmetric(6, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (caret_rect, _) =
                    ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                icons::paint_duotone(ui.painter(), caret_rect, caret, style::TEXT_MUTED);
                let (icon_rect, _) =
                    ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                icons::paint_duotone(
                    ui.painter(),
                    icon_rect,
                    inspector::component_icon(&component.type_name),
                    ui.visuals().text_color(),
                );
                ui.label(
                    egui::RichText::new(inspector::component_label(&component.type_name)).strong(),
                );
            });
        });
    let rect = frame.response.rect;
    if remember_moved_rect(&mut session.section_rects, &component.type_name, rect) {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            component = component.type_name.as_str(),
            x = rect.min.x,
            y = rect.min.y,
            width = rect.width(),
            height = rect.height(),
            "[editor] inspector section rect"
        );
    }
    ui.interact(
        rect,
        ui.id()
            .with(("inspector-section", component.type_name.as_str())),
        egui::Sense::click(),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}
