//! The shell's UI work: state, docking layout, panel dispatch and painting.
//!
//! # Responsibilities
//!
//! - Owns [`EditorShellState`], the resource holding the dock layout, the
//!   widget state, the text engine, the theme, the stylesheet and the atlas.
//! - Lays the dock out through `pill_editor_core`, paints the chrome and
//!   every visible panel's subframe into the frame the executable drains.
//!
//! # Design
//!
//! Everything here is called by `editor_shell_update` in the crate root,
//! which is the piece the hot patch machinery compiles separately - so the
//! helpers it calls are `pub` and imported through the package name
//! (`pill_editor_shell_editor::shell::...`), the pattern the editor shell
//! follows exactly like a project's hot file.
//!
//! A panel is built and painted at the origin and the list is translated to
//! the panel's dock rectangle afterwards, so panels never know where they
//! are. Hit rectangles are stored per widget id across all panels: each
//! panel's build must hit-test against the previous frame's full map, so the
//! map is restored around every panel and the panels' fresh rectangles are
//! merged into one for the next frame.

// Standard library
use std::collections::BTreeMap;

// External crates
use pill_core::math::Vector2f;
use pill_editor_api::EditorShellFrame;
use pill_editor_core::layout::{
    LayoutModel, LayoutNode, LayoutSnapshot, PanelKind, Rect as LayoutRect,
};
use pill_engine::Resource;
use pill_ui::panel::ClipboardRequest;
use pill_ui::widget::WidgetId;
use pill_ui::{
    paint, run_frame, Atlas, DrawList, PanelBuilder, Rect, Stylesheet, TextSystem, Theme, UiState,
};

// =============================================================================
// Constants
// =============================================================================

/// The shell UI's atlas size; one texture holds every glyph and icon the
/// editor draws.
const ATLAS_SIZE: (u32, u32) = (2048, 2048);

/// The native editor's own layout file, beside the classic editor's.
const NATIVE_LAYOUT_FILE: &str = "editor_layout_native.json";

/// The classic (Dioxus) editor's layout file, read once for migration.
const CLASSIC_LAYOUT_FILE: &str = "editor_layout.json";

// =============================================================================
// EditorShellState
// =============================================================================

/// Everything the shell owns, as one resource so a reload keeps it whole.
///
/// Not persistable: the value survives reloads through the resource-claim
/// contract (every `register` re-claims it), not through serialization, and
/// its types (the widget state, the text engine) are process state rather
/// than document state.
pub struct EditorShellState {
    /// The dock layout: the tree, the selected tabs, the weights.
    pub layout: LayoutModel,
    /// The UI widgets' state: interaction, scroll offsets, expansion.
    pub ui: UiState,
    /// Shaping and rasterization for every text the shell draws.
    pub text: TextSystem,
    /// The built-in theme, under the stylesheet.
    pub theme: Theme,
    /// The editor's stylesheet; the built-in theme until one exists.
    pub sheet: Stylesheet,
    /// The glyph and icon atlas the executable uploads from.
    pub atlas: Atlas,
    /// Frames the shell has drawn, for the Statistics panel and the
    /// reload-proof heartbeat.
    pub frame_index: u64,
}

impl EditorShellState {
    /// The state's first value: the native layout file when one exists,
    /// the classic editor's file migrated once when only that exists, the
    /// default workspace otherwise.
    pub fn new() -> EditorShellState {
        EditorShellState {
            layout: load_native_layout(),
            ui: UiState::new(),
            text: TextSystem::new(),
            theme: Theme::default(),
            sheet: Stylesheet::parse(pill_ui::DEFAULT_SHEET),
            atlas: Atlas::new(ATLAS_SIZE.0, ATLAS_SIZE.1),
            frame_index: 0,
        }
    }
}

impl Default for EditorShellState {
    fn default() -> EditorShellState {
        EditorShellState::new()
    }
}

impl Resource for EditorShellState {}

// =============================================================================
// Layout files
// =============================================================================

