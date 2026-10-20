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
//!
//! # Patchability
//!
//! A generated patch crate holds one function body and reaches this module
//! only through the crate root's re-export, so every item an edited body can
//! name - a sibling function, a constant, a type - is `pub` here. A body that
//! calls a helper by bare name is therefore fine; a body that names a private
//! item fails to compile as a patch and the host reloads the module instead.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;

/// The shared names every shell module globs.
pub mod prelude;

/// The [`claim`] module: the generation's claim on the shared egui context.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod claim;

pub use claim::claim_context_once;
pub use claim::claim_shell_context;

/// The Systems tab: pause/step and the recorded system toggles.
///
/// Public like every tab module: the build script's function inventory
/// names each addressable function by its module path, so the module a
/// patchable function lives in cannot be private.
pub mod tab_systems;

pub use tab_systems::systems_tab;

/// The [`toasts`] module: the host's notices drawn as a short-lived stack.
///
/// Public like every tab module: the build script's function inventory
/// names each addressable function by its module path, so the module cannot
/// be private.
pub mod toasts;

/// The [`menubar`] module: the menu bar: the rows' menus and the actions they run.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod bar;

pub use bar::about_window;
pub use bar::menu_bar;
pub use bar::menu_body;
pub use bar::run_menu_action;
pub use bar::MENU_TITLE_GAP;

/// The [`views`] module: the scene and game viewports: their geometry and overlay chips.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod views;

pub use views::frame_counter_rect;
pub use views::scene_body_rect;
pub use views::scene_overlay;
pub use views::shell_rect;
pub use views::SCENE_BUTTON_GAP;
pub use views::SCENE_BUTTON_WIDTH;
pub use views::SCENE_COUNTER_HEIGHT;
pub use views::SCENE_COUNTER_MARGIN;
pub use views::SCENE_COUNTER_WIDTH;
pub use views::SCENE_RECT_OVERSHOOT;
pub use views::SLOW_UPDATE_MICROS;

/// The [`chrome`] module: the chrome both tabs share: icons, selection moves and logged rects.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod chrome;

pub use chrome::apply_tab_redock;
pub use chrome::context_menu;
pub use chrome::empty_area;
pub use chrome::gizmo_mode_label;
pub use chrome::gizmo_space_label;
pub use chrome::log_context_menu;
pub use chrome::mark_menu_region;
pub use chrome::remember_moved_rect;
pub use chrome::select_asset;
pub use chrome::select_entity;
pub use chrome::tab_icon;
pub use chrome::SPACE_ROWS;
pub use chrome::TOOL_ROWS;

/// The [`menus`] module: the context menus the tabs open.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod menus;

pub use menus::component_menu_items;
pub use menus::scene_context_menu;

/// The [`toolbox`] module: the floating toolbox over a scene viewport.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod toolbox_panel;

pub use toolbox_panel::toolbox_overlay;

/// The [`tab_entities`] module: the entities tab: the entity rows and their menu.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod tab_entities;

pub use tab_entities::entities_tab;
pub use tab_entities::entity_menu_items;
pub use tab_entities::entity_rows;

/// The [`tab_preferences`] module: the preferences tab: the editor-wide rows and the project's gizmo state.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod tab_preferences;

pub use tab_preferences::preferences_tab;

/// The [`tab_inspector`] module: the inspector tab: the selection's components and their rows.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod tab_inspector;

pub use tab_inspector::addable_components;
pub use tab_inspector::entity_inspector;
pub use tab_inspector::inspector_add_popup;
pub use tab_inspector::inspector_header;
pub use tab_inspector::inspector_section_header;
pub use tab_inspector::inspector_tab;
pub use tab_inspector::INSPECTOR_LABEL_WIDTH;
pub use tab_inspector::INSPECTOR_VECTOR_FIELD_WIDTH;

/// The [`inspector_fields`] module: the inspector's field rows: the decorators, the row model and the.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod inspector_fields;

pub use inspector_fields::field_widget_name;
pub use inspector_fields::inspector_field_row;
pub use inspector_fields::inspector_row;
pub use inspector_fields::is_scalar_field;
pub use inspector_fields::remember_inspector_row;

/// The [`inspector_widgets`] module: the inspector's field controls.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod inspector_widgets;

pub use inspector_widgets::ascending;
pub use inspector_widgets::component_rows;
pub use inspector_widgets::format_field_value;
pub use inspector_widgets::integer_edit;
pub use inspector_widgets::read_only_row;
pub use inspector_widgets::scalar_edit;
pub use inspector_widgets::scalar_edit_decorated;
pub use inspector_widgets::Number;

/// The [`inspector_settings`] module: the inspector's asset import settings.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod inspector_settings;

pub use inspector_settings::asset_settings_row;
pub use inspector_settings::asset_settings_rows;
pub use inspector_settings::settings_commit;

/// The [`tab_assets`] module: the assets tab: the folder tree and the open folder's grid.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod tab_assets;

