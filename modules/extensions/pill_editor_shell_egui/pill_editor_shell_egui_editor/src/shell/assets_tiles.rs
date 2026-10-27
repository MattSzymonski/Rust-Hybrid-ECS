//! The Assets tab's tiles, their menu and the asset dialog.
//!
//! # Responsibilities
//!
//! - `asset_tile` and `asset_icon`: one file's tile, its icon, its two-line name and its drag source.
//! - `asset_menu_items`: the tile's context menu.
//! - `asset_dialog_window`: the create and rename dialogs.
//!
//! The globs below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// The horizontal padding inside a tile's cell, in points.
pub const ASSETS_TILE_GAP: f32 = 8.0;

/// The band under a tile's icon that holds its name, in points.
pub const ASSETS_TILE_LABEL_HEIGHT: f32 = 26.0;

/// The gap between a tile's icon and its name, in points.
pub const ASSETS_TILE_LABEL_GAP: f32 = 2.0;

/// The inset between a tile's cell edge and its icon, in points.
pub const ASSETS_TILE_ICON_INSET: f32 = 6.0;

/// The corner radius of a selected or hovered tile's wash, in points.
pub const ASSETS_TILE_CORNER_RADIUS: f32 = 4.0;

/// The rows a tile's name may wrap to before it elides, like Unity's two.
pub const ASSETS_TILE_LABEL_ROWS: usize = 2;

/// The side of the type icon a finished preview keeps in its bottom-left
/// corner, in points.
pub const ASSETS_TILE_BADGE_SIZE: f32 = 14.0;

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
    /// The asset path a drag handed to the OS: the shell
    /// reports it, the executable starts the OLE drag.
    pub drag_out: &'a mut Option<String>,
    /// The rendered previews, by row key: a tile draws the one
    /// it finds and keeps its icon otherwise.
    pub previews: &'a std::collections::BTreeMap<String, egui::TextureHandle>,
    /// The tiles drawn this frame, by row key, which the executable turns into
    /// preview requests: only what is on screen costs work.
    pub preview_visible: &'a mut Vec<String>,
}

/// Log a tile's rectangle once per placement, so a suite can drag a file onto
/// a drop target (an Inspector asset row, a folder) without reading pixels.
pub fn remember_tile_rect(browser: &mut ProjectBrowser, path: &str, rect: egui::Rect) {
    let corners = [rect.min.x, rect.min.y, rect.max.x, rect.max.y];
    if let Some((_, logged)) = browser
        .tile_rects
        .iter_mut()
        .find(|(logged_path, _)| logged_path == path)
    {
        if *logged == corners {
            return;
        }
        *logged = corners;
    } else {
        browser.tile_rects.push((path.to_owned(), corners));
    }
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        path,
        min_x = rect.min.x,
        min_y = rect.min.y,
        max_x = rect.max.x,
        max_y = rect.max.y,
        "[editor] assets tile rect"
    );
}

/// Log a folder tree row's rectangle once per placement, keyed by the folder,
/// so a suite can right-click the row without reading pixels.
pub fn remember_tree_row_rect(browser: &mut ProjectBrowser, path: &str, rect: egui::Rect) {
    let corners = [rect.min.x, rect.min.y, rect.max.x, rect.max.y];
    if let Some((_, logged)) = browser
        .tree_rects
        .iter_mut()
        .find(|(logged_path, _)| logged_path == path)
    {
        if *logged == corners {
            return;
        }
        *logged = corners;
    } else {
        browser.tree_rects.push((path.to_owned(), corners));
    }
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        path,
        min_x = rect.min.x,
        min_y = rect.min.y,
        max_x = rect.max.x,
        max_y = rect.max.y,
        "[editor] assets tree row rect"
    );
}

