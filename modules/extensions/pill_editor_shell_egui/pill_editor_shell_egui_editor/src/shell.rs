//! The shell's UI work: the dock, the tabs and the frame build.
//!
//! # Responsibilities
//!
//! - [`run_frame`]: one egui pass, from the boundary input to the boundary
//!   frame the executable draws, including the scene rectangle the frontend
//!   places the scene's child window at.
//! - The tab bodies ([`entities_tab`], [`systems_tab`], [`inspector_tab`],
//!   [`assets_tab`], [`preferences_tab`], all `#[pill_hot_fn]` so a body
//!   edit patches live) and their row builders.
//!   The scene and game tabs draw nothing: their bodies stay holes in the
//!   UI so the scene and game child windows, which sit under the overlay,
//!   show through.
//!
//! # Design
//!
//! The dock is the session's `egui_dock` state, so layout edits land in the
//! resource the next generation re-claims. Nothing here stores anything in
//! egui's `Context` (the reload-safety contract `pill_ui_egui` documents);
//! the tabs read the snapshot and write the shared selection.

// External crates
use pill_editor_core_egui::assets::{self, AssetDialog};
use pill_editor_core_egui::browser::{self, ProjectBrowser};
use pill_editor_core_egui::commands::PendingEditorCommands;
use pill_editor_core_egui::editor_state::{
    AssetListing, EditorCommand, EditorSelection, EditorSnapshot, EntityDetail,
};
use pill_editor_core_egui::entities;
use pill_editor_core_egui::gizmo::{GizmoMode, GizmoSpace};
use pill_editor_core_egui::inspector;
use pill_editor_core_egui::layout::EditorTab;
use pill_editor_core_egui::menubar;
use pill_editor_core_egui::preferences;
use pill_editor_core_egui::session::{EguiShellState, MenubarSession, ToolboxSession};
use pill_editor_core_egui::systems;
use pill_editor_core_egui::toolbox;
use pill_engine::{pill_hot_fn, Entity, FieldValue, Input};
use pill_ui_egui::boundary::{
    frame_from_output, EguiShellFrame, EguiShellInput, ShellRect, SCENE_VIEWPORTS,
};
use pill_ui_egui::egui;
use pill_ui_egui::egui_dock::{DockArea, DockState, NodeIndex, SurfaceIndex, TabViewer};
use pill_ui_egui::icons;
use pill_ui_egui::style;
use serde_json::Value as JsonValue;

// =============================================================================
// Constants
// =============================================================================

/// How far the scene rectangle extends past the measured tab body, in
/// logical points: the body's padding and outline sit inside the leaf, and
/// the child window must back them too, so no sliver of the game view
/// behind the overlay shows around the viewport's edge.
const SCENE_RECT_OVERSHOOT: f32 = 16.0;

/// The scene's frame counter: its box in logical points, and the gap to the
/// tab body's corner.
const SCENE_COUNTER_WIDTH: f32 = 76.0;
const SCENE_COUNTER_HEIGHT: f32 = 20.0;
const SCENE_COUNTER_MARGIN: f32 = 8.0;

/// The example button beside the counter: its width, and the gap between
/// the two.
const SCENE_BUTTON_WIDTH: f32 = 72.0;
const SCENE_BUTTON_GAP: f32 = 6.0;

/// The Assets tab's tree pane: its default width and the range the splitter
/// drags it through, in points.
const ASSETS_TREE_WIDTH: f32 = 176.0;
const ASSETS_TREE_WIDTH_RANGE: std::ops::RangeInclusive<f32> = 96.0..=320.0;

/// One folder tree row's indentation per level and the width the expander's
/// caret (or its empty slot) takes, in points.
const ASSETS_TREE_INDENT: f32 = 12.0;
const ASSETS_TREE_CARET_WIDTH: f32 = 14.0;
const ASSETS_TREE_CARET_SIZE: f32 = 10.0;

/// The Assets tab's grid tiles: the gap around each tile, the band under its
/// icon for the name, the icon's inset, and the corner radius of the
/// selected and hovered washes, in points.
const ASSETS_TILE_GAP: f32 = 8.0;
const ASSETS_TILE_LABEL_HEIGHT: f32 = 26.0;
const ASSETS_TILE_LABEL_GAP: f32 = 2.0;
const ASSETS_TILE_ICON_INSET: f32 = 6.0;
const ASSETS_TILE_CORNER_RADIUS: f32 = 4.0;
const ASSETS_TILE_LABEL_SIZE: f32 = 11.0;

/// The rows a tile's name may wrap to before it elides, like Unity's two.
const ASSETS_TILE_LABEL_ROWS: usize = 2;

/// One shell frame: engine input becomes egui input, the dock runs, and the
/// output becomes the executable's frame. The asset dialog, when one is
/// open, is drawn over the dock.
pub fn run_frame(
    state: &mut EguiShellState,
    shell_input: &EguiShellInput,
    frame: &mut EguiShellFrame,
    snapshot: &EditorSnapshot,
    selection: &mut EditorSelection,
    pending: &mut PendingEditorCommands,
    engine_input: &Input,
) {
    state.frame_index = state.frame_index.wrapping_add(1);
    // The row heartbeat: the editor suite reads it to prove both tabs see
    // the project's data, that the frame counter - and with it the session
    // resource - survives a reload or patch, and which entity the
    // Inspector's detail tier describes.
    if state.frame_index.is_multiple_of(210) {
        let detail_entity = snapshot
            .detail
            .as_ref()
            .map_or_else(|| "-".to_string(), |detail| detail.entity.to_string());
        let detail_components = snapshot
            .detail
            .as_ref()
            .map_or(0, |detail| detail.components.len());
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            frames = state.frame_index,
            entities = snapshot.entities.len(),
            systems = snapshot.systems.len(),
            paused = snapshot.paused,
            detail = detail_entity.as_str(),
            detail_components,
            "[editor] egui shell rows"
        );
    }
    let raw_input = pill_ui_egui::input::raw_input(engine_input, shell_input);
    // The brand mark is uploaded once per session: the context owns the
    // texture, so a shell reload finds it already there.
    let mark = state.mark_texture();
    // The context is a cheap clone (refcounted handle); the dock and the
    // asset dialog are separate fields, borrowed apart so the dock's layout
    // borrow and the dialog's state borrow coexist.
    let context = state.context.clone();
    let EguiShellState {
        dock,
        asset_dialog,
        preferences_draft,
        toolbox,
        browser,
        menubar,
        ..
    } = state;
    // Named through the package path, not as a file-local item: a hot patch
    // compiles this body in a crate of its own, where only the package's
    // public items exist (see the crate header).
    let mut viewer = pill_editor_shell_egui_editor::shell::EditorTabViewer {
        snapshot,
        selection,
        asset_dialog,
        pending,
        preferences_draft,
        toolbox,
        browser,
        scene_rects: [None; SCENE_VIEWPORTS],
        game_rect: None,
        overlay_regions: Vec::new(),
        frames_per_second: shell_input.frames_per_second,
        tab_redock: None,
    };
    let output = context.run(raw_input, |context| {
        // The strip comes first, so the dock - and every viewport rectangle it
        // reports - starts below it.
        menu_bar(context, menubar, &mark, &mut viewer);
        // Tab bars carry the tabs and their close buttons only: without the
        // collapse chevron and the panel-level close-all ✖, a panel closes
        // exactly when its last tab does.
        DockArea::new(dock.state_mut())
            .style(style::dock_style(context.style().as_ref()))
            .show_leaf_collapse_buttons(false)
            .show_leaf_close_all_buttons(false)
            .show(context, &mut viewer);
        // The create/rename dialog floats over the dock while it is open.
        pill_editor_shell_egui_editor::shell::asset_dialog_window(
            context,
            viewer.asset_dialog,
            viewer.pending,
            viewer.snapshot,
        );
        pill_editor_shell_egui_editor::shell::about_window(context, &mark, &mut menubar.about_open);
    });
    let mut shell_frame = frame_from_output(&context, output);
    shell_frame.scene_rects = viewer.scene_rects;
    shell_frame.game_rect = viewer.game_rect;
    shell_frame.overlay_regions = std::mem::take(&mut viewer.overlay_regions);
    shell_frame.quit_requested = std::mem::take(&mut menubar.quit_requested);
    // A tab menu's re-dock runs after the dock finished drawing: changing
    // the tree mid-draw would edit it under the widget that asked.
    if let Some(surface) = viewer.tab_redock.take() {
        pill_editor_shell_egui_editor::shell::apply_tab_redock(dock.state_mut(), surface);
    }
    *frame = shell_frame;
}

