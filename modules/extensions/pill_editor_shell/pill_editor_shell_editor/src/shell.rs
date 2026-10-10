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
use pill_core::telemetry::capture::LogBuffer;
use pill_editor_api::panels::{EditorPanelRegistry, PanelContext, PanelWork};
use pill_editor_api::{
    ClipboardObject, EditorClipboardAnswer, EditorOpenRequest, EditorProblem, EditorProblemLevel,
    EditorShellFrame, EditorShellInput,
};
use pill_editor_core::clipboard::{self, Payload, SeededComponent};
use pill_editor_core::commands::{
    menu_tree, CommandInbox, CommandRegistry, CommandSpec, EditorPreferences, KeybindingsFile,
    PendingEditorCommands,
};
use pill_editor_core::editor_state::{
    ComponentSeed, EditorCommand, EditorSelection, EditorSnapshot,
};
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
/// The preferences file's name, next to the layout file (D31); the shared
/// constant lives in `pill_editor_core` because the executable writes it too.
pub const PREFERENCES_FILE: &str = pill_editor_core::commands::PREFERENCES_FILE;

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
    /// Wall time the panels' paint took on the previous frame, in
    /// microseconds: the heartbeat reports it, so a panel regression is
    /// visible in any log (the D32 measurement found one this way).
    pub last_panels_us: u64,
    /// The clipboard work a command queued this frame (Stage 18): the hot
    /// root runs it where the frame, the snapshot and the clipboard answer
    /// are in scope.
    pub clipboard_action: Option<ClipboardAction>,
    /// The clipboard answer sequence last consumed, so a paste only applies
    /// an answer newer than the one it has seen.
    pub clipboard_seq_seen: u64,
    /// Whether a paste is waiting for the executable's clipboard read.
    pub clipboard_paste_pending: bool,
    /// How many files an OS drag is hovering over the Assets panel with,
    /// when one is (Stage 18); the preview paints from this.
    pub drop_hover: Option<usize>,
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
    /// Build time of every visible panel last frame, keyed by title; the
    /// heartbeat reports the slowest, which is how the D32 measurement's
    /// 222 ms/frame paint regression was pinned to a panel.
    pub panel_draw_millis: std::collections::BTreeMap<String, f32>,
    /// Consecutive over-budget frames per module panel, for the warning.
    pub panel_over_frames: std::collections::BTreeMap<String, u32>,
    /// Module panels currently over budget; their tabs carry the badge.
    pub panel_over: std::collections::BTreeSet<String>,
    /// The live panel set last synced into the command registry.
    pub panel_registry_signature: Vec<(String, String, String)>,
    /// Log-once keys for the panel layer (registered, built, missing).
    pub panel_reported: std::collections::BTreeSet<String>,
    /// The console's captured lines, bounded, oldest first.
    pub console_lines: std::collections::VecDeque<ConsoleLine>,
    /// The newest captured sequence number the console has copied.
    pub console_watermark: u64,
    /// Total lines the console has seen since startup (for its header).
    pub console_total: u64,
    /// The minimum severity the console shows (1 trace .. 5 error).
    pub console_level: u8,
    /// The console's channel filter: 0 all, 1 engine/host, 2 game.
    pub console_channel: u8,
    /// The asset count the shell last reported (log-once on change).
    pub assets_listed: usize,
    /// Every panel widget's id by the name it was built with (including its
    /// row's scope), refreshed every frame; the scripted activation path
    /// addresses widgets through this.
    pub widget_names: std::collections::BTreeMap<String, pill_ui::widget::WidgetId>,
    /// The focus last reported, so a focus change logs once.
    pub last_focus: Option<pill_ui::widget::WidgetId>,
    /// The rectangle of the last activated widget, the anchor a scripted
    /// wheel scrolls at.
    pub last_activated_rect: Option<Rect>,
    /// Persistent text buffers for panel widgets (search fields, inspector
    /// editors), keyed by widget id.
    pub panel_text: std::collections::BTreeMap<String, String>,
    /// The toasts the executable's notices produced (Stage 19), newest
    /// last; each expires by frame count.
    pub toasts: std::collections::VecDeque<Toast>,
    /// The problems revision last handled, so the shell logs a change once.
    pub notices_revision_seen: u64,
    /// The gizmo tool choices (Stage 20) the shell last saw in the
    /// preferences file: what a toggle command negates. The executable
    /// owns the per-project entry and writes it back; the file returning
    /// through `sync_command_files` is what keeps this mirror honest.
    pub gizmo: pill_editor_core::commands::GizmoProjectPreferences,
}

/// One toast: a short-lived line about something the host did (Stage 19).
#[derive(Clone, Debug)]
pub struct Toast {
    /// Error, warning or plain notice; picks the marker.
    pub level: EditorProblemLevel,
    /// The line to show.
    pub text: String,
    /// The frame index at which it disappears.
    pub expires_at_frame: u64,
}

/// Frames a toast stays on screen (about four seconds at 60 fps).
pub const TOAST_FRAMES: u64 = 240;

/// How many toasts show at once; older ones drop off the stack.
const TOASTS_MAX: usize = 5;

/// The toast stack's width and row height, in logical pixels.
const TOAST_WIDTH: f32 = 420.0;
const TOAST_HEIGHT: f32 = 22.0;

/// Classifies a tracing target into the console's channels: 1 for the
/// engine, host and editor subsystems, 2 for everything else (the project
/// and the module crates it loads).
pub fn console_channel(target: &str) -> u8 {
    const ENGINE_PREFIXES: [&str; 10] = [
        "engine",
        "host",
        "editor",
        "pill_core",
        "pill_engine",
        "pill_host",
        "pill_runtime",
        "pill_ui",
        "pill_input",
        "pill_assets",
    ];
    if ENGINE_PREFIXES
        .iter()
        .any(|prefix| target.starts_with(prefix))
    {
        1
    } else {
        2
    }
}

/// Lines the console keeps in the shell (the capture ring is the fallback
/// source, this is the display window).
pub const CONSOLE_LINES_MAX: usize = 2048;

/// One console row, owned by the shell.
#[derive(Clone, Debug)]
pub struct ConsoleLine {
    /// Severity, 1 trace .. 5 error.
    pub level: u8,
    /// The tracing target, shown in the row.
    pub target: String,
    /// The rendered message.
    pub message: String,
    /// The classified channel: 1 engine/host, 2 game.
    pub channel: u8,
}

