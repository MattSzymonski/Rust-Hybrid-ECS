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
use pill_core::platform::Instant;
use pill_editor_api::panels::{EditorPanelRegistry, PanelContext, PanelWork};
use pill_editor_api::EditorShellFrame;
use pill_editor_core::commands::{
    menu_tree, CommandInbox, CommandRegistry, CommandSpec, EditorPreferences, KeybindingsFile,
    PendingEditorCommands,
};
use pill_editor_core::editor_state::{EditorCommand, EditorSnapshot};
use pill_editor_core::history::{CommandHistory, HistoryEntryKind};
use pill_editor_core::layout::{
    LayoutAction, LayoutModel, LayoutNode, LayoutSnapshot, PanelKind, Rect as LayoutRect,
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

/// The keybindings file: the active keymap preset and binding overrides.
const KEYBINDINGS_FILE: &str = "keybindings.json";

/// The preferences file (D31).
const PREFERENCES_FILE: &str = "preferences.json";

/// The menu bar's height, in logical pixels; the dock layout gets what is
/// left below it.
pub const MENU_BAR_HEIGHT: f64 = 26.0;

/// One dropdown or palette row's height, in logical pixels.
const MENU_ITEM_HEIGHT: f64 = 24.0;

/// How wide a top-level menu title is, in logical pixels (monospace rows).
const MENU_TITLE_WIDTH: f64 = 84.0;

/// The palette's size, in logical pixels.
const PALETTE_WIDTH: f64 = 420.0;
const PALETTE_HEIGHT: f64 = 260.0;

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
    /// The top-level menu currently open, when one is.
    pub menu_open: Option<String>,
    /// Whether the command palette is open, and its search text.
    pub palette_query: Option<String>,
    /// The modifiers currently held, tracked from the input stream so a key
    /// event can become a chord.
    pub modifiers: pill_engine::Modifiers,
    /// The cursor's last logical position, for menu hit-testing.
    pub cursor: Vector2f,
    /// The keybindings file's raw text last applied; a change re-applies it
    /// live (Stage 14).
    pub keybindings_text: Option<String>,
    /// The preferences file's raw text last read.
    pub preferences_text: Option<String>,
    /// The last registry shape logged, so the ready line follows module
    /// registrations instead of repeating.
    pub registry_signature: (usize, usize),
    /// Problems already reported once (a conflict printed every frame would
    /// drown the log).
    pub reported: std::collections::BTreeSet<String>,
    /// Whether the engine's systems were paused as of this frame's input.
    pub paused: bool,
    /// Top-level menu titles with their hit rectangles, from the last paint.
    pub menu_bar_hits: Vec<(String, LayoutRect)>,
    /// The open dropdown's rectangle, from the last paint.
    pub menu_dropdown_rect: Option<LayoutRect>,
    /// The open dropdown's command rows, from the last paint.
    pub menu_dropdown_item_hits: Vec<(String, LayoutRect)>,
    /// The palette's rectangle, from the last paint.
    pub palette_rect: Option<LayoutRect>,
    /// The palette's command rows, from the last paint.
    pub palette_item_hits: Vec<(String, LayoutRect)>,
    /// Build time of every module panel last frame, in milliseconds (Stage
    /// 16's budget: measured, shown in Statistics, never throttled).
    pub panel_millis: std::collections::BTreeMap<String, f32>,
    /// Consecutive over-budget frames per module panel, for the warning.
    pub panel_over_frames: std::collections::BTreeMap<String, u32>,
    /// Module panels currently over budget; their tabs carry the badge.
    pub panel_over: std::collections::BTreeSet<String>,
    /// The live panel set last synced into the command registry.
    pub panel_registry_signature: Vec<(String, String, String)>,
    /// Log-once keys for the panel layer (registered, built, missing).
    pub panel_reported: std::collections::BTreeSet<String>,
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
            menu_open: None,
            palette_query: None,
            modifiers: pill_engine::Modifiers::default(),
            cursor: Vector2f::ZERO,
            keybindings_text: None,
            preferences_text: None,
            registry_signature: (0, 0),
            reported: std::collections::BTreeSet::new(),
            paused: false,
            menu_bar_hits: Vec::new(),
            menu_dropdown_rect: None,
            menu_dropdown_item_hits: Vec::new(),
            palette_rect: None,
            palette_item_hits: Vec::new(),
            panel_millis: std::collections::BTreeMap::new(),
            panel_over_frames: std::collections::BTreeMap::new(),
            panel_over: std::collections::BTreeSet::new(),
            panel_registry_signature: Vec::new(),
            panel_reported: std::collections::BTreeSet::new(),
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
        panel_over,
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
                            // An over-budget module panel's tab carries the
                            // warning badge (Stage 16).
                            match &tab.panel {
                                PanelKind::Module(id) if panel_over.contains(id) => {
                                    row.label(&format!("{}{}", tab.title, PANEL_BADGE));
                                }
                                _ => {
                                    row.label(&tab.title);
                                }
                            }
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
///
/// Module panels are built through the registry (Stage 16): a missing
/// registration paints the placeholder, a panicking one is contained, and
/// every build's duration feeds the budget badge. The returned [`PanelWork`]
/// is what the panels handed back; the caller drains it into the command
/// queues.
pub fn paint_panels(
    state: &mut EditorShellState,
    placements: &[PanelPlacement],
    frame: &mut EditorShellFrame,
    snapshot: &EditorSnapshot,
    panels: &EditorPanelRegistry,
    editor_frames_per_second: f32,
    previous_hits: &BTreeMap<WidgetId, Rect>,
) -> PanelWork {
    let EditorShellState {
        ui,
        text,
        theme,
        sheet,
        atlas,
        panel_millis,
        panel_over_frames,
        panel_over,
        panel_reported,
        ..
    } = state;
    let mut merged: BTreeMap<WidgetId, Rect> = BTreeMap::new();
    let mut collected = PanelWork::default();
    for placement in placements {
        // The scene and game tabs' rectangles belong to their surfaces; the
        // shell draws nothing there.
        if placement.kind == PanelKind::Scene || placement.kind == PanelKind::Game {
            continue;
        }
        // Restore the previous frame's map so this panel's widgets test
        // against it, then capture this panel's fresh rectangles.
        ui.set_hit_rects(previous_hits.clone());
        let mut work = PanelWork::default();
        let origin = Vector2f::new(placement.rect.x as f32, placement.rect.y as f32);
        let size = Vector2f::new(placement.rect.width as f32, placement.rect.height as f32);
        // A module panel may panic; the shell contains it and keeps going
        // (the plan's rule: the panel shows its error, everything else
        // survives). Build time is measured for the Statistics panel and
        // the over-budget badge, and nothing is ever throttled.
        let started = Instant::now();
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            subframe(ui, text, theme, sheet, atlas, origin, size, |builder| {
                build_panel(
                    builder,
                    &placement.kind,
                    editor_frames_per_second,
                    snapshot,
                    panels,
                    panel_millis,
                    &placement.rect,
                    &mut work,
                )
            })
        }));
        let millis = started.elapsed().as_secs_f32() * 1000.0;
        let (list, clipboard) = match built {
            Ok(built) => built,
            Err(payload) => {
                let message = panic_message(&payload);
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    id = placement.kind.title(),
                    message = message.as_str(),
                    "[editor] module panel panicked"
                );
                subframe(ui, text, theme, sheet, atlas, origin, size, |builder| {
                    builder.heading(placement.kind.title());
                    builder.separator();
                    builder.label("This panel panicked; see the log.");
                })
            }
        };
        // The budget tracking and the log-once lines, for module panels.
        if let PanelKind::Module(id) = &placement.kind {
            panel_millis.insert(id.clone(), millis);
            if millis > PANEL_BUDGET_MILLIS {
                let over_frames = panel_over_frames.entry(id.clone()).or_insert(0);
                *over_frames += 1;
                if *over_frames == PANEL_OVER_FRAMES && !panel_over.contains(id) {
                    panel_over.insert(id.clone());
                    pill_core::warn!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        id = id.as_str(),
                        millis,
                        "[editor] panel over budget"
                    );
                }
            } else {
                panel_over_frames.insert(id.clone(), 0);
                panel_over.remove(id);
            }
            let built_key = format!("built:{id}");
            if !panel_reported.contains(&built_key) && panels.panel(id).is_some() {
                panel_reported.insert(built_key);
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    id = id.as_str(),
                    millis,
                    "[editor] module panel built"
                );
            }
            let missing_key = format!("missing:{id}");
            if panels.panel(id).is_none() && !panel_reported.contains(&missing_key) {
                panel_reported.insert(missing_key);
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    id = id.as_str(),
                    "[editor] module panel missing"
                );
            }
        }
        merged.extend(ui.hit_rects().iter().map(|(id, rect)| (*id, *rect)));
        frame.panels.append(list);
        frame.clipboard.extend(clipboard);
        collected.batches.extend(work.batches);
        collected.commands.extend(work.commands);
    }
    ui.set_hit_rects(merged);
    collected
}

