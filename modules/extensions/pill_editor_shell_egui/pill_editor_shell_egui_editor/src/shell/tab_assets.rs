//! The Assets tab: the folder tree and the open folder's grid.
//!
//! # Responsibilities
//!
//! - `assets_tab`: the toolbar, the tree and the grid.
//! - `assets_tree` and `folder_tree_rows`: the folder rows, their carets and the drag targets.
//! - The path bar, the zoom bar and `assets_grid`.
//!
//! Bodies unchanged from the shell's root file (W-0037 Stage 3); the globs
//! below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// The Assets tab's tree pane: its default width and the range the splitter
/// drags it through, in points.
pub const ASSETS_TREE_WIDTH: f32 = 176.0;

pub const ASSETS_TREE_WIDTH_RANGE: std::ops::RangeInclusive<f32> = 96.0..=320.0;

/// One folder tree row's indentation per level and the width the expander's
/// caret (or its empty slot) takes, in points.
pub const ASSETS_TREE_INDENT: f32 = 12.0;

pub const ASSETS_TREE_CARET_WIDTH: f32 = 14.0;

pub const ASSETS_TREE_CARET_SIZE: f32 = 10.0;

/// The Assets tab: Unity's project panel - a folder tree on the left, the
/// open folder's path over a grid of tiles on the right, and the toolbar
/// that edits the selected entry. A tile dragged onto a folder row (or onto
/// a folder tile) moves the file there; a row's menu and the strip under
/// the tiles open the create/rename dialogs as windows over the dock.
#[pill_hot_fn]
pub fn assets_tab(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    selection: &mut EditorSelection,
    dialog: &mut Option<AssetDialog>,
    pending: &mut PendingEditorCommands,
    overlay_regions: &mut Vec<ShellRect>,
    browser: &mut ProjectBrowser,
) {
    // A folder that vanished (deleted, renamed, or the project reloaded onto
    // another tree) falls back to its nearest ancestor, so the grid never
    // lists a path the listing no longer holds.
    browser.settle(&snapshot.assets);
    let selected_row = assets::selected_row(selection, &snapshot.assets);
    let selected_is_file = selected_row.is_some_and(|row| row.kind != "folder");
    let missing_meta =
        selected_row.is_some_and(|row| row.kind != "folder" && !row.standalone && !row.has_meta);
    // The toolbar: creating opens the dialog; the rest act on the selected
    // entry and queue recorded file commands. The buttons only appear when
    // they can act, like the native panel's. The tab's own title already
    // names the panel, so a second heading would only cost the grid room.
    ui.horizontal_wrapped(|ui| {
        if ui
            .add(icons::IconButton::new(icons::Icon::Plus, "Create Asset..."))
            .clicked()
        {
            assets::open_create(
                dialog,
                selection,
                &snapshot.assets,
                &snapshot.standalone_types,
            );
        }
        if selected_is_file {
            if ui
                .add(icons::IconButton::new(icons::Icon::Trash, "Delete Asset"))
                .clicked()
            {
                assets::delete_selection(pending, selection, &snapshot.assets);
            }
            if ui
                .add(icons::IconButton::new(
                    icons::Icon::PencilSimple,
                    "Rename Asset...",
                ))
                .clicked()
            {
                assets::open_rename(dialog, selection, &snapshot.assets);
            }
        }
        if missing_meta
            && ui
                .add(icons::IconButton::new(
                    icons::Icon::FileArrowDown,
                    "Write Asset Meta",
                ))
                .clicked()
        {
            assets::write_meta_selection(pending, selection, &snapshot.assets);
        }
        ui.add_space(8.0);
        ui.label(egui::RichText::new(format!("{} entries", snapshot.assets.len())).weak());
    });
    ui.separator();
    egui::Panel::left("pill_editor_shell_egui.assets_tree")
        .resizable(true)
        .default_size(ASSETS_TREE_WIDTH)
        .size_range(ASSETS_TREE_WIDTH_RANGE)
        .show(ui, |ui| {
            assets_tree(ui, snapshot, browser, selection, pending);
        });
    egui::Panel::top("pill_editor_shell_egui.assets_path").show(ui, |ui| {
        assets_path_bar(ui, browser);
    });
    egui::Panel::bottom("pill_editor_shell_egui.assets_zoom").show(ui, |ui| {
        assets_zoom_bar(ui, &snapshot.assets, browser);
    });
    egui::CentralPanel::default().show(ui, |ui| {
        assets_grid(
            ui,
            snapshot,
            browser,
            selection,
            dialog,
            pending,
            overlay_regions,
        );
    });
}

/// The Assets tab's folder tree: the project's asset root, then every folder
/// under it at its own indent, with the open folder's row highlighted. A
/// row click opens that folder in the grid and selects it (the create
/// dialog's destination follows the selection); the caret opens and closes
/// its children; a dropped tile moves its file into the row's folder.
pub fn assets_tree(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    browser: &mut ProjectBrowser,
    selection: &mut EditorSelection,
    pending: &mut PendingEditorCommands,
) {
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let root = ui.selectable_label(
                browser.folder.is_empty(),
                icons::rich(icons::Icon::Folder, browser::ROOT_LABEL),
            );
            if root.clicked() {
                browser.open("");
            }
            if let Some(dragged) = root.dnd_release_payload::<String>() {
                if let Some(to) = assets::drop_destination(&dragged, "") {
                    assets::queue_move(pending, &dragged, &to);
                }
            }
            root.on_hover_text("The project's asset root");
            folder_tree_rows(ui, &snapshot.assets, browser, selection, pending, "", 1);
        });
}

