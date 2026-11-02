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
use std::collections::{BTreeMap, BTreeSet};

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
    /// The executable's theme generation this session last installed. The
    /// executable resolves the layers and publishes the
    /// merged theme on the snapshot; the shell installs it when this moves, so
    /// a reloaded shell picks up the theme already in force.
    pub applied_theme_generation: u64,
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
    /// The asset tiles' previews: the textures the executable read back and
    /// uploaded, the tiles it answered without pixels, and the tiles this
    /// pass drew - one value, because the three answer one question (may this
    /// tile be asked for now?).
    pub previews: PreviewStates,
    /// The menu bar's already-logged geometry and the requests its items
    /// raised.
    pub menubar: MenubarSession,
    /// The Inspector's collapsed component sections and its logged geometry.
    pub inspector: InspectorSession,
    /// The toasts the host's notices put on screen.
    pub toasts: ToastSession,
    /// The command palette's open state and query.
    pub palette: PaletteState,
    /// The Console tab's filters: the tab edits these, and
    /// the executable applies them to the history on the next frame.
    pub console_filters: crate::console::ConsoleFilterSettings,
    /// The Problems row a click asked to open, drained by the executable.
    pub problem_open: Option<usize>,
    /// The per-tab and whole-pass timings the Statistics tab shows.
    pub stats: StatisticsSession,
    /// Every module panel's build time, and which of them overshoots the
    /// budget.
    pub panel_budget: PanelBudget,
    /// Whether this session already mapped the saved layout's module tabs
    /// from the panel ids it carried to the slots the registry assigns today.
    /// Once per session, not once per shell generation: a reload must not
    /// undo a dock the user rearranged.
    pub panels_remapped: bool,
    /// The windows the module panels asked for.
    pub panel_windows: PanelWindowSession,
    /// A window the scripted channel asked a panel to open: the panel's slot
    /// and the window id it should choose.
    pub panel_window_ping: Option<(u16, String)>,
    /// One egui context per popped-out tab. A popped-out
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

/// The windows the module panels asked for.
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

/// The timings the Statistics tab reads.
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

/// The command palette's session state.
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

/// The floating viewport toolbox's session state: a drag parks
/// one scene view's toolbox, and the log reports a placement once.
///
/// Session-scoped, like the dock's floating tabs' positions: a restart
/// starts every toolbox at its default corner.
pub struct ToolboxSession {
    /// Per-scene-view panel origins in logical points; `None` until a drag
    /// parks that view's toolbox, so the default anchor keeps following the
    /// viewport until the user moves it. One slot per view index, however
    /// many views the person opened.
    pub origins: Vec<Option<Pos2>>,
    /// Per-scene-view panel rectangles whose rows the log already reported,
    /// so the rect lines appear once per placement and once per drag.
    pub logged: LoggedRects<usize>,
    /// Per-scene-view row rectangles, in [`crate::toolbox::TOOLBOX_ITEMS`]
    /// order: the face a row's button drew this pass, which is what a click
    /// aims at. A row's face is narrower than the row it sits in, so the
    /// logged row rectangle alone would not say where the button is.
    pub faces: Vec<Vec<Rect>>,
}