/// The clipboard work a copy, cut or paste command queued (Stage 18, D28).
///
/// The commands run inside [`execute_command`], which has neither the frame
/// to answer through nor the snapshot to copy from; they record the intent
/// here and the hot root runs it with everything in scope.
#[derive(Clone, Debug, PartialEq)]
pub enum ClipboardAction {
    /// Offer the selection (the entity's detail, or the asset) to the
    /// system clipboard.
    Copy,
    /// Offer it, then remove it as one recorded step.
    Cut,
    /// Offer one component of the selection's entity, by type name (the
    /// Inspector's per-component Copy button).
    CopyComponent {
        /// Registered component type name.
        type_name: String,
    },
    /// Read the clipboard through the executable and apply what it carries.
    Paste,
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
            last_panels_us: 0,
            clipboard_action: None,
            clipboard_seq_seen: 0,
            clipboard_paste_pending: false,
            drop_hover: None,
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
            panel_draw_millis: std::collections::BTreeMap::new(),
            panel_over_frames: std::collections::BTreeMap::new(),
            panel_over: std::collections::BTreeSet::new(),
            panel_registry_signature: Vec::new(),
            panel_reported: std::collections::BTreeSet::new(),
            console_lines: std::collections::VecDeque::new(),
            console_watermark: 0,
            console_total: 0,
            console_level: 3,
            console_channel: 0,
            assets_listed: 0,
            widget_names: std::collections::BTreeMap::new(),
            last_focus: None,
            last_activated_rect: None,
            panel_text: std::collections::BTreeMap::new(),
            toasts: std::collections::VecDeque::new(),
            notices_revision_seen: 0,
            gizmo: pill_editor_core::commands::GizmoProjectPreferences::default(),
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
/// Plain data both the paint pass and the crate root read (the root forwards
/// the Assets panel's rectangle to the executable for scripted drops).
pub struct PanelPlacement {
    /// The panel the placement belongs to.
    pub kind: PanelKind,
    /// Where the panel sits, in logical pixels of the client area.
    pub rect: LayoutRect,
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
) -> (
    DrawList,
    Vec<ClipboardRequest>,
    std::collections::BTreeMap<String, pill_ui::widget::WidgetId>,
) {
    let (output, layout) = run_frame(ui, text, theme, sheet, size, build);
    let mut list = DrawList::new();
    paint(&output.tree, &layout, theme, ui, text, atlas, &mut list);
    list.translate(origin);
    (list, output.clipboard, output.named)
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
        let (mut list, clipboard, _named) = subframe(
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

/// The read-only view every panel builds from (Stage 17).
pub struct PanelView<'a> {
    /// The editor's per-frame snapshot: entities, systems, selections.
    pub snapshot: &'a EditorSnapshot,
    /// The console's capture ring.
    pub logs: &'a LogBuffer,
    /// The shared selection the Hierarchy, Inspector and Assets follow.
    pub selection: &'a EditorSelection,
    /// The live module-panel registry (the Window menu and module tabs).
    pub panels: &'a EditorPanelRegistry,
    /// The host's problems, from the executable's notice feed (Stage 19);
    /// the Problems panel draws them.
    pub problems: &'a [EditorProblem],
}

/// Shell state the built-in panels read or commit through [`PanelWork`].
pub struct PanelExtras<'a> {
    /// Module panels' last build times, shown in Statistics.
    pub panel_millis: &'a std::collections::BTreeMap<String, f32>,
    /// Persistent widget text buffers (search, field editors).
    pub text: &'a mut std::collections::BTreeMap<String, String>,
    /// The console's captured lines, oldest first.
    pub console_lines: &'a std::collections::VecDeque<ConsoleLine>,
    /// The console's minimum severity and channel filters.
    pub console_level: u8,
    /// 0 all, 1 engine/host, 2 game.
    pub console_channel: u8,
    /// The preferences file's current text, when one exists.
    pub preferences_text: Option<&'a str>,
}

/// The visible panels' contents, merged into `frame` with their hit
/// rectangles restored around each build.
///
/// Module panels are built through the registry (Stage 16): a missing
/// registration paints the placeholder, a panicking one is contained, and
/// every build's duration feeds the budget badge. The returned [`PanelWork`]
/// is what the panels handed back; the caller drains it into the command
/// queues, the selection and the console filters.
pub fn paint_panels(
    state: &mut EditorShellState,
    placements: &[PanelPlacement],
    frame: &mut EditorShellFrame,
    view: &PanelView<'_>,
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
        panel_draw_millis,
        panel_over_frames,
        panel_over,
        panel_reported,
        panel_text,
        console_lines,
        console_level,
        console_channel,
        preferences_text,
        widget_names,
        ..
    } = state;
    let mut merged: BTreeMap<WidgetId, Rect> = BTreeMap::new();
    let mut names: BTreeMap<String, WidgetId> = BTreeMap::new();
    let mut collected = PanelWork::default();
    if state.frame_index.is_multiple_of(210) {
        for placement in placements {
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                kind = placement.kind.title(),
                x = placement.rect.x,
                y = placement.rect.y,
                width = placement.rect.width,
                height = placement.rect.height,
                "[editor] panel placement"
            );
        }
    }
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
        // A panel may panic; the shell contains it and keeps going (the
        // plan's rule: the panel shows its error, everything else
        // survives). Build time is measured for the Statistics panel and
        // the over-budget badge, and nothing is ever throttled.
        let started = Instant::now();
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // The extras borrow the shell's text buffers and console window
            // for this panel's build only.
            let mut extras = PanelExtras {
                panel_millis,
                text: &mut *panel_text,
                console_lines,
                console_level: *console_level,
                console_channel: *console_channel,
                preferences_text: preferences_text.as_deref(),
            };
            subframe(ui, text, theme, sheet, atlas, origin, size, |builder| {
                // A scroll area per panel: it clips the content to the dock
                // rectangle (rows that do not fit neither draw nor hit-test
                // outside it) and makes the overflow reachable by wheel.
                let area = format!("panel_scroll_{}", placement.kind.title());
                builder.scroll_area(&area, size.y, |panel| {
                    build_panel(
                        panel,
                        &placement.kind,
                        editor_frames_per_second,
                        view,
                        &mut extras,
                        &placement.rect,
                        &mut work,
                    )
                });
            })
        }));
        let millis = started.elapsed().as_secs_f32() * 1000.0;
        let (list, clipboard, named) = match built {
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
        names.extend(named);
        panel_draw_millis.insert(placement.kind.title().to_string(), millis);
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
            if !panel_reported.contains(&built_key) && view.panels.panel(id).is_some() {
                panel_reported.insert(built_key);
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    id = id.as_str(),
                    millis,
                    "[editor] module panel built"
                );
            }
            let missing_key = format!("missing:{id}");
            if view.panels.panel(id).is_none() && !panel_reported.contains(&missing_key) {
                panel_reported.insert(missing_key);
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    id = id.as_str(),
                    "[editor] module panel missing"
                );
            }
        }
        // The panel's fresh rectangles, moved into client space: the
        // layout produces them in the panel's own frame, while input (and
        // so hit-testing) is in client coordinates.
        merged.extend(ui.hit_rects().iter().map(|(id, rect)| {
            (
                *id,
                Rect::from_min_size(
                    Vector2f::new(rect.min.x + origin.x, rect.min.y + origin.y),
                    Vector2f::new(rect.width(), rect.height()),
                ),
            )
        }));
        frame.panels.append(list);
        frame.clipboard.extend(clipboard);
        collected.batches.extend(work.batches);
        collected.commands.extend(work.commands);
        if work.select_entity.is_some() {
            collected.select_entity = work.select_entity;
        }
        if work.select_asset.is_some() {
            collected.select_asset = work.select_asset;
        }
        if work.console_level.is_some() {
            collected.console_level = work.console_level;
        }
        if work.console_channel.is_some() {
            collected.console_channel = work.console_channel;
        }
        if work.console_search.is_some() {
            collected.console_search = work.console_search;
        }
        if work.preferences.is_some() {
            collected.preferences = work.preferences;
        }
        if work.copy_component.is_some() {
            collected.copy_component = work.copy_component;
        }
        if work.drag_candidate.is_some() {
            collected.drag_candidate = work.drag_candidate;
        }
        if work.open_file.is_some() {
            collected.open_file = work.open_file;
        }
    }
    ui.set_hit_rects(merged);
    // The frame closes here: every panel has seen this frame's release edge,
    // so the drag (if any) ends now, once.
    ui.end_frame();
    *widget_names = names;
    collected
}

