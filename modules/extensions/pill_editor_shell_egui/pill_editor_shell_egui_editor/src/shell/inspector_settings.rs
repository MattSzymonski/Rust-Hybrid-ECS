//! The Inspector's asset import settings.
//!
//! # Responsibilities
//!
//! - `asset_settings_rows` and `asset_settings_row`: the selected asset's
//!   settings as a recursive walk of the shared rows - a container is a
//!   heading over its children one level in, a leaf is one label/control row.
//! - `settings_row_label`, `settings_heading` and `settings_level`: the one
//!   naming rule the walk and the reference rows share, so `parameters.pbr_base.Color`
//!   draws as a `parameters` heading, a `pbr_base` heading and a `Color` row.
//! - `reference_row`: one row per reference field the asset's type declares -
//!   the same row for a material's shader and its texture slots, driven by the
//!   snapshot's answers rather than by knowing a renderer type.
//! - `reference_list_rows` and `reference_list_entry_row`: a reference family
//!   as a list a person reorders by dragging its rows - a chain's pass list.
//! - `settings_commit`: the recorded command one committed settings value
//!   queues.
//! - The colour parameter shape (`parameters.<slot>.Color`, an array of
//!   numbers) drawn as a swatch.
//!
//! # Design
//!
//! Every settings edit is one recorded `SetAssetSettings` step carrying the
//! whole patched document, so undo and the file's round trip are unchanged
//! whatever row wrote it. A row that references another asset writes that
//! asset's **guid**, which is what the file holds: each listing row carries
//! its own guid for exactly this. The first write is what creates a slot the
//! document does not hold yet.
//!
//! The globs below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// The indent one nesting level of a settings document adds, in points: the
/// whole row moves right, so a container's children read as belonging to it
/// while the label column stays the width every level uses.
pub const SETTINGS_ROW_INDENT: f32 = 12.0;

/// The label a settings row carries: the leaf of its key, because the path is
/// what the headings above the row already spell out and a truncated
/// `parameters...` names nothing.
pub fn settings_row_label(key: &str) -> String {
    key.rsplit_once('.')
        .map_or(key, |(_, leaf)| leaf)
        .to_owned()
}

/// A settings container's heading: its leaf name, muted, with the whole key as
/// its tooltip, over the rows one level in.
pub fn settings_heading(ui: &mut egui::Ui, key: &str) {
    let name = settings_row_label(key);
    ui.label(egui::RichText::new(name).color(style::text_muted()))
        .on_hover_text(key);
}

/// Draw `add_contents` one settings level in.
///
/// The step is this module's own rather than the panel's indent, which is
/// deeper than a short settings key needs.
pub fn settings_level(ui: &mut egui::Ui, add_contents: &mut dyn FnMut(&mut egui::Ui)) {
    let previous = ui.spacing().indent;
    ui.spacing_mut().indent = SETTINGS_ROW_INDENT;
    ui.indent("pill.settings", |ui| add_contents(ui));
    ui.spacing_mut().indent = previous;
}

/// The settings document's rows: objects and arrays nest their children,
/// every scalar leaf edits - unless `read_only`, when every row reads a module
/// resource that nothing may write.
///
/// `references` are the reference fields the asset's type declares: each gets a
/// picker row, whether or not the document binds it yet, and the document key
/// behind it is left out of the walk so one field has one row.
#[allow(clippy::too_many_arguments)]
pub fn asset_settings_rows(
    ui: &mut egui::Ui,
    asset_path: &str,
    settings: &JsonValue,
    assets: &[AssetListing],
    engine_assets: &[EngineAsset],
    pending: &mut PendingEditorCommands,
    session: &mut InspectorSession,
    overlay_regions: &mut Vec<ShellRect>,
    read_only: bool,
    references: &[AssetReferenceField],
) {
    match settings {
        JsonValue::Object(entries) => {
            for (key, value) in entries {
                // A key a reference field owns is drawn by that field's row -
                // including a list family, whose entries arrive as
                // `passes[0]`, `passes[1]`, ... and whose array would
                // otherwise be walked as raw JSON, guids and all.
                let owned = references.iter().any(|field| {
                    field.key == *key
                        || field.key.starts_with(&format!("{key}."))
                        || reference_family(field) == Some(key.as_str())
                });
                if owned {
                    continue;
                }
                asset_settings_row(
                    ui,
                    asset_path,
                    settings,
                    value,
                    key.clone(),
                    assets,
                    pending,
                    session,
                    overlay_regions,
                    read_only,
                );
            }
            if read_only {
                return;
            }
            for field in references {
                // A list entry is drawn by its family's block, which owns the
                // row's index, its drag handle and its remove button.
                if reference_family(field).is_some() {
                    continue;
                }
                reference_row(
                    ui,
                    asset_path,
                    settings,
                    field,
                    assets,
                    engine_assets,
                    pending,
                    session,
                    overlay_regions,
                );
            }
            reference_list_rows(
                ui,
                asset_path,
                settings,
                references,
                assets,
                engine_assets,
                pending,
                session,
                overlay_regions,
            );
        }
        other => {
            read_only_row(ui, "settings", &other.to_string());
        }
    }
}

/// The width the remove button takes at the end of a list row, in points.
pub const LIST_REMOVE_WIDTH: f32 = 18.0;

