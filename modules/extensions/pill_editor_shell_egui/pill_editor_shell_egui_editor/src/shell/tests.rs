//! The shell's unit tests.
//!
//! # Responsibilities
//!
//! - The menus', the toolbox's and the inspector's model-level tests.
//!
//! # Design
//!
//! Split out of the shell's root file (W-0037 Stage 3) with its bodies
//! unchanged. The block was inline (`mod tests { ... }`) and is now the file
//! itself; `use super::*;` still reaches the shell's items.

// Current crate

use super::*;
use pill_engine::Engine;

/// Run one egui pass for its side effects.
///
/// epaint 0.36 panics when a `TexturesDelta` with unapplied deltas is
/// dropped, and a test has no GPU to apply them on: the delta is cleared
/// instead, the contract epaint asks a caller that ignores it to keep.
///
/// The `cfg` gate is the one `#[test]` puts on a test function: it keeps
/// this helper out of the hot-patch function inventory, which cannot name a
/// function that is absent from the built artifact.
#[cfg(test)]
fn run_pass(context: &egui::Context, pass: &mut dyn FnMut(&mut egui::Ui)) {
    let mut output = context.run_ui(egui::RawInput::default(), pass);
    output.textures_delta.clear();
}

#[test]
fn entity_rows_list_the_worlds_entities() {
    let mut engine = Engine::new();
    engine
        .world_mut()
        .create_entity()
        .build()
        .expect("entity builds");
    let second = engine
        .world_mut()
        .create_entity()
        .build()
        .expect("entity builds");
    let snapshot = EditorSnapshot::capture_list(&engine, 0, &[], Vec::new());
    let rows = entity_rows(&snapshot);
    assert_eq!(rows.len(), 2);
    let expected = format!("{}v{}", second.id(), second.generation());
    assert!(
        rows.iter()
            .any(|(entity, label)| *entity == second && label.contains(&expected)),
        "the second entity's row carries its display id: {rows:?}"
    );
}

#[test]
fn the_systems_rows_name_the_worlds_systems() {
    let mut engine = Engine::new();
    engine.register_system("tick", || {});
    engine.register_system("render", || {});
    let snapshot = EditorSnapshot::capture_list(&engine, 0, &[], Vec::new());
    let rows: Vec<String> = snapshot
        .systems
        .iter()
        .map(systems::system_row_label)
        .collect();
    assert!(rows.iter().any(|row| row.contains("tick")), "{rows:?}");
    assert!(rows.iter().any(|row| row.contains("render")), "{rows:?}");
}

#[test]
fn the_tab_bodies_draw_without_panicking() {
    let mut engine = Engine::new();
    engine
        .world_mut()
        .create_entity()
        .build()
        .expect("entity builds");
    engine.register_system("tick", || {});
    let snapshot = EditorSnapshot::capture_list(&engine, 0, &[], Vec::new());
    let mut selection = EditorSelection::default();
    let mut dialog: Option<AssetDialog> = None;
    let mut pending = PendingEditorCommands::default();
    let mut draft: Option<preferences::PreferencesDraft> = None;
    let mut browser = ProjectBrowser::default();
    let mut drag_out: Option<String> = None;
    let context = egui::Context::default();
    style::apply(&context);
    let mut regions = Vec::new();
    let mut inspector_session = InspectorSession::default();
    let previews = std::collections::BTreeMap::new();
    let mut preview_visible = Vec::new();
    run_pass(&context, &mut |ui| {
        egui::CentralPanel::default().show(ui, |ui| {
            entities_tab(
                ui,
                &snapshot,
                &mut selection,
                &mut pending,
                &mut regions,
                &mut inspector_session,
            );
            systems_tab(ui, &snapshot, &mut pending);
            inspector_tab(
                ui,
                &snapshot,
                &selection,
                &mut pending,
                &mut regions,
                &mut inspector_session,
            );
            {
                let mut writes = AssetTileWrites {
                    selection: &mut selection,
                    dialog: &mut dialog,
                    pending: &mut pending,
                    overlay_regions: &mut regions,
                    drag_out: &mut drag_out,
                    previews: &previews,
                    preview_visible: &mut preview_visible,
                };
                assets_tab(ui, &snapshot, &mut browser, &mut writes);
            }
            preferences_tab(ui, &snapshot, &mut draft, &mut pending);
            scene_overlay(ui, 120.0, &mut regions);
        });
    });
    assert_eq!(regions.len(), 2, "the two chips are the UI's regions");
}