/// Log the grid pane's rectangle once per placement: the area a right-click on
/// empty space opens the container menu in.
pub fn remember_pane_rect(browser: &mut ProjectBrowser, rect: egui::Rect) {
    let corners = [rect.min.x, rect.min.y, rect.max.x, rect.max.y];
    if browser.pane_rect == Some(corners) {
        return;
    }
    browser.pane_rect = Some(corners);
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        min_x = rect.min.x,
        min_y = rect.min.y,
        max_x = rect.max.x,
        max_y = rect.max.y,
        "[editor] assets pane rect"
    );
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
    let key = row.key();
    remember_tile_rect(browser, &key, rect);
    let visuals = ui.visuals().clone();
    let selected = writes.selection.asset.as_deref() == Some(key.as_str());
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
    // A finished preview takes the tile's picture area; the type icon stays in
    // the picture's bottom-left corner, so a person still reads what the file
    // is. Until one exists the icon alone stands.
    let preview = writes.previews.get(&key);
    writes.preview_visible.push(key.clone());
    match preview {
        Some(preview) => {
            let size = preview.size_vec2();
            let scale = (icon_rect.width() / size.x).min(icon_rect.height() / size.y);
            let drawn = egui::Rect::from_center_size(icon_rect.center(), size * scale);
            ui.painter().image(
                preview.id(),
                drawn,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            let badge = egui::Rect::from_min_size(
                egui::pos2(drawn.min.x, drawn.max.y - ASSETS_TILE_BADGE_SIZE),
                egui::Vec2::splat(ASSETS_TILE_BADGE_SIZE),
            );
            icons::paint_duotone(ui.painter(), badge, asset_icon(row), tint);
        }
        None => icons::paint_duotone(ui.painter(), icon_rect, asset_icon(row), tint),
    }
    let font = egui::TextStyle::Small.resolve(ui.style());
    let job = tile_name_job(
        browser::file_name(&key),
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
        select_asset(writes.selection, &key);
    }
    if response.double_clicked() && browser::is_folder(row) {
        browser.open(&key);
    }
    // A scene tile opens on the second click, the way a document does
    // everywhere else; the command is the File menu's, so both reach one
    // implementation.
    if response.double_clicked()
        && !browser::is_folder(row)
        && row.path.to_ascii_lowercase().ends_with(".scene")
    {
        let command = pill_editor_core::editor_state::EditorCommand::SceneOpen {
            path: row.path.clone(),
        };
        writes.pending.push(vec![command]);
    }
    if response.secondary_clicked() {
        select_asset(writes.selection, &key);
    }
    // A folder offers what a container is for; a file offers the two edits the
    // panel has for an asset. A module's resource opens no menu at all: nothing
    // in it may be written, and it is no folder to open either.
    if browser::is_folder(row) || !row.read_only {
        context_menu(
            "assets tile",
            &response,
            writes.overlay_regions,
            &mut |ui| {
                if browser::is_folder(row) {
                    asset_container_menu(
                        ui,
                        snapshot,
                        writes.selection,
                        writes.dialog,
                        writes.pending,
                        &key,
                    );
                } else {
                    asset_menu_items(
                        ui,
                        snapshot,
                        writes.selection,
                        writes.dialog,
                        writes.pending,
                    );
                }
            },
        );
    }
    // Folders are drop targets (a tile lands inside one) and every tile is a
    // drag source, so a file moves by dragging it onto a folder. A module's
    // folder takes nothing: nothing may be written into it.
    if browser::is_folder(row) {
        if let Some(dragged) = response.dnd_release_payload::<String>() {
            if !row.read_only {
                if let Some(to) = assets::drop_destination(&dragged, &key) {
                    assets::queue_move(writes.pending, &dragged, &to);
                }
            }
        }
    }
    // A module's resource is not a drag source either: the payload names a
    // key no command may act on.
    if !row.read_only {
        response.dnd_set_drag_payload(key.clone());
    }
    // A file dragged past the window edge is the OS's: the
    // tile reports the path once, and the executable starts an OLE drag with
    // it. The pointer's release never arrives once it leaves the windows, so
    // "button still down, no pointer position" is what a drag out looks like.
    if response.dragged() && writes.drag_out.is_none() && !browser::is_folder(row) {
        let left_window =
            ui.input(|input| input.pointer.any_down() && input.pointer.hover_pos().is_none());
        if left_window {
            *writes.drag_out = Some(row.path.clone());
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                path = row.path.as_str(),
                "[editor] drag out requested"
            );
        }
    }
    response.on_hover_text(key.as_str());
}

/// The icon a row's tile draws: folders, the project's asset types by their
/// last type-name segment, and plain files by their extension.
pub fn asset_icon(row: &AssetListing) -> icons::Icon {
    if browser::is_folder(row) {
        return icons::Icon::Folder;
    }
    let type_icon = asset_type_icon(&row.kind);
    if !matches!(type_icon, icons::Icon::File) {
        return type_icon;
    }
    let extension = row
        .path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "bmp" | "tga" | "hdr" => icons::Icon::Image,
        "obj" | "gltf" | "glb" => icons::Icon::Cube,
        "hlsl" | "wgsl" | "glsl" | "frag" | "vert" | "comp" => icons::Icon::Code,
        _ => icons::Icon::File,
    }
}