/// What a list entry carries while it is dragged: the family it belongs to and
/// the index it left.
#[derive(Clone, Debug)]
pub struct ListEntryDrag {
    /// The settings key of the list it belongs to - `passes` for a chain's pass
    /// list.
    pub family: String,
    /// The entry's index in that list when the drag began.
    pub index: usize,
}

/// The list families a reference field set names - `passes[0]`, `passes[1]`,
/// ... - as one editable list each: a draggable row per entry, and the picker
/// that appends the next one.
///
/// A chain's pass list is the first such family. A drag reorders the list, and
/// a drag or a remove rewrites the whole array in one recorded step, so undo
/// takes back that one change and not the file.
#[allow(clippy::too_many_arguments)]
pub fn reference_list_rows(
    ui: &mut egui::Ui,
    asset_path: &str,
    root: &JsonValue,
    references: &[AssetReferenceField],
    assets: &[AssetListing],
    engine_assets: &[EngineAsset],
    pending: &mut PendingEditorCommands,
    session: &mut InspectorSession,
    overlay_regions: &mut Vec<ShellRect>,
) {
    let mut families: Vec<(String, String)> = references
        .iter()
        .filter_map(|field| {
            let family = reference_family(field)?;
            Some((family.to_owned(), field.target.clone()))
        })
        .collect();
    families.sort();
    families.dedup();
    for (family, target) in families {
        let entries = root
            .get(&family)
            .and_then(|list| list.as_array())
            .cloned()
            .unwrap_or_default();
        let mut rows: Vec<egui::Rect> = Vec::new();
        // The block a drop reads its insertion point from: the heading, the
        // list's own rows and the row that appends one, so a release anywhere
        // on them lands - including on a picker button.
        let block = ui
            .scope(|ui| {
                ui.separator();
                ui.label(
                    egui::RichText::new(format!("{family} ({})", entries.len()))
                        .color(style::text_muted()),
                )
                .on_hover_text("Drag a row to reorder the list");
                for index in 0..entries.len() {
                    let field =
                        list_entry_field(references, &family, index, &entries[index], &target);
                    rows.push(reference_list_entry_row(
                        ui,
                        asset_path,
                        root,
                        &family,
                        &entries,
                        index,
                        &field,
                        assets,
                        engine_assets,
                        pending,
                        session,
                        overlay_regions,
                    ));
                }
                // Appending is the picker of the entry one past the end: the
                // inserting patch is what creates it.
                let append = AssetReferenceField {
                    key: format!("{family}[{}]", entries.len()),
                    target: target.clone(),
                    guid: None,
                    nested: true,
                    label: None,
                };
                list_append_row(
                    ui,
                    asset_path,
                    root,
                    &append,
                    assets,
                    engine_assets,
                    pending,
                    session,
                    overlay_regions,
                );
            })
            .response
            .rect;
        list_drag_update(
            ui, &family, &entries, &rows, block, asset_path, root, pending,
        );
    }
}

/// The reference field one list entry draws: the one the snapshot expanded for
/// its key, or - when the document holds an entry the snapshot does not name -
/// one built from the entry itself, so the row still reads and writes.
pub fn list_entry_field(
    references: &[AssetReferenceField],
    family: &str,
    index: usize,
    entry: &JsonValue,
    target: &str,
) -> AssetReferenceField {
    let key = format!("{family}[{index}]");
    if let Some(field) = references.iter().find(|field| field.key == key) {
        return field.clone();
    }
    AssetReferenceField {
        key,
        target: target.to_owned(),
        guid: reference_guid(entry),
        nested: !matches!(entry, JsonValue::String(_)),
        label: None,
    }
}