/// The Add list the popup and the component menu share omits every component
/// the entity already carries, whether it is addable or not (W-0049 Stage 9).
#[test]
fn the_addable_list_omits_the_components_the_entity_carries() {
    let mut engine = Engine::new();
    engine
        .world_mut()
        .create_entity()
        .build()
        .expect("entity builds");
    let mut snapshot = EditorSnapshot::capture_list(&engine, 0, &[], Vec::new());
    snapshot.components = vec![
        pill_editor_core::editor_state::RegisteredComponent {
            type_name: "demo::Carried".to_string(),
            addable: true,
        },
        pill_editor_core::editor_state::RegisteredComponent {
            type_name: "demo::Missing".to_string(),
            addable: true,
        },
        pill_editor_core::editor_state::RegisteredComponent {
            type_name: "demo::NotAddable".to_string(),
            addable: false,
        },
    ];
    let detail = EntityDetail {
        entity: Entity::from_raw(1, 0),
        components: vec![ComponentView {
            type_name: "demo::Carried".to_string(),
            persistable: false,
            editable: true,
            fields: Vec::new(),
            values: Vec::new(),
            asset_labels: Vec::new(),
            entity_labels: Vec::new(),
        }],
    };
    assert_eq!(
        addable_components(&snapshot, &detail),
        vec!["demo::Missing"],
        "a carried component is not offered again, and neither is a non-addable one"
    );
}

#[test]
fn the_browser_draws_its_tree_path_and_tiles() {
    let engine = Engine::new();
    let mut snapshot = EditorSnapshot::capture_list(&engine, 0, &[], Vec::new());
    snapshot.assets = vec![
        AssetListing {
            path: "models".to_string(),
            kind: "folder".to_string(),
            standalone: false,
            has_meta: false,
            guid: None,
        },
        AssetListing {
            path: "models/nested".to_string(),
            kind: "folder".to_string(),
            standalone: false,
            has_meta: false,
            guid: None,
        },
        AssetListing {
            path: "models/cube.obj".to_string(),
            kind: "pill_master_renderer::assets::Mesh".to_string(),
            standalone: false,
            has_meta: true,
            guid: None,
        },
        AssetListing {
            path: "shaders/code.wgsl".to_string(),
            kind: "file".to_string(),
            standalone: false,
            has_meta: false,
            guid: None,
        },
    ];
    let mut browser = ProjectBrowser::default();
    assert!(browser.open("models"));
    assert!(browser.is_expanded("models") || browser.folder == "models");
    let mut selection = EditorSelection::default();
    let mut dialog: Option<AssetDialog> = None;
    let mut pending = PendingEditorCommands::default();
    let mut drag_out: Option<String> = None;
    let context = egui::Context::default();
    style::apply(&context);
    let mut regions = Vec::new();
    let mut caption_rows = 0usize;
    let mut caption_width = 0.0f32;
    run_pass(&context, &mut |ui| {
        egui::CentralPanel::default().show(ui, |ui| {
            assets_tree(ui, &snapshot, &mut browser, &mut selection, &mut pending);
            assets_path_bar(ui, &mut browser);
            assets_zoom_bar(ui, &snapshot.assets, &mut browser);
            {
                let previews = std::collections::BTreeMap::new();
                let mut preview_visible = Vec::new();
                let mut writes = AssetTileWrites {
                    selection: &mut selection,
                    dialog: &mut dialog,
                    pending: &mut pending,
                    overlay_regions: &mut regions,
                    drag_out: &mut drag_out,
                    previews: &previews,
                    preview_visible: &mut preview_visible,
                };
                assets_grid(ui, &snapshot, &mut browser, &mut writes);
            }
            let font = egui::TextStyle::Small.resolve(ui.style());
            let job = tile_name_job(
                "a_name_that_is_much_too_long_for_a_tile.png",
                &font,
                egui::Color32::WHITE,
                40.0,
            );
            let galley = ui.painter().layout_job(job);
            caption_rows = galley.rows.len();
            caption_width = galley.size().x;
        });
    });
    assert_eq!(caption_rows, 2, "a long name wraps to the two caption rows");
    assert!(
        caption_width <= 41.0,
        "the caption stays inside the tile: {caption_width}"
    );
    assert!(pending.batches.is_empty(), "drawing queues no commands");
    assert_eq!(regions.len(), 0, "the tiles draw no overlay regions");
    assert_eq!(
        browser::children(&snapshot.assets, "models")
            .iter()
            .map(|row| row.path.as_str())
            .collect::<Vec<_>>(),
        vec!["models/nested", "models/cube.obj"],
        "the grid lists the open folder's own children"
    );
}