/// A module panel build lasting longer than this (milliseconds) counts
/// against the budget; nothing is throttled, the badge just warns.
const PANEL_BUDGET_MILLIS: f32 = 4.0;

/// Consecutive over-budget frames before the warning badge and the log.
const PANEL_OVER_FRAMES: u32 = 120;

/// The badge a tab title carries while its panel is over budget.
const PANEL_BADGE: &str = " [!]";

/// One panel's content by kind. Built-in placeholders stand until the stage
/// that fills them; module panels are built through their registration
/// (Stage 16); the gallery is real, proving the widget vocabulary.
fn build_panel(
    builder: &mut PanelBuilder<'_>,
    kind: &PanelKind,
    editor_frames_per_second: f32,
    snapshot: &EditorSnapshot,
    panels: &EditorPanelRegistry,
    panel_millis: &std::collections::BTreeMap<String, f32>,
    rect: &LayoutRect,
    work: &mut PanelWork,
) {
    match kind {
        PanelKind::Scene => {
            builder.label("The scene draws into its own surface.");
        }
        PanelKind::Game => {
            builder.label("The game view draws into its own surface.");
        }
        PanelKind::Statistics => statistics_panel(
            builder,
            editor_frames_per_second,
            snapshot,
            panels,
            panel_millis,
        ),
        PanelKind::Gallery => pill_ui::ui_gallery(builder),
        PanelKind::Module(id) => match panels.panel(id) {
            Some(build) => {
                let mut context = PanelContext {
                    snapshot,
                    rect: pill_editor_api::EditorRect::new(
                        rect.x as f32,
                        rect.y as f32,
                        rect.width as f32,
                        rect.height as f32,
                    ),
                    work,
                };
                build(builder, &mut context);
            }
            None => {
                builder.heading(id);
                builder.separator();
                builder.label("module not loaded");
            }
        },
        other => {
            builder.heading(other.title());
            builder.separator();
            builder.label("This panel arrives in a later stage.");
        }
    }
}