/// One entry of a reference list: its index behind a drag handle, its picker,
/// and the button that drops it from the list.
///
/// The whole row carries the drag - the handle, and the picker beside it, since
/// a person grabs an entry by its name more often than by the handle - so the
/// row moves wherever it is held, while a press that does not move still opens
/// the picker. The row's rectangle is what a drop turns into an index.
#[allow(clippy::too_many_arguments)]
pub fn reference_list_entry_row(
    ui: &mut egui::Ui,
    asset_path: &str,
    root: &JsonValue,
    family: &str,
    entries: &[JsonValue],
    index: usize,
    field: &AssetReferenceField,
    assets: &[AssetListing],
    engine_assets: &[EngineAsset],
    pending: &mut PendingEditorCommands,
    session: &mut InspectorSession,
    overlay_regions: &mut Vec<ShellRect>,
) -> egui::Rect {
    let key = field.key.as_str();
    let drag = ListEntryDrag {
        family: family.to_owned(),
        index,
    };
    let text = reference_text(field);
    // The row takes exactly the width it has, so the rows below it keep the
    // panel's width: a run that sizes itself by its children comes back one
    // item spacing wider each row, and the list drifts out of the panel.
    let row_width = ui.available_width();
    let picker_width = list_picker_width(ui, row_width);
    let mut chosen: Option<Option<String>> = None;
    let mut button_rect = None;
    let mut remove = false;
    let response = ui
        .allocate_ui_with_layout(
            egui::vec2(row_width, ROW_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                // The handle names the row's place and is the affordance that says
                // the row moves; the drag payload it carries is what the block reads
                // back on release.
                let handle = ui
                    .add_sized(
                        egui::vec2(WIDE_ROW_LABEL_WIDTH, ROW_HEIGHT),
                        egui::Label::new(
                            icons::rich(icons::Icon::ArrowsOutCardinal, &index.to_string())
                                .color(style::text_muted()),
                        )
                        .truncate(),
                    )
                    .interact(egui::Sense::drag());
                handle.dnd_set_drag_payload(drag.clone());
                handle.on_hover_text("Drag to reorder the list");
                label_gap(ui);
                let button = ui.add_sized(
                    egui::vec2(picker_width, ROW_HEIGHT),
                    egui::Button::new(icons::rich(
                        assets_tiles::asset_type_icon(&field.target),
                        &text,
                    ))
                    .truncate(),
                );
                let button = button.interact(egui::Sense::click_and_drag());
                button.dnd_set_drag_payload(drag);
                if let Some(dropped) = dropped_asset_path(&button) {
                    chosen = assets
                        .iter()
                        .find(|row| row.key() == dropped)
                        .and_then(|row| row.guid.clone())
                        .map(Some);
                }
                if button.clicked() {
                    session.settings_picker = Some(key.to_string());
                    session.settings_search.clear();
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ENGINE,
                        slot = key,
                        target = field.target.as_str(),
                        "[editor] settings reference picker"
                    );
                }
                button_rect = Some(button.rect);
                if ui
                    .add_sized(
                        egui::vec2(LIST_REMOVE_WIDTH, ROW_HEIGHT),
                        egui::Button::new("x").small(),
                    )
                    .on_hover_text("Remove this entry")
                    .clicked()
                {
                    remove = true;
                }
            },
        )
        .response;
    if session.settings_picker.as_deref() == Some(key) {
        if let Some(rect) = button_rect {
            if let Some(reference) = reference_picker(
                ui,
                field,
                assets,
                engine_assets,
                pending,
                session,
                overlay_regions,
                rect,
            ) {
                session.settings_picker = None;
                chosen = Some(reference);
            }
        }
    }
    if remove {
        let mut fewer = entries.to_vec();
        fewer.remove(index);
        settings_commit(pending, asset_path, root, family, JsonValue::Array(fewer));
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            slot = key,
            "[editor] settings list entry removed"
        );
    }
    if let Some(reference) = chosen {
        let value = match reference {
            Some(guid) if field.nested => serde_json::json!({ "guid": guid }),
            Some(guid) => JsonValue::String(guid),
            None => JsonValue::Null,
        };
        settings_commit(pending, asset_path, root, key, value);
    }
    remember_list_row_rect(session, asset_path, family, index, response.rect);
    response.rect
}

/// The row that appends an entry: the same picker one past the end, whose
/// choice is what creates the entry.
///
/// Its label column stays empty so the button lines up with the rows above it,
/// and it takes a file dragged from the Assets tab like any other picker.
#[allow(clippy::too_many_arguments)]
pub fn list_append_row(
    ui: &mut egui::Ui,
    asset_path: &str,
    root: &JsonValue,
    field: &AssetReferenceField,
    assets: &[AssetListing],
    engine_assets: &[EngineAsset],
    pending: &mut PendingEditorCommands,
    session: &mut InspectorSession,
    overlay_regions: &mut Vec<ShellRect>,
) {
    let mut chosen: Option<Option<String>> = None;
    let mut button_rect = None;
    let row_width = ui.available_width();
    let picker_width = list_picker_width(ui, row_width);
    ui.allocate_ui_with_layout(
        egui::vec2(row_width, ROW_HEIGHT),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            // An empty label column keeps the button under the entries' pickers.
            label_cell_in(ui, WIDE_ROW_LABEL_WIDTH, "", None);
            label_gap(ui);
            let button = ui.add_sized(
                egui::vec2(picker_width, ROW_HEIGHT),
                egui::Button::new(icons::rich(icons::Icon::Plus, "Add")),
            );
            if let Some(dropped) = dropped_asset_path(&button) {
                chosen = assets
                    .iter()
                    .find(|row| row.key() == dropped)
                    .and_then(|row| row.guid.clone())
                    .map(Some);
            }
            if button.clicked() {
                session.settings_picker = Some(field.key.clone());
                session.settings_search.clear();
            }
            button_rect = Some(button.rect);
        },
    );
    if session.settings_picker.as_deref() == Some(field.key.as_str()) {
        if let Some(rect) = button_rect {
            if let Some(reference) = reference_picker(
                ui,
                field,
                assets,
                engine_assets,
                pending,
                session,
                overlay_regions,
                rect,
            ) {
                session.settings_picker = None;
                chosen = Some(reference);
            }
        }
    }
    if let Some(reference) = chosen {
        let value = match reference {
            Some(guid) => serde_json::json!({ "guid": guid }),
            None => JsonValue::Null,
        };
        settings_commit(pending, asset_path, root, &field.key, value);
    }
}