// =============================================================================
// The tab viewer and the tabs
// =============================================================================

/// The menu bar above the dock: the pill mark, then Unity's six titles.
///
/// Drawn as a top panel before the dock, so the dock's layout and every
/// viewport rectangle the frame reports start below the strip. The bar's
/// geometry is logged once per placement (the toolbox's pattern), so a suite
/// can click a title without guessing at fonts or theme metrics, and an open
/// menu's entries log theirs for the same reason.
///
/// Public because a hot patch of [`run_frame`] calls it through the package
/// path, like every other function a patch can reach.
pub fn menu_bar(
    context: &egui::Context,
    session: &mut MenubarSession,
    mark: &egui::TextureHandle,
    viewer: &mut EditorTabViewer<'_>,
) {
    let style = context.style();
    let tint = style.visuals.text_color();
    egui::TopBottomPanel::top("pill_editor_shell_egui.menu_bar")
        .exact_height(menubar::MENU_BAR_HEIGHT)
        .frame(
            egui::Frame::side_top_panel(style.as_ref()).inner_margin(egui::Margin::symmetric(0, 4)),
        )
        .show(context, |ui| {
            let bar = ui.max_rect();
            let mut open_menu: Option<menubar::Menu> = None;
            if session.bar != Some(bar) {
                session.bar = Some(bar);
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    x = bar.min.x,
                    y = bar.min.y,
                    width = bar.width(),
                    height = bar.height(),
                    "[editor] menu bar rect"
                );
            }
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                // Unity's strip: plain labels at rest, a wash under the
                // pointer or the open menu - not resting buttons.
                ui.visuals_mut().widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
                ui.add_space(menubar::MENU_MARK_LEFT);
                let side = egui::vec2(menubar::MENU_MARK_SIDE, menubar::MENU_MARK_SIDE);
                ui.add(egui::Image::new((mark.id(), side)).tint(tint));
                ui.add_space(menubar::MENU_MARK_GAP);
                for (index, menu) in menubar::Menu::ALL.iter().enumerate() {
                    if index > 0 {
                        ui.add_space(MENU_TITLE_GAP);
                    }
                    let title = ui.menu_button(menu.title(), |ui| {
                        menu_body(ui, *menu, session, viewer);
                    });
                    if title.inner.is_some() {
                        open_menu = Some(*menu);
                    }
                    let response = title.response;
                    if session.titles[index] != Some(response.rect) {
                        session.titles[index] = Some(response.rect);
                        pill_core::info!(
                            target: pill_core::telemetry::telemetry_target::ENGINE,
                            name = menu.name(),
                            x = response.rect.min.x,
                            y = response.rect.min.y,
                            width = response.rect.width(),
                            height = response.rect.height(),
                            "[editor] menu item rect"
                        );
                    }
                }
            });
            if let Some(menu) = open_menu {
                if session.open != Some(menu) {
                    session.open = Some(menu);
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ENGINE,
                        menu = menu.name(),
                        "[editor] menu opened"
                    );
                }
            } else {
                session.open = None;
            }
        });
}

/// One open menu: its model rows, their placement logging, and the action a
/// click runs.
pub fn menu_body(
    ui: &mut egui::Ui,
    menu: menubar::Menu,
    session: &mut MenubarSession,
    viewer: &mut EditorTabViewer<'_>,
) {
    let view = menubar::MenuView {
        snapshot: viewer.snapshot,
        selection: viewer.selection,
    };
    for entry in menubar::entries(menu, &view) {
        match entry {
            menubar::MenuEntry::Separator => {
                ui.separator();
            }
            menubar::MenuEntry::Action {
                label,
                action,
                enabled,
                checked,
            } => {
                let response = ui.add_enabled(
                    enabled,
                    egui::Button::selectable(checked.unwrap_or(false), label),
                );
                let key = format!("{}/{}", menu.name(), label);
                if session
                    .entries
                    .iter()
                    .find(|(logged, _)| *logged == key)
                    .is_none_or(|(_, rect)| *rect != response.rect)
                {
                    if let Some((_, rect)) = session
                        .entries
                        .iter_mut()
                        .find(|(logged, _)| *logged == key)
                    {
                        *rect = response.rect;
                    } else {
                        session.entries.push((key, response.rect));
                    }
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ENGINE,
                        menu = menu.name(),
                        label,
                        x = response.rect.min.x,
                        y = response.rect.min.y,
                        width = response.rect.width(),
                        height = response.rect.height(),
                        "[editor] menu entry rect"
                    );
                }
                if response.clicked() {
                    run_menu_action(menu, label, action, session, viewer);
                    ui.close();
                }
            }
        }
    }
    // The popup floats over the dock and the viewports, so its rectangle is
    // the UI's: a click on an item must never become a scene pick.
    let rect = ui.min_rect();
    viewer.overlay_regions.push(ShellRect {
        x: rect.min.x,
        y: rect.min.y,
        width: rect.width(),
        height: rect.height(),
    });
}