/// One level of the folder tree: a row per folder under `parent` and, for
/// each open row, its own children under it.
pub fn folder_tree_rows(
    ui: &mut egui::Ui,
    assets: &[AssetListing],
    browser: &mut ProjectBrowser,
    selection: &mut EditorSelection,
    pending: &mut PendingEditorCommands,
    parent: &str,
    depth: usize,
) {
    for folder in browser::child_folders(assets, parent) {
        let path = folder.path.as_str();
        let expanded = browser.is_expanded(path);
        let has_children = !browser::child_folders(assets, path).is_empty();
        let mut toggle = false;
        let mut open = false;
        let mut dropped = None;
        ui.horizontal(|ui| {
            ui.add_space(ASSETS_TREE_INDENT * depth as f32);
            if has_children {
                let icon = if expanded {
                    icons::Icon::CaretDown
                } else {
                    icons::Icon::CaretRight
                };
                let caret = egui::RichText::new(icon.glyph().to_string())
                    .family(egui::FontFamily::Name(icons::FAMILY.into()))
                    .size(ASSETS_TREE_CARET_SIZE);
                if ui.add(egui::Button::new(caret).frame(false)).clicked() {
                    toggle = true;
                }
            } else {
                ui.add_space(ASSETS_TREE_CARET_WIDTH);
            }
            let label = ui.selectable_label(
                browser.folder == path,
                icons::rich(icons::Icon::Folder, browser::file_name(path)),
            );
            if label.clicked() {
                open = true;
            }
            if let Some(dragged) = label.dnd_release_payload::<String>() {
                dropped = Some(dragged);
            }
            label.on_hover_text(path);
        });
        if toggle {
            browser.toggle_expanded(path);
        }
        if open {
            browser.open(path);
            select_asset(selection, path);
        }
        if let Some(dragged) = dropped {
            if let Some(to) = assets::drop_destination(&dragged, path) {
                assets::queue_move(pending, &dragged, &to);
            }
        }
        if expanded {
            folder_tree_rows(ui, assets, browser, selection, pending, path, depth + 1);
        }
    }
}

/// The path strip over the grid: the asset root and every segment of the
/// open folder, the ancestors clickable so a single click walks back up.
pub fn assets_path_bar(ui: &mut egui::Ui, browser: &mut ProjectBrowser) {
    ui.horizontal(|ui| {
        let segments = browser::breadcrumb(&browser.folder);
        let last = segments.len().saturating_sub(1);
        for (index, (path, label)) in segments.iter().enumerate() {
            if index > 0 {
                ui.label(egui::RichText::new(">").weak());
            }
            if index == last {
                ui.label(egui::RichText::new(label).strong());
            } else if ui.link(label).clicked() {
                browser.open(path);
            }
        }
    });
}

/// The strip under the grid: how many items the open folder holds, and
/// Unity's tile size slider, which resizes every tile live.
pub fn assets_zoom_bar(ui: &mut egui::Ui, assets: &[AssetListing], browser: &mut ProjectBrowser) {
    ui.horizontal(|ui| {
        let count = browser::children(assets, &browser.folder).len();
        ui.label(egui::RichText::new(format!("{count} items")).weak());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(
                egui::Slider::new(&mut browser.tile_size, browser::TILE_SIZE_RANGE)
                    .show_value(false),
            )
            .on_hover_text("Tile size");
        });
    });
}

/// The open folder's tiles: its folders first, then its files, each an icon
/// with its own name under it. The strip below the tiles lands a dropped
/// tile in the open folder and opens the create dialog from its menu.
pub fn assets_grid(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    browser: &mut ProjectBrowser,
    selection: &mut EditorSelection,
    dialog: &mut Option<AssetDialog>,
    pending: &mut PendingEditorCommands,
    overlay_regions: &mut Vec<ShellRect>,
) {
    let cell = egui::vec2(
        browser.tile_size + ASSETS_TILE_GAP,
        browser.tile_size + ASSETS_TILE_LABEL_HEIGHT,
    );
    let folder = browser.folder.clone();
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // The gap lives inside each cell, so the tiles sit edge to edge:
            // egui's item spacing would add a gutter per tile and push the
            // last column of a full row past the pane's width.
            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
            let columns = browser::tile_columns(ui.available_width(), cell.x);
            let entries = browser::children(&snapshot.assets, &folder);
            if entries.is_empty() {
                ui.label(egui::RichText::new("This folder is empty").weak());
            }
            let mut writes = AssetTileWrites {
                selection: &mut *selection,
                dialog: &mut *dialog,
                pending: &mut *pending,
                overlay_regions: &mut *overlay_regions,
            };
            for row in entries.chunks(columns) {
                ui.horizontal(|ui| {
                    for entry in row {
                        asset_tile(ui, entry, cell, browser, snapshot, &mut writes);
                    }
                });
            }
            let empty = ui.allocate_response(empty_area(ui), egui::Sense::click_and_drag());
            if let Some(dragged) = empty.dnd_release_payload::<String>() {
                if let Some(to) = assets::drop_destination(&dragged, &folder) {
                    assets::queue_move(pending, &dragged, &to);
                }
            }
            context_menu("assets", &empty, overlay_regions, &mut |ui| {
                if ui
                    .add(icons::IconButton::new(icons::Icon::Plus, "Create Asset..."))
                    .clicked()
                {
                    assets::open_create(
                        dialog,
                        selection,
                        &snapshot.assets,
                        &snapshot.standalone_types,
                    );
                    ui.close();
                }
            });
        });
}