#[test]
fn tiles_pick_an_icon_from_the_kind_and_the_extension() {
    let row = |path: &str, kind: &str| AssetListing {
        path: path.to_string(),
        kind: kind.to_string(),
        standalone: false,
        has_meta: false,
        guid: None,
    };
    assert_eq!(asset_icon(&row("models", "folder")), icons::Icon::Folder);
    assert_eq!(
        asset_icon(&row(
            "models/cube.obj",
            "pill_master_renderer::assets::Mesh"
        )),
        icons::Icon::Cube
    );
    assert_eq!(
        asset_icon(&row(
            "textures/skin.png",
            "pill_master_renderer::assets::Texture"
        )),
        icons::Icon::Image
    );
    assert_eq!(
        asset_icon(&row(
            "textures/stone.material",
            "pill_master_renderer::Material"
        )),
        icons::Icon::Sphere
    );
    assert_eq!(
        asset_icon(&row("shaders/code.wgsl", "file")),
        icons::Icon::Code
    );
    assert_eq!(asset_icon(&row("config.ini", "file")), icons::Icon::File);
}

#[test]
fn an_asset_type_names_its_row_icon() {
    assert_eq!(
        asset_type_icon("pill_master_renderer::assets::Mesh"),
        icons::Icon::Cube
    );
    assert_eq!(
        asset_type_icon("pill_master_renderer::assets::Material"),
        icons::Icon::Sphere
    );
    assert_eq!(
        asset_type_icon("pill_master_renderer::assets::Texture"),
        icons::Icon::Image
    );
    assert_eq!(
        asset_type_icon("pill_master_renderer::assets::Shader"),
        icons::Icon::Code
    );
    // A type with no shape of its own falls back to the plain file icon.
    assert_eq!(asset_type_icon("demo::Sound"), icons::Icon::File);
    assert_eq!(asset_type_icon(""), icons::Icon::File);
}

/// One egui pass over a fixed screen with an explicit event list: the drag
/// tests need presses and releases, which the plain [`run_pass`] never sends.
#[cfg(test)]
fn run_pass_with(
    context: &egui::Context,
    events: Vec<egui::Event>,
    pass: &mut dyn FnMut(&mut egui::Ui),
) {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1280.0, 720.0),
        )),
        events,
        ..Default::default()
    };
    let mut output = context.run_ui(input, pass);
    output.textures_delta.clear();
}