/// Run one menu item through the same calls the tabs, the toolbox and the
/// scripted channel make.
pub fn run_menu_action(
    menu: menubar::Menu,
    label: &str,
    action: menubar::MenuAction,
    session: &mut MenubarSession,
    viewer: &mut EditorTabViewer<'_>,
) {
    match action {
        menubar::MenuAction::Inert => {}
        menubar::MenuAction::Queue {
            label: history_label,
            commands,
        } => {
            viewer.pending.push_recorded(commands, history_label, None);
        }
        menubar::MenuAction::CreateAsset => {
            assets::open_create(
                viewer.asset_dialog,
                viewer.selection,
                &viewer.snapshot.assets,
                &viewer.snapshot.standalone_types,
            );
        }
        menubar::MenuAction::WriteAssetMeta => {
            assets::write_meta_selection(viewer.pending, viewer.selection, &viewer.snapshot.assets);
        }
        menubar::MenuAction::Undo => session
            .history_requests
            .push(pill_editor_core_egui::session::HistoryRequest::Undo),
        menubar::MenuAction::Redo => session
            .history_requests
            .push(pill_editor_core_egui::session::HistoryRequest::Redo),
        menubar::MenuAction::ActivateTab(tab) => session.pending_tab = Some(tab),
        menubar::MenuAction::About => session.about_open = true,
        menubar::MenuAction::Quit => session.quit_requested = true,
    }
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        menu = menu.name(),
        label,
        "[editor] menu action"
    );
}

/// The About window: the mark, the editor's name and the shell's version.
pub fn about_window(context: &egui::Context, mark: &egui::TextureHandle, open: &mut bool) {
    if !*open {
        return;
    }
    let mut opened = *open;
    egui::Window::new("About Pill Editor")
        .open(&mut opened)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(context, |ui| {
            ui.horizontal(|ui| {
                let side = egui::vec2(40.0, 40.0);
                ui.add(egui::Image::new((mark.id(), side)));
                ui.vertical(|ui| {
                    ui.heading("Pill Editor");
                    ui.label(format!("shell {}", env!("CARGO_PKG_VERSION")));
                    ui.label("an egui frontend for the Pill engine");
                });
            });
        });
    *open = opened;
}

/// The gap between two menu titles in the bar.
const MENU_TITLE_GAP: f32 = 2.0;

/// The icon a dock tab's title carries: the view or panel the tab shows.
pub fn tab_icon(tab: EditorTab) -> icons::Icon {
    match tab {
        EditorTab::Scene => icons::Icon::Cube,
        EditorTab::SecondScene => icons::Icon::CubeTransparent,
        EditorTab::Game => icons::Icon::MonitorPlay,
        EditorTab::Entities => icons::Icon::TreeStructure,
        EditorTab::Systems => icons::Icon::Gear,
        EditorTab::Inspector => icons::Icon::SlidersHorizontal,
        EditorTab::Assets => icons::Icon::FolderOpen,
        EditorTab::Preferences => icons::Icon::Faders,
    }
}

/// Draws the dock's tabs; the tab bodies come from the tab functions.
///
/// Public - fields included - because a hot patch for [`run_frame`] is
/// compiled as its own crate and builds this value: the generated source
/// reaches the type and its fields through the package path, the rule the
/// crate header states for every hot function's dependencies.
pub struct EditorTabViewer<'a> {
    /// The engine data the tabs read.
    pub snapshot: &'a EditorSnapshot,
    /// The shared selection the Entities tab writes.
    pub selection: &'a mut EditorSelection,
    /// The asset dialog that is open, if one is; the Assets tab and the
    /// dialog window both read and write it.
    pub asset_dialog: &'a mut Option<AssetDialog>,
    /// The queue the assets actions push their recorded batches into.
    pub pending: &'a mut PendingEditorCommands,
    /// The Preferences tab's unsaved field buffers; the tab seeds them
    /// from the snapshot and a save clears them.
    pub preferences_draft: &'a mut Option<preferences::PreferencesDraft>,
    /// The floating viewport toolboxes' session state: where each scene
    /// view's toolbox floats and what the log already reported.
    pub toolbox: &'a mut ToolboxSession,
    /// The Assets tab's project browser: the folder its grid lists, the
    /// tree's open folders and the tile size.
    pub browser: &'a mut ProjectBrowser,
    /// The scene viewports' body rectangles this pass, in logical points,
    /// one slot per scene tab; the frontend places each viewport's child
    /// window at its slot.
    pub scene_rects: [Option<ShellRect>; SCENE_VIEWPORTS],
    /// The game viewport's body rectangle this pass, in logical points; the
    /// frontend places the primary surface's child window at it.
    pub game_rect: Option<ShellRect>,
    /// The overlay chips' rectangles this pass, in logical points: points
    /// over a scene viewport where a click is the UI's, not the scene's.
    pub overlay_regions: Vec<ShellRect>,
    /// The editor's smoothed frame rate, for the scene's counter.
    pub frames_per_second: f32,
    /// The surface a tab menu asked to re-dock, drained by `run_frame`
    /// after the dock finishes drawing.
    pub tab_redock: Option<SurfaceIndex>,
}

impl TabViewer for EditorTabViewer<'_> {
    type Tab = EditorTab;

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        icons::rich(tab_icon(*tab), (*tab).title()).into()
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        match tab {
            EditorTab::Scene => {
                self.scene_rects[0] = Some(scene_body_rect(ui));
                self.scene_context_menu(ui);
                scene_overlay(ui, self.frames_per_second, &mut self.overlay_regions);
                self.toolbox_overlay(ui, 0);
            }
            EditorTab::SecondScene => {
                self.scene_rects[1] = Some(scene_body_rect(ui));
                self.scene_context_menu(ui);
                scene_overlay(ui, self.frames_per_second, &mut self.overlay_regions);
                self.toolbox_overlay(ui, 1);
            }
            EditorTab::Game => {
                // The game viewport is the primary surface's child window: a
                // hole, measured like a scene tab, with no menu and no
                // overlay chips - it is the project's own view.
                self.game_rect = Some(scene_body_rect(ui));
            }
            EditorTab::Entities => entities_tab(
                ui,
                self.snapshot,
                self.selection,
                self.pending,
                &mut self.overlay_regions,
            ),
            EditorTab::Systems => systems_tab(ui, self.snapshot, self.pending),
            EditorTab::Inspector => inspector_tab(
                ui,
                self.snapshot,
                &*self.selection,
                self.pending,
                &mut self.overlay_regions,
            ),
            EditorTab::Assets => assets_tab(
                ui,
                self.snapshot,
                self.selection,
                self.asset_dialog,
                self.pending,
                &mut self.overlay_regions,
                self.browser,
            ),
            EditorTab::Preferences => {
                preferences_tab(ui, self.snapshot, self.preferences_draft, self.pending)
            }
        }
    }

    /// The dock tab's menu. The dock itself offers Eject (into a window)
    /// and Close; a floating window's tab gets Redock, which brings the
    /// surface's tabs back into the dock.
    fn context_menu(
        &mut self,
        ui: &mut egui::Ui,
        _tab: &mut Self::Tab,
        surface: SurfaceIndex,
        _node: NodeIndex,
    ) {
        if !surface.is_main()
            && ui
                .add(icons::IconButton::new(
                    icons::Icon::ArrowSquareOut,
                    "Redock",
                ))
                .clicked()
        {
            self.tab_redock = Some(surface);
            ui.close();
        }
        // The dock owns this menu's shell, so its drawn content is what is
        // reported: an item click over the viewport must not pick either.
        let rect = ui.min_rect();
        self.overlay_regions.push(ShellRect {
            x: rect.min.x,
            y: rect.min.y,
            width: rect.width(),
            height: rect.height(),
        });
    }

    /// The tab button hook: a right-click that opens the tab menu is logged
    /// here, once per click - the menu content runs every frame it is open.
    fn on_tab_button(&mut self, _tab: &mut Self::Tab, response: &egui::Response) {
        if response.secondary_clicked() {
            log_context_menu("tab");
        }
    }

    /// The scene and game tabs keep their bodies transparent: the scene and
    /// game child windows show through them from under the overlay.
    fn clear_background(&self, tab: &Self::Tab) -> bool {
        !matches!(
            tab,
            EditorTab::Scene | EditorTab::SecondScene | EditorTab::Game
        )
    }

    /// The scene and game tabs' bodies are viewports; they never scroll.
    fn scroll_bars(&self, tab: &Self::Tab) -> [bool; 2] {
        match tab {
            EditorTab::Scene | EditorTab::SecondScene | EditorTab::Game => [false, false],
            _ => [true, true],
        }
    }
}

