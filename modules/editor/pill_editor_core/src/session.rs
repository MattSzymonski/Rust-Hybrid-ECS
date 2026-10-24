//! The egui shell's long-lived session state.
//!
//! # Responsibilities
//!
//! - Own [`EguiShellState`]: the egui `Context` and the dock, the shell's one
//!   world resource.
//! - Own [`ToolboxSession`]: where each scene view's floating toolbox sits
//!   and what the log already reported about it.
//! - Carry the Assets tab's [`ProjectBrowser`]: the folder its grid lists.
//!
//! # Design
//!
//! The type lives in this shared `dylib`, not in the reloadable shell
//! module, so its identity is stable across shell reloads: the arriving
//! generation's `register` finds the same resource and keeps the context and
//! the dock exactly where the user left them. A resource whose type the
//! module defined would be reset by every reload - and the context's drop
//! code would then run from an unloaded image.

// External crates
use std::collections::BTreeMap;

use egui::{Context, Pos2, Rect, TextureHandle, TextureOptions};
use pill_engine::Resource;

// Current crate
use crate::assets::AssetDialog;
use crate::boundary::SCENE_VIEWS_MAX;
use crate::browser::ProjectBrowser;
use crate::layout::{EditorDock, EditorTab};
use crate::notices::NoticeLevel;

/// The egui shell's session: the UI context and the dock layout.
pub struct EguiShellState {
    /// The egui context: fonts, memory and widget state.
    pub context: Context,
    /// The dock layout; persisted across restarts.
    pub dock: EditorDock,
    /// Completed egui passes; the shell's log cadence reads it.
    pub frame_index: u64,
    /// The asset dialog that is open, if one is: the create or rename
    /// window's state, kept here so a shell reload keeps it.
    pub asset_dialog: Option<AssetDialog>,
    /// The Preferences tab's unsaved field buffers, keyed like the file;
    /// `None` until the tab seeds them from the snapshot and after a save,
    /// so the rows re-seed from the file the next frame.
    pub preferences_draft: Option<BTreeMap<String, String>>,
    /// The floating viewport toolboxes' session state.
    pub toolbox: ToolboxSession,
    /// The Assets tab's project browser: the folder its grid lists, the
    /// tree's open folders and the tile size.
    pub browser: ProjectBrowser,
    /// The brand mark's texture, uploaded on the first frame and kept for the
    /// session: the context owns it, so a shell reload reuses it.
    pub mark: Option<TextureHandle>,
    /// The menu bar's already-logged geometry and the requests its items
    /// raised.
    pub menubar: MenubarSession,
    /// The Inspector's collapsed component sections and its logged geometry.
    pub inspector: InspectorSession,
    /// The toasts the host's notices put on screen (W-0038 Stage 4).
    pub toasts: ToastSession,
    /// The command palette's open state and query (W-0038 Stage 3).
    pub palette: PaletteState,
    /// The Console tab's filters (W-0038 Stage 5): the tab edits these, and
    /// the executable applies them to the history on the next frame.
    pub console_filters: crate::console::ConsoleFilterSettings,
    /// The Problems row a click asked to open, drained by the executable
    /// (W-0038 Stage 6).
    pub problem_open: Option<usize>,
    /// The per-tab and whole-pass timings the Statistics tab shows
    /// (W-0038 Stage 7).
    pub stats: StatisticsSession,
    /// Every module panel's build time, and which of them overshoots the
    /// budget (W-0038 Stage 11).
    pub panel_budget: PanelBudget,
    /// Whether this session already mapped the saved layout's module tabs
    /// from the panel ids it carried to the slots the registry assigns today.
    /// Once per session, not once per shell generation: a reload must not
    /// undo a dock the user rearranged.
    pub panels_remapped: bool,
    /// The windows the module panels asked for (W-0038 Stage 13).
    pub panel_windows: PanelWindowSession,
    /// A window the scripted channel asked a panel to open: the panel's slot
    /// and the window id it should choose (W-0038 Stage 13).
    pub panel_window_ping: Option<(u16, String)>,
    /// One egui context per popped-out tab (W-0038 Stage 23). A popped-out
    /// window runs its own pass, so it owns its own context - the main one is
    /// running the dock in the same frame. The map lives in the session for
    /// the same reason the main context does: a shell reload must find each
    /// window's widget state where it left it.
    pub popout_contexts: std::collections::BTreeMap<EditorTab, Context>,
}