impl Default for ToolboxSession {
    fn default() -> Self {
        Self {
            origins: vec![None; SCENE_VIEWS_MAX],
            logged: LoggedRects::default(),
            faces: vec![Vec::new(); SCENE_VIEWS_MAX],
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

    /// Record `view`'s toolbox rectangle as logged, answering whether it is a
    /// placement the log has not named yet.
    ///
    /// Pixel-exact: the panel is dragged as a whole, so a fraction of a point
    /// is a move the user made.
    pub fn remember_logged(&mut self, view: usize, rect: Rect) -> bool {
        self.logged.remember_exact(view, rect)
    }

    /// The face `view`'s row `index` drew this pass, when it drew one.
    pub fn face(&self, view: usize, index: usize) -> Option<Rect> {
        self.faces.get(view)?.get(index).copied()
    }

    /// Record the faces `view`'s rows drew this pass, in row order.
    pub fn set_faces(&mut self, view: usize, faces: Vec<Rect>) {
        if self.faces.len() <= view {
            self.faces.resize(view + 1, Vec::new());
        }
        self.faces[view] = faces;
    }
}

/// The slot a menu-bar rectangle is remembered under: the strip itself, or one
/// title by its [`crate::menubar::Menu::ALL`] index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MenubarSlot {
    /// The strip behind the titles.
    Bar,
    /// One menu title, by its index in [`crate::menubar::Menu::ALL`].
    Title(usize),
}

/// The menu bar's logged geometry: the strip and each title, so a placement is
/// reported once (the toolbox's pattern) and a suite can click the titles.
#[derive(Default)]
pub struct MenubarSession {
    /// The strip's and each title's last logged rectangle, keyed by slot: the
    /// bar reports a placement once, and a title that moves is reported again.
    pub geometry: LoggedRects<MenubarSlot>,
    /// Each open menu entry's last logged rectangle, keyed `menu/label`; a
    /// menu's entries log while it is open, so their presence is the proof it
    /// opened.
    pub entries: LoggedRects<String>,
    /// Undo/redo the menus asked for, drained by the executable.
    pub history_requests: Vec<HistoryRequest>,
    /// The dock tab the Window menu asked for, drained by the executable.
    pub pending_tab: Option<EditorTab>,
    /// Whether the Window menu asked for one more scene view; the shell docks
    /// it after the pass, because the dock is borrowed by the `DockArea` while
    /// the menu draws.
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

/// Every module panel's build time.
///
/// Written by the shell's own pass; the dock's tab title and the executable's
/// panels dump read it. The measurement belongs to whoever timed the call, so
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

/// The rectangles a pass last reported, so a placement reaches the log once
/// rather than once a frame.
///
/// One type for every "log this rectangle once" cache: the comparison rule is
/// [`Self::remember`]'s, the prune is [`Self::forget_missing`]'s and dropping
/// everything is [`Self::clear`]'s, so eleven collections cannot drift into
/// eleven rules.
///
/// A placement is compared **quantized** to whole points by default, because
/// egui lays a panel out in fractional points: a fraction of a point of jitter
/// is the same place, and a line per frame of it is noise. A caller that needs
/// the pixel-exact comparison says so with [`Self::remember_exact`].
#[derive(Clone, Debug)]
pub struct LoggedRects<K: Ord + Clone> {
    /// The rectangle last logged per key, as it was logged.
    entries: BTreeMap<K, Rect>,
}

impl<K: Ord + Clone> Default for LoggedRects<K> {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
}

impl<K: Ord + Clone> LoggedRects<K> {
    /// Record `rect` for `key`, answering whether it is a placement to log.
    pub fn remember(&mut self, key: K, rect: Rect) -> bool {
        let quantized = Self::quantize(rect);
        match self.entries.get(&key) {
            Some(logged) if Self::quantize(*logged) == quantized => false,
            _ => {
                self.entries.insert(key, quantized);
                true
            }
        }
    }

    /// [`Self::remember`] without the quantization: any pixel difference is a
    /// new placement.
    pub fn remember_exact(&mut self, key: K, rect: Rect) -> bool {
        match self.entries.get(&key) {
            Some(logged) if *logged == rect => false,
            _ => {
                self.entries.insert(key, rect);
                true
            }
        }
    }

    /// The rectangle last logged for `key`.
    #[must_use]
    pub fn get(&self, key: &K) -> Option<Rect> {
        self.entries.get(key).copied()
    }

    /// Every remembered key with its rectangle, in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&K, Rect)> {
        self.entries.iter().map(|(key, rect)| (key, *rect))
    }

    /// Whether `key` has a logged rectangle.
    #[must_use]
    pub fn contains(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// Drop every key `seen` does not name.
    ///
    /// A row that stopped drawing is struck off instead of keeping its last
    /// placement forever: the geometry of everything a session has ever drawn
    /// is not worth remembering, and an unbounded cache of it is a slow leak.
    pub fn forget_missing(&mut self, seen: &BTreeSet<K>) {
        self.entries.retain(|key, _| seen.contains(key));
    }

    /// Drop every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// How many rectangles are remembered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is remembered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether two rectangles are the same placement under [`Self::remember`]'s
    /// rule, for the callers that keep a single rectangle rather than a cache.
    #[must_use]
    pub fn same_placement(left: Rect, right: Rect) -> bool {
        Self::quantize(left) == Self::quantize(right)
    }

    /// `rect` with both corners rounded to whole points.
    fn quantize(rect: Rect) -> Rect {
        Rect::from_min_max(
            Pos2::new(rect.min.x.round(), rect.min.y.round()),
            Pos2::new(rect.max.x.round(), rect.max.y.round()),
        )
    }
}

/// The texts a pass last reported per key, so a row logs a changed value once
/// rather than once a frame.
#[derive(Clone, Debug)]
pub struct LoggedTexts<K: Ord + Clone> {
    /// The text last logged per key.
    entries: BTreeMap<K, String>,
}

impl<K: Ord + Clone> Default for LoggedTexts<K> {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
}

impl<K: Ord + Clone> LoggedTexts<K> {
    /// Record `text` for `key`, answering whether it changed since the last
    /// draw that reported it.
    pub fn remember(&mut self, key: K, text: &str) -> bool {
        match self.entries.get(&key) {
            Some(logged) if logged == text => false,
            _ => {
                self.entries.insert(key, text.to_owned());
                true
            }
        }
    }