/// A scene tab's body rectangle in logical points, overshooting by
/// [`SCENE_RECT_OVERSHOOT`] so the child window backs the tab body's padding
/// and outline too.
pub fn scene_body_rect(ui: &egui::Ui) -> ShellRect {
    shell_rect(ui.max_rect().expand(SCENE_RECT_OVERSHOOT))
}

/// Convert an egui rectangle into the boundary's plain rectangle.
pub fn shell_rect(rect: egui::Rect) -> ShellRect {
    ShellRect {
        x: rect.min.x,
        y: rect.min.y,
        width: rect.width(),
        height: rect.height(),
    }
}

/// A scene tab's overlay strip, anchored to the viewport's top-right
/// corner: an example button and the frame counter, both drawn as the
/// theme's chips. The overlay paints above the scene's child window, so
/// both sit over the viewport, and both take input, so a click on either
/// never reaches the project. Both rectangles are recorded as the UI's
/// own, so a click on them is never read as a scene pick.
pub fn scene_overlay(
    ui: &mut egui::Ui,
    frames_per_second: f32,
    overlay_regions: &mut Vec<ShellRect>,
) {
    let counter_rect = frame_counter_rect(ui);
    let button_rect = egui::Rect::from_min_size(
        egui::pos2(
            counter_rect.left() - SCENE_BUTTON_WIDTH - SCENE_BUTTON_GAP,
            counter_rect.top(),
        ),
        egui::vec2(SCENE_BUTTON_WIDTH, SCENE_COUNTER_HEIGHT),
    );
    overlay_regions.push(shell_rect(counter_rect));
    overlay_regions.push(shell_rect(button_rect));
    let button = ui.put(
        button_rect,
        egui::Button::new("Example")
            .min_size(button_rect.size())
            .fill(style::CONTROL_FILL)
            .corner_radius(egui::CornerRadius::same(4)),
    );
    if button.clicked() {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            "[editor] example button clicked"
        );
    }
    // The counter is painted rather than laid out, so it costs the tab body
    // no layout space; its rectangle still senses clicks.
    let _ = ui.interact(
        counter_rect,
        ui.id().with("scene_frame_counter"),
        egui::Sense::click(),
    );
    ui.painter().rect(
        counter_rect,
        egui::CornerRadius::same(5),
        style::OVERLAY_FILL,
        egui::Stroke::new(1.0, style::OVERLAY_BORDER),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        counter_rect.center(),
        egui::Align2::CENTER_CENTER,
        format!("{frames_per_second:.0} fps"),
        egui::FontId::monospace(12.0),
        style::TEXT_STRONG,
    );
}

/// The frame counter's rectangle: the strip's right-anchored box.
pub fn frame_counter_rect(ui: &egui::Ui) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(
            ui.max_rect().right() - SCENE_COUNTER_WIDTH - SCENE_COUNTER_MARGIN,
            ui.max_rect().top() + SCENE_COUNTER_MARGIN,
        ),
        egui::vec2(SCENE_COUNTER_WIDTH, SCENE_COUNTER_HEIGHT),
    )
}

/// One row per live entity: the entity and its label, for the Entities tab.
pub fn entity_rows(snapshot: &EditorSnapshot) -> Vec<(Entity, String)> {
    snapshot
        .entities
        .iter()
        .map(|entry| {
            (
                entry.entity,
                format!("{}  ({} components)", entry.display, entry.component_count),
            )
        })
        .collect()
}

/// The Systems tab: the engine's pause and single-step controls in a header
/// row, then one row per registered system - its name and owner, and a
/// state button (`on`/`off`) that queues the recorded, undoable toggle the
/// native panel records.
#[pill_hot_fn]
pub fn systems_tab(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    pending: &mut PendingEditorCommands,
) {
    ui.horizontal(|ui| {
        let pause_icon = if snapshot.paused {
            icons::Icon::Play
        } else {
            icons::Icon::Pause
        };
        if ui
            .add(icons::IconButton::new(
                pause_icon,
                systems::pause_button_label(snapshot.paused),
            ))
            .clicked()
        {
            pending.push(vec![systems::pause_toggle_command(snapshot.paused)]);
        }
        if ui
            .add(icons::IconButton::new(icons::Icon::SkipForward, "Step"))
            .clicked()
        {
            pending.push(vec![EditorCommand::StepOnce]);
        }
        let enabled = snapshot.systems.iter().filter(|row| row.enabled).count();
        ui.label(
            egui::RichText::new(format!("{}/{} enabled", enabled, snapshot.systems.len())).weak(),
        );
    });
    ui.separator();
    egui::ScrollArea::vertical().show(ui, |ui| {
        for system in &snapshot.systems {
            ui.horizontal(|ui| {
                ui.label(systems::system_row_label(system));
                if ui
                    .add(icons::IconButton::new(
                        icons::Icon::Power,
                        systems::system_state_label(system.enabled),
                    ))
                    .clicked()
                {
                    let (command, label) = systems::system_toggle(system.index, !system.enabled);
                    pending.push_recorded(vec![command], label, None);
                }
            });
        }
    });
}