/// How many panel windows one session may have open at once.
pub const PANEL_WINDOWS_MAX: usize = 8;

/// One window a panel asked the shell for.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelWindowState {
    /// The window's identity within its panel.
    pub id: String,
    /// The title its bar shows.
    pub title: String,
    /// The crate that asked for it: its panels' markers decide its lifetime.
    pub source: String,
    /// The registry slot of the panel that draws it.
    pub slot: u16,
    /// How wide it opens, in logical points.
    pub width: f32,
    /// How tall it opens, in logical points.
    pub height: f32,
    /// Whether it takes the editor's input until answered.
    pub modal: bool,
}

/// The answer a modal window took: Enter accepts, Escape dismisses.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelWindowAnswer {
    /// The window that was answered.
    pub window: String,
    /// The crate that owns it.
    pub source: String,
    /// Whether the answer was Enter rather than Escape.
    pub accepted: bool,
}

/// The windows the module panels asked for (W-0038 Stage 13).
///
/// Session-scoped like the dock: a shell reload keeps every window open, and
/// only the owning module's unload closes its windows - which is exactly why
/// the source is recorded on each of them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PanelWindowSession {
    /// The open windows, oldest first.
    pub windows: Vec<PanelWindowState>,
    /// The answer this frame owes the modal that asked for it; the panel reads
    /// it on its next draw and the shell clears it afterwards.
    pub answer: Option<PanelWindowAnswer>,
}

impl PanelWindowSession {
    /// Open a window, replacing one with the same id.
    pub fn open(&mut self, source: &str, slot: u16, request: &crate::panels::PanelWindowRequest) {
        if let Some(existing) = self
            .windows
            .iter_mut()
            .find(|window| window.id == request.id)
        {
            existing.title = request.title.clone();
            existing.width = request.width;
            existing.height = request.height;
            existing.modal = request.modal;
            existing.slot = slot;
            return;
        }
        if self.windows.len() >= PANEL_WINDOWS_MAX {
            self.windows.remove(0);
        }
        self.windows.push(PanelWindowState {
            id: request.id.clone(),
            title: request.title.clone(),
            source: source.to_string(),
            slot,
            width: request.width,
            height: request.height,
            modal: request.modal,
        });
    }

    /// Close one window of one source.
    pub fn close(&mut self, source: &str, id: &str) {
        self.windows
            .retain(|window| !(window.source == source && window.id == id));
    }

    /// Close every window a source opened; returns how many went.
    pub fn close_source(&mut self, source: &str) -> usize {
        let before = self.windows.len();
        self.windows.retain(|window| window.source != source);
        before - self.windows.len()
    }

    /// Whether a modal window is on screen: the editor's input belongs to it.
    pub fn modal(&self) -> Option<&PanelWindowState> {
        self.windows.iter().find(|window| window.modal)
    }
}

/// One tab's measured cost, smoothed so a single slow frame does not dominate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TabTiming {
    /// Which tab.
    pub tab: EditorTab,
    /// The exponentially weighted mean of one body pass, in microseconds.
    pub micros: f64,
    /// How many passes were measured.
    pub passes: u64,
}