/// Turn a held list row into a reorder: while a row of this family is over the
/// block, the insertion point is marked; on release the list is rewritten in one
/// recorded step.
///
/// The drop is read from the drag payload and the pointer rather than from a
/// drop target, so a release anywhere over the block counts - on a picker
/// button, in a gap, or on the row that appends - which is what makes dropping
/// between two rows work.
#[allow(clippy::too_many_arguments)]
pub fn list_drag_update(
    ui: &mut egui::Ui,
    family: &str,
    entries: &[JsonValue],
    rows: &[egui::Rect],
    block: egui::Rect,
    asset_path: &str,
    root: &JsonValue,
    pending: &mut PendingEditorCommands,
) {
    if !block.is_positive() {
        return;
    }
    let Some(dragged) =
        egui::DragAndDrop::payload::<ListEntryDrag>(ui.ctx()).filter(|it| it.family == family)
    else {
        return;
    };
    let Some(pointer) = ui
        .ctx()
        .pointer_latest_pos()
        .filter(|pointer| block.contains(*pointer))
    else {
        return;
    };
    let insert_at = insertion_index(rows, pointer.y);
    // The held row keeps a mark of its own, so the row that is moving stays
    // readable while the line shows where it lands.
    if let Some(source) = rows.get(dragged.index) {
        ui.painter().rect_stroke(
            *source,
            2.0,
            egui::Stroke::new(1.0, style::accent()),
            egui::StrokeKind::Inside,
        );
    }
    let line_y = match rows.get(insert_at) {
        Some(row) => row.top(),
        None => rows.last().map_or(block.bottom(), |row| row.bottom()),
    };
    ui.painter().rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(block.left(), line_y - 1.0),
            egui::pos2(block.right(), line_y + 1.0),
        ),
        0.0,
        style::accent(),
    );
    if !ui.input(|input| input.pointer.any_released()) {
        return;
    }
    let Some(released) = egui::DragAndDrop::take_payload::<ListEntryDrag>(ui.ctx()) else {
        return;
    };
    let moved = moved_entry(entries, released.index, insert_at);
    if moved == entries {
        return;
    }
    settings_commit(pending, asset_path, root, family, JsonValue::Array(moved));
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        family,
        from = released.index,
        to = insert_at,
        "[editor] settings list reordered"
    );
}

/// The insertion point a pointer at `y` names: the first row whose middle the
/// pointer has not passed, or the end of the list.
pub fn insertion_index(rows: &[egui::Rect], y: f32) -> usize {
    rows.iter()
        .position(|row| y < row.center().y)
        .unwrap_or(rows.len())
}

/// The entries with the one at `from` moved to the insertion point `to` (an
/// index in `0..=len`).
///
/// A drop states where the entry lands *between* rows, so a move downwards
/// takes the entry out first and lands one slot earlier than the raw index; an
/// index that does not move the entry leaves the list as it was.
pub fn moved_entry(entries: &[JsonValue], from: usize, to: usize) -> Vec<JsonValue> {
    let mut moved = entries.to_vec();
    if from >= moved.len() {
        return moved;
    }
    let entry = moved.remove(from);
    let insert_at = if to > from { to - 1 } else { to }.min(moved.len());
    moved.insert(insert_at, entry);
    moved
}

/// The width a list row's picker takes: the row's width less its label column,
/// the gap after it, its remove button and the spacing around them.
///
/// The trailing spacing is egui's: a row's own extent includes the gap its last
/// control leaves, so a row sized without it comes back one gap wider and pushes
/// every row below it out of the panel.
pub fn list_picker_width(ui: &egui::Ui, row_width: f32) -> f32 {
    let spacing = ui.spacing().item_spacing.x;
    (row_width - WIDE_ROW_LABEL_WIDTH - ROW_LABEL_GAP - LIST_REMOVE_WIDTH - 2.0 * spacing).max(60.0)
}

/// Log a list row's rectangle once per placement, keyed `asset:family[index]`,
/// so a suite can aim a drag at it.
///
/// The placement is recorded either way - the shell's own tests read it - and
/// the line is a scripted run's, for the same reason every other row rectangle
/// is.
pub fn remember_list_row_rect(
    session: &mut InspectorSession,
    asset_path: &str,
    family: &str,
    index: usize,
    rect: egui::Rect,
) {
    let key = format!("{asset_path}:{family}[{index}]");
    let Some(rect) =
        inspector_fields::record_row_placement(&mut session.settings_row_rects, &key, rect)
    else {
        return;
    };
    if !session.log_rects {
        return;
    }
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        row = key.as_str(),
        min_x = rect.min.x,
        min_y = rect.min.y,
        max_x = rect.max.x,
        max_y = rect.max.y,
        "[editor] settings list row rect"
    );
}

