//! The editor's tabs and the default dock layout.
//!
//! # Responsibilities
//!
//! - [`EditorTab`]: the editor's eight tabs - the game viewport and the
//!   two scene viewports' placeholders, the world's entities and systems,
//!   the selection's inspector, the assets mock, and the preferences form -
//!   and their titles.
//! - [`EditorDock`]: the `egui_dock` state with the default layout (the
//!   scene and game views in the center, the entity column on the left, the
//!   inspector above the system column on the right, the assets row below),
//!   the schema version its documents carry, and
//!   [`EditorDock::settled`] which replaces the placeholder rectangles an
//!   unpacked layout has not computed yet.
//!
//! # Design
//!
//! The dock state is the shell's long-lived state, so it lives in the shared
//! core crate rather than the reloadable shell module: a reload of the shell
//! finds the layout exactly where the user left it, and persistence carries
//! it across restarts.

// External crates
use egui::Rect;
use egui_dock::{DockState, Node, NodeIndex};
use serde::{Deserialize, Serialize};

/// The fraction of the window's width one side column takes in the default
/// layout.
pub const SIDE_COLUMN_FRACTION: f32 = 0.22;

/// The fraction of the window's height the bottom row takes in the default
/// layout.
pub const BOTTOM_ROW_FRACTION: f32 = 0.3;

/// The fraction of the right column's height the Inspector takes in the
/// default layout; the Systems list keeps the rest below it.
pub const INSPECTOR_COLUMN_FRACTION: f32 = 0.4;

/// One tab of the egui editor.
///
/// The enum is serialized into the saved layout, so adding a tab is a
/// schema change (`LAYOUT_SCHEMA_VERSION`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum EditorTab {
    /// A scene view's placeholder: the shell measures this tab's body and
    /// draws nothing, so that view's child window shows through. The payload
    /// is the view's index, which is what pairs the tab with a child window,
    /// a renderer surface and a camera (W-0038 Stage 21).
    Scene(u8),
    /// The game viewport's placeholder: like the scene tabs, but its child
    /// window carries the primary surface, so it shows the project through
    /// the project's own camera.
    Game,
    /// The world's entities: id, name, click to select.
    Entities,
    /// The world's systems: name, stage, enabled.
    Systems,
    /// The selected entity's detail: its components and their field values.
    Inspector,
    /// The assets panel's mock: fixed rows until the asset store is ported.
    Assets,
    /// The engine's and the project's recent log lines (W-0038 Stage 5).
    Console,
    /// Build errors and warnings, grouped by module (W-0038 Stage 6).
    Problems,
    /// Frame, pass and per-tab timings (W-0038 Stage 7).
    Statistics,
    /// The preferences file's editor-wide rows and the current project's
    /// remembered gizmo choices.
    Preferences,
    /// A panel a module registered (W-0038 Stage 11): the payload is the
    /// panel's slot in the registry, which is stable across that module's
    /// reload. The tab's title comes from the registry, so a panel whose
    /// module unloaded reads "module not loaded".
    Module(u16),
}