/// The timings the Statistics tab reads (W-0038 Stage 7).
///
/// Written by the shell's own pass, read by the tab: the measurement is the
/// shell's, so the tab cannot inflate what it reports by drawing itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatisticsSession {
    /// One entry per tab drawn since the session began, in first-drawn order.
    pub tabs: Vec<TabTiming>,
    /// The whole shell pass's smoothed time, in microseconds.
    pub shell_micros: f64,
    /// Completed shell passes.
    pub shell_passes: u64,
    /// The editor's smoothed frame rate, as the shell measured it.
    pub frames_per_second: f32,
}

impl StatisticsSession {
    /// How strongly one pass moves the mean: a twentieth, so about twenty
    /// passes settle a value without hiding a real regression for long.
    const WEIGHT: f64 = 0.05;

    /// Fold one tab body's time into its entry.
    pub fn record(&mut self, tab: EditorTab, micros: f64) {
        match self.tabs.iter_mut().find(|entry| entry.tab == tab) {
            Some(entry) => {
                entry.micros = smoothing(entry.micros, micros, Self::WEIGHT);
                entry.passes += 1;
            }
            None => self.tabs.push(TabTiming {
                tab,
                micros,
                passes: 1,
            }),
        }
    }

    /// Fold one whole shell pass's time in.
    pub fn record_shell(&mut self, micros: u64) {
        self.shell_micros = smoothing(self.shell_micros, micros as f64, Self::WEIGHT);
        self.shell_passes += 1;
    }

    /// One tab's smoothed time, or zero when it was never drawn.
    pub fn tab_micros(&self, tab: EditorTab) -> f64 {
        self.tabs
            .iter()
            .find(|entry| entry.tab == tab)
            .map_or(0.0, |entry| entry.micros)
    }
}

/// One step of an exponential moving average, seeded by the first sample.
fn smoothing(current: f64, sample: f64, weight: f64) -> f64 {
    if current <= 0.0 {
        sample
    } else {
        current * (1.0 - weight) + sample * weight
    }
}

/// The command palette's session state (W-0038 Stage 3).
///
/// Owned strings, not callbacks: the palette outlives a shell reload, and a
/// reload must not leave a closure from the old image behind.
#[derive(Clone, Debug, Default)]
pub struct PaletteState {
    /// Whether the palette is on screen.
    pub open: bool,
    /// What the query field holds; the filter is a substring match on titles.
    pub query: String,
    /// The selected row's index in the filtered list.
    pub selected: usize,
    /// A run the scripted channel asked for, drained by the shell's next
    /// pass so the palette and a click take the same action path.
    pub run_requested: bool,
    /// The query the log already reported, so one query logs one line.
    pub logged_query: Option<String>,
}

/// The floating viewport toolbox's session state (Stage 22): a drag parks
/// one scene view's toolbox, and the log reports a placement once.
///
/// Session-scoped, like the dock's floating tabs' positions: a restart
/// starts every toolbox at its default corner. Persisting the origins into
/// the layout document is follow-up work.
pub struct ToolboxSession {
    /// Per-scene-view panel origins in logical points; `None` until a drag
    /// parks that view's toolbox, so the default anchor keeps following the
    /// viewport until the user moves it. One slot per view index, however
    /// many views the person opened (W-0038 Stage 21).
    pub origins: Vec<Option<Pos2>>,
    /// Per-scene-view panel rectangles whose rows the log already reported,
    /// so the rect lines appear once per placement and once per drag.
    pub logged: Vec<Option<Rect>>,
}

impl Default for ToolboxSession {
    fn default() -> Self {
        Self {
            origins: vec![None; SCENE_VIEWS_MAX],
            logged: vec![None; SCENE_VIEWS_MAX],
        }
    }
}

impl ToolboxSession {
    /// The placed origin of `view`'s toolbox.
    pub fn origin(&self, view: usize) -> Option<Pos2> {
        self.origins.get(view).copied().flatten()
    }

    /// Park `view`'s toolbox at `origin`.
    pub fn set_origin(&mut self, view: usize, origin: Pos2) {
        if self.origins.len() <= view {
            self.origins.resize(view + 1, None);
        }
        self.origins[view] = Some(origin);
    }