/// A module panel build lasting longer than this (milliseconds) counts
/// against the budget; nothing is throttled, the badge just warns.
const PANEL_BUDGET_MILLIS: f32 = 4.0;

/// Consecutive over-budget frames before the warning badge and the log.
const PANEL_OVER_FRAMES: u32 = 120;

/// The badge a tab title carries while its panel is over budget.
const PANEL_BADGE: &str = " [!]";

/// One panel's content by kind: the built-ins of this crate, the registered
/// module panels, or the not-yet stage's placeholder.
fn build_panel(
    builder: &mut PanelBuilder<'_>,
    kind: &PanelKind,
    editor_frames_per_second: f32,
    view: &PanelView<'_>,
    extras: &mut PanelExtras<'_>,
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
        PanelKind::Hierarchy => hierarchy_panel(builder, view, work),
        PanelKind::Inspector => inspector_panel(builder, view, extras, work),
        PanelKind::Systems => systems_panel(builder, view, work),
        PanelKind::Console => console_panel(builder, extras, work),
        PanelKind::Preferences => preferences_panel(builder, extras, work),
        PanelKind::Assets => assets_panel(builder, view, work),
        PanelKind::Problems => problems_panel(builder, view, work),
        PanelKind::Statistics => {
            statistics_panel(builder, editor_frames_per_second, view, extras, work)
        }
        PanelKind::Gallery => pill_ui::ui_gallery(builder),
        PanelKind::Module(id) => match view.panels.panel(id) {
            Some(build) => {
                let mut context = PanelContext {
                    snapshot: view.snapshot,
                    logs: view.logs,
                    selection: view.selection,
                    rect: pill_editor_api::EditorRect::new(
                        rect.x as f32,
                        rect.y as f32,
                        rect.width as f32,
                        rect.height as f32,
                    ),
                    text_buffers: &mut *extras.text,
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
    }
}

/// The Hierarchy: every live entity, the selection highlighted, a click
/// selects (the shared selection the Inspector and Stage 20 follow).
fn hierarchy_panel(builder: &mut PanelBuilder<'_>, view: &PanelView<'_>, work: &mut PanelWork) {
    builder.heading("Hierarchy");
    builder.label(&format!("{} entities", view.snapshot.entities.len()));
    builder.separator();
    let mut selected: Option<pill_engine::Entity> = None;
    let shown = view.snapshot.entities.len().min(PANEL_ROWS);
    for entry in view.snapshot.entities.iter().take(shown) {
        let label = if view.selection.entity == Some(entry.entity) {
            format!("> {} ({})", entry.display, entry.archetype_label)
        } else {
            format!("  {} ({})", entry.display, entry.archetype_label)
        };
        builder.row(|row| {
            if row.button(&label) {
                selected = Some(entry.entity);
            }
        });
    }
    if view.snapshot.entities.len() > shown {
        builder.label(&format!(
            "... {} more",
            view.snapshot.entities.len() - shown
        ));
    }
    if let Some(entity) = selected {
        work.select(entity);
    }
}

/// Rows a list panel paints before summarising the rest.
const PANEL_ROWS: usize = 24;

/// The Inspector: the selected entity's components, every editable field
/// live, all commits recorded `SetField`/`RemoveComponent` edits.
fn inspector_panel(
    builder: &mut PanelBuilder<'_>,
    view: &PanelView<'_>,
    extras: &mut PanelExtras<'_>,
    work: &mut PanelWork,
) {
    builder.heading("Inspector");
    let Some(detail) = view.snapshot.detail.as_ref() else {
        builder.separator();
        match view.selection.entity {
            Some(entity) => builder.label(&format!("entity {entity} is not alive")),
            None => builder.label("Select an entity in the Hierarchy."),
        }
        return;
    };
    builder.label(&format!("entity {}", detail.entity));
    builder.separator();
    for component in &detail.components {
        builder.label(&component.type_name);
        if !component.editable {
            builder.label("  (no editable fields)");
            continue;
        }
        let entity = detail.entity;
        for (index, descriptor) in component.fields.iter().enumerate() {
            let Some(value) = component.values.get(index) else {
                continue;
            };
            let widget_id = format!(
                "inspector.{}.{}.{}",
                detail.entity, component.type_name, descriptor.name
            );
            let buffer = extras
                .text
                .entry(widget_id.clone())
                .or_insert_with(|| format_field_value(value));
            let mut committed = false;
            builder.row(|row| {
                row.label(descriptor.name);
                committed = row.text_field(&widget_id, buffer);
            });
            if committed {
                match parse_field_value(buffer, value) {
                    Ok(new_value) => work.edit_coalesced(
                        vec![EditorCommand::SetField {
                            entity,
                            component: component.type_name.clone(),
                            field: descriptor.name.to_string(),
                            value: new_value,
                        }],
                        format!(
                            "set {}.{}",
                            short_name(&component.type_name),
                            descriptor.name
                        ),
                        widget_id.clone(),
                    ),
                    Err(message) => pill_core::warn!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        field = descriptor.name,
                        message = message.as_str(),
                        "[editor] inspector value not understood"
                    ),
                }
            }
        }
        let (copy, remove) = {
            let mut copy = false;
            let mut remove = false;
            builder.row(|row| {
                copy = row.button(&format!("Copy {}", short_name(&component.type_name)));
                remove = row.button(&format!("Remove {}", short_name(&component.type_name)));
            });
            (copy, remove)
        };
        if copy {
            work.copy_component(component.type_name.clone());
        }
        if remove {
            work.edit(
                vec![EditorCommand::RemoveComponent {
                    entity,
                    component: component.type_name.clone(),
                }],
                format!("remove {}", short_name(&component.type_name)),
            );
        }
        builder.separator();
    }
    // The add-component picker: one button per registered addable type.
    builder.label("Add component");
    let mut add: Option<String> = None;
    for registered in view
        .snapshot
        .components
        .iter()
        .filter(|c| c.addable)
        .take(PANEL_ROWS)
    {
        builder.row(|row| {
            if row.button(&registered.type_name) {
                add = Some(registered.type_name.clone());
            }
        });
    }
    if let Some(component) = add {
        work.edit(
            vec![EditorCommand::AddComponent {
                entity: detail.entity,
                component: component.clone(),
            }],
            format!("add {}", short_name(&component)),
        );
    }
}

/// The Systems tab: every registered system with its toggle (undoable), a
/// pause/resume button and a single-step button.
fn systems_panel(builder: &mut PanelBuilder<'_>, view: &PanelView<'_>, work: &mut PanelWork) {
    builder.heading("Systems");
    builder.separator();
    let pause_label = if view.snapshot.paused {
        "Resume"
    } else {
        "Pause"
    };
    let mut pause = false;
    let mut step = false;
    builder.row(|row| {
        pause = row.button(pause_label);
        step = row.button("Step");
    });
    if pause {
        work.command("editor.pause_toggle");
    }
    if step {
        work.command("editor.step");
    }
    builder.separator();
    let mut toggle: Option<(usize, bool)> = None;
    for system in view.snapshot.systems.iter().take(PANEL_ROWS) {
        let state_label = if system.enabled { "on" } else { "off" };
        let name = if system.name_is_ambiguous {
            format!("{} ({}) [ambiguous]", system.name, system.owner_label)
        } else {
            format!("{} ({})", system.name, system.owner_label)
        };
        builder.row(|row| {
            row.label(&name);
            if row.button(state_label) {
                toggle = Some((system.index, !system.enabled));
            }
        });
    }
    if let Some((index, enabled)) = toggle {
        work.edit(
            vec![EditorCommand::SetSystemEnabled { index, enabled }],
            format!("system {} {}", index, if enabled { "on" } else { "off" }),
        );
    }
}

/// The Console: captured engine/host and game lines, the shared command
/// failures, a level filter, a channel filter and a live search.
fn console_panel(
    builder: &mut PanelBuilder<'_>,
    extras: &mut PanelExtras<'_>,
    work: &mut PanelWork,
) {
    builder.heading("Console");
    let search = extras.text.entry("console_search".to_string()).or_default();
    let mut changed = false;
    builder.row(|row| {
        row.label("search");
        changed = row.text_field("console_search_field", search);
    });
    let mut level = None;
    let mut channel = None;
    builder.row(|row| {
        if row.button("Errors") {
            level = Some(5);
        }
        if row.button("Warnings+") {
            level = Some(4);
        }
        if row.button("Info+") {
            level = Some(3);
        }
        if row.button("All levels") {
            level = Some(1);
        }
    });
    builder.row(|row| {
        if row.button("Engine") {
            channel = Some(1);
        }
        if row.button("Game") {
            channel = Some(2);
        }
        if row.button("All channels") {
            channel = Some(0);
        }
    });
    let needle = search.to_lowercase();
    let visible: Vec<&ConsoleLine> = extras
        .console_lines
        .iter()
        .filter(|line| line.level >= extras.console_level)
        .filter(|line| extras.console_channel == 0 || line.channel == extras.console_channel)
        .filter(|line| {
            needle.is_empty()
                || line.message.to_lowercase().contains(&needle)
                || line.target.to_lowercase().contains(&needle)
        })
        .collect();
    builder.label(&format!(
        "{} of {} lines (level {}, channel {})",
        visible.len(),
        extras.console_lines.len(),
        extras.console_level,
        extras.console_channel
    ));
    builder.separator();
    // Newest first, a bounded window.
    for line in visible.iter().rev().take(PANEL_ROWS).rev() {
        let severity = match line.level {
            5 => "ERROR",
            4 => "WARN",
            3 => "INFO",
            2 => "DEBUG",
            _ => "TRACE",
        };
        builder.label(&format!("{severity} {}: {}", line.target, line.message));
    }
    if let Some(level) = level {
        work.console_level = Some(level);
    }
    if let Some(channel) = channel {
        work.console_channel = Some(channel);
    }
    if changed {
        work.console_search = Some(search.clone());
    }
}

/// The Preferences panel (D31): edits the preferences file; the file and the
/// panel stay in step because both sides go through
/// `sync_command_files`' reader.
fn preferences_panel(
    builder: &mut PanelBuilder<'_>,
    extras: &mut PanelExtras<'_>,
    work: &mut PanelWork,
) {
    builder.heading("Preferences");
    builder.label("Preferences file (live-reloaded; blank keeps the default).");
    // The file's current document, parsed once: it seeds the fields, and it
    // is the base the save writes back into, so keys this panel does not own
    // (the theme, the executable's per-project gizmo entry) survive a save.
    let mut document: serde_json::Map<String, serde_json::Value> = extras
        .preferences_text
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    // Seed the buffers once from the file, so the fields start where the
    // file is; later edits live in the buffers until saved.
    if !extras.text.contains_key("preferences_seeded") {
        let current = serde_json::Value::Object(document.clone());
        let seed =
            |key: &str, source: &str, buffer: &mut std::collections::BTreeMap<String, String>| {
                let value = current
                    .get(source)
                    .map(|value| match value {
                        serde_json::Value::String(text) => text.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default();
                buffer.entry(format!("preferences_{key}")).or_insert(value);
            };
        seed("preset", "keymap_preset", extras.text);
        seed("code_editor", "code_editor_command", extras.text);
        seed("autosave", "autosave_interval_seconds", extras.text);
        seed("step_cap", "history_step_cap", extras.text);
        seed("memory_mb", "history_memory_budget_mb", extras.text);
        // The snapping steps (Stage 20) are nested; seed each from its own
        // key with the default the gizmo uses when the file has none.
        let steps = current.get("snapping_steps");
        let mut seed_step = |key: &str, source: &str, fallback: &str| {
            let value = steps
                .and_then(|steps| steps.get(source))
                .map(|value| value.to_string())
                .unwrap_or_else(|| fallback.to_string());
            extras
                .text
                .entry(format!("snapping_{key}"))
                .or_insert(value);
        };
        seed_step("translation", "translation", "0.5");
        seed_step("rotation", "rotation_degrees", "15");
        seed_step("scale", "scale", "0.1");
        extras
            .text
            .insert("preferences_seeded".to_string(), "1".to_string());
    }
    // The tools row: the same commands the shortcuts and the menu fire. It
    // closes the panel rather than opening it, so the form's first row
    // stays the first thing a short dock shows.
    builder.separator();
    let mut save = false;
    for (key, label) in [
        ("preset", "keymap preset"),
        ("code_editor", "code editor command"),
        ("autosave", "autosave seconds"),
        // The snapping steps sit above the history limits: together with the
        // Save button they fit the dock without scrolling past each other.
        ("snapping_translation", "snap translate step"),
        ("snapping_rotation", "snap rotate degrees"),
        ("snapping_scale", "snap scale step"),
        ("step_cap", "undo step cap"),
        ("memory_mb", "undo memory MB"),
    ] {
        let buffer = extras.text.entry(format!("preferences_{key}")).or_default();
        builder.row(|row| {
            row.label(label);
            row.text_field(&format!("preferences_field_{key}"), buffer);
        });
    }
    builder.row(|row| {
        save = row.button("Save preferences");
    });
    // The gizmo tool row (Stage 20): the same commands the shortcuts and the
    // menu fire, for a user who would rather click the mode than chord it.
    builder.label("Gizmo tool");
    builder.row(|row| {
        for (label, id) in [
            ("Translate", "editor.tool_translate"),
            ("Rotate", "editor.tool_rotate"),
            ("Scale", "editor.tool_scale"),
            ("Snap", "editor.tool_snap_toggle"),
            ("Local/World", "editor.tool_space_toggle"),
        ] {
            if row.button(label) {
                work.command(id);
            }
        }
    });
    if save {
        let take =
            |key: &str, source: &str, target: &mut serde_json::Map<String, serde_json::Value>| {
                let text = extras
                    .text
                    .get(&format!("preferences_{key}"))
                    .cloned()
                    .unwrap_or_default();
                let text = text.trim();
                if text.is_empty() {
                    return;
                }
                // Bare JSON first, so a number or a boolean round-trips;
                // anything else is the string it looks like.
                let parsed = serde_json::from_str::<serde_json::Value>(text)
                    .or_else(|_| serde_json::from_str::<serde_json::Value>(&format!("\"{text}\"")))
                    .unwrap_or_else(|_| serde_json::Value::String(text.to_string()));
                target.insert(source.to_string(), parsed);
            };
        take("preset", "keymap_preset", &mut document);
        take("code_editor", "code_editor_command", &mut document);
        take("autosave", "autosave_interval_seconds", &mut document);
        take("step_cap", "history_step_cap", &mut document);
        take("memory_mb", "history_memory_budget_mb", &mut document);
        // The snapping steps belong together, as the gizmo reads them.
        let number = |key: &str, fallback: f64| -> f64 {
            extras
                .text
                .get(&format!("preferences_{key}"))
                .and_then(|text| text.trim().parse::<f64>().ok())
                .unwrap_or(fallback)
        };
        document.insert(
            "snapping_steps".to_string(),
            serde_json::json!({
                "translation": number("snapping_translation", 0.5),
                "rotation_degrees": number("snapping_rotation", 15.0),
                "scale": number("snapping_scale", 0.1),
            }),
        );
        work.preferences = Some(serde_json::Value::Object(document).to_string());
    }
}

/// The Problems panel (Stage 19): the host's build and patch failures, a
/// row per problem, and a click that opens the file at its line.
///
/// The list is the executable's; the panel only draws it and asks. A row
/// without a file - a load failure, a failed patch - has nowhere to jump to
/// and stays inert.
fn problems_panel(builder: &mut PanelBuilder<'_>, view: &PanelView<'_>, work: &mut PanelWork) {
    builder.heading("Problems");
    let errors = view
        .problems
        .iter()
        .filter(|problem| problem.level == EditorProblemLevel::Error)
        .count();
    builder.label(&format!(
        "{} problems ({errors} errors)",
        view.problems.len()
    ));
    builder.label("Click a row to open the file at its line.");
    builder.separator();
    if view.problems.is_empty() {
        builder.label("No build or patch problems.");
        return;
    }
    let mut open: Option<EditorOpenRequest> = None;
    for (index, problem) in view.problems.iter().take(PANEL_ROWS * 2).enumerate() {
        let location = match (&problem.file, problem.line) {
            (Some(file), Some(line)) => format!("{file}:{line}"),
            (Some(file), None) => file.clone(),
            _ => String::new(),
        };
        let severity = match problem.level {
            EditorProblemLevel::Error => "error",
            EditorProblemLevel::Warning => "warn",
            EditorProblemLevel::Note => "note",
        };
        let label = format!(
            "{severity} [{}] {location} {}",
            problem.module, problem.message
        );
        let target = problem.file.clone().map(|file| EditorOpenRequest {
            file,
            line: problem.line.unwrap_or(0),
        });
        // The row is addressed by a stable name (`problems.open.N`) because
        // its visible text carries the compiler's own words; the scripted
        // channel clicks a row by the name, never by the message.
        builder.row(|row| {
            if row.button_named(&format!("problems.open.{index}"), &label) {
                open = target;
            }
        });
    }
    if view.problems.len() > PANEL_ROWS * 2 {
        builder.label(&format!(
            "... {} more",
            view.problems.len() - PANEL_ROWS * 2
        ));
    }
    if let Some(request) = open {
        work.open_file(request);
    }
}

/// The Assets: the browser's rows, a click selects for the shared selection,
/// and a row held down is offered to an OS drag (Stage 18).
fn assets_panel(builder: &mut PanelBuilder<'_>, view: &PanelView<'_>, work: &mut PanelWork) {
    builder.heading("Assets");
    builder.label(&format!("{} files", view.snapshot.assets.len()));
    builder.separator();
    let mut selected: Option<String> = None;
    let mut held: Option<String> = None;
    for asset in view.snapshot.assets.iter().take(PANEL_ROWS * 2) {
        let label = if view.selection.asset.as_deref() == Some(asset.path.as_str()) {
            format!("> {} [{}]", asset.path, asset.kind)
        } else {
            format!("  {} [{}]", asset.path, asset.kind)
        };
        builder.row(|row| {
            let (clicked, is_held) = row.button_held(&label);
            if clicked {
                selected = Some(asset.path.clone());
            }
            if is_held {
                held = Some(asset.path.clone());
            }
        });
    }
    if view.snapshot.assets.len() > PANEL_ROWS * 2 {
        builder.label(&format!(
            "... {} more",
            view.snapshot.assets.len() - PANEL_ROWS * 2
        ));
    }
    if let Some(path) = selected {
        work.select_asset(path);
    }
    if let Some(path) = held {
        work.drag_candidate(path);
    }
}

/// The Statistics panel: the editor's frame rate, the module panels' build
/// times, and a first action row.
fn statistics_panel(
    builder: &mut PanelBuilder<'_>,
    editor_frames_per_second: f32,
    view: &PanelView<'_>,
    extras: &mut PanelExtras<'_>,
    work: &mut PanelWork,
) {
    builder.heading("Statistics");
    builder.separator();
    builder.label(&format!("{editor_frames_per_second:.1} FPS"));
    builder.label(&format!("entities={}", view.snapshot.entities.len()));
    builder.label(&format!(
        "console lines={} (total {})",
        extras.console_lines.len(),
        view.logs.latest_seq()
    ));
    builder.separator();
    for info in view.panels.list() {
        let millis = extras.panel_millis.get(&info.id).copied().unwrap_or(0.0);
        builder.row(|row| {
            row.label(&format!("panel {} {millis:.1}ms", info.title));
        });
    }
    let mut reset = false;
    builder.row(|row| {
        reset = row.button("Reset Layout");
        row.button_enabled("Detach Scene", false);
    });
    if reset {
        work.command("editor.reset_layout");
    }
}

/// The name after the last `::`, for labels and history entries.
fn short_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// A field value as editable text.
fn format_field_value(value: &pill_engine::FieldValue) -> String {
    match value {
        pill_engine::FieldValue::F32(number) => format!("{number}"),
        pill_engine::FieldValue::F64(number) => format!("{number}"),
        pill_engine::FieldValue::I8(number) => format!("{number}"),
        pill_engine::FieldValue::I16(number) => format!("{number}"),
        pill_engine::FieldValue::I32(number) => format!("{number}"),
        pill_engine::FieldValue::I64(number) => format!("{number}"),
        pill_engine::FieldValue::U8(number) => format!("{number}"),
        pill_engine::FieldValue::U16(number) => format!("{number}"),
        pill_engine::FieldValue::U32(number) => format!("{number}"),
        pill_engine::FieldValue::U64(number) => format!("{number}"),
        pill_engine::FieldValue::Usize(number) => format!("{number}"),
        pill_engine::FieldValue::Bool(flag) => format!("{flag}"),
        pill_engine::FieldValue::Text(text) => text.clone(),
        other => format!("{other:?}"),
    }
}

/// Parse an edited field's text back into the value's own kind.
fn parse_field_value(
    text: &str,
    current: &pill_engine::FieldValue,
) -> Result<pill_engine::FieldValue, String> {
    use pill_engine::FieldValue;
    let text = text.trim();
    let parsed = match current {
        FieldValue::F32(_) => FieldValue::F32(text.parse().map_err(|_| "not a float")?),
        FieldValue::F64(_) => FieldValue::F64(text.parse().map_err(|_| "not a float")?),
        FieldValue::I8(_) => FieldValue::I8(text.parse().map_err(|_| "not an integer")?),
        FieldValue::I16(_) => FieldValue::I16(text.parse().map_err(|_| "not an integer")?),
        FieldValue::I32(_) => FieldValue::I32(text.parse().map_err(|_| "not an integer")?),
        FieldValue::I64(_) => FieldValue::I64(text.parse().map_err(|_| "not an integer")?),
        FieldValue::U8(_) => FieldValue::U8(text.parse().map_err(|_| "not an unsigned")?),
        FieldValue::U16(_) => FieldValue::U16(text.parse().map_err(|_| "not an unsigned")?),
        FieldValue::U32(_) => FieldValue::U32(text.parse().map_err(|_| "not an unsigned")?),
        FieldValue::U64(_) => FieldValue::U64(text.parse().map_err(|_| "not an unsigned")?),
        FieldValue::Usize(_) => FieldValue::Usize(text.parse().map_err(|_| "not an unsigned")?),
        FieldValue::Bool(_) => FieldValue::Bool(text.parse().map_err(|_| "not a boolean")?),
        FieldValue::Text(_) => FieldValue::Text(text.to_string()),
        other => return Err(format!("`{other:?}` fields are not editable here")),
    };
    Ok(parsed)
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
                    // The gizmo tool choices (Stage 20): the file returning
                    // after the executable writes it back is what keeps the
                    // shell's mirror in step, so toggling reads from here.
                    state.gizmo = preferences
                        .gizmo
                        .as_ref()
                        .map(
                            |gizmo| pill_editor_core::commands::GizmoProjectPreferences {
                                mode: gizmo.mode,
                                space: gizmo.space,
                                snap: gizmo.snap,
                            },
                        )
                        .unwrap_or_default();
                }
                Err(error) => {
                    report_once(state, &format!("preferences: could not parse: {error}"));
                }
            }
        }
    }
}

