//! The chrome the shell's tabs share: icons, selection moves and logged rects.
//!
//! # Responsibilities
//!
//! - `tab_icon`: the icon a dock tab draws beside its title.
//! - `paint_active_tab_underline`: the accent line under the active tab's name, the one tab the dock does not colour itself.
//! - The selection moves the tabs and the menus make, each logging the token a suite watches for.
//! - The redock action, the empty-area helper and the context-menu logging and region marking every menu goes through.
//! - The once-per-rect logging the inspector's sections and the viewport badges use.
//!
//! The globs below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
// Deliberately unused in this compilation: a patch crate replays this
// file's opening, and the first sibling this body names resolves through it.
#[allow(unused_imports)]
use pill_editor_shell_egui_editor::shell::*;

/// The icon a dock tab's title carries: the view or panel the tab shows.
pub fn tab_icon(tab: EditorTab) -> icons::Icon {
    match tab {
        EditorTab::Scene(0) => icons::Icon::Cube,
        EditorTab::Scene(_) => icons::Icon::CubeTransparent,
        EditorTab::Game => icons::Icon::MonitorPlay,
        EditorTab::Entities => icons::Icon::TreeStructure,
        EditorTab::Systems => icons::Icon::Gear,
        EditorTab::Inspector => icons::Icon::SlidersHorizontal,
        EditorTab::Assets => icons::Icon::FolderOpen,
        // The vendored set has no terminal mark; the code glyph is the
        // closest a prompt-in-a-panel gets without adding a glyph.
        EditorTab::Console => icons::Icon::Code,
        EditorTab::Problems => icons::Icon::XCircle,
        EditorTab::Statistics => icons::Icon::Gear,
        EditorTab::Preferences => icons::Icon::Faders,
        // A module panel is somebody else's: the puzzle-piece reading of the
        // set is the grid, which is as close as a generic "extension" gets.
        EditorTab::Module(_) => icons::Icon::GridFour,
    }
}

/// Paint the accent underline under the active tab's name.
///
/// The dock draws the strip's separator line under every tab in one neutral
/// colour, so the accent that marks the selected tab is the shell's to paint.
/// The line sits where the dock would have drawn its own: the last pixel of
/// the tab bar, spanning the tab button.
///
/// Public because [`run_frame`]'s generated hot patch reaches it through the
/// package path.
pub fn paint_active_tab_underline(response: &egui::Response) {
    let pixel = response.ctx.pixels_per_point().recip();
    response.ctx.layer_painter(response.layer_id).hline(
        response.rect.x_range(),
        response.rect.bottom() - pixel,
        (pixel, style::accent()),
    );
}

/// One gizmo mode's lowercase name, for the preferences rows.
pub fn gizmo_mode_label(mode: GizmoMode) -> &'static str {
    match mode {
        GizmoMode::Translate => "translate",
        GizmoMode::Rotate => "rotate",
        GizmoMode::Scale => "scale",
    }
}

/// One gizmo space's lowercase name, for the preferences rows.
pub fn gizmo_space_label(space: GizmoSpace) -> &'static str {
    match space {
        GizmoSpace::World => "world",
        GizmoSpace::Local => "local",
    }
}

/// What a click on an entity row means.
pub use pill_editor_core::editor_state::SelectionClick;

/// The meaning of a click made with these modifiers.
pub fn click_from_modifiers(modifiers: &egui::Modifiers) -> SelectionClick {
    if modifiers.shift {
        SelectionClick::Range
    } else if modifiers.command || modifiers.ctrl {
        SelectionClick::Add
    } else {
        SelectionClick::Reset
    }
}

/// Move the shared selection onto `entity` alone, logging the same token the
/// scene's picks log when it actually changed - so a suite can watch either
/// path move the selection. Returns whether the selection moved.
pub fn select_entity(selection: &mut EditorSelection, entity: Entity) -> bool {
    select_entity_click(selection, entity, SelectionClick::Reset, &[])
}

