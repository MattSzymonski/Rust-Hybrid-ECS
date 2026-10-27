//! The Assets tab: the folder tree and the open folder's grid.
//!
//! # Responsibilities
//!
//! - `assets_tab`: the toolbar, the tree and the grid.
//! - `assets_tree` and `folder_tree_rows`: the folder rows, their carets and the drag targets.
//! - The path bar, the zoom bar and `assets_grid`.
//!
//! The globs below are absolute paths, so a patch crate resolves them too.

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
    browser: &mut ProjectBrowser,
    writes: &mut AssetTileWrites<'_>,
) {
    // A folder that vanished (deleted, renamed, or the project reloaded onto
    // another tree) falls back to its nearest ancestor, so the grid never
    // lists a path the listing no longer holds.
    browser.settle(&snapshot.assets);
    // The tab's whole body, claimed before anything in it is drawn: the grid is
    // only as tall as the dock row lets it, so the band under it, the strip
    // beside the zoom bar and the count row all look like the folder's empty
    // space but belong to no widget. A right-click on any of them opens the
    // open folder's container menu - the one the grid's spare space offers -
    // and a tile dropped there lands in the open folder. Panels, tree rows,
    // tiles and every control register after this one, so their own menus and
    // clicks win wherever they cover it.
    let body = ui.available_rect_before_wrap();
    let background = ui.interact(
        body,
        ui.id().with("pill_editor_shell_egui.assets_tab"),
        egui::Sense::click_and_drag(),
    );
    if let Some(dragged) = background.dnd_release_payload::<String>() {
        if let Some(to) = assets::drop_destination(&dragged, &browser.folder) {
            assets::queue_move(writes.pending, &dragged, &to);
        }
    }
    context_menu("assets", &background, writes.overlay_regions, &mut |ui| {
        asset_container_menu(
            ui,
            snapshot,
            writes.selection,
            writes.dialog,
            writes.pending,
            &browser.folder,
        );
    });
    // The panel has no management buttons: the empty strip's menu creates and
    // opens a folder, a file's menu renames and deletes it. What is left of the
    // bar is the count, which tells a person how much the walk found.
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(format!("{} entries", snapshot.assets.len())).weak());
    });
    ui.separator();
    egui::Panel::left("pill_editor_shell_egui.assets_tree")
        .resizable(true)
        .default_size(ASSETS_TREE_WIDTH)
        .size_range(ASSETS_TREE_WIDTH_RANGE)
        .show(ui, |ui| {
            assets_tree(
                ui,
                snapshot,
                browser,
                writes.selection,
                writes.dialog,
                writes.pending,
                writes.overlay_regions,
            );
        });
    egui::Panel::top("pill_editor_shell_egui.assets_path").show(ui, |ui| {
        assets_path_bar(ui, browser);
    });
    egui::Panel::bottom("pill_editor_shell_egui.assets_zoom").show(ui, |ui| {
        assets_zoom_bar(ui, &snapshot.assets, browser);
    });
    egui::CentralPanel::default().show(ui, |ui| {
        assets_grid(ui, snapshot, browser, writes);
    });
}

/// The Assets tab's folder tree: the panel's two roots - the project's
/// assets and the extensions' read-only resources - then every folder under
/// each at its own indent, with the open folder's row highlighted. A row
/// click opens that folder in the grid and selects it (the create dialog's
/// destination follows the selection); the caret opens and closes its
/// children; a dropped tile moves its file into the row's folder, and the
/// extensions side takes no drops at all. A row's menu is the container's -
/// make an asset in that folder, open it in the file manager - and the spare
/// space below the rows offers the same for the open folder.
pub fn assets_tree(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    browser: &mut ProjectBrowser,
    selection: &mut EditorSelection,
    dialog: &mut Option<AssetDialog>,
    pending: &mut PendingEditorCommands,
    overlay_regions: &mut Vec<ShellRect>,
) {
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for root in browser::ROOTS {
                // The panel's own level: the project's tree, and every
                // extension's resources beside it rather than inside it.
                let root_row = ui.selectable_label(
                    browser.folder == root.key,
                    icons::rich(icons::Icon::Folder, root.label),
                );
                if root_row.clicked() {
                    browser.open(root.key);
                }
                if let Some(dragged) = root_row.dnd_release_payload::<String>() {
                    if !root.read_only {
                        if let Some(to) = assets::drop_destination(&dragged, root.key) {
                            assets::queue_move(pending, &dragged, &to);
                        }
                    }
                }
                root_row.on_hover_text(if root.read_only {
                    "The extensions' resources: listed and inspected, never written"
                } else {
                    "The project's asset root"
                });
                folder_tree_rows(
                    ui,
                    &snapshot.assets,
                    browser,
                    selection,
                    dialog,
                    pending,
                    overlay_regions,
                    snapshot,
                    root.key,
                    1,
                );
            }
            // The spare space under the rows: the same container menu, for the
            // folder the grid has open.
            let rest = ui.available_rect_before_wrap();
            let background = ui.interact(
                rest,
                ui.id().with("pill_editor_shell_egui.assets_tree_space"),
                egui::Sense::click(),
            );
            let folder = browser.folder.clone();
            context_menu("assets tree", &background, overlay_regions, &mut |ui| {
                asset_container_menu(ui, snapshot, selection, dialog, pending, &folder);
            });
        });
}