/// One settings row: a leaf gets the widget its JSON kind asks for (its
/// commit carries the whole patched document), a container becomes a heading
/// over its children one level in - except the two shapes that get a control
/// of their own, a texture slot and a colour parameter.
///
/// A leaf draws from its key and its value alone; the row's assets, session
/// and overlay regions exist for the rows below it, so the attributes are the
/// recursion's own shape rather than an unused argument.
#[allow(clippy::too_many_arguments, clippy::only_used_in_recursion)]
pub fn asset_settings_row(
    ui: &mut egui::Ui,
    asset_path: &str,
    root: &JsonValue,
    value: &JsonValue,
    key: String,
    assets: &[AssetListing],
    pending: &mut PendingEditorCommands,
    session: &mut InspectorSession,
    overlay_regions: &mut Vec<ShellRect>,
    read_only: bool,
) {
    // The colour parameter is the one array drawn as a control of its own, so
    // it is claimed before the container shapes below take the value.
    if !read_only {
        if let Some(channels) = colour_parameter(&key, value) {
            colour_parameter_row(ui, asset_path, root, &key, channels, pending);
            return;
        }
    }
    match value {
        JsonValue::Object(entries) => {
            settings_heading(ui, &key);
            settings_level(ui, &mut |ui| {
                for (child_key, child) in entries {
                    asset_settings_row(
                        ui,
                        asset_path,
                        root,
                        child,
                        format!("{key}.{child_key}"),
                        assets,
                        pending,
                        session,
                        overlay_regions,
                        read_only,
                    );
                }
            });
            return;
        }
        JsonValue::Array(elements) => {
            settings_heading(ui, &key);
            settings_level(ui, &mut |ui| {
                for (index, child) in elements.iter().enumerate() {
                    asset_settings_row(
                        ui,
                        asset_path,
                        root,
                        child,
                        format!("{key}[{index}]"),
                        assets,
                        pending,
                        session,
                        overlay_regions,
                        read_only,
                    );
                }
            });
            return;
        }
        _ => {}
    }
    // A leaf: the shared row, its label the leaf of the key.
    let label = settings_row_label(&key);
    if read_only {
        read_only_row(ui, &label, &value.to_string());
        return;
    }
    match value {
        JsonValue::Bool(current) => {
            let mut edited = *current;
            let mut changed = false;
            row_in(ui, WIDE_ROW_LABEL_WIDTH, &label, Some(&key), &mut |ui| {
                changed = ui.checkbox(&mut edited, "").changed();
            });
            if changed {
                settings_commit(pending, asset_path, root, &key, JsonValue::Bool(edited));
            }
        }
        JsonValue::Number(current) => {
            let mut integer_edit = current
                .as_i64()
                .or_else(|| current.as_u64().map(|value| value as i64));
            match integer_edit.as_mut() {
                Some(edited) => {
                    let mut changed = false;
                    row_in(ui, WIDE_ROW_LABEL_WIDTH, &label, Some(&key), &mut |ui| {
                        changed = ui.add(egui::DragValue::new(edited)).changed();
                    });
                    if changed {
                        settings_commit(
                            pending,
                            asset_path,
                            root,
                            &key,
                            JsonValue::Number((*edited).into()),
                        );
                    }
                }
                None => {
                    let mut edited = current.as_f64().unwrap_or_default();
                    let mut changed = false;
                    row_in(ui, WIDE_ROW_LABEL_WIDTH, &label, Some(&key), &mut |ui| {
                        changed = ui
                            .add(
                                egui::DragValue::new(&mut edited)
                                    .speed(0.05)
                                    .max_decimals(9),
                            )
                            .changed();
                    });
                    if changed {
                        if let Some(number) = serde_json::Number::from_f64(edited) {
                            settings_commit(
                                pending,
                                asset_path,
                                root,
                                &key,
                                JsonValue::Number(number),
                            );
                        }
                    }
                }
            }
        }
        JsonValue::String(current) => {
            let mut edited = current.clone();
            let mut commit = false;
            row_in(ui, WIDE_ROW_LABEL_WIDTH, &label, Some(&key), &mut |ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut edited).desired_width(ui.available_width()),
                );
                commit = response.lost_focus() && edited != *current;
            });
            if commit {
                settings_commit(pending, asset_path, root, &key, JsonValue::String(edited));
            }
        }
        other => {
            read_only_row(ui, &label, &other.to_string());
        }
    }
}

/// The channel values of a colour parameter, when `key` names one: a `Color`
/// leaf holding three or four numbers (the material document's
/// `parameters.<slot>.Color`).
pub fn colour_parameter(key: &str, value: &JsonValue) -> Option<Vec<f32>> {
    let leaf = key.rsplit('.').next().unwrap_or(key);
    if !leaf.eq_ignore_ascii_case("Color") {
        return None;
    }
    let JsonValue::Array(elements) = value else {
        return None;
    };
    if !(3..=4).contains(&elements.len()) {
        return None;
    }
    elements
        .iter()
        .map(|element| element.as_f64().map(|number| number as f32))
        .collect()
}

/// One colour parameter: a swatch whose popup holds the picker. A commit
/// writes the whole array back, as one recorded settings step.
pub fn colour_parameter_row(
    ui: &mut egui::Ui,
    asset_path: &str,
    root: &JsonValue,
    key: &str,
    channels: Vec<f32>,
    pending: &mut PendingEditorCommands,
) {
    let mut edited = [1.0f32; 4];
    for (channel, value) in edited.iter_mut().zip(&channels) {
        *channel = *value;
    }
    let label = settings_row_label(key);
    let mut picked = None;
    row_in(ui, WIDE_ROW_LABEL_WIDTH, &label, Some(key), &mut |ui| {
        if ui
            .color_edit_button_rgba_unmultiplied(&mut edited)
            .changed()
        {
            picked = Some(edited);
        }
    });
    if let Some(edited) = picked {
        let numbers: Option<Vec<JsonValue>> = channels
            .iter()
            .enumerate()
            .map(|(index, _)| {
                serde_json::Number::from_f64(edited[index] as f64).map(JsonValue::Number)
            })
            .collect();
        if let Some(numbers) = numbers {
            settings_commit(pending, asset_path, root, key, JsonValue::Array(numbers));
        }
    }
}

/// The path an asset dragged from the Assets tab releases onto `response`, when
/// one is being dragged.
///
/// The type is checked first because egui's release helper *takes* the payload
/// it finds: a picker the pointer happens to be over would otherwise swallow a
/// drag of any other kind - a list row moving to a new index, for one.
pub fn dropped_asset_path(response: &egui::Response) -> Option<String> {
    if !egui::DragAndDrop::has_payload_of_type::<String>(&response.ctx) {
        return None;
    }
    response
        .dnd_release_payload::<String>()
        .map(|path| (*path).clone())
}

