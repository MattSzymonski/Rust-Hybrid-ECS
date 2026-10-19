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
use pill_ui_egui::egui::Rect;
use pill_ui_egui::egui_dock::{DockState, Node, NodeIndex, TabIndex};
use serde::{Deserialize, Serialize};

/// The fraction of the window's width one side column takes in the default
/// layout.
pub const SIDE_COLUMN_FRACTION: f32 = 0.16;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EditorTab {
    /// The first scene view's placeholder: the shell measures this tab's
    /// body and draws nothing, so the scene's child window shows through.
    Scene,
    /// The second scene view's placeholder: like [`EditorTab::Scene`], with
    /// its own child window and renderer surface.
    SecondScene,
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
    /// The preferences file's editor-wide rows and the current project's
    /// remembered gizmo choices.
    Preferences,
}

impl EditorTab {
    /// Every tab, in tab-strip order.
    pub const ALL: [EditorTab; 8] = [
        EditorTab::Scene,
        EditorTab::SecondScene,
        EditorTab::Game,
        EditorTab::Entities,
        EditorTab::Systems,
        EditorTab::Inspector,
        EditorTab::Assets,
        EditorTab::Preferences,
    ];

    /// The tab's display title.
    pub fn title(self) -> &'static str {
        match self {
            EditorTab::Scene => "Scene",
            EditorTab::SecondScene => "Scene 2",
            EditorTab::Game => "Game",
            EditorTab::Entities => "Entities",
            EditorTab::Systems => "Systems",
            EditorTab::Inspector => "Inspector",
            EditorTab::Assets => "Assets",
            EditorTab::Preferences => "Preferences",
        }
    }

    /// The wire name the logs and the scripted channel use.
    pub fn name(self) -> &'static str {
        match self {
            EditorTab::Scene => "scene",
            EditorTab::SecondScene => "scene2",
            EditorTab::Game => "game",
            EditorTab::Entities => "entities",
            EditorTab::Systems => "systems",
            EditorTab::Inspector => "inspector",
            EditorTab::Assets => "assets",
            EditorTab::Preferences => "preferences",
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
            EditorTab::Scene,
            EditorTab::SecondScene,
            EditorTab::Game,
        ]);
        let tree = state.main_surface_mut();
        // The assets row spans the full width, so it splits off first; the
        // old node keeps the space above it.
        let [upper, _assets] = tree.split_below(
            NodeIndex::root(),
            1.0 - BOTTOM_ROW_FRACTION,
            vec![EditorTab::Assets],
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

    /// Whether `tab` is docked somewhere: the main surface or a window.
    pub fn contains(&self, tab: EditorTab) -> bool {
        // `DockState::find_tab` reports the surface and node holding the tab.
        self.state.find_tab(&tab).is_some()
    }

    /// Whether the document was written by this build's schema.
    pub(crate) fn is_current(&self) -> bool {
        self.schema_version == super::LAYOUT_SCHEMA_VERSION
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
    let Some((surface, node, _)) = dock.find_tab(&tab) else {
        return false;
    };
    let Some(index) = dock[surface][node]
        .get_leaf()
        .and_then(|leaf| leaf.tabs.iter().position(|candidate| *candidate == tab))
    else {
        return false;
    };
    dock.set_active_tab((surface, node, TabIndex(index)));
    dock.set_focused_node_and_surface((surface, node));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_ui_egui::egui_dock::SurfaceIndex;

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
        assert_eq!(tabs_at(root.right()), vec![EditorTab::Assets]);
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
            vec![EditorTab::Scene, EditorTab::SecondScene, EditorTab::Game]
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
    fn activating_a_tab_brings_it_to_its_leaf_s_front() {
        let mut dock = EditorDock::new();
        assert!(activate_tab(dock.state_mut(), EditorTab::Systems));
        let (surface, node, _) = dock
            .state()
            .find_tab(&EditorTab::Systems)
            .expect("the systems tab is in the default dock");
        let leaf = dock.state()[surface][node]
            .get_leaf()
            .expect("the tab sits in a leaf");
        let active = leaf.tabs[leaf.active.0];
        assert_eq!(active, EditorTab::Systems);
    }

    #[test]
    fn activating_a_tab_the_dock_lacks_reports_it() {
        let mut state = DockState::new(vec![EditorTab::Scene]);
        assert!(!activate_tab(&mut state, EditorTab::Systems));
        assert!(activate_tab(&mut state, EditorTab::Scene));
    }
}