    /// The text last logged for `key`.
    #[must_use]
    pub fn get(&self, key: &K) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    /// Every remembered key with its text, in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &str)> {
        self.entries.iter().map(|(key, text)| (key, text.as_str()))
    }

    /// Drop every key `seen` does not name; see [`LoggedRects::forget_missing`].
    pub fn forget_missing(&mut self, seen: &BTreeSet<K>) {
        self.entries.retain(|key, _| seen.contains(key));
    }

    /// Drop every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// How many texts are remembered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is remembered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
    pub section_rects: LoggedRects<String>,
    /// The Add Component button's last logged rectangle.
    pub add_rect: Option<Rect>,
    /// Whether the Add Component popup is open.
    pub add_open: bool,
    /// The addable entries' last logged rectangles, keyed by type name.
    pub add_entries: LoggedRects<String>,
    /// The row model each drawn component's section shows, keyed by its
    /// registered type name: the rows, their once-per-subject log memory and
    /// the selection's write targets.
    pub row_models: std::collections::BTreeMap<String, crate::inspector::CachedRows>,
    /// The one-shot notices the Inspector already logged, keyed by what they
    /// describe (the selected asset's key): the tab redraws every frame.
    pub notices_logged: std::collections::BTreeSet<String>,
    /// The entity whose rows were last reported. A different subject clears
    /// the row memories below, because the rows on screen are that entity's.
    pub subject: Option<pill_engine::Entity>,
    /// Each asset reference row's last logged rectangle, keyed by
    /// `entity.component.field`: what a suite drops an asset onto.
    /// Only a row on screen keeps an entry; see the shell's
    /// `remember_asset_row_rect`.
    pub asset_row_rects: LoggedRects<String>,
    /// Whether the row and section rectangles reach the log.
    ///
    /// The frontend sets it from the frame's [`crate::boundary::EguiShellInput`]:
    /// a scripted run reads these rectangles to aim a click or a drop, and a
    /// person's editor does not need a line per placement.
    pub log_rects: bool,
    /// Each asset reference row's last logged text, keyed the same way: the
    /// row logs again whenever it draws a different asset.
    pub asset_row_texts: LoggedTexts<String>,
    /// The `component.field` whose asset picker is open, if one is.
    pub asset_picker: Option<String>,
    /// The open asset picker's search text.
    pub asset_search: String,
    /// Each entity reference row's last logged rectangle, keyed by
    /// `entity.component.field`: what a suite drags an
    /// entity onto.
    pub entity_row_rects: LoggedRects<String>,
    /// Each entity reference row's last logged text, keyed the same way: the
    /// row logs again whenever it draws a different entity.
    pub entity_row_texts: LoggedTexts<String>,
    /// The `component.field` whose entity picker is open, if one is.
    pub entity_picker: Option<String>,
    /// The open entity picker's search text.
    pub entity_search: String,
    /// The settings-document key (`textures.albedo`) whose picker is open, if
    /// one is.
    pub settings_picker: Option<String>,
    /// The open settings picker's search text.
    pub settings_search: String,
    /// Each reference-list row's last logged rectangle, keyed by
    /// `asset:family[index]`: what a suite drags to reorder a list.
    pub settings_row_rects: LoggedRects<String>,
    /// Each Hierarchy row's last logged rectangle, keyed by the entity's
    /// display: what a suite drags onto an entity reference
    /// row. The Entities tab shares this session, since the cache is the same
    /// "geometry the log already reported" concern.
    pub hierarchy_row_rects: LoggedRects<String>,
    /// Each scene badge's last logged rectangle, keyed by `view:entity`:
    /// what a suite clicks to select through a badge.
    pub badge_rects: LoggedRects<String>,
    /// The Hierarchy rows this pass drew, so [`Self::begin_geometry_pass`] can
    /// drop the ones that left: the tab redraws what it shows, and the
    /// Entities list can be tens of thousands of rows long.
    pub hierarchy_seen: BTreeSet<String>,
    /// The scene badges this pass drew, keyed like [`Self::badge_rects`].
    pub badge_seen: BTreeSet<String>,
}

impl InspectorSession {
    /// Forget every rectangle and text the log already reported.
    ///
    /// Called when the subject changes: the rows on screen are another
    /// entity's, so their geometry has to be reported again for the new one.
    pub fn clear_geometry(&mut self) {
        self.section_rects.clear();
        self.add_rect = None;
        self.add_entries.clear();
        self.asset_row_rects.clear();
        self.asset_row_texts.clear();
        self.entity_row_rects.clear();
        self.entity_row_texts.clear();
        self.settings_row_rects.clear();
        self.hierarchy_row_rects.clear();
        self.badge_rects.clear();
    }