/// Queue one gizmo tool change, remembering it in the shell's mirror.
///
/// The mirror is what a later toggle negates before the file comes back
/// around; the executable applies the batch and persists the choice under
/// the current project's name, which it alone knows.
fn gizmo_command(
    state: &mut EditorShellState,
    pending: &mut PendingEditorCommands,
    mode: Option<pill_editor_core::gizmo::GizmoMode>,
    space: Option<pill_editor_core::gizmo::GizmoSpace>,
    snap: Option<bool>,
) {
    if let Some(mode) = mode {
        state.gizmo.mode = Some(mode);
    }
    if let Some(space) = space {
        state.gizmo.space = Some(space);
    }
    if let Some(snap) = snap {
        state.gizmo.snap = Some(snap);
    }
    // NOTE: asserted on by the editor's end-to-end suite.
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        mode = ?state.gizmo.mode,
        space = ?state.gizmo.space,
        snap = ?state.gizmo.snap,
        "[editor] gizmo tool"
    );
    pending.push(vec![EditorCommand::SetGizmoTool { mode, space, snap }]);
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

/// Synthesizes a click on the widget called `name`.
///
/// Panel widgets built at their panel's root scope are addressed by name
/// (`WidgetId::from_name`), so the scripted channel can activate a button or
/// focus a field without knowing the layout: the click lands on the
/// rectangle the last paint recorded, through the same input path real
/// input takes.
pub fn activate_widget(state: &mut EditorShellState, name: &str) {
    // Prefer the id the panel actually built the widget under (a widget in
    // a row derives its id from the row's scope); plain panel-root names
    // fall back to the single-segment form.
    let id = state
        .widget_names
        .get(name)
        .copied()
        .unwrap_or_else(|| pill_ui::widget::WidgetId::from_name(name));
    let Some(rect) = state.ui.hit_rects().get(&id).copied() else {
        let names: Vec<&String> = state.widget_names.keys().collect();
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ECS,
            name,
            hits = state.ui.hit_rects().len(),
            names = ?names,
            "[editor] widget not found"
        );
        return;
    };
    state.last_activated_rect = Some(rect);
    let center = Vector2f::new(
        rect.min.x + rect.width() / 2.0,
        rect.min.y + rect.height() / 2.0,
    );
    for event in [
        pill_engine::InputEvent::CursorMoved { position: center },
        pill_engine::InputEvent::MouseButton {
            button: pill_engine::MouseButton::Left,
            state: pill_engine::ButtonState::Pressed,
        },
        pill_engine::InputEvent::MouseButton {
            button: pill_engine::MouseButton::Left,
            state: pill_engine::ButtonState::Released,
        },
    ] {
        state.ui.input.apply(&event);
    }
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        name,
        "[editor] widget activated"
    );
}