/// One level of the folder tree: a row per folder under `parent` and, for
/// each open row, its own children under it.
#[allow(clippy::too_many_arguments)]
pub fn folder_tree_rows(
    ui: &mut egui::Ui,
    assets: &[AssetListing],
    browser: &mut ProjectBrowser,
    selection: &mut EditorSelection,
    dialog: &mut Option<AssetDialog>,
    pending: &mut PendingEditorCommands,
    overlay_regions: &mut Vec<ShellRect>,
    snapshot: &EditorSnapshot,
    parent: &str,
    depth: usize,
) {
    for folder in browser::child_folders(assets, parent) {
        let key = folder.key();
        let path = key.as_str();
        let read_only = folder.read_only;
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
            // The row is a container: its menu makes an asset inside it and
            // opens it in the file manager.
            context_menu("assets tree", &label, overlay_regions, &mut |ui| {
                asset_container_menu(ui, snapshot, selection, dialog, pending, path);
            });
            // A module's resource tree takes no drops: nothing may be written
            // into it.
            if let Some(dragged) = label.dnd_release_payload::<String>() {
                if !read_only {
                    dropped = Some(dragged);
                }
            }
            let row_rect = label.rect;
            label.on_hover_text(path);
            remember_tree_row_rect(browser, path, row_rect);
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
            folder_tree_rows(
                ui,
                assets,
                browser,
                selection,
                dialog,
                pending,
                overlay_regions,
                snapshot,
                path,
                depth + 1,
            );
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
    writes: &mut AssetTileWrites<'_>,
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
            // The pane behind the tiles, claimed before them: a right-click
            // anywhere they do not cover - the gap beside a row's last tile, the
            // space under the last row, whatever a short folder leaves - is the
            // container's menu, and a tile dropped there lands in the open
            // folder. A tile is registered after this, so it sits on top where
            // the pointer is over one and its own menu wins there.
            //
            // The claimed rectangle has to reach past the visible pane down to
            // the last row: a folder with more rows than the panel shows puts
            // its lower rows outside the viewport slice, and a gap in one of
            // those rows is exactly where a right-click used to answer nothing.
            let pane = ui.available_rect_before_wrap();
            let rows = entries.len().div_ceil(columns.max(1));
            let covered = egui::Rect::from_min_size(
                pane.min,
                egui::vec2(
                    pane.width().max(columns as f32 * cell.x),
                    pane.height().max(rows as f32 * cell.y),
                ),
            );
            let background = ui.interact(
                covered,
                ui.id().with("pill_editor_shell_egui.assets_pane"),
                egui::Sense::click_and_drag(),
            );
            remember_pane_rect(browser, covered);
            if let Some(dragged) = background.dnd_release_payload::<String>() {
                if let Some(to) = assets::drop_destination(&dragged, &folder) {
                    assets::queue_move(writes.pending, &dragged, &to);
                }
            }
            context_menu("assets", &background, writes.overlay_regions, &mut |ui| {
                asset_container_menu(
                    ui,
                    snapshot,
                    writes.selection,
                    writes.dialog,
                    writes.pending,
                    &folder,
                );
            });
            if entries.is_empty() {
                ui.label(egui::RichText::new("This folder is empty").weak());
            }
            for row in entries.chunks(columns) {
                ui.horizontal(|ui| {
                    for entry in row {
                        asset_tile(ui, entry, cell, browser, snapshot, writes);
                    }
                });
            }
        });
}