    /// The rectangle of `view`'s toolbox the log already reported.
    pub fn logged(&self, view: usize) -> Option<Rect> {
        self.logged.get(view).copied().flatten()
    }

    /// Record `view`'s toolbox rectangle as logged.
    pub fn set_logged(&mut self, view: usize, rect: Rect) {
        if self.logged.len() <= view {
            self.logged.resize(view + 1, None);
        }
        self.logged[view] = Some(rect);
    }
}

/// The menu bar's logged geometry: the strip and each title, so a placement is
/// reported once (the toolbox's pattern) and a suite can click the titles.
pub struct MenubarSession {
    /// The strip's last logged rectangle.
    pub bar: Option<Rect>,
    /// Each title's last logged rectangle, by [`crate::menubar::Menu::ALL`]
    /// index.
    pub titles: [Option<Rect>; crate::menubar::MENU_TITLE_COUNT],
    /// Each open menu entry's last logged rectangle, keyed `menu/label`; a
    /// menu's entries log while it is open, so their presence is the proof it
    /// opened.
    pub entries: Vec<(String, Rect)>,
    /// Undo/redo the menus asked for, drained by the executable.
    pub history_requests: Vec<HistoryRequest>,
    /// The dock tab the Window menu asked for, drained by the executable.
    pub pending_tab: Option<EditorTab>,
    /// Whether the Window menu asked for one more scene view (W-0038 Stage
    /// 21); the shell docks it after the pass, because the dock is borrowed
    /// by the `DockArea` while the menu draws.
    pub new_scene_view: bool,
    /// Whether the About window is open.
    pub about_open: bool,
    /// The menu whose dropdown is open, while one is; the log reports each
    /// opening once so a suite can tell an open menu from a closed one.
    pub open: Option<crate::menubar::Menu>,
    /// Whether a menu asked the editor to quit; the shell moves it to the
    /// frame at the end of the pass and clears it.
    pub quit_requested: bool,
}

impl Default for MenubarSession {
    fn default() -> Self {
        Self {
            bar: None,
            titles: [None; crate::menubar::MENU_TITLE_COUNT],
            entries: Vec::new(),
            history_requests: Vec::new(),
            pending_tab: None,
            about_open: false,
            open: None,
            quit_requested: false,
            new_scene_view: false,
        }
    }
}

/// An undo or redo the menu bar asked for, drained by the executable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryRequest {
    /// Undo the newest recorded step.
    Undo,
    /// Redo the newest undone step.
    Redo,
}

/// One toast on screen: a short-lived line about something the host did.
#[derive(Clone, Debug)]
pub struct Toast {
    /// The notice event's id, so a frame never queues the same line twice.
    pub seq: u64,
    /// Error, warning or note, picking the stripe's colour.
    pub level: NoticeLevel,
    /// The line to show.
    pub text: String,
    /// The frame index at which it disappears.
    pub expires_at_frame: u64,
}

/// How many toasts show at once; older ones drop off the stack.
pub const TOASTS_MAX: usize = 5;

/// Frames a toast stays up - about four seconds at 60 fps.
pub const TOAST_FRAMES: u64 = 240;

/// The toast stack's session state.
#[derive(Default)]
pub struct ToastSession {
    /// The live toasts, oldest first.
    pub toasts: std::collections::VecDeque<Toast>,
    /// The newest notice id already queued; `None` until the first event.
    pub last_seq: Option<u64>,
    /// The ids whose log line already went out, so a redrawn toast does not
    /// flood the log.
    pub logged: Vec<u64>,
    /// How many toasts the shell queued itself, counted up from
    /// [`ToastSession::LOCAL_SEQ_BASE`] so a local toast can never collide
    /// with a host notice's id.
    local_count: u64,
}

impl ToastSession {
    /// Where the shell's own toasts start their ids: past any host notice's.
    pub const LOCAL_SEQ_BASE: u64 = u64::MAX / 2;