/// Synthesizes a wheel at the last activated widget: the area under it
/// scrolls exactly as it does under a real wheel there.
pub fn scroll_last_widget(state: &mut EditorShellState, lines: f32) {
    let Some(rect) = state.last_activated_rect else {
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ECS,
            "[editor] wheel with no activated widget"
        );
        return;
    };
    let center = Vector2f::new(
        rect.min.x + rect.width() / 2.0,
        rect.min.y + rect.height() / 2.0,
    );
    for event in [
        pill_engine::InputEvent::CursorMoved { position: center },
        pill_engine::InputEvent::MouseWheel {
            delta: pill_engine::ScrollDelta::Lines(Vector2f::new(0.0, lines)),
        },
    ] {
        state.ui.input.apply(&event);
    }
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        lines,
        "[editor] widget wheeled"
    );
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
        // The transform tools (Stage 20). The shell flips its mirror, the
        // executable applies the change to the gizmo state and writes the
        // per-project preference.
        "editor.tool_translate" => gizmo_command(
            state,
            pending,
            Some(pill_editor_core::gizmo::GizmoMode::Translate),
            None,
            None,
        ),
        "editor.tool_rotate" => gizmo_command(
            state,
            pending,
            Some(pill_editor_core::gizmo::GizmoMode::Rotate),
            None,
            None,
        ),
        "editor.tool_scale" => gizmo_command(
            state,
            pending,
            Some(pill_editor_core::gizmo::GizmoMode::Scale),
            None,
            None,
        ),
        "editor.tool_snap_toggle" => {
            let snap = !state.gizmo.snap.unwrap_or(false);
            gizmo_command(state, pending, None, None, Some(snap));
        }
        "editor.tool_space_toggle" => {
            use pill_editor_core::gizmo::GizmoSpace;
            let space = match state.gizmo.space.unwrap_or(GizmoSpace::World) {
                GizmoSpace::World => GizmoSpace::Local,
                GizmoSpace::Local => GizmoSpace::World,
            };
            gizmo_command(state, pending, None, Some(space), None);
        }
        // The clipboard commands queue their intent; the hot root runs it
        // where the frame and the snapshot are in scope (Stage 18, D28).
        "editor.copy" => state.clipboard_action = Some(ClipboardAction::Copy),
        "editor.cut" => state.clipboard_action = Some(ClipboardAction::Cut),
        "editor.paste" => state.clipboard_action = Some(ClipboardAction::Paste),
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