/// The native layout file: loaded when valid, migrated from the classic
/// editor's file once, or the default workspace.
///
/// The native editor never writes the classic file (D30/Stage 25: the
/// native file becomes the editor's only layout file, and the classic file
/// is read here exactly once).
fn load_native_layout() -> LayoutModel {
    let directory = pill_editor_core::layout::layout_directory();
    // Step 1: the native file wins when it parses and validates.
    let native_path = directory.join(NATIVE_LAYOUT_FILE);
    if native_path.is_file() {
        match std::fs::read_to_string(&native_path) {
            Ok(text) => match serde_json::from_str::<LayoutModel>(&text) {
                Ok(model) if model.validate().is_ok() => return model,
                Ok(_) => pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    path = %native_path.display(),
                    "[editor] saved layout is invalid; using the default workspace"
                ),
                Err(error) => pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    path = %native_path.display(),
                    error = %error,
                    "[editor] saved layout could not be parsed; using the default workspace"
                ),
            },
            Err(error) => pill_core::warn!(
                target: pill_core::telemetry::telemetry_target::ECS,
                path = %native_path.display(),
                error = %error,
                "[editor] saved layout could not be read; using the default workspace"
            ),
        }
        return LayoutModel::default_editor();
    }
    // Step 2: one-time migration from the classic editor's file. Known
    // panel kinds map across unchanged (same model, same schema); module
    // panels - the classic editor's extension panels - are dropped by name,
    // and a layout emptied by the drops falls back to the default.
    let classic_path = directory.join(CLASSIC_LAYOUT_FILE);
    if classic_path.is_file() {
        let mut model = pill_editor_core::layout::load_or_default();
        let module_tabs: Vec<(pill_editor_core::layout::NodeId, String)> = model
            .nodes
            .iter()
            .filter_map(|(id, node)| match node {
                LayoutNode::Tab(tab) if matches!(tab.panel, PanelKind::Module(_)) => {
                    Some((*id, tab.title.clone()))
                }
                _ => None,
            })
            .collect();
        for (_, title) in &module_tabs {
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                title = %title,
                "[editor] layout migration dropped panel"
            );
        }
        if !module_tabs.is_empty() {
            let mut emptied = false;
            for node in model.nodes.values_mut() {
                let LayoutNode::TabSet(tabset) = node else {
                    continue;
                };
                tabset
                    .tabs
                    .retain(|id| !module_tabs.iter().any(|(dropped, _)| dropped == id));
                if tabset.tabs.is_empty() {
                    emptied = true;
                } else if tabset
                    .selected
                    .is_some_and(|selected| !tabset.tabs.contains(&selected))
                {
                    tabset.selected = tabset.tabs.first().copied();
                }
            }
            for (id, _) in &module_tabs {
                model.nodes.remove(id);
            }
            if emptied || model.validate().is_err() {
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    "[editor] classic layout could not be fully migrated; using the default workspace"
                );
                model = LayoutModel::default_editor();
            }
        }
        save_native_layout(&model);
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            dropped = module_tabs.len(),
            "[editor] classic layout migrated to the native layout file"
        );
        return model;
    }
    // Step 3: nothing saved anywhere.
    LayoutModel::default_editor()
}

/// Write the native layout file, creating the state directory on first use.
fn save_native_layout(model: &LayoutModel) {
    let directory = pill_editor_core::layout::layout_directory();
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let path = directory.join(NATIVE_LAYOUT_FILE);
    match serde_json::to_string_pretty(model) {
        Ok(text) => {
            if let Err(error) = std::fs::write(&path, text) {
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    path = %path.display(),
                    error = %error,
                    "[editor] layout file could not be written"
                );
            }
        }
        Err(error) => pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ECS,
            error = %error,
            "[editor] layout could not be serialized"
        ),
    }
}

// =============================================================================
// Panel placement
// =============================================================================

/// One visible panel: its kind and its rectangle in logical pixels.
///
/// Opaque to the crate root: it collects the placements and hands them to
/// [`paint_panels`] unchanged.
pub struct PanelPlacement {
    kind: PanelKind,
    rect: LayoutRect,
}