/// The Statistics panel: the editor's frame rate, the module panels' build
/// times, and a first action row.
fn statistics_panel(
    builder: &mut PanelBuilder<'_>,
    editor_frames_per_second: f32,
    snapshot: &EditorSnapshot,
    panels: &EditorPanelRegistry,
    panel_millis: &std::collections::BTreeMap<String, f32>,
) {
    builder.heading("Statistics");
    builder.separator();
    builder.label(&format!("{editor_frames_per_second:.1} FPS"));
    builder.label(&format!("entities={}", snapshot.entities.len()));
    builder.separator();
    for info in panels.list() {
        let millis = panel_millis.get(&info.id).copied().unwrap_or(0.0);
        builder.row(|row| {
            row.label(&format!("panel {} {millis:.1}ms", info.title));
        });
    }
    builder.row(|row| {
        row.button_enabled("Reset Layout", true);
        row.button_enabled("Detach Scene", false);
    });
}

/// The panic payload as text, for the log line that contains it.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_string();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    "non-string panic payload".to_string()
}

// =============================================================================
// Commands (Stage 14)
// =============================================================================

/// Re-reads the keybindings and preferences files when their text changed.
///
/// The keybindings file carries the active keymap preset and the overrides;
/// both apply live, and what changed (or went wrong) is reported once: the
/// preset logs, conflicts name both commands, and parse problems surface as
/// warnings rather than silently keeping a stale map.
pub fn sync_command_files(
    state: &mut EditorShellState,
    registry: &mut CommandRegistry,
    history: &mut CommandHistory,
) {
    let directory = pill_editor_core::layout::layout_directory();
    // Step 1: the keybindings file, when its bytes differ from the last pass.
    let keybindings_text = std::fs::read_to_string(directory.join(KEYBINDINGS_FILE)).ok();
    if keybindings_text != state.keybindings_text {
        state.keybindings_text = keybindings_text.clone();
        if let Some(text) = keybindings_text {
            match serde_json::from_str::<KeybindingsFile>(&text) {
                Ok(file) => {
                    let problems = registry.apply_file(&file);
                    let preset = registry.preset();
                    let overrides = registry.override_count();
                    let conflicts = registry.conflicts();
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        preset = %preset,
                        overrides,
                        "[editor] keymap applied"
                    );
                    for problem in problems {
                        report_once(state, &problem);
                    }
                    for (chord, ids) in conflicts {
                        report_once(
                            state,
                            &format!(
                                "[editor] keybinding conflict  chord=\"{chord}\" commands={}",
                                ids.join(",")
                            ),
                        );
                    }
                }
                Err(error) => {
                    report_once(state, &format!("keybindings: could not parse: {error}"));
                }
            }
        }
    }
    // Step 2: the preferences file (D31); the history limits apply live,
    // the remaining fields arrive with the stages that consume them.
    let preferences_text = std::fs::read_to_string(directory.join(PREFERENCES_FILE)).ok();
    if preferences_text != state.preferences_text {
        state.preferences_text = preferences_text.clone();
        if let Some(text) = preferences_text {
            match serde_json::from_str::<EditorPreferences>(&text) {
                Ok(preferences) => {
                    let step_cap = preferences
                        .history_step_cap
                        .unwrap_or(CommandHistory::DEFAULT_STEP_CAP);
                    let memory_mb = preferences
                        .history_memory_budget_mb
                        .unwrap_or(CommandHistory::DEFAULT_MEMORY_BUDGET_BYTES / (1024 * 1024));
                    let outcome = history.set_limits(step_cap, memory_mb * 1024 * 1024);
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        step_cap,
                        memory_mb,
                        "[editor] preferences applied"
                    );
                    if outcome.trimmed > 0 {
                        pill_core::info!(
                            target: pill_core::telemetry::telemetry_target::ECS,
                            dropped = outcome.trimmed,
                            reason = ?outcome.trim_reason,
                            "[editor] history trimmed"
                        );
                    }
                }
                Err(error) => {
                    report_once(state, &format!("preferences: could not parse: {error}"));
                }
            }
        }
    }
}