/// Run the clipboard work a command queued this frame (Stage 18, D28).
///
/// Copy and cut offer the selection - an entity's components, one component
/// or an asset - as a structured payload plus its readable text; paste reads
/// the clipboard through the executable (the answer arrives on a later
/// frame, so a slow clipboard owner never stalls the loop) and queues
/// recorded commands, making one paste one undo step.
pub fn run_clipboard_action(
    state: &mut EditorShellState,
    frame: &mut EditorShellFrame,
    snapshot: &EditorSnapshot,
    selection: &mut EditorSelection,
    pending: &mut PendingEditorCommands,
    answer: &EditorClipboardAnswer,
) {
    // Step 1: a paste whose read the executable has now answered.
    if state.clipboard_paste_pending && answer.seq > state.clipboard_seq_seen {
        state.clipboard_paste_pending = false;
        state.clipboard_seq_seen = answer.seq;
        apply_clipboard_paste(snapshot, selection, pending, answer);
    }
    let Some(action) = state.clipboard_action.take() else {
        return;
    };
    match action {
        ClipboardAction::Copy | ClipboardAction::Cut => {
            let selected_detail = selection.entity.and_then(|entity| {
                snapshot
                    .detail
                    .as_ref()
                    .filter(|detail| detail.entity == entity)
            });
            let offered = match selected_detail {
                Some(detail) => {
                    let text = clipboard::encode_entity(detail);
                    offer_object(frame, text, "entity", "");
                    Some(detail.entity)
                }
                None => match selection.asset.clone() {
                    Some(path) => {
                        let text = clipboard::encode_asset(&path);
                        offer_object(frame, text, "asset", path.as_str());
                        None
                    }
                    None => {
                        pill_core::info!(
                            target: pill_core::telemetry::telemetry_target::ECS,
                            "[editor] clipboard copy skipped nothing selected"
                        );
                        return;
                    }
                },
            };
            if action == ClipboardAction::Cut {
                match offered {
                    Some(entity) => {
                        pending.push_recorded(
                            vec![EditorCommand::DestroyEntity { entity }],
                            "cut entity",
                            None,
                        );
                        if selection.entity == Some(entity) {
                            selection.entity = None;
                        }
                        pill_core::info!(
                            target: pill_core::telemetry::telemetry_target::ECS,
                            entity = %entity,
                            "[editor] clipboard cut"
                        );
                    }
                    None => pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        "[editor] clipboard cut skipped kind=asset"
                    ),
                }
            }
        }
        ClipboardAction::CopyComponent { type_name } => {
            let component = snapshot.detail.as_ref().and_then(|detail| {
                detail
                    .components
                    .iter()
                    .find(|component| component.type_name == type_name)
            });
            let Some(component) = component else {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    type_name = type_name.as_str(),
                    "[editor] clipboard copy skipped no such component"
                );
                return;
            };
            let text = clipboard::encode_component(
                &component.type_name,
                &clipboard::component_field_pairs(component),
            );
            offer_object(frame, text, "component", component.type_name.as_str());
        }
        ClipboardAction::Paste => {
            if answer.seq > state.clipboard_seq_seen {
                state.clipboard_seq_seen = answer.seq;
                apply_clipboard_paste(snapshot, selection, pending, answer);
            } else {
                // The read is answered next frame; the pending flag applies
                // it (or a fresh in-process answer) without another press.
                frame.requested_clipboard = Some(clipboard::FORMAT_NAME.to_string());
                state.clipboard_paste_pending = true;
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    "[editor] clipboard read requested"
                );
            }
        }
    }
}