    /// Start a frame of geometry reporting: drop the rows and badges the
    /// previous frame did not draw, then collect this frame's.
    ///
    /// The seen sets accumulate every pass of a frame, the main dock pass and
    /// the popped-out tabs', so a tab that draws in its own window still counts
    /// as drawn. Without the prune the two caches grow with every entity the
    /// session ever listed and never shrink again; a hidden row's rectangle is
    /// not one a suite can click, so forgetting it costs nothing - it logs
    /// again when the row comes back.
    pub fn begin_geometry_pass(&mut self) {
        let hierarchy_seen = std::mem::take(&mut self.hierarchy_seen);
        self.hierarchy_row_rects.forget_missing(&hierarchy_seen);
        let badge_seen = std::mem::take(&mut self.badge_seen);
        self.badge_rects.forget_missing(&badge_seen);
    }
}

/// The preview state of every asset tile, keyed by the panel's row key.
///
/// One value instead of three parallel collections. A key is ready, refused or
/// neither; the visible list is rebuilt every pass; and the two rules that have
/// to agree live here rather than in a convention between the shell and the
/// executable: a key is never both ready and refused ([`PreviewStates::mark_ready`]
/// clears the refusal, [`PreviewStates::mark_refused`] drops the texture), and
/// only a tile this pass drew is asked for ([`PreviewStates::wants`]).
#[derive(Default)]
pub struct PreviewStates {
    ready: BTreeMap<String, TextureHandle>,
    refused: BTreeSet<String>,
    visible: Vec<String>,
}

impl PreviewStates {
    /// Start a pass: the visible list is whatever this pass draws.
    pub fn begin_pass(&mut self) {
        self.visible.clear();
    }

    /// Record one tile the grid drew, in draw order.
    pub fn push_visible(&mut self, key: &str) {
        self.visible.push(key.to_string());
    }

    /// The keys this pass drew, in draw order.
    #[must_use]
    pub fn visible(&self) -> &[String] {
        &self.visible
    }

    /// The rendered texture for `key`, when there is one.
    #[must_use]
    pub fn ready(&self, key: &str) -> Option<&TextureHandle> {
        self.ready.get(key)
    }

    /// Whether `key` may be requested now: drawn this pass, not ready, not
    /// refused.
    #[must_use]
    pub fn wants(&self, key: &str) -> bool {
        self.visible.iter().any(|visible| visible == key)
            && !self.ready.contains_key(key)
            && !self.refused.contains(key)
    }

    /// Record a rendered texture for `key`.
    pub fn mark_ready(&mut self, key: &str, texture: TextureHandle) {
        self.refused.remove(key);
        self.ready.insert(key.to_string(), texture);
    }

    /// Record that `key` was answered without pixels: an undecodable file or a
    /// frame that drew nothing. The tile keeps its type icon and the grid asks
    /// for the next one instead of waiting on this one forever.
    pub fn mark_refused(&mut self, key: &str) {
        self.ready.remove(key);
        self.refused.insert(key.to_string());
    }

    /// Drop every preview whose key the listing still holds, when `unchanged`
    /// says so.
    pub fn retain_keys(&mut self, kept: &BTreeSet<String>) {
        self.ready.retain(|key, _| kept.contains(key));
        self.refused.retain(|key| kept.contains(key));
    }