/// Reports one problem the first time; repeats are dropped.
fn report_once(state: &mut EditorShellState, message: &str) {
    if state.reported.insert(message.to_string()) {
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ECS,
            message,
            "[editor] command layer problem"
        );
    }
}

// =============================================================================
// Module panels (Stage 16)
// =============================================================================

/// Syncs the Window menu and the dock tabs with the live panel registry.
///
/// Runs when the registry's signature moves - a module registered, reloaded
/// or unloaded - and replaces the "panels" owner's command specs with one
/// entry per panel, so the Window menu lists exactly what is loaded. A
/// `Module(id)` tab carrying a registered title is retitled in place.
pub fn sync_panel_commands(
    state: &mut EditorShellState,
    panels: &EditorPanelRegistry,
    registry: &mut CommandRegistry,
) {
    let signature = panels.signature();
    if signature == state.panel_registry_signature {
        return;
    }
    state.panel_registry_signature = signature.clone();
    // A new generation's panels log their first build (or miss) again.
    state.panel_reported.clear();
    let specs = signature
        .iter()
        .map(|(id, title, _owner)| {
            let command_id = format!("editor.panel.{id}");
            CommandSpec::new(&command_id, title.as_str())
                .menu(&["Window", title])
                .global()
        })
        .collect::<Vec<_>>();
    registry.register("panels", specs);
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        panels = signature.len(),
        "[editor] panel registry ready"
    );
    for (id, title, owner) in &signature {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            id = id.as_str(),
            title = title.as_str(),
            owner = owner.as_str(),
            "[editor] panel registered"
        );
    }
    // A module panel's tab shows its registered title once one exists.
    for (_, node) in state.layout.nodes.iter_mut() {
        let LayoutNode::Tab(tab) = node else {
            continue;
        };
        let PanelKind::Module(id) = &tab.panel else {
            continue;
        };
        let Some((_, title, _)) = signature.iter().find(|(registered, _, _)| registered == id)
        else {
            continue;
        };
        if &tab.title != title {
            tab.title = title.clone();
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                id = id.as_str(),
                title = title.as_str(),
                "[editor] panel tab retitled"
            );
        }
    }
}

