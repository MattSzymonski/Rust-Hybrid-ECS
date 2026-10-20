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
    let context = egui::Context::default();
    style::apply(&context);
    let mut regions = Vec::new();
    let mut inspector_session = InspectorSession::default();
    let _ = context.run(egui::RawInput::default(), |context| {
        egui::CentralPanel::default().show(context, |ui| {
            entities_tab(ui, &snapshot, &mut selection, &mut pending, &mut regions);
            systems_tab(ui, &snapshot, &mut pending);
            inspector_tab(
                ui,
                &snapshot,
                &selection,
                &mut pending,
                &mut regions,
                &mut inspector_session,
            );
            assets_tab(
                ui,
                &snapshot,
                &mut selection,
                &mut dialog,
                &mut pending,
                &mut regions,
                &mut browser,
            );
            preferences_tab(ui, &snapshot, &mut draft, &mut pending);
            scene_overlay(ui, 120.0, &mut regions);
        });
    });
    assert_eq!(regions.len(), 2, "the two chips are the UI's regions");
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
        },
        AssetListing {
            path: "models/nested".to_string(),
            kind: "folder".to_string(),
            standalone: false,
            has_meta: false,
        },
        AssetListing {
            path: "models/cube.obj".to_string(),
            kind: "pill_master_renderer::assets::Mesh".to_string(),
            standalone: false,
            has_meta: true,
        },
        AssetListing {
            path: "shaders/code.wgsl".to_string(),
            kind: "file".to_string(),
            standalone: false,
            has_meta: false,
        },
    ];
    let mut browser = ProjectBrowser::default();
    assert!(browser.open("models"));
    assert!(browser.is_expanded("models") || browser.folder == "models");
    let mut selection = EditorSelection::default();
    let mut dialog: Option<AssetDialog> = None;
    let mut pending = PendingEditorCommands::default();
    let context = egui::Context::default();
    style::apply(&context);
    let mut regions = Vec::new();
    let mut caption_rows = 0usize;
    let mut caption_width = 0.0f32;
    let _ = context.run(egui::RawInput::default(), |context| {
        egui::CentralPanel::default().show(context, |ui| {
            assets_tree(ui, &snapshot, &mut browser, &mut selection, &mut pending);
            assets_path_bar(ui, &mut browser);
            assets_zoom_bar(ui, &snapshot.assets, &mut browser);
            assets_grid(
                ui,
                &snapshot,
                &mut browser,
                &mut selection,
                &mut dialog,
                &mut pending,
                &mut regions,
            );
            let font = egui::FontId::proportional(ASSETS_TILE_LABEL_SIZE);
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
fn a_row_click_moves_the_shared_selection() {
    let mut engine = Engine::new();
    let entity = engine
        .world_mut()
        .create_entity()
        .build()
        .expect("entity builds");
    let mut selection = EditorSelection::default();
    assert!(select_entity(&mut selection, entity));
    assert_eq!(selection.entity, Some(entity));
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
    use pill_editor_core_egui::editor_state::ComponentView;
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
    selection.entity = Some(entity);
    selection.asset = Some("textures/rock.png".to_string());
    snapshot.asset_detail = Some(pill_editor_core_egui::editor_state::AssetDetail {
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
    let _ = context.run(egui::RawInput::default(), |context| {
        egui::CentralPanel::default().show(context, |ui| {
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