/// The guid a settings reference holds, in either spelling: the value itself
/// when the document writes a bare guid, or the object form's `guid`.
pub fn reference_guid(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::String(guid) => Some(guid.clone()),
        JsonValue::Object(entries) => entries
            .get("guid")
            .and_then(|guid| guid.as_str())
            .map(str::to_owned),
        _ => None,
    }
}

/// The listing's rows a reference's picker offers: the assets whose type is the
/// one the registry named for the field.
pub fn reference_candidates<'a>(assets: &'a [AssetListing], target: &str) -> Vec<&'a AssetListing> {
    inspector::picker_candidates(assets, &[], Some(target)).0
}

/// The list family a reference field's key belongs to: `passes[3]` is an entry
/// of `passes`, and a key without brackets is its own field.
pub fn reference_family(field: &AssetReferenceField) -> Option<&str> {
    field.key.split_once('[').map(|(family, _)| family)
}

/// What a reference row reads: the label the executable resolved - a listing
/// path, or the name the store loaded the asset under when it has no file - and
/// only a guid's head when neither knows it.
pub fn reference_text(field: &AssetReferenceField) -> String {
    match (field.label.as_deref(), field.guid.as_deref()) {
        (Some(label), _) => label.to_owned(),
        (None, Some(guid)) => format!("{}...", guid.chars().take(8).collect::<String>()),
        (None, None) => "(none)".to_string(),
    }
}

/// The listing path of the asset a reference names, when the listing holds it:
/// what a picker marks as the current choice.
pub fn reference_path<'a>(
    field: &AssetReferenceField,
    assets: &'a [AssetListing],
) -> Option<&'a str> {
    let guid = field.guid.as_deref()?;
    assets
        .iter()
        .find(|row| row.guid.as_deref() == Some(guid))
        .map(|row| row.path.as_str())
}

/// One reference field: the referenced asset's path (or its guid, when the
/// listing no longer holds it), a clear button, a drop target and a picker over
/// the rows of the type the field accepts.
///
/// Every choice writes the chosen asset's guid in the shape the document
/// already uses - the object form for a named reference, the bare string for a
/// texture slot - and a field the document does not hold yet writes the object
/// form. The same row serves a material's shader and its declared slots,
/// because what differs between them is data; a list entry draws through
/// [`reference_list_entry_row`], which keeps this row's picker.
#[allow(clippy::too_many_arguments)]
pub fn reference_row(
    ui: &mut egui::Ui,
    asset_path: &str,
    root: &JsonValue,
    field: &AssetReferenceField,
    assets: &[AssetListing],
    engine_assets: &[EngineAsset],
    pending: &mut PendingEditorCommands,
    session: &mut InspectorSession,
    overlay_regions: &mut Vec<ShellRect>,
) {
    let key = field.key.as_str();
    let label = settings_row_label(key);
    let text = reference_text(field);
    let mut chosen: Option<Option<String>> = None;
    let mut button_rect = None;
    row_in(ui, WIDE_ROW_LABEL_WIDTH, &label, Some(key), &mut |ui| {
        // The picker takes what the row has left beside its clear button, so
        // the two stay on one line however narrow the panel is: a wrapped run
        // would drop the button, and then the clear button, onto rows of their
        // own.
        let clear_width = inspector_fields::ASSET_ROW_CLEAR_WIDTH;
        let spacing = ui.spacing().item_spacing.x;
        let button_width = (ui.available_width() - clear_width - spacing).max(60.0);
        let response = ui.add_sized(
            egui::vec2(button_width, ROW_HEIGHT),
            egui::Button::new(icons::rich(icons::Icon::Code, &text)).truncate(),
        );
        if let Some(dropped) = dropped_asset_path(&response) {
            chosen = assets
                .iter()
                .find(|row| row.key() == dropped)
                .and_then(|row| row.guid.clone())
                .map(Some);
        }
        if response.clicked() {
            session.settings_picker = Some(key.to_string());
            session.settings_search.clear();
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                slot = key,
                target = field.target.as_str(),
                "[editor] settings reference picker"
            );
        }
        button_rect = Some(response.rect);
        let clear = ui
            .add_enabled_ui(field.guid.is_some(), |ui| {
                ui.add_sized(
                    egui::vec2(clear_width, ROW_HEIGHT),
                    egui::Button::new("x").small(),
                )
            })
            .inner
            .on_hover_text("Clear the reference");
        if clear.clicked() {
            chosen = Some(None);
        }
    });

    if session.settings_picker.as_deref() == Some(key) {
        if let Some(rect) = button_rect {
            if let Some(reference) = reference_picker(
                ui,
                field,
                assets,
                engine_assets,
                pending,
                session,
                overlay_regions,
                rect,
            ) {
                session.settings_picker = None;
                chosen = Some(reference);
            }
        }
    }

    if let Some(reference) = chosen {
        session.settings_picker = None;
        let value = match reference {
            Some(guid) if field.nested => serde_json::json!({ "guid": guid }),
            Some(guid) => JsonValue::String(guid),
            None => JsonValue::Null,
        };
        settings_commit(pending, asset_path, root, key, value);
    }
}

