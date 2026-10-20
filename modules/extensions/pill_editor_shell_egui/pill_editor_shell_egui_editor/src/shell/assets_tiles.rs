//! The Assets tab's tiles, their menu and the asset dialog.
//!
//! # Responsibilities
//!
//! - `asset_tile` and `asset_icon`: one file's tile, its icon, its two-line name and its drag source.
//! - `asset_menu_items`: the tile's context menu.
//! - `asset_dialog_window`: the create and rename dialogs.
//!
//! Bodies unchanged from the shell's root file (W-0037 Stage 3); the globs
//! below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// The Assets tab's grid tiles: the gap around each tile, the band under its
/// icon for the name, the icon's inset, and the corner radius of the
/// selected and hovered washes, in points.
pub const ASSETS_TILE_GAP: f32 = 8.0;

pub const ASSETS_TILE_LABEL_HEIGHT: f32 = 26.0;

pub const ASSETS_TILE_LABEL_GAP: f32 = 2.0;

pub const ASSETS_TILE_ICON_INSET: f32 = 6.0;

pub const ASSETS_TILE_CORNER_RADIUS: f32 = 4.0;

pub const ASSETS_TILE_LABEL_SIZE: f32 = 11.0;

/// The rows a tile's name may wrap to before it elides, like Unity's two.
pub const ASSETS_TILE_LABEL_ROWS: usize = 2;

/// The handles a grid tile writes through: the selection a click moves, the
/// dialog a menu entry opens, the commands it queues and the overlay regions
/// an open menu marks as the UI's. One bundle keeps `asset_tile`'s signature
/// readable and within clippy's argument budget.
pub struct AssetTileWrites<'a> {
    /// The shared selection the tile's clicks move.
    pub selection: &'a mut EditorSelection,
    /// The create/rename dialog the tile's menu can open.
    pub dialog: &'a mut Option<AssetDialog>,
    /// The recorded file commands the tile's menu queues.
    pub pending: &'a mut PendingEditorCommands,
    /// The regions an open menu marks so a click inside is never a scene pick.
    pub overlay_regions: &'a mut Vec<ShellRect>,
}

/// One grid tile: the row's duotone icon and its name, the selection and
/// hover washes, the double-click that opens a folder, the drag a tile
/// carries and the menu the right-click opens.
pub fn asset_tile(
    ui: &mut egui::Ui,
    row: &AssetListing,
    cell: egui::Vec2,
    browser: &mut ProjectBrowser,
    snapshot: &EditorSnapshot,
    writes: &mut AssetTileWrites<'_>,
) {
    let (rect, response) = ui.allocate_exact_size(cell, egui::Sense::click_and_drag());
    let visuals = ui.visuals().clone();
    let selected = writes.selection.asset.as_deref() == Some(row.path.as_str());
    if selected || response.hovered() {
        let fill = if selected {
            visuals.selection.bg_fill
        } else {
            visuals.widgets.hovered.weak_bg_fill
        };
        ui.painter()
            .rect_filled(rect, ASSETS_TILE_CORNER_RADIUS, fill);
    }
    let icon_side = (cell.x - ASSETS_TILE_GAP - ASSETS_TILE_ICON_INSET * 2.0).max(8.0);
    let icon_rect = egui::Rect::from_min_size(
        egui::pos2(
            rect.center().x - icon_side * 0.5,
            rect.min.y + ASSETS_TILE_ICON_INSET,
        ),
        egui::vec2(icon_side, icon_side),
    );
    let tint = if selected {
        visuals.selection.stroke.color
    } else {
        visuals.text_color()
    };
    icons::paint_duotone(ui.painter(), icon_rect, asset_icon(row), tint);
    let font = egui::FontId::proportional(ASSETS_TILE_LABEL_SIZE);
    let job = tile_name_job(
        browser::file_name(&row.path),
        &font,
        visuals.text_color(),
        cell.x - ASSETS_TILE_GAP,
    );
    let label = ui.painter().layout_job(job);
    let label_origin = egui::pos2(
        rect.center().x - label.size().x * 0.5,
        icon_rect.max.y + ASSETS_TILE_LABEL_GAP,
    );
    ui.painter()
        .galley(label_origin, label, visuals.text_color());
    if response.clicked() {
        select_asset(writes.selection, &row.path);
    }
    if response.double_clicked() && browser::is_folder(row) {
        browser.open(&row.path);
    }
    if response.secondary_clicked() {
        select_asset(writes.selection, &row.path);
    }
    context_menu("assets", &response, writes.overlay_regions, &mut |ui| {
        asset_menu_items(
            ui,
            snapshot,
            row,
            writes.selection,
            writes.dialog,
            writes.pending,
        )
    });
    // Folders are drop targets (a tile lands inside one) and every tile is a
    // drag source, so a file moves by dragging it onto a folder.
    if browser::is_folder(row) {
        if let Some(dragged) = response.dnd_release_payload::<String>() {
            if let Some(to) = assets::drop_destination(&dragged, &row.path) {
                assets::queue_move(writes.pending, &dragged, &to);
            }
        }
    }
    response.dnd_set_drag_payload(row.path.clone());
    response.on_hover_text(row.path.as_str());
}