/// Dragging an asset tile from the grid onto its Inspector asset row queues
/// the reference edit, through egui's drag-and-drop payload (W-0048).
#[test]
fn a_dropped_asset_tile_queues_the_reference_edit() {
    const MESH_TYPE: &str = "pill_master_renderer::assets::Mesh";
    /// Draw the grid and the asset row, one frame.
    #[allow(clippy::too_many_arguments)]
    fn draw(
        context: &egui::Context,
        events: Vec<egui::Event>,
        snapshot: &EditorSnapshot,
        browser: &mut ProjectBrowser,
        selection: &mut EditorSelection,
        dialog: &mut Option<AssetDialog>,
        pending: &mut PendingEditorCommands,
        regions: &mut Vec<ShellRect>,
        drag_out: &mut Option<String>,
        session: &mut InspectorSession,
        view: &ComponentView,
        entity: Entity,
    ) {
        run_pass_with(context, events, &mut |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.allocate_ui(egui::vec2(640.0, 300.0), |ui| {
                    let previews = std::collections::BTreeMap::new();
                    let mut preview_visible = Vec::new();
                    let mut writes = AssetTileWrites {
                        selection,
                        dialog,
                        pending,
                        overlay_regions: regions,
                        drag_out,
                        previews: &previews,
                        preview_visible: &mut preview_visible,
                    };
                    assets_grid(ui, snapshot, browser, &mut writes);
                });
                inspector_fields::inspector_asset_row(
                    ui,
                    inspector_fields::AssetRowInput {
                        entity,
                        component: view,
                        index: 0,
                        field: "mesh",
                        asset_type: Some(MESH_TYPE),
                        tooltip: None,
                        targets: std::slice::from_ref(&entity),
                        details: &[],
                        assets: &snapshot.assets,
                    },
                    pending,
                    session,
                    regions,
                );
            });
        });
    }

    let engine = Engine::new();
    let mut snapshot = EditorSnapshot::capture_list(&engine, 0, &[], Vec::new());
    snapshot.assets = vec![AssetListing {
        path: "cube.obj".to_string(),
        kind: MESH_TYPE.to_string(),
        standalone: false,
        has_meta: true,
        guid: None,
    }];
    let entity = Entity::from_raw(1, 0);
    let view = ComponentView {
        type_name: "demo::MeshRenderer".to_string(),
        persistable: false,
        editable: true,
        fields: Vec::new(),
        values: vec![FieldValue::Asset {
            asset_type: Some(MESH_TYPE.to_string()),
            index: u32::MAX,
            generation: u32::MAX,
        }],
        asset_labels: vec![Some(AssetReferenceLabel::Unset)],
        entity_labels: vec![None],
    };
    let mut browser = ProjectBrowser::default();
    let mut selection = EditorSelection::default();
    let mut dialog: Option<AssetDialog> = None;
    let mut pending = PendingEditorCommands::default();
    let mut regions: Vec<ShellRect> = Vec::new();
    let mut drag_out: Option<String> = None;
    let mut session = InspectorSession::default();
    let context = egui::Context::default();
    style::apply(&context);

    // One idle pass lays both ends out and records their rectangles.
    draw(
        &context,
        Vec::new(),
        &snapshot,
        &mut browser,
        &mut selection,
        &mut dialog,
        &mut pending,
        &mut regions,
        &mut drag_out,
        &mut session,
        &view,
        entity,
    );
    let tile = browser
        .tile_rects
        .iter()
        .find(|(path, _)| path == "cube.obj")
        .map(|(_, corners)| {
            egui::Rect::from_min_max(
                egui::pos2(corners[0], corners[1]),
                egui::pos2(corners[2], corners[3]),
            )
        })
        .expect("the tile drew");
    let row = session
        .asset_row_rects
        .first()
        .map(|(_, rect)| *rect)
        .expect("the asset row drew");
    let from = tile.center();
    let to = row.center();

    // Press on the tile, move onto the row, release: the scripted drag's
    // three frames, in the same event shapes the window bridge produces.
    let press = egui::Event::PointerButton {
        pos: from,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::default(),
    };
    let release = egui::Event::PointerButton {
        pos: to,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::default(),
    };
    for events in [
        vec![egui::Event::PointerMoved(from), press],
        vec![egui::Event::PointerMoved(to)],
        vec![release],
    ] {
        draw(
            &context,
            events,
            &snapshot,
            &mut browser,
            &mut selection,
            &mut dialog,
            &mut pending,
            &mut regions,
            &mut drag_out,
            &mut session,
            &view,
            entity,
        );
    }

    assert_eq!(
        pending.batches.len(),
        1,
        "the drop queued one recorded edit"
    );
    assert!(
        matches!(
            pending.batches[0].commands.as_slice(),
            [EditorCommand::SetFieldAsset { field, reference: Some(path), .. }]
                if field == "mesh" && path == "cube.obj"
        ),
        "the drop points the field at the tile's asset: {:?}",
        pending.batches[0].commands
    );
}