/// The picker a reference opens under its row: every asset of the field's
/// target type the listing holds, then the store's own entries, with the focus
/// affordance that shows a candidate in the Assets panel.
///
/// Returns the choice made this frame - `Some(None)` clears the reference,
/// `Some(Some(guid))` points it somewhere - and leaves the session's picker
/// open until the caller takes the choice. A list entry's picker offers no
/// `None`: an entry leaves a list through its row's remove button, not by
/// being emptied in place.
#[allow(clippy::too_many_arguments)]
pub fn reference_picker(
    ui: &mut egui::Ui,
    field: &AssetReferenceField,
    assets: &[AssetListing],
    engine_assets: &[EngineAsset],
    pending: &mut PendingEditorCommands,
    session: &mut InspectorSession,
    overlay_regions: &mut Vec<ShellRect>,
    anchor: egui::Rect,
) -> Option<Option<String>> {
    let key = field.key.as_str();
    let current_path = reference_path(field, assets);
    let clearable = reference_family(field).is_none();
    let mut chosen: Option<Option<String>> = None;
    let picker_id = egui::Id::new(("pill_editor_shell_egui.reference_picker", key.to_string()));
    let area = floating_popup(
        ui,
        picker_id,
        anchor,
        anchor.width().max(180.0),
        overlay_regions,
        &mut |ui| {
            egui::ScrollArea::vertical()
                .max_height(inspector_fields::ASSET_PICKER_HEIGHT)
                .show(ui, |ui| {
                    if clearable && ui.selectable_label(field.guid.is_none(), "None").clicked() {
                        chosen = Some(None);
                    }
                    let (files, built) =
                        inspector::picker_candidates(assets, engine_assets, Some(&field.target));
                    for candidate in files {
                        let current = current_path == Some(candidate.path.as_str());
                        ui.horizontal(|ui| {
                            // A module's own resource: shown so a person
                            // sees what exists, but an import setting of
                            // a read-only resource is not this project's
                            // to write, so the row explains itself
                            // instead of taking the click.
                            let from_module = !candidate.root.is_empty();
                            if from_module {
                                engine_badge(ui);
                            }
                            let pickable = candidate.guid.is_some();
                            let row = ui.add_enabled_ui(pickable, |ui| {
                                ui.selectable_label(current, &candidate.path).clicked()
                            });
                            if row.inner {
                                chosen = candidate.guid.clone().map(Some);
                            }
                            if !pickable {
                                row.response.on_hover_text(
                                    "Shipped with the engine, so its settings are read-only",
                                );
                            }
                            // The focus affordance: a single click shows the
                            // candidate in the Assets panel - the selection
                            // moves and its folder opens - while the picker
                            // stays open, so browsing does not throw the edit
                            // away; a double click also makes it the
                            // Inspector's subject and closes the picker.
                            let focus = ui
                                .small_button("o")
                                .on_hover_text("Show this asset in the Assets panel");
                            if focus.double_clicked() {
                                pending.focus_asset(candidate.key());
                                session.settings_picker = None;
                                pill_core::info!(
                                    target: pill_core::telemetry::telemetry_target::ENGINE,
                                    path = candidate.path.as_str(),
                                    "[editor] asset focused"
                                );
                            } else if focus.clicked() {
                                pending.focus_asset(candidate.key());
                            }
                        });
                    }
                    // The store's own assets: they have no file, so no
                    // folder to reveal, and choosing one writes its
                    // guid - or the name it was added under, which the
                    // field write resolves through the store.
                    for candidate in built {
                        let current = field.guid.as_deref() == candidate.guid.as_deref();
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
    // Any press outside the picker and its button closes it, whatever
    // button it was: a picker that only closes by picking fights the
    // person who changed their mind. The press that opens the picker is
    // on the button, so it never closes what it just opened.
    let press = ui.input(|input| (input.pointer.any_pressed(), input.pointer.interact_pos()));
    if let (true, Some(at)) = press {
        if !anchor.contains(at) && !area.response.rect.contains(at) {
            session.settings_picker = None;
        }
    }
    chosen
}

/// The guid a dropped asset writes: the listing row's own, or `None` when the
/// path names no asset that carries one.
///
/// The dropped payload is the row's key, so a module row is found the same way
/// a project one is.
pub fn texture_guid_for(assets: &[AssetListing], path: &str) -> Option<String> {
    assets
        .iter()
        .find(|row| row.key() == path)
        .and_then(|row| row.guid.clone())
}

/// Queue the recorded edit that puts `value` at `key` in the settings; the
/// command carries the whole patched document.
pub fn settings_commit(
    pending: &mut PendingEditorCommands,
    asset_path: &str,
    root: &JsonValue,
    key: &str,
    value: JsonValue,
) {
    let Ok(path) = inspector::parse_settings_key_path(key) else {
        return;
    };
    let mut document = root.clone();
    // The inserting rule: a slot or a reference the type declares but the file
    // does not hold yet is created by its first write, which is exactly the
    // unbound-texture case.
    if let Err(message) = inspector::patch_settings_inserting(&mut document, &path, value) {
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ECS,
            key,
            message = message.as_str(),
            "[editor] asset settings edit refused"
        );
        return;
    }
    let (command, label) = inspector::settings_edit(asset_path, document, key);
    pending.push_recorded(
        vec![command],
        label,
        Some(inspector::settings_coalesce_key(asset_path, key)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, kind: &str, guid: Option<&str>) -> AssetListing {
        AssetListing {
            root: String::new(),
            path: path.to_string(),
            read_only: false,
            kind: kind.to_string(),
            standalone: true,
            has_meta: guid.is_some(),
            guid: guid.map(str::to_owned),
        }
    }

    /// A reference reads both spellings the documents use: the object form this
    /// build writes, and the bare guid every older file holds.
    #[test]
    fn a_reference_reads_both_spellings() {
        assert_eq!(
            reference_guid(&serde_json::json!({ "guid": "abc" })).as_deref(),
            Some("abc")
        );
        assert_eq!(
            reference_guid(&serde_json::json!("abc")).as_deref(),
            Some("abc")
        );
        assert_eq!(reference_guid(&serde_json::Value::Null), None);
    }

    /// A picker offers the type its field accepts, and nothing else: the same
    /// filter serves a shader reference, a texture slot and a pass entry.
    #[test]
    fn a_reference_lists_the_type_its_field_accepts() {
        let assets = vec![
            row(
                "materials/a.material",
                "pill_master_renderer::assets::Material",
                Some("m"),
            ),
            row(
                "shaders/pbr.hlsl",
                "pill_master_renderer::assets::Shader",
                Some("s"),
            ),
            row(
                "textures/rock.png",
                "pill_master_renderer::assets::Texture",
                Some("t"),
            ),
            row(
                "passes/opaque.render_pass",
                "pill_master_renderer::assets::RenderPass",
                Some("p"),
            ),
        ];
        let shaders = |target: &str| -> Vec<String> {
            reference_candidates(&assets, target)
                .iter()
                .map(|row| row.path.clone())
                .collect()
        };
        assert_eq!(
            shaders("pill_master_renderer::assets::Shader"),
            vec!["shaders/pbr.hlsl"]
        );
        assert_eq!(shaders("Texture"), vec!["textures/rock.png"]);
        assert_eq!(shaders("RenderPass"), vec!["passes/opaque.render_pass"]);
    }

    /// A drop between rows moves the entry there: a move downwards lands one
    /// slot earlier than the raw insertion index, a move upwards lands on it,
    /// and dropping an entry on either half of itself changes nothing.
    #[test]
    fn a_drop_moves_the_entry_to_the_insertion_point() {
        let entries = vec![
            serde_json::json!("a"),
            serde_json::json!("b"),
            serde_json::json!("c"),
        ];
        let order = |from: usize, to: usize| -> Vec<String> {
            moved_entry(&entries, from, to)
                .into_iter()
                .map(|entry| entry.as_str().unwrap_or_default().to_owned())
                .collect()
        };
        assert_eq!(order(0, 0), vec!["a", "b", "c"]);
        assert_eq!(order(0, 1), vec!["a", "b", "c"]);
        assert_eq!(order(0, 2), vec!["b", "a", "c"]);
        assert_eq!(order(0, 3), vec!["b", "c", "a"]);
        assert_eq!(order(2, 0), vec!["c", "a", "b"]);
        assert_eq!(order(1, 1), vec!["a", "b", "c"]);
        assert_eq!(order(1, 2), vec!["a", "b", "c"]);
        assert_eq!(order(1, 3), vec!["a", "c", "b"]);
        // A drag that began on an entry the list no longer holds leaves it.
        assert_eq!(order(7, 0), vec!["a", "b", "c"]);
    }

    /// The insertion point is the first row whose middle the pointer has not
    /// passed: above every row's middle is the list's head, below every one its
    /// end.
    #[test]
    fn the_insertion_point_follows_the_pointer() {
        let rows: Vec<egui::Rect> = (0..3)
            .map(|index| {
                egui::Rect::from_min_size(
                    egui::pos2(0.0, index as f32 * 20.0),
                    egui::vec2(80.0, 18.0),
                )
            })
            .collect();
        assert_eq!(insertion_index(&rows, 0.0), 0);
        assert_eq!(insertion_index(&rows, 8.0), 0);
        assert_eq!(insertion_index(&rows, 9.0), 1);
        assert_eq!(insertion_index(&rows, 29.0), 2);
        assert_eq!(insertion_index(&rows, 49.0), 3);
        assert_eq!(insertion_index(&[], 10.0), 0);
    }

    /// A settings row is named by its leaf: the headings above it spell the
    /// path out, and a truncated `parameters...` names nothing.
    #[test]
    fn a_settings_row_is_named_by_its_leaf() {
        assert_eq!(settings_row_label("shader"), "shader");
        assert_eq!(settings_row_label("textures.base_color"), "base_color");
        assert_eq!(settings_row_label("parameters.pbr_base.Color"), "Color");
        // An array element has no leaf of its own, so its own name stands.
        assert_eq!(settings_row_label("passes[2]"), "passes[2]");
    }

    /// A list row draws the field the snapshot expanded for its key; an entry
    /// the snapshot does not name still reads from the document itself.
    #[test]
    fn a_list_entry_reads_its_own_field() {
        let references = vec![AssetReferenceField {
            key: "passes[0]".to_string(),
            target: "pill_master_renderer::assets::RenderPass".to_string(),
            guid: Some("first".to_string()),
            nested: true,
            label: None,
        }];
        let expanded = list_entry_field(&references, "passes", 0, &JsonValue::Null, "RenderPass");
        assert_eq!(expanded.key, "passes[0]");
        assert_eq!(expanded.guid.as_deref(), Some("first"));
        let parsed = list_entry_field(
            &references,
            "passes",
            1,
            &serde_json::json!({ "guid": "second" }),
            "RenderPass",
        );
        assert_eq!(parsed.key, "passes[1]");
        assert_eq!(parsed.guid.as_deref(), Some("second"));
        assert!(parsed.nested);
    }
}