/// Offer one payload document to the system clipboard under the editor
/// format, with the same document as its readable text.
fn offer_object(frame: &mut EditorShellFrame, text: String, kind: &str, detail: &str) {
    let bytes = text.clone().into_bytes();
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        kind,
        detail,
        bytes = bytes.len(),
        "[editor] clipboard copied"
    );
    frame.objects.push(ClipboardObject {
        format: clipboard::FORMAT_NAME.to_string(),
        bytes,
        text,
    });
}

/// Apply whatever the clipboard held: decode the payload and queue the
/// recorded commands the paste becomes.
fn apply_clipboard_paste(
    snapshot: &EditorSnapshot,
    selection: &mut EditorSelection,
    pending: &mut PendingEditorCommands,
    answer: &EditorClipboardAnswer,
) {
    let source = answer
        .bytes
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .map(str::to_string)
        .or_else(|| answer.text.clone());
    let Some(text) = source else {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            "[editor] clipboard paste skipped empty"
        );
        return;
    };
    let payload = match clipboard::decode(&text) {
        Ok(payload) => payload,
        Err(error) => {
            pill_core::warn!(
                target: pill_core::telemetry::telemetry_target::ECS,
                error = error.as_str(),
                "[editor] clipboard payload not understood"
            );
            return;
        }
    };
    match payload {
        Payload::Entity { components } => {
            let mut seeds = Vec::new();
            let mut skipped: Vec<String> = Vec::new();
            for component in components {
                if !component_type_registered(snapshot, &component.type_name) {
                    skipped.push(component.type_name);
                    continue;
                }
                seeds.push(seed_from(component));
            }
            if seeds.is_empty() {
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    skipped = ?skipped,
                    "[editor] clipboard paste skipped components"
                );
                return;
            }
            let carried = seeds.len();
            pending.push_recorded(
                vec![EditorCommand::CreateEntity { components: seeds }],
                "paste entity",
                None,
            );
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                components = carried,
                skipped = skipped.len(),
                "[editor] clipboard paste entity"
            );
        }
        Payload::Component { component } => {
            let Some(entity) = selection.entity else {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    "[editor] clipboard paste skipped no selection"
                );
                return;
            };
            if !component_type_registered(snapshot, &component.type_name) {
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    type_name = component.type_name.as_str(),
                    "[editor] clipboard paste skipped component"
                );
                return;
            }
            let present = snapshot
                .detail
                .as_ref()
                .filter(|detail| detail.entity == entity)
                .is_some_and(|detail| {
                    detail
                        .components
                        .iter()
                        .any(|view| view.type_name == component.type_name)
                });
            let seed = seed_from(component);
            let mut commands = Vec::new();
            if !present {
                commands.push(EditorCommand::AddComponent {
                    entity,
                    component: seed.type_name.clone(),
                });
            }
            for (field, value) in seed.fields {
                commands.push(EditorCommand::SetField {
                    entity,
                    component: seed.type_name.clone(),
                    field,
                    value,
                });
            }
            if commands.is_empty() {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    "[editor] clipboard paste skipped empty component"
                );
                return;
            }
            let short = short_name(&seed.type_name);
            pending.push_recorded(commands, format!("paste {short}"), None);
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                type_name = seed.type_name.as_str(),
                "[editor] clipboard paste component"
            );
        }
        Payload::Field {
            component,
            field,
            value,
        } => {
            let Some(entity) = selection.entity else {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    "[editor] clipboard paste skipped no selection"
                );
                return;
            };
            let Some(registered) = registered_name(snapshot, &component) else {
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    component = component.as_str(),
                    "[editor] clipboard paste skipped component"
                );
                return;
            };
            let Some(parsed) = clipboard::parse_value(&value) else {
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    value = value.as_str(),
                    "[editor] clipboard paste skipped value"
                );
                return;
            };
            let label = format!("paste {}.{}", short_name(&registered), field);
            pending.push_recorded(
                vec![EditorCommand::SetField {
                    entity,
                    component: registered,
                    field,
                    value: parsed,
                }],
                label,
                None,
            );
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                "[editor] clipboard paste field"
            );
        }
        Payload::Asset { path } => {
            selection.asset = Some(path.clone());
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                path = path.as_str(),
                "[editor] clipboard paste asset"
            );
        }
    }
}

/// Whether the world snapshot lists a component type under this name.
fn component_type_registered(snapshot: &EditorSnapshot, type_name: &str) -> bool {
    snapshot
        .components
        .iter()
        .any(|component| component.type_name == type_name)
}

/// Resolve a component name (exact or `::Name` suffix) against the
/// snapshot's registry; an ambiguous suffix resolves to nothing.
fn registered_name(snapshot: &EditorSnapshot, name: &str) -> Option<String> {
    let suffix = format!("::{name}");
    let mut matches = snapshot
        .components
        .iter()
        .filter(|component| component.type_name == name || component.type_name.ends_with(&suffix))
        .map(|component| component.type_name.clone());
    let first = matches.next()?;
    if matches.any(|candidate| candidate != first) {
        return None;
    }
    Some(first)
}

/// The payload's component seed with every parseable field; a token this
/// build does not understand is dropped from the seed (best-effort paste).
fn seed_from(component: SeededComponent) -> ComponentSeed {
    let fields = component
        .fields
        .into_iter()
        .filter_map(|(name, token)| clipboard::parse_value(&token).map(|value| (name, value)))
        .collect();
    ComponentSeed {
        type_name: component.type_name,
        fields,
    }
}