/// Move the shared entity selection for one click on a row.
///
/// `rows` is the list's own order, which only a range click reads: a range is
/// every row between the primary and the clicked one, so a Shift-click selects
/// what the list shows between them rather than an arbitrary set. Returns
/// whether the selection changed.
pub fn select_entity_click(
    selection: &mut EditorSelection,
    entity: Entity,
    click: SelectionClick,
    rows: &[Entity],
) -> bool {
    let before = selection.entities().to_vec();
    let clicked = selection.clicked(click, entity, rows);
    selection.set_entities(clicked);
    report_selection_change(selection, &before, entity)
}

/// Log and report a selection move the click helpers made.
pub fn report_selection_change(
    selection: &EditorSelection,
    before: &[Entity],
    entity: Entity,
) -> bool {
    if selection.entities() == before {
        return false;
    }
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        entity = %entity,
        count = selection.entities().len(),
        "[editor] selection changed"
    );
    true
}

/// Move the shared selection onto the asset at `path`, logging the token a
/// suite watches. Returns whether the selection moved.
pub fn select_asset(selection: &mut EditorSelection, path: &str) -> bool {
    if selection.asset.as_deref() == Some(path) {
        return false;
    }
    selection.asset = Some(path.to_string());
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        path,
        "[editor] asset selected"
    );
    true
}

/// Bring a floating surface's tabs back into the dock: the tab menu's
/// Redock, run after the dock has drawn the frame that asked.
///
/// Public because [`run_frame`]'s generated hot patch calls it through the
/// package path.
pub fn apply_tab_redock(dock: &mut DockState<EditorTab>, surface: SurfaceIndex) {
    let tabs: Vec<EditorTab> = dock
        .iter_all_tabs()
        .filter(|(path, _)| path.surface == surface)
        .map(|(_, tab)| *tab)
        .collect();
    if tabs.is_empty() {
        return;
    }
    dock.remove_surface(surface);
    for tab in &tabs {
        dock.push_to_first_leaf(*tab);
    }
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        count = tabs.len(),
        "[editor] tabs redocked"
    );
}

/// The trailing right-click area below a panel's rows: a strip of the
/// panel's width where the panel's own menu opens, away from any row.
pub fn empty_area(ui: &egui::Ui) -> egui::Vec2 {
    egui::Vec2::new(ui.available_width(), 32.0)
}

/// Log a context menu opening, so a suite can see which area's menu the
/// right-click opened. Public because the dock's tab menu logs through it
/// directly (its hook shape is not a [`context_menu`] call).
pub fn log_context_menu(area: &str) {
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        area,
        "[editor] context menu"
    );
}

/// Claim one floating surface's rectangle for the UI.
///
/// Every surface the shell draws outside the tab viewer - the create and
/// rename dialog, the About window, a picker popup under a field, a menu -
/// pushes its rectangle through here, so a press inside it is the UI's and
/// never a scene pick. One helper, so a new floating surface cannot forget the
/// rule.
pub fn claim_pointer(rect: egui::Rect, regions: &mut Vec<ShellRect>) {
    regions.push(ShellRect {
        x: rect.min.x,
        y: rect.min.y,
        width: rect.width(),
        height: rect.height(),
    });
}

/// Add an open context menu's rectangle to the frame's overlay regions: a
/// menu floats over whatever it was opened on (a clamped menu can hang over
/// the scene viewport), and a click inside it must never reach the scene
/// behind it.
///
/// Public like every helper the shell re-exports, so a patch crate reaches it
/// through the package path.
pub fn mark_menu_region(opened: Option<egui::InnerResponse<()>>, regions: &mut Vec<ShellRect>) {
    if let Some(menu) = opened {
        claim_pointer(menu.response.rect, regions);
    }
}