/// Focuses the tab of module panel `panel_id`, opening one in the active
/// tabset when the panel is not docked yet. A panel whose module is not
/// loaded still opens: the placeholder is what a restored layout shows.
pub fn focus_module_panel(state: &mut EditorShellState, panel_id: &str) {
    let existing = state.layout.nodes.iter().find_map(|(id, node)| match node {
        LayoutNode::Tab(tab) => match &tab.panel {
            PanelKind::Module(registered) if registered == panel_id => Some(*id),
            _ => None,
        },
        _ => None,
    });
    let outcome = match existing {
        Some(tab) => state
            .layout
            .apply(LayoutAction::SelectTab { tab })
            .map(|_| true),
        None => match state.layout.resolved_active_tabset() {
            Some(target) => state
                .layout
                .apply(LayoutAction::OpenTab {
                    panel: PanelKind::Module(panel_id.to_string()),
                    target_tabset: target,
                })
                .map(|_| false),
            None => {
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    id = panel_id,
                    "[editor] panel focus has no tabset to open into"
                );
                return;
            }
        },
    };
    match outcome {
        Ok(existing) => pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            id = panel_id,
            existing,
            "[editor] panel focused"
        ),
        Err(error) => pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ECS,
            id = panel_id,
            error = %error,
            "[editor] panel focus failed"
        ),
    }
}

/// Executes a registered command by id: logs it, hands it to every module
/// owner through the inbox, and runs the built-ins of this crate.
///
/// Engine mutations are queued as `EditorCommand`s; the executable applies
/// the batch at the frame boundary, which is the only place they may run.
pub fn execute_command(
    state: &mut EditorShellState,
    registry: &CommandRegistry,
    inbox: &mut CommandInbox,
    pending: &mut PendingEditorCommands,
    history: &mut CommandHistory,
    id: &str,
) -> bool {
    if registry.command(id).is_none() {
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ECS,
            id,
            "[editor] command has no registration"
        );
        return false;
    }
    // NOTE: asserted on by the editor's end-to-end suite.
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        id,
        "[editor] command executed"
    );
    inbox.executed.push(id.to_string());
    // A module panel's Window entry names its panel; focusing opens the
    // tab when the panel is not docked yet.
    if let Some(panel_id) = id.strip_prefix("editor.panel.") {
        focus_module_panel(state, panel_id);
        return true;
    }
    match id {
        "editor.pause_toggle" => {
            pending.push(vec![EditorCommand::SetPaused {
                paused: !state.paused,
            }]);
        }
        "editor.step" => pending.push(vec![EditorCommand::StepOnce]),
        "editor.undo" => {
            let undo = match history.pop_undo() {
                Some(entry) => Some(entry),
                None => {
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        "[editor] history undo empty"
                    );
                    None
                }
            };
            if let Some(entry) = undo {
                let label = entry.label.clone();
                match entry.entry {
                    HistoryEntryKind::Engine { inverse, .. } => pending.push(inverse),
                    // Selection entries need the selection state the panels
                    // own (Stage 16+); the entry type carries it already.
                    HistoryEntryKind::Selection { .. } => {}
                }
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    label = label.as_str(),
                    remaining = history.undo_depth(),
                    "[editor] history undo"
                );
            }
        }
        "editor.redo" => {
            let redo = match history.pop_redo() {
                Some(entry) => Some(entry),
                None => {
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        "[editor] history redo empty"
                    );
                    None
                }
            };
            if let Some(entry) = redo {
                let label = entry.label.clone();
                match entry.entry {
                    HistoryEntryKind::Engine { forward, .. } => pending.push(forward),
                    HistoryEntryKind::Selection { .. } => {}
                }
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    label = label.as_str(),
                    remaining = history.redo_depth(),
                    "[editor] history redo"
                );
            }
        }
        "editor.save_layout" => {
            save_native_layout(&state.layout);
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                "[editor] layout saved"
            );
        }
        "editor.reset_layout" => {
            state.layout = LayoutModel::default_editor();
            save_native_layout(&state.layout);
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                "[editor] layout reset"
            );
        }
        "editor.command_palette" => {
            state.palette_query = match state.palette_query.take() {
                Some(_) => {
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        "[editor] command palette closed"
                    );
                    None
                }
                None => {
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        "[editor] command palette opened"
                    );
                    Some(String::new())
                }
            };
        }
        // A module's command: the inbox entry above is what its owning
        // system drains; having a registration is what proves a handler will.
        _ => {}
    }
    true
}