impl<'de> Deserialize<'de> for EditorTab {
    /// Reads the current form (`{"Scene": 2}`) and the one every document
    /// before v12 wrote (a bare `"Scene"`, `"SecondScene"`), because the
    /// layout's migrations carry old documents forward instead of replacing
    /// them. Only JSON is read: the layout file is the only format.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(name) = value.as_str() {
            return match name {
                // The two viewports every document before v12 had.
                "Scene" => Ok(EditorTab::Scene(0)),
                "SecondScene" => Ok(EditorTab::Scene(1)),
                "Game" => Ok(EditorTab::Game),
                "Entities" => Ok(EditorTab::Entities),
                "Systems" => Ok(EditorTab::Systems),
                "Inspector" => Ok(EditorTab::Inspector),
                "Assets" => Ok(EditorTab::Assets),
                "Console" => Ok(EditorTab::Console),
                "Problems" => Ok(EditorTab::Problems),
                "Statistics" => Ok(EditorTab::Statistics),
                "Preferences" => Ok(EditorTab::Preferences),
                other => Err(D::Error::custom(format!("unknown editor tab {other}"))),
            };
        }
        let Some((name, index)) = value
            .as_object()
            .and_then(|object| object.iter().next())
            .and_then(|(name, index)| Some((name.as_str(), index.as_u64()?)))
        else {
            return Err(D::Error::custom(
                "an editor tab is a name, or a name with an index",
            ));
        };
        let index = u16::try_from(index)
            .map_err(|_| D::Error::custom(format!("editor tab {name} is out of range")))?;
        match name {
            "Scene" => Ok(EditorTab::Scene(index as u8)),
            "Module" => Ok(EditorTab::Module(index)),
            other => Err(D::Error::custom(format!(
                "editor tab {other} takes no index"
            ))),
        }
    }
}

impl EditorTab {
    /// Every tab, in tab-strip order.
    pub const ALL: [EditorTab; 11] = [
        EditorTab::Scene(0),
        EditorTab::Scene(1),
        EditorTab::Game,
        EditorTab::Entities,
        EditorTab::Systems,
        EditorTab::Inspector,
        EditorTab::Assets,
        EditorTab::Console,
        EditorTab::Problems,
        EditorTab::Statistics,
        EditorTab::Preferences,
    ];

    /// The view index, when this is a scene view's tab.
    pub fn scene_view(self) -> Option<u8> {
        match self {
            EditorTab::Scene(index) => Some(index),
            _ => None,
        }
    }

    /// The tab's display title.
    ///
    /// A title, not a `&'static str`: the scene views are numbered, so the
    /// second and later ones build their own name.
    pub fn title(self) -> String {
        match self {
            EditorTab::Scene(0) => "Scene".to_string(),
            EditorTab::Scene(index) => format!("Scene {}", index as u16 + 1),
            EditorTab::Game => "Game".to_string(),
            EditorTab::Entities => "Entities".to_string(),
            EditorTab::Systems => "Systems".to_string(),
            EditorTab::Inspector => "Inspector".to_string(),
            EditorTab::Assets => "Assets".to_string(),
            EditorTab::Console => "Console".to_string(),
            EditorTab::Problems => "Problems".to_string(),
            EditorTab::Statistics => "Statistics".to_string(),
            EditorTab::Preferences => "Preferences".to_string(),
            // A module panel's title belongs to the registry, not here: the
            // shell asks it, because it changes when the module unloads.
            EditorTab::Module(_) => "Module".to_string(),
        }
    }

    /// The wire name the logs and the scripted channel use: `scene`, then
    /// `scene2` for the second view and on.
    pub fn name(self) -> String {
        match self {
            EditorTab::Scene(0) => "scene".to_string(),
            EditorTab::Scene(index) => format!("scene{}", index as u16 + 1),
            EditorTab::Game => "game".to_string(),
            EditorTab::Entities => "entities".to_string(),
            EditorTab::Systems => "systems".to_string(),
            EditorTab::Inspector => "inspector".to_string(),
            EditorTab::Assets => "assets".to_string(),
            EditorTab::Console => "console".to_string(),
            EditorTab::Problems => "problems".to_string(),
            EditorTab::Statistics => "statistics".to_string(),
            EditorTab::Preferences => "preferences".to_string(),
            EditorTab::Module(_) => "module".to_string(),
        }
    }

    /// The panel slot, when this is a module panel's tab.
    pub fn module_slot(self) -> Option<u16> {
        match self {
            EditorTab::Module(slot) => Some(slot),
            _ => None,
        }
    }
}