/// The Preferences tab (Stage 20): the editor-wide rows seeded from the
/// preferences file and saved as one read-modify-write batch, then the
/// project's remembered gizmo choices with the tool buttons that change
/// them - the same queued commands the shortcuts, the menus and the
/// scripted channel land in.
#[pill_hot_fn]
pub fn preferences_tab(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    draft: &mut Option<preferences::PreferencesDraft>,
    pending: &mut PendingEditorCommands,
) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        let view = &snapshot.preferences;
        // The buffers seed from the file once and stay until a save;
        // `None` means "re-seed from the file" and covers the first frame
        // and the frame after a save.
        let buffers = draft.get_or_insert_with(|| view.seed_buffers());
        ui.heading("Editor");
        ui.label(
            egui::RichText::new("Preferences file: blank keeps the default, Save writes the rows.")
                .weak(),
        );
        for field in preferences::PREFERENCE_FIELDS {
            let buffer = buffers.entry(field.key.to_string()).or_default();
            ui.horizontal(|ui| {
                ui.label(field.label);
                ui.add(egui::TextEdit::singleline(buffer).desired_width(150.0));
            });
        }
        let mut save = false;
        ui.horizontal(|ui| {
            save = ui
                .add(icons::IconButton::new(
                    icons::Icon::FloppyDisk,
                    "Save preferences",
                ))
                .clicked();
        });
        if save {
            let values = preferences::PREFERENCE_FIELDS
                .iter()
                .map(|field| {
                    (
                        field.key.to_string(),
                        buffers.get(field.key).cloned().unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>();
            pending.push(vec![EditorCommand::SetPreferences { values }]);
            // Re-seed from the file next frame, so the rows show what the
            // save actually wrote.
            *draft = None;
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                "[editor] preferences save queued"
            );
        }
        ui.separator();
        ui.heading("Project");
        ui.label(format!(
            "Project: {}",
            view.project.as_deref().unwrap_or("(unknown)")
        ));
        if view.project_remembered {
            ui.label(format!(
                "Remembered tool: mode={} space={} snap={}",
                view.project_gizmo.mode.map_or("(unset)", gizmo_mode_label),
                view.project_gizmo
                    .space
                    .map_or("(unset)", gizmo_space_label),
                view.project_gizmo
                    .snap
                    .map_or("(unset)".to_string(), |snap| snap.to_string()),
            ));
        } else {
            ui.label(
                egui::RichText::new(
                    "Remembered tool: none yet - a tool choice is remembered per project.",
                )
                .weak(),
            );
        }
        ui.label(format!(
            "Active: mode={} space={} snap={}",
            gizmo_mode_label(snapshot.gizmo.mode),
            gizmo_space_label(snapshot.gizmo.space),
            snapshot.gizmo.snap,
        ));
        ui.horizontal(|ui| {
            for (label, mode, icon) in [
                (
                    "Translate",
                    GizmoMode::Translate,
                    icons::Icon::ArrowsOutCardinal,
                ),
                ("Rotate", GizmoMode::Rotate, icons::Icon::ArrowsClockwise),
                ("Scale", GizmoMode::Scale, icons::Icon::ArrowsOut),
            ] {
                let button =
                    icons::IconButton::new(icon, label).active(snapshot.gizmo.mode == mode);
                if ui.add(button).clicked() {
                    pending.push(vec![EditorCommand::SetGizmoTool {
                        mode: Some(mode),
                        space: None,
                        snap: None,
                    }]);
                }
            }
            for (label, space, icon) in [
                ("World", GizmoSpace::World, icons::Icon::GlobeHemisphereWest),
                ("Local", GizmoSpace::Local, icons::Icon::MapPin),
            ] {
                let button =
                    icons::IconButton::new(icon, label).active(snapshot.gizmo.space == space);
                if ui.add(button).clicked() {
                    pending.push(vec![EditorCommand::SetGizmoTool {
                        mode: None,
                        space: Some(space),
                        snap: None,
                    }]);
                }
            }
            let snap =
                icons::IconButton::new(icons::Icon::Magnet, "Snap").active(snapshot.gizmo.snap);
            if ui.add(snap).clicked() {
                pending.push(vec![EditorCommand::SetGizmoTool {
                    mode: None,
                    space: None,
                    snap: Some(!snapshot.gizmo.snap),
                }]);
            }
        });
    });
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

/// The Entities tab: the world's entities; a click selects one, a right
/// click selects it and opens its menu - the recorded, undoable create and
/// delete the native Hierarchy carries as buttons.
#[pill_hot_fn]
pub fn entities_tab(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    selection: &mut EditorSelection,
    pending: &mut PendingEditorCommands,
    overlay_regions: &mut Vec<ShellRect>,
) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        for (entity, label) in entity_rows(snapshot) {
            let selected = selection.entity == Some(entity);
            let response = ui.selectable_label(selected, label);
            if response.clicked() {
                select_entity(selection, entity);
            }
            if response.secondary_clicked() {
                select_entity(selection, entity);
                log_context_menu("entities");
            }
            let opened =
                response.context_menu(|ui| entity_menu_items(ui, Some(entity), selection, pending));
            mark_menu_region(opened, overlay_regions);
        }
        // The space below the rows creates one entity; there is nothing to
        // delete away from a row.
        let empty = ui.allocate_response(empty_area(ui), egui::Sense::click());
        if empty.secondary_clicked() {
            log_context_menu("entities");
        }
        let opened = empty.context_menu(|ui| entity_menu_items(ui, None, selection, pending));
        mark_menu_region(opened, overlay_regions);
    });
}

/// The entity menu: New Entity everywhere, Delete Entity for the row under
/// the pointer. Both edits are recorded, so one undo step reverses them.
pub fn entity_menu_items(
    ui: &mut egui::Ui,
    entity: Option<Entity>,
    selection: &EditorSelection,
    pending: &mut PendingEditorCommands,
) {
    if ui
        .add(icons::IconButton::new(icons::Icon::Plus, "New Entity"))
        .clicked()
    {
        let (command, label) = entities::entity_create();
        pending.push_recorded(vec![command], label, None);
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            "[editor] new entity queued"
        );
        ui.close();
    }
    if let Some(entity) = entity {
        if ui
            .add(icons::IconButton::new(icons::Icon::Trash, "Delete Entity"))
            .clicked()
        {
            let (commands, label) = entities::entity_delete(entity, selection.asset.clone());
            pending.push_recorded(commands, label, None);
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                entity = %entity,
                "[editor] delete entity queued"
            );
            ui.close();
        }
    }
}

/// The trailing right-click area below a panel's rows: a strip of the
/// panel's width where the panel's own menu opens, away from any row.
pub fn empty_area(ui: &egui::Ui) -> egui::Vec2 {
    egui::Vec2::new(ui.available_width(), 32.0)
}

/// Log a context menu opening, so a suite can see which area's menu the
/// right-click opened.
pub fn log_context_menu(area: &str) {
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        area,
        "[editor] context menu"
    );
}