/// Dispatches the frame's key presses through the registry.
///
/// While the palette is open its field owns the keys; otherwise a press that
/// matches an effective binding executes its command, except inside a focused
/// text field, where only a global binding wins (the plan's focus rule).
pub fn handle_command_shortcuts(
    state: &mut EditorShellState,
    registry: &CommandRegistry,
    inbox: &mut CommandInbox,
    pending: &mut PendingEditorCommands,
    history: &mut CommandHistory,
    pressed: &[(pill_engine::KeyCode, pill_engine::Modifiers)],
) {
    if state.palette_query.is_some() {
        return;
    }
    for (key, modifiers) in pressed {
        let resolved = registry
            .resolve(*key, *modifiers)
            .map(|spec| (spec.id.clone(), spec.global));
        let Some((id, global)) = resolved else {
            continue;
        };
        if state.ui.focus().is_some() && !global {
            continue;
        }
        execute_command(state, registry, inbox, pending, history, &id);
    }
}

/// Routes one pointer event to the menu bar, an open dropdown or the palette.
///
/// Returns whether the event was consumed; a consumed event never reaches the
/// panels, so a dropdown item click cannot also press a widget underneath it.
/// The rectangles are the previous paint's, matching how the widgets
/// themselves hit-test.
pub fn handle_menu_pointer(
    state: &mut EditorShellState,
    registry: &CommandRegistry,
    inbox: &mut CommandInbox,
    pending: &mut PendingEditorCommands,
    history: &mut CommandHistory,
    event: &pill_engine::InputEvent,
) -> bool {
    let pill_engine::InputEvent::MouseButton {
        button: pill_engine::MouseButton::Left,
        state: pill_engine::ButtonState::Released,
    } = event
    else {
        return false;
    };
    let cursor = state.cursor;
    let in_rect = |rect: &LayoutRect| {
        cursor.x as f64 >= rect.x
            && cursor.x as f64 <= rect.x + rect.width
            && cursor.y as f64 >= rect.y
            && cursor.y as f64 <= rect.y + rect.height
    };
    // Step 1: the palette owns the pointer while it is open.
    if state.palette_query.is_some() {
        if state.palette_rect.is_some_and(|rect| in_rect(&rect)) {
            let hit = state
                .palette_item_hits
                .iter()
                .find(|(_, rect)| in_rect(rect))
                .map(|(id, _)| id.clone());
            if let Some(id) = hit {
                state.palette_query = None;
                execute_command(state, registry, inbox, pending, history, &id);
            }
            return true;
        }
        state.palette_query = None;
        return true;
    }
    // Step 2: an open dropdown runs its item, or closes.
    if state.menu_open.is_some() {
        if state.menu_dropdown_rect.is_some_and(|rect| in_rect(&rect)) {
            let hit = state
                .menu_dropdown_item_hits
                .iter()
                .find(|(_, rect)| in_rect(rect))
                .map(|(id, _)| id.clone());
            if let Some(id) = hit {
                state.menu_open = None;
                execute_command(state, registry, inbox, pending, history, &id);
            }
            return true;
        }
        // A click on another title switches menus; anything else closes.
        let title = state
            .menu_bar_hits
            .iter()
            .find(|(_, rect)| in_rect(rect))
            .map(|(top, _)| top.clone());
        state.menu_open = match title {
            Some(top) if Some(&top) != state.menu_open.as_ref() => {
                // NOTE: asserted on by the editor's end-to-end suite.
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    path = %top,
                    "[editor] menu opened"
                );
                Some(top)
            }
            _ => None,
        };
        return true;
    }
    // Step 3: the bar itself opens a menu.
    let title = state
        .menu_bar_hits
        .iter()
        .find(|(_, rect)| in_rect(rect))
        .map(|(top, _)| top.clone());
    if let Some(top) = title {
        // NOTE: asserted on by the editor's end-to-end suite.
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            path = %top,
            "[editor] menu opened"
        );
        state.menu_open = Some(top);
        return true;
    }
    false
}