    /// Forget the refusals, so the next pass asks for those tiles again.
    pub fn clear_refused(&mut self) {
        self.refused.clear();
    }

    /// Forget every preview.
    pub fn clear(&mut self) {
        self.ready.clear();
        self.refused.clear();
    }
}

impl Resource for EguiShellState {}

/// What one generation's claim on the session's egui contexts cleared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContextClaim {
    /// The entries the main context's `Memory::data` held.
    pub main_entries: usize,
    /// How many popped-out windows' contexts held anything.
    pub popout_windows: usize,
}

impl EguiShellState {
    /// Clear every egui context this session owns, answering what was cleared.
    ///
    /// The set of contexts to clear is whatever the session owns - the main one
    /// and one per popped-out tab - so a third kind of context joins this
    /// method instead of gaining a flag of its own in the shell. A shell
    /// generation calls it on its first frame, before its pass: a value the
    /// previous generation left in `Memory::data` carries that image's drop
    /// code, and touching it after the reload is a use-after-unmap.
    pub fn claim_generation(&mut self) -> ContextClaim {
        let main_entries = self.context.memory_mut(|memory| {
            let entries = memory.data.len();
            memory.data.clear();
            entries
        });
        let mut popout_windows = 0;
        for context in self.popout_contexts.values() {
            let entries = context.memory_mut(|memory| {
                let entries = memory.data.len();
                memory.data.clear();
                entries
            });
            if entries > 0 {
                popout_windows += 1;
            }
        }
        ContextClaim {
            main_entries,
            popout_windows,
        }
    }

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
            // The record already holds whatever the executable resolved, and
            // the install above put it in this context: the shell is current
            // until the next generation arrives.
            applied_theme_generation: crate::style::generation(),
            asset_dialog: None,
            preferences_draft: None,
            toolbox: ToolboxSession::default(),
            browser: ProjectBrowser::default(),
            mark: None,
            previews: PreviewStates::default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A cell of the log-token surface: a rectangle that repeats draws once, a
    /// rectangle that moves draws again, and a key that stops drawing is
    /// dropped so the cache cannot grow with everything the session ever drew.
    #[test]
    fn a_logged_rect_answers_once_per_placement() {
        let mut logged = LoggedRects::default();
        let rect = Rect::from_min_max(Pos2::new(10.0, 20.0), Pos2::new(110.0, 40.0));
        assert!(
            logged.remember("row".to_owned(), rect),
            "the first draw logs"
        );
        assert!(
            !logged.remember("row".to_owned(), rect),
            "the same placement does not log again"
        );
        let jittered = rect.translate(egui::vec2(0.2, -0.3));
        assert!(
            !logged.remember("row".to_owned(), jittered),
            "a fraction of a point is the same placement"
        );
        let moved = rect.translate(egui::vec2(0.0, 12.0));
        assert!(logged.remember("row".to_owned(), moved), "a move logs");

        let seen = BTreeSet::from(["other".to_owned()]);
        logged.forget_missing(&seen);
        assert!(logged.is_empty(), "a key that stopped drawing is dropped");
    }

    /// The strict comparison is the caller's choice, not the type's second
    /// accidental rule: half a point is a new placement only for [`LoggedRects::remember_exact`].
    #[test]
    fn the_exact_rule_takes_every_pixel() {
        let mut logged = LoggedRects::default();
        let rect = Rect::from_min_max(Pos2::new(10.0, 20.0), Pos2::new(110.0, 40.0));
        assert!(logged.remember_exact("header".to_owned(), rect));
        assert!(!logged.remember_exact("header".to_owned(), rect));
        assert!(logged.remember_exact("header".to_owned(), rect.translate(egui::vec2(0.5, 0.0))));
    }

    /// A row logs the text it draws once per change, not once per frame.
    #[test]
    fn a_logged_text_answers_only_on_change() {
        let mut logged = LoggedTexts::default();
        assert!(logged.remember("cfg:speed".to_owned(), "1.5"));
        assert!(!logged.remember("cfg:speed".to_owned(), "1.5"));
        assert!(logged.remember("cfg:speed".to_owned(), "2.0"));
        assert_eq!(logged.get(&"cfg:speed".to_owned()), Some("2.0"));
        let seen = BTreeSet::new();
        logged.forget_missing(&seen);
        assert!(logged.is_empty());
    }
}