/// Add an open context menu's rectangle to the frame's overlay regions: a
/// menu floats over whatever it was opened on (a clamped menu can hang over
/// the scene viewport), and a click inside it must never reach the scene
/// behind it.
pub fn mark_menu_region(opened: Option<egui::InnerResponse<()>>, regions: &mut Vec<ShellRect>) {
    if let Some(menu) = opened {
        let rect = menu.response.rect;
        regions.push(ShellRect {
            x: rect.min.x,
            y: rect.min.y,
            width: rect.width(),
            height: rect.height(),
        });
    }
}

/// Bring a floating surface's tabs back into the dock: the tab menu's
/// Redock, run after the dock has drawn the frame that asked.
///
/// Public because [`run_frame`]'s generated hot patch calls it through the
/// package path.
pub fn apply_tab_redock(dock: &mut DockState<EditorTab>, surface: SurfaceIndex) {
    let tabs: Vec<EditorTab> = dock
        .iter_all_tabs()
        .filter(|((index, _), _)| *index == surface)
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

/// The component menu: detach the heading's component, and attach any
/// registered one the entity does not carry yet - both recorded under the
/// native labels (`add {short}` / `remove {short}`).
pub fn component_menu_items(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    detail: &EntityDetail,
    current: Option<&str>,
    pending: &mut PendingEditorCommands,
) {
    if let Some(component) = current {
        let label = format!("Remove {}", inspector::short_component_name(component));
        if ui
            .add(icons::IconButton::new(icons::Icon::MinusCircle, &label))
            .clicked()
        {
            let (command, label) = inspector::component_remove(detail.entity, component);
            pending.push_recorded(vec![command], label, None);
            ui.close();
        }
        ui.separator();
    }
    ui.menu_button(
        icons::rich(icons::Icon::PlusCircle, "Add Component"),
        |ui| {
            for registered in snapshot
                .components
                .iter()
                .filter(|registered| registered.addable)
            {
                let attached = detail
                    .components
                    .iter()
                    .any(|component| component.type_name == registered.type_name);
                if attached {
                    continue;
                }
                let short = inspector::short_component_name(&registered.type_name);
                if ui
                    .add(icons::IconButton::new(icons::Icon::PlusCircle, short))
                    .clicked()
                {
                    let (command, label) =
                        inspector::component_add(detail.entity, &registered.type_name);
                    pending.push_recorded(vec![command], label, None);
                    ui.close();
                }
            }
        },
    );
}

impl EditorTabViewer<'_> {
    /// The scene viewport's menu: the transform tools, snapping and the
    /// space, checked against the snapshot's gizmo settings. The items queue
    /// the same `SetGizmoTool` edit the tools' shortcuts land in.
    fn scene_context_menu(&mut self, ui: &mut egui::Ui) {
        let settings = self.snapshot.gizmo;
        let grid_enabled = self.snapshot.grid_enabled;
        let response = ui.allocate_response(ui.available_size_before_wrap(), egui::Sense::click());
        if response.secondary_clicked() {
            log_context_menu("scene");
        }
        let opened = response.context_menu(|ui| {
            let tools = [
                (
                    "Translate",
                    GizmoMode::Translate,
                    icons::Icon::ArrowsOutCardinal,
                ),
                ("Rotate", GizmoMode::Rotate, icons::Icon::ArrowsClockwise),
                ("Scale", GizmoMode::Scale, icons::Icon::ArrowsOut),
            ];
            for (label, mode, icon) in tools {
                let button = icons::IconButton::new(icon, label).active(settings.mode == mode);
                if ui.add(button).clicked() {
                    self.pending.push(vec![EditorCommand::SetGizmoTool {
                        mode: Some(mode),
                        space: None,
                        snap: None,
                    }]);
                    ui.close();
                }
            }
            ui.separator();
            let snapping =
                icons::IconButton::new(icons::Icon::Magnet, "Snapping").active(settings.snap);
            if ui.add(snapping).clicked() {
                self.pending.push(vec![EditorCommand::SetGizmoTool {
                    mode: None,
                    space: None,
                    snap: Some(!settings.snap),
                }]);
                ui.close();
            }
            let spaces = [
                (
                    "World space",
                    GizmoSpace::World,
                    icons::Icon::GlobeHemisphereWest,
                ),
                ("Local space", GizmoSpace::Local, icons::Icon::MapPin),
            ];
            for (label, space, icon) in spaces {
                let button = icons::IconButton::new(icon, label).active(settings.space == space);
                if ui.add(button).clicked() {
                    self.pending.push(vec![EditorCommand::SetGizmoTool {
                        mode: None,
                        space: Some(space),
                        snap: None,
                    }]);
                    ui.close();
                }
            }
            ui.separator();
            let grid = icons::IconButton::new(icons::Icon::GridFour, "Grid").active(grid_enabled);
            if ui.add(grid).clicked() {
                self.pending.push(vec![EditorCommand::SetGrid {
                    enabled: !grid_enabled,
                }]);
                ui.close();
            }
        });
        // While the menu is open its rectangle joins the overlay regions:
        // a click on an item is the UI's, never a scene pick, even though
        // the menu floats over the viewport.
        mark_menu_region(opened, &mut self.overlay_regions);
    }

    /// The floating toolbox over one scene viewport (Stage 22): the gizmo's
    /// tools, its space and its snapping as one panel the user drags
    /// anywhere inside the view - the always-visible twin of the viewport
    /// menu's items.
    ///
    /// The panel floats over the scene's child window (the composition
    /// overlay paints above it), and its rectangle joins the frame's
    /// overlay regions, so a click anywhere on it - a row or the gap
    /// between rows - is the UI's and never becomes a scene pick. The drag
    /// starts on the handle and its result is clamped to the viewport, so
    /// the panel can be parked anywhere inside the view but not lost
    /// outside it. Both rectangles, the panel's and every row's, are
    /// logged once per placement and again after each drag: the suite's
    /// scripted clicks read their targets straight from the log.
    fn toolbox_overlay(&mut self, ui: &mut egui::Ui, view: usize) {
        let viewport = ui.max_rect();
        let state = toolbox::ToolboxState {
            gizmo: self.snapshot.gizmo,
            grid: self.snapshot.grid_enabled,
        };
        let mut origin = toolbox::clamped_origin(
            self.toolbox.origins[view].unwrap_or_else(|| toolbox::default_origin(viewport)),
            viewport,
        );
        // The handle is the one part of the panel a press drags; a click on
        // a row below it still reaches the row's button.
        let handle = toolbox::handle_rect(origin);
        let response = ui.interact(
            handle,
            ui.id().with(("scene-toolbox", view)),
            egui::Sense::drag(),
        );
        if response.dragged() {
            origin = toolbox::clamped_origin(origin + response.drag_delta(), viewport);
            self.toolbox.origins[view] = Some(origin);
        }
        if response.drag_stopped() {
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                view,
                x = origin.x,
                y = origin.y,
                "[editor] toolbox moved"
            );
        }
        let panel = toolbox::panel_rect(origin);
        ui.painter().rect(
            panel,
            egui::CornerRadius::same(5),
            style::OVERLAY_FILL,
            egui::Stroke::new(1.0, style::OVERLAY_BORDER),
            egui::StrokeKind::Inside,
        );
        ui.painter().text(
            egui::pos2(panel.center().x, handle.center().y),
            egui::Align2::CENTER_CENTER,
            "Tools",
            egui::FontId::proportional(11.0),
            style::TEXT_MUTED,
        );
        for item in toolbox::TOOLBOX_ITEMS {
            let rect = toolbox::item_rect(item, origin);
            let fill = if item.active(&state) {
                style::SELECTION_FILL
            } else {
                style::CONTROL_FILL
            };
            let button = ui.put(
                rect,
                icons::IconButton::new(item.icon(&state), item.label(&state))
                    .active(item.active(&state))
                    .fill(fill)
                    .fixed_size(rect.size()),
            );
            if button.clicked() {
                self.pending.push(vec![item.command(&state)]);
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    view,
                    item = item.name(),
                    "[editor] toolbox click"
                );
            }
        }
        self.overlay_regions.push(shell_rect(panel));
        if self.toolbox.logged[view] != Some(panel) {
            self.toolbox.logged[view] = Some(panel);
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                view,
                x = panel.min.x,
                y = panel.min.y,
                width = panel.width(),
                height = panel.height(),
                "[editor] toolbox rect"
            );
            for (item, rect) in std::iter::once(("handle", handle)).chain(
                toolbox::TOOLBOX_ITEMS
                    .iter()
                    .map(|item| (item.name(), toolbox::item_rect(*item, origin))),
            ) {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    view,
                    item,
                    x = rect.min.x,
                    y = rect.min.y,
                    width = rect.width(),
                    height = rect.height(),
                    "[editor] toolbox item"
                );
            }
        }
    }
}