/// The chords a command answers to, as menu text (`space / ctrl+p`).
fn chord_label(registry: &CommandRegistry, id: &str) -> String {
    registry
        .effective_chords(id)
        .iter()
        .map(|chord| chord.to_string())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// A layout rectangle as the UI's rectangle type.
fn layout_rect_to_ui(rect: LayoutRect) -> Rect {
    Rect::from_min_size(
        Vector2f::new(rect.x as f32, rect.y as f32),
        Vector2f::new(rect.width as f32, rect.height as f32),
    )
}

/// The commands of one open menu, as painted: id, title, shortcut text.
type MenuPaint = Vec<(String, Vec<(String, String, String)>)>;

/// Paints the menu bar and, when one is open, its dropdown (Stage 14).
///
/// The bar sits over the top of the client area, above the dock layout; the
/// dropdown paints after the panels, so it covers their content. The hit
/// rectangles the pointer router reads are recorded here, for the next
/// frame's events, the way widgets do.
pub fn paint_menu(
    state: &mut EditorShellState,
    registry: &CommandRegistry,
    frame: &mut EditorShellFrame,
    available: Vector2f,
) {
    let menus: MenuPaint = menu_tree(registry)
        .into_iter()
        .map(|(top, specs)| {
            let items = specs
                .iter()
                .map(|spec| {
                    (
                        spec.id.clone(),
                        spec.title.clone(),
                        chord_label(registry, &spec.id),
                    )
                })
                .collect();
            (top, items)
        })
        .collect();
    state.menu_bar_hits.clear();
    state.menu_dropdown_rect = None;
    state.menu_dropdown_item_hits.clear();
    if menus.is_empty() {
        return;
    }
    {
        let EditorShellState {
            ui,
            text,
            theme,
            sheet,
            atlas,
            ..
        } = state;
        frame.panels.push_fill(
            Rect::from_min_size(
                Vector2f::ZERO,
                Vector2f::new(available.x, MENU_BAR_HEIGHT as f32),
            ),
            theme.raised,
        );
        ui.set_hit_rects(BTreeMap::new());
        let (mut list, clipboard) = subframe(
            ui,
            text,
            theme,
            sheet,
            atlas,
            Vector2f::ZERO,
            Vector2f::new(available.x, MENU_BAR_HEIGHT as f32),
            |builder| {
                builder.row(|row| {
                    for (top, _) in &menus {
                        row.label(top);
                    }
                });
            },
        );
        frame.panels.append(std::mem::take(&mut list));
        frame.clipboard.extend(clipboard);
    }
    // The titles' hit rectangles, matching the uniform layout above.
    let mut x = 6.0;
    for (top, _) in &menus {
        state.menu_bar_hits.push((
            top.clone(),
            LayoutRect::new(x, 0.0, MENU_TITLE_WIDTH, MENU_BAR_HEIGHT),
        ));
        x += MENU_TITLE_WIDTH;
    }
    // The dropdown, when one is open.
    let Some(open) = state.menu_open.clone() else {
        return;
    };
    let Some((top, items)) = menus.iter().find(|(name, _)| *name == open) else {
        state.menu_open = None;
        return;
    };
    let title_x = state
        .menu_bar_hits
        .iter()
        .find(|(name, _)| name == top)
        .map(|(_, rect)| rect.x)
        .unwrap_or(6.0);
    // The view surface is a real child window: anything drawn over its
    // rectangle is covered. Until the composition overlay exists (Stage 13
    // deviation, D24), popups clamp to the area left of it.
    let view_left = frame
        .game_rect
        .or(frame.scene_rect)
        .map(|rect| rect.x as f64)
        .unwrap_or(available.x as f64);
    let width = items
        .iter()
        .map(|(_, title, chord)| (title.chars().count() + chord.chars().count() + 8) as f64 * 7.2)
        .fold(240.0_f64, f64::max)
        .min(460.0)
        .min((view_left - title_x - 8.0).max(200.0));
    let height = items.len() as f64 * MENU_ITEM_HEIGHT + 8.0;
    let rect = LayoutRect::new(title_x, MENU_BAR_HEIGHT, width, height);
    state.menu_dropdown_rect = Some(rect);
    let rows: Vec<String> = items
        .iter()
        .map(|(_, title, chord)| {
            if chord.is_empty() {
                title.clone()
            } else {
                format!("{title}   {chord}")
            }
        })
        .collect();
    {
        let EditorShellState {
            ui,
            text,
            theme,
            sheet,
            atlas,
            ..
        } = state;
        frame
            .panels
            .push_fill(layout_rect_to_ui(rect), theme.window);
        frame.panels.push_fill(
            layout_rect_to_ui(LayoutRect::new(rect.x, rect.y, rect.width, 1.0)),
            theme.border,
        );
        ui.set_hit_rects(BTreeMap::new());
        let (mut list, clipboard) = subframe(
            ui,
            text,
            theme,
            sheet,
            atlas,
            Vector2f::new(rect.x as f32 + 6.0, rect.y as f32 + 4.0),
            Vector2f::new(rect.width as f32 - 12.0, rect.height as f32 - 8.0),
            |builder| {
                for row_text in &rows {
                    builder.label(row_text);
                }
            },
        );
        frame.panels.append(std::mem::take(&mut list));
        frame.clipboard.extend(clipboard);
    }
    // The item hit rectangles, one per row.
    let mut y = rect.y + 4.0;
    for (id, _, _) in items {
        state.menu_dropdown_item_hits.push((
            id.clone(),
            LayoutRect::new(rect.x + 4.0, y, rect.width - 8.0, MENU_ITEM_HEIGHT),
        ));
        y += MENU_ITEM_HEIGHT;
    }
}

/// Paints the command palette when open: a search field and the matching
/// commands, clickable (Stage 14).
pub fn paint_palette(
    state: &mut EditorShellState,
    registry: &CommandRegistry,
    frame: &mut EditorShellFrame,
    available: Vector2f,
) {
    let Some(mut query) = state.palette_query.clone() else {
        state.palette_rect = None;
        state.palette_item_hits.clear();
        return;
    };
    let mut commands: Vec<(String, String, String)> = registry
        .commands()
        .map(|spec| {
            (
                spec.id.clone(),
                spec.title.clone(),
                chord_label(registry, &spec.id),
            )
        })
        .collect();
    let needle = query.to_lowercase();
    commands.retain(|(id, title, _)| {
        needle.is_empty()
            || title.to_lowercase().contains(&needle)
            || id.to_lowercase().contains(&needle)
    });
    // Clamped left of the view surface, like the menu dropdowns: an
    // in-window popup cannot paint over the child window (D24 pending).
    let view_left = frame
        .game_rect
        .or(frame.scene_rect)
        .map(|rect| rect.x as f64)
        .unwrap_or(available.x as f64);
    let width = PALETTE_WIDTH
        .min(view_left - 32.0)
        .max(200.0)
        .min(available.x as f64 - 32.0);
    let height = PALETTE_HEIGHT.min(available.y as f64 - 60.0).max(80.0);
    let rect = LayoutRect::new(16.0, 40.0, width, height);
    state.palette_rect = Some(rect);
    state.palette_item_hits.clear();
    // The rows start below the search field and fit the visible box.
    let row_y = rect.y + MENU_ITEM_HEIGHT + 6.0;
    let visible = commands
        .iter()
        .take(
            (((rect.y + rect.height - row_y) / MENU_ITEM_HEIGHT)
                .floor()
                .max(0.0)) as usize,
        )
        .count();
    let mut y = row_y;
    for (id, _, _) in commands.iter().take(visible) {
        state.palette_item_hits.push((
            id.clone(),
            LayoutRect::new(rect.x + 4.0, y, rect.width - 8.0, MENU_ITEM_HEIGHT),
        ));
        y += MENU_ITEM_HEIGHT;
    }
    let rows: Vec<String> = commands
        .iter()
        .take(visible)
        .map(|(_, title, chord)| {
            if chord.is_empty() {
                title.clone()
            } else {
                format!("{title}   {chord}")
            }
        })
        .collect();
    let mut changed = false;
    {
        let EditorShellState {
            ui,
            text,
            theme,
            sheet,
            atlas,
            ..
        } = state;
        frame
            .panels
            .push_fill(layout_rect_to_ui(rect), theme.window);
        frame.panels.push_fill(
            layout_rect_to_ui(LayoutRect::new(rect.x, rect.y, rect.width, 1.0)),
            theme.border,
        );
        ui.set_hit_rects(BTreeMap::new());
        let (mut list, clipboard) = subframe(
            ui,
            text,
            theme,
            sheet,
            atlas,
            Vector2f::new(rect.x as f32 + 6.0, rect.y as f32 + 2.0),
            Vector2f::new(rect.width as f32 - 12.0, rect.height as f32 - 4.0),
            |builder| {
                changed = builder.text_field("palette_query", &mut query);
                builder.separator();
                for row_text in &rows {
                    builder.label(row_text);
                }
            },
        );
        frame.panels.append(std::mem::take(&mut list));
        frame.clipboard.extend(clipboard);
    }
    if changed {
        state.palette_query = Some(query);
    }
}