/// The icon an asset type draws, by its last type-name segment: what a tile
/// of that type shows, and what an Inspector asset row puts before the
/// asset's name.
pub fn asset_type_icon(asset_type: &str) -> icons::Icon {
    match asset_type.rsplit("::").next().unwrap_or(asset_type) {
        "Mesh" => icons::Icon::Cube,
        "Texture" => icons::Icon::Image,
        "Material" => icons::Icon::Sphere,
        "Shader" => icons::Icon::Code,
        _ => icons::Icon::File,
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

/// The menu a file tile offers: renaming it and deleting it, the two things
/// the panel does to an asset. Creating and opening a folder belong to a
/// container's menu ([`asset_container_menu`]), so a file's menu never carries
/// them.
pub fn asset_menu_items(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    selection: &EditorSelection,
    dialog: &mut Option<AssetDialog>,
    pending: &mut PendingEditorCommands,
) {
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
}

/// The menu a container offers - the grid's empty strip and a folder tile: the
/// two things a folder is for, making an asset in it and opening it in the
/// platform's file manager.
///
/// A module's folder is opened but not written into: its resources are shown
/// and inspected, never created beside.
pub fn asset_container_menu(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    selection: &EditorSelection,
    dialog: &mut Option<AssetDialog>,
    pending: &mut PendingEditorCommands,
    folder: &str,
) {
    if !assets::is_read_only_key(&snapshot.assets, folder)
        && ui
            .add(icons::IconButton::new(
                icons::Icon::Plus,
                "Create New Asset...",
            ))
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
    if ui
        .add(icons::IconButton::new(
            icons::Icon::FolderOpen,
            "Open Folder in File Explorer",
        ))
        .clicked()
    {
        pending.reveal(folder.to_owned());
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            path = folder,
            "[editor] reveal queued"
        );
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
///
/// Its rectangle joins the frame's overlay regions while it is open:
/// a click on a dialog button or in its name field is the dialog's, never a
/// scene pick behind it.
pub fn asset_dialog_window(
    context: &egui::Context,
    dialog: &mut Option<AssetDialog>,
    pending: &mut PendingEditorCommands,
    snapshot: &EditorSnapshot,
    overlay_regions: &mut Vec<ShellRect>,
) {
    let Some(current) = dialog.as_ref() else {
        return;
    };
    // An existing name asks first: the same window gains a warning and the
    // keep-the-guid checkbox, and its submit reads Replace.
    let collision = assets::create_collision(current, &snapshot.standalone_types, &snapshot.assets);
    let (title, submit_label) = match current {
        AssetDialog::Create { .. } if collision.is_some() => ("Replace Asset", "Replace"),
        AssetDialog::Create { .. } => ("Create Asset", "Create"),
        AssetDialog::Rename { .. } => ("Rename Asset", "Rename"),
    };
    let mut submit = false;
    let mut cancel = false;
    let window = egui::Window::new(title)
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
                    kind,
                    overwrite,
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
                    if *kind != assets::CreateKind::Asset {
                        ui.label(
                            egui::RichText::new(match kind {
                                assets::CreateKind::EmptyScene => {
                                    "Creates an empty scene and opens it."
                                }
                                assets::CreateKind::SaveWorld => {
                                    "Writes the current world to this file."
                                }
                                assets::CreateKind::Asset => "",
                            })
                            .weak()
                            .italics(),
                        );
                    }
                    if let Some(path) = collision.as_deref() {
                        ui.separator();
                        ui.label(
                            egui::RichText::new(format!("{path} already exists."))
                                .color(style::warning()),
                        );
                        let mut keep = overwrite.unwrap_or(true);
                        // Confirming the warning is the answer to it. The
                        // checkbox only moves the choice away from the
                        // keep-the-guid default; leaving `overwrite` unset
                        // would send the drain the same unanswered refusal
                        // the warning exists to avoid.
                        if overwrite.is_none() {
                            *overwrite = Some(keep);
                        }
                        if ui
                            .checkbox(&mut keep, "Keep the existing guid")
                            .on_hover_text(
                                "Other assets that reference this one stay valid. Untick to write a fresh guid.",
                            )
                            .changed()
                        {
                            *overwrite = Some(keep);
                        }
                    }
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
    if let Some(window) = window {
        claim_pointer(window.response.rect, overlay_regions);
    }
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