/// The editor's dock: an `egui_dock` state with the editor's default
/// layout.
///
/// The shell keeps one of these in its session resource, so a reload of the
/// shell module finds the dock exactly where the user left it; persistence
/// carries it across restarts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditorDock {
    /// The schema this document was written with.
    schema_version: u32,
    /// The panel id each module tab's slot named when the document was
    /// written (W-0038 Stage 11): a slot means nothing to the next run, the
    /// id does, so a panel that registers again finds its tab.
    #[serde(default)]
    panel_ids: Vec<String>,
    /// The tabs popped out into their own OS windows (W-0038 Stage 23). A
    /// popped-out tab is *not* in the dock tree; redocking puts it back in the
    /// focused leaf.
    #[serde(default)]
    popouts: Vec<EditorTab>,
    /// The dock's tabs and splits.
    state: DockState<EditorTab>,
}

impl Default for EditorDock {
    fn default() -> Self {
        Self::new()
    }
}

impl EditorDock {
    /// A dock with the default layout: the game and scene viewports tabbed
    /// together in the center, the entity list as the left column, the
    /// inspector above the system list on the right, and the assets mock as
    /// a bottom row across the window.
    pub fn new() -> Self {
        let mut state = DockState::new(vec![
            EditorTab::Scene(0),
            EditorTab::Scene(1),
            EditorTab::Game,
        ]);
        let tree = state.main_surface_mut();
        // The assets row spans the full width, so it splits off first; the
        // old node keeps the space above it.
        let [upper, _assets] = tree.split_below(
            NodeIndex::root(),
            1.0 - BOTTOM_ROW_FRACTION,
            vec![
                EditorTab::Assets,
                EditorTab::Console,
                EditorTab::Problems,
                EditorTab::Statistics,
            ],
        );
        // The entity column then splits the upper node; the old node keeps
        // the space right of the column. Unlike `split_below`/`split_right`,
        // whose fraction is taken by the old node, `split_left` puts the new
        // node in the fraction-sized slot on the left.
        let [middle, _entities] =
            tree.split_left(upper, SIDE_COLUMN_FRACTION, vec![EditorTab::Entities]);
        // The right column takes as much of the middle node as a side
        // column takes of the whole window; it carries the inspector.
        let system_fraction = 1.0 - SIDE_COLUMN_FRACTION / (1.0 - SIDE_COLUMN_FRACTION);
        let [_center, right_column] =
            tree.split_right(middle, system_fraction, vec![EditorTab::Inspector]);
        // The inspector sits on top of the right column; `split_below`'s
        // fraction is the share the old node keeps, so the systems list
        // takes the space below it - with the preferences form tabbed
        // beside it.
        let [_inspector, _systems] = tree.split_below(
            right_column,
            INSPECTOR_COLUMN_FRACTION,
            vec![EditorTab::Systems, EditorTab::Preferences],
        );
        Self {
            schema_version: super::LAYOUT_SCHEMA_VERSION,
            panel_ids: Vec::new(),
            popouts: Vec::new(),
            state,
        }
    }

    /// The dock state, for the shell's `DockArea`.
    pub fn state(&self) -> &DockState<EditorTab> {
        &self.state
    }

    /// The dock state, mutably: the shell runs the tabs through this, and
    /// `egui_dock` applies drags and closes to it directly.
    pub fn state_mut(&mut self) -> &mut DockState<EditorTab> {
        &mut self.state
    }

    /// The schema version the document carries.
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Stamp the document with `version`: what a migration does before it is
    /// written back.
    pub fn set_schema_version(&mut self, version: u32) {
        self.schema_version = version;
    }

    /// Whether `tab` is docked somewhere: the main surface or a window.
    pub fn contains(&self, tab: EditorTab) -> bool {
        // `DockState::find_tab` reports the surface and node holding the tab.
        self.state.find_tab(&tab).is_some()
    }