/// The selected panel of every tabset, with its content rectangle.
///
/// The snapshot keys selected-panel rectangles by the selected *tab* node,
/// which is what makes a tab's rectangle appear the moment it is selected.
pub fn collect_panel_placements(
    layout: &LayoutModel,
    snapshot: &LayoutSnapshot,
) -> Vec<PanelPlacement> {
    let mut placements = Vec::new();
    for node in layout.nodes.values() {
        let LayoutNode::TabSet(tabset) = node else {
            continue;
        };
        let Some(selected) = tabset.selected else {
            continue;
        };
        let Some(tab) = layout.tab(selected) else {
            continue;
        };
        let Some(rect) = snapshot.selected_panel_rects.get(&selected) else {
            continue;
        };
        placements.push(PanelPlacement {
            kind: tab.panel.clone(),
            rect: *rect,
        });
    }
    placements
}

/// The number of visible panels, for the heartbeat and the frame report.
pub fn panel_count(placements: &[PanelPlacement]) -> usize {
    placements.len()
}

/// Select the first tab whose title matches `title`, activating its tab set;
/// reports whether a tab was found.
///
/// The tab keeps its place in the layout: only the selection moves, so the
/// switch between the Scene and Game views (D29) neither re-docks nor
/// re-saves anything on its own.
pub fn select_tab_by_title(layout: &mut LayoutModel, title: &str) -> bool {
    let found = layout.nodes.iter().find_map(|(tab_id, node)| match node {
        LayoutNode::Tab(tab) if tab.title.eq_ignore_ascii_case(title) => Some(*tab_id),
        _ => None,
    });
    let Some(tab_id) = found else {
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ECS,
            title = %title,
            "[editor] tab selection requested for an unknown title"
        );
        return false;
    };
    let mut selected_set = None;
    for (set_id, node) in layout.nodes.iter_mut() {
        let LayoutNode::TabSet(tabset) = node else {
            continue;
        };
        if tabset.tabs.contains(&tab_id) {
            tabset.selected = Some(tab_id);
            selected_set = Some(*set_id);
            break;
        }
    }
    if let Some(set_id) = selected_set {
        layout.active_tabset = Some(set_id);
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            title = %title,
            "[editor] tab selected"
        );
        return true;
    }
    false
}

// =============================================================================
// Painting
// =============================================================================