/// Decide what a hovering or dropped OS file drag means (Stage 18, D28).
///
/// A drag over the Assets panel previews an import; a drop there queues it
/// as one recorded step, so undo removes the copies. A drop anywhere else
/// is ignored with a log line (v1 imports into the asset root).
pub fn handle_file_drops(
    state: &mut EditorShellState,
    input: &mut EditorShellInput,
    placements: &[PanelPlacement],
    pending: &mut PendingEditorCommands,
) {
    let assets_rect = placements
        .iter()
        .find(|placement| placement.kind == PanelKind::Assets)
        .map(|placement| placement.rect);
    let over_assets = assets_rect
        .map(|rect| {
            let cursor = state.cursor;
            cursor.x as f64 >= rect.x
                && cursor.x as f64 <= rect.x + rect.width
                && cursor.y as f64 >= rect.y
                && cursor.y as f64 <= rect.y + rect.height
        })
        .unwrap_or(false);
    // Step 1: the preview's state, logged on change so a suite can see it.
    let hover = if !input.hovered_files.is_empty() && over_assets {
        Some(input.hovered_files.len())
    } else {
        None
    };
    if hover != state.drop_hover {
        match hover {
            Some(count) => pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                count,
                "[editor] drop preview shown"
            ),
            None => pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                "[editor] drop preview cleared"
            ),
        }
        state.drop_hover = hover;
    }
    // Step 2: a drop queues the import; the runner applies it at the frame
    // boundary and the asset watcher imports the new files.
    if input.dropped_files.is_empty() {
        return;
    }
    let dropped = std::mem::take(&mut input.dropped_files);
    if !over_assets {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            count = dropped.len(),
            "[editor] file drop ignored outside the assets panel"
        );
        return;
    }
    let Some(root) = input.asset_root.clone() else {
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ECS,
            "[editor] file drop ignored: the project has no asset root"
        );
        return;
    };
    let count = dropped.len();
    let label = if count == 1 {
        "import file".to_string()
    } else {
        format!("import {count} files")
    };
    pending.push_recorded(
        vec![EditorCommand::ImportFiles {
            sources: dropped,
            destination: root,
        }],
        label,
        None,
    );
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        count,
        "[editor] file drop import queued"
    );
}

/// Draw the drop preview (Stage 18): a band across the Assets panel while
/// a file drag hovers over it.
pub fn paint_drop_preview(
    state: &mut EditorShellState,
    frame: &mut EditorShellFrame,
    placements: &[PanelPlacement],
) {
    let Some(count) = state.drop_hover else {
        return;
    };
    let Some(rect) = placements
        .iter()
        .find(|placement| placement.kind == PanelKind::Assets)
        .map(|placement| placement.rect)
    else {
        return;
    };
    let band = Rect::from_min_size(
        Vector2f::new(rect.x as f32, rect.y as f32),
        Vector2f::new(rect.width as f32, 28.0),
    );
    frame
        .panels
        .push_fill(band, pill_ui::Color::rgba(0.15, 0.45, 0.9, 0.85));
    let message = format!("Drop {count} file(s) to import");
    let style = pill_ui::text::TextStyle::default();
    let _ = state.text.draw_text(
        &mut frame.panels,
        &mut state.atlas,
        &message,
        Vector2f::new(rect.x as f32 + 8.0, rect.y as f32 + 4.0),
        &style,
        None,
        pill_ui::Color::WHITE,
    );
}

/// Paint the toast stack: the newest notices the executable published, one
/// filled row each, the stack expiring by frame count (Stage 19).
///
/// Drawn into the panel layer's end (the topmost list entry), under the menu
/// chrome. The view surfaces are real child windows and always cover their
/// rectangle (D24's pending composition overlay), so the stack sits at the
/// client area's top-left, over the dock's left column.
pub fn paint_toasts(
    state: &mut EditorShellState,
    frame: &mut EditorShellFrame,
    available: Vector2f,
) {
    let current_frame = state.frame_index;
    state
        .toasts
        .retain(|toast| toast.expires_at_frame > current_frame);
    while state.toasts.len() > TOASTS_MAX {
        state.toasts.pop_front();
    }
    if state.toasts.is_empty() {
        return;
    }
    let mut y = MENU_BAR_HEIGHT as f32 + 6.0;
    let style = pill_ui::text::TextStyle::default();
    // Newest at the top of the stack: the reason the user just saw a flash
    // of activity is the one they are looking for.
    let toasts: Vec<(EditorProblemLevel, String)> = state
        .toasts
        .iter()
        .rev()
        .map(|toast| (toast.level, toast.text.clone()))
        .collect();
    for (level, text) in toasts {
        let width = TOAST_WIDTH.min(available.x - 16.0).max(120.0);
        let rect = Rect::from_min_size(Vector2f::new(8.0, y), Vector2f::new(width, TOAST_HEIGHT));
        frame.panels.push_fill(rect, state.theme.raised);
        frame.panels.push_fill(
            Rect::from_min_size(Vector2f::new(8.0, y), Vector2f::new(3.0, TOAST_HEIGHT)),
            match level {
                EditorProblemLevel::Error => pill_ui::Color::rgba(0.85, 0.25, 0.2, 1.0),
                EditorProblemLevel::Warning => pill_ui::Color::rgba(0.9, 0.7, 0.2, 1.0),
                EditorProblemLevel::Note => pill_ui::Color::rgba(0.35, 0.65, 0.9, 1.0),
            },
        );
        let marker = match level {
            EditorProblemLevel::Error => "error",
            EditorProblemLevel::Warning => "warning",
            EditorProblemLevel::Note => "info",
        };
        let message = format!("{marker}: {text}");
        let _ = state.text.draw_text(
            &mut frame.panels,
            &mut state.atlas,
            &message,
            Vector2f::new(rect.min.x + 8.0, rect.min.y + 3.0),
            &style,
            None,
            state.theme.text,
        );
        y += TOAST_HEIGHT + 4.0;
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
    // The menu layer draws over the panels but does not own their hit
    // rectangles: its own builds isolate against an empty map (its pointer
    // handling uses the manual rectangles below), and the panels' map goes
    // back before returning so widget input keeps working.
    let saved_hits = state.ui.hit_rects().clone();
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
        state.ui.set_hit_rects(saved_hits);
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
        let (mut list, clipboard, _named) = subframe(
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
        state.ui.set_hit_rects(saved_hits);
        return;
    };
    let Some((top, items)) = menus.iter().find(|(name, _)| *name == open) else {
        state.menu_open = None;
        state.ui.set_hit_rects(saved_hits);
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
        let (mut list, clipboard, _named) = subframe(
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
    state.ui.set_hit_rects(saved_hits);
}

/// Paints the command palette when open: a search field and the matching
/// commands, clickable (Stage 14).
pub fn paint_palette(
    state: &mut EditorShellState,
    registry: &CommandRegistry,
    frame: &mut EditorShellFrame,
    available: Vector2f,
) {
    // Like the menu layer: the palette isolates its own builds and puts the
    // panels' hit rectangles back before returning.
    let saved_hits = state.ui.hit_rects().clone();
    let Some(mut query) = state.palette_query.clone() else {
        state.palette_rect = None;
        state.palette_item_hits.clear();
        state.ui.set_hit_rects(saved_hits);
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
        let (mut list, clipboard, _named) = subframe(
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
    state.ui.set_hit_rects(saved_hits);
}