/// The icon a row's tile draws: folders, the project's asset types by their
/// last type-name segment, and plain files by their extension.
pub fn asset_icon(row: &AssetListing) -> icons::Icon {
    if browser::is_folder(row) {
        return icons::Icon::Folder;
    }
    let type_leaf = row.kind.rsplit("::").next().unwrap_or(row.kind.as_str());
    let extension = row
        .path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match type_leaf {
        "Mesh" => icons::Icon::Cube,
        "Texture" => icons::Icon::Image,
        "Material" => icons::Icon::Sphere,
        _ => match extension.as_str() {
            "png" | "jpg" | "jpeg" | "bmp" | "tga" | "hdr" => icons::Icon::Image,
            "obj" | "gltf" | "glb" => icons::Icon::Cube,
            "hlsl" | "wgsl" | "glsl" | "frag" | "vert" | "comp" => icons::Icon::Code,
            _ => icons::Icon::File,
        },
    }
}

/// A tile name's layout job: wrapped to two rows at `width` points, broken
/// inside a word when it must (a long extension has no spaces), with an
/// ellipsis where the second row runs out - Unity's tile captions.
pub fn tile_name_job(
    text: &str,
    font: &egui::FontId,
    color: egui::Color32,
    width: f32,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::simple(text.to_string(), font.clone(), color, width);
    job.wrap.max_rows = ASSETS_TILE_LABEL_ROWS;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    job
}

/// The asset row's menu: the toolbar's actions on the row the right-click
/// selected. Create always; rename, delete and the write-meta action only
/// when they can act on that row, like the toolbar's buttons.
pub fn asset_menu_items(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    row: &AssetListing,
    selection: &EditorSelection,
    dialog: &mut Option<AssetDialog>,
    pending: &mut PendingEditorCommands,
) {
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
    if row.kind == "folder" {
        return;
    }
    if ui
        .add(icons::IconButton::new(
            icons::Icon::PencilSimple,
            "Rename Asset...",
        ))
        .clicked()
    {
        assets::open_rename(dialog, selection, &snapshot.assets);
        ui.close();
    }
    if ui
        .add(icons::IconButton::new(icons::Icon::Trash, "Delete Asset"))
        .clicked()
    {
        assets::delete_selection(pending, selection, &snapshot.assets);
        ui.close();
    }
    if !row.standalone
        && !row.has_meta
        && ui
            .add(icons::IconButton::new(
                icons::Icon::FileArrowDown,
                "Write Asset Meta",
            ))
            .clicked()
    {
        assets::write_meta_selection(pending, selection, &snapshot.assets);
        ui.close();
    }
}

/// The create or rename dialog, drawn over the dock while one is open: the
/// create dialog lists the live standalone types and a name field, the
/// rename dialog the current path and the new name; their buttons run the
/// shared submit helpers, which queue the recorded batch and close the
/// dialog.
///
/// Public because [`run_frame`]'s generated hot patch calls it through the
/// package path.
pub fn asset_dialog_window(
    context: &egui::Context,
    dialog: &mut Option<AssetDialog>,
    pending: &mut PendingEditorCommands,
    snapshot: &EditorSnapshot,
) {
    let Some(current) = dialog.as_ref() else {
        return;
    };
    let (title, submit_label) = match current {
        AssetDialog::Create { .. } => ("Create Asset", "Create"),
        AssetDialog::Rename { .. } => ("Rename Asset", "Rename"),
    };
    let mut submit = false;
    let mut cancel = false;
    egui::Window::new(title)
        .id(egui::Id::new("pill_editor_shell_egui.asset_dialog"))
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .collapsible(false)
        .resizable(false)
        .show(context, |ui| {
            let Some(current) = dialog.as_mut() else {
                return;
            };
            match current {
                AssetDialog::Create {
                    folder,
                    type_name,
                    name,
                } => {
                    ui.label(format!(
                        "In res{}{}",
                        if folder.is_empty() { "" } else { "/" },
                        folder
                    ));
                    ui.separator();
                    if snapshot.standalone_types.is_empty() {
                        ui.label("No asset types are loaded.");
                    }
                    for info in &snapshot.standalone_types {
                        let chosen = *type_name == info.type_name;
                        let marker = if chosen { ">" } else { " " };
                        let label = format!("{marker} {} (.{})", info.display_name, info.extension);
                        if ui.selectable_label(chosen, label).clicked() {
                            *type_name = info.type_name.clone();
                        }
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("Name");
                        ui.text_edit_singleline(name);
                    });
                }
                AssetDialog::Rename { from, name } => {
                    ui.label(format!("Rename {from}"));
                    ui.separator();
                    ui.label("New name");
                    ui.text_edit_singleline(name);
                    ui.label(
                        egui::RichText::new("The file keeps its extension.")
                            .weak()
                            .italics(),
                    );
                }
            }
            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .add(icons::IconButton::new(
                        icons::Icon::CheckCircle,
                        submit_label,
                    ))
                    .clicked()
                {
                    submit = true;
                }
                if ui
                    .add(icons::IconButton::new(icons::Icon::XCircle, "Cancel"))
                    .clicked()
                {
                    cancel = true;
                }
            });
        });
    if submit {
        match dialog.as_ref() {
            Some(AssetDialog::Create { .. }) => {
                assets::submit_create(dialog, pending);
            }
            Some(AssetDialog::Rename { .. }) => {
                assets::submit_rename(dialog, pending);
            }
            None => {}
        }
    }
    if cancel {
        assets::cancel(dialog);
    }
}