    /// The tab a new scene view takes: one past the highest view index the
    /// dock holds today (W-0038 Stage 21).
    ///
    /// Highest plus one rather than "one per view", so a view the user closed
    /// in the middle never hands its index to a second tab - the closed
    /// view's child window and camera are gone, and a new tab taking the
    /// index would find them in the frontend's list.
    pub fn next_scene_view(&self) -> EditorTab {
        let highest = self
            .state
            .iter_all_tabs()
            .filter_map(|(_path, tab)| tab.scene_view())
            .chain(self.popouts.iter().filter_map(|tab| tab.scene_view()))
            .max();
        EditorTab::Scene(highest.map_or(0, |index| index.saturating_add(1)))
    }

    /// How many scene view slots the dock needs: one past its highest view
    /// index, so a view whose tab is not shown keeps its slot.
    ///
    /// A popped-out view counts: it is still a view, its child window
    /// replaced by a window of its own, and dropping its slot would hand its
    /// index to the next view opened (W-0038 Stage 23).
    pub fn scene_view_count(&self) -> usize {
        self.state
            .iter_all_tabs()
            .filter_map(|(_path, tab)| tab.scene_view())
            .chain(self.popouts.iter().filter_map(|tab| tab.scene_view()))
            .max()
            .map_or(0, |index| index as usize + 1)
    }

    /// Whether one more scene view fits: each view is a child window and a
    /// renderer surface, so the frontend's limit is the dock's too.
    pub fn can_open_scene_view(&self) -> bool {
        self.scene_view_count() < crate::boundary::SCENE_VIEWS_MAX
    }

    /// Whether the document was written by this build's schema.
    pub(crate) fn is_current(&self) -> bool {
        self.schema_version == super::LAYOUT_SCHEMA_VERSION
    }

    /// Record the registry's panel ids, so the module tabs a save writes can be
    /// found again by id on the next run.
    pub fn set_panel_ids(&mut self, ids: Vec<String>) {
        self.panel_ids = ids;
    }

    /// The panel ids the document carried.
    pub fn panel_ids(&self) -> &[String] {
        &self.panel_ids
    }

    /// The tabs popped out into their own OS windows.
    pub fn popouts(&self) -> &[EditorTab] {
        &self.popouts
    }

    /// Whether `tab` is currently popped out.
    pub fn is_popped_out(&self, tab: EditorTab) -> bool {
        self.popouts.contains(&tab)
    }

    /// Pop `tab` out into its own window: it leaves the dock tree, so the dock
    /// draws it nowhere, and joins `popouts` so the next save remembers it.
    /// Returns whether the tab was docked and moved.
    pub fn pop_out(&mut self, tab: EditorTab) -> bool {
        if self.popouts.contains(&tab) {
            return false;
        }
        // The last tab is never popped out: an empty dock has no focused leaf
        // for a redock to return to.
        if self.state.iter_all_tabs().count() <= 1 {
            return false;
        }
        let Some(path) = self.state.find_tab(&tab) else {
            return false;
        };
        if self.state.remove_tab(path).is_none() {
            return false;
        }
        self.popouts.push(tab);
        true
    }

    /// Put a popped-out `tab` back into the focused leaf.
    /// Returns whether it was popped out.
    pub fn redock(&mut self, tab: EditorTab) -> bool {
        let Some(index) = self.popouts.iter().position(|popped| *popped == tab) else {
            return false;
        };
        self.popouts.remove(index);
        self.state.push_to_focused_leaf(tab);
        true
    }

    /// Rewrite every module tab's slot from the ids the document carried to
    /// the slots `ids` assigns today.
    ///
    /// A tab whose panel is no longer registered keeps its slot value only
    /// when that slot still exists; otherwise it points past the registry and
    /// the shell draws the "module panel" placeholder - the tab is the user's
    /// and is not taken away without a word.
    pub fn remap_panel_slots(&mut self, ids: &[String]) {
        let recorded = std::mem::take(&mut self.panel_ids);
        for (_path, tab) in self.state.iter_all_tabs_mut() {
            let Some(slot) = tab.module_slot() else {
                continue;
            };
            let resolved = recorded
                .get(slot as usize)
                .and_then(|id| ids.iter().position(|known| known == id))
                .map_or(u16::MAX, |index| index as u16);
            *tab = EditorTab::Module(resolved);
        }
        self.set_panel_ids(ids.to_vec());
    }