    /// Queue one toast the shell itself produced - a panel over its budget,
    /// rather than something the host reported.
    pub fn push_local(&mut self, level: NoticeLevel, text: String, frame_index: u64) -> u64 {
        if self.toasts.len() >= TOASTS_MAX {
            self.toasts.pop_front();
        }
        let seq = Self::LOCAL_SEQ_BASE + self.local_count;
        self.local_count += 1;
        self.toasts.push_back(Toast {
            seq,
            level,
            text,
            expires_at_frame: frame_index + TOAST_FRAMES,
        });
        seq
    }
}

/// How long one panel may take before the editor says so.
pub const PANEL_BUDGET_MICROS: f32 = 4000.0;

/// How many frames in a row a panel must overshoot its budget before the
/// editor says so - a single slow frame (a reload, a texture upload) is not a
/// slow panel.
pub const PANEL_BUDGET_FRAMES: u32 = 120;

/// One panel's build time and how long it has been over budget.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PanelTiming {
    /// The panel's registry slot.
    pub slot: u16,
    /// The panel body's smoothed build time, in microseconds.
    pub micros: f32,
    /// Passes the panel body was timed.
    pub passes: u64,
    /// Consecutive passes over [`PANEL_BUDGET_MICROS`].
    pub slow_frames: u32,
    /// Whether the overshoot was already reported, so one panel warns once.
    pub warned: bool,
    /// Whether the warning toast was queued, so a redrawn frame does not
    /// queue it again.
    pub toasted: bool,
}

/// Every module panel's build time (W-0038 Stage 11).
///
/// Written by the shell's own pass and read by the tab that draws the narm
/// and by the suite: the measurement belongs to whoever timed the call, and
/// a panel cannot inflate its own row by drawing it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PanelBudget {
    /// One entry per panel drawn since the session began, in first-drawn order.
    pub entries: Vec<PanelTiming>,
}

impl PanelBudget {
    /// How strongly one pass moves the mean, the same weight the statistics
    /// use.
    pub const WEIGHT: f32 = 0.05;

    /// Record one panel body's build time; returns whether this pass is the
    /// one that reports an overshoot.
    ///
    /// The mean is smoothed, but the overshoot count is not: a panel that
    /// sits just over budget every frame is slow, and a panel that is fast
    /// again resets the count, so only a sustained overshoot warns.
    pub fn record(&mut self, slot: u16, micros: f32) -> bool {
        let index = match self.entries.iter().position(|entry| entry.slot == slot) {
            Some(index) => index,
            None => {
                self.entries.push(PanelTiming {
                    slot,
                    ..PanelTiming::default()
                });
                self.entries.len() - 1
            }
        };
        let entry = &mut self.entries[index];
        entry.micros = if entry.passes == 0 {
            micros
        } else {
            entry.micros * (1.0 - Self::WEIGHT) + micros * Self::WEIGHT
        };
        entry.passes += 1;
        if micros > PANEL_BUDGET_MICROS {
            entry.slow_frames = entry.slow_frames.saturating_add(1);
        } else {
            entry.slow_frames = 0;
        }
        if entry.slow_frames >= PANEL_BUDGET_FRAMES && !entry.warned {
            entry.warned = true;
            return true;
        }
        false
    }

    /// The timing of one slot, when it has been timed.
    pub fn get(&self, slot: u16) -> Option<&PanelTiming> {
        self.entries.iter().find(|entry| entry.slot == slot)
    }

    /// Whether the slot's panel is currently over budget and warned about it:
    /// what the tab draws its badge from.
    pub fn is_slow(&self, slot: u16) -> bool {
        self.get(slot).is_some_and(|entry| entry.warned)
    }
}