/// Open a floating popup under an anchor rectangle, and claim its area.
///
/// A picker draws in a foreground `Area` anchored under the field it belongs
/// to, so it floats over the rows below instead of pushing them down, and
/// its rectangle joins the frame's overlay regions, so a click inside it never
/// becomes a scene pick. The area's response is returned, so the caller can
/// tell whether a press landed outside it.
pub fn floating_popup(
    ui: &egui::Ui,
    id: egui::Id,
    anchor: egui::Rect,
    width: f32,
    regions: &mut Vec<ShellRect>,
    contents: &mut dyn FnMut(&mut egui::Ui),
) -> egui::InnerResponse<()> {
    let area = egui::Area::new(id)
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(anchor.left(), anchor.bottom() + 2.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(width);
                contents(ui);
            });
        });
    claim_pointer(area.response.rect, regions);
    area
}

/// Open a panel's context menu on a secondary click: the area is logged once
/// per click (so a suite can see whose menu opened), the menu is built by
/// `contents`, and its rectangle joins the frame's overlay regions while it is
/// open.
///
/// Every surface with a menu goes through here - the scene viewport, the
/// Entities rows and strip, the Inspector's headings and strip, the Assets
/// rows, tiles and strip - so no surface can forget to mark its menu.
pub fn context_menu(
    area: &str,
    response: &egui::Response,
    regions: &mut Vec<ShellRect>,
    contents: &mut dyn FnMut(&mut egui::Ui),
) {
    if response.secondary_clicked() {
        log_context_menu(area);
    }
    let opened = response.context_menu(|ui| contents(ui));
    mark_menu_region(opened, regions);
}

/// The badge an engine candidate carries in a picker: the asset is the
/// engine's own - a chain's built-in pass, a module's default material - and
/// not one of the project's files.
///
/// Public like every helper the shell re-exports, so a patch crate reaches it
/// through the package path.
pub fn engine_badge(ui: &mut egui::Ui) {
    let background = ui.visuals().extreme_bg_color;
    ui.label(
        egui::RichText::new("engine")
            .small()
            .color(style::text_muted())
            .background_color(background),
    );
}

/// The three transform tools with their menu labels and icons, in the order
/// every surface lists them: the viewport menu, the toolbox and the
/// Preferences tab.
pub const TOOL_ROWS: [(GizmoMode, &str, icons::Icon); 3] = [
    (
        GizmoMode::Translate,
        "Translate",
        icons::Icon::ArrowsOutCardinal,
    ),
    (GizmoMode::Rotate, "Rotate", icons::Icon::ArrowsClockwise),
    (GizmoMode::Scale, "Scale", icons::Icon::ArrowsOut),
];

/// The two gizmo spaces with their menu labels and icons, in the order every
/// surface lists them.
pub const SPACE_ROWS: [(GizmoSpace, &str, icons::Icon); 2] = [
    (
        GizmoSpace::World,
        "World space",
        icons::Icon::GlobeHemisphereWest,
    ),
    (GizmoSpace::Local, "Local space", icons::Icon::MapPin),
];

/// Whether `rect` is new for `key`, recording it as the placement to compare
/// against. The caller logs with its own literal message - a token passed as
/// a variable would become a tracing field instead of the message.
pub fn remember_entry_rect(
    logged: &mut Vec<(String, egui::Rect)>,
    key: &str,
    rect: egui::Rect,
) -> bool {
    if let Some((_, previous)) = logged.iter_mut().find(|(name, _)| name == key) {
        if *previous == rect {
            return false;
        }
        *previous = rect;
    } else {
        logged.push((key.to_string(), rect));
    }
    true
}

/// Note one keyed rectangle, answering whether it is new or has moved, so a
/// section that shifts as the panel grows or collapses reports its place
/// again instead of leaving a stale one in the log.
pub fn remember_moved_rect(
    logged: &mut Vec<(String, egui::Rect)>,
    key: &str,
    rect: egui::Rect,
) -> bool {
    if let Some(entry) = logged.iter_mut().find(|(name, _)| name == key) {
        if entry.1 == rect {
            return false;
        }
        entry.1 = rect;
        return true;
    }
    logged.push((key.to_string(), rect));
    true
}