    /// A copy with placeholder geometry replaced by finite values.
    ///
    /// A fresh dock's nodes start with `Rect::NOTHING` (`±INF`) until the
    /// first `DockArea::show` lays them out; serde_json writes non-finite
    /// floats as `null`, which cannot be read back. The values are
    /// recomputed on the next show, so a zero placeholder is safe.
    pub(crate) fn settled(&self) -> Self {
        let mut dock = self.clone();
        for (_surface, node) in dock.state.iter_all_nodes_mut() {
            match node {
                Node::Leaf(leaf) => {
                    leaf.rect = settle_rect(leaf.rect);
                    leaf.viewport = settle_rect(leaf.viewport);
                }
                Node::Vertical(split) | Node::Horizontal(split) => {
                    split.rect = settle_rect(split.rect);
                }
                // An empty node holds no geometry to settle.
                Node::Empty => {}
            }
        }
        dock
    }
}

/// `rect`, or a finite zero rect when it still holds placeholder infinities.
fn settle_rect(rect: Rect) -> Rect {
    let finite = |value: f32| value.is_finite();
    if finite(rect.min.x) && finite(rect.min.y) && finite(rect.max.x) && finite(rect.max.y) {
        rect
    } else {
        Rect::ZERO
    }
}

/// Bring `tab` to the front of its group, returning whether the dock held it.
///
/// The Window menu and the scripted `tab` share this path. `set_active_tab`
/// takes the tab's index inside its leaf, and the node index must address the
/// leaf on its own surface: a floating surface's tree is separate.
pub fn activate_tab(dock: &mut DockState<EditorTab>, tab: EditorTab) -> bool {
    let Some(path) = dock.find_tab(&tab) else {
        return false;
    };
    // A path the dock no longer holds is a no-op, not an error to report.
    if dock.set_active_tab(path).is_err() {
        return false;
    }
    dock.set_focused_node_and_surface(path.node_path());
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_dock::SurfaceIndex;

    #[test]
    fn the_default_layout_holds_every_tab() {
        let dock = EditorDock::new();
        for tab in EditorTab::ALL {
            assert!(
                dock.contains(tab),
                "{tab:?} is missing from the default layout"
            );
        }
    }

    #[test]
    fn the_default_layout_centers_scene_with_columns_and_a_bottom_row() {
        let dock = EditorDock::new();
        let state = dock.state();
        let surface = SurfaceIndex::main();
        let tabs_at = |index: NodeIndex| -> Vec<EditorTab> {
            state[surface][index]
                .get_leaf()
                .expect("the layout index points at a leaf")
                .tabs
                .clone()
        };
        // A split's fraction is the share of its top/left child, which is
        // the slot each `split_*` call assigns by its own rule.
        let fraction_at = |index: NodeIndex| -> f32 {
            match &state[surface][index] {
                Node::Vertical(split) | Node::Horizontal(split) => split.fraction,
                _ => panic!("the layout index points at a split"),
            }
        };
        // The root stacks the assets row below everything else.
        let root = NodeIndex::root();
        assert!(
            state[surface][root].is_vertical(),
            "the assets row is not a bottom split"
        );
        assert_eq!(
            tabs_at(root.right()),
            vec![
                EditorTab::Assets,
                EditorTab::Console,
                EditorTab::Problems,
                EditorTab::Statistics
            ]
        );
        // The node above splits into the entity column and the rest.
        let upper = root.left();
        assert!(
            state[surface][upper].is_horizontal(),
            "the entity column is not a side split"
        );
        assert_eq!(tabs_at(upper.left()), vec![EditorTab::Entities]);
        // The rest splits into the scene center and the system column; the
        // two scene viewports and the game view share the center group.
        let rest = upper.right();
        assert!(
            state[surface][rest].is_horizontal(),
            "the system column is not a side split"
        );
        assert_eq!(
            tabs_at(rest.left()),
            vec![EditorTab::Scene(0), EditorTab::Scene(1), EditorTab::Game]
        );
        // The right column stacks the inspector above the systems list,
        // with the preferences form tabbed beside the list.
        let column = rest.right();
        assert!(
            state[surface][column].is_vertical(),
            "the inspector is not stacked above the systems list"
        );
        assert_eq!(tabs_at(column.left()), vec![EditorTab::Inspector]);
        assert_eq!(
            tabs_at(column.right()),
            vec![EditorTab::Systems, EditorTab::Preferences]
        );
        // Each side column is one `SIDE_COLUMN_FRACTION` of its split, the
        // assets row one `BOTTOM_ROW_FRACTION` of the window, the inspector
        // one `INSPECTOR_COLUMN_FRACTION` of its column, and the scene
        // keeps the rest.
        assert!((fraction_at(root) - (1.0 - BOTTOM_ROW_FRACTION)).abs() < f32::EPSILON);
        assert!((fraction_at(upper) - SIDE_COLUMN_FRACTION).abs() < f32::EPSILON);
        let scene_share = 1.0 - SIDE_COLUMN_FRACTION / (1.0 - SIDE_COLUMN_FRACTION);
        assert!((fraction_at(rest) - scene_share).abs() < f32::EPSILON);
        assert!((fraction_at(column) - INSPECTOR_COLUMN_FRACTION).abs() < f32::EPSILON);
    }

    #[test]
    fn a_layout_round_trips_through_serde() {
        let dock = EditorDock::new();
        // The save path settles the placeholder rects first; a fresh dock is
        // the worst case and must round-trip.
        let json = serde_json::to_string(&dock.settled()).expect("serialize the dock");
        let restored: EditorDock = serde_json::from_str(&json).expect("deserialize the dock");
        for tab in EditorTab::ALL {
            assert!(restored.contains(tab));
        }
        assert_eq!(restored.schema_version(), dock.schema_version());
    }

    #[test]
    fn popping_a_tab_out_removes_it_from_the_tree_and_remembers_it() {
        let mut dock = EditorDock::new();
        assert!(dock.pop_out(EditorTab::Inspector));
        assert!(
            !dock.contains(EditorTab::Inspector),
            "the tab is still docked"
        );
        assert_eq!(dock.popouts(), [EditorTab::Inspector]);
        // Popping the same tab out twice changes nothing: the dock no longer
        // holds it, so there is no second tab to move.
        assert!(!dock.pop_out(EditorTab::Inspector));
    }

    #[test]
    fn a_popped_out_tab_redocks_into_the_focused_leaf() {
        let mut dock = EditorDock::new();
        assert!(dock.pop_out(EditorTab::Inspector));
        assert!(dock.redock(EditorTab::Inspector));
        assert!(
            dock.contains(EditorTab::Inspector),
            "the tab did not come back"
        );
        assert!(dock.popouts().is_empty(), "the dock still lists a pop-out");
        // A tab that was never popped out has nothing to redock.
        assert!(!dock.redock(EditorTab::Console));
    }

    #[test]
    fn the_last_docked_tab_is_never_popped_out() {
        let mut dock = EditorDock::new();
        for tab in [
            EditorTab::Scene(0),
            EditorTab::Scene(1),
            EditorTab::Game,
            EditorTab::Entities,
            EditorTab::Systems,
            EditorTab::Preferences,
            EditorTab::Inspector,
            EditorTab::Console,
            EditorTab::Problems,
            EditorTab::Statistics,
        ] {
            assert!(dock.pop_out(tab), "{tab:?} could not be popped out");
        }
        assert_eq!(dock.state().iter_all_tabs().count(), 1);
        assert!(
            !dock.pop_out(EditorTab::Assets),
            "the last docked tab left the dock without a leaf to return to"
        );
    }

    #[test]
    fn a_popped_out_scene_view_keeps_its_slot() {
        let mut dock = EditorDock::new();
        // The default layout opens two views; the third is the highest slot.
        dock.state_mut().push_to_focused_leaf(EditorTab::Scene(2));
        assert_eq!(dock.scene_view_count(), 3);
        assert!(dock.pop_out(EditorTab::Scene(2)));
        // The view left the tree but not the bookkeeping: its slot stays, so
        // the frontend keeps its window and the next view does not take its
        // index.
        assert_eq!(dock.scene_view_count(), 3);
        assert_eq!(dock.next_scene_view(), EditorTab::Scene(3));
    }

    #[test]
    fn activating_a_tab_brings_it_to_its_leaf_s_front() {
        let mut dock = EditorDock::new();
        assert!(activate_tab(dock.state_mut(), EditorTab::Systems));
        let path = dock
            .state()
            .find_tab(&EditorTab::Systems)
            .expect("the systems tab is in the default dock");
        let leaf = dock.state()[path.surface][path.node]
            .get_leaf()
            .expect("the tab sits in a leaf");
        let active = leaf.tabs[leaf.active.0];
        assert_eq!(active, EditorTab::Systems);
    }

    #[test]
    fn activating_a_tab_the_dock_lacks_reports_it() {
        let mut state = DockState::new(vec![EditorTab::Scene(0)]);
        assert!(!activate_tab(&mut state, EditorTab::Systems));
        assert!(activate_tab(&mut state, EditorTab::Scene(0)));
    }

    #[test]
    fn a_new_scene_view_takes_the_next_index() {
        let dock = EditorDock::new();
        assert_eq!(dock.next_scene_view(), EditorTab::Scene(2));
        let mut dock = dock;
        dock.state_mut().push_to_focused_leaf(EditorTab::Scene(2));
        assert_eq!(dock.next_scene_view(), EditorTab::Scene(3));
        assert_eq!(dock.scene_view_count(), 3);
    }

    #[test]
    fn a_second_view_survives_as_its_own_tab_and_title() {
        let dock = EditorDock::new();
        assert!(dock.contains(EditorTab::Scene(0)));
        assert!(dock.contains(EditorTab::Scene(1)));
        assert_eq!(EditorTab::Scene(0).title(), "Scene");
        assert_eq!(EditorTab::Scene(1).title(), "Scene 2");
        assert_eq!(EditorTab::Scene(2).title(), "Scene 3");
        assert_eq!(EditorTab::Scene(0).name(), "scene");
        assert_eq!(EditorTab::Scene(1).name(), "scene2");
        assert_eq!(EditorTab::Scene(2).name(), "scene3");
    }

    #[test]
    fn a_v11_document_reads_its_two_scene_tabs_as_views() {
        // What every document before v12 wrote: bare variant names, which the
        // migration then carries forward instead of dropping the layout.
        let tab: EditorTab = serde_json::from_str("\"Scene\"").expect("the legacy scene tab");
        assert_eq!(tab, EditorTab::Scene(0));
        let tab: EditorTab =
            serde_json::from_str("\"SecondScene\"").expect("the legacy second tab");
        assert_eq!(tab, EditorTab::Scene(1));
        let tab: EditorTab = serde_json::from_str("\"Systems\"").expect("a legacy unit tab");
        assert_eq!(tab, EditorTab::Systems);
        let tab: EditorTab = serde_json::from_str("{\"Scene\":3}").expect("an indexed scene tab");
        assert_eq!(tab, EditorTab::Scene(3));
        let tab: EditorTab = serde_json::from_str("{\"Module\":2}").expect("an indexed panel tab");
        assert_eq!(tab, EditorTab::Module(2));
        // An unknown name is a refusal, not a silently dropped tab.
        assert!(serde_json::from_str::<EditorTab>("\"Nonsense\"").is_err());
        assert!(serde_json::from_str::<EditorTab>("{\"Game\":1}").is_err());
    }
}