/// The Inspector's session state: which component sections are collapsed, and
/// the geometry its log already reported.
///
/// Session-scoped like the toolbox: a shell reload keeps it, a restart starts
/// every section expanded. The collapse state is keyed by the registered type
/// name, so selecting another entity keeps a type's foldout where the user
/// left it (Unity's behaviour).
#[derive(Default)]
pub struct InspectorSession {
    /// The component type names whose sections are collapsed.
    pub collapsed: std::collections::BTreeSet<String>,
    /// Each section header's last logged rectangle, keyed by type name.
    pub section_rects: Vec<(String, Rect)>,
    /// The Add Component button's last logged rectangle.
    pub add_rect: Option<Rect>,
    /// Whether the Add Component popup is open.
    pub add_open: bool,
    /// The addable entries' last logged rectangles, keyed by type name.
    pub add_entries: Vec<(String, Rect)>,
    /// The row signatures already logged, so a panel that redraws every frame
    /// does not flood the log with the same row.
    pub rows_logged: Vec<String>,
    /// Each asset reference row's last logged rectangle, keyed by
    /// `entity.component.field` (W-0048): what a suite drops an asset onto.
    /// Only a row on screen keeps an entry; see the shell's
    /// `remember_asset_row_rect`.
    pub asset_row_rects: Vec<(String, Rect)>,
    /// Each asset reference row's last logged text, keyed the same way: the
    /// row logs again whenever it draws a different asset.
    pub asset_row_texts: Vec<(String, String)>,
    /// The `component.field` whose asset picker is open, if one is.
    pub asset_picker: Option<String>,
    /// The open asset picker's search text.
    pub asset_search: String,
    /// Each entity reference row's last logged rectangle, keyed by
    /// `entity.component.field` (W-0048 Stage 6): what a suite drags an
    /// entity onto.
    pub entity_row_rects: Vec<(String, Rect)>,
    /// Each entity reference row's last logged text, keyed the same way: the
    /// row logs again whenever it draws a different entity.
    pub entity_row_texts: Vec<(String, String)>,
    /// The `component.field` whose entity picker is open, if one is.
    pub entity_picker: Option<String>,
    /// The open entity picker's search text.
    pub entity_search: String,
    /// Each Hierarchy row's last logged rectangle, keyed by the entity's
    /// display (W-0048 Stage 6): what a suite drags onto an entity reference
    /// row. The Entities tab shares this session, since the cache is the same
    /// "geometry the log already reported" concern.
    pub hierarchy_row_rects: Vec<(String, Rect)>,
}

impl Resource for EguiShellState {}

impl EguiShellState {
    /// Session state with the editor's theme and the saved dock.
    ///
    /// A document this build cannot use falls back to the default layout and
    /// logs why (`[editor] layout reset`); a missing one is the first run.
    pub fn new() -> Self {
        let context = Context::default();
        crate::style::apply(&context);
        Self {
            context,
            dock: crate::layout::load_or_default(),
            frame_index: 0,
            asset_dialog: None,
            preferences_draft: None,
            toolbox: ToolboxSession::default(),
            browser: ProjectBrowser::default(),
            mark: None,
            menubar: MenubarSession::default(),
            inspector: InspectorSession::default(),
            toasts: ToastSession::default(),
            palette: PaletteState::default(),
            console_filters: crate::console::ConsoleFilterSettings::default(),
            problem_open: None,
            stats: StatisticsSession::default(),
            panel_budget: PanelBudget::default(),
            panels_remapped: false,
            panel_windows: PanelWindowSession::default(),
            panel_window_ping: None,
            popout_contexts: std::collections::BTreeMap::new(),
        }
    }

    /// The brand mark's texture, uploaded on first use and cloned from then on.
    pub fn mark_texture(&mut self) -> TextureHandle {
        if let Some(mark) = &self.mark {
            return mark.clone();
        }
        let texture = self.context.load_texture(
            "pill_editor.mark",
            crate::brand::mark_color_image(),
            TextureOptions::LINEAR,
        );
        self.mark = Some(texture.clone());
        texture
    }
}

impl Default for EguiShellState {
    fn default() -> Self {
        Self::new()
    }
}