/// A panel's painted list, translated to `origin`, with the frame's
/// clipboard requests collected along the way.
///
/// The argument list is the state's pieces, split so the borrow checker
/// sees each field separately; one `&mut EditorShellState` would not allow
/// building the panel while painting reads the theme.
#[allow(clippy::too_many_arguments)]
fn subframe(
    ui: &mut UiState,
    text: &mut TextSystem,
    theme: &Theme,
    sheet: &mut Stylesheet,
    atlas: &mut Atlas,
    origin: Vector2f,
    size: Vector2f,
    build: impl FnOnce(&mut PanelBuilder<'_>),
) -> (DrawList, Vec<ClipboardRequest>) {
    let (output, layout) = run_frame(ui, text, theme, sheet, size, build);
    let mut list = DrawList::new();
    paint(&output.tree, &layout, theme, ui, text, atlas, &mut list);
    list.translate(origin);
    (list, output.clipboard)
}

/// The docking chrome: a raised strip with the tab titles for every tabset,
/// and one thin line per splitter.
pub fn paint_dock_chrome(
    state: &mut EditorShellState,
    snapshot: &LayoutSnapshot,
    frame: &mut EditorShellFrame,
    previous_hits: &BTreeMap<WidgetId, Rect>,
) {
    let EditorShellState {
        layout,
        ui,
        text,
        theme,
        sheet,
        atlas,
        ..
    } = state;
    // Step 1: the tab strips. The fill goes into the frame first so it sits
    // under the titles painted after it.
    for (id, node) in &layout.nodes {
        let LayoutNode::TabSet(tabset) = node else {
            continue;
        };
        let Some(strip) = snapshot.tab_strip_rects.get(id) else {
            continue;
        };
        let strip_rect = Rect::from_min_size(
            Vector2f::new(strip.x as f32, strip.y as f32),
            Vector2f::new(strip.width as f32, strip.height as f32),
        );
        frame.panels.push_fill(strip_rect, theme.raised);
        // The titles as a label row over the strip. The chrome has no
        // interactive widgets, so its build may replace the hit map; the
        // panel loop restores its own copy around each panel.
        ui.set_hit_rects(previous_hits.clone());
        let (mut list, clipboard) = subframe(
            ui,
            text,
            theme,
            sheet,
            atlas,
            strip_rect.min,
            Vector2f::new(strip_rect.width(), strip_rect.height()),
            |builder| {
                builder.row(|row| {
                    for tab_id in &tabset.tabs {
                        if let Some(tab) = layout.tab(*tab_id) {
                            row.label(&tab.title);
                        }
                    }
                });
            },
        );
        frame.panels.append(std::mem::take(&mut list));
        frame.clipboard.extend(clipboard);
    }
    // Step 2: the splitters, one thin line each.
    for splitter in &snapshot.splitters {
        let rect = Rect::from_min_size(
            Vector2f::new(splitter.rect.x as f32, splitter.rect.y as f32),
            Vector2f::new(splitter.rect.width as f32, splitter.rect.height as f32),
        );
        frame.panels.push_fill(rect, theme.border);
    }
}

/// The visible panels' contents, merged into `frame` with their hit
/// rectangles restored around each build.
pub fn paint_panels(
    state: &mut EditorShellState,
    placements: &[PanelPlacement],
    frame: &mut EditorShellFrame,
    editor_frames_per_second: f32,
    previous_hits: &BTreeMap<WidgetId, Rect>,
) {
    let EditorShellState {
        ui,
        text,
        theme,
        sheet,
        atlas,
        ..
    } = state;
    let mut merged: BTreeMap<WidgetId, Rect> = BTreeMap::new();
    for placement in placements {
        // The scene and game tabs' rectangles belong to their surfaces; the
        // shell draws nothing there.
        if placement.kind == PanelKind::Scene || placement.kind == PanelKind::Game {
            continue;
        }
        // Restore the previous frame's map so this panel's widgets test
        // against it, then capture this panel's fresh rectangles.
        ui.set_hit_rects(previous_hits.clone());
        let (list, clipboard) = subframe(
            ui,
            text,
            theme,
            sheet,
            atlas,
            Vector2f::new(placement.rect.x as f32, placement.rect.y as f32),
            Vector2f::new(placement.rect.width as f32, placement.rect.height as f32),
            |builder| build_panel(builder, &placement.kind, editor_frames_per_second),
        );
        merged.extend(ui.hit_rects().iter().map(|(id, rect)| (*id, *rect)));
        frame.panels.append(list);
        frame.clipboard.extend(clipboard);
    }
    ui.set_hit_rects(merged);
}

/// One panel's content by kind. Placeholders stand until the stage that
/// fills them; the gallery is real, proving the widget vocabulary.
fn build_panel(builder: &mut PanelBuilder<'_>, kind: &PanelKind, editor_frames_per_second: f32) {
    match kind {
        PanelKind::Scene => {
            builder.label("The scene draws into its own surface.");
        }
        PanelKind::Game => {
            builder.label("The game view draws into its own surface.");
        }
        PanelKind::Statistics => statistics_panel(builder, editor_frames_per_second),
        PanelKind::Gallery => pill_ui::ui_gallery(builder),
        other => {
            builder.heading(other.title());
            builder.separator();
            builder.label("This panel arrives in a later stage.");
        }
    }
}

/// The Statistics panel: the editor's frame rate and a first action row.
fn statistics_panel(builder: &mut PanelBuilder<'_>, editor_frames_per_second: f32) {
    builder.heading("Statistics");
    builder.separator();
    builder.label(&format!("{editor_frames_per_second:.1} FPS"));
    builder.separator();
    builder.row(|row| {
        row.button_enabled("Reset Layout", true);
        row.button_enabled("Detach Scene", false);
    });
}