/// Move the shared selection onto `entity`, logging the same token the
/// scene's picks log when it actually changed - so a suite can watch either
/// path move the selection. Returns whether the selection moved.
pub fn select_entity(selection: &mut EditorSelection, entity: Entity) -> bool {
    if selection.entity == Some(entity) {
        return false;
    }
    selection.entity = Some(entity);
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        entity = %entity,
        "[editor] selection changed"
    );
    true
}

/// The Inspector tab: the selected entity's components with interactive
/// field widgets (checkboxes, number drags, text fields; an array field
/// gets one control per element), and the selected asset's import settings
/// as editable leaves. Every commit is a recorded `SetField` /
/// `SetArrayElement` / `SetAssetSettings` edit, coalesced per widget so a
/// drag or a typing run is one undo step - the add/remove component actions
/// stay with the W-0027 port.
#[pill_hot_fn]
pub fn inspector_tab(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    selection: &EditorSelection,
    pending: &mut PendingEditorCommands,
    overlay_regions: &mut Vec<ShellRect>,
) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        match snapshot.detail.as_ref() {
            Some(detail) => {
                ui.label(egui::RichText::new(format!("entity {}", detail.entity)).strong());
                ui.separator();
                entity_inspector(ui, snapshot, detail, pending, overlay_regions);
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
            if empty.secondary_clicked() {
                log_context_menu("inspector");
            }
            let opened =
                empty.context_menu(|ui| component_menu_items(ui, snapshot, detail, None, pending));
            mark_menu_region(opened, overlay_regions);
        }
    });
}

/// One entity's components; every editable field is a widget row and every
/// component heading carries its menu (attach and detach - the native
/// Inspector's two component actions, recorded and undoable).
pub fn entity_inspector(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    detail: &EntityDetail,
    pending: &mut PendingEditorCommands,
    overlay_regions: &mut Vec<ShellRect>,
) {
    for component in &detail.components {
        let heading = ui.add(
            egui::Label::new(egui::RichText::new(component.type_name.as_str()).strong())
                .sense(egui::Sense::click()),
        );
        if heading.secondary_clicked() {
            log_context_menu("inspector");
        }
        let opened = heading.context_menu(|ui| {
            component_menu_items(
                ui,
                snapshot,
                detail,
                Some(component.type_name.as_str()),
                pending,
            )
        });
        mark_menu_region(opened, overlay_regions);
        if !component.editable {
            ui.label(egui::RichText::new("no editable fields").weak().italics());
            ui.separator();
            continue;
        }
        for (descriptor, value) in component.fields.iter().zip(component.values.iter()) {
            entity_field_row(
                ui,
                detail.entity,
                &component.type_name,
                descriptor.name,
                value,
                pending,
            );
        }
        ui.separator();
    }
}

/// One field row: the widget its value's kind asks for, and the recorded
/// commit its change queues. A field that changed while the snapshot was
/// stale is not a problem: each commit carries the widget's value, and a
/// change the next snapshot overwrites was one the user just made anyway.
pub fn entity_field_row(
    ui: &mut egui::Ui,
    entity: Entity,
    component: &str,
    field: &str,
    value: &FieldValue,
    pending: &mut PendingEditorCommands,
) {
    match value {
        FieldValue::Text(current) => {
            let mut edited = current.clone();
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(field).color(style::TEXT_MUTED));
                let response = ui.add(egui::TextEdit::singleline(&mut edited).desired_width(160.0));
                if response.lost_focus() && edited != *current {
                    let (command, label) =
                        inspector::field_edit(entity, component, field, FieldValue::Text(edited));
                    pending.push_recorded(
                        vec![command],
                        label,
                        Some(inspector::field_coalesce_key(entity, component, field)),
                    );
                }
            });
        }
        FieldValue::Array(elements) if elements.iter().all(is_scalar_field) => {
            for (index, element) in elements.iter().enumerate() {
                let label = format!("{field}[{index}]");
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(&label).color(style::TEXT_MUTED));
                    if let Some(edited) = scalar_edit(ui, element) {
                        let (command, commit_label) =
                            inspector::element_edit(entity, component, field, index, edited);
                        pending.push_recorded(
                            vec![command],
                            commit_label,
                            Some(inspector::field_coalesce_key(entity, component, &label)),
                        );
                    }
                });
            }
        }
        other => {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(field).color(style::TEXT_MUTED));
                match scalar_edit(ui, other) {
                    Some(edited) => {
                        let (command, label) =
                            inspector::field_edit(entity, component, field, edited);
                        pending.push_recorded(
                            vec![command],
                            label,
                            Some(inspector::field_coalesce_key(entity, component, field)),
                        );
                    }
                    None => {
                        ui.label(format_field_value(other));
                    }
                }
            });
        }
    }
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