/// A wheel event over a scroll area scrolls it; the bridge and the dock both
/// pass wheel deltas through untouched (W-0048: the Inspector is scrolled by
/// wheeling over it).
#[test]
fn a_row_click_moves_the_shared_selection() {
    let mut engine = Engine::new();
    let entity = engine
        .world_mut()
        .create_entity()
        .build()
        .expect("entity builds");
    let mut selection = EditorSelection::default();
    assert!(select_entity(&mut selection, entity));
    assert_eq!(selection.primary(), Some(entity));
    // A second click on the selected row is not a change.
    assert!(!select_entity(&mut selection, entity));
}

#[test]
fn an_asset_row_click_moves_the_asset_selection() {
    let mut selection = EditorSelection::default();
    assert!(select_asset(&mut selection, "textures/rock.png"));
    assert_eq!(selection.asset.as_deref(), Some("textures/rock.png"));
    // A second click on the selected row is not a change.
    assert!(!select_asset(&mut selection, "textures/rock.png"));
}

#[test]
fn the_inspector_lists_the_selections_components_and_fields() {
    use pill_editor_core::editor_state::ComponentView;
    use pill_engine::component_registry::ComponentFieldDescriptor;
    let mut engine = Engine::new();
    let entity = engine
        .world_mut()
        .create_entity()
        .build()
        .expect("entity builds");
    let detail = EntityDetail {
        entity,
        components: vec![
            ComponentView {
                asset_labels: Vec::new(),
                entity_labels: Vec::new(),
                type_name: "tests::Health".to_string(),
                persistable: false,
                editable: true,
                fields: vec![ComponentFieldDescriptor {
                    name: "current",
                    type_tag: "f32",
                    offset: 0,
                    size: 4,
                    align: 4,
                    element_count: 0,
                    decorators: std::borrow::Cow::Borrowed(&[]),
                }],
                values: vec![FieldValue::F32(3.0)],
            },
            ComponentView {
                asset_labels: Vec::new(),
                entity_labels: Vec::new(),
                type_name: "tests::Marker".to_string(),
                persistable: false,
                editable: false,
                fields: Vec::new(),
                values: Vec::new(),
            },
        ],
    };
    let rows = component_rows(&detail);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, "tests::Health");
    assert_eq!(rows[0].1, vec![("current".to_string(), "3".to_string())]);
    // A component without editable fields lists none.
    assert_eq!(rows[1].0, "tests::Marker");
    assert!(rows[1].1.is_empty());

    // The tab draws the detail without panicking.
    let mut snapshot = EditorSnapshot::capture_list(&engine, 0, &[], Vec::new());
    snapshot.detail = Some(detail);
    let mut selection = EditorSelection::default();
    selection.select_only(entity);
    selection.asset = Some("textures/rock.png".to_string());
    snapshot.asset_detail = Some(pill_editor_core::editor_state::AssetDetail {
        path: "textures/rock.png".to_string(),
        kind: "pill_master_renderer::assets::Texture".to_string(),
        standalone: false,
        settings: Some(serde_json::json!({
            "texture_type": "Color",
            "mips": 4,
            "flip_v": false,
            "waves": [{ "amplitude": 1.0 }],
        })),
    });
    let mut pending = PendingEditorCommands::default();
    let context = egui::Context::default();
    style::apply(&context);
    let mut regions = Vec::new();
    let mut inspector_session = InspectorSession::default();
    run_pass(&context, &mut |ui| {
        egui::CentralPanel::default().show(ui, |ui| {
            inspector_tab(
                ui,
                &snapshot,
                &selection,
                &mut pending,
                &mut regions,
                &mut inspector_session,
            );
        });
    });
    assert!(pending.batches.is_empty(), "drawing queues no edits");
}

#[test]
fn field_values_format_for_display() {
    assert_eq!(format_field_value(&FieldValue::F32(1.5)), "1.5");
    assert_eq!(format_field_value(&FieldValue::U32(7)), "7");
    assert_eq!(format_field_value(&FieldValue::Bool(true)), "true");
    assert_eq!(
        format_field_value(&FieldValue::Text("hi".to_string())),
        "hi"
    );
}