pub use tab_assets::assets_grid;
pub use tab_assets::assets_path_bar;
pub use tab_assets::assets_tab;
pub use tab_assets::assets_tree;
pub use tab_assets::assets_zoom_bar;
pub use tab_assets::folder_tree_rows;
pub use tab_assets::ASSETS_TREE_CARET_SIZE;
pub use tab_assets::ASSETS_TREE_CARET_WIDTH;
pub use tab_assets::ASSETS_TREE_INDENT;
pub use tab_assets::ASSETS_TREE_WIDTH;
pub use tab_assets::ASSETS_TREE_WIDTH_RANGE;

/// The [`assets_tiles`] module: the assets tab's tiles, their menu and the asset dialog.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod assets_tiles;

pub use assets_tiles::asset_dialog_window;
pub use assets_tiles::asset_icon;
pub use assets_tiles::asset_menu_items;
pub use assets_tiles::asset_tile;
pub use assets_tiles::tile_name_job;
pub use assets_tiles::AssetTileWrites;
pub use assets_tiles::ASSETS_TILE_CORNER_RADIUS;
pub use assets_tiles::ASSETS_TILE_GAP;
pub use assets_tiles::ASSETS_TILE_ICON_INSET;
pub use assets_tiles::ASSETS_TILE_LABEL_GAP;
pub use assets_tiles::ASSETS_TILE_LABEL_HEIGHT;
pub use assets_tiles::ASSETS_TILE_LABEL_ROWS;
pub use assets_tiles::ASSETS_TILE_LABEL_SIZE;
pub use toasts::{draw_toasts, push_notices};

/// The [`tests`] module: the shell's unit tests.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
#[cfg(test)]
pub mod tests;

// =============================================================================
// Constants
// =============================================================================

// Public like every item a patched body may name: a patch crate sees this
// module through the crate root's re-export, and a private constant would
// fail its compile (`error[E0425]`) after which the host reloads.

/// One shell frame: the shell's own events become egui input, the dock runs,
/// and the output becomes the executable's frame. The asset dialog, when one
/// is open, is drawn over the dock.
pub fn run_frame(
    state: &mut EguiShellState,
    shell_input: &EguiShellInput,
    frame: &mut EguiShellFrame,
    snapshot: &EditorSnapshot,
    selection: &mut EditorSelection,
    pending: &mut PendingEditorCommands,
    notices: &EditorNotices,
) {
    state.frame_index = state.frame_index.wrapping_add(1);
    let frame_index = state.frame_index;
    // The host's notices become toasts before the dock draws, so a reload that
    // happened this frame is on screen in this frame.
    push_notices(state, notices);
    // The row heartbeat: the editor suite reads it to prove both tabs see
    // the project's data, that the frame counter - and with it the session
    // resource - survives a reload or patch, and which entity the
    // Inspector's detail tier describes.
    if frame_index.is_multiple_of(210) {
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
            frames = frame_index,
            entities = snapshot.entities.len(),
            systems = snapshot.systems.len(),
            paused = snapshot.paused,
            detail = detail_entity.as_str(),
            detail_components,
            "[editor] egui shell rows"
        );
    }
    // The first frame of a shell generation owns the shared egui context:
    // whatever the previous generation left in its temporary data would be a
    // dangling read once this one draws (claim.rs has the reasoning).
    claim_shell_context(&state.context);
    // The UI heartbeat: one full shell update (input conversion, the egui
    // dock and its tabs, the frame conversion) in microseconds. The shell
    // benchmark measures the D32 UI-redraw case from it; the GPU overlay
    // draw is outside it, as presentation is vsync-bound.
    let ui_started = pill_core::platform::Instant::now();
    let raw_input = pill_ui_egui::input::raw_input(shell_input);
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
        inspector,
        toasts,
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
        inspector,
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
        // The toast stack floats over the dock and every viewport it covers,
        // so its rows join the frame's overlay regions.
        pill_editor_shell_egui_editor::shell::draw_toasts(
            toasts,
            frame_index,
            context,
            &mut viewer.overlay_regions,
        );
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
    let ui_micros = ui_started.elapsed().as_micros() as u64;
    // The periodic heartbeat, plus any update that runs past the D32 UI
    // budget: the suites prove the counter advances, and the benchmark gets
    // samples even when 210 frames take longer than its measurement window.
    if frame_index.is_multiple_of(210) || ui_micros >= SLOW_UPDATE_MICROS {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            frames = frame_index,
            ui_us = ui_micros,
            "[editor] egui frame"
        );
    }
}

// =============================================================================
// The tab viewer and the tabs
// =============================================================================

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
    /// The Inspector's collapsed sections and its logged geometry.
    pub inspector: &'a mut InspectorSession,
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
                scene_context_menu(self, ui);
                scene_overlay(ui, self.frames_per_second, &mut self.overlay_regions);
                toolbox_overlay(self, ui, 0);
            }
            EditorTab::SecondScene => {
                self.scene_rects[1] = Some(scene_body_rect(ui));
                scene_context_menu(self, ui);
                scene_overlay(ui, self.frames_per_second, &mut self.overlay_regions);
                toolbox_overlay(self, ui, 1);
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
                self.inspector,
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