/// The edit control for one scalar: a checkbox for booleans, a drag value
/// (drag to scrub, click to type) for numbers. `None` when the kind has no
/// widget, so the caller paints it read-only.
pub fn scalar_edit(ui: &mut egui::Ui, value: &FieldValue) -> Option<FieldValue> {
    match value {
        FieldValue::Bool(current) => {
            let mut edited = *current;
            ui.checkbox(&mut edited, "")
                .changed()
                .then_some(FieldValue::Bool(edited))
        }
        FieldValue::F32(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited).speed(0.05))
                .changed()
                .then_some(FieldValue::F32(edited))
        }
        FieldValue::F64(current) => {
            let mut edited = *current;
            ui.add(
                egui::DragValue::new(&mut edited)
                    .speed(0.05)
                    .max_decimals(9),
            )
            .changed()
            .then_some(FieldValue::F64(edited))
        }
        FieldValue::I8(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::I8(edited))
        }
        FieldValue::I16(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::I16(edited))
        }
        FieldValue::I32(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::I32(edited))
        }
        FieldValue::I64(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::I64(edited))
        }
        FieldValue::U8(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::U8(edited))
        }
        FieldValue::U16(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::U16(edited))
        }
        FieldValue::U32(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::U32(edited))
        }
        FieldValue::U64(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::U64(edited))
        }
        FieldValue::Usize(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::Usize(edited))
        }
        _ => None,
    }
}

/// One read-only (name, value) row.
pub fn read_only_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(label).color(style::TEXT_MUTED));
        ui.label(value);
    });
}

/// The settings document's rows: objects and arrays nest their children,
/// every scalar leaf edits.
pub fn asset_settings_rows(
    ui: &mut egui::Ui,
    asset_path: &str,
    settings: &JsonValue,
    pending: &mut PendingEditorCommands,
) {
    match settings {
        JsonValue::Object(entries) => {
            for (key, value) in entries {
                asset_settings_row(ui, asset_path, settings, value, key.clone(), pending);
            }
        }
        other => {
            read_only_row(ui, "settings", &other.to_string());
        }
    }
}

/// One settings row: a leaf gets the widget its JSON kind asks for (its
/// commit carries the whole patched document), a container nests.
pub fn asset_settings_row(
    ui: &mut egui::Ui,
    asset_path: &str,
    root: &JsonValue,
    value: &JsonValue,
    key: String,
    pending: &mut PendingEditorCommands,
) {
    match value {
        JsonValue::Object(entries) => {
            ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
            ui.indent(key.as_str(), |ui| {
                for (child_key, child) in entries {
                    asset_settings_row(
                        ui,
                        asset_path,
                        root,
                        child,
                        format!("{key}.{child_key}"),
                        pending,
                    );
                }
            });
        }
        JsonValue::Array(elements) => {
            ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
            ui.indent(key.as_str(), |ui| {
                for (index, child) in elements.iter().enumerate() {
                    asset_settings_row(
                        ui,
                        asset_path,
                        root,
                        child,
                        format!("{key}[{index}]"),
                        pending,
                    );
                }
            });
        }
        JsonValue::Bool(current) => {
            let mut edited = *current;
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
                if ui.checkbox(&mut edited, "").changed() {
                    settings_commit(pending, asset_path, root, &key, JsonValue::Bool(edited));
                }
            });
        }
        JsonValue::Number(current) => {
            let mut integer_edit = current
                .as_i64()
                .or_else(|| current.as_u64().map(|value| value as i64));
            match integer_edit.as_mut() {
                Some(edited) => {
                    let mut changed = false;
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
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
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
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
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
                let response = ui.add(egui::TextEdit::singleline(&mut edited).desired_width(160.0));
                if response.lost_focus() && edited != *current {
                    settings_commit(pending, asset_path, root, &key, JsonValue::String(edited));
                }
            });
        }
        other => {
            read_only_row(ui, key.as_str(), &other.to_string());
        }
    }
}

/// Queue the recorded edit that puts `value` at `key` in the settings; the
/// command carries the whole patched document, the native contract.
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
    if let Err(message) = inspector::patch_settings(&mut document, &path, value) {
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

/// The Inspector's rows: one (type name, field rows) pair per component of
/// the detail, each field row a (name, formatted value) pair.
pub fn component_rows(detail: &EntityDetail) -> Vec<(String, Vec<(String, String)>)> {
    detail
        .components
        .iter()
        .map(|component| {
            let fields = component
                .fields
                .iter()
                .zip(&component.values)
                .map(|(descriptor, value)| (descriptor.name.to_string(), format_field_value(value)))
                .collect();
            (component.type_name.clone(), fields)
        })
        .collect()
}

/// A field value as display text; the kinds without their own formatting
/// fall back to their debug form.
pub fn format_field_value(value: &FieldValue) -> String {
    match value {
        FieldValue::F32(number) => format!("{number}"),
        FieldValue::F64(number) => format!("{number}"),
        FieldValue::I8(number) => format!("{number}"),
        FieldValue::I16(number) => format!("{number}"),
        FieldValue::I32(number) => format!("{number}"),
        FieldValue::I64(number) => format!("{number}"),
        FieldValue::U8(number) => format!("{number}"),
        FieldValue::U16(number) => format!("{number}"),
        FieldValue::U32(number) => format!("{number}"),
        FieldValue::U64(number) => format!("{number}"),
        FieldValue::Usize(number) => format!("{number}"),
        FieldValue::Bool(flag) => format!("{flag}"),
        FieldValue::Text(text) => text.clone(),
        other => format!("{other:?}"),
    }
}

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
    egui::SidePanel::left("pill_editor_shell_egui.assets_tree")
        .resizable(true)
        .default_width(ASSETS_TREE_WIDTH)
        .width_range(ASSETS_TREE_WIDTH_RANGE)
        .show_inside(ui, |ui| {
            assets_tree(ui, snapshot, browser, selection, pending);
        });
    egui::TopBottomPanel::top("pill_editor_shell_egui.assets_path").show_inside(ui, |ui| {
        assets_path_bar(ui, browser);
    });
    egui::TopBottomPanel::bottom("pill_editor_shell_egui.assets_zoom").show_inside(ui, |ui| {
        assets_zoom_bar(ui, &snapshot.assets, browser);
    });
    egui::CentralPanel::default().show_inside(ui, |ui| {
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
            if empty.secondary_clicked() {
                log_context_menu("assets");
            }
            let opened = empty.context_menu(|ui| {
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
            mark_menu_region(opened, overlay_regions);
        });
}

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
        log_context_menu("assets");
    }
    let opened = response.context_menu(|ui| {
        asset_menu_items(
            ui,
            snapshot,
            row,
            writes.selection,
            writes.dialog,
            writes.pending,
        )
    });
    mark_menu_region(opened, writes.overlay_regions);
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

#[cfg(test)]
mod tests {
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
        let _ = context.run(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| {
                entities_tab(ui, &snapshot, &mut selection, &mut pending, &mut regions);
                systems_tab(ui, &snapshot, &mut pending);
                inspector_tab(ui, &snapshot, &selection, &mut pending, &mut regions);
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
        let _ = context.run(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| {
                inspector_tab(ui, &snapshot, &selection, &mut pending, &mut regions);
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
}
